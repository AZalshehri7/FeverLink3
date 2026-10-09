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
fn session_is_full_at_max_players() {
    let mut h = Harness::with("Hote", HostOptions { max_players: 2, ..HostOptions::default() });
    h.connect("A");
    h.connect("B");
    let c = h.connect("C");
    assert_eq!("full", h.p(c).reject.as_ref().unwrap().reason);
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
    let state = SessionState { started: true, speed: 4, pause_at: None, lead: 0 };
    h.player_game(a, MessageKind::Session, &json(&state));
    let load = ResyncLoadMessage { id: 1, name: "evil".into(), sha256: "00".into() };
    h.player_game(a, MessageKind::ResyncLoad, &json(&load));
    let request = ChecksumRequestMessage { n: 9, at: 0, paused: false };
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

#[test]
fn clock_becomes_a_peer_clock_for_the_others_and_feeds_the_stamp() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    h.player_game(a, MessageKind::Clock, &json(&ClockMessage { t: 50000, speed: 1, ahead: 3 }));
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
fn start_speed_and_pause_are_broadcast_and_speed_requests_obeyed() {
    let mut h = Harness::new();
    let a = h.connect("A");
    h.host_game(MessageKind::Hello, &json(&HelloMessage::new(18600)));
    h.host_game(MessageKind::Clock, &json(&ClockMessage { t: 18600, speed: 0, ahead: 3 }));
    h.player_game(a, MessageKind::Hello, &json(&HelloMessage::new(18600)));
    h.player_game(a, MessageKind::Clock, &json(&ClockMessage { t: 18600, speed: 0, ahead: 3 }));

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
fn disconnect_removes_the_player_and_tells_the_others() {
    let mut h = Harness::new();
    let a = h.connect("A");
    let b = h.connect("B");
    h.player_game(a, MessageKind::Clock, &json(&ClockMessage { t: 5, speed: 1, ahead: 3 }));
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
    let state = SessionState { started: true, speed: 4, pause_at: None, lead: 0 };
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
