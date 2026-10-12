//! The sending engine of a parallel transfer.
//!
//! One *lane* is one Steam connection to the receiver's [`super::PAR_PORT`]. Every lane has a thread that, while the
//! lane has less than `inflight` unacknowledged bytes, takes the next chunk (first the ones requeued from lanes that
//! died, then the next new index), reads it from the source and sends it. Because a lane only takes a chunk when it
//! has room, the chunks go to whichever connection is emptiest (work stealing). The calling thread runs the control
//! loop: it feeds [`super::growth::Growth`] with the acknowledged bytes and the control connection's ping and
//! queue, opens or closes lanes as it says, and ends when the receiver reports `Done`, something fails, or nothing
//! moves for [`super::ParConfig::stall`].

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use super::growth::{Growth, GrowthCfg, Step};
use super::wire::{self, reason, Hello, Msg};
use super::{ParConfig, PAR_PORT};
use crate::steamnet::bulk_io::SyntheticSource;
use crate::steamnet::{CoreRef, End, Handle, Owner};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---- sources

/// Bytes that can be read at any offset from several threads.
#[allow(clippy::len_without_is_empty)]
pub trait PayloadSource: Send + Sync {
    fn len(&self) -> u64;
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()>;
}

/// A file, read with positional reads (no shared file position).
pub struct FileSource {
    file: File,
    len: u64,
}

impl FileSource {
    pub fn open(path: &std::path::Path) -> io::Result<FileSource> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(FileSource { file, len })
    }
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
pub(crate) fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}

#[cfg(unix)]
pub(crate) fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}

pub(crate) fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = read_at(file, buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the file is shorter than announced"));
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

pub(crate) fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = write_at(file, buf, offset)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "wrote nothing"));
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

impl PayloadSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        read_exact_at(&self.file, out, offset)
    }
}

/// The speed test's payload: random-looking bytes that depend only on (id, position).
pub struct SyntheticPayload {
    pub id: u64,
    pub len: u64,
}

impl PayloadSource for SyntheticPayload {
    fn len(&self) -> u64 {
        self.len
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        SyntheticSource::fill(self.id, offset, out);
        Ok(())
    }
}

/// What the engine reads about the session's own connection to the peer.
pub trait Control: Send + Sync {
    /// Steam's round-trip estimate in ms.
    fn ping_ms(&self) -> Option<u32>;
    /// Reliable bytes queued and not yet on the wire.
    fn pending(&self) -> u32;
}

/// The ping and queue of one Steam connection of a core, read from the core's periodic measurement.
pub struct CoreControl {
    pub core: CoreRef,
    pub handle: Handle,
}

impl Control for CoreControl {
    fn ping_ms(&self) -> Option<u32> {
        self.core.link(self.handle).and_then(|l| l.ping_ms)
    }

    fn pending(&self) -> u32 {
        self.core.link(self.handle).map_or(0, |l| l.pending)
    }
}

// ---- the job

pub struct SendSpec {
    /// The receiver's Steam ID.
    pub remote: u64,
    pub xfer: u32,
    pub ctx: u32,
    pub secret: [u8; 32],
    pub sha256: [u8; 32],
    pub source: Arc<dyn PayloadSource>,
    pub cfg: ParConfig,
    /// The connections wanted in all ([`ParConfig::conns_for`]).
    pub conns: u32,
    pub control: Option<Arc<dyn Control>>,
}

/// What the progress callback hears, about every 250 ms.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// Bytes the receiver has acknowledged.
    pub bytes: u64,
    pub total: u64,
    /// Connections working now.
    pub lanes: usize,
    /// Bytes per second over the last 2 s.
    pub rate: f64,
}

#[derive(Debug, Clone)]
pub struct SendReport {
    pub ok: bool,
    /// Why it failed.
    pub error: Option<String>,
    /// The connections wanted, the most that worked at once, how many opened in all, how many worked at the end.
    pub target: u32,
    pub lanes_max: usize,
    pub lanes_opened: usize,
    pub lanes_final: usize,
    pub bytes: u64,
    pub secs: f64,
    /// Bytes per second over the whole transfer.
    pub rate: f64,
    /// Why the connections stopped growing (or that they did not).
    pub stop: String,
    /// Chunks that were sent again because their connection died.
    pub requeued: u64,
    /// The most unacknowledged bytes on all connections together at any moment (the sender's memory in flight).
    pub peak_inflight: usize,
    pub notes: Vec<String>,
}

impl SendReport {
    /// One line for the log and the window.
    pub fn summary(&self) -> String {
        format!(
            "{} connection(s) (wanted {}, {} at most, {} opened), {:.2} MB/s, {:.1} s; {}",
            self.lanes_final.max(self.lanes_max),
            self.target,
            self.lanes_max,
            self.lanes_opened,
            self.rate / 1e6,
            self.secs,
            self.stop
        )
    }
}

// ---- state shared by the lanes

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LaneState {
    Connecting,
    Hello,
    Ready,
    Dead,
}

struct Lane {
    idx: u16,
    conn: AtomicU32,
    state: Mutex<LaneState>,
    cv: Condvar,
    inflight: Mutex<HashMap<u32, usize>>,
    inflight_bytes: AtomicUsize,
    why: Mutex<Option<String>>,
    ever_ready: AtomicBool,
}

impl Lane {
    fn new(idx: u16) -> Arc<Lane> {
        Arc::new(Lane {
            idx,
            conn: AtomicU32::new(0),
            state: Mutex::new(LaneState::Connecting),
            cv: Condvar::new(),
            inflight: Mutex::new(HashMap::new()),
            inflight_bytes: AtomicUsize::new(0),
            why: Mutex::new(None),
            ever_ready: AtomicBool::new(false),
        })
    }

    fn state(&self) -> LaneState {
        *lock(&self.state)
    }

    fn set(&self, s: LaneState) {
        let mut st = lock(&self.state);
        if *st != LaneState::Dead {
            *st = s;
            if s == LaneState::Ready {
                self.ever_ready.store(true, Ordering::Release);
            }
        }
        self.cv.notify_all();
    }
}

#[derive(Default)]
struct Queue {
    requeue: VecDeque<u32>,
    next: u32,
}

enum Outcome {
    Done(bool),
    Fatal(String),
}

struct Shared {
    core: CoreRef,
    cfg: ParConfig,
    xfer: u32,
    count: u32,
    total: u64,
    chunk: usize,
    source: Arc<dyn PayloadSource>,
    hello: Hello,
    q: Mutex<Queue>,
    qcv: Condvar,
    acked: Vec<AtomicBool>,
    acked_count: AtomicU32,
    acked_bytes: AtomicU64,
    requeued: AtomicU64,
    outcome: Mutex<Option<Outcome>>,
    finished: AtomicBool,
    last_move: Mutex<Instant>,
    lanes: Mutex<Vec<Arc<Lane>>>,
    notes: Mutex<Vec<String>>,
}

impl Shared {
    fn note(&self, text: String) {
        let mut n = lock(&self.notes);
        if n.len() < 200 {
            n.push(text);
        }
    }

    fn moved(&self) {
        *lock(&self.last_move) = Instant::now();
    }

    fn set_outcome(&self, o: Outcome) {
        let mut slot = lock(&self.outcome);
        // the first word wins, except that a failure never overrides a success
        if slot.is_none() {
            *slot = Some(o);
        }
        self.qcv.notify_all();
    }

    /// The next chunk a lane should send: a requeued one (not acknowledged meanwhile) or the next new one.
    fn next_chunk(&self) -> Option<u32> {
        let mut q = lock(&self.q);
        while let Some(i) = q.requeue.pop_front() {
            if !self.acked[i as usize].load(Ordering::Acquire) {
                return Some(i);
            }
        }
        if q.next < self.count {
            q.next += 1;
            return Some(q.next - 1);
        }
        None
    }

    fn mark_acked(&self, index: u32, len: usize) {
        if let Some(flag) = self.acked.get(index as usize) {
            if !flag.swap(true, Ordering::AcqRel) {
                self.acked_count.fetch_add(1, Ordering::AcqRel);
                self.acked_bytes.fetch_add(len as u64, Ordering::AcqRel);
                self.moved();
            }
        }
    }

    /// A lane is over: its unacknowledged chunks go back to the queue; its connection is closed (what was already
    /// sent still arrives).
    fn kill(&self, lane: &Lane, why: &str) {
        {
            let mut st = lock(&lane.state);
            if *st == LaneState::Dead {
                return;
            }
            *st = LaneState::Dead;
        }
        lock(&lane.why).get_or_insert_with(|| why.to_owned());
        let pending: Vec<u32> = {
            let mut inflight = lock(&lane.inflight);
            let v = inflight.keys().copied().collect();
            inflight.clear();
            v
        };
        lane.inflight_bytes.store(0, Ordering::Release);
        if !pending.is_empty() && !self.finished.load(Ordering::Acquire) {
            let mut q = lock(&self.q);
            let mut n = 0;
            for i in pending {
                if !self.acked[i as usize].load(Ordering::Acquire) {
                    q.requeue.push_front(i);
                    n += 1;
                }
            }
            self.requeued.fetch_add(n, Ordering::AcqRel);
        }
        lane.cv.notify_all();
        self.qcv.notify_all();
        let conn = lane.conn.load(Ordering::Acquire);
        if conn != 0 {
            self.core.close(conn);
        }
        if !self.finished.load(Ordering::Acquire) {
            self.note(format!("connection {} ended: {why}", lane.idx + 1));
        }
    }

    fn ready_lanes(&self) -> usize {
        lock(&self.lanes).iter().filter(|l| l.state() == LaneState::Ready).count()
    }

    fn alive_lanes(&self) -> usize {
        lock(&self.lanes).iter().filter(|l| l.state() != LaneState::Dead).count()
    }
}

struct LaneOwner {
    shared: Arc<Shared>,
    lane: Arc<Lane>,
}

impl Owner for LaneOwner {
    fn opened(&self, conn: Handle, _incoming: bool, _remote: Option<u64>) {
        self.lane.conn.store(conn, Ordering::Release);
        if self.lane.state() == LaneState::Dead {
            self.shared.core.close(conn);
            return;
        }
        let note = self.shared.core.set_connection_send_rate(conn, self.shared.cfg.per_conn_rate);
        if self.lane.idx == 0 {
            self.shared
                .note(format!("send rate of a bulk connection set to {} B/s: {note}", self.shared.cfg.per_conn_rate));
        }
        let hello = Hello { lane: self.lane.idx, ..self.shared.hello.clone() };
        match self.shared.core.send_reliable(conn, &wire::encode_hello(&hello), Duration::from_secs(5)) {
            Ok(()) => self.lane.set(LaneState::Hello),
            Err(e) => self.shared.kill(&self.lane, &format!("cannot send the hello: {e}")),
        }
    }

    fn message(&self, _conn: Handle, data: &[u8]) {
        let Some(msg) = wire::parse(data) else { return };
        let s = &self.shared;
        match msg {
            Msg::Accept { xfer } if xfer == s.xfer => self.lane.set(LaneState::Ready),
            Msg::Refuse { xfer, reason: code } if xfer == s.xfer => {
                let text = format!("the receiver refused the transfer: {}", wire::reason_text(code));
                s.kill(&self.lane, &text);
                // a refusal of the transfer itself (not of this one connection) ends all of it
                if matches!(
                    code,
                    reason::BAD_TOKEN
                        | reason::NOT_EXPECTED
                        | reason::TOO_BIG
                        | reason::BAD_FIELDS
                        | reason::MISMATCH
                        | reason::BUSY
                ) {
                    s.set_outcome(Outcome::Fatal(text));
                }
            }
            Msg::Ack { xfer, indices } if xfer == s.xfer => {
                let mut freed = 0usize;
                {
                    let mut inflight = lock(&self.lane.inflight);
                    for i in &indices {
                        if let Some(len) = inflight.remove(i) {
                            freed += len;
                        }
                    }
                }
                self.lane
                    .inflight_bytes
                    .fetch_sub(freed.min(self.lane.inflight_bytes.load(Ordering::Acquire)), Ordering::AcqRel);
                for i in indices {
                    if let Some(len) = wire::chunk_len(s.total, s.chunk as u32, i) {
                        s.mark_acked(i, len);
                    }
                }
                self.lane.cv.notify_all();
            }
            Msg::Done { xfer, ok } if xfer == s.xfer => s.set_outcome(Outcome::Done(ok)),
            Msg::Abort { xfer, reason: code } if xfer == s.xfer => {
                s.set_outcome(Outcome::Fatal(format!("the receiver aborted: {}", wire::reason_text(code))));
            }
            _ => {}
        }
    }

    fn closed(&self, _conn: Handle, end: End) {
        self.shared.kill(&self.lane, &end.describe());
    }
}

fn lane_worker(shared: Arc<Shared>, lane: Arc<Lane>) {
    // wait for the handshake
    let started = Instant::now();
    loop {
        match lane.state() {
            LaneState::Ready => break,
            LaneState::Dead => return,
            _ => {}
        }
        if shared.finished.load(Ordering::Acquire) {
            return;
        }
        if started.elapsed() > shared.cfg.connect_timeout {
            shared.kill(&lane, &format!("no answer within {} s", shared.cfg.connect_timeout.as_secs()));
            return;
        }
        let st = lock(&lane.state);
        let _ = lane.cv.wait_timeout(st, Duration::from_millis(20));
    }
    shared.moved();
    while !shared.finished.load(Ordering::Acquire) && lane.state() == LaneState::Ready {
        if lane.inflight_bytes.load(Ordering::Acquire) >= shared.cfg.inflight {
            let st = lock(&lane.state);
            let _ = lane.cv.wait_timeout(st, Duration::from_millis(10));
            continue;
        }
        let Some(index) = shared.next_chunk() else {
            let q = lock(&shared.q);
            let _ = shared.qcv.wait_timeout(q, Duration::from_millis(20));
            continue;
        };
        let Some(len) = wire::chunk_len(shared.total, shared.chunk as u32, index) else { continue };
        let mut buf = wire::chunk_buffer(shared.xfer, index, shared.count, len);
        if let Err(e) = shared.source.read(u64::from(index) * shared.chunk as u64, &mut buf[wire::CHUNK_HEADER..]) {
            shared.set_outcome(Outcome::Fatal(format!("cannot read the file: {e}")));
            return;
        }
        lock(&lane.inflight).insert(index, len);
        lane.inflight_bytes.fetch_add(len, Ordering::AcqRel);
        let conn = lane.conn.load(Ordering::Acquire);
        if let Err(e) = shared.core.send_reliable(conn, &buf, Duration::from_secs(15)) {
            shared.kill(&lane, &format!("send failed: {e}"));
            return;
        }
    }
}

/// Sends the payload to `spec.remote` over parallel connections and returns when it is over. Blocks the calling
/// thread. `cancelled` is polled; `progress` hears about every 250 ms.
pub fn run_send(
    core: &CoreRef,
    spec: SendSpec,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(&Progress),
) -> SendReport {
    let total = spec.source.len();
    let chunk = spec.cfg.chunk.clamp(4096, 256 * 1024);
    let target = spec.conns.clamp(1, spec.cfg.max_conns.max(1));
    let started = Instant::now();
    let mut report = SendReport {
        ok: false,
        error: None,
        target,
        lanes_max: 0,
        lanes_opened: 0,
        lanes_final: 0,
        bytes: 0,
        secs: 0.0,
        rate: 0.0,
        stop: String::new(),
        requeued: 0,
        peak_inflight: 0,
        notes: Vec::new(),
    };
    let Some(count) = wire::chunk_count(total, chunk as u32) else {
        report.error = Some("nothing to send".into());
        return report;
    };
    let shared = Arc::new(Shared {
        core: core.clone(),
        cfg: spec.cfg.clone(),
        xfer: spec.xfer,
        count,
        total,
        chunk,
        source: spec.source.clone(),
        hello: Hello::new(&spec.secret, spec.xfer, spec.ctx, total, chunk as u32, count, spec.sha256, 0),
        q: Mutex::new(Queue::default()),
        qcv: Condvar::new(),
        acked: (0..count).map(|_| AtomicBool::new(false)).collect(),
        acked_count: AtomicU32::new(0),
        acked_bytes: AtomicU64::new(0),
        requeued: AtomicU64::new(0),
        outcome: Mutex::new(None),
        finished: AtomicBool::new(false),
        last_move: Mutex::new(Instant::now()),
        lanes: Mutex::new(Vec::new()),
        notes: Mutex::new(Vec::new()),
    });
    let mut growth = Growth::new(GrowthCfg {
        rate: f64::from(spec.cfg.per_conn_rate),
        target: target as usize,
        initial: (spec.cfg.initial.max(1)).min(target) as usize,
        forced: spec.cfg.forced.is_some(),
        gain_fraction: spec.cfg.gain_fraction,
        open_every: spec.cfg.open_every,
        settle: 2.0,
    });
    let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
    let mut next_idx: u16 = 0;
    let mut open = |shared: &Arc<Shared>, workers: &mut Vec<thread::JoinHandle<()>>| -> bool {
        if next_idx >= 64 {
            return false;
        }
        let lane = Lane::new(next_idx);
        next_idx += 1;
        lock(&shared.lanes).push(Arc::clone(&lane));
        let owner = Arc::new(LaneOwner { shared: Arc::clone(shared), lane: Arc::clone(&lane) });
        match shared.core.connect(spec.remote, PAR_PORT, owner) {
            Ok(h) => lane.conn.store(h, Ordering::Release),
            Err(e) => {
                shared.kill(&lane, &format!("cannot open a connection: {e}"));
                return false;
            }
        }
        let (s, l) = (Arc::clone(shared), Arc::clone(&lane));
        match thread::Builder::new().name("feverlink3-par-lane".into()).spawn(move || lane_worker(s, l)) {
            Ok(h) => {
                workers.push(h);
                true
            }
            Err(e) => {
                shared.kill(&lane, &format!("cannot start a thread: {e}"));
                false
            }
        }
    };
    let initial = growth.config().initial;
    for _ in 0..initial {
        open(&shared, &mut workers);
    }
    report.lanes_opened = initial;

    let mut last_progress_call = Instant::now() - Duration::from_secs(1);
    let mut first_ready_noted = false;
    let mut failure: Option<String> = None;
    let mut done_ok = false;
    loop {
        let now = Instant::now();
        let t = now.duration_since(started).as_secs_f64();
        if cancelled() {
            failure = Some("cancelled".into());
            break;
        }
        match lock(&shared.outcome).take() {
            Some(Outcome::Done(true)) => {
                done_ok = true;
                break;
            }
            Some(Outcome::Done(false)) => {
                failure = Some("the receiver's SHA-256 of the whole file did not match".into());
                break;
            }
            Some(Outcome::Fatal(why)) => {
                failure = Some(why);
                break;
            }
            None => {}
        }
        let ready = shared.ready_lanes();
        let alive = shared.alive_lanes();
        report.lanes_max = report.lanes_max.max(ready);
        let inflight: usize = lock(&shared.lanes).iter().map(|l| l.inflight_bytes.load(Ordering::Acquire)).sum();
        report.peak_inflight = report.peak_inflight.max(inflight);
        if ready > 0 && !first_ready_noted {
            first_ready_noted = true;
            growth.ready(t);
            shared.moved();
        }
        if !first_ready_noted && now.duration_since(started) > spec.cfg.connect_timeout {
            failure = Some(format!(
                "no extra Steam connection opened within {} s{}",
                spec.cfg.connect_timeout.as_secs(),
                lane_causes(&shared)
            ));
            break;
        }
        if alive == 0 {
            failure = Some(if first_ready_noted {
                format!("every connection was lost{}", lane_causes(&shared))
            } else {
                format!("could not open any extra Steam connection{}", lane_causes(&shared))
            });
            break;
        }
        if first_ready_noted && lock(&shared.last_move).elapsed() > spec.cfg.stall {
            failure = Some(format!("no progress for {} s", spec.cfg.stall.as_secs()));
            break;
        }
        let acked = shared.acked_bytes.load(Ordering::Acquire);
        let (ping, pending) = match &spec.control {
            Some(c) => (c.ping_ms(), c.pending()),
            None => (None, 0),
        };
        growth.record(t, acked, ping, pending);
        // all chunks are in: no need to grow; the receiver is checking the file
        if shared.acked_count.load(Ordering::Acquire) < count {
            match growth.decide(t, alive, ready) {
                Step::Open => {
                    growth.note_opened(t);
                    report.lanes_opened += 1;
                    open(&shared, &mut workers);
                }
                Step::CloseNewest => {
                    let newest = lock(&shared.lanes).iter().rev().find(|l| l.state() != LaneState::Dead).cloned();
                    if let Some(lane) = newest {
                        shared.kill(&lane, "closed because the Steam ping rose");
                    }
                }
                Step::Hold => {}
            }
        }
        if now.duration_since(last_progress_call) >= Duration::from_millis(250) {
            last_progress_call = now;
            progress(&Progress { bytes: acked, total, lanes: ready, rate: growth.agg(t, 2.0) });
        }
        thread::sleep(Duration::from_millis(15));
    }
    // end
    let secs = started.elapsed().as_secs_f64().max(0.001);
    report.lanes_final = shared.ready_lanes();
    report.bytes = if done_ok { total } else { shared.acked_bytes.load(Ordering::Acquire) };
    report.secs = secs;
    report.rate = report.bytes as f64 / secs;
    report.requeued = shared.requeued.load(Ordering::Acquire);
    report.stop = match growth.stopped() {
        Some(why) => format!("stopped adding connections: {why}"),
        None if report.lanes_opened as u32 >= target => format!("all {target} connections were opened"),
        None => {
            format!("the transfer ended with {} of {target} connections opened (no need for more)", report.lanes_opened)
        }
    };
    shared.finished.store(true, Ordering::Release);
    shared.qcv.notify_all();
    let lanes: Vec<Arc<Lane>> = lock(&shared.lanes).clone();
    for lane in &lanes {
        // after a clean end the receiver closes too; a close lingers, so nothing in flight is lost
        shared.kill(lane, "transfer over");
    }
    for w in workers {
        let _ = w.join();
    }
    report.notes = lock(&shared.notes).clone();
    report.notes.extend(growth.notes.iter().cloned());
    report.ok = done_ok;
    report.error = failure;
    progress(&Progress { bytes: report.bytes, total, lanes: report.lanes_final, rate: report.rate });
    report
}

/// Why the lanes ended, for an error message: "(connection 1: ...; connection 2: ...)".
fn lane_causes(shared: &Shared) -> String {
    let lanes = lock(&shared.lanes);
    let mut causes: Vec<String> = Vec::new();
    for l in lanes.iter() {
        if let Some(why) = lock(&l.why).clone() {
            if !causes.contains(&why) {
                causes.push(why);
            }
        }
    }
    if causes.is_empty() {
        String::new()
    } else {
        format!(" ({})", causes.join("; "))
    }
}
