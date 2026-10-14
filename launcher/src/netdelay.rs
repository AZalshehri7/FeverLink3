//! A TEST-ONLY network delay between the games: `FEVERLINK3_TEST_NET_DELAY_MS=<one-way ms>`.
//!
//! Two games on one PC talk over loopback, which hides the network time a real session has: the stamp margin of a build (how far
//! past the fastest clock a build is stamped, so that every game has it before it reaches the stamp) cannot be proven there. With
//! the variable set, every event a session's transport delivers (frames, opened, closed, bulk files) reaches the session thread
//! that many milliseconds later, in order. Each end delays what it receives, so a host and a player that both have the variable
//! see a one-way delay of that value in each direction (a round trip of twice it plus loopback), measured by the host's pings like
//! a real link. Unset, empty, 0 or not a number: nothing changes (the transport's channel is used directly, no thread).

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::net::NetEvent;

/// The environment variable (test switch, never set by a player).
pub const ENV: &str = "FEVERLINK3_TEST_NET_DELAY_MS";
/// The largest delay accepted: a longer one is a typing error, not a network.
pub const MAX_MS: u64 = 5000;

/// The one-way delay a value of [`ENV`] asks for; `None` for unset, empty, 0, over [`MAX_MS`] or not a number.
pub fn parse(value: Option<&str>) -> Option<Duration> {
    let ms: u64 = value?.trim().parse().ok()?;
    (1..=MAX_MS).contains(&ms).then(|| Duration::from_millis(ms))
}

/// The delay this process was started with.
pub fn from_env() -> Option<Duration> {
    parse(std::env::var(ENV).ok().as_deref())
}

/// The channel a session's transport sends its events into and the session thread reads from. Without a delay it is a plain
/// channel. With one, a forwarding thread takes each event as it arrives, keeps it until `delay` has passed since its arrival, and
/// passes it on in arrival order; the thread ends when the transport's senders are gone and everything kept has been passed on
/// (the session thread then sees the channel closed, as without the delay).
pub fn channel(delay: Option<Duration>) -> (Sender<NetEvent>, Receiver<NetEvent>) {
    let Some(delay) = delay else { return mpsc::channel() };
    let (in_tx, in_rx) = mpsc::channel::<NetEvent>();
    let (out_tx, out_rx) = mpsc::channel::<NetEvent>();
    let spawned =
        thread::Builder::new().name("feverlink3-test-delay".into()).spawn(move || forward(&in_rx, &out_tx, delay));
    if spawned.is_err() {
        // no thread: no delay rather than no session
        return mpsc::channel();
    }
    (in_tx, out_rx)
}

fn forward(input: &Receiver<NetEvent>, output: &Sender<NetEvent>, delay: Duration) {
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
            Ok(event) => kept.push_back((Instant::now() + delay, event)),
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
        let (tx, rx) = channel(Some(delay));
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
}
