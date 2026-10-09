use feverlink3_core::crypto::{OpenError, Opener, ReplayWindow, Sealer, SessionKey};
use feverlink3_core::protocol::{Frame, FrameCodec, FrameHeader};

const SECRET: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

#[test]
fn same_secret_and_password_give_the_same_key_and_different_ones_do_not() {
    let a = SessionKey::derive(&SECRET, None);
    let b = SessionKey::derive(&SECRET, None);
    let c = SessionKey::derive(&SECRET, Some("hunter2"));
    let d = SessionKey::derive(&SessionKey::new_secret(), None);
    assert_eq!(a.key(), b.key());
    assert_ne!(a.key(), c.key());
    assert_ne!(a.key(), d.key());
    assert_eq!(SessionKey::KEY_SIZE, a.key().len());
}

#[test]
fn sealed_frame_opens_to_the_same_plaintext() {
    let key = SessionKey::derive(&SECRET, None);
    let mut sealer = Sealer::new(&key, 5);
    let mut opener = Opener::new(&key);
    let text = b"{\"kind\":\"hello\"}";
    let frame = sealer.seal(3, text);

    assert_eq!(3, frame.header.kind);
    assert_eq!(5, frame.header.sender);
    assert_eq!(1, frame.header.counter);
    assert_eq!(text.len() + Sealer::TAG_SIZE, frame.body.len());
    let plain = opener.try_open(&frame).expect("opens");
    assert_eq!(text.to_vec(), plain);
}

#[test]
fn sealed_frame_survives_the_wire_format() {
    let key = SessionKey::derive(&SECRET, Some("pw"));
    let mut sealer = Sealer::new(&key, 1);
    let mut opener = Opener::new(&key);
    let frame = sealer.seal(9, &[1, 2, 3]);
    let (decoded, _) = FrameCodec::try_decode(&FrameCodec::encode_frame(&frame).unwrap()).unwrap().expect("a frame");
    assert_eq!(Ok(vec![1, 2, 3]), opener.try_open(&decoded));
}

fn altered_body_is_tamper(idx: usize) {
    let key = SessionKey::derive(&SECRET, None);
    let mut sealer = Sealer::new(&key, 1);
    let mut opener = Opener::new(&key);
    let frame = sealer.seal(1, &[10, 20, 30, 40]);
    let mut body = frame.body.clone();
    body[idx] ^= 0x01;
    assert_eq!(Err(OpenError::Tamper), opener.try_open(&Frame::new(frame.header, body)));
}

#[test]
fn altered_body_is_tamper_ciphertext() {
    altered_body_is_tamper(0);
}

#[test]
fn altered_body_is_tamper_tag() {
    altered_body_is_tamper(4 + Sealer::TAG_SIZE - 1);
}

#[test]
fn altered_header_kind_or_sender_is_tamper() {
    let key = SessionKey::derive(&SECRET, None);
    let mut sealer = Sealer::new(&key, 1);
    let mut opener = Opener::new(&key);
    let frame = sealer.seal(1, &[1]);
    let h = frame.header;
    let other_kind = FrameHeader { kind: 2, ..h };
    assert_eq!(Err(OpenError::Tamper), opener.try_open(&Frame::new(other_kind, frame.body.clone())));
    let other_sender = FrameHeader { sender: 2, ..h };
    assert_eq!(Err(OpenError::Tamper), opener.try_open(&Frame::new(other_sender, frame.body.clone())));
}

#[test]
fn wrong_password_is_tamper() {
    let mut sealer = Sealer::new(&SessionKey::derive(&SECRET, Some("right")), 1);
    let mut opener = Opener::new(&SessionKey::derive(&SECRET, Some("wrong")));
    let frame = sealer.seal(1, &[1]);
    assert_eq!(Err(OpenError::Tamper), opener.try_open(&frame));
}

#[test]
fn replayed_frame_is_refused_and_a_forged_frame_does_not_burn_its_counter() {
    let key = SessionKey::derive(&SECRET, None);
    let mut sealer = Sealer::new(&key, 1);
    let mut opener = Opener::new(&key);
    let f1 = sealer.seal(1, &[1]);
    let f2 = sealer.seal(1, &[2]);

    // a forgery carrying counter 2 arrives first: refused, and counter 2 stays usable by the real frame
    let mut forged = f2.body.clone();
    forged[0] ^= 0xFF;
    assert_eq!(Err(OpenError::Tamper), opener.try_open(&Frame::new(f2.header, forged)));

    assert!(opener.try_open(&f2).is_ok());
    assert!(opener.try_open(&f1).is_ok()); // out of order within the window: fine
    assert_eq!(Err(OpenError::Replay), opener.try_open(&f1));
    assert_eq!(Err(OpenError::Replay), opener.try_open(&f2));
}

#[test]
fn counters_older_than_the_window_are_refused() {
    let key = SessionKey::derive(&SECRET, None);
    let mut sealer = Sealer::new(&key, 1);
    let mut opener = Opener::new(&key);
    let early = sealer.seal(1, &[1]);
    let mut last = None;
    for _ in 0..ReplayWindow::SIZE {
        last = Some(sealer.seal(1, &[2]));
    }
    assert!(opener.try_open(&last.unwrap()).is_ok()); // highest = 65
    assert_eq!(Err(OpenError::Replay), opener.try_open(&early));
}

#[test]
fn senders_have_independent_counters() {
    let key = SessionKey::derive(&SECRET, None);
    let mut a = Sealer::new(&key, 1);
    let mut b = Sealer::new(&key, 2);
    let mut opener = Opener::new(&key);
    assert!(opener.try_open(&a.seal(1, &[1])).is_ok());
    assert!(opener.try_open(&b.seal(1, &[1])).is_ok()); // counter 1 again, other sender
}

#[test]
fn other_version_or_short_body_is_refused_without_decrypting() {
    let mut opener = Opener::new(&SessionKey::derive(&SECRET, None));
    assert_eq!(Err(OpenError::BadVersion), opener.try_open(&Frame::new(FrameHeader::new(2, 1, 1, 1), vec![0; 32])));
    assert_eq!(Err(OpenError::TooShort), opener.try_open(&Frame::new(FrameHeader::new(1, 1, 1, 1), vec![0; 15])));
}

#[test]
fn replay_window_bitmap_tracks_out_of_order_counters() {
    let mut w = ReplayWindow::new();
    assert!(!w.is_fresh(0));
    w.accept(5).unwrap();
    assert!(w.is_fresh(3));
    assert!(!w.is_fresh(5));
    w.accept(3).unwrap();
    assert!(!w.is_fresh(3));
    w.accept(5 + ReplayWindow::SIZE).unwrap(); // slides the window past 5
    assert!(!w.is_fresh(5));
    assert!(w.is_fresh(6));
    assert_eq!(Err(OpenError::Replay), w.accept(5));
}
