//! Catching up after a short drop (`docs/PROTOCOL.md`, "Catching up"): a player whose link drops for less than the grace and whose
//! game kept its world gets every message it missed, in order and once, resends what its own game sent that never arrived, and
//! catches up as a stalled game does; no reload. Beyond what the host kept, or with a world that differs afterwards, the old repair.

mod net;

use feverlink3_core::session::*;
use net::*;

fn action(stamp: i64, seq: i64, body: &str) -> Vec<u8> {
    json(&ActionMessage::new(stamp, seq, body).with_category("build"))
}

/// The bodies of the actions a game was given, in order.
fn bodies(game: &[(MessageKind, Vec<u8>)]) -> Vec<String> {
    game.iter()
        .filter(|g| g.0 == MessageKind::Action)
        .map(|g| codec::try_decode::<ActionMessage>(&g.1).expect("decodes").body)
        .collect()
}

fn vetoes(game: &[(MessageKind, Vec<u8>)]) -> Vec<BuildVetoMessage> {
    game.iter()
        .filter(|g| g.0 == MessageKind::BuildVeto)
        .map(|g| codec::try_decode::<BuildVetoMessage>(&g.1).expect("decodes"))
        .collect()
}

#[test]
fn a_short_drop_is_caught_up_in_order_without_a_reload_and_nothing_arrives_twice() {
    let (mut n, ids) = Net::running(2, 1, HostOptions::default());
    let (a, b) = (ids[0], ids[1]);
    let a_id = n.id(a);

    // delivered and acknowledged (pings and pongs) before the drop: never sent again
    n.send(b, MessageKind::Action, &action(20_000, 1, "before"));
    n.run_for(4.0, 0.5);

    // the link dies unnoticed: what goes either way now is lost on the way
    n.cut(a);
    n.send(b, MessageKind::Action, &action(20_200, 2, "lost-to-alice"));
    n.send(a, MessageKind::Action, &action(50_000, 1, "lost-from-alice"));
    n.send(a, MessageKind::Action, &action(10_000, 2, "late-from-alice"));
    // the host notices; the others run on while she is away
    n.lose(a);
    n.send(b, MessageKind::Action, &action(20_400, 3, "while-away"));
    n.host_send(MessageKind::Action, &action(20_600, 1, "host-while-away"));
    n.run_for(5.0, 0.5);

    let a2 = n.reconnect(a);
    let w = &n.peer(a2).welcomes[0];
    assert!(w.resumed && w.catch_up.is_some(), "caught up: {w:?}");
    assert_eq!(a_id, n.id(a2));
    assert_eq!(ResyncState::Idle, n.host.resync_state(), "no reload: {:?}", n.host_log);
    assert!(n.peer(a2).saves.is_empty());
    assert!(n.host.players()[&a_id].ready, "its game kept its world: still ready");
    assert!(n.logged("resume: P0 is caught up"), "{:?}", n.host_log);

    // what her game is given: exactly what it missed, in the order every other game had it, after the welcome
    let mut seen = bodies(&n.peer(a).game);
    seen.extend(bodies(&n.peer(a2).game));
    assert_eq!(vec!["before", "lost-to-alice", "while-away", "host-while-away"], seen);
    let game = &n.peer(a2).game;
    assert_eq!(MessageKind::Welcome, game[0].0, "the welcome first");
    assert!(game.iter().all(|g| g.0 != MessageKind::ResyncLoad));

    // what she sent on the way arrives once; the action stamped before what the others may have simulated is refused everywhere
    let at_b = bodies(&n.peer(b).game);
    assert_eq!(1, at_b.iter().filter(|x| *x == "lost-from-alice").count(), "{at_b:?}");
    assert!(!at_b.iter().any(|x| x == "late-from-alice"), "never applied late: {at_b:?}");
    let refused = vetoes(&n.peer(b).game);
    assert!(refused.iter().any(|v| v.host && v.owner == a_id && v.seq == 2), "{refused:?}");
    assert!(vetoes(&n.peer(a2).game).iter().any(|v| v.host && v.seq == 2), "and on her own game");
    let host_game: Vec<ActionMessage> = n.host_game_all(MessageKind::Action);
    assert_eq!(1, host_game.iter().filter(|m| m.body == "lost-from-alice").count());

    // she catches up as a stalled game: the others are given the host's promise for her and run on
    assert!(n.host.state().stalled.contains(&a_id));
    let proxy: PeerClockMessage = n.peer(b).last(MessageKind::PeerClock);
    assert!(proxy.stalled && proxy.from == a_id, "{proxy:?}");
    let promise = proxy.safe.unwrap();
    // once her own promise reaches it, she is an ordinary game again
    n.send(
        a2,
        MessageKind::Clock,
        &json(&ClockMessage {
            t: promise - 600,
            speed: 1,
            ahead: 3,
            need: None,
            safe: Some(promise),
            pause: false,
            stop: None,
            hop: None,
        }),
    );
    assert!(!n.host.state().stalled.contains(&a_id), "{:?}", n.host_log);
}

#[test]
fn a_world_that_differs_after_catching_up_is_repaired_at_once() {
    let (mut n, ids) = Net::running(1, 1, HostOptions::default());
    let a = ids[0];
    n.cut(a);
    n.lose(a);
    n.run_for(2.0, 0.5);
    let a2 = n.reconnect(a);
    assert!(n.peer(a2).welcomes[0].catch_up.is_some());

    n.host_call(HostSession::request_checksum);
    let req: ChecksumRequestMessage = n.peer(a2).last(MessageKind::ChecksumRequest);
    n.host_send(
        MessageKind::ChecksumReport,
        &json(&ChecksumReportMessage::new(req.n, req.at, parts(&[("roads", "1")]))),
    );
    n.send(
        a2,
        MessageKind::ChecksumReport,
        &json(&ChecksumReportMessage::new(req.n, req.at, parts(&[("roads", "2")]))),
    );
    assert!(n.logged("caught up but the worlds differ"), "{:?}", n.host_log);
    assert_ne!(ResyncState::Idle, n.host.resync_state(), "the repair starts at once, not after a second round");
}

#[test]
fn a_matching_world_after_catching_up_is_left_alone() {
    let (mut n, ids) = Net::running(1, 1, HostOptions::default());
    let a = ids[0];
    n.lose(a);
    let a2 = n.reconnect(a);
    n.host_call(HostSession::request_checksum);
    let req: ChecksumRequestMessage = n.peer(a2).last(MessageKind::ChecksumRequest);
    for c in [None, Some(a2)] {
        let report = json(&ChecksumReportMessage::new(req.n, req.at, parts(&[("roads", "1")])));
        match c {
            None => n.host_send(MessageKind::ChecksumReport, &report),
            Some(c) => n.send(c, MessageKind::ChecksumReport, &report),
        }
    }
    assert!(n.logged("caught up and the worlds match"), "{:?}", n.host_log);
    assert_eq!(ResyncState::Idle, n.host.resync_state());
}

#[test]
fn away_longer_than_the_grace_the_return_reloads() {
    let (mut n, ids) = Net::running(1, 1, HostOptions::default());
    let a = ids[0];
    n.lose(a);
    n.run_for(61.0, 1.0);
    assert!(n.logged("away longer than 60 s"), "{:?}", n.host_log);
    let a2 = n.reconnect(a);
    let w = &n.peer(a2).welcomes[0];
    assert!(w.resumed && w.catch_up.is_none());
    assert!(n.logged("cannot be caught up (away 61 s, longer than the 60 s kept)"), "{:?}", n.host_log);
    assert!(n.logged("resync 1 started: player returned"), "{:?}", n.host_log);
}

#[test]
fn a_resync_while_the_player_is_away_makes_its_return_a_reload() {
    let (mut n, ids) = Net::running(1, 1, HostOptions::default());
    let a = ids[0];
    n.lose(a);
    n.host_call(|h| h.start_resync("manual"));
    let a2 = n.reconnect(a);
    assert!(n.peer(a2).welcomes[0].catch_up.is_none());
    assert!(n.logged("cannot be caught up"), "{:?}", n.host_log);
}

#[test]
fn a_launcher_that_kept_nothing_or_a_grace_of_zero_reloads_as_before() {
    // a fresh session with only the ticket (an older launcher): no `last` in its claim
    let (mut n, ids) = Net::running(1, 1, HostOptions::default());
    let a = ids[0];
    let ticket = n.ticket(a);
    n.lose(a);
    let a2 = n.connect_resuming("P0", ticket);
    assert!(n.peer(a2).welcomes[0].resumed && n.peer(a2).welcomes[0].catch_up.is_none());
    assert!(n.logged("its launcher kept nothing"), "{:?}", n.host_log);

    let (mut n, ids) = Net::running(1, 1, HostOptions { resume_grace_seconds: 0.0, ..HostOptions::default() });
    let a = ids[0];
    n.lose(a);
    let a2 = n.reconnect(a);
    assert!(n.peer(a2).welcomes[0].catch_up.is_none());
    assert!(n.logged("catching up is off"), "{:?}", n.host_log);
}

#[test]
fn a_claim_whose_last_frame_was_changed_on_the_way_is_no_claim() {
    // `last` is covered by the mac: a different number fails the claim, which then is a plain join (a new id), never a skipped frame
    let (mut n, ids) = Net::running(1, 1, HostOptions::default());
    let a = ids[0];
    let a_id = n.id(a);
    n.lose(a);
    let c = n.open_raw();
    let challenge: ChallengeMessage = codec::try_decode(&n.raw[&c][0].body).unwrap();
    let ticket = n.ticket(a);
    let proof = HostSession::proof(&n.key, &challenge.nonce, "P0").to_vec();
    let mut join = JoinMessage::new("P0", "0.1", proof);
    join.resume = Some(ResumeClaim {
        id: a_id,
        mac: HostSession::resume_proof(&ticket.token, &challenge.nonce, a_id, "P0", Some(5)).to_vec(),
        last: Some(500),
    });
    let header = feverlink3_core::protocol::FrameHeader::new(1, MessageKind::Join.into(), HostSession::UNASSIGNED, 0);
    n.inject(c, &feverlink3_core::protocol::Frame::new(header, codec::encode(&join)));
    assert!(n.logged("no slot"), "{:?}", n.host_log);
}

#[test]
fn only_what_came_after_the_last_frame_it_had_is_replayed() {
    // what it had (and acknowledged with its pongs) is not sent again
    let (mut n, ids) = Net::running(2, 1, HostOptions::default());
    let (a, b) = (ids[0], ids[1]);
    for seq in 1..=20 {
        n.send(b, MessageKind::Action, &action(20_000 + seq * 200, seq, "old"));
    }
    n.run_for(4.0, 0.5);
    n.cut(a);
    n.send(b, MessageKind::Action, &action(30_000, 21, "new"));
    n.lose(a);
    let a2 = n.reconnect(a);
    assert_eq!(vec!["new"], bodies(&n.peer(a2).game));
}
