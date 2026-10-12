//! The Steam speed test: how fast can a save travel between two players, in each direction.
//!
//! The test runs on the connection of the connection test (virtual port 1, the probe), so it needs nothing the
//! probe does not have. The tester sends N MB of random (incompressible) data in messages as large as a resync
//! chunk is on the wire, the friend's probe times what arrives and answers, then the friend's probe sends N MB
//! back. This file holds the message format, the timing math, the sender loop and the text of the result; the
//! probe (`steamnet.rs`) holds the state machine.
//!
//! Messages are one ASCII header line, `\n`, and (for data) the payload:
//!
//! | message | from | meaning |
//! |---|---|---|
//! | `tput up <id> <total> <chunk> [<n>]` | tester | N bytes follow in data messages (over `n` connections); time them |
//! | `tput d <id> <seq>` + payload | both | one chunk |
//! | `tput end <id> <sent> <peak_pending> <max_block_ms>` | sender | the last chunk is out; the sender's numbers |
//! | `tput prog <id> <bytes>` | receiver | bytes received so far (about every 500 ms) |
//! | `tput upres <id> <bytes> <ms> <gap_ms> <steady>` | friend | the upload arrived: the friend's own timing |
//! | `tput down <id> <total> <chunk> [<n>]` | tester | now send N bytes to me (over `n` connections) |
//! | `tput dgdown <id> <total>` | tester | now send N bytes to me as bulk datagrams |
//! | `tput dres <id> k=v ...` | receiver | datagram transfer received: bytes, ms, gap, ok |
//! | `tput dsend <id> k=v ...` | sender | datagram transfer sent: the sender's numbers (loss, delay, rate) |

use super::bulk::SenderStats;
use super::{CoreRef, Handle, LinkSnapshot};
use feverlink3_core::session::ResyncLimits;
use std::time::{Duration, Instant};

/// Bytes of payload in a full chunk: what a resync chunk weighs on the wire (256 KiB of save, base64-coded).
pub const CHUNK_PAYLOAD: usize = ResyncLimits::CHUNK_SIZE / 3 * 4;
/// One MB, as the players count (a save of 71 109 822 bytes is "71 MB").
pub const MB: u64 = 1_000_000;
/// The save the estimate is made for.
pub const SAVE_MB: f64 = 80.0;
/// Base64 turns 3 bytes of save into 4 on the wire.
pub const WIRE_FACTOR: f64 = 4.0 / 3.0;
/// No data for longer than this is a stall.
pub const STALL: Duration = Duration::from_secs(5);
/// The test gives up after this long without any progress.
pub const ABORT_AFTER: Duration = Duration::from_secs(60);
/// The most a peer may ask us to send or may send us in one test.
pub const MAX_TOTAL: u64 = 128 * MB;
/// How long one chunk may wait for room in Steam's send buffer.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// The receiver reports its progress this often.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(500);
/// Longest header line.
const HEADER_LIMIT: usize = 400;
/// Most parallel connections a reliable variant (or a friend's request) uses.
pub const MAX_CONNS: u32 = 16;

// ---- messages

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Up {
        id: u64,
        total: u64,
        chunk: u64,
        conns: u32,
    },
    /// `rate` (bytes per second, 0 for none): the Steam send rate the responder sets on its own connections before it
    /// streams. Old programs ignore the extra fields.
    Down {
        id: u64,
        total: u64,
        chunk: u64,
        conns: u32,
        rate: u32,
    },
    /// Stop sending test `id` (the tester gave up on it).
    Stop {
        id: u64,
    },
    /// The responder's answer to a `rate` request: what was asked and the rate Steam then reports (0: not readable).
    RateAck {
        id: u64,
        asked: u32,
        got: u32,
    },
    /// Send me `total` bytes as bulk datagrams.
    DgDown {
        id: u64,
        total: u64,
    },
    /// I will send `total` bytes over parallel connections to your parallel port (`Auto (scaled)`): get ready.
    ParUp {
        id: u64,
        total: u64,
    },
    /// Ready for the parallel upload `id`.
    ParReady {
        id: u64,
    },
    /// The parallel upload `id` was received (or failed): the receiver's numbers (`ok`, `bytes`, `ms`, `lanes`).
    ParRes {
        id: u64,
        kv: Vec<(String, u64)>,
    },
    /// Send me `total` bytes over parallel connections.
    ParDown {
        id: u64,
        total: u64,
    },
    /// The parallel transfer `id` was sent: the sender's numbers and a sentence on why it stopped adding connections.
    ParSend {
        id: u64,
        kv: Vec<(String, u64)>,
        text: String,
    },
    /// The datagram transfer `id` arrived (or failed): the receiver's numbers.
    Dres {
        id: u64,
        kv: Vec<(String, u64)>,
    },
    /// The datagram transfer `id` was sent: the sender's numbers.
    Dsend {
        id: u64,
        kv: Vec<(String, u64)>,
    },
    Data {
        id: u64,
        seq: u64,
        len: usize,
    },
    End {
        id: u64,
        sent: u64,
        peak_pending: u32,
        max_block_ms: u64,
    },
    Prog {
        id: u64,
        bytes: u64,
    },
    UpRes {
        id: u64,
        bytes: u64,
        ms: u64,
        gap_ms: u64,
        steady: u64,
    },
}

/// Reads a speed-test message; `None` for anything else (a ping, garbage, a header that does not parse).
pub fn parse(data: &[u8]) -> Option<Msg> {
    let rest = data.strip_prefix(b"tput ")?;
    let end = rest.iter().take(HEADER_LIMIT).position(|b| *b == b'\n')?;
    let header = std::str::from_utf8(&rest[..end]).ok()?;
    let payload = rest.len() - end - 1;
    let mut words = header.split_whitespace();
    let kind = words.next()?;
    let mut num = || -> Option<u64> { words.next()?.parse().ok() };
    let msg = match kind {
        "up" | "down" => {
            let (id, total, chunk) = (num()?, num()?, num()?);
            let conns = u32::try_from(num().unwrap_or(1)).ok()?.clamp(1, MAX_CONNS);
            if kind == "up" {
                Msg::Up { id, total, chunk, conns }
            } else {
                let rate = u32::try_from(num().unwrap_or(0)).ok()?;
                Msg::Down { id, total, chunk, conns, rate }
            }
        }
        "stop" => Msg::Stop { id: num()? },
        "rate" => Msg::RateAck { id: num()?, asked: u32::try_from(num()?).ok()?, got: u32::try_from(num()?).ok()? },
        "dgdown" => Msg::DgDown { id: num()?, total: num()? },
        "parup" => Msg::ParUp { id: num()?, total: num()? },
        "parready" => Msg::ParReady { id: num()? },
        "pardown" => Msg::ParDown { id: num()?, total: num()? },
        "parres" | "parsend" => {
            let id = num()?;
            let mut kv = Vec::new();
            for word in header.split_whitespace().skip(2).take(24) {
                let (k, v) = word.split_once('=')?;
                kv.push((k.to_string(), v.parse().ok()?));
            }
            if kind == "parres" {
                Msg::ParRes { id, kv }
            } else {
                // the sentence follows the header line
                let body = &rest[end + 1..];
                let text = String::from_utf8_lossy(&body[..body.len().min(400)]).into_owned();
                Msg::ParSend { id, kv, text }
            }
        }
        "dres" | "dsend" => {
            let id = num()?;
            let mut kv = Vec::new();
            for word in header.split_whitespace().skip(2).take(24) {
                let (k, v) = word.split_once('=')?;
                kv.push((k.to_string(), v.parse().ok()?));
            }
            if kind == "dres" {
                Msg::Dres { id, kv }
            } else {
                Msg::Dsend { id, kv }
            }
        }
        "d" => Msg::Data { id: num()?, seq: num()?, len: payload },
        "end" => Msg::End { id: num()?, sent: num()?, peak_pending: u32::try_from(num()?).ok()?, max_block_ms: num()? },
        "prog" => Msg::Prog { id: num()?, bytes: num()? },
        "upres" => Msg::UpRes { id: num()?, bytes: num()?, ms: num()?, gap_ms: num()?, steady: num()? },
        _ => return None,
    };
    Some(msg)
}

/// A message without payload, as bytes.
pub fn encode(msg: &Msg) -> Vec<u8> {
    let text = match msg {
        Msg::Up { id, total, chunk, conns: 1 } => format!("tput up {id} {total} {chunk}\n"),
        Msg::Up { id, total, chunk, conns } => format!("tput up {id} {total} {chunk} {conns}\n"),
        Msg::Down { id, total, chunk, conns: 1, rate: 0 } => format!("tput down {id} {total} {chunk}\n"),
        Msg::Down { id, total, chunk, conns, rate: 0 } => format!("tput down {id} {total} {chunk} {conns}\n"),
        Msg::Down { id, total, chunk, conns, rate } => format!("tput down {id} {total} {chunk} {conns} {rate}\n"),
        Msg::Stop { id } => format!("tput stop {id}\n"),
        Msg::RateAck { id, asked, got } => format!("tput rate {id} {asked} {got}\n"),
        Msg::DgDown { id, total } => format!("tput dgdown {id} {total}\n"),
        Msg::ParUp { id, total } => format!("tput parup {id} {total}\n"),
        Msg::ParReady { id } => format!("tput parready {id}\n"),
        Msg::ParDown { id, total } => format!("tput pardown {id} {total}\n"),
        Msg::ParRes { id, kv } => format!("tput parres {id}{}\n", kv_text(kv)),
        Msg::ParSend { id, kv, text } => format!("tput parsend {id}{}\n{text}", kv_text(kv)),
        Msg::Dres { id, kv } => format!("tput dres {id}{}\n", kv_text(kv)),
        Msg::Dsend { id, kv } => format!("tput dsend {id}{}\n", kv_text(kv)),
        Msg::Data { id, seq, .. } => format!("tput d {id} {seq}\n"),
        Msg::End { id, sent, peak_pending, max_block_ms } => {
            format!("tput end {id} {sent} {peak_pending} {max_block_ms}\n")
        }
        Msg::Prog { id, bytes } => format!("tput prog {id} {bytes}\n"),
        Msg::UpRes { id, bytes, ms, gap_ms, steady } => format!("tput upres {id} {bytes} {ms} {gap_ms} {steady}\n"),
    };
    text.into_bytes()
}

fn kv_text(kv: &[(String, u64)]) -> String {
    kv.iter().map(|(k, v)| format!(" {k}={v}")).collect()
}

/// A value of a key=value message, 0 when absent.
pub fn kv_get(kv: &[(String, u64)], key: &str) -> u64 {
    kv.iter().find(|(k, _)| k == key).map_or(0, |(_, v)| *v)
}

/// A data message of `len` payload bytes: header, then random bytes that no compression can shrink.
pub fn data_message(id: u64, seq: u64, len: usize) -> Vec<u8> {
    let mut out = encode(&Msg::Data { id, seq, len });
    let mut x =
        id.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seq.wrapping_mul(0xD6E8_FEB8_6659_FD93) ^ 0x1234_5678_9ABC_DEF1;
    out.reserve(len);
    let mut left = len;
    while left > 0 {
        // xorshift64*
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let word = x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
        let take = left.min(8);
        out.extend_from_slice(&word[..take]);
        left -= take;
    }
    out
}

/// Chunks needed for `total` bytes.
pub fn chunk_count(total: u64, chunk: u64) -> u64 {
    if chunk == 0 {
        0
    } else {
        total.div_ceil(chunk)
    }
}

// ---- the receiving side

/// Times what arrives.
#[derive(Debug, Clone)]
pub struct Receiver {
    pub id: u64,
    pub total: u64,
    pub got: u64,
    pub chunks: u64,
    started: Instant,
    first: Option<Instant>,
    first_len: u64,
    last: Instant,
    max_gap: Duration,
    last_prog: Instant,
    seen: std::collections::HashSet<u64>,
}

impl Receiver {
    pub fn new(id: u64, total: u64, now: Instant) -> Receiver {
        Receiver {
            id,
            total,
            got: 0,
            chunks: 0,
            started: now,
            first: None,
            first_len: 0,
            last: now,
            max_gap: Duration::ZERO,
            last_prog: now,
            seen: std::collections::HashSet::new(),
        }
    }

    /// As [`Receiver::on_chunk`] for chunk number `seq`; a number seen before (it cannot be, on one connection) is
    /// ignored, so chunks that arrive over several connections in any order are counted once each. False for a repeat.
    pub fn on_chunk_seq(&mut self, now: Instant, seq: u64, len: usize) -> bool {
        // bounded: a test has at most MAX_TOTAL / 1 chunks, and a peer repeating numbers adds nothing
        if self.seen.len() < 1_000_000 && !self.seen.insert(seq) {
            return false;
        }
        self.on_chunk(now, len);
        true
    }

    pub fn on_chunk(&mut self, now: Instant, len: usize) {
        let len = len as u64;
        match self.first {
            None => {
                self.first = Some(now);
                self.first_len = len;
                // the wait for the first chunk counts as a gap too: a route that is slow to start is a stall
                self.max_gap = now.saturating_duration_since(self.started);
            }
            Some(_) => self.max_gap = self.max_gap.max(now.saturating_duration_since(self.last)),
        }
        self.last = now;
        self.got = self.got.saturating_add(len);
        self.chunks += 1;
    }

    /// Whether a progress message is due; marks it sent.
    pub fn progress_due(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.last_prog) >= PROGRESS_EVERY {
            self.last_prog = now;
            true
        } else {
            false
        }
    }

    /// First chunk to last chunk, in ms.
    pub fn span_ms(&self) -> u64 {
        self.first.map_or(0, |f| self.last.saturating_duration_since(f).as_millis() as u64)
    }

    /// The bytes that arrived after the first chunk: what `span_ms` measures.
    pub fn steady(&self) -> u64 {
        self.got.saturating_sub(self.first_len)
    }

    pub fn max_gap_ms(&self) -> u64 {
        self.max_gap.as_millis() as u64
    }

    /// Bytes per second after the first chunk; `None` when too little arrived to time.
    pub fn rate(&self) -> Option<f64> {
        rate_of(self.steady(), self.span_ms())
    }

    /// Seconds since this receiver was made.
    pub fn since_start(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.started).as_secs_f64()
    }
}

/// Bytes per second for `bytes` in `ms`; `None` when no time passed.
pub fn rate_of(bytes: u64, ms: u64) -> Option<f64> {
    (ms > 0 && bytes > 0).then(|| bytes as f64 * 1000.0 / ms as f64)
}

// ---- the sending side

/// What a sender loop did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendStats {
    pub sent: u64,
    pub elapsed_ms: u64,
    /// Most reliable bytes Steam reported queued and not yet on the wire (measured by the core every second).
    pub peak_pending: u32,
    /// Longest wait for room in Steam's send buffer.
    pub max_block_ms: u64,
    pub error: Option<String>,
}

/// Sends `total` bytes in chunks of `chunk` bytes over the connections `conns` (chunks go to whichever connection is
/// free, so all of them are kept busy; with one connection that is plain in-order streaming), then the end message on
/// the first one. Blocks while Steam's buffer is full, which is how the link's speed paces it. Runs on a thread of its
/// own; `cancelled` is asked before every chunk. Chunks are sent without the Nagle delay and each connection is
/// flushed at the end.
pub fn stream(
    core: &CoreRef,
    conns: &[Handle],
    id: u64,
    total: u64,
    chunk: u64,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> SendStats {
    let start = Instant::now();
    let chunk = chunk.clamp(1, CHUNK_PAYLOAD as u64);
    let count = chunk_count(total, chunk);
    let next = std::sync::atomic::AtomicU64::new(0);
    let shared = std::sync::Mutex::new(SendStats::default());
    let work = |conn: Handle| {
        let mut local = SendStats::default();
        loop {
            if cancelled() {
                local.error = Some("stopped".into());
                break;
            }
            let seq = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if seq >= count {
                break;
            }
            let len = (total - seq * chunk).min(chunk) as usize;
            let message = data_message(id, seq, len);
            let before = Instant::now();
            if let Err(e) = core.send_reliable_no_nagle(conn, &message, SEND_TIMEOUT) {
                local.error = Some(e.to_string());
                break;
            }
            local.max_block_ms = local.max_block_ms.max(before.elapsed().as_millis() as u64);
            local.sent += len as u64;
            if let Some(link) = core.link(conn) {
                local.peak_pending = local.peak_pending.max(link.pending);
            }
        }
        core.flush(conn);
        let mut all = shared.lock().unwrap_or_else(|p| p.into_inner());
        all.sent += local.sent;
        all.peak_pending = all.peak_pending.max(local.peak_pending);
        all.max_block_ms = all.max_block_ms.max(local.max_block_ms);
        if all.error.is_none() {
            all.error = local.error;
        }
    };
    if conns.len() <= 1 {
        work(conns.first().copied().unwrap_or(0));
    } else {
        std::thread::scope(|scope| {
            for &conn in conns {
                scope.spawn(move || work(conn));
            }
        });
    }
    let mut stats = std::mem::take(&mut *shared.lock().unwrap_or_else(|p| p.into_inner()));
    stats.elapsed_ms = start.elapsed().as_millis() as u64;
    if stats.error.is_none() {
        let end = encode(&Msg::End {
            id,
            sent: stats.sent,
            peak_pending: stats.peak_pending,
            max_block_ms: stats.max_block_ms,
        });
        let primary = conns.first().copied().unwrap_or(0);
        if let Err(e) = core.send_reliable_no_nagle(primary, &end, SEND_TIMEOUT) {
            stats.error = Some(e.to_string());
        }
        core.flush(primary);
    }
    stats
}

// ---- the result

/// One direction of a finished test.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Direction {
    pub bytes: u64,
    /// Bytes per second as the receiver timed it; `None` when too little arrived to time.
    pub rate: Option<f64>,
    pub secs: f64,
    /// Reliable bytes the sender saw queued at most.
    pub peak_pending: u32,
    /// The longest time no data moved: the receiver's longest gap or the sender's longest wait.
    pub longest_pause_ms: u64,
    /// The highest round trip Steam reported while this direction ran, ms (`None`: no sample).
    pub ping_peak_ms: Option<u32>,
    /// The direction was stopped after the time limit; `bytes` and `rate` are what had moved by then.
    pub capped: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpeedResult {
    pub total_bytes: u64,
    /// You to your friend.
    pub up: Direction,
    /// Your friend to you.
    pub down: Direction,
    pub total_secs: f64,
    /// Steam's own round-trip estimate.
    pub ping_ms: Option<u32>,
    pub route: String,
    /// The send rate we asked Steam for, and the one it reports.
    pub rate_set: i32,
    pub rate_effective: Option<u32>,
    /// Connections the data went over (1 for the plain test).
    pub conns: u32,
    /// Changes of the route during the run ("direct -> through a Steam relay at 12 s").
    pub route_notes: Vec<String>,
    /// What the run did to the Steam send rate: asked, applied here, the responder's answer (empty: left alone).
    pub rate_note: String,
    /// One line about what Steam itself reported during the run (empty when there were no samples).
    pub steam_line: String,
}

/// Seconds to send `save_mb` MB of save at `wire_rate` bytes per second (the save grows by 4/3 on the wire).
pub fn estimate_secs(save_mb: f64, wire_rate: Option<f64>) -> Option<f64> {
    wire_rate.filter(|r| *r > 0.0).map(|r| save_mb * MB as f64 * WIRE_FACTOR / r)
}

/// "58 s" or "2 min 5 s".
pub fn secs_text(secs: f64) -> String {
    let s = secs.round().max(0.0) as u64;
    if s >= 120 {
        format!("{} min {} s", s / 60, s % 60)
    } else {
        format!("{s} s")
    }
}

/// "1.84 MB/s", or "unknown".
pub fn rate_text(rate: Option<f64>) -> String {
    rate.map_or_else(|| "unknown".into(), |r| format!("{:.2} MB/s", r / MB as f64))
}

fn kib_text(bytes: u32) -> String {
    format!("{} KiB", (bytes + 512) / 1024)
}

impl SpeedResult {
    /// The longest pause in either direction, ms.
    pub fn longest_pause_ms(&self) -> u64 {
        self.up.longest_pause_ms.max(self.down.longest_pause_ms)
    }

    pub fn stalled(&self) -> bool {
        self.longest_pause_ms() > STALL.as_millis() as u64
    }

    /// Estimated seconds for the 80 MB save when you host (it travels like your upload) and when your friend hosts.
    pub fn estimates(&self) -> (Option<f64>, Option<f64>) {
        (estimate_secs(SAVE_MB, self.up.rate), estimate_secs(SAVE_MB, self.down.rate))
    }

    fn route_line(&self) -> String {
        let route = if self.route.is_empty() { "unknown route" } else { &self.route };
        let ping = self.ping_ms.map_or_else(|| "unknown".into(), |p| format!("{p} ms"));
        let rate = match self.rate_effective {
            Some(r) if i64::from(r) == i64::from(self.rate_set) => format!("send rate {r} B/s"),
            Some(r) => format!("send rate {r} B/s (asked for {})", self.rate_set),
            None => format!("send rate asked for {}", self.rate_set),
        };
        format!("Route: {route}, round trip {ping}, {rate}")
    }

    /// The lines the window shows under the button.
    pub fn lines(&self) -> Vec<String> {
        let mb = self.total_bytes as f64 / MB as f64;
        let over = if self.conns > 1 { format!(" over {} connections", self.conns) } else { String::new() };
        let mut out = vec![
            format!(
                "Speed test, {mb:.0} MB each way{over}: you to friend {}, friend to you {}, {:.1} s in all",
                rate_text(self.up.rate),
                rate_text(self.down.rate),
                self.total_secs
            ),
            self.route_line(),
        ];
        let pause = self.longest_pause_ms() as f64 / 1000.0;
        out.push(format!(
            "Queue peak: {} sending, {} receiving; longest pause {pause:.1} s, {}",
            kib_text(self.up.peak_pending),
            kib_text(self.down.peak_pending),
            if self.stalled() { "STALLED (more than 5 s without data)" } else { "no stall" }
        ));
        let wire = SAVE_MB * WIRE_FACTOR;
        let (host, guest) = self.estimates();
        let secs = |e: Option<f64>| e.map_or_else(|| "unknown".to_string(), secs_text);
        out.push(format!(
            "An {SAVE_MB:.0} MB save (about {wire:.0} MB over Steam): about {} if you host, {} if your friend hosts",
            secs(host),
            secs(guest)
        ));
        if !self.steam_line.is_empty() {
            out.push(self.steam_line.clone());
        }
        out
    }

    /// One line for the session log.
    pub fn log_line(&self) -> String {
        let (host, guest) = self.estimates();
        let secs = |e: Option<f64>| e.map_or_else(|| "?".to_string(), |s| format!("{s:.0}"));
        format!(
            "steam speed test: {:.0} MB each way{}, up {}, down {}, total {:.1} s, {}, ping {} ms, send rate {} (asked {}), queue peak {}/{} KiB, longest pause {} ms{}, 80 MB save about {} s hosting / {} s joining",
            self.total_bytes as f64 / MB as f64,
            if self.conns > 1 { format!(" over {} connections", self.conns) } else { String::new() },
            rate_text(self.up.rate),
            rate_text(self.down.rate),
            self.total_secs,
            if self.route.is_empty() { "route unknown" } else { &self.route },
            self.ping_ms.map_or_else(|| "?".into(), |p| p.to_string()),
            self.rate_effective.map_or_else(|| "?".into(), |r| r.to_string()),
            self.rate_set,
            self.up.peak_pending / 1024,
            self.down.peak_pending / 1024,
            self.longest_pause_ms(),
            if self.stalled() { " STALL" } else { "" },
            secs(host),
            secs(guest),
        )
    }
}

// ---- variants: reliable on 1, 2 or 4 connections, datagrams

/// One way of moving the data; a speed test run is a plan of these, one after the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Steam's reliable stream on this many connections at once.
    Reliable(u32),
    /// Bulk datagrams with our own reliability and congestion control.
    Datagram,
    /// Parallel Steam connections, their number scaled to the size of the file and adapted to the path: the way a
    /// resync save is sent (`steamnet::par`).
    Auto,
}

impl Variant {
    pub fn label(self) -> String {
        match self {
            Variant::Reliable(1) => "reliable".to_string(),
            Variant::Reliable(n) => format!("reliable, {n} connections"),
            Variant::Datagram => "datagrams".to_string(),
            Variant::Auto => "Auto (scaled)".to_string(),
        }
    }
}

/// The variants a mode word from the window stands for: `reliable` (the default), `dgram`, `both`, `parallel`
/// (reliable on 1, 2 and 4 connections) and `all` (those and the datagrams).
pub fn plan_for(mode: &str) -> Vec<Variant> {
    use Variant::{Datagram, Reliable};
    match mode.trim().to_ascii_lowercase().as_str() {
        "dgram" | "datagram" | "datagrams" => vec![Datagram],
        "auto" | "par" | "scaled" => vec![Variant::Auto],
        "both" => vec![Reliable(1), Datagram],
        "parallel" => vec![Reliable(1), Reliable(2), Reliable(4)],
        "all" => vec![Reliable(1), Reliable(2), Reliable(4), Datagram],
        _ => vec![Reliable(1)],
    }
}

/// Steam's own numbers about the connection, sampled about once a second while a variant ran.
#[derive(Debug, Clone, Default)]
pub struct SteamSamples(pub Vec<(f64, LinkSnapshot)>);

/// Most samples kept for one run.
const SAMPLE_LIMIT: usize = 600;

impl SteamSamples {
    pub fn push(&mut self, secs: f64, link: LinkSnapshot) {
        if self.0.len() < SAMPLE_LIMIT {
            self.0.push((secs, link));
        }
    }

    /// One line per sample for the session log.
    pub fn log_lines(&self, label: &str) -> Vec<String> {
        self.0
            .iter()
            .map(|(t, l)| {
                format!(
                    "steam speed test sample ({label}) t={t:.0}s: route {}, ping {} ms, send rate {} B/s, pending reliable {} B, pending unreliable {} B, sent unacked reliable {} B, quality local {:.3} remote {:.3}, out {:.0} B/s {:.0} pkt/s, in {:.0} B/s {:.0} pkt/s, queue time {} ms",
                    l.route.text(),
                    l.ping_ms.map_or_else(|| "?".to_string(), |p| p.to_string()),
                    l.send_rate,
                    l.steam.pending_reliable,
                    l.steam.pending_unreliable,
                    l.steam.sent_unacked_reliable,
                    l.quality,
                    l.steam.quality_remote,
                    l.steam.out_bytes_per_sec,
                    l.steam.out_packets_per_sec,
                    l.steam.in_bytes_per_sec,
                    l.steam.in_packets_per_sec,
                    l.steam.queue_time_us / 1000,
                )
            })
            .collect()
    }

    /// One line for the result: the extremes of what Steam reported. Empty without samples.
    pub fn summary_line(&self) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let links: Vec<&LinkSnapshot> = self.0.iter().map(|(_, l)| l).collect();
        let max = |f: &dyn Fn(&LinkSnapshot) -> f64| links.iter().map(|l| f(l)).fold(0.0f64, f64::max);
        let min = |f: &dyn Fn(&LinkSnapshot) -> f64| links.iter().map(|l| f(l)).fold(f64::MAX, f64::min);
        let rates: Vec<u32> = links.iter().map(|l| l.send_rate).collect();
        format!(
            "Steam's own view ({} samples): send rate {}..{} B/s, reliable queued up to {} KiB, unacked up to {} KiB, sending up to {}, receiving up to {}, delivery quality down to {:.1}% local / {:.1}% remote, wait in Steam's queue up to {:.0} ms",
            links.len(),
            rates.iter().min().copied().unwrap_or(0),
            rates.iter().max().copied().unwrap_or(0),
            max(&|l| f64::from(l.steam.pending_reliable)) as u64 / 1024,
            max(&|l| f64::from(l.steam.sent_unacked_reliable)) as u64 / 1024,
            rate_text(Some(max(&|l| f64::from(l.steam.out_bytes_per_sec)))),
            rate_text(Some(max(&|l| f64::from(l.steam.in_bytes_per_sec)))),
            100.0 * min(&|l| f64::from(l.quality)),
            100.0 * min(&|l| f64::from(l.steam.quality_remote)),
            max(&|l| l.steam.queue_time_us as f64) / 1000.0,
        )
    }
}

/// The numbers the sender of a datagram transfer sends back in `dsend` (and keeps for itself).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SenderSummary {
    pub elapsed_ms: u64,
    /// Datagrams declared lost and datagrams retransmitted, per 10 000 sent.
    pub loss_bp: u64,
    pub retx_bp: u64,
    pub min_rtt_us: u64,
    pub qd_mean_us: u64,
    pub qd_p95_us: u64,
    pub qd_max_us: u64,
    pub settled_rate: u64,
    pub peak_rate: u64,
    pub peak_inflight: u64,
    pub sent_packets: u64,
    pub rtos: u64,
    pub blocked: u64,
}

impl SenderSummary {
    pub fn from_stats(stats: &SenderStats) -> SenderSummary {
        let bp = |pct: f64| (pct * 100.0).round() as u64;
        SenderSummary {
            elapsed_ms: stats.elapsed.as_millis() as u64,
            loss_bp: bp(stats.loss_percent()),
            retx_bp: bp(stats.retransmit_percent()),
            min_rtt_us: stats.min_rtt.map_or(0, |d| d.as_micros() as u64),
            qd_mean_us: (stats.queue_delay_mean_ms() * 1000.0) as u64,
            qd_p95_us: (stats.queue_delay_percentile_ms(0.95) * 1000.0) as u64,
            qd_max_us: (stats.qd_max * 1e6) as u64,
            settled_rate: stats.settled_rate() as u64,
            peak_rate: stats.rate_peak as u64,
            peak_inflight: stats.peak_inflight,
            sent_packets: stats.sent_packets,
            rtos: stats.rtos,
            blocked: stats.blocked,
        }
    }

    pub fn to_kv(&self) -> Vec<(String, u64)> {
        [
            ("ms", self.elapsed_ms),
            ("loss", self.loss_bp),
            ("retx", self.retx_bp),
            ("minrtt", self.min_rtt_us),
            ("qmean", self.qd_mean_us),
            ("qp95", self.qd_p95_us),
            ("qmax", self.qd_max_us),
            ("rate", self.settled_rate),
            ("peakrate", self.peak_rate),
            ("inflight", self.peak_inflight),
            ("sent", self.sent_packets),
            ("rtos", self.rtos),
            ("blocked", self.blocked),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    pub fn from_kv(kv: &[(String, u64)]) -> SenderSummary {
        SenderSummary {
            elapsed_ms: kv_get(kv, "ms"),
            loss_bp: kv_get(kv, "loss"),
            retx_bp: kv_get(kv, "retx"),
            min_rtt_us: kv_get(kv, "minrtt"),
            qd_mean_us: kv_get(kv, "qmean"),
            qd_p95_us: kv_get(kv, "qp95"),
            qd_max_us: kv_get(kv, "qmax"),
            settled_rate: kv_get(kv, "rate"),
            peak_rate: kv_get(kv, "peakrate"),
            peak_inflight: kv_get(kv, "inflight"),
            sent_packets: kv_get(kv, "sent"),
            rtos: kv_get(kv, "rtos"),
            blocked: kv_get(kv, "blocked"),
        }
    }

    /// "lost 0.4%, resent 0.4%, ..." for the result lines.
    pub fn text(&self) -> String {
        let ms = |us: u64| us as f64 / 1000.0;
        format!(
            "lost {:.2}%, resent {:.2}%, round trip {:.0} ms when idle and {:.0} ms more under load (95% of the time under +{:.0}, at worst +{:.0}), rate settled at {}, {} datagrams in flight at most, {} timeouts, {:.1} s",
            self.loss_bp as f64 / 100.0,
            self.retx_bp as f64 / 100.0,
            ms(self.min_rtt_us),
            ms(self.qd_mean_us),
            ms(self.qd_p95_us),
            ms(self.qd_max_us),
            rate_text(Some(self.settled_rate as f64)),
            self.peak_inflight,
            self.rtos,
            self.elapsed_ms as f64 / 1000.0,
        )
    }
}

/// One direction of a datagram transfer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DgramSide {
    pub ok: bool,
    /// Why it did not work (empty when it did).
    pub why: String,
    pub bytes: u64,
    /// Bytes per second as the receiver timed it, first datagram to last.
    pub rate: Option<f64>,
    pub pause_ms: u64,
    /// The sending side's numbers, when they arrived.
    pub sender: Option<SenderSummary>,
    pub ping_peak_ms: Option<u32>,
    /// Stopped after the time limit: the numbers are those of the part that moved.
    pub capped: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DgramResult {
    pub total_bytes: u64,
    pub up: DgramSide,
    pub down: DgramSide,
    pub total_secs: f64,
    pub ping_ms: Option<u32>,
    pub route: String,
    /// Changes of the route during the run ("direct -> through a Steam relay at 12 s").
    pub route_notes: Vec<String>,
    /// What Steam reported while it ran, and the notes about raising and restoring its send rate.
    pub steam: SteamSamples,
    pub rate_notes: Vec<String>,
}

impl DgramResult {
    /// The longest pause in either direction, ms.
    pub fn longest_pause_ms(&self) -> u64 {
        self.up.pause_ms.max(self.down.pause_ms)
    }

    pub fn stalled(&self) -> bool {
        self.longest_pause_ms() > STALL.as_millis() as u64
    }

    /// Seconds for the 80 MB save as raw bytes (no base64 on this path) at the two directions' rates.
    pub fn estimates(&self) -> (Option<f64>, Option<f64>) {
        let secs = |rate: Option<f64>| rate.filter(|r| *r > 0.0).map(|r| SAVE_MB * MB as f64 / r);
        (secs(self.up.rate), secs(self.down.rate))
    }

    fn side_lines(&self, name: &str, who: &str, side: &DgramSide) -> Vec<String> {
        if !side.ok {
            return vec![format!("{name}: FAILED: {}", if side.why.is_empty() { "unknown" } else { &side.why })];
        }
        match &side.sender {
            Some(sum) => vec![format!("{name}: {who} {}", sum.text())],
            None => vec![format!("{name}: arrived intact; the sender's numbers did not come back")],
        }
    }

    /// The lines the window shows for a finished datagram run.
    pub fn lines(&self) -> Vec<String> {
        let mb = self.total_bytes as f64 / MB as f64;
        let route = if self.route.is_empty() { "unknown route" } else { &self.route };
        let ping = self.ping_ms.map_or_else(|| "unknown".into(), |p| format!("{p} ms"));
        let mut out = vec![
            format!(
                "Datagram speed test, {mb:.0} MB each way: you to friend {}, friend to you {}, {:.1} s in all",
                rate_text(self.up.rate),
                rate_text(self.down.rate),
                self.total_secs
            ),
            format!(
                "Route: {route}, round trip {ping}{}",
                if self.route_notes.is_empty() {
                    ", no route change".to_string()
                } else {
                    format!(", ROUTE CHANGED: {}", self.route_notes.join("; "))
                }
            ),
        ];
        out.extend(self.side_lines("You to friend", "you sent:", &self.up));
        out.extend(self.side_lines("Friend to you", "your friend sent:", &self.down));
        out.push(format!(
            "Longest pause {:.1} s, {}",
            self.longest_pause_ms() as f64 / 1000.0,
            if self.stalled() { "STALLED (more than 5 s without data)" } else { "no stall" }
        ));
        let (host, guest) = self.estimates();
        let secs = |e: Option<f64>| e.map_or_else(|| "unknown".to_string(), secs_text);
        out.push(format!(
            "An {SAVE_MB:.0} MB save (datagrams carry it as it is, no base64): about {} if you host, {} if your friend hosts",
            secs(host),
            secs(guest)
        ));
        let steam = self.steam.summary_line();
        if !steam.is_empty() {
            out.push(steam);
        }
        out
    }

    /// One line for the session log.
    pub fn log_line(&self) -> String {
        let (host, guest) = self.estimates();
        let secs = |e: Option<f64>| e.map_or_else(|| "?".to_string(), |s| format!("{s:.0}"));
        let sender =
            |s: &DgramSide| s.sender.as_ref().map_or_else(|| "no sender numbers".to_string(), SenderSummary::text);
        format!(
            "steam speed test (datagrams): {:.0} MB each way, up {} [{}], down {} [{}], total {:.1} s, {}, ping {} ms, route changes: {}, longest pause {} ms{}, 80 MB save about {} s hosting / {} s joining",
            self.total_bytes as f64 / MB as f64,
            rate_text(self.up.rate),
            if self.up.ok { sender(&self.up) } else { format!("FAILED: {}", self.up.why) },
            rate_text(self.down.rate),
            if self.down.ok { sender(&self.down) } else { format!("FAILED: {}", self.down.why) },
            self.total_secs,
            if self.route.is_empty() { "route unknown" } else { &self.route },
            self.ping_ms.map_or_else(|| "?".into(), |p| p.to_string()),
            if self.route_notes.is_empty() { "none".to_string() } else { self.route_notes.join("; ") },
            self.longest_pause_ms(),
            if self.stalled() { " STALL" } else { "" },
            secs(host),
            secs(guest),
        )
    }
}

// ---- Auto (scaled): parallel connections

/// One direction of an `Auto (scaled)` transfer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParSide {
    pub ok: bool,
    /// Why it did not work (empty when it did).
    pub why: String,
    pub bytes: u64,
    /// Bytes per second over the whole transfer as the sender counted them (acknowledged by the receiver).
    pub rate: Option<f64>,
    pub secs: f64,
    /// The connections the size rule asked for, the most that worked at once, how many were opened in all.
    pub target: u32,
    pub used: u32,
    pub opened: u32,
    /// Why the connections stopped growing (or that they did not).
    pub stop: String,
    pub requeued: u64,
    pub ping_peak_ms: Option<u32>,
    pub capped: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ParResult {
    pub total_bytes: u64,
    pub up: ParSide,
    pub down: ParSide,
    pub total_secs: f64,
    pub ping_ms: Option<u32>,
    pub route: String,
    pub route_notes: Vec<String>,
    pub steam: SteamSamples,
    pub rate_notes: Vec<String>,
}

impl ParResult {
    /// Seconds for the 80 MB save at the two directions' rates (no base64 on this path).
    pub fn estimates(&self) -> (Option<f64>, Option<f64>) {
        let secs = |rate: Option<f64>| rate.filter(|r| *r > 0.0).map(|r| SAVE_MB * MB as f64 / r);
        (secs(self.up.rate), secs(self.down.rate))
    }

    fn side_line(name: &str, side: &ParSide) -> String {
        if !side.ok {
            return format!("{name}: FAILED: {}", if side.why.is_empty() { "unknown" } else { &side.why });
        }
        format!(
            "{name}: {} connection(s) (the rule asked for {}, {} opened), {} in {:.1} s{}; {}",
            side.used,
            side.target,
            side.opened,
            rate_text(side.rate),
            side.secs,
            if side.requeued > 0 {
                format!(", {} chunk(s) re-sent after a connection died", side.requeued)
            } else {
                String::new()
            },
            side.stop
        )
    }

    /// The lines the window shows for a finished `Auto (scaled)` run.
    pub fn lines(&self) -> Vec<String> {
        let mb = self.total_bytes as f64 / MB as f64;
        let route = if self.route.is_empty() { "unknown route" } else { &self.route };
        let ping = self.ping_ms.map_or_else(|| "unknown".into(), |p| format!("{p} ms"));
        let mut out = vec![
            format!(
                "Auto (scaled) speed test, {mb:.0} MB each way: you to friend {} on {} connection(s), friend to you {} on {}, {:.1} s in all",
                rate_text(self.up.rate),
                self.up.used,
                rate_text(self.down.rate),
                self.down.used,
                self.total_secs
            ),
            format!(
                "Route: {route}, round trip {ping}{}",
                if self.route_notes.is_empty() {
                    ", no route change".to_string()
                } else {
                    format!(", ROUTE CHANGED: {}", self.route_notes.join("; "))
                }
            ),
            Self::side_line("You to friend", &self.up),
            Self::side_line("Friend to you", &self.down),
        ];
        let (host, guest) = self.estimates();
        let secs = |e: Option<f64>| e.map_or_else(|| "unknown".to_string(), secs_text);
        let n80 = crate::steamnet::par::ParConfig::default().conns_for((SAVE_MB * MB as f64) as u64);
        out.push(format!(
            "An {SAVE_MB:.0} MB save (carried as it is, no base64, on up to {n80} connections): about {} if you host, {} if your friend hosts",
            secs(host),
            secs(guest)
        ));
        let steam = self.steam.summary_line();
        if !steam.is_empty() {
            out.push(steam);
        }
        out
    }

    /// One line for the session log.
    pub fn log_line(&self) -> String {
        let (host, guest) = self.estimates();
        let secs = |e: Option<f64>| e.map_or_else(|| "?".to_string(), |s| format!("{s:.0}"));
        let side = |s: &ParSide| {
            if s.ok {
                format!("{} conns of {} wanted, {}", s.used, s.target, s.stop)
            } else {
                format!("FAILED: {}", s.why)
            }
        };
        format!(
            "steam speed test (auto scaled): {:.0} MB each way, up {} [{}], down {} [{}], total {:.1} s, {}, ping {} ms, route changes: {}, 80 MB save about {} s hosting / {} s joining",
            self.total_bytes as f64 / MB as f64,
            rate_text(self.up.rate),
            side(&self.up),
            rate_text(self.down.rate),
            side(&self.down),
            self.total_secs,
            if self.route.is_empty() { "route unknown" } else { &self.route },
            self.ping_ms.map_or_else(|| "?".into(), |p| p.to_string()),
            if self.route_notes.is_empty() { "none".to_string() } else { self.route_notes.join("; ") },
            secs(host),
            secs(guest),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_through_their_headers() {
        let all = [
            Msg::Up { id: 7, total: 32_000_000, chunk: 349_525, conns: 1 },
            Msg::Up { id: 7, total: 32_000_000, chunk: 349_525, conns: 4 },
            Msg::DgDown { id: 9, total: 80_000_000 },
            Msg::Dres { id: 9, kv: vec![("bytes".into(), 80_000_000), ("ms".into(), 17_000), ("ok".into(), 1)] },
            Msg::Dsend { id: 9, kv: vec![] },
            Msg::Down { id: 7, total: 32_000_000, chunk: 349_525, conns: 2, rate: 0 },
            Msg::Down { id: 7, total: 32_000_000, chunk: 349_525, conns: 1, rate: 4_194_304 },
            Msg::Stop { id: 7 },
            Msg::RateAck { id: 7, asked: 4_194_304, got: 4_194_304 },
            Msg::End { id: 7, sent: 32_000_000, peak_pending: 524_288, max_block_ms: 812 },
            Msg::Prog { id: 7, bytes: 1_048_576 },
            Msg::UpRes { id: 7, bytes: 32_000_000, ms: 17_500, gap_ms: 900, steady: 31_650_475 },
            Msg::ParUp { id: 7, total: 80_000_000 },
            Msg::ParReady { id: 7 },
            Msg::ParDown { id: 7, total: 80_000_000 },
            Msg::ParRes { id: 7, kv: vec![("ok".into(), 1), ("bytes".into(), 80_000_000), ("lanes".into(), 7)] },
            Msg::ParSend {
                id: 7,
                kv: vec![("ok".into(), 1), ("used".into(), 8), ("target".into(), 8)],
                text: "all 8 connections were opened".into(),
            },
        ];
        for m in all {
            assert_eq!(parse(&encode(&m)), Some(m.clone()), "{m:?}");
        }
        let d = data_message(7, 3, 1000);
        assert_eq!(parse(&d), Some(Msg::Data { id: 7, seq: 3, len: 1000 }));
    }

    #[test]
    fn garbage_and_pings_are_not_speed_messages() {
        for bad in [
            &b"ping 123"[..],
            b"tput",
            b"tput up 1 2\n",
            b"tput up x 2 3\n",
            b"tput nope 1\n",
            b"tput up 1 2 3",
            b"",
            b"\xff\xfe",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
        // a header that never ends within the limit
        let mut long = b"tput up ".to_vec();
        long.extend(std::iter::repeat_n(b'1', 500));
        long.push(b'\n');
        assert_eq!(parse(&long), None);
        // a peak that does not fit a u32
        assert_eq!(parse(b"tput end 1 2 99999999999 4\n"), None);
    }

    #[test]
    fn data_is_exact_in_size_and_does_not_compress() {
        let m = data_message(1, 0, CHUNK_PAYLOAD);
        let header = encode(&Msg::Data { id: 1, seq: 0, len: 0 }).len();
        assert_eq!(m.len(), header + CHUNK_PAYLOAD);
        assert_eq!(CHUNK_PAYLOAD, 349_524);
        // a different chunk is different data
        assert_ne!(data_message(1, 0, 64), data_message(1, 1, 64));
        // random bytes use every value about equally
        let mut seen = [0u32; 256];
        for b in &m[header..] {
            seen[*b as usize] += 1;
        }
        let (min, max) = (seen.iter().min().unwrap(), seen.iter().max().unwrap());
        assert!(*min > 1000 && *max < 1800, "{min} {max}");
        // a length that is no multiple of 8
        assert_eq!(data_message(2, 9, 13).len(), encode(&Msg::Data { id: 2, seq: 9, len: 0 }).len() + 13);
    }

    #[test]
    fn chunks_are_counted_like_the_resync_does() {
        assert_eq!(chunk_count(0, 100), 0);
        assert_eq!(chunk_count(100, 100), 1);
        assert_eq!(chunk_count(101, 100), 2);
        assert_eq!(chunk_count(10, 0), 0);
        assert_eq!(chunk_count(32 * MB, CHUNK_PAYLOAD as u64), 92);
    }

    #[test]
    fn the_receiver_times_from_the_first_chunk_and_finds_the_longest_gap() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut r = Receiver::new(1, 4000, t0);
        assert_eq!(r.rate(), None);
        r.on_chunk(at(300), 1000); // 300 ms until the first chunk counts as a gap
        assert_eq!(r.rate(), None, "one chunk cannot be timed");
        r.on_chunk(at(400), 1000);
        r.on_chunk(at(1400), 1000); // a pause of 1 s
        r.on_chunk(at(1500), 1000);
        assert_eq!((r.got, r.chunks, r.steady(), r.span_ms()), (4000, 4, 3000, 1200));
        assert_eq!(r.max_gap_ms(), 1000);
        assert_eq!(r.rate(), Some(2500.0));
        assert!(!r.progress_due(at(100)));
        assert!(r.progress_due(at(600)));
        assert!(!r.progress_due(at(700)));
        assert!(r.progress_due(at(1100)));
    }

    #[test]
    fn a_slow_start_counts_as_a_pause() {
        let t0 = Instant::now();
        let mut r = Receiver::new(1, 10, t0);
        r.on_chunk(t0 + Duration::from_secs(7), 10);
        assert_eq!(r.max_gap_ms(), 7000);
    }

    #[test]
    fn rates_and_estimates_are_plain_arithmetic() {
        assert_eq!(rate_of(1_000_000, 500), Some(2_000_000.0));
        assert_eq!(rate_of(0, 500), None);
        assert_eq!(rate_of(10, 0), None);
        // 80 MB of save is 106.67 MB on the wire: at 1 MiB/s about 102 s, at 4 MB/s about 27 s
        let slow = estimate_secs(80.0, Some(1_048_576.0)).unwrap();
        assert!((slow - 101.7).abs() < 0.2, "{slow}");
        let fast = estimate_secs(80.0, Some(4_000_000.0)).unwrap();
        assert!((fast - 26.67).abs() < 0.01, "{fast}");
        assert_eq!(estimate_secs(80.0, None), None);
        assert_eq!(estimate_secs(80.0, Some(0.0)), None);
        assert_eq!(secs_text(58.4), "58 s");
        assert_eq!(secs_text(125.0), "2 min 5 s");
        assert_eq!(rate_text(Some(1_843_000.0)), "1.84 MB/s");
        assert_eq!(rate_text(None), "unknown");
    }

    fn result() -> SpeedResult {
        SpeedResult {
            total_bytes: 32 * MB,
            up: Direction {
                bytes: 32 * MB,
                rate: Some(1_800_000.0),
                secs: 17.8,
                peak_pending: 524_288,
                longest_pause_ms: 400,
                ..Direction::default()
            },
            down: Direction {
                bytes: 32 * MB,
                rate: Some(2_100_000.0),
                secs: 15.2,
                peak_pending: 400_000,
                longest_pause_ms: 900,
                ..Direction::default()
            },
            total_secs: 34.2,
            ping_ms: Some(199),
            route: "through a Steam relay".into(),
            rate_set: 1_048_576,
            rate_effective: Some(1_048_576),
            conns: 1,
            route_notes: Vec::new(),
            rate_note: String::new(),
            steam_line: String::new(),
        }
    }

    #[test]
    fn the_result_reads_as_four_lines() {
        let r = result();
        let lines = r.lines();
        assert_eq!(lines.len(), 4);
        assert_eq!(
            lines[0],
            "Speed test, 32 MB each way: you to friend 1.80 MB/s, friend to you 2.10 MB/s, 34.2 s in all"
        );
        assert_eq!(lines[1], "Route: through a Steam relay, round trip 199 ms, send rate 1048576 B/s");
        assert_eq!(lines[2], "Queue peak: 512 KiB sending, 391 KiB receiving; longest pause 0.9 s, no stall");
        assert_eq!(
            lines[3],
            "An 80 MB save (about 107 MB over Steam): about 59 s if you host, 51 s if your friend hosts"
        );
        assert!(!r.stalled());
    }

    #[test]
    fn a_pause_over_five_seconds_is_a_stall_and_a_changed_rate_is_shown() {
        let mut r = result();
        r.down.longest_pause_ms = 5001;
        r.rate_effective = Some(262_144);
        assert!(r.stalled());
        let lines = r.lines();
        assert!(lines[2].contains("STALLED"), "{lines:?}");
        assert!(lines[1].contains("send rate 262144 B/s (asked for 1048576)"), "{lines:?}");
        r.down.longest_pause_ms = 5000;
        assert!(!r.stalled(), "exactly 5 s is not yet a stall");
    }

    #[test]
    fn the_log_line_is_one_line_with_the_numbers() {
        let line = result().log_line();
        assert!(!line.contains('\n'));
        for part in [
            "32 MB each way",
            "up 1.80 MB/s",
            "down 2.10 MB/s",
            "total 34.2 s",
            "through a Steam relay",
            "ping 199 ms",
            "send rate 1048576 (asked 1048576)",
            "queue peak 512/390 KiB",
            "longest pause 900 ms",
            "about 59 s hosting / 51 s joining",
        ] {
            assert!(line.contains(part), "{part} missing in {line}");
        }
        let mut r = result();
        r.up.rate = None;
        r.down.longest_pause_ms = 9000;
        let line = r.log_line();
        assert!(line.contains("up unknown") && line.contains("STALL") && line.contains("? s hosting"), "{line}");
    }
}
