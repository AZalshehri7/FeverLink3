//! Bounds and refusals that keep a hostile or broken peer from hurting the host or a player: one test per rule of
//! `docs/SECURITY.md`. The randomised counterpart is `robustness_tests.rs`.

mod common;

use std::collections::HashSet;

use common::*;
use feverlink3_core::crypto::{Sealer, SessionKey};
use feverlink3_core::protocol::{Frame, FrameHeader};
use feverlink3_core::session::*;

fn sealed(h: &Harness, sender: u16, last_counter: u64, kind: MessageKind, payload: &[u8]) -> Frame {
    Sealer::resume(&h.key, sender, last_counter).seal(kind.into(), payload)
}

/// A joiner that announces `version`, wired to the host by hand; what the host answers it with, if it rejects.
fn join_announcing(h: &mut Harness, connection: ConnectionId, version: &str) -> Option<RejectMessage> {
    let mut player = PlayerSession::with_clock(h.key.clone(), "Old", version, Box::new(|| 0.0));
    let mut to_player: Vec<Frame> = Vec::new();
    let absorb = |outs: Vec<HostOutput>, to_player: &mut Vec<Frame>| {
        for o in outs {
            if let HostOutput::SendToConnection { connection: c, frame } = o {
                if c == connection {
                    to_player.push(frame);
                }
            }
        }
    };
    absorb(h.host.connection_opened(connection), &mut to_player);
    let mut rejected = None;
    while let Some(frame) = to_player.pop() {
        for o in player.frame_received(&frame) {
            match o {
                PlayerOutput::SendToHost(f) => absorb(h.host.frame_received(connection, &f), &mut to_player),
                PlayerOutput::Rejected(r) => rejected = Some(r),
                _ => {}
            }
        }
    }
    rejected
}

#[test]
fn the_host_closes_connections_that_never_join_and_keeps_the_ones_that_did() {
    let mut h = Harness::new();
    let player = h.connect("Pat");
    for i in 0..5 {
        h.host.connection_opened(1000 + i);
    }
    // the host is ticked every second: a longer gap would be taken for a stall of the host and forgiven
    let limit = HostOptions::default().handshake_timeout_seconds;
    let mut closed: Vec<ConnectionId> = Vec::new();
    for second in 1..=(limit as u32 + 2) {
        h.advance(1.0);
        let outs = h.host.tick();
        let now_closed: Vec<ConnectionId> = outs
            .iter()
            .filter_map(|o| match o {
                HostOutput::CloseConnection(c) => Some(*c),
                _ => None,
            })
            .collect();
        if f64::from(second) < limit {
            assert!(now_closed.is_empty(), "not yet, at second {second}");
        }
        closed.extend(now_closed);
    }
    closed.sort_unstable();
    assert_eq!(closed, (1000..1005).collect::<Vec<_>>());
    assert!(h.host.players().len() == 2 && h.routable.contains(&player), "the joined player stays");
    h.advance(600.0);
    assert!(h.host.tick().iter().all(|o| !matches!(o, HostOutput::CloseConnection(_))));
    // the late close event of the transport finds nothing to remove
    assert!(h.host.connection_closed(1000).is_empty());
}

#[test]
fn the_host_refuses_connections_beyond_the_pending_limit_and_takes_new_ones_when_room_returns() {
    let mut h = Harness::new();
    // a joined player does not count as pending
    let player = h.connect("Pat");
    for i in 0..HostSession::MAX_PENDING as u64 {
        let outs = h.host.connection_opened(2000 + i);
        assert!(outs.iter().all(|o| !matches!(o, HostOutput::CloseConnection(_))));
    }
    let outs = h.host.connection_opened(9999);
    assert!(outs.iter().any(|o| matches!(o, HostOutput::CloseConnection(9999))), "refused");
    assert!(!outs.iter().any(|o| matches!(o, HostOutput::SendToConnection { .. })), "no challenge for it");
    assert_eq!(h.p(player).session.state(), PlayerState::Joined);
    for _ in 0..(HostOptions::default().handshake_timeout_seconds as u32 + 1) {
        h.advance(1.0);
        h.host.tick();
    }
    let outs = h.host.connection_opened(10_000);
    assert!(outs.iter().any(|o| matches!(o, HostOutput::SendToConnection { .. })), "room again");
}

#[test]
fn clocks_that_are_not_game_times_are_dropped() {
    let (mut h, ids) = Harness::running(2, 1, HostOptions::default());
    let (liar, other) = (ids[0], ids[1]);
    let stamp = h.host.future_stamp();
    let peer_clocks = h.p(other).count(MessageKind::PeerClock);
    let dropped = h.host.dropped();
    for (t, speed) in
        [(i64::MAX, 1), (i64::MIN, 1), (-1, 1), (1 << 51, 1), (10_000, 1000), (10_000, -1), (10_000, i32::MAX)]
    {
        h.player_game(
            liar,
            MessageKind::Clock,
            &json(&ClockMessage { t, speed, ahead: 3, need: None, safe: None, pause: false, stop: None, hop: None }),
        );
    }
    assert_eq!(h.host.dropped(), dropped + 7);
    assert_eq!(h.p(other).count(MessageKind::PeerClock), peer_clocks, "nothing was relayed");
    assert_eq!(h.host.future_stamp(), stamp, "the stamps did not move");
    assert!(h.host_log.iter().filter(|l| l.contains("clock of")).count() <= 1, "logged once");
    // the extremes that are real are accepted
    h.player_game(
        liar,
        MessageKind::Clock,
        &json(&ClockMessage {
            t: PlayerClock::MAX_TIME,
            speed: 16,
            ahead: 3,
            need: None,
            safe: None,
            pause: false,
            stop: None,
            hop: None,
        }),
    );
    assert_eq!(h.host.dropped(), dropped + 7);
    // and the host's own arithmetic holds at the edge
    h.host_call(HostSession::pause);
    h.host_call(HostSession::request_checksum);
    assert!(h.host.future_stamp() > stamp);
}

#[test]
fn an_action_body_over_the_limit_is_not_relayed_and_one_at_the_limit_is() {
    let (mut h, ids) = Harness::running(2, 1, HostOptions::default());
    let (sender, other) = (ids[0], ids[1]);
    let at_limit = ActionMessage::new(30_000, 1, &"x".repeat(HostSession::MAX_ACTION_BODY));
    h.player_game(sender, MessageKind::Action, &json(&at_limit));
    assert_eq!(h.host.actions_relayed(), 1);
    assert_eq!(h.p(other).count(MessageKind::Action), 1);
    let over = ActionMessage::new(30_200, 2, &"x".repeat(HostSession::MAX_ACTION_BODY + 1));
    h.player_game(sender, MessageKind::Action, &json(&over));
    assert_eq!(h.host.actions_relayed(), 1);
    assert_eq!(h.p(other).count(MessageKind::Action), 1);
    assert!(h.host_log.iter().any(|l| l.contains("body over")));
}

#[test]
fn checksum_reports_that_are_not_the_size_of_a_world_are_ignored() {
    let (mut h, ids) = Harness::running(1, 1, HostOptions::default());
    let c = ids[0];
    h.host_call(HostSession::request_checksum);
    let n = h.p(c).last::<ChecksumRequestMessage>(MessageKind::ChecksumRequest).n;
    let many: std::collections::BTreeMap<String, String> =
        (0..ChecksumRounds::MAX_PARTS + 1).map(|i| (format!("part{i}"), "00000000".to_owned())).collect();
    h.player_game(c, MessageKind::ChecksumReport, &json(&ChecksumReportMessage::new(n, 10_000, many)));
    let too_long = "f".repeat(ChecksumRounds::MAX_TEXT + 1);
    let long = parts(&[("money", too_long.as_str())]);
    h.player_game(c, MessageKind::ChecksumReport, &json(&ChecksumReportMessage::new(n, 10_000, long)));
    assert!(h.events.iter().all(|e| !matches!(e, HostOutput::ChecksumCompleted(_))), "neither report counted");
    let fine = parts(&[("money", "00000001")]);
    h.player_game(c, MessageKind::ChecksumReport, &json(&ChecksumReportMessage::new(n, 10_000, fine.clone())));
    h.host_game(MessageKind::ChecksumReport, &json(&ChecksumReportMessage::new(n, 10_000, fine)));
    assert!(h.events.iter().any(|e| matches!(e, HostOutput::ChecksumCompleted(_))), "a normal report still is");
}

#[test]
fn the_log_does_not_grow_with_what_a_peer_chooses() {
    // this is about memory, not rates: the frame limit would end the connection after its burst
    let mut h =
        Harness::with(HostOptions { max_frames_per_second: 0, max_bytes_per_second: 0, ..HostOptions::default() });
    let a = h.connect("A");
    let logged_before = h.host_log.len();
    // 5000 frames that claim 5000 different senders, all over A's connection
    for i in 0..5000u64 {
        let claimed = (i % 60_000) as u16 + 2;
        let frame = Frame::new(FrameHeader::new(1, MessageKind::Chat.into(), claimed, i + 1), vec![1; 40]);
        let outs = h.host.frame_received(a, &frame);
        h.pump_host(outs);
    }
    assert_eq!(h.host.dropped(), 5000);
    assert_eq!(h.host_log.len() - logged_before, 1, "one line, however many frames");
    // a payload the player chose is cut before it reaches a log line
    h.player_game(a, MessageKind::Hello, &vec![b'h'; 900_000]);
    h.player_game(a, MessageKind::ReplayFailed, &json(&ReplayFailedMessage { detail: "d".repeat(900_000) }));
    assert!(h.host_log.iter().all(|l| l.len() < 500), "no log line is longer than a screen");
    // so is a version string in a reject
    let reject = join_announcing(&mut h, 70_000, &"9".repeat(4000)).expect("rejected");
    assert!(reject.detail.unwrap().len() < 100);
}

#[test]
fn the_drop_memory_is_bounded_even_by_many_connections() {
    let mut h = Harness::new();
    h.connect("A");
    // each stray connection sends one frame that is not a Join: a different line each
    for i in 0..5000u64 {
        let id = 100_000 + i;
        h.host.connection_opened(id);
        let frame = Frame::new(FrameHeader::new(1, MessageKind::Chat.into(), 0xFFFF, 1), vec![1; 20]);
        h.host.frame_received(id, &frame);
        h.host.connection_closed(id);
    }
    assert_eq!(h.host.dropped(), 5000);
    // not observable directly; the point is that this finishes with a bounded footprint, which the allocation
    // test of robustness_tests.rs measures. Here: the host still works.
    let b = h.connect("B");
    assert_eq!(h.p(b).session.state(), PlayerState::Joined);
}

#[test]
fn a_second_welcome_does_not_restart_the_players_counter() {
    let mut h = Harness::new();
    let c = h.connect("Pat");
    let me = h.p(c).session.id().unwrap();
    let counter_of = |outs: &[PlayerOutput]| {
        outs.iter()
            .find_map(|o| match o {
                PlayerOutput::SendToHost(f) => Some((f.header.sender, f.header.counter)),
                _ => None,
            })
            .expect("a frame")
    };
    let first = counter_of(&h.players.get_mut(&c).unwrap().session.local_game_message(MessageKind::Hello, b"{}"));

    // an authentic second Welcome (the host's key, a fresh counter) would have replaced the sealer
    let welcome = WelcomeMessage {
        you: 40,
        host_name: "Hote".into(),
        version: "0.1".into(),
        players: vec![],
        session: SessionState {
            started: false,
            speed: 0,
            pause_at: None,
            lead: 1200,
            resync: None,
            mod_warnings: Vec::new(),
            stalled: Vec::new(),
        },
        nonce: vec![],
        resume: None,
        resumed: false,
        mods_checked: false,
        catch_up: None,
    };
    let frame = sealed(&h, HostSession::HOST_ID, 100_000, MessageKind::Welcome, &json(&welcome));
    let session = &mut h.players.get_mut(&c).unwrap().session;
    let outs = session.frame_received(&frame);
    assert!(outs.iter().any(|o| matches!(o, PlayerOutput::Log(l) if l.contains("second welcome"))));
    assert_eq!(session.id(), Some(me), "the id the host gave stays");
    let second = counter_of(&session.local_game_message(MessageKind::Hello, b"{}"));
    assert_eq!(second, (me, first.1 + 1), "the counter went on");
}

#[test]
fn an_injected_reject_does_not_end_a_joined_session() {
    let mut h = Harness::new();
    let c = h.connect("Pat");
    let reject = Frame::new(
        FrameHeader::new(1, MessageKind::Reject.into(), HostSession::UNASSIGNED, 0),
        json(&RejectMessage::new("bad-proof", None)),
    );
    let session = &mut h.players.get_mut(&c).unwrap().session;
    let dropped = session.dropped();
    let outs = session.frame_received(&reject);
    assert!(!outs.iter().any(|o| matches!(o, PlayerOutput::Rejected(_))));
    assert_eq!(session.state(), PlayerState::Joined);
    assert_eq!(session.dropped(), dropped + 1);
    assert!(!session.local_game_message(MessageKind::Hello, b"{}").is_empty(), "and the player still speaks");
}

#[test]
fn a_challenge_with_a_nonce_of_the_wrong_size_is_not_answered() {
    let key = SessionKey::derive(&SECRET, None);
    for size in [0usize, 1, 15, 17, 64, 100_000] {
        let mut player = PlayerSession::new(key.clone(), "Pat", "0.1");
        let challenge = ChallengeMessage { nonce: vec![9; size], protocol: 1, version: "0.1".into() };
        let header = FrameHeader::new(1, MessageKind::Challenge.into(), HostSession::UNASSIGNED, 0);
        let outs = player.frame_received(&Frame::new(header, json(&challenge)));
        assert!(!outs.iter().any(|o| matches!(o, PlayerOutput::SendToHost(_))), "nonce of {size} bytes");
    }
}

#[test]
fn no_nonce_is_used_twice_however_players_come_and_go() {
    let mut h = Harness::with(HostOptions { max_players: 100, ..HostOptions::default() });
    let mut seen: HashSet<(u16, u64)> = HashSet::new();
    let mut ids: Vec<u16> = Vec::new();
    let mut live: Vec<ConnectionId> = Vec::new();
    for round in 0..40u32 {
        let c = h.connect(&format!("P{round}"));
        ids.push(h.p(c).session.id().expect("welcomed"));
        live.push(c);
        for c in live.clone() {
            let outs = h.players.get_mut(&c).unwrap().session.local_game_message(MessageKind::Hello, b"{}");
            for o in &outs {
                if let PlayerOutput::SendToHost(f) = o {
                    // a repeat of a sealed (sender, counter) is a repeat of a nonce
                    assert!(f.header.counter > 0);
                    assert!(seen.insert((f.header.sender, f.header.counter)), "player frame repeated: {:?}", f.header);
                }
            }
            h.pump_player(c, outs);
        }
        h.host_game(MessageKind::Chat, &json(&ChatMessage::new("hello everybody")));
        if round % 3 == 2 {
            let gone = live.remove(0);
            h.disconnect(gone);
        }
    }
    let unique_ids: HashSet<u16> = ids.iter().copied().collect();
    assert_eq!(unique_ids.len(), ids.len(), "the host never gives an id twice");
    let mut host_frames = 0;
    for e in &h.events {
        if let HostOutput::SendToConnection { frame, .. } = e {
            if frame.header.sender == HostSession::HOST_ID {
                host_frames += 1;
                assert!(
                    seen.insert((frame.header.sender, frame.header.counter)),
                    "host counter {}",
                    frame.header.counter
                );
            }
        }
    }
    assert!(host_frames > 100, "the scenario sent a real number of frames");
    let player_frames = seen.iter().filter(|(s, _)| *s != HostSession::HOST_ID).count();
    assert!(player_frames > 100);
}

#[test]
fn a_connection_over_its_byte_budget_is_closed_and_the_others_are_not() {
    // few big frames do what many small ones are stopped from doing: a stranger's disk and CPU are the cost
    let options = HostOptions { max_bytes_per_second: 10_000, byte_burst: 40_000, ..HostOptions::default() };
    let (mut h, ids) = Harness::running(2, 1, options);
    let (flooder, other) = (ids[0], ids[1]);
    let chat = json(&ChatMessage::new(&"x".repeat(5_000)));
    let mut sent = 0;
    while h.routable.contains(&flooder) && sent < 100 {
        h.player_game(flooder, MessageKind::Chat, &chat);
        sent += 1;
    }
    assert!(!h.routable.contains(&flooder), "closed");
    assert!((6..=10).contains(&sent), "after about 40 000 bytes at once, not after {sent} frames");
    assert!(h.host_log.iter().any(|l| l.contains("bytes per second")), "{:?}", h.host_log);
    assert!(h.routable.contains(&other), "nobody else is touched");
}

#[test]
fn traffic_inside_the_byte_rate_is_never_refused_and_zero_turns_it_off() {
    let options = HostOptions { max_bytes_per_second: 10_000, byte_burst: 40_000, ..HostOptions::default() };
    let (mut h, ids) = Harness::running(1, 1, options);
    let chat = json(&ChatMessage::new(&"x".repeat(5_000)));
    for _ in 0..60 {
        h.player_game(ids[0], MessageKind::Chat, &chat);
        h.advance(1.0); // 5 000 bytes a second against 10 000
    }
    assert!(h.routable.contains(&ids[0]));

    let off = HostOptions { max_bytes_per_second: 0, ..HostOptions::default() };
    let (mut h, ids) = Harness::running(1, 1, off);
    for _ in 0..200 {
        h.player_game(ids[0], MessageKind::Chat, &chat);
    }
    assert!(h.routable.contains(&ids[0]), "no byte limit, and 200 frames are inside the frame limit");
}
