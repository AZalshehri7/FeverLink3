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

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use feverlink3_core::protocol::{Frame, FrameCodec};
use feverlink3_core::session::ConnectionId;

/// How long [`ClientNet::connect`] waits for each endpoint.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one socket write may wait for the peer to read before the connection is given up.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

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

/// What a connection attempt is given besides its target, for transports where finding the other side can take
/// long (a Steam route through the relays takes 10 to 20 s): where its events go, how long it may take, a way to
/// ask it to stop, and a way for it to say what it is doing ("finding a route"). TCP ignores the last two.
pub struct ConnectContext<'a> {
    /// The id the connection's events carry.
    pub conn: ConnectionId,
    pub tx: Sender<NetEvent>,
    pub timeout: Duration,
    /// Polled while waiting; true means give up now.
    pub cancelled: &'a dyn Fn() -> bool,
    /// Called with a short status text each time it changes.
    pub progress: &'a mut dyn FnMut(&str),
}

/// What a transport measured about one connection, for the window.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkInfo {
    pub conn: ConnectionId,
    /// Round trip in ms as the transport measures it; `None` when it does not (yet).
    pub ping_ms: Option<u32>,
    /// How the connection travels (`direct`, `through a Steam relay`); `None` when the transport does not say.
    /// Steam can move a relayed connection to a direct path later, so this is re-measured.
    pub route: Option<String>,
}

/// What the host session needs from a listening transport. Its events come back on the channel the transport was
/// built with, as [`NetEvent`]s; the rules at the top of this file (a connection's `Closed` exactly once and last,
/// no frames after `close`) hold for every implementation. [`HostNet`] is TCP,
/// [`crate::steam_transport::SteamHostNet`] is Steam.
pub trait HostTransport: Send + 'static {
    /// Writes a frame to one connection. An error means the connection is gone or was just closed because of this
    /// write, or the frame cannot be sent at all.
    fn send(&self, conn: ConnectionId, frame: &Frame) -> io::Result<()>;
    /// Closes one connection. Frames already written are delivered first; `Closed` follows.
    fn close(&self, conn: ConnectionId);
    /// The socket address it listens on, for transports that have one.
    fn local_addr(&self) -> Option<SocketAddr> {
        None
    }
    /// Where it listens, for the log.
    fn describe(&self) -> String;
    /// What it measured about each open connection (ping, route); empty for a transport that measures nothing.
    fn links(&self) -> Vec<LinkInfo> {
        Vec::new()
    }
}

/// What a player session needs from its connection to the host ([`ClientNet`] or
/// [`crate::steam_transport::SteamClientNet`]).
pub trait ClientTransport: Send + 'static {
    fn send(&self, frame: &Frame) -> io::Result<()>;
    /// Where it is connected to, for the log.
    fn describe(&self) -> String;
    /// What it measured about the connection to the host (ping, route), `None` for a transport that measures nothing.
    fn link(&self) -> Option<LinkInfo> {
        None
    }
}

/// One TCP connection: its writer, a handle to shut it down, and the flag that makes closing idempotent.
#[derive(Debug)]
struct Link {
    writer: Mutex<BufWriter<TcpStream>>,
    stream: TcpStream,
    closed: AtomicBool,
}

impl Link {
    fn new(stream: TcpStream) -> io::Result<Arc<Link>> {
        stream.set_nodelay(true)?;
        // One peer that stops reading must not freeze the host thread for ever: a write that makes no progress for
        // this long fails and the connection closes. Generous, so a big savegame streamed to a slow link (each
        // write is at most a chunk of about 350 KB) is not cut off.
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let writer = Mutex::new(BufWriter::new(stream.try_clone()?));
        Ok(Arc::new(Link { writer, stream, closed: AtomicBool::new(false) }))
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

/// Reads frames until the connection ends, then reports `Closed`. Runs on its own thread.
fn read_loop(conn: ConnectionId, link: &Link, tx: &Sender<NetEvent>) {
    let mut reader = BufReader::new(match link.stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(NetEvent::Closed { conn, reason: format!("cannot read: {e}") });
            return;
        }
    });
    let reason = loop {
        match FrameCodec::read(&mut reader) {
            Ok(Some(frame)) => {
                if link.closed.load(Ordering::Acquire) {
                    break "closed locally".to_string();
                }
                if tx.send(NetEvent::Frame { conn, frame }).is_err() {
                    break "session ended".to_string();
                }
            }
            Ok(None) => break "closed by the peer".to_string(),
            Err(e) => {
                break if link.closed.load(Ordering::Acquire) { "closed locally".to_string() } else { e.to_string() };
            }
        }
    };
    link.close();
    let _ = tx.send(NetEvent::Closed { conn, reason });
}

fn spawn_reader(conn: ConnectionId, link: Arc<Link>, tx: Sender<NetEvent>, role: &str) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name(format!("feverlink3-{role}-read-{conn}")).spawn(move || read_loop(conn, &link, &tx))
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
}

impl HostNet {
    /// Binds `addr` (port 0 picks a free one, see [`HostNet::local_addr`]) and starts accepting. Connection ids
    /// start at 1 and are never reused. Events go to `tx`.
    pub fn bind(addr: SocketAddr, tx: Sender<NetEvent>) -> io::Result<HostNet> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let links: Arc<Mutex<HashMap<ConnectionId, Arc<Link>>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let (links, stop) = (Arc::clone(&links), Arc::clone(&stop));
            thread::Builder::new()
                .name("feverlink3-accept".into())
                .spawn(move || accept_loop(&listener, &links, &stop, &tx))?
        };
        Ok(HostNet { local_addr, links, stop, accept: Some(accept) })
    }

    /// The address actually bound.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
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

impl HostTransport for HostNet {
    fn send(&self, conn: ConnectionId, frame: &Frame) -> io::Result<()> {
        HostNet::send(self, conn, frame)
    }

    fn close(&self, conn: ConnectionId) {
        HostNet::close(self, conn);
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        Some(self.local_addr)
    }

    fn describe(&self) -> String {
        self.local_addr.to_string()
    }
}

fn accept_loop(
    listener: &TcpListener,
    links: &Mutex<HashMap<ConnectionId, Arc<Link>>>,
    stop: &AtomicBool,
    tx: &Sender<NetEvent>,
) {
    let next_id = AtomicU64::new(1);
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, peer)) => {
                // accepted sockets inherit non-blocking mode on some platforms
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let Ok(link) = Link::new(stream) else { continue };
                let conn = next_id.fetch_add(1, Ordering::Relaxed);
                {
                    let mut map = links.lock().unwrap_or_else(|p| p.into_inner());
                    map.retain(|_, l| !l.closed.load(Ordering::Acquire));
                    map.insert(conn, Arc::clone(&link));
                }
                if tx.send(NetEvent::Opened { conn, peer: Some(peer) }).is_err() {
                    return;
                }
                if spawn_reader(conn, link, tx.clone(), "host").is_err() {
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
                    let link = Link::new(stream)?;
                    tx.send(NetEvent::Opened { conn, peer: Some(ep) })
                        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "session ended"))?;
                    spawn_reader(conn, Arc::clone(&link), tx, "client")?;
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

impl ClientTransport for ClientNet {
    fn send(&self, frame: &Frame) -> io::Result<()> {
        ClientNet::send(self, frame)
    }

    fn describe(&self) -> String {
        self.peer.to_string()
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
    fn lan_endpoints_always_yield_something_with_the_port() {
        let eps = lan_endpoints(28090);
        assert!(!eps.is_empty() && eps.len() <= 8);
        assert!(eps.iter().all(|e| e.port() == 28090 && e.is_ipv4()));
    }

    #[test]
    fn connect_tries_endpoints_in_order_and_reports_failure() {
        let (tx, _rx) = mpsc::channel();
        // A dead address that stays dead: the port is held by a listener for the whole test (a port that was only
        // freed could be taken by a test running in parallel), and the address is another loopback address than the
        // one the listener is bound to, where nothing listens.
        let holder = TcpListener::bind(loopback()).unwrap();
        let dead = SocketAddr::from((Ipv4Addr::new(127, 0, 0, 2), holder.local_addr().unwrap().port()));
        assert!(ClientNet::connect(&[dead], 1, tx.clone()).is_err());
        assert!(ClientNet::connect(&[], 1, tx.clone()).is_err());
        let (htx, _hrx) = mpsc::channel();
        let host = HostNet::bind(loopback(), htx).unwrap();
        let ok = ClientNet::connect(&[dead, host.local_addr()], 2, tx).unwrap();
        assert_eq!(ok.peer(), host.local_addr());
        drop(holder);
    }
}
