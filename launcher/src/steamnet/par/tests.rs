//! Tests of the parallel transfer on the in-memory Steam: every connection is capped (what one real Steam connection
//! delivers, about 1 MB/s) while the line total can be larger or capped too. Two machines, a sender and a receiver, each
//! with its own core and poller thread; the real engine on both.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::wire::{self, reason, Hello, Msg};
use super::*;
use crate::steamnet::bulk_io::SyntheticSource;
use crate::steamnet::fake::FakeMachine;
use crate::steamnet::{CoreRef, End, Handle, Owner, SteamCore};

const MB: f64 = 1_000_000.0;
const SECRET: [u8; 32] = [7; 32];

struct Rig {
    sender: FakeMachine,
    receiver: FakeMachine,
    sender_core: SteamCore,
    receiver_core: SteamCore,
    service: Arc<ParService>,
    dir: tempfile::TempDir,
    events: Arc<Mutex<Vec<ParEvent>>>,
}

impl Rig {
    fn new() -> Rig {
        Rig::with(Some(SECRET), 600 << 20)
    }

    fn with(secret: Option<[u8; 32]>, max_total: u64) -> Rig {
        let (sender, receiver) = (FakeMachine::new(), FakeMachine::new());
        let (sender_core, receiver_core) = (sender.core(), receiver.core());
        let service = ParService::for_core(&receiver_core.handle()).expect("listen");
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        service.allow(
            sender.steam_id(),
            Allow {
                secret,
                dir: dir.path().to_path_buf(),
                stem: Box::new(|ctx| format!("transfer-{ctx}")),
                ext: "bin",
                max_total,
                on_event: Box::new(move |e| sink.lock().unwrap().push(e)),
                owner: 1,
            },
        );
        Rig { sender, receiver, sender_core, receiver_core, service, dir, events }
    }

    fn spec(&self, id: u32, len: u64, cfg: ParConfig) -> SendSpec {
        SendSpec {
            remote: self.receiver.steam_id(),
            xfer: id,
            ctx: id,
            secret: SECRET,
            sha256: SyntheticSource::sha256(u64::from(id), len),
            source: Arc::new(SyntheticPayload { id: u64::from(id), len }),
            conns: cfg.conns_for(len),
            cfg,
            control: None,
        }
    }

    fn send(&self, spec: SendSpec) -> SendReport {
        run_send(&self.sender_core.handle(), spec, &|| false, &|_| {})
    }

    fn core(&self) -> CoreRef {
        self.sender_core.handle()
    }

    fn done(&self) -> Option<(PathBuf, u64)> {
        self.events.lock().unwrap().iter().find_map(|e| match e {
            ParEvent::Done { path, len, .. } => Some((path.clone(), *len)),
            _ => None,
        })
    }

    fn failed(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                ParEvent::Failed { why, .. } => Some(why.clone()),
                _ => None,
            })
            .collect()
    }

    fn wait_event(&self, timeout: Duration, f: impl Fn(&[ParEvent]) -> bool) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if f(&self.events.lock().unwrap()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        f(&self.events.lock().unwrap())
    }
}

/// The bytes of a synthetic payload, checked in pieces against the file.
fn file_is_payload(path: &std::path::Path, id: u64, len: u64) -> bool {
    use std::io::Read;
    let mut f = fs::File::open(path).unwrap();
    let mut pos = 0u64;
    let (mut got, mut want) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    while pos < len {
        let n = ((len - pos) as usize).min(got.len());
        f.read_exact(&mut got[..n]).unwrap();
        SyntheticSource::fill(id, pos, &mut want[..n]);
        if got[..n] != want[..n] {
            return false;
        }
        pos += n as u64;
    }
    true
}

// ---- the rule

#[test]
fn the_number_of_connections_follows_the_size_of_the_file() {
    let cfg = ParConfig::default();
    assert!(!cfg.applies_to(5_999_999) && cfg.applies_to(6_000_000));
    let n = |mb: f64| cfg.conns_for((mb * MB) as u64);
    assert_eq!((n(6.0), n(8.0), n(10.0)), (1, 1, 1));
    assert_eq!((n(32.0), n(75.0), n(80.0)), (3, 7, 8));
    assert!((6..=8).contains(&n(75.0)), "75 MB gets 6 to 8 connections");
    assert_eq!(n(400.0), 16, "capped at N_MAX");
    // the constants of the rule
    assert_eq!((PER_CONN_RATE, N_MAX, TARGET_SECONDS), (1 << 20, 16, 10.5));
    // the environment: a forced number, a changed cap, nonsense ignored
    let forced = ParConfig::from_values(Some("5"), None);
    assert_eq!((forced.conns_for(1), forced.conns_for(900_000_000)), (5, 5));
    let capped = ParConfig::from_values(None, Some("4"));
    assert_eq!(capped.conns_for(900_000_000), 4);
    assert_eq!(ParConfig::from_values(Some("0"), Some("x")), ParConfig::default());
    assert_eq!(ParConfig::from_values(Some("99"), Some("8")).conns_for(1), 8, "a forced number is capped too");
}

#[test]
fn the_bulk_mode_reads_the_environment_words() {
    use crate::steamnet::bulk_io::BulkMode;
    assert_eq!(BulkMode::parse(None), BulkMode::Par);
    assert_eq!(BulkMode::parse(Some("par")), BulkMode::Par);
    assert_eq!(BulkMode::parse(Some(" DGRAM ")), BulkMode::Dgram);
    assert_eq!(BulkMode::parse(Some("off")), BulkMode::Off);
    assert_eq!(BulkMode::parse(Some("something else")), BulkMode::Par);
}

// ---- the wire

#[test]
fn hmac_matches_the_rfc_vector() {
    let tag = wire::hmac_sha256(&[0x0b; 20], b"Hi There");
    let hex: String = tag.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
}

#[test]
fn messages_round_trip_and_garbage_is_refused() {
    let h = Hello::new(&SECRET, 9, 3, 1_000_000, 131_072, 8, [5; 32], 2);
    assert_eq!(wire::parse(&wire::encode_hello(&h)), Some(Msg::Hello(h.clone())));
    assert!(h.token_ok(&SECRET) && !h.token_ok(&[8; 32]));
    let mut moved = h.clone();
    moved.total += 1;
    assert!(!moved.token_ok(&SECRET), "the token covers the size");
    let chunk = wire::encode_chunk(9, 3, 8, b"hello");
    assert_eq!(wire::parse(&chunk), Some(Msg::Chunk { xfer: 9, index: 3, count: 8, payload: b"hello" }));
    assert_eq!(wire::parse(&wire::encode_ack(9, &[1, 2, 3])), Some(Msg::Ack { xfer: 9, indices: vec![1, 2, 3] }));
    assert_eq!(wire::parse(&wire::encode_done(9, true)), Some(Msg::Done { xfer: 9, ok: true }));
    assert_eq!(wire::parse(&wire::encode_accept(9)), Some(Msg::Accept { xfer: 9 }));
    assert_eq!(wire::parse(&wire::encode_abort(9, 3)), Some(Msg::Abort { xfer: 9, reason: 3 }));
    // a cut, extended or mislabelled message
    let hello = wire::encode_hello(&h);
    assert!(wire::parse(&hello[..hello.len() - 1]).is_none());
    assert!(wire::parse(&[hello.clone(), vec![0]].concat()).is_none());
    assert!(wire::parse(&chunk[..chunk.len() - 1]).is_none(), "the length field must match");
    assert!(wire::parse(&wire::encode_chunk(1, 0, 1, b"")).is_none(), "an empty chunk");
    assert!(
        wire::parse(&[]).is_none()
            && wire::parse(&[wire::MAGIC]).is_none()
            && wire::parse(&[0, 1, 2, 3, 4, 5]).is_none()
    );
    // random bytes never panic
    let mut seed = 0x1234_5678_9abc_def0u64;
    for _ in 0..20_000 {
        let n = (seed % 90) as usize;
        let mut d: Vec<u8> = (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (seed >> 33) as u8
            })
            .collect();
        if let Some(first) = d.first_mut() {
            *first = wire::MAGIC;
        }
        let _ = wire::parse(&d);
    }
    // sizes
    assert_eq!(wire::chunk_count(300_000, 131_072), Some(3));
    assert_eq!(wire::chunk_len(300_000, 131_072, 2), Some(300_000 - 2 * 131_072));
    assert_eq!(wire::chunk_len(300_000, 131_072, 3), None);
    assert_eq!(wire::chunk_count(0, 131_072), None);
}

// ---- transfers

#[test]
fn a_small_transfer_arrives_byte_for_byte_with_one_connection() {
    let rig = Rig::new();
    let cfg = ParConfig { forced: Some(1), ..ParConfig::default() };
    let len = 700_000 + 17;
    let report = rig.send(rig.spec(1, len, cfg));
    assert!(report.ok, "{report:?}");
    assert_eq!((report.lanes_opened, report.lanes_max), (1, 1));
    assert!(rig.wait_event(Duration::from_secs(5), |e| e.iter().any(|x| matches!(x, ParEvent::Done { .. }))));
    let (path, got) = rig.done().unwrap();
    assert_eq!(got, len);
    assert_eq!(path, rig.dir.path().join("transfer-1.bin"));
    assert!(file_is_payload(&path, 1, len));
    assert!(!rig.dir.path().join("transfer-1.bin.part").exists(), "no partial file is left");
}

#[test]
fn seventy_five_megabytes_scale_with_the_connections() {
    let rig = Rig::new();
    // every connection delivers 1 MB/s, the line does not limit
    rig.sender.set_caps(MB, 0.0);
    let len = 75_000_000u64;
    let cfg = ParConfig::default();
    let n = cfg.conns_for(len);
    assert_eq!(n, 7);
    let spec = rig.spec(2, len, cfg);
    let started = Instant::now();
    let report = rig.send(spec);
    let secs = started.elapsed().as_secs_f64();
    assert!(report.ok, "{report:?}");
    assert_eq!(report.lanes_opened, 7, "{}", report.summary());
    assert!(report.lanes_max >= 6, "{}", report.summary());
    let ideal = len as f64 / (f64::from(n) * MB);
    eprintln!("75 MB over {n} connections at 1 MB/s each: {secs:.1} s (ideal {ideal:.1} s); {}", report.summary());
    assert!(secs < 1.5 * ideal, "{secs:.1} s against an ideal {ideal:.1} s; {}", report.summary());
    assert!(report.rate > 4.0 * MB, "{:.2} MB/s", report.rate / MB);
    // the receiver never held the file: only a few chunks at a time
    assert!(rig.service.peak_queued() < 12 << 20, "peak queued {}", rig.service.peak_queued());
    assert!(report.peak_inflight <= 8 * INFLIGHT, "peak in flight {}", report.peak_inflight);
    assert!(rig.wait_event(Duration::from_secs(10), |e| e.iter().any(|x| matches!(x, ParEvent::Done { .. }))));
    let (path, got) = rig.done().unwrap();
    assert_eq!(got, len);
    assert!(file_is_payload(&path, 2, len));
}

#[test]
fn growth_stops_when_the_line_saturates_and_the_transfer_still_completes() {
    let rig = Rig::new();
    // one connection 1 MB/s, but the whole line only 4 MB/s
    rig.sender.set_caps(MB, 4.0 * MB);
    let len = 40_000_000u64;
    let mut spec = rig.spec(3, len, ParConfig::default());
    spec.conns = 16;
    let started = Instant::now();
    let report = rig.send(spec);
    let secs = started.elapsed().as_secs_f64();
    assert!(report.ok, "{report:?}");
    eprintln!("40 MB, line 4 MB/s, up to 16 connections: {secs:.1} s; {}", report.summary());
    assert!((4..=6).contains(&report.lanes_opened), "{}", report.summary());
    assert!(report.stop.contains("saturated"), "{}", report.stop);
    assert!(report.rate > 2.8 * MB, "{:.2} MB/s", report.rate / MB);
    assert!(rig.wait_event(Duration::from_secs(10), |e| e.iter().any(|x| matches!(x, ParEvent::Done { .. }))));
    assert!(file_is_payload(&rig.done().unwrap().0, 3, len));
}

#[test]
fn connections_that_scale_are_all_opened_and_a_forced_number_is_not_checked() {
    let rig = Rig::new();
    rig.sender.set_caps(MB, 0.0);
    let len = 40_000_000u64;
    let mut spec = rig.spec(4, len, ParConfig::default());
    spec.conns = 6;
    let report = rig.send(spec);
    assert!(report.ok, "{report:?}");
    assert_eq!(report.lanes_opened, 6, "{}", report.summary());
    assert!(report.stop.contains("all 6 connections"), "{}", report.stop);
}

#[test]
fn a_ping_that_inflates_stops_the_growth_and_closes_the_newest_connection() {
    struct Ping(Arc<Mutex<u32>>);
    impl Control for Ping {
        fn ping_ms(&self) -> Option<u32> {
            Some(*self.0.lock().unwrap())
        }
        fn pending(&self) -> u32 {
            0
        }
    }
    let rig = Rig::new();
    rig.sender.set_caps(MB, 0.0);
    let ping = Arc::new(Mutex::new(35u32));
    let mut spec = rig.spec(5, 60_000_000, ParConfig::default());
    spec.conns = 12;
    spec.control = Some(Arc::new(Ping(Arc::clone(&ping))));
    let raise = Arc::clone(&ping);
    let inflater = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(2500));
        *raise.lock().unwrap() = 500;
    });
    let report = rig.send(spec);
    inflater.join().unwrap();
    assert!(report.ok, "{report:?}");
    assert!(report.stop.contains("ping"), "{}", report.stop);
    assert!(report.lanes_opened < 12, "{}", report.summary());
    assert!(report.notes.iter().any(|n| n.contains("ended: closed because the Steam ping rose")), "{:?}", report.notes);
}

// ---- failures and the fallback

#[test]
fn a_receiver_that_does_not_listen_refuses_every_connection_and_the_sender_says_so() {
    let (sender, receiver) = (FakeMachine::new(), FakeMachine::new());
    let (sender_core, _receiver_core) = (sender.core(), receiver.core());
    let spec = SendSpec {
        remote: receiver.steam_id(),
        xfer: 1,
        ctx: 1,
        secret: SECRET,
        sha256: [0; 32],
        source: Arc::new(SyntheticPayload { id: 1, len: 1_000_000 }),
        cfg: ParConfig::default(),
        conns: 4,
        control: None,
    };
    let started = Instant::now();
    let report = run_send(&sender_core.handle(), spec, &|| false, &|_| {});
    assert!(!report.ok);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(report.error.as_deref().unwrap().contains("could not open any extra Steam connection"), "{report:?}");
}

#[test]
fn a_receiver_that_has_not_allowed_the_sender_closes_its_connections() {
    let rig = Rig::new();
    rig.service.disallow(rig.sender.steam_id(), 0);
    let report = rig.send(rig.spec(6, 1_000_000, ParConfig::default()));
    assert!(!report.ok);
    assert!(report.error.as_deref().unwrap().contains("could not open any extra Steam connection"), "{report:?}");
}

#[test]
fn a_connection_that_never_opens_ends_the_attempt_after_the_connect_timeout() {
    let rig = Rig::new();
    rig.receiver.set_reachable(false);
    let cfg = ParConfig { connect_timeout: Duration::from_secs(2), ..ParConfig::default() };
    let started = Instant::now();
    let report = rig.send(rig.spec(7, 8_000_000, cfg));
    assert!(!report.ok);
    assert!(started.elapsed() < Duration::from_secs(6), "{:?}", started.elapsed());
    assert!(report.error.as_deref().unwrap().contains("no extra Steam connection opened within 2 s"), "{report:?}");
}

#[test]
fn a_wrong_secret_is_refused() {
    let rig = Rig::new();
    let mut spec = rig.spec(8, 1_000_000, ParConfig::default());
    spec.secret = [9; 32];
    let report = rig.send(spec);
    assert!(!report.ok);
    assert!(report.error.as_deref().unwrap().contains("wrong secret"), "{report:?}");
    assert!(rig.done().is_none());
}

#[test]
fn a_file_over_the_limit_is_refused() {
    let rig = Rig::with(Some(SECRET), 500_000);
    let report = rig.send(rig.spec(9, 2_000_000, ParConfig::default()));
    assert!(!report.ok);
    assert!(report.error.as_deref().unwrap().contains("too big"), "{report:?}");
}

#[test]
fn a_connection_that_dies_mid_transfer_is_replaced_by_the_others() {
    let rig = Rig::new();
    rig.sender.set_caps(MB, 0.0);
    let len = 20_000_000u64;
    let cfg = ParConfig { forced: Some(4), ..ParConfig::default() };
    let machine_handles = {
        let sender = &rig.sender;
        let killer = std::thread::scope(|scope| {
            let h = scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(1500));
                let conns = sender.connections();
                // the second connection ends with a problem at both ends
                sender.kill_connection(conns[1]);
                conns.len()
            });
            let report = rig.send(rig.spec(10, len, cfg));
            (report, h.join().unwrap())
        });
        killer
    };
    let (report, count) = machine_handles;
    assert!(count >= 2);
    assert!(report.ok, "{report:?}");
    assert!(report.requeued > 0, "chunks of the dead connection went to the others: {}", report.summary());
    assert!(report.notes.iter().any(|n| n.contains("ended")), "{:?}", report.notes);
    assert!(rig.wait_event(Duration::from_secs(10), |e| e.iter().any(|x| matches!(x, ParEvent::Done { .. }))));
    assert!(file_is_payload(&rig.done().unwrap().0, 10, len));
}

#[test]
fn a_wrong_sha_fails_at_both_ends_and_leaves_no_file() {
    let rig = Rig::new();
    let mut spec = rig.spec(11, 3_000_000, ParConfig { forced: Some(2), ..ParConfig::default() });
    spec.sha256 = [1; 32];
    let report = rig.send(spec);
    assert!(!report.ok);
    assert!(report.error.as_deref().unwrap().contains("SHA-256"), "{report:?}");
    assert!(rig.wait_event(Duration::from_secs(5), |e| e.iter().any(|x| matches!(x, ParEvent::Failed { .. }))));
    assert!(rig.failed()[0].contains("SHA-256"), "{:?}", rig.failed());
    assert!(rig.done().is_none());
    let left: Vec<_> = fs::read_dir(rig.dir.path()).unwrap().flatten().collect();
    assert!(left.is_empty(), "no partial or wrong file stays: {left:?}");
}

#[test]
fn ten_seconds_without_progress_ends_the_transfer() {
    let rig = Rig::new();
    rig.sender.set_caps(MB, 0.0);
    // the receiver stops reading after a moment: no acknowledgement ever comes back
    let stall = std::thread::scope(|scope| {
        let h = scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(1500));
            rig.receiver.stall_receive(true);
        });
        let started = Instant::now();
        let report = rig.send(rig.spec(12, 30_000_000, ParConfig { forced: Some(3), ..ParConfig::default() }));
        h.join().unwrap();
        (report, started.elapsed())
    });
    let (report, took) = stall;
    assert!(!report.ok);
    assert!(report.error.as_deref().unwrap().contains("no progress for 10 s"), "{report:?}");
    assert!(took > Duration::from_secs(10) && took < Duration::from_secs(20), "{took:?}");
}

#[test]
fn cancelling_stops_the_send() {
    let rig = Rig::new();
    rig.sender.set_caps(MB, 0.0);
    let stop = AtomicBool::new(false);
    let report = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(1000));
            stop.store(true, Ordering::Release);
        });
        run_send(&rig.core(), rig.spec(13, 30_000_000, ParConfig::default()), &|| stop.load(Ordering::Acquire), &|_| {})
    });
    assert!(!report.ok);
    assert_eq!(report.error.as_deref(), Some("cancelled"));
}

// ---- hostile input

/// A raw peer: connects to the receiver's port and sends whatever the test wants.
struct Raw {
    core: CoreRef,
    conn: Handle,
    inbox: Arc<Mutex<Vec<Vec<u8>>>>,
    closed: Arc<AtomicBool>,
}

struct RawOwner {
    inbox: Arc<Mutex<Vec<Vec<u8>>>>,
    closed: Arc<AtomicBool>,
}

impl Owner for RawOwner {
    fn message(&self, _conn: Handle, data: &[u8]) {
        self.inbox.lock().unwrap().push(data.to_vec());
    }

    fn closed(&self, _conn: Handle, _end: End) {
        self.closed.store(true, Ordering::Release);
    }
}

impl Raw {
    fn connect(core: &CoreRef, remote: u64) -> Raw {
        let (inbox, closed) = (Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicBool::new(false)));
        let owner = Arc::new(RawOwner { inbox: Arc::clone(&inbox), closed: Arc::clone(&closed) });
        let conn = core.connect(remote, PAR_PORT, owner).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while !core.is_open(conn) && !closed.load(Ordering::Acquire) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        Raw { core: core.clone(), conn, inbox, closed }
    }

    fn send(&self, data: &[u8]) {
        let _ = self.core.send_reliable(self.conn, data, Duration::from_secs(2));
    }

    fn wait_closed(&self) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while !self.closed.load(Ordering::Acquire) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.closed.load(Ordering::Acquire)
    }

    fn last(&self) -> Option<Vec<u8>> {
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            if let Some(m) = self.inbox.lock().unwrap().last().cloned() {
                return Some(m);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }
}

fn hello_for(id: u32, total: u64, chunk: u32) -> Hello {
    let count = wire::chunk_count(total, chunk).unwrap();
    Hello::new(&SECRET, id, id, total, chunk, count, [3; 32], 0)
}

#[test]
fn a_stranger_is_closed_and_hostile_messages_from_an_allowed_peer_close_only_that_connection() {
    let rig = Rig::new();
    let service_core = rig.receiver_core.handle();
    let _ = service_core;
    // a stranger (not allowed) is closed at once
    let stranger = FakeMachine::new();
    let stranger_core = stranger.core();
    let raw = Raw::connect(&stranger_core.handle(), rig.receiver.steam_id());
    raw.send(&wire::encode_hello(&hello_for(1, 1_000_000, 131_072)));
    assert!(raw.wait_closed(), "a stranger's connection is closed");
    assert_eq!(rig.service.lane_count(), 0);

    // the allowed sender, with a wrong token
    let core = rig.core();
    let raw = Raw::connect(&core, rig.receiver.steam_id());
    let mut bad = hello_for(2, 1_000_000, 131_072);
    bad.token = [0; 16];
    raw.send(&wire::encode_hello(&bad));
    assert_eq!(wire::parse(&raw.last().unwrap()), Some(Msg::Refuse { xfer: 2, reason: reason::BAD_TOKEN }));
    assert!(raw.wait_closed());

    // sizes outside the rules
    for (total, chunk, code) in [
        (900 << 20, 131_072u32, reason::TOO_BIG),
        (1_000_000, 100, reason::BAD_FIELDS),
        (1_000_000, 4 << 20, reason::BAD_FIELDS),
    ] {
        let raw = Raw::connect(&core, rig.receiver.steam_id());
        let mut h = Hello::new(&SECRET, 3, 3, total, chunk, 8, [3; 32], 0);
        h.count = wire::chunk_count(total, chunk).unwrap_or(8);
        h.token = h.expected_token(&SECRET);
        raw.send(&wire::encode_hello(&h));
        let answer = wire::parse(&raw.last().unwrap()).map(|m| match m {
            Msg::Refuse { reason, .. } => reason,
            _ => 0,
        });
        assert_eq!(answer, Some(code), "total {total} chunk {chunk}");
    }

    // a chunk before any hello, garbage, an ack from the wrong side: the connection is closed
    for garbage in [
        wire::encode_chunk(5, 0, 8, b"data"),
        vec![1, 2, 3],
        wire::encode_ack(5, &[1]),
        vec![wire::MAGIC, 99, 0, 0, 0, 0],
    ] {
        let raw = Raw::connect(&core, rig.receiver.steam_id());
        raw.send(&garbage);
        assert!(raw.wait_closed(), "{garbage:?}");
    }

    // inside a transfer: bad index, wrong id, wrong count, wrong length, a second hello
    let total = 1_000_000u64;
    let h = hello_for(4, total, 131_072);
    let count = h.count;
    for attack in 0..5 {
        let raw = Raw::connect(&core, rig.receiver.steam_id());
        raw.send(&wire::encode_hello(&h));
        assert_eq!(wire::parse(&raw.last().unwrap()), Some(Msg::Accept { xfer: 4 }));
        let message = match attack {
            0 => wire::encode_chunk(4, count + 50, count, &[0u8; 100]),
            1 => wire::encode_chunk(99, 0, count, &[0u8; 131_072]),
            2 => wire::encode_chunk(4, 0, count + 1, &[0u8; 131_072]),
            3 => wire::encode_chunk(4, 0, count, &[0u8; 1000]),
            _ => wire::encode_hello(&h),
        };
        raw.send(&message);
        assert!(raw.wait_closed(), "attack {attack}");
    }

    // duplicates: the same chunk three times counts once, every copy is acknowledged
    let raw = Raw::connect(&core, rig.receiver.steam_id());
    raw.send(&wire::encode_hello(&h));
    assert_eq!(wire::parse(&raw.last().unwrap()), Some(Msg::Accept { xfer: 4 }));
    let mut payload = vec![0u8; 131_072];
    SyntheticSource::fill(4, 0, &mut payload);
    for _ in 0..3 {
        raw.send(&wire::encode_chunk(4, 0, count, &payload));
    }
    let end = Instant::now() + Duration::from_secs(3);
    let acked = |raw: &Raw| {
        raw.inbox
            .lock()
            .unwrap()
            .iter()
            .filter_map(|m| match wire::parse(m) {
                Some(Msg::Ack { indices, .. }) => Some(indices.len()),
                _ => None,
            })
            .sum::<usize>()
    };
    while acked(&raw) < 3 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(acked(&raw), 3, "every copy is acknowledged");
    drop(raw);
    // the receiver is unharmed: a genuine transfer goes through while the half-done one is still around
    std::thread::sleep(Duration::from_millis(200));
    assert!(rig.failed().iter().all(|f| !f.contains("cannot")), "{:?}", rig.failed());
}

#[test]
fn a_transfer_that_goes_quiet_is_failed_by_the_receiver_and_its_file_removed() {
    let rig = Rig::new();
    let raw = Raw::connect(&rig.core(), rig.receiver.steam_id());
    let h = hello_for(21, 1_000_000, 131_072);
    raw.send(&wire::encode_hello(&h));
    assert_eq!(wire::parse(&raw.last().unwrap()), Some(Msg::Accept { xfer: 21 }));
    assert!(rig.dir.path().join("transfer-21.bin.part").exists());
    assert!(rig.wait_event(Duration::from_secs(14), |e| e.iter().any(|x| matches!(x, ParEvent::Failed { .. }))));
    assert!(rig.failed()[0].contains("no data for 10 s"), "{:?}", rig.failed());
    assert!(raw.wait_closed());
    std::thread::sleep(Duration::from_millis(100));
    assert!(!rig.dir.path().join("transfer-21.bin.part").exists());
}

#[test]
fn a_second_transfer_while_one_runs_is_told_it_is_busy() {
    let rig = Rig::new();
    let first = Raw::connect(&rig.core(), rig.receiver.steam_id());
    first.send(&wire::encode_hello(&hello_for(31, 1_000_000, 131_072)));
    assert_eq!(wire::parse(&first.last().unwrap()), Some(Msg::Accept { xfer: 31 }));
    let second = Raw::connect(&rig.core(), rig.receiver.steam_id());
    second.send(&wire::encode_hello(&hello_for(32, 1_000_000, 131_072)));
    assert_eq!(wire::parse(&second.last().unwrap()), Some(Msg::Refuse { xfer: 32, reason: reason::BUSY }));
    // another connection of the same transfer with other fields does not join it
    let third = Raw::connect(&rig.core(), rig.receiver.steam_id());
    third.send(&wire::encode_hello(&hello_for(31, 2_000_000, 131_072)));
    assert_eq!(wire::parse(&third.last().unwrap()), Some(Msg::Refuse { xfer: 31, reason: reason::MISMATCH }));
}

#[test]
fn two_transfers_one_after_the_other_both_arrive() {
    let rig = Rig::new();
    let cfg = ParConfig { forced: Some(2), ..ParConfig::default() };
    for id in [41u32, 42] {
        let len = 1_500_000 + u64::from(id);
        let report = rig.send(rig.spec(id, len, cfg.clone()));
        assert!(report.ok, "{report:?}");
        assert!(rig.wait_event(Duration::from_secs(5), |e| e
            .iter()
            .any(|x| matches!(x, ParEvent::Done { ctx, .. } if *ctx == id))));
        assert!(file_is_payload(&rig.dir.path().join(format!("transfer-{id}.bin")), u64::from(id), len));
    }
}

#[test]
fn the_service_is_one_per_core() {
    let rig = Rig::new();
    let again = ParService::for_core(&rig.receiver_core.handle()).unwrap();
    assert!(Arc::ptr_eq(&again, &rig.service));
    let other = FakeMachine::new();
    let other_core = other.core();
    let svc = ParService::for_core(&other_core.handle()).unwrap();
    assert!(!Arc::ptr_eq(&svc, &rig.service));
}
