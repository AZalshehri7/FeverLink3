//! A check of the Steam transport that needs one Steam account and no second PC.
//!
//! The host transport listens on virtual port 0 and the client transport connects to the same account: a
//! connection to one's own Steam ID works inside one process (the probe proved it on 2026-10-09). It cannot show how
//! NATs and relays behave, only a second account can. What it does run is the code the sessions run, against the real
//! Steam library: connecting, the order of events, round trips through the poller, messages from the size of a
//! savegame chunk up to the biggest Steam takes, the send buffer filling and draining, and a close that must not
//! overtake the frame written before it. A failure here is a bug in the transport or a wrong assumption about Steam,
//! found in a minute on one PC instead of in a test between two.
//!
//! [`loopback_check`] blocks for up to a minute or so: run it on a thread of its own.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;

use feverlink3_core::protocol::FrameHeader;

use super::*;

/// How long one step waits for something to arrive.
const STEP_WAIT: Duration = Duration::from_secs(10);
/// Round trips measured.
const ROUND_TRIPS: u32 = 30;
/// Chunk-sized frames streamed: about 14 MB.
const STREAM_FRAMES: u32 = 40;
/// Body of a savegame chunk frame at its biggest (`largest_chunk_frame` in the tests): 256 KiB of data, base64,
/// JSON, tag.
const CHUNK_BODY: usize = 349_704 - FrameCodec::LENGTH_FIELD_SIZE - FrameHeader::SIZE;

/// One line of the check: what was tried, whether it held, and the numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

fn frame(kind: u8, counter: u64, body: Vec<u8>) -> Frame {
    Frame::new(FrameHeader::new(1, kind, 0, counter), body)
}

/// The next event on `rx`, or what went wrong waiting for it.
fn next(rx: &Receiver<NetEvent>, what: &str) -> Result<NetEvent, String> {
    rx.recv_timeout(STEP_WAIT).map_err(|e| match e {
        RecvTimeoutError::Timeout => {
            format!("nothing arrived within {} s while waiting for {what}", STEP_WAIT.as_secs())
        }
        RecvTimeoutError::Disconnected => format!("the event channel closed while waiting for {what}"),
    })
}

fn next_frame(rx: &Receiver<NetEvent>, conn: ConnectionId, what: &str) -> Result<Frame, String> {
    match next(rx, what)? {
        NetEvent::Frame { conn: c, frame } if c == conn => Ok(frame),
        other => Err(format!("expected {what} on connection {conn}, got {}", brief(&other))),
    }
}

fn brief(event: &NetEvent) -> String {
    match event {
        NetEvent::Opened { conn, .. } => format!("Opened on {conn}"),
        NetEvent::Frame { conn, frame } => format!("a frame of {} bytes on {conn}", frame.body.len()),
        NetEvent::Closed { conn, reason } => format!("Closed on {conn} ({reason})"),
    }
}

/// Records a step; a failed step ends the check.
macro_rules! step {
    ($checks:expr, $name:expr, $result:expr) => {
        match $result {
            Ok(detail) => $checks.push(Check { name: $name.to_string(), ok: true, detail }),
            Err(detail) => {
                $checks.push(Check { name: $name.to_string(), ok: false, detail });
                return;
            }
        }
    };
}

/// Runs the self-check on `core`. Every step that ran is in the result, in order; the last one failed unless all held.
pub fn loopback_check(core: &CoreRef) -> Vec<Check> {
    let mut checks = Vec::new();
    run(core, &mut checks);
    checks
}

fn run(core: &CoreRef, checks: &mut Vec<Check>) {
    let own = core.status().steam_id.map(SteamEndpoint);
    let who = own.map(|e| e.to_string()).ok_or_else(|| "Steam has not said who this account is".to_string());
    step!(checks, "this account", who);
    let Some(own) = own else { return };

    // the host transport, as a session would start it
    let (htx, hrx) = mpsc::channel();
    let host = SteamHostNet::listen_on(core, htx, Duration::from_secs(5), 4);
    step!(
        checks,
        "listen on virtual port 0",
        host.as_ref().map(|_| "listening".to_string()).map_err(|e| e.to_string())
    );
    let Ok(host) = host else { return };

    // the client transport, connecting to ourselves, with the progress texts a player would see
    let (ctx, crx) = mpsc::channel();
    let started = Instant::now();
    let mut texts: Vec<String> = Vec::new();
    let client = SteamClientNet::connect_with(
        core,
        own,
        ConnectContext {
            conn: 1,
            tx: ctx,
            timeout: Duration::from_secs(25),
            cancelled: &|| false,
            progress: &mut |text| texts.push(text.to_string()),
        },
    );
    let detail = format!("{} ms: {}", started.elapsed().as_millis(), texts.join(" > "));
    step!(checks, "connect to this account", client.as_ref().map(|_| detail).map_err(|e| e.to_string()));
    let Ok(client) = client else { return };

    // the first event on each side is Opened, and the host numbers its connection 1
    let opened = (|| {
        match next(&crx, "the client's Opened")? {
            NetEvent::Opened { conn: 1, .. } => {}
            other => return Err(format!("the client's first event was {}", brief(&other))),
        }
        match next(&hrx, "the host's Opened")? {
            NetEvent::Opened { conn: 1, peer: None } => Ok("both sides, connection 1".to_string()),
            other => Err(format!("the host's first event was {}", brief(&other))),
        }
    })();
    step!(checks, "both ends report the connection", opened);

    // round trips through both pollers and Steam's loopback path
    let round_trips = (|| {
        let mut times = Vec::new();
        for i in 0..ROUND_TRIPS {
            let sent = Instant::now();
            client.send(&frame(30, u64::from(i), vec![i as u8; 64])).map_err(|e| format!("send: {e}"))?;
            let got = next_frame(&hrx, 1, "a request")?;
            host.send(1, &frame(11, u64::from(i), got.body)).map_err(|e| format!("send back: {e}"))?;
            let back = next_frame(&crx, 1, "the answer")?;
            if back.body != vec![i as u8; 64] || back.header.counter != u64::from(i) {
                return Err(format!("round trip {i} came back changed"));
            }
            times.push(sent.elapsed());
        }
        times.sort_unstable();
        let ms = |d: Option<&Duration>| format!("{:.1}", d.copied().unwrap_or_default().as_secs_f64() * 1000.0);
        Ok(format!(
            "{ROUND_TRIPS} round trips, fastest {} ms, median {} ms, slowest {} ms",
            ms(times.first()),
            ms(times.get(times.len() / 2)),
            ms(times.last())
        ))
    })();
    step!(checks, "round trips", round_trips);

    // savegame-chunk-sized frames, more than Steam's send buffer holds, in order and intact
    let streamed = (|| {
        let started = Instant::now();
        let outcome = thread::scope(|s| {
            let sender = s.spawn(|| {
                for i in 0..STREAM_FRAMES {
                    let mut body = vec![(i % 251) as u8; CHUNK_BODY];
                    body[..4].copy_from_slice(&i.to_be_bytes());
                    client.send(&frame(18, u64::from(i), body)).map_err(|e| format!("send {i}: {e}"))?;
                }
                Ok::<(), String>(())
            });
            let mut received: Result<(), String> = Ok(());
            for i in 0..STREAM_FRAMES {
                match next_frame(&hrx, 1, "a chunk") {
                    Ok(f) if f.body.len() == CHUNK_BODY && f.body.starts_with(&i.to_be_bytes()) => {}
                    Ok(f) => {
                        received = Err(format!("chunk {i} arrived wrong: {} bytes", f.body.len()));
                        break;
                    }
                    Err(e) => {
                        received = Err(e);
                        break;
                    }
                }
            }
            let sent = sender.join().unwrap_or_else(|_| Err("the sender thread failed".to_string()));
            received.and(sent)
        });
        outcome?;
        let seconds = started.elapsed().as_secs_f64().max(0.001);
        let megabytes = f64::from(STREAM_FRAMES) * (CHUNK_BODY as f64) / 1_000_000.0;
        Ok(format!(
            "{STREAM_FRAMES} frames of {CHUNK_BODY} bytes, {megabytes:.1} MB in {seconds:.2} s ({:.1} MB/s)",
            megabytes / seconds
        ))
    })();
    step!(checks, "chunk-sized frames, in order", streamed);

    // the biggest frame Steam takes, and one byte more
    let limits = (|| {
        let biggest = MAX_MESSAGE - FrameCodec::LENGTH_FIELD_SIZE - FrameHeader::SIZE;
        host.send(1, &frame(18, 1, vec![0x5A; biggest])).map_err(|e| format!("the biggest frame: {e}"))?;
        let got = next_frame(&crx, 1, "the biggest frame")?;
        if got.body.len() != biggest || got.body.iter().any(|b| *b != 0x5A) {
            return Err("the biggest frame arrived changed".into());
        }
        match host.send(1, &frame(18, 2, vec![0; biggest + 1])) {
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {}
            other => return Err(format!("one byte more was not refused as too big: {other:?}")),
        }
        host.send(1, &frame(11, 3, b"still open".to_vec())).map_err(|e| format!("after the refusal: {e}"))?;
        next_frame(&crx, 1, "a frame after the refusal")?;
        Ok(format!("{biggest} body bytes cross whole, one more is refused and the connection stays open"))
    })();
    step!(checks, "biggest frame and the limit", limits);

    // what is written before a close arrives before the close (a reject must reach the player)
    let lingering = (|| {
        host.send(1, &frame(3, 4, b"bad-proof".to_vec())).map_err(|e| format!("send: {e}"))?;
        host.close(1);
        let got = next_frame(&crx, 1, "the frame written before the close")?;
        if got.body != b"bad-proof" {
            return Err("the frame written before the close arrived changed".into());
        }
        match next(&crx, "the end of the connection")? {
            NetEvent::Closed { conn: 1, reason } => Ok(format!("the frame came first, then: {reason}")),
            other => Err(format!("expected the end of the connection, got {}", brief(&other))),
        }
    })();
    step!(checks, "close does not overtake a frame", lingering);

    // the host side ended too, once, and nothing is left behind
    let ended = (|| match next(&hrx, "the host's end of the connection")? {
        NetEvent::Closed { conn: 1, reason } => {
            if host.connection_count() != 0 || client.send(&frame(30, 5, vec![1])).is_ok() {
                return Err("the connection is still usable after its end".into());
            }
            Ok(format!("the host reports: {reason}"))
        }
        other => Err(format!("expected the host's end of the connection, got {}", brief(&other))),
    })();
    step!(checks, "both ends report the end once", ended);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steamnet::fake::FakeMachine;

    fn lines(checks: &[Check]) -> String {
        checks
            .iter()
            .map(|c| format!("{} {}: {}", if c.ok { "ok  " } else { "FAIL" }, c.name, c.detail))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_whole_check_passes_against_the_fake_steam() {
        let machine = FakeMachine::new();
        let core = machine.core();
        let checks = loopback_check(&core.handle());
        let report = lines(&checks);
        assert!(checks.len() == 9 && checks.iter().all(|c| c.ok), "{report}");
        // the numbers a person reads are there
        assert!(checks[0].detail == machine.steam_id().to_string(), "{report}");
        assert!(checks[2].detail.contains("contacting the host"), "{report}");
        assert!(checks[4].detail.starts_with("30 round trips, fastest"), "{report}");
        assert!(checks[5].detail.contains("40 frames of"), "{report}");
        assert!(checks[6].detail.contains("524272 body bytes"), "{report}");
        // it leaves nothing behind: the listen socket is closed and the connections are gone
        assert_eq!(machine.listeners(), 0, "{report}");
        assert!(machine.connections().is_empty(), "{report}");
        // the biggest message the check sent is exactly Steam's limit
        assert_eq!(machine.largest_sent(), MAX_MESSAGE);
    }

    #[test]
    fn the_check_says_what_stopped_it() {
        // Steam has not said who this account is
        let machine = FakeMachine::new();
        machine.set_ready(false);
        let core = machine.core_with(crate::steamnet::fake::fast());
        let checks = loopback_check(&core.handle());
        assert_eq!(checks.len(), 1);
        assert!(!checks[0].ok && checks[0].detail.contains("has not said who"), "{}", lines(&checks));

        // virtual port 0 is taken (a session is being hosted)
        let machine = FakeMachine::new();
        let core = machine.core();
        let (tx, _rx) = mpsc::channel();
        let _hosting = SteamHostNet::listen_on(&core.handle(), tx, Duration::from_secs(5), 4).expect("listen");
        let checks = loopback_check(&core.handle());
        assert_eq!(checks.len(), 2, "{}", lines(&checks));
        assert!(checks[0].ok && !checks[1].ok && checks[1].detail.contains("virtual port 0"), "{}", lines(&checks));
    }
}
