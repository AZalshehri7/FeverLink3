//! The kind numbers and JSON shapes that docs/PROTOCOL.md gives to the session layer's own messages: ping and pong,
//! being removed, leaving, and coming back.

use feverlink3_core::session::*;
use serde_json::{json, Value};

fn value<T: serde::Serialize>(m: &T) -> Value {
    serde_json::from_slice(&codec::encode(m)).unwrap()
}

#[test]
fn the_new_kinds_have_their_documented_numbers() {
    for (kind, number) in
        [(MessageKind::Ping, 21u8), (MessageKind::Kicked, 22), (MessageKind::Pong, 37), (MessageKind::Leave, 38)]
    {
        assert_eq!(number, u8::from(kind), "{kind:?}");
        assert_eq!(Some(kind), MessageKind::from_u8(number));
    }
}

#[test]
fn every_kind_number_maps_back_to_itself_and_nothing_else_does() {
    let known: Vec<u8> = (0..=255u8).filter(|&v| MessageKind::from_u8(v).is_some()).collect();
    assert_eq!(
        vec![
            1, 2, 3, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 30, 31, 32, 33, 34, 35, 36, 37, 38, 50, 51,
            52, 53
        ],
        known
    );
    for v in known {
        assert_eq!(v, u8::from(MessageKind::from_u8(v).unwrap()));
    }
}

#[test]
fn a_player_may_pong_and_leave_but_not_ping_or_kick() {
    assert!(MessageKind::Pong.allowed_from_player());
    assert!(MessageKind::Leave.allowed_from_player());
    assert!(!MessageKind::Ping.allowed_from_player());
    assert!(!MessageKind::Kicked.allowed_from_player());
    assert_eq!(13, MessageKind::FROM_PLAYER.len());
    assert!(MessageKind::FROM_PLAYER.iter().all(|k| !k.is_unsealed()));
    assert!(!MessageKind::Ping.is_unsealed() && !MessageKind::Kicked.is_unsealed(), "both are sealed");
}

#[test]
fn join_carries_the_claim_only_when_there_is_one() {
    let plain = JoinMessage::new("Ann", "0.1", vec![1, 2, 3]);
    assert_eq!(json!({"name": "Ann", "version": "0.1", "proof": "AQID"}), value(&plain));

    let mut back = plain.clone();
    back.resume = Some(ResumeClaim { id: 3, mac: vec![4, 5, 6] });
    assert_eq!(
        json!({"name": "Ann", "version": "0.1", "proof": "AQID", "resume": {"id": 3, "mac": "BAUG"}}),
        value(&back)
    );
    assert_eq!(Some(back.clone()), codec::try_decode::<JoinMessage>(&codec::encode(&back)));
    // a join from a build that knows nothing of claims
    let old = br#"{"name":"Ann","version":"0.1","proof":"AQID"}"#;
    assert_eq!(Some(plain), codec::try_decode::<JoinMessage>(old));
}

#[test]
fn welcome_carries_the_grant_and_says_when_it_gave_a_slot_back() {
    let session = SessionState { started: false, speed: 0, pause_at: None, lead: 1200, resync: None };
    let host = PlayerInfo { id: 0, name: "H".into(), ready: false };
    let mut w = WelcomeMessage {
        you: 2,
        host_name: "H".into(),
        version: "0.1".into(),
        players: vec![host],
        session,
        nonce: vec![9, 8, 7],
        resume: Some(ResumeGrant { token: vec![1, 2, 3], epoch: 2 }),
        resumed: true,
    };
    let v = value(&w);
    assert_eq!(json!({"token": "AQID", "epoch": 2}), v["resume"]);
    assert_eq!(json!(true), v["resumed"]);
    assert_eq!(json!(2), v["you"]);
    assert_eq!(json!("CQgH"), v["nonce"], "the challenge this welcome answers");

    w.resumed = false;
    assert!(value(&w).get("resumed").is_none(), "false is left out");
    w.resume = None;
    assert!(value(&w).get("resume").is_none());
    w.nonce.clear();
    assert!(value(&w).get("nonce").is_none(), "a host that predates the nonce sends none");

    // a welcome from a host that grants nothing still decodes
    let old =
        br#"{"you":2,"hostName":"H","version":"0.1","players":[],"session":{"started":false,"speed":0,"lead":1200}}"#;
    let decoded = codec::try_decode::<WelcomeMessage>(old).expect("decodes");
    assert_eq!((None, false, true), (decoded.resume, decoded.resumed, decoded.nonce.is_empty()));
}

#[test]
fn player_left_says_when_the_slot_is_held() {
    let gone = PlayerLeftMessage { id: 3, name: "Ann".into(), away: false };
    assert_eq!(json!({"id": 3, "name": "Ann"}), value(&gone));
    let away = PlayerLeftMessage { away: true, ..gone };
    assert_eq!(json!({"id": 3, "name": "Ann", "away": true}), value(&away));
    assert_eq!(Some(away), codec::try_decode(br#"{"id":3,"name":"Ann","away":true}"#));
    assert_eq!(
        Some(PlayerLeftMessage { id: 3, name: "Ann".into(), away: false }),
        codec::try_decode(br#"{"id":3,"name":"Ann"}"#)
    );
}

#[test]
fn ping_and_pong_are_tiny() {
    let ping = PingMessage { n: 7, rtt: [(1, 23.4), (3, 150.0)].into_iter().collect() };
    assert_eq!(json!({"n": 7, "rtt": {"1": 23.4, "3": 150.0}}), value(&ping));
    assert_eq!(Some(ping), codec::try_decode(br#"{"n":7,"rtt":{"1":23.4,"3":150.0}}"#));
    assert_eq!(json!({"n": 7}), value(&PingMessage { n: 7, rtt: Default::default() }), "no table, no key");
    assert_eq!(json!({"n": 7}), value(&PongMessage { n: 7 }));
    assert_eq!(None, codec::try_decode::<PongMessage>(b"{}"));
    assert!(codec::encode(&PongMessage { n: u32::MAX }).len() < 20);
}

#[test]
fn being_removed_uses_the_shape_of_a_reject() {
    let kicked = RejectMessage::new("kicked", Some("removed by the host"));
    assert_eq!(json!({"reason": "kicked", "detail": "removed by the host"}), value(&kicked));
    assert_eq!(json!({"reason": "flood"}), value(&RejectMessage::new("flood", None)));
}
