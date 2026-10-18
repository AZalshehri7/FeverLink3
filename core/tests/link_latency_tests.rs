//! The latency logs (owner list 2026-10-17, items 4 and 6): the wall-clock stamps a clock or an action carries on its way
//! (`Hops`), the players' clock offsets measured from the pongs, and the host's `link.ping` / `link.host` lines. Measurement only:
//! the same session without stamps relays exactly what it relayed before.

mod common;

use common::*;
use feverlink3_core::session::*;

const WALL0: i64 = 1_760_000_000_000;

/// A host and two players whose wall clocks run `offsets` ms ahead of the host's (the test's clock in ms past WALL0).
fn stamped(offsets: [i64; 2]) -> (Harness, ConnectionId, ConnectionId) {
    let mut h = Harness::new();
    let now = h.now.clone();
    h.host.set_wall_clock(Box::new(move || WALL0 + (*now.lock().unwrap() * 1000.0) as i64));
    let a = h.connect("A");
    let b = h.connect("B");
    for (c, off) in [(a, offsets[0]), (b, offsets[1])] {
        let now = h.now.clone();
        h.players
            .get_mut(&c)
            .unwrap()
            .session
            .set_wall_clock(Box::new(move || WALL0 + off + (*now.lock().unwrap() * 1000.0) as i64));
    }
    (h, a, b)
}

fn clock(t: i64, hop: Option<Hops>) -> Vec<u8> {
    json(&ClockMessage { t, speed: 1, ahead: 3, need: None, safe: None, pause: false, stop: None, hop })
}

#[test]
fn pongs_carry_the_players_wall_clock_and_the_host_measures_and_reports_each_offset() {
    let (mut h, a, b) = stamped([500, -250]);
    h.advance(1.0);
    h.host_call(HostSession::tick);
    assert_eq!(h.host.wall_offset_ms(1), Some(500));
    assert_eq!(h.host.wall_offset_ms(2), Some(-250));
    assert_eq!(h.host.wall_offset_ms(HostSession::HOST_ID), Some(0));
    let pings: Vec<&String> = h.host_log.iter().filter(|l| l.starts_with("link.ping ")).collect();
    assert_eq!(pings.len(), 2, "{:?}", h.host_log);
    assert!(pings[0].contains("peer=1 rtt_ms=0.0 srtt_ms=0.0 off_ms=500"), "{}", pings[0]);
    assert!(pings[0].contains(&format!("wall={}", WALL0 + 1000)), "{}", pings[0]);
    // the next ping tells each player its offset
    h.advance(2.5);
    h.host_call(HostSession::tick);
    assert_eq!(h.p(a).session.wall_offset_ms(), Some(500));
    assert_eq!(h.p(b).session.wall_offset_ms(), Some(-250));
}

#[test]
fn a_relayed_clock_carries_the_senders_stamps_and_the_hosts_never_what_a_player_claims_for_the_host() {
    let (mut h, a, b) = stamped([500, 0]);
    h.advance(1.0);
    h.host_call(HostSession::tick);
    h.hello_all(20_000, 1);
    // a player that forges the host's part and an implausible stamp of its own
    let forged = Hops {
        g: Some(WALL0 + 1490),
        l: Some(42),
        h: Some(7),
        so: Some(9),
        sr: Some(1),
        w: Some(5),
        ro: Some(5),
        rr: Some(5),
    };
    h.player_game(a, MessageKind::Clock, &clock(20_200, Some(forged)));
    let pc = h.p(b).last::<PeerClockMessage>(MessageKind::PeerClock);
    assert_eq!(pc.from, 1);
    let hop = pc.hop.expect("the stamps are relayed");
    assert_eq!(
        hop,
        Hops {
            g: Some(WALL0 + 1490),
            l: None,
            h: Some(WALL0 + 1000),
            so: Some(500),
            sr: Some(0),
            w: None,
            ro: None,
            rr: None
        }
    );
    // the host's own game: offset 0, round trip 0
    h.host_game(
        MessageKind::Clock,
        &clock(20_400, Some(Hops { g: Some(WALL0 + 999), l: Some(WALL0 + 1000), ..Hops::default() })),
    );
    let pc = h.p(b).last::<PeerClockMessage>(MessageKind::PeerClock);
    assert_eq!(pc.from, 0);
    assert_eq!(pc.hop.and_then(|x| x.so), Some(0));
    // a clock without stamps is relayed without any (an older mod)
    h.player_game(a, MessageKind::Clock, &clock(20_600, None));
    assert_eq!(h.p(b).last::<PeerClockMessage>(MessageKind::PeerClock).hop, None);
}

#[test]
fn a_relayed_action_carries_its_stamps_too() {
    let (mut h, a, b) = stamped([0, 0]);
    h.advance(1.0);
    h.host_call(HostSession::tick);
    h.hello_all(20_000, 1);
    let mut m = ActionMessage::new(21_000, 1, "{x}");
    m.hop = Some(Hops { g: Some(WALL0 + 900), l: Some(WALL0 + 950), ..Hops::default() });
    h.player_game(a, MessageKind::Action, &json(&m));
    let got = h.p(b).last::<ActionMessage>(MessageKind::Action);
    assert_eq!(got.hop.and_then(|x| x.h), Some(WALL0 + 1000));
    assert_eq!(got.body, "{x}");
}

#[test]
fn the_host_writes_a_link_host_line_per_game_every_minute() {
    let (mut h, a, _b) = stamped([0, 0]);
    h.host_call(HostSession::tick);
    h.hello_all(20_000, 1);
    for i in 0..5 {
        h.advance(0.25);
        h.player_game(a, MessageKind::Clock, &clock(20_200 + 200 * i, None));
    }
    for _ in 0..30 {
        h.advance(2.0);
        h.host_call(HostSession::tick);
    }
    let minute: Vec<&String> = h.host_log.iter().filter(|l| l.starts_with("link.host ")).collect();
    assert!(
        minute.iter().any(|l| l.contains(" peer=1 ") && l.contains("clock_gap_ms_n=5 clock_gap_ms=250/250/250")),
        "{minute:?}"
    );
    assert!(minute.iter().any(|l| l.contains(" peer=2 ") && l.contains("rtt_ms_n=")), "{minute:?}");
}
