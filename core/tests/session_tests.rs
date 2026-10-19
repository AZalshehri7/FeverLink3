use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use feverlink3_core::crypto::{Sealer, SessionKey};
use feverlink3_core::protocol::Frame;
use feverlink3_core::session::*;
use serde::de::DeserializeOwned;
use serde::Serialize;

const SECRET: [u8; 16] = [7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22];

fn json<T: Serialize>(m: &T) -> Vec<u8> {
    codec::encode(m)
}

/// One player of the harness: its session plus everything it produced.
struct Player {
    session: PlayerSession,
    game: Vec<(MessageKind, Vec<u8>)>,
    log: Vec<String>,
    reject: Option<RejectMessage>,
}

impl Player {
    fn last<T: DeserializeOwned>(&self, kind: MessageKind) -> T {
        let (_, payload) = self.game.iter().rev().find(|g| g.0 == kind).unwrap_or_else(|| panic!("no {kind:?}"));
        codec::try_decode(payload).unwrap_or_else(|| panic!("{kind:?} decodes"))
    }

    fn all<T: DeserializeOwned>(&self, kind: MessageKind) -> Vec<T> {
        self.game.iter().filter(|g| g.0 == kind).filter_map(|g| codec::try_decode(&g.1)).collect()
    }

    fn count(&self, kind: MessageKind) -> usize {
        self.game.iter().filter(|g| g.0 == kind).count()
    }
}

/// A host and players wired together in memory, with a clock the test moves.
struct Harness {
    key: SessionKey,
    now: Arc<Mutex<f64>>,
    host: HostSession,
    host_game: Vec<(MessageKind, Vec<u8>)>,
    host_log: Vec<String>,
    closed: Vec<ConnectionId>,
    outcomes: Vec<ChecksumOutcome>,
    players: BTreeMap<ConnectionId, Player>,
    /// Connections the host may still reach; a close removes one here but the test keeps the `Player`.
    routable: BTreeSet<ConnectionId>,
    next_connection: ConnectionId,
}

impl Harness {
    fn new() -> Self {
        Self::with("Hote", HostOptions::default())
    }

    fn with(host_name: &str, options: HostOptions) -> Self {
        let key = SessionKey::derive(&SECRET, None);
        let now = Arc::new(Mutex::new(0.0));
        let clock = Arc::clone(&now);
        let host = HostSession::new(key.clone(), host_name, Box::new(move || *clock.lock().unwrap()), options);
        Self {
            key,
            now,
            host,
            host_game: Vec::new(),
            host_log: Vec::new(),
            closed: Vec::new(),
            outcomes: Vec::new(),
            players: BTreeMap::new(),
            routable: BTreeSet::new(),
            next_connection: 0,
        }
    }

    fn advance(&mut self, seconds: f64) {
        *self.now.lock().unwrap() += seconds;
    }

    fn connect(&mut self, name: &str) -> ConnectionId {
        self.connect_with(name, None, "0.1")
    }

    fn connect_with(&mut self, name: &str, key: Option<SessionKey>, version: &str) -> ConnectionId {
        self.next_connection += 1;
        let c = self.next_connection;
        let session = PlayerSession::new(key.unwrap_or_else(|| self.key.clone()), name, version);
        self.players.insert(c, Player { session, game: Vec::new(), log: Vec::new(), reject: None });
        self.routable.insert(c);
        let outs = self.host.connection_opened(c);
        self.pump_host(outs);
        c
    }

    fn disconnect(&mut self, c: ConnectionId) {
        self.routable.remove(&c);
        let outs = self.host.connection_closed(c);
        self.pump_host(outs);
    }

    fn p(&self, c: ConnectionId) -> &Player {
        &self.players[&c]
    }

    /// `player.Session.LocalGameMessage(...)` with the frames routed to the host.
    fn player_game(&mut self, c: ConnectionId, kind: MessageKind, payload: &[u8]) {
        let outs = self.players.get_mut(&c).unwrap().session.local_game_message(kind, payload);
        self.pump_player(c, outs);
    }

    /// `Host.FrameReceived(connection, frame)` with the replies routed back.
    fn host_frame(&mut self, c: ConnectionId, frame: &Frame) {
        let outs = self.host.frame_received(c, frame);
        self.pump_host(outs);
    }

    /// `Host.LocalGameMessage(...)` with the frames routed to the players.
    fn host_game(&mut self, kind: MessageKind, payload: &[u8]) {
        let outs = self.host.local_game_message(kind, payload);
        self.pump_host(outs);
    }

    /// Any other host call (`Start`, `SetSpeed`, `Pause`, `Tick`).
    fn host_call(&mut self, call: impl FnOnce(&mut HostSession) -> Vec<HostOutput>) {
        let outs = call(&mut self.host);
        self.pump_host(outs);
    }

    fn pump_host(&mut self, outs: Vec<HostOutput>) {
        for o in outs {
            match o {
                HostOutput::SendToConnection { connection, frame } => {
                    if self.routable.contains(&connection) {
                        let outs = self.players.get_mut(&connection).unwrap().session.frame_received(&frame);
                        self.pump_player(connection, outs);
                    }
                }
                HostOutput::CloseConnection(c) => {
                    self.closed.push(c);
                    self.routable.remove(&c);
                }
                HostOutput::DeliverToLocalGame { kind, payload } => self.host_game.push((kind, payload)),
                HostOutput::Log(line) => self.host_log.push(line),
                HostOutput::ChecksumCompleted(o) => self.outcomes.push(o),
                HostOutput::Received { .. }
                | HostOutput::PlayerJoined(_)
                | HostOutput::PlayerLeft(_)
                | HostOutput::ResyncRequested { .. }
                | HostOutput::ResyncProgress { .. }
                | HostOutput::ResyncFailed { .. } => {}
            }
        }
    }

    fn pump_player(&mut self, c: ConnectionId, outs: Vec<PlayerOutput>) {
        for o in outs {
            match o {
                PlayerOutput::SendToHost(frame) => {
                    let outs = self.host.frame_received(c, &frame);
                    self.pump_host(outs);
                }
                PlayerOutput::DeliverToLocalGame { kind, payload } => {
                    self.players.get_mut(&c).unwrap().game.push((kind, payload));
                }
                PlayerOutput::Log(line) => self.players.get_mut(&c).unwrap().log.push(line),
                PlayerOutput::Rejected(r) => self.players.get_mut(&c).unwrap().reject = Some(r),
                PlayerOutput::Welcomed(_) | PlayerOutput::SaveReceived { .. } => {}
            }
        }
    }

    fn host_game_last<T: DeserializeOwned>(&self, kind: MessageKind) -> T {
        let (_, payload) = self.host_game.iter().rev().find(|g| g.0 == kind).unwrap_or_else(|| panic!("no {kind:?}"));
        codec::try_decode(payload).unwrap()
    }

    fn host_game_count(&self, kind: MessageKind) -> usize {
        self.host_game.iter().filter(|g| g.0 == kind).count()
    }
}

fn parts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[test]
fn player_with_the_key_is_welcomed_and_everyone_learns_about_it() {
    let mut h = Harness::new();
    let a = h.connect("Alice");
    assert_eq!(PlayerState::Joined, h.p(a).session.state());
    assert_eq!(Some(1), h.p(a).session.id());
    assert_eq!(Some("Hote"), h.p(a).session.host_name());
    let w: WelcomeMessage = h.p(a).last(MessageKind::Welcome);
    assert_eq!(vec!["Hote", "Alice"], w.players.iter().map(|p| p.name.as_str()).collect::<Vec<_>>());

    let b = h.connect("Bob");
    assert_eq!(Some(2), h.p(b).session.id());
    assert_eq!("Bob", h.p(a).last::<PlayerJoinedMessage>(MessageKind::PlayerJoined).player.name);
    assert_eq!("Bob", h.host_game_last::<PlayerJoinedMessage>(MessageKind::PlayerJoined).player.name);
    assert_eq!(3, h.host.players().len());
    assert!(h.closed.is_empty());
}

#[test]
fn wrong_key_is_rejected_and_the_connection_closed() {
    let mut h = Harness::new();
    let intruder = h.connect_with("Mallory", Some(SessionKey::derive(&SECRET, Some("guess"))), "0.1");
    assert_eq!(PlayerState::Rejected, h.p(intruder).session.state());
    assert_eq!("bad-proof", h.p(intruder).reject.as_ref().unwrap().reason);
    assert_eq!(vec![intruder], h.closed);
    assert_eq!(1, h.host.players().len()); // the host game only
}

#[test]
fn other_version_is_rejected_with_both_versions_named() {
    let mut h = Harness::new();
    let old = h.connect_with("Old", None, "0.0");
    let reject = h.p(old).reject.clone().unwrap();
    assert_eq!("version", reject.reason);
    let detail = reject.detail.unwrap();
    assert!(detail.contains("0.1"));
    assert!(detail.contains("0.0"));
}

#[test]
fn session_is_full_at_max_players_and_the_host_counts() {
    let mut h = Harness::with("Hote", HostOptions { max_players: 3, ..HostOptions::default() });
    let a = h.connect("A");
    let b = h.connect("B");
    let c = h.connect("C");
    assert_eq!(PlayerState::Joined, h.p(a).session.state());
    assert_eq!(PlayerState::Joined, h.p(b).session.state());
    let reject = h.p(c).reject.as_ref().unwrap();
    assert_eq!("full", reject.reason);
    assert_eq!(Some("3 players"), reject.detail.as_deref());
    assert_eq!(3, h.host.players().len());
}

#[test]
fn duplicate_and_dirty_names_become_unique_and_clean() {
    let mut h = Harness::with("Hote", HostOptions::default());
    let a = h.connect("Hote");
    let b = h.connect("Al ice\t\n!");
    let c = h.connect("Alice");
    assert_eq!(Some("Hote#1"), h.p(a).session.assigned_name());
    assert_eq!(Some("Alice"), h.p(b).session.assigned_name());
    assert_eq!(Some("Alice#3"), h.p(c).session.assigned_name());
}

#[test]
fn frame_carrying_another_players_sender_id_is_dropped() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    let mut forged_as_b = Sealer::new(&h.key, h.p(b).session.id().unwrap());
    let frame = forged_as_b.seal(MessageKind::Chat.into(), &json(&ChatMessage::new("I am B")));
    h.host_frame(a, &frame); // sent over A's connection
    assert_eq!(1, h.host.dropped());
    assert_eq!(0, h.p(b).count(MessageKind::Chat));
    assert_eq!(0, h.host_game_count(MessageKind::Chat));
}

#[test]
fn host_only_kinds_from_a_player_are_dropped_and_never_relayed() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    let before = h.p(b).game.len();
    let state = SessionState {
        started: true,
        speed: 4,
        pause_at: None,
        lead: 0,
        resync: None,
        mod_warnings: Vec::new(),
        stalled: Vec::new(),
    };
    h.player_game(a, MessageKind::Session, &json(&state));
    let load = ResyncLoadMessage { id: 1, name: "evil".into(), sha256: "00".into() };
    h.player_game(a, MessageKind::ResyncLoad, &json(&load));
    let request = ChecksumRequestMessage::new(9, 0, false, true, 0);
    h.player_game(a, MessageKind::ChecksumRequest, &json(&request));
    assert_eq!(3, h.host.dropped());
    assert_eq!(before, h.p(b).game.len());
    assert!(!h.host.state().started);
    assert!(h.host_log.iter().any(|l| l.contains("host only")));
}

#[test]
fn replayed_frame_is_dropped() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let outs =
        h.players.get_mut(&a).unwrap().session.local_game_message(MessageKind::Chat, &json(&ChatMessage::new("hi")));
    let captured = outs
        .iter()
        .find_map(|o| match o {
            PlayerOutput::SendToHost(f) => Some(f.clone()),
            _ => None,
        })
        .expect("the chat frame");
    h.pump_player(a, outs);
    h.host_frame(a, &captured);
    assert_eq!(1, h.host.dropped());
    assert_eq!(1, h.host_game_count(MessageKind::Chat));
}

#[test]
fn action_is_relayed_to_everyone_but_the_sender_with_its_origin_filled_in() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    let c = h.connect("C");
    h.player_game(a, MessageKind::Action, &json(&ActionMessage::new(30000, 1, "{road}")));
    h.player_game(a, MessageKind::Action, &json(&ActionMessage::new(30200, 2, "{buy}")));

    assert_eq!(0, h.p(a).count(MessageKind::Action));
    assert_eq!(2, h.p(b).count(MessageKind::Action));
    assert_eq!(2, h.p(c).count(MessageKind::Action));
    let host_acts: Vec<ActionMessage> =
        h.host_game.iter().filter(|g| g.0 == MessageKind::Action).map(|g| codec::try_decode(&g.1).unwrap()).collect();
    assert_eq!(vec![1, 2], host_acts.iter().map(|x| x.seq).collect::<Vec<_>>());
    let a_id = h.p(a).session.id().unwrap();
    assert!(host_acts.iter().all(|x| x.from == a_id));
    assert_eq!("{buy}", h.p(b).last::<ActionMessage>(MessageKind::Action).body);
    assert_eq!(2, h.host.actions_relayed());
}

#[test]
fn host_game_action_reaches_every_player() {
    let mut h = Harness::new();
    let a = h.connect("A");
    h.host_game(MessageKind::Action, &json(&ActionMessage::new(1000, 5, "{x}")));
    let got: ActionMessage = h.p(a).last(MessageKind::Action);
    assert_eq!(HostSession::HOST_ID, got.from);
    assert_eq!(0, h.host_game_count(MessageKind::Action)); // not echoed to itself
}

/// The paused actions a game received, in its order: (from, seq, pause_seq).
fn paused_seen(game: &[(MessageKind, Vec<u8>)]) -> Vec<(u16, i64, u64)> {
    game.iter()
        .filter(|g| g.0 == MessageKind::Action)
        .filter_map(|g| codec::try_decode::<ActionMessage>(&g.1))
        .filter_map(|m| m.pause_seq.map(|n| (m.from, m.seq, n)))
        .collect()
}

#[test]
fn actions_made_during_a_pause_are_numbered_in_one_order_and_sent_to_every_game_the_sender_included() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    let (ia, ib) = (h.p(a).session.id().unwrap(), h.p(b).session.id().unwrap());
    // two players and the host act in one pause, interleaved
    h.player_game(a, MessageKind::Action, &json(&ActionMessage::new(30_000, 1, "{a1}").paused()));
    h.player_game(b, MessageKind::Action, &json(&ActionMessage::new(30_000, 1, "{b1}").paused()));
    h.host_game(MessageKind::Action, &json(&ActionMessage::new(30_000, 1, "{h1}").paused()));
    h.player_game(a, MessageKind::Action, &json(&ActionMessage::new(30_000, 2, "{a2}").paused()));
    let want = vec![(ia, 1, 1), (ib, 1, 2), (HostSession::HOST_ID, 1, 3), (ia, 2, 4)];
    assert_eq!(want, paused_seen(&h.p(a).game), "the sender gets its own back, numbered");
    assert_eq!(want, paused_seen(&h.p(b).game));
    assert_eq!(want, paused_seen(&h.host_game), "the host's game too");
    // an action that is not a pause action keeps the old relay: everybody but its sender, no number
    h.player_game(a, MessageKind::Action, &json(&ActionMessage::new(31_000, 3, "{a3}")));
    let last: ActionMessage = h.p(b).last(MessageKind::Action);
    assert_eq!((3, None), (last.seq, last.pause_seq));
    assert_eq!(4, h.p(a).count(MessageKind::Action));
}

#[test]
fn a_players_own_pause_number_is_overwritten_and_a_paused_commit_is_numbered_in_the_same_order() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    let mut forged = ActionMessage::new(30_000, 1, "{x}").paused();
    forged.pause_seq = Some(99);
    h.player_game(a, MessageKind::Action, &json(&forged));
    let mut sneaky = ActionMessage::new(31_000, 2, "{y}");
    sneaky.pause_seq = Some(7);
    h.player_game(a, MessageKind::Action, &json(&sneaky));
    assert_eq!(
        Some(1),
        h.p(b)
            .game
            .iter()
            .filter(|g| g.0 == MessageKind::Action)
            .find_map(|g| codec::try_decode::<ActionMessage>(&g.1))
            .unwrap()
            .pause_seq
    );
    assert_eq!(None, h.p(b).last::<ActionMessage>(MessageKind::Action).pause_seq, "no pause flag: no number");
    let commit = BuildCommitMessage {
        seq: 1,
        owner: 2,
        stamp: 30_000,
        from: 0,
        pause: true,
        pause_seq: Some(50),
        pause_upto: None,
    };
    h.player_game(b, MessageKind::BuildCommit, &json(&commit));
    let got: BuildCommitMessage = h.p(b).last(MessageKind::BuildCommit);
    assert_eq!(Some(2), got.pause_seq, "the commit takes the next number, and comes back to its sender");
    assert_eq!(Some(2), h.p(a).last::<BuildCommitMessage>(MessageKind::BuildCommit).pause_seq);
    let plain = BuildCommitMessage {
        seq: 2,
        owner: 2,
        stamp: 30_000,
        from: 0,
        pause: false,
        pause_seq: None,
        pause_upto: None,
    };
    h.player_game(b, MessageKind::BuildCommit, &json(&plain));
    assert_eq!(1, h.p(b).count(MessageKind::BuildCommit), "a commit outside a pause is not echoed");
}

#[test]
fn the_resume_reaches_every_game_after_every_paused_action_numbered_before_it() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    let clock =
        ClockMessage { t: 18_600, speed: 0, ahead: 3, need: None, safe: None, pause: false, stop: None, hop: None };
    h.host_game(MessageKind::Clock, &json(&clock));
    h.player_game(a, MessageKind::Clock, &json(&clock));
    h.player_game(b, MessageKind::Clock, &json(&clock));
    h.host_call(HostSession::start);
    assert_eq!(Some(18_600), h.host.state().pause_at, "held at the start: a pause");
    h.player_game(a, MessageKind::Action, &json(&ActionMessage::new(18_600, 1, "{a1}").paused()));
    h.player_game(a, MessageKind::SpeedRequest, &json(&SpeedRequestMessage { speed: 1 }));
    // one that left its game during the pause and reached the host after the resume: still numbered, after the resume line
    h.player_game(b, MessageKind::Action, &json(&ActionMessage::new(18_600, 1, "{b1}").paused()));
    for game in [&h.p(a).game, &h.p(b).game, &h.host_game] {
        let order: Vec<String> = game
            .iter()
            .filter_map(|g| match g.0 {
                MessageKind::Action => {
                    codec::try_decode::<ActionMessage>(&g.1).map(|m| format!("#{}", m.pause_seq.unwrap()))
                }
                MessageKind::Session => codec::try_decode::<SessionState>(&g.1)
                    .filter(|s| s.started && s.pause_at.is_none())
                    .map(|_| "resume".to_owned()),
                _ => None,
            })
            .collect();
        let from = order.iter().position(|x| x == "#1").expect("#1 arrives");
        assert_eq!(vec!["#1", "resume", "#2"], order[from..from + 3].to_vec());
    }
}

/// A clock of a game standing still at a pause (its promise is the pause, `stop` where it really stands).
fn stopped(t: i64, pause_at: i64) -> Vec<u8> {
    json(&ClockMessage {
        t,
        speed: 0,
        ahead: 3,
        need: None,
        safe: Some(pause_at),
        pause: true,
        stop: Some(t),
        hop: None,
    })
}

/// Three ready games (the host's and two players'), running at x3, then paused; returns the players and the pause stamp.
fn three_paused() -> (Harness, ConnectionId, ConnectionId, i64) {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    for c in [a, b] {
        h.player_game(c, MessageKind::Hello, &json(&HelloMessage::new(30_000)));
    }
    h.host_game(MessageKind::Hello, &json(&HelloMessage::new(30_000)));
    let running = |t: i64| {
        json(&ClockMessage { t, speed: 3, ahead: 3, need: None, safe: None, pause: false, stop: None, hop: None })
    };
    h.host_game(MessageKind::Clock, &running(30_000));
    h.player_game(a, MessageKind::Clock, &running(30_000));
    h.player_game(b, MessageKind::Clock, &running(30_000));
    h.host_call(HostSession::start);
    h.host_call(|host| host.set_speed(3));
    h.host_call(HostSession::pause);
    let p = h.host.state().pause_at.unwrap();
    (h, a, b, p)
}

fn paused_actions(game: &[(MessageKind, Vec<u8>)]) -> Vec<(i64, i64, u64)> {
    game.iter()
        .filter(|g| g.0 == MessageKind::Action)
        .filter_map(|g| codec::try_decode::<ActionMessage>(&g.1))
        .filter_map(|m| m.pause_seq.map(|n| (m.seq, m.stamp, n)))
        .collect()
}

#[test]
fn an_action_of_the_pause_is_stamped_past_the_furthest_place_a_game_really_stands() {
    let (mut h, a, b, p) = three_paused();
    // speed 0 takes effect late, by a different amount on each game (real run pause3: 3 steps at x3)
    h.host_game(MessageKind::Clock, &stopped(p, p));
    h.player_game(a, MessageKind::Clock, &stopped(p + 2 * Lockstep::STEP, p));
    h.player_game(a, MessageKind::Action, &json(&ActionMessage::new(p, 1, "{a1}").paused()));
    assert_eq!(
        Vec::<(i64, i64, u64)>::new(),
        paused_actions(&h.p(b).game),
        "held: B has not said where it stands (it may still move)"
    );
    // a stop that is not the clock's own time counts for nothing
    let mut odd = ClockMessage {
        t: p + 600,
        speed: 0,
        ahead: 3,
        need: None,
        safe: Some(p),
        pause: true,
        stop: Some(p),
        hop: None,
    };
    h.player_game(b, MessageKind::Clock, &json(&odd));
    assert!(paused_actions(&h.p(b).game).is_empty());
    odd.stop = Some(p + 600);
    h.player_game(b, MessageKind::Clock, &json(&odd));
    let first = p + 600 + Lockstep::STEP;
    for game in [&h.p(a).game, &h.p(b).game, &h.host_game] {
        assert_eq!(
            vec![(1, first, 1)],
            paused_actions(game),
            "one step past the furthest stop, to every game, the sender too"
        );
    }
    // the next one waits until every game stands still at or past that stamp again (they run on to apply it)
    h.player_game(b, MessageKind::Action, &json(&ActionMessage::new(p, 1, "{b1}").paused()));
    assert_eq!(1, paused_actions(&h.p(a).game).len());
    h.host_game(MessageKind::Clock, &stopped(first, p));
    h.player_game(a, MessageKind::Clock, &stopped(first + 3 * Lockstep::STEP, p));
    h.player_game(b, MessageKind::Clock, &stopped(first + Lockstep::STEP, p));
    let second = first + 4 * Lockstep::STEP;
    for game in [&h.p(a).game, &h.p(b).game, &h.host_game] {
        assert_eq!(vec![(1, first, 1), (1, second, 2)], paused_actions(game));
    }
}

#[test]
fn every_item_of_one_flush_of_the_pause_order_names_the_last_one_of_it() {
    let (mut h, a, b, p) = three_paused();
    h.host_game(MessageKind::Clock, &stopped(p, p));
    h.player_game(a, MessageKind::Clock, &stopped(p, p));
    h.player_game(a, MessageKind::Action, &json(&ActionMessage::new(p, 1, "{a1}").paused()));
    h.player_game(a, MessageKind::Action, &json(&ActionMessage::new(p, 2, "{a2}").paused()));
    h.player_game(b, MessageKind::Clock, &stopped(p, p)); // the flush: both at once
    h.player_game(b, MessageKind::Action, &json(&ActionMessage::new(p, 1, "{b1}").paused()));
    h.host_game(MessageKind::Clock, &stopped(p + 2 * Lockstep::STEP, p));
    h.player_game(a, MessageKind::Clock, &stopped(p + 2 * Lockstep::STEP, p));
    h.player_game(b, MessageKind::Clock, &stopped(p + 2 * Lockstep::STEP, p)); // a flush of one
    let marks: Vec<(Option<u64>, Option<u64>)> = h
        .p(b)
        .game
        .iter()
        .filter(|g| g.0 == MessageKind::Action)
        .filter_map(|g| codec::try_decode::<ActionMessage>(&g.1))
        .map(|m| (m.pause_seq, m.pause_upto))
        .collect();
    assert_eq!(vec![(Some(1), Some(2)), (Some(2), Some(2)), (Some(3), Some(3))], marks);
}

#[test]
fn a_checksum_round_at_a_pause_is_stamped_where_the_furthest_game_stands() {
    let (mut h, a, b, p) = three_paused();
    h.host_game(MessageKind::Clock, &stopped(p, p));
    h.player_game(a, MessageKind::Clock, &stopped(p + 400, p));
    h.host_call(HostSession::request_checksum);
    let paused_rounds = |h: &Harness| {
        h.p(b).all::<ChecksumRequestMessage>(MessageKind::ChecksumRequest).into_iter().filter(|r| r.paused).count()
    };
    assert_eq!(0, paused_rounds(&h), "waits until every game said where it stands");
    h.player_game(b, MessageKind::Clock, &stopped(p + 200, p));
    let round = h
        .p(b)
        .all::<ChecksumRequestMessage>(MessageKind::ChecksumRequest)
        .into_iter()
        .find(|r| r.paused)
        .expect("the round");
    assert_eq!(p + 400, round.at);
}

#[test]
fn an_action_of_the_pause_still_held_at_the_resume_gets_a_normal_stamp_after_the_resume_line() {
    let (mut h, a, b, p) = three_paused();
    h.host_game(MessageKind::Clock, &stopped(p, p));
    h.player_game(a, MessageKind::Action, &json(&ActionMessage::new(p, 1, "{a1}").paused()));
    h.host_call(|host| host.set_speed(3));
    let got = paused_actions(&h.p(b).game);
    assert_eq!(1, got.len());
    assert!(
        got[0].1 >= h.host.future_stamp() - Lockstep::STEP && got[0].1 > p,
        "a stamp past every clock and promise: {got:?}"
    );
    let kinds: Vec<MessageKind> =
        h.p(a).game.iter().map(|g| g.0).filter(|k| matches!(k, MessageKind::Action | MessageKind::Session)).collect();
    let resume = kinds.iter().rposition(|k| *k == MessageKind::Session).unwrap();
    let action = kinds.iter().rposition(|k| *k == MessageKind::Action).unwrap();
    assert!(action > resume, "after the resume line");
}

#[test]
fn clock_becomes_a_peer_clock_for_the_others_and_feeds_the_stamp() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    h.player_game(
        a,
        MessageKind::Clock,
        &json(&ClockMessage {
            t: 50000,
            speed: 1,
            ahead: 3,
            need: None,
            safe: None,
            pause: false,
            stop: None,
            hop: None,
        }),
    );
    let pc: PeerClockMessage = h.p(b).last(MessageKind::PeerClock);
    assert_eq!(h.p(a).session.id().unwrap(), pc.from);
    assert_eq!(50000, pc.t);
    assert_eq!(0, h.p(a).count(MessageKind::PeerClock));
    assert_eq!(50000, h.host.max_clock());
    h.advance(2.0); // two real seconds at x1 = 2000 units
    assert_eq!(52000, h.host.max_clock());
    assert_eq!(Lockstep::round_up_to_step((52000 + Lockstep::lead_units(1)) as f64), h.host.future_stamp());
}

#[test]
fn a_measured_stamp_margin_is_relayed_with_the_clock_and_clamped() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    h.player_game(
        a,
        MessageKind::Clock,
        &json(&ClockMessage {
            t: 49800,
            speed: 1,
            ahead: 3,
            need: None,
            safe: None,
            pause: false,
            stop: None,
            hop: None,
        }),
    );
    assert_eq!(None, h.p(b).last::<PeerClockMessage>(MessageKind::PeerClock).need, "nothing measured: none relayed");
    h.player_game(
        a,
        MessageKind::Clock,
        &json(&ClockMessage {
            t: 50000,
            speed: 1,
            ahead: 3,
            need: Some(2),
            safe: None,
            pause: false,
            stop: None,
            hop: None,
        }),
    );
    assert_eq!(Some(2), h.p(b).last::<PeerClockMessage>(MessageKind::PeerClock).need);
    h.player_game(
        a,
        MessageKind::Clock,
        &json(&ClockMessage {
            t: 50200,
            speed: 1,
            ahead: 3,
            need: Some(1_000_000),
            safe: None,
            pause: false,
            stop: None,
            hop: None,
        }),
    );
    assert_eq!(Some(ClockMessage::MAX_NEED), h.p(b).last::<PeerClockMessage>(MessageKind::PeerClock).need);
    h.player_game(
        a,
        MessageKind::Clock,
        &json(&ClockMessage {
            t: 50400,
            speed: 1,
            ahead: 3,
            need: Some(-5),
            safe: None,
            pause: false,
            stop: None,
            hop: None,
        }),
    );
    assert_eq!(Some(0), h.p(b).last::<PeerClockMessage>(MessageKind::PeerClock).need);
}

#[test]
fn the_input_barriers_promise_is_relayed_after_the_actions_shipped_before_it_and_bounded() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    let clock = |t: i64, safe: Option<i64>, pause: bool| ClockMessage {
        t,
        speed: 1,
        ahead: 3,
        need: None,
        safe,
        pause,
        stop: None,
        hop: None,
    };
    // an action, then the clock whose promise covers it: the other game gets them in that order (the barrier counts on it)
    h.player_game(a, MessageKind::Action, &json(&ActionMessage::new(51_400, 1, "{x}")));
    h.player_game(a, MessageKind::Clock, &json(&clock(50_000, Some(51_400), false)));
    let kinds: Vec<MessageKind> =
        h.p(b).game.iter().map(|g| g.0).filter(|k| matches!(k, MessageKind::Action | MessageKind::PeerClock)).collect();
    assert_eq!(vec![MessageKind::Action, MessageKind::PeerClock], kinds);
    let pc: PeerClockMessage = h.p(b).last(MessageKind::PeerClock);
    assert_eq!((Some(51_400), false), (pc.safe, pc.pause));
    // a pause stop is relayed with its flag
    h.player_game(a, MessageKind::Clock, &json(&clock(52_000, Some(52_000), true)));
    let pc: PeerClockMessage = h.p(b).last(MessageKind::PeerClock);
    assert_eq!((Some(52_000), true), (pc.safe, pc.pause));
    // a promise far past its clock would let the others run into steps it may still stamp: cut down; one below it only holds them more
    h.player_game(a, MessageKind::Clock, &json(&clock(52_200, Some(i64::MAX), false)));
    assert_eq!(
        Some(52_200 + ClockMessage::MAX_SAFE_AHEAD),
        h.p(b).last::<PeerClockMessage>(MessageKind::PeerClock).safe
    );
    h.player_game(a, MessageKind::Clock, &json(&clock(52_400, Some(-7), false)));
    assert_eq!(Some(-7), h.p(b).last::<PeerClockMessage>(MessageKind::PeerClock).safe);
    // none: an older mod, none relayed
    h.player_game(a, MessageKind::Clock, &json(&clock(52_600, None, true)));
    let pc: PeerClockMessage = h.p(b).last(MessageKind::PeerClock);
    assert_eq!((None, false), (pc.safe, pc.pause));
}

#[test]
fn a_pause_or_checksum_stamp_is_never_before_a_promise_a_game_made() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let clock = |t: i64, safe: Option<i64>| ClockMessage {
        t,
        speed: 1,
        ahead: 3,
        need: None,
        safe,
        pause: false,
        stop: None,
        hop: None,
    };
    h.host_game(MessageKind::Clock, &json(&clock(50_000, Some(50_000 + 2 * Lockstep::STEP))));
    h.player_game(a, MessageKind::Clock, &json(&clock(50_000, None)));
    let by_lead = Lockstep::round_up_to_step((50_000 + Lockstep::lead_units(1)) as f64);
    assert_eq!(by_lead, h.host.future_stamp(), "a promise inside the lead changes nothing");
    // a game promised more (a longer lead it has not given up yet, the barrier's extra): another game may run up to it
    h.player_game(a, MessageKind::Clock, &json(&clock(50_000, Some(54_000))));
    assert_eq!(54_000, h.host.future_stamp());
    h.host_call(HostSession::start);
    h.player_game(a, MessageKind::SpeedRequest, &json(&SpeedRequestMessage { speed: 1 }));
    h.player_game(a, MessageKind::SpeedRequest, &json(&SpeedRequestMessage { speed: 0 }));
    assert!(h.host.state().pause_at.unwrap() >= 54_000, "the pause lies where no game can have passed it");
}

#[test]
fn start_speed_and_pause_are_broadcast_and_speed_requests_obeyed() {
    let mut h = Harness::new();
    let a = h.connect("A");
    h.host_game(MessageKind::Hello, &json(&HelloMessage::new(18600)));
    h.host_game(
        MessageKind::Clock,
        &json(&ClockMessage {
            t: 18600,
            speed: 0,
            ahead: 3,
            need: None,
            safe: None,
            pause: false,
            stop: None,
            hop: None,
        }),
    );
    h.player_game(a, MessageKind::Hello, &json(&HelloMessage::new(18600)));
    h.player_game(
        a,
        MessageKind::Clock,
        &json(&ClockMessage {
            t: 18600,
            speed: 0,
            ahead: 3,
            need: None,
            safe: None,
            pause: false,
            stop: None,
            hop: None,
        }),
    );

    h.player_game(a, MessageKind::SpeedRequest, &json(&SpeedRequestMessage { speed: 2 }));
    assert!(!h.host.state().started); // ignored before start

    h.host_call(HostSession::start);
    assert!(h.host.state().started);
    assert_eq!(Some(18600), h.host.state().pause_at); // every game held at the common time
    assert!(h.p(a).session.session().unwrap().started);
    assert_eq!(1, h.p(a).count(MessageKind::ChecksumRequest));

    h.player_game(a, MessageKind::SpeedRequest, &json(&SpeedRequestMessage { speed: 2 }));
    assert_eq!(2, h.host.state().speed);
    assert_eq!(None, h.host.state().pause_at);
    assert_eq!(2, h.p(a).session.session().unwrap().speed);
    assert_eq!(Lockstep::lead_units(2), h.p(a).session.session().unwrap().lead);

    h.player_game(a, MessageKind::SpeedRequest, &json(&SpeedRequestMessage { speed: 2 })); // same speed: no echo
    let sessions = h.p(a).count(MessageKind::Session);
    h.player_game(a, MessageKind::SpeedRequest, &json(&SpeedRequestMessage { speed: 0 }));
    assert!(h.host.state().pause_at.is_some());
    assert_eq!(sessions + 1, h.p(a).count(MessageKind::Session));
    assert_eq!(0, h.host.state().pause_at.unwrap() % Lockstep::STEP);
}

#[test]
fn checksum_round_completes_when_every_ready_game_reported_and_names_the_differences() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let _b = h.connect("B");
    h.host_game(MessageKind::Hello, &json(&HelloMessage::new(0)));
    h.player_game(a, MessageKind::Hello, &json(&HelloMessage::new(0)));
    // B never says hello: it is not expected to report
    h.host_call(HostSession::start);
    let req: ChecksumRequestMessage = h.p(a).last(MessageKind::ChecksumRequest);

    let host_parts = parts(&[("money", "1"), ("roads", "r1"), ("vehicles", "v")]);
    let a_parts = parts(&[("money", "2"), ("roads", "r1"), ("vehicles", "v"), ("extra", "e")]);
    let mut host_report = ChecksumReportMessage::new(req.n, req.at, host_parts);
    host_report.authority = Some([("balance".to_string(), 1000.0)].into_iter().collect());
    h.host_game(MessageKind::ChecksumReport, &json(&host_report));
    assert!(h.outcomes.is_empty());
    assert_eq!(1000.0, h.p(a).last::<AuthorityMessage>(MessageKind::Authority).values["balance"]);
    h.player_game(a, MessageKind::ChecksumReport, &json(&ChecksumReportMessage::new(req.n, req.at, a_parts)));

    assert_eq!(1, h.outcomes.len());
    let o = &h.outcomes[0];
    assert!(!o.identical());
    assert_eq!(vec!["extra", "money"], o.differing_parts().collect::<Vec<_>>());
    assert_eq!(vec![0, 1], o.reporters);
    let money = o.differences.iter().find(|d| d.part == "money").unwrap();
    assert_eq!(Some("1".to_string()), money.values[&0]);
    assert_eq!(Some("2".to_string()), money.values[&1]);
    assert_eq!(None, o.differences.iter().find(|d| d.part == "extra").unwrap().values[&0]);
}

#[test]
fn identical_reports_give_an_identical_outcome() {
    let p = parts(&[("a", "1"), ("b", "2")]);
    let reports: BTreeMap<u16, ChecksumReportMessage> =
        [(0, ChecksumReportMessage::new(1, 5, p.clone())), (1, ChecksumReportMessage::new(1, 5, p))]
            .into_iter()
            .collect();
    let o = ChecksumRounds::compare(1, &reports);
    assert!(o.identical());
    assert_eq!(5, o.time);
}

#[test]
fn reports_taken_at_different_sim_times_are_not_compared() {
    let reports: BTreeMap<u16, ChecksumReportMessage> = [
        (0, ChecksumReportMessage::new(1, 5000, parts(&[("time", "a"), ("money", "1")]))),
        (1, ChecksumReportMessage::new(1, 5100, parts(&[("time", "b"), ("money", "2")]))),
    ]
    .into_iter()
    .collect();
    let o = ChecksumRounds::compare(1, &reports);
    assert!(!o.comparable() && !o.identical());
    assert!(o.differences.is_empty());
    assert_eq!(Some(5100), o.time_skew.as_ref().map(|m| m[&1]));
}

#[test]
fn a_part_a_game_reports_not_ready_is_not_compared_and_a_missing_part_still_differs() {
    let mut late = ChecksumReportMessage::new(1, 5, parts(&[("a", "1"), ("b", "other")]));
    late.not_ready = vec!["companies".to_owned()];
    let reports: BTreeMap<u16, ChecksumReportMessage> = [
        (0, ChecksumReportMessage::new(1, 5, parts(&[("a", "1"), ("b", "2"), ("companies", "c"), ("towns:7", "t")]))),
        (1, late),
    ]
    .into_iter()
    .collect();
    let o = ChecksumRounds::compare(1, &reports);
    assert!(o.comparable() && !o.ready() && !o.identical());
    assert_eq!(vec![1], o.not_ready["companies"]);
    // what both games hashed is compared as always, and a part one game simply does not have is a difference (a town only one
    // world has): only a part the game itself names not ready is unknown
    assert_eq!(vec!["b", "towns:7"], o.differing_parts().collect::<Vec<_>>());
    assert_eq!("companies on 1", o.describe_not_ready());
}

#[test]
fn a_light_round_drops_heavy_parts_from_the_not_ready_list_too() {
    let mut rounds = ChecksumRounds::new();
    rounds.open_light(3);
    let mut r = ChecksumReportMessage::new(3, 5, parts(&[("a", "1")]));
    r.not_ready = vec!["edges".to_owned()];
    assert!(rounds.report(1, r, &[0, 1]).is_none());
    let o = rounds.report(0, ChecksumReportMessage::new(3, 5, parts(&[("a", "1")])), &[0, 1]).expect("complete");
    assert!(o.identical(), "{o:?}");
}

#[test]
fn tick_rebroadcasts_the_session_and_requests_checksums_while_running() {
    let options = HostOptions { session_broadcast_seconds: 3.0, checksum_seconds: 10.0, ..HostOptions::default() };
    let mut h = Harness::with("Hote", options);
    let a = h.connect("A");
    h.host_call(HostSession::tick);
    assert_eq!(1, h.p(a).count(MessageKind::Session));
    h.advance(2.0);
    h.host_call(HostSession::tick);
    assert_eq!(1, h.p(a).count(MessageKind::Session));
    h.advance(1.5);
    h.host_call(HostSession::tick);
    assert_eq!(2, h.p(a).count(MessageKind::Session));

    assert_eq!(0, h.p(a).count(MessageKind::ChecksumRequest));
    h.host_call(HostSession::start); // paused at start (no clocks): no periodic checksum yet
    h.host_call(|host| host.set_speed(1));
    let n0 = h.p(a).count(MessageKind::ChecksumRequest);
    h.advance(10.5);
    h.host_call(HostSession::tick);
    assert_eq!(n0 + 1, h.p(a).count(MessageKind::ChecksumRequest));
    h.host_call(HostSession::pause);
    h.advance(20.0);
    h.host_call(HostSession::tick);
    assert_eq!(n0 + 1, h.p(a).count(MessageKind::ChecksumRequest)); // paused: none
}

#[test]
fn a_running_full_round_is_spread_one_batch_apart_and_light_and_paused_rounds_are_not() {
    let options = HostOptions { checksum_seconds: 10.0, full_checksum_every: 2, ..HostOptions::default() };
    let mut h = Harness::with("Hote", options);
    let a = h.connect("A");
    h.host_call(HostSession::start);
    h.host_call(|host| host.set_speed(3));
    for _ in 0..4 {
        h.advance(10.5);
        h.host_call(HostSession::tick);
    }
    let running: Vec<(bool, bool, u32)> = h
        .p(a)
        .all::<ChecksumRequestMessage>(MessageKind::ChecksumRequest)
        .into_iter()
        .filter(|r| !r.paused)
        .map(|r| (r.paused, r.full, r.spread))
        .collect();
    assert!(running.contains(&(false, true, 3)), "a full round at x3 is read one chunk every 3 steps: {running:?}");
    assert!(running.contains(&(false, false, 0)), "a light round is hashed whole: {running:?}");
    assert!(running.iter().all(|r| r.1 == (r.2 > 0)), "{running:?}");
    let paused = h.p(a).all::<ChecksumRequestMessage>(MessageKind::ChecksumRequest).into_iter().filter(|r| r.paused);
    assert!(paused.into_iter().all(|r| r.spread == 0), "a paused round is hashed whole by the interface half");
}

/// A running session of the host's game and player A, both ready, at x3; returns A's connection.
fn running_pair(options: HostOptions) -> (Harness, ConnectionId) {
    let mut h = Harness::with("Hote", options);
    let a = h.connect("A");
    h.host_game(MessageKind::Hello, &json(&HelloMessage::new(0)));
    h.player_game(a, MessageKind::Hello, &json(&HelloMessage::new(0)));
    h.host_call(HostSession::start);
    h.host_call(|host| host.set_speed(3));
    // the round asked at the start is reported by both, then forgotten by the test: what follows is the timer's rounds alone
    for r in running_requests(&h, a) {
        report(&mut h, None, &r);
        report(&mut h, Some(a), &r);
    }
    h.players.get_mut(&a).unwrap().game.clear();
    h.outcomes.clear();
    h.host_log.clear();
    (h, a)
}

fn running_requests(h: &Harness, a: ConnectionId) -> Vec<ChecksumRequestMessage> {
    h.p(a).all::<ChecksumRequestMessage>(MessageKind::ChecksumRequest).into_iter().filter(|r| !r.paused).collect()
}

fn report(h: &mut Harness, who: Option<ConnectionId>, r: &ChecksumRequestMessage) {
    let m = json(&ChecksumReportMessage::new(r.n, r.at, parts(&[("roads", "r"), ("edges", "e")])));
    match who {
        None => h.host_game(MessageKind::ChecksumReport, &m),
        Some(c) => h.player_game(c, MessageKind::ChecksumReport, &m),
    }
}

#[test]
fn every_regular_round_is_full_and_spread_by_default() {
    let (mut h, a) = running_pair(HostOptions::default());
    for _ in 0..4 {
        h.advance(10.5);
        h.host_call(HostSession::tick);
        let r = running_requests(&h, a).pop().expect("a round");
        report(&mut h, None, &r);
        report(&mut h, Some(a), &r);
    }
    let rounds = running_requests(&h, a);
    assert_eq!(4, rounds.len(), "{rounds:?}");
    assert!(
        rounds.iter().all(|r| r.full && r.spread == 3),
        "every round full, one chunk every 3 steps at x3: {rounds:?}"
    );
    assert!(h.outcomes.iter().all(|o| o.full && o.identical()), "{:?}", h.outcomes);
}

#[test]
fn the_chunk_follows_the_slowest_games_cost_and_a_round_that_differed_asks_for_detail_until_one_is_identical() {
    // (no repair: the differing rounds stay regular rounds)
    let (mut h, a) = running_pair(HostOptions { auto_resync: false, ..HostOptions::default() });
    // (host cost, player cost in ns an entity, player's roads hash) per round
    let script =
        [(20_000, 25_000, "r"), (20_000, 25_000, "x"), (20_000, 60_000, "x"), (20_000, 60_000, "r"), (0, 0, "r")];
    let mut seen = Vec::new();
    for (host_ns, player_ns, roads) in script {
        h.advance(5.5);
        h.host_call(HostSession::tick);
        let r = running_requests(&h, a).pop().expect("a round every 5 s");
        seen.push((r.chunk, r.detail));
        let cost = |ns: u32| (ns > 0).then_some(ChecksumCost { entity_ns: ns, step_max_us: 0, chunk: r.chunk });
        let host = ChecksumReportMessage {
            cost: cost(host_ns),
            ..ChecksumReportMessage::new(r.n, r.at, parts(&[("roads", "r")]))
        };
        let player = ChecksumReportMessage {
            cost: cost(player_ns),
            ..ChecksumReportMessage::new(r.n, r.at, parts(&[("roads", roads)]))
        };
        h.host_game(MessageKind::ChecksumReport, &json(&host));
        h.player_game(a, MessageKind::ChecksumReport, &json(&player));
    }
    // 200 until a cost is known; 12 ms / 25 us = 480, but at most twice the last (400), then 480; 12 ms / 60 us = 200 at once
    // (the dearest of each game's last three rounds counts); detail from the round after the one that differed until an identical one
    assert_eq!(seen, [(200, false), (400, false), (480, true), (200, true), (200, false)], "{:?}", h.host_log);
    assert!(h
        .host_log
        .iter()
        .any(|l| l.contains("checksum chunk: 400 entities a step") && l.contains("25.0 us an entity")));
    // the old cadence for lab comparisons: no chunk size, the games' own 200
    let (mut h, a) = running_pair(HostOptions { sized_checksum_chunks: false, ..HostOptions::default() });
    h.advance(5.5);
    h.host_call(HostSession::tick);
    assert_eq!(0, running_requests(&h, a).pop().expect("a round").chunk);
}

#[test]
fn a_regular_round_waits_for_a_slow_game_and_starts_right_after_it_reported() {
    let (mut h, a) = running_pair(HostOptions::default());
    h.advance(10.5);
    h.host_call(HostSession::tick);
    let first = running_requests(&h, a).pop().expect("round one");
    report(&mut h, None, &first);
    // A is slow: its spread round is still being read when the 10 s timer fires twice; no second round is asked meanwhile
    for _ in 0..2 {
        h.advance(10.0);
        h.host_call(HostSession::tick);
        assert_eq!(1, running_requests(&h, a).len(), "no overlap while A has not reported");
    }
    report(&mut h, Some(a), &first);
    assert_eq!(1, h.outcomes.len());
    // the timer is overdue: the next round starts at the next tick, not 10 s after the late report
    h.advance(0.3);
    h.host_call(HostSession::tick);
    let rounds = running_requests(&h, a);
    assert_eq!(2, rounds.len());
    assert!(rounds[1].n > first.n && rounds[1].full);
    // the wait is logged once a minute: how many rounds waited and the longest wait
    report(&mut h, None, &rounds[1]);
    report(&mut h, Some(a), &rounds[1]);
    for _ in 0..8 {
        h.advance(10.0);
        h.host_call(HostSession::tick);
        let r = running_requests(&h, a).pop().unwrap();
        report(&mut h, None, &r);
        report(&mut h, Some(a), &r);
    }
    let waits: Vec<&String> = h.host_log.iter().filter(|l| l.contains("waited for the previous one")).collect();
    assert_eq!(1, waits.len(), "{:?}", h.host_log);
    assert!(waits[0].contains("checksum: 1 round(s)") && waits[0].contains("longest wait 10.3 s"), "{}", waits[0]);
}

#[test]
fn a_round_that_never_completes_is_dropped_after_the_bound_and_the_next_one_starts() {
    let options = HostOptions { checksum_round_timeout_seconds: 30.0, ..HostOptions::default() };
    let (mut h, a) = running_pair(options);
    h.advance(10.5);
    h.host_call(HostSession::tick);
    let first = running_requests(&h, a).pop().expect("round one");
    report(&mut h, None, &first);
    // A never reports (it reloads, say): the round holds the next one back only up to the bound
    for _ in 0..2 {
        h.advance(10.0);
        h.host_call(HostSession::tick);
    }
    assert_eq!(1, running_requests(&h, a).len());
    h.advance(10.5);
    h.host_call(HostSession::tick);
    assert!(
        h.host_log.iter().any(|l| l.contains(&format!("checksum round {} dropped: not reported by A", first.n))),
        "{:?}",
        h.host_log
    );
    assert_eq!(2, running_requests(&h, a).len(), "the next round starts once the stuck one is dropped");
    // a late report of the dropped round is ignored
    report(&mut h, Some(a), &first);
    assert!(h.outcomes.is_empty());
}

#[test]
fn with_the_switch_a_light_round_is_still_hashed_whole_and_waits_like_any_round() {
    let options = HostOptions { full_checksum_every: 6, ..HostOptions::default() };
    let (mut h, a) = running_pair(options);
    for _ in 0..3 {
        h.advance(10.5);
        h.host_call(HostSession::tick);
        let r = running_requests(&h, a).pop().unwrap();
        report(&mut h, None, &r);
        report(&mut h, Some(a), &r);
    }
    let kinds: Vec<(bool, u32)> = running_requests(&h, a).iter().map(|r| (r.full, r.spread)).collect();
    assert_eq!(3, kinds.len());
    assert!(kinds.contains(&(false, 0)), "the old split: light rounds between the full ones: {kinds:?}");
    assert!(h.outcomes.iter().any(|o| !o.full));
}

#[test]
fn a_request_without_spread_reads_as_a_whole_round() {
    let r: ChecksumRequestMessage = serde_json::from_str(r#"{"n":1,"at":5,"paused":false}"#).unwrap();
    assert_eq!((true, 0), (r.full, r.spread));
    assert!(
        !String::from_utf8(json(&r)).unwrap().contains("spread"),
        "0 is left out, so an older game reads the same request"
    );
}

#[test]
fn disconnect_removes_the_player_and_tells_the_others() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    h.player_game(
        a,
        MessageKind::Clock,
        &json(&ClockMessage { t: 5, speed: 1, ahead: 3, need: None, safe: None, pause: false, stop: None, hop: None }),
    );
    h.disconnect(a);
    assert_eq!(2, h.host.players().len());
    assert!(h.host.clocks().is_empty());
    assert_eq!("A", h.p(b).last::<PlayerLeftMessage>(MessageKind::PlayerLeft).name);
    assert_eq!("A", h.host_game_last::<PlayerLeftMessage>(MessageKind::PlayerLeft).name);
}

#[test]
fn player_ignores_frames_not_from_the_host_and_messages_before_welcome() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let mut other = Sealer::new(&h.key, 2);
    let state = SessionState {
        started: true,
        speed: 4,
        pause_at: None,
        lead: 0,
        resync: None,
        mod_warnings: Vec::new(),
        stalled: Vec::new(),
    };
    let frame = other.seal(MessageKind::Session.into(), &json(&state));
    h.players.get_mut(&a).unwrap().session.frame_received(&frame);
    assert_eq!(1, h.p(a).session.dropped());
    assert!(!h.p(a).session.session().unwrap().started);

    let mut fresh = PlayerSession::new(h.key.clone(), "F", "0.1");
    fresh.local_game_message(MessageKind::Chat, &json(&ChatMessage::new("too early")));
    assert_eq!(1, fresh.dropped());
}

#[test]
fn chat_is_relayed_with_the_senders_name_and_trimmed() {
    let mut h = Harness::new();
    let a = h.connect("Alice");
    let b = h.connect("Bob");
    h.player_game(a, MessageKind::Chat, &json(&ChatMessage::new(&"x".repeat(600))));
    let got: ChatMessage = h.p(b).last(MessageKind::Chat);
    assert_eq!(Some("Alice"), got.name.as_deref());
    assert_eq!(h.p(a).session.id().unwrap(), got.from);
    assert_eq!(500, got.text.len());
}

#[test]
fn codec_round_trips_records_and_rejects_garbage() {
    let mut m = ChecksumReportMessage::new(3, 1200, parts(&[("roads", "abc")]));
    m.authority = Some([("balance".to_string(), -5.5)].into_iter().collect());
    let back: ChecksumReportMessage = codec::try_decode(&codec::encode(&m)).unwrap();
    assert_eq!(m.n, back.n);
    assert_eq!("abc", back.parts["roads"]);
    assert_eq!(-5.5, back.authority.unwrap()["balance"]);
    assert_eq!(None, codec::try_decode::<ChecksumReportMessage>(b"{not json"));
    assert_eq!(None, codec::try_decode::<ChecksumReportMessage>(b"[1,2]"));
    let json = String::from_utf8(codec::encode(&ActionMessage::new(1, 2, "b"))).unwrap();
    assert!(json.contains("\"stamp\":1"));
    assert!(!json.contains("null"));
}

#[test]
fn proof_depends_on_key_nonce_and_name() {
    let key = SessionKey::derive(&SECRET, None);
    let nonce = [0u8; 16];
    let p = HostSession::proof(&key, &nonce, "A");
    assert_eq!(32, p.len());
    assert_ne!(p, HostSession::proof(&key, &nonce, "B"));
    assert_ne!(p, HostSession::proof(&key, &[1], "A"));
    assert_ne!(p, HostSession::proof(&SessionKey::derive(&SECRET, Some("pw")), &nonce, "A"));
}

#[test]
fn skewed_rounds_are_counted_and_logged_every_twenty_rounds() {
    let mut h = Harness::new();
    let a = h.connect("A");
    h.host_game(MessageKind::Hello, &json(&HelloMessage::new(0)));
    h.player_game(a, MessageKind::Hello, &json(&HelloMessage::new(0)));
    h.host_call(HostSession::start);
    let p = parts(&[("money", "1")]);
    for i in 0..20 {
        h.host_call(HostSession::request_checksum);
        let req: ChecksumRequestMessage = h.p(a).last(MessageKind::ChecksumRequest);
        let skew = if i % 4 == 0 { 200 } else { 0 };
        h.host_game(MessageKind::ChecksumReport, &json(&ChecksumReportMessage::new(req.n, req.at, p.clone())));
        h.player_game(
            a,
            MessageKind::ChecksumReport,
            &json(&ChecksumReportMessage::new(req.n, req.at + skew, p.clone())),
        );
    }
    let lines: Vec<&String> = h.host_log.iter().filter(|l| l.contains("skipped as skewed")).collect();
    assert_eq!(1, lines.len(), "{:?}", h.host_log);
    assert!(lines[0].contains("5 of 20"), "{}", lines[0]);
    assert!(!h.host_log.iter().any(|l| l.contains("WARNING")), "never 3 in a row: {:?}", h.host_log);
}

/// Owner's live test 2026-10-18: 44 of 60 rounds were skewed (no desync could be seen) and the only trace was the line every 20
/// rounds. A run of skewed rounds is a WARNING, its end is said, and a minute with a skewed round has its own count.
#[test]
fn skewed_rounds_in_a_row_give_a_warning_a_minute_line_and_a_line_when_compared_again() {
    let mut h = Harness::new();
    let a = h.connect("A");
    h.host_game(MessageKind::Hello, &json(&HelloMessage::new(0)));
    h.player_game(a, MessageKind::Hello, &json(&HelloMessage::new(0)));
    h.host_call(HostSession::start);
    let p = parts(&[("money", "1")]);
    let round = |h: &mut Harness, skew: i64| {
        h.host_call(HostSession::request_checksum);
        let req: ChecksumRequestMessage = h.p(a).last(MessageKind::ChecksumRequest);
        h.host_game(MessageKind::ChecksumReport, &json(&ChecksumReportMessage::new(req.n, req.at, p.clone())));
        h.player_game(
            a,
            MessageKind::ChecksumReport,
            &json(&ChecksumReportMessage::new(req.n, req.at + skew, p.clone())),
        );
        h.advance(8.0);
    };
    let warnings = |h: &Harness| h.host_log.iter().filter(|l| l.starts_with("WARNING:")).cloned().collect::<Vec<_>>();
    round(&mut h, 200);
    round(&mut h, 200);
    assert!(warnings(&h).is_empty());
    round(&mut h, 200);
    assert_eq!(
        vec!["WARNING: 3 checksum rounds in a row not compared (the games hashed at different sim times): no desync can be detected until one is"],
        warnings(&h)
    );
    for _ in 0..10 {
        round(&mut h, 400);
    }
    assert_eq!(2, warnings(&h).len(), "again after 10 more: {:?}", warnings(&h));
    assert!(warnings(&h)[1].starts_with("WARNING: 13 checksum rounds in a row"), "{:?}", warnings(&h));
    let minute: Vec<&String> = h.host_log.iter().filter(|l| l.starts_with("checksum minute:")).collect();
    assert!(!minute.is_empty(), "{:?}", h.host_log);
    assert!(minute[0].contains("round(s) not compared"), "{}", minute[0]);
    round(&mut h, 0);
    assert!(
        h.host_log.iter().any(|l| l == "checksum rounds compared again after 13 skewed round(s) in a row"),
        "{:?}",
        h.host_log
    );
}
