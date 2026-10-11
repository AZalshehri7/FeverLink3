//! Threads that connect the bulk transfer state machines ([`super::bulk`]) to a Steam connection: a [`Hub`] that routes
//! the datagrams an owner receives to the transfer they belong to, and the two runners that drive a
//! [`Sender`] or a [`Receiver`] with the real clock and the connection's unreliable messages.
//!
//! The connection stays the owner's: the hub only takes the datagrams that start with the bulk magic byte (the core
//! hands those to [`super::Owner::datagram`] instead of `message`), and a runner only sends and reads datagrams.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver as Channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::bulk::wire::{self, reason, Offer, Packet};
use super::bulk::{Limits, Receiver, ReceiverState, Sender, SenderConfig, SenderState, SenderStats, Source};
use super::{CoreRef, Handle, SendOutcome};

/// The environment variable that switches the resync save to datagrams (`dgram`); anything else leaves it off.
pub const BULK_ENV: &str = "FEVERLINK3_BULK";
/// The capability a player announces in its join message when it can receive a save over datagrams.
pub const CAPABILITY: &str = "bulk-dgram-1";
/// Datagrams queued for one transfer before the runner reads them; beyond this they are dropped (a lost datagram is
/// recovered, memory is not unbounded).
const QUEUE: usize = 16_384;
/// A receiver gives up when nothing arrives for this long.
pub const RECEIVER_IDLE: Duration = Duration::from_secs(20);
/// The Steam send rate (bytes per second) a connection is given while a datagram transfer runs. Steam paces *all* of a
/// connection's messages with it, unreliable ones too, so it must stay above anything our controller asks for.
pub const STEAM_RATE_DURING: i32 = 64 << 20;

/// Whether `FEVERLINK3_BULK=dgram` is set.
pub fn enabled() -> bool {
    enabled_for(std::env::var(BULK_ENV).ok().as_deref())
}

pub fn enabled_for(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim().eq_ignore_ascii_case("dgram"))
}

// ---- routing

type Acceptor = Arc<dyn Fn(Handle, u32, Vec<u8>, Channel<Vec<u8>>) + Send + Sync>;

/// Routes the bulk datagrams of one owner's connections to the transfers they belong to.
#[derive(Default)]
pub struct Hub {
    endpoints: Mutex<HashMap<(Handle, u32), SyncSender<Vec<u8>>>>,
    acceptor: Mutex<Option<Acceptor>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Hub {
    pub fn new() -> Arc<Hub> {
        Arc::default()
    }

    /// What to do with an offer for a transfer nobody has registered: the acceptor gets the connection, the transfer id,
    /// the offer datagram and the channel the rest of the transfer's datagrams will arrive on.
    pub fn set_acceptor(&self, acceptor: impl Fn(Handle, u32, Vec<u8>, Channel<Vec<u8>>) + Send + Sync + 'static) {
        *lock(&self.acceptor) = Some(Arc::new(acceptor));
    }

    /// The channel of a transfer; its datagrams on `conn` are delivered there until [`Hub::unregister`].
    pub fn register(&self, conn: Handle, xfer: u32) -> Channel<Vec<u8>> {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        lock(&self.endpoints).insert((conn, xfer), tx);
        rx
    }

    pub fn unregister(&self, conn: Handle, xfer: u32) {
        lock(&self.endpoints).remove(&(conn, xfer));
    }

    /// The connection ended: every transfer on it loses its channel (its runner then stops).
    pub fn drop_connection(&self, conn: Handle) {
        lock(&self.endpoints).retain(|(c, _), _| *c != conn);
    }

    /// A datagram arrived (called on the poller thread; never blocks).
    pub fn on_datagram(&self, conn: Handle, data: &[u8]) {
        let Some((kind, xfer)) = wire::peek(data) else { return };
        let target = lock(&self.endpoints).get(&(conn, xfer)).cloned();
        if let Some(tx) = target {
            // a full queue loses the datagram, which the protocol recovers from
            let _ = tx.try_send(data.to_vec());
            return;
        }
        if kind != 1 {
            return;
        }
        let acceptor = lock(&self.acceptor).clone();
        if let Some(accept) = acceptor {
            let rx = self.register(conn, xfer);
            accept(conn, xfer, data.to_vec(), rx);
        }
    }
}

// ---- the connection

/// Sends datagrams on one Steam connection.
pub struct Port {
    pub core: CoreRef,
    pub conn: Handle,
}

impl Port {
    /// True when handed to Steam; false when Steam's buffer is full or the connection is gone.
    pub fn send(&self, datagram: &[u8]) -> bool {
        matches!(self.core.send_datagram(self.conn, datagram), SendOutcome::Sent)
    }
}

// ---- sources

/// The bytes of a file, read at any offset.
pub struct FileSource {
    file: File,
    len: u64,
}

impl FileSource {
    pub fn open(path: &PathBuf) -> io::Result<FileSource> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(FileSource { file, len })
    }
}

impl Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(out)
    }
}

/// Random-looking bytes that depend only on (id, position): the speed test's payload, which no compression shrinks.
pub struct SyntheticSource {
    pub id: u64,
    pub len: u64,
}

fn mix(mut x: u64) -> u64 {
    // splitmix64
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

impl SyntheticSource {
    pub fn fill(id: u64, offset: u64, out: &mut [u8]) {
        let mut pos = offset;
        let mut written = 0;
        while written < out.len() {
            let word = mix(id.wrapping_mul(0xD6E8_FEB8_6659_FD93) ^ (pos / 8)).to_le_bytes();
            let skip = (pos % 8) as usize;
            let take = (8 - skip).min(out.len() - written);
            out[written..written + take].copy_from_slice(&word[skip..skip + take]);
            written += take;
            pos += take as u64;
        }
    }

    /// The SHA-256 of the whole payload, streamed.
    pub fn sha256(id: u64, len: u64) -> [u8; 32] {
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut pos = 0u64;
        while pos < len {
            let n = ((len - pos) as usize).min(buf.len());
            Self::fill(id, pos, &mut buf[..n]);
            hasher.update(&buf[..n]);
            pos += n as u64;
        }
        hasher.finalize().into()
    }
}

impl Source for SyntheticSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        Self::fill(self.id, offset, out);
        Ok(())
    }
}

// ---- runners

/// What a finished send says.
#[derive(Debug, Clone)]
pub struct SendReport {
    pub state: SenderState,
    pub stats: SenderStats,
    /// What raising and restoring the connection's Steam send rate did, for the log.
    pub rate_notes: Vec<String>,
}

pub struct SendJob {
    pub xfer: u32,
    pub ctx: u32,
    pub sha256: [u8; 32],
    pub source: Box<dyn Source>,
    pub cfg: SenderConfig,
}

/// Sends a payload to the peer of `conn` and waits for the end. Blocks the calling thread (give it a thread of its
/// own). `cancelled` is asked every loop; `progress` hears the bytes acknowledged so far, about every 100 ms.
pub fn run_sender(
    core: &CoreRef,
    hub: &Hub,
    conn: Handle,
    job: SendJob,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(u64),
) -> SendReport {
    let port = Port { core: core.clone(), conn };
    let rx = hub.register(conn, job.xfer);
    let mut notes = Vec::new();
    notes.push(format!(
        "steam rate for the transfer: {}",
        core.set_connection_send_rate(conn, STEAM_RATE_DURING)
    ));
    let start = Instant::now();
    let xfer = job.xfer;
    let mut sender = match Sender::new(start, job.xfer, job.ctx, job.sha256, job.source, job.cfg) {
        Ok(s) => s,
        Err(why) => {
            hub.unregister(conn, xfer);
            return SendReport {
                state: SenderState::Failed(why),
                stats: SenderStats::default(),
                rate_notes: notes,
            };
        }
    };
    let mut last_progress = Instant::now();
    loop {
        let now = Instant::now();
        if cancelled() {
            sender.cancel(now);
        }
        if !core.is_open(conn) {
            sender.cancel(now);
            notes.push("the connection closed".into());
        }
        sender.poll(now, &mut |d| port.send(d));
        if sender.is_finished() {
            break;
        }
        if now.saturating_duration_since(last_progress) >= Duration::from_millis(100) {
            last_progress = now;
            progress(sender.acked_prefix_bytes());
        }
        match rx.recv_timeout(sender.wait_hint(now)) {
            Ok(first) => {
                let now = Instant::now();
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                    if batch.len() >= 512 {
                        break;
                    }
                }
                for d in &batch {
                    if let Some(p) = wire::parse(d) {
                        sender.on_packet(now, &p);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => sender.cancel(Instant::now()),
        }
    }
    // an abort or a last fin may be waiting to go out
    sender.poll(Instant::now(), &mut |d| port.send(d));
    hub.unregister(conn, xfer);
    progress(sender.acked_prefix_bytes());
    notes.push(format!("steam rate restored: {}", core.set_connection_send_rate(conn, core.send_rate_setting())));
    SendReport { state: sender.state().clone(), stats: sender.stats.clone(), rate_notes: notes }
}

/// What a receive runner hands to `on_complete` and returns.
#[derive(Debug, Clone)]
pub struct RecvReport {
    pub xfer: u32,
    pub ctx: u32,
    pub state: ReceiverState,
    pub bytes: u64,
    pub span_ms: u64,
    pub max_gap_ms: u64,
    pub unique: u64,
    pub duplicates: u64,
    pub acks_sent: u64,
}

fn recv_report(r: &Receiver) -> RecvReport {
    RecvReport {
        xfer: r.xfer(),
        ctx: r.ctx(),
        state: r.state(),
        bytes: r.received_bytes(),
        span_ms: r.span_ms(),
        max_gap_ms: r.max_gap_ms(),
        unique: r.stats.unique,
        duplicates: r.stats.duplicates,
        acks_sent: r.stats.acks_sent,
    }
}

/// Receives the transfer whose offer is `offer`; `rx` is the channel the hub registered for it. `on_complete` is
/// called once, as soon as every datagram is in and the hash has been checked, with the receiver (take the data from
/// it). The runner then goes on answering the sender for a few seconds. `progress` hears the bytes received so far.
/// `None` when the offer was refused.
pub fn run_receiver(
    core: &CoreRef,
    hub: &Hub,
    conn: Handle,
    offer: &[u8],
    rx: Channel<Vec<u8>>,
    limits: &Limits,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(u64),
    on_complete: &mut dyn FnMut(&mut Receiver),
) -> Option<RecvReport> {
    let port = Port { core: core.clone(), conn };
    let Some(Packet::Offer(parsed)) = wire::parse(offer) else { return None };
    let xfer = parsed.xfer;
    let start = Instant::now();
    let mut receiver = match Receiver::accept(start, &parsed, limits) {
        Ok(r) => r,
        Err(code) => {
            port.send(&wire::encode_abort(xfer, code));
            hub.unregister(conn, xfer);
            return None;
        }
    };
    let mut completed = false;
    let mut last_input = Instant::now();
    let mut last_progress = Instant::now();
    let mut out = Vec::new();
    loop {
        let now = Instant::now();
        if cancelled() {
            receiver.abort(reason::CANCELLED);
        }
        if !completed && now.saturating_duration_since(last_input) >= RECEIVER_IDLE {
            receiver.abort(reason::TIMEOUT);
        }
        receiver.poll(now, &mut out);
        for d in out.drain(..) {
            port.send(&d);
        }
        if matches!(receiver.state(), ReceiverState::Complete { .. }) && !completed {
            completed = true;
            on_complete(&mut receiver);
            progress(receiver.received_bytes());
        }
        if receiver.is_over(now) || !core.is_open(conn) {
            break;
        }
        if now.saturating_duration_since(last_progress) >= Duration::from_millis(100) {
            last_progress = now;
            progress(receiver.received_bytes());
        }
        match rx.recv_timeout(receiver.wait_hint(now).clamp(Duration::from_micros(200), Duration::from_millis(50))) {
            Ok(first) => {
                let now = Instant::now();
                last_input = now;
                let mut batch = vec![first];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                    if batch.len() >= 512 {
                        break;
                    }
                }
                for d in &batch {
                    if let Some(p) = wire::parse(d) {
                        receiver.on_packet(now, &p);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    receiver.poll(Instant::now(), &mut out);
    for d in out.drain(..) {
        port.send(&d);
    }
    hub.unregister(conn, xfer);
    let report = recv_report(&receiver);
    if !completed {
        // nothing was delivered; the caller learns it from the report's state
        return Some(report);
    }
    Some(report)
}

/// The offer a sender would make, for tests and for a receiver's checks.
pub fn offer_of(sha256: [u8; 32], total: u64, xfer: u32, ctx: u32) -> Offer {
    Offer { xfer, ts: 0, ctx, total, payload: wire::PAYLOAD as u16, sha256 }
}
