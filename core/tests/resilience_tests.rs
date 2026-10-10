//! What a round, a stalled host and a replayed handshake do to the session layer: found in review, kept as tests.
//! A player that drops out of a resync round and returns under the same id; a checksum round whose last reporter
//! leaves; a host that was away and now reads a backlog; the others told when a held slot is released; a welcome
//! or host frame replayed from an earlier connection; a returner before the start.

mod net;

use feverlink3_core::protocol::Frame;
use feverlink3_core::session::*;
use net::*;

fn state_of(n: &Net, id: u16) -> Option<ConnectionState> {
    n.host.roster().iter().find(|e| e.id == id).map(|e| e.state)
}

/// Starts a resync round on a running session and drives it to the end of the transfer: Loading.
fn to_loading(n: &mut Net, save: &[u8]) -> (i32, i64) {
    n.host_call(|h| h.start_resync("test"));
    let pause_at = n.host.state().pause_at.expect("paused");
    n.stop_all_at(pause_at);
    let id = n.host.resync_id().unwrap();
    n.host_send(MessageKind::SaveDone, &json(&SaveDoneMessage { id, ok: true, detail: None }));
    send_file(n, id, save);
    assert_eq!(ResyncState::Loading, n.host.resync_state());
    (id, pause_at)
}

fn send_file(n: &mut Net, id: i32, save: &[u8]) {
    let sha = sha256_hex(save);
    n.host_call(|h| h.resync_file_ready(id, &sha, save.len() as u64));
    for (i, chunk) in save.chunks(ResyncLimits::CHUNK_SIZE).enumerate() {
        n.host_call(|h| h.resync_chunk(id, i as u32, chunk));
    }
}

fn report(n: &Net, parts_: &[(&str, &str)]) -> Vec<u8> {
    let req: ChecksumRequestMessage = n.host_game_all(MessageKind::ChecksumRequest).pop().unwrap();
    json(&ChecksumReportMessage::new(req.n, req.at, parts(parts_)))
}

// ------------------------------------------------------------------ a round and a player that drops out of it

#[test]
fn a_participant_that_drops_and_returns_during_loading_does_not_hold_the_round() {
    let (mut n, ids) = Net::running(2, 1, HostOptions::default());
    let (p0, p1) = (ids[0], ids[1]);
    let (p1_id, ticket) = (n.id(p1), n.ticket(p1));
    let (_, pause_at) = to_loading(&mut n, &blob(1000));

    n.lose(p1);
    let back = n.connect_resuming("P1", ticket);
    assert!(n.logged("player returned waits for the next resync round"), "{:?}", n.host_log);
    assert_eq!(0, n.frames_to(back, MessageKind::ResyncLoad), "it was not sent the load of a save it never got");

    // the others load and say hello: the round goes on without waiting for the returner's hello
    n.host_hello(pause_at, 0);
    n.hello(p0, pause_at, 0);
    assert_eq!(ResyncState::Verifying, n.host.resync_state(), "{:?}", n.host_log);

    // the returner's game still runs its old world: a report from it does not count, and cannot fail the round
    let stale = report(&n, &[("roads", "OLD WORLD")]);
    n.send(back, MessageKind::ChecksumReport, &stale);
    assert!(n.logged("checksum report from P1 ignored"), "{:?}", n.host_log);
    let good = report(&n, &[("roads", "r")]);
    n.host_send(MessageKind::ChecksumReport, &good);
    n.send(p0, MessageKind::ChecksumReport, &good);
    assert!(n.failures().is_empty(), "{:?}", n.failures());

    // and then the round the returner was promised starts, with the whole save for it
    assert_eq!(ResyncState::Pausing, n.host.resync_state());
    assert!(n.logged("player returned"), "{:?}", n.host_log);
    assert_eq!(Some(ConnectionState::Connected), state_of(&n, p1_id));
}

impl Net {
    fn failures(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                HostOutput::ResyncFailed { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn a_participant_that_returns_mid_transfer_gets_no_half_a_save_and_the_whole_one_next_round() {
    let (mut n, ids) = Net::running(2, 1, HostOptions::default());
    let (p0, p1) = (ids[0], ids[1]);
    let ticket = n.ticket(p1);
    n.host_call(|h| h.start_resync("test"));
    let pause_at = n.host.state().pause_at.unwrap();
    n.stop_all_at(pause_at);
    let id = n.host.resync_id().unwrap();
    n.host_send(MessageKind::SaveDone, &json(&SaveDoneMessage { id, ok: true, detail: None }));

    let save = blob(3 * ResyncLimits::CHUNK_SIZE - 100);
    let sha = sha256_hex(&save);
    n.host_call(|h| h.resync_file_ready(id, &sha, save.len() as u64));
    let chunks: Vec<&[u8]> = save.chunks(ResyncLimits::CHUNK_SIZE).collect();
    n.host_call(|h| h.resync_chunk(id, 0, chunks[0]));

    n.lose(p1);
    let back = n.connect_resuming("P1", ticket);
    n.host_call(|h| h.resync_chunk(id, 1, chunks[1]));
    n.host_call(|h| h.resync_chunk(id, 2, chunks[2]));
    assert_eq!(ResyncState::Loading, n.host.resync_state());
    assert_eq!(0, n.frames_to(back, MessageKind::ResyncChunk), "the end of a transfer it cannot assemble");
    assert_eq!(0, n.frames_to(back, MessageKind::ResyncLoad));
    assert!(n.peer(back).saves.is_empty());

    // the round ends without it ...
    n.host_hello(pause_at, 0);
    n.hello(p0, pause_at, 0);
    let good = report(&n, &[("roads", "r")]);
    n.host_send(MessageKind::ChecksumReport, &good);
    n.send(p0, MessageKind::ChecksumReport, &good);

    // ... and the next one brings the save to it, whole
    assert_eq!(ResyncState::Pausing, n.host.resync_state());
    let pause_at = n.host.state().pause_at.unwrap();
    n.stop_all_at(pause_at);
    let id2 = n.host.resync_id().unwrap();
    assert_ne!(id, id2);
    n.host_send(MessageKind::SaveDone, &json(&SaveDoneMessage { id: id2, ok: true, detail: None }));
    send_file(&mut n, id2, &save);
    assert_eq!(vec![(id2, sha, save)], n.peer(back).saves);
}

#[test]
fn a_checksum_round_waiting_for_a_game_that_leaves_completes_without_it() {
    let (mut n, ids) = Net::running(2, 1, HostOptions::default());
    let p1_id = n.id(ids[1]);
    let (_, pause_at) = to_loading(&mut n, &blob(1000));
    n.hello_all(pause_at, 0);
    assert_eq!(ResyncState::Verifying, n.host.resync_state());

    let good = report(&n, &[("roads", "r")]);
    n.host_send(MessageKind::ChecksumReport, &good);
    n.send(ids[0], MessageKind::ChecksumReport, &good);
    assert_eq!(ResyncState::Verifying, n.host.resync_state(), "P1 has not reported");
    n.host_call(|h| h.kick(p1_id));
    assert_eq!(ResyncState::Idle, n.host.resync_state(), "nobody is left to wait for");
    assert!(n.failures().is_empty());
    assert_eq!(1, n.host.state().speed, "the session runs again");
}

#[test]
fn and_when_the_games_that_are_left_differ_the_round_still_fails_as_it_should() {
    let (mut n, ids) = Net::running(2, 1, HostOptions::default());
    let p1_id = n.id(ids[1]);
    let (_, pause_at) = to_loading(&mut n, &blob(1000));
    n.hello_all(pause_at, 0);
    n.host_send(MessageKind::ChecksumReport, &report(&n, &[("roads", "host")]));
    n.send(ids[0], MessageKind::ChecksumReport, &report(&n, &[("roads", "other")]));
    n.timeout_p1(p1_id);
    assert_eq!(ResyncState::Idle, n.host.resync_state());
    assert!(n.failures().iter().any(|f| f.contains("still differ")), "{:?}", n.failures());
}

impl Net {
    /// The game the round waits for is cut off by the host.
    fn timeout_p1(&mut self, id: u16) {
        self.host_call(|h| h.kick(id));
    }
}

// ------------------------------------------------------------------ released slots are announced

#[test]
fn a_slot_that_expires_or_is_released_is_announced_so_nobody_keeps_a_ghost() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    let c = n.connect("Carl");
    let (a_id, b_id) = (n.id(a), n.id(b));
    n.lose(a);
    n.lose(b);
    let seen_by_carl = |n: &Net| n.peer(c).session.roster().iter().map(|e| (e.id, e.state)).collect::<Vec<_>>();
    assert!(seen_by_carl(&n).contains(&(a_id, ConnectionState::Away)));

    // Bob's slot is released by the host
    n.host_call(|h| h.kick(b_id));
    let left: PlayerLeftMessage = n.peer(c).last(MessageKind::PlayerLeft);
    assert_eq!((b_id, false), (left.id, left.away));
    assert!(!seen_by_carl(&n).iter().any(|(id, _)| *id == b_id));

    // Alice's runs out
    n.run_for(331.0, 1.0);
    let left: PlayerLeftMessage = n.peer(c).last(MessageKind::PlayerLeft);
    assert_eq!((a_id, false), (left.id, left.away));
    assert!(!seen_by_carl(&n).iter().any(|(id, _)| *id == a_id), "no ghost: {:?}", seen_by_carl(&n));
    assert_eq!(2, n.host.roster().len());
}

// ------------------------------------------------------------------ a host that was away

fn tight() -> HostOptions {
    HostOptions { max_frames_per_second: 50, frame_burst: 100, ..HostOptions::default() }
}

#[test]
fn a_backlog_read_after_the_host_was_away_is_not_a_flood() {
    let mut n = Net::with(tight());
    let a = n.connect("A");
    let b = n.connect("B");
    // the host's thread was stuck for a minute; the players' frames wait in the channel and are read in one go
    n.advance(60.0);
    let chat = json(&ChatMessage::new("clock"));
    for _ in 0..400 {
        n.send(a, MessageKind::Chat, &chat);
        n.send(b, MessageKind::Chat, &chat);
    }
    assert!(n.peer(a).rejected.is_none() && n.peer(b).rejected.is_none());
    assert_eq!(0, n.host.dropped());
    assert_eq!(400, n.peer(a).count(MessageKind::Chat));

    // once the host is back on its feet the limit holds again
    n.run_for(61.0, 1.0);
    for _ in 0..400 {
        n.send(a, MessageKind::Chat, &chat);
    }
    assert_eq!(Some("flood"), n.peer(a).rejected.as_ref().map(|r| r.reason.as_str()));
    assert!(n.peer(b).rejected.is_none());
}

#[test]
fn a_stall_does_not_cut_held_slots_short_or_make_an_old_ping_a_round_trip_time() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let b = n.connect("Bob");
    let (a_id, b_id) = (n.id(a), n.id(b));

    // a ping that Bob has not answered when the host is stalled
    n.cut(b);
    n.tick();
    let ping: Frame = n
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            HostOutput::SendToConnection { connection, frame }
                if *connection == b && frame.header.kind == u8::from(MessageKind::Ping) =>
            {
                Some(frame.clone())
            }
            _ => None,
        })
        .expect("a ping to Bob");
    let pong = n
        .peers
        .get_mut(&b)
        .unwrap()
        .session
        .frame_received(&ping)
        .into_iter()
        .find_map(|o| match o {
            PlayerOutput::SendToHost(f) => Some(f),
            _ => None,
        })
        .expect("Bob's pong");
    n.lose(a);
    n.advance(400.0); // the host was away for 400 s
    n.reachable.insert(b);
    n.inject(b, &pong);
    assert_eq!(None, n.host.rtt_ms(b_id), "the answer to a ping from before the stall measures nothing");
    assert!(n.host.players().contains_key(&b_id), "and Bob was not timed out for the host's own silence");

    // Alice's slot kept its whole window: the host could not have let her back in meanwhile
    n.tick();
    n.run_for(325.0, 1.0);
    assert_eq!(Some(ConnectionState::Away), state_of(&n, a_id));
    n.run_for(10.0, 1.0);
    assert_eq!(None, state_of(&n, a_id));
}

// ------------------------------------------------------------------ the handshake cannot be replayed

fn frames_to(n: &Net, c: ConnectionId, kind: MessageKind) -> Vec<Frame> {
    n.events
        .iter()
        .filter_map(|e| match e {
            HostOutput::SendToConnection { connection, frame }
                if *connection == c && frame.header.kind == u8::from(kind) =>
            {
                Some(frame.clone())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_welcome_or_host_frame_from_an_earlier_connection_is_refused() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    let old_welcome = frames_to(&n, a, MessageKind::Welcome).pop().expect("Alice's welcome");
    // a frame the host sealed long before Eve comes: permissions that let nobody build
    n.host_call(|h| h.set_permissions(Permissions { build: PlayerSet::HostOnly, ..Permissions::default() }));
    let old_permissions = frames_to(&n, a, MessageKind::Permissions).pop().expect("a permissions frame");
    n.host_call(|h| h.set_permissions(Permissions::default()));

    // Eve joins, and somebody on the path plays the old welcome before the real one
    let c = n.open_raw();
    let challenge = n.raw[&c][0].clone();
    let mut eve = PlayerSession::new(n.key.clone(), "Eve", "0.1");
    let join = eve
        .frame_received(&challenge)
        .into_iter()
        .find_map(|o| match o {
            PlayerOutput::SendToHost(f) => Some(f),
            _ => None,
        })
        .expect("a join");
    let outs = eve.frame_received(&old_welcome);
    assert!(outs.iter().any(|o| matches!(o, PlayerOutput::Log(l) if l.contains("welcome for another connection"))));
    assert_eq!(
        (PlayerState::Connecting, None),
        (eve.state(), eve.id()),
        "no id, no counters from a welcome that is not hers"
    );

    // a second challenge is not the host's either
    let outs = eve.frame_received(&challenge);
    assert!(outs.iter().any(|o| matches!(o, PlayerOutput::Log(l) if l.contains("second challenge"))));

    // the real welcome still works
    n.inject(c, &join);
    let welcome = n.raw[&c].iter().find(|f| f.header.kind == u8::from(MessageKind::Welcome)).cloned().unwrap();
    eve.frame_received(&welcome);
    assert_eq!(PlayerState::Joined, eve.state());
    assert_eq!(Some(2), eve.id());

    // and a frame the host sealed before this connection existed is a replay, whatever its window says
    let outs = eve.frame_received(&old_permissions);
    assert!(outs
        .iter()
        .any(|o| matches!(o, PlayerOutput::Log(l) if l.contains("sealed before this connection began"))));
    assert_eq!(&Permissions::default(), eve.permissions());
}

// ------------------------------------------------------------------ a returner before the start

#[test]
fn a_returner_before_the_start_is_as_ready_as_it_was_and_after_it_not() {
    let mut n = Net::new();
    let a = n.connect("Alice");
    n.hello(a, 0, 0);
    let a_id = n.id(a);
    let ready = |n: &Net| n.host.roster().iter().find(|e| e.id == a_id).map(|e| e.ready);
    assert_eq!(Some(true), ready(&n));
    let ticket = n.ticket(a);
    n.lose(a);
    assert_eq!(Some(false), ready(&n), "away is not ready");
    let _a2 = n.connect_resuming("Alice", ticket);
    assert_eq!(Some(true), ready(&n), "its game was not reloaded: nothing to say hello again for");

    let (mut n, ids) = Net::running(1, 1, HostOptions::default());
    let ticket = n.ticket(ids[0]);
    n.lose(ids[0]);
    let _back = n.connect_resuming("P0", ticket);
    assert_eq!(
        Some(false),
        n.host.roster().iter().find(|e| e.id == 1).map(|e| e.ready),
        "the hot-join round reloads it"
    );
}
