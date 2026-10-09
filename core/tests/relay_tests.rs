//! The relay protocol (`docs/RELAY.md`): message round trips, the limits, partial reads, and the room id.

use std::io::{self, Read};

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::protocol::{Frame, FrameCodec, FrameHeader};
use feverlink3_core::relay::{
    CloseReason, ErrorCode, ReadStep, RelayCodec, RelayError, RelayMessage, RelayReader, RoomId, Timing,
    DEFAULT_RELAY_PORT, RELAY_PROTOCOL_VERSION,
};

const ROOM: RoomId = [0xA5; 16];

fn frame(kind: u8, body: &[u8]) -> Frame {
    Frame::new(FrameHeader::new(1, kind, 7, 42), body.to_vec())
}

fn inner(kind: u8, body: &[u8]) -> Vec<u8> {
    FrameCodec::encode_frame(&frame(kind, body)).unwrap()
}

fn every_message() -> Vec<RelayMessage> {
    vec![
        RelayMessage::Register { version: RELAY_PROTOCOL_VERSION, room: ROOM },
        RelayMessage::Join { version: RELAY_PROTOCOL_VERSION, room: [0; 16] },
        RelayMessage::Registered(Timing { ping_secs: 15, idle_secs: 45 }),
        RelayMessage::Joined(Timing { ping_secs: 65535, idle_secs: 0 }),
        RelayMessage::error(ErrorCode::ROOM_TAKEN, "a host already holds this room"),
        RelayMessage::error(ErrorCode(250), ""),
        RelayMessage::Ping(u64::MAX),
        RelayMessage::Pong(0),
        RelayMessage::Data(inner(30, b"hello")),
        RelayMessage::Data(inner(50, b"")),
        RelayMessage::HostData { vconn: 0xDEAD_BEEF, frame: inner(11, &[7u8; 300]) },
        RelayMessage::Open { vconn: 1 },
        RelayMessage::Close { vconn: u32::MAX, reason: CloseReason::KICKED },
    ]
}

/// A deterministic byte source for the fuzz test.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn every_message_round_trips_and_consumes_exactly_its_bytes() {
    for message in every_message() {
        let bytes = RelayCodec::encode(&message).unwrap();
        let (back, used) = RelayCodec::try_decode(&bytes).unwrap().expect("a whole message");
        assert_eq!(back, message, "{}", message.summary());
        assert_eq!(used, bytes.len());
        // trailing bytes of the next message are left alone
        let mut two = bytes.clone();
        two.extend_from_slice(&bytes);
        assert_eq!(RelayCodec::try_decode(&two).unwrap().unwrap().1, bytes.len());
    }
}

#[test]
fn the_wire_bytes_are_the_documented_ones() {
    // length 18 = type + version + 16 room bytes
    let mut want = vec![0, 0, 0, 18, 1, RELAY_PROTOCOL_VERSION];
    want.extend_from_slice(&ROOM);
    assert_eq!(
        RelayCodec::encode(&RelayMessage::Register { version: RELAY_PROTOCOL_VERSION, room: ROOM }).unwrap(),
        want
    );
    assert_eq!(RelayCodec::encode(&RelayMessage::Open { vconn: 0x0102_0304 }).unwrap(), [0, 0, 0, 5, 18, 1, 2, 3, 4]);
    assert_eq!(
        RelayCodec::encode(&RelayMessage::Close { vconn: 9, reason: CloseReason::TIMEOUT }).unwrap(),
        [0, 0, 0, 6, 19, 0, 0, 0, 9, 2]
    );
    assert_eq!(
        RelayCodec::encode(&RelayMessage::Registered(Timing { ping_secs: 15, idle_secs: 45 })).unwrap(),
        [0, 0, 0, 5, 3, 0, 15, 0, 45]
    );
    let f = inner(30, b"x");
    let mut data = vec![0, 0, 0, 1 + f.len() as u8, 16];
    data.extend_from_slice(&f);
    assert_eq!(RelayCodec::encode(&RelayMessage::Data(f.clone())).unwrap(), data);
    // the hot-path encoders give the same bytes as the general one
    assert_eq!(RelayCodec::encode_data(&frame(30, b"x")).unwrap(), data);
    assert_eq!(
        RelayCodec::encode_host_data(3, &frame(30, b"x")).unwrap(),
        RelayCodec::encode(&RelayMessage::HostData { vconn: 3, frame: f }).unwrap()
    );
}

#[test]
fn no_proper_prefix_of_a_message_decodes_or_fails() {
    for message in every_message() {
        let bytes = RelayCodec::encode(&message).unwrap();
        for n in 0..bytes.len() {
            match RelayCodec::try_decode(&bytes[..n]) {
                Ok(None) => {}
                other => panic!("{} cut to {n} bytes gave {other:?}", message.summary()),
            }
        }
    }
}

#[test]
fn the_length_field_is_bounded_before_anything_is_allocated() {
    assert!(matches!(RelayCodec::try_decode(&[0, 0, 0, 0]), Err(RelayError::LengthZero)));
    let over = (RelayCodec::MAX_LENGTH as u32 + 1).to_be_bytes();
    assert!(
        matches!(RelayCodec::try_decode(&over), Err(RelayError::LengthOverLimit(_))),
        "four bytes are enough to refuse"
    );
    assert!(matches!(RelayCodec::try_decode(&[0xFF; 4]), Err(RelayError::LengthOverLimit(_))));
    // a stream that announces a huge message is refused after its first four bytes
    let mut reader = RelayReader::new();
    let mut stream: &[u8] = &[0x41; 4];
    assert!(matches!(reader.read_message(&mut stream), Err(RelayError::LengthOverLimit(_))));
}

#[test]
fn the_largest_frame_fits_in_a_host_data_message() {
    // a frame of exactly MAX_FRAME_LENGTH after its length field
    let body = vec![0xEE; FrameCodec::MAX_FRAME_LENGTH - FrameHeader::SIZE];
    let f = frame(18, &body);
    let bytes = RelayCodec::encode_host_data(70_000, &f).unwrap();
    assert!(bytes.len() - 4 <= RelayCodec::MAX_LENGTH);
    match RelayCodec::try_decode(&bytes).unwrap().unwrap().0 {
        RelayMessage::HostData { vconn, frame: inner } => {
            assert_eq!(vconn, 70_000);
            assert_eq!(RelayCodec::decode_frame(&inner).unwrap(), f);
        }
        other => panic!("{other:?}"),
    }
    // one byte more than the core allows is refused by the encoder
    let too_big = frame(18, &vec![0; FrameCodec::MAX_FRAME_LENGTH - FrameHeader::SIZE + 1]);
    assert!(RelayCodec::encode_data(&too_big).is_err());
    assert!(RelayCodec::encode_host_data(1, &too_big).is_err());
}

#[test]
fn a_data_message_must_hold_exactly_one_frame() {
    let good = inner(30, b"abc");
    let mut cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("shorter than a prefix", vec![0, 0]),
        ("prefix only", vec![0, 0, 0, 0]),
        ("shorter than a header", vec![0, 0, 0, 5, 1, 2, 3, 4, 5]),
    ];
    let mut long = good.clone();
    long.push(0);
    cases.push(("one byte too many", long));
    cases.push(("one byte too few", good[..good.len() - 1].to_vec()));
    let mut two = good.clone();
    two.extend_from_slice(&good);
    cases.push(("two frames", two));
    for (what, bytes) in cases {
        let mut wire = vec![0, 0, 0, 0, 16];
        wire.extend_from_slice(&bytes);
        let len = (wire.len() - 4) as u32;
        wire[..4].copy_from_slice(&len.to_be_bytes());
        match RelayCodec::try_decode(&wire) {
            Err(RelayError::BadFrame(_)) => {}
            other => panic!("{what}: {other:?}"),
        }
        assert!(RelayCodec::encode(&RelayMessage::Data(bytes)).is_err(), "{what}: the encoder refuses it too");
    }
}

#[test]
fn an_oversized_inner_frame_is_reported_as_too_large() {
    // inner prefix claims 2 MiB; the outer length is within bounds
    let mut f = vec![0u8; 4 + 64];
    f[..4].copy_from_slice(&(2u32 << 20).to_be_bytes());
    let mut wire = vec![0, 0, 0, 0, 16];
    wire.extend_from_slice(&f);
    let len = (wire.len() - 4) as u32;
    wire[..4].copy_from_slice(&len.to_be_bytes());
    let err = RelayCodec::try_decode(&wire).unwrap_err();
    assert!(matches!(err, RelayError::FrameTooLarge(n) if n == 2 << 20), "{err}");
    assert_eq!(err.code(), ErrorCode::FRAME_TOO_LARGE);
    assert_eq!(RelayError::LengthOverLimit(5).code(), ErrorCode::FRAME_TOO_LARGE);
    assert_eq!(RelayError::UnknownType(99).code(), ErrorCode::BAD_REQUEST);
}

#[test]
fn unknown_types_and_wrong_sizes_are_errors() {
    for kind in [0u8, 8, 9, 15, 20, 255] {
        let wire = [0, 0, 0, 2, kind, 0];
        assert!(matches!(RelayCodec::try_decode(&wire), Err(RelayError::UnknownType(k)) if k == kind), "{kind}");
    }
    // (type, body length that is wrong)
    for (kind, len) in
        [(1u8, 16usize), (1, 18), (2, 0), (3, 3), (4, 5), (5, 0), (6, 7), (7, 9), (17, 3), (18, 3), (19, 4)]
    {
        let mut wire = vec![0, 0, 0, 0, kind];
        wire.extend(std::iter::repeat_n(0u8, len));
        let l = (wire.len() - 4) as u32;
        wire[..4].copy_from_slice(&l.to_be_bytes());
        assert!(RelayCodec::try_decode(&wire).is_err(), "type {kind} with a body of {len} bytes");
    }
}

#[test]
fn error_text_is_cut_on_a_character_boundary_and_long_text_is_refused_on_input() {
    let long = "é".repeat(300); // two bytes each
    let bytes = RelayCodec::encode(&RelayMessage::error(ErrorCode::BAD_REQUEST, &long)).unwrap();
    match RelayCodec::try_decode(&bytes).unwrap().unwrap().0 {
        RelayMessage::Error { text, .. } => {
            assert!(text.len() <= RelayCodec::MAX_ERROR_TEXT && text.chars().all(|c| c == 'é'), "{}", text.len());
            assert_eq!(text.len(), RelayCodec::MAX_ERROR_TEXT);
        }
        other => panic!("{other:?}"),
    }
    let mut wire = vec![0, 0, 0, 0, 5, 1];
    wire.extend(std::iter::repeat_n(b'x', 2000));
    let l = (wire.len() - 4) as u32;
    wire[..4].copy_from_slice(&l.to_be_bytes());
    assert!(RelayCodec::try_decode(&wire).is_err());
    // invalid UTF-8 from a peer becomes replacement characters, not an error
    let wire = [0, 0, 0, 4, 5, 1, 0xFF, 0xFE];
    assert!(matches!(RelayCodec::try_decode(&wire).unwrap().unwrap().0, RelayMessage::Error { .. }));
}

#[test]
fn random_bytes_never_panic_the_decoder() {
    let mut rng = Xorshift(0x9E37_79B9_7F4A_7C15);
    let valid: Vec<Vec<u8>> = every_message().iter().map(|m| RelayCodec::encode(m).unwrap()).collect();
    for round in 0..20_000u32 {
        let mut buf: Vec<u8> = if round % 2 == 0 {
            // a valid message with a few bytes flipped
            let mut b = valid[(rng.next() % valid.len() as u64) as usize].clone();
            for _ in 0..(1 + rng.next() % 3) {
                let i = (rng.next() % b.len() as u64) as usize;
                b[i] = rng.next() as u8;
            }
            b
        } else {
            let n = (rng.next() % 80) as usize;
            let mut b: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
            if b.len() >= 4 && rng.next().is_multiple_of(2) {
                let len = (b.len() - 4) as u32;
                b[..4].copy_from_slice(&len.to_be_bytes()); // a plausible length, so the body parser runs
            }
            b
        };
        let _ = RelayCodec::try_decode(&buf);
        let mut reader = RelayReader::new();
        let mut stream: &[u8] = &buf;
        while let Ok(Some(_)) = reader.read_message(&mut stream) {}
        buf.clear();
    }
}

/// A stream that hands out its bytes `chunk` at a time and, with `stall`, reports a timeout before each chunk.
struct Trickle {
    data: Vec<u8>,
    at: usize,
    chunk: usize,
    stall: bool,
    stalled: bool,
}

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.stall && !self.stalled && self.at < self.data.len() {
            self.stalled = true;
            return Err(io::Error::new(io::ErrorKind::TimedOut, "read timeout"));
        }
        self.stalled = false;
        let n = self.chunk.min(buf.len()).min(self.data.len() - self.at);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[test]
fn the_reader_assembles_messages_from_single_bytes() {
    let messages = every_message();
    let wire: Vec<u8> = messages.iter().flat_map(|m| RelayCodec::encode(m).unwrap()).collect();
    for chunk in [1usize, 2, 3, 7, 4096] {
        let mut stream = Trickle { data: wire.clone(), at: 0, chunk, stall: false, stalled: false };
        let mut reader = RelayReader::new();
        for want in &messages {
            assert_eq!(&reader.read_message(&mut stream).unwrap().expect("a message"), want, "chunk {chunk}");
        }
        assert!(reader.read_message(&mut stream).unwrap().is_none(), "clean end of stream");
        assert_eq!(reader.buffered(), 0);
    }
}

#[test]
fn step_returns_after_every_read_so_a_peer_that_trickles_cannot_keep_the_caller_waiting() {
    let wire = RelayCodec::encode(&RelayMessage::Ping(9)).unwrap();
    let mut stream = Trickle { data: wire.clone(), at: 0, chunk: 1, stall: false, stalled: false };
    let mut reader = RelayReader::new();
    let mut more = 0;
    loop {
        match reader.step(&mut stream).unwrap() {
            ReadStep::More => more += 1,
            ReadStep::Message(m) => {
                assert_eq!(m, RelayMessage::Ping(9));
                break;
            }
            ReadStep::End => panic!("the stream ended before the message"),
        }
    }
    assert_eq!(more, wire.len() - 1, "one step per byte, the last one brings the message");
    assert_eq!(reader.step(&mut stream).unwrap(), ReadStep::End);
    // several messages in one read come out one per step, the later ones without touching the stream
    let two: Vec<u8> =
        [RelayMessage::Ping(1), RelayMessage::Pong(2)].iter().flat_map(|m| RelayCodec::encode(m).unwrap()).collect();
    let mut stream: &[u8] = &two;
    let mut reader = RelayReader::new();
    assert_eq!(reader.step(&mut stream).unwrap(), ReadStep::Message(RelayMessage::Ping(1)));
    assert_eq!(reader.buffered(), two.len() / 2);
    assert_eq!(reader.step(&mut stream).unwrap(), ReadStep::Message(RelayMessage::Pong(2)));
    assert_eq!(reader.step(&mut stream).unwrap(), ReadStep::End);
}

#[test]
fn the_default_port_is_the_documented_one() {
    assert_eq!(DEFAULT_RELAY_PORT, 28100);
}

#[test]
fn a_read_timeout_in_the_middle_of_a_message_loses_nothing() {
    let messages = every_message();
    let wire: Vec<u8> = messages.iter().flat_map(|m| RelayCodec::encode(m).unwrap()).collect();
    let mut stream = Trickle { data: wire, at: 0, chunk: 5, stall: true, stalled: false };
    let mut reader = RelayReader::new();
    let mut got = Vec::new();
    let mut timeouts = 0;
    while got.len() < messages.len() {
        match reader.read_message(&mut stream) {
            Ok(Some(m)) => got.push(m),
            Ok(None) => panic!("end of stream too early"),
            Err(e) if e.is_timeout() => timeouts += 1,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(got, messages);
    assert!(timeouts > 10, "the stream really stalled {timeouts} times");
}

#[test]
fn a_stream_that_ends_inside_a_message_is_truncated_not_clean() {
    let bytes = RelayCodec::encode(&RelayMessage::Ping(5)).unwrap();
    let mut reader = RelayReader::new();
    let mut stream: &[u8] = &bytes[..bytes.len() - 1];
    assert!(matches!(reader.read_message(&mut stream), Err(RelayError::Truncated)));
    let mut reader = RelayReader::new();
    let mut empty: &[u8] = &[];
    assert!(reader.read_message(&mut empty).unwrap().is_none());
    // a stream with two bytes of a length field is truncated too
    let mut reader = RelayReader::new();
    let mut stream: &[u8] = &[0, 0];
    assert!(matches!(reader.read_message(&mut stream), Err(RelayError::Truncated)));
}

#[test]
fn a_big_frame_does_not_leave_its_buffer_behind() {
    let body = vec![1u8; 600_000];
    let wire = RelayCodec::encode_host_data(2, &frame(18, &body)).unwrap();
    let mut stream: &[u8] = &wire;
    let mut reader = RelayReader::new();
    assert!(matches!(reader.read_message(&mut stream).unwrap(), Some(RelayMessage::HostData { vconn: 2, .. })));
    assert_eq!(reader.buffered(), 0);
}

#[test]
fn names_of_codes_are_stable_because_they_are_logged_and_documented() {
    let names: Vec<String> = (1..=13).map(|n| ErrorCode(n).name()).collect();
    assert_eq!(
        names,
        [
            "bad-request",
            "bad-version",
            "room-taken",
            "no-such-room",
            "room-full",
            "too-many-rooms",
            "too-many-connections",
            "ip-limit",
            "frame-too-large",
            "rate-limited",
            "timeout",
            "host-gone",
            "shutdown"
        ]
    );
    assert_eq!(ErrorCode(99).to_string(), "error-99");
    assert_eq!(CloseReason::LEFT.to_string(), "left");
    assert_eq!(CloseReason::KICKED.to_string(), "kicked");
    assert_eq!(CloseReason(77).to_string(), "reason-77");
}

// ---- the room id

const SECRET: [u8; 16] = [7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22];

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_room_id_is_a_fixed_128_bit_function_of_the_secret() {
    let id = SessionKey::relay_room_id(&SECRET);
    assert_eq!(id.len(), 16);
    assert_eq!(id, SessionKey::relay_room_id(&SECRET), "everyone with the code derives the same room");
    // HKDF-SHA256(ikm = secret, salt = "FeverLink3 v1", info = "relay room", 16), checked against a second
    // implementation (Python's hmac and hashlib)
    assert_eq!(hex(&id), "ac85250ee7fdeba70bb55864e6f17950");
    let mut other = SECRET;
    other[15] ^= 1;
    assert_ne!(SessionKey::relay_room_id(&other), id);
}

#[test]
fn the_room_id_does_not_reveal_the_secret_or_the_session_key() {
    let id = SessionKey::relay_room_id(&SECRET);
    assert_ne!(id[..], SECRET[..], "not the secret");
    let key = SessionKey::derive(&SECRET, None);
    assert_ne!(id[..], key.key()[..16], "not the start of the session key either");
    assert_ne!(id[..], key.key()[16..], "nor the end of it");
    // the password changes the session key, never the room: a joiner knows the code before the password
    let with_password = SessionKey::derive(&SECRET, Some("pw"));
    assert_ne!(key.key(), with_password.key());
    assert_eq!(SessionKey::relay_room_id(&SECRET), id);
}

#[test]
fn room_ids_of_random_secrets_do_not_collide() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..200 {
        assert!(seen.insert(SessionKey::relay_room_id(&SessionKey::new_secret())));
    }
}
