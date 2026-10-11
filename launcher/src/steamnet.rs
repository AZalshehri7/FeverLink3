//! Steam peer-to-peer connectivity.
//!
//! Steam can connect two players by Steam ID, punching through NATs and falling back to its own relays (Steam
//! Datagram Relay), without any port forwarding and without a server of ours. The functions come from the *flat*
//! Steam API (`SteamAPI_ISteamNetworkingSockets_*`), looked up by name in the `steam_api64.dll` the game has already
//! loaded, so no Steam SDK is linked and nothing runs unless the game has Steam loaded.
//!
//! - This file holds the layouts of the Steam structures the code reads and writes (as offsets next to pure parsers
//!   with tests, because getting one wrong is a silent crash), the ping protocol, and the [`Probe`]: a service that
//!   listens on virtual port 1, answers every ping, and can ping a friend's Steam ID and report the round trip. It
//!   carries no session traffic; it shows, on real machines, whether Steam can connect two players.
//! - [`SteamCore`] (in `steam_core.rs`) owns the interfaces, the one global callback and the one poller thread that
//!   every Steam user in the process shares. The probe is one of its owners; the session transport
//!   ([`crate::steam_transport`], virtual port 0) is another.
//!
//! The layouts were checked against `steamnetworkingtypes.h` of ValveSoftware/GameNetworkingSockets on 2026-10-10
//! and the pieces the probe uses were proven in the game on 2026-10-09.

mod steam_core;

/// The speed test: message format, timing math, the sender loop, the text of the result.
pub mod throughput;

/// Bulk transfer over unreliable datagrams with our own reliability and congestion control.
pub mod bulk;

/// Threads that connect the bulk transfer to a Steam connection.
pub mod bulk_io;

/// The speed test engine of the probe: one run in two directions, reliable or as datagrams.
pub mod speed;

/// An in-memory Steam for the tests of this module and of the transport.
#[cfg(test)]
pub(crate) mod fake;

use std::collections::VecDeque;

use speed::{Incoming, Phase, SpeedRun};
pub use speed::{specs_for, RunKind, RunOutcome, RunSpec};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use steam_core::{
    global, send_rate_setting, Cause, Config, CoreError, CoreRef, CoreStatus, End, Handle, LinkSnapshot, Owner, Route,
    SendOutcome, SteamCore, MAX_MESSAGE, SEND_RATE_ENV,
};

// ---- layouts of the Steam structures (Steamworks SDK, `#pragma pack(8)`); pure, tested

/// `SteamNetworkingIdentity`: type, size, then a union of 128 bytes.
pub const IDENTITY_SIZE: usize = 136;
/// `k_ESteamNetworkingIdentityType_SteamID`.
const IDENTITY_TYPE_STEAMID: i32 = 16;

/// The identity of a Steam account, in the layout `ConnectP2P` takes.
pub fn identity_bytes(steam_id: u64) -> [u8; IDENTITY_SIZE] {
    let mut b = [0u8; IDENTITY_SIZE];
    b[0..4].copy_from_slice(&IDENTITY_TYPE_STEAMID.to_le_bytes());
    b[4..8].copy_from_slice(&8i32.to_le_bytes());
    b[8..16].copy_from_slice(&steam_id.to_le_bytes());
    b
}

/// The Steam ID in an identity buffer, `None` for another identity type.
pub fn steam_id_of(identity: &[u8]) -> Option<u64> {
    if le_i32(identity, 0)? != IDENTITY_TYPE_STEAMID {
        return None;
    }
    Some(u64::from_le_bytes(identity.get(8..16)?.try_into().ok()?))
}

/// Size of `SteamNetConnectionInfo_t`.
pub const CONNECTION_INFO_SIZE: usize = 696;
/// Offsets inside `SteamNetConnectionInfo_t` (the identity of the peer is at 0).
const INFO_LISTEN_SOCKET: usize = 144;
const INFO_POP_RELAY: usize = 172;
const INFO_STATE: usize = 176;
const INFO_END_REASON: usize = 180;
/// `m_szEndDebug`, 128 bytes.
const INFO_END_DEBUG: usize = 184;
const END_DEBUG_SIZE: usize = 128;
/// `m_nFlags`; bit 16 is `k_nSteamNetworkConnectionInfoFlags_Relayed`.
const INFO_FLAGS: usize = 440;
const INFO_FLAG_RELAYED: u32 = 16;
/// `SteamNetConnectionStatusChangedCallback_t`: connection handle, then the info (8-aligned), then the old state.
const CALLBACK_INFO: usize = 8;
const CALLBACK_MIN_SIZE: usize = CALLBACK_INFO + CONNECTION_INFO_SIZE + 4;

fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

fn le_i32(b: &[u8], at: usize) -> Option<i32> {
    le_u32(b, at).map(|v| v as i32)
}

/// Text up to the first NUL, lossy, at most `bytes.len()` long.
fn c_text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(bytes.get(..end).unwrap_or_default()).trim().to_string()
}

/// Connection states (`ESteamNetworkingConnectionState`).
pub mod state {
    pub const NONE: i32 = 0;
    pub const CONNECTING: i32 = 1;
    pub const FINDING_ROUTE: i32 = 2;
    pub const CONNECTED: i32 = 3;
    pub const CLOSED_BY_PEER: i32 = 4;
    pub const PROBLEM_DETECTED_LOCALLY: i32 = 5;
}

/// What `GetConnectionInfo` (or the info inside a status callback) says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnInfo {
    /// The peer's Steam ID, `None` when Steam does not know it (yet) or the identity is not a Steam ID.
    pub remote: Option<u64>,
    /// The listen socket the connection arrived on, 0 for one we opened.
    pub listen_socket: u32,
    /// The relay point (0 when the route is direct).
    pub relay_pop: u32,
    pub state: i32,
    pub end_reason: i32,
    /// Steam's own text for why the connection ended; for logs, not for players.
    pub end_debug: String,
    /// Through a relay, by the relay point or by Steam's flag.
    pub relayed: bool,
}

/// Reads a `SteamNetConnectionInfo_t`. `None` when the buffer is shorter than the structure.
pub fn parse_connection_info(info: &[u8]) -> Option<ConnInfo> {
    if info.len() < CONNECTION_INFO_SIZE {
        return None;
    }
    let relay_pop = le_u32(info, INFO_POP_RELAY)?;
    Some(ConnInfo {
        remote: steam_id_of(info),
        listen_socket: le_u32(info, INFO_LISTEN_SOCKET)?,
        relay_pop,
        state: le_i32(info, INFO_STATE)?,
        end_reason: le_i32(info, INFO_END_REASON)?,
        end_debug: c_text(info.get(INFO_END_DEBUG..INFO_END_DEBUG + END_DEBUG_SIZE)?),
        relayed: relay_pop != 0 || le_u32(info, INFO_FLAGS)? & INFO_FLAG_RELAYED != 0,
    })
}

/// What the status-changed callback reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEvent {
    pub conn: u32,
    pub state: i32,
    pub old_state: i32,
    /// The listen socket the connection arrived on, 0 for one we opened.
    pub listen_socket: u32,
    pub end_reason: i32,
    /// The peer's Steam ID when the callback names one.
    pub remote: Option<u64>,
    /// Steam's text for why the connection ended.
    pub end_debug: String,
}

/// Reads the callback structure. `None` when it is too short or holds a state Steam does not define.
pub fn parse_status_event(buf: &[u8]) -> Option<StatusEvent> {
    if buf.len() < CALLBACK_MIN_SIZE {
        return None;
    }
    let info = parse_connection_info(buf.get(CALLBACK_INFO..CALLBACK_INFO + CONNECTION_INFO_SIZE)?)?;
    let event = StatusEvent {
        conn: le_u32(buf, 0)?,
        state: info.state,
        old_state: le_i32(buf, CALLBACK_INFO + CONNECTION_INFO_SIZE)?,
        listen_socket: info.listen_socket,
        end_reason: info.end_reason,
        remote: info.remote,
        end_debug: info.end_debug,
    };
    let valid = |s: i32| (-1..=5).contains(&s);
    (valid(event.state) && valid(event.old_state)).then_some(event)
}

/// The relay point of a connection (0 when the route is direct), from a `GetConnectionInfo` buffer.
pub fn relay_pop(info: &[u8]) -> u32 {
    le_u32(info, INFO_POP_RELAY).unwrap_or(0)
}

/// Bytes to give `GetConnectionRealTimeStatus`: the structure is 120 bytes, the buffer leaves room for growth.
pub const REAL_TIME_STATUS_BUFFER: usize = 256;
/// Offsets inside `SteamNetConnectionRealTimeStatus_t`.
const RT_STATE: usize = 0;
const RT_PING: usize = 4;
const RT_QUALITY_LOCAL: usize = 8;
const RT_QUALITY_REMOTE: usize = 12;
const RT_OUT_PPS: usize = 16;
const RT_OUT_BPS: usize = 20;
const RT_IN_PPS: usize = 24;
const RT_IN_BPS: usize = 28;
const RT_SEND_RATE: usize = 32;
const RT_PENDING_UNRELIABLE: usize = 36;
const RT_PENDING_RELIABLE: usize = 40;
const RT_SENT_UNACKED: usize = 44;
const RT_QUEUE_TIME: usize = 48;

/// What `GetConnectionRealTimeStatus` says about a connection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RealTime {
    pub state: i32,
    /// Steam's round-trip estimate in ms (`m_nPing`); `None` while it has none (Steam reports a negative number).
    pub ping_ms: Option<u32>,
    /// Packets delivered end to end in order, as measured here and as the other side reports, 0 to 1.
    pub quality_local: f32,
    pub quality_remote: f32,
    /// The rate Steam sends at, bytes per second (`m_nSendRateBytesPerSecond`); it shows whether the send-rate
    /// tuning took effect (Steam's default is 262 144).
    pub send_rate: u32,
    /// Reliable bytes queued and not yet on the wire.
    pub pending_reliable: u32,
    pub pending_unreliable: u32,
    /// Reliable bytes on the wire and not yet acknowledged.
    pub sent_unacked_reliable: u32,
    pub out_packets_per_sec: f32,
    pub out_bytes_per_sec: f32,
    pub in_packets_per_sec: f32,
    pub in_bytes_per_sec: f32,
    /// How long a message queued now would wait before it goes on the wire, microseconds.
    pub queue_time_us: i64,
}

/// Reads a `SteamNetConnectionRealTimeStatus_t`. `None` when the buffer is too short.
pub fn parse_real_time_status(buf: &[u8]) -> Option<RealTime> {
    let float_at = |at: usize| le_u32(buf, at).map(f32::from_bits);
    Some(RealTime {
        state: le_i32(buf, RT_STATE)?,
        ping_ms: u32::try_from(le_i32(buf, RT_PING)?).ok(),
        quality_local: float_at(RT_QUALITY_LOCAL)?,
        quality_remote: float_at(RT_QUALITY_REMOTE)?,
        send_rate: u32::try_from(le_i32(buf, RT_SEND_RATE)?).unwrap_or(0),
        pending_reliable: u32::try_from(le_i32(buf, RT_PENDING_RELIABLE)?).unwrap_or(0),
        pending_unreliable: le_i32(buf, RT_PENDING_UNRELIABLE).and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
        sent_unacked_reliable: le_i32(buf, RT_SENT_UNACKED).and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
        out_packets_per_sec: float_at(RT_OUT_PPS).unwrap_or(0.0),
        out_bytes_per_sec: float_at(RT_OUT_BPS).unwrap_or(0.0),
        in_packets_per_sec: float_at(RT_IN_PPS).unwrap_or(0.0),
        in_bytes_per_sec: float_at(RT_IN_BPS).unwrap_or(0.0),
        queue_time_us: buf
            .get(RT_QUEUE_TIME..RT_QUEUE_TIME + 8)
            .and_then(|b| b.try_into().ok())
            .map_or(0, i64::from_le_bytes),
    })
}

/// A sentence for one of Steam's connection end reasons (`ESteamNetConnectionEnd`), the number always included so
/// a log line can be looked up in `steamnetworkingtypes.h`.
pub fn end_reason_text(code: i32) -> String {
    let what = match code {
        0 => return "no reason given".to_string(),
        1000..=1999 => "closed by the other side's program",
        2000..=2999 => "the other side's program failed",
        3001 => "Steam is in offline mode",
        3002..=3004 => "Steam network configuration problem",
        3005 => "this account may not connect",
        3006 | 4007 => "no public address for a direct connection",
        3000..=3999 => "local network or Steam problem",
        4001 => "the other side stopped answering",
        4002 | 4003 => "the other side failed to authenticate",
        4006 => "the other side runs an incompatible Steam version",
        4000..=4999 => "problem on the other side",
        5003 => "timed out",
        5005 => "no connection to Steam",
        5006 => "no Steam relay available",
        5008 => "Steam could not arrange the connection with the other side",
        5009 => "a NAT or firewall blocked the connection",
        5010 => "the other side does not know this connection",
        5000..=5999 => "Steam networking problem",
        _ => "unknown reason",
    };
    format!("{what} ({code})")
}

// ---- the ping protocol

/// The answer to a received message: `ping <id>` is answered with `pong <id>`.
pub fn reply_for(payload: &[u8]) -> Option<Vec<u8>> {
    let rest = payload.strip_prefix(b"ping ")?;
    let mut out = b"pong ".to_vec();
    out.extend_from_slice(rest);
    Some(out)
}

/// The `<id>` of a `pong <id>` message.
pub fn pong_id(payload: &[u8]) -> Option<u64> {
    std::str::from_utf8(payload.strip_prefix(b"pong ")?).ok()?.trim().parse().ok()
}

/// What the probe reports to the window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// This account's Steam ID, once Steam answered.
    pub steam_id: Option<u64>,
    /// `waiting for Steam`, `ready` or an error text.
    pub state: String,
    /// The latest events, newest last (at most 8).
    pub events: Vec<String>,
    /// The connection test: target, state, pongs received, last round trip in ms, route.
    pub test: Option<TestReport>,
    /// Pings other players sent us.
    pub pings_received: u32,
    /// Players connected to our probe now, with the route and ping Steam measured.
    pub peers: Vec<PeerReport>,
    /// The speed test, once one was started.
    pub speed: Option<SpeedReport>,
}

/// What the window shows about the speed test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeedReport {
    /// Identifies the run (its start time); the controller logs each finished run once.
    pub id: u64,
    pub running: bool,
    /// One line: the progress while it runs, the reason when it failed, the headline when it is done.
    pub status: String,
    /// The result lines of a finished run.
    pub lines: Vec<String>,
    /// The one line for the session log, once the run has ended.
    pub log: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestReport {
    pub target: u64,
    pub state: String,
    pub pongs: u32,
    /// The probe's own round trip: ping to pong, as seen by the polling on both ends.
    pub last_rtt_ms: Option<u64>,
    /// `direct` or `through a Steam relay`, measured again every second (Steam can upgrade a relay to a direct path).
    pub route: String,
    /// Steam's own round-trip estimate for the connection, without the probe's polling.
    pub ping_ms: Option<u32>,
    /// The rate Steam sends at, bytes per second: 1 048 576 when the send-rate tuning took effect, Steam's own
    /// 262 144 when it did not.
    pub send_rate: Option<u32>,
}

/// A player who connected to our probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerReport {
    pub steam_id: Option<u64>,
    pub route: String,
    pub ping_ms: Option<u32>,
    pub send_rate: Option<u32>,
}

/// Parses the Steam ID a player pasted: digits only (the 17-digit `steamID64`).
pub fn parse_steam_id(text: &str) -> Option<u64> {
    let t = text.trim();
    (t.len() >= 16 && t.len() <= 20 && t.bytes().all(|b| b.is_ascii_digit())).then(|| t.parse().ok()).flatten()
}

/// Newest-last ring of event texts.
#[derive(Default)]
struct Events(VecDeque<String>);

impl Events {
    fn push(&mut self, text: String) {
        if self.0.len() == 8 {
            self.0.pop_front();
        }
        self.0.push_back(text);
    }
}

// ---- the probe

/// The virtual port the probe listens on and tests; the session transport uses 0.
pub const PROBE_PORT: i32 = 1;
/// Pings sent per test, one every [`PING_EVERY`].
const PROBE_PINGS: u32 = 20;
const PING_EVERY: Duration = Duration::from_secs(2);
/// A refused listen socket is tried again after this long.
const LISTEN_RETRY: Duration = Duration::from_secs(5);
/// Longest ping the probe answers.
const PING_LIMIT: usize = 4096;
/// Players that may be connected to the probe's listen socket at once; the next one is closed on arrival.
const MAX_PEERS: usize = 16;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

struct TestConn {
    conn: Handle,
    target: u64,
    connected: bool,
    pongs: u32,
    last_rtt_ms: Option<u64>,
    next_ping: Instant,
    sent: u32,
    route: String,
    ping_ms: Option<u32>,
    send_rate: Option<u32>,
    state: String,
}

/// A player connected to the probe's listen socket.
struct Peer {
    conn: Handle,
    steam_id: Option<u64>,
    link: Option<LinkSnapshot>,
}

#[derive(Default)]
struct ProbeState {
    events: Events,
    test: Option<TestConn>,
    peers: Vec<Peer>,
    pings_received: u32,
    speed: Option<SpeedRun>,
    /// The last run's outcome, once it has ended.
    outcome: Option<RunOutcome>,
    incoming: Option<Incoming>,
    senders: usize,
    /// A datagram upload from a friend that we are receiving (its transfer id).
    dg_in: Option<u32>,
    /// Tests the tester asked us to stop sending.
    stopped_ids: std::collections::HashSet<u64>,
    last_run_id: u64,
    /// A series of runs (the window's "both" or "parallel"): the ones still to run, the result blocks so far.
    plan: VecDeque<RunSpec>,
    plan_peer: u64,
    plan_total: usize,
    plan_blocks: Vec<String>,
    plan_collected: u64,
    /// Lines for the session log, taken by [`Probe::take_log`].
    log: Vec<String>,
}

/// The probe's side of the core: what happens to its connections, on the poller thread.
struct ProbeService {
    core: CoreRef,
    me: std::sync::Weak<ProbeService>,
    state: Mutex<ProbeState>,
    /// Routes the bulk datagrams of this probe's connections.
    hub: Arc<bulk_io::Hub>,
    /// Acts like an older program that does not know the rate request (tests).
    legacy: std::sync::atomic::AtomicBool,
}

impl ProbeService {
    fn is_test(state: &ProbeState, conn: Handle) -> bool {
        state.test.as_ref().is_some_and(|t| t.conn == conn)
    }
}

impl Owner for ProbeService {
    fn opened(&self, conn: Handle, _incoming: bool, remote: Option<u64>) {
        let route = self.core.route(conn).map_or("unknown", Route::text);
        let mut guard = lock(&self.state);
        let state = &mut *guard;
        if let Some(report) = self.core.config_report(conn) {
            self.log(state, format!("steam connection {conn} ({}): settings at its start: {report}", if _incoming { "incoming" } else { "outgoing" }));
        }
        if let Some(extra) = state.speed.as_mut().and_then(|r| r.extras.iter_mut().find(|(h, _)| *h == conn)) {
            extra.1 = true;
            return;
        }
        if let Some(t) = state.test.as_mut().filter(|t| t.conn == conn) {
            t.connected = true;
            t.state = "connected".into();
            t.route = route.into();
            t.next_ping = Instant::now();
            state.events.push("test connection established".into());
        } else if state.peers.len() >= MAX_PEERS {
            // anyone who knows an account's Steam ID can connect to the probe: it holds a few, not as many as asked
            state.events.push("a connection was turned away: the probe is full".into());
            drop(guard);
            self.core.close(conn);
        } else {
            state.peers.push(Peer { conn, steam_id: remote, link: None });
            state.events.push("a player connected".into());
        }
    }

    fn progress(&self, conn: Handle, step: i32) {
        let mut state = lock(&self.state);
        if let Some(t) = state.test.as_mut().filter(|t| t.conn == conn) {
            if step == self::state::FINDING_ROUTE {
                t.state = "finding a route".into();
            }
        }
    }

    fn message(&self, conn: Handle, data: &[u8]) {
        if let Some(msg) = throughput::parse(data) {
            return self.tput(conn, msg);
        }
        if data.len() > PING_LIMIT {
            return;
        }
        if let Some(reply) = reply_for(data) {
            {
                let mut state = lock(&self.state);
                state.pings_received = state.pings_received.saturating_add(1);
                state.events.push("received a ping, answered".into());
            }
            // a full send buffer only loses one pong; the pinger asks again
            let _ = self.core.send(conn, &reply);
        } else if let Some(sent) = pong_id(data) {
            if let Some(t) = lock(&self.state).test.as_mut().filter(|t| t.conn == conn) {
                t.pongs = t.pongs.saturating_add(1);
                t.last_rtt_ms = Some(now_ms().saturating_sub(sent));
            }
        }
    }

    fn datagram(&self, conn: Handle, data: &[u8]) {
        self.hub.on_datagram(conn, data);
    }

    fn closed(&self, conn: Handle, end: End) {
        self.hub.drop_connection(conn);
        let mut state = lock(&self.state);
        state.peers.retain(|p| p.conn != conn);
        let ours = Self::is_test(&state, conn);
        if state.incoming.as_ref().is_some_and(|i| i.conn == conn) {
            state.incoming = None;
        }
        if end.cause != Cause::Local {
            if let Some(run) = state.speed.as_mut().filter(|r| r.owns(conn) && r.active()) {
                run.phase = Phase::Failed(format!("the connection ended: {}", end.describe()));
            }
        }
        if end.cause == Cause::Local {
            // we closed it: a replaced test, or the probe going away
            return;
        }
        state.events.push(format!("connection ended: {}", end.describe()));
        if ours {
            if let Some(t) = state.test.as_mut() {
                t.connected = false;
                t.state = format!("failed: {}", end.describe());
            }
        }
    }
}

/// The Steam connection probe: listens on [`PROBE_PORT`], answers pings and tests a friend's Steam ID. The window
/// calls [`Probe::poll`] every 100 ms; everything else happens on the core's thread.
pub struct Probe {
    core: CoreRef,
    service: Arc<ProbeService>,
    listener: Option<Handle>,
    steam_id: Option<u64>,
    state: String,
    started: Instant,
    last_note: String,
    next_listen: Instant,
}

impl Probe {
    /// `None` when the process has no Steam API loaded (a test run, a game started without Steam).
    pub fn load() -> Option<Probe> {
        global().map(Probe::on)
    }

    /// A probe on a given core (the tests use a fake one).
    pub fn on(core: CoreRef) -> Probe {
        let service = Arc::new_cyclic(|me| ProbeService {
            core: core.clone(),
            me: me.clone(),
            state: Mutex::new(ProbeState::default()),
            hub: bulk_io::Hub::new(),
            legacy: std::sync::atomic::AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&service);
        service.hub.set_acceptor(move |conn, xfer, offer, rx| {
            if let Some(service) = weak.upgrade() {
                service.accept_dgram(conn, xfer, offer, rx);
            }
        });
        Probe {
            core,
            service,
            listener: None,
            steam_id: None,
            state: "waiting for Steam".into(),
            started: Instant::now(),
            last_note: String::new(),
            next_listen: Instant::now(),
        }
    }

    fn event(&self, text: String) {
        lock(&self.service.state).events.push(text);
    }

    /// One step: notice that Steam is ready, open the listen socket, send the next ping of a running test.
    pub fn poll(&mut self) {
        let status = self.core.status();
        self.steam_id = status.steam_id.or(self.steam_id);
        if !status.ready {
            if self.last_note != status.note {
                self.event(status.note.clone());
                self.last_note = status.note.clone();
            }
            if self.started.elapsed() > Duration::from_secs(120) {
                self.state = "Steam networking is not available (is the game running through Steam?)".into();
            }
            return;
        }
        if self.last_note != status.note {
            self.event(format!("Steam is ready: {}", status.note));
            self.last_note = status.note;
        }
        if self.listener.is_none() && Instant::now() >= self.next_listen {
            self.next_listen = Instant::now() + LISTEN_RETRY;
            let owner: Arc<dyn Owner> = self.service.clone();
            match self.core.listen(PROBE_PORT, owner) {
                Ok(listener) => {
                    self.listener = Some(listener);
                    self.state = "ready".into();
                    self.event("listening for Steam connections".into());
                }
                Err(e) => {
                    self.state = "could not open the Steam listen socket".into();
                    self.event(e.to_string());
                }
            }
        }
        self.refresh_links();
        self.drive_test();
        self.drive_speed();
        self.advance_plan();
    }

    /// Takes the core's latest route and ping of the test connection and of the players connected to us. The core
    /// measures again every second, so a relay that Steam upgrades to a direct path shows up here.
    fn refresh_links(&self) {
        let handles: Vec<Handle> = {
            let state = lock(&self.service.state);
            let test = state.test.iter().filter(|t| t.connected).map(|t| t.conn);
            test.chain(state.peers.iter().map(|p| p.conn)).collect()
        };
        let links: Vec<(Handle, Option<LinkSnapshot>)> = handles.into_iter().map(|h| (h, self.core.link(h))).collect();
        let mut state = lock(&self.service.state);
        for (conn, link) in links {
            let Some(link) = link else { continue };
            if let Some(peer) = state.peers.iter_mut().find(|p| p.conn == conn) {
                peer.link = Some(link);
            }
            if let Some(t) = state.test.as_mut().filter(|t| t.conn == conn) {
                t.ping_ms = link.ping_ms;
                t.send_rate = Some(link.send_rate).filter(|rate| *rate > 0);
                let route = link.route.text();
                if t.route != route {
                    let change =
                        format!("route changed: {} -> {route}", if t.route.is_empty() { "unknown" } else { &t.route });
                    t.route = route.to_string();
                    state.events.push(change);
                }
            }
        }
    }

    fn drive_test(&self) {
        let due = {
            let mut state = lock(&self.service.state);
            match state.test.as_mut() {
                Some(t) if t.connected && t.sent < PROBE_PINGS && Instant::now() >= t.next_ping => {
                    t.sent += 1;
                    t.next_ping = Instant::now() + PING_EVERY;
                    Some((t.conn, format!("ping {}", now_ms())))
                }
                _ => None,
            }
        };
        if let Some((conn, message)) = due {
            let _ = self.core.send(conn, message.as_bytes());
        }
    }

    /// Starts (or restarts) the outgoing test towards `steam_id`.
    pub fn test(&mut self, steam_id: u64) {
        if !self.core.status().ready {
            return;
        }
        // the state stays locked across connect(): the core may report the connection the moment it exists
        let mut state = lock(&self.service.state);
        if let Some(old) = state.test.take() {
            self.core.close(old.conn);
            if let Some(run) = state.speed.as_mut().filter(|r| r.active() && r.phase != Phase::Connecting) {
                run.phase = Phase::Failed("the connection was replaced".into());
            }
        }
        let owner: Arc<dyn Owner> = self.service.clone();
        match self.core.connect(steam_id, PROBE_PORT, owner) {
            Ok(conn) => {
                state.events.push(format!("connecting to Steam ID {steam_id}"));
                state.test = Some(TestConn {
                    conn,
                    target: steam_id,
                    connected: false,
                    pongs: 0,
                    last_rtt_ms: None,
                    next_ping: Instant::now(),
                    sent: 0,
                    route: String::new(),
                    ping_ms: None,
                    send_rate: None,
                    state: "connecting".into(),
                });
            }
            Err(e) => state.events.push(format!("could not start the test connection: {e}")),
        }
    }

    pub fn report(&self) -> Report {
        let state = lock(&self.service.state);
        Report {
            steam_id: self.steam_id,
            state: self.state.clone(),
            events: state.events.0.iter().cloned().collect(),
            test: state.test.as_ref().map(|t| TestReport {
                target: t.target,
                state: t.state.clone(),
                pongs: t.pongs,
                last_rtt_ms: t.last_rtt_ms,
                route: t.route.clone(),
                ping_ms: t.ping_ms,
                send_rate: t.send_rate,
            }),
            pings_received: state.pings_received,
            speed: state.speed.as_ref().map(|r| speed::speed_report(r, state.outcome.as_ref(), speed::PlanView::of(&state))),
            peers: state
                .peers
                .iter()
                .map(|p| PeerReport {
                    steam_id: p.steam_id,
                    route: p.link.map_or_else(String::new, |l| l.route.text().to_string()),
                    ping_ms: p.link.and_then(|l| l.ping_ms),
                    send_rate: p.link.map(|l| l.send_rate).filter(|rate| *rate > 0),
                })
                .collect(),
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(listener) = self.listener.take() {
            self.core.close_listener(listener);
        }
        if let Some(t) = lock(&self.service.state).test.take() {
            self.core.close(t.conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeMachine;
    use super::*;

    #[test]
    fn identity_has_the_steam_layout() {
        let b = identity_bytes(76561198869158889);
        assert_eq!(b.len(), 136);
        assert_eq!(i32::from_le_bytes(b[0..4].try_into().unwrap()), 16);
        assert_eq!(i32::from_le_bytes(b[4..8].try_into().unwrap()), 8);
        assert_eq!(steam_id_of(&b), Some(76561198869158889));
        assert!(b[16..].iter().all(|x| *x == 0));
        assert_eq!(steam_id_of(&[0u8; 136]), None);
        assert_eq!(steam_id_of(&[0u8; 4]), None);
        assert_eq!(steam_id_of(&[]), None);
    }

    fn callback(conn: u32, state: i32, old: i32, listen: u32, reason: i32) -> Vec<u8> {
        let mut b = vec![0u8; CALLBACK_MIN_SIZE];
        b[0..4].copy_from_slice(&conn.to_le_bytes());
        let info = CALLBACK_INFO;
        b[info + INFO_LISTEN_SOCKET..][..4].copy_from_slice(&listen.to_le_bytes());
        b[info + INFO_STATE..][..4].copy_from_slice(&state.to_le_bytes());
        b[info + INFO_END_REASON..][..4].copy_from_slice(&reason.to_le_bytes());
        b[info + CONNECTION_INFO_SIZE..][..4].copy_from_slice(&old.to_le_bytes());
        b
    }

    #[test]
    fn the_status_callback_is_read_at_the_documented_offsets() {
        let b = callback(77, state::CONNECTING, state::NONE, 5, 0);
        assert_eq!(
            parse_status_event(&b),
            Some(StatusEvent {
                conn: 77,
                state: 1,
                old_state: 0,
                listen_socket: 5,
                end_reason: 0,
                remote: None,
                end_debug: String::new()
            })
        );
        let closed = callback(9, state::CLOSED_BY_PEER, state::CONNECTED, 0, 1002);
        let e = parse_status_event(&closed).unwrap();
        assert_eq!((e.state, e.old_state, e.end_reason), (4, 3, 1002));
        // too short, or a state Steam does not define
        assert_eq!(parse_status_event(&b[..100]), None);
        assert_eq!(parse_status_event(&[]), None);
        assert_eq!(parse_status_event(&callback(1, 99, 0, 0, 0)), None);
    }

    #[test]
    fn the_callback_names_the_peer_and_carries_steams_explanation() {
        let mut b = callback(5, state::CLOSED_BY_PEER, state::CONNECTED, 0, 1000);
        b[CALLBACK_INFO..CALLBACK_INFO + IDENTITY_SIZE].copy_from_slice(&identity_bytes(76561198000000042));
        let text = b"The remote host closed the connection\0garbage after the NUL";
        b[CALLBACK_INFO + INFO_END_DEBUG..][..text.len()].copy_from_slice(text);
        let e = parse_status_event(&b).unwrap();
        assert_eq!(e.remote, Some(76561198000000042));
        assert_eq!(e.end_debug, "The remote host closed the connection");
    }

    #[test]
    fn the_connection_info_is_read_at_the_documented_offsets() {
        let mut info = [0u8; CONNECTION_INFO_SIZE];
        info[..IDENTITY_SIZE].copy_from_slice(&identity_bytes(76561198000000007));
        info[INFO_LISTEN_SOCKET..][..4].copy_from_slice(&12u32.to_le_bytes());
        info[INFO_STATE..][..4].copy_from_slice(&3i32.to_le_bytes());
        let direct = parse_connection_info(&info).unwrap();
        assert_eq!(direct.remote, Some(76561198000000007));
        assert_eq!((direct.listen_socket, direct.state, direct.relay_pop), (12, 3, 0));
        assert!(!direct.relayed);
        // a relay point or Steam's own flag both mean relayed
        info[INFO_POP_RELAY..][..4].copy_from_slice(&0x646162u32.to_le_bytes());
        assert!(parse_connection_info(&info).unwrap().relayed);
        info[INFO_POP_RELAY..][..4].copy_from_slice(&0u32.to_le_bytes());
        info[INFO_FLAGS..][..4].copy_from_slice(&INFO_FLAG_RELAYED.to_le_bytes());
        assert!(parse_connection_info(&info).unwrap().relayed);
        assert_eq!(parse_connection_info(&info[..CONNECTION_INFO_SIZE - 1]), None);
    }

    #[test]
    fn the_real_time_status_is_read_at_the_documented_offsets() {
        // SteamNetConnectionRealTimeStatus_t: state 0, ping 4, quality local 8 and remote 12, send rate 32,
        // pending reliable 40 (steamnetworkingtypes.h)
        let mut b = [0u8; REAL_TIME_STATUS_BUFFER];
        b[0..4].copy_from_slice(&3i32.to_le_bytes());
        b[4..8].copy_from_slice(&87i32.to_le_bytes());
        b[8..12].copy_from_slice(&0.75f32.to_le_bytes());
        b[12..16].copy_from_slice(&0.5f32.to_le_bytes());
        b[32..36].copy_from_slice(&1_048_576i32.to_le_bytes());
        b[40..44].copy_from_slice(&350_000i32.to_le_bytes());
        assert_eq!(
            parse_real_time_status(&b),
            Some(RealTime {
                state: 3,
                ping_ms: Some(87),
                quality_local: 0.75,
                quality_remote: 0.5,
                send_rate: 1_048_576,
                pending_reliable: 350_000,
                pending_unreliable: 0,
                sent_unacked_reliable: 0,
                out_packets_per_sec: 0.0,
                out_bytes_per_sec: 0.0,
                in_packets_per_sec: 0.0,
                in_bytes_per_sec: 0.0,
                queue_time_us: 0,
            })
        );
        // Steam reports a negative ping while it has no estimate
        b[4..8].copy_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(parse_real_time_status(&b).map(|r| r.ping_ms), Some(None));
        // the rates, the unacked reliable bytes and the queue time sit at 16..32, 44 and 48
        b[16..20].copy_from_slice(&1200.5f32.to_le_bytes());
        b[20..24].copy_from_slice(&900_000.0f32.to_le_bytes());
        b[24..28].copy_from_slice(&80.0f32.to_le_bytes());
        b[28..32].copy_from_slice(&7000.0f32.to_le_bytes());
        b[36..40].copy_from_slice(&123i32.to_le_bytes());
        b[44..48].copy_from_slice(&65_536i32.to_le_bytes());
        b[48..56].copy_from_slice(&250_000i64.to_le_bytes());
        let r = parse_real_time_status(&b).unwrap();
        assert_eq!((r.out_packets_per_sec, r.out_bytes_per_sec, r.in_packets_per_sec, r.in_bytes_per_sec), (1200.5, 900_000.0, 80.0, 7000.0));
        assert_eq!((r.pending_unreliable, r.sent_unacked_reliable, r.queue_time_us), (123, 65_536, 250_000));
        assert_eq!(parse_real_time_status(&b[..40]), None);
        assert_eq!(parse_real_time_status(&[]), None);
    }

    #[test]
    fn the_relay_point_is_read_from_the_info() {
        let mut info = [0u8; CONNECTION_INFO_SIZE];
        assert_eq!(relay_pop(&info), 0);
        info[INFO_POP_RELAY..][..4].copy_from_slice(&0x646162u32.to_le_bytes());
        assert_eq!(relay_pop(&info), 0x646162);
        assert_eq!(relay_pop(&[0u8; 10]), 0);
    }

    #[test]
    fn end_reasons_read_as_sentences_that_keep_the_code() {
        assert_eq!(end_reason_text(0), "no reason given");
        assert_eq!(end_reason_text(1000), "closed by the other side's program (1000)");
        assert_eq!(end_reason_text(5003), "timed out (5003)");
        assert_eq!(end_reason_text(5009), "a NAT or firewall blocked the connection (5009)");
        assert_eq!(end_reason_text(4321), "problem on the other side (4321)");
        assert_eq!(end_reason_text(-7), "unknown reason (-7)");
    }

    #[test]
    fn pings_are_answered_and_pongs_read() {
        assert_eq!(reply_for(b"ping 12345").as_deref(), Some(&b"pong 12345"[..]));
        assert_eq!(reply_for(b"pong 1"), None);
        assert_eq!(reply_for(b"ping"), None);
        assert_eq!(pong_id(b"pong 12345"), Some(12345));
        assert_eq!(pong_id(b"pong x"), None);
        assert_eq!(pong_id(b"ping 1"), None);
    }

    #[test]
    fn a_pasted_steam_id_must_be_digits_of_the_right_length() {
        assert_eq!(parse_steam_id(" 76561198869158889 "), Some(76561198869158889));
        assert_eq!(parse_steam_id("7656119886915888"), Some(7656119886915888));
        assert_eq!(parse_steam_id("1234"), None);
        assert_eq!(parse_steam_id("7656119886915888x"), None);
        assert_eq!(parse_steam_id(""), None);
        assert_eq!(parse_steam_id("99999999999999999999999"), None);
    }

    #[test]
    fn events_keep_the_newest_eight() {
        let mut e = Events::default();
        for i in 0..12 {
            e.push(format!("e{i}"));
        }
        assert_eq!(e.0.len(), 8);
        assert_eq!(e.0.front().map(String::as_str), Some("e4"));
        assert_eq!(e.0.back().map(String::as_str), Some("e11"));
    }

    // ---- the probe on the core, against the in-memory Steam

    const WAIT: Duration = Duration::from_secs(10);

    /// Calls `step` (which polls the probes and says whether it is done) every few ms until it holds.
    fn until(what: &str, mut step: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !step() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn without_steam_in_the_process_there_is_no_probe() {
        // a test run has no steam_api64.dll: the live controller then reports "not available"
        assert!(Probe::load().is_none());
    }

    #[test]
    fn the_probe_waits_for_steam_then_listens_on_virtual_port_one() {
        let machine = FakeMachine::new();
        machine.set_ready(false);
        let core = machine.core_with(super::fake::fast());
        let mut probe = Probe::on(core.handle());
        until("the interface diagnostics", || {
            probe.poll();
            probe.report().events.iter().any(|e| e.starts_with("Steam interfaces: hsteamuser 0"))
        });
        let waiting = probe.report();
        assert_eq!((waiting.state.as_str(), waiting.steam_id), ("waiting for Steam", None));
        assert_eq!(machine.listeners(), 0);

        machine.set_ready(true);
        until("the listen socket", || {
            probe.poll();
            probe.report().state == "ready"
        });
        let ready = probe.report();
        assert_eq!(ready.steam_id, Some(machine.steam_id()));
        assert_eq!(machine.listeners(), 1);
        assert!(ready.events.iter().any(|e| e == "listening for Steam connections"), "{:?}", ready.events);
        assert!(ready.events.iter().any(|e| e.starts_with("Steam is ready: tuned")), "{:?}", ready.events);
        assert_eq!((ready.test, ready.peers, ready.pings_received), (None, Vec::new(), 0));
    }

    #[test]
    fn a_probe_and_a_session_listener_share_one_core_on_different_virtual_ports() {
        let machine = FakeMachine::new();
        let core = machine.core();
        let mut probe = Probe::on(core.handle());
        until("the probe", || {
            probe.poll();
            probe.report().state == "ready"
        });
        // the session transport listens on virtual port 0 of the same core while the probe holds port 1
        let (tx, _rx) = std::sync::mpsc::channel();
        let net = crate::steam_transport::SteamHostNet::listen_on(&core.handle(), tx, WAIT, 8).expect("listen");
        assert_eq!(machine.listeners(), 2);
        assert_eq!(PROBE_PORT, 1);
        drop(net);
        assert_eq!(machine.listeners(), 1);
    }

    #[test]
    fn a_friends_test_reaches_the_probe_and_is_answered() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let (core_a, core_b) = (a.core(), b.core());
        let (mut probe_a, mut probe_b) = (Probe::on(core_a.handle()), Probe::on(core_b.handle()));
        until("both probes", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().state == "ready" && probe_b.report().state == "ready"
        });
        a.set_ping(87);
        b.set_ping(95);
        probe_a.test(b.steam_id());
        assert_eq!(probe_a.report().test.map(|t| (t.target, t.state)), Some((b.steam_id(), "connecting".to_string())));
        until("a pong", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().test.is_some_and(|t| t.pongs >= 1 && t.ping_ms == Some(87))
        });
        let tested = probe_a.report();
        let test = tested.test.expect("a test");
        assert_eq!(test.send_rate, Some(Config::default().send_rate as u32), "the tuned send rate is in force");
        assert_eq!((test.state.as_str(), test.route.as_str()), ("connected", "direct"));
        assert!(test.last_rtt_ms.is_some_and(|ms| ms < 1000), "{test:?}");
        assert!(tested.events.iter().any(|e| e == "test connection established"), "{:?}", tested.events);

        // the other side counted the ping and lists the friend with the route and ping Steam measured
        until("the peer's view", || {
            probe_b.poll();
            probe_b.report().peers.first().is_some_and(|p| p.ping_ms == Some(95))
        });
        let answered = probe_b.report();
        assert_eq!(answered.pings_received, 1);
        assert_eq!(
            answered.peers,
            vec![PeerReport {
                steam_id: Some(a.steam_id()),
                route: "direct".into(),
                ping_ms: Some(95),
                send_rate: Some(Config::default().send_rate as u32),
            }]
        );
        assert!(answered.events.iter().any(|e| e == "a player connected"));
        assert!(answered.events.iter().any(|e| e == "received a ping, answered"));

        // testing again replaces the connection; the old one disappears from the other side
        probe_a.test(b.steam_id());
        until("the second test", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().test.is_some_and(|t| t.pongs >= 1) && probe_b.report().peers.len() == 1
        });
        assert_eq!(probe_b.report().pings_received, 2);
        assert!(b.connections().len() == 1 && a.connections().len() == 1);
    }

    #[test]
    fn the_probe_tests_the_own_account_inside_one_process() {
        let machine = FakeMachine::new();
        let core = machine.core();
        let mut probe = Probe::on(core.handle());
        until("the listen socket", || {
            probe.poll();
            probe.report().state == "ready"
        });
        probe.test(machine.steam_id());
        until("a pong from ourselves", || {
            probe.poll();
            let report = probe.report();
            report.test.is_some_and(|t| t.pongs >= 1) && report.peers.len() == 1
        });
        assert_eq!(probe.report().peers[0].steam_id, Some(machine.steam_id()));
    }

    #[test]
    fn the_probe_holds_a_few_players_and_turns_the_rest_away() {
        // anyone who knows an account's Steam ID can connect to the probe
        struct Quiet;
        impl Owner for Quiet {
            fn message(&self, _conn: Handle, _data: &[u8]) {}
            fn closed(&self, _conn: Handle, _end: End) {}
        }
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let (core_a, core_b) = (a.core(), b.core());
        let mut probe = Probe::on(core_a.handle());
        until("the listen socket", || {
            probe.poll();
            probe.report().state == "ready"
        });
        let owner: Arc<dyn Owner> = Arc::new(Quiet);
        let extra = 4;
        let guests: Vec<Handle> = (0..MAX_PEERS + extra)
            .map(|_| core_b.handle().connect(a.steam_id(), PROBE_PORT, owner.clone()).expect("connect"))
            .collect();
        until("the probe to be full and to turn the others away", || {
            probe.poll();
            probe.report().peers.len() == MAX_PEERS && a.closes().len() >= extra
        });
        for _ in 0..20 {
            probe.poll();
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(probe.report().peers.len(), MAX_PEERS, "it does not grow");
        assert_eq!(guests.len(), MAX_PEERS + extra);
    }

    #[test]
    fn a_test_that_cannot_connect_says_why() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let (core_a, _core_b) = (a.core(), b.core());
        let mut probe = Probe::on(core_a.handle());
        until("the listen socket", || {
            probe.poll();
            probe.report().state == "ready"
        });
        // b runs no probe: nobody listens on virtual port 1
        probe.test(b.steam_id());
        until("the refusal", || {
            probe.poll();
            probe.report().test.is_some_and(|t| t.state.starts_with("failed"))
        });
        let refused = probe.report();
        let state = refused.test.expect("a test").state;
        assert!(state.contains("closed by the other side") && state.contains("(5001)"), "{state}");
        assert!(refused.events.iter().any(|e| e.starts_with("connection ended: closed by the other side")));

        // an account Steam cannot find
        probe.test(76561198123456789);
        until("the timeout", || {
            probe.poll();
            probe.report().test.is_some_and(|t| t.state.contains("timed out (5003)"))
        });
        assert_eq!(probe.report().test.map(|t| t.pongs), Some(0));
    }

    #[test]
    fn a_test_while_steam_is_not_ready_changes_nothing() {
        let machine = FakeMachine::new();
        machine.set_ready(false);
        let core = machine.core_with(super::fake::fast());
        let mut probe = Probe::on(core.handle());
        probe.poll();
        probe.test(76561198000000001);
        assert_eq!(probe.report().test, None);
    }

    #[test]
    fn the_route_is_measured_again_and_a_change_is_reported() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        a.set_relay(0x646162);
        let (core_a, core_b) = (a.core(), b.core());
        let (mut probe_a, mut probe_b) = (Probe::on(core_a.handle()), Probe::on(core_b.handle()));
        until("both probes", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().state == "ready" && probe_b.report().state == "ready"
        });
        probe_a.test(b.steam_id());
        until("the relayed route", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().test.is_some_and(|t| t.route == "through a Steam relay" && t.pongs >= 1)
        });
        // Steam finds a direct path some time after the connection opened
        a.set_relay(0);
        until("the upgrade", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().test.is_some_and(|t| t.route == "direct")
        });
        let events = probe_a.report().events;
        assert!(events.iter().any(|e| e == "route changed: through a Steam relay -> direct"), "{events:?}");
    }

    #[test]
    fn dropping_the_probe_closes_its_listener_and_its_test() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let (core_a, core_b) = (a.core(), b.core());
        let (mut probe_a, mut probe_b) = (Probe::on(core_a.handle()), Probe::on(core_b.handle()));
        until("both probes", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().state == "ready" && probe_b.report().state == "ready"
        });
        probe_a.test(b.steam_id());
        until("the test connection", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().test.is_some_and(|t| t.pongs >= 1)
        });
        drop(probe_a);
        assert_eq!(a.listeners(), 0);
        until("the other side to notice", || {
            probe_b.poll();
            probe_b.report().peers.is_empty()
        });
        assert!(a.connections().is_empty());
    }

    // ---- the speed test, between two fake accounts

    fn two_probes(
    ) -> (FakeMachine, FakeMachine, super::steam_core::SteamCore, super::steam_core::SteamCore, Probe, Probe) {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let (core_a, core_b) = (a.core(), b.core());
        let (mut probe_a, mut probe_b) = (Probe::on(core_a.handle()), Probe::on(core_b.handle()));
        until("both probes", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().state == "ready" && probe_b.report().state == "ready"
        });
        (a, b, core_a, core_b, probe_a, probe_b)
    }

    #[test]
    fn a_speed_test_moves_data_both_ways_and_reports_the_numbers() {
        let (a, b, _ca, _cb, mut probe_a, mut probe_b) = two_probes();
        a.set_ping(120);
        a.set_relay(0x646162);
        probe_a.speed_test(b.steam_id(), 3);
        let first = probe_a.report().speed.expect("a speed test");
        assert!(first.running && first.status.contains("connecting"), "{first:?}");
        until("the speed test to finish", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().speed.is_some_and(|s| !s.running)
        });
        let done = probe_a.report().speed.expect("report");
        assert!(done.lines.len() == 4, "{done:?}");
        assert!(done.lines[0].starts_with("Speed test, 3 MB each way: you to friend "), "{done:?}");
        assert!(
            done.lines[1].contains("through a Steam relay") && done.lines[1].contains("round trip 120 ms"),
            "{done:?}"
        );
        assert!(done.lines[1].contains(&format!("send rate {} B/s", Config::default().send_rate)), "{done:?}");
        assert!(done.lines[2].contains("no stall"), "{done:?}");
        assert!(done.lines[3].starts_with("An 80 MB save"), "{done:?}");
        let log = done.log.expect("a log line");
        assert!(log.starts_with("steam speed test: 3 MB each way, up ") && !log.contains(char::from(10)), "{log}");
        // the friend counted what it received and what it sent
        let events = probe_b.report().events;
        assert!(events.iter().any(|e| e == "a friend is testing the speed: 3 MB coming"), "{events:?}");
        assert!(events.iter().any(|e| e == "sending 3 MB for a friend's speed test"), "{events:?}");
        // a ping still works on the same connection afterwards
        until("a pong", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().test.is_some_and(|t| t.pongs >= 1)
        });
    }

    fn finish(probe_a: &mut Probe, probe_b: &mut Probe, what: &str) -> RunOutcome {
        until(what, || {
            probe_a.poll();
            probe_b.poll();
            !probe_a.run_active()
        });
        probe_a.run_outcome().expect("an outcome")
    }

    #[test]
    fn a_datagram_run_moves_data_both_ways_through_lost_datagrams() {
        let (a, b, _ca, _cb, mut probe_a, mut probe_b) = two_probes();
        // every 25th datagram each machine sends is lost on the wire
        a.lose_every_nth_unreliable(25);
        b.lose_every_nth_unreliable(25);
        probe_a.start_run(b.steam_id(), RunSpec::datagrams(4));
        let outcome = finish(&mut probe_a, &mut probe_b, "the datagram run to finish");
        assert_eq!(outcome.failure, None, "{outcome:?}");
        let Some(RunKind::Datagram(r)) = outcome.kind else { panic!("{outcome:?}") };
        assert!(r.up.ok && r.down.ok, "{r:?}");
        assert_eq!((r.up.bytes, r.down.bytes), (4 * throughput::MB, 4 * throughput::MB), "{r:?}");
        assert!(r.up.rate.is_some_and(|x| x > 0.0) && r.down.rate.is_some_and(|x| x > 0.0), "{r:?}");
        // the sender's numbers came back for both directions, and the datagrams that were lost were sent again
        let (up, down) = (r.up.sender.as_ref().expect("up sender"), r.down.sender.as_ref().expect("down sender"));
        assert!(up.loss_bp > 0 && down.loss_bp > 0, "{r:?}");
        let lines = outcome.lines;
        assert!(lines[0].starts_with("Datagram speed test, 4 MB each way: you to friend "), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("You to friend: you sent: lost ")), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("Friend to you: your friend sent: lost ")), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("An 80 MB save (datagrams")), "{lines:?}");
        assert!(outcome.log_line.starts_with("steam speed test (datagrams): 4 MB each way"), "{}", outcome.log_line);
        // the connection's Steam send rate was raised for the transfer and put back after
        let log = probe_a.take_log().join("\n");
        assert!(log.contains("steam rate restored"), "{log}");
        let during = bulk_io::STEAM_RATE_DURING;
        assert!(a.connection_config().iter().any(|c| c.1 == 10 && c.2 == during), "{:?}", a.connection_config());
        assert!(a.connection_config().iter().rfind(|c| c.1 == 10).is_some_and(|c| c.2 == Config::default().send_rate));
        // a ping still works afterwards
        until("a pong", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().test.is_some_and(|t| t.pongs >= 1)
        });
    }

    #[test]
    fn a_datagram_offer_while_the_friend_is_busy_is_refused() {
        let (a, b, _ca, _cb, mut probe_a, mut probe_b) = two_probes();
        // two uploads at once: the second offer meets a busy receiver and is aborted with a reason, not hung
        probe_a.start_run(b.steam_id(), RunSpec::datagrams(1));
        let _ = finish(&mut probe_a, &mut probe_b, "the first run");
        let _ = a;
        // a hostile peer sends an offer for a huge transfer: refused as too big
        let conn = lock(&probe_a.service.state).test.as_ref().map(|t| t.conn).unwrap();
        let offer = bulk::wire::Offer {
            xfer: 999,
            ts: 0,
            ctx: 0,
            total: 10_000 * throughput::MB,
            payload: bulk::PAYLOAD as u16,
            sha256: [1; 32],
        };
        probe_a.service.core.send_datagram(conn, &bulk::wire::encode_offer(&offer));
        until("the refusal", || {
            probe_a.poll();
            probe_b.poll();
            probe_b.take_log().is_empty() && lock(&probe_b.service.state).dg_in.is_none()
        });
        assert!(lock(&probe_b.service.state).dg_in.is_none());
    }

    #[test]
    fn reliable_runs_over_several_connections_count_every_chunk_once() {
        for n in [2u32, 4] {
            let (_a, b, _ca, _cb, mut probe_a, mut probe_b) = two_probes();
            let spec = RunSpec { variant: throughput::Variant::Reliable(n), ..RunSpec::reliable(3) };
            probe_a.start_run(b.steam_id(), spec);
            let outcome = finish(&mut probe_a, &mut probe_b, &format!("the parallel run of {n}"));
            assert_eq!(outcome.failure, None, "{outcome:?}");
            let Some(RunKind::Reliable(r)) = outcome.kind else { panic!("{outcome:?}") };
            assert_eq!((r.up.bytes, r.down.bytes), (3 * throughput::MB, 3 * throughput::MB), "{r:?}");
            assert_eq!(r.conns, n);
            assert!(outcome.lines[0].contains(&format!("over {n} connections")), "{:?}", outcome.lines);
            // the extra connections were closed again
            until("the extras to close", || {
                probe_a.poll();
                probe_b.poll();
                probe_b.report().peers.len() <= 1
            });
        }
    }

    #[test]
    fn a_run_at_a_chosen_send_rate_asks_the_peer_to_set_its_own_and_both_restore_it() {
        let (a, b, _ca, _cb, mut probe_a, mut probe_b) = two_probes();
        let spec = RunSpec { rate: Some(4 << 20), ..RunSpec::reliable(2) };
        probe_a.start_run(b.steam_id(), spec);
        let outcome = finish(&mut probe_a, &mut probe_b, "the run");
        assert_eq!(outcome.failure, None, "{outcome:?}");
        let Some(RunKind::Reliable(r)) = outcome.kind else { panic!("{outcome:?}") };
        assert!(r.rate_note.contains("own send rate set to 4194304") && r.rate_note.contains("peer set its send rate to 4194304"), "{}", r.rate_note);
        let default = Config::default().send_rate;
        for machine in [&a, &b] {
            let rates: Vec<i32> = machine.connection_config().iter().filter(|c| c.1 == 10).map(|c| c.2).collect();
            assert!(rates.contains(&(4 << 20)), "{rates:?}");
        }
        until("the peer to put its rate back", || {
            probe_a.poll();
            probe_b.poll();
            b.connection_config().iter().filter(|c| c.1 == 10).last().is_some_and(|c| c.2 == default)
        });
        assert!(a.connection_config().iter().filter(|c| c.1 == 10).last().is_some_and(|c| c.2 == default));
    }

    #[test]
    fn a_peer_that_ignores_the_rate_request_is_reported_as_such() {
        let (_a, b, _ca, _cb, mut probe_a, mut probe_b) = two_probes();
        probe_b.act_like_an_old_program();
        let spec = RunSpec { rate: Some(2 << 20), ..RunSpec::reliable(1) };
        probe_a.start_run(b.steam_id(), spec);
        let outcome = finish(&mut probe_a, &mut probe_b, "the run");
        assert_eq!(outcome.failure, None, "{outcome:?}");
        let Some(RunKind::Reliable(r)) = outcome.kind else { panic!("{outcome:?}") };
        assert!(r.rate_note.contains("peer did not change its rate"), "{}", r.rate_note);
        assert_eq!(r.down.bytes, throughput::MB);
    }

    #[test]
    fn a_direction_still_running_at_the_time_limit_is_stopped_and_reported_as_it_stands() {
        let (a, b, _ca, _cb, mut probe_a, mut probe_b) = two_probes();
        // a receiver that stops reading: the sender's buffer fills and nothing moves
        b.stall_receive(true);
        let spec = RunSpec { cap: Duration::from_millis(400), ..RunSpec::reliable(64) };
        probe_a.start_run(b.steam_id(), spec);
        let _ = a;
        let outcome = finish(&mut probe_a, &mut probe_b, "the capped run");
        b.stall_receive(false);
        assert_eq!(outcome.failure, None, "{outcome:?}");
        let Some(RunKind::Reliable(r)) = outcome.kind else { panic!("{outcome:?}") };
        assert!(r.up.capped && r.down.capped, "{r:?}");
    }

    #[test]
    fn a_series_of_runs_shows_one_block_per_run_and_stops_at_the_first_failure() {
        let (a, b, _ca, _cb, mut probe_a, mut probe_b) = two_probes();
        a.lose_every_nth_unreliable(40);
        probe_a.start_plan(b.steam_id(), specs_for("both", 2));
        let first = probe_a.report().speed.expect("a report");
        assert!(first.running && first.status.starts_with("[run 1 of 2] "), "{first:?}");
        until("both runs", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().speed.is_some_and(|s| !s.running)
        });
        let lines = probe_a.report().speed.unwrap().lines;
        let headers: Vec<&String> = lines.iter().filter(|l| l.starts_with("== ")).collect();
        assert_eq!(headers, ["== reliable (2 MB) ==", "== datagrams (2 MB) =="], "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("Speed test, 2 MB each way")), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("Datagram speed test, 2 MB each way")), "{lines:?}");
        // the datagrams went out unreliably, some were lost, and every one was sent with the settings of the connection
        let (sent, lost, _) = a.unreliable_counts();
        assert!(sent > 1000 && lost > 0, "{sent} {lost}");
        // the settings the connections were created with: the same send rate, send rate and send buffer as the tuning
        let rate = Config::default().send_rate;
        let options = a.creation_options();
        assert!(options.contains(&(10, rate)) && options.contains(&(11, rate)) && options.contains(&(9, 2 << 20)), "{options:?}");

        // a series with a peer that is gone stops after the first block
        b.set_reachable(false);
        a.break_connections();
        probe_a.start_plan(b.steam_id(), specs_for("all", 1));
        until("the failure", || {
            probe_a.poll();
            probe_a.report().speed.is_some_and(|s| !s.running)
        });
        let lines = probe_a.report().speed.unwrap().lines;
        assert!(lines.iter().any(|l| l.starts_with("Speed test failed:")), "{lines:?}");
        assert_eq!(lines.iter().filter(|l| l.starts_with("== ")).count(), 1, "{lines:?}");
    }

    #[test]
    fn the_numbers_of_a_finished_speed_test_are_complete() {
        let (_a, b, _ca, _cb, mut probe_a, mut probe_b) = two_probes();
        probe_a.speed_test(b.steam_id(), 4);
        until("the speed test to finish", || {
            probe_a.poll();
            probe_b.poll();
            probe_a.report().speed.is_some_and(|s| !s.running)
        });
        let state = lock(&probe_a.service.state);
        let result = state.speed.as_ref().and_then(|r| r.result.clone()).expect("a result");
        assert_eq!(result.total_bytes, 4 * throughput::MB);
        assert_eq!((result.up.bytes, result.down.bytes), (4 * throughput::MB, 4 * throughput::MB));
        assert!(result.up.rate.is_some_and(|r| r > 0.0) && result.down.rate.is_some_and(|r| r > 0.0), "{result:?}");
        assert!(result.total_secs > 0.0 && !result.stalled(), "{result:?}");
        assert_eq!(result.rate_set, Config::default().send_rate);
        assert_eq!(result.route, "direct");
        // the queue never exceeds Steam's 512 KiB buffer
        assert!(result.up.peak_pending <= 512 * 1024 && result.down.peak_pending <= 512 * 1024, "{result:?}");
    }

    #[test]
    fn a_speed_test_to_a_friend_without_a_probe_fails_with_the_reason() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let (core_a, _core_b) = (a.core(), b.core());
        let mut probe = Probe::on(core_a.handle());
        until("the listen socket", || {
            probe.poll();
            probe.report().state == "ready"
        });
        probe.speed_test(b.steam_id(), 1);
        until("the failure", || {
            probe.poll();
            probe.report().speed.is_some_and(|s| !s.running)
        });
        let s = probe.report().speed.unwrap();
        assert!(s.status.starts_with("Speed test failed: no connection: failed: closed by the other side"), "{s:?}");
        assert!(s.log.is_some_and(|l| l.starts_with("steam speed test failed:")));
    }

    #[test]
    fn a_speed_test_ends_when_the_friend_goes_away() {
        let (_a, b, _ca, core_b, mut probe_a, probe_b) = two_probes();
        // the friend's side stops reading, so the upload cannot finish before the friend goes away
        b.stall_receive(true);
        probe_a.speed_test(b.steam_id(), 128);
        until("the upload to begin", || {
            probe_a.poll();
            probe_a.report().speed.is_some_and(|s| s.status.contains("sending to your friend"))
        });
        drop(probe_b);
        drop(core_b);
        until("the end", || {
            probe_a.poll();
            probe_a.report().speed.is_some_and(|s| !s.running)
        });
        let s = probe_a.report().speed.unwrap();
        assert!(s.status.starts_with("Speed test failed:"), "{s:?}");
    }

    #[test]
    fn a_friend_cannot_make_the_probe_receive_or_send_more_than_the_limit() {
        let (_a, b, _ca, _cb, mut probe_a, mut probe_b) = two_probes();
        probe_a.test(b.steam_id());
        until("connected", || {
            probe_a.poll();
            probe_a.report().test.is_some_and(|t| t.state == "connected")
        });
        // b's probe is asked for far too much: nothing is started
        let conn = lock(&probe_a.service.state).test.as_ref().map(|t| t.conn).unwrap();
        let ask = throughput::encode(&throughput::Msg::Down { id: 1, total: 10_000 * throughput::MB, chunk: 1000, conns: 1, rate: 0 });
        assert_eq!(probe_a.core.send(conn, &ask), SendOutcome::Sent);
        thread::sleep(Duration::from_millis(100));
        probe_b.poll();
        let events = probe_b.report().events;
        assert!(!events.iter().any(|e| e.starts_with("sending ")), "{events:?}");
    }
}
