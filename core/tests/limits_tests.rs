//! The per-connection frame limit: the token bucket, and what the host does to a connection that goes over it.

mod net;

use feverlink3_core::protocol::{Frame, FrameHeader};
use feverlink3_core::session::*;
use net::*;

fn tight() -> HostOptions {
    HostOptions { max_frames_per_second: 8, frame_burst: 20, ..HostOptions::default() }
}

fn chat() -> Vec<u8> {
    json(&ChatMessage::new("spam"))
}

// ------------------------------------------------------------------ the bucket

#[test]
fn the_bucket_lets_a_burst_through_and_then_the_rate() {
    // 8 per second = one frame per 0.125 s; every time below is exact in binary, so there is no rounding to argue about
    let mut l = RateLimiter::new(8, 4, 0.0);
    assert_eq!(4, (0..20).filter(|_| l.allow(0.0)).count(), "the burst");
    assert!(!l.allow(0.0625), "half a frame's worth has come back");
    assert!(l.allow(0.125), "one frame's worth has");
    assert!(!l.allow(0.125));
    assert_eq!(4, (0..20).filter(|_| l.allow(100.0)).count(), "a long pause refills to the burst and not beyond");
}

#[test]
fn a_rate_of_zero_never_refuses_and_a_burst_of_zero_counts_as_one() {
    let mut off = RateLimiter::new(0, 0, 0.0);
    assert!((0..10_000).all(|_| off.allow(0.0)));
    let mut one = RateLimiter::new(1, 0, 0.0);
    assert!(one.allow(0.0));
    assert!(!one.allow(0.0));
}

#[test]
fn a_clock_that_goes_back_gives_nothing_and_loses_nothing() {
    let mut l = RateLimiter::new(8, 4, 10.0);
    assert_eq!(4, (0..10).filter(|_| l.allow(10.0)).count());
    assert!(!l.allow(5.0), "earlier time: no refill");
    assert!(l.allow(10.125), "the refill is still measured from the latest time seen");
}

// ------------------------------------------------------------------ the host

#[test]
fn a_player_over_the_limit_is_refused_with_flood_and_loses_its_slot() {
    let mut n = Net::with(tight());
    let a = n.connect("A");
    let b = n.connect("B");
    let (a_id, ticket) = (n.id(a), n.ticket(a));
    let before = n.host.dropped();

    // the bucket holds 20 frames: the join was one of them, so 19 chats pass and the 20th is the flood
    for _ in 0..100 {
        n.send(a, MessageKind::Chat, &chat());
    }
    assert_eq!(19, n.peer(b).count(MessageKind::Chat), "only what came before the limit was relayed");
    assert_eq!(1, n.host.dropped() - before, "the refused frame is counted");

    // a clear refusal, sealed so only the host can say it
    let reject = n.peer(a).session.state();
    assert_eq!(PlayerState::Rejected, reject);
    let r = n.peer(a).rejected.clone().expect("the player is told");
    assert_eq!("flood", r.reason);
    assert_eq!(Some("more than 8 frames per second"), r.detail.as_deref());
    assert!(n.logged("A sent more than 8 frames per second"), "{:?}", n.host_log);

    // and it is gone: connection closed, players told, slot not held
    assert!(n.closed.contains(&a));
    assert!(!n.host.players().contains_key(&a_id));
    assert!(n.host.roster().iter().all(|e| e.id != a_id));
    let left: PlayerLeftMessage = n.peer(b).last(MessageKind::PlayerLeft);
    assert_eq!((a_id, false), (left.id, left.away));
    let again = n.connect_resuming("A", ticket);
    assert!(!n.peer(again).welcomes[0].resumed, "a flooder does not get its slot back");
    assert_ne!(a_id, n.id(again));
}

#[test]
fn the_limit_is_per_connection() {
    let mut n = Net::with(tight());
    let a = n.connect("A");
    let b = n.connect("B");
    let c = n.connect("C");
    for _ in 0..100 {
        n.send(a, MessageKind::Chat, &chat());
    }
    assert_eq!(Some("flood"), n.peer(a).rejected.as_ref().map(|r| r.reason.as_str()));
    assert!(n.peer(b).rejected.is_none() && n.peer(c).rejected.is_none());
    let from_b = |n: &Net| n.peer(c).all::<ChatMessage>(MessageKind::Chat).iter().filter(|m| m.from == n.id(b)).count();
    for _ in 0..10 {
        n.send(b, MessageKind::Chat, &chat());
    }
    assert_eq!(10, from_b(&n), "B still talks, C hears all of it");
    assert_eq!(PlayerState::Joined, n.peer(b).session.state());
}

#[test]
fn a_player_under_the_rate_is_never_refused_however_long_it_plays() {
    let mut n = Net::with(tight());
    let a = n.connect("A");
    let b = n.connect("B");
    // 6 chats a second, and the host's own pings, against a limit of 8 a second
    for _ in 0..180 {
        for _ in 0..6 {
            n.send(a, MessageKind::Chat, &chat());
        }
        n.advance(1.0);
        n.tick();
    }
    assert!(n.peer(a).rejected.is_none());
    assert_eq!(180 * 6, n.peer(b).count(MessageKind::Chat));
}

#[test]
fn a_steady_flood_above_the_rate_is_refused_once_the_burst_is_spent() {
    let mut n = Net::with(tight());
    let a = n.connect("A");
    // 16 a second against 8: the 20-frame bucket drains by 8 a second
    let mut seconds = 0;
    while n.peer(a).rejected.is_none() && seconds < 30 {
        for _ in 0..16 {
            n.send(a, MessageKind::Chat, &chat());
        }
        n.advance(1.0);
        seconds += 1;
    }
    assert!(n.peer(a).rejected.is_some(), "refused eventually");
    assert!((2..=4).contains(&seconds), "after about 20 / 8 seconds, not {seconds}");
}

#[test]
fn an_unintroduced_connection_that_floods_gets_an_unsealed_reject() {
    let mut n = Net::with(tight());
    let c = n.open_raw();
    let junk = Frame::new(FrameHeader::new(1, MessageKind::Chat.into(), HostSession::UNASSIGNED, 1), vec![0; 4]);
    for _ in 0..100 {
        n.inject(c, &junk);
    }
    let frames = &n.raw[&c];
    assert_eq!(u8::from(MessageKind::Challenge), frames[0].header.kind);
    let last = frames.last().unwrap();
    assert_eq!(u8::from(MessageKind::Reject), last.header.kind);
    assert_eq!(HostSession::UNASSIGNED, last.header.sender, "unsealed, as every handshake frame is");
    let reject: RejectMessage = codec::try_decode(&last.body).unwrap();
    assert_eq!("flood", reject.reason);
    assert_eq!(vec![c], n.closed);
    assert_eq!(1, frames.iter().filter(|f| f.header.kind == u8::from(MessageKind::Reject)).count());
    // 20 refused as not introduced, then the one that was over the limit; nothing after the close
    assert_eq!(21, n.host.dropped());
}

#[test]
fn the_hosts_own_game_is_never_limited() {
    let mut n = Net::with(tight());
    let a = n.connect("A");
    for _ in 0..5000 {
        n.host_send(MessageKind::Chat, &chat());
    }
    assert_eq!(5000, n.peer(a).count(MessageKind::Chat));
}

#[test]
fn a_limit_of_zero_turns_it_off() {
    let mut n = Net::with(HostOptions { max_frames_per_second: 0, ..HostOptions::default() });
    let a = n.connect("A");
    let b = n.connect("B");
    for _ in 0..5000 {
        n.send(a, MessageKind::Chat, &chat());
    }
    assert_eq!(5000, n.peer(b).count(MessageKind::Chat));
    assert!(n.peer(a).rejected.is_none());
}

#[test]
fn the_defaults_leave_room_for_a_busy_game() {
    // a clock line per rendered frame at 240 fps, a long road's worth of actions at once: nowhere near a flood
    let o = HostOptions::default();
    assert!(o.max_frames_per_second >= 300 && o.frame_burst >= 1000, "{o:?}");
    let (mut n, ids) = Net::running(2, 1, HostOptions::default());
    for i in 0..1500 {
        n.send(ids[0], MessageKind::Action, &json(&ActionMessage::new(20_000 + i, i, "{}")));
    }
    assert!(n.peer(ids[0]).rejected.is_none());
    assert_eq!(1500, n.peer(ids[1]).count(MessageKind::Action));
}
