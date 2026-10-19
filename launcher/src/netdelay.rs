//! A TEST-ONLY network between the games: `FEVERLINK3_TEST_NET_DELAY_MS=<one-way ms>`, with jitter and loss on top
//! (`FEVERLINK3_TEST_NET_JITTER_MS`, `FEVERLINK3_TEST_NET_LOSS_PCT`, seeded by `FEVERLINK3_TEST_NET_SEED`).
//!
//! Two games on one PC talk over loopback, which hides the network time a real session has: the stamp margin of a build (how far
//! past the fastest clock a build is stamped, so that every game has it before it reaches the stamp) cannot be proven there. With
//! the variables set, every event a session's transport delivers (frames, opened, closed, bulk files) reaches the session thread
//! later, in order. Each end delays what it receives, so a host and a player that both have the variables see the shape in each
//! direction (a round trip of twice the delay plus loopback), measured by the host's pings like a real link.
//!
//! A constant delay is not what the internet test showed (friend test of 2026-10-18: round trips 26/30/71 ms p50/p90/max, clock gaps
//! up to 825 ms): the waits at the input barrier come from the spikes, which a constant delay never has. So:
//!   * `JITTER_MS`: each event waits a further uniform 0..=jitter ms;
//!   * `LOSS_PCT`: that percentage of events is "lost" and sent again: it waits a resend time more (`FEVERLINK3_TEST_NET_RESEND_MS`,
//!     default [`DEFAULT_RESEND_MS`], at least twice the one-way delay: a sender notices a loss a round trip later at the soonest).
//!     Nothing is dropped: the session's transport is reliable and ordered (Steam's reliable messages), so a loss is a late
//!     message, never a missing one;
//!   * the order is kept: an event never overtakes the one before it, so a late one holds the ones behind it up (head-of-line
//!     blocking, as on an ordered reliable stream). This is a model of such a link, not a measurement of Steam's own resend timer;
//!   * `SEED` (default 1): the draws come from a fixed generator (splitmix64), so the same seed gives the same delay for the n-th
//!     event of a run.
//!
//! Unset, empty, 0 or not a number: that part is off; all of them off: nothing changes (the transport's channel is used directly,
//! no thread).

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::net::NetEvent;

/// The environment variables (test switches, never set by a player).
pub const ENV: &str = "FEVERLINK3_TEST_NET_DELAY_MS";
pub const ENV_JITTER: &str = "FEVERLINK3_TEST_NET_JITTER_MS";
pub const ENV_LOSS: &str = "FEVERLINK3_TEST_NET_LOSS_PCT";
pub const ENV_RESEND: &str = "FEVERLINK3_TEST_NET_RESEND_MS";
pub const ENV_SEED: &str = "FEVERLINK3_TEST_NET_SEED";
/// The largest delay or jitter accepted: a longer one is a typing error, not a network.
pub const MAX_MS: u64 = 5000;
/// The largest loss accepted, percent: a link that loses more is no link to play on.
pub const MAX_LOSS_PCT: f64 = 50.0;
/// The resend time of a lost event when none is given, ms: the usual floor of a retransmission timer (RFC 6298 allows 200 ms in
/// practice; TCP stacks use 200).
pub const DEFAULT_RESEND_MS: u64 = 200;

/// A delay in milliseconds a value asks for; `None` for unset, empty, 0, over [`MAX_MS`] or not a number.
pub fn parse(value: Option<&str>) -> Option<Duration> {
    let ms: u64 = value?.trim().parse().ok()?;
    (1..=MAX_MS).contains(&ms).then(|| Duration::from_millis(ms))
}

/// A loss percentage; `None` for unset, empty, 0, negative, over [`MAX_LOSS_PCT`] or not a number.
pub fn parse_loss(value: Option<&str>) -> Option<f64> {
    let pct: f64 = value?.trim().parse().ok()?;
    (pct.is_finite() && pct > 0.0 && pct <= MAX_LOSS_PCT).then_some(pct)
}

/// What the test network does to each event.
#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub delay: Duration,
    pub jitter: Duration,
    /// Percent of events sent again.
    pub loss_pct: f64,
    /// The extra wait of an event sent again.
    pub resend: Duration,
    pub seed: u64,
}

impl Shape {
    /// The shape the values ask for (as the environment variables give them); `None` when delay, jitter and loss are all off.
    pub fn from_values(
        delay: Option<&str>,
        jitter: Option<&str>,
        loss: Option<&str>,
        resend: Option<&str>,
        seed: Option<&str>,
    ) -> Option<Shape> {
        let delay = parse(delay);
        let jitter = parse(jitter);
        let loss = parse_loss(loss);
        if delay.is_none() && jitter.is_none() && loss.is_none() {
            return None;
        }
        let delay = delay.unwrap_or_default();
        let resend = parse(resend).unwrap_or(Duration::from_millis(DEFAULT_RESEND_MS)).max(delay * 2);
        let seed = seed.and_then(|s| s.trim().parse().ok()).unwrap_or(1);
        Some(Shape { delay, jitter: jitter.unwrap_or_default(), loss_pct: loss.unwrap_or(0.0), resend, seed })
    }

    /// The line the session logs, so a run's log shows it was a test and with what.
    pub fn describe(&self) -> String {
        format!(
            "{ENV}={} {ENV_JITTER}={} {ENV_LOSS}={} (resend {} ms) {ENV_SEED}={}",
            self.delay.as_millis(),
            self.jitter.as_millis(),
            self.loss_pct,
            self.resend.as_millis(),
            self.seed
        )
    }
}

/// The shape this process was started with.
pub fn from_env() -> Option<Shape> {
    let var = |name: &str| std::env::var(name).ok();
    Shape::from_values(
        var(ENV).as_deref(),
        var(ENV_JITTER).as_deref(),
        var(ENV_LOSS).as_deref(),
        var(ENV_RESEND).as_deref(),
        var(ENV_SEED).as_deref(),
    )
}

/// splitmix64: a small generator whose sequence is fixed by its seed (no crate version can change it).
#[derive(Debug, Clone)]
struct Draws(u64);

impl Draws {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in 0..1.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The wait of each event in turn: its delay, jitter and resend, never due before the one before it.
#[derive(Debug, Clone)]
struct Timeline {
    shape: Shape,
    draws: Draws,
    last_due: Option<Instant>,
}

impl Timeline {
    fn new(shape: Shape) -> Self {
        let draws = Draws(shape.seed);
        Self { shape, draws, last_due: None }
    }

    /// The wait of the next event alone (jitter and resend drawn), before the order is applied.
    fn wait(&mut self) -> Duration {
        let s = &self.shape;
        let mut wait = s.delay;
        if !s.jitter.is_zero() {
            wait += s.jitter.mul_f64(self.draws.unit());
        }
        if s.loss_pct > 0.0 && self.draws.unit() * 100.0 < s.loss_pct {
            wait += s.resend;
        }
        wait
    }

    /// When an event that arrived at `at` is passed on.
    fn due(&mut self, at: Instant) -> Instant {
        let due = at + self.wait();
        let due = self.last_due.map_or(due, |last| due.max(last));
        self.last_due = Some(due);
        due
    }
}

/// The channel a session's transport sends its events into and the session thread reads from. Without a shape it is a plain
/// channel. With one, a forwarding thread takes each event as it arrives, keeps it until its wait has passed since its arrival, and
/// passes it on in arrival order; the thread ends when the transport's senders are gone and everything kept has been passed on
/// (the session thread then sees the channel closed, as without the shape).
pub fn channel(shape: Option<Shape>) -> (Sender<NetEvent>, Receiver<NetEvent>) {
    let Some(shape) = shape else { return mpsc::channel() };
    let (in_tx, in_rx) = mpsc::channel::<NetEvent>();
    let (out_tx, out_rx) = mpsc::channel::<NetEvent>();
    let spawned = thread::Builder::new()
        .name("feverlink3-test-delay".into())
        .spawn(move || forward(&in_rx, &out_tx, Timeline::new(shape)));
    if spawned.is_err() {
        // no thread: no delay rather than no session
        return mpsc::channel();
    }
    (in_tx, out_rx)
}

fn forward(input: &Receiver<NetEvent>, output: &Sender<NetEvent>, mut timeline: Timeline) {
    let mut kept: VecDeque<(Instant, NetEvent)> = VecDeque::new();
    let mut open = true;
    while open || !kept.is_empty() {
        let now = Instant::now();
        while kept.front().is_some_and(|(due, _)| *due <= now) {
            if let Some((_, event)) = kept.pop_front() {
                if output.send(event).is_err() {
                    return; // the session thread is gone
                }
            }
        }
        if !open {
            if let Some((due, _)) = kept.front() {
                thread::sleep(due.saturating_duration_since(Instant::now()));
            }
            continue;
        }
        // wait for the next arrival, but not past the next due time: an arrival is stamped the moment it comes in
        let wait = kept.front().map_or(Duration::from_millis(50), |(due, _)| due.saturating_duration_since(now));
        match input.recv_timeout(wait) {
            Ok(event) => {
                let due = timeline.due(Instant::now());
                kept.push_back((due, event));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => open = false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(conn: u64) -> NetEvent {
        NetEvent::Opened { conn, peer: None }
    }

    fn conn_of(e: &NetEvent) -> u64 {
        match e {
            NetEvent::Opened { conn, .. } => *conn,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn shape(delay: &str, jitter: &str, loss: &str, seed: &str) -> Option<Shape> {
        Shape::from_values(Some(delay), Some(jitter), Some(loss), None, Some(seed))
    }

    #[test]
    fn only_a_positive_number_of_milliseconds_switches_the_delay_on() {
        assert_eq!(None, parse(None));
        assert_eq!(None, parse(Some("")));
        assert_eq!(None, parse(Some("0")));
        assert_eq!(None, parse(Some("abc")));
        assert_eq!(None, parse(Some("-40")));
        assert_eq!(None, parse(Some("5001")));
        assert_eq!(Some(Duration::from_millis(40)), parse(Some("40")));
        assert_eq!(Some(Duration::from_millis(100)), parse(Some(" 100 ")));
        assert_eq!(None, parse_loss(Some("0")));
        assert_eq!(None, parse_loss(Some("-1")));
        assert_eq!(None, parse_loss(Some("51")));
        assert_eq!(None, parse_loss(Some("NaN")));
        assert_eq!(Some(1.5), parse_loss(Some("1.5")));
    }

    #[test]
    fn the_shape_is_off_unless_delay_jitter_or_loss_is_set_and_the_resend_covers_a_round_trip() {
        assert_eq!(None, Shape::from_values(None, None, None, Some("300"), Some("7")));
        assert_eq!(None, shape("0", "", "0", "1"));
        let s = shape("", "40", "", "").unwrap();
        assert_eq!((Duration::ZERO, Duration::from_millis(40), 0.0, 1), (s.delay, s.jitter, s.loss_pct, s.seed));
        let s = shape("150", "", "2", "9").unwrap();
        assert_eq!(Duration::from_millis(300), s.resend, "a loss is noticed a round trip later at the soonest");
        assert_eq!(9, s.seed);
        let s = Shape::from_values(Some("15"), None, Some("1"), Some("250"), None).unwrap();
        assert_eq!(Duration::from_millis(250), s.resend);
        assert!(s.describe().contains("FEVERLINK3_TEST_NET_LOSS_PCT=1"), "{}", s.describe());
    }

    /// The friend test's link (2026-10-18: round trip 26/30/71 ms p50/p90/max) as one end sees it: 15 ms one way, 12 ms of jitter, 1 %
    /// resent. The same seed gives the same waits; the order is kept, so a resent event holds the next ones up.
    #[test]
    fn jitter_and_loss_are_seeded_reproducible_and_keep_the_order() {
        let s = shape("15", "12", "1", "42").unwrap();
        let t0 = Instant::now();
        let run = |s: &Shape| {
            let mut tl = Timeline::new(s.clone());
            (0..2000u32).map(|i| tl.due(t0 + Duration::from_millis(u64::from(i) * 5))).collect::<Vec<_>>()
        };
        let a = run(&s);
        assert_eq!(a, run(&s), "the same seed, the same waits");
        assert_ne!(a, run(&shape("15", "12", "1", "43").unwrap()), "another seed, other waits");
        assert!(a.windows(2).all(|w| w[0] <= w[1]), "no event overtakes the one before it");
        let waits: Vec<f64> = a
            .iter()
            .enumerate()
            .map(|(i, due)| due.duration_since(t0 + Duration::from_millis(i as u64 * 5)).as_secs_f64() * 1000.0)
            .collect();
        assert!(waits.iter().all(|&w| w >= 15.0), "never sooner than the delay");
        let mut own = Timeline::new(s.clone());
        let alone: Vec<f64> = (0..2000).map(|_| own.wait().as_secs_f64() * 1000.0).collect();
        let resent = alone.iter().filter(|&&w| w >= 200.0).count();
        assert!((8..=35).contains(&resent), "about 1 % of 2000 resent: {resent}");
        let mut sorted = alone.clone();
        sorted.sort_by(f64::total_cmp);
        let p50 = sorted[1000];
        assert!((18.0..=24.0).contains(&p50), "median about delay + jitter/2: {p50}");
        // head-of-line: the events right after a resent one wait for it
        let i = alone.iter().position(|&w| w >= 200.0).unwrap();
        if i + 1 < waits.len() {
            assert!(waits[i + 1] >= waits[i] - 5.0 - 1e-6, "the next event waits behind the resent one");
        }
    }

    #[test]
    fn without_a_delay_events_pass_at_once() {
        let (tx, rx) = channel(None);
        tx.send(opened(1)).unwrap();
        assert_eq!(1, conn_of(&rx.try_recv().unwrap()));
    }

    #[test]
    fn with_a_delay_events_arrive_that_much_later_in_order_and_the_close_follows() {
        let delay = Duration::from_millis(60);
        let (tx, rx) = channel(shape("60", "", "", ""));
        let start = Instant::now();
        tx.send(opened(1)).unwrap();
        thread::sleep(Duration::from_millis(20));
        tx.send(opened(2)).unwrap();
        drop(tx);
        assert!(rx.try_recv().is_err(), "nothing before the delay");
        let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let t1 = start.elapsed();
        let second = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let t2 = start.elapsed();
        assert_eq!((1, 2), (conn_of(&first), conn_of(&second)));
        assert!(t1 >= delay, "first after {t1:?}");
        assert!(
            t2 >= delay + Duration::from_millis(20),
            "second after {t2:?}: its own arrival counts, not the first's"
        );
        // the transport's senders are gone and nothing is kept: the session sees the channel closed
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Err(RecvTimeoutError::Disconnected)));
    }

    #[test]
    fn a_lost_event_arrives_a_resend_later_and_holds_the_next_one_up() {
        // 50 % loss with a seed whose first draw loses: find one, so the test does not depend on luck
        let seed = (1..200u64)
            .find(|&seed| {
                let mut tl = Timeline::new(shape("10", "", "50", &seed.to_string()).unwrap());
                tl.wait() >= Duration::from_millis(200)
            })
            .unwrap();
        let (tx, rx) = channel(shape("10", "", "50", &seed.to_string()));
        let start = Instant::now();
        tx.send(opened(1)).unwrap();
        tx.send(opened(2)).unwrap();
        drop(tx);
        let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let t1 = start.elapsed();
        let second = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!((1, 2), (conn_of(&first), conn_of(&second)), "the order is kept");
        assert!(t1 >= Duration::from_millis(210), "resent: delay plus the resend time ({t1:?})");
    }
}
