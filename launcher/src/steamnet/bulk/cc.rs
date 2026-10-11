//! The send-rate controller of the bulk sender: delay-based and loss-aware, in the spirit of LEDBAT and BBR.
//!
//! It controls a pacing *rate* (bytes per second), not a window, because the path here is long (100 to 250 ms) and the
//! rate that fits is what matters. What it measures:
//!
//! - `min_rtt`: the smallest round trip seen in the last 10 s, taken as the path with empty queues;
//! - `queue delay`: the smallest round trip of the last ~60 ms minus `min_rtt` (the minimum filters jitter: a standing
//!   queue shows in every sample, a jitter spike in one);
//! - the delivery rate: bytes newly acknowledged in the last round trip (clamped to 50 to 300 ms) per second. While a
//!   queue stands at the bottleneck, that is the bottleneck's rate.
//!
//! What it does:
//!
//! - *Startup*: the rate grows every round trip (by `1 + 1.5 x target / round trip`, 1.28 at 214 ms and 40 ms, at most
//!   doubling: the queue that builds before its first sign comes back is half that growth times the round trip) from
//!   [`CcConfig::start_rate`] until the queue delay reaches half the
//!   target, the delivery rate stops growing, loss gets heavy, or the maximum is reached.
//! - *Steady*: queue delay above the target cuts the rate below the delivery rate (so the queue drains, whatever
//!   the old rate was, and by as much as drains the excess in a round trip); below it the rate grows by up to 25 % per
//!   round trip, the less the closer the queue is to the target, and never above 1.05 to 1.13 x the delivery rate, so
//!   it feels for the capacity in small steps.
//! - *Loss* is not delay: with an empty queue a lost datagram is chance and costs nothing; with a queue it is
//!   congestion and cuts the rate like the delay does; a heavy loss ratio (over 15 %) cuts it either way.
//! - *Timeout*: halves the rate.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct CcConfig {
    /// Bytes per second the transfer starts at.
    pub start_rate: f64,
    pub min_rate: f64,
    pub max_rate: f64,
    /// The queueing delay (round trip above the minimum) the controller aims to stay under.
    pub target_delay: Duration,
}

impl Default for CcConfig {
    fn default() -> CcConfig {
        CcConfig {
            start_rate: 400_000.0,
            min_rate: 30_000.0,
            max_rate: 40_000_000.0,
            target_delay: Duration::from_millis(40),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Startup,
    Steady,
}

/// Round-trip samples kept for the filtered minimum.
const SAMPLE_KEEP: usize = 64;
/// The window of the filtered minimum, and the fewest samples it covers.
const Q_WINDOW: Duration = Duration::from_millis(60);
const Q_MIN_SAMPLES: usize = 4;
/// How long a minimum round trip stays valid.
const MIN_RTT_EPOCH: Duration = Duration::from_secs(10);

pub struct Cc {
    cfg: CcConfig,
    rate: f64,
    phase: Phase,
    min_rtt: Option<f64>,
    epoch_start: Instant,
    epoch_min: Option<f64>,
    srtt: Option<f64>,
    rttvar: f64,
    samples: VecDeque<(Instant, f64)>,
    acks: VecDeque<(Instant, u32)>,
    acks_sum: u64,
    first_ack: Option<Instant>,
    last_adjust: Instant,
    last_cut: Option<Instant>,
    // startup plateau detection, one look per round trip
    round_start: Instant,
    best_dr: f64,
    stale_rounds: u32,
    // loss ratio, both counters decayed with the round trip
    sent_decay: f64,
    lost_decay: f64,
    decay_at: Instant,
    pub startup_exit: Option<(Instant, &'static str)>,
    pub cuts: u32,
}

impl Cc {
    pub fn new(cfg: CcConfig, now: Instant) -> Cc {
        Cc {
            cfg,
            rate: cfg.start_rate.clamp(cfg.min_rate, cfg.max_rate),
            phase: Phase::Startup,
            min_rtt: None,
            epoch_start: now,
            epoch_min: None,
            srtt: None,
            rttvar: 0.0,
            samples: VecDeque::new(),
            acks: VecDeque::new(),
            acks_sum: 0,
            first_ack: None,
            last_adjust: now,
            last_cut: None,
            round_start: now,
            best_dr: 0.0,
            stale_rounds: 0,
            sent_decay: 0.0,
            lost_decay: 0.0,
            decay_at: now,
            startup_exit: None,
            cuts: 0,
        }
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn min_rtt(&self) -> Option<Duration> {
        self.min_rtt.map(Duration::from_secs_f64)
    }

    pub fn srtt(&self) -> Option<Duration> {
        self.srtt.map(Duration::from_secs_f64)
    }

    /// The retransmission timeout from the smoothed round trip.
    pub fn rto(&self) -> Duration {
        match self.srtt {
            None => Duration::from_secs(1),
            Some(s) => Duration::from_secs_f64((s + (4.0 * self.rttvar).max(0.025) + 0.010).clamp(0.06, 4.0)),
        }
    }

    /// The round-trip jitter estimate, seconds.
    pub fn rttvar(&self) -> f64 {
        self.rttvar
    }

    fn rtt_base(&self) -> f64 {
        self.min_rtt.or(self.srtt).unwrap_or(0.1).max(0.005)
    }

    /// Round trip above the minimum, filtered, seconds; 0 until there are samples.
    pub fn queue_delay(&self) -> f64 {
        let Some(min) = self.min_rtt else { return 0.0 };
        let Some(&(newest, _)) = self.samples.back() else { return 0.0 };
        let mut lowest = f64::MAX;
        for (seen, &(t, rtt)) in self.samples.iter().rev().enumerate() {
            if seen >= Q_MIN_SAMPLES && newest.saturating_duration_since(t) > Q_WINDOW {
                break;
            }
            lowest = lowest.min(rtt);
        }
        (lowest - min).max(0.0)
    }

    /// The queue delay above what jitter alone explains (twice the round-trip variation): what the decisions use, so a
    /// path with jittery delay (Wi-Fi, a busy relay) is not mistaken for a path with a queue.
    pub fn queue_excess(&self) -> f64 {
        (self.queue_delay() - 2.0 * self.rttvar).max(0.0)
    }

    /// Bytes per second delivered in the last round trip; `None` before there is a window's worth of history.
    pub fn delivery_rate(&self, now: Instant) -> Option<f64> {
        let window = self.srtt.unwrap_or(0.1).clamp(0.05, 0.3);
        let first = self.first_ack?;
        if now.saturating_duration_since(first).as_secs_f64() < window {
            return None;
        }
        let sum: u64 = self
            .acks
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t).as_secs_f64() <= window)
            .map(|(_, b)| u64::from(*b))
            .sum();
        Some(sum as f64 / window)
    }

    fn decay(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.decay_at).as_secs_f64();
        if dt > 0.0 {
            let k = 0.5f64.powf(dt / (self.srtt.unwrap_or(0.2) * 2.0).max(0.1));
            self.sent_decay *= k;
            self.lost_decay *= k;
            self.decay_at = now;
        }
    }

    pub fn on_sent(&mut self, now: Instant) {
        self.decay(now);
        self.sent_decay += 1.0;
    }

    /// Recent loss ratio, 0 to 1.
    pub fn loss_ratio(&self) -> f64 {
        if self.sent_decay < 20.0 {
            0.0
        } else {
            (self.lost_decay / self.sent_decay).min(1.0)
        }
    }

    pub fn on_rtt(&mut self, now: Instant, rtt: Duration) {
        let r = rtt.as_secs_f64().max(0.0001);
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2.0;
            }
            Some(s) => {
                self.rttvar = 0.75 * self.rttvar + 0.25 * (s - r).abs();
                self.srtt = Some(0.875 * s + 0.125 * r);
            }
        }
        if now.saturating_duration_since(self.epoch_start) >= MIN_RTT_EPOCH {
            // the old minimum expires: the best of the epoch just ended takes over
            self.min_rtt = self.epoch_min.or(self.min_rtt);
            self.epoch_min = None;
            self.epoch_start = now;
        }
        self.epoch_min = Some(self.epoch_min.map_or(r, |m| m.min(r)));
        self.min_rtt = Some(self.min_rtt.map_or(r, |m| m.min(r)));
        self.samples.push_back((now, r));
        if self.samples.len() > SAMPLE_KEEP {
            self.samples.pop_front();
        }
    }

    /// Bytes newly acknowledged; runs the controller.
    pub fn on_acked(&mut self, now: Instant, bytes: u32) {
        if bytes > 0 {
            self.first_ack.get_or_insert(now);
            self.acks.push_back((now, bytes));
            self.acks_sum += u64::from(bytes);
        }
        let horizon = 0.5f64;
        while let Some(&(t, b)) = self.acks.front() {
            if now.saturating_duration_since(t).as_secs_f64() > horizon {
                self.acks.pop_front();
                self.acks_sum -= u64::from(b);
            } else {
                break;
            }
        }
        self.control(now);
    }

    fn cut(&mut self, now: Instant, to: f64) {
        let to = to.clamp(self.cfg.min_rate, self.cfg.max_rate);
        if to < self.rate {
            self.rate = to;
        }
        self.last_cut = Some(now);
        self.cuts += 1;
    }

    fn exit_startup(&mut self, now: Instant, why: &'static str, dr: Option<f64>) {
        if self.phase == Phase::Startup {
            self.phase = Phase::Steady;
            self.startup_exit = Some((now, why));
            if let Some(dr) = dr {
                self.cut(now, dr * 0.85);
            }
        }
    }

    fn control(&mut self, now: Instant) {
        let rtt = self.rtt_base();
        let interval = Duration::from_secs_f64((rtt / 4.0).clamp(0.005, 0.05));
        let since = now.saturating_duration_since(self.last_adjust);
        if since < interval {
            return;
        }
        self.last_adjust = now;
        let dt = since.as_secs_f64().min(rtt.max(0.05));
        let target = self.cfg.target_delay.as_secs_f64();
        let q = self.queue_excess();
        let dr = self.delivery_rate(now);
        match self.phase {
            Phase::Startup => {
                // any sign of a queue ends the start-up: the first sign comes a round trip late, and what is sent
                // meanwhile still grows
                if q > 0.004 {
                    self.exit_startup(now, "queue delay", dr);
                    return;
                }
                // one look per round trip at whether the delivery rate still grows
                if now.saturating_duration_since(self.round_start).as_secs_f64() >= rtt.max(0.05) {
                    self.round_start = now;
                    if let Some(dr) = dr {
                        if dr < self.best_dr * 1.25 {
                            self.stale_rounds += 1;
                        } else {
                            self.stale_rounds = 0;
                        }
                        self.best_dr = self.best_dr.max(dr);
                        if self.stale_rounds >= 3 {
                            self.exit_startup(now, "delivery rate plateau", Some(dr));
                            return;
                        }
                    }
                }
                // The queue that builds before the first sign of it comes back is about half the growth per round
                // trip above 1, times the round trip: so the growth is chosen to keep that near the target.
                let g = (1.0 + 1.5 * target / rtt.max(0.02)).clamp(1.2, 2.0);
                self.rate *= g.powf(dt / rtt.max(0.05));
                if self.rate >= self.cfg.max_rate {
                    self.rate = self.cfg.max_rate;
                    self.exit_startup(now, "maximum rate", None);
                }
            }
            Phase::Steady => {
                if q > target {
                    let base = dr.unwrap_or(self.rate);
                    // aim to drain what is above half the target within one round trip; idempotent, because it is
                    // relative to the delivery rate: a rate already below it stays
                    let factor = (1.0 - (q - target * 0.5) / rtt.max(0.05)).clamp(0.5, 0.9);
                    self.cut(now, base * factor);
                } else if q > target * 0.25 && dr.is_some_and(|dr| self.rate > dr * 1.03) {
                    // a queue shows and more is sent than arrives: the probe has found the capacity, stop climbing
                    // and let the queue drain a little
                    self.cut(now, dr.unwrap_or(self.rate) * 0.95);
                } else {
                    let x = (1.0 - q / target).clamp(0.0, 1.0);
                    let growth = 1.0 + 0.25 * x * x;
                    let mut next = self.rate * growth.powf(dt / rtt.max(0.05));
                    if let Some(dr) = dr {
                        let cap = dr * (1.05 + 0.08 * (1.0 - q / (target * 0.5)).clamp(0.0, 1.0));
                        next = next.min(cap.max(self.rate));
                    }
                    self.rate = next.clamp(self.cfg.min_rate, self.cfg.max_rate);
                }
            }
        }
    }

    /// `n` datagrams were declared lost.
    pub fn on_loss(&mut self, now: Instant, n: u32) {
        self.decay(now);
        self.lost_decay += f64::from(n);
        let q = self.queue_excess();
        let target = self.cfg.target_delay.as_secs_f64();
        let ratio = self.loss_ratio();
        let rtt = self.rtt_base();
        let recently = self.last_cut.is_some_and(|t| now.saturating_duration_since(t).as_secs_f64() < rtt.max(0.05));
        let dr = self.delivery_rate(now);
        match self.phase {
            Phase::Startup => {
                if q > target * 0.25 || ratio > 0.10 {
                    self.exit_startup(now, "loss", Some(dr.unwrap_or(self.rate / 2.0)));
                }
            }
            Phase::Steady => {
                if recently {
                    return;
                }
                if q > target * 0.25 {
                    self.cut(now, dr.unwrap_or(self.rate) * 0.85);
                } else if ratio > 0.15 {
                    self.cut(now, self.rate * 0.8);
                }
            }
        }
    }

    /// The retransmission timer fired.
    pub fn on_rto(&mut self, now: Instant) {
        if self.phase == Phase::Startup {
            self.phase = Phase::Steady;
            self.startup_exit = Some((now, "timeout"));
        }
        self.cut(now, self.rate * 0.5);
    }
}
