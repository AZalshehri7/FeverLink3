//! Up to eight players: capacity, ids and fan-out; removing a player; the roster.

mod net;

use feverlink3_core::protocol::{Frame, FrameHeader};
use feverlink3_core::session::*;
use net::*;

fn state_of(n: &Net, id: u16) -> Option<ConnectionState> {
    n.host.roster().iter().find(|e| e.id == id).map(|e| e.state)
}

fn seven(n: &mut Net) -> Vec<ConnectionId> {
    (1..=7).map(|i| n.connect(&format!("P{i}"))).collect()
}

// ------------------------------------------------------------------ capacity and ids

#[test]
fn eight_games_fit_the_host_and_seven_players_and_the_ninth_is_refused() {
    let mut n = Net::new();
    let cs = seven(&mut n);
    let ids: Vec<u16> = cs.iter().map(|&c| n.id(c)).collect();
    assert_eq!((1..=7).collect::<Vec<u16>>(), ids, "ids are handed out one by one from 1");
    assert!(ids.iter().all(|&i| i != HostSession::HOST_ID && i != HostSession::UNASSIGNED));
    assert_eq!(8, n.host.players().len(), "the host's game counts");

    let ninth = n.connect("P8");
    let r = n.peer(ninth).rejected.clone().expect("refused");
    assert_eq!(("full", Some("8 players")), (r.reason.as_str(), r.detail.as_deref()));
    assert!(cs.iter().all(|&c| n.peer(c).rejected.is_none() && n.peer(c).session.state() == PlayerState::Joined));
    assert_eq!(8, n.host.players().len());

    // everyone learned of everyone who came after it, and the last welcome lists all eight
    for (i, &c) in cs.iter().enumerate() {
        assert_eq!(6 - i, n.peer(c).count(MessageKind::PlayerJoined), "player {}", i + 1);
    }
    assert_eq!(8, n.peer(cs[6]).welcomes[0].players.len());
    assert_eq!(8, n.host.roster().len());
}

#[test]
fn a_room_freed_by_a_leaver_goes_to_a_new_id_and_the_session_stays_at_eight() {
    let mut n = Net::new();
    let cs = seven(&mut n);
    n.leave(cs[2]); // id 3
    assert_eq!(7, n.host.players().len());
    let late = n.connect("Late");
    assert_eq!(8, n.id(late), "id 3 is not given again");
    assert_eq!(8, n.host.players().len());
    let one_too_many = n.connect("Extra");
    assert_eq!(Some("full"), n.peer(one_too_many).rejected.as_ref().map(|r| r.reason.as_str()));
}

#[test]
fn a_dropped_player_still_holds_one_of_the_eight_places() {
    let mut n = Net::new();
    let cs = seven(&mut n);
    let ticket = n.ticket(cs[4]);
    n.lose(cs[4]);
    assert_eq!(7, n.host.players().len());
    let stranger = n.connect("Stranger");
    assert_eq!(Some("full"), n.peer(stranger).rejected.as_ref().map(|r| r.reason.as_str()));
    let back = n.connect_resuming("P5", ticket);
    assert!(n.peer(back).welcomes[0].resumed);
    assert_eq!(8, n.host.players().len());
}

// ------------------------------------------------------------------ fan-out

#[test]
fn every_game_gets_every_action_once_in_the_order_the_host_received_them() {
    let mut n = Net::new();
    let cs = seven(&mut n);
    let ids: Vec<u16> = cs.iter().map(|&c| n.id(c)).collect();

    // an interleaving: round-robin, with one player sending twice as often
    let mut sent: Vec<(u16, i64)> = Vec::new();
    for round in 0..4i64 {
        for (i, &c) in cs.iter().enumerate() {
            for k in 0..(1 + usize::from(i == 2)) {
                let seq = round * 10 + k as i64;
                // claims to be somebody else: the host sets `from`
                let mut m = ActionMessage::new(30_000 + seq * 200, seq, "{x}");
                m.from = 99;
                n.send(c, MessageKind::Action, &json(&m));
                sent.push((ids[i], seq));
            }
        }
    }
    assert_eq!(sent.len() as u64, n.host.actions_relayed());

    for (i, &c) in cs.iter().enumerate() {
        let got: Vec<(u16, i64)> =
            n.peer(c).all::<ActionMessage>(MessageKind::Action).iter().map(|m| (m.from, m.seq)).collect();
        let want: Vec<(u16, i64)> = sent.iter().copied().filter(|(from, _)| *from != ids[i]).collect();
        assert_eq!(want, got, "player {} hears everyone else, in one order, and not itself", i + 1);
    }
    let at_host: Vec<(u16, i64)> =
        n.host_game_all::<ActionMessage>(MessageKind::Action).iter().map(|m| (m.from, m.seq)).collect();
    assert_eq!(sent, at_host, "the host's own game hears all of it");
}

#[test]
fn a_clock_goes_to_every_other_game_and_the_host_keeps_all_eight() {
    let mut n = Net::new();
    let cs = seven(&mut n);
    for t in 1..=5 {
        for &c in &cs {
            n.send(c, MessageKind::Clock, &json(&ClockMessage { t: t * 200, speed: 1, ahead: 3, need: None }));
        }
    }
    for (i, &c) in cs.iter().enumerate() {
        let from: Vec<u16> = n.peer(c).all::<PeerClockMessage>(MessageKind::PeerClock).iter().map(|m| m.from).collect();
        assert_eq!(6 * 5, from.len(), "player {}: 6 others, 5 clocks each", i + 1);
        assert!(!from.contains(&n.id(c)), "never its own clock back");
    }
    assert_eq!(7 * 5, n.host_game_count(MessageKind::PeerClock));
    assert_eq!(7, n.host.clocks().len());
}

#[test]
fn chat_reaches_the_other_seven_with_the_senders_name() {
    let mut n = Net::new();
    let cs = seven(&mut n);
    n.send(cs[3], MessageKind::Chat, &json(&ChatMessage::new("hello all")));
    for (i, &c) in cs.iter().enumerate() {
        let heard = n.peer(c).count(MessageKind::Chat);
        assert_eq!(usize::from(i != 3), heard, "player {}", i + 1);
    }
    let chat: ChatMessage = n.host_game_all(MessageKind::Chat).pop().unwrap();
    assert_eq!((Some("P4"), 4), (chat.name.as_deref(), chat.from));
}

#[test]
fn a_full_resync_round_with_seven_players_and_the_host() {
    let (mut n, ids) = Net::running(7, 2, HostOptions::default());
    assert_eq!(8, n.host.players().len());
    n.host_call(|h| h.start_resync("eight"));
    let pause_at = n.host.state().pause_at.expect("paused");
    n.stop_all_at(pause_at);
    assert_eq!(ResyncState::Saving, n.host.resync_state());
    let id = n.host.resync_id().unwrap();
    n.host_send(MessageKind::SaveDone, &json(&SaveDoneMessage { id, ok: true, detail: None }));

    let save = blob(2 * ResyncLimits::CHUNK_SIZE + 77);
    let sha = sha256_hex(&save);
    n.host_call(|h| h.resync_file_ready(id, &sha, save.len() as u64));
    for (i, chunk) in save.chunks(ResyncLimits::CHUNK_SIZE).enumerate() {
        n.host_call(|h| h.resync_chunk(id, i as u32, chunk));
    }
    assert_eq!(ResyncState::Loading, n.host.resync_state());
    for &c in &ids {
        assert_eq!(vec![(id, sha.clone(), save.clone())], n.peer(c).saves, "every player has the whole save");
        assert_eq!(1, n.peer(c).count(MessageKind::ResyncLoad));
    }

    n.hello_all(pause_at, 0);
    assert_eq!(ResyncState::Verifying, n.host.resync_state());
    let req: ChecksumRequestMessage = n.host_game_all(MessageKind::ChecksumRequest).pop().unwrap();
    let report = json(&ChecksumReportMessage::new(req.n, req.at, parts(&[("roads", "r"), ("lines", "l")])));
    n.host_send(MessageKind::ChecksumReport, &report);
    for &c in &ids {
        n.send(c, MessageKind::ChecksumReport, &report);
    }
    assert_eq!(ResyncState::Idle, n.host.resync_state());
    assert_eq!(2, n.host.state().speed);
}

// ------------------------------------------------------------------ removing a player

#[test]
fn kicking_a_player_removes_it_and_says_why() {
    let mut n = Net::with(HostOptions { max_players: 3, ..HostOptions::default() });
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    let (b_id, b_ticket) = (n.id(b), n.ticket(b));

    n.host_call(|h| h.kick(b_id));
    let r = n.peer(b).rejected.clone().expect("Bob is told");
    assert_eq!(("kicked", Some("removed by the host")), (r.reason.as_str(), r.detail.as_deref()));
    assert_eq!(PlayerState::Rejected, n.peer(b).session.state());
    assert!(n.closed.contains(&b));
    assert!(!n.host.players().contains_key(&b_id));
    assert_eq!(None, state_of(&n, b_id), "no slot is held for a player who was removed");
    let left: PlayerLeftMessage = n.peer(a).last(MessageKind::PlayerLeft);
    assert_eq!((b_id, false), (left.id, left.away));
    assert!(n.logged("Bob was removed by the host"), "{:?}", n.host_log);
    assert_eq!(PlayerState::Joined, n.peer(a).session.state(), "Alice is untouched");

    // what Bob's game says afterwards goes nowhere
    let before = n.peer(a).count(MessageKind::Chat);
    n.send(b, MessageKind::Chat, &json(&ChatMessage::new("still here?")));
    assert_eq!(before, n.peer(a).count(MessageKind::Chat));

    // the room is free for somebody else, and Bob's ticket is worth nothing
    let c = n.connect_resuming("Bob", b_ticket);
    assert!(n.peer(c).rejected.is_none());
    assert!(!n.peer(c).welcomes[0].resumed);
    assert_ne!(b_id, n.id(c));
}

#[test]
fn kicking_a_player_who_is_away_releases_its_slot() {
    let mut n = Net::with(HostOptions { max_players: 2, ..HostOptions::default() });
    let a = n.connect("Alice");
    let (a_id, ticket) = (n.id(a), n.ticket(a));
    n.lose(a);
    let c = n.connect("Carl");
    assert_eq!(Some("full"), n.peer(c).rejected.as_ref().map(|r| r.reason.as_str()), "held for Alice");

    n.host_call(|h| h.kick(a_id));
    assert_eq!(None, state_of(&n, a_id));
    assert!(n.logged("the slot of Alice was released"), "{:?}", n.host_log);
    let d = n.connect("Dora");
    assert!(n.peer(d).rejected.is_none(), "the room is free");
    n.leave(d);
    let a2 = n.connect_resuming("Alice", ticket);
    assert!(!n.peer(a2).welcomes[0].resumed, "and Alice's claim no longer holds");
}

#[test]
fn ending_the_session_tells_every_player_and_closes_every_connection() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    n.lose(b);
    let c = n.connect("Carl");
    let stranger = n.open_raw();

    n.host_call(|h| h.end_session());
    for p in [a, c] {
        let r = n.peer(p).rejected.clone().expect("told");
        assert_eq!(("ended", Some("the host ended the session")), (r.reason.as_str(), r.detail.as_deref()));
        assert_eq!(PlayerState::Rejected, n.peer(p).session.state());
        assert!(n.closed.contains(&p), "connection {p} is closed");
    }
    assert!(n.closed.contains(&stranger), "a connection that never joined is closed too");
    assert!(n.logged("the host ended the session"), "{:?}", n.host_log);
}

#[test]
fn the_hosts_own_game_cannot_be_removed_and_nobody_else_is_hurt_by_a_wrong_id() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    n.host_call(|h| h.kick(HostSession::HOST_ID));
    assert!(n.logged("the host cannot be removed"));
    assert!(n.host.players().contains_key(&HostSession::HOST_ID));
    n.host_call(|h| h.kick(42));
    assert!(n.logged("there is no player #42"));
    assert_eq!(PlayerState::Joined, n.peer(a).session.state());
    assert_eq!(2, n.host.players().len());
}

#[test]
fn kicking_a_player_during_a_round_does_not_stall_it() {
    let (mut n, ids) = Net::running(2, 1, HostOptions::default());
    n.host_call(|h| h.start_resync("test"));
    assert_eq!(ResyncState::Pausing, n.host.resync_state());
    let first = n.id(ids[0]);
    n.host_call(|h| h.kick(first));
    let pause_at = n.host.state().pause_at.unwrap();
    n.stop_all_at(pause_at);
    assert_eq!(ResyncState::Saving, n.host.resync_state(), "the round no longer waits for the removed game");
}

#[test]
fn players_cannot_send_what_only_the_host_may_say() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    let before = n.host.dropped();
    n.send(a, MessageKind::Kicked, &json(&RejectMessage::new("kicked", Some("by Alice"))));
    n.send(a, MessageKind::Ping, &json(&PingMessage { n: 1, rtt: Default::default() }));
    assert_eq!(before + 2, n.host.dropped());
    assert!(n.peer(b).rejected.is_none());
    assert_eq!(PlayerState::Joined, n.peer(b).session.state());
}

#[test]
fn an_unsealed_reject_after_the_welcome_is_ignored() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let forged = Frame::new(
        FrameHeader::new(1, MessageKind::Reject.into(), HostSession::UNASSIGNED, 0),
        json(&RejectMessage::new("kicked", Some("anybody on the path can write this"))),
    );
    let dropped = n.peer(a).session.dropped();
    n.peers.get_mut(&a).unwrap().session.frame_received(&forged);
    assert_eq!(PlayerState::Joined, n.peer(a).session.state());
    assert_eq!(dropped + 1, n.peer(a).session.dropped());

    // and an unsealed frame of the sealed kind that ends a session is no better
    let forged = Frame::new(
        FrameHeader::new(1, MessageKind::Kicked.into(), HostSession::HOST_ID, 1),
        json(&RejectMessage::new("kicked", None)),
    );
    n.peers.get_mut(&a).unwrap().session.frame_received(&forged);
    assert_eq!(PlayerState::Joined, n.peer(a).session.state());
}

// ------------------------------------------------------------------ the roster

#[test]
fn the_host_roster_has_everyone_with_role_state_and_round_trip() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    let (a_id, b_id) = (n.id(a), n.id(b));
    n.set_latency(a, 0.05);
    n.set_latency(b, 0.25);
    n.tick();
    n.hello(a, 0, 0);
    n.lose(b);

    let roster = n.host.roster();
    assert_eq!(vec![0, a_id, b_id], roster.iter().map(|e| e.id).collect::<Vec<_>>());
    let (host, alice, bob) = (&roster[0], &roster[1], &roster[2]);
    assert_eq!(
        ("host", "Host", ConnectionState::Connected, Some(0.0)),
        (host.role(), host.name.as_str(), host.state, host.rtt_ms)
    );
    assert_eq!(
        ("player", "Alice", ConnectionState::Connected, true),
        (alice.role(), alice.name.as_str(), alice.state, alice.ready)
    );
    assert!(alice.rtt_ms.is_some_and(|ms| (ms - 50.0).abs() < 0.5), "{:?}", alice.rtt_ms);
    assert_eq!(("Bob", ConnectionState::Away, None, false), (bob.name.as_str(), bob.state, bob.rtt_ms, bob.ready));
    assert_eq!("away", bob.state.as_str());
    assert_eq!("connected", alice.state.as_str());
}

#[test]
fn a_players_roster_follows_who_joins_leaves_and_drops() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let roster = |n: &Net| -> Vec<(u16, String, ConnectionState)> {
        n.peer(a).session.roster().into_iter().map(|e| (e.id, e.name, e.state)).collect()
    };
    assert_eq!(
        vec![(0, "Host".to_owned(), ConnectionState::Connected), (1, "Alice".to_owned(), ConnectionState::Connected)],
        roster(&n)
    );

    let b = n.connect("Bob");
    let b_ticket = n.ticket(b);
    assert_eq!((2, "Bob".to_owned(), ConnectionState::Connected), roster(&n)[2]);
    n.lose(b);
    assert_eq!((2, "Bob".to_owned(), ConnectionState::Away), roster(&n)[2], "away, not gone");
    assert_eq!(None, n.peer(a).session.roster()[2].rtt_ms);
    let b2 = n.connect_resuming("Bob", b_ticket);
    assert_eq!((2, "Bob".to_owned(), ConnectionState::Connected), roster(&n)[2]);
    n.leave(b2);
    assert_eq!(2, roster(&n).len(), "gone for good");

    let c = n.connect("Carl");
    let c_id = n.id(c);
    n.host_call(|h| h.kick(c_id));
    assert_eq!(2, roster(&n).len());
}
