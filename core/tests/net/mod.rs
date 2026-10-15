//! A host and its players wired together in memory, with a clock the test moves and a link the test can cut. Shared by
//! the limits, liveness, reconnect and roster tests. Frames are delivered at once, in the order the state machines
//! produce them; a pong takes the player's `latency` of host time to come back, so the host measures it.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::protocol::Frame;
use feverlink3_core::session::*;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub const SECRET: [u8; 16] = [3, 1, 4, 1, 5, 9, 2, 6, 5, 3, 5, 8, 9, 7, 9, 3];

pub fn json<T: Serialize>(m: &T) -> Vec<u8> {
    codec::encode(m)
}

/// One player's side: its session and everything that came out of it.
pub struct Peer {
    pub session: PlayerSession,
    /// What its game was given, in order.
    pub game: Vec<(MessageKind, Vec<u8>)>,
    pub log: Vec<String>,
    pub rejected: Option<RejectMessage>,
    pub welcomes: Vec<WelcomeMessage>,
    pub saves: Vec<(i32, String, Vec<u8>)>,
    /// Host time a pong takes to come back, seconds.
    pub latency: f64,
}

impl Peer {
    pub fn count(&self, kind: MessageKind) -> usize {
        self.game.iter().filter(|g| g.0 == kind).count()
    }

    pub fn all<T: DeserializeOwned>(&self, kind: MessageKind) -> Vec<T> {
        self.game.iter().filter(|g| g.0 == kind).map(|g| codec::try_decode(&g.1).expect("decodes")).collect()
    }

    pub fn last<T: DeserializeOwned>(&self, kind: MessageKind) -> T {
        self.all(kind).pop().unwrap_or_else(|| panic!("no {kind:?}"))
    }

    pub fn id(&self) -> u16 {
        self.session.id().expect("welcomed")
    }
}

pub struct Net {
    pub key: SessionKey,
    pub now: Arc<Mutex<f64>>,
    pub host: HostSession,
    pub host_game: Vec<(MessageKind, Vec<u8>)>,
    pub host_log: Vec<String>,
    /// Connections the host asked to have closed, in order.
    pub closed: Vec<ConnectionId>,
    pub events: Vec<HostOutput>,
    pub peers: BTreeMap<ConnectionId, Peer>,
    /// Connections whose frames still get through, both ways.
    pub reachable: BTreeSet<ConnectionId>,
    /// Unsealed frames the host sent to connections that are not peers of this harness (raw connections).
    pub raw: BTreeMap<ConnectionId, Vec<Frame>>,
    /// (sender, counter) of every frame a player's session sealed, in order, whether or not it got through.
    pub uplink: Vec<(u16, u64)>,
    next_connection: ConnectionId,
}

impl Net {
    pub fn new() -> Self {
        Self::with(HostOptions::default())
    }

    pub fn with(options: HostOptions) -> Self {
        let key = SessionKey::derive(&SECRET, None);
        let now = Arc::new(Mutex::new(1000.0));
        let clock = Arc::clone(&now);
        let host = HostSession::new(key.clone(), "Host", Box::new(move || *clock.lock().unwrap()), options);
        Self {
            key,
            now,
            host,
            host_game: Vec::new(),
            host_log: Vec::new(),
            closed: Vec::new(),
            events: Vec::new(),
            peers: BTreeMap::new(),
            reachable: BTreeSet::new(),
            raw: BTreeMap::new(),
            uplink: Vec::new(),
            next_connection: 0,
        }
    }

    pub fn clock(&self) -> impl Fn() -> f64 + Send + 'static {
        let now = Arc::clone(&self.now);
        move || *now.lock().unwrap()
    }

    pub fn time(&self) -> f64 {
        *self.now.lock().unwrap()
    }

    pub fn advance(&mut self, seconds: f64) {
        *self.now.lock().unwrap() += seconds;
    }

    /// Moves the clock in `step` seconds, ticking the host after each, until `seconds` have passed.
    pub fn run_for(&mut self, seconds: f64, step: f64) {
        let mut left = seconds;
        while left > 1e-9 {
            let s = step.min(left);
            self.advance(s);
            self.tick();
            left -= s;
        }
    }

    pub fn tick(&mut self) {
        let outs = self.host.tick();
        self.pump_host(outs);
    }

    // ---------------------------------------------------------------- connections

    pub fn connect(&mut self, name: &str) -> ConnectionId {
        let session = PlayerSession::with_clock(self.key.clone(), name, "0.1", Box::new(self.clock()));
        self.attach(session)
    }

    /// A new connection of a player coming back with the ticket of its old one.
    pub fn connect_resuming(&mut self, name: &str, ticket: ResumeTicket) -> ConnectionId {
        let session = PlayerSession::resuming_with_clock(self.key.clone(), name, "0.1", ticket, Box::new(self.clock()));
        self.attach(session)
    }

    pub fn attach(&mut self, session: PlayerSession) -> ConnectionId {
        self.next_connection += 1;
        let c = self.next_connection;
        let peer = Peer {
            session,
            game: Vec::new(),
            log: Vec::new(),
            rejected: None,
            welcomes: Vec::new(),
            saves: Vec::new(),
            latency: 0.0,
        };
        self.peers.insert(c, peer);
        self.reachable.insert(c);
        let outs = self.host.connection_opened(c);
        self.pump_host(outs);
        c
    }

    /// A connection with no player behind it: the harness keeps the unsealed frames the host sends it.
    pub fn open_raw(&mut self) -> ConnectionId {
        self.next_connection += 1;
        let c = self.next_connection;
        self.reachable.insert(c);
        let outs = self.host.connection_opened(c);
        self.pump_host(outs);
        c
    }

    /// The transport reports the connection lost, both ways.
    pub fn lose(&mut self, c: ConnectionId) {
        self.reachable.remove(&c);
        let outs = self.host.connection_closed(c);
        self.pump_host(outs);
    }

    /// The link dies without anyone noticing: nothing gets through either way and the host is not told.
    pub fn cut(&mut self, c: ConnectionId) {
        self.reachable.remove(&c);
    }

    pub fn peer(&self, c: ConnectionId) -> &Peer {
        &self.peers[&c]
    }

    pub fn id(&self, c: ConnectionId) -> u16 {
        self.peers[&c].id()
    }

    pub fn ticket(&self, c: ConnectionId) -> ResumeTicket {
        self.peers[&c].session.ticket().expect("a ticket").clone()
    }

    pub fn set_latency(&mut self, c: ConnectionId, seconds: f64) {
        self.peers.get_mut(&c).unwrap().latency = seconds;
    }

    // ---------------------------------------------------------------- messages

    /// A message from a player's game.
    pub fn send(&mut self, c: ConnectionId, kind: MessageKind, payload: &[u8]) {
        let outs = self.peers.get_mut(&c).unwrap().session.local_game_message(kind, payload);
        self.pump_player(c, outs);
    }

    /// A message from the host's own game.
    pub fn host_send(&mut self, kind: MessageKind, payload: &[u8]) {
        let outs = self.host.local_game_message(kind, payload);
        self.pump_host(outs);
    }

    pub fn host_call(&mut self, call: impl FnOnce(&mut HostSession) -> Vec<HostOutput>) {
        let outs = call(&mut self.host);
        self.pump_host(outs);
    }

    /// A frame handed to the host as if it came over connection `c`.
    pub fn inject(&mut self, c: ConnectionId, frame: &Frame) {
        let outs = self.host.frame_received(c, frame);
        self.pump_host(outs);
    }

    pub fn leave(&mut self, c: ConnectionId) {
        let outs = self.peers.get_mut(&c).unwrap().session.leave();
        self.pump_player(c, outs);
    }

    pub fn hello(&mut self, c: ConnectionId, t: i64, speed: i32) {
        self.send(c, MessageKind::Hello, &json(&HelloMessage::new(t)));
        self.send(
            c,
            MessageKind::Clock,
            &json(&ClockMessage { t, speed, ahead: 3, need: None, safe: None, pause: false }),
        );
    }

    pub fn host_hello(&mut self, t: i64, speed: i32) {
        self.host_send(MessageKind::Hello, &json(&HelloMessage::new(t)));
        self.host_send(
            MessageKind::Clock,
            &json(&ClockMessage { t, speed, ahead: 3, need: None, safe: None, pause: false }),
        );
    }

    /// Every game says hello and reports a clock at `t`.
    pub fn hello_all(&mut self, t: i64, speed: i32) {
        self.host_hello(t, speed);
        let live: Vec<ConnectionId> = self.reachable.iter().copied().filter(|c| self.peers.contains_key(c)).collect();
        for c in live {
            if self.peers[&c].session.state() == PlayerState::Joined {
                self.hello(c, t, speed);
            }
        }
    }

    /// Every connected game reports a stopped clock at `t`.
    pub fn stop_all_at(&mut self, t: i64) {
        self.host_send(
            MessageKind::Clock,
            &json(&ClockMessage { t, speed: 0, ahead: 3, need: None, safe: None, pause: false }),
        );
        let live: Vec<ConnectionId> = self.reachable.iter().copied().filter(|c| self.peers.contains_key(c)).collect();
        for c in live {
            if self.peers[&c].session.state() == PlayerState::Joined {
                self.send(
                    c,
                    MessageKind::Clock,
                    &json(&ClockMessage { t, speed: 0, ahead: 3, need: None, safe: None, pause: false }),
                );
            }
        }
    }

    /// `players` connected and ready games, the session started and running at `speed`, all at game time 10 000.
    pub fn running(players: usize, speed: i32, options: HostOptions) -> (Self, Vec<ConnectionId>) {
        let mut n = Self::with(options);
        let ids: Vec<ConnectionId> = (0..players).map(|i| n.connect(&format!("P{i}"))).collect();
        n.hello_all(10_000, 0);
        n.host_call(HostSession::start);
        n.host_call(|h| h.set_speed(speed));
        n.hello_all(10_000, speed);
        (n, ids)
    }

    // ---------------------------------------------------------------- plumbing

    pub fn pump_host(&mut self, outs: Vec<HostOutput>) {
        for o in outs {
            match &o {
                HostOutput::SendToConnection { connection, frame } => {
                    if self.reachable.contains(connection) {
                        if let Some(peer) = self.peers.get_mut(connection) {
                            let outs = peer.session.frame_received(frame);
                            self.pump_player(*connection, outs);
                        } else {
                            self.raw.entry(*connection).or_default().push(frame.clone());
                        }
                    }
                }
                HostOutput::CloseConnection(c) => {
                    self.closed.push(*c);
                    self.reachable.remove(c);
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
                    if frame.header.sender != HostSession::UNASSIGNED {
                        self.uplink.push((frame.header.sender, frame.header.counter));
                    }
                    if !self.reachable.contains(&c) {
                        continue;
                    }
                    // the pong takes `latency` to get back; undone afterwards so that pings sent in the same tick
                    // are measured side by side and not one after the other
                    let latency =
                        if frame.header.kind == u8::from(MessageKind::Pong) { self.peers[&c].latency } else { 0.0 };
                    self.advance(latency);
                    let outs = self.host.frame_received(c, &frame);
                    self.pump_host(outs);
                    self.advance(-latency);
                }
                PlayerOutput::DeliverToLocalGame { kind, payload } => {
                    self.peers.get_mut(&c).unwrap().game.push((kind, payload));
                }
                PlayerOutput::Log(line) => self.peers.get_mut(&c).unwrap().log.push(line),
                PlayerOutput::SaveReceived { id, sha256, bytes } => {
                    self.peers.get_mut(&c).unwrap().saves.push((id, sha256, bytes));
                }
                PlayerOutput::Welcomed(w) => self.peers.get_mut(&c).unwrap().welcomes.push(w),
                PlayerOutput::Rejected(r) => self.peers.get_mut(&c).unwrap().rejected = Some(r),
            }
        }
    }

    // ---------------------------------------------------------------- what the host's game saw

    pub fn host_game_all<T: DeserializeOwned>(&self, kind: MessageKind) -> Vec<T> {
        self.host_game.iter().filter(|g| g.0 == kind).map(|g| codec::try_decode(&g.1).expect("decodes")).collect()
    }

    pub fn host_game_count(&self, kind: MessageKind) -> usize {
        self.host_game.iter().filter(|g| g.0 == kind).count()
    }

    pub fn logged(&self, needle: &str) -> bool {
        self.host_log.iter().any(|l| l.contains(needle))
    }

    /// How many frames of `kind` were sent to connection `c` so far.
    pub fn frames_to(&self, c: ConnectionId, kind: MessageKind) -> usize {
        self.events
            .iter()
            .filter(|e| {
                matches!(e, HostOutput::SendToConnection { connection, frame }
                    if *connection == c && frame.header.kind == u8::from(kind))
            })
            .count()
    }
}

pub fn parts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
}

/// A deterministic blob of `len` bytes.
pub fn blob(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}
