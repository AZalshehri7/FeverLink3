//! An in-memory Steam for the tests: the functions of [`Api`] (`extern "C"`, like the real flat API) backed by one
//! global model of machines, listen sockets, connections and message queues. The core, the probe, the transports
//! and the session run against it unchanged, on their real threads.
//!
//! The model is Steam as far as the code relies on it, written from `steamnetworkingtypes.h` and
//! `isteamnetworkingsockets.h`:
//!
//! - A connection is a pair of ends. `ConnectP2P` creates the local end (Connecting) and, when the target account
//!   listens on that virtual port, the remote end (Connecting, with the listen socket). `AcceptConnection` moves
//!   both to FindingRoute and, after the route delay, to Connected. Every state change is a callback that
//!   `RunCallbacks` delivers to the global callback, in the layout of `SteamNetConnectionStatusChangedCallback_t`
//!   (written here with literal offsets, so a wrong constant in the real parser cannot hide).
//! - Messages are reliable and in order. Each end can have 512 KiB queued at the peer; more is `LimitExceeded` until
//!   the peer reads. A message is visible to the peer after the wire delay. Sending is only possible when Connected.
//! - `CloseConnection` invalidates the handle at once. With linger, what was sent still arrives; without it, what is
//!   still on the wire is discarded. The peer goes to ClosedByPeer and can still read what had arrived.
//! - A machine is one Steam client: its own account, its own interfaces, its own callback and event queue. One
//!   machine can connect to itself, as the game's own account can in the real thing.
//!
//! Knobs on [`FakeMachine`] make the failures the tests need: Steam not initialised yet, an unreachable account, a
//! slow route, a slow wire, a broken network, a receiver that stops reading, garbage injected into a connection.

use std::collections::{HashMap, VecDeque};
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::steam_core::{Api, Config, Located, SteamCore};

type Callback = unsafe extern "C" fn(*mut c_void);

/// `sizeof(SteamNetConnectionStatusChangedCallback_t)`: handle, 696-byte info (8-aligned), old state.
const CALLBACK_BYTES: usize = 712;
/// `sizeof(SteamNetworkingMessage_t)`.
const MESSAGE_BYTES: usize = 216;
/// Per-connection send buffer (`k_ESteamNetworkingConfig_SendBufferSize` default).
const SEND_BUFFER: usize = 512 * 1024;
/// `k_cbMaxSteamNetworkingSocketsMessageSizeSend`.
const MAX_SEND: usize = 512 * 1024;

const CONNECTING: i32 = 1;
const FINDING_ROUTE: i32 = 2;
const CONNECTED: i32 = 3;
const CLOSED_BY_PEER: i32 = 4;
const PROBLEM: i32 = 5;

/// First fake Steam ID: a real-looking individual account (universe 1, type 1, instance 1).
const FIRST_STEAM_ID: u64 = 76561198000000000;
static NEXT_STEAM_ID: AtomicU64 = AtomicU64::new(FIRST_STEAM_ID);

struct Machine {
    steam_id: u64,
    /// Steam is initialised: the interfaces exist.
    ready: bool,
    /// Connections to this account get an answer.
    reachable: bool,
    callback: Option<Callback>,
    events: VecDeque<Vec<u64>>,
    route_delay: Duration,
    wire_delay: Duration,
    relay_pop: u32,
    stall_receive: bool,
    config_ok: bool,
    config: Vec<(i32, i32, i32, i32)>,
    /// (handle, linger) of every CloseConnection that found its connection.
    closes: Vec<(u32, bool)>,
    accepts: usize,
    relay_inits: usize,
    /// The biggest message this machine handed to Steam.
    largest_sent: usize,
    /// Sends Steam answered with "limit exceeded" (the peer had not read enough yet).
    full_sends: usize,
    /// Calls of `RunCallbacks`: how often the core polls.
    callback_runs: usize,
    /// What `GetConnectionRealTimeStatus` reports as the ping.
    ping_ms: i32,
    /// The send rate in force: Steam's default until `SetConfigValue` changes it.
    send_rate: i32,
}

struct Listen {
    machine: usize,
    virtual_port: i32,
}

struct Wire {
    visible_at: Instant,
    data: Vec<u8>,
}

struct Conn {
    machine: usize,
    peer: Option<u32>,
    /// The listen socket it arrived on, 0 for an outgoing connection.
    listen: u32,
    /// The other account.
    remote: u64,
    state: i32,
    end_reason: i32,
    end_debug: String,
    inbox: VecDeque<Wire>,
    /// State changes that happen later: (when, state, reason, debug).
    scheduled: Vec<(Instant, i32, i32, String)>,
}

/// A message handed out and not yet released; the boxes keep the memory the core reads alive.
struct Message {
    machine: usize,
    _header: Box<[u64; MESSAGE_BYTES / 8]>,
    _data: Box<[u8]>,
}

#[derive(Default)]
struct Model {
    next_machine: usize,
    next_handle: u32,
    machines: HashMap<usize, Machine>,
    listens: HashMap<u32, Listen>,
    conns: HashMap<u32, Conn>,
    messages: HashMap<usize, Message>,
    bad_releases: usize,
}

static MODEL: LazyLock<Mutex<Model>> = LazyLock::new(|| Mutex::new(Model::default()));

fn model() -> MutexGuard<'static, Model> {
    MODEL.lock().unwrap_or_else(|p| p.into_inner())
}

fn terminal(state: i32) -> bool {
    state == CLOSED_BY_PEER || state == PROBLEM
}

/// The machine an interface pointer belongs to (`machine << 4 | kind`; the real ones are opaque).
fn machine_of(interface: *mut c_void) -> usize {
    (interface as usize) >> 4
}

fn interface(machine: usize, kind: usize) -> *mut c_void {
    ((machine << 4) | kind) as *mut c_void
}

/// A `SteamNetConnectionStatusChangedCallback_t`, as 8-byte words so the pointer handed to the callback is aligned.
#[allow(clippy::too_many_arguments)]
fn status_bytes(
    conn: u32,
    state: i32,
    old: i32,
    listen: u32,
    remote: u64,
    relay_pop: u32,
    reason: i32,
    debug: &str,
) -> Vec<u64> {
    let mut b = vec![0u8; CALLBACK_BYTES];
    b[0..4].copy_from_slice(&conn.to_le_bytes());
    let info = 8;
    // m_identityRemote: type 16 (a Steam ID), size 8, the ID at offset 8 of the identity
    b[info..info + 4].copy_from_slice(&16i32.to_le_bytes());
    b[info + 4..info + 8].copy_from_slice(&8i32.to_le_bytes());
    b[info + 8..info + 16].copy_from_slice(&remote.to_le_bytes());
    b[info + 144..info + 148].copy_from_slice(&listen.to_le_bytes());
    b[info + 172..info + 176].copy_from_slice(&relay_pop.to_le_bytes());
    b[info + 176..info + 180].copy_from_slice(&state.to_le_bytes());
    b[info + 180..info + 184].copy_from_slice(&reason.to_le_bytes());
    let text = debug.as_bytes();
    let n = text.len().min(127);
    b[info + 184..info + 184 + n].copy_from_slice(&text[..n]);
    let flags: u32 = if relay_pop != 0 { 16 } else { 0 };
    b[info + 440..info + 444].copy_from_slice(&flags.to_le_bytes());
    // m_eOldState follows the 696 bytes of info
    b[info + 696..info + 700].copy_from_slice(&old.to_le_bytes());
    b.chunks(8).map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes"))).collect()
}

impl Model {
    fn new_handle(&mut self) -> u32 {
        self.next_handle += 1;
        self.next_handle + 1000
    }

    /// Queues the callback for `conn`'s current state on its machine.
    fn queue(&mut self, conn: u32, old: i32) {
        let Some(c) = self.conns.get(&conn) else { return };
        let Some(m) = self.machines.get_mut(&c.machine) else { return };
        let bytes = status_bytes(conn, c.state, old, c.listen, c.remote, m.relay_pop, c.end_reason, &c.end_debug);
        m.events.push_back(bytes);
    }

    fn set_state(&mut self, conn: u32, state: i32, reason: i32, debug: &str) {
        let Some(c) = self.conns.get_mut(&conn) else { return };
        let old = c.state;
        if old == state || terminal(old) {
            return;
        }
        c.state = state;
        c.end_reason = reason;
        c.end_debug = debug.to_string();
        self.queue(conn, old);
    }

    fn schedule(&mut self, conn: u32, at: Instant, state: i32, reason: i32, debug: &str) {
        if at <= Instant::now() {
            self.set_state(conn, state, reason, debug);
        } else if let Some(c) = self.conns.get_mut(&conn) {
            c.scheduled.push((at, state, reason, debug.to_string()));
        }
    }

    /// Applies the state changes whose time has come.
    fn tick(&mut self) {
        let now = Instant::now();
        let mut due: Vec<(Instant, u32, i32, i32, String)> = Vec::new();
        for (&handle, c) in &mut self.conns {
            let (ready, later): (Vec<_>, Vec<_>) =
                std::mem::take(&mut c.scheduled).into_iter().partition(|s| s.0 <= now);
            c.scheduled = later;
            due.extend(ready.into_iter().map(|(at, state, reason, debug)| (at, handle, state, reason, debug)));
        }
        due.sort_by_key(|d| d.0);
        for (_, handle, state, reason, debug) in due {
            self.set_state(handle, state, reason, &debug);
        }
    }

    fn inbox_bytes(&self, conn: u32) -> usize {
        self.conns.get(&conn).map_or(0, |c| c.inbox.iter().map(|w| w.data.len()).sum())
    }

    fn wire_delay(&self, machine: usize) -> Duration {
        self.machines.get(&machine).map_or(Duration::ZERO, |m| m.wire_delay)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The functions of the Api
// ---------------------------------------------------------------------------------------------------------------

unsafe extern "C" fn init_relay(utils: *mut c_void) {
    if let Some(m) = model().machines.get_mut(&machine_of(utils)) {
        m.relay_inits += 1;
    }
}

unsafe extern "C" fn set_status_callback(utils: *mut c_void, callback: Callback) -> bool {
    match model().machines.get_mut(&machine_of(utils)) {
        Some(m) => {
            m.callback = Some(callback);
            true
        }
        None => false,
    }
}

unsafe extern "C" fn set_config(
    utils: *mut c_void,
    value: i32,
    scope: i32,
    _scope_object: isize,
    data_type: i32,
    arg: *const c_void,
) -> bool {
    let mut m = model();
    let Some(machine) = m.machines.get_mut(&machine_of(utils)) else { return false };
    if !machine.config_ok || data_type != 1 || arg.is_null() {
        return false;
    }
    // SAFETY: the caller passes a pointer to one int32 for data type 1 (Int32), as SetConfigValue requires.
    let number = unsafe { *(arg as *const i32) };
    machine.config.push((value, scope, data_type, number));
    // k_ESteamNetworkingConfig_SendRateMin / SendRateMax set the rate connections send at
    if scope == 1 && (value == 10 || value == 11) {
        machine.send_rate = number;
    }
    true
}

unsafe extern "C" fn run_callbacks(sockets: *mut c_void) {
    let (callback, events) = {
        let mut m = model();
        m.tick();
        let Some(machine) = m.machines.get_mut(&machine_of(sockets)) else { return };
        machine.callback_runs += 1;
        match machine.callback {
            Some(cb) => (Some(cb), machine.events.drain(..).collect::<Vec<_>>()),
            None => (None, Vec::new()),
        }
    };
    if let Some(callback) = callback {
        for mut event in events {
            // SAFETY: the callback reads CALLBACK_BYTES from an 8-aligned buffer, which `event` is.
            unsafe { callback(event.as_mut_ptr().cast()) };
        }
    }
}

unsafe extern "C" fn create_listen(sockets: *mut c_void, virtual_port: i32, _n: i32, _options: *const c_void) -> u32 {
    let mut m = model();
    let machine = machine_of(sockets);
    if m.listens.values().any(|l| l.machine == machine && l.virtual_port == virtual_port) {
        return 0;
    }
    let handle = m.new_handle();
    m.listens.insert(handle, Listen { machine, virtual_port });
    handle
}

unsafe extern "C" fn close_listen(sockets: *mut c_void, listener: u32) -> bool {
    let mut m = model();
    let machine = machine_of(sockets);
    if m.listens.get(&listener).is_some_and(|l| l.machine == machine) {
        m.listens.remove(&listener);
        return true;
    }
    false
}

unsafe extern "C" fn connect_p2p(
    sockets: *mut c_void,
    identity: *const c_void,
    virtual_port: i32,
    _n: i32,
    _options: *const c_void,
) -> u32 {
    // SAFETY: ConnectP2P is given a 136-byte SteamNetworkingIdentity.
    let identity = unsafe { std::slice::from_raw_parts(identity as *const u8, 136) };
    let kind = i32::from_le_bytes(identity[0..4].try_into().expect("4 bytes"));
    let size = i32::from_le_bytes(identity[4..8].try_into().expect("4 bytes"));
    if kind != 16 || size != 8 {
        return 0;
    }
    let target_id = u64::from_le_bytes(identity[8..16].try_into().expect("8 bytes"));

    let mut m = model();
    m.tick();
    let local = machine_of(sockets);
    let Some(local_id) = m.machines.get(&local).map(|mm| mm.steam_id) else { return 0 };
    let outgoing = m.new_handle();
    m.conns.insert(
        outgoing,
        Conn {
            machine: local,
            peer: None,
            listen: 0,
            remote: target_id,
            state: CONNECTING,
            end_reason: 0,
            end_debug: String::new(),
            inbox: VecDeque::new(),
            scheduled: Vec::new(),
        },
    );
    m.queue(outgoing, 0);

    let target = m.machines.iter().find(|(_, mm)| mm.steam_id == target_id).map(|(&id, mm)| (id, mm.reachable));
    match target {
        None => m.set_state(outgoing, PROBLEM, 5003, "Timed out attempting to connect"),
        // nothing answers: the connection stays where it is until somebody gives up
        Some((_, false)) => {}
        Some((target_machine, true)) => {
            let listener = m
                .listens
                .iter()
                .find(|(_, l)| l.machine == target_machine && l.virtual_port == virtual_port)
                .map(|(&h, _)| h);
            match listener {
                None => m.set_state(outgoing, CLOSED_BY_PEER, 5001, "no listen socket on that virtual port"),
                Some(listen) => {
                    let incoming = m.new_handle();
                    m.conns.insert(
                        incoming,
                        Conn {
                            machine: target_machine,
                            peer: Some(outgoing),
                            listen,
                            remote: local_id,
                            state: CONNECTING,
                            end_reason: 0,
                            end_debug: String::new(),
                            inbox: VecDeque::new(),
                            scheduled: Vec::new(),
                        },
                    );
                    if let Some(c) = m.conns.get_mut(&outgoing) {
                        c.peer = Some(incoming);
                    }
                    m.queue(incoming, 0);
                }
            }
        }
    }
    outgoing
}

unsafe extern "C" fn accept(sockets: *mut c_void, conn: u32) -> i32 {
    let mut m = model();
    m.tick();
    let machine = machine_of(sockets);
    let Some(c) = m.conns.get(&conn).filter(|c| c.machine == machine) else { return 8 };
    if c.state != CONNECTING || c.listen == 0 {
        return 11;
    }
    let peer = c.peer;
    if let Some(mm) = m.machines.get_mut(&machine) {
        mm.accepts += 1;
    }
    m.set_state(conn, FINDING_ROUTE, 0, "");
    let mut delay = m.machines.get(&machine).map_or(Duration::ZERO, |mm| mm.route_delay);
    if let Some(p) = peer {
        m.set_state(p, FINDING_ROUTE, 0, "");
        let peer_machine = m.conns.get(&p).map(|c| c.machine);
        delay = delay.max(peer_machine.and_then(|id| m.machines.get(&id)).map_or(Duration::ZERO, |mm| mm.route_delay));
        let at = Instant::now() + delay;
        m.schedule(p, at, CONNECTED, 0, "");
    }
    let at = Instant::now() + delay;
    m.schedule(conn, at, CONNECTED, 0, "");
    1
}

unsafe extern "C" fn send(
    sockets: *mut c_void,
    conn: u32,
    data: *const c_void,
    len: u32,
    _flags: i32,
    out_number: *mut i64,
) -> i32 {
    let len = len as usize;
    let mut m = model();
    m.tick();
    let machine = machine_of(sockets);
    let Some(c) = m.conns.get(&conn).filter(|c| c.machine == machine) else { return 8 };
    match c.state {
        CONNECTED => {}
        CLOSED_BY_PEER | PROBLEM => return 3,
        _ => return 11,
    }
    if len > MAX_SEND {
        return 8;
    }
    let Some(peer) = c.peer.filter(|p| m.conns.contains_key(p)) else { return 3 };
    if m.inbox_bytes(peer) + len > SEND_BUFFER {
        if let Some(mm) = m.machines.get_mut(&machine) {
            mm.full_sends += 1;
        }
        return 25;
    }
    // SAFETY: the caller passes len readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, len) }.to_vec();
    let visible_at = Instant::now() + m.wire_delay(machine);
    if let Some(p) = m.conns.get_mut(&peer) {
        p.inbox.push_back(Wire { visible_at, data: bytes });
    }
    if let Some(mm) = m.machines.get_mut(&machine) {
        mm.largest_sent = mm.largest_sent.max(len);
    }
    if !out_number.is_null() {
        // SAFETY: a non-null out pointer is one int64.
        unsafe { *out_number = 1 };
    }
    1
}

unsafe extern "C" fn receive(sockets: *mut c_void, conn: u32, out: *mut *mut c_void, max: i32) -> i32 {
    let mut m = model();
    m.tick();
    let machine = machine_of(sockets);
    if m.machines.get(&machine).is_some_and(|mm| mm.stall_receive) && m.conns.contains_key(&conn) {
        return 0;
    }
    let now = Instant::now();
    let mut taken = Vec::new();
    {
        let Some(c) = m.conns.get_mut(&conn).filter(|c| c.machine == machine) else { return -1 };
        while taken.len() < max.max(0) as usize && c.inbox.front().is_some_and(|w| w.visible_at <= now) {
            if let Some(w) = c.inbox.pop_front() {
                taken.push(w.data);
            }
        }
    }
    let count = taken.len();
    for (i, data) in taken.into_iter().enumerate() {
        let data = data.into_boxed_slice();
        let mut header = Box::new([0u64; MESSAGE_BYTES / 8]);
        let base = header.as_mut_ptr() as *mut u8;
        // SAFETY: header is MESSAGE_BYTES long. Layout (steamnetworkingtypes.h): m_pData at 0, m_cbSize at 8,
        // m_conn at 12, m_nFlags at 196.
        unsafe {
            std::ptr::write_unaligned(base as *mut *const u8, data.as_ptr());
            std::ptr::write_unaligned(base.add(8) as *mut i32, data.len() as i32);
            std::ptr::write_unaligned(base.add(12) as *mut u32, conn);
            std::ptr::write_unaligned(base.add(196) as *mut i32, 8);
        }
        let pointer = header.as_mut_ptr() as *mut c_void;
        m.messages.insert(pointer as usize, Message { machine, _header: header, _data: data });
        // SAFETY: the caller's array has room for `max` pointers and i < count <= max.
        unsafe { *out.add(i) = pointer };
    }
    count as i32
}

unsafe extern "C" fn release_message(message: *mut c_void) {
    let mut m = model();
    if m.messages.remove(&(message as usize)).is_none() {
        m.bad_releases += 1;
    }
}

unsafe extern "C" fn close(sockets: *mut c_void, conn: u32, reason: i32, _debug: *const c_char, linger: bool) -> bool {
    let mut m = model();
    m.tick();
    let machine = machine_of(sockets);
    if !m.conns.get(&conn).is_some_and(|c| c.machine == machine) {
        return false;
    }
    let Some(c) = m.conns.remove(&conn) else { return false };
    if let Some(mm) = m.machines.get_mut(&machine) {
        mm.closes.push((conn, linger));
    }
    let Some(peer) = c.peer.filter(|p| m.conns.get(p).is_some_and(|pc| !terminal(pc.state))) else { return true };
    let now = Instant::now();
    let mut at = now + m.wire_delay(machine);
    if linger {
        // the close travels behind the data that is still on the wire
        if let Some(last) = m.conns.get(&peer).and_then(|p| p.inbox.back()).map(|w| w.visible_at) {
            at = at.max(last);
        }
    } else if let Some(p) = m.conns.get_mut(&peer) {
        p.inbox.retain(|w| w.visible_at <= now);
    }
    let code = if reason == 0 { 1000 } else { reason };
    m.schedule(peer, at, CLOSED_BY_PEER, code, "The remote host closed the connection");
    true
}

unsafe extern "C" fn connection_info(sockets: *mut c_void, conn: u32, out: *mut c_void) -> bool {
    let m = model();
    let Some(c) = m.conns.get(&conn).filter(|c| c.machine == machine_of(sockets)) else { return false };
    let relay_pop = m.machines.get(&c.machine).map_or(0, |mm| mm.relay_pop);
    let mut b = [0u8; 696];
    b[0..4].copy_from_slice(&16i32.to_le_bytes());
    b[4..8].copy_from_slice(&8i32.to_le_bytes());
    b[8..16].copy_from_slice(&c.remote.to_le_bytes());
    b[144..148].copy_from_slice(&c.listen.to_le_bytes());
    b[172..176].copy_from_slice(&relay_pop.to_le_bytes());
    b[176..180].copy_from_slice(&c.state.to_le_bytes());
    b[180..184].copy_from_slice(&c.end_reason.to_le_bytes());
    let text = c.end_debug.as_bytes();
    let n = text.len().min(127);
    b[184..184 + n].copy_from_slice(&text[..n]);
    let flags: u32 = if relay_pop != 0 { 16 } else { 0 };
    b[440..444].copy_from_slice(&flags.to_le_bytes());
    // SAFETY: the caller passes a buffer of sizeof(SteamNetConnectionInfo_t) = 696 bytes.
    unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), out as *mut u8, b.len()) };
    true
}

unsafe extern "C" fn real_time_status(
    sockets: *mut c_void,
    conn: u32,
    out: *mut c_void,
    lanes: i32,
    lane_status: *mut c_void,
) -> i32 {
    // the core asks for no lane data
    if lanes != 0 || !lane_status.is_null() {
        return 8;
    }
    let m = model();
    let Some(c) = m.conns.get(&conn).filter(|c| c.machine == machine_of(sockets)) else { return 3 };
    let Some(machine) = m.machines.get(&c.machine) else { return 3 };
    let pending = c.peer.map_or(0, |p| m.inbox_bytes(p)) as i32;
    // SteamNetConnectionRealTimeStatus_t: state 0, ping 4, quality local 8, quality remote 12, send rate 32,
    // pending unreliable 36, pending reliable 40; 120 bytes in all
    let mut b = [0u8; 120];
    b[0..4].copy_from_slice(&c.state.to_le_bytes());
    b[4..8].copy_from_slice(&machine.ping_ms.to_le_bytes());
    b[8..12].copy_from_slice(&0.99f32.to_le_bytes());
    b[12..16].copy_from_slice(&0.98f32.to_le_bytes());
    b[32..36].copy_from_slice(&machine.send_rate.to_le_bytes());
    b[40..44].copy_from_slice(&pending.to_le_bytes());
    // SAFETY: the caller passes a buffer of at least sizeof(SteamNetConnectionRealTimeStatus_t) bytes.
    unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), out as *mut u8, b.len()) };
    1
}

fn locate(machine: usize) -> Located {
    let m = model();
    let Some(mm) = m.machines.get(&machine) else {
        return Located {
            sockets: std::ptr::null_mut(),
            utils: std::ptr::null_mut(),
            steam_id: None,
            note: "no machine".into(),
        };
    };
    if !mm.ready {
        return Located {
            sockets: std::ptr::null_mut(),
            utils: std::ptr::null_mut(),
            steam_id: None,
            note: "hsteamuser 0, user null sockets null utils null".into(),
        };
    }
    Located {
        sockets: interface(machine, 1),
        utils: interface(machine, 2),
        steam_id: Some(mm.steam_id),
        note: "hsteamuser 1, user ok sockets ok utils ok".into(),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// What the tests hold
// ---------------------------------------------------------------------------------------------------------------

/// Poller timings that keep the tests quick.
pub fn fast() -> Config {
    Config {
        active: Duration::from_millis(1),
        listening: Duration::from_millis(2),
        idle: Duration::from_millis(5),
        locate: Duration::from_millis(5),
        link_every: Duration::from_millis(20),
        ..Config::default()
    }
}

/// One Steam client in the model: its own account and interfaces. Start a core on it with [`FakeMachine::core`].
pub struct FakeMachine {
    id: usize,
}

impl FakeMachine {
    /// A machine with Steam initialised and a fresh account.
    pub fn new() -> FakeMachine {
        let steam_id = NEXT_STEAM_ID.fetch_add(1, Ordering::Relaxed);
        let mut m = model();
        m.next_machine += 1;
        let id = m.next_machine;
        m.machines.insert(
            id,
            Machine {
                steam_id,
                ready: true,
                reachable: true,
                callback: None,
                events: VecDeque::new(),
                route_delay: Duration::ZERO,
                wire_delay: Duration::ZERO,
                relay_pop: 0,
                stall_receive: false,
                config_ok: true,
                config: Vec::new(),
                closes: Vec::new(),
                accepts: 0,
                relay_inits: 0,
                largest_sent: 0,
                full_sends: 0,
                callback_runs: 0,
                ping_ms: 30,
                send_rate: 262_144,
            },
        );
        FakeMachine { id }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Machine) -> R) -> R {
        let mut m = model();
        let machine = m.machines.get_mut(&self.id).expect("a machine of this model");
        f(machine)
    }

    pub fn steam_id(&self) -> u64 {
        self.with(|m| m.steam_id)
    }

    /// The function table for a core on this machine.
    pub(super) fn api(&self) -> Api {
        let id = self.id;
        Api {
            locate: Box::new(move || locate(id)),
            alive: Box::new(move || model().machines.get(&id).is_some_and(|m| m.ready)),
            init_relay,
            set_status_callback,
            set_config: Some(set_config),
            run_callbacks,
            create_listen,
            close_listen: Some(close_listen),
            connect_p2p,
            accept,
            send,
            receive,
            release_message,
            close,
            connection_info,
            real_time_status: Some(real_time_status),
        }
    }

    /// A core on this machine with quick timings, started and waited for until Steam is ready.
    pub fn core(&self) -> SteamCore {
        let core = self.core_with(fast());
        core.handle().wait_ready(Instant::now() + Duration::from_secs(10), &|| false).expect("the fake Steam is ready");
        core
    }

    /// A core with the given timings, not waited for.
    pub fn core_with(&self, config: Config) -> SteamCore {
        SteamCore::start(self.api(), config).expect("the poller thread starts")
    }

    // ---- knobs

    /// Steam initialised or not (the interfaces exist or are null). Shutting Steam down destroys what it held: this
    /// machine's listen sockets and connections go, and the other ends find out by timing out.
    pub fn set_ready(&self, ready: bool) {
        let mut m = model();
        let Some(machine) = m.machines.get_mut(&self.id) else { return };
        machine.ready = ready;
        if ready {
            return;
        }
        m.listens.retain(|_, l| l.machine != self.id);
        let mine: Vec<u32> = m.conns.iter().filter(|(_, c)| c.machine == self.id).map(|(&h, _)| h).collect();
        for handle in mine {
            let Some(conn) = m.conns.remove(&handle) else { continue };
            if let Some(peer) = conn.peer {
                m.set_state(peer, PROBLEM, 5003, "Timed out");
            }
        }
    }

    /// An unreachable account never answers a connection attempt.
    pub fn set_reachable(&self, reachable: bool) {
        self.with(|m| m.reachable = reachable);
    }

    /// Time between accepting a connection and having a route (applies to either end).
    pub fn set_route_delay(&self, delay: Duration) {
        self.with(|m| m.route_delay = delay);
    }

    /// Time a message or a close sent from this machine needs to be visible at the other end.
    pub fn set_wire_delay(&self, delay: Duration) {
        self.with(|m| m.wire_delay = delay);
    }

    /// A relay point other than 0 makes this machine's connections report a relayed route.
    pub fn set_relay(&self, pop: u32) {
        self.with(|m| m.relay_pop = pop);
    }

    /// The ping Steam reports for this machine's connections; a negative number is "no estimate yet".
    pub fn set_ping(&self, ms: i32) {
        self.with(|m| m.ping_ms = ms);
    }

    /// The application stops reading: receives return nothing, so the peer's send buffer fills.
    pub fn stall_receive(&self, stall: bool) {
        self.with(|m| m.stall_receive = stall);
    }

    /// Steam refuses `SetConfigValue`.
    pub fn refuse_config(&self) {
        self.with(|m| m.config_ok = false);
    }

    /// The network dies: every connection of this machine ends with a problem at both ends.
    pub fn break_connections(&self) {
        let mut m = model();
        let mine: Vec<u32> = m.conns.iter().filter(|(_, c)| c.machine == self.id).map(|(&h, _)| h).collect();
        for handle in mine {
            let peer = m.conns.get(&handle).and_then(|c| c.peer);
            m.set_state(handle, PROBLEM, 5003, "Timed out");
            if let Some(p) = peer {
                m.set_state(p, PROBLEM, 5003, "Timed out");
            }
        }
    }

    /// Puts a message into a connection's inbox as if the peer had sent it, past every limit.
    pub fn inject(&self, conn: u32, bytes: &[u8]) {
        let mut m = model();
        if let Some(c) = m.conns.get_mut(&conn).filter(|c| c.machine == self.id) {
            c.inbox.push_back(Wire { visible_at: Instant::now(), data: bytes.to_vec() });
        }
    }

    /// A status callback for a connection nobody tracks, as another user of Steam in the process would cause.
    pub fn foreign_event(&self, conn: u32, state: i32, listen: u32) {
        let bytes = status_bytes(conn, state, 0, listen, 76561198999999999, 0, 0, "");
        self.with(|m| m.events.push_back(bytes));
    }

    // ---- observations

    /// Handles of the live connections of this machine, oldest first.
    pub fn connections(&self) -> Vec<u32> {
        let m = model();
        let mut handles: Vec<u32> = m.conns.iter().filter(|(_, c)| c.machine == self.id).map(|(&h, _)| h).collect();
        handles.sort_unstable();
        handles
    }

    pub fn listeners(&self) -> usize {
        model().listens.values().filter(|l| l.machine == self.id).count()
    }

    /// `SetConfigValue` calls Steam accepted: (value, scope, data type, number).
    pub fn config_calls(&self) -> Vec<(i32, i32, i32, i32)> {
        self.with(|m| m.config.clone())
    }

    /// (handle, linger) of every close that found its connection.
    pub fn closes(&self) -> Vec<(u32, bool)> {
        self.with(|m| m.closes.clone())
    }

    pub fn accepts(&self) -> usize {
        self.with(|m| m.accepts)
    }

    pub fn relay_inits(&self) -> usize {
        self.with(|m| m.relay_inits)
    }

    /// Size in bytes of the biggest message this machine sent.
    pub fn largest_sent(&self) -> usize {
        self.with(|m| m.largest_sent)
    }

    /// How many sends of this machine found the send buffer full.
    pub fn full_sends(&self) -> usize {
        self.with(|m| m.full_sends)
    }

    /// How many times the core has pumped this machine's callbacks (once per poll cycle).
    pub fn callback_runs(&self) -> usize {
        self.with(|m| m.callback_runs)
    }

    /// Messages handed to this machine's core and not released.
    pub fn unreleased_messages(&self) -> usize {
        model().messages.values().filter(|msg| msg.machine == self.id).count()
    }

    /// Releases of a message nobody had handed out (anywhere in the model).
    pub fn bad_releases() -> usize {
        model().bad_releases
    }
}
