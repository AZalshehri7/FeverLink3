use std::io::Cursor;

use feverlink3_core::protocol::{Frame, FrameCodec, FrameHeader, ProtocolError};

#[test]
fn header_round_trips() {
    let h = FrameHeader::new(1, 0x2A, 513, 0x0102030405060708);
    let b = h.to_bytes();
    assert_eq!(FrameHeader::SIZE, b.len());
    assert_eq!(h, FrameHeader::read(&b));
}

#[test]
fn frame_round_trips_through_a_stream() {
    let mut body = vec![0u8; 1000];
    getrandom::getrandom(&mut body).unwrap();
    let frame = Frame::new(FrameHeader::new(1, 7, 3, 42), body.clone());
    let mut ms = Cursor::new(Vec::new());
    FrameCodec::write(&mut ms, &frame).unwrap();
    FrameCodec::write(&mut ms, &Frame::new(FrameHeader::new(1, 8, 3, 43), Vec::new())).unwrap();
    ms.set_position(0);

    let first = FrameCodec::read(&mut ms).unwrap();
    let second = FrameCodec::read(&mut ms).unwrap();
    let end = FrameCodec::read(&mut ms).unwrap();

    let first = first.expect("first frame");
    assert_eq!(frame.header, first.header);
    assert_eq!(body, first.body);
    let second = second.expect("second frame");
    assert_eq!(8, second.header.kind);
    assert!(second.body.is_empty());
    assert!(end.is_none());
}

#[test]
fn try_decode_waits_for_a_whole_frame_then_reports_what_it_consumed() {
    let bytes = FrameCodec::encode(&FrameHeader::new(1, 1, 1, 1), &[9, 9, 9]).unwrap();
    assert!(FrameCodec::try_decode(&bytes[..bytes.len() - 1]).unwrap().is_none());
    let two: Vec<u8> = bytes.iter().chain(bytes.iter()).copied().collect();
    let (f, consumed) = FrameCodec::try_decode(&two).unwrap().expect("a whole frame");
    assert_eq!(bytes.len(), consumed);
    assert_eq!(vec![9, 9, 9], f.body);
}

#[test]
fn oversized_length_is_refused_before_any_allocation() {
    let buf = (FrameCodec::MAX_FRAME_LENGTH as u32 + 1).to_be_bytes();
    assert!(matches!(FrameCodec::read(&mut Cursor::new(buf)), Err(ProtocolError::LengthOverLimit(_))));
    assert!(matches!(FrameCodec::try_decode(&buf), Err(ProtocolError::LengthOverLimit(_))));
}

#[test]
fn length_shorter_than_a_header_is_refused() {
    let buf = 3u32.to_be_bytes();
    assert!(matches!(FrameCodec::read(&mut Cursor::new(buf)), Err(ProtocolError::LengthBelowHeader(3))));
}

#[test]
fn stream_ending_inside_a_frame_is_an_error_not_a_clean_end() {
    let bytes = FrameCodec::encode(&FrameHeader::new(1, 1, 1, 1), &[0u8; 100]).unwrap();
    let mut ms = Cursor::new(&bytes[..bytes.len() - 10]);
    assert!(FrameCodec::read(&mut ms).is_err());
}

#[test]
fn encode_refuses_a_body_over_the_limit() {
    let body = vec![0u8; FrameCodec::MAX_FRAME_LENGTH];
    assert!(matches!(FrameCodec::encode(&FrameHeader::new(1, 1, 1, 1), &body), Err(ProtocolError::LengthOverLimit(_))));
}
