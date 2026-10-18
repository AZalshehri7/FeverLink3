//! A host and players wired together in memory with a clock the test moves. Shared by the resync and permission
//! tests; `session_tests.rs` keeps its own copy.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::session::*;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub const SECRET: [u8; 16] = [7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22];

pub fn json<T: Serialize>(m: &T) -> Vec<u8> {
    codec::encode(m)
}

pub struct Player {
    pub session: PlayerSession,
    pub game: Vec<(MessageKind, Vec<u8>)>,
    pub log: Vec<String>,
    pub saves: Vec<(i32, String, Vec<u8>)>,
}

impl Player {
    pub fn last<T: DeserializeOwned>(&self, kind: MessageKind) -> T {
        let (_, payload) = self.game.iter().rev().find(|g| g.0 == kind).unwrap_or_else(|| panic!("no {kind:?}"));
        codec::try_decode(payload).unwrap_or_else(|| panic!("{kind:?} decodes"))
    }

    pub fn count(&self, kind: MessageKind) -> usize {
        self.game.iter().filter(|g| g.0 == kind).count()
    }
}

pub struct Harness {
    pub key: SessionKey,
    pub now: Arc<Mutex<f64>>,
    pub host: HostSession,
    pub host_game: Vec<(MessageKind, Vec<u8>)>,
    pub host_log: Vec<String>,
    pub events: Vec<HostOutput>,
    pub players: BTreeMap<ConnectionId, Player>,
    pub routable: BTreeSet<ConnectionId>,
    next_connection: ConnectionId,
}

impl Harness {
    pub fn new() -> Self {
        Self::with(HostOptions::default())
    }

    pub fn with(options: HostOptions) -> Self {
        let key = SessionKey::derive(&SECRET, None);
        let now = Arc::new(Mutex::new(0.0));
        let clock = Arc::clone(&now);
        let host = HostSession::new(key.clone(), "Hote", Box::new(move || *clock.lock().unwrap()), options);
        Self {
            key,
            now,
            host,
            host_game: Vec::new(),
            host_log: Vec::new(),
            events: Vec::new(),
            players: BTreeMap::new(),
            routable: BTreeSet::new(),
            next_connection: 0,
        }
    }

    pub fn advance(&mut self, seconds: f64) {
        *self.now.lock().unwrap() += seconds;
    }

    pub fn connect(&mut self, name: &str) -> ConnectionId {
        self.connect_with_caps(name, &[])
    }

    /// A player whose join message announces these capabilities.
    pub fn connect_with_caps(&mut self, name: &str, caps: &[&str]) -> ConnectionId {
        self.next_connection += 1;
        let c = self.next_connection;
        let clock = Arc::clone(&self.now);
        let mut session =
            PlayerSession::with_clock(self.key.clone(), name, "0.1", Box::new(move || *clock.lock().unwrap()));
        session.set_capabilities(caps.iter().map(|c| (*c).to_owned()).collect());
        self.players.insert(c, Player { session, game: Vec::new(), log: Vec::new(), saves: Vec::new() });
        self.routable.insert(c);
        let outs = self.host.connection_opened(c);
        self.pump_host(outs);
        c
    }

    pub fn disconnect(&mut self, c: ConnectionId) {
        self.routable.remove(&c);
        let outs = self.host.connection_closed(c);
        self.pump_host(outs);
    }

    pub fn p(&self, c: ConnectionId) -> &Player {
        &self.players[&c]
    }

    pub fn player_game(&mut self, c: ConnectionId, kind: MessageKind, payload: &[u8]) {
        let outs = self.players.get_mut(&c).unwrap().session.local_game_message(kind, payload);
        self.pump_player(c, outs);
    }

    pub fn host_game(&mut self, kind: MessageKind, payload: &[u8]) {
        let outs = self.host.local_game_message(kind, payload);
        self.pump_host(outs);
    }

    pub fn host_call(&mut self, call: impl FnOnce(&mut HostSession) -> Vec<HostOutput>) {
        let outs = call(&mut self.host);
        self.pump_host(outs);
    }

    pub fn pump_host(&mut self, outs: Vec<HostOutput>) {
        for o in outs {
            match &o {
                HostOutput::SendToConnection { connection, frame } => {
                    if self.routable.contains(connection) {
                        let outs = self.players.get_mut(connection).unwrap().session.frame_received(frame);
                        self.pump_player(*connection, outs);
                    }
                }
                HostOutput::CloseConnection(c) => {
                    self.routable.remove(c);
                }
                HostOutput::DeliverToLocalGame { kind, payload } => self.host_game.push((*kind, payload.clone())),
                HostOutput::Log(line) => self.host_log.push(line.clone()),
                _ => {}
            }
            self.events.push(o);
        }
    }

    pub fn pump_player(&mut self, c: ConnectionId, outs: Vec<PlayerOutput>) {
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
                PlayerOutput::SaveReceived { id, sha256, bytes } => {
                    self.players.get_mut(&c).unwrap().saves.push((id, sha256, bytes));
                }
                PlayerOutput::Welcomed(_) | PlayerOutput::Rejected(_) => {}
            }
        }
    }

    pub fn host_game_last<T: DeserializeOwned>(&self, kind: MessageKind) -> T {
        let (_, payload) = self.host_game.iter().rev().find(|g| g.0 == kind).unwrap_or_else(|| panic!("no {kind:?}"));
        codec::try_decode(payload).unwrap()
    }

    pub fn host_game_count(&self, kind: MessageKind) -> usize {
        self.host_game.iter().filter(|g| g.0 == kind).count()
    }

    // ---- helpers for resync scenarios

    /// Every game says Hello and reports a clock.
    pub fn hello_all(&mut self, t: i64, speed: i32) {
        self.host_game(MessageKind::Hello, &json(&HelloMessage::new(t)));
        self.host_game(
            MessageKind::Clock,
            &json(&ClockMessage { t, speed, ahead: 3, need: None, safe: None, pause: false, stop: None, hop: None }),
        );
        let ids: Vec<ConnectionId> = self.players.keys().copied().collect();
        for c in ids {
            self.hello_player(c, t, speed);
        }
    }

    pub fn hello_player(&mut self, c: ConnectionId, t: i64, speed: i32) {
        self.player_game(c, MessageKind::Hello, &json(&HelloMessage::new(t)));
        self.player_game(
            c,
            MessageKind::Clock,
            &json(&ClockMessage { t, speed, ahead: 3, need: None, safe: None, pause: false, stop: None, hop: None }),
        );
    }

    /// Every game reports a stopped clock at `t`.
    pub fn stop_all_at(&mut self, t: i64) {
        self.host_game(
            MessageKind::Clock,
            &json(&ClockMessage { t, speed: 0, ahead: 3, need: None, safe: None, pause: false, stop: None, hop: None }),
        );
        let ids: Vec<ConnectionId> = self.players.keys().copied().collect();
        for c in ids {
            self.player_game(
                c,
                MessageKind::Clock,
                &json(&ClockMessage {
                    t,
                    speed: 0,
                    ahead: 3,
                    need: None,
                    safe: None,
                    pause: false,
                    stop: None,
                    hop: None,
                }),
            );
        }
    }

    /// A started session at `speed`, with the given number of ready players, all at game time 10 000.
    pub fn running(players: usize, speed: i32, options: HostOptions) -> (Self, Vec<ConnectionId>) {
        let caps = vec![Vec::new(); players];
        Self::running_with_caps(&caps, speed, options)
    }

    /// As `running`, one player for each entry of `caps`, announcing those capabilities.
    pub fn running_with_caps(caps: &[Vec<&str>], speed: i32, options: HostOptions) -> (Self, Vec<ConnectionId>) {
        let mut h = Self::with(options);
        let ids: Vec<ConnectionId> =
            caps.iter().enumerate().map(|(i, c)| h.connect_with_caps(&format!("P{i}"), c)).collect();
        h.hello_all(10_000, 0);
        h.host_call(HostSession::start);
        h.host_call(|host| host.set_speed(speed));
        h.hello_all(10_000, speed);
        (h, ids)
    }

    pub fn progress_states(&self) -> Vec<ResyncState> {
        self.events
            .iter()
            .filter_map(|e| match e {
                HostOutput::ResyncProgress { state, .. } => Some(*state),
                _ => None,
            })
            .collect()
    }

    pub fn failures(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                HostOutput::ResyncFailed { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn requests(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                HostOutput::ResyncRequested { reason } => Some(reason.clone()),
                _ => None,
            })
            .collect()
    }

    /// Drives a round from Pausing to Transferring: everyone stops at the pause stamp and the host's game saves.
    pub fn reach_transferring(&mut self) -> i32 {
        let pause_at = self.host.state().pause_at.expect("paused");
        self.stop_all_at(pause_at);
        assert_eq!(ResyncState::Saving, self.host.resync_state());
        let id = self.host.resync_id().unwrap();
        self.host_game(MessageKind::SaveDone, &json(&SaveDoneMessage { id, ok: true, detail: None }));
        assert_eq!(ResyncState::Transferring, self.host.resync_state());
        id
    }

    /// Announces `bytes` and streams them whole.
    pub fn send_file(&mut self, id: i32, bytes: &[u8]) {
        let sha = sha256_hex(bytes);
        self.host_call(|h| h.resync_file_ready(id, &sha, bytes.len() as u64));
        for (i, chunk) in bytes.chunks(ResyncLimits::CHUNK_SIZE).enumerate() {
            self.host_call(|h| h.resync_chunk(id, i as u32, chunk));
        }
    }

    pub fn ready_count(&self) -> usize {
        self.host.players().values().filter(|p| p.ready).count()
    }
}

pub fn parts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
}

/// A deterministic blob of `len` bytes.
pub fn blob(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}
