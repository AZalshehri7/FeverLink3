//! The Steam transport: the sealed frames of [`crate::net`] over Steam peer-to-peer connections, so players behind
//! any NAT can reach a host by its Steam ID with no port forwarding (Steam punches NATs and falls back to its own
//! relays). It is shaped like the TCP transport: [`SteamHostNet`] mirrors `HostNet`, [`SteamClientNet`] mirrors
//! `ClientNet`, and both turn what happens on the wire into [`NetEvent`]s on an `mpsc` channel, so the session
//! thread and the protocol above it do not change.
//!
//! - **One Steam message is exactly one frame**, `FrameCodec::encode_frame` on the way out and
//!   `FrameCodec::try_decode` on the way in. Steam keeps message boundaries, so no stream parsing is needed; a
//!   message that holds anything but one whole frame (trailing bytes, a cut-off frame, a length outside the
//!   protocol's bounds) is a protocol violation and closes that connection, as a bad frame does on TCP.
//! - **Sizes.** Steam sends at most 512 KiB per message ([`MAX_MESSAGE`]), the protocol allows frames up to 1 MiB.
//!   The largest frame the protocol really produces is a savegame chunk, about 350 KB (256 KiB of data, base64,
//!   JSON, tag), so everything fits; a frame that does not is refused by `send` with an error before Steam sees it,
//!   and the connection stays open.
//! - **Backpressure.** Steam queues at most 512 KiB per connection and answers more with "limit exceeded", where a
//!   TCP write would block. `send` therefore waits for room (up to [`SEND_TIMEOUT`]); a savegame streamed chunk by
//!   chunk is paced by the link instead of failing.
//! - **Closing** follows the rules at the top of `net.rs`: `Closed` is reported exactly once, last; after `close` or
//!   a failed write no more frames are delivered. A close lingers, so a frame written just before it (a `Reject`)
//!   still reaches the other side.
//! - **Events** come from [`crate::steamnet::SteamCore`]'s poller thread; there is no thread per connection.
//!
//! The host listens on virtual port 0, the probe uses 1. `Opened` carries no address (`peer: None`): a Steam peer
//! is not a socket address.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use feverlink3_core::protocol::{Frame, FrameCodec};
use feverlink3_core::session::ConnectionId;

use feverlink3_core::crypto::SessionKey;

use crate::gamelink;
use crate::net::{
    BulkSendJob, BulkSendResult, BulkSender, ClientTransport, ConnectContext, HostTransport, LinkInfo, NetEvent,
    ParContext, ParJob, ParProgress, ParSender,
};
use crate::steamnet::par::{self, Allow, ParEvent, ParService};
use crate::session::{self, par_note, HostConfig, PlayerConfig, SessionHandle};
use crate::steamnet::bulk::{Limits, Receiver as BulkReceiver};
use crate::steamnet::bulk_io::{self, FileSource, Hub, SendJob};
use crate::steamnet::speed::bulk_config;
use crate::steamnet::{self, CoreRef, End, Handle, Owner, Route, MAX_MESSAGE};
use feverlink3_core::session::ResyncLimits;

/// The virtual port sessions use; the probe has 1.
pub const VIRTUAL_PORT: i32 = 0;
/// How long [`SteamClientNet::connect`] waits for a route. Steam's own search takes 10 to 20 s through the relays.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a send waits for room in Steam's send buffer before the connection is given up.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// How long [`SteamHostNet::listen`] waits for Steam to be initialised.
pub const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// Connections a host keeps at once (waiting for their handshake included). A session has at most 8 games (the host and seven players); this
/// only stops a stranger who knows the Steam ID from using up memory.
pub const MAX_CONNECTIONS: usize = 64;
/// The status texts a connection attempt reports.
pub const STATUS_CONTACTING: &str = "contacting the host";
pub const STATUS_FINDING_ROUTE: &str = "finding a route";

/// A host's address on Steam: its Steam ID (`steamID64`), 17 digits. The join code will carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SteamEndpoint(pub u64);

impl SteamEndpoint {
    /// Digits of an individual account's Steam ID.
    pub const DIGITS: usize = 17;

    /// Parses what a player pastes: exactly 17 digits that form an individual account of the public universe (a
    /// typo in a Steam ID almost never does). Spaces around it are ignored.
    pub fn parse(text: &str) -> Option<SteamEndpoint> {
        let text = text.trim();
        if text.len() != Self::DIGITS || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let id: u64 = text.parse().ok()?;
        let universe_public = id >> 56 == 1;
        let kind_individual = (id >> 52) & 0xF == 1;
        let account = id & 0xFFFF_FFFF;
        (universe_public && kind_individual && account != 0).then_some(SteamEndpoint(id))
    }

    /// The 17 digits.
    pub fn format(self) -> String {
        format!("{:017}", self.0)
    }
}

impl fmt::Display for SteamEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format())
    }
}

impl FromStr for SteamEndpoint {
    type Err = String;

    fn from_str(text: &str) -> Result<SteamEndpoint, String> {
        SteamEndpoint::parse(text).ok_or_else(|| format!("{:?} is not a Steam ID (17 digits)", text.trim()))
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The frame as the one Steam message that carries it. Errors keep the connection open: nothing was sent.
fn encode(frame: &Frame) -> io::Result<Vec<u8>> {
    let bytes =
        FrameCodec::encode_frame(frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    if bytes.len() > MAX_MESSAGE {
        let why = format!("frame of {} bytes exceeds Steam's message limit of {MAX_MESSAGE} bytes", bytes.len());
        return Err(io::Error::new(io::ErrorKind::InvalidInput, why));
    }
    Ok(bytes)
}

/// The frame a received Steam message holds. `Err` says what is wrong with a message that is not exactly one frame.
fn decode(data: &[u8]) -> Result<Frame, String> {
    let limit = FrameCodec::LENGTH_FIELD_SIZE + FrameCodec::MAX_FRAME_LENGTH;
    if data.len() > limit {
        return Err(format!("message of {} bytes exceeds the frame limit of {limit} bytes", data.len()));
    }
    match FrameCodec::try_decode(data) {
        Ok(Some((frame, used))) if used == data.len() => Ok(frame),
        Ok(Some((_, used))) => Err(format!("{} bytes follow the frame in one message", data.len() - used)),
        Ok(None) => Err(format!("message of {} bytes is shorter than the frame it announces", data.len())),
        Err(e) => Err(e.to_string()),
    }
}

/// The core's latest measurement of one connection, in the transport-neutral form the session reports.
fn link_info(core: &CoreRef, link: &Link) -> LinkInfo {
    let snapshot = core.link(link.handle);
    LinkInfo {
        conn: link.conn,
        ping_ms: snapshot.and_then(|s| s.ping_ms),
        route: snapshot.map(|s| s.route.text().to_string()),
    }
}

/// One Steam connection as the transports see it.
struct Link {
    conn: ConnectionId,
    handle: Handle,
    closed: AtomicBool,
    /// Why we closed it (the first reason wins); `None` when the other side ended it.
    reason: Mutex<Option<String>>,
    /// The other side's Steam ID when Steam said (0: unknown); parallel bulk connections go to it.
    remote: AtomicU64,
}

impl Link {
    fn new(conn: ConnectionId, handle: Handle) -> Arc<Link> {
        Arc::new(Link {
            conn,
            handle,
            closed: AtomicBool::new(false),
            reason: Mutex::new(None),
            remote: AtomicU64::new(0),
        })
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Stops delivering frames, remembers why, and closes the Steam connection; `Closed` follows from the core.
    fn close(&self, core: &CoreRef, reason: &str) {
        lock(&self.reason).get_or_insert_with(|| reason.to_string());
        self.closed.store(true, Ordering::Release);
        core.close(self.handle);
    }

    /// Sends a frame. A failed write closes the connection, as on TCP; a frame that cannot be sent at all (too big)
    /// does not.
    fn send(&self, core: &CoreRef, frame: &Frame, timeout: Duration) -> io::Result<()> {
        let bytes = encode(frame)?;
        if self.is_closed() {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "connection closed"));
        }
        let result = core.send_reliable(self.handle, &bytes, timeout);
        if let Err(e) = &result {
            self.close(core, &format!("write failed: {e}"));
        }
        result
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct HostLinks {
    by_handle: HashMap<Handle, Arc<Link>>,
    by_conn: HashMap<ConnectionId, Arc<Link>>,
}

struct HostShared {
    core: CoreRef,
    tx: Sender<NetEvent>,
    links: Mutex<HostLinks>,
    next_id: AtomicU64,
    max_connections: usize,
    /// Routes the bulk datagrams (the acks of a save being sent) to their transfers.
    hub: Arc<Hub>,
}

impl Owner for HostShared {
    fn opened(&self, handle: Handle, incoming: bool, remote: Option<u64>) {
        if !incoming {
            return;
        }
        let link = {
            let mut links = lock(&self.links);
            if links.by_handle.len() >= self.max_connections {
                None
            } else {
                let link = Link::new(self.next_id.fetch_add(1, Ordering::Relaxed), handle);
                link.remote.store(remote.unwrap_or(0), Ordering::Release);
                links.by_handle.insert(handle, Arc::clone(&link));
                links.by_conn.insert(link.conn, Arc::clone(&link));
                Some(link)
            }
        };
        match link {
            // never shown to the session: it has no connection to forget
            None => {
                self.core.close(handle);
            }
            Some(link) => {
                if self.tx.send(NetEvent::Opened { conn: link.conn, peer: None }).is_err() {
                    link.close(&self.core, "session ended");
                }
            }
        }
    }

    fn message(&self, handle: Handle, data: &[u8]) {
        let Some(link) = lock(&self.links).by_handle.get(&handle).cloned() else { return };
        if link.is_closed() {
            return;
        }
        match decode(data) {
            Ok(frame) => {
                if self.tx.send(NetEvent::Frame { conn: link.conn, frame }).is_err() {
                    link.close(&self.core, "session ended");
                }
            }
            Err(why) => link.close(&self.core, &why),
        }
    }

    fn datagram(&self, handle: Handle, data: &[u8]) {
        self.hub.on_datagram(handle, data);
    }

    fn closed(&self, handle: Handle, end: End) {
        self.hub.drop_connection(handle);
        let link = {
            let mut links = lock(&self.links);
            let link = links.by_handle.remove(&handle);
            if let Some(link) = &link {
                links.by_conn.remove(&link.conn);
            }
            link
        };
        // a connection the session never heard of (turned away, or closed before it opened) ends silently
        let Some(link) = link else { return };
        link.closed.store(true, Ordering::Release);
        let reason = lock(&link.reason).clone().unwrap_or_else(|| end.describe());
        let _ = self.tx.send(NetEvent::Closed { conn: link.conn, reason });
    }
}

/// The listening side. Dropping it stops listening and closes every connection.
pub struct SteamHostNet {
    core: CoreRef,
    shared: Arc<HostShared>,
    listener: Handle,
    send_timeout: Duration,
    stopped: bool,
}

impl SteamHostNet {
    /// Listens for Steam connections on [`VIRTUAL_PORT`] with the process's Steam core. Connection ids start at 1
    /// and are never reused. Fails when the process has no Steam or Steam is not initialised within
    /// [`READY_TIMEOUT`].
    pub fn listen(tx: Sender<NetEvent>) -> io::Result<SteamHostNet> {
        let core = steamnet::global().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "Steam networking is not available (is the game running through Steam?)",
            )
        })?;
        Self::listen_on(&core, tx, READY_TIMEOUT, MAX_CONNECTIONS)
    }

    /// As [`SteamHostNet::listen`] on a given core, with its own wait for Steam and connection limit.
    pub fn listen_on(
        core: &CoreRef,
        tx: Sender<NetEvent>,
        ready_timeout: Duration,
        max_connections: usize,
    ) -> io::Result<SteamHostNet> {
        core.wait_ready(Instant::now() + ready_timeout, &|| false)?;
        let shared = Arc::new(HostShared {
            core: core.clone(),
            tx,
            links: Mutex::new(HostLinks::default()),
            next_id: AtomicU64::new(1),
            max_connections,
            hub: Hub::new(),
        });
        let owner: Arc<dyn Owner> = shared.clone();
        let listener = core.listen(VIRTUAL_PORT, owner)?;
        Ok(SteamHostNet { core: core.clone(), shared, listener, send_timeout: SEND_TIMEOUT, stopped: false })
    }

    /// The address a joiner connects to: this account's Steam ID, once Steam has said it.
    pub fn endpoint(&self) -> Option<SteamEndpoint> {
        self.core.status().steam_id.map(SteamEndpoint)
    }

    /// Changes how long a send waits for room in Steam's send buffer.
    pub fn set_send_timeout(&mut self, timeout: Duration) {
        self.send_timeout = timeout;
    }

    fn link(&self, conn: ConnectionId) -> Option<Arc<Link>> {
        lock(&self.shared.links).by_conn.get(&conn).cloned()
    }

    /// Sends a frame to one connection. An error means the connection is gone or was just closed because of this
    /// write (its `Closed` event follows), or the frame was too big for Steam and nothing was sent.
    pub fn send(&self, conn: ConnectionId, frame: &Frame) -> io::Result<()> {
        match self.link(conn) {
            Some(link) => link.send(&self.core, frame, self.send_timeout),
            None => Err(io::Error::new(io::ErrorKind::NotConnected, format!("no connection {conn}"))),
        }
    }

    /// Closes one connection. Frames already written are delivered first; `Closed` follows.
    pub fn close(&self, conn: ConnectionId) {
        if let Some(link) = self.link(conn) {
            link.close(&self.core, "closed locally");
        }
    }

    /// Number of connections that are open now.
    pub fn connection_count(&self) -> usize {
        lock(&self.shared.links).by_conn.values().filter(|l| !l.is_closed()).count()
    }

    /// The route of one connection ("direct" or "through a Steam relay").
    pub fn route(&self, conn: ConnectionId) -> Option<Route> {
        self.core.route(self.link(conn)?.handle)
    }

    /// The latest ping and route of every open connection. Steam measures again every second, so a relayed
    /// connection that Steam moves to a direct path shows as direct a moment later.
    pub fn links(&self) -> Vec<LinkInfo> {
        let links: Vec<Arc<Link>> =
            lock(&self.shared.links).by_conn.values().filter(|l| !l.is_closed()).cloned().collect();
        let mut infos: Vec<LinkInfo> = links.iter().map(|l| link_info(&self.core, l)).collect();
        infos.sort_by_key(|i| i.conn);
        infos
    }

    /// Stops listening and closes every connection. Idempotent; also done on drop.
    pub fn shutdown(&mut self) {
        if std::mem::replace(&mut self.stopped, true) {
            return;
        }
        self.core.close_listener(self.listener);
        let links: Vec<Arc<Link>> = lock(&self.shared.links).by_handle.values().cloned().collect();
        for link in links {
            link.close(&self.core, "host stopped");
        }
    }
}

impl Drop for SteamHostNet {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------------------------------------

enum Phase {
    /// Waiting for a route; the text is what Steam is doing.
    Connecting(String),
    Open,
    /// Ended before it opened, or abandoned by the caller.
    Failed(String),
}

struct ClientState {
    phase: Phase,
    link: Option<Arc<Link>>,
}

struct ClientShared {
    core: CoreRef,
    conn: ConnectionId,
    tx: Sender<NetEvent>,
    state: Mutex<ClientState>,
    changed: Condvar,
    me: std::sync::Weak<ClientShared>,
    /// Routes the bulk datagrams of the savegame the host sends.
    hub: Arc<Hub>,
    /// A bulk receive is running (one at a time).
    bulk_busy: AtomicBool,
}

impl ClientShared {
    /// The host offers a savegame as bulk datagrams. Only the host can (the connection is the one to the host), one at
    /// a time, at most as big as a resync allows. The finished save goes to the session thread as a `BulkSave` event.
    fn accept_bulk(&self, handle: Handle, xfer: u32, offer: Vec<u8>, rx: std::sync::mpsc::Receiver<Vec<u8>>) {
        let link = lock(&self.state).link.clone();
        let (Some(link), Some(me)) = (link, self.me.upgrade()) else {
            self.hub.unregister(handle, xfer);
            return;
        };
        if self.bulk_busy.swap(true, Ordering::AcqRel) {
            self.hub.unregister(handle, xfer);
            let busy = steamnet::bulk::wire::encode_abort(xfer, steamnet::bulk::wire::reason::BUSY);
            let _ = self.core.send_datagram(handle, &busy);
            return;
        }
        let spawned = std::thread::Builder::new().name("feverlink3-steam-bulk-recv".into()).spawn(move || {
            let limits = Limits { max_total: ResyncLimits::MAX_CHUNKS as u64 * ResyncLimits::CHUNK_SIZE as u64 };
            let started = Instant::now();
            let mut on_complete = |r: &mut BulkReceiver| {
                let Some(bytes) = r.take_data() else { return };
                let sha: String = r.sha256().iter().map(|b| format!("{b:02x}")).collect();
                let secs = started.elapsed().as_secs_f64().max(0.001);
                let note = format!(
                    "resync {}: save received as bulk datagrams: {} bytes in {secs:.1} s ({:.2} MB/s), {} duplicate datagram(s), {} acks sent",
                    r.ctx(),
                    bytes.len(),
                    bytes.len() as f64 / secs / 1e6,
                    r.stats.duplicates,
                    r.stats.acks_sent
                );
                let event = NetEvent::BulkSave { conn: me.conn, id: r.ctx() as i32, sha256: sha, bytes, note };
                let _ = me.tx.send(event);
            };
            let _ = bulk_io::run_receiver(
                &me.core,
                &me.hub,
                handle,
                &offer,
                rx,
                &limits,
                &|| link.is_closed(),
                &|_| {},
                &mut on_complete,
            );
            me.bulk_busy.store(false, Ordering::Release);
        });
        if spawned.is_err() {
            self.bulk_busy.store(false, Ordering::Release);
            self.hub.unregister(handle, xfer);
        }
    }
}

fn status_text(step: i32) -> &'static str {
    if step == steamnet::state::FINDING_ROUTE {
        STATUS_FINDING_ROUTE
    } else {
        STATUS_CONTACTING
    }
}

impl Owner for ClientShared {
    fn progress(&self, _handle: Handle, step: i32) {
        let mut state = lock(&self.state);
        if let Phase::Connecting(text) = &mut state.phase {
            let new = status_text(step);
            if text.as_str() != new {
                *text = new.to_string();
                self.changed.notify_all();
            }
        }
    }

    fn opened(&self, handle: Handle, _incoming: bool, _remote: Option<u64>) {
        let mut state = lock(&self.state);
        if !matches!(state.phase, Phase::Connecting(_)) {
            return;
        }
        // the session hears about the connection before the caller of connect() does: `Opened` is its first event
        if self.tx.send(NetEvent::Opened { conn: self.conn, peer: None }).is_err() {
            state.phase = Phase::Failed("session ended".into());
            self.core.close(handle);
        } else {
            state.link = Some(Link::new(self.conn, handle));
            state.phase = Phase::Open;
        }
        self.changed.notify_all();
    }

    fn message(&self, _handle: Handle, data: &[u8]) {
        let Some(link) = lock(&self.state).link.clone() else { return };
        if link.is_closed() {
            return;
        }
        match decode(data) {
            Ok(frame) => {
                if self.tx.send(NetEvent::Frame { conn: self.conn, frame }).is_err() {
                    link.close(&self.core, "session ended");
                }
            }
            Err(why) => link.close(&self.core, &why),
        }
    }

    fn datagram(&self, handle: Handle, data: &[u8]) {
        self.hub.on_datagram(handle, data);
    }

    fn closed(&self, handle: Handle, end: End) {
        self.hub.drop_connection(handle);
        let link = {
            let mut state = lock(&self.state);
            match &state.phase {
                Phase::Open => state.link.clone(),
                Phase::Connecting(_) => {
                    // never opened: connect() reports it; the session has no connection to close
                    state.phase = Phase::Failed(end.describe());
                    self.changed.notify_all();
                    None
                }
                Phase::Failed(_) => None,
            }
        };
        if let Some(link) = link {
            link.closed.store(true, Ordering::Release);
            let reason = lock(&link.reason).clone().unwrap_or_else(|| end.describe());
            let _ = self.tx.send(NetEvent::Closed { conn: self.conn, reason });
        }
    }
}

/// The connecting side. Dropping it closes the connection.
pub struct SteamClientNet {
    core: CoreRef,
    endpoint: SteamEndpoint,
    link: Arc<Link>,
    send_timeout: Duration,
    /// The listener for parallel bulk connections (shared by the process), when Steam opened it.
    par: Option<Arc<ParService>>,
    tx: Sender<NetEvent>,
}

impl SteamClientNet {
    /// Connects to the host's Steam ID with the process's Steam core, waiting up to [`CONNECT_TIMEOUT`] for a route.
    /// `conn` is the id its events carry; the first event is `Opened`.
    pub fn connect(endpoint: SteamEndpoint, conn: ConnectionId, tx: Sender<NetEvent>) -> io::Result<SteamClientNet> {
        let core = steamnet::global().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "Steam networking is not available (is the game running through Steam?)",
            )
        })?;
        Self::connect_with(
            &core,
            endpoint,
            ConnectContext { conn, tx, timeout: CONNECT_TIMEOUT, cancelled: &|| false, progress: &mut |_| {} },
        )
    }

    /// Connects on a given core and reports progress: `progress` hears "contacting the host", "finding a route" and
    /// the route once open, and `cancelled` is polled while waiting. Returns when the connection is open, Steam gave
    /// up on it (the error says why), the timeout passed or `cancelled` said stop. No event reaches `tx` for an
    /// attempt that fails.
    pub fn connect_with(
        core: &CoreRef,
        endpoint: SteamEndpoint,
        ctx: ConnectContext<'_>,
    ) -> io::Result<SteamClientNet> {
        let ConnectContext { conn, tx, timeout, cancelled, progress } = ctx;
        let deadline = Instant::now() + timeout;
        if !core.status().ready {
            progress("waiting for Steam");
        }
        core.wait_ready(deadline, cancelled)?;

        let shared = Arc::new_cyclic(|me| ClientShared {
            core: core.clone(),
            conn,
            tx,
            state: Mutex::new(ClientState { phase: Phase::Connecting(STATUS_CONTACTING.into()), link: None }),
            changed: Condvar::new(),
            me: me.clone(),
            hub: Hub::new(),
            bulk_busy: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&shared);
        shared.hub.set_acceptor(move |handle, xfer, offer, rx| {
            if let Some(shared) = weak.upgrade() {
                shared.accept_bulk(handle, xfer, offer, rx);
            }
        });
        let owner: Arc<dyn Owner> = shared.clone();
        let handle = core.connect(endpoint.0, VIRTUAL_PORT, owner)?;
        let mut shown = STATUS_CONTACTING.to_string();
        progress(&shown);

        let mut state = lock(&shared.state);
        let outcome = loop {
            match &state.phase {
                Phase::Open => break Ok(()),
                Phase::Failed(why) => {
                    let why = format!("could not connect to Steam ID {endpoint}: {why}");
                    break Err(io::Error::new(io::ErrorKind::ConnectionRefused, why));
                }
                Phase::Connecting(text) if *text != shown => {
                    shown = text.clone();
                    drop(state);
                    progress(&shown);
                    state = lock(&shared.state);
                    continue;
                }
                Phase::Connecting(_) => {}
            }
            if cancelled() {
                break Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let now = Instant::now();
            if now >= deadline {
                let why = format!(
                    "no route to Steam ID {endpoint} after {:.1} s (last status: {shown})",
                    timeout.as_secs_f32()
                );
                break Err(io::Error::new(io::ErrorKind::TimedOut, why));
            }
            state = shared
                .changed
                .wait_timeout(state, (deadline - now).min(Duration::from_millis(20)))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        };
        let link = state.link.clone();
        if outcome.is_err() {
            // late events of this attempt find a failed state and do nothing
            state.phase = Phase::Failed("abandoned".into());
        }
        drop(state);
        if let Err(e) = outcome {
            core.close(handle);
            return Err(e);
        }
        let link = link.ok_or_else(|| io::Error::other("connection opened without a link"))?;
        let route = core.route(handle).map_or("route unknown", Route::text);
        progress(&format!("connected, {route}"));
        let par = ParService::for_core(core).ok();
        let tx = shared.tx.clone();
        Ok(SteamClientNet { core: core.clone(), endpoint, link, send_timeout: SEND_TIMEOUT, par, tx })
    }

    pub fn conn(&self) -> ConnectionId {
        self.link.conn
    }

    /// The host this connection goes to.
    pub fn endpoint(&self) -> SteamEndpoint {
        self.endpoint
    }

    /// Direct or through a Steam relay.
    pub fn route(&self) -> Option<Route> {
        self.core.route(self.link.handle)
    }

    /// The latest ping and route of the connection to the host (measured again every second).
    pub fn link_info(&self) -> LinkInfo {
        link_info(&self.core, &self.link)
    }

    /// Changes how long a send waits for room in Steam's send buffer.
    pub fn set_send_timeout(&mut self, timeout: Duration) {
        self.send_timeout = timeout;
    }

    pub fn send(&self, frame: &Frame) -> io::Result<()> {
        self.link.send(&self.core, frame, self.send_timeout)
    }

    pub fn close(&self) {
        self.link.close(&self.core, "closed locally");
    }
}

impl Drop for SteamClientNet {
    fn drop(&mut self) {
        if let Some(par) = &self.par {
            par.disallow(self.endpoint.0, self.link.conn);
        }
        self.close();
    }
}

/// The ping and queue of one connection, as the parallel sender reads them.
struct LinkControl {
    core: CoreRef,
    handle: Handle,
}

impl par::Control for LinkControl {
    fn ping_ms(&self) -> Option<u32> {
        self.core.link(self.handle).and_then(|l| l.ping_ms)
    }

    fn pending(&self) -> u32 {
        self.core.link(self.handle).map_or(0, |l| l.pending)
    }
}

impl HostTransport for SteamHostNet {
    fn send(&self, conn: ConnectionId, frame: &Frame) -> io::Result<()> {
        SteamHostNet::send(self, conn, frame)
    }

    fn close(&self, conn: ConnectionId) {
        SteamHostNet::close(self, conn);
    }

    fn describe(&self) -> String {
        match self.endpoint() {
            Some(endpoint) => format!("Steam (Steam ID {endpoint}, virtual port {VIRTUAL_PORT})"),
            None => format!("Steam (virtual port {VIRTUAL_PORT})"),
        }
    }

    fn links(&self) -> Vec<LinkInfo> {
        SteamHostNet::links(self)
    }

    fn par_sender(&self, conn: ConnectionId) -> Option<ParSender> {
        self.par_sender_for(conn)
    }

    fn bulk_sender(&self, conn: ConnectionId) -> Option<BulkSender> {
        let link = self.link(conn).filter(|l| !l.is_closed())?;
        let core = self.core.clone();
        let shared = Arc::clone(&self.shared);
        Some(Box::new(move |job: BulkSendJob, cancelled: &dyn Fn() -> bool, progress: &dyn Fn(u64)| {
            let source = match FileSource::open(&job.path) {
                Ok(s) => s,
                Err(e) => return BulkSendResult::Failed(format!("cannot read {}: {e}", job.path.display())),
            };
            let send = SendJob {
                xfer: job.ctx,
                ctx: job.ctx,
                sha256: job.sha256,
                source: Box::new(source),
                cfg: bulk_config(),
            };
            let report = bulk_io::run_sender(&core, &shared.hub, link.handle, send, cancelled, progress);
            match report.state {
                steamnet::bulk::SenderState::Done => BulkSendResult::Done {
                    summary: steamnet::throughput::SenderSummary::from_stats(&report.stats).text(),
                },
                steamnet::bulk::SenderState::Failed(why) => BulkSendResult::Failed(why),
                other => BulkSendResult::Failed(format!("ended in state {other:?}")),
            }
        }))
    }
}

impl SteamHostNet {
    /// Sending one file to the player on `conn` over parallel connections to the player's Steam ID.
    fn par_sender_for(&self, conn: ConnectionId) -> Option<ParSender> {
        let link = self.link(conn).filter(|l| !l.is_closed())?;
        let remote = link.remote.load(Ordering::Acquire);
        if remote == 0 {
            return None;
        }
        let core = self.core.clone();
        Some(Box::new(move |job: ParJob, cancelled: &dyn Fn() -> bool, progress: &dyn Fn(&ParProgress)| {
            let source = match par::FileSource::open(&job.path) {
                Ok(s) => s,
                Err(e) => return BulkSendResult::Failed(format!("cannot read {}: {e}", job.path.display())),
            };
            let size = par::PayloadSource::len(&source);
            let spec = par::SendSpec {
                remote,
                xfer: job.ctx,
                ctx: job.ctx,
                secret: job.secret,
                sha256: job.sha256,
                source: Arc::new(source),
                conns: job.cfg.conns_for(size),
                cfg: job.cfg,
                control: Some(Arc::new(LinkControl { core: core.clone(), handle: link.handle })),
            };
            let report = par::run_send(&core, spec, cancelled, progress);
            if report.ok {
                BulkSendResult::Done { summary: report.summary() }
            } else {
                BulkSendResult::Failed(format!(
                    "{}; {}",
                    report.error.clone().unwrap_or_else(|| "failed".into()),
                    report.summary()
                ))
            }
        }))
    }
}

impl ClientTransport for SteamClientNet {
    fn send(&self, frame: &Frame) -> io::Result<()> {
        SteamClientNet::send(self, frame)
    }

    fn describe(&self) -> String {
        format!("Steam ID {}", self.endpoint)
    }

    fn link(&self) -> Option<LinkInfo> {
        Some(self.link_info())
    }

    fn bulk_capable(&self) -> bool {
        true
    }

    fn par_capable(&self) -> bool {
        self.par.is_some()
    }

    fn enable_par(&self, ctx: ParContext) {
        let Some(par) = &self.par else { return };
        let (conn, tx) = (self.link.conn, Mutex::new(self.tx.clone()));
        let emit = move |event: NetEvent| {
            let _ = lock(&tx).send(event);
        };
        let on_event = move |event: ParEvent| match event {
            ParEvent::Progress { ctx, bytes, total, lanes, rate } => {
                emit(NetEvent::BulkProgress { conn, id: ctx as i32, done: bytes, total, note: par_note(lanes, rate) })
            }
            ParEvent::Done { ctx, sha256, len, path, secs, lanes_max } => {
                let note = format!(
                    "resync {ctx}: save received over parallel connections: {len} bytes in {secs:.1} s ({:.2} MB/s), {lanes_max} connection(s) at most",
                    len as f64 / secs.max(0.001) / 1e6
                );
                emit(NetEvent::BulkFile { conn, id: ctx as i32, sha256: par::service::hex(&sha256), len, path, note })
            }
            ParEvent::Failed { ctx, why } => {
                emit(NetEvent::Note { conn, text: format!("resync {ctx}: the parallel receive failed: {why}") })
            }
        };
        par.allow(
            self.endpoint.0,
            Allow {
                secret: Some(ctx.secret),
                dir: ctx.dir,
                stem: Box::new(crate::resync::save_name_for),
                ext: "sav",
                max_total: ResyncLimits::MAX_CHUNKS as u64 * ResyncLimits::CHUNK_SIZE as u64,
                on_event: Box::new(on_event),
                owner: self.link.conn,
            },
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Sessions over Steam
// ---------------------------------------------------------------------------------------------------------------

/// Hosts a session over Steam with the process's Steam core (listening on [`VIRTUAL_PORT`]); `config.bind` is not
/// used. Everything else is [`session::run_host`].
pub fn run_steam_host(config: HostConfig) -> io::Result<SessionHandle> {
    session::run_host_over(config, SteamHostNet::listen)
}

/// As [`run_steam_host`] on a given core (the tests use a fake Steam). Fails at once, without waiting for Steam, when
/// the core is not ready.
pub fn run_steam_host_on(core: &CoreRef, config: HostConfig) -> io::Result<SessionHandle> {
    let core = core.clone();
    session::run_host_over(config, move |tx| SteamHostNet::listen_on(&core, tx, Duration::ZERO, MAX_CONNECTIONS))
}

/// The connection step of a player session over Steam, for [`session::run_player_over`]: every attempt, the first
/// and each reconnect, connects to `host` on `core`.
pub fn steam_connector(
    core: CoreRef,
    host: SteamEndpoint,
) -> impl FnMut(ConnectContext<'_>) -> io::Result<Box<dyn ClientTransport>> + Send + 'static {
    move |ctx| SteamClientNet::connect_with(&core, host, ctx).map(|net| Box::new(net) as Box<dyn ClientTransport>)
}

/// Joins the session of `host` over Steam with the process's Steam core. The first attempt waits up to
/// `config.connect_timeout` (use [`steam_player_config`]: finding a route takes 10 to 20 s) and tells `progress`
/// what it is doing ("finding a route"); reconnects are the session's, as for TCP.
pub fn run_steam_player(
    config: PlayerConfig,
    host: SteamEndpoint,
    progress: &mut dyn FnMut(&str),
) -> io::Result<SessionHandle> {
    let core = steamnet::global().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "Steam networking is not available (is the game running through Steam?)",
        )
    })?;
    run_steam_player_on(&core, config, host, progress)
}

/// As [`run_steam_player`] on a given core (the tests use a fake Steam).
pub fn run_steam_player_on(
    core: &CoreRef,
    config: PlayerConfig,
    host: SteamEndpoint,
    progress: &mut dyn FnMut(&str),
) -> io::Result<SessionHandle> {
    session::run_player_over(config, steam_connector(core.clone(), host), progress)
}

/// A player configuration for a Steam session: no endpoints, [`CONNECT_TIMEOUT`] to find a route.
pub fn steam_player_config(name: &str, key: SessionKey, link: gamelink::Session) -> PlayerConfig {
    let mut config = PlayerConfig::new(Vec::new(), name, key, link);
    config.connect_timeout = CONNECT_TIMEOUT;
    config
}

impl fmt::Debug for SteamClientNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SteamClientNet")
            .field("conn", &self.link.conn)
            .field("endpoint", &self.endpoint)
            .field("closed", &self.link.is_closed())
            .finish()
    }
}

impl fmt::Debug for SteamHostNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SteamHostNet")
            .field("endpoint", &self.endpoint())
            .field("connections", &self.connection_count())
            .field("stopped", &self.stopped)
            .finish()
    }
}

mod selfcheck;
pub use selfcheck::{loopback_check, Check};

#[cfg(test)]
mod session_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steamnet::fake::FakeMachine;
    use crate::steamnet::SteamCore;
    use feverlink3_core::protocol::FrameHeader;
    use feverlink3_core::session::{codec, ResyncChunkMessage, ResyncLimits};
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    const WAIT: Duration = Duration::from_secs(10);

    fn frame(kind: u8, body: &[u8]) -> Frame {
        Frame::new(FrameHeader::new(1, kind, 3, 9), body.to_vec())
    }

    fn next(rx: &Receiver<NetEvent>) -> NetEvent {
        rx.recv_timeout(WAIT).expect("an event")
    }

    /// An event in a line, without the bytes of a big frame.
    fn brief(event: &NetEvent) -> String {
        match event {
            NetEvent::Frame { conn, frame } => {
                format!("Frame on {conn}: kind {}, {} body bytes", frame.header.kind, frame.body.len())
            }
            other => format!("{other:?}"),
        }
    }

    fn next_frame(rx: &Receiver<NetEvent>, conn: ConnectionId) -> Frame {
        match next(rx) {
            NetEvent::Frame { conn: c, frame } if c == conn => frame,
            other => panic!("expected a frame on {conn}, got {}", brief(&other)),
        }
    }

    fn next_closed(rx: &Receiver<NetEvent>, conn: ConnectionId) -> String {
        match next(rx) {
            NetEvent::Closed { conn: c, reason } if c == conn => reason,
            other => panic!("expected the end of {conn}, got {}", brief(&other)),
        }
    }

    fn quiet(rx: &Receiver<NetEvent>) {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(150)) {
            panic!("expected silence, got {}", brief(&event));
        }
    }

    /// A host on a machine of its own.
    struct Host {
        machine: FakeMachine,
        _core: SteamCore,
        net: SteamHostNet,
        rx: Receiver<NetEvent>,
    }

    fn host_with(max_connections: usize) -> Host {
        let machine = FakeMachine::new();
        let core = machine.core();
        let (tx, rx) = mpsc::channel();
        let net = SteamHostNet::listen_on(&core.handle(), tx, WAIT, max_connections).expect("listen");
        Host { machine, _core: core, net, rx }
    }

    fn host() -> Host {
        host_with(MAX_CONNECTIONS)
    }

    impl Host {
        fn endpoint(&self) -> SteamEndpoint {
            SteamEndpoint(self.machine.steam_id())
        }
    }

    /// A player on a machine of its own, connected to `host`.
    struct Guest {
        machine: FakeMachine,
        _core: SteamCore,
        net: SteamClientNet,
        rx: Receiver<NetEvent>,
    }

    fn connect_to(
        core: &CoreRef,
        host: SteamEndpoint,
        conn: ConnectionId,
        tx: Sender<NetEvent>,
    ) -> io::Result<SteamClientNet> {
        let ctx = ConnectContext { conn, tx, timeout: WAIT, cancelled: &|| false, progress: &mut |_| {} };
        SteamClientNet::connect_with(core, host, ctx)
    }

    fn guest_of(host: &Host, conn: ConnectionId) -> Guest {
        let machine = FakeMachine::new();
        let core = machine.core();
        let (tx, rx) = mpsc::channel();
        let net = connect_to(&core.handle(), host.endpoint(), conn, tx).expect("connect");
        Guest { machine, _core: core, net, rx }
    }

    /// Host and one guest, both `Opened` events consumed.
    fn pair() -> (Host, Guest) {
        let host = host();
        let guest = guest_of(&host, 7);
        assert!(matches!(next(&guest.rx), NetEvent::Opened { conn: 7, peer: None }));
        assert!(matches!(next(&host.rx), NetEvent::Opened { conn: 1, peer: None }));
        (host, guest)
    }

    #[test]
    fn endpoints_are_17_digit_steam_ids() {
        let e = SteamEndpoint::parse(" 76561198869158889 ").expect("a Steam ID");
        assert_eq!(e, SteamEndpoint(76561198869158889));
        assert_eq!((e.format(), e.to_string()), ("76561198869158889".to_string(), "76561198869158889".to_string()));
        assert_eq!("76561198869158889".parse::<SteamEndpoint>(), Ok(e));
        assert_eq!(SteamEndpoint::parse(&e.format()), Some(e));
        // the lowest and the highest individual accounts
        assert!(SteamEndpoint::parse("76561197960265729").is_some());
        assert!(SteamEndpoint::parse("76561202255233023").is_some());
        for bad in [
            "",
            "1234",
            "7656119886915888",
            "765611988691588890",
            "7656119886915888x",
            "76561198 869158889",
            "76561197960265728",
            "99999999999999999",
            "00000000000000001",
            "+6561198869158889",
        ] {
            assert_eq!(SteamEndpoint::parse(bad), None, "{bad:?}");
        }
        assert!("12".parse::<SteamEndpoint>().expect_err("not a Steam ID").contains("17 digits"));
    }

    #[test]
    fn frames_flow_both_ways_and_close_is_reported_once() {
        let (host, guest) = pair();
        guest.net.send(&frame(30, b"hello")).expect("send");
        let got = next_frame(&host.rx, 1);
        assert_eq!((got.header.kind, got.body.as_slice()), (30, &b"hello"[..]));
        host.net.send(1, &frame(11, b"state")).expect("send");
        assert_eq!(next_frame(&guest.rx, 7).body, b"state");
        assert_eq!(host.net.connection_count(), 1);
        assert_eq!(host.net.route(1), Some(Route::Direct));
        assert_eq!(guest.net.route(), Some(Route::Direct));

        host.net.close(1);
        assert_eq!(next_closed(&host.rx, 1), "closed locally");
        let reason = next_closed(&guest.rx, 7);
        assert!(reason.contains("closed by the other side"), "{reason}");
        quiet(&host.rx);
        quiet(&guest.rx);
        assert!(host.net.send(1, &frame(11, b"late")).is_err());
        assert!(guest.net.send(&frame(30, b"late")).is_err());
        assert_eq!(host.net.connection_count(), 0);
    }

    #[test]
    fn many_frames_cross_both_ways_at_once_in_order() {
        let (host, guest) = pair();
        let body = |side: u8, i: u32| -> Vec<u8> {
            let mut b = i.to_be_bytes().to_vec();
            b.extend(std::iter::repeat_n(side, (i as usize * 37) % 3000));
            b
        };
        thread::scope(|s| {
            s.spawn(|| {
                for i in 0..400 {
                    host.net.send(1, &frame(11, &body(1, i))).expect("host send");
                }
            });
            s.spawn(|| {
                for i in 0..400 {
                    guest.net.send(&frame(30, &body(2, i))).expect("guest send");
                }
            });
        });
        for i in 0..400 {
            assert_eq!(next_frame(&host.rx, 1).body, body(2, i));
            assert_eq!(next_frame(&guest.rx, 7).body, body(1, i));
        }
        quiet(&host.rx);
    }

    /// The most a savegame chunk frame can weigh: the biggest chunk, the widest numbers, the 16-byte tag.
    fn largest_chunk_frame() -> usize {
        let message = ResyncChunkMessage {
            id: i32::MIN,
            index: i32::MIN,
            count: i32::MIN,
            sha256: "f".repeat(64),
            data: vec![0xAB; ResyncLimits::CHUNK_SIZE],
        };
        FrameCodec::LENGTH_FIELD_SIZE + FrameHeader::SIZE + codec::encode(&message).len() + 16
    }

    #[test]
    fn a_savegame_chunk_fits_in_a_steam_message_with_room_to_spare() {
        let biggest = largest_chunk_frame();
        assert_eq!(biggest, 349_704, "the worst case the docs and the self-check quote");
        assert!(biggest + 100_000 < MAX_MESSAGE, "{biggest} bytes against Steam's {MAX_MESSAGE}");
        // Steam's message limit is the tighter of the two
        const { assert!(MAX_MESSAGE < FrameCodec::LENGTH_FIELD_SIZE + FrameCodec::MAX_FRAME_LENGTH) };
    }

    #[test]
    fn the_biggest_frames_cross_whole_and_one_byte_more_is_refused_without_closing() {
        let (host, guest) = pair();
        // a chunk-sized frame, and the biggest one a Steam message holds
        let chunk_body = vec![0x5Au8; largest_chunk_frame() - FrameCodec::LENGTH_FIELD_SIZE - FrameHeader::SIZE];
        let max_body = vec![0xA5u8; MAX_MESSAGE - FrameCodec::LENGTH_FIELD_SIZE - FrameHeader::SIZE];
        for body in [&chunk_body, &max_body] {
            host.net.send(1, &frame(18, body)).expect("host send");
            guest.net.send(&frame(18, body)).expect("guest send");
            assert_eq!(&next_frame(&guest.rx, 7).body, body);
            assert_eq!(&next_frame(&host.rx, 1).body, body);
        }
        assert_eq!(host.machine.largest_sent(), MAX_MESSAGE);

        // over Steam's limit: refused before Steam, connection untouched
        let too_big = frame(18, &vec![1u8; max_body.len() + 1]);
        for err in [host.net.send(1, &too_big).expect_err("refused"), guest.net.send(&too_big).expect_err("refused")] {
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
            assert!(err.to_string().contains("Steam's message limit"), "{err}");
        }
        // over the protocol's own limit
        let beyond_protocol = frame(18, &vec![1u8; FrameCodec::MAX_FRAME_LENGTH]);
        assert_eq!(guest.net.send(&beyond_protocol).expect_err("refused").kind(), io::ErrorKind::InvalidInput);
        guest.net.send(&frame(30, b"still open")).expect("send");
        assert_eq!(next_frame(&host.rx, 1).body, b"still open");
        quiet(&host.rx);
        quiet(&guest.rx);
    }

    /// A host with one guest; `bytes` arrive at the host's end (`to_host`) or at the guest's end as a Steam message.
    /// Returns the reason the receiving transport reports when it closes the connection.
    fn violation(bytes: &[u8], to_host: bool) -> String {
        let (host, guest) = pair();
        let (machine, handle) = if to_host {
            (&host.machine, host.machine.connections()[0])
        } else {
            (&guest.machine, guest.machine.connections()[0])
        };
        machine.inject(handle, bytes);
        // whatever follows the offence is not delivered
        machine.inject(handle, &FrameCodec::encode_frame(&frame(30, b"after")).expect("encodes"));
        let (rx, other, other_conn, conn) =
            if to_host { (&host.rx, &guest.rx, 7, 1) } else { (&guest.rx, &host.rx, 1, 7) };
        let reason = next_closed(rx, conn);
        quiet(rx);
        // the other side sees an ordinary close
        let seen = next_closed(other, other_conn);
        assert!(seen.contains("closed by the other side"), "{seen}");
        reason
    }

    #[test]
    fn a_message_that_is_not_exactly_one_frame_closes_that_connection() {
        let good = FrameCodec::encode_frame(&frame(30, b"fine")).expect("encodes");
        let mut trailing = good.clone();
        trailing.extend_from_slice(b"xx");
        let truncated = &good[..good.len() - 1];
        let mut below_header = vec![0, 0, 0, 5];
        below_header.extend_from_slice(&[1; 5]);
        let over_limit = 0x7fff_ffffu32.to_be_bytes();
        let oversize = vec![0u8; FrameCodec::MAX_FRAME_LENGTH + FrameCodec::LENGTH_FIELD_SIZE + 1];

        for to_host in [true, false] {
            assert!(violation(&trailing, to_host).contains("2 bytes follow the frame"));
            assert!(violation(truncated, to_host).contains("shorter than the frame it announces"));
            assert!(violation(&below_header, to_host).contains("shorter than a header"));
            assert!(violation(&over_limit, to_host).contains("exceeds the 1048576 byte limit"));
            assert!(violation(&[], to_host).contains("message of 0 bytes"));
            assert!(violation(&oversize, to_host).contains("exceeds the frame limit"));
        }
    }

    #[test]
    fn a_bad_message_only_closes_its_own_connection() {
        let host = host();
        let (a, b) = (guest_of(&host, 7), guest_of(&host, 8));
        for conn in [1, 2] {
            assert!(matches!(next(&host.rx), NetEvent::Opened { conn: c, .. } if c == conn));
        }
        assert!(matches!(next(&a.rx), NetEvent::Opened { .. }) && matches!(next(&b.rx), NetEvent::Opened { .. }));
        let handles = host.machine.connections();
        host.machine.inject(handles[0], b"not a frame at all");
        assert!(next_closed(&host.rx, 1).contains("exceeds the 1048576 byte limit"));
        next_closed(&a.rx, 7);
        b.net.send(&frame(30, b"still here")).expect("send");
        assert_eq!(next_frame(&host.rx, 2).body, b"still here");
        assert_eq!(host.net.connection_count(), 1);
        host.net.send(2, &frame(11, b"you too")).expect("send");
        assert_eq!(next_frame(&b.rx, 8).body, b"you too");
    }

    #[test]
    fn closing_from_the_guest_reports_both_ends_once() {
        let (host, guest) = pair();
        guest.net.close();
        guest.net.close();
        assert_eq!(next_closed(&guest.rx, 7), "closed locally");
        assert!(next_closed(&host.rx, 1).contains("closed by the other side"));
        quiet(&host.rx);
        quiet(&guest.rx);
        assert_eq!(host.net.connection_count(), 0);
        // dropping a connected client closes it too
        let again = guest_of(&host, 9);
        assert!(matches!(next(&again.rx), NetEvent::Opened { conn: 9, .. }));
        assert!(matches!(next(&host.rx), NetEvent::Opened { conn: 2, .. }));
        drop(again.net);
        assert!(next_closed(&host.rx, 2).contains("closed by the other side"));
    }

    #[test]
    fn a_frame_written_just_before_a_close_still_arrives_first() {
        let (host, guest) = pair();
        // this is the host's reject: a frame, then the close, with the wire slower than the close call
        host.machine.set_wire_delay(Duration::from_millis(40));
        host.net.send(1, &frame(3, b"bad-proof")).expect("send");
        host.net.close(1);
        assert_eq!(next_frame(&guest.rx, 7).body, b"bad-proof");
        assert!(next_closed(&guest.rx, 7).contains("closed by the other side"));
        let closes = host.machine.closes();
        assert!(closes.len() == 1 && closes[0].1, "a close of ours lingers: {closes:?}");
    }

    #[test]
    fn the_host_leaving_closes_every_client_and_stops_listening() {
        let mut host = host();
        let guests: Vec<Guest> = (0..3).map(|i| guest_of(&host, 10 + i)).collect();
        for (i, guest) in guests.iter().enumerate() {
            assert!(matches!(next(&guest.rx), NetEvent::Opened { conn, .. } if conn == 10 + i as u64));
            assert!(matches!(next(&host.rx), NetEvent::Opened { .. }));
        }
        assert_eq!(host.net.connection_count(), 3);
        host.net.shutdown();
        host.net.shutdown();
        for (i, guest) in guests.iter().enumerate() {
            let reason = next_closed(&guest.rx, 10 + i as u64);
            assert!(reason.contains("closed by the other side"), "{reason}");
            assert!(guest.net.send(&frame(30, b"x")).is_err());
        }
        let mut ended = Vec::new();
        for _ in 0..3 {
            match next(&host.rx) {
                NetEvent::Closed { conn, reason } => {
                    assert_eq!(reason, "host stopped");
                    ended.push(conn);
                }
                other => panic!("{other:?}"),
            }
        }
        ended.sort_unstable();
        assert_eq!(ended, vec![1, 2, 3]);
        assert_eq!((host.net.connection_count(), host.machine.listeners()), (0, 0));
        assert!(host.net.send(1, &frame(11, b"x")).is_err());

        // and nobody can join a host that has stopped
        let machine = FakeMachine::new();
        let core = machine.core();
        let (tx, rx) = mpsc::channel();
        let err = connect_to(&core.handle(), host.endpoint(), 5, tx).expect_err("nobody listens");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        assert!(err.to_string().contains("(5001)"), "{err}");
        quiet(&rx);
    }

    #[test]
    fn dropping_the_host_does_the_same() {
        let (host, guest) = pair();
        let Host { machine, _core, net, rx } = host;
        drop(net);
        assert!(next_closed(&guest.rx, 7).contains("closed by the other side"));
        assert_eq!(next_closed(&rx, 1), "host stopped");
        assert_eq!(machine.listeners(), 0);
    }

    #[test]
    fn eight_players_join_one_host_and_each_keeps_its_own_connection() {
        let host = host();
        let guests: Vec<Guest> = (0..8).map(|i| guest_of(&host, 100 + i)).collect();
        for (i, guest) in guests.iter().enumerate() {
            assert!(matches!(next(&guest.rx), NetEvent::Opened { conn, .. } if conn == 100 + i as u64));
        }
        // ids 1..=8 in the order the host saw them
        let mut opened = Vec::new();
        for _ in 0..8 {
            match next(&host.rx) {
                NetEvent::Opened { conn, peer: None } => opened.push(conn),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(opened, (1..=8).collect::<Vec<u64>>());
        assert_eq!(host.net.connection_count(), 8);

        // every guest says who it is; the host answers each on its own connection
        for (i, guest) in guests.iter().enumerate() {
            guest.net.send(&frame(30, format!("player {i}").as_bytes())).expect("send");
        }
        let mut who = HashMap::new();
        for _ in 0..8 {
            match next(&host.rx) {
                NetEvent::Frame { conn, frame } => {
                    who.insert(conn, String::from_utf8(frame.body).expect("text"));
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(who.len(), 8);
        for (&conn, name) in &who {
            host.net.send(conn, &frame(11, format!("hello {name}").as_bytes())).expect("send");
        }
        for (i, guest) in guests.iter().enumerate() {
            assert_eq!(next_frame(&guest.rx, 100 + i as u64).body, format!("hello player {i}").as_bytes());
        }
        // one leaves, the others carry on
        host.net.close(3);
        next_closed(&host.rx, 3);
        assert_eq!(host.net.connection_count(), 7);
        guests[0].net.send(&frame(30, b"still")).expect("send");
        assert_eq!(next_frame(&host.rx, 1).body, b"still");
    }

    #[test]
    fn a_host_turns_away_connections_over_its_limit() {
        let host = host_with(2);
        let (a, b) = (guest_of(&host, 1), guest_of(&host, 2));
        let c = guest_of(&host, 3);
        assert!(matches!(next(&a.rx), NetEvent::Opened { .. }) && matches!(next(&b.rx), NetEvent::Opened { .. }));
        // the third gets through Steam's handshake and is then closed by the host, without the session ever hearing of it
        assert!(matches!(next(&c.rx), NetEvent::Opened { conn: 3, .. }));
        assert!(next_closed(&c.rx, 3).contains("closed by the other side"));
        for conn in [1, 2] {
            assert!(matches!(next(&host.rx), NetEvent::Opened { conn: x, .. } if x == conn));
        }
        quiet(&host.rx);
        assert_eq!(host.net.connection_count(), 2);
        // a place frees up, a new player gets it
        host.net.close(1);
        next_closed(&host.rx, 1);
        let d = guest_of(&host, 4);
        assert!(matches!(next(&d.rx), NetEvent::Opened { conn: 4, .. }));
        assert!(matches!(next(&host.rx), NetEvent::Opened { conn: 3, .. }), "ids are never reused");
    }

    #[test]
    fn a_connection_that_never_finds_a_host_fails_and_says_why() {
        // nobody listening on the virtual port
        let target = FakeMachine::new();
        let _target_core = target.core();
        let machine = FakeMachine::new();
        let core = machine.core();
        let (tx, rx) = mpsc::channel();
        let err = connect_to(&core.handle(), SteamEndpoint(target.steam_id()), 1, tx.clone()).expect_err("refused");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        let text = err.to_string();
        assert!(text.contains(&format!("could not connect to Steam ID {}", target.steam_id())), "{text}");
        assert!(text.contains("(5001)"), "{text}");
        // an account Steam cannot reach
        let err = connect_to(&core.handle(), SteamEndpoint(76561198123456789), 1, tx).expect_err("timed out");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        assert!(err.to_string().contains("timed out (5003)"), "{err}");
        quiet(&rx);
        assert_eq!(core.handle().tracked(), (0, 0), "nothing is left behind");
    }

    #[test]
    fn a_connection_without_an_answer_times_out_and_reports_where_it_stood() {
        let target = FakeMachine::new();
        target.set_reachable(false);
        let _target_core = target.core();
        let machine = FakeMachine::new();
        let core = machine.core();
        let (tx, rx) = mpsc::channel();
        let mut texts: Vec<String> = Vec::new();
        let started = Instant::now();
        let err = SteamClientNet::connect_with(
            &core.handle(),
            SteamEndpoint(target.steam_id()),
            ConnectContext {
                conn: 1,
                tx,
                timeout: Duration::from_millis(250),
                cancelled: &|| false,
                progress: &mut |t| texts.push(t.to_string()),
            },
        )
        .expect_err("no answer");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= Duration::from_millis(250) && started.elapsed() < Duration::from_secs(5));
        assert!(err.to_string().contains("last status: contacting the host"), "{err}");
        assert_eq!(texts, vec![STATUS_CONTACTING.to_string()]);
        quiet(&rx);
        assert_eq!(core.handle().tracked(), (0, 0));
        assert_eq!(machine.connections().len(), 0, "the abandoned attempt is closed in Steam");
    }

    #[test]
    fn progress_follows_the_route_search_and_ends_with_the_route() {
        for (relay, last) in [(0, "connected, direct"), (0x646162, "connected, through a Steam relay")] {
            let host = host();
            host.machine.set_route_delay(Duration::from_millis(250));
            let machine = FakeMachine::new();
            machine.set_relay(relay);
            let core = machine.core();
            let (tx, rx) = mpsc::channel();
            let mut texts: Vec<String> = Vec::new();
            let net = SteamClientNet::connect_with(
                &core.handle(),
                host.endpoint(),
                ConnectContext {
                    conn: 4,
                    tx,
                    timeout: WAIT,
                    cancelled: &|| false,
                    progress: &mut |t| texts.push(t.to_string()),
                },
            )
            .expect("connect");
            assert_eq!(texts, vec![STATUS_CONTACTING.to_string(), STATUS_FINDING_ROUTE.to_string(), last.to_string()]);
            assert!(matches!(next(&rx), NetEvent::Opened { conn: 4, .. }), "Opened is the first event");
            assert_eq!(net.route(), Some(if relay == 0 { Route::Direct } else { Route::Relayed }));
        }
    }

    #[test]
    fn a_connection_attempt_can_be_cancelled() {
        let target = FakeMachine::new();
        target.set_reachable(false);
        let _target_core = target.core();
        let machine = FakeMachine::new();
        let core = machine.core();
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        let err = SteamClientNet::connect_with(
            &core.handle(),
            SteamEndpoint(target.steam_id()),
            ConnectContext {
                conn: 1,
                tx,
                timeout: Duration::from_secs(60),
                cancelled: &|| started.elapsed() > Duration::from_millis(80),
                progress: &mut |_| {},
            },
        )
        .expect_err("cancelled");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(5));
        quiet(&rx);
        assert_eq!(core.handle().tracked(), (0, 0));
    }

    #[test]
    fn steam_that_is_not_up_yet_is_waited_for_within_the_timeout() {
        let machine = FakeMachine::new();
        machine.set_ready(false);
        let core = machine.core_with(crate::steamnet::fake::fast());
        let (tx, _rx) = mpsc::channel();
        // hosting: a clear error after the wait
        let err =
            SteamHostNet::listen_on(&core.handle(), tx.clone(), Duration::from_millis(60), 8).expect_err("not ready");
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        assert!(err.to_string().contains("not ready"), "{err}");
        // joining: says it is waiting, then fails
        let mut texts: Vec<String> = Vec::new();
        let err = SteamClientNet::connect_with(
            &core.handle(),
            SteamEndpoint(76561198000000001),
            ConnectContext {
                conn: 1,
                tx,
                timeout: Duration::from_millis(60),
                cancelled: &|| false,
                progress: &mut |t| texts.push(t.to_string()),
            },
        )
        .expect_err("not ready");
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        assert_eq!(texts, vec!["waiting for Steam".to_string()]);

        // Steam comes up while a host is waiting for it
        let waiting = {
            let handle = core.handle();
            thread::spawn(move || {
                let (tx, rx) = mpsc::channel();
                SteamHostNet::listen_on(&handle, tx, WAIT, 8).map(|net| (net, rx))
            })
        };
        thread::sleep(Duration::from_millis(40));
        machine.set_ready(true);
        let (net, _rx) = waiting.join().expect("thread").expect("listens once Steam is up");
        assert_eq!(net.endpoint(), Some(SteamEndpoint(machine.steam_id())));
    }

    #[test]
    fn a_write_that_cannot_be_delivered_closes_the_connection() {
        let (host, mut guest) = pair();
        host.machine.stall_receive(true);
        guest.net.set_send_timeout(Duration::from_millis(100));
        let chunk = frame(18, &vec![3u8; 300 * 1024]);
        guest.net.send(&chunk).expect("fits into the empty buffer");
        let err = guest.net.send(&chunk).expect_err("the buffer stays full");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let reason = next_closed(&guest.rx, 7);
        assert!(reason.starts_with("write failed:"), "{reason}");
        assert!(guest.net.send(&chunk).is_err());
        host.machine.stall_receive(false);
        // the frame that did get through may still be read before the host sees the end; the end is what counts
        let reason = loop {
            match next(&host.rx) {
                NetEvent::Frame { conn: 1, .. } => {}
                NetEvent::Closed { conn: 1, reason } => break reason,
                other => panic!("unexpected {}", brief(&other)),
            }
        };
        assert!(reason.contains("closed by the other side"), "{reason}");
    }

    #[test]
    fn a_session_that_stopped_listening_ends_its_connections() {
        let (host, guest) = pair();
        let Host { machine, _core, net, rx } = host;
        drop(rx);
        guest.net.send(&frame(30, b"anyone there?")).expect("send");
        assert!(next_closed(&guest.rx, 7).contains("closed by the other side"));
        assert_eq!(net.connection_count(), 0);
        drop(net);
        assert_eq!(machine.connections().len(), 0);
    }

    #[test]
    fn the_transports_report_ping_and_route_per_connection() {
        let (host, guest) = pair();
        host.machine.set_ping(44);
        guest.machine.set_ping(61);
        guest.machine.set_relay(0x646162);
        let until = |what: &str, done: &dyn Fn() -> bool| {
            let deadline = Instant::now() + WAIT;
            while !done() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                thread::sleep(Duration::from_millis(5));
            }
        };
        until("the host's view", &|| {
            let links = host.net.links();
            links.len() == 1 && links[0].ping_ms == Some(44) && links[0].route.as_deref() == Some("direct")
        });
        until("the guest's view", &|| {
            let link = guest.net.link_info();
            link.ping_ms == Some(61) && link.route.as_deref() == Some("through a Steam relay")
        });
        // the session sees the same through the transport traits
        let as_host: &dyn HostTransport = &host.net;
        let as_guest: &dyn ClientTransport = &guest.net;
        assert_eq!(as_host.links().iter().map(|l| l.conn).collect::<Vec<_>>(), vec![1]);
        assert_eq!(as_guest.link().map(|l| l.conn), Some(7));
        assert!(as_host.describe().starts_with("Steam (Steam ID 7656119"), "{}", as_host.describe());
        assert_eq!(as_guest.describe(), format!("Steam ID {}", host.endpoint()));
        assert_eq!(as_host.local_addr(), None);
        // a closed connection is no longer listed
        host.net.close(1);
        next_closed(&host.rx, 1);
        assert!(host.net.links().is_empty());
    }

    #[test]
    fn a_dead_network_ends_both_sides_with_steams_reason() {
        let (host, guest) = pair();
        host.machine.break_connections();
        for (rx, conn) in [(&host.rx, 1), (&guest.rx, 7)] {
            let reason = next_closed(rx, conn);
            assert!(reason.contains("timed out (5003)"), "{reason}");
        }
    }
}
