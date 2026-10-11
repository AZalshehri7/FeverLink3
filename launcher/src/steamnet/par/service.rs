//! The receiving side of parallel transfers: one listen socket per process on [`super::PAR_PORT`], the lanes that
//! arrive on it, and a writer thread per transfer that puts every chunk at its offset in the file.
//!
//! What a stranger can do: connect (it is closed at once unless its Steam ID was allowed with [`ParService::allow`]),
//! and then send a `Hello`, which must carry the right token for the allowed secret and sizes within the allowance;
//! chunks are checked against the hello (transfer id, count, index, exact length) and a connection that breaks the
//! rules is closed. Nothing is held in memory but the chunks queued for the writer (at most 64).

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::send::{read_exact_at, write_all_at};
use super::wire::{self, reason, Hello, Msg};
use super::{PAR_PORT, STALL};
use crate::steamnet::{CoreRef, End, Handle, Owner};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The most connections one transfer takes.
pub const MAX_LANES: usize = 32;
/// Chunks waiting for the writer thread.
const QUEUE: usize = 64;
/// A connection that has not said hello after this long is closed.
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// A finished transfer's connections are closed after this long if the sender has not closed them.
const LINGER: Duration = Duration::from_secs(5);

/// What the owner of an allowance hears.
#[derive(Debug, Clone, PartialEq)]
pub enum ParEvent {
    Progress {
        ctx: u32,
        bytes: u64,
        total: u64,
        lanes: usize,
        rate: f64,
    },
    /// The whole file is in, its SHA-256 matched and it is at `path`.
    Done {
        ctx: u32,
        sha256: [u8; 32],
        len: u64,
        path: PathBuf,
        secs: f64,
        lanes_max: usize,
    },
    Failed {
        ctx: u32,
        why: String,
    },
}

/// Who may send what, and where it goes.
pub struct Allow {
    /// The per-transfer secret every hello must prove; `None`: the peer's (Steam-authenticated) ID is enough.
    pub secret: Option<[u8; 32]>,
    /// The folder of the finished file, and its name from the transfer's `ctx`.
    pub dir: PathBuf,
    pub stem: Box<dyn Fn(u32) -> String + Send + Sync>,
    pub ext: &'static str,
    /// The largest file taken.
    pub max_total: u64,
    pub on_event: Box<dyn Fn(ParEvent) + Send + Sync>,
    /// Who set the allowance up (a connection id), so that an old owner going away does not withdraw a newer one.
    pub owner: u64,
}

const RUNNING: u8 = 0;
const DONE: u8 = 1;
const FAILED: u8 = 2;

enum Job {
    Chunk {
        lane: Handle,
        index: u32,
        data: Vec<u8>,
    },
    /// A chunk that was already in: only acknowledge it (so the sender's window opens).
    Dup {
        lane: Handle,
        index: u32,
    },
}

struct Xfer {
    id: u32,
    ctx: u32,
    total: u64,
    count: u32,
    chunk: u32,
    sha256: [u8; 32],
    allow: Arc<Allow>,
    part: PathBuf,
    fin: PathBuf,
    seen: Mutex<Vec<u64>>,
    tx: Mutex<Option<SyncSender<Job>>>,
    lanes: Mutex<Vec<Handle>>,
    started: Instant,
    last_in: Mutex<Instant>,
    finished_at: Mutex<Option<Instant>>,
    bytes_in: AtomicU64,
    chunks_in: AtomicU64,
    duplicates: AtomicU64,
    lanes_max: AtomicUsize,
    state: AtomicU8,
}

impl Xfer {
    fn matches(&self, h: &Hello) -> bool {
        self.id == h.xfer
            && self.ctx == h.ctx
            && self.total == h.total
            && self.count == h.count
            && self.chunk == h.chunk
            && self.sha256 == h.sha256
    }

    fn running(&self) -> bool {
        self.state.load(Ordering::Acquire) == RUNNING
    }

    /// Marks an index as seen; true when it was new.
    fn first_time(&self, index: u32) -> bool {
        let mut seen = lock(&self.seen);
        let (word, bit) = (index as usize / 64, index % 64);
        let Some(w) = seen.get_mut(word) else { return false };
        let new = *w & (1 << bit) == 0;
        *w |= 1 << bit;
        new
    }
}

struct RxLane {
    conn: Handle,
    remote: u64,
    since: Instant,
    xfer: Mutex<Option<Arc<Xfer>>>,
}

struct Inner {
    core: CoreRef,
    allow: Mutex<HashMap<u64, Vec<Arc<Allow>>>>,
    lanes: Mutex<HashMap<Handle, Arc<RxLane>>>,
    xfers: Mutex<HashMap<u64, Arc<Xfer>>>,
    stop: AtomicBool,
    me: Weak<Inner>,
    /// Chunk bytes waiting for a writer now, and the most there ever were (a memory check for the tests).
    queued: AtomicU64,
    peak_queued: AtomicU64,
}

/// The listener and everything that arrives on it. One per core ([`ParService::for_core`]).
pub struct ParService {
    inner: Arc<Inner>,
    listener: Handle,
}

static SERVICES: Mutex<Vec<(usize, Weak<ParService>)>> = Mutex::new(Vec::new());

impl ParService {
    /// The service of this core, started on first use. Fails when Steam would not open the listen socket.
    pub fn for_core(core: &CoreRef) -> io::Result<Arc<ParService>> {
        let mut services = lock(&SERVICES);
        services.retain(|(_, w)| w.strong_count() > 0);
        if let Some(existing) = services.iter().find(|(id, _)| *id == core.id()).and_then(|(_, w)| w.upgrade()) {
            return Ok(existing);
        }
        let inner = Arc::new_cyclic(|me| Inner {
            core: core.clone(),
            allow: Mutex::new(HashMap::new()),
            lanes: Mutex::new(HashMap::new()),
            xfers: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
            me: me.clone(),
            queued: AtomicU64::new(0),
            peak_queued: AtomicU64::new(0),
        });
        let owner: Arc<dyn Owner> = inner.clone();
        let listener = core.listen(PAR_PORT, owner)?;
        let service = Arc::new(ParService { inner: Arc::clone(&inner), listener });
        services.push((core.id(), Arc::downgrade(&service)));
        let weak = Arc::downgrade(&inner);
        let _ = thread::Builder::new().name("feverlink3-par-keeper".into()).spawn(move || keeper(weak));
        Ok(service)
    }

    /// Lets `remote` send transfers. An account can have several allowances (a session's and a speed test's, each
    /// with its own secret); a later one by the same `owner` replaces the earlier one.
    pub fn allow(&self, remote: u64, allow: Allow) {
        let mut all = lock(&self.inner.allow);
        let list = all.entry(remote).or_default();
        list.retain(|a| a.owner != allow.owner);
        list.push(Arc::new(allow));
    }

    /// Withdraws the allowance of `owner` (0: whoever's it is); its running transfer fails and its connections close.
    pub fn disallow(&self, remote: u64, owner: u64) {
        let left = {
            let mut all = lock(&self.inner.allow);
            let Some(list) = all.get_mut(&remote) else { return };
            list.retain(|a| owner != 0 && a.owner != owner);
            let left = list.len();
            if left == 0 {
                all.remove(&remote);
            }
            left
        };
        // a transfer that came in under the withdrawn allowance fails
        let doomed = {
            let mut xfers = lock(&self.inner.xfers);
            match xfers.get(&remote) {
                Some(x) if owner == 0 || x.allow.owner == owner => xfers.remove(&remote),
                _ => None,
            }
        };
        if let Some(x) = doomed {
            self.inner.fail(&x, "the session ended");
        }
        if left == 0 {
            let lanes: Vec<Handle> =
                lock(&self.inner.lanes).values().filter(|l| l.remote == remote).map(|l| l.conn).collect();
            for conn in lanes {
                self.inner.core.close(conn);
            }
        }
    }

    /// The most chunk bytes ever waiting for the writer at once: the receiver never holds more of a file than this.
    pub fn peak_queued(&self) -> u64 {
        self.inner.peak_queued.load(Ordering::Acquire)
    }

    /// Transfers running now.
    pub fn running(&self) -> usize {
        lock(&self.inner.xfers).values().filter(|x| x.running()).count()
    }

    /// Connections open now.
    pub fn lane_count(&self) -> usize {
        lock(&self.inner.lanes).len()
    }
}

impl Drop for ParService {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Release);
        let xs: Vec<Arc<Xfer>> = lock(&self.inner.xfers).drain().map(|(_, x)| x).collect();
        for x in xs {
            self.inner.fail(&x, "Steam networking stopped");
        }
        let conns: Vec<Handle> = lock(&self.inner.lanes).keys().copied().collect();
        for c in conns {
            self.inner.core.close(c);
        }
        self.inner.core.close_listener(self.listener);
    }
}

/// Housekeeping: silent connections, stalled and finished transfers.
fn keeper(inner: Weak<Inner>) {
    loop {
        thread::sleep(Duration::from_millis(250));
        let Some(inner) = inner.upgrade() else { return };
        if inner.stop.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        let silent: Vec<Handle> = lock(&inner.lanes)
            .values()
            .filter(|l| lock(&l.xfer).is_none() && now.duration_since(l.since) > HELLO_WAIT)
            .map(|l| l.conn)
            .collect();
        for c in silent {
            inner.core.close(c);
        }
        let xs: Vec<(u64, Arc<Xfer>)> = lock(&inner.xfers).iter().map(|(r, x)| (*r, Arc::clone(x))).collect();
        for (remote, x) in xs {
            if x.running() {
                if now.duration_since(*lock(&x.last_in)) > STALL {
                    inner.fail(&x, &format!("no data for {} s", STALL.as_secs()));
                }
            } else if lock(&x.finished_at).is_some_and(|t| now.duration_since(t) > LINGER) {
                let lanes: Vec<Handle> = lock(&x.lanes).clone();
                for c in lanes {
                    inner.core.close(c);
                }
                let mut map = lock(&inner.xfers);
                if map.get(&remote).is_some_and(|m| Arc::ptr_eq(m, &x)) {
                    map.remove(&remote);
                }
            }
        }
    }
}

impl Inner {
    fn send(&self, conn: Handle, msg: &[u8]) {
        let _ = self.core.send_reliable(conn, msg, Duration::from_secs(2));
    }

    fn refuse(&self, conn: Handle, xfer: u32, code: u8) {
        self.send(conn, &wire::encode_refuse(xfer, code));
        self.core.close(conn);
    }

    fn violation(&self, conn: Handle) {
        self.core.close(conn);
    }

    /// The transfer failed: the writer stops (and removes the partial file), the sender is told, the lanes close.
    fn fail(&self, x: &Arc<Xfer>, why: &str) {
        if x.state.compare_exchange(RUNNING, FAILED, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        *lock(&x.finished_at) = Some(Instant::now());
        // dropping the sender ends the writer thread, which removes the partial file
        lock(&x.tx).take();
        let lanes: Vec<Handle> = lock(&x.lanes).clone();
        for c in &lanes {
            self.send(*c, &wire::encode_abort(x.id, reason::CANCELLED));
        }
        for c in lanes {
            self.core.close(c);
        }
        (x.allow.on_event)(ParEvent::Failed { ctx: x.ctx, why: why.to_owned() });
    }

    fn on_hello(&self, lane: &Arc<RxLane>, h: Hello) {
        let conn = lane.conn;
        if lock(&lane.xfer).is_some() {
            return self.violation(conn);
        }
        let allows = lock(&self.allow).get(&lane.remote).cloned().unwrap_or_default();
        if allows.is_empty() {
            return self.refuse(conn, h.xfer, reason::NOT_EXPECTED);
        }
        if h.total == 0
            || !(4096..=256 * 1024).contains(&h.chunk)
            || wire::chunk_count(h.total, h.chunk) != Some(h.count)
        {
            return self.refuse(conn, h.xfer, reason::BAD_FIELDS);
        }
        // the allowances whose secret the hello proves (one without a secret takes any), then the one that takes the size
        let proven: Vec<&Arc<Allow>> =
            allows.iter().filter(|a| a.secret.as_ref().is_none_or(|secret| h.token_ok(secret))).collect();
        if proven.is_empty() {
            return self.refuse(conn, h.xfer, reason::BAD_TOKEN);
        }
        let Some(allow) = proven.into_iter().find(|a| h.total <= a.max_total).cloned() else {
            return self.refuse(conn, h.xfer, reason::TOO_BIG);
        };
        let x = {
            let mut xfers = lock(&self.xfers);
            match xfers.get(&lane.remote).cloned() {
                Some(x) if x.running() => {
                    if !x.matches(&h) {
                        let code = if x.id == h.xfer { reason::MISMATCH } else { reason::BUSY };
                        return self.refuse(conn, h.xfer, code);
                    }
                    if lock(&x.lanes).len() >= MAX_LANES {
                        return self.refuse(conn, h.xfer, reason::TOO_MANY);
                    }
                    x
                }
                Some(x) if x.id == h.xfer => return self.refuse(conn, h.xfer, reason::NOT_EXPECTED),
                _ => match self.start(&h, &allow) {
                    Ok(x) => {
                        xfers.insert(lane.remote, Arc::clone(&x));
                        x
                    }
                    Err(e) => {
                        (allow.on_event)(ParEvent::Failed { ctx: h.ctx, why: format!("cannot create the file: {e}") });
                        return self.refuse(conn, h.xfer, reason::WRITE);
                    }
                },
            }
        };
        {
            let mut lanes = lock(&x.lanes);
            lanes.push(conn);
            x.lanes_max.fetch_max(lanes.len(), Ordering::AcqRel);
        }
        *lock(&lane.xfer) = Some(Arc::clone(&x));
        *lock(&x.last_in) = Instant::now();
        self.send(conn, &wire::encode_accept(h.xfer));
    }

    /// A new transfer: the file is created at its full size and a writer thread started.
    fn start(&self, h: &Hello, allow: &Arc<Allow>) -> io::Result<Arc<Xfer>> {
        let me = self.me.upgrade().ok_or_else(|| io::Error::other("the service ended"))?;
        fs::create_dir_all(&allow.dir)?;
        let stem = (allow.stem)(h.ctx);
        let fin = allow.dir.join(format!("{stem}.{}", allow.ext));
        let part = allow.dir.join(format!("{stem}.{}.part", allow.ext));
        let file = OpenOptions::new().create(true).write(true).read(true).truncate(true).open(&part)?;
        file.set_len(h.total)?;
        let (tx, rx) = mpsc::sync_channel::<Job>(QUEUE);
        let words = (h.count as usize).div_ceil(64);
        let x = Arc::new(Xfer {
            id: h.xfer,
            ctx: h.ctx,
            total: h.total,
            count: h.count,
            chunk: h.chunk,
            sha256: h.sha256,
            allow: Arc::clone(allow),
            part,
            fin,
            seen: Mutex::new(vec![0; words]),
            tx: Mutex::new(Some(tx)),
            lanes: Mutex::new(Vec::new()),
            started: Instant::now(),
            last_in: Mutex::new(Instant::now()),
            finished_at: Mutex::new(None),
            bytes_in: AtomicU64::new(0),
            chunks_in: AtomicU64::new(0),
            duplicates: AtomicU64::new(0),
            lanes_max: AtomicUsize::new(0),
            state: AtomicU8::new(RUNNING),
        });
        let (inner, xc) = (me, Arc::clone(&x));
        thread::Builder::new().name("feverlink3-par-writer".into()).spawn(move || writer(inner, xc, file, rx))?;
        Ok(x)
    }

    fn on_chunk(&self, lane: &Arc<RxLane>, xfer: u32, index: u32, count: u32, payload: &[u8]) {
        let Some(x) = lock(&lane.xfer).clone() else { return self.violation(lane.conn) };
        if xfer != x.id || count != x.count || wire::chunk_len(x.total, x.chunk, index) != Some(payload.len()) {
            return self.violation(lane.conn);
        }
        if !x.running() {
            return;
        }
        *lock(&x.last_in) = Instant::now();
        let job = if x.first_time(index) {
            let now = self.queued.fetch_add(payload.len() as u64, Ordering::AcqRel) + payload.len() as u64;
            self.peak_queued.fetch_max(now, Ordering::AcqRel);
            Job::Chunk { lane: lane.conn, index, data: payload.to_vec() }
        } else {
            x.duplicates.fetch_add(1, Ordering::AcqRel);
            Job::Dup { lane: lane.conn, index }
        };
        let tx = lock(&x.tx).clone();
        if let Some(tx) = tx {
            // blocks only while the writer is behind (64 chunks); the writer always drains
            let _ = tx.send(job);
        }
    }
}

impl Owner for Inner {
    fn opened(&self, conn: Handle, incoming: bool, remote: Option<u64>) {
        let allowed = incoming && remote.is_some_and(|r| lock(&self.allow).get(&r).is_some_and(|l| !l.is_empty()));
        let Some(remote) = remote.filter(|_| allowed) else {
            self.core.close(conn);
            return;
        };
        lock(&self.lanes)
            .insert(conn, Arc::new(RxLane { conn, remote, since: Instant::now(), xfer: Mutex::new(None) }));
    }

    fn message(&self, conn: Handle, data: &[u8]) {
        let Some(lane) = lock(&self.lanes).get(&conn).cloned() else { return };
        match wire::parse(data) {
            Some(Msg::Hello(h)) => self.on_hello(&lane, h),
            Some(Msg::Chunk { xfer, index, count, payload }) => self.on_chunk(&lane, xfer, index, count, payload),
            Some(Msg::Abort { xfer, .. }) => {
                let x = lock(&lane.xfer).clone();
                if let Some(x) = x.filter(|x| x.id == xfer) {
                    self.fail(&x, "the sender aborted");
                }
            }
            _ => self.violation(conn),
        }
    }

    fn closed(&self, conn: Handle, _end: End) {
        let lane = lock(&self.lanes).remove(&conn);
        if let Some(x) = lane.and_then(|l| lock(&l.xfer).clone()) {
            lock(&x.lanes).retain(|c| *c != conn);
        }
    }
}

/// Flushes the acknowledgements collected per lane.
fn flush_acks(inner: &Inner, x: &Xfer, acks: &mut HashMap<Handle, Vec<u32>>) {
    for (lane, indices) in acks.iter_mut() {
        for part in indices.chunks(wire::ACK_MAX) {
            inner.send(*lane, &wire::encode_ack(x.id, part));
        }
        indices.clear();
    }
    acks.retain(|_, v| !v.is_empty());
}

fn writer(inner: Arc<Inner>, x: Arc<Xfer>, file: File, rx: mpsc::Receiver<Job>) {
    let mut acks: HashMap<Handle, Vec<u32>> = HashMap::new();
    let mut written = 0u32;
    let mut last_flush = Instant::now();
    let mut last_event = Instant::now() - Duration::from_secs(1);
    let mut window: VecDeque<(Instant, u64)> = VecDeque::new();
    let mut ok = false;
    loop {
        let job = match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(j) => Some(j),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if !x.running() {
            break;
        }
        match job {
            Some(Job::Chunk { lane, index, data }) => {
                inner.queued.fetch_sub(data.len() as u64, Ordering::AcqRel);
                if let Err(e) = write_all_at(&file, &data, u64::from(index) * u64::from(x.chunk)) {
                    inner.fail(&x, &format!("cannot write the file: {e}"));
                    break;
                }
                x.bytes_in.fetch_add(data.len() as u64, Ordering::AcqRel);
                x.chunks_in.fetch_add(1, Ordering::AcqRel);
                written += 1;
                acks.entry(lane).or_default().push(index);
            }
            Some(Job::Dup { lane, index }) => acks.entry(lane).or_default().push(index),
            None => {}
        }
        let now = Instant::now();
        if written == x.count
            || now.duration_since(last_flush) >= Duration::from_millis(20)
            || acks.values().any(|v| v.len() >= 16)
        {
            flush_acks(&inner, &x, &mut acks);
            last_flush = now;
        }
        if now.duration_since(last_event) >= Duration::from_millis(250) {
            last_event = now;
            let bytes = x.bytes_in.load(Ordering::Acquire);
            window.push_back((now, bytes));
            while window.len() > 2 && window.get(1).is_some_and(|w| now.duration_since(w.0) > Duration::from_secs(2)) {
                window.pop_front();
            }
            let rate = window.front().map_or(0.0, |(t0, b0)| {
                bytes.saturating_sub(*b0) as f64 / now.duration_since(*t0).as_secs_f64().max(0.05)
            });
            let lanes = lock(&x.lanes).len();
            (x.allow.on_event)(ParEvent::Progress { ctx: x.ctx, bytes, total: x.total, lanes, rate });
        }
        if written == x.count {
            ok = true;
            break;
        }
    }
    if !ok {
        // failed or abandoned: the partial file goes
        drop(file);
        let _ = fs::remove_file(&x.part);
        return;
    }
    finish(&inner, &x, file);
}

/// Every chunk is in: check the SHA-256 of the whole file, move it into place, tell the sender and the owner.
fn finish(inner: &Arc<Inner>, x: &Arc<Xfer>, file: File) {
    let matched = {
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut pos = 0u64;
        let mut read_ok = true;
        while pos < x.total {
            let n = ((x.total - pos) as usize).min(buf.len());
            if read_exact_at(&file, &mut buf[..n], pos).is_err() {
                read_ok = false;
                break;
            }
            hasher.update(&buf[..n]);
            pos += n as u64;
        }
        read_ok && hasher.finalize().as_slice() == x.sha256
    };
    if !matched {
        drop(file);
        let _ = fs::remove_file(&x.part);
        let lanes: Vec<Handle> = lock(&x.lanes).clone();
        if x.state.compare_exchange(RUNNING, FAILED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            *lock(&x.finished_at) = Some(Instant::now());
            for c in lanes {
                inner.send(c, &wire::encode_done(x.id, false));
            }
            (x.allow.on_event)(ParEvent::Failed {
                ctx: x.ctx,
                why: "the SHA-256 of the received file does not match".into(),
            });
        }
        return;
    }
    let _ = file.sync_all();
    drop(file);
    if let Err(e) = fs::rename(&x.part, &x.fin) {
        let _ = fs::remove_file(&x.part);
        inner.fail(x, &format!("cannot move the file into place: {e}"));
        return;
    }
    if x.state.compare_exchange(RUNNING, DONE, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    *lock(&x.finished_at) = Some(Instant::now());
    // the owner first (it queues the event the session thread reads before anything the host sends next), then the sender
    (x.allow.on_event)(ParEvent::Done {
        ctx: x.ctx,
        sha256: x.sha256,
        len: x.total,
        path: x.fin.clone(),
        secs: x.started.elapsed().as_secs_f64(),
        lanes_max: x.lanes_max.load(Ordering::Acquire),
    });
    let lanes: Vec<Handle> = lock(&x.lanes).clone();
    for c in lanes {
        inner.send(c, &wire::encode_done(x.id, true));
    }
}

/// Lower-case hex of a SHA-256.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
