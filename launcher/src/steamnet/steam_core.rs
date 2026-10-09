//! The Steam networking core: one set of interfaces, one global callback and one poller thread for everything that
//! talks over Steam in this process.
//!
//! Steam delivers connection events through a single process-wide callback and only while somebody calls
//! `RunCallbacks`, and a connection that arrives on a listen socket must be accepted within a second or two. So the
//! core owns that work: its poller thread pumps the callbacks, accepts connections that arrive on a listen socket
//! some [`Owner`] opened, reads the messages of every tracked connection and hands everything to the owner of the
//! connection, in order. Owners are the probe ([`super::Probe`]), the host transport and the client transport
//! ([`crate::steam_transport`]); a connection belongs to exactly one.
//!
//! Locking. `iface` serialises every call into Steam, so nothing here relies on Steam's own threading rules (it is
//! only held for the duration of a call, or of one poll cycle). `registry` maps handles to owners and is always
//! taken *after* `iface`, never before. Owner callbacks run on the poller thread with no lock held, so an owner may
//! call back into the core (send, close, connect) from inside a callback.
//!
//! Everything Steam is called through [`Api`], a table of function pointers: the game's `steam_api64.dll` exports on
//! Windows ([`global`]), an in-memory model in the tests (`super::fake`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{
    end_reason_text, identity_bytes, parse_connection_info, parse_real_time_status, parse_status_event, state,
    StatusEvent, CALLBACK_MIN_SIZE, CONNECTION_INFO_SIZE, REAL_TIME_STATUS_BUFFER,
};

/// A Steam connection or listen socket handle (`HSteamNetConnection`, `HSteamListenSocket`); 0 is invalid.
pub type Handle = u32;

/// `k_cbMaxSteamNetworkingSocketsMessageSizeSend`: the largest message Steam sends and, by default, accepts.
pub const MAX_MESSAGE: usize = 512 * 1024;
/// A received message above this is not copied out of Steam: the connection is closed as a protocol violation.
/// Steam itself refuses more than [`MAX_MESSAGE`] by default; this is the bound if that setting ever changes.
pub const COPY_LIMIT: usize = 2 << 20;
/// Messages asked for in one `ReceiveMessagesOnConnection` call.
const RECEIVE_BATCH: usize = 16;
/// Messages taken from one connection per poll cycle, so one busy connection cannot starve the others.
const RECEIVE_PER_CYCLE: usize = 64;
/// Status events kept between two poll cycles; more are dropped (and counted).
const SINK_LIMIT: usize = 1024;
/// How long a send that finds Steam's send buffer full waits before trying again.
const FULL_RETRY: Duration = Duration::from_millis(2);
/// `k_nSteamNetworkingSend_Reliable`.
const SEND_RELIABLE: i32 = 8;
/// After this long without Steam interfaces the status says Steam networking is not available.
const GIVE_UP_NOTE: Duration = Duration::from_secs(120);

/// `EResult` values `SendMessageToConnection` and `AcceptConnection` return (`steamclientpublic.h`).
pub mod result {
    pub const OK: i32 = 1;
    pub const NO_CONNECTION: i32 = 3;
    pub const INVALID_PARAM: i32 = 8;
    pub const INVALID_STATE: i32 = 11;
    pub const LIMIT_EXCEEDED: i32 = 25;
}

/// `ESteamNetworkingConfigValue` and friends (`steamnetworkingtypes.h`), for `SetConfigValue`.
pub mod config_value {
    pub const SEND_RATE_MIN: i32 = 10;
    pub const SEND_RATE_MAX: i32 = 11;
    pub const TIMEOUT_INITIAL: i32 = 24;
    pub const SCOPE_GLOBAL: i32 = 1;
    pub const TYPE_INT32: i32 = 1;
}

/// Where Steam's interfaces are, and who this account is.
pub(super) struct Located {
    /// `ISteamNetworkingSockets*`, null while Steam is not initialised.
    pub sockets: *mut c_void,
    /// `ISteamNetworkingUtils*`, null while Steam is not initialised.
    pub utils: *mut c_void,
    pub steam_id: Option<u64>,
    /// What was found, for the diagnostics while waiting.
    pub note: String,
}

type LocateFn = Box<dyn Fn() -> Located + Send + Sync>;
type CallbackFn = unsafe extern "C" fn(*mut c_void);
type SetConfigFn = unsafe extern "C" fn(*mut c_void, i32, i32, isize, i32, *const c_void) -> bool;
type RealTimeStatusFn = unsafe extern "C" fn(*mut c_void, u32, *mut c_void, i32, *mut c_void) -> i32;

/// The Steam functions the core uses (the flat API of `steam_api64.dll`, `steamnetworkingsockets_flat.h`).
pub(super) struct Api {
    /// Finds the interfaces; called again and again until Steam is initialised.
    pub locate: LocateFn,
    pub init_relay: unsafe extern "C" fn(*mut c_void),
    pub set_status_callback: unsafe extern "C" fn(*mut c_void, CallbackFn) -> bool,
    /// `ISteamNetworkingUtils::SetConfigValue`; the tuning is skipped when the export is missing.
    pub set_config: Option<SetConfigFn>,
    pub run_callbacks: unsafe extern "C" fn(*mut c_void),
    pub create_listen: unsafe extern "C" fn(*mut c_void, i32, i32, *const c_void) -> u32,
    /// An absent export only costs a leaked listen socket for the rest of the process.
    pub close_listen: Option<unsafe extern "C" fn(*mut c_void, u32) -> bool>,
    pub connect_p2p: unsafe extern "C" fn(*mut c_void, *const c_void, i32, i32, *const c_void) -> u32,
    pub accept: unsafe extern "C" fn(*mut c_void, u32) -> i32,
    pub send: unsafe extern "C" fn(*mut c_void, u32, *const c_void, u32, i32, *mut i64) -> i32,
    pub receive: unsafe extern "C" fn(*mut c_void, u32, *mut *mut c_void, i32) -> i32,
    pub release_message: unsafe extern "C" fn(*mut c_void),
    pub close: unsafe extern "C" fn(*mut c_void, u32, i32, *const c_char, bool) -> bool,
    pub connection_info: unsafe extern "C" fn(*mut c_void, u32, *mut c_void) -> bool,
    /// `GetConnectionRealTimeStatus` (ping, quality, send rate); a missing export only means no ping.
    pub real_time_status: Option<RealTimeStatusFn>,
}

/// How the poller paces itself, and what it asks Steam for. The defaults are what the game uses.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Sleep between poll cycles while there is a listen socket or a connection. Every message waits for the next
    /// cycle, so this is the latency the core adds; the probe's 100 ms polling read as 199 ms on a real relay link.
    pub active: Duration,
    /// Sleep between poll cycles while there is nothing to serve.
    pub idle: Duration,
    /// How often to look for Steam's interfaces until they exist.
    pub locate: Duration,
    /// How often an open connection's route and ping are measured again. Steam may move a relayed connection to a
    /// direct path after it opened, so the route is never settled.
    pub link_every: Duration,
    /// `TimeoutInitial`: how long Steam keeps looking for a route before it fails a new connection. Its own default
    /// is 10 s; a route through the relays can take longer, so the transports' connect timeout is the limit instead.
    pub initial_timeout_ms: i32,
    /// `SendRateMin` and `SendRateMax` in bytes per second. Steam's default is 256 KiB/s, which makes a 31 MB
    /// savegame take two minutes.
    pub send_rate: i32,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            active: Duration::from_millis(2),
            idle: Duration::from_millis(100),
            locate: Duration::from_millis(250),
            link_every: Duration::from_secs(1),
            initial_timeout_ms: 45_000,
            send_rate: 1 << 20,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Owners and what they are told
// ---------------------------------------------------------------------------------------------------------------

/// Why a connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// We closed it ([`CoreRef::close`]).
    Local,
    /// The other side closed it.
    Peer,
    /// Steam detected a problem (timeout, no route, network down).
    Problem,
    /// The other side broke the message rules; we closed it.
    Violation,
    /// The core stopped.
    Shutdown,
}

/// The last thing an owner hears about a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct End {
    /// The connection had been established (an owner that never saw it open usually has nothing to report).
    pub opened: bool,
    pub cause: Cause,
    /// Steam's end reason code (`ESteamNetConnectionEnd`), 0 when it gave none.
    pub code: i32,
    /// Steam's explanation or ours, may be empty.
    pub detail: String,
}

impl End {
    fn new(opened: bool, cause: Cause) -> End {
        End { opened, cause, code: 0, detail: String::new() }
    }

    /// A sentence for logs and error messages.
    pub fn describe(&self) -> String {
        let what = match self.cause {
            Cause::Local => return "closed locally".to_string(),
            Cause::Shutdown => return "Steam networking stopped".to_string(),
            Cause::Violation => return self.detail.clone(),
            Cause::Peer => "closed by the other side",
            Cause::Problem => "connection problem",
        };
        let mut text = format!("{what}: {}", end_reason_text(self.code));
        if !self.detail.is_empty() {
            text.push_str(&format!(" (Steam says: {})", self.detail));
        }
        text
    }
}

/// Whoever owns a connection or a listen socket. Called on the poller thread, in order for each connection, with no
/// core lock held; the callbacks of one handle never overlap.
pub trait Owner: Send + Sync {
    /// The connection is established (Steam state Connected). `incoming` is true for a peer that connected to one of
    /// our listen sockets, `remote` is the peer's Steam ID when Steam said.
    fn opened(&self, _conn: Handle, _incoming: bool, _remote: Option<u64>) {}
    /// The connection moved through an intermediate state ([`state::CONNECTING`], [`state::FINDING_ROUTE`]).
    fn progress(&self, _conn: Handle, _state: i32) {}
    /// One message arrived (one Steam message is one unit; the owner defines what it holds).
    fn message(&self, conn: Handle, data: &[u8]);
    /// The connection is over. The last call for `conn`, made exactly once.
    fn closed(&self, conn: Handle, end: End);
}

/// What the poller found, to be handed to owners once the locks are released.
enum Notice {
    Opened { owner: Arc<dyn Owner>, conn: Handle, incoming: bool, remote: Option<u64> },
    Progress { owner: Arc<dyn Owner>, conn: Handle, state: i32 },
    Message { owner: Arc<dyn Owner>, conn: Handle, data: Vec<u8> },
    Closed { owner: Arc<dyn Owner>, conn: Handle, end: End },
}

impl Notice {
    fn dispatch(self) {
        match self {
            Notice::Opened { owner, conn, incoming, remote } => owner.opened(conn, incoming, remote),
            Notice::Progress { owner, conn, state } => owner.progress(conn, state),
            Notice::Message { owner, conn, data } => owner.message(conn, &data),
            Notice::Closed { owner, conn, end } => owner.closed(conn, end),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Errors and small answers
// ---------------------------------------------------------------------------------------------------------------

/// Why the core could not do something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    /// Steam is not initialised yet (the text says what was found).
    NotReady(String),
    /// The core was stopped.
    Stopped,
    /// Steam refused.
    Refused(String),
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreError::NotReady(note) => write!(f, "Steam networking is not ready ({note})"),
            CoreError::Stopped => f.write_str("Steam networking was stopped"),
            CoreError::Refused(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for CoreError {}

impl From<CoreError> for io::Error {
    fn from(e: CoreError) -> io::Error {
        let kind = match e {
            CoreError::NotReady(_) => io::ErrorKind::NotConnected,
            CoreError::Stopped => io::ErrorKind::BrokenPipe,
            CoreError::Refused(_) => io::ErrorKind::Other,
        };
        io::Error::new(kind, e.to_string())
    }
}

/// The outcome of handing one message to Steam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    /// Steam's send buffer is full; try again shortly.
    Full,
    /// Over [`MAX_MESSAGE`]; Steam would refuse it.
    TooBig,
    /// The connection is closed, not yet open, or not ours.
    Closed,
    /// Steam answered with another `EResult`.
    Failed(i32),
}

/// How a connection reaches the other side, for the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Direct,
    Relayed,
}

impl Route {
    pub fn text(self) -> &'static str {
        match self {
            Route::Direct => "direct",
            Route::Relayed => "through a Steam relay",
        }
    }
}

/// What the core last measured about an open connection (every [`Config::link_every`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinkSnapshot {
    /// Where the packets go now; Steam may switch a relayed connection to a direct path later.
    pub route: Route,
    /// Steam's own round-trip estimate in ms, `None` while it has none.
    pub ping_ms: Option<u32>,
    /// Packets delivered end to end in order, 0 to 1.
    pub quality: f32,
    /// The rate Steam sends at, in bytes per second; 0 when Steam did not say.
    pub send_rate: u32,
    /// Reliable bytes queued and not yet on the wire.
    pub pending: u32,
}

/// What the core knows about Steam, readable without touching Steam.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoreStatus {
    /// Interfaces found, relay access requested, connection callback registered.
    pub ready: bool,
    /// This account's Steam ID, once Steam said.
    pub steam_id: Option<u64>,
    /// While waiting: what was found. Once ready: how the tuning went.
    pub note: String,
}

// ---------------------------------------------------------------------------------------------------------------
// Status events from Steam's global callback
// ---------------------------------------------------------------------------------------------------------------

/// Status events from Steam's callback, queued for the poller.
#[derive(Default)]
struct EventSink {
    events: Mutex<Vec<StatusEvent>>,
    dropped: Mutex<u64>,
}

impl EventSink {
    fn push(&self, event: StatusEvent) {
        let mut events = lock(&self.events);
        if events.len() < SINK_LIMIT {
            events.push(event);
        } else {
            *lock(&self.dropped) += 1;
        }
    }

    fn take(&self) -> Vec<StatusEvent> {
        std::mem::take(&mut *lock(&self.events))
    }
}

thread_local! {
    /// The sink of the core whose poller runs on this thread. Steam calls the callback on the thread that runs
    /// `RunCallbacks`, which is the poller; with several cores in one process (the tests) this keeps them apart.
    static THREAD_SINK: RefCell<Option<Arc<EventSink>>> = const { RefCell::new(None) };
}

/// The sink of the process-wide core. Steam may also call the callback from a thread of the game that runs
/// `SteamAPI_RunCallbacks`; such events land here.
static PROCESS_SINK: Mutex<Option<Arc<EventSink>>> = Mutex::new(None);

/// The global connection-status callback. It only queues the event: it runs inside Steam's call.
pub(super) unsafe extern "C" fn on_status_changed(callback: *mut c_void) {
    if callback.is_null() {
        return;
    }
    // SAFETY: Steam passes a SteamNetConnectionStatusChangedCallback_t, CALLBACK_MIN_SIZE bytes long.
    let bytes = unsafe { std::slice::from_raw_parts(callback as *const u8, CALLBACK_MIN_SIZE) };
    let Some(event) = parse_status_event(bytes) else { return };
    let sink = THREAD_SINK.try_with(|s| s.borrow().clone()).ok().flatten().or_else(|| lock(&PROCESS_SINK).clone());
    if let Some(sink) = sink {
        sink.push(event);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The core
// ---------------------------------------------------------------------------------------------------------------

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Steam's interface pointers, behind the lock that serialises every call into Steam.
struct Iface {
    sockets: *mut c_void,
    next_locate: Instant,
}

// SAFETY: the pointer is Steam's process-wide singleton; it is only used while the `iface` mutex is held.
unsafe impl Send for Iface {}

struct Tracked {
    owner: Arc<dyn Owner>,
    /// Arrived on one of our listen sockets.
    incoming: bool,
    /// Steam reported it connected.
    open: bool,
    remote: Option<u64>,
    /// The latest measurement of an open connection, and when the next one is due.
    link: Option<LinkSnapshot>,
    link_due: Instant,
}

impl Tracked {
    fn new(owner: Arc<dyn Owner>, incoming: bool, remote: Option<u64>) -> Tracked {
        Tracked { owner, incoming, open: false, remote, link: None, link_due: Instant::now() }
    }
}

#[derive(Default)]
struct Registry {
    listeners: HashMap<Handle, Arc<dyn Owner>>,
    conns: HashMap<Handle, Tracked>,
}

struct Shared {
    api: Api,
    config: Config,
    iface: Mutex<Iface>,
    registry: Mutex<Registry>,
    sink: Arc<EventSink>,
    /// Notices made by calls from other threads (a local close), delivered by the poller with the rest.
    local: Mutex<Vec<Notice>>,
    status: Mutex<CoreStatus>,
    status_changed: Condvar,
    woken: Mutex<bool>,
    wake: Condvar,
    stopped: AtomicBool,
    created: Instant,
}

/// The owner of the poller thread. Dropping it stops the thread and closes everything it still tracks. The process
/// has one ([`global`]); the tests make one per fake machine.
pub struct SteamCore {
    shared: Arc<Shared>,
    poller: Mutex<Option<JoinHandle<()>>>,
}

/// A cheap handle for the services that use the core.
#[derive(Clone)]
pub struct CoreRef(Arc<Shared>);

impl SteamCore {
    /// Starts the poller. It waits for Steam on its own; nothing here needs Steam to be up yet.
    pub(super) fn start(api: Api, config: Config) -> io::Result<SteamCore> {
        let shared = Arc::new(Shared {
            api,
            config,
            iface: Mutex::new(Iface { sockets: std::ptr::null_mut(), next_locate: Instant::now() }),
            registry: Mutex::new(Registry::default()),
            sink: Arc::new(EventSink::default()),
            local: Mutex::new(Vec::new()),
            status: Mutex::new(CoreStatus { ready: false, steam_id: None, note: "waiting for Steam".into() }),
            status_changed: Condvar::new(),
            woken: Mutex::new(false),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            created: Instant::now(),
        });
        let poller = {
            let shared = Arc::clone(&shared);
            thread::Builder::new().name("feverlink3-steam".into()).spawn(move || shared.run())?
        };
        Ok(SteamCore { shared, poller: Mutex::new(Some(poller)) })
    }

    pub fn handle(&self) -> CoreRef {
        CoreRef(Arc::clone(&self.shared))
    }
}

impl Drop for SteamCore {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        self.shared.wake();
        self.shared.status_changed.notify_all();
        if let Some(poller) = lock(&self.poller).take() {
            let _ = poller.join();
        }
        self.shared.sweep();
    }
}

impl Shared {
    fn wake(&self) {
        *lock(&self.woken) = true;
        self.wake.notify_all();
    }

    /// Waits between two poll cycles. While there is something to serve this is a plain short sleep (waiting on a
    /// condition variable is only as precise as the system timer tick on Windows); with nothing to serve the poller
    /// sleeps longer and a new listen socket or connection wakes it.
    fn nap(&self, busy: bool) {
        if busy {
            thread::sleep(self.config.active);
            return;
        }
        let guard = lock(&self.woken);
        let (mut guard, _) =
            self.wake.wait_timeout_while(guard, self.config.idle, |woken| !*woken).unwrap_or_else(|p| p.into_inner());
        *guard = false;
    }

    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    fn busy(&self) -> bool {
        let reg = lock(&self.registry);
        !reg.listeners.is_empty() || !reg.conns.is_empty()
    }

    fn run(self: &Arc<Self>) {
        // the callback routes by thread: this poller is the thread that runs RunCallbacks
        let _ = THREAD_SINK.try_with(|s| *s.borrow_mut() = Some(Arc::clone(&self.sink)));
        while !self.stopped() {
            for notice in self.cycle() {
                notice.dispatch();
            }
            self.nap(self.busy());
        }
    }

    fn set_status(&self, f: impl FnOnce(&mut CoreStatus)) {
        f(&mut lock(&self.status));
        self.status_changed.notify_all();
    }

    /// One pass: find Steam if needed, pump the callbacks, react to events, read messages.
    fn cycle(&self) -> Vec<Notice> {
        let mut out = Vec::new();
        let mut iface = lock(&self.iface);
        if iface.sockets.is_null() {
            self.try_locate(&mut iface);
            if iface.sockets.is_null() {
                return out;
            }
        } else if lock(&self.status).steam_id.is_none() && Instant::now() >= iface.next_locate {
            // the account may only become known after the interfaces
            iface.next_locate = Instant::now() + self.config.locate;
            if let Some(id) = (self.api.locate)().steam_id {
                self.set_status(|s| s.steam_id = Some(id));
            }
        }
        let sockets = iface.sockets;
        // SAFETY: sockets is Steam's ISteamNetworkingSockets singleton, located above; the iface lock is held.
        unsafe { (self.api.run_callbacks)(sockets) };
        for event in self.sink.take() {
            self.on_event(sockets, event, &mut out);
        }
        let open: Vec<(Handle, Arc<dyn Owner>)> = {
            let reg = lock(&self.registry);
            reg.conns.iter().filter(|(_, t)| t.open).map(|(&h, t)| (h, Arc::clone(&t.owner))).collect()
        };
        for (conn, owner) in open {
            match self.receive(sockets, conn, &owner, &mut out) {
                Receive::Fine => {}
                Receive::Violation(detail) => self.end_tracked(sockets, conn, Cause::Violation, 0, detail, &mut out),
                Receive::Gone => {
                    let detail = "Steam no longer knows the connection".to_string();
                    self.end_tracked(sockets, conn, Cause::Problem, 0, detail, &mut out);
                }
            }
        }
        self.refresh_links(sockets);
        out.append(&mut lock(&self.local));
        out
    }

    /// Measures the route and the ping of the open connections whose turn has come. It never stops: Steam may move
    /// a relayed connection to a direct path some time after it opened.
    fn refresh_links(&self, sockets: *mut c_void) {
        let now = Instant::now();
        let due: Vec<Handle> =
            lock(&self.registry).conns.iter().filter(|(_, t)| t.open && now >= t.link_due).map(|(&h, _)| h).collect();
        for conn in due {
            let snapshot = self.sample(sockets, conn);
            if let Some(t) = lock(&self.registry).conns.get_mut(&conn) {
                t.link_due = now + self.config.link_every;
                t.link = snapshot.or(t.link);
            }
        }
    }

    /// One measurement: the route from the connection info, the ping and send rate from the real-time status.
    fn sample(&self, sockets: *mut c_void, conn: Handle) -> Option<LinkSnapshot> {
        let mut info = [0u8; CONNECTION_INFO_SIZE];
        // SAFETY: info is CONNECTION_INFO_SIZE bytes, the size of SteamNetConnectionInfo_t; the iface lock is held.
        let known = unsafe { (self.api.connection_info)(sockets, conn, info.as_mut_ptr().cast()) };
        let route =
            if parse_connection_info(known.then_some(&info[..])?)?.relayed { Route::Relayed } else { Route::Direct };
        let real = self.api.real_time_status.and_then(|status| {
            let mut buf = [0u8; REAL_TIME_STATUS_BUFFER];
            // SAFETY: buf is larger than SteamNetConnectionRealTimeStatus_t; no lanes are asked for (0 and null), as
            // the header allows; the iface lock is held.
            let code = unsafe { status(sockets, conn, buf.as_mut_ptr().cast(), 0, std::ptr::null_mut()) };
            if code == result::OK {
                parse_real_time_status(&buf)
            } else {
                None
            }
        });
        Some(LinkSnapshot {
            route,
            ping_ms: real.and_then(|r| r.ping_ms),
            quality: real.map_or(1.0, |r| r.quality_local),
            send_rate: real.map_or(0, |r| r.send_rate),
            pending: real.map_or(0, |r| r.pending_reliable),
        })
    }

    /// Looks for Steam's interfaces; on success asks for relay access, registers the global callback and applies
    /// the tuning.
    fn try_locate(&self, iface: &mut Iface) {
        if Instant::now() < iface.next_locate {
            return;
        }
        iface.next_locate = Instant::now() + self.config.locate;
        let found = (self.api.locate)();
        if found.sockets.is_null() || found.utils.is_null() {
            let waited = self.created.elapsed();
            self.set_status(|s| {
                s.steam_id = found.steam_id.or(s.steam_id);
                s.note = if waited > GIVE_UP_NOTE {
                    "Steam networking is not available (is the game running through Steam?)".to_string()
                } else {
                    format!("Steam interfaces: {}", found.note)
                };
            });
            return;
        }
        // SAFETY: utils is Steam's ISteamNetworkingUtils singleton, valid once Steam is initialised; the iface lock
        // is held. Registering the same callback again is harmless, so a failed attempt is simply retried.
        let registered = unsafe {
            (self.api.init_relay)(found.utils);
            (self.api.set_status_callback)(found.utils, on_status_changed)
        };
        if !registered {
            self.set_status(|s| s.note = "could not register the connection callback".into());
            return;
        }
        let tuning = self.tune(found.utils);
        iface.sockets = found.sockets;
        self.set_status(|s| {
            s.ready = true;
            s.steam_id = found.steam_id.or(s.steam_id);
            s.note = tuning;
        });
    }

    /// Best-effort connection settings; a refusal is reported in the status note and changes nothing else.
    fn tune(&self, utils: *mut c_void) -> String {
        let Some(set) = self.api.set_config else {
            return "tuning unavailable (SetConfigValue is not exported)".into();
        };
        let settings = [
            ("initial timeout", config_value::TIMEOUT_INITIAL, self.config.initial_timeout_ms),
            ("send rate min", config_value::SEND_RATE_MIN, self.config.send_rate),
            ("send rate max", config_value::SEND_RATE_MAX, self.config.send_rate),
        ];
        let mut refused = Vec::new();
        for (name, value, number) in &settings {
            // SAFETY: SetConfigValue reads one int32 through the pointer; utils is Steam's singleton.
            let accepted = unsafe {
                set(
                    utils,
                    *value,
                    config_value::SCOPE_GLOBAL,
                    0,
                    config_value::TYPE_INT32,
                    std::ptr::from_ref(number).cast(),
                )
            };
            if !accepted {
                refused.push(*name);
            }
        }
        if refused.is_empty() {
            format!(
                "tuned: initial timeout {} ms, send rate {} bytes/s",
                self.config.initial_timeout_ms, self.config.send_rate
            )
        } else {
            format!("tuning refused for: {}", refused.join(", "))
        }
    }

    fn on_event(&self, sockets: *mut c_void, e: StatusEvent, out: &mut Vec<Notice>) {
        let mut reg = lock(&self.registry);
        match e.state {
            state::CONNECTING | state::FINDING_ROUTE => {
                if let Some(t) = reg.conns.get(&e.conn) {
                    out.push(Notice::Progress { owner: Arc::clone(&t.owner), conn: e.conn, state: e.state });
                } else if e.state == state::CONNECTING && e.listen_socket != 0 {
                    // a peer connecting to one of our listen sockets: accept now, Steam times the peer out otherwise
                    let Some(owner) = reg.listeners.get(&e.listen_socket).cloned() else { return };
                    // SAFETY: e.conn is a handle Steam just reported; the iface lock is held.
                    let accepted = unsafe { (self.api.accept)(sockets, e.conn) };
                    if accepted == result::OK {
                        reg.conns.insert(e.conn, Tracked::new(owner, true, e.remote));
                    } else {
                        // SAFETY: as above.
                        unsafe { (self.api.close)(sockets, e.conn, 0, std::ptr::null(), false) };
                    }
                }
            }
            state::CONNECTED => {
                if let Some(t) = reg.conns.get_mut(&e.conn) {
                    if !t.open {
                        t.open = true;
                        out.push(Notice::Opened {
                            owner: Arc::clone(&t.owner),
                            conn: e.conn,
                            incoming: t.incoming,
                            remote: t.remote,
                        });
                    }
                }
            }
            state::CLOSED_BY_PEER | state::PROBLEM_DETECTED_LOCALLY => {
                drop(reg);
                let cause = if e.state == state::CLOSED_BY_PEER { Cause::Peer } else { Cause::Problem };
                // messages that arrived before the close can still be read
                self.drain_then_end(sockets, e.conn, cause, e.end_reason, e.end_debug, out);
            }
            _ => {}
        }
    }

    /// A tracked connection reached a terminal state: pass on what is still queued, report the end, free it.
    fn drain_then_end(
        &self,
        sockets: *mut c_void,
        conn: Handle,
        cause: Cause,
        code: i32,
        detail: String,
        out: &mut Vec<Notice>,
    ) {
        let Some(owner) = lock(&self.registry).conns.get(&conn).map(|t| Arc::clone(&t.owner)) else { return };
        // a violation while draining is ignored: the connection is ending anyway
        let _ = self.receive(sockets, conn, &owner, out);
        self.end_tracked(sockets, conn, cause, code, detail, out);
    }

    /// Reports the end of a tracked connection and closes it in Steam. Nothing happens for an unknown handle.
    fn end_tracked(
        &self,
        sockets: *mut c_void,
        conn: Handle,
        cause: Cause,
        code: i32,
        detail: String,
        out: &mut Vec<Notice>,
    ) {
        let Some(t) = lock(&self.registry).conns.remove(&conn) else { return };
        // SAFETY: conn is a handle Steam gave us; the iface lock is held. For a connection that has already ended
        // Steam ignores the reason and only frees it.
        unsafe { (self.api.close)(sockets, conn, 0, std::ptr::null(), false) };
        out.push(Notice::Closed { owner: t.owner, conn, end: End { opened: t.open, cause, code, detail } });
    }

    /// Reads what is queued on `conn` (bounded per cycle) into `out`.
    fn receive(&self, sockets: *mut c_void, conn: Handle, owner: &Arc<dyn Owner>, out: &mut Vec<Notice>) -> Receive {
        let mut taken = 0;
        while taken < RECEIVE_PER_CYCLE {
            let mut messages = [std::ptr::null_mut::<c_void>(); RECEIVE_BATCH];
            // SAFETY: messages has room for RECEIVE_BATCH pointers; the iface lock is held.
            let n = unsafe { (self.api.receive)(sockets, conn, messages.as_mut_ptr(), RECEIVE_BATCH as i32) };
            let Ok(n) = usize::try_from(n) else { return Receive::Gone };
            let mut violation = None;
            for &message in messages.iter().take(n) {
                if message.is_null() {
                    continue;
                }
                // SAFETY: a message ReceiveMessagesOnConnection returned and nobody released yet.
                match unsafe { self.take_message(message) } {
                    Ok(data) if violation.is_none() => {
                        out.push(Notice::Message { owner: Arc::clone(owner), conn, data });
                    }
                    Ok(_) => {}
                    Err(size) => {
                        violation = Some(format!("message of {size} bytes exceeds the {COPY_LIMIT} byte limit"))
                    }
                }
            }
            if let Some(why) = violation {
                return Receive::Violation(why);
            }
            taken += n;
            if n < RECEIVE_BATCH {
                break;
            }
        }
        Receive::Fine
    }

    /// Copies a message out of Steam's buffer and releases it; `Err(size)` for one above [`COPY_LIMIT`].
    ///
    /// # Safety
    /// `message` is a `SteamNetworkingMessage_t*` from `ReceiveMessagesOnConnection`, not released yet.
    unsafe fn take_message(&self, message: *mut c_void) -> Result<Vec<u8>, usize> {
        // SAFETY: the structure starts with m_pData (a pointer) and m_cbSize (an int at offset 8), see
        // steamnetworkingtypes.h; the reads are unaligned, so only the layout matters.
        let (data, size) = unsafe {
            (
                std::ptr::read_unaligned(message as *const *const u8),
                std::ptr::read_unaligned((message as *const u8).add(8) as *const i32),
            )
        };
        let size = usize::try_from(size).unwrap_or(0);
        let copied = if size > COPY_LIMIT {
            Err(size)
        } else if data.is_null() || size == 0 {
            Ok(Vec::new())
        } else {
            // SAFETY: Steam guarantees m_pData holds m_cbSize bytes until the message is released.
            Ok(unsafe { std::slice::from_raw_parts(data, size) }.to_vec())
        };
        // SAFETY: the message is ours to release, exactly once.
        unsafe { (self.api.release_message)(message) };
        copied
    }

    /// Closes everything that is still tracked (the core is stopping) and tells the owners.
    fn sweep(&self) {
        let (listeners, conns) = {
            let mut reg = lock(&self.registry);
            (std::mem::take(&mut reg.listeners), std::mem::take(&mut reg.conns))
        };
        {
            let iface = lock(&self.iface);
            if !iface.sockets.is_null() {
                for conn in conns.keys() {
                    // SAFETY: a handle Steam gave us; the iface lock is held.
                    unsafe { (self.api.close)(iface.sockets, *conn, 0, std::ptr::null(), false) };
                }
                if let Some(close_listen) = self.api.close_listen {
                    for listener in listeners.keys() {
                        // SAFETY: a listen socket Steam gave us; the iface lock is held.
                        unsafe { close_listen(iface.sockets, *listener) };
                    }
                }
            }
        }
        for (conn, t) in conns {
            t.owner.closed(conn, End::new(t.open, Cause::Shutdown));
        }
        for notice in std::mem::take(&mut *lock(&self.local)) {
            notice.dispatch();
        }
    }
}

enum Receive {
    Fine,
    /// A message above the copy limit; the connection must be closed.
    Violation(String),
    /// Steam does not know the handle (any more).
    Gone,
}

impl CoreRef {
    /// What the core knows about Steam.
    pub fn status(&self) -> CoreStatus {
        lock(&self.0.status).clone()
    }

    /// Waits until Steam is ready, `deadline` passes or `cancelled` says so.
    pub fn wait_ready(&self, deadline: Instant, cancelled: &dyn Fn() -> bool) -> Result<(), CoreError> {
        let mut status = lock(&self.0.status);
        loop {
            if status.ready {
                return Ok(());
            }
            if self.0.stopped() {
                return Err(CoreError::Stopped);
            }
            let now = Instant::now();
            if now >= deadline || cancelled() {
                return Err(CoreError::NotReady(status.note.clone()));
            }
            let step = (deadline - now).min(Duration::from_millis(20));
            status = self.0.status_changed.wait_timeout(status, step).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    fn lock_ready(&self) -> Result<MutexGuard<'_, Iface>, CoreError> {
        if self.0.stopped() {
            return Err(CoreError::Stopped);
        }
        let iface = lock(&self.0.iface);
        if iface.sockets.is_null() {
            return Err(CoreError::NotReady(lock(&self.0.status).note.clone()));
        }
        Ok(iface)
    }

    /// Opens a P2P listen socket on `virtual_port`; connections that arrive on it belong to `owner`.
    pub fn listen(&self, virtual_port: i32, owner: Arc<dyn Owner>) -> Result<Handle, CoreError> {
        let iface = self.lock_ready()?;
        // SAFETY: sockets is Steam's singleton (checked by lock_ready); no options are passed.
        let listener = unsafe { (self.0.api.create_listen)(iface.sockets, virtual_port, 0, std::ptr::null()) };
        if listener == 0 {
            return Err(CoreError::Refused(format!(
                "Steam would not open a listen socket on virtual port {virtual_port} (already in use?)"
            )));
        }
        lock(&self.0.registry).listeners.insert(listener, owner);
        drop(iface);
        self.0.wake();
        Ok(listener)
    }

    /// Closes a listen socket. Connections accepted on it stay until they are closed or end.
    pub fn close_listener(&self, listener: Handle) {
        let Ok(iface) = self.lock_ready() else { return };
        if lock(&self.0.registry).listeners.remove(&listener).is_none() {
            return;
        }
        if let Some(close_listen) = self.0.api.close_listen {
            // SAFETY: a listen socket Steam gave us; the iface lock is held.
            unsafe { close_listen(iface.sockets, listener) };
        }
    }

    /// Starts connecting to the account `remote`, listening on its `virtual_port`. The owner hears
    /// [`Owner::progress`] and then [`Owner::opened`], or [`Owner::closed`] when no route is found.
    pub fn connect(&self, remote: u64, virtual_port: i32, owner: Arc<dyn Owner>) -> Result<Handle, CoreError> {
        let iface = self.lock_ready()?;
        let identity = identity_bytes(remote);
        // SAFETY: identity is IDENTITY_SIZE bytes in the layout ConnectP2P reads; no options are passed.
        let conn = unsafe {
            (self.0.api.connect_p2p)(iface.sockets, identity.as_ptr().cast(), virtual_port, 0, std::ptr::null())
        };
        if conn == 0 {
            return Err(CoreError::Refused("Steam could not start the connection".into()));
        }
        lock(&self.0.registry).conns.insert(conn, Tracked::new(owner, false, Some(remote)));
        drop(iface);
        self.0.wake();
        Ok(conn)
    }

    /// Hands one reliable message to Steam. Only an open connection of ours accepts one.
    pub fn send(&self, conn: Handle, data: &[u8]) -> SendOutcome {
        if data.len() > MAX_MESSAGE {
            return SendOutcome::TooBig;
        }
        let Ok(iface) = self.lock_ready() else { return SendOutcome::Closed };
        if !lock(&self.0.registry).conns.get(&conn).is_some_and(|t| t.open) {
            return SendOutcome::Closed;
        }
        // SAFETY: data is valid for data.len() bytes (at most MAX_MESSAGE, so the u32 cast is lossless); conn is a
        // handle of ours; the iface lock is held.
        let code = unsafe {
            (self.0.api.send)(
                iface.sockets,
                conn,
                data.as_ptr().cast(),
                data.len() as u32,
                SEND_RELIABLE,
                std::ptr::null_mut(),
            )
        };
        match code {
            result::OK => SendOutcome::Sent,
            result::LIMIT_EXCEEDED => SendOutcome::Full,
            result::NO_CONNECTION | result::INVALID_STATE | result::INVALID_PARAM => SendOutcome::Closed,
            other => SendOutcome::Failed(other),
        }
    }

    /// As [`CoreRef::send`], but waits while Steam's send buffer is full (it holds 512 KiB, so a stream of savegame
    /// chunks needs this), up to `timeout`.
    pub fn send_reliable(&self, conn: Handle, data: &[u8], timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.send(conn, data) {
                SendOutcome::Sent => return Ok(()),
                SendOutcome::Full if Instant::now() < deadline => thread::sleep(FULL_RETRY),
                SendOutcome::Full => {
                    let why = format!("Steam's send buffer stayed full for {} ms", timeout.as_millis());
                    return Err(io::Error::new(io::ErrorKind::TimedOut, why));
                }
                SendOutcome::TooBig => {
                    let why = format!("message of {} bytes exceeds Steam's limit of {MAX_MESSAGE}", data.len());
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, why));
                }
                SendOutcome::Closed => return Err(io::Error::new(io::ErrorKind::NotConnected, "connection closed")),
                SendOutcome::Failed(code) => {
                    return Err(io::Error::other(format!("Steam refused the message (result {code})")))
                }
            }
        }
    }

    /// Closes a connection of ours. What was already sent is still delivered (Steam's "linger"); the owner then
    /// hears [`Owner::closed`] with [`Cause::Local`] on the poller thread. False when the handle is not tracked
    /// (already ended or closed).
    pub fn close(&self, conn: Handle) -> bool {
        let Ok(iface) = self.lock_ready() else { return false };
        let Some(t) = lock(&self.0.registry).conns.remove(&conn) else { return false };
        // SAFETY: conn is a handle Steam gave us; the iface lock is held.
        unsafe { (self.0.api.close)(iface.sockets, conn, 0, std::ptr::null(), true) };
        lock(&self.0.local).push(Notice::Closed { owner: t.owner, conn, end: End::new(t.open, Cause::Local) });
        drop(iface);
        self.0.wake();
        true
    }

    /// Whether the connection is tracked and established.
    pub fn is_open(&self, conn: Handle) -> bool {
        lock(&self.0.registry).conns.get(&conn).is_some_and(|t| t.open)
    }

    /// Direct or through a relay, `None` when Steam does not know the connection.
    pub fn route(&self, conn: Handle) -> Option<Route> {
        let iface = self.lock_ready().ok()?;
        let mut info = [0u8; CONNECTION_INFO_SIZE];
        // SAFETY: info is CONNECTION_INFO_SIZE bytes, the size of SteamNetConnectionInfo_t; the iface lock is held.
        let known = unsafe { (self.0.api.connection_info)(iface.sockets, conn, info.as_mut_ptr().cast()) };
        if !known {
            return None;
        }
        let info = parse_connection_info(&info)?;
        Some(if info.relayed { Route::Relayed } else { Route::Direct })
    }

    /// The latest measurement of an open connection: route, ping, send rate. It comes from the poller's periodic
    /// sampling, so this costs no call into Steam; `None` until the first sample (one poll cycle after the open)
    /// and for a connection the core does not track.
    pub fn link(&self, conn: Handle) -> Option<LinkSnapshot> {
        lock(&self.0.registry).conns.get(&conn).and_then(|t| t.link)
    }

    /// Listen sockets and connections the core tracks now.
    pub fn tracked(&self) -> (usize, usize) {
        let reg = lock(&self.0.registry);
        (reg.listeners.len(), reg.conns.len())
    }

    /// Status events Steam's callback had to drop because the poller was not draining them.
    pub fn dropped_events(&self) -> u64 {
        *lock(&self.0.sink.dropped)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The process-wide core
// ---------------------------------------------------------------------------------------------------------------

static GLOBAL: Mutex<Option<SteamCore>> = Mutex::new(None);

/// The core of this process, started on first use. `None` when the process has no `steam_api64.dll` loaded (a test
/// run, a game started without Steam) or the poller thread cannot start; asking again later tries again, because
/// the game may load Steam late.
pub fn global() -> Option<CoreRef> {
    let mut slot = lock(&GLOBAL);
    if let Some(core) = slot.as_ref() {
        return Some(core.handle());
    }
    let core = SteamCore::start(win::load_api()?, Config::default()).ok()?;
    *lock(&PROCESS_SINK) = Some(Arc::clone(&core.shared.sink));
    let handle = core.handle();
    *slot = Some(core);
    Some(handle)
}

#[cfg(windows)]
mod win {
    use super::*;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    /// A function pointer `T` (an `extern "C" fn` type) from a NUL-terminated export name.
    ///
    /// # Safety
    /// `module` must be a loaded module and `T` the exact type of the export.
    unsafe fn symbol<T: Copy>(module: *mut c_void, name: &str) -> Option<T> {
        if std::mem::size_of::<T>() != std::mem::size_of::<*mut c_void>() {
            return None;
        }
        // SAFETY: name is NUL-terminated (every caller passes concat!(.., "\0")).
        let p = unsafe { GetProcAddress(module, name.as_ptr()) };
        if p.is_null() {
            return None;
        }
        // SAFETY: T is a function pointer type, the same size as a pointer (checked above).
        Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
    }

    /// The functions that find the interfaces; kept apart because they are the only ones that may return null.
    struct Finders {
        sockets_accessor: unsafe extern "C" fn() -> *mut c_void,
        utils_accessor: unsafe extern "C" fn() -> *mut c_void,
        user_accessor: unsafe extern "C" fn() -> *mut c_void,
        get_steam_id: unsafe extern "C" fn(*mut c_void) -> u64,
        hsteamuser: unsafe extern "C" fn() -> i32,
        find_interface: unsafe extern "C" fn(i32, *const c_char) -> *mut c_void,
    }

    impl Finders {
        /// The accessors return null in the game; what works is asking the client for each interface by its
        /// version name, once Steam is initialised (which the game does late).
        fn locate(&self) -> Located {
            // SAFETY: the signatures are the flat API's (steam_api_flat.h); the version names are NUL-terminated.
            unsafe {
                let huser = (self.hsteamuser)();
                let mut user = (self.user_accessor)();
                if user.is_null() && huser != 0 {
                    user = (self.find_interface)(huser, c"SteamUser023".as_ptr());
                }
                let steam_id =
                    if user.is_null() { None } else { Some((self.get_steam_id)(user)).filter(|id| *id != 0) };
                let mut sockets = (self.sockets_accessor)();
                let mut utils = (self.utils_accessor)();
                if sockets.is_null() && huser != 0 {
                    sockets = (self.find_interface)(huser, c"SteamNetworkingSockets012".as_ptr());
                }
                if utils.is_null() && huser != 0 {
                    utils = (self.find_interface)(huser, c"SteamNetworkingUtils004".as_ptr());
                }
                let seen = |p: *mut c_void| if p.is_null() { "null" } else { "ok" };
                let note =
                    format!("hsteamuser {huser}, user {} sockets {} utils {}", seen(user), seen(sockets), seen(utils));
                Located { sockets, utils, steam_id, note }
            }
        }
    }

    /// `None` when `steam_api64.dll` is not loaded in this process or lacks a function.
    pub(super) fn load_api() -> Option<Api> {
        // SAFETY: GetModuleHandleA/GetProcAddress with NUL-terminated names; the signatures below are the flat API's
        // (steam_api_flat.h of the Steamworks SDK) and every function is called only through the core, which waits
        // for Steam to be initialised.
        unsafe {
            let module = GetModuleHandleA(c"steam_api64.dll".as_ptr().cast());
            if module.is_null() {
                return None;
            }
            macro_rules! sym {
                ($name:literal) => {
                    symbol(module, concat!($name, "\0"))?
                };
            }
            let finders = Finders {
                sockets_accessor: sym!("SteamAPI_SteamNetworkingSockets_SteamAPI_v012"),
                utils_accessor: sym!("SteamAPI_SteamNetworkingUtils_SteamAPI_v004"),
                user_accessor: sym!("SteamAPI_SteamUser_v023"),
                get_steam_id: sym!("SteamAPI_ISteamUser_GetSteamID"),
                hsteamuser: sym!("SteamAPI_GetHSteamUser"),
                find_interface: sym!("SteamInternal_FindOrCreateUserInterface"),
            };
            Some(Api {
                locate: Box::new(move || finders.locate()),
                init_relay: sym!("SteamAPI_ISteamNetworkingUtils_InitRelayNetworkAccess"),
                set_status_callback: sym!(
                    "SteamAPI_ISteamNetworkingUtils_SetGlobalCallback_SteamNetConnectionStatusChanged"
                ),
                set_config: symbol(module, "SteamAPI_ISteamNetworkingUtils_SetConfigValue\0"),
                run_callbacks: sym!("SteamAPI_ISteamNetworkingSockets_RunCallbacks"),
                create_listen: sym!("SteamAPI_ISteamNetworkingSockets_CreateListenSocketP2P"),
                close_listen: symbol(module, "SteamAPI_ISteamNetworkingSockets_CloseListenSocket\0"),
                connect_p2p: sym!("SteamAPI_ISteamNetworkingSockets_ConnectP2P"),
                accept: sym!("SteamAPI_ISteamNetworkingSockets_AcceptConnection"),
                send: sym!("SteamAPI_ISteamNetworkingSockets_SendMessageToConnection"),
                receive: sym!("SteamAPI_ISteamNetworkingSockets_ReceiveMessagesOnConnection"),
                release_message: sym!("SteamAPI_SteamNetworkingMessage_t_Release"),
                close: sym!("SteamAPI_ISteamNetworkingSockets_CloseConnection"),
                connection_info: sym!("SteamAPI_ISteamNetworkingSockets_GetConnectionInfo"),
                real_time_status: symbol(module, "SteamAPI_ISteamNetworkingSockets_GetConnectionRealTimeStatus\0"),
            })
        }
    }
}

#[cfg(not(windows))]
mod win {
    use super::Api;

    /// Steam networking is only driven on Windows.
    pub(super) fn load_api() -> Option<Api> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::fake::{fast, FakeMachine};
    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Ev {
        Opened(Handle, bool, Option<u64>),
        Progress(Handle, i32),
        Message(Handle, Vec<u8>),
        Closed(Handle, End),
    }

    /// An owner that writes down what it is told.
    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<Ev>>,
        changed: Condvar,
    }

    impl Recorder {
        fn new() -> Arc<Recorder> {
            Arc::default()
        }

        fn push(&self, ev: Ev) {
            lock(&self.events).push(ev);
            self.changed.notify_all();
        }

        fn events(&self) -> Vec<Ev> {
            lock(&self.events).clone()
        }

        /// Waits until `done` holds for the events so far; fails the test with the events when it never does.
        fn wait(&self, what: &str, done: impl Fn(&[Ev]) -> bool) -> Vec<Ev> {
            let deadline = Instant::now() + WAIT;
            let mut events = lock(&self.events);
            while !done(&events) {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "timed out waiting for {what}; events: {events:#?}");
                events = self.changed.wait_timeout(events, left).unwrap_or_else(|p| p.into_inner()).0;
            }
            events.clone()
        }

        fn messages(&self) -> Vec<Vec<u8>> {
            self.events()
                .into_iter()
                .filter_map(|e| if let Ev::Message(_, data) = e { Some(data) } else { None })
                .collect()
        }

        fn closes(&self) -> Vec<End> {
            self.events()
                .into_iter()
                .filter_map(|e| if let Ev::Closed(_, end) = e { Some(end) } else { None })
                .collect()
        }

        fn opened(&self) -> Option<Handle> {
            self.events().into_iter().find_map(|e| if let Ev::Opened(h, ..) = e { Some(h) } else { None })
        }

        /// Waits for the first open event and returns its handle.
        fn wait_opened(&self, what: &str) -> Handle {
            self.wait(what, |e| e.iter().any(|e| matches!(e, Ev::Opened(..))));
            self.opened().expect("an open event")
        }
    }

    fn opened_count(events: &[Ev]) -> usize {
        events.iter().filter(|e| matches!(e, Ev::Opened(..))).count()
    }

    fn message_count(events: &[Ev]) -> usize {
        events.iter().filter(|e| matches!(e, Ev::Message(..))).count()
    }

    impl Owner for Recorder {
        fn opened(&self, conn: Handle, incoming: bool, remote: Option<u64>) {
            self.push(Ev::Opened(conn, incoming, remote));
        }
        fn progress(&self, conn: Handle, step: i32) {
            self.push(Ev::Progress(conn, step));
        }
        fn message(&self, conn: Handle, data: &[u8]) {
            self.push(Ev::Message(conn, data.to_vec()));
        }
        fn closed(&self, conn: Handle, end: End) {
            self.push(Ev::Closed(conn, end));
        }
    }

    /// Two machines with a core each; `host` listens on virtual port 0 and `guest` has connected to it and both
    /// ends are open.
    struct Pair {
        host: FakeMachine,
        guest: FakeMachine,
        host_core: SteamCore,
        guest_core: SteamCore,
        host_owner: Arc<Recorder>,
        guest_owner: Arc<Recorder>,
        host_conn: Handle,
        guest_conn: Handle,
    }

    impl Pair {
        fn new() -> Pair {
            let (host, guest) = (FakeMachine::new(), FakeMachine::new());
            let (host_core, guest_core) = (host.core(), guest.core());
            let (host_owner, guest_owner) = (Recorder::new(), Recorder::new());
            host_core.handle().listen(0, host_owner.clone()).expect("listen");
            let guest_conn = guest_core.handle().connect(host.steam_id(), 0, guest_owner.clone()).expect("connect");
            let host_conn = host_owner.wait_opened("the host to see the connection open");
            guest_owner.wait_opened("the guest to see the connection open");
            Pair { host, guest, host_core, guest_core, host_owner, guest_owner, host_conn, guest_conn }
        }
    }

    #[test]
    fn the_core_waits_for_steam_and_then_tunes_the_connections() {
        let machine = FakeMachine::new();
        machine.set_ready(false);
        let core = machine.core_with(fast());
        let handle = core.handle();
        let status = loop {
            let status = handle.status();
            if status.note.starts_with("Steam interfaces") {
                break status;
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(!status.ready && status.steam_id.is_none(), "{status:?}");
        assert!(status.note.contains("sockets null"), "{status:?}");
        // nothing works until Steam is up, and the error says why
        let owner = Recorder::new();
        assert!(matches!(handle.listen(0, owner.clone()), Err(CoreError::NotReady(_))));
        assert!(matches!(handle.connect(7, 0, owner), Err(CoreError::NotReady(_))));
        assert!(handle.wait_ready(Instant::now() + Duration::from_millis(30), &|| false).is_err());
        assert!(handle.wait_ready(Instant::now() + WAIT, &|| true).is_err(), "cancel is honoured");

        machine.set_ready(true);
        handle.wait_ready(Instant::now() + WAIT, &|| false).expect("ready once Steam is up");
        let status = handle.status();
        assert!(status.ready);
        assert_eq!(status.steam_id, Some(machine.steam_id()));
        assert!(status.note.starts_with("tuned"), "{status:?}");
        assert_eq!(machine.relay_inits(), 1);
        let defaults = Config::default();
        let calls = machine.config_calls();
        for (value, number) in [
            (config_value::TIMEOUT_INITIAL, defaults.initial_timeout_ms),
            (config_value::SEND_RATE_MIN, defaults.send_rate),
            (config_value::SEND_RATE_MAX, defaults.send_rate),
        ] {
            assert!(
                calls.contains(&(value, config_value::SCOPE_GLOBAL, config_value::TYPE_INT32, number)),
                "{calls:?}"
            );
        }
    }

    #[test]
    fn a_refused_tuning_is_reported_and_changes_nothing_else() {
        let machine = FakeMachine::new();
        machine.refuse_config();
        let core = machine.core();
        let status = core.handle().status();
        assert!(status.ready);
        assert!(
            status.note.starts_with("tuning refused for: initial timeout, send rate min, send rate max"),
            "{status:?}"
        );
    }

    #[test]
    fn a_connection_is_accepted_through_the_callback_and_opens_on_both_sides() {
        let p = Pair::new();
        assert_eq!(p.host.accepts(), 1);
        assert_eq!(
            p.host_owner.events(),
            vec![
                Ev::Progress(p.host_conn, state::FINDING_ROUTE),
                Ev::Opened(p.host_conn, true, Some(p.guest.steam_id()))
            ]
        );
        let guest_events = p.guest_owner.events();
        assert_eq!(
            guest_events,
            vec![
                Ev::Progress(p.guest_conn, state::CONNECTING),
                Ev::Progress(p.guest_conn, state::FINDING_ROUTE),
                Ev::Opened(p.guest_conn, false, Some(p.host.steam_id())),
            ]
        );
        assert_eq!(p.host_core.handle().tracked(), (1, 1));
        assert_eq!(p.guest_core.handle().tracked(), (0, 1));
        assert!(p.host_core.handle().is_open(p.host_conn) && p.guest_core.handle().is_open(p.guest_conn));
        assert_eq!(p.guest_core.handle().route(p.guest_conn), Some(Route::Direct));
    }

    #[test]
    fn messages_arrive_in_order_both_ways_and_every_one_is_released() {
        let p = Pair::new();
        let (host, guest) = (p.host_core.handle(), p.guest_core.handle());
        for i in 0u32..300 {
            guest.send_reliable(p.guest_conn, format!("to host {i}").as_bytes(), WAIT).expect("send");
            host.send_reliable(p.host_conn, format!("to guest {i}").as_bytes(), WAIT).expect("send");
        }
        p.host_owner.wait("300 messages at the host", |e| message_count(e) == 300);
        p.guest_owner.wait("300 messages at the guest", |e| message_count(e) == 300);
        let at_host: Vec<String> =
            p.host_owner.messages().iter().map(|m| String::from_utf8_lossy(m).into_owned()).collect();
        let at_guest: Vec<String> =
            p.guest_owner.messages().iter().map(|m| String::from_utf8_lossy(m).into_owned()).collect();
        assert_eq!(at_host, (0..300).map(|i| format!("to host {i}")).collect::<Vec<_>>());
        assert_eq!(at_guest, (0..300).map(|i| format!("to guest {i}")).collect::<Vec<_>>());
        assert_eq!(p.host.unreleased_messages() + p.guest.unreleased_messages(), 0);
        assert_eq!(FakeMachine::bad_releases(), 0);
    }

    #[test]
    fn a_full_send_buffer_is_waited_out_and_nothing_is_lost() {
        let p = Pair::new();
        let guest = p.guest_core.handle();
        p.host.stall_receive(true);
        let chunk = vec![7u8; 300 * 1024];
        assert_eq!(guest.send(p.guest_conn, &chunk), SendOutcome::Sent);
        // 600 KiB do not fit into a 512 KiB buffer while the other side does not read
        assert_eq!(guest.send(p.guest_conn, &chunk), SendOutcome::Full);
        let err = guest.send_reliable(p.guest_conn, &chunk, Duration::from_millis(60)).expect_err("times out");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        let sender = {
            let guest = guest.clone();
            let (conn, chunk) = (p.guest_conn, chunk.clone());
            thread::spawn(move || guest.send_reliable(conn, &chunk, WAIT))
        };
        thread::sleep(Duration::from_millis(40));
        p.host.stall_receive(false);
        sender.join().expect("sender thread").expect("the stuck send goes through once the peer reads");
        p.host_owner.wait("both chunks", |e| e.iter().filter(|e| matches!(e, Ev::Message(..))).count() == 2);
        assert!(p.host_owner.messages().iter().all(|m| *m == chunk));
    }

    #[test]
    fn a_message_over_steams_limit_never_reaches_steam() {
        let p = Pair::new();
        let guest = p.guest_core.handle();
        assert_eq!(guest.send(p.guest_conn, &vec![1u8; MAX_MESSAGE + 1]), SendOutcome::TooBig);
        let err = guest.send_reliable(p.guest_conn, &vec![1u8; MAX_MESSAGE + 1], WAIT).expect_err("too big");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // the largest one Steam takes goes through whole
        let biggest = vec![9u8; MAX_MESSAGE];
        guest.send_reliable(p.guest_conn, &biggest, WAIT).expect("send");
        p.host_owner.wait("the biggest message", |e| e.iter().any(|e| matches!(e, Ev::Message(..))));
        assert_eq!(p.host_owner.messages(), vec![biggest]);
    }

    #[test]
    fn what_the_peer_sent_before_closing_is_delivered_and_the_end_is_reported_once() {
        let p = Pair::new();
        p.host.set_wire_delay(Duration::from_millis(30));
        let host = p.host_core.handle();
        for i in 0..3u8 {
            host.send_reliable(p.host_conn, &[i; 100], WAIT).expect("send");
        }
        assert!(host.close(p.host_conn));
        assert!(!host.close(p.host_conn), "a handle closes once");
        let events = p.guest_owner.wait("the end", |e| e.iter().any(|e| matches!(e, Ev::Closed(..))));
        let tail: Vec<&Ev> = events.iter().skip(3).collect();
        assert_eq!(tail.len(), 4, "{events:#?}");
        for (i, ev) in tail.iter().take(3).enumerate() {
            assert_eq!(**ev, Ev::Message(p.guest_conn, vec![i as u8; 100]), "the data comes before the end");
        }
        let Ev::Closed(_, end) = tail[3] else { panic!("last event is the end: {events:#?}") };
        assert_eq!((end.cause, end.opened, end.code), (Cause::Peer, true, 1000));
        assert!(end.describe().contains("closed by the other side") && end.describe().contains("(1000)"), "{end:?}");

        // the closer hears it too, as a local close; our own close lingers, the clean-up of the other end does not
        let closer = p.host_owner.wait("the local close", |e| e.iter().any(|e| matches!(e, Ev::Closed(..))));
        let Some(Ev::Closed(_, end)) = closer.last() else { panic!("{closer:#?}") };
        assert_eq!((end.cause, end.describe().as_str()), (Cause::Local, "closed locally"));
        assert_eq!(p.host.closes(), vec![(p.host_conn, true)]);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(p.guest.closes(), vec![(p.guest_conn, false)]);
        assert_eq!(p.guest_owner.closes().len(), 1);
        assert_eq!(p.host_owner.closes().len(), 1);
        assert_eq!(p.guest_core.handle().tracked().1, 0);
        assert!(p.guest.connections().is_empty() && p.host.connections().is_empty());
    }

    #[test]
    fn a_dead_network_ends_the_connection_with_steams_reason() {
        let p = Pair::new();
        p.host.break_connections();
        for (owner, name) in [(&p.guest_owner, "guest"), (&p.host_owner, "host")] {
            let events =
                owner.wait(&format!("the {name} to notice"), |e| e.iter().any(|e| matches!(e, Ev::Closed(..))));
            let Some(Ev::Closed(_, end)) = events.last() else { panic!("{events:#?}") };
            assert_eq!((end.cause, end.code), (Cause::Problem, 5003));
            assert!(
                end.describe().contains("timed out (5003)") && end.describe().contains("Steam says: Timed out"),
                "{end:?}"
            );
        }
    }

    #[test]
    fn a_connection_to_nobody_ends_without_ever_opening() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let (core_a, core_b) = (a.core(), b.core());
        let handle = core_a.handle();

        // an account that is not listening on that virtual port
        let owner = Recorder::new();
        core_b.handle().listen(5, Recorder::new()).expect("listen");
        handle.connect(b.steam_id(), 6, owner.clone()).expect("connect starts");
        let events = owner.wait("the refusal", |e| e.iter().any(|e| matches!(e, Ev::Closed(..))));
        let Some(Ev::Closed(_, end)) = events.last() else { panic!("{events:#?}") };
        assert_eq!((end.opened, end.cause, end.code), (false, Cause::Peer, 5001));
        assert!(!events.iter().any(|e| matches!(e, Ev::Opened(..))));

        // an account Steam cannot find
        let owner = Recorder::new();
        handle.connect(76561198123456789, 0, owner.clone()).expect("connect starts");
        let events = owner.wait("the timeout", |e| e.iter().any(|e| matches!(e, Ev::Closed(..))));
        let Some(Ev::Closed(_, end)) = events.last() else { panic!("{events:#?}") };
        assert_eq!((end.opened, end.cause, end.code), (false, Cause::Problem, 5003));
        assert_eq!(handle.tracked().1, 0, "both are forgotten");
    }

    #[test]
    fn an_unreachable_account_leaves_the_connection_looking_for_a_route_until_it_is_closed() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        b.set_reachable(false);
        let (core_a, _core_b) = (a.core(), b.core());
        let handle = core_a.handle();
        let owner = Recorder::new();
        let conn = handle.connect(b.steam_id(), 0, owner.clone()).expect("connect starts");
        owner.wait("the first progress", |e| !e.is_empty());
        thread::sleep(Duration::from_millis(50));
        assert_eq!(owner.events(), vec![Ev::Progress(conn, state::CONNECTING)], "nothing else happens");
        assert!(!handle.is_open(conn));
        assert_eq!(handle.send(conn, b"x"), SendOutcome::Closed, "no sending before the connection is open");
        assert!(handle.close(conn));
        let events = owner.wait("the local close", |e| e.iter().any(|e| matches!(e, Ev::Closed(..))));
        let Some(Ev::Closed(_, end)) = events.last() else { panic!("{events:#?}") };
        assert_eq!((end.opened, end.cause), (false, Cause::Local));
    }

    #[test]
    fn events_about_connections_of_others_are_ignored() {
        let machine = FakeMachine::new();
        let core = machine.core();
        let handle = core.handle();
        let owner = Recorder::new();
        let listener = handle.listen(0, owner.clone()).expect("listen");
        // a status change for a connection nobody here made, and a peer on a listen socket that is not ours
        machine.foreign_event(4242, state::CONNECTED, 0);
        machine.foreign_event(4243, state::CLOSED_BY_PEER, 0);
        machine.foreign_event(4244, state::CONNECTING, listener + 77);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(handle.tracked(), (1, 0));
        assert!(owner.events().is_empty());
        assert_eq!(machine.closes(), vec![], "the core does not touch what it does not own");
        // an arrival on our own listen socket whose handle Steam no longer knows is refused and forgotten
        machine.foreign_event(4245, state::CONNECTING, listener);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(handle.tracked(), (1, 0));
        assert!(owner.events().is_empty());
    }

    #[test]
    fn stopping_the_core_closes_everything_and_tells_the_owners() {
        let p = Pair::new();
        let Pair { host, guest, host_core, guest_core, host_owner, guest_owner, host_conn, .. } = p;
        drop(host_core);
        let events = host_owner.events();
        let Some(Ev::Closed(conn, end)) = events.last() else { panic!("{events:#?}") };
        assert_eq!((*conn, end.cause, end.opened), (host_conn, Cause::Shutdown, true));
        assert_eq!(end.describe(), "Steam networking stopped");
        let events = guest_owner.wait("the guest to notice", |e| e.iter().any(|e| matches!(e, Ev::Closed(..))));
        let Some(Ev::Closed(_, end)) = events.last() else { panic!("{events:#?}") };
        assert_eq!(end.cause, Cause::Peer);
        assert_eq!(host.listeners(), 0, "the listen socket is closed too");
        drop(guest_core);
        assert!(guest.connections().is_empty() && host.connections().is_empty());
    }

    #[test]
    fn a_stopped_core_refuses_work_instead_of_hanging() {
        let machine = FakeMachine::new();
        let core = machine.core();
        let handle = core.handle();
        drop(core);
        let owner = Recorder::new();
        assert_eq!(handle.listen(0, owner.clone()), Err(CoreError::Stopped));
        assert_eq!(handle.connect(7, 0, owner), Err(CoreError::Stopped));
        assert_eq!(handle.send(1, b"x"), SendOutcome::Closed);
        assert!(!handle.close(1));
        assert_eq!(handle.wait_ready(Instant::now() + WAIT, &|| false), Ok(()), "it was ready before it stopped");
    }

    #[test]
    fn a_virtual_port_has_one_listener_and_each_port_its_own_owner() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let (core_a, core_b) = (a.core(), b.core());
        let (zero, one) = (Recorder::new(), Recorder::new());
        let listener = core_b.handle().listen(0, zero.clone()).expect("port 0");
        core_b.handle().listen(1, one.clone()).expect("port 1");
        assert!(matches!(core_b.handle().listen(0, Recorder::new()), Err(CoreError::Refused(_))));
        for port in [0, 1, 1] {
            core_a.handle().connect(b.steam_id(), port, Recorder::new()).expect("connect");
        }
        zero.wait("one arrival on port 0", |e| opened_count(e) == 1);
        one.wait("two arrivals on port 1", |e| opened_count(e) == 2);
        assert_eq!(core_b.handle().tracked(), (2, 3));
        // a closed listen socket takes no more arrivals, and the port can be used again
        core_b.handle().close_listener(listener);
        assert_eq!(b.listeners(), 1);
        core_b.handle().listen(0, Recorder::new()).expect("port 0 is free again");
    }

    #[test]
    fn a_core_can_connect_to_its_own_account() {
        let machine = FakeMachine::new();
        let core = machine.core();
        let handle = core.handle();
        let (server, client) = (Recorder::new(), Recorder::new());
        handle.listen(0, server.clone()).expect("listen");
        let conn = handle.connect(machine.steam_id(), 0, client.clone()).expect("connect");
        let server_conn = server.wait_opened("the arrival");
        assert!(server.events().contains(&Ev::Opened(server_conn, true, Some(machine.steam_id()))));
        client.wait_opened("the open");
        handle.send_reliable(conn, b"ping 1", WAIT).expect("send");
        server.wait("the ping", |e| e.iter().any(|e| matches!(e, Ev::Message(..))));
        handle.send_reliable(server_conn, b"pong 1", WAIT).expect("send");
        client.wait("the pong", |e| e.iter().any(|e| matches!(e, Ev::Message(..))));
        assert_eq!((server.messages(), client.messages()), (vec![b"ping 1".to_vec()], vec![b"pong 1".to_vec()]));
    }

    /// Answers every message by sending it back, from inside the callback.
    struct Echo {
        core: CoreRef,
    }

    impl Owner for Echo {
        fn message(&self, conn: Handle, data: &[u8]) {
            let _ = self.core.send_reliable(conn, data, WAIT);
        }
        fn closed(&self, _conn: Handle, _end: End) {}
    }

    #[test]
    fn an_owner_may_call_the_core_from_inside_a_callback() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let (core_a, core_b) = (a.core(), b.core());
        core_b.handle().listen(0, Arc::new(Echo { core: core_b.handle() })).expect("listen");
        let client = Recorder::new();
        let conn = core_a.handle().connect(b.steam_id(), 0, client.clone()).expect("connect");
        client.wait("the open", |e| e.iter().any(|e| matches!(e, Ev::Opened(..))));
        for i in 0..50u8 {
            core_a.handle().send_reliable(conn, &[i; 10], WAIT).expect("send");
        }
        client.wait("fifty echoes", |e| e.iter().filter(|e| matches!(e, Ev::Message(..))).count() == 50);
        assert_eq!(client.messages(), (0..50u8).map(|i| vec![i; 10]).collect::<Vec<_>>());
    }

    #[test]
    fn a_message_above_the_copy_limit_closes_the_connection_as_a_violation() {
        let p = Pair::new();
        let host = p.host_core.handle();
        host.send_reliable(p.host_conn, b"fine", WAIT).expect("send");
        p.guest_owner.wait("the first message", |e| e.iter().any(|e| matches!(e, Ev::Message(..))));
        p.guest.inject(p.guest_conn, b"before");
        p.guest.inject(p.guest_conn, &vec![0u8; COPY_LIMIT + 1]);
        p.guest.inject(p.guest_conn, b"after");
        let events = p.guest_owner.wait("the violation", |e| e.iter().any(|e| matches!(e, Ev::Closed(..))));
        let Some(Ev::Closed(_, end)) = events.last() else { panic!("{events:#?}") };
        assert_eq!((end.cause, end.opened), (Cause::Violation, true));
        assert!(end.describe().contains("exceeds"), "{end:?}");
        assert_eq!(p.guest_owner.messages(), vec![b"fine".to_vec(), b"before".to_vec()], "nothing after the violation");
        // the other side sees an ordinary close, and nothing is left unreleased
        p.host_owner.wait("the host to notice", |e| e.iter().any(|e| matches!(e, Ev::Closed(..))));
        assert_eq!(p.guest.unreleased_messages(), 0);
    }

    #[test]
    fn the_route_of_a_relayed_connection_is_reported() {
        let p = Pair::new();
        assert_eq!(p.host_core.handle().route(p.host_conn), Some(Route::Direct));
        p.host.set_relay(0x646162);
        assert_eq!(p.host_core.handle().route(p.host_conn), Some(Route::Relayed));
        assert_eq!(p.host_core.handle().route(999_999), None);
        assert_eq!(Route::Relayed.text(), "through a Steam relay");
    }

    /// Polls the core's cached measurement of `conn` until `done` accepts it.
    fn wait_link(core: &CoreRef, conn: Handle, what: &str, done: impl Fn(&LinkSnapshot) -> bool) -> LinkSnapshot {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(link) = core.link(conn).filter(|l| done(l)) {
                return link;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {:?}", core.link(conn));
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_ping_and_the_route_of_an_open_connection_are_measured_and_kept_current() {
        let p = Pair::new();
        let guest = p.guest_core.handle();
        // the first sample follows the open; the fake reports 30 ms and a direct path
        let first = wait_link(&guest, p.guest_conn, "the first sample", |_| true);
        assert_eq!((first.route, first.ping_ms), (Route::Direct, Some(30)));
        assert!(first.quality > 0.9 && first.quality <= 1.0, "{first:?}");
        assert_eq!(first.pending, 0);
        // the tuned send rate is what Steam reports: this is how the real game shows the tuning took effect
        assert_eq!(first.send_rate, Config::default().send_rate as u32);
        assert_eq!(guest.link(999_999), None, "nothing is known about a connection nobody tracks");

        // the path changes under the connection: a relay at first, then Steam finds a direct route
        p.guest.set_ping(180);
        p.guest.set_relay(0x646162);
        let relayed = wait_link(&guest, p.guest_conn, "the relay to show", |l| l.route == Route::Relayed);
        assert_eq!(relayed.ping_ms, Some(180));
        p.guest.set_relay(0);
        p.guest.set_ping(35);
        let upgraded = wait_link(&guest, p.guest_conn, "the upgrade to a direct path", |l| l.route == Route::Direct);
        wait_link(&guest, p.guest_conn, "the new ping", |l| l.ping_ms == Some(35));
        assert_eq!(upgraded.route, Route::Direct);

        // Steam has no estimate yet: no ping rather than a made-up one
        p.guest.set_ping(-1);
        wait_link(&guest, p.guest_conn, "no estimate", |l| l.ping_ms.is_none());
        // a closed connection has no measurement any more
        assert!(guest.close(p.guest_conn));
        assert_eq!(guest.link(p.guest_conn), None);
    }

    #[test]
    fn without_tuning_the_report_shows_steams_default_send_rate() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        a.refuse_config();
        let (core_a, core_b) = (a.core(), b.core());
        let (server, client) = (Recorder::new(), Recorder::new());
        core_b.handle().listen(0, server).expect("listen");
        let conn = core_a.handle().connect(b.steam_id(), 0, client.clone()).expect("connect");
        client.wait_opened("the open");
        let link = wait_link(&core_a.handle(), conn, "a sample", |_| true);
        assert_eq!(link.send_rate, 262_144, "Steam's own default");
        let link = wait_link(&core_b.handle(), b.connections()[0], "a sample", |_| true);
        assert_eq!(link.send_rate, Config::default().send_rate as u32);
    }

    #[test]
    fn the_production_timings_work_too() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let core_a = a.core_with(Config::default());
        let core_b = b.core_with(Config::default());
        for core in [&core_a, &core_b] {
            core.handle().wait_ready(Instant::now() + WAIT, &|| false).expect("ready");
        }
        let (server, client) = (Recorder::new(), Recorder::new());
        core_b.handle().listen(0, server.clone()).expect("listen");
        let conn = core_a.handle().connect(b.steam_id(), 0, client.clone()).expect("connect");
        client.wait("the open", |e| e.iter().any(|e| matches!(e, Ev::Opened(..))));
        core_a.handle().send_reliable(conn, b"hello", WAIT).expect("send");
        server.wait("the message", |e| e.iter().any(|e| matches!(e, Ev::Message(..))));
        assert!(server.opened().is_some());
        assert_eq!(server.messages(), vec![b"hello".to_vec()]);
    }

    #[test]
    fn end_texts_cover_every_cause() {
        let end = |cause, code, detail: &str| End { opened: true, cause, code, detail: detail.into() };
        assert_eq!(end(Cause::Local, 0, "").describe(), "closed locally");
        assert_eq!(end(Cause::Shutdown, 0, "").describe(), "Steam networking stopped");
        assert_eq!(end(Cause::Violation, 0, "bad message").describe(), "bad message");
        assert_eq!(
            end(Cause::Peer, 1000, "").describe(),
            "closed by the other side: closed by the other side's program (1000)"
        );
        assert_eq!(
            end(Cause::Problem, 5003, "Timed out").describe(),
            "connection problem: timed out (5003) (Steam says: Timed out)"
        );
        let io_error: io::Error = CoreError::NotReady("x".into()).into();
        assert_eq!(io_error.kind(), io::ErrorKind::NotConnected);
        assert_eq!(io::Error::from(CoreError::Stopped).kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(io::Error::from(CoreError::Refused("no".into())).to_string(), "no");
    }
}
