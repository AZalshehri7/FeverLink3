//! Pings, round-trip times, and closing connections that went quiet or never finished the handshake.

mod net;

use feverlink3_core::session::*;
use net::*;

fn near(expected: f64, actual: Option<f64>) -> bool {
    actual.is_some_and(|a| (a - expected).abs() < 0.5)
}

fn state_of(n: &Net, id: u16) -> Option<ConnectionState> {
    n.host.roster().iter().find(|e| e.id == id).map(|e| e.state)
}

// ------------------------------------------------------------------ round-trip time

#[test]
fn a_ping_measures_the_round_trip_of_every_player() {
    let mut n = Net::new();
    let a = n.connect("A");
    let b = n.connect("B");
    n.set_latency(a, 0.040);
    n.set_latency(b, 0.120);
    assert_eq!(None, n.host.rtt_ms(n.id(a)), "nothing measured before the first ping");
    n.tick();
    assert!(near(40.0, n.host.rtt_ms(n.id(a))), "{:?}", n.host.rtt_ms(n.id(a)));
    assert!(near(120.0, n.host.rtt_ms(n.id(b))), "{:?}", n.host.rtt_ms(n.id(b)));
    assert_eq!(Some(0.0), n.host.rtt_ms(HostSession::HOST_ID), "the host's own game is local");
}

#[test]
fn pings_go_out_every_two_seconds_and_not_more_often() {
    let mut n = Net::new();
    let a = n.connect("A");
    n.tick();
    assert_eq!(1, n.frames_to(a, MessageKind::Ping));
    n.advance(1.0);
    n.tick();
    assert_eq!(1, n.frames_to(a, MessageKind::Ping), "too soon");
    n.advance(1.0);
    n.tick();
    assert_eq!(2, n.frames_to(a, MessageKind::Ping));
    n.run_for(20.0, 0.25);
    assert_eq!(2 + 10, n.frames_to(a, MessageKind::Ping));
}

#[test]
fn the_round_trip_time_is_smoothed_over_recent_pings() {
    let mut n = Net::new();
    let a = n.connect("A");
    let id = n.id(a);
    n.set_latency(a, 0.100);
    n.tick();
    assert!(near(100.0, n.host.rtt_ms(id)));
    n.set_latency(a, 0.020);
    n.advance(2.0);
    n.tick();
    assert!(near(80.0, n.host.rtt_ms(id)), "a quarter of the way to the new sample: {:?}", n.host.rtt_ms(id));
    n.advance(2.0);
    n.tick();
    assert!(near(65.0, n.host.rtt_ms(id)), "{:?}", n.host.rtt_ms(id));
    for _ in 0..30 {
        n.advance(2.0);
        n.tick();
    }
    assert!(near(20.0, n.host.rtt_ms(id)), "settles on the new value: {:?}", n.host.rtt_ms(id));
}

#[test]
fn every_player_learns_everyones_round_trip_time() {
    let mut n = Net::new();
    let a = n.connect("A");
    let b = n.connect("B");
    n.set_latency(a, 0.040);
    n.set_latency(b, 0.120);
    n.tick(); // measures
    n.advance(2.0);
    n.tick(); // reports what it measured
    let seen_by_a = n.peer(a).session.roster();
    let rtt_of = |id: u16| seen_by_a.iter().find(|e| e.id == id).and_then(|e| e.rtt_ms);
    assert!(near(120.0, rtt_of(n.id(b))), "{seen_by_a:?}");
    assert!(near(40.0, rtt_of(n.id(a))));
    assert_eq!(Some(0.0), rtt_of(0));
    assert!(near(40.0, n.peer(a).session.rtt_ms()), "a player's own value");
    assert!(near(120.0, n.peer(b).session.rtt_ms()));
}

#[test]
fn ping_and_pong_stay_between_the_host_and_the_launcher() {
    let mut n = Net::new();
    let a = n.connect("A");
    n.run_for(10.0, 0.5);
    assert!(n.frames_to(a, MessageKind::Ping) >= 4);
    assert_eq!(0, n.peer(a).count(MessageKind::Ping), "the player's game is never given a ping");
    assert!(n.host_game.iter().all(|(k, _)| *k != MessageKind::Ping && *k != MessageKind::Pong));
    assert_eq!(0, n.host.dropped(), "pongs are an ordinary message from a player");
}

#[test]
fn a_pong_that_answers_no_ping_changes_nothing() {
    let mut n = Net::new();
    let a = n.connect("A");
    let id = n.id(a);
    n.send(a, MessageKind::Pong, &json(&PongMessage { n: 4242 }));
    assert_eq!(None, n.host.rtt_ms(id));
    n.tick();
    let first = n.host.rtt_ms(id);
    assert!(first.is_some());
    // the same number again, much later: that ping is long answered
    n.set_latency(a, 5.0);
    n.advance(60.0);
    n.send(a, MessageKind::Pong, &json(&PongMessage { n: 1 }));
    assert_eq!(first, n.host.rtt_ms(id));
}

#[test]
fn pongs_from_the_hosts_own_game_are_ignored() {
    let mut n = Net::new();
    let _a = n.connect("A");
    n.tick();
    n.host_send(MessageKind::Pong, &json(&PongMessage { n: 1 }));
    n.host_send(MessageKind::Leave, b"{}");
    assert!(n.host.players().contains_key(&0));
}

// ------------------------------------------------------------------ silence

#[test]
fn a_player_that_goes_silent_is_cut_off_and_its_slot_held() {
    let mut n = Net::new();
    let a = n.connect("A");
    let b = n.connect("B");
    let a_id = n.id(a);
    n.tick(); // everyone answers a ping: heard just now
    n.cut(a); // the cable is pulled; nobody is told
    n.run_for(29.0, 1.0);
    assert!(n.host.players().contains_key(&a_id), "still inside the 30 s");
    n.run_for(2.0, 1.0);
    assert!(!n.host.players().contains_key(&a_id));
    assert!(n.logged("A timed out"), "{:?}", n.host_log);
    assert!(n.closed.contains(&a));
    let left: PlayerLeftMessage = n.peer(b).last(MessageKind::PlayerLeft);
    assert_eq!((a_id, true), (left.id, left.away), "the others learn it is away, not gone");
    assert_eq!(Some(ConnectionState::Away), state_of(&n, a_id));
    assert_eq!(PlayerState::Joined, n.peer(b).session.state(), "B is untouched");
}

#[test]
fn a_player_that_answers_is_never_timed_out() {
    let mut n = Net::new();
    let a = n.connect("A");
    n.run_for(600.0, 1.0);
    assert!(n.host.players().contains_key(&n.id(a)));
    assert!(!n.logged("timed out"));
}

#[test]
fn a_stalled_host_times_nobody_out() {
    let mut n = Net::new();
    let a = n.connect("A");
    let a_id = n.id(a);
    n.tick();
    n.cut(a);
    // the host's own process was suspended for 100 s: nobody had the chance to speak
    n.advance(100.0);
    n.tick();
    assert!(n.host.players().contains_key(&a_id));
    // but a silence it did watch counts in full
    n.run_for(29.0, 1.0);
    assert!(n.host.players().contains_key(&a_id));
    n.run_for(2.0, 1.0);
    assert!(!n.host.players().contains_key(&a_id));
}

#[test]
fn any_frame_counts_as_a_sign_of_life() {
    let clock = json(&ClockMessage { t: 5, speed: 1, ahead: 3 });
    // pings 100 s apart: between them, only what the game itself sends can show the connection is alive
    let options = HostOptions { ping_seconds: 100.0, ..HostOptions::default() };
    let mut n = Net::with(options.clone());
    let a = n.connect("A");
    n.tick();
    for _ in 0..30 {
        n.run_for(10.0, 1.0);
        n.send(a, MessageKind::Clock, &clock);
    }
    assert!(n.host.players().contains_key(&n.id(a)), "a clock every 10 s is enough");

    let mut quiet = Net::with(options);
    let b = quiet.connect("B");
    quiet.tick();
    quiet.run_for(60.0, 1.0);
    assert!(!quiet.host.players().contains_key(&quiet.id(b)), "while saying nothing at all it is cut off");
}

#[test]
fn without_pings_there_is_no_idle_timeout() {
    let mut n = Net::with(HostOptions { ping_seconds: 0.0, ..HostOptions::default() });
    let a = n.connect("A");
    n.run_for(300.0, 1.0);
    assert_eq!(0, n.frames_to(a, MessageKind::Ping));
    assert!(n.host.players().contains_key(&n.id(a)), "a quiet lobby is not a dead connection");
}

#[test]
fn an_idle_timeout_of_zero_keeps_silent_players() {
    let mut n = Net::with(HostOptions { idle_timeout_seconds: 0.0, ..HostOptions::default() });
    let a = n.connect("A");
    n.tick();
    n.cut(a);
    n.run_for(300.0, 1.0);
    assert!(n.host.players().contains_key(&n.id(a)));
}

// ------------------------------------------------------------------ the handshake

#[test]
fn a_connection_that_never_joins_is_closed_after_ten_seconds() {
    let mut n = Net::new();
    let c = n.open_raw();
    n.tick(); // the host notes it
    n.run_for(9.0, 1.0);
    assert!(n.closed.is_empty());
    n.run_for(2.0, 1.0);
    assert_eq!(vec![c], n.closed);
    let reject = n.raw[&c].iter().find(|f| f.header.kind == u8::from(MessageKind::Reject)).expect("told why");
    let r: RejectMessage = codec::try_decode(&reject.body).unwrap();
    assert_eq!("timeout", r.reason);
}

#[test]
fn a_joined_connection_is_not_subject_to_the_handshake_timeout() {
    let mut n = Net::with(HostOptions { idle_timeout_seconds: 0.0, ..HostOptions::default() });
    let a = n.connect("A");
    n.run_for(120.0, 1.0);
    assert!(n.host.players().contains_key(&n.id(a)));
    assert!(n.closed.is_empty());
}

#[test]
fn a_handshake_timeout_of_zero_waits_for_ever() {
    let mut n = Net::with(HostOptions { handshake_timeout_seconds: 0.0, ..HostOptions::default() });
    let _c = n.open_raw();
    n.run_for(300.0, 1.0);
    assert!(n.closed.is_empty());
}
