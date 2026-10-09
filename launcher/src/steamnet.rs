//! Steam peer-to-peer connectivity probe.
//!
//! Steam can connect two players by Steam ID, punching through NATs and falling back to its own relays (Steam
//! Datagram Relay), without any port forwarding and without a server of ours. This module checks that the game's
//! own `steam_api64.dll` can do that for us: it listens on a P2P virtual port, answers every ping, and can ping a
//! friend's Steam ID and report the round trip. It carries no session traffic yet; the aim is to find out, on real
//! machines, whether the transport is usable before it is built into the session.
//!
//! The functions come from the *flat* Steam API (`SteamAPI_ISteamNetworkingSockets_*`), looked up by name in the
//! already loaded `steam_api64.dll`, so no Steam SDK is linked and nothing runs unless the game has Steam loaded.
//! Every structure layout used (identity, connection info, status-changed callback, message) is written down as
//! offsets next to a pure parser with tests, because getting one wrong is a silent crash.

use std::collections::VecDeque;
use std::sync::Mutex;

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
    if identity.len() < 16 || i32::from_le_bytes(identity[0..4].try_into().ok()?) != IDENTITY_TYPE_STEAMID {
        return None;
    }
    Some(u64::from_le_bytes(identity[8..16].try_into().ok()?))
}

/// Size of `SteamNetConnectionInfo_t`.
pub const CONNECTION_INFO_SIZE: usize = 696;
/// Offsets inside `SteamNetConnectionInfo_t`.
const INFO_LISTEN_SOCKET: usize = 144;
const INFO_POP_RELAY: usize = 172;
const INFO_STATE: usize = 176;
const INFO_END_REASON: usize = 180;
/// `SteamNetConnectionStatusChangedCallback_t`: connection handle, then the info (8-aligned), then the old state.
const CALLBACK_INFO: usize = 8;
const CALLBACK_MIN_SIZE: usize = CALLBACK_INFO + CONNECTION_INFO_SIZE + 4;

/// Connection states (`ESteamNetworkingConnectionState`).
pub mod state {
    pub const NONE: i32 = 0;
    pub const CONNECTING: i32 = 1;
    pub const FINDING_ROUTE: i32 = 2;
    pub const CONNECTED: i32 = 3;
    pub const CLOSED_BY_PEER: i32 = 4;
    pub const PROBLEM_DETECTED_LOCALLY: i32 = 5;
}

/// What the status-changed callback reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusEvent {
    pub conn: u32,
    pub state: i32,
    pub old_state: i32,
    /// The listen socket the connection arrived on, 0 for one we opened.
    pub listen_socket: u32,
    pub end_reason: i32,
}

/// Reads the callback structure. `None` when it is too short or holds a state Steam does not define.
pub fn parse_status_event(buf: &[u8]) -> Option<StatusEvent> {
    if buf.len() < CALLBACK_MIN_SIZE {
        return None;
    }
    let u32_at = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let i32_at = |o: usize| u32_at(o) as i32;
    let info = CALLBACK_INFO;
    let event = StatusEvent {
        conn: u32_at(0),
        state: i32_at(info + INFO_STATE),
        old_state: i32_at(info + CONNECTION_INFO_SIZE),
        listen_socket: u32_at(info + INFO_LISTEN_SOCKET),
        end_reason: i32_at(info + INFO_END_REASON),
    };
    let valid = |s: i32| (-1..=5).contains(&s);
    (valid(event.state) && valid(event.old_state)).then_some(event)
}

/// The relay point of a connection (0 when the route is direct), from a `GetConnectionInfo` buffer.
pub fn relay_pop(info: &[u8]) -> u32 {
    if info.len() < INFO_POP_RELAY + 4 {
        return 0;
    }
    u32::from_le_bytes([
        info[INFO_POP_RELAY],
        info[INFO_POP_RELAY + 1],
        info[INFO_POP_RELAY + 2],
        info[INFO_POP_RELAY + 3],
    ])
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

/// Status events from Steam's callback, queued for the thread that owns the probe.
static QUEUE: Mutex<Vec<StatusEvent>> = Mutex::new(Vec::new());

#[cfg(windows)]
pub use win::Probe;

#[cfg(not(windows))]
pub use other::Probe;

#[cfg(not(windows))]
mod other {
    use super::Report;
    /// Steam networking is only driven on Windows.
    pub struct Probe;
    impl Probe {
        pub fn load() -> Option<Probe> {
            None
        }
        pub fn poll(&mut self) {}
        pub fn test(&mut self, _steam_id: u64) {}
        pub fn report(&self) -> Report {
            Report::default()
        }
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::ffi::{c_char, c_void};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    /// The flat API functions the probe uses, looked up once.
    struct Api {
        sockets_accessor: unsafe extern "C" fn() -> *mut c_void,
        utils_accessor: unsafe extern "C" fn() -> *mut c_void,
        user_accessor: unsafe extern "C" fn() -> *mut c_void,
        get_steam_id: unsafe extern "C" fn(*mut c_void) -> u64,
        init_relay: unsafe extern "C" fn(*mut c_void),
        set_status_callback: unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut c_void)) -> bool,
        run_callbacks: unsafe extern "C" fn(*mut c_void),
        create_listen: unsafe extern "C" fn(*mut c_void, i32, i32, *const c_void) -> u32,
        connect_p2p: unsafe extern "C" fn(*mut c_void, *const c_void, i32, i32, *const c_void) -> u32,
        accept: unsafe extern "C" fn(*mut c_void, u32) -> i32,
        send: unsafe extern "C" fn(*mut c_void, u32, *const c_void, u32, i32, *mut i64) -> i32,
        receive: unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void, i32) -> i32,
        release_message: unsafe extern "C" fn(*mut c_void),
        close: unsafe extern "C" fn(*mut c_void, u32, i32, *const c_char, bool) -> bool,
        connection_info: unsafe extern "C" fn(*mut c_void, u32, *mut c_void) -> bool,
        hsteamuser: unsafe extern "C" fn() -> i32,
        find_interface: unsafe extern "C" fn(i32, *const c_char) -> *mut c_void,
    }

    /// A function pointer `T` (an `extern "C" fn` type) from a NUL-terminated export name.
    ///
    /// # Safety
    /// `module` must be a loaded module and `T` the exact type of the export.
    unsafe fn symbol<T: Copy>(module: *mut c_void, name: &str) -> Option<T> {
        assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
        let p = unsafe { GetProcAddress(module, name.as_ptr()) };
        if p.is_null() {
            return None;
        }
        // SAFETY: T is a function pointer type, the same size as a pointer (asserted above).
        Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
    }

    fn load_api() -> Option<Api> {
        // SAFETY: GetModuleHandleA/GetProcAddress with NUL-terminated names; the signatures below are the flat API's
        // (steam_api_flat.h of the Steamworks SDK) and every function is called only while Steam is initialised.
        unsafe {
            let module = GetModuleHandleA(c"steam_api64.dll".as_ptr().cast());
            if module.is_null() {
                return None;
            }
            macro_rules! sym {
                ($name:literal) => {
                    match symbol(module, concat!($name, "\0")) {
                        Some(f) => f,
                        None => return None,
                    }
                };
            }
            Some(Api {
                sockets_accessor: sym!("SteamAPI_SteamNetworkingSockets_SteamAPI_v012"),
                utils_accessor: sym!("SteamAPI_SteamNetworkingUtils_SteamAPI_v004"),
                user_accessor: sym!("SteamAPI_SteamUser_v023"),
                get_steam_id: sym!("SteamAPI_ISteamUser_GetSteamID"),
                init_relay: sym!("SteamAPI_ISteamNetworkingUtils_InitRelayNetworkAccess"),
                set_status_callback: sym!(
                    "SteamAPI_ISteamNetworkingUtils_SetGlobalCallback_SteamNetConnectionStatusChanged"
                ),
                run_callbacks: sym!("SteamAPI_ISteamNetworkingSockets_RunCallbacks"),
                create_listen: sym!("SteamAPI_ISteamNetworkingSockets_CreateListenSocketP2P"),
                connect_p2p: sym!("SteamAPI_ISteamNetworkingSockets_ConnectP2P"),
                accept: sym!("SteamAPI_ISteamNetworkingSockets_AcceptConnection"),
                send: sym!("SteamAPI_ISteamNetworkingSockets_SendMessageToConnection"),
                receive: sym!("SteamAPI_ISteamNetworkingSockets_ReceiveMessagesOnConnection"),
                release_message: sym!("SteamAPI_SteamNetworkingMessage_t_Release"),
                close: sym!("SteamAPI_ISteamNetworkingSockets_CloseConnection"),
                connection_info: sym!("SteamAPI_ISteamNetworkingSockets_GetConnectionInfo"),
                hsteamuser: sym!("SteamAPI_GetHSteamUser"),
                find_interface: sym!("SteamInternal_FindOrCreateUserInterface"),
            })
        }
    }

    unsafe extern "C" fn on_status_changed(callback: *mut c_void) {
        if callback.is_null() {
            return;
        }
        // SAFETY: Steam passes a SteamNetConnectionStatusChangedCallback_t, CALLBACK_MIN_SIZE bytes long.
        let bytes = unsafe { std::slice::from_raw_parts(callback as *const u8, CALLBACK_MIN_SIZE) };
        if let Some(event) = parse_status_event(bytes) {
            if let Ok(mut q) = QUEUE.lock() {
                if q.len() < 256 {
                    q.push(event);
                }
            }
        }
    }

    struct TestConn {
        conn: u32,
        target: u64,
        connected: bool,
        pongs: u32,
        last_rtt_ms: Option<u64>,
        next_ping: Instant,
        sent: u32,
        route: String,
        state: String,
    }

    pub struct Probe {
        api: Api,
        sockets: *mut c_void,
        utils: *mut c_void,
        listen: u32,
        steam_id: Option<u64>,
        state: String,
        events: Events,
        /// Connections that arrived on our listen socket.
        incoming: Vec<u32>,
        test: Option<TestConn>,
        pings_received: u32,
        started: Instant,
        last_seen: String,
    }

    // SAFETY: the raw pointers are Steam's singletons, valid for the process lifetime and used from the one
    // controller thread that owns the probe.
    unsafe impl Send for Probe {}

    fn now_ms() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }

    impl Probe {
        /// `None` when the process has no Steam API loaded (a test run, a game started without Steam).
        pub fn load() -> Option<Probe> {
            let api = load_api()?;
            Some(Probe {
                api,
                sockets: std::ptr::null_mut(),
                utils: std::ptr::null_mut(),
                listen: 0,
                steam_id: None,
                state: "waiting for Steam".into(),
                events: Events::default(),
                incoming: Vec::new(),
                test: None,
                pings_received: 0,
                started: Instant::now(),
                last_seen: String::new(),
            })
        }

        fn setup(&mut self) {
            // SAFETY: see load_api; the accessors return null until Steam is initialised.
            unsafe {
                let user = (self.api.user_accessor)();
                if self.steam_id.is_none() && !user.is_null() {
                    let id = (self.api.get_steam_id)(user);
                    if id != 0 {
                        self.steam_id = Some(id);
                    }
                }
                let mut sockets = (self.api.sockets_accessor)();
                let mut utils = (self.api.utils_accessor)();
                let huser = (self.api.hsteamuser)();
                if sockets.is_null() && huser != 0 {
                    // what the SDK's inline accessors do: ask the client for the interface by version name
                    sockets = (self.api.find_interface)(huser, c"SteamNetworkingSockets012".as_ptr());
                }
                if utils.is_null() && huser != 0 {
                    utils = (self.api.find_interface)(huser, c"SteamNetworkingUtils004".as_ptr());
                }
                if sockets.is_null() || utils.is_null() {
                    let seen = format!(
                        "hsteamuser {huser}, user {} sockets {} utils {}",
                        if user.is_null() { "null" } else { "ok" },
                        if sockets.is_null() { "null" } else { "ok" },
                        if utils.is_null() { "null" } else { "ok" }
                    );
                    if self.last_seen != seen {
                        self.events.push(format!("Steam interfaces: {seen}"));
                        self.last_seen = seen;
                    }
                    if self.started.elapsed() > Duration::from_secs(120) {
                        self.state = "Steam networking is not available (is the game running through Steam?)".into();
                    }
                    return;
                }
                self.sockets = sockets;
                self.utils = utils;
                (self.api.init_relay)(utils);
                if !(self.api.set_status_callback)(utils, on_status_changed) {
                    self.events.push("could not register the connection callback".into());
                }
                self.listen = (self.api.create_listen)(sockets, 0, 0, std::ptr::null());
                if self.listen == 0 {
                    self.state = "could not open the Steam listen socket".into();
                } else {
                    self.state = "ready".into();
                    self.events.push("listening for Steam connections".into());
                }
            }
        }

        /// One step: run Steam's callbacks, accept arrivals, answer pings, drive the outgoing test.
        pub fn poll(&mut self) {
            if self.sockets.is_null() {
                self.setup();
                if self.sockets.is_null() {
                    return;
                }
            }
            // SAFETY: sockets is Steam's singleton; handles passed below came from Steam.
            unsafe {
                (self.api.run_callbacks)(self.sockets);
                let events: Vec<StatusEvent> = QUEUE.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
                for e in events {
                    self.on_event(e);
                }
                let conns: Vec<u32> = self.incoming.iter().copied().chain(self.test.as_ref().map(|t| t.conn)).collect();
                for conn in conns {
                    self.receive(conn);
                }
                self.drive_test();
            }
        }

        unsafe fn on_event(&mut self, e: StatusEvent) {
            let ours = self.test.as_ref().is_some_and(|t| t.conn == e.conn);
            match e.state {
                state::CONNECTING if e.listen_socket != 0 && e.listen_socket == self.listen => {
                    let result = unsafe { (self.api.accept)(self.sockets, e.conn) };
                    self.events.push(format!("a player is connecting (accept result {result})"));
                    self.incoming.push(e.conn);
                }
                state::CONNECTED => {
                    if ours {
                        let route = unsafe { self.route(e.conn) };
                        if let Some(t) = &mut self.test {
                            t.connected = true;
                            t.state = "connected".into();
                            t.route = route;
                            t.next_ping = Instant::now();
                        }
                        self.events.push("test connection established".into());
                    } else {
                        self.events.push("a player connected".into());
                    }
                }
                state::FINDING_ROUTE if ours => {
                    if let Some(t) = &mut self.test {
                        t.state = "finding a route".into();
                    }
                }
                state::CLOSED_BY_PEER | state::PROBLEM_DETECTED_LOCALLY => {
                    let why =
                        if e.state == state::CLOSED_BY_PEER { "closed by the other side" } else { "problem detected" };
                    self.events.push(format!("connection ended: {why} (reason {})", e.end_reason));
                    if ours {
                        if let Some(t) = &mut self.test {
                            t.connected = false;
                            t.state = format!("failed: {why}, reason {}", e.end_reason);
                        }
                    }
                    self.incoming.retain(|c| *c != e.conn);
                    unsafe { (self.api.close)(self.sockets, e.conn, 0, std::ptr::null(), false) };
                }
                _ => {}
            }
        }

        unsafe fn route(&self, conn: u32) -> String {
            let mut info = [0u8; CONNECTION_INFO_SIZE];
            if unsafe { (self.api.connection_info)(self.sockets, conn, info.as_mut_ptr().cast()) } {
                if relay_pop(&info) != 0 {
                    "through a Steam relay".into()
                } else {
                    "direct".into()
                }
            } else {
                "unknown".into()
            }
        }

        unsafe fn receive(&mut self, conn: u32) {
            let mut messages = [std::ptr::null_mut::<c_void>(); 8];
            let n = unsafe { (self.api.receive)(self.sockets, conn, messages.as_mut_ptr(), 8) };
            for &message in messages.iter().take(n.max(0) as usize) {
                if message.is_null() {
                    continue;
                }
                // SAFETY: SteamNetworkingMessage_t starts with m_pData (pointer) and m_cbSize (int at offset 8).
                let (data, len) = unsafe {
                    let p = *(message as *const *const u8);
                    let l = *((message as *const u8).add(8) as *const i32);
                    (p, l.clamp(0, 4096) as usize)
                };
                let payload =
                    if data.is_null() { Vec::new() } else { unsafe { std::slice::from_raw_parts(data, len) }.to_vec() };
                unsafe { (self.api.release_message)(message) };
                if let Some(reply) = reply_for(&payload) {
                    self.pings_received += 1;
                    self.events.push("received a ping, answered".into());
                    unsafe { self.send(conn, &reply) };
                } else if let Some(sent) = pong_id(&payload) {
                    if let Some(t) = &mut self.test {
                        if t.conn == conn {
                            t.pongs += 1;
                            t.last_rtt_ms = Some(now_ms().saturating_sub(sent));
                        }
                    }
                }
            }
        }

        unsafe fn send(&self, conn: u32, payload: &[u8]) {
            // flag 8 = k_nSteamNetworkingSend_Reliable
            unsafe {
                (self.api.send)(
                    self.sockets,
                    conn,
                    payload.as_ptr().cast(),
                    payload.len() as u32,
                    8,
                    std::ptr::null_mut(),
                )
            };
        }

        unsafe fn drive_test(&mut self) {
            let Some(t) = &mut self.test else { return };
            if t.connected && t.sent < 20 && Instant::now() >= t.next_ping {
                let message = format!("ping {}", now_ms());
                let conn = t.conn;
                t.sent += 1;
                t.next_ping = Instant::now() + Duration::from_secs(2);
                unsafe { self.send(conn, message.as_bytes()) };
            }
        }

        /// Starts (or restarts) the outgoing test towards `steam_id`.
        pub fn test(&mut self, steam_id: u64) {
            if self.sockets.is_null() {
                return;
            }
            if let Some(old) = self.test.take() {
                // SAFETY: a handle we got from ConnectP2P.
                unsafe { (self.api.close)(self.sockets, old.conn, 0, std::ptr::null(), false) };
            }
            let identity = identity_bytes(steam_id);
            // SAFETY: identity is IDENTITY_SIZE bytes in the layout ConnectP2P reads.
            let conn =
                unsafe { (self.api.connect_p2p)(self.sockets, identity.as_ptr().cast(), 0, 0, std::ptr::null()) };
            if conn == 0 {
                self.events.push("could not start the test connection".into());
                return;
            }
            self.events.push(format!("connecting to Steam ID {steam_id}"));
            self.test = Some(TestConn {
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

        pub fn report(&self) -> Report {
            Report {
                steam_id: self.steam_id,
                state: self.state.clone(),
                events: self.events.0.iter().cloned().collect(),
                test: self.test.as_ref().map(|t| TestReport {
                    target: t.target,
                    state: t.state.clone(),
                    pongs: t.pongs,
                    last_rtt_ms: t.last_rtt_ms,
                    route: t.route.clone(),
                }),
                pings_received: self.pings_received,
            }
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
            Some(StatusEvent { conn: 77, state: 1, old_state: 0, listen_socket: 5, end_reason: 0 })
        );
        let closed = callback(9, state::CLOSED_BY_PEER, state::CONNECTED, 0, 1002);
        let e = parse_status_event(&closed).unwrap();
        assert_eq!((e.state, e.old_state, e.end_reason), (4, 3, 1002));
        // too short, or a state Steam does not define
        assert_eq!(parse_status_event(&b[..100]), None);
        assert_eq!(parse_status_event(&callback(1, 99, 0, 0, 0)), None);
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
