//! The receiving end of a bulk transfer: collects datagrams in any order, drops duplicates and anything out of range,
//! acknowledges with selective acks, checks the whole payload against the offered SHA-256 and says so. No Steam and no
//! threads in here: the caller feeds it parsed datagrams and the clock, and sends what [`Receiver::poll`] returns.

use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

use super::wire::{self, reason, Ack, Offer, Packet, MAX_RANGES, MIN_PAYLOAD, PAYLOAD};

/// An ack goes out after this many datagrams arrived...
pub const ACK_EVERY: u32 = 8;
/// ...or this long after the first one that is not acknowledged yet.
pub const ACK_DELAY: Duration = Duration::from_millis(5);
/// While incomplete and quiet, the ack is repeated this often (it may have been lost).
pub const ACK_KEEPALIVE: Duration = Duration::from_millis(50);
/// A finished receiver keeps answering the sender this long, because the sender's copy of `done` may be lost.
pub const LINGER: Duration = Duration::from_secs(4);
/// Everything above the cumulative ack is listed again after this long.
const REPORT_RESET: Duration = Duration::from_millis(400);
/// `done` is repeated this often while the sender has not stopped talking.
const DONE_REPEAT: Duration = Duration::from_millis(250);

/// What a receiver accepts.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest transfer, bytes. An offer above it is refused; memory use never exceeds it.
    pub max_total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverState {
    Receiving,
    /// Every datagram arrived; `ok` says whether the SHA-256 matched.
    Complete {
        ok: bool,
    },
    /// The sender gave up (abort).
    Aborted,
}

#[derive(Debug, Clone, Default)]
pub struct RecvStats {
    /// Distinct datagrams stored.
    pub unique: u64,
    pub duplicates: u64,
    /// Datagrams of this transfer with a sequence number or size that cannot be right.
    pub rejected: u64,
    pub bytes: u64,
    pub acks_sent: u64,
}

pub struct Receiver {
    offer: Offer,
    packets: u32,
    data: Vec<u8>,
    have: Vec<u64>,
    /// Packets already listed in an ack we sent; ranges with news are listed first, so a window with more holes than an
    /// ack has room for still gets every range to the sender over a few acks.
    reported: Vec<u64>,
    reported_reset: Instant,
    cum: u32,
    high: u32,
    state: ReceiverState,
    /// Sender clock of the newest datagram and when it arrived: echoed in the next ack.
    echo: Option<(u32, Instant)>,
    unacked: u32,
    first_unacked: Option<Instant>,
    last_ack: Instant,
    ack_now: bool,
    done_due: bool,
    last_done: Option<Instant>,
    finished_at: Option<Instant>,
    started: Instant,
    first_data: Option<Instant>,
    last_data: Instant,
    max_gap: Duration,
    abort_to_send: Option<u8>,
    pub stats: RecvStats,
}

impl Receiver {
    /// Takes an offer. `Err(reason)` is the abort reason to answer with.
    pub fn accept(now: Instant, offer: &Offer, limits: &Limits) -> Result<Receiver, u8> {
        let packets = offer.packets().ok_or(reason::REFUSED)?;
        let payload = usize::from(offer.payload);
        if !(MIN_PAYLOAD..=PAYLOAD).contains(&payload) {
            return Err(reason::REFUSED);
        }
        if offer.total > limits.max_total {
            return Err(reason::TOO_BIG);
        }
        Ok(Receiver {
            offer: offer.clone(),
            packets,
            data: Vec::new(),
            have: vec![0; (packets as usize).div_ceil(64)],
            reported: vec![0; (packets as usize).div_ceil(64)],
            reported_reset: now,
            cum: 0,
            high: 0,
            state: ReceiverState::Receiving,
            echo: Some((offer.ts, now)),
            unacked: 0,
            first_unacked: None,
            last_ack: now,
            ack_now: true,
            done_due: false,
            last_done: None,
            finished_at: None,
            started: now,
            first_data: None,
            last_data: now,
            max_gap: Duration::ZERO,
            abort_to_send: None,
            stats: RecvStats::default(),
        })
    }

    pub fn xfer(&self) -> u32 {
        self.offer.xfer
    }

    pub fn ctx(&self) -> u32 {
        self.offer.ctx
    }

    pub fn total(&self) -> u64 {
        self.offer.total
    }

    pub fn sha256(&self) -> [u8; 32] {
        self.offer.sha256
    }

    pub fn state(&self) -> ReceiverState {
        self.state
    }

    /// Bytes stored so far (distinct datagrams).
    pub fn received_bytes(&self) -> u64 {
        self.stats.bytes
    }

    /// First datagram to last, milliseconds.
    pub fn span_ms(&self) -> u64 {
        self.first_data.map_or(0, |f| self.last_data.saturating_duration_since(f).as_millis() as u64)
    }

    /// The longest time nothing new arrived, counting the wait for the first datagram.
    pub fn max_gap_ms(&self) -> u64 {
        self.max_gap.as_millis() as u64
    }

    /// Seconds since the offer.
    pub fn since_start(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.started).as_secs_f64()
    }

    /// The verified payload, once, when the transfer completed with a matching hash.
    pub fn take_data(&mut self) -> Option<Vec<u8>> {
        match self.state {
            ReceiverState::Complete { ok: true } => Some(std::mem::take(&mut self.data)),
            _ => None,
        }
    }

    /// Whether this receiver has nothing more to do and may be dropped.
    pub fn is_over(&self, now: Instant) -> bool {
        match self.state {
            ReceiverState::Receiving => false,
            ReceiverState::Aborted => true,
            ReceiverState::Complete { .. } => {
                self.finished_at.is_some_and(|t| now.saturating_duration_since(t) >= LINGER)
            }
        }
    }

    fn is_set(&self, seq: u32) -> bool {
        self.have.get(seq as usize / 64).is_some_and(|w| w >> (seq % 64) & 1 == 1)
    }

    fn expected_len(&self, seq: u32) -> usize {
        let payload = u64::from(self.offer.payload);
        if seq + 1 < self.packets {
            payload as usize
        } else {
            (self.offer.total - u64::from(self.packets - 1) * payload) as usize
        }
    }

    /// Feeds one parsed datagram. Datagrams of other transfers are ignored.
    pub fn on_packet(&mut self, now: Instant, packet: &Packet<'_>) {
        if packet.xfer() != self.offer.xfer {
            return;
        }
        match packet {
            Packet::Offer(o) => {
                // the sender did not get our first ack
                let same = o.ctx == self.offer.ctx
                    && o.total == self.offer.total
                    && o.payload == self.offer.payload
                    && o.sha256 == self.offer.sha256;
                if same {
                    self.echo = Some((o.ts, now));
                    self.ack_now = true;
                    self.done_due = matches!(self.state, ReceiverState::Complete { .. });
                }
            }
            Packet::Data { seq, ts, payload, .. } => self.on_data(now, *seq, *ts, payload),
            Packet::Fin { .. } => self.done_due = matches!(self.state, ReceiverState::Complete { .. }),
            Packet::Abort { .. } => {
                if self.state == ReceiverState::Receiving {
                    self.state = ReceiverState::Aborted;
                }
            }
            Packet::Ack(_) | Packet::Done { .. } => {}
        }
    }

    fn on_data(&mut self, now: Instant, seq: u32, ts: u32, payload: &[u8]) {
        if matches!(self.state, ReceiverState::Complete { .. }) {
            // the sender still sends: it has not seen our done
            self.done_due = true;
            return;
        }
        if self.state != ReceiverState::Receiving {
            return;
        }
        if seq >= self.packets || payload.len() != self.expected_len(seq) {
            self.stats.rejected += 1;
            return;
        }
        self.echo = Some((ts, now));
        self.unacked += 1;
        self.first_unacked.get_or_insert(now);
        if self.is_set(seq) {
            self.stats.duplicates += 1;
            return;
        }
        let start = seq as usize * usize::from(self.offer.payload);
        let end = start + payload.len();
        if self.data.len() < end {
            self.data.resize(end, 0);
        }
        self.data[start..end].copy_from_slice(payload);
        self.have[seq as usize / 64] |= 1 << (seq % 64);
        self.stats.unique += 1;
        self.stats.bytes += payload.len() as u64;
        self.high = self.high.max(seq + 1);
        match self.first_data {
            None => {
                self.first_data = Some(now);
                self.max_gap = self.max_gap.max(now.saturating_duration_since(self.started));
            }
            Some(_) => self.max_gap = self.max_gap.max(now.saturating_duration_since(self.last_data)),
        }
        self.last_data = now;
        while self.cum < self.packets && self.is_set(self.cum) {
            self.cum += 1;
        }
        if self.cum == self.packets {
            self.complete(now);
        }
    }

    fn complete(&mut self, now: Instant) {
        self.data.truncate(self.offer.total as usize);
        let digest = Sha256::digest(&self.data);
        let ok = digest.as_slice() == self.offer.sha256;
        self.state = ReceiverState::Complete { ok };
        self.finished_at = Some(now);
        self.done_due = true;
        self.ack_now = true;
        if !ok {
            self.data = Vec::new();
        }
    }

    fn is_reported(&self, seq: u32) -> bool {
        self.reported.get(seq as usize / 64).is_some_and(|w| w >> (seq % 64) & 1 == 1)
    }

    /// Selective ack ranges above `cum`, ascending: at most [`MAX_RANGES`], those with packets the sender was not told
    /// about first, then the lowest of the rest. Marks what it lists as reported.
    fn ranges(&mut self, now: Instant) -> Vec<(u32, u32)> {
        if now.saturating_duration_since(self.reported_reset) >= REPORT_RESET {
            // an ack may have been lost: tell it all again, a few ranges per ack
            self.reported.iter_mut().for_each(|w| *w = 0);
            self.reported_reset = now;
        }
        let mut all: Vec<(u32, u32, bool)> = Vec::new();
        let mut seq = self.cum + 1;
        while seq < self.high {
            if self.is_set(seq) {
                let start = seq;
                let mut news = false;
                while seq < self.high && self.is_set(seq) {
                    news |= !self.is_reported(seq);
                    seq += 1;
                }
                all.push((start, seq - start, news));
            } else {
                seq += 1;
            }
        }
        let mut chosen: Vec<(u32, u32)> = all.iter().filter(|r| r.2).take(MAX_RANGES).map(|r| (r.0, r.1)).collect();
        if chosen.len() < MAX_RANGES {
            let room = MAX_RANGES - chosen.len();
            chosen.extend(all.iter().filter(|r| !r.2).take(room).map(|r| (r.0, r.1)));
            chosen.sort_unstable();
        }
        for &(start, len) in &chosen {
            for seq in start..start + len {
                self.reported[seq as usize / 64] |= 1 << (seq % 64);
            }
        }
        chosen
    }

    /// Builds the datagrams due now: acks, `done`, an abort. The caller sends them (losing one is fine).
    pub fn poll(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) {
        if let Some(code) = self.abort_to_send.take() {
            out.push(wire::encode_abort(self.offer.xfer, code));
        }
        match self.state {
            ReceiverState::Receiving => {
                let waited = self.first_unacked.map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
                let quiet = now.saturating_duration_since(self.last_ack);
                let due = self.ack_now
                    || self.unacked >= ACK_EVERY
                    || (self.unacked > 0 && waited >= ACK_DELAY)
                    || (self.cum < self.packets && quiet >= ACK_KEEPALIVE);
                if due {
                    self.send_ack(now, out);
                }
            }
            ReceiverState::Complete { ok } => {
                let repeat = self.last_done.is_none_or(|t| now.saturating_duration_since(t) >= DONE_REPEAT);
                if self.done_due && repeat {
                    self.done_due = false;
                    self.last_done = Some(now);
                    out.push(wire::encode_done(self.offer.xfer, ok));
                }
            }
            ReceiverState::Aborted => {}
        }
    }

    fn send_ack(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) {
        let (ts_echo, hold_us) = match self.echo {
            Some((ts, at)) => (ts, now.saturating_duration_since(at).as_micros().min(u128::from(u32::MAX)) as u32),
            None => (0, 0),
        };
        let ranges = self.ranges(now);
        let ack = Ack { xfer: self.offer.xfer, cum: self.cum, ts_echo, hold_us, ranges };
        out.push(wire::encode_ack(&ack));
        self.stats.acks_sent += 1;
        self.unacked = 0;
        self.first_unacked = None;
        self.last_ack = now;
        self.ack_now = false;
    }

    /// Tells the sender to stop (the local side gives up).
    pub fn abort(&mut self, code: u8) {
        if self.state == ReceiverState::Receiving {
            self.state = ReceiverState::Aborted;
            self.abort_to_send = Some(code);
        }
    }

    /// The next time `poll` has something to do, as a wait from `now`.
    pub fn wait_hint(&self, now: Instant) -> Duration {
        match self.state {
            ReceiverState::Receiving => {
                if self.ack_now || self.unacked >= ACK_EVERY {
                    return Duration::ZERO;
                }
                let mut wait = ACK_KEEPALIVE.saturating_sub(now.saturating_duration_since(self.last_ack));
                if let Some(t) = self.first_unacked {
                    wait = wait.min(ACK_DELAY.saturating_sub(now.saturating_duration_since(t)));
                }
                wait
            }
            ReceiverState::Complete { .. } => {
                if self.done_due {
                    Duration::ZERO
                } else {
                    Duration::from_millis(50)
                }
            }
            ReceiverState::Aborted => Duration::from_millis(50),
        }
    }
}
