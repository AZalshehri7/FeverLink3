//! The sending end of a bulk transfer: numbers the datagrams, paces them at the rate of the controller
//! ([`super::cc`]), retransmits only what is lost, and finishes when the receiver says its SHA-256 matched. No Steam and
//! no threads: the caller feeds it parsed datagrams and the clock and sends what [`Sender::poll`] hands out.
//!
//! Loss detection is time-based (RACK): a datagram is lost when one that was sent later than it by more than the
//! reordering allowance was acknowledged, or when nothing was acknowledged for a retransmission timeout. Lost
//! datagrams go out again before new ones, at the pace of the controller, not in a burst.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use super::cc::{Cc, CcConfig, Phase};
use super::wire::{self, reason, Ack, Offer, Packet, DATAGRAM_SIZE, MIN_PAYLOAD, PAYLOAD};

/// The bytes being sent, readable at any offset again (a lost datagram is read twice).
#[allow(clippy::len_without_is_empty)]
pub trait Source: Send {
    fn len(&self) -> u64;
    fn read(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()>;
}

/// A source in memory.
pub struct MemSource(pub std::sync::Arc<Vec<u8>>);

impl Source for MemSource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read(&mut self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        let end = start.checked_add(out.len()).filter(|e| *e <= self.0.len());
        match end {
            Some(end) => {
                out.copy_from_slice(&self.0[start..end]);
                Ok(())
            }
            None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past the end")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// Payload bytes per full datagram.
    pub payload: usize,
    pub cc: CcConfig,
    /// Most datagrams on the way (unacknowledged) at once, whatever the rate says.
    pub max_inflight: usize,
    /// The transfer fails when nothing is acknowledged for this long.
    pub stall: Duration,
}

impl Default for SenderConfig {
    fn default() -> SenderConfig {
        SenderConfig { payload: PAYLOAD, cc: CcConfig::default(), max_inflight: 20_000, stall: Duration::from_secs(10) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderState {
    /// The offer is out; nothing is known about the receiver yet.
    Offering,
    Sending,
    /// Everything is acknowledged; waiting for the receiver's verdict on the hash.
    Draining,
    Done,
    Failed(String),
}

#[derive(Debug, Clone, Default)]
pub struct SenderStats {
    pub sent_packets: u64,
    pub sent_bytes: u64,
    pub retransmits: u64,
    /// Datagrams declared lost (a datagram lost twice counts twice).
    pub lost: u64,
    pub rtos: u64,
    pub acked_bytes: u64,
    pub peak_inflight: u64,
    /// Sends the wire refused (Steam's buffer was full).
    pub blocked: u64,
    pub bogus_acks: u64,
    pub qd_sum: f64,
    pub qd_count: u64,
    pub qd_max: f64,
    /// Queueing delay samples in microseconds (bounded).
    pub qd_samples: Vec<u32>,
    pub min_rtt: Option<Duration>,
    pub srtt: Option<Duration>,
    pub rate_final: f64,
    pub rate_peak: f64,
    rate_acc: f64,
    rate_time: f64,
    pub startup_exit: Option<String>,
    pub elapsed: Duration,
}

impl SenderStats {
    /// Percent of datagrams sent that were declared lost.
    pub fn loss_percent(&self) -> f64 {
        if self.sent_packets == 0 {
            0.0
        } else {
            100.0 * self.lost as f64 / self.sent_packets as f64
        }
    }

    pub fn retransmit_percent(&self) -> f64 {
        if self.sent_packets == 0 {
            0.0
        } else {
            100.0 * self.retransmits as f64 / self.sent_packets as f64
        }
    }

    /// The pacing rate averaged over the time after the start-up phase, bytes per second.
    pub fn settled_rate(&self) -> f64 {
        if self.rate_time > 0.2 {
            self.rate_acc / self.rate_time
        } else {
            self.rate_final
        }
    }

    pub fn queue_delay_mean_ms(&self) -> f64 {
        if self.qd_count == 0 {
            0.0
        } else {
            self.qd_sum / self.qd_count as f64 * 1000.0
        }
    }

    /// The `p`th percentile (0 to 1) of the queueing delay, milliseconds.
    pub fn queue_delay_percentile_ms(&self, p: f64) -> f64 {
        if self.qd_samples.is_empty() {
            return 0.0;
        }
        let mut v = self.qd_samples.clone();
        v.sort_unstable();
        let i = ((v.len() - 1) as f64 * p).round() as usize;
        f64::from(v[i]) / 1000.0
    }
}

const QD_SAMPLE_LIMIT: usize = 400_000;

#[derive(Debug, Clone)]
struct Slot {
    acked: bool,
    lost: bool,
    tx: u8,
    last_tx: Instant,
}

pub struct Sender {
    offer: Offer,
    packets: u32,
    source: Box<dyn Source>,
    cfg: SenderConfig,
    cc: Cc,
    state: SenderState,
    started: Instant,
    slots: VecDeque<Slot>,
    base: u32,
    next_new: u32,
    retx: VecDeque<u32>,
    inflight: usize,
    tokens: f64,
    last_poll: Instant,
    rack_ts: Option<Instant>,
    last_progress: Instant,
    last_rto: Instant,
    rto_backoff: u32,
    offer_next: Instant,
    offer_tries: u32,
    fin_next: Instant,
    abort_to_send: Option<u8>,
    scratch: Vec<u8>,
    pub stats: SenderStats,
}

impl Sender {
    /// `sha256` is the digest of the whole payload (the receiver checks it); `ctx` rides in the offer.
    pub fn new(
        now: Instant,
        xfer: u32,
        ctx: u32,
        sha256: [u8; 32],
        source: Box<dyn Source>,
        cfg: SenderConfig,
    ) -> Result<Sender, String> {
        let total = source.len();
        let payload = cfg.payload.clamp(MIN_PAYLOAD, PAYLOAD);
        let offer = Offer { xfer, ts: 0, ctx, total, payload: payload as u16, sha256 };
        let packets = offer.packets().ok_or_else(|| "nothing to send, or too much".to_string())?;
        let mut cfg = cfg;
        cfg.payload = payload;
        Ok(Sender {
            offer,
            packets,
            source,
            cc: Cc::new(cfg.cc, now),
            cfg,
            state: SenderState::Offering,
            started: now,
            slots: VecDeque::new(),
            base: 0,
            next_new: 0,
            retx: VecDeque::new(),
            inflight: 0,
            tokens: 0.0,
            last_poll: now,
            rack_ts: None,
            last_progress: now,
            last_rto: now,
            rto_backoff: 0,
            offer_next: now,
            offer_tries: 0,
            fin_next: now,
            abort_to_send: None,
            scratch: vec![0; payload],
            stats: SenderStats::default(),
        })
    }

    pub fn xfer(&self) -> u32 {
        self.offer.xfer
    }

    pub fn total(&self) -> u64 {
        self.offer.total
    }

    pub fn state(&self) -> &SenderState {
        &self.state
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, SenderState::Done | SenderState::Failed(_))
    }

    pub fn cc(&self) -> &Cc {
        &self.cc
    }

    pub fn inflight_packets(&self) -> usize {
        self.inflight
    }

    /// Bytes the receiver has acknowledged contiguously.
    pub fn acked_prefix_bytes(&self) -> u64 {
        (u64::from(self.base) * u64::from(self.offer.payload)).min(self.offer.total)
    }

    fn ts(&self, now: Instant) -> u32 {
        now.saturating_duration_since(self.started).as_micros() as u32
    }

    fn len_of(&self, seq: u32) -> usize {
        let payload = u64::from(self.offer.payload);
        if seq + 1 < self.packets {
            payload as usize
        } else {
            (self.offer.total - u64::from(self.packets - 1) * payload) as usize
        }
    }

    fn fail(&mut self, now: Instant, why: impl Into<String>) {
        if !self.is_finished() {
            self.state = SenderState::Failed(why.into());
            self.finish_stats(now);
        }
    }

    fn finish_stats(&mut self, now: Instant) {
        self.stats.elapsed = now.saturating_duration_since(self.started);
        self.stats.rate_final = self.cc.rate();
        self.stats.min_rtt = self.cc.min_rtt();
        self.stats.srtt = self.cc.srtt();
        self.stats.startup_exit = self.cc.startup_exit.map(|(_, why)| why.to_string());
    }

    /// Stops the transfer and tells the receiver on the next poll.
    pub fn cancel(&mut self, now: Instant) {
        if !self.is_finished() {
            self.abort_to_send = Some(reason::CANCELLED);
            self.fail(now, "cancelled");
        }
    }

    /// Feeds one parsed datagram.
    pub fn on_packet(&mut self, now: Instant, packet: &Packet<'_>) {
        if self.is_finished() || packet.xfer() != self.offer.xfer {
            return;
        }
        match packet {
            Packet::Ack(ack) => self.on_ack(now, ack),
            Packet::Done { ok: true, .. } => {
                self.state = SenderState::Done;
                self.finish_stats(now);
            }
            Packet::Done { ok: false, .. } => self.fail(now, "the receiver's SHA-256 did not match the offer"),
            Packet::Abort { reason, .. } => {
                self.fail(now, format!("the receiver stopped it: {}", reason::text(*reason)))
            }
            _ => {}
        }
    }

    fn on_ack(&mut self, now: Instant, ack: &Ack) {
        if ack.cum > self.next_new
            || ack.ranges.last().is_some_and(|&(s, l)| u64::from(s) + u64::from(l) > u64::from(self.next_new))
        {
            self.stats.bogus_acks += 1;
            return;
        }
        if self.state == SenderState::Offering {
            self.state = SenderState::Sending;
            self.last_progress = now;
        }
        // round trip: the echoed send time of the newest datagram the receiver saw, minus how long it held it
        let elapsed = self.ts(now).wrapping_sub(ack.ts_echo);
        let rtt = (elapsed >= ack.hold_us && elapsed < 60_000_000)
            .then(|| Duration::from_micros(u64::from(elapsed - ack.hold_us)));
        let mut newly = 0u32;
        while self.base < ack.cum {
            let Some(slot) = self.slots.pop_front() else { break };
            if !slot.acked {
                newly += self.len_of(self.base) as u32;
                if !slot.lost {
                    self.inflight -= 1;
                }
            }
            self.base += 1;
        }
        for &(start, len) in &ack.ranges {
            let end = start.saturating_add(len).min(self.next_new);
            for seq in start.max(self.base)..end {
                let idx = (seq - self.base) as usize;
                let slot_len = self.len_of(seq) as u32;
                let Some(slot) = self.slots.get_mut(idx) else { break };
                if !slot.acked {
                    slot.acked = true;
                    newly += slot_len;
                    if !slot.lost {
                        self.inflight -= 1;
                    }
                }
            }
        }
        // The newest datagram the receiver has seen, by the time it was sent: everything sent clearly before it
        // that is still unacknowledged was overtaken. (The echo, not the slots: a slot that was retransmitted and
        // then acknowledged may have been delivered by its first transmission.)
        if elapsed < 60_000_000 {
            let sent_at = self.started + Duration::from_micros(u64::from(ack.ts_echo));
            self.rack_ts = Some(self.rack_ts.map_or(sent_at, |t| t.max(sent_at)));
        }
        if newly > 0 {
            self.last_progress = now;
            self.rto_backoff = 0;
            self.stats.acked_bytes += u64::from(newly);
        }
        if let Some(rtt) = rtt {
            self.cc.on_rtt(now, rtt);
            let q = self.cc.queue_delay();
            self.stats.qd_sum += q;
            self.stats.qd_count += 1;
            self.stats.qd_max = self.stats.qd_max.max(q);
            if self.stats.qd_samples.len() < QD_SAMPLE_LIMIT {
                self.stats.qd_samples.push((q * 1e6) as u32);
            }
        }
        if newly > 0 {
            self.detect_losses(now);
        }
        self.cc.on_acked(now, newly);
        if self.base == self.packets && self.state == SenderState::Sending {
            self.state = SenderState::Draining;
            self.fin_next = now;
        }
    }

    /// Declares lost the outstanding datagrams that a later-sent, acknowledged one has overtaken.
    fn detect_losses(&mut self, now: Instant) {
        let Some(rack) = self.rack_ts else { return };
        let min_rtt = self.cc.min_rtt().unwrap_or(Duration::from_millis(100));
        let reo = (min_rtt / 8)
            .max(Duration::from_secs_f64(4.0 * self.cc.rttvar()))
            .clamp(Duration::from_millis(2), min_rtt / 2 + Duration::from_millis(2));
        let mut lost = 0u32;
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if !slot.acked && !slot.lost && slot.last_tx + reo <= rack {
                slot.lost = true;
                self.inflight -= 1;
                self.retx.push_back(self.base + i as u32);
                lost += 1;
            }
        }
        if lost > 0 {
            self.stats.lost += u64::from(lost);
            self.cc.on_loss(now, lost);
        }
    }

    fn check_rto(&mut self, now: Instant) {
        if self.inflight == 0 {
            return;
        }
        let rto = self.cc.rto() * (1u32 << self.rto_backoff.min(6));
        let since = now.saturating_duration_since(self.last_progress.max(self.last_rto));
        if since < rto {
            return;
        }
        self.last_rto = now;
        self.rto_backoff = (self.rto_backoff + 1).min(6);
        self.stats.rtos += 1;
        let plain = self.cc.rto();
        let mut lost = 0u32;
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if !slot.acked && !slot.lost && slot.last_tx + plain <= now {
                slot.lost = true;
                self.inflight -= 1;
                self.retx.push_back(self.base + i as u32);
                lost += 1;
            }
        }
        self.stats.lost += u64::from(lost);
        self.cc.on_rto(now);
    }

    fn inflight_limit(&self) -> usize {
        let rtt = self.cc.srtt().unwrap_or(Duration::from_millis(200)).as_secs_f64();
        let bdp = 2.0 * self.cc.rate() * rtt / DATAGRAM_SIZE as f64;
        (bdp as usize + 32).min(self.cfg.max_inflight)
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last_poll).as_secs_f64().min(0.25);
        self.last_poll = now;
        let rate = self.cc.rate();
        let cap = (rate * 0.010).clamp(4.0 * DATAGRAM_SIZE as f64, 128.0 * DATAGRAM_SIZE as f64);
        self.tokens = (self.tokens + rate * dt).min(cap);
        if self.cc.phase() == Phase::Steady {
            self.stats.rate_acc += rate * dt;
            self.stats.rate_time += dt;
        }
        self.stats.rate_peak = self.stats.rate_peak.max(rate);
    }

    /// Sends what the pacing allows through `send`. `send` returns false when the wire refuses the datagram (Steam's
    /// buffer is full): it is then not counted as sent and `poll` returns.
    pub fn poll(&mut self, now: Instant, send: &mut dyn FnMut(&[u8]) -> bool) {
        if let Some(code) = self.abort_to_send.take() {
            send(&wire::encode_abort(self.offer.xfer, code));
        }
        if self.is_finished() {
            return;
        }
        self.refill(now);
        if now.saturating_duration_since(self.last_progress) >= self.cfg.stall {
            let secs = self.cfg.stall.as_secs();
            self.fail(now, format!("no progress for {secs} s"));
            return;
        }
        match self.state {
            SenderState::Offering => {
                if now >= self.offer_next {
                    let mut offer = self.offer.clone();
                    offer.ts = self.ts(now);
                    if send(&wire::encode_offer(&offer)) {
                        self.offer_tries += 1;
                        let wait = Duration::from_millis(400) * (1u32 << self.offer_tries.min(3));
                        self.offer_next = now + wait.min(Duration::from_secs(2));
                    }
                }
            }
            SenderState::Sending => {
                self.check_rto(now);
                self.send_data(now, send);
            }
            SenderState::Draining => {
                if now >= self.fin_next && send(&wire::encode_fin(self.offer.xfer)) {
                    self.fin_next = now + self.cc.rto().max(Duration::from_millis(100));
                }
            }
            SenderState::Done | SenderState::Failed(_) => {}
        }
    }

    fn send_data(&mut self, now: Instant, send: &mut dyn FnMut(&[u8]) -> bool) {
        loop {
            // a datagram may leave while there is any credit; the debt it leaves keeps the average exact
            if self.tokens <= 0.0 {
                break;
            }
            let seq = loop {
                match self.retx.front() {
                    Some(&s) => {
                        let live = s >= self.base
                            && self.slots.get((s - self.base) as usize).is_some_and(|slot| slot.lost && !slot.acked);
                        if live {
                            break Some(s);
                        }
                        self.retx.pop_front();
                    }
                    None => break None,
                }
            };
            let retransmit = seq.is_some();
            let seq = match seq {
                Some(s) => s,
                None => {
                    if self.next_new >= self.packets || self.inflight >= self.inflight_limit() {
                        break;
                    }
                    self.next_new
                }
            };
            let len = self.len_of(seq);
            let offset = u64::from(seq) * u64::from(self.offer.payload);
            self.scratch.resize(len, 0);
            if let Err(e) = self.source.read(offset, &mut self.scratch) {
                self.abort_to_send = Some(reason::INTERNAL);
                self.fail(now, format!("cannot read the data to send: {e}"));
                return;
            }
            let datagram = wire::encode_data(self.offer.xfer, seq, self.ts(now), &self.scratch);
            if !send(&datagram) {
                self.stats.blocked += 1;
                break;
            }
            self.tokens -= datagram.len() as f64;
            if retransmit {
                self.retx.pop_front();
                if let Some(slot) = self.slots.get_mut((seq - self.base) as usize) {
                    slot.lost = false;
                    slot.tx = slot.tx.saturating_add(1);
                    slot.last_tx = now;
                }
                self.stats.retransmits += 1;
            } else {
                self.slots.push_back(Slot { acked: false, lost: false, tx: 1, last_tx: now });
                self.next_new += 1;
            }
            self.inflight += 1;
            self.stats.peak_inflight = self.stats.peak_inflight.max(self.inflight as u64);
            self.stats.sent_packets += 1;
            self.stats.sent_bytes += datagram.len() as u64;
            self.cc.on_sent(now);
        }
    }

    /// How long until `poll` has something to do, from `now`.
    pub fn wait_hint(&self, now: Instant) -> Duration {
        let cap = Duration::from_millis(5);
        match self.state {
            SenderState::Offering => self.offer_next.saturating_duration_since(now).min(cap),
            SenderState::Sending => {
                let have_data = !self.retx.is_empty() || self.next_new < self.packets;
                if !have_data {
                    return cap;
                }
                if self.tokens > 0.0 {
                    return Duration::from_micros(500);
                }
                Duration::from_secs_f64(-self.tokens / self.cc.rate().max(1.0)).clamp(Duration::from_micros(200), cap)
            }
            SenderState::Draining => self.fin_next.saturating_duration_since(now).min(cap),
            SenderState::Done | SenderState::Failed(_) => cap,
        }
    }
}
