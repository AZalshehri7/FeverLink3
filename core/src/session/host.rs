use std::collections::{BTreeMap, HashSet};
use std::fmt;

use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::checksum::{ChecksumOutcome, ChecksumRounds};
use super::kind::MessageKind;
use super::lockstep::{Lockstep, PlayerClock};
use super::messages::{
    codec, ActionMessage, AuthorityMessage, ChallengeMessage, ChatMessage, ChecksumReportMessage,
    ChecksumRequestMessage, ClockMessage, JoinMessage, PeerClockMessage, Permissions, PlayerInfo, PlayerJoinedMessage,
    PlayerLeftMessage, RejectMessage, ReplayFailedMessage, SaveDoneMessage, SessionState, SpeedRequestMessage,
    WelcomeMessage,
};
use super::names::Names;
use super::policy::{ChecksumPolicy, ChecksumPolicyConfig};
use super::resync::ResyncState;
use crate::crypto::{Opener, Sealer, SessionKey};
use crate::protocol::{Frame, FrameHeader};

mod resync_impl;
use resync_impl::ResyncRound;

/// A transport connection, numbered by the caller.
pub type ConnectionId = u64;

#[derive(Debug, Clone, PartialEq)]
pub struct HostOptions {
    pub version: String,
    pub max_players: usize,
    pub session_broadcast_seconds: f64,
    pub checksum_seconds: f64,
    /// Start a resync round by itself when the checksum policy asks for one (default true). When false the host
    /// only emits [`HostOutput::ResyncRequested`] and the launcher decides.
    pub auto_resync: bool,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            version: "0.1".to_owned(),
            max_players: 8,
            session_broadcast_seconds: 3.0,
            checksum_seconds: 10.0,
            auto_resync: true,
        }
    }
}

/// What the host asks its caller to do, in this order.
#[derive(Debug, Clone, PartialEq)]
pub enum HostOutput {
    /// A frame to write to a connection.
    SendToConnection {
        connection: ConnectionId,
        frame: Frame,
    },
    /// The caller must close this connection (after a reject).
    CloseConnection(ConnectionId),
    /// A message for the host's own game.
    DeliverToLocalGame {
        kind: MessageKind,
        payload: Vec<u8>,
    },
    /// Every message accepted from a player or the local game, for logging and for kinds the host only observes
    /// (save done, replay failed, action refused).
    Received {
        from: u16,
        kind: MessageKind,
        payload: Vec<u8>,
    },
    PlayerJoined(PlayerInfo),
    PlayerLeft(PlayerInfo),
    ChecksumCompleted(ChecksumOutcome),
    /// The checksum policy (or a replay failure) wants a resync. Always emitted; the round starts by itself only
    /// with [`HostOptions::auto_resync`].
    ResyncRequested {
        reason: String,
    },
    /// The resync round changed state (`Idle` = finished and resumed). `detail` is for the host's UI.
    ResyncProgress {
        id: i32,
        state: ResyncState,
        detail: String,
    },
    /// The round gave up. The session stays paused; resume with `set_speed`.
    ResyncFailed {
        id: i32,
        reason: String,
    },
    Log(String),
}

#[derive(Debug)]
struct Connection {
    id: ConnectionId,
    nonce: [u8; 16],
    player: Option<u16>,
}

/// The host side of a session, as a state machine with no I/O: connections are integers the caller owns, frames
/// go in through [`HostSession::frame_received`] and every call returns the [`HostOutput`]s to act on. The host's
/// own game is player 0, reached through [`HostSession::local_game_message`] and
/// [`HostOutput::DeliverToLocalGame`]. Time comes from the `now` function given to the constructor, so tests can
/// drive it.
///
/// Thread-safety: none. The caller serialises all calls (one lock around the host), which also gives every game
/// the same message order.
pub struct HostSession {
    key: SessionKey,
    options: HostOptions,
    now: Box<dyn Fn() -> f64 + Send>,
    sealer: Sealer,
    opener: Opener,
    connections: BTreeMap<ConnectionId, Connection>,
    players: BTreeMap<u16, PlayerInfo>,
    clocks: BTreeMap<u16, PlayerClock>,
    checksums: ChecksumRounds,
    permissions: Permissions,
    policy: ChecksumPolicy,
    resync: Option<ResyncRound>,
    resync_counter: i32,
    last_running_speed: i32,
    pending_new_player: bool,
    dropped_once: HashSet<String>,
    next_player_id: u16,
    checksum_n: i32,
    last_session_broadcast: f64,
    last_checksum: f64,
    host_name: String,
    state: SessionState,
    actions_relayed: u64,
    dropped: u64,
    out: Vec<HostOutput>,
}

impl fmt::Debug for HostSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostSession")
            .field("host_name", &self.host_name)
            .field("state", &self.state)
            .field("players", &self.players)
            .field("connections", &self.connections)
            .finish_non_exhaustive()
    }
}

impl HostSession {
    pub const HOST_ID: u16 = 0;
    pub const UNASSIGNED: u16 = 0xFFFF;
    pub const PROTOCOL_VERSION: i32 = FrameHeader::CURRENT_VERSION as i32;

    /// `now` returns seconds on any monotonic clock.
    pub fn new(key: SessionKey, host_name: &str, now: Box<dyn Fn() -> f64 + Send>, options: HostOptions) -> Self {
        let host_name = Names::sanitize(host_name);
        let mut players = BTreeMap::new();
        players.insert(Self::HOST_ID, PlayerInfo { id: Self::HOST_ID, name: host_name.clone(), ready: false });
        Self {
            sealer: Sealer::new(&key, Self::HOST_ID),
            opener: Opener::new(&key),
            key,
            options,
            now,
            connections: BTreeMap::new(),
            players,
            clocks: BTreeMap::new(),
            checksums: ChecksumRounds::new(),
            permissions: Permissions::default(),
            policy: ChecksumPolicy::new(ChecksumPolicyConfig::default()),
            resync: None,
            resync_counter: 0,
            last_running_speed: 1,
            pending_new_player: false,
            dropped_once: HashSet::new(),
            next_player_id: 1,
            checksum_n: 0,
            last_session_broadcast: f64::NEG_INFINITY,
            last_checksum: f64::NEG_INFINITY,
            host_name,
            state: SessionState { started: false, speed: 0, pause_at: None, lead: Lockstep::lead_units(1) },
            actions_relayed: 0,
            dropped: 0,
            out: Vec::new(),
        }
    }

    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Every player, the host's game (id 0) included, by id.
    pub fn players(&self) -> &BTreeMap<u16, PlayerInfo> {
        &self.players
    }

    pub fn clocks(&self) -> &BTreeMap<u16, PlayerClock> {
        &self.clocks
    }

    pub fn permissions(&self) -> &Permissions {
        &self.permissions
    }

    pub fn actions_relayed(&self) -> u64 {
        self.actions_relayed
    }

    /// Frames refused so far (wrong sender, failed authentication, disallowed kind, before the handshake).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    fn now(&self) -> f64 {
        (self.now)()
    }

    fn drain(&mut self) -> Vec<HostOutput> {
        std::mem::take(&mut self.out)
    }

    fn log(&mut self, text: String) {
        self.out.push(HostOutput::Log(text));
    }

    // ---------------------------------------------------------------- connections

    pub fn connection_opened(&mut self, connection: ConnectionId) -> Vec<HostOutput> {
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce).expect("operating system random source");
        self.connections.insert(connection, Connection { id: connection, nonce, player: None });
        let challenge = ChallengeMessage {
            nonce: nonce.to_vec(),
            protocol: Self::PROTOCOL_VERSION,
            version: self.options.version.clone(),
        };
        self.send_unsealed(connection, MessageKind::Challenge, codec::encode(&challenge));
        self.drain()
    }

    pub fn connection_closed(&mut self, connection: ConnectionId) -> Vec<HostOutput> {
        if let Some(Connection { player: Some(id), .. }) = self.connections.remove(&connection) {
            if let Some(info) = self.players.remove(&id) {
                self.clocks.remove(&id);
                self.log(format!("{} left", info.name));
                let left = PlayerLeftMessage { id, name: info.name.clone() };
                self.broadcast(MessageKind::PlayerLeft, &codec::encode(&left), Some(id));
                self.out.push(HostOutput::PlayerLeft(info));
                self.resync_step();
            }
        }
        self.drain()
    }

    pub fn frame_received(&mut self, connection: ConnectionId, frame: &Frame) -> Vec<HostOutput> {
        let Some(c) = self.connections.get(&connection) else {
            return self.drain();
        };
        let kind_byte = frame.header.kind;
        let kind = MessageKind::from_u8(kind_byte);
        match c.player {
            None => {
                if kind != Some(MessageKind::Join) {
                    self.drop(connection, kind_byte, "not introduced".to_owned());
                } else {
                    self.handle_join(connection, frame);
                }
            }
            Some(player) => {
                if frame.header.sender != player {
                    let why = format!("sender id {} on the connection of player {player}", frame.header.sender);
                    self.drop(connection, kind_byte, why);
                    return self.drain();
                }
                match self.opener.try_open(frame) {
                    Err(err) => self.drop(connection, kind_byte, err.to_string()),
                    Ok(plain) => match kind {
                        Some(k) if k.allowed_from_player() => self.handle_message(player, k, &plain),
                        Some(_) => self.drop(connection, kind_byte, "host only".to_owned()),
                        None => self.drop(connection, kind_byte, "unknown kind".to_owned()),
                    },
                }
            }
        }
        self.drain()
    }

    fn handle_join(&mut self, connection: ConnectionId, frame: &Frame) {
        let Some(join) = codec::try_decode::<JoinMessage>(&frame.body) else {
            self.reject(connection, "malformed", None);
            return;
        };
        if join.version != self.options.version {
            let detail = format!("host runs {}, you run {}", self.options.version, join.version);
            self.reject(connection, "version", Some(&detail));
            return;
        }
        let nonce = self.connections[&connection].nonce;
        if !Self::verify_proof(&self.key, &nonce, &join.name, &join.proof) {
            self.reject(connection, "bad-proof", None);
            return;
        }
        if self.players.len() > self.options.max_players {
            let detail = format!("{} players", self.options.max_players);
            self.reject(connection, "full", Some(&detail));
            return;
        }
        if self.next_player_id == Self::UNASSIGNED {
            self.reject(connection, "full", Some("no ids left"));
            return;
        }

        let id = self.next_player_id;
        self.next_player_id += 1;
        let name = Names::unique(&Names::sanitize(&join.name), id, self.players.values().map(|p| p.name.as_str()));
        let info = PlayerInfo { id, name: name.clone(), ready: false };
        self.players.insert(id, info.clone());
        if let Some(c) = self.connections.get_mut(&connection) {
            c.player = Some(id);
        }
        self.log(format!("{name} joined (connection {connection})"));
        let welcome = WelcomeMessage {
            you: id,
            host_name: self.host_name.clone(),
            version: self.options.version.clone(),
            players: self.players.values().cloned().collect(),
            session: self.state.clone(),
        };
        self.send(connection, MessageKind::Welcome, &codec::encode(&welcome));
        let joined = PlayerJoinedMessage { player: info.clone() };
        self.broadcast(MessageKind::PlayerJoined, &codec::encode(&joined), Some(id));
        self.send(connection, MessageKind::Permissions, &codec::encode(&self.permissions));
        self.out.push(HostOutput::PlayerJoined(info));
        self.on_new_player();
    }

    fn reject(&mut self, connection: ConnectionId, reason: &str, detail: Option<&str>) {
        self.log(format!("connection {connection} rejected: {reason} {}", detail.unwrap_or("")));
        self.send_unsealed(connection, MessageKind::Reject, codec::encode(&RejectMessage::new(reason, detail)));
        self.connections.remove(&connection);
        self.out.push(HostOutput::CloseConnection(connection));
    }

    /// What a joiner must send back for a challenge: `HMAC-SHA256(key, nonce ‖ UTF8(name))`.
    pub fn proof(key: &SessionKey, nonce: &[u8], name: &str) -> [u8; 32] {
        Self::proof_mac(key, nonce, name).finalize().into_bytes().into()
    }

    /// Constant-time check of a proof.
    pub fn verify_proof(key: &SessionKey, nonce: &[u8], name: &str, proof: &[u8]) -> bool {
        Self::proof_mac(key, nonce, name).verify_slice(proof).is_ok()
    }

    fn proof_mac(key: &SessionKey, nonce: &[u8], name: &str) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(key.key()).expect("HMAC takes any key length");
        mac.update(nonce);
        mac.update(name.as_bytes());
        mac
    }

    fn drop(&mut self, connection: ConnectionId, kind: u8, why: String) {
        self.dropped += 1;
        let kind = kind_name(kind);
        if self.dropped_once.insert(format!("{connection}/{kind}/{why}")) {
            self.log(format!("connection {connection}: {kind} dropped ({why})"));
        }
    }

    // ---------------------------------------------------------------- the host's own game

    /// A message from the host's own game. It is trusted like the host itself, so every kind is accepted.
    pub fn local_game_message(&mut self, kind: MessageKind, payload: &[u8]) -> Vec<HostOutput> {
        self.handle_message(Self::HOST_ID, kind, payload);
        self.drain()
    }

    // ---------------------------------------------------------------- messages

    fn handle_message(&mut self, from: u16, kind: MessageKind, payload: &[u8]) {
        match kind {
            MessageKind::Hello => {
                if let Some(p) = self.players.get_mut(&from) {
                    p.ready = true;
                }
                self.log(format!("{} ready {}", self.name(from), String::from_utf8_lossy(payload)));
                self.resync_step();
                if from != Self::HOST_ID {
                    if let Some(connection) = self.connection_of(from) {
                        self.send(connection, MessageKind::Session, &codec::encode(&self.state));
                    }
                }
            }

            MessageKind::Clock => {
                if let Some(m) = codec::try_decode::<ClockMessage>(payload) {
                    self.clocks.insert(from, PlayerClock { t: m.t, speed: m.speed, at: self.now() });
                    self.resync_step();
                    let peer = PeerClockMessage { from, t: m.t, ahead: m.ahead };
                    self.broadcast(MessageKind::PeerClock, &codec::encode(&peer), Some(from));
                }
            }

            MessageKind::Action => {
                if let Some(mut m) = codec::try_decode::<ActionMessage>(payload) {
                    m.from = from;
                    m.category = m.normalized_category().to_owned();
                    m.allowed = self.permissions.allows_action(&m.category, from);
                    if !m.allowed && self.dropped_once.insert(format!("denied/{from}/{}", m.category)) {
                        let who = self.name(from);
                        self.log(format!("{who} may not {}; action relayed as refused", m.category));
                    }
                    self.broadcast(MessageKind::Action, &codec::encode(&m), Some(from));
                    self.actions_relayed += 1;
                }
            }

            MessageKind::Chat => {
                if let Some(m) = codec::try_decode::<ChatMessage>(payload) {
                    if !m.text.trim().is_empty() {
                        let text: String = m.text.chars().take(500).collect();
                        let chat = ChatMessage { text, from, name: Some(self.name(from)) };
                        self.broadcast(MessageKind::Chat, &codec::encode(&chat), Some(from));
                    }
                }
            }

            MessageKind::SpeedRequest => {
                if let Some(m) = codec::try_decode::<SpeedRequestMessage>(payload) {
                    if !self.permissions.speed.allows(from) {
                        self.dropped += 1;
                        if self.dropped_once.insert(format!("speed/{from}")) {
                            let who = self.name(from);
                            self.log(format!("speed request from {who} dropped (not allowed)"));
                        }
                    } else if self.resync.is_some() {
                        let who = self.name(from);
                        self.log(format!("speed request from {who} ignored (resync in progress)"));
                    } else if self.state.started {
                        let same = (m.speed > 0 && self.state.pause_at.is_none() && m.speed == self.state.speed)
                            || (m.speed <= 0 && self.state.pause_at.is_some());
                        if !same {
                            if m.speed <= 0 {
                                self.pause_inner();
                            } else {
                                self.set_speed_inner(m.speed);
                            }
                            let what = if m.speed <= 0 { "pause".to_owned() } else { format!("x{}", m.speed) };
                            self.log(format!("speed {what} asked by {}", self.name(from)));
                        }
                    }
                }
            }

            MessageKind::ChecksumReport => {
                if let Some(m) = codec::try_decode::<ChecksumReportMessage>(payload) {
                    if from == Self::HOST_ID {
                        if let Some(authority) = m.authority.as_ref().filter(|a| !a.is_empty()) {
                            let msg = AuthorityMessage { n: m.n, values: authority.clone() };
                            self.broadcast(MessageKind::Authority, &codec::encode(&msg), Some(Self::HOST_ID));
                        }
                    }
                    let expected: Vec<u16> = self.players.values().filter(|p| p.ready).map(|p| p.id).collect();
                    if let Some(outcome) = self.checksums.report(from, m, &expected) {
                        self.on_checksum_outcome(outcome);
                    }
                }
            }

            MessageKind::SaveDone => {
                if let Some(m) = codec::try_decode::<SaveDoneMessage>(payload) {
                    self.on_save_done(from, m);
                }
            }

            MessageKind::ReplayFailed => {
                if let Some(m) = codec::try_decode::<ReplayFailedMessage>(payload) {
                    self.on_replay_failed(from, &m.detail);
                }
            }

            _ => {}
        }
        self.out.push(HostOutput::Received { from, kind, payload: payload.to_vec() });
    }

    // ---------------------------------------------------------------- session control (the launcher's host UI)

    pub fn start(&mut self) -> Vec<HostOutput> {
        let mut times: Vec<i64> = self.clocks.values().map(|c| c.t).collect();
        times.sort_unstable();
        times.dedup();
        let common = if times.len() == 1 { Some(times[0]) } else { None };
        self.state = SessionState { started: true, speed: 0, pause_at: common, ..self.state.clone() };
        let ready = self.players.values().filter(|p| p.ready).count();
        let at = common.map_or("different times".to_owned(), |t| t.to_string());
        self.log(format!("session started with {ready} ready game(s) at {at}"));
        self.broadcast_session();
        self.request_checksum_inner();
        self.drain()
    }

    pub fn set_speed(&mut self, speed: i32) -> Vec<HostOutput> {
        if self.resync.is_some() {
            self.log("speed change ignored: resync in progress".to_owned());
        } else {
            self.set_speed_inner(speed);
        }
        self.drain()
    }

    pub fn pause(&mut self) -> Vec<HostOutput> {
        if self.resync.is_none() {
            self.pause_inner();
        }
        self.drain()
    }

    pub fn request_checksum(&mut self) -> Vec<HostOutput> {
        self.request_checksum_inner();
        self.drain()
    }

    /// Replaces who may build, bulldoze, spend and change speed, and tells every game (kind 20). Only the host's
    /// launcher can call this; a player cannot send the kind.
    pub fn set_permissions(&mut self, permissions: Permissions) -> Vec<HostOutput> {
        if permissions != self.permissions {
            self.permissions = permissions;
            self.log("permissions changed".to_owned());
        }
        self.broadcast(MessageKind::Permissions, &codec::encode(&self.permissions), None);
        self.drain()
    }

    /// Call regularly (every 250 ms or so): periodic session broadcast, checksum rounds and the resync round's
    /// waits and timeouts.
    pub fn tick(&mut self) -> Vec<HostOutput> {
        self.resync_step();
        let t = self.now();
        if t - self.last_session_broadcast >= self.options.session_broadcast_seconds {
            self.broadcast_session();
        }
        if self.state.started
            && self.state.pause_at.is_none()
            && t - self.last_checksum >= self.options.checksum_seconds
        {
            self.request_checksum_inner();
        }
        self.drain()
    }

    fn set_speed_inner(&mut self, speed: i32) {
        if !self.state.started {
            return;
        }
        let speed = speed.clamp(1, 4);
        self.last_running_speed = speed;
        self.state = SessionState { speed, pause_at: None, lead: Lockstep::lead_units(speed), ..self.state.clone() };
        self.broadcast_session();
    }

    fn pause_inner(&mut self) {
        if !self.state.started {
            return;
        }
        self.state.pause_at = Some(self.future_stamp());
        self.broadcast_session();
    }

    fn request_checksum_inner(&mut self) {
        self.checksum_n += 1;
        let n = self.checksum_n;
        self.checksums.open(n);
        let paused = matches!(self.state.pause_at, Some(p) if self.max_clock() >= p);
        let at = match self.state.pause_at {
            Some(p) if paused => p.max(self.clocks.values().map(|c| c.t).max().unwrap_or(0)),
            _ => self.future_stamp(),
        };
        let request = ChecksumRequestMessage { n, at, paused };
        self.broadcast(MessageKind::ChecksumRequest, &codec::encode(&request), None);
        self.last_checksum = self.now();
    }

    /// The fastest game's estimated clock now; 0 with no clocks.
    pub fn max_clock(&self) -> i64 {
        if self.clocks.is_empty() {
            return 0;
        }
        let now = self.now();
        self.clocks.values().map(|c| c.estimate(now)).fold(f64::NEG_INFINITY, f64::max) as i64
    }

    /// A stamp every game reaches after receiving a message sent now.
    pub fn future_stamp(&self) -> i64 {
        Lockstep::round_up_to_step((self.max_clock() + Lockstep::lead_units(self.state.speed.max(1))) as f64)
    }

    fn broadcast_session(&mut self) {
        self.broadcast(MessageKind::Session, &codec::encode(&self.state), None);
        self.last_session_broadcast = self.now();
    }

    // ---------------------------------------------------------------- sending

    /// To every player connection and the local game, except `except`. One sealer, so the host's counter stream is
    /// shared by all connections; a receiver only ever sees it increase.
    fn broadcast(&mut self, kind: MessageKind, payload: &[u8], except: Option<u16>) {
        let targets: Vec<ConnectionId> = self
            .connections
            .values()
            .filter(|c| matches!(c.player, Some(id) if Some(id) != except))
            .map(|c| c.id)
            .collect();
        for connection in targets {
            self.send(connection, kind, payload);
        }
        if except != Some(Self::HOST_ID) {
            self.out.push(HostOutput::DeliverToLocalGame { kind, payload: payload.to_vec() });
        }
    }

    fn send(&mut self, connection: ConnectionId, kind: MessageKind, payload: &[u8]) {
        let frame = self.sealer.seal(kind.into(), payload);
        self.out.push(HostOutput::SendToConnection { connection, frame });
    }

    fn send_unsealed(&mut self, connection: ConnectionId, kind: MessageKind, payload: Vec<u8>) {
        let header = FrameHeader::new(FrameHeader::CURRENT_VERSION, kind.into(), Self::UNASSIGNED, 0);
        self.out.push(HostOutput::SendToConnection { connection, frame: Frame::new(header, payload) });
    }

    fn connection_of(&self, player: u16) -> Option<ConnectionId> {
        self.connections.values().find(|c| c.player == Some(player)).map(|c| c.id)
    }

    fn name(&self, id: u16) -> String {
        self.players.get(&id).map_or_else(|| format!("#{id}"), |p| p.name.clone())
    }
}

fn kind_name(kind: u8) -> String {
    MessageKind::from_u8(kind).map_or_else(|| kind.to_string(), |k| format!("{k:?}"))
}
