//! The limits of the sealed channel and the frame codec: what a peer who has not proved anything, or who sends
//! nonsense, can make the receiver allocate, accept or repeat. See `docs/SECURITY.md`.

use std::io::Cursor;

use feverlink3_core::crypto::{Opener, SealError, Sealer, SessionKey};
use feverlink3_core::lobby::{base32_decode, JoinCode};
use feverlink3_core::protocol::{Frame, FrameCodec, FrameHeader, ProtocolError};

const SECRET: [u8; 16] = [7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22];

#[test]
fn forged_frames_leave_the_opener_empty() {
    let key = SessionKey::derive(&SECRET, None);
    let mut opener = Opener::new(&key);
    for sender in (0..=u16::MAX).step_by(13) {
        let forged = Frame::new(FrameHeader::new(1, 30, sender, 1), vec![7u8; 40]);
        assert!(opener.try_open(&forged).is_err());
    }
    // a window is made for a sender only once a frame of that sender has authenticated
    assert_eq!(format!("{opener:?}"), "Opener { windows: {} }");
    let mut sealer = Sealer::new(&key, 9);
    assert!(opener.try_open(&sealer.seal(30, b"x")).is_ok());
    assert_eq!(format!("{opener:?}").matches("ReplayWindow").count(), 1);
}

#[test]
fn the_sealer_refuses_what_cannot_fit_a_frame_and_never_wraps_its_counter() {
    let key = SessionKey::derive(&SECRET, None);
    let mut sealer = Sealer::new(&key, 3);
    assert_eq!(sealer.try_seal(1, &vec![0; Sealer::MAX_PAYLOAD + 1]), Err(SealError::TooLong(Sealer::MAX_PAYLOAD + 1)));
    assert_eq!(sealer.last_counter(), 0, "a refused message uses no counter");
    let largest = sealer.try_seal(1, &vec![0; Sealer::MAX_PAYLOAD]).expect("the largest payload seals");
    assert!(FrameCodec::encode_frame(&largest).is_ok(), "and the frame fits the wire limit exactly");

    let mut last = Sealer::resume(&key, 3, u64::MAX - 1);
    assert_eq!(last.try_seal(1, b"x").map(|f| f.header.counter), Ok(u64::MAX));
    assert_eq!(last.try_seal(1, b"x"), Err(SealError::CounterExhausted));
    assert_eq!(last.last_counter(), u64::MAX, "no wrap, so no nonce is ever used twice");
}

#[test]
fn a_resumed_sealer_continues_the_counter_so_a_reused_id_never_repeats_a_nonce() {
    let key = SessionKey::derive(&SECRET, None);
    let mut first = Sealer::new(&key, 4);
    let used: Vec<u64> = (0..5).map(|_| first.seal(1, b"a").header.counter).collect();
    let mut second = Sealer::resume(&key, 4, first.last_counter());
    let next = second.seal(1, b"b").header.counter;
    assert!(used.iter().all(|c| *c < next));
    let mut opener = Opener::new(&key);
    assert!(opener.try_open(&second.seal(1, b"c")).is_ok());
}

#[test]
fn the_handshake_limit_refuses_a_long_first_frame_before_reading_it() {
    let header = FrameHeader::new(1, 2, 0xFFFF, 0);
    let body = vec![0u8; FrameCodec::HANDSHAKE_MAX_LENGTH];
    let mut cursor = Cursor::new(FrameCodec::encode(&header, &body).unwrap());
    let err = FrameCodec::read_limited(&mut cursor, FrameCodec::HANDSHAKE_MAX_LENGTH).unwrap_err();
    assert!(matches!(err, ProtocolError::OverLimit { limit: FrameCodec::HANDSHAKE_MAX_LENGTH, .. }), "{err}");
    assert_eq!(cursor.position(), 4, "only the length field was consumed");

    let fits = vec![0u8; FrameCodec::HANDSHAKE_MAX_LENGTH - FrameHeader::SIZE];
    let mut cursor = Cursor::new(FrameCodec::encode(&header, &fits).unwrap());
    let frame = FrameCodec::read_limited(&mut cursor, FrameCodec::HANDSHAKE_MAX_LENGTH).unwrap().unwrap();
    assert_eq!(frame.body.len(), fits.len());
    // asking for more than the protocol allows is the protocol's limit
    let huge = (FrameCodec::MAX_FRAME_LENGTH as u32 + 1).to_be_bytes();
    assert!(matches!(
        FrameCodec::read_limited(&mut Cursor::new(huge), usize::MAX),
        Err(ProtocolError::LengthOverLimit(_))
    ));
}

#[test]
fn header_try_read_is_the_form_for_outside_bytes() {
    assert_eq!(FrameHeader::try_read(&[1; 11]), None);
    assert_eq!(FrameHeader::try_read(&[]), None);
    let h = FrameHeader::new(1, 9, 513, 77);
    let mut padded = h.to_bytes().to_vec();
    padded.extend_from_slice(b"more");
    assert_eq!(FrameHeader::try_read(&padded), Some(h));
}

#[test]
fn join_code_text_is_bounded_before_it_is_decoded() {
    let code = JoinCode::create(&["10.0.0.9:28090".parse().unwrap()], SECRET, Some(7)).unwrap().encode();
    let spaced =
        format!("  {}  ", code.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c)).collect::<Vec<_>>().join("-"));
    assert!(spaced.len() < JoinCode::MAX_TEXT_LENGTH);
    assert!(JoinCode::try_decode(&spaced).is_some(), "a code written with dashes still reads");
    assert_eq!(JoinCode::try_decode(&"A".repeat(JoinCode::MAX_TEXT_LENGTH + 1)), None);
    assert_eq!(JoinCode::try_decode(&format!("{code}{}", " ".repeat(JoinCode::MAX_TEXT_LENGTH))), None);
    assert_eq!(base32_decode("MFRGG===").as_deref(), Some(&b"abc"[..]), "the general decoder is unchanged");
}

#[test]
fn an_encoded_code_never_holds_more_endpoints_than_a_code_can_carry() {
    let endpoints: Vec<std::net::SocketAddr> = (0..20).map(|i| format!("10.0.0.{i}:28090").parse().unwrap()).collect();
    let code = JoinCode { version: 1, created: 5, endpoints, secret: SECRET };
    let back = JoinCode::try_decode(&code.encode()).expect("the code reads back");
    assert_eq!(back.endpoints.len(), JoinCode::MAX_ENDPOINTS);
}
