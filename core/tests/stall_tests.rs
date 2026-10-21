//! The stall timeout (host/stall.rs): a game whose clock lines stop (an autosave, a frozen window) no longer holds the others at their
//! input barriers; it catches up later, and the input barrier stays exact (no action of it is ever relayed stamped before a promise the
//! others were given).
//!
//! Numbers from the friend test of 2026-10-18: x4, round trip about 30 ms, a clock line per engine batch (4 steps, 167-218
//! ms apart), the lead of the 907af4e fix (7 steps at x4 on that link) and the clone freezes of 7-10 s of the owner's notes of 2026-10-17
//! as the stall.

mod net;

use feverlink3_core::session::*;
use net::*;

const STEP: i64 = Lockstep::STEP;
const BATCH_S: f64 = 0.2;

fn clock(t: i64, speed: i32, lead: i64) -> Vec<u8> {
    // a game's promise (the mod's L.promise): its clock plus the lead plus barrierExtra (speed + 2 steps)
    let safe = Lockstep::round_up_to_step((t + lead) as f64) + i64::from(speed.clamp(1, 4) + 2) * STEP;
    json(&ClockMessage { t, speed, ahead: 7, need: None, safe: Some(safe), pause: false, stop: None, hop: None })
}

fn action(stamp: i64, seq: i64, pause: bool) -> Vec<u8> {
    json(&ActionMessage {
        stamp,
        seq,
        body: "{}".into(),
        from: 0,
        category: "build".into(),
        allowed: true,
        pause,
        pause_seq: None,
        pause_upto: None,
        hop: None,
    })
}

/// Host and one guest at x4. Each game runs a batch every BATCH_S unless `frozen` says it is stalled at that host time; returns the
/// clocks reached. The host's own game is player 0.
struct Session {
    n: Net,
    guest: ConnectionId,
    t_host: i64,
    t_guest: i64,
}

impl Session {
    fn new(options: HostOptions) -> Self {
        let (n, ids) = Net::running(1, 4, options);
        Session { n, guest: ids[0], t_host: 10_000, t_guest: 10_000 }
    }

    fn lead(&self) -> i64 {
        self.n.host.state().lead
    }

    /// `seconds` of play: both games send a clock line each batch; the guest only while `guest_runs`, and it moves `guest_steps`
    /// steps a batch (4 at x4, 8 while it catches up at twice the pace).
    fn play(&mut self, seconds: f64, guest_runs: bool, guest_steps: i64) {
        let batches = (seconds / BATCH_S).round() as i64;
        for _ in 0..batches {
            self.n.advance(BATCH_S);
            self.t_host += 4 * STEP;
            let lead = self.lead();
            self.n.host_send(MessageKind::Clock, &clock(self.t_host, 4, lead));
            if guest_runs {
                self.t_guest += guest_steps * STEP;
                self.n.send(self.guest, MessageKind::Clock, &clock(self.t_guest, 4, lead));
            }
            self.n.tick();
        }
    }

    fn guest_id(&self) -> u16 {
        self.n.id(self.guest)
    }

    /// The peer clocks of the guest the host's own game read, in order.
    fn guest_lines(&self) -> Vec<PeerClockMessage> {
        let g = self.guest_id();
        self.n.host_game_all::<PeerClockMessage>(MessageKind::PeerClock).into_iter().filter(|p| p.from == g).collect()
    }
}

/// The input barrier's rule, checked on what one game received in order: once it has read a promise `safe` of player `from`, every
/// action of `from` it reads afterwards is stamped at or after it (outside a pause).
fn assert_barrier_exact(game: &[(MessageKind, Vec<u8>)], from: u16) {
    let mut promised = i64::MIN;
    for (kind, payload) in game {
        match kind {
            MessageKind::PeerClock => {
                let p: PeerClockMessage = codec::try_decode(payload).unwrap();
                if p.from == from && !p.pause {
                    promised = promised.max(p.safe.unwrap_or(i64::MIN));
                }
            }
            MessageKind::Action => {
                let a: ActionMessage = codec::try_decode(payload).unwrap();
                if a.from == from {
                    assert!(
                        a.stamp >= promised,
                        "action {}:{} stamped {} after a promise of {promised}",
                        a.from,
                        a.seq,
                        a.stamp
                    );
                }
            }
            _ => {}
        }
    }
}

#[test]
fn a_guest_that_stops_for_an_autosave_holds_the_host_only_until_the_timeout_then_the_host_runs_on() {
    let mut s = Session::new(HostOptions::default());
    s.play(5.0, true, 4);
    assert!(s.n.host.state().stalled.is_empty());
    let lines_before = s.guest_lines().len();
    // the guest's game freezes (a clone freeze of the owner's notes: 7-10 s); its launcher still answers pings
    s.play(2.6, false, 4);
    assert!(s.n.host.state().stalled.is_empty(), "under the timeout (3 s) nothing happens: a slow frame is no stall");
    let held_promise = s.guest_lines().last().unwrap().safe.unwrap();
    s.play(0.8, false, 4);
    let g = s.guest_id();
    assert_eq!(vec![g], s.n.host.state().stalled, "stalled after 3 s");
    assert!(s.n.logged("stall: "), "{:?}", s.n.host_log);
    let lines = s.guest_lines();
    let proxy = lines.last().unwrap();
    assert!(proxy.stalled && lines.len() > lines_before);
    assert!(proxy.safe.unwrap() >= held_promise, "the promise the host keeps is never below the guest's own");
    // the host game's barrier (the promise less the receiver's keep, speed + 2 steps) is ahead of its clock: it runs on
    s.play(5.0, false, 4);
    let proxy = s.guest_lines().last().unwrap().safe.unwrap();
    let keep = 6 * STEP;
    assert!(proxy - keep >= s.t_host, "the host game has room: barrier {} >= clock {}", proxy - keep, s.t_host);
    let session: SessionState = s.n.peer(s.guest).last(MessageKind::Session);
    assert_eq!(vec![g], session.stalled, "the guest is told too (its window says it is catching up)");
}

#[test]
fn the_stalled_guest_catches_up_and_its_promises_are_relayed_again_never_lower_than_the_hosts() {
    let mut s = Session::new(HostOptions::default());
    s.play(4.0, true, 4);
    s.play(8.0, false, 4); // stalled after 3 s, 5 s more
    let g = s.guest_id();
    assert_eq!(vec![g], s.n.host.state().stalled);
    let behind = (s.t_host - s.t_guest) / STEP;
    assert!(behind >= 150, "8 s at x4 is about 160 steps: {behind}");
    // back: it runs at twice the pace until it has caught up (the mod's catch-up)
    let mut caught_after = None;
    for i in 0..200 {
        let steps = if s.t_guest + 8 * STEP <= s.t_host { 8 } else { 4 };
        s.play(BATCH_S, true, steps);
        if s.n.host.state().stalled.is_empty() {
            caught_after = Some(f64::from(i + 1) * BATCH_S);
            break;
        }
    }
    let caught = caught_after.expect("the stall ends once the guest has caught up");
    assert!(caught < 12.0, "160 steps at 4 a batch more than the host: about 8 s ({caught} s)");
    assert!(s.n.logged("stall: P0 runs again"), "{:?}", s.n.host_log);
    assert!(s.n.logged("caught up"), "{:?}", s.n.host_log);
    // what the host's game read of the guest: the promises never go down (its barrier never moves back)
    let lines = s.guest_lines();
    let mut last = i64::MIN;
    for v in lines.iter().filter_map(|p| p.safe) {
        assert!(v >= last, "a promise of the guest went back from {last} to {v}");
        last = v;
    }
    assert!(!lines.last().unwrap().stalled, "its own clocks are relayed again");
    // and the session runs on as usual
    s.play(2.0, true, 4);
    assert!(s.n.host.state().stalled.is_empty());
}

#[test]
fn actions_of_a_stalled_game_are_stamped_by_the_host_or_refused_on_every_game_never_relayed_late() {
    let mut s = Session::new(HostOptions::default());
    s.play(4.0, true, 4);
    s.play(4.0, false, 4);
    let g = s.guest_id();
    assert_eq!(vec![g], s.n.host.state().stalled);
    let proxy = s.guest_lines().last().unwrap().safe.unwrap();
    // 1. shipped before the guest knew (stamped by its own rule from its old clock): refused on every game, its own included
    let old = s.t_guest + 9 * STEP;
    assert!(old < proxy);
    s.n.send(s.guest, MessageKind::Action, &action(old, 7, false));
    let vetoes: Vec<BuildVetoMessage> = s.n.host_game_all(MessageKind::BuildVeto);
    let v = vetoes.last().expect("the host game is told");
    assert!(v.host && v.owner == g && v.seq == 7, "{v:?}");
    let own: BuildVetoMessage = s.n.peer(s.guest).last(MessageKind::BuildVeto);
    assert!(own.host && own.seq == 7, "the sender is told too: {own:?}");
    assert!(s.n.host_game_all::<ActionMessage>(MessageKind::Action).iter().all(|a| a.seq != 7), "never relayed");
    assert!(s.n.logged("refused on every game"));
    // 2. stamped by the host (the mod marks every action of a stalled game so): past every promise, to every game, its sender too
    s.n.send(s.guest, MessageKind::Action, &action(s.t_guest + 9 * STEP, 8, true));
    let a = s.n.host_game_all::<ActionMessage>(MessageKind::Action).into_iter().find(|a| a.seq == 8).expect("relayed");
    assert!(a.stamp >= proxy, "{} >= {proxy}", a.stamp);
    assert!(a.pause_seq.is_some());
    let back: ActionMessage = s.n.peer(s.guest).last(MessageKind::Action);
    assert_eq!((8, a.stamp), (back.seq, back.stamp), "the sender applies it at the host's stamp too");
    // 3. stamped at or after the host's promise by its own game: relayed as it is
    let later = s.guest_lines().last().unwrap().safe.unwrap() + 2 * STEP;
    s.n.send(s.guest, MessageKind::Action, &action(later, 9, false));
    let a = s.n.host_game_all::<ActionMessage>(MessageKind::Action).into_iter().find(|a| a.seq == 9).expect("relayed");
    assert_eq!(later, a.stamp);
    // a player cannot pass its own veto off as the host's
    s.n.send(
        s.guest,
        MessageKind::BuildVeto,
        &json(&BuildVetoMessage { seq: 1, owner: 0, why: "x".into(), from: 0, name: None, host: true }),
    );
    assert!(!s.n.host_game_all::<BuildVetoMessage>(MessageKind::BuildVeto).last().unwrap().host);
    // the whole run: the host's game never read an action of the guest stamped before a promise it had read
    s.play(3.0, true, 8);
    assert_barrier_exact(&s.n.host_game, g);
}

#[test]
fn the_host_game_stalled_is_handled_the_same_way_for_the_guest() {
    let mut s = Session::new(HostOptions::default());
    s.play(4.0, true, 4);
    // the host's own game freezes; its launcher (the session) runs on
    for _ in 0..20 {
        s.n.advance(BATCH_S);
        s.t_guest += 4 * STEP;
        let lead = s.lead();
        s.n.send(s.guest, MessageKind::Clock, &clock(s.t_guest, 4, lead));
        s.n.tick();
    }
    assert_eq!(vec![HostSession::HOST_ID], s.n.host.state().stalled);
    let proxies: Vec<PeerClockMessage> = s.n.peer(s.guest).all::<PeerClockMessage>(MessageKind::PeerClock);
    let p = proxies.iter().rev().find(|p| p.from == HostSession::HOST_ID).unwrap();
    assert!(p.stalled && p.safe.unwrap() - 6 * STEP >= s.t_guest, "the guest has room: {p:?} at {}", s.t_guest);
    // an action of the host's own game stamped before that promise: refused on every game, the host's own included
    s.n.host_send(MessageKind::Action, &action(s.t_host + 9 * STEP, 3, false));
    let own: Vec<BuildVetoMessage> = s.n.host_game_all(MessageKind::BuildVeto);
    assert!(own.last().is_some_and(|v| v.host && v.owner == HostSession::HOST_ID && v.seq == 3));
}

#[test]
fn no_stall_while_paused_when_nobody_is_heard_or_when_switched_off() {
    // switched off: the old behaviour, the others wait
    let mut off = Session::new(HostOptions { stall_timeout_seconds: 0.0, ..HostOptions::default() });
    off.play(3.0, true, 4);
    off.play(8.0, false, 4);
    assert!(off.n.host.state().stalled.is_empty());
    // nobody heard (the host's own session thread was away): no game is singled out
    let mut away = Session::new(HostOptions::default());
    away.play(3.0, true, 4);
    away.n.run_for(6.0, 0.25);
    assert!(away.n.host.state().stalled.is_empty());
    // paused: nobody needs anybody's promise
    let mut paused = Session::new(HostOptions::default());
    paused.play(3.0, true, 4);
    paused.n.host_call(HostSession::pause);
    for _ in 0..30 {
        paused.n.advance(BATCH_S);
        let lead = paused.lead();
        paused.n.host_send(MessageKind::Clock, &clock(paused.t_host, 0, lead));
        paused.n.tick();
    }
    assert!(paused.n.host.state().stalled.is_empty());
}

#[test]
fn the_silence_of_a_stall_does_not_lengthen_the_lead_and_a_reload_ends_the_stall() {
    let mut s = Session::new(HostOptions::default());
    s.play(6.0, true, 4);
    let lead = s.lead();
    s.play(6.0, false, 4);
    for _ in 0..60 {
        let steps = if s.t_guest + 8 * STEP <= s.t_host { 8 } else { 4 };
        s.play(BATCH_S, true, steps);
    }
    assert!(
        s.lead() <= lead + STEP,
        "a 6 s silence is a stall, not jitter the lead must cover: {} -> {}",
        lead / STEP,
        s.lead() / STEP
    );
    // a stalled game that loads a world (a resync) makes its promises from it again
    let mut r = Session::new(HostOptions::default());
    r.play(3.0, true, 4);
    r.play(4.0, false, 4);
    assert!(!r.n.host.state().stalled.is_empty());
    r.n.send(r.guest, MessageKind::Hello, &json(&HelloMessage::new(r.t_guest)));
    assert!(r.n.host.state().stalled.is_empty());
    assert!(r.n.logged("is no longer stalled"));
}

#[test]
fn a_pause_and_a_checksum_asked_for_during_a_stall_lie_past_the_promise_the_host_keeps() {
    let mut s = Session::new(HostOptions::default());
    s.play(3.0, true, 4);
    s.play(5.0, false, 4);
    let proxy = s.guest_lines().last().unwrap().safe.unwrap();
    assert!(s.n.host.future_stamp() >= proxy);
    s.n.host_call(HostSession::pause);
    assert!(s.n.host.state().pause_at.unwrap() >= proxy, "no game can have passed the pause");
}
