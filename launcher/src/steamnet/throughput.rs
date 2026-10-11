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
//! | `tput up <id> <total> <chunk>` | tester | N bytes follow in data messages; time them |
//! | `tput d <id> <seq>` + payload | both | one chunk |
//! | `tput end <id> <sent> <peak_pending> <max_block_ms>` | sender | the last chunk is out; the sender's numbers |
//! | `tput prog <id> <bytes>` | receiver | bytes received so far (about every 500 ms) |
//! | `tput upres <id> <bytes> <ms> <gap_ms> <steady>` | friend | the upload arrived: the friend's own timing |
//! | `tput down <id> <total> <chunk>` | tester | now send N bytes to me |

use super::{CoreRef, Handle};
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
const HEADER_LIMIT: usize = 120;

// ---- messages

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Up { id: u64, total: u64, chunk: u64 },
    Down { id: u64, total: u64, chunk: u64 },
    Data { id: u64, seq: u64, len: usize },
    End { id: u64, sent: u64, peak_pending: u32, max_block_ms: u64 },
    Prog { id: u64, bytes: u64 },
    UpRes { id: u64, bytes: u64, ms: u64, gap_ms: u64, steady: u64 },
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
        "up" => Msg::Up { id: num()?, total: num()?, chunk: num()? },
        "down" => Msg::Down { id: num()?, total: num()?, chunk: num()? },
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
        Msg::Up { id, total, chunk } => format!("tput up {id} {total} {chunk}\n"),
        Msg::Down { id, total, chunk } => format!("tput down {id} {total} {chunk}\n"),
        Msg::Data { id, seq, .. } => format!("tput d {id} {seq}\n"),
        Msg::End { id, sent, peak_pending, max_block_ms } => {
            format!("tput end {id} {sent} {peak_pending} {max_block_ms}\n")
        }
        Msg::Prog { id, bytes } => format!("tput prog {id} {bytes}\n"),
        Msg::UpRes { id, bytes, ms, gap_ms, steady } => format!("tput upres {id} {bytes} {ms} {gap_ms} {steady}\n"),
    };
    text.into_bytes()
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
        }
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

/// Sends `total` bytes in chunks of `chunk` bytes, then the end message. Blocks while Steam's buffer is full, which
/// is how the link's speed paces it. Runs on a thread of its own; `cancelled` is asked before every chunk.
pub fn stream(
    core: &CoreRef,
    conn: Handle,
    id: u64,
    total: u64,
    chunk: u64,
    cancelled: &dyn Fn() -> bool,
) -> SendStats {
    let start = Instant::now();
    let mut stats = SendStats::default();
    let chunk = chunk.clamp(1, CHUNK_PAYLOAD as u64);
    let mut seq = 0u64;
    while stats.sent < total {
        if cancelled() {
            stats.error = Some("stopped".into());
            break;
        }
        let len = (total - stats.sent).min(chunk) as usize;
        let message = data_message(id, seq, len);
        let before = Instant::now();
        if let Err(e) = core.send_reliable(conn, &message, SEND_TIMEOUT) {
            stats.error = Some(e.to_string());
            break;
        }
        stats.max_block_ms = stats.max_block_ms.max(before.elapsed().as_millis() as u64);
        stats.sent += len as u64;
        seq += 1;
        if let Some(link) = core.link(conn) {
            stats.peak_pending = stats.peak_pending.max(link.pending);
        }
    }
    stats.elapsed_ms = start.elapsed().as_millis() as u64;
    if stats.error.is_none() {
        let end = encode(&Msg::End {
            id,
            sent: stats.sent,
            peak_pending: stats.peak_pending,
            max_block_ms: stats.max_block_ms,
        });
        if let Err(e) = core.send_reliable(conn, &end, SEND_TIMEOUT) {
            stats.error = Some(e.to_string());
        }
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
        let mut out = vec![
            format!(
                "Speed test, {mb:.0} MB each way: you to friend {}, friend to you {}, {:.1} s in all",
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
        out
    }

    /// One line for the session log.
    pub fn log_line(&self) -> String {
        let (host, guest) = self.estimates();
        let secs = |e: Option<f64>| e.map_or_else(|| "?".to_string(), |s| format!("{s:.0}"));
        format!(
            "steam speed test: {:.0} MB each way, up {}, down {}, total {:.1} s, {}, ping {} ms, send rate {} (asked {}), queue peak {}/{} KiB, longest pause {} ms{}, 80 MB save about {} s hosting / {} s joining",
            self.total_bytes as f64 / MB as f64,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_through_their_headers() {
        let all = [
            Msg::Up { id: 7, total: 32_000_000, chunk: 349_525 },
            Msg::Down { id: 7, total: 32_000_000, chunk: 349_525 },
            Msg::End { id: 7, sent: 32_000_000, peak_pending: 524_288, max_block_ms: 812 },
            Msg::Prog { id: 7, bytes: 1_048_576 },
            Msg::UpRes { id: 7, bytes: 32_000_000, ms: 17_500, gap_ms: 900, steady: 31_650_475 },
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
        long.extend(std::iter::repeat(b'1').take(500));
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
            },
            down: Direction {
                bytes: 32 * MB,
                rate: Some(2_100_000.0),
                secs: 15.2,
                peak_pending: 400_000,
                longest_pause_ms: 900,
            },
            total_secs: 34.2,
            ping_ms: Some(199),
            route: "through a Steam relay".into(),
            rate_set: 1_048_576,
            rate_effective: Some(1_048_576),
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
