//! A player whose connection drops and returns within the window gets its id, name, slot and permissions back.

mod net;

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::protocol::Frame;
use feverlink3_core::session::*;
use net::*;

fn state_of(n: &Net, id: u16) -> Option<ConnectionState> {
    n.host.roster().iter().find(|e| e.id == id).map(|e| e.state)
}

fn chat() -> Vec<u8> {
    json(&ChatMessage::new("hi"))
}

/// The frame a player's game message turns into, delivered as usual.
fn send_capturing(n: &mut Net, c: ConnectionId, kind: MessageKind, payload: &[u8]) -> Frame {
    let outs = n.peers.get_mut(&c).unwrap().session.local_game_message(kind, payload);
    let frame = outs
        .iter()
        .find_map(|o| match o {
            PlayerOutput::SendToHost(f) => Some(f.clone()),
            _ => None,
        })
        .expect("a frame to the host");
    n.pump_player(c, outs);
    frame
}

// ------------------------------------------------------------------ the basics

#[test]
fn a_dropped_player_comes_back_as_itself() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    let (a_id, b_id) = (n.id(a), n.id(b));
    let ticket = n.ticket(a);
    assert_eq!(a_id, ticket.id);

    n.lose(a);
    let left: PlayerLeftMessage = n.peer(b).last(MessageKind::PlayerLeft);
    assert_eq!((a_id, "Alice", true), (left.id, left.name.as_str(), left.away), "the others learn it is away");
    assert_eq!(Some(ConnectionState::Away), state_of(&n, a_id));
    assert_eq!(2, n.host.players().len(), "not connected: not a player of the session meanwhile");

    let a2 = n.connect_resuming("Alice", ticket);
    assert_eq!(a_id, n.id(a2), "the same id");
    assert_eq!(Some("Alice"), n.peer(a2).session.assigned_name(), "the same name");
    assert!(n.peer(a2).welcomes[0].resumed);
    assert_eq!(Some(ConnectionState::Connected), state_of(&n, a_id));
    assert_eq!(3, n.host.players().len());
    assert_eq!(3, n.host.roster().len(), "one slot, not two");
    let joined: PlayerJoinedMessage = n.peer(b).last(MessageKind::PlayerJoined);
    assert_eq!((a_id, "Alice"), (joined.player.id, joined.player.name.as_str()));

    // the id was only held: a stranger gets the next one
    let c = n.connect("Carl");
    assert_eq!(b_id + 1, n.id(c));
}

#[test]
fn the_slot_comes_back_with_its_permissions() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    let perms = Permissions { build: PlayerSet::Only(vec![n.id(a)]), ..Permissions::default() };
    n.host_call(|h| h.set_permissions(perms.clone()));
    let ticket = n.ticket(a);
    n.lose(a);
    assert_eq!(&perms, n.host.permissions(), "the host does not forget them while the player is away");

    let a2 = n.connect_resuming("Alice", ticket);
    assert_eq!(&perms, n.peer(a2).session.permissions(), "and tells the returner");
    let build = |category: &str| json(&ActionMessage::new(20_000, 1, "{}").with_category(category));
    n.send(a2, MessageKind::Action, &build("build"));
    assert!(n.peer(b).last::<ActionMessage>(MessageKind::Action).allowed, "Alice may still build");
    n.send(b, MessageKind::Action, &build("build"));
    assert!(!n.peer(a2).last::<ActionMessage>(MessageKind::Action).allowed, "and Bob may not");
}

#[test]
fn the_returner_is_given_the_session_and_starts_not_ready() {
    let (mut n, ids) = Net::running(2, 3, HostOptions::default());
    let a = ids[0];
    let (a_id, ticket) = (n.id(a), n.ticket(a));
    n.lose(a);
    let a2 = n.connect_resuming("P0", ticket);
    let w = &n.peer(a2).welcomes[0];
    assert!(w.session.started);
    assert_eq!(3, w.session.speed);
    assert!(w.players.iter().any(|p| p.id == a_id), "it is in its own welcome");
    assert!(!n.host.players()[&a_id].ready, "it says hello again after it has loaded the host's world");
    assert!(!n.host.clocks().contains_key(&a_id));
    n.hello(a2, 10_000, 0);
    assert!(n.host.players()[&a_id].ready);
}

// ------------------------------------------------------------------ the secret

#[test]
fn the_token_is_in_the_welcome_but_never_reaches_the_game() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let w = &n.peer(a).welcomes[0];
    let grant = w.resume.clone().expect("a grant in the first welcome");
    assert_eq!(16, grant.token.len());
    assert_eq!(0, grant.epoch);
    assert!(!w.resumed);

    let (_, payload) = n.peer(a).game.iter().find(|g| g.0 == MessageKind::Welcome).expect("the game gets a welcome");
    let seen: serde_json::Value = serde_json::from_slice(payload).unwrap();
    assert!(seen.get("resume").is_none(), "{seen}");
    assert_eq!("Host", seen["hostName"], "everything else is passed on");
    assert_eq!(2, seen["players"].as_array().map_or(0, Vec::len), "the host and Alice");

    // nor does it show in a debug print
    let printed = format!("{grant:?} {:?} {:?}", n.ticket(a), n.peer(a).welcomes[0]);
    assert!(!printed.contains(&format!("{:?}", grant.token)), "{printed}");
    assert!(printed.contains("epoch"));
}

#[test]
fn a_wrong_token_is_a_plain_join_and_leaves_the_real_slot_alone() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let a_id = n.id(a);
    let real = n.ticket(a);
    n.lose(a);

    let forged = ResumeTicket { id: a_id, token: vec![7; 16] };
    let x = n.connect_resuming("Alice", forged);
    let w = &n.peer(x).welcomes[0];
    assert!(!w.resumed);
    assert_ne!(a_id, n.id(x), "a new id");
    let name = n.peer(x).session.assigned_name().unwrap();
    assert!(name.starts_with("Alice#"), "the away player's name is still taken: {name}");
    assert!(n.logged("no slot"), "{:?}", n.host_log);
    assert_eq!(Some(ConnectionState::Away), state_of(&n, a_id), "the real Alice's slot is untouched");

    let a2 = n.connect_resuming("Alice", real);
    assert!(n.peer(a2).welcomes[0].resumed);
    assert_eq!(a_id, n.id(a2));
}

#[test]
fn a_claim_for_a_slot_the_host_does_not_have_is_a_plain_join() {
    let mut n = Net::new();
    let x = n.connect_resuming("Alice", ResumeTicket { id: 99, token: vec![1; 16] });
    assert!(!n.peer(x).welcomes[0].resumed);
    assert_eq!(1, n.id(x), "ids are the host's to give");
}

#[test]
fn a_claim_cannot_take_the_slot_of_a_player_who_is_connected_without_its_token() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    let (a_id, b_id) = (n.id(a), n.id(b));
    // Bob holds the session key, so he can join; he does not hold Alice's token
    let stolen = ResumeTicket { id: a_id, token: n.ticket(b).token };
    let x = n.connect_resuming("Mallory", stolen);
    assert!(!n.peer(x).welcomes[0].resumed);
    assert!(n.host.players().contains_key(&a_id) && n.host.players().contains_key(&b_id));
    assert_eq!(PlayerState::Joined, n.peer(a).session.state(), "Alice's connection was not touched");
    assert!(!n.closed.contains(&a));
}

#[test]
fn a_claim_belongs_to_one_connection() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let a_id = n.id(a);
    let ticket = n.ticket(a);
    n.lose(a);

    // what Alice's launcher sends in answer to one connection's challenge
    let c1 = n.open_raw();
    let challenge = n.raw[&c1][0].clone();
    let mut alice = PlayerSession::resuming(n.key.clone(), "Alice", "0.1", ticket);
    let join = alice
        .frame_received(&challenge)
        .into_iter()
        .find_map(|o| match o {
            PlayerOutput::SendToHost(f) => Some(f),
            _ => None,
        })
        .expect("a join");

    // somebody who captured it plays it on another connection: the nonce is another one, the proof fails
    let c2 = n.open_raw();
    n.inject(c2, &join);
    let reply = n.raw[&c2].last().unwrap();
    assert_eq!(u8::from(MessageKind::Reject), reply.header.kind);
    assert_eq!("bad-proof", codec::try_decode::<RejectMessage>(&reply.body).unwrap().reason);
    assert_eq!(Some(ConnectionState::Away), state_of(&n, a_id), "the slot is still waiting for the real Alice");

    n.inject(c1, &join);
    assert_eq!(Some(ConnectionState::Connected), state_of(&n, a_id), "on its own connection it is good");
}

#[test]
fn a_claim_with_the_wrong_session_key_is_rejected_before_the_token_is_looked_at() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let ticket = n.ticket(a);
    n.lose(a);
    let other = SessionKey::derive(&[9; 16], None);
    let x = n.attach(PlayerSession::resuming(other, "Alice", "0.1", ticket));
    assert_eq!(Some("bad-proof"), n.peer(x).rejected.as_ref().map(|r| r.reason.as_str()));
}

// ------------------------------------------------------------------ frame counters

#[test]
fn frame_counters_continue_above_the_old_connections_and_old_frames_are_refused() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    let ticket = n.ticket(a);
    let old = send_capturing(&mut n, a, MessageKind::Chat, &chat());
    let old_last = send_capturing(&mut n, a, MessageKind::Chat, &chat());
    assert_eq!((1, 2), (old.header.counter, old_last.header.counter));
    n.lose(a);

    let a2 = n.connect_resuming("Alice", ticket);
    let first = send_capturing(&mut n, a2, MessageKind::Chat, &chat());
    assert_eq!((1u64 << 40) + 1, first.header.counter, "epoch 1 starts above everything epoch 0 can reach");
    assert_eq!(3, n.peer(b).count(MessageKind::Chat), "the host accepted it");

    // a capture of the old connection's traffic is of no use: older than the replay window
    let dropped = n.host.dropped();
    n.inject(a2, &old_last);
    assert_eq!(dropped + 1, n.host.dropped());
    assert_eq!(3, n.peer(b).count(MessageKind::Chat));
}

#[test]
fn every_return_uses_a_later_epoch() {
    let mut n = Net::new();
    let mut c = n.connect("Alice");
    let id = n.id(c);
    let mut counters = Vec::new();
    for epoch in 0..4u64 {
        assert_eq!(epoch as u32, n.peer(c).welcomes[0].resume.as_ref().unwrap().epoch);
        counters.push(send_capturing(&mut n, c, MessageKind::Chat, &chat()).header.counter);
        let ticket = n.ticket(c);
        n.lose(c);
        c = n.connect_resuming("Alice", ticket);
        assert_eq!(id, n.id(c));
    }
    let expected: Vec<u64> = (0..4u64).map(|e| (e << 40) + 1).collect();
    assert_eq!(expected, counters, "no (sender, counter) nonce is ever used twice");
}

// ------------------------------------------------------------------ the window

#[test]
fn the_slot_is_held_for_the_window_and_then_released() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let _b = n.connect("Bob");
    let (a_id, ticket) = (n.id(a), n.ticket(a));
    n.lose(a);
    n.run_for(329.0, 1.0);
    assert_eq!(Some(ConnectionState::Away), state_of(&n, a_id));
    n.run_for(2.0, 1.0);
    assert_eq!(None, state_of(&n, a_id), "gone from the roster");
    assert!(n.logged("did not come back"), "{:?}", n.host_log);

    let a2 = n.connect_resuming("Alice", ticket);
    assert!(!n.peer(a2).welcomes[0].resumed, "too late: a new player");
    assert_ne!(a_id, n.id(a2));
    assert_eq!(Some("Alice"), n.peer(a2).session.assigned_name(), "and the name is free again");
}

#[test]
fn a_held_slot_counts_toward_the_limit_until_it_expires() {
    let mut n = Net::with(HostOptions { max_players: 3, ..HostOptions::default() });
    let a = n.connect("Alice");
    let _b = n.connect("Bob");
    let ticket = n.ticket(a);
    n.lose(a);

    let c = n.connect("Carl");
    let r = n.peer(c).rejected.clone().expect("the session is full while Alice's slot is held");
    assert_eq!("full", r.reason);
    let detail = r.detail.unwrap();
    assert!(detail.starts_with("3 players") && detail.contains("1 of them away"), "{detail}");

    let a2 = n.connect_resuming("Alice", ticket);
    assert!(n.peer(a2).welcomes[0].resumed, "Alice needs no room: hers was kept");

    // and once the window is over the room is real
    n.lose(a2);
    n.run_for(331.0, 1.0);
    let d = n.connect("Dora");
    assert!(n.peer(d).rejected.is_none() && n.peer(d).session.id().is_some());
}

#[test]
fn a_window_of_zero_holds_nothing() {
    let mut n = Net::with(HostOptions { reconnect_window_seconds: 0.0, ..HostOptions::default() });
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    let (a_id, ticket) = (n.id(a), n.ticket(a));
    n.lose(a);
    let left: PlayerLeftMessage = n.peer(b).last(MessageKind::PlayerLeft);
    assert!(!left.away);
    assert_eq!(None, state_of(&n, a_id));
    let a2 = n.connect_resuming("Alice", ticket);
    assert!(!n.peer(a2).welcomes[0].resumed);
}

// ------------------------------------------------------------------ the old connection

#[test]
fn a_return_takes_over_a_connection_the_host_has_not_noticed_is_dead() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    let (a_id, ticket) = (n.id(a), n.ticket(a));
    n.cut(a); // the cable is pulled: the host still believes in the connection

    let a2 = n.connect_resuming("Alice", ticket);
    assert!(n.peer(a2).welcomes[0].resumed);
    assert_eq!(a_id, n.id(a2));
    assert!(n.closed.contains(&a), "the old connection is closed by the host");
    let lefts: Vec<PlayerLeftMessage> = n.peer(b).all(MessageKind::PlayerLeft);
    assert_eq!((a_id, true), (lefts[0].id, lefts[0].away), "the others see it leave and join again");
    assert_eq!(a_id, n.peer(b).last::<PlayerJoinedMessage>(MessageKind::PlayerJoined).player.id);

    // the transport reports the old connection closed, late: nothing changes
    n.host_call(|h| h.connection_closed(a));
    assert!(n.host.players().contains_key(&a_id));
    assert_eq!(PlayerState::Joined, n.peer(a2).session.state());
    // and anything still arriving over it is ignored
    let before = n.peer(b).count(MessageKind::Chat);
    let stale = send_capturing(&mut n, a, MessageKind::Chat, &chat());
    n.inject(a, &stale);
    assert_eq!(before, n.peer(b).count(MessageKind::Chat));
}

// ------------------------------------------------------------------ the hot-join round

#[test]
fn in_a_started_session_the_return_starts_a_hot_join_round_that_brings_it_back_into_play() {
    let (mut n, ids) = Net::running(2, 2, HostOptions::default());
    let (a, other) = (ids[0], ids[1]);
    let (a_id, ticket) = (n.id(a), n.ticket(a));
    n.lose(a);
    assert_eq!(ResyncState::Idle, n.host.resync_state(), "dropping starts nothing; the session goes on without it");

    let a2 = n.connect_resuming("P0", ticket);
    assert_eq!(ResyncState::Pausing, n.host.resync_state());
    assert!(n.logged("resync 1 started: player returned"), "{:?}", n.host_log);

    let pause_at = n.host.state().pause_at.expect("paused");
    n.stop_all_at(pause_at);
    assert_eq!(ResyncState::Saving, n.host.resync_state(), "the returner, not ready, is not waited for");
    let id = n.host.resync_id().unwrap();
    n.host_send(MessageKind::SaveDone, &json(&SaveDoneMessage { id, ok: true, detail: None }));
    assert_eq!(ResyncState::Transferring, n.host.resync_state());

    let save = blob(300_000);
    let sha = sha256_hex(&save);
    n.host_call(|h| h.resync_file_ready(id, &sha, save.len() as u64));
    for (i, chunk) in save.chunks(ResyncLimits::CHUNK_SIZE).enumerate() {
        n.host_call(|h| h.resync_chunk(id, i as u32, chunk));
    }
    assert_eq!(ResyncState::Loading, n.host.resync_state());
    assert_eq!(vec![(id, sha.clone(), save.clone())], n.peer(a2).saves, "the returner gets the host's world");
    assert_eq!(1, n.peer(a2).count(MessageKind::ResyncLoad));
    assert!(n.peer(a).saves.is_empty(), "the dead connection gets nothing");

    n.hello_all(pause_at, 0);
    assert_eq!(ResyncState::Verifying, n.host.resync_state());
    let req: ChecksumRequestMessage = n.host_game_all(MessageKind::ChecksumRequest).pop().unwrap();
    let report = json(&ChecksumReportMessage::new(req.n, req.at, parts(&[("roads", "r")])));
    n.host_send(MessageKind::ChecksumReport, &report);
    n.send(other, MessageKind::ChecksumReport, &report);
    n.send(a2, MessageKind::ChecksumReport, &report);
    assert_eq!(ResyncState::Idle, n.host.resync_state());
    assert_eq!(2, n.host.state().speed, "the session runs again at the speed it had");
    assert!(n.host.players()[&a_id].ready);
}

#[test]
fn before_the_start_a_return_starts_no_round() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let ticket = n.ticket(a);
    n.lose(a);
    let _a2 = n.connect_resuming("Alice", ticket);
    assert_eq!(ResyncState::Idle, n.host.resync_state());
}

#[test]
fn a_return_during_a_round_waits_for_the_next_one_when_the_files_are_moving() {
    let (mut n, ids) = Net::running(2, 1, HostOptions::default());
    let ticket = n.ticket(ids[0]);
    n.host_call(|h| h.start_resync("manual"));
    let pause_at = n.host.state().pause_at.unwrap();
    n.stop_all_at(pause_at);
    let id = n.host.resync_id().unwrap();
    n.host_send(MessageKind::SaveDone, &json(&SaveDoneMessage { id, ok: true, detail: None }));
    let save = blob(1000);
    let sha = sha256_hex(&save);
    n.host_call(|h| h.resync_file_ready(id, &sha, 1000));
    n.host_call(|h| h.resync_chunk(id, 0, &save));

    n.lose(ids[0]);
    let a2 = n.connect_resuming("P0", ticket);
    assert!(n.peer(a2).saves.is_empty(), "it missed this round's save");
    assert_eq!(Some(id), n.host.resync_id());
    assert!(n.logged("player returned waits for the next resync round"), "{:?}", n.host_log);
}

// ------------------------------------------------------------------ leaving on purpose, and after a restart

#[test]
fn leaving_on_purpose_frees_the_slot_at_once() {
    let mut n = Net::with(HostOptions { max_players: 3, ..HostOptions::default() });
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    let (a_id, ticket) = (n.id(a), n.ticket(a));

    n.leave(a);
    assert!(n.closed.contains(&a), "the host closes the connection");
    let left: PlayerLeftMessage = n.peer(b).last(MessageKind::PlayerLeft);
    assert_eq!((a_id, false), (left.id, left.away), "gone, not away");
    assert_eq!(None, state_of(&n, a_id));
    assert!(n.logged("Alice leaves the session"));

    let c = n.connect("Carl");
    assert!(n.peer(c).rejected.is_none(), "the room is free at once");
    n.leave(c);
    let a2 = n.connect_resuming("Alice", ticket);
    assert!(!n.peer(a2).welcomes[0].resumed, "a player who left has no slot to return to");
}

#[test]
fn ids_are_never_given_twice() {
    let mut n = Net::new();
    let a = n.connect("A");
    let b = n.connect("B");
    n.leave(b);
    let c = n.connect("C");
    assert_eq!(
        vec![1, 2, 3],
        vec![n.id(a), n.id(b), n.id(c)],
        "B's id 2 stays unused: entity keys and nonces depend on it"
    );
    n.lose(a);
    n.run_for(400.0, 1.0);
    let d = n.connect("D");
    assert_eq!(4, n.id(d));
}

#[test]
fn after_a_host_restart_the_old_ticket_gives_a_new_player_and_never_somebody_elses_slot() {
    let mut old = Net::new();
    let a = old.connect("Alice");
    let ticket = old.ticket(a);
    assert_eq!(1, ticket.id);

    // the same code, a new host: Bob got id 1 there
    let mut fresh = Net::new();
    let b = fresh.connect("Bob");
    assert_eq!(1, fresh.id(b));
    let a2 = fresh.connect_resuming("Alice", ticket);
    assert!(!fresh.peer(a2).welcomes[0].resumed);
    assert_eq!(2, fresh.id(a2));
    assert_eq!(PlayerState::Joined, fresh.peer(b).session.state());
    assert!(fresh.host.players().contains_key(&1) && fresh.host.players().contains_key(&2));
}
