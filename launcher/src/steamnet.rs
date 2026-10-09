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

/// An in-memory Steam for the tests of this module and of the transport.
#[cfg(test)]
pub(crate) mod fake;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub use steam_core::{
    global, Cause, Config, CoreError, CoreRef, CoreStatus, End, Handle, Owner, Route, SendOutcome, SteamCore,
    MAX_MESSAGE,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestReport {
    pub target: u64,
    pub state: String,
    pub pongs: u32,
    pub last_rtt_ms: Option<u64>,
    pub route: String,
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
    state: String,
}

#[derive(Default)]
struct ProbeState {
    events: Events,
    test: Option<TestConn>,
    pings_received: u32,
}

/// The probe's side of the core: what happens to its connections, on the poller thread.
struct ProbeService {
    core: CoreRef,
    state: Mutex<ProbeState>,
}

impl ProbeService {
    fn is_test(state: &ProbeState, conn: Handle) -> bool {
        state.test.as_ref().is_some_and(|t| t.conn == conn)
    }
}

impl Owner for ProbeService {
    fn opened(&self, conn: Handle, _incoming: bool, _remote: Option<u64>) {
        let route = self.core.route(conn).map_or("unknown", Route::text);
        let mut state = lock(&self.state);
        if let Some(t) = state.test.as_mut().filter(|t| t.conn == conn) {
            t.connected = true;
            t.state = "connected".into();
            t.route = route.into();
            t.next_ping = Instant::now();
            state.events.push("test connection established".into());
        } else {
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
        if data.len() > PING_LIMIT {
            return;
        }
        if let Some(reply) = reply_for(data) {
            {
                let mut state = lock(&self.state);
                state.pings_received += 1;
                state.events.push("received a ping, answered".into());
            }
            // a full send buffer only loses one pong; the pinger asks again
            let _ = self.core.send(conn, &reply);
        } else if let Some(sent) = pong_id(data) {
            if let Some(t) = lock(&self.state).test.as_mut().filter(|t| t.conn == conn) {
                t.pongs += 1;
                t.last_rtt_ms = Some(now_ms().saturating_sub(sent));
            }
        }
    }

    fn closed(&self, conn: Handle, end: End) {
        let mut state = lock(&self.state);
        let ours = Self::is_test(&state, conn);
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
        let service = Arc::new(ProbeService { core: core.clone(), state: Mutex::new(ProbeState::default()) });
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
        self.drive_test();
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
            }),
            pings_received: state.pings_received,
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
}
