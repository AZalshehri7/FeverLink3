//! The controller that decides how many connections a transfer uses. Pure: it is told the time (seconds since the
//! start), the bytes acknowledged so far, the Steam ping and the pending queue of the *control* connection, and it
//! answers hold, open another connection, or close the newest one.
//!
//! Rules (all on the numbers it is given):
//!
//! - Start with `initial` connections (`min(N, 4)`). The first decision comes 2 s after the first connection works.
//! - **Scaling.** A new connection must pay for itself: 2 s after it was opened (measured over the last 1.5 s of
//!   them, so its start-up does not count) the aggregate rate must have risen by at least 60 % of the per-connection
//!   rate over what it was just before. Only then is the next connection opened, so connections are added one at a
//!   time, every 2 s at the fastest. The first group is checked against the rate times its size. Not met: stop adding,
//!   say why. (With `forced` connections this check is skipped.)
//! - **Safety.** The control connection's Steam ping (average over the last 3 s) must stay below
//!   `max(3 x baseline, baseline + 120 ms)`; when even its minimum is above that, the newest connection is closed
//!   as well. No data for the last 3 s (a stall), or a pending queue on the control connection that is above 128 KiB
//!   and still growing, also stops the growth.

use std::collections::VecDeque;

/// What the controller wants done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Hold,
    /// Open one more connection.
    Open,
    /// Close the newest connection (and no more are added).
    CloseNewest,
}

#[derive(Debug, Clone)]
pub struct GrowthCfg {
    /// Bytes per second one connection is expected to deliver.
    pub rate: f64,
    /// The connections wanted in all.
    pub target: usize,
    /// Connections started together.
    pub initial: usize,
    /// Skip the scaling check (a forced number of connections).
    pub forced: bool,
    pub gain_fraction: f64,
    pub open_every: f64,
    /// Seconds to wait after the first connection works before the first decision.
    pub settle: f64,
}

#[derive(Debug, Clone, Copy)]
struct Opened {
    at: f64,
    /// The aggregate rate over the 2 s before it was opened.
    before: f64,
}

#[derive(Debug, Clone)]
pub struct Growth {
    cfg: GrowthCfg,
    samples: VecDeque<(f64, u64)>,
    pings: VecDeque<(f64, u32)>,
    pending: VecDeque<(f64, u32)>,
    baseline: Option<u32>,
    first_record: Option<f64>,
    first_ready: Option<f64>,
    opened: Vec<Opened>,
    last_open: f64,
    stopped: Option<String>,
    ping_closed: bool,
    /// Notes for the report, in order.
    pub notes: Vec<String>,
}

const WINDOW: f64 = 2.0;

impl Growth {
    pub fn new(cfg: GrowthCfg) -> Growth {
        Growth {
            cfg,
            samples: VecDeque::new(),
            pings: VecDeque::new(),
            pending: VecDeque::new(),
            baseline: None,
            first_record: None,
            first_ready: None,
            opened: Vec::new(),
            last_open: f64::NEG_INFINITY,
            stopped: None,
            ping_closed: false,
            notes: Vec::new(),
        }
    }

    pub fn config(&self) -> &GrowthCfg {
        &self.cfg
    }

    /// Why growth ended, `None` while it can still go on.
    pub fn stopped(&self) -> Option<&str> {
        self.stopped.as_deref()
    }

    /// The ping the limits are measured from.
    pub fn baseline(&self) -> Option<u32> {
        self.baseline
    }

    /// The ping above which growth stops: `max(3 x baseline, baseline + 120 ms)`.
    pub fn ping_limit(&self) -> Option<f64> {
        self.baseline.map(|b| (3.0 * f64::from(b)).max(f64::from(b) + 120.0))
    }

    /// One measurement. Call about every 25 to 100 ms.
    pub fn record(&mut self, t: f64, acked: u64, ping: Option<u32>, pending: u32) {
        let first = *self.first_record.get_or_insert(t);
        self.samples.push_back((t, acked));
        while self.samples.len() > 2 && self.samples.get(1).is_some_and(|s| s.0 < t - 6.0) {
            self.samples.pop_front();
        }
        if let Some(p) = ping.filter(|p| *p > 0) {
            self.pings.push_back((t, p));
            while self.pings.front().is_some_and(|s| s.0 < t - 6.0) {
                self.pings.pop_front();
            }
            // the baseline: the lowest ping seen while the transfer was still starting (the first second)
            if t - first <= 1.0 || self.baseline.is_none() {
                self.baseline = Some(self.baseline.map_or(p, |b| b.min(p)));
            }
        }
        self.pending.push_back((t, pending));
        while self.pending.front().is_some_and(|s| s.0 < t - 6.0) {
            self.pending.pop_front();
        }
    }

    /// The first connection is working (data can flow) at `t`.
    pub fn ready(&mut self, t: f64) {
        self.first_ready.get_or_insert(t);
    }

    /// A connection was opened at `t` (the scaling check of the next decision looks at its effect).
    pub fn note_opened(&mut self, t: f64) {
        let before = self.agg(t, WINDOW);
        self.opened.push(Opened { at: t, before });
        self.last_open = t;
    }

    /// Stops the growth from outside (a connection failed, say).
    pub fn stop(&mut self, why: impl Into<String>) {
        if self.stopped.is_none() {
            self.stopped = Some(why.into());
        }
    }

    /// Bytes per second acknowledged over the last `w` seconds (less when the transfer is younger).
    pub fn agg(&self, t: f64, w: f64) -> f64 {
        let Some(&(_, now)) = self.samples.back() else { return 0.0 };
        let from = t - w;
        // the newest sample at or before `from`, else the oldest
        let then = self.samples.iter().rev().find(|s| s.0 <= from).or(self.samples.front());
        let Some(&(t0, b0)) = then else { return 0.0 };
        let span = (t - t0).max(1e-3);
        now.saturating_sub(b0) as f64 / span
    }

    fn ping_stats(&self, t: f64, w: f64) -> Option<(f64, f64, usize)> {
        let v: Vec<u32> = self.pings.iter().filter(|p| p.0 >= t - w).map(|p| p.1).collect();
        if v.is_empty() {
            return None;
        }
        let min = f64::from(*v.iter().min()?);
        let avg = v.iter().map(|x| f64::from(*x)).sum::<f64>() / v.len() as f64;
        Some((min, avg, v.len()))
    }

    /// What to do now, given `lanes` connections open or opening and `ready` of them working.
    pub fn decide(&mut self, t: f64, lanes: usize, ready: usize) -> Step {
        // safety first: the ping of the control connection. Inflation that lasts (even the lowest ping of the last 3 s
        // is over the limit) closes the newest connection, once, even if the growth had stopped before.
        if let (Some(limit), Some((min, avg, n))) = (self.ping_limit(), self.ping_stats(t, 3.0)) {
            if n >= 3 && min > limit && lanes > 1 && !self.ping_closed {
                self.ping_closed = true;
                let text = format!(
                    "the Steam ping rose from {} ms to {min:.0} ms (limit {limit:.0} ms): closed the newest connection",
                    self.baseline.unwrap_or(0)
                );
                self.notes.push(text.clone());
                self.stopped.get_or_insert(text);
                return Step::CloseNewest;
            }
            if self.stopped.is_none() && n >= 3 && avg > limit {
                self.stopped = Some(format!(
                    "the Steam ping rose from {} ms to {avg:.0} ms on average (limit {limit:.0} ms)",
                    self.baseline.unwrap_or(0)
                ));
                return Step::Hold;
            }
        }
        if self.stopped.is_some() {
            return Step::Hold;
        }
        // a growing queue on the control connection
        if let Some(&(_, now)) = self.pending.back() {
            let earlier = self.pending.iter().find(|p| p.0 >= t - 2.0).map_or(now, |p| p.1);
            if now > 128 * 1024 && now > earlier.saturating_add(32 * 1024) {
                self.stopped =
                    Some(format!("the control connection's queue is growing ({} KiB pending)", now / 1024));
                return Step::Hold;
            }
        }
        if lanes >= self.cfg.target {
            return Step::Hold;
        }
        let Some(first) = self.first_ready else { return Step::Hold };
        if t < first + self.cfg.settle || t - self.last_open < self.cfg.open_every {
            return Step::Hold;
        }
        // a stall: nothing arrived in the last 2 s although connections are up
        if ready > 0 && self.agg(t, 2.0) <= 0.0 {
            self.stopped = Some("no data arrived for 2 s".to_owned());
            return Step::Hold;
        }
        if !self.cfg.forced {
            match self.opened.last().copied() {
                None => {
                    let want = self.cfg.gain_fraction * self.cfg.rate * ready.max(1) as f64;
                    let got = self.agg(t, WINDOW);
                    if got < want {
                        self.stopped = Some(format!(
                            "the first {ready} connection(s) delivered {:.2} MB/s, less than {:.0} % of {:.2} MB/s",
                            got / 1e6,
                            self.cfg.gain_fraction * 100.0,
                            self.cfg.rate * ready.max(1) as f64 / 1e6
                        ));
                        return Step::Hold;
                    }
                }
                Some(last) => {
                    let gain = self.agg(t, 1.5) - last.before;
                    let need = self.cfg.gain_fraction * self.cfg.rate;
                    self.notes.push(format!(
                        "connection {} added {:.2} MB/s (needs {:.2})",
                        lanes,
                        gain / 1e6,
                        need / 1e6
                    ));
                    if gain < need {
                        self.stopped = Some(format!(
                            "connection {lanes} added only {:.2} MB/s (less than {:.0} % of {:.2} MB/s): the path is saturated",
                            gain.max(0.0) / 1e6,
                            self.cfg.gain_fraction * 100.0,
                            self.cfg.rate / 1e6
                        ));
                        return Step::Hold;
                    }
                }
            }
        }
        Step::Open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(target: usize) -> GrowthCfg {
        GrowthCfg { rate: 1_000_000.0, target, initial: 4, forced: false, gain_fraction: 0.6, open_every: 2.0, settle: 2.0 }
    }

    /// Runs the controller against a line that delivers `min(lanes x per_lane, line)`; returns lanes at the end.
    fn simulate(target: usize, per_lane: f64, line: f64, seconds: f64) -> (usize, Option<String>) {
        let mut g = Growth::new(cfg(target));
        let mut lanes = 4.min(target);
        g.ready(0.0);
        let mut acked = 0.0f64;
        let mut t = 0.0;
        // a lane opened at time x delivers from x
        while t < seconds {
            let rate = (lanes as f64 * per_lane).min(line);
            acked += rate * 0.05;
            t += 0.05;
            g.record(t, acked as u64, Some(35), 0);
            match g.decide(t, lanes, lanes) {
                Step::Open => {
                    lanes += 1;
                    g.note_opened(t);
                }
                Step::CloseNewest => lanes -= 1,
                Step::Hold => {}
            }
        }
        (lanes, g.stopped().map(str::to_owned))
    }

    #[test]
    fn grows_to_the_target_when_the_path_scales() {
        let (lanes, stopped) = simulate(8, 1_000_000.0, 100e6, 20.0);
        assert_eq!(lanes, 8, "{stopped:?}");
        assert_eq!(stopped, None);
    }

    #[test]
    fn stops_when_the_line_is_saturated() {
        let (lanes, stopped) = simulate(16, 1_000_000.0, 4_000_000.0, 30.0);
        assert!((4..=5).contains(&lanes), "{lanes} {stopped:?}");
        assert!(stopped.unwrap().contains("saturated"));
    }

    #[test]
    fn starts_with_at_most_four_and_never_beyond_the_target() {
        let (lanes, _) = simulate(2, 1_000_000.0, 100e6, 10.0);
        assert_eq!(lanes, 2);
    }

    #[test]
    fn a_weak_first_group_stops_growth() {
        let (lanes, stopped) = simulate(8, 200_000.0, 100e6, 10.0);
        assert_eq!(lanes, 4);
        assert!(stopped.unwrap().contains("delivered"));
    }

    #[test]
    fn a_rising_ping_stops_and_a_high_one_closes_the_newest() {
        let mut g = Growth::new(cfg(8));
        g.ready(0.0);
        let mut acked = 0u64;
        let mut t = 0.0;
        let mut closed = false;
        let mut lanes = 4;
        while t < 14.0 {
            t += 0.05;
            acked += (lanes as u64) * 50_000;
            // the ping inflates from 6 s on
            let ping = if t < 6.0 { 35 } else { 400 };
            g.record(t, acked, Some(ping), 0);
            match g.decide(t, lanes, lanes) {
                Step::Open => {
                    lanes += 1;
                    g.note_opened(t);
                }
                Step::CloseNewest => {
                    closed = true;
                    lanes -= 1;
                }
                Step::Hold => {}
            }
        }
        assert!(closed);
        assert!(g.stopped().unwrap().contains("ping"));
        assert_eq!(g.ping_limit(), Some(155.0));
    }

    #[test]
    fn the_ping_limit_is_three_times_the_baseline_or_120_ms_more() {
        let mut g = Growth::new(cfg(8));
        g.record(0.0, 0, Some(35), 0);
        assert_eq!(g.ping_limit(), Some(155.0));
        let mut g = Growth::new(cfg(8));
        g.record(0.0, 0, Some(230), 0);
        assert_eq!(g.ping_limit(), Some(690.0));
    }

    #[test]
    fn a_growing_control_queue_stops_growth() {
        let mut g = Growth::new(cfg(8));
        g.ready(0.0);
        let mut t = 0.0;
        let mut acked = 0u64;
        let mut stopped = false;
        while t < 8.0 {
            t += 0.05;
            acked += 200_000;
            g.record(t, acked, Some(35), (t * 100_000.0) as u32);
            g.decide(t, 4, 4);
            stopped |= g.stopped().is_some();
        }
        assert!(stopped);
        assert!(g.stopped().unwrap().contains("queue"));
    }

    #[test]
    fn forced_connections_skip_the_scaling_check() {
        let mut c = cfg(8);
        c.forced = true;
        let mut g = Growth::new(c);
        g.ready(0.0);
        let mut t = 0.0;
        let mut lanes = 4;
        let mut acked = 0u64;
        while t < 10.0 {
            t += 0.05;
            acked += 4_000 * 50 / 50; // a flat, tiny line
            acked += 100_000;
            g.record(t, acked, Some(35), 0);
            if g.decide(t, lanes, lanes) == Step::Open {
                lanes += 1;
                g.note_opened(t);
            }
        }
        assert_eq!(lanes, 8);
    }
}
