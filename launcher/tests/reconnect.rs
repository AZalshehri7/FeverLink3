//! Players that drop and come back, that stop on purpose, that are removed, that go quiet or flood: real launcher
//! sessions over loopback with fake games. A forwarder in front of the host lets a test cut or silence one player's
//! link without touching the others. No game is started.

mod multi_support;

use std::fs;
use std::io;
use std::net::TcpStream;
use std::time::Duration;

use feverlink3_core::protocol::{Frame, FrameCodec, FrameHeader, ProtocolError};
use feverlink3_core::session::{
    codec, ChatMessage, ConnectionState, HostOptions, MessageKind, Permissions, PlayerOutput, PlayerSession, PlayerSet,
    PlayerState, RejectMessage, ResyncLimits,
};
use feverlink3_launcher::session::{Command, SessionHandle};
use multi_support::*;

const WAIT: Duration = Duration::from_secs(20);

fn away(h: &SessionHandle, id: u16) -> bool {
    h.status().roster.iter().any(|e| e.id == id && e.state == ConnectionState::Away)
}

// ------------------------------------------------------------------ coming back

#[test]
fn a_dropped_player_comes_back_as_itself_with_its_permissions_and_is_brought_up_to_date() {
    let root = tempfile::TempDir::new().unwrap();
    let key = key();
    let save: Vec<u8> = (0..(ResyncLimits::CHUNK_SIZE + 1234)).map(|i| (i % 247) as u8).collect();
    let (host, mut host_game) = start_host(root.path(), &key, HostOptions::default(), save.clone());
    let addr = host.local_addr().unwrap();
    let proxy = Proxy::start(addr);

    let (alice, mut alice_game) = start_player(root.path(), &key, "Alice", proxy.addr);
    assert!(alice.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));
    let (bob, mut bob_game) = start_player(root.path(), &key, "Bob", addr);
    assert!(bob.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));
    let (a_id, b_id) = (alice.status().my_id.unwrap(), bob.status().my_id.unwrap());
    assert_eq!((1, 2), (a_id, b_id));

    // Alice may build; Bob may not
    host.command(Command::SetPermissions(Permissions { build: PlayerSet::Only(vec![a_id]), ..Permissions::default() }));
    host_game.announce();
    alice_game.announce();
    bob_game.announce();
    assert!(host.wait_for(WAIT, |s| s.clocks.len() == 3));
    host.command(Command::Start);
    {
        let mut games = [&mut host_game, &mut alice_game, &mut bob_game];
        assert!(wait_games(WAIT, &mut games, |gs| gs.iter().all(|g| g.saw("session", "[\"started\"]=true"))));
    }

    // her link is cut and stays down while the test looks at the outage: the host keeps her place and tells the others
    // she is away
    proxy.refuse(true);
    proxy.cut();
    assert!(wait(WAIT, || away(&host, a_id)), "{:?}", host.status().roster);
    assert!(wait(WAIT, || away(&bob, a_id)), "Bob is told too: {:?}", bob.status().roster);
    assert_eq!(2, host.status().players.len(), "away is not connected");
    assert!(alice.status().reconnecting, "her launcher is trying");

    // the link is back: her launcher tries again by itself and is the same player
    proxy.refuse(false);
    assert!(alice.wait_for(WAIT, |s| s.connected && !s.reconnecting && s.my_id == Some(a_id)), "{:?}", alice.status());
    assert!(alice.status().reconnect_attempts >= 1);

    // the host runs the round that brings her up to date, like for any player that joins a running session
    {
        let mut games = [&mut host_game, &mut alice_game, &mut bob_game];
        let done = wait_games(Duration::from_secs(40), &mut games, |_| host.status().resync.as_deref() == Some("Idle"));
        assert!(done, "host log:\n{}", host.logs().join("\n"));
    }
    assert!(host.logs().iter().any(|l| l.contains("player returned")), "{:?}", host.logs());
    assert_eq!(save, fs::read(root.path().join("Alice-saves").join("feverlink3-resync-1.sav")).unwrap());
    assert!(alice_game.saw("resync_load", "feverlink3-resync-1"));
    let roster = host.status().roster;
    assert_eq!(vec![0, a_id, b_id], roster.iter().map(|e| e.id).collect::<Vec<_>>());
    assert!(roster.iter().all(|e| e.state == ConnectionState::Connected));
    assert_eq!(a_id, alice.status().my_id.unwrap());

    // what her game saw: the outage, then a welcome that says she is back, without the secret
    assert!(alice_game.saw("session", "[\"started\"]=false"), "told while it had no host");
    let welcomes = alice_game.lines("welcome");
    assert_eq!(2, welcomes.len());
    assert!(welcomes[1].payload.contains("[\"resumed\"]=true"), "{}", welcomes[1].payload);
    assert!(welcomes.iter().all(|w| !w.payload.contains("[\"resume\"]")), "the token stays in the launcher");
    // permissions are what they were
    let last = |g: &Game| g.lines("permissions").last().map(|l| l.payload.clone()).unwrap_or_default();
    assert!(last(&alice_game).contains("[\"build\"]=true"), "{}", last(&alice_game));
    assert!(last(&bob_game).contains("[\"build\"]=false"), "{}", last(&bob_game));
    // Bob saw her go and come back under the same id
    let id_text = format!("[\"id\"]={a_id}");
    assert!(bob_game
        .lines("player_left")
        .iter()
        .any(|l| l.payload.contains(&id_text) && l.payload.contains("[\"away\"]=true")));
    assert!(bob_game.lines("player_joined").iter().any(|l| l.payload.contains(&id_text)));

    for h in [&alice, &bob, &host] {
        h.stop();
    }
}

#[test]
fn a_link_that_goes_silent_is_cut_off_by_the_host_and_the_player_returns_to_its_slot() {
    let root = tempfile::TempDir::new().unwrap();
    let key = key();
    let options = HostOptions { ping_seconds: 0.25, idle_timeout_seconds: 2.0, ..HostOptions::default() };
    let (host, _host_game) = start_host(root.path(), &key, options, Vec::new());
    let proxy = Proxy::start(host.local_addr().unwrap());
    let (alice, _alice_game) = start_player_with(root.path(), &key, "Alice", proxy.addr, Duration::from_secs(1));
    assert!(alice.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));
    let a_id = alice.status().my_id.unwrap();
    assert!(wait(WAIT, || host.status().roster.iter().any(|e| e.id == a_id && e.rtt_ms.is_some())), "pings work");

    // nothing gets through, and nobody closes anything: the host has to notice by itself
    proxy.blackhole(true);
    assert!(wait(Duration::from_secs(15), || away(&host, a_id)), "{:?}", host.status().roster);
    assert!(host.logs().iter().any(|l| l.contains("Alice timed out")), "{:?}", host.logs());
    proxy.blackhole(false);

    assert!(alice.wait_for(WAIT, |s| s.connected && !s.reconnecting && s.my_id == Some(a_id)), "{:?}", alice.status());
    assert!(wait(WAIT, || host.status().roster.iter().all(|e| e.state == ConnectionState::Connected)));
    assert_eq!(2, host.status().players.len());
    alice.stop();
    host.stop();
}

#[test]
fn a_host_that_accepts_and_resets_does_not_keep_a_player_trying_beyond_its_window() {
    let root = tempfile::TempDir::new().unwrap();
    let key = key();
    let (host, _host_game) = start_host(root.path(), &key, HostOptions::default(), Vec::new());
    let proxy = Proxy::start(host.local_addr().unwrap());
    let (alice, _a) = start_player_window(
        root.path(),
        &key,
        "Alice",
        proxy.addr,
        Duration::from_millis(100),
        Duration::from_millis(1500),
    );
    assert!(alice.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));

    // every new connection is accepted and closed at once: each attempt "connects", none is welcomed
    proxy.refuse(true);
    proxy.cut();
    assert!(alice.wait_for(Duration::from_secs(10), |s| s.finished), "{:?}", alice.status());
    let error = alice.status().error.unwrap_or_default();
    assert!(error.contains("could not reconnect within 1 s"), "{error}");
    host.stop();
}

// ------------------------------------------------------------------ leaving and being removed

#[test]
fn a_player_that_stops_says_goodbye_and_is_gone_not_away() {
    let root = tempfile::TempDir::new().unwrap();
    let key = key();
    let options = HostOptions { max_players: 3, ..HostOptions::default() };
    let (host, _host_game) = start_host(root.path(), &key, options, Vec::new());
    let addr = host.local_addr().unwrap();
    let (alice, _a) = start_player(root.path(), &key, "Alice", addr);
    let (bob, _b) = start_player(root.path(), &key, "Bob", addr);
    assert!(alice.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));
    assert!(bob.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));
    assert!(host.wait_for(WAIT, |s| s.players.len() == 3));

    let a_id = alice.status().my_id.unwrap();
    alice.stop();
    assert!(
        host.wait_for(WAIT, |s| s.players.len() == 2 && s.roster.iter().all(|e| e.id != a_id)),
        "{:?}",
        host.status().roster
    );
    assert!(host.logs().iter().any(|l| l.contains("Alice leaves the session")), "{:?}", host.logs());
    assert!(bob.wait_for(WAIT, |s| s.roster.iter().all(|e| e.id != a_id)), "Bob is told she is gone, not away");

    // the room is free at once
    let (carl, _c) = start_player(root.path(), &key, "Carl", addr);
    assert!(carl.wait_for(WAIT, |s| s.connected && s.my_id.is_some()), "{:?}", carl.status());
    for h in [&carl, &bob, &host] {
        h.stop();
    }
}

#[test]
fn a_dropped_player_holds_its_place_until_the_host_releases_it() {
    let root = tempfile::TempDir::new().unwrap();
    let key = key();
    let options = HostOptions { max_players: 3, ..HostOptions::default() };
    let (host, _host_game) = start_host(root.path(), &key, options, Vec::new());
    let addr = host.local_addr().unwrap();
    let proxy = Proxy::start(addr);
    // Alice will not try again for a while, so nothing but the host decides who gets her place
    let (alice, _a) = start_player_with(root.path(), &key, "Alice", proxy.addr, Duration::from_secs(30));
    let (bob, _b) = start_player(root.path(), &key, "Bob", addr);
    assert!(alice.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));
    assert!(bob.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));
    let a_id = alice.status().my_id.unwrap();

    proxy.cut();
    assert!(wait(WAIT, || away(&host, a_id)));
    let (carl, _c) = start_player(root.path(), &key, "Carl", addr);
    assert!(carl.wait_for(WAIT, |s| s.finished));
    let refusal = carl.status().rejected.unwrap_or_default();
    assert!(refusal.starts_with("full (3 players") && refusal.contains("1 of them away"), "{refusal}");

    host.command(Command::Kick(a_id));
    assert!(host.wait_for(WAIT, |s| s.roster.iter().all(|e| e.id != a_id)));
    let (dora, _d) = start_player(root.path(), &key, "Dora", addr);
    assert!(
        dora.wait_for(WAIT, |s| s.connected && s.my_id.is_some()),
        "the released place is free: {:?}",
        dora.status()
    );
    for h in [&alice, &bob, &dora, &host] {
        h.stop();
    }
}

#[test]
fn a_removed_player_is_told_why_and_does_not_knock_again() {
    let root = tempfile::TempDir::new().unwrap();
    let key = key();
    let (host, _host_game) = start_host(root.path(), &key, HostOptions::default(), Vec::new());
    let (alice, _a) = start_player(root.path(), &key, "Alice", host.local_addr().unwrap());
    assert!(alice.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));
    host.command(Command::Kick(alice.status().my_id.unwrap()));
    assert!(alice.wait_for(WAIT, |s| s.finished));
    let s = alice.status();
    assert_eq!(Some("kicked (removed by the host)".to_owned()), s.rejected);
    assert_eq!((0, false), (s.reconnect_attempts, s.reconnecting));
    assert!(host.wait_for(WAIT, |s| s.roster.len() == 1), "{:?}", host.status().roster);
    host.stop();
}

#[test]
fn a_host_that_stops_tells_its_players_who_end_their_session_instead_of_reconnecting() {
    let root = tempfile::TempDir::new().unwrap();
    let key = key();
    let (host, _host_game) = start_host(root.path(), &key, HostOptions::default(), Vec::new());
    let addr = host.local_addr().unwrap();
    let (alice, _a) = start_player(root.path(), &key, "Alice", addr);
    let (bob, _b) = start_player(root.path(), &key, "Bob", addr);
    assert!(alice.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));
    assert!(bob.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));

    host.stop();
    for p in [&alice, &bob] {
        assert!(p.wait_for(WAIT, |s| s.finished), "{:?}", p.status());
        let s = p.status();
        assert_eq!(Some("ended (the host ended the session)".to_owned()), s.rejected);
        assert_eq!((0, false), (s.reconnect_attempts, s.reconnecting), "no reconnecting to a host that said goodbye");
    }
}

// ------------------------------------------------------------------ flooding

fn flood_options() -> HostOptions {
    HostOptions { max_frames_per_second: 20, frame_burst: 50, ..HostOptions::default() }
}

/// Reads what the host sent until it closes the connection or `Rejected` comes out of the session.
fn read_until_closed(stream: &mut TcpStream, session: &mut PlayerSession) -> Vec<PlayerOutput> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut all = Vec::new();
    loop {
        match FrameCodec::read(stream) {
            Ok(Some(frame)) => all.extend(session.frame_received(&frame)),
            Ok(None) => return all,
            Err(ProtocolError::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return all
            }
            Err(_) => return all,
        }
    }
}

#[test]
fn a_connection_that_floods_before_it_joins_gets_a_reject_naming_the_limit() {
    let root = tempfile::TempDir::new().unwrap();
    let key = key();
    let (host, _host_game) = start_host(root.path(), &key, flood_options(), Vec::new());
    let addr = host.local_addr().unwrap();

    let mut raw = TcpStream::connect(addr).unwrap();
    let junk = Frame::new(FrameHeader::new(1, MessageKind::Chat.into(), 0xFFFF, 1), vec![0; 4]);
    // the bucket holds 50: the 51st frame is the one over
    for _ in 0..51 {
        FrameCodec::write(&mut raw, &junk).unwrap();
    }
    raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut kinds = Vec::new();
    let mut reject = None;
    while let Ok(Some(frame)) = FrameCodec::read(&mut raw) {
        kinds.push(frame.header.kind);
        if frame.header.kind == u8::from(MessageKind::Reject) {
            reject = codec::try_decode::<RejectMessage>(&frame.body);
        }
    }
    let reject = reject.unwrap_or_else(|| panic!("no reject among frames {kinds:?}"));
    assert_eq!(("flood", Some("more than 20 frames per second")), (reject.reason.as_str(), reject.detail.as_deref()));

    // the host is none the worse: an honest player joins
    let (alice, _a) = start_player(root.path(), &key, "Alice", addr);
    assert!(alice.wait_for(WAIT, |s| s.connected && s.my_id.is_some()), "{:?}", alice.status());
    alice.stop();
    host.stop();
}

#[test]
fn a_player_that_floods_is_told_so_by_the_host_and_dropped() {
    let root = tempfile::TempDir::new().unwrap();
    let key = key();
    let (host, _host_game) = start_host(root.path(), &key, flood_options(), Vec::new());
    let (honest, _h) = start_player(root.path(), &key, "Honest", host.local_addr().unwrap());
    assert!(honest.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));

    // a hand-made client: joins, then sends chat as fast as it can
    let mut raw = TcpStream::connect(host.local_addr().unwrap()).unwrap();
    raw.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut session = PlayerSession::new(key.clone(), "Mallory", "0.1");
    while session.state() != PlayerState::Joined {
        let frame = FrameCodec::read(&mut raw).unwrap().expect("the host answers");
        for output in session.frame_received(&frame) {
            if let PlayerOutput::SendToHost(f) = output {
                FrameCodec::write(&mut raw, &f).unwrap();
            }
        }
    }
    let chat = codec::encode(&ChatMessage::new("spam"));
    // the join was the first of the 50: 49 chats pass, the 50th is the flood
    for _ in 0..50 {
        for output in session.local_game_message(MessageKind::Chat, &chat) {
            if let PlayerOutput::SendToHost(f) = output {
                FrameCodec::write(&mut raw, &f).unwrap();
            }
        }
    }
    let outputs = read_until_closed(&mut raw, &mut session);
    let rejected = outputs.iter().find_map(|o| match o {
        PlayerOutput::Rejected(r) => Some(r.clone()),
        _ => None,
    });
    let rejected = rejected.unwrap_or_else(|| panic!("the refusal never arrived: {outputs:?}"));
    assert_eq!(
        ("flood", Some("more than 20 frames per second")),
        (rejected.reason.as_str(), rejected.detail.as_deref())
    );
    assert_eq!(PlayerState::Rejected, session.state());

    // gone for good, and the honest player never noticed
    assert!(host.wait_for(WAIT, |s| s.roster.len() == 2), "{:?}", host.status().roster);
    assert!(honest.status().connected);
    assert!(host.logs().iter().any(|l| l.contains("Mallory sent more than 20 frames per second")), "{:?}", host.logs());
    honest.stop();
    host.stop();
}
