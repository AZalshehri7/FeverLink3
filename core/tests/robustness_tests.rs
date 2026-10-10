//! Randomised robustness tests: random and mutated input to everything in the core crate that reads bytes from
//! outside. Nothing may panic, nothing may allocate absurd amounts, and what is valid must round-trip.
//!
//! The generators are seeded, so a failure repeats: the seed is in the test name's constant and the failing
//! iteration is in the assertion message. `FEVERLINK3_FUZZ_SCALE=50 cargo test -p feverlink3_core --test
//! robustness_tests --release` runs every test 50 times longer; `FEVERLINK3_FUZZ_SEED=n` changes the seeds.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Debug;
use std::io::{self, Cursor, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use common::*;
use feverlink3_core::crypto::{OpenError, Opener, ReplayWindow, Sealer, SessionKey};
use feverlink3_core::lobby::{base32_decode, base32_encode, JoinCode, RelayAddr};
use feverlink3_core::protocol::{Frame, FrameCodec, FrameHeader};
use feverlink3_core::relay::{
    CloseReason, ErrorCode, ReadStep, RelayCodec, RelayError, RelayMessage, RelayReader, Timing,
};
use feverlink3_core::session::*;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

// ------------------------------------------------------------------------------------------ memory measurement

thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

/// Counts the bytes the current thread holds, so a test can ask how much a call needed at its largest.
struct Tracking;

fn grow(n: usize) {
    let _ = LIVE.try_with(|live| {
        live.set(live.get() + n);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(live.get())));
    });
}

fn shrink(n: usize) {
    let _ = LIVE.try_with(|live| live.set(live.get().saturating_sub(n)));
}

// SAFETY: every call is forwarded to the system allocator unchanged; the counters are thread-local `Cell`s that
// need no allocation of their own.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        shrink(layout.size());
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            shrink(layout.size());
            grow(new_size);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Tracking = Tracking;

/// Runs `f` and returns its result with the most bytes it held at once, beyond what was held when it began.
fn peak_of<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let base = LIVE.with(Cell::get);
    PEAK.with(|p| p.set(base));
    let result = f();
    (result, PEAK.with(Cell::get).saturating_sub(base))
}

const MIB: usize = 1 << 20;

// ------------------------------------------------------------------------------------------------- generators

fn scale() -> usize {
    std::env::var("FEVERLINK3_FUZZ_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

/// Iterations of a test: `base`, times the scale.
fn iters(base: usize) -> usize {
    base * scale()
}

fn rng(salt: u64) -> StdRng {
    let seed: u64 = std::env::var("FEVERLINK3_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0xFEED_F00D);
    StdRng::seed_from_u64(seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

fn random_bytes(r: &mut StdRng, max: usize) -> Vec<u8> {
    let len = if r.gen_bool(0.3) { r.gen_range(0..16.min(max + 1)) } else { r.gen_range(0..=max) };
    (0..len).map(|_| r.gen()).collect()
}

/// Text with the awkward characters in it: quotes, backslashes, controls, NUL, non-ASCII, astral planes.
fn random_text(r: &mut StdRng, max_chars: usize) -> String {
    const SPECIAL: [char; 14] =
        ['"', '\\', '\n', '\r', '\t', '\0', '\u{7f}', '\u{85}', '\u{2028}', 'é', 'ß', '日', '😀', '\u{feff}'];
    let len = r.gen_range(0..=max_chars);
    (0..len)
        .map(|_| match r.gen_range(0..4) {
            0 => SPECIAL[r.gen_range(0..SPECIAL.len())],
            1 => char::from_u32(r.gen_range(0..0x30000)).unwrap_or('?'),
            _ => char::from(r.gen_range(b' '..=b'~')),
        })
        .collect()
}

fn mutate(r: &mut StdRng, bytes: &mut Vec<u8>) {
    for _ in 0..r.gen_range(1..=4) {
        match r.gen_range(0..8) {
            0 if !bytes.is_empty() => {
                let i = r.gen_range(0..bytes.len());
                bytes[i] ^= 1 << r.gen_range(0..8);
            }
            1 if !bytes.is_empty() => {
                let i = r.gen_range(0..bytes.len());
                bytes[i] = r.gen();
            }
            2 => {
                let at = r.gen_range(0..=bytes.len());
                let extra = random_bytes(r, 16);
                bytes.splice(at..at, extra);
            }
            3 if !bytes.is_empty() => {
                let a = r.gen_range(0..bytes.len());
                let b = r.gen_range(a..=bytes.len());
                bytes.drain(a..b);
            }
            4 if !bytes.is_empty() => {
                let a = r.gen_range(0..bytes.len());
                let b = r.gen_range(a..=bytes.len());
                let copy = bytes[a..b].to_vec();
                let at = r.gen_range(0..=bytes.len());
                bytes.splice(at..at, copy);
            }
            5 => {
                let keep = r.gen_range(0..=bytes.len());
                bytes.truncate(keep);
            }
            6 if !bytes.is_empty() => {
                let i = r.gen_range(0..bytes.len());
                bytes[i] = b"\"{}[],:\\0-"[r.gen_range(0..10)];
            }
            _ => bytes.extend(random_bytes(r, 8)),
        }
    }
}

const KEYS: &[&str] = &[
    "t",
    "speed",
    "ahead",
    "name",
    "version",
    "proof",
    "nonce",
    "protocol",
    "reason",
    "detail",
    "you",
    "hostName",
    "players",
    "session",
    "started",
    "pauseAt",
    "lead",
    "from",
    "id",
    "ready",
    "player",
    "n",
    "at",
    "paused",
    "values",
    "index",
    "count",
    "sha256",
    "data",
    "time",
    "save",
    "stamp",
    "seq",
    "body",
    "category",
    "allowed",
    "text",
    "parts",
    "authority",
    "ok",
    "build",
    "bulldoze",
    "spend",
    "only",
    "everyone",
    "hostOnly",
    "money",
];

/// A JSON value of random shape whose object keys are mostly the protocol's own field names.
fn json_value(r: &mut StdRng, depth: usize) -> Value {
    match r.gen_range(0..12) {
        0 => Value::Null,
        1 => json!(r.gen::<bool>()),
        2 => json!(r.gen_range(-5i64..5)),
        3 => json!(
            [i64::MIN, i64::MAX, 0, -1, 1 << 53, 255, 65_535, 65_536, i32::MAX as i64, i32::MIN as i64]
                [r.gen_range(0..10)]
        ),
        4 => json!(u64::MAX - r.gen_range(0..3)),
        5 => json!([0.5, -0.0, 1e308, 1e-308, 123456789.125][r.gen_range(0..5)]),
        6 => json!(random_text(r, 20)),
        7 => json!(
            ["", "AAAA", "AA==", "not base64!", "00".repeat(32).as_str(), "ff".repeat(32).as_str()][r.gen_range(0..6)]
        ),
        8 if depth < 4 => Value::Array((0..r.gen_range(0..4)).map(|_| json_value(r, depth + 1)).collect()),
        _ if depth < 4 => {
            let mut map = serde_json::Map::new();
            for _ in 0..r.gen_range(0..7) {
                let key = if r.gen_bool(0.9) { KEYS[r.gen_range(0..KEYS.len())].to_owned() } else { random_text(r, 6) };
                map.insert(key, json_value(r, depth + 1));
            }
            Value::Object(map)
        }
        _ => json!(r.gen_range(0..100)),
    }
}

// ----------------------------------------------------------------------------------------------- frame codec

fn random_header(r: &mut StdRng) -> FrameHeader {
    FrameHeader::new(r.gen(), r.gen(), r.gen(), if r.gen_bool(0.2) { r.gen_range(0..4) } else { r.gen() })
}

/// A byte buffer that looks like frames until it does not: a random length field, sometimes plausible.
fn random_wire_bytes(r: &mut StdRng) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..r.gen_range(0..4) {
        let body = random_bytes(r, 300);
        let length: u32 = match r.gen_range(0..8) {
            0 => r.gen(),
            1 => r.gen_range(0..40),
            2 => FrameCodec::MAX_FRAME_LENGTH as u32 + r.gen_range(0..3),
            3 => u32::MAX - r.gen_range(0..3),
            _ => (FrameHeader::SIZE + body.len()) as u32,
        };
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(&random_header(r).to_bytes());
        out.extend_from_slice(&body);
    }
    if r.gen_bool(0.3) {
        mutate(r, &mut out);
    }
    out
}

#[test]
fn frame_decoding_never_panics_and_never_allocates_more_than_the_input_asks_for() {
    let mut r = rng(1);
    for i in 0..iters(4000) {
        let bytes = random_wire_bytes(&mut r);
        let (decoded, peak_decode) = peak_of(|| FrameCodec::try_decode(&bytes));
        let (read, peak_read) = peak_of(|| FrameCodec::read(&mut Cursor::new(&bytes)));
        let (limited, _) =
            peak_of(|| FrameCodec::read_limited(&mut Cursor::new(&bytes), FrameCodec::HANDSHAKE_MAX_LENGTH));
        // a declared length is only trusted up to what the buffer holds or the limit: never a 4 GiB allocation
        assert!(
            peak_decode <= bytes.len() + 4096,
            "iteration {i}: decode held {peak_decode} for {} bytes",
            bytes.len()
        );
        assert!(peak_read <= FrameCodec::MAX_FRAME_LENGTH + 4096, "iteration {i}: read held {peak_read}");
        match (&decoded, &read) {
            (Ok(Some((frame, used))), Ok(Some(streamed))) => {
                assert_eq!(frame, streamed, "iteration {i}");
                assert!(*used <= bytes.len());
                assert_eq!(FrameCodec::encode_frame(frame).unwrap(), bytes[..*used], "iteration {i}: re-encoding");
            }
            (Ok(Some(_)), other) => panic!("iteration {i}: try_decode found a frame, read said {other:?}"),
            (Err(_), Ok(_)) => panic!("iteration {i}: try_decode refused what read accepted"),
            _ => {}
        }
        if let Ok(Some(frame)) = limited {
            assert!(frame.body.len() + FrameHeader::SIZE <= FrameCodec::HANDSHAKE_MAX_LENGTH);
        }
    }
}

#[test]
fn frames_round_trip_through_encode_try_decode_and_a_stream_that_delivers_bytes_in_drips() {
    /// A reader that returns at most a few bytes per call, like a socket.
    struct Drip<'a> {
        data: &'a [u8],
        rng: StdRng,
    }
    impl Read for Drip<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.rng.gen_range(1..=7).min(buf.len()).min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    let mut r = rng(2);
    for i in 0..iters(300) {
        let frames: Vec<Frame> =
            (0..r.gen_range(1..5)).map(|_| Frame::new(random_header(&mut r), random_bytes(&mut r, 3000))).collect();
        let mut wire = Vec::new();
        for f in &frames {
            let one = FrameCodec::encode_frame(f).unwrap();
            let (back, used) = FrameCodec::try_decode(&one).unwrap().expect("a whole frame");
            assert_eq!((&back, used), (f, one.len()), "iteration {i}");
            // one byte short is not yet a frame
            assert!(FrameCodec::try_decode(&one[..one.len() - 1]).unwrap().is_none());
            wire.extend_from_slice(&one);
        }
        let mut stream = Drip { data: &wire, rng: StdRng::seed_from_u64(i as u64) };
        for f in &frames {
            assert_eq!(FrameCodec::read(&mut stream).unwrap().as_ref(), Some(f), "iteration {i}");
        }
        assert!(FrameCodec::read(&mut stream).unwrap().is_none());
    }
}

#[test]
fn a_length_field_is_never_trusted_before_it_is_checked() {
    for length in [0u32, 11, 12, 13, 1 << 20, (1 << 20) + 1, u32::MAX / 2, u32::MAX] {
        let bytes = length.to_be_bytes();
        let (r, peak) = peak_of(|| FrameCodec::read(&mut Cursor::new(bytes)));
        assert!(peak < 64 * 1024 || length <= 1 << 20, "length {length}: {peak} bytes held");
        if !(12..=1 << 20).contains(&length) {
            assert!(r.is_err(), "length {length}");
        }
    }
}

// ---------------------------------------------------------------------------------------- the sealed channel

#[test]
fn the_opener_accepts_exactly_what_a_sealer_made_and_survives_everything_else() {
    let key = SessionKey::derive(&SECRET, None);
    let mut r = rng(3);
    for i in 0..iters(1500) {
        let mut sealer = Sealer::new(&key, r.gen_range(0..5));
        let mut opener = Opener::new(&key);
        let payload = random_bytes(&mut r, 400);
        let frame = sealer.seal(r.gen(), &payload);

        // random junk: refused, whatever the header says
        let junk = Frame::new(random_header(&mut r), random_bytes(&mut r, 100));
        assert!(opener.try_open(&junk).is_err(), "iteration {i}");

        // the frame with any one change: refused (and the real one still opens afterwards: a refused frame burns nothing)
        let mut altered = frame.clone();
        match r.gen_range(0..5) {
            0 if !altered.body.is_empty() => {
                let at = r.gen_range(0..altered.body.len());
                altered.body[at] ^= 1 << r.gen_range(0..8);
            }
            1 => altered.header.kind ^= 1 << r.gen_range(0..8),
            2 => altered.header.sender ^= 1 << r.gen_range(0..16),
            3 => altered.header.counter ^= 1 << r.gen_range(0..64),
            _ => altered.body.truncate(r.gen_range(0..altered.body.len() + 1)),
        }
        if altered != frame {
            assert!(opener.try_open(&altered).is_err(), "iteration {i}: {altered:?}");
        }
        assert_eq!(opener.try_open(&frame).as_deref(), Ok(payload.as_slice()), "iteration {i}");
        assert_eq!(opener.try_open(&frame), Err(OpenError::Replay), "iteration {i}: replay");
    }
}

#[test]
fn the_replay_window_agrees_with_a_model_of_the_rules() {
    let mut r = rng(4);
    for run in 0..iters(300) {
        let mut window = ReplayWindow::new();
        let (mut highest, mut seen): (u64, BTreeSet<u64>) = (0, BTreeSet::new());
        let start: u64 = if r.gen_bool(0.2) { u64::MAX - r.gen_range(0..300) } else { r.gen_range(0..200) };
        for step in 0..200 {
            // mostly near the head of the window, sometimes a repeat, a jump, zero, or the extremes
            let counter = match r.gen_range(0..10) {
                0 => 0,
                1 => u64::MAX - r.gen_range(0..4),
                2 => highest.saturating_add(r.gen_range(0..300)),
                3 => highest.saturating_sub(r.gen_range(0..80)),
                _ => start.saturating_add(step as u64 + r.gen_range(0..6)).saturating_sub(r.gen_range(0..4)),
            };
            let model_fresh = counter != 0
                && (counter > highest || (highest - counter < ReplayWindow::SIZE && !seen.contains(&counter)));
            assert_eq!(window.is_fresh(counter), model_fresh, "run {run} step {step} counter {counter}");
            let outcome = window.accept(counter);
            assert_eq!(outcome.is_ok(), model_fresh, "run {run} step {step} counter {counter}");
            if model_fresh {
                highest = highest.max(counter);
                seen.insert(counter);
                seen.retain(|c| highest - *c < ReplayWindow::SIZE);
            }
            assert_eq!(window.highest(), highest);
        }
    }
}

// ------------------------------------------------------------------------------------------------- join codes

fn random_endpoint(r: &mut StdRng) -> std::net::SocketAddr {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    let ip = if r.gen_bool(0.5) {
        IpAddr::V4(Ipv4Addr::from(r.gen::<u32>()))
    } else {
        IpAddr::V6(Ipv6Addr::from(r.gen::<u128>()))
    };
    SocketAddr::new(ip, r.gen())
}

#[test]
fn join_codes_round_trip_and_garbage_never_decodes_to_something_that_does_not() {
    let mut r = rng(5);
    for i in 0..iters(1500) {
        let endpoints: Vec<_> =
            (0..r.gen_range(0..=JoinCode::MAX_ENDPOINTS)).map(|_| random_endpoint(&mut r)).collect();
        let secret: [u8; 16] = r.gen();
        let created: u32 = r.gen();
        let code = JoinCode::create(&endpoints, secret, Some(created)).unwrap();
        let text = code.encode();
        assert_eq!(JoinCode::try_decode(&text).as_ref(), Some(&code), "iteration {i}");
        // typed with dashes, spaces and lower case
        let typed: String = text
            .chars()
            .enumerate()
            .flat_map(
                |(k, c)| if k % 5 == 4 { vec![c.to_ascii_lowercase(), '-'] } else { vec![c.to_ascii_lowercase()] },
            )
            .collect();
        assert_eq!(JoinCode::try_decode(&format!("  {typed} ")).as_ref(), Some(&code), "iteration {i}");
        assert!(text.len() <= JoinCode::MAX_TEXT_LENGTH);

        // one character changed, one dropped, one added, the tail cut: nothing decodes to a different secret
        let mut chars: Vec<char> = text.chars().collect();
        let at = r.gen_range(0..chars.len());
        match r.gen_range(0..4) {
            0 => chars[at] = ['A', 'Z', '2', '7', '1', '0', '=', '-', ' ', 'é', '\0'][r.gen_range(0..11)],
            1 => {
                chars.remove(at);
            }
            2 => chars.insert(at, char::from(r.gen_range(b'A'..=b'Z'))),
            _ => chars.truncate(at),
        }
        let changed: String = chars.into_iter().collect();
        if let Some(other) = JoinCode::try_decode(&changed) {
            // only a change the format forgives can get here (a dash, a case, padding), and then it is the same code
            assert_eq!(other, code, "iteration {i}: {changed}");
        }
    }
}

#[test]
fn random_text_is_never_a_join_code_and_never_a_panic() {
    let mut r = rng(6);
    for i in 0..iters(4000) {
        let text = match r.gen_range(0..4) {
            0 => random_text(&mut r, 80),
            1 => (0..r.gen_range(0..120))
                .map(|_| char::from(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567-= "[r.gen_range(0..35)]))
                .collect(),
            2 => String::from_utf8_lossy(&random_bytes(&mut r, 120)).into_owned(),
            _ => "A".repeat(r.gen_range(0..2000)),
        };
        let (decoded, peak) = peak_of(|| JoinCode::try_decode(&text));
        assert!(peak <= 8 * 1024, "iteration {i}: {peak} bytes for {} characters", text.len());
        if let Some(code) = decoded {
            assert!(code.endpoints.len() <= JoinCode::MAX_ENDPOINTS);
            assert_eq!(JoinCode::try_decode(&code.encode()), Some(code));
        }
        // the raw decoder is lenient but bounded in what it reserves
        let (_, peak) = peak_of(|| base32_decode(&text));
        assert!(peak <= text.len() + 8 * 1024, "iteration {i}");
    }
    // base32 itself round-trips
    for _ in 0..iters(500) {
        let bytes = random_bytes(&mut r, 100);
        assert_eq!(base32_decode(&base32_encode(&bytes)), Some(bytes));
    }
    // a megabyte of anything is refused before it is looked at
    let big = "ABCD".repeat(MIB / 4);
    let (decoded, peak) = peak_of(|| JoinCode::try_decode(&big));
    assert_eq!((decoded, peak < 4096), (None, true));
}

// ---------------------------------------------------------------------------------------------------- messages

fn decode_every_message_type(bytes: &[u8]) {
    macro_rules! decode {
        ($($t:ty),* $(,)?) => { $( let _ = codec::try_decode::<$t>(bytes); )* };
    }
    decode!(
        ChallengeMessage,
        JoinMessage,
        RejectMessage,
        SessionState,
        PlayerInfo,
        WelcomeMessage,
        PlayerJoinedMessage,
        PlayerLeftMessage,
        PeerClockMessage,
        ChecksumRequestMessage,
        AuthorityMessage,
        ResyncSaveMessage,
        ResyncChunkMessage,
        ResyncLoadMessage,
        HelloMessage,
        ClockMessage,
        SpeedRequestMessage,
        ChecksumReportMessage,
        SaveDoneMessage,
        ReplayFailedMessage,
        ActionRefusedMessage,
        ActionMessage,
        Permissions,
        PlayerSet,
        ChatMessage,
    );
}

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: T) {
    let bytes = codec::encode(&value);
    let back: T = codec::try_decode(&bytes)
        .unwrap_or_else(|| panic!("{value:?} does not decode from {}", String::from_utf8_lossy(&bytes)));
    assert_eq!(value, back);
    assert_eq!(codec::encode(&back), bytes, "the encoding is stable");
}

fn finite_f64(r: &mut StdRng) -> f64 {
    loop {
        let v = if r.gen_bool(0.5) { r.gen::<f64>() * 1e6 - 5e5 } else { f64::from_bits(r.gen()) };
        if v.is_finite() {
            return v;
        }
    }
}

fn player_set(r: &mut StdRng) -> PlayerSet {
    match r.gen_range(0..3) {
        0 => PlayerSet::Everyone,
        1 => PlayerSet::HostOnly,
        _ => PlayerSet::Only((0..r.gen_range(0..6)).map(|_| r.gen()).collect()),
    }
}

fn player_info(r: &mut StdRng) -> PlayerInfo {
    PlayerInfo { id: r.gen(), name: random_text(r, 30), ready: r.gen() }
}

fn session_state(r: &mut StdRng) -> SessionState {
    SessionState { started: r.gen(), speed: r.gen(), pause_at: r.gen_bool(0.5).then(|| r.gen()), lead: r.gen() }
}

fn text_map(r: &mut StdRng) -> BTreeMap<String, String> {
    (0..r.gen_range(0..6)).map(|_| (random_text(r, 12), random_text(r, 12))).collect()
}

#[test]
fn every_message_round_trips_and_every_decoder_survives_every_shape_of_json() {
    let mut r = rng(7);
    for i in 0..iters(400) {
        let rr = &mut r;
        round_trip(ChallengeMessage { nonce: random_bytes(rr, 40), protocol: rr.gen(), version: random_text(rr, 10) });
        round_trip(JoinMessage {
            name: random_text(rr, 30),
            version: random_text(rr, 10),
            proof: random_bytes(rr, 40),
        });
        round_trip(RejectMessage {
            reason: random_text(rr, 10),
            detail: rr.gen_bool(0.5).then(|| random_text(rr, 30)),
        });
        round_trip(session_state(rr));
        round_trip(player_info(rr));
        round_trip(WelcomeMessage {
            you: rr.gen(),
            host_name: random_text(rr, 20),
            version: random_text(rr, 6),
            players: (0..rr.gen_range(0..5)).map(|_| player_info(rr)).collect(),
            session: session_state(rr),
        });
        round_trip(PlayerJoinedMessage { player: player_info(rr) });
        round_trip(PlayerLeftMessage { id: rr.gen(), name: random_text(rr, 20) });
        round_trip(PeerClockMessage { from: rr.gen(), t: rr.gen(), ahead: rr.gen() });
        round_trip(ChecksumRequestMessage { n: rr.gen(), at: rr.gen(), paused: rr.gen() });
        round_trip(AuthorityMessage {
            n: rr.gen(),
            values: (0..rr.gen_range(0..4)).map(|_| (random_text(rr, 8), finite_f64(rr))).collect(),
        });
        round_trip(ResyncSaveMessage { id: rr.gen(), name: random_text(rr, 20) });
        round_trip(ResyncChunkMessage {
            id: rr.gen(),
            index: rr.gen(),
            count: rr.gen(),
            sha256: random_text(rr, 64),
            data: random_bytes(rr, 300),
        });
        round_trip(ResyncLoadMessage { id: rr.gen(), name: random_text(rr, 20), sha256: random_text(rr, 64) });
        round_trip(HelloMessage { time: rr.gen(), save: rr.gen_bool(0.5).then(|| random_text(rr, 20)) });
        round_trip(ClockMessage { t: rr.gen(), speed: rr.gen(), ahead: rr.gen() });
        round_trip(SpeedRequestMessage { speed: rr.gen() });
        round_trip(ChecksumReportMessage {
            n: rr.gen(),
            time: rr.gen(),
            parts: text_map(rr),
            authority: rr
                .gen_bool(0.5)
                .then(|| (0..rr.gen_range(0..4)).map(|_| (random_text(rr, 8), finite_f64(rr))).collect()),
        });
        round_trip(SaveDoneMessage {
            id: rr.gen(),
            ok: rr.gen(),
            detail: rr.gen_bool(0.5).then(|| random_text(rr, 20)),
        });
        round_trip(ReplayFailedMessage { detail: random_text(rr, 40) });
        round_trip(ActionRefusedMessage { seq: rr.gen(), detail: random_text(rr, 40) });
        round_trip(ActionMessage {
            stamp: rr.gen(),
            seq: rr.gen(),
            body: random_text(rr, 80),
            from: rr.gen(),
            category: random_text(rr, 8),
            allowed: rr.gen(),
        });
        round_trip(Permissions {
            build: player_set(rr),
            bulldoze: player_set(rr),
            spend: player_set(rr),
            speed: player_set(rr),
        });
        round_trip(ChatMessage {
            text: random_text(rr, 60),
            from: rr.gen(),
            name: rr.gen_bool(0.5).then(|| random_text(rr, 20)),
        });

        // arbitrary JSON, and valid encodings with damage: no decoder may panic, whatever it makes of them
        let bytes = serde_json::to_vec(&json_value(&mut r, 0)).unwrap();
        let (_, peak) = peak_of(|| decode_every_message_type(&bytes));
        assert!(peak <= 64 * bytes.len() + 64 * 1024, "iteration {i}: {peak} bytes for {} bytes of JSON", bytes.len());
        let mut damaged = codec::encode(&WelcomeMessage {
            you: 3,
            host_name: "Host".into(),
            version: "0.1".into(),
            players: vec![PlayerInfo { id: 3, name: "A".into(), ready: true }],
            session: SessionState { started: true, speed: 2, pause_at: Some(4000), lead: 1600 },
        });
        mutate(&mut r, &mut damaged);
        decode_every_message_type(&damaged);
        decode_every_message_type(&random_bytes(&mut r, 200));
    }
}

#[test]
fn decoding_what_decodes_is_stable() {
    // a value that decodes from damaged bytes encodes to bytes that decode to the same value
    let mut r = rng(8);
    for i in 0..iters(1500) {
        let source = codec::encode(&ActionMessage::new(r.gen(), r.gen(), &random_text(&mut r, 40)));
        let mut bytes = source;
        mutate(&mut r, &mut bytes);
        if let Some(m) = codec::try_decode::<ActionMessage>(&bytes) {
            let again = codec::encode(&m);
            assert_eq!(codec::try_decode::<ActionMessage>(&again), Some(m), "iteration {i}");
        }
    }
}

#[test]
fn json_nested_a_million_deep_is_refused_without_a_stack_overflow() {
    for open in ["[", "{\"a\":", "[{\"b\":"] {
        let bytes = open.repeat(1_000_000).into_bytes();
        let (_, peak) = peak_of(|| decode_every_message_type(&bytes));
        assert!(peak < 8 * MIB, "{peak} bytes held");
    }
}

#[test]
fn a_megabyte_of_json_costs_a_bounded_multiple_of_itself() {
    // 1 MiB frames are legal; decoding one into every shape must not need a hundred times that
    let fat = serde_json::to_vec(&json!({ "body": "x".repeat(MIB - 100), "stamp": 1, "seq": 2 })).unwrap();
    let (m, peak) = peak_of(|| codec::try_decode::<ActionMessage>(&fat));
    assert!(m.is_some() && peak < 4 * MIB, "{peak}");
    let many = serde_json::to_vec(
        &json!({ "parts": (0..30_000).map(|i| (format!("p{i}"), "0")).collect::<BTreeMap<_, _>>() }),
    )
    .unwrap();
    let (_, peak) = peak_of(|| decode_every_message_type(&many));
    assert!(peak < 64 * MIB, "{peak}");
}

// ----------------------------------------------------------------------------------- the host and the player

/// A joined player's channel, for sending the host anything: the real sealer's key, a counter of its own.
struct Hostile {
    connection: ConnectionId,
    id: u16,
    counter: u64,
}

impl Hostile {
    fn frame(&mut self, h: &Harness, kind: u8, payload: &[u8]) -> Frame {
        self.counter += 1;
        Sealer::resume(&h.key, self.id, self.counter - 1).seal(kind, payload)
    }
}

fn some_payload(r: &mut StdRng) -> Vec<u8> {
    match r.gen_range(0..6) {
        0 => random_bytes(r, 300),
        1 => serde_json::to_vec(&json_value(r, 0)).unwrap(),
        2 => {
            let mut b = codec::encode(&ClockMessage { t: r.gen_range(0..1 << 30), speed: r.gen_range(0..5), ahead: 3 });
            mutate(r, &mut b);
            b
        }
        3 => {
            let m = ActionMessage {
                stamp: r.gen(),
                seq: r.gen(),
                body: random_text(r, 50),
                from: r.gen(),
                category: random_text(r, 8),
                allowed: r.gen(),
            };
            codec::encode(&m)
        }
        4 => codec::encode(&ClockMessage {
            t: [i64::MIN, -1, 0, 1 << 50, 1 << 51, i64::MAX][r.gen_range(0..6)],
            speed: [i32::MIN, -1, 0, 1, 16, 17, i32::MAX][r.gen_range(0..7)],
            ahead: r.gen(),
        }),
        _ => codec::encode(&SpeedRequestMessage { speed: r.gen_range(-3..8) }),
    }
}

#[test]
fn a_hostile_joined_player_cannot_panic_or_bloat_the_host() {
    let mut r = rng(9);
    let (mut h, ids) = Harness::running(3, 2, HostOptions::default());
    let victim = ids[0];
    let mut hostile = Hostile { connection: ids[2], id: h.p(ids[2]).session.id().unwrap(), counter: 5000 };
    let others = [ids[0], ids[1]];

    for i in 0..iters(3000) {
        let kind: u8 = match r.gen_range(0..4) {
            0 => r.gen(),
            _ => MessageKind::FROM_PLAYER[r.gen_range(0..MessageKind::FROM_PLAYER.len())].into(),
        };
        let payload = some_payload(&mut r);
        let frame = match r.gen_range(0..10) {
            // sealed with the real key under the connection's own id: the host has to decide what to do with it
            0..=6 => hostile.frame(&h, kind, &payload),
            // claims another sender, or a header of its own making
            7 => Frame::new(FrameHeader::new(1, kind, r.gen(), r.gen()), payload.clone()),
            8 => Sealer::resume(&h.key, others[r.gen_range(0..2)].min(255) as u16, 9000).seal(kind, &payload),
            _ => {
                let mut f = hostile.frame(&h, kind, &payload);
                mutate(&mut r, &mut f.body);
                f
            }
        };
        let (outs, peak) = peak_of(|| h.host.frame_received(hostile.connection, &frame));
        assert!(
            peak < 8 * MIB + 40 * payload.len(),
            "iteration {i}: {peak} bytes for a {} byte payload",
            payload.len()
        );
        h.pump_host(outs);
        if i % 500 == 499 {
            // the host's clock and tick still work, whatever the player has been feeding it
            h.advance(1.0);
            let ticked = h.host.tick();
            h.pump_host(ticked);
        }
    }
    // the honest players are still served
    let before = h.p(others[1]).count(MessageKind::Action);
    h.player_game(victim, MessageKind::Action, &json(&ActionMessage::new(1_000_000, 1, "{}")));
    assert_eq!(h.p(others[1]).count(MessageKind::Action), before + 1);
    assert_eq!(h.host.players().len(), 4);
    assert!(h.host.dropped() > 0);
}

#[test]
fn the_hosts_own_game_and_the_connection_table_survive_random_calls() {
    let mut r = rng(10);
    let mut h = Harness::running(2, 1, HostOptions::default()).0;
    for i in 0..iters(2000) {
        let kind = MessageKind::from_u8(r.gen()).unwrap_or(MessageKind::Hello);
        let payload = some_payload(&mut r);
        let (outs, peak) = peak_of(|| h.host.local_game_message(kind, &payload));
        assert!(peak < 8 * MIB + 40 * payload.len(), "iteration {i}");
        h.pump_host(outs);
        match r.gen_range(0..14) {
            0 => h.host_call(|host| host.set_speed(r.gen())),
            1 => h.host_call(HostSession::pause),
            2 => h.host_call(HostSession::request_checksum),
            3 => h.host_call(|host| host.start_resync("fuzz")),
            4 => {
                let id = h.host.resync_id().unwrap_or(1);
                let len: u64 = r.gen_range(0..2_000_000);
                h.host_call(|host| host.resync_file_ready(id, &"ab".repeat(32), len));
            }
            5 => {
                let id = h.host.resync_id().unwrap_or(1);
                let chunk = random_bytes(&mut r, 300);
                h.host_call(|host| host.resync_chunk(id, r.gen_range(0..4), &chunk));
            }
            6 => {
                h.advance(r.gen_range(0.0..400.0));
                h.host_call(HostSession::tick);
            }
            7 => {
                h.host_call(|host| {
                    host.set_permissions(Permissions { build: player_set(&mut r), ..Permissions::default() })
                });
            }
            8 => {
                let c = 10_000 + r.gen_range(0..40);
                let outs = h.host.connection_opened(c);
                h.pump_host(outs);
            }
            9 => {
                let c = 10_000 + r.gen_range(0..40);
                let outs = h.host.connection_closed(c);
                h.pump_host(outs);
            }
            10 => {
                // a frame on a connection the host never heard of
                let outs = h.host.frame_received(r.gen(), &Frame::new(random_header(&mut r), random_bytes(&mut r, 50)));
                h.pump_host(outs);
            }
            _ => {}
        }
        let _ = (h.host.max_clock(), h.host.future_stamp(), h.host.resync_state());
    }
}

#[test]
fn a_player_survives_a_hostile_host() {
    let mut r = rng(11);
    let mut h = Harness::running(1, 1, HostOptions::default()).0;
    let c = *h.players.keys().next().unwrap();
    let host_kinds = [
        MessageKind::Welcome,
        MessageKind::Session,
        MessageKind::PeerClock,
        MessageKind::PlayerJoined,
        MessageKind::PlayerLeft,
        MessageKind::ChecksumRequest,
        MessageKind::Authority,
        MessageKind::ResyncSave,
        MessageKind::ResyncChunk,
        MessageKind::ResyncLoad,
        MessageKind::Permissions,
        MessageKind::Action,
        MessageKind::Chat,
    ];
    let mut counter = 1_000_000u64;
    for i in 0..iters(3000) {
        let kind: u8 = if r.gen_bool(0.1) { r.gen() } else { host_kinds[r.gen_range(0..host_kinds.len())].into() };
        let payload = match r.gen_range(0..4) {
            0 => random_bytes(&mut r, 300),
            1 => serde_json::to_vec(&json_value(&mut r, 0)).unwrap(),
            2 => codec::encode(&ResyncChunkMessage {
                id: r.gen_range(0..3),
                index: r.gen_range(-2..6),
                count: [1, 2, 5, 2048, 2049, 0, -1, i32::MAX][r.gen_range(0..8)],
                sha256: if r.gen_bool(0.7) { "ab".repeat(32) } else { random_text(&mut r, 70) },
                data: {
                    let max = if r.gen_bool(0.02) { 300_000 } else { 400 };
                    random_bytes(&mut r, max)
                },
            }),
            _ => codec::encode(&ResyncLoadMessage { id: r.gen_range(0..3), name: "n".into(), sha256: "ab".repeat(32) }),
        };
        counter += 1;
        let frame = match r.gen_range(0..8) {
            0 => Frame::new(FrameHeader::new(1, kind, r.gen_range(0..2), r.gen()), random_bytes(&mut r, 80)),
            1 => Frame::new(FrameHeader::new(1, r.gen_range(1..4), HostSession::UNASSIGNED, 0), payload.clone()),
            _ => Sealer::resume(&h.key, HostSession::HOST_ID, counter - 1).seal(kind, &payload),
        };
        let session = &mut h.players.get_mut(&c).unwrap().session;
        let (outs, peak) = peak_of(|| session.frame_received(&frame));
        assert!(peak < 8 * MIB, "iteration {i}: {peak}");
        let _ = outs;
        if i % 300 == 299 {
            let session = &mut h.players.get_mut(&c).unwrap().session;
            session.tick();
            session.local_game_message(MessageKind::Hello, b"{}");
        }
    }
    let session = &h.players[&c].session;
    assert_eq!(session.state(), PlayerState::Joined, "no frame took the player out of the session");
    assert!(session.dropped() > 0);
}

#[test]
fn a_savegame_assembles_from_chunks_in_any_order_with_repeats() {
    let mut r = rng(12);
    for i in 0..iters(12) {
        let bytes = random_bytes(&mut r, 3 * ResyncLimits::CHUNK_SIZE);
        if bytes.is_empty() {
            continue;
        }
        let sha = sha256_hex(&bytes);
        let count = ResyncLimits::chunk_count(bytes.len() as u64).unwrap() as usize;
        // every chunk once in a random order, some repeated before the last one arrives
        let mut order: Vec<usize> = (0..count).collect();
        order.shuffle(&mut r);
        let last = order.pop().unwrap();
        let repeats: Vec<usize> = order.iter().copied().filter(|_| r.gen_bool(0.3)).collect();
        order.extend(repeats);
        order.shuffle(&mut r);
        order.push(last);

        let mut incoming = IncomingSave::new();
        let mut done = None;
        for index in order {
            let chunk =
                &bytes[index * ResyncLimits::CHUNK_SIZE..((index + 1) * ResyncLimits::CHUNK_SIZE).min(bytes.len())];
            let m = ResyncChunkMessage {
                id: 4,
                index: index as i32,
                count: count as i32,
                sha256: sha.clone(),
                data: chunk.to_vec(),
            };
            if let Some(save) = incoming.accept(1.0, m).unwrap() {
                assert!(done.replace(save).is_none(), "iteration {i}: completed twice");
            }
        }
        let save = done.unwrap_or_else(|| panic!("iteration {i}: never completed"));
        assert_eq!((save.id, save.sha256.as_str(), &save.bytes), (4, sha.as_str(), &bytes));
        assert!(incoming.has(4, &sha) && incoming.in_progress().is_none());
    }
}

#[test]
fn chunk_arithmetic_holds_for_every_size() {
    let mut r = rng(13);
    let largest = ResyncLimits::MAX_CHUNKS as u64 * ResyncLimits::CHUNK_SIZE as u64;
    for _ in 0..iters(3000) {
        let len: u64 = match r.gen_range(0..4) {
            0 => r.gen(),
            1 => r.gen_range(0..4_000_000),
            2 => ResyncLimits::CHUNK_SIZE as u64 * r.gen_range(0..2050) + r.gen_range(0..3),
            _ => largest - 1 + r.gen_range(0..3),
        };
        let count = ResyncLimits::chunk_count(len);
        match count {
            None => assert!(len == 0 || len > largest, "len {len}"),
            Some(count) => {
                let total: u64 = (0..count).map(|i| ResyncLimits::chunk_len(len, i).unwrap() as u64).sum();
                assert_eq!(total, len);
                assert_eq!(ResyncLimits::chunk_len(len, count), None);
            }
        }
        let index: u32 = if r.gen_bool(0.5) { r.gen() } else { r.gen_range(0..2050) };
        assert_eq!(
            ResyncLimits::chunk_len(len, index).is_some(),
            count.is_some_and(|c| index < c),
            "len {len} index {index}"
        );
    }
}

#[test]
fn small_pure_functions_take_any_input() {
    let mut r = rng(14);
    for _ in 0..iters(3000) {
        // names: always usable, whatever was typed
        let typed = random_text(&mut r, 60);
        let clean = Names::sanitize(&typed);
        assert!(!clean.is_empty() && clean.chars().count() <= Names::MAX_LENGTH);
        assert!(clean.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-'));
        assert_eq!(Names::sanitize(&clean), clean, "sanitising twice changes nothing");
        let taken = [Names::sanitize(&random_text(&mut r, 10)), clean.to_lowercase()];
        let unique = Names::unique(&clean, r.gen(), taken.iter().map(String::as_str));
        assert!(unique.starts_with(&clean), "a taken name is extended, never replaced");
        assert_eq!(unique == clean, !taken.iter().any(|t| t.to_uppercase() == clean.to_uppercase()));
        // hashes
        if let Some(sha) = normalize_sha256(&random_text(&mut r, 70)) {
            assert!(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        }
        // time arithmetic with floats of every kind
        let stamp = Lockstep::round_up_to_step(f64::from_bits(r.gen()));
        assert!(stamp % Lockstep::STEP == 0 || stamp == i64::MAX || stamp == i64::MIN, "stamp {stamp}");
        let clock = PlayerClock { t: r.gen(), speed: r.gen(), at: f64::from_bits(r.gen()) };
        let _ = clock.estimate(f64::from_bits(r.gen()));
        let _ = PlayerClock::plausible(r.gen(), r.gen());
        assert!(Lockstep::lead_units(r.gen()) >= Lockstep::STEP * 6);
    }
    for kind in 0..=u8::MAX {
        if let Some(k) = MessageKind::from_u8(kind) {
            assert_eq!(u8::from(k), kind);
            assert_eq!(k.allowed_from_player(), MessageKind::FROM_PLAYER.contains(&k));
            assert!(!(k.is_unsealed() && k.allowed_from_player()), "an unsealed kind is never a player's");
        }
    }
}

#[test]
fn checksum_rounds_and_policy_take_reports_in_any_order_and_any_numbers() {
    let mut r = rng(15);
    for i in 0..iters(300) {
        let mut rounds = ChecksumRounds::new();
        let mut policy = ChecksumPolicy::new(ChecksumPolicyConfig::default());
        for step in 0..60 {
            let n = [i32::MIN, -1, 0, 1, 2, 3, 100, i32::MAX][r.gen_range(0..8)];
            if r.gen_bool(0.4) {
                rounds.open(n);
            }
            let reporter: u16 = r.gen_range(0..4);
            let parts: BTreeMap<String, String> = (0..r.gen_range(0..4))
                .map(|_| {
                    (
                        ["money", "vehicles", "towns", "script:x", "roads"][r.gen_range(0..5)].to_owned(),
                        format!("{:08x}", r.gen_range(0..3)),
                    )
                })
                .collect();
            let expected: Vec<u16> = (0..r.gen_range(0..4)).collect();
            if let Some(outcome) = rounds.report(reporter, ChecksumReportMessage::new(n, r.gen(), parts), &expected) {
                let now = if r.gen_bool(0.1) { f64::from_bits(r.gen()) } else { (i * 1000 + step * 10) as f64 };
                let _ = policy.on_outcome(now, &outcome);
                let _ = policy.on_replay_failed(now);
                policy.resync_finished(now);
            }
        }
    }
}

#[test]
fn text_a_player_chooses_is_clipped_before_it_reaches_the_host_log() {
    // the same bound as hardening_tests, over random payload sizes and the three messages that echo text
    let mut r = rng(16);
    let (mut h, ids) = Harness::running(1, 1, HostOptions::default());
    for _ in 0..iters(30) {
        let size = r.gen_range(0..200_000);
        h.player_game(ids[0], MessageKind::Hello, &vec![b'x'; size]);
        h.player_game(
            ids[0],
            MessageKind::ReplayFailed,
            &json(&ReplayFailedMessage { detail: random_text(&mut r, size.min(50_000)) }),
        );
        h.player_game(ids[0], MessageKind::Chat, &json(&ChatMessage::new(&random_text(&mut r, size.min(50_000)))));
    }
    assert!(h.host_log.iter().all(|l| l.chars().count() < 600));
    let unique: HashSet<&String> = h.host_log.iter().collect();
    assert!(!unique.is_empty());
}

// ------------------------------------------------------------------------- join code extensions and relay names

fn random_label(r: &mut StdRng, max: usize) -> String {
    let n = r.gen_range(1..=max);
    (0..n)
        .map(|k| {
            let edge = k == 0 || k + 1 == n;
            let pool: &[u8] =
                if edge { b"abcdefghijklmnopqrstuvwxyz0123456789" } else { b"abcdefghijklmnopqrstuvwxyz0123456789-" };
            char::from(pool[r.gen_range(0..pool.len())])
        })
        .collect()
}

fn random_relay(r: &mut StdRng) -> RelayAddr {
    let port: u16 = r.gen_range(1..=u16::MAX);
    match r.gen_range(0..3) {
        0 => RelayAddr::from(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(r.gen::<u32>())), port)),
        1 => RelayAddr::from(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(r.gen::<u128>())), port)),
        _ => loop {
            // up to three labels; one that ends in digits only is not a name, so that draw is made again
            let name = (0..r.gen_range(1..4)).map(|_| random_label(r, 20)).collect::<Vec<_>>().join(".");
            if let Some(addr) = RelayAddr::new(&name, port) {
                return addr;
            }
        },
    }
}

/// A join code with every optional part that a code can have, in random amounts.
fn random_code(r: &mut StdRng) -> JoinCode {
    let endpoints: Vec<SocketAddr> =
        (0..r.gen_range(0..=JoinCode::MAX_ENDPOINTS)).map(|_| random_endpoint(r)).collect();
    let mut code = JoinCode::create(&endpoints, r.gen(), Some(r.gen())).unwrap();
    if r.gen_bool(0.7) {
        let relays: Vec<RelayAddr> = (0..r.gen_range(1..=JoinCode::MAX_RELAYS)).map(|_| random_relay(r)).collect();
        code = code.with_relays(&relays).unwrap();
    }
    if r.gen_bool(0.6) {
        code = code.with_steam_id(r.gen_range(1..=u64::MAX));
    }
    code
}

/// The text of a code whose body is `body`: the checksum an attacker can always compute is computed, so what is
/// tested is the parser of the structure, not the typing check.
fn seal_body(mut body: Vec<u8>) -> String {
    let check = Sha256::digest(&body);
    body.extend_from_slice(&check[..4]);
    base32_encode(&body)
}

/// A code body that follows the layout often enough to get deep into it and breaks it at random places.
fn structured_code_body(r: &mut StdRng) -> Vec<u8> {
    let mut b = vec![[1u8, 2, 2, 2, 3, 0, 255][r.gen_range(0..7)]];
    b.extend(r.gen::<u32>().to_be_bytes());
    b.extend(r.gen::<[u8; 16]>());
    let endpoints = [0u8, 1, 1, 2, 8, 9, 255][r.gen_range(0..7)];
    b.push(endpoints);
    for _ in 0..endpoints.min(10) {
        let family = [4u8, 4, 6, 6, 1, 0, 255][r.gen_range(0..7)];
        b.push(family);
        let len = match family {
            4 => 4,
            6 => 16,
            _ => r.gen_range(0..20),
        };
        b.extend((0..len).map(|_| r.gen::<u8>()));
        b.extend(r.gen::<u16>().to_be_bytes());
    }
    if r.gen_bool(0.85) {
        let count = [0u8, 1, 2, 2, 3, 8, 9, 255][r.gen_range(0..8)];
        b.push(count);
        for _ in 0..count.min(10) {
            let tag = [1u8, 1, 2, 2, 3, 99, 0, 255][r.gen_range(0..8)];
            let value: Vec<u8> = match tag {
                1 => {
                    let n = [0u8, 1, 2, 4, 5, 255][r.gen_range(0..6)];
                    let mut v = vec![n];
                    for _ in 0..n.min(6) {
                        let family = [4u8, 6, 1, 1, 7][r.gen_range(0..5)];
                        v.push(family);
                        let len = match family {
                            4 => 4,
                            6 => 16,
                            _ => r.gen_range(0..110),
                        };
                        if family == 1 {
                            v.push(len as u8);
                            v.extend((0..len).map(|_| b"abcxyz019-."[r.gen_range(0..11)]));
                        } else {
                            v.extend((0..len).map(|_| r.gen::<u8>()));
                        }
                        v.extend(r.gen::<u16>().to_be_bytes());
                    }
                    v
                }
                2 => {
                    let mut v = r.gen::<u64>().to_be_bytes().to_vec();
                    if r.gen_bool(0.2) {
                        v.truncate(r.gen_range(0..8));
                    }
                    if r.gen_bool(0.1) {
                        v = vec![0; 8];
                    }
                    v
                }
                _ => random_bytes(r, 40),
            };
            b.push(tag);
            // the length is usually the true one
            let declared = if r.gen_bool(0.85) { value.len() as u16 } else { r.gen_range(0..300) };
            b.extend(declared.to_be_bytes());
            b.extend(value);
        }
    }
    if r.gen_bool(0.15) {
        b.extend(random_bytes(r, 10));
    }
    b
}

#[test]
fn join_codes_with_relays_and_a_steam_id_round_trip() {
    let mut r = rng(17);
    for i in 0..iters(1500) {
        let code = random_code(&mut r);
        let text = code.encode();
        assert!(text.len() <= 976, "iteration {i}: {} characters", text.len());
        assert_eq!(JoinCode::try_decode(&text).as_ref(), Some(&code), "iteration {i}");
        // a code is version 1 exactly when nothing needs version 2, so that older builds can still read it
        assert_eq!(code.version, if code.relays.is_empty() && code.steam_id.is_none() { 1 } else { 2 });
        assert_eq!(code.relay_room(), SessionKey::relay_room_id(&code.secret));
        // typed in groups with dashes, in lower case
        let typed: String =
            text.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).to_lowercase()).collect::<Vec<_>>().join("-");
        assert_eq!(JoinCode::try_decode(&typed).as_ref(), Some(&code), "iteration {i}");
    }
}

#[test]
fn relay_addresses_print_back_to_themselves_and_text_of_any_kind_is_read_or_refused() {
    let mut r = rng(18);
    for i in 0..iters(3000) {
        let relay = random_relay(&mut r);
        assert_eq!(RelayAddr::parse(&relay.to_string(), None).as_ref(), Some(&relay), "iteration {i}: {relay}");
        assert_eq!(RelayAddr::new(&relay.host(), relay.port()).as_ref(), Some(&relay), "iteration {i}");
        assert_eq!(relay.socket_addr().map(|a| a.port()).unwrap_or(relay.port()), relay.port());

        let text = match r.gen_range(0..4) {
            0 => random_text(&mut r, 60),
            1 => String::from_utf8_lossy(&random_bytes(&mut r, 60)).into_owned(),
            2 => (0..r.gen_range(0..12))
                .map(|_| {
                    ["[", "]", ":", ".", "a", "1", "-", "::", "65535", "0", " ", "é", "999", "example", "com"]
                        [r.gen_range(0..15)]
                })
                .collect(),
            _ => format!("{}:{}", random_label(&mut r, 70), r.gen::<u32>()),
        };
        for default_port in [None, Some(28100)] {
            if let Some(parsed) = RelayAddr::parse(&text, default_port) {
                assert!(parsed.port() != 0, "{text:?}");
                assert_eq!(RelayAddr::parse(&parsed.to_string(), None), Some(parsed), "{text:?}");
            }
        }
        let _ = RelayAddr::new(&text, r.gen());
    }
}

#[test]
fn join_code_structure_after_a_valid_checksum_is_read_or_refused_and_what_is_read_can_be_written() {
    let mut r = rng(19);
    let mut accepted = 0;
    for i in 0..iters(10_000) {
        let body = if r.gen_bool(0.6) {
            structured_code_body(&mut r)
        } else {
            // a valid code with its bytes damaged under a fresh checksum
            let mut body = base32_decode(&random_code(&mut r).encode()).unwrap();
            body.truncate(body.len() - 4);
            mutate(&mut r, &mut body);
            body
        };
        let text = seal_body(body);
        let (decoded, peak) = peak_of(|| JoinCode::try_decode(&text));
        assert!(peak <= 16 * 1024, "iteration {i}: {peak} bytes for {} characters", text.len());
        if let Some(code) = decoded {
            accepted += 1;
            assert!(code.endpoints.len() <= JoinCode::MAX_ENDPOINTS && code.relays.len() <= JoinCode::MAX_RELAYS);
            assert!(code.steam_id != Some(0) && (code.version == 1 || code.version == 2));
            assert!(code.version == 2 || (code.relays.is_empty() && code.steam_id.is_none()));
            assert!(code.relays.iter().all(|relay| relay.port() != 0));
            // what was read can be written, and read again as the same thing; a field of a later build is not kept
            let again = JoinCode::try_decode(&code.encode()).unwrap_or_else(|| panic!("iteration {i}: {code:?}"));
            assert_eq!(
                (again.created, &again.endpoints, &again.relays, again.steam_id, again.secret),
                (code.created, &code.endpoints, &code.relays, code.steam_id, code.secret),
                "iteration {i}"
            );
        }
    }
    assert!(
        accepted > iters(10_000) / 50,
        "the generator reaches valid codes often enough to mean something: {accepted}"
    );
}

// ------------------------------------------------------------------------------------------------ relay messages

fn random_frame_bytes(r: &mut StdRng) -> Vec<u8> {
    FrameCodec::encode_frame(&Frame::new(random_header(r), random_bytes(r, 400))).unwrap()
}

fn random_relay_message(r: &mut StdRng) -> RelayMessage {
    match r.gen_range(0..11) {
        0 => RelayMessage::Register { version: r.gen(), room: r.gen() },
        1 => RelayMessage::Join { version: r.gen(), room: r.gen() },
        2 => RelayMessage::Registered(Timing { ping_secs: r.gen(), idle_secs: r.gen() }),
        3 => RelayMessage::Joined(Timing { ping_secs: r.gen(), idle_secs: r.gen() }),
        4 => RelayMessage::Error { code: ErrorCode(r.gen()), text: random_text(r, 300) },
        5 => RelayMessage::Ping(r.gen()),
        6 => RelayMessage::Pong(r.gen()),
        7 => RelayMessage::Data(random_frame_bytes(r)),
        8 => RelayMessage::HostData { vconn: r.gen(), frame: random_frame_bytes(r) },
        9 => RelayMessage::Open { vconn: r.gen() },
        _ => RelayMessage::Close { vconn: r.gen(), reason: CloseReason(r.gen()) },
    }
}

/// The message as it comes back from the wire: an error text is cut to 200 bytes at a character boundary.
fn as_received(message: &RelayMessage) -> RelayMessage {
    match message {
        RelayMessage::Error { code, text } => {
            let mut end = text.len().min(RelayCodec::MAX_ERROR_TEXT);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            RelayMessage::Error { code: *code, text: text[..end].to_owned() }
        }
        other => other.clone(),
    }
}

/// A stream that gives a few bytes at a time and sometimes says it would block, like a socket with a timeout.
struct Stutter {
    data: Vec<u8>,
    at: usize,
    rng: StdRng,
}

impl Read for Stutter {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at < self.data.len() && self.rng.gen_bool(0.3) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = self.rng.gen_range(1..=7).min(buf.len()).min(self.data.len() - self.at);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[test]
fn relay_messages_round_trip_whole_and_through_a_reader_that_stutters() {
    let mut r = rng(20);
    for i in 0..iters(800) {
        let messages: Vec<RelayMessage> = (0..r.gen_range(1..7)).map(|_| random_relay_message(&mut r)).collect();
        let mut wire = Vec::new();
        for m in &messages {
            let one = RelayCodec::encode(m).unwrap_or_else(|e| panic!("iteration {i}: {m:?}: {e}"));
            let (back, used) = RelayCodec::try_decode(&one).unwrap().expect("a whole message");
            assert_eq!((&back, used), (&as_received(m), one.len()), "iteration {i}");
            assert!(
                RelayCodec::try_decode(&one[..one.len() - 1]).unwrap().is_none(),
                "one byte short is not a message"
            );
            assert!(!m.summary().is_empty());
            wire.extend_from_slice(&one);
        }
        let expected: Vec<RelayMessage> = messages.iter().map(as_received).collect();

        // all at once
        let (mut rest, mut got) = (&wire[..], Vec::new());
        while let Some((m, used)) = RelayCodec::try_decode(rest).unwrap() {
            got.push(m);
            rest = &rest[used..];
        }
        assert_eq!((got, rest.len()), (expected.clone(), 0), "iteration {i}");

        // through a reader that never gives more than a few bytes and keeps saying "not yet"
        let mut stream = Stutter { data: wire, at: 0, rng: StdRng::seed_from_u64(i as u64) };
        let (mut reader, mut got) = (RelayReader::new(), Vec::new());
        loop {
            match reader.read_message(&mut stream) {
                Ok(Some(m)) => got.push(m),
                Ok(None) => break,
                Err(e) if e.is_timeout() => {}
                Err(e) => panic!("iteration {i}: {e}"),
            }
        }
        assert_eq!(got, expected, "iteration {i}");
        assert_eq!(reader.buffered(), 0);
    }
}

fn read_everything(bytes: &[u8]) -> (Vec<RelayMessage>, Option<RelayError>) {
    let (mut reader, mut cursor, mut got) = (RelayReader::new(), Cursor::new(bytes.to_vec()), Vec::new());
    loop {
        match reader.step(&mut cursor) {
            Ok(ReadStep::Message(m)) => got.push(m),
            Ok(ReadStep::More) => {}
            Ok(ReadStep::End) => return (got, None),
            Err(e) => return (got, Some(e)),
        }
    }
}

#[test]
fn relay_messages_of_any_shape_are_read_or_refused_and_a_length_costs_nothing_until_the_bytes_come() {
    let mut r = rng(21);
    for i in 0..iters(5000) {
        let bytes = match r.gen_range(0..5) {
            0 => random_bytes(&mut r, 300),
            1 => {
                let mut b = RelayCodec::encode(&random_relay_message(&mut r)).unwrap();
                mutate(&mut r, &mut b);
                b
            }
            2 => {
                // a length field at its limits, with next to nothing behind it
                let length = [
                    0u32,
                    1,
                    2,
                    17,
                    RelayCodec::MAX_LENGTH as u32,
                    RelayCodec::MAX_LENGTH as u32 + 1,
                    u32::MAX,
                    1 << 31,
                ][r.gen_range(0..8)];
                let mut b = length.to_be_bytes().to_vec();
                b.push([1u8, 2, 5, 16, 17, 18, 19, 200][r.gen_range(0..8)]);
                b.extend(random_bytes(&mut r, 20));
                b
            }
            3 => {
                // data messages whose frame inside is wrong in each way a frame can be
                let mut frame = random_frame_bytes(&mut r);
                match r.gen_range(0..4) {
                    0 => frame.truncate(r.gen_range(0..frame.len())),
                    1 => frame.extend(random_bytes(&mut r, 8)),
                    2 => frame[..4].copy_from_slice(&r.gen::<u32>().to_be_bytes()),
                    _ => frame.clear(),
                }
                let mut b = ((frame.len() + 1) as u32).to_be_bytes().to_vec();
                b.push(16);
                b.extend(frame);
                b
            }
            _ => (0..r.gen_range(1..4))
                .flat_map(|_| RelayCodec::encode(&random_relay_message(&mut r)).unwrap())
                .collect(),
        };
        let (decoded, peak) = peak_of(|| RelayCodec::try_decode(&bytes));
        assert!(peak <= bytes.len() + 4096, "iteration {i}: decode held {peak} for {} bytes", bytes.len());
        let ((messages, error), peak) = peak_of(|| read_everything(&bytes));
        // the reader reserves what the message in progress needs, a quarter MiB at most per read, and no more
        assert!(
            peak <= 2 * bytes.len() + 600 * 1024,
            "iteration {i}: the reader held {peak} for {} bytes",
            bytes.len()
        );
        if let Ok(Some((first, _))) = &decoded {
            assert_eq!(messages.first(), Some(first), "iteration {i}: the reader and the decoder agree");
        }
        // what came out can be sent again, and what is received then is stable
        for m in &messages {
            let again = RelayCodec::encode(m).unwrap_or_else(|e| panic!("iteration {i}: {m:?}: {e}"));
            let (back, _) = RelayCodec::try_decode(&again).unwrap().expect("a whole message");
            assert_eq!(back, as_received(m), "iteration {i}");
        }
        if let Some(e) = error {
            assert!(!e.to_string().is_empty() && !e.is_timeout());
            let _ = e.code();
        }
    }
}

#[test]
fn a_frame_inside_a_data_message_is_checked_and_only_a_whole_frame_passes() {
    let mut r = rng(22);
    for i in 0..iters(4000) {
        let valid = random_frame_bytes(&mut r);
        let bytes = match r.gen_range(0..3) {
            0 => valid.clone(),
            1 => random_bytes(&mut r, 100),
            _ => {
                let mut b = valid.clone();
                mutate(&mut r, &mut b);
                b
            }
        };
        match RelayCodec::decode_frame(&bytes) {
            Ok(frame) => assert_eq!(FrameCodec::encode_frame(&frame).unwrap(), bytes, "iteration {i}"),
            Err(e) => assert!(!e.to_string().is_empty()),
        }
        // the two ways of making a data message give the same bytes
        let frame = FrameCodec::try_decode(&valid).unwrap().unwrap().0;
        assert_eq!(
            RelayCodec::encode_data(&frame).unwrap(),
            RelayCodec::encode(&RelayMessage::Data(valid.clone())).unwrap()
        );
        let vconn: u32 = r.gen();
        assert_eq!(
            RelayCodec::encode_host_data(vconn, &frame).unwrap(),
            RelayCodec::encode(&RelayMessage::HostData { vconn, frame: valid }).unwrap()
        );
    }
    for code in 0..=u8::MAX {
        assert!(!ErrorCode(code).name().is_empty() && !CloseReason(code).name().is_empty());
    }
}

#[test]
fn relay_rooms_depend_on_the_secret_alone_and_say_nothing_about_the_key() {
    let mut r = rng(23);
    let mut seen = HashSet::new();
    for _ in 0..iters(2000) {
        let secret: [u8; 16] = r.gen();
        let room = SessionKey::relay_room_id(&secret);
        assert_eq!(room, SessionKey::relay_room_id(&secret));
        assert!(seen.insert(room), "two secrets with one room");
        for password in [None, Some("pw")] {
            let key = SessionKey::derive(&secret, password);
            assert_ne!(&room[..], &key.key()[..SessionKey::ROOM_ID_SIZE], "the room is not a part of the key");
        }
    }
}
