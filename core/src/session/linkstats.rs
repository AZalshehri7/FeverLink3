//! Per-minute measurements for the latency logs (owner list 2026-10-17, items 4 and 6: the input barrier's stand-stills and the
//! build lag). Observation only: nothing here decides anything. The host and the launcher collect samples (round trips, the gaps
//! between clock arrivals, the latency of a hop of the link) and write one line per minute; the format of a group is
//! `name=min/median/max` with `name_n=count`, in milliseconds, so the lines grep and parse alike everywhere
//! (`tools/gametest/run_summary.py`).

use std::collections::BTreeMap;
use std::fmt::Write;

/// The samples of one quantity in the current minute.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Samples {
    values: Vec<f64>,
}

impl Samples {
    /// At most this many samples a minute are kept (a clock line per frame is about 3600); later ones only count.
    pub const KEEP: usize = 8192;

    pub fn push(&mut self, v: f64) {
        if v.is_finite() && self.values.len() < Self::KEEP {
            self.values.push(v);
        }
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// (min, median, max), `None` without samples.
    pub fn summary(&self) -> Option<(f64, f64, f64)> {
        if self.values.is_empty() {
            return None;
        }
        let mut v = self.values.clone();
        v.sort_by(f64::total_cmp);
        Some((v[0], v[v.len() / 2], v[v.len() - 1]))
    }

    /// ` name_n=N name=min/med/max` (whole ms), or ` name_n=0` without samples.
    pub fn describe(&self, name: &str) -> String {
        match self.summary() {
            Some((lo, mid, hi)) => format!(" {name}_n={} {name}={lo:.0}/{mid:.0}/{hi:.0}", self.values.len()),
            None => format!(" {name}_n=0"),
        }
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }
}

/// The gaps between arrivals of one stream (a peer's clock lines): each arrival after the first adds the time since the one
/// before, in milliseconds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Arrivals {
    last: Option<f64>,
    pub gaps: Samples,
}

impl Arrivals {
    /// An arrival at `seconds` on a monotonic clock. Returns the gap in ms, `None` for the first.
    pub fn arrived(&mut self, seconds: f64) -> Option<f64> {
        let gap = self.last.map(|l| ((seconds - l) * 1000.0).max(0.0));
        self.last = Some(seconds);
        if let Some(g) = gap {
            self.gaps.push(g);
        }
        gap
    }

    /// Milliseconds since the last arrival, `None` before the first.
    pub fn age_ms(&self, seconds: f64) -> Option<f64> {
        self.last.map(|l| ((seconds - l) * 1000.0).max(0.0))
    }
}

/// What the host measures per game for its `link.host` line: round trips (pongs) and the gaps between the clocks it handles
/// (the host's own game's too, which shows the cadence of its mod and the file link on the host's machine).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostLinkStats {
    started: Option<f64>,
    rtt: BTreeMap<u16, Samples>,
    clocks: BTreeMap<u16, Arrivals>,
    actions: BTreeMap<u16, usize>,
}

impl HostLinkStats {
    /// Seconds between two summaries.
    pub const MINUTE: f64 = 60.0;

    pub fn rtt(&mut self, player: u16, ms: f64) {
        self.rtt.entry(player).or_default().push(ms);
    }

    pub fn clock(&mut self, player: u16, now: f64) {
        self.clocks.entry(player).or_default().arrived(now);
    }

    pub fn action(&mut self, player: u16) {
        *self.actions.entry(player).or_default() += 1;
    }

    /// A player left: its stream starts afresh when it is back (a gap across an outage says nothing about the link).
    pub fn forget(&mut self, player: u16) {
        self.clocks.remove(&player);
    }

    /// Once a minute: one `link.host` line per game heard from (or measured), then the minute starts over. `offset` gives each
    /// player's measured clock offset for the line; `wall_ms` the host's wall clock (for lining the line up with the others).
    pub fn minute(&mut self, now: f64, wall_ms: Option<i64>, offset: impl Fn(u16) -> Option<i64>) -> Vec<String> {
        let started = *self.started.get_or_insert(now);
        if now - started < Self::MINUTE {
            return Vec::new();
        }
        self.started = Some(now);
        let mut ids: Vec<u16> = self.rtt.keys().chain(self.clocks.keys()).chain(self.actions.keys()).copied().collect();
        ids.sort_unstable();
        ids.dedup();
        let mut out = Vec::new();
        for id in ids {
            let mut line = format!("link.host wall={} secs={:.0} peer={id}", wall_ms.unwrap_or(0), now - started);
            let rtt = self.rtt.get(&id).cloned().unwrap_or_default();
            line.push_str(&rtt.describe("rtt_ms"));
            match offset(id) {
                Some(o) => {
                    let _ = write!(line, " off_ms={o}");
                }
                None => line.push_str(" off_ms=?"),
            }
            let gaps = self.clocks.get(&id).map(|a| a.gaps.clone()).unwrap_or_default();
            line.push_str(&gaps.describe("clock_gap_ms"));
            let _ = write!(line, " actions={}", self.actions.get(&id).copied().unwrap_or(0));
            out.push(line);
        }
        for s in self.rtt.values_mut() {
            s.clear();
        }
        for a in self.clocks.values_mut() {
            a.gaps.clear();
        }
        self.actions.clear();
        out
    }
}

/// A clock offset from NTP-style samples: of the last few, the one with the shortest round trip (its midpoint guess is the
/// tightest).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OffsetEstimate {
    recent: Vec<(f64, i64)>,
}

impl OffsetEstimate {
    const KEEP: usize = 8;

    /// A ping sent at host wall `sent`, answered at peer wall `peer`, back at host wall `back` (all ms). Returns the estimate.
    pub fn sample(&mut self, sent: i64, peer: i64, back: i64) -> i64 {
        let rtt = (back - sent).max(0) as f64;
        let off = peer - (sent + back) / 2;
        if self.recent.len() >= Self::KEEP {
            self.recent.remove(0);
        }
        self.recent.push((rtt, off));
        self.value().unwrap_or(off)
    }

    pub fn value(&self) -> Option<i64> {
        self.recent.iter().min_by(|a, b| a.0.total_cmp(&b.0)).map(|&(_, o)| o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_describe_min_median_max_and_an_empty_group() {
        let mut s = Samples::default();
        assert_eq!(s.describe("gap_ms"), " gap_ms_n=0");
        for v in [30.0, 10.0, 20.0, f64::NAN] {
            s.push(v);
        }
        assert_eq!(s.describe("gap_ms"), " gap_ms_n=3 gap_ms=10/20/30");
    }

    #[test]
    fn arrivals_count_gaps_and_age() {
        let mut a = Arrivals::default();
        assert_eq!(a.arrived(1.0), None);
        assert_eq!(a.arrived(1.25), Some(250.0));
        assert_eq!(a.age_ms(2.25), Some(1000.0));
        assert_eq!(a.gaps.len(), 1);
    }

    #[test]
    fn the_host_writes_one_line_per_game_a_minute_and_starts_over() {
        let mut h = HostLinkStats::default();
        assert!(h.minute(0.0, None, |_| None).is_empty(), "the first call starts the minute");
        h.rtt(1, 40.0);
        h.rtt(1, 60.0);
        h.clock(1, 1.0);
        h.clock(1, 1.2);
        h.clock(0, 1.0);
        h.action(1);
        assert!(h.minute(30.0, None, |_| None).is_empty());
        let lines = h.minute(60.0, Some(1_760_000_000_000), |id| (id == 1).then_some(-12));
        assert_eq!(
            lines,
            vec![
                "link.host wall=1760000000000 secs=60 peer=0 rtt_ms_n=0 off_ms=? clock_gap_ms_n=0 actions=0".to_owned(),
                "link.host wall=1760000000000 secs=60 peer=1 rtt_ms_n=2 rtt_ms=40/60/60 off_ms=-12 clock_gap_ms_n=1 clock_gap_ms=200/200/200 actions=1"
                    .to_owned(),
            ]
        );
        let next = h.minute(120.0, None, |_| None);
        assert!(next.iter().all(|l| l.contains("rtt_ms_n=0") && l.contains("clock_gap_ms_n=0")), "{next:?}");
    }

    #[test]
    fn the_offset_is_the_one_of_the_tightest_round_trip() {
        let mut o = OffsetEstimate::default();
        // peer 500 ms ahead; a slow sample with an asymmetric path says 600
        assert_eq!(o.sample(1000, 1550, 1100), 500);
        assert_eq!(o.sample(2000, 2700, 2200), 500, "the slower sample does not move the estimate");
        assert_eq!(o.sample(3000, 3505, 3010), 500);
    }
}
