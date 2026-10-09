//! TCP transport: `std::net` and threads, no async runtime.
//!
//! Both ends turn sockets into [`NetEvent`]s on an `mpsc` channel so that exactly one thread (the session thread,
//! [`crate::session`]) owns the protocol state machine and sees every event in one order.
//!
//! - [`HostNet`] listens on one thread, numbers each accepted connection, and runs one reader thread per
//!   connection. Frames are read with [`FrameCodec`]; writes go through a per-connection `Mutex<BufWriter>`, so any
//!   thread may send and two frames never interleave.
//! - [`ClientNet`] connects to the endpoints of a join code in order, 5 s each, and runs one reader thread.
//!
//! A connection ends when the peer closes it, a read or write fails, the codec rejects what it reads (length
//! outside the protocol's bounds, truncated frame), or the owner calls `close`. Whatever the cause, the reader
//! thread reports [`NetEvent::Closed`] exactly once, last. After `close` or a failed write no more frames are
//! delivered for that connection.
//!
//! A host listens on a port the whole internet can reach, so its limits ([`NetLimits`]) are what strangers cost:
//! connections are capped in total and per address, the first frame of a connection is read with a small size
//! limit (a stranger's only valid first frame is a Join), and a write that makes no progress ends the connection
//! instead of stopping the session thread that does the writing. A stranger that opens a connection and says
//! nothing is closed by the session itself (`HostSession::HANDSHAKE_SECONDS`).

use std::collections::HashMap;
use std::io::{self, BufWriter, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use feverlink3_core::protocol::{Frame, FrameCodec, ProtocolError};
use feverlink3_core::session::ConnectionId;

/// How long [`ClientNet::connect`] waits for each endpoint.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// What a host spends on connections it has not been asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetLimits {
    /// A write that cannot make progress for this long closes the connection. The session thread does all the
    /// writing, so a peer that stops reading must not be able to hold it.
    pub write_timeout: Duration,
    /// Connections open at once, handshakes included. The game has eight seats; the rest is room for a rejoin that
    /// overlaps its dead connection and for strangers knocking.
    pub max_connections: usize,
    /// Connections one address may hold open. Friends behind one router share an address, so this is about seats.
    pub max_per_address: usize,
    /// Longest first frame (header included) a connection may send before it has sent one: the only frame a
    /// stranger can usefully send is a Join of a few hundred bytes.
    pub first_frame: usize,
    /// Frames per second one connection may deliver, on average (twice that in a burst); 0 = no limit. A peer that
    /// sends more is read more slowly, and TCP slows its sender in turn: a flood costs the flooder its own bandwidth
    /// and leaves the session thread, and the queue in front of it, alone. A real player sends tens per second.
    pub frames_per_second: u32,
    /// The same for bytes per second (0 = no limit).
    pub bytes_per_second: u32,
}

impl Default for NetLimits {
    fn default() -> Self {
        Self {
            write_timeout: Duration::from_secs(10),
            max_connections: 64,
            max_per_address: 16,
            first_frame: FrameCodec::HANDSHAKE_MAX_LENGTH,
            frames_per_second: 1000,
            bytes_per_second: 4 << 20,
        }
    }
}

/// How the reader of one connection treats what arrives: the part of [`NetLimits`] a reader needs.
#[derive(Debug, Clone, Copy)]
struct ReadPolicy {
    first_frame: usize,
    frames_per_second: u32,
    bytes_per_second: u32,
}

impl ReadPolicy {
    fn of(limits: &NetLimits) -> Self {
        Self {
            first_frame: limits.first_frame,
            frames_per_second: limits.frames_per_second,
            bytes_per_second: limits.bytes_per_second,
        }
    }

    /// For a connection to a host: what a host sends is the protocol's business (a savegame is megabytes).
    fn unlimited() -> Self {
        Self { first_frame: FrameCodec::MAX_FRAME_LENGTH, frames_per_second: 0, bytes_per_second: 0 }
    }
}

/// Token bucket over frames and bytes. [`Throttle::admit`] says how long to wait before the next frame is passed on.
#[derive(Debug)]
struct Throttle {
    frames: f64,
    bytes: f64,
    frame_rate: f64,
    byte_rate: f64,
    last: Instant,
}

impl Throttle {
    /// The longest a single frame is made to wait: a throttled peer is slowed, not frozen.
    const MAX_WAIT: Duration = Duration::from_secs(1);

    fn new(policy: &ReadPolicy, now: Instant) -> Option<Throttle> {
        if policy.frames_per_second == 0 && policy.bytes_per_second == 0 {
            return None;
        }
        let (frame_rate, byte_rate) = (f64::from(policy.frames_per_second), f64::from(policy.bytes_per_second));
        // a full bucket holds two seconds of the rate
        Some(Throttle { frames: 2.0 * frame_rate, bytes: 2.0 * byte_rate, frame_rate, byte_rate, last: now })
    }

    fn admit(&mut self, frame_bytes: usize, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        let mut wait = 0.0f64;
        if self.frame_rate > 0.0 {
            self.frames = (self.frames + elapsed * self.frame_rate).min(2.0 * self.frame_rate) - 1.0;
            if self.frames < 0.0 {
                wait = wait.max(-self.frames / self.frame_rate);
            }
        }
        if self.byte_rate > 0.0 {
            self.bytes = (self.bytes + elapsed * self.byte_rate).min(2.0 * self.byte_rate) - frame_bytes as f64;
            if self.bytes < 0.0 {
                wait = wait.max(-self.bytes / self.byte_rate);
            }
        }
        Duration::from_secs_f64(wait).min(Self::MAX_WAIT)
    }
}

/// What the sockets tell the session thread.
#[derive(Debug)]
pub enum NetEvent {
    /// A connection exists (host: accepted; client: connected). `peer` is the remote address when known.
    Opened { conn: ConnectionId, peer: Option<SocketAddr> },
    /// One complete frame arrived.
    Frame { conn: ConnectionId, frame: Frame },
    /// The connection is over; `reason` says why. The last event of a connection.
    Closed { conn: ConnectionId, reason: String },
}

/// One TCP connection: its writer, a handle to shut it down, and the flag that makes closing idempotent.
#[derive(Debug)]
struct Link {
    writer: Mutex<BufWriter<TcpStream>>,
    stream: TcpStream,
    closed: AtomicBool,
    /// The peer's address, for the per-address limit.
    ip: Option<IpAddr>,
}

impl Link {
    fn new(stream: TcpStream, write_timeout: Duration) -> io::Result<Arc<Link>> {
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(write_timeout))?;
        let ip = stream.peer_addr().ok().map(|a| a.ip());
        let writer = Mutex::new(BufWriter::new(stream.try_clone()?));
        Ok(Arc::new(Link { writer, stream, closed: AtomicBool::new(false), ip }))
    }

    /// Encodes and writes one frame, flushed. Any failure closes the connection and is returned.
    fn send(&self, frame: &Frame) -> io::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "connection closed"));
        }
        let bytes =
            FrameCodec::encode_frame(frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let result = {
            let mut w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
            w.write_all(&bytes).and_then(|()| w.flush())
        };
        if result.is_err() {
            self.close();
        }
        result
    }

    /// Shuts the socket down in both directions; the reader thread then ends and reports `Closed`.
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let _ = self.stream.shutdown(Shutdown::Both);
        }
    }
}

/// How often a blocked read wakes to look at the `closed` flag. Shutting a socket down from another thread does not
/// reliably wake a read on Windows, and a peer that says nothing and never closes would hold the reader thread (and
/// its socket) for ever, so every reader polls.
const READ_POLL: Duration = Duration::from_millis(250);

/// Reads frames until the connection ends, then reports `Closed`. Runs on its own thread. The first frame may be at
/// most `policy.first_frame` bytes long; after it the protocol's own limit applies; frames are passed on no faster
/// than the policy's rates.
///
/// The bytes of a frame are collected as they arrive and the length field is checked as soon as it has, so what the
/// reader holds is what the peer really sent (never what it announced), and a read that times out loses nothing.
fn read_loop(conn: ConnectionId, link: &Link, tx: &Sender<NetEvent>, policy: ReadPolicy) {
    let mut stream = match link.stream.try_clone().and_then(|s| s.set_read_timeout(Some(READ_POLL)).map(|()| s)) {
        Ok(s) => s,
        Err(e) => {
            link.close();
            let _ = tx.send(NetEvent::Closed { conn, reason: format!("cannot read: {e}") });
            return;
        }
    };
    let (mut buf, mut chunk) = (Vec::new(), vec![0u8; 16 * 1024]);
    let mut limit = policy.first_frame;
    let mut throttle = Throttle::new(&policy, Instant::now());
    let reason = 'read: loop {
        // every frame the buffer holds already, in order
        let mut start = 0;
        loop {
            match FrameCodec::try_decode_limited(buf.get(start..).unwrap_or_default(), limit) {
                Ok(Some((frame, used))) => {
                    start += used;
                    limit = FrameCodec::MAX_FRAME_LENGTH;
                    if let Some(throttle) = throttle.as_mut() {
                        // while it waits it reads nothing: the sender is slowed by TCP, not by a growing queue
                        let mut wait = throttle.admit(used, Instant::now());
                        while !wait.is_zero() && !link.closed.load(Ordering::Acquire) {
                            let step = wait.min(READ_POLL);
                            thread::sleep(step);
                            wait -= step;
                        }
                    }
                    if link.closed.load(Ordering::Acquire) {
                        break 'read "closed locally".to_string();
                    }
                    if tx.send(NetEvent::Frame { conn, frame }).is_err() {
                        break 'read "session ended".to_string();
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    break 'read if link.closed.load(Ordering::Acquire) {
                        "closed locally".to_string()
                    } else {
                        e.to_string()
                    }
                }
            }
        }
        buf.drain(..start.min(buf.len()));
        if link.closed.load(Ordering::Acquire) {
            break "closed locally".to_string();
        }
        match stream.read(&mut chunk) {
            Ok(0) if buf.is_empty() => break "closed by the peer".to_string(),
            Ok(0) => break ProtocolError::Truncated.to_string(),
            Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => {
                break if link.closed.load(Ordering::Acquire) { "closed locally".to_string() } else { e.to_string() }
            }
        }
    };
    link.close();
    let _ = tx.send(NetEvent::Closed { conn, reason });
}

fn spawn_reader(
    conn: ConnectionId,
    link: Arc<Link>,
    tx: Sender<NetEvent>,
    role: &str,
    policy: ReadPolicy,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("feverlink3-{role}-read-{conn}"))
        .spawn(move || read_loop(conn, &link, &tx, policy))
}

// ---------------------------------------------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------------------------------------------

/// The listening side. Dropping it stops accepting and closes every connection.
#[derive(Debug)]
pub struct HostNet {
    local_addr: SocketAddr,
    links: Arc<Mutex<HashMap<ConnectionId, Arc<Link>>>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    refused: Arc<AtomicU64>,
}

impl HostNet {
    /// Binds `addr` (port 0 picks a free one, see [`HostNet::local_addr`]) and starts accepting, within the default
    /// [`NetLimits`]. Connection ids start at 1 and are never reused. Events go to `tx`.
    pub fn bind(addr: SocketAddr, tx: Sender<NetEvent>) -> io::Result<HostNet> {
        Self::bind_with(addr, tx, NetLimits::default())
    }

    /// As [`HostNet::bind`] with other limits.
    pub fn bind_with(addr: SocketAddr, tx: Sender<NetEvent>, limits: NetLimits) -> io::Result<HostNet> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let links: Arc<Mutex<HashMap<ConnectionId, Arc<Link>>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let refused = Arc::new(AtomicU64::new(0));
        let accept = {
            let (links, stop, refused) = (Arc::clone(&links), Arc::clone(&stop), Arc::clone(&refused));
            thread::Builder::new()
                .name("feverlink3-accept".into())
                .spawn(move || accept_loop(&listener, &links, &stop, &tx, limits, &refused))?
        };
        Ok(HostNet { local_addr, links, stop, accept: Some(accept), refused })
    }

    /// The address actually bound.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Connections turned away so far because a limit of [`NetLimits`] was reached.
    pub fn refused_count(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// Writes a frame to one connection. An error means the connection is gone or was just closed because of
    /// this write; its `Closed` event follows.
    pub fn send(&self, conn: ConnectionId, frame: &Frame) -> io::Result<()> {
        let link = self.links.lock().unwrap_or_else(|p| p.into_inner()).get(&conn).cloned();
        match link {
            Some(link) => link.send(frame),
            None => Err(io::Error::new(io::ErrorKind::NotConnected, format!("no connection {conn}"))),
        }
    }

    /// Closes one connection. Frames already written are delivered first; `Closed` follows.
    pub fn close(&self, conn: ConnectionId) {
        if let Some(link) = self.links.lock().unwrap_or_else(|p| p.into_inner()).get(&conn) {
            link.close();
        }
    }

    /// Number of connections that are open now.
    pub fn connection_count(&self) -> usize {
        self.links
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter(|l| !l.closed.load(Ordering::Acquire))
            .count()
    }

    /// Stops accepting and closes every connection. Idempotent; also done on drop.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
        for link in self.links.lock().unwrap_or_else(|p| p.into_inner()).values() {
            link.close();
        }
    }
}

impl Drop for HostNet {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(
    listener: &TcpListener,
    links: &Mutex<HashMap<ConnectionId, Arc<Link>>>,
    stop: &AtomicBool,
    tx: &Sender<NetEvent>,
    limits: NetLimits,
    refused: &AtomicU64,
) {
    let next_id = AtomicU64::new(1);
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, peer)) => {
                // accepted sockets inherit non-blocking mode on some platforms
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let admitted = {
                    let mut map = links.lock().unwrap_or_else(|p| p.into_inner());
                    map.retain(|_, l| !l.closed.load(Ordering::Acquire));
                    let same_address = map.values().filter(|l| l.ip == Some(peer.ip())).count();
                    map.len() < limits.max_connections && same_address < limits.max_per_address
                };
                if !admitted {
                    // dropping the stream closes it: no thread, no table entry, nothing for the session to see
                    refused.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let Ok(link) = Link::new(stream, limits.write_timeout) else { continue };
                let conn = next_id.fetch_add(1, Ordering::Relaxed);
                {
                    let mut map = links.lock().unwrap_or_else(|p| p.into_inner());
                    map.insert(conn, Arc::clone(&link));
                }
                if tx.send(NetEvent::Opened { conn, peer: Some(peer) }).is_err() {
                    return;
                }
                if spawn_reader(conn, link, tx.clone(), "host", ReadPolicy::of(&limits)).is_err() {
                    let _ = tx.send(NetEvent::Closed { conn, reason: "cannot start reader thread".into() });
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(20)),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------------------------------------

/// The connecting side. Dropping it closes the connection.
#[derive(Debug)]
pub struct ClientNet {
    conn: ConnectionId,
    peer: SocketAddr,
    link: Arc<Link>,
}

impl ClientNet {
    /// Connects to the first endpoint that answers within [`CONNECT_TIMEOUT`], trying them in order, and starts
    /// the reader. `conn` is the id its events carry (a session numbers its attempts so a late event of an old
    /// connection is recognisable). The first event is `Opened`.
    pub fn connect(endpoints: &[SocketAddr], conn: ConnectionId, tx: Sender<NetEvent>) -> io::Result<ClientNet> {
        Self::connect_with_timeout(endpoints, conn, tx, CONNECT_TIMEOUT)
    }

    /// As [`ClientNet::connect`] with another per-endpoint timeout.
    pub fn connect_with_timeout(
        endpoints: &[SocketAddr],
        conn: ConnectionId,
        tx: Sender<NetEvent>,
        timeout: Duration,
    ) -> io::Result<ClientNet> {
        let mut last = io::Error::new(io::ErrorKind::InvalidInput, "the join code holds no endpoint");
        for &ep in endpoints {
            match TcpStream::connect_timeout(&ep, timeout) {
                Ok(stream) => {
                    let link = Link::new(stream, NetLimits::default().write_timeout)?;
                    tx.send(NetEvent::Opened { conn, peer: Some(ep) })
                        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "session ended"))?;
                    spawn_reader(conn, Arc::clone(&link), tx, "client", ReadPolicy::unlimited())?;
                    return Ok(ClientNet { conn, peer: ep, link });
                }
                Err(e) => last = io::Error::new(e.kind(), format!("{ep}: {e}")),
            }
        }
        Err(last)
    }

    pub fn conn(&self) -> ConnectionId {
        self.conn
    }

    /// The endpoint that answered.
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn send(&self, frame: &Frame) -> io::Result<()> {
        self.link.send(frame)
    }

    pub fn close(&self) {
        self.link.close();
    }
}

impl Drop for ClientNet {
    fn drop(&mut self) {
        self.link.close();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Local addresses
// ---------------------------------------------------------------------------------------------------------------

/// The addresses a host puts in its join code: this machine's LAN IPv4 addresses with `port`, at most
/// [`feverlink3_core::lobby::JoinCode::MAX_ENDPOINTS`]. No public-IP lookup (that needs a third party); a player
/// across the internet needs the host's forwarded address added by hand. Falls back to loopback when the machine
/// has no usable address, so the code is still valid on one PC.
///
/// The primary address comes from asking the OS which interface it would use towards a documentation-range
/// address (a connected UDP socket sends nothing); the rest from resolving the machine's host name.
pub fn lan_endpoints(port: u16) -> Vec<SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs, UdpSocket};

    let usable = |ip: &IpAddr| match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified() && !v4.is_multicast(),
        IpAddr::V6(_) => false,
    };
    let mut ips: Vec<IpAddr> = Vec::new();
    if let Some(ip) = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").and_then(|()| s.local_addr()))
        .ok()
        .map(|a| a.ip())
    {
        ips.push(ip);
    }
    let host = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default();
    if !host.is_empty() {
        if let Ok(resolved) = (host.as_str(), port).to_socket_addrs() {
            ips.extend(resolved.map(|a| a.ip()));
        }
    }
    let mut out: Vec<SocketAddr> = Vec::new();
    for ip in ips.into_iter().filter(usable) {
        let ep = SocketAddr::new(ip, port);
        if !out.contains(&ep) && out.len() < feverlink3_core::lobby::JoinCode::MAX_ENDPOINTS {
            out.push(ep);
        }
    }
    if out.is_empty() {
        out.push(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use feverlink3_core::protocol::FrameHeader;
    use std::io::Read;
    use std::net::Ipv4Addr;
    use std::sync::mpsc;

    const WAIT: Duration = Duration::from_secs(5);

    fn loopback() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
    }

    fn frame(kind: u8, body: &[u8]) -> Frame {
        Frame::new(FrameHeader::new(1, kind, 3, 9), body.to_vec())
    }

    #[test]
    fn frames_flow_both_ways_and_close_is_reported_once() {
        let (htx, hrx) = mpsc::channel();
        let (ctx, crx) = mpsc::channel();
        let host = HostNet::bind(loopback(), htx).unwrap();
        let client = ClientNet::connect(&[host.local_addr()], 7, ctx).unwrap();
        assert!(matches!(crx.recv_timeout(WAIT).unwrap(), NetEvent::Opened { conn: 7, .. }));
        let NetEvent::Opened { conn, .. } = hrx.recv_timeout(WAIT).unwrap() else { panic!("opened") };
        assert_eq!(conn, 1);

        client.send(&frame(30, b"hello")).unwrap();
        match hrx.recv_timeout(WAIT).unwrap() {
            NetEvent::Frame { conn: 1, frame: f } => {
                assert_eq!(f.body, b"hello");
                assert_eq!(f.header.kind, 30);
            }
            other => panic!("{other:?}"),
        }
        host.send(1, &frame(11, b"state")).unwrap();
        match crx.recv_timeout(WAIT).unwrap() {
            NetEvent::Frame { conn: 7, frame: f } => assert_eq!(f.body, b"state"),
            other => panic!("{other:?}"),
        }

        host.close(1);
        assert!(matches!(hrx.recv_timeout(WAIT).unwrap(), NetEvent::Closed { conn: 1, .. }));
        assert!(matches!(crx.recv_timeout(WAIT).unwrap(), NetEvent::Closed { conn: 7, .. }));
        assert!(hrx.recv_timeout(Duration::from_millis(200)).is_err(), "closed is reported once");
        assert!(host.send(1, &frame(11, b"late")).is_err());
        assert!(client.send(&frame(30, b"late")).is_err());
    }

    #[test]
    fn a_frame_the_codec_rejects_closes_only_that_connection() {
        let (htx, hrx) = mpsc::channel();
        let host = HostNet::bind(loopback(), htx).unwrap();
        let mut bad = TcpStream::connect(host.local_addr()).unwrap();
        let (ctx, _crx) = mpsc::channel();
        let good = ClientNet::connect(&[host.local_addr()], 1, ctx).unwrap();
        let (mut bad_id, mut good_id) = (0, 0);
        for _ in 0..2 {
            let NetEvent::Opened { conn, peer } = hrx.recv_timeout(WAIT).unwrap() else { panic!("opened") };
            if peer.unwrap() == bad.local_addr().unwrap() {
                bad_id = conn;
            } else {
                good_id = conn;
            }
        }
        // a length of 0x41414141 is far over the limit
        let _ = bad.write_all(&vec![b'A'; 2 << 20]);
        match hrx.recv_timeout(WAIT).unwrap() {
            NetEvent::Closed { conn, reason } => {
                assert_eq!(conn, bad_id);
                assert!(reason.contains("exceeds"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        let mut buf = [0u8; 1];
        assert!(matches!(bad.read(&mut buf), Ok(0) | Err(_)), "the offender sees the close");
        good.send(&frame(30, b"still here")).unwrap();
        match hrx.recv_timeout(WAIT).unwrap() {
            NetEvent::Frame { conn, .. } => assert_eq!(conn, good_id),
            other => panic!("{other:?}"),
        }
        assert_eq!(host.connection_count(), 1);
    }

    #[test]
    fn the_first_frame_is_read_with_the_small_limit_and_later_ones_with_the_big_one() {
        let (htx, hrx) = mpsc::channel();
        let host = HostNet::bind(loopback(), htx).unwrap();

        // a stranger announcing a first frame of 5000 bytes is closed before the host reads it
        let mut stranger = TcpStream::connect(host.local_addr()).unwrap();
        let NetEvent::Opened { conn: stranger_id, .. } = hrx.recv_timeout(WAIT).unwrap() else { panic!("opened") };
        stranger.write_all(&5000u32.to_be_bytes()).unwrap();
        match hrx.recv_timeout(WAIT).unwrap() {
            NetEvent::Closed { conn, reason } => {
                assert_eq!(conn, stranger_id);
                assert!(reason.contains("exceeds"), "{reason}");
            }
            other => panic!("{other:?}"),
        }

        // a peer whose first frame is small may then send big ones
        let (ctx, _crx) = mpsc::channel();
        let client = ClientNet::connect(&[host.local_addr()], 1, ctx).unwrap();
        let NetEvent::Opened { conn: client_id, .. } = hrx.recv_timeout(WAIT).unwrap() else { panic!("opened") };
        client.send(&frame(2, b"join")).unwrap();
        client.send(&frame(30, &vec![7u8; 200_000])).unwrap();
        for expected in [4usize, 200_000] {
            match hrx.recv_timeout(WAIT).unwrap() {
                NetEvent::Frame { conn, frame } => assert_eq!((conn, frame.body.len()), (client_id, expected)),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn connections_over_the_limits_are_turned_away_without_a_trace() {
        let limits = NetLimits { max_connections: 3, max_per_address: 2, ..NetLimits::default() };
        let (htx, hrx) = mpsc::channel();
        let host = HostNet::bind_with(loopback(), htx, limits).unwrap();
        let first = TcpStream::connect(host.local_addr()).unwrap();
        let second = TcpStream::connect(host.local_addr()).unwrap();
        for _ in 0..2 {
            assert!(matches!(hrx.recv_timeout(WAIT).unwrap(), NetEvent::Opened { .. }));
        }
        // the same address a third time: refused, and the refused socket sees the close
        let mut third = TcpStream::connect(host.local_addr()).unwrap();
        third.set_read_timeout(Some(WAIT)).unwrap();
        let mut buf = [0u8; 1];
        assert!(matches!(third.read(&mut buf), Ok(0) | Err(_)));
        assert_eq!(host.refused_count(), 1);
        assert!(hrx.recv_timeout(Duration::from_millis(200)).is_err(), "no event for a refused connection");
        assert_eq!(host.connection_count(), 2);
        // a closed connection makes room again
        drop(first);
        drop(second);
        let deadline = std::time::Instant::now() + WAIT;
        while host.connection_count() > 0 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let _again = TcpStream::connect(host.local_addr()).unwrap();
        loop {
            match hrx.recv_timeout(WAIT).unwrap() {
                NetEvent::Opened { .. } => break,
                NetEvent::Closed { .. } => {}
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(host.refused_count(), 1);
    }

    #[test]
    fn a_peer_that_stops_reading_is_cut_off_instead_of_stopping_the_writer() {
        let limits = NetLimits { write_timeout: Duration::from_millis(300), ..NetLimits::default() };
        let (htx, hrx) = mpsc::channel();
        let host = HostNet::bind_with(loopback(), htx, limits).unwrap();
        let _silent = TcpStream::connect(host.local_addr()).unwrap(); // never reads
        let NetEvent::Opened { conn, .. } = hrx.recv_timeout(WAIT).unwrap() else { panic!("opened") };

        let big = frame(11, &vec![1u8; 900_000]);
        let started = std::time::Instant::now();
        let mut longest = Duration::ZERO;
        let mut failed = false;
        for _ in 0..400 {
            let before = std::time::Instant::now();
            if host.send(conn, &big).is_err() {
                failed = true;
                longest = before.elapsed();
                break;
            }
        }
        assert!(failed, "buffers are finite: the writes must end in an error");
        assert!(longest < Duration::from_secs(5), "the failing write took {longest:?}");
        assert!(started.elapsed() < Duration::from_secs(30));
        // the connection was closed by the failed write
        loop {
            match hrx.recv_timeout(WAIT).unwrap() {
                NetEvent::Closed { conn: c, .. } => {
                    assert_eq!(c, conn);
                    break;
                }
                NetEvent::Frame { .. } => {}
                other => panic!("{other:?}"),
            }
        }
        assert!(host.send(conn, &big).is_err());
    }

    #[test]
    fn the_throttle_lets_a_burst_through_then_spaces_frames_and_forgives_idle_time() {
        let t0 = Instant::now();
        let policy = ReadPolicy { first_frame: 4096, frames_per_second: 100, bytes_per_second: 0 };
        let mut throttle = Throttle::new(&policy, t0).unwrap();
        // two seconds' worth is free
        for _ in 0..200 {
            assert_eq!(throttle.admit(50, t0), Duration::ZERO);
        }
        // then each frame waits its turn, a hundredth of a second after the one before
        assert!((throttle.admit(50, t0).as_secs_f64() - 0.01).abs() < 1e-9);
        assert!((throttle.admit(50, t0).as_secs_f64() - 0.02).abs() < 1e-9);
        // time passes: the debt is paid and the bucket refills, to two seconds' worth at most
        let later = t0 + Duration::from_secs(60);
        for _ in 0..200 {
            assert_eq!(throttle.admit(50, later), Duration::ZERO);
        }
        assert!(throttle.admit(50, later) > Duration::ZERO);

        // bytes: a bucket of 2000 at 1000 per second; a frame of 1500 bytes is free once, then costs a second
        let policy = ReadPolicy { first_frame: 4096, frames_per_second: 0, bytes_per_second: 1000 };
        let mut throttle = Throttle::new(&policy, t0).unwrap();
        assert_eq!(throttle.admit(1500, t0), Duration::ZERO);
        assert!((throttle.admit(1500, t0).as_secs_f64() - 1.0).abs() < 1e-9);
        // and nothing waits longer than the cap, whatever the debt
        assert_eq!(throttle.admit(1 << 20, t0), Throttle::MAX_WAIT);
        assert!(Throttle::new(&ReadPolicy::unlimited(), t0).is_none());
    }

    #[test]
    fn a_peer_that_sends_faster_than_the_limit_is_slowed_not_dropped() {
        let limits = NetLimits { frames_per_second: 50, bytes_per_second: 0, ..NetLimits::default() };
        let (htx, hrx) = mpsc::channel();
        let host = HostNet::bind_with(loopback(), htx, limits).unwrap();
        let mut raw = TcpStream::connect(host.local_addr()).unwrap();
        let NetEvent::Opened { conn, .. } = hrx.recv_timeout(WAIT).unwrap() else { panic!("opened") };
        let one = FrameCodec::encode_frame(&frame(30, b"x")).unwrap();
        let started = Instant::now();
        // the whole flood in one write: 160 frames, of which 100 are the burst
        raw.write_all(&one.repeat(160)).unwrap();
        for n in 1..=160 {
            match hrx.recv_timeout(WAIT).unwrap() {
                NetEvent::Frame { conn: c, .. } => assert_eq!(c, conn, "frame {n}"),
                other => panic!("frame {n}: {other:?}"),
            }
        }
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(900), "60 frames over the burst at 50 a second take a second: {took:?}");
        assert!(took < Duration::from_secs(4), "{took:?}");
        // a polite peer on the same host is not held up
        let (ctx, _crx) = mpsc::channel();
        let polite = ClientNet::connect(&[host.local_addr()], 9, ctx).unwrap();
        let NetEvent::Opened { conn: polite_id, .. } = hrx.recv_timeout(WAIT).unwrap() else { panic!("opened") };
        polite.send(&frame(30, b"hi")).unwrap();
        match hrx.recv_timeout(Duration::from_millis(500)).unwrap() {
            NetEvent::Frame { conn, .. } => assert_eq!(conn, polite_id),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn lan_endpoints_always_yield_something_with_the_port() {
        let eps = lan_endpoints(28090);
        assert!(!eps.is_empty() && eps.len() <= 8);
        assert!(eps.iter().all(|e| e.port() == 28090 && e.is_ipv4()));
    }

    #[test]
    fn connect_tries_endpoints_in_order_and_reports_failure() {
        let (tx, _rx) = mpsc::channel();
        let dead = {
            let l = TcpListener::bind(loopback()).unwrap();
            l.local_addr().unwrap()
        };
        assert!(ClientNet::connect(&[dead], 1, tx.clone()).is_err());
        assert!(ClientNet::connect(&[], 1, tx.clone()).is_err());
        let (htx, _hrx) = mpsc::channel();
        let host = HostNet::bind(loopback(), htx).unwrap();
        let ok = ClientNet::connect(&[dead, host.local_addr()], 2, tx).unwrap();
        assert_eq!(ok.peer(), host.local_addr());
    }
}
