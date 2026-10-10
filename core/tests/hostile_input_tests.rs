//! Whatever arrives from the network must not panic the host (a panic aborts the game), admit anyone it should not,
//! or disturb the other players: garbage before the handshake, tricky resume claims, sealed garbage after it.

mod net;

use feverlink3_core::crypto::Sealer;
use feverlink3_core::protocol::{Frame, FrameHeader};
use feverlink3_core::session::*;
use net::*;
use serde_json::json;

/// A small deterministic generator (xorshift64*): the same junk on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// Payloads that are valid JSON of the wrong shape, or not JSON at all.
fn tricky(rng: &mut Rng) -> Vec<u8> {
    let fixed: [&[u8]; 14] = [
        b"",
        b"null",
        b"[]",
        b"{}",
        b"\"text\"",
        br#"{"n":"x"}"#,
        br#"{"n":-1}"#,
        br#"{"n":99999999999999999999}"#,
        br#"{"id":-1,"name":5}"#,
        br#"{"t":1e400,"speed":"fast","ahead":null}"#,
        br#"{"text":12}"#,
        br#"{"stamp":"a","seq":[],"body":{}}"#,
        br#"{"speed":{"only":[1]}}"#,
        &[0xff, 0xfe, 0x00, 0x7b],
    ];
    if rng.below(3) == 0 {
        let len = rng.below(200) as usize;
        rng.bytes(len)
    } else {
        fixed[rng.below(fixed.len() as u64) as usize].to_vec()
    }
}

#[test]
fn garbage_before_the_handshake_admits_nobody_and_never_panics() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut n = Net::with(HostOptions { max_frames_per_second: 0, ..HostOptions::default() });
    for _ in 0..150 {
        let c = n.open_raw();
        for _ in 0..6 {
            let kind = rng.below(256) as u8;
            let sender = if rng.below(2) == 0 { HostSession::UNASSIGNED } else { rng.below(70_000) as u16 };
            let len = rng.below(80) as usize;
            let body = if rng.below(2) == 0 { tricky(&mut rng) } else { rng.bytes(len) };
            let frame = Frame::new(FrameHeader::new(1, kind, sender, rng.next()), body);
            n.inject(c, &frame);
        }
    }
    n.run_for(30.0, 1.0);
    assert_eq!(1, n.host.players().len(), "only the host's own game");
    assert_eq!(1, n.host.roster().len());
    assert!(n.host.dropped() > 0);
}

/// A Join with a valid proof for this connection and whatever the test wants as `resume`.
fn join_with(n: &mut Net, c: ConnectionId, name: &str, resume: serde_json::Value) -> Frame {
    let challenge: ChallengeMessage = codec::try_decode(&n.raw[&c][0].body).unwrap();
    let proof = HostSession::proof(&n.key, &challenge.nonce, name);
    let mut body = json!({"name": name, "version": "0.1", "proof": base64(&proof)});
    if !resume.is_null() {
        body["resume"] = resume;
    }
    let header = FrameHeader::new(1, MessageKind::Join.into(), HostSession::UNASSIGNED, 0);
    Frame::new(header, serde_json::to_vec(&body).unwrap())
}

fn base64(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.encode(bytes)
}

#[test]
fn tricky_resume_claims_never_panic_and_never_take_a_slot() {
    let mut n = Net::new();
    let alice = n.connect("Alice");
    let a_id = n.id(alice);
    let claims = [
        json!(5),
        json!("text"),
        json!([]),
        json!({}),
        json!({"id": 0, "mac": ""}),
        json!({"id": 65535, "mac": "AAAA"}),
        json!({"id": 70000, "mac": "AAAA"}),
        json!({"id": -1, "mac": "AAAA"}),
        json!({"id": a_id, "mac": 3}),
        json!({"id": a_id, "mac": "!!not base64!!"}),
        json!({"id": a_id, "mac": ""}),
        json!({"id": a_id, "mac": base64(&[0; 32])}),
        json!({"id": a_id, "mac": base64(&[7; 4096])}),
        json!({"id": a_id}),
        json!({"mac": "AAAA"}),
    ];
    for (i, claim) in claims.into_iter().enumerate() {
        let c = n.open_raw();
        let frame = join_with(&mut n, c, &format!("Mallory{i}"), claim);
        n.inject(c, &frame);
    }
    // Alice's own connection was never touched, whatever was claimed in her name
    assert_eq!(PlayerState::Joined, n.peer(alice).session.state());
    assert!(!n.closed.contains(&alice));
    assert_eq!(Some(ConnectionState::Connected), n.host.roster().iter().find(|e| e.id == a_id).map(|e| e.state));
    // a claim that does not even decode is refused as malformed; one that decodes but does not hold (no such slot, or
    // Alice's slot with a MAC that is not hers) is an ordinary join under a name of its own: ids 0 and 65535, an empty
    // MAC, 32 zero bytes and 4096 sevens
    let mallorys = n.host.roster().iter().filter(|e| e.name.starts_with("Mallory")).count();
    assert_eq!(5, mallorys);
    assert_eq!(7, n.host.players().len(), "the host, Alice and those five");
}

#[test]
fn sealed_garbage_from_a_joined_player_never_panics_and_disturbs_nobody() {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let mut n = Net::with(HostOptions { max_frames_per_second: 0, ..HostOptions::default() });
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    n.hello_all(5000, 0);
    n.host_call(HostSession::start);
    let (a_id, b_id) = (n.id(a), n.id(b));
    // a forger with the session key, writing as Alice from a counter range of its own
    let mut forger = Sealer::with_epoch(&n.key, a_id, 9).unwrap();
    for i in 0..4000 {
        // every kind but Leave, which is a legitimate request to go
        let kind = loop {
            let k = rng.below(256) as u8;
            if k != u8::from(MessageKind::Leave) {
                break k;
            }
        };
        let frame = forger.seal(kind, &tricky(&mut rng));
        n.inject(a, &frame);
        if i % 500 == 0 {
            n.advance(1.0);
            n.tick();
        }
    }
    // frames sealed for the wrong sender id, and unsealed ones, are no better
    let mut as_bob = Sealer::with_epoch(&n.key, b_id, 9).unwrap();
    for _ in 0..200 {
        let frame = as_bob.seal(MessageKind::Chat.into(), &tricky(&mut rng));
        n.inject(a, &frame);
    }
    for _ in 0..200 {
        let len = rng.below(100) as usize;
        let frame = Frame::new(FrameHeader::new(1, rng.below(256) as u8, a_id, rng.next()), rng.bytes(len));
        n.inject(a, &frame);
    }
    n.run_for(5.0, 1.0);

    assert!(n.host.players().contains_key(&a_id) && n.host.players().contains_key(&b_id));
    assert_eq!(3, n.host.players().len());
    assert_eq!(PlayerState::Joined, n.peer(b).session.state());
    // the host is still itself: the garbage did not start, pause or reconfigure anything it should not
    assert!(n.host.state().started);
    assert_eq!(&Permissions::default(), n.host.permissions());
    // and it still works
    n.send(b, MessageKind::Chat, &json(&ChatMessage::new("still here")));
    assert!(n.peer(a).count(MessageKind::Chat) >= 1);
}

#[test]
fn kick_and_roster_survive_any_id() {
    let mut n = Net::new();
    let _a = n.connect("Alice");
    for id in [0u16, 1, 2, 500, 65534, 65535] {
        n.host_call(|h| h.kick(id));
    }
    n.host_call(|h| h.kick(0));
    assert!(n.host.roster().iter().all(|e| e.id != 1), "id 1 was Alice and was removed");
    assert_eq!(1, n.host.roster().len());
}
