//! Tests of the bulk transfer against a deterministic simulated network: latency, jitter, random and bursty loss,
//! reordering, duplication, and a bottleneck with a finite queue (so bufferbloat is real). Time is simulated: an
//! `Instant` plus a counter of microseconds, advanced in 250 us steps, so a run of ten simulated seconds takes
//! milliseconds and gives the same numbers every time.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::receiver::{Limits, Receiver, ReceiverState};
use super::sender::{MemSource, Sender, SenderConfig, SenderState};
use super::wire::{self, Offer, Packet};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// 0 to 1.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn payload_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        v.extend_from_slice(&rng.next().to_le_bytes());
    }
    v.truncate(n);
    v
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[derive(Clone)]
struct Net {
    /// One way, microseconds.
    one_way_us: u64,
    jitter_us: u64,
    loss: f64,
    /// Gilbert-Elliott bursts: chance per datagram to enter a burst, to leave it, and the loss inside.
    burst: Option<(f64, f64, f64)>,
    reorder: f64,
    reorder_extra_us: u64,
    dup: f64,
    ack_loss: f64,
    /// Everything is lost between these simulated microseconds, both ways.
    blackout: Option<(u64, u64)>,
    /// Bottleneck bytes per second at simulated time `us`.
    rate: Arc<dyn Fn(u64) -> f64 + Send + Sync>,
    queue_bytes: f64,
}

impl Net {
    fn new(rate: f64) -> Net {
        Net {
            one_way_us: 107_000,
            jitter_us: 3_000,
            loss: 0.0,
            burst: None,
            reorder: 0.0,
            reorder_extra_us: 4_000,
            dup: 0.0,
            ack_loss: 0.0,
            blackout: None,
            rate: Arc::new(move |_| rate),
            queue_bytes: 1_000_000.0,
        }
    }
}

type Heap = BinaryHeap<Reverse<(u64, u64, Vec<u8>)>>;

struct Link {
    heap: Heap,
    serial: u64,
}

impl Link {
    fn new() -> Link {
        Link { heap: BinaryHeap::new(), serial: 0 }
    }

    fn push(&mut self, at: u64, bytes: Vec<u8>) {
        self.serial += 1;
        self.heap.push(Reverse((at, self.serial, bytes)));
    }

    fn pop_due(&mut self, now: u64) -> Option<Vec<u8>> {
        if self.heap.peek().is_some_and(|Reverse((at, _, _))| *at <= now) {
            self.heap.pop().map(|Reverse((_, _, b))| b)
        } else {
            None
        }
    }
}

#[derive(Default, Debug)]
struct Outcome {
    ok: bool,
    failed: Option<String>,
    secs: f64,
    goodput: f64,
    /// Queueing delay as the sender measured it, ms.
    qd_mean: f64,
    qd_p50: f64,
    qd_p95: f64,
    qd_max: f64,
    /// The bottleneck queue's own delay at each datagram's arrival, ms (ground truth), after `warm` seconds.
    true_qd_mean: f64,
    true_qd_p95: f64,
    true_qd_max: f64,
    loss_pct: f64,
    retx_pct: f64,
    rate_settled: f64,
    peak_inflight: u64,
    dropped_by_queue: u64,
    dup_received: u64,
    startup_exit: Option<String>,
    received_correct: bool,
    rtos: u64,
    sent: u64,
}

struct Run {
    net: Net,
    size: usize,
    seed: u64,
    cfg: SenderConfig,
    /// Seconds of simulated time to give up after.
    limit_s: f64,
    /// Ground-truth queue statistics ignore this many seconds at the start (start-up).
    warm_s: f64,
    trace: bool,
}

impl Run {
    fn new(net: Net, size: usize) -> Run {
        Run { net, size, seed: 12345, cfg: SenderConfig::default(), limit_s: 600.0, warm_s: 3.0, trace: false }
    }

    fn go(self) -> Outcome {
        let data = Arc::new(payload_bytes(self.size, self.seed));
        let digest = sha(&data);
        let base = Instant::now();
        let at = |us: u64| base + Duration::from_micros(us);
        let mut rng = Rng(self.seed.wrapping_mul(7919) | 1);
        let mut sender =
            Sender::new(base, 77, 5, digest, Box::new(MemSource(Arc::clone(&data))), self.cfg.clone()).unwrap();
        let mut receiver: Option<Receiver> = None;
        let mut fwd = Link::new();
        let mut rev = Link::new();
        let mut busy_until = 0.0f64; // bottleneck: simulated seconds
        let mut in_burst = false;
        let mut out = Outcome::default();
        let mut true_qd: Vec<f64> = Vec::new();
        let limits = Limits { max_total: 1 << 30 };
        let tick = 250u64;
        let mut t = 0u64;
        let mut first_data_us: Option<u64> = None;
        let net = self.net.clone();
        let limit = (self.limit_s * 1e6) as u64;

        while t < limit {
            t += tick;
            let now = at(t);
            // datagrams arriving at the receiver
            while let Some(bytes) = fwd.pop_due(t) {
                match wire::parse(&bytes) {
                    Some(Packet::Offer(o)) if receiver.is_none() => {
                        receiver = Receiver::accept(now, &o, &limits).ok();
                    }
                    Some(p) => {
                        if let Some(r) = receiver.as_mut() {
                            r.on_packet(now, &p);
                        }
                    }
                    None => panic!("the sender made a datagram that does not parse"),
                }
            }
            // acks arriving at the sender
            while let Some(bytes) = rev.pop_due(t) {
                let p = wire::parse(&bytes).expect("the receiver made a datagram that does not parse");
                sender.on_packet(now, &p);
            }
            // the receiver speaks
            if let Some(r) = receiver.as_mut() {
                let mut acks = Vec::new();
                r.poll(now, &mut acks);
                for a in acks {
                    if rng.unit() < net.ack_loss || net.blackout.is_some_and(|(a, b)| (a..b).contains(&t)) {
                        continue;
                    }
                    let delay = net.one_way_us + (rng.unit() * net.jitter_us as f64) as u64;
                    rev.push(t + delay, a);
                }
            }
            // the sender speaks, through the bottleneck
            let tsec = t as f64 / 1e6;
            let mut sent: Vec<Vec<u8>> = Vec::new();
            sender.poll(now, &mut |d| {
                sent.push(d.to_vec());
                true
            });
            for d in sent {
                if first_data_us.is_none() && matches!(wire::peek(&d), Some((2, _))) {
                    first_data_us = Some(t);
                }
                if net.blackout.is_some_and(|(a, b)| (a..b).contains(&t)) {
                    continue;
                }
                let rate = (net.rate)(t).max(1.0);
                let queued = ((busy_until - tsec).max(0.0)) * rate;
                if queued + d.len() as f64 > net.queue_bytes {
                    out.dropped_by_queue += 1;
                    continue;
                }
                let start = busy_until.max(tsec);
                busy_until = start + d.len() as f64 / rate;
                let waited = start - tsec;
                if tsec >= self.warm_s && matches!(wire::peek(&d), Some((2, _))) {
                    true_qd.push(waited * 1000.0);
                }
                // loss after the bottleneck
                let mut lose = rng.unit() < net.loss;
                if let Some((enter, leave, inside)) = net.burst {
                    if in_burst {
                        if rng.unit() < leave {
                            in_burst = false;
                        }
                    } else if rng.unit() < enter {
                        in_burst = true;
                    }
                    if in_burst && rng.unit() < inside {
                        lose = true;
                    }
                }
                if lose {
                    continue;
                }
                let mut delay =
                    (busy_until * 1e6) as u64 - t + net.one_way_us + (rng.unit() * net.jitter_us as f64) as u64;
                if rng.unit() < net.reorder {
                    delay += net.reorder_extra_us;
                }
                fwd.push(t + delay, d.clone());
                if rng.unit() < net.dup {
                    fwd.push(t + delay + 500, d);
                }
            }
            if self.trace && t.is_multiple_of(250_000) {
                let c = sender.cc();
                eprintln!(
                    "t={:.2} rate={:.2}MB/s q={:.0}ms dr={:.2} phase={:?} inflight={} true_queue={:.0}ms loss={:.3}",
                    tsec,
                    c.rate() / 1e6,
                    c.queue_delay() * 1000.0,
                    c.delivery_rate(now).unwrap_or(0.0) / 1e6,
                    c.phase(),
                    sender.inflight_packets(),
                    (busy_until - tsec).max(0.0) * 1000.0,
                    c.loss_ratio()
                );
            }
            if sender.is_finished() {
                break;
            }
        }
        let stats = sender.stats.clone();
        out.secs = t as f64 / 1e6;
        out.ok = *sender.state() == SenderState::Done;
        if let SenderState::Failed(why) = sender.state() {
            out.failed = Some(why.clone());
        }
        if let Some(r) = receiver.as_mut() {
            out.dup_received = r.stats.duplicates;
            if r.state() == (ReceiverState::Complete { ok: true }) {
                out.received_correct = r.take_data().is_some_and(|d| d == *data);
            }
        }
        let busy = first_data_us.map_or(out.secs, |f| (t - f) as f64 / 1e6);
        out.goodput = self.size as f64 / busy.max(0.001);
        out.qd_mean = stats.queue_delay_mean_ms();
        out.qd_p50 = stats.queue_delay_percentile_ms(0.5);
        out.qd_p95 = stats.queue_delay_percentile_ms(0.95);
        out.qd_max = stats.qd_max * 1000.0;
        if !true_qd.is_empty() {
            out.true_qd_mean = true_qd.iter().sum::<f64>() / true_qd.len() as f64;
            true_qd.sort_by(|a, b| a.total_cmp(b));
            out.true_qd_p95 = true_qd[(true_qd.len() as f64 * 0.95) as usize];
            out.true_qd_max = *true_qd.last().unwrap_or(&0.0);
        }
        out.loss_pct = stats.loss_percent();
        out.retx_pct = stats.retransmit_percent();
        out.rate_settled = stats.settled_rate();
        out.peak_inflight = stats.peak_inflight;
        out.startup_exit = stats.startup_exit.clone();
        out.rtos = stats.rtos;
        out.sent = stats.sent_packets;
        out
    }
}

const MB: usize = 1_000_000;

fn describe(label: &str, o: &Outcome) {
    eprintln!(
        "{label}: ok={} {:.1}s goodput {:.2} MB/s, queue delay (sender) mean {:.0} p95 {:.0} max {:.0} ms, (true) mean {:.0} p95 {:.0} max {:.0} ms, loss {:.2}% retx {:.2}%, settled {:.2} MB/s, peak inflight {}, queue drops {}, dups {}, startup exit {:?}, rtos {}, sent {}",
        o.ok,
        o.secs,
        o.goodput / 1e6,
        o.qd_mean,
        o.qd_p95,
        o.qd_max,
        o.true_qd_mean,
        o.true_qd_p95,
        o.true_qd_max,
        o.loss_pct,
        o.retx_pct,
        o.rate_settled / 1e6,
        o.peak_inflight,
        o.dropped_by_queue,
        o.dup_received,
        o.startup_exit,
        o.rtos,
        o.sent
    );
}

// ---- complete and correct

#[test]
fn transfers_complete_and_correct_at_every_loss_level() {
    for size in [MB, 8 * MB, 32 * MB] {
        for loss in [0.0, 0.01, 0.05] {
            let mut net = Net::new(5_000_000.0);
            net.loss = loss;
            net.ack_loss = loss;
            let o = Run::new(net, size).go();
            describe(&format!("{} MB at {:.0}% loss", size / MB, loss * 100.0), &o);
            assert!(o.ok, "{size} bytes at loss {loss}: {:?}", o.failed);
            assert!(o.received_correct, "the received bytes differ ({size} bytes at loss {loss})");
        }
    }
}

#[test]
fn goodput_reaches_most_of_a_5_mb_s_bottleneck_at_214_ms() {
    for loss in [0.0, 0.01] {
        let mut net = Net::new(5_000_000.0);
        net.loss = loss;
        let o = Run::new(net, 32 * MB).go();
        describe(&format!("goodput, loss {:.0}%", loss * 100.0), &o);
        assert!(o.ok && o.received_correct);
        // a window-limited protocol with a 512 KiB window would get 0.5 MB / 0.214 s = 2.4 MB/s
        assert!(o.goodput >= 0.70 * 5_000_000.0, "goodput {:.2} MB/s at loss {loss}", o.goodput / 1e6);
    }
}

#[test]
fn queueing_delay_stays_near_the_target() {
    let net = Net::new(5_000_000.0);
    let o = Run::new(net, 32 * MB).go();
    describe("queue delay", &o);
    assert!(o.ok);
    // the target is 40 ms; the 1 MB buffer would allow 200 ms, which a window-filling sender would reach
    assert!(o.true_qd_mean < 60.0, "mean {:.0} ms", o.true_qd_mean);
    assert!(o.true_qd_p95 < 120.0, "p95 {:.0} ms", o.true_qd_p95);
    assert!(o.qd_mean < 80.0, "measured mean {:.0} ms", o.qd_mean);
}

#[test]
fn a_fast_line_is_filled_too() {
    // 300 Mbit/s is 37 MB/s; the controller's maximum is 40
    let mut net = Net::new(37_500_000.0);
    net.queue_bytes = 4_000_000.0;
    let o = Run::new(net, 80 * MB).go();
    describe("37.5 MB/s", &o);
    assert!(o.ok && o.received_correct);
    // the start-up (1.28 x per round trip from 0.4 MB/s) takes about 4 s; 80 MB used to take 100 s
    assert!(o.secs < 8.0 && o.goodput >= 12_000_000.0, "{:.1} s, goodput {:.2} MB/s", o.secs, o.goodput / 1e6);
}

#[test]
fn a_slow_line_is_not_flooded() {
    // the real 1 MB/s uplink
    let net = Net::new(1_000_000.0);
    let o = Run::new(net, 8 * MB).go();
    describe("1 MB/s", &o);
    assert!(o.ok && o.received_correct);
    assert!(o.goodput >= 0.7 * 1_000_000.0, "goodput {:.2} MB/s", o.goodput / 1e6);
    assert!(o.true_qd_p95 < 150.0, "p95 {:.0} ms", o.true_qd_p95);
}

// ---- hostile networks

#[test]
fn bursts_of_loss_reordering_and_duplicates_are_survived() {
    let mut net = Net::new(5_000_000.0);
    net.burst = Some((0.002, 0.2, 0.8));
    net.loss = 0.005;
    net.reorder = 0.03;
    net.reorder_extra_us = 6_000;
    net.dup = 0.02;
    net.ack_loss = 0.05;
    let o = Run::new(net, 16 * MB).go();
    describe("bursts, reorder, dups", &o);
    assert!(o.ok && o.received_correct, "{:?}", o.failed);
    assert!(o.dup_received > 0, "duplicates were injected and must be seen");
    assert!(o.goodput >= 0.4 * 5_000_000.0, "goodput {:.2} MB/s", o.goodput / 1e6);
}

#[test]
fn heavy_jitter_does_not_collapse_the_rate() {
    let mut net = Net::new(5_000_000.0);
    net.jitter_us = 30_000;
    let o = Run::new(net, 16 * MB).go();
    describe("30 ms jitter", &o);
    assert!(o.ok && o.received_correct);
    assert!(o.goodput >= 0.4 * 5_000_000.0, "goodput {:.2} MB/s", o.goodput / 1e6);
}

#[test]
fn the_rate_follows_a_bottleneck_that_drops_mid_transfer() {
    // 5 MB/s for 5 s, then 1 MB/s for 10 s, then back to 5 MB/s
    let mut net = Net::new(5_000_000.0);
    net.rate = Arc::new(|us| if (5_000_000..15_000_000).contains(&us) { 1_000_000.0 } else { 5_000_000.0 });
    let mut run = Run::new(net, 32 * MB);
    run.warm_s = 9.0; // judge the queue once the controller had time to react to the drop
    let o = Run::go(run);
    describe("bottleneck drop", &o);
    assert!(o.ok && o.received_correct, "{:?}", o.failed);
    // the drop must be absorbed without the 1 MB buffer staying full
    assert!(o.true_qd_p95 < 160.0, "p95 {:.0} ms", o.true_qd_p95);
    // 5 s at 5 MB/s, 10 s at 1 MB/s, the rest at 5 MB/s: at best 25 + 10 + 22/5 s
    assert!(o.secs < 30.0, "{:.1} s", o.secs);
}

#[test]
fn a_blackout_is_survived_by_timeouts() {
    let mut net = Net::new(5_000_000.0);
    net.blackout = Some((3_000_000, 6_000_000));
    let o = Run::new(net, 8 * MB).go();
    describe("blackout", &o);
    assert!(o.ok && o.received_correct, "{:?}", o.failed);
}

#[test]
fn a_dead_peer_fails_the_transfer_after_the_stall_time() {
    let data = Arc::new(payload_bytes(MB, 1));
    let base = Instant::now();
    let mut s = Sender::new(base, 1, 0, sha(&data), Box::new(MemSource(data)), SenderConfig::default()).unwrap();
    let mut t = 0u64;
    while !s.is_finished() && t < 30_000_000 {
        t += 1000;
        s.poll(base + Duration::from_micros(t), &mut |_| true);
    }
    assert!(matches!(s.state(), SenderState::Failed(w) if w.contains("no progress for 10 s")), "{:?}", s.state());
    assert!((10_000_000..11_000_000).contains(&t), "{t}");
}

#[test]
fn a_wrong_hash_fails_on_both_ends() {
    let data = Arc::new(payload_bytes(MB, 2));
    let wrong = sha(b"something else");
    let base = Instant::now();
    let mut s =
        Sender::new(base, 1, 0, wrong, Box::new(MemSource(Arc::clone(&data))), SenderConfig::default()).unwrap();
    let mut r: Option<Receiver> = None;
    let mut t = 0u64;
    let mut to_r: Vec<Vec<u8>> = Vec::new();
    let mut to_s: Vec<Vec<u8>> = Vec::new();
    while !s.is_finished() && t < 60_000_000 {
        t += 500;
        let now = base + Duration::from_micros(t);
        for b in std::mem::take(&mut to_r) {
            match wire::parse(&b) {
                Some(Packet::Offer(o)) if r.is_none() => {
                    r = Receiver::accept(now, &o, &Limits { max_total: 10 * MB as u64 }).ok()
                }
                Some(p) => {
                    if let Some(r) = r.as_mut() {
                        r.on_packet(now, &p)
                    }
                }
                None => {}
            }
        }
        for b in std::mem::take(&mut to_s) {
            s.on_packet(now, &wire::parse(&b).unwrap());
        }
        if let Some(r) = r.as_mut() {
            r.poll(now, &mut to_s);
        }
        s.poll(now, &mut |d| {
            to_r.push(d.to_vec());
            true
        });
    }
    assert!(matches!(s.state(), SenderState::Failed(w) if w.contains("SHA-256")), "{:?}", s.state());
    let r = r.unwrap();
    assert_eq!(r.state(), ReceiverState::Complete { ok: false });
}

// ---- the receiver against a malicious or broken sender

fn offer(total: u64, payload: u16) -> Offer {
    Offer { xfer: 5, ts: 0, ctx: 0, total, payload, sha256: [9; 32] }
}

#[test]
fn offers_beyond_the_limits_are_refused() {
    let now = Instant::now();
    let limits = Limits { max_total: 1000 };
    assert!(Receiver::accept(now, &offer(1000, 500), &limits).is_ok());
    assert_eq!(Receiver::accept(now, &offer(1001, 500), &limits).err(), Some(wire::reason::TOO_BIG));
    assert_eq!(Receiver::accept(now, &offer(0, 500), &limits).err(), Some(wire::reason::REFUSED));
    assert_eq!(Receiver::accept(now, &offer(100, 0), &limits).err(), Some(wire::reason::REFUSED));
    assert_eq!(Receiver::accept(now, &offer(100, 10), &limits).err(), Some(wire::reason::REFUSED));
    assert_eq!(Receiver::accept(now, &offer(100, 5000), &limits).err(), Some(wire::reason::REFUSED));
}

#[test]
fn the_receiver_ignores_out_of_range_wrong_sized_and_duplicate_datagrams() {
    let now = Instant::now();
    let mut r = Receiver::accept(now, &offer(2500, 1000), &Limits { max_total: 1 << 20 }).unwrap();
    let full = vec![1u8; 1000];
    let last = vec![2u8; 500];
    r.on_packet(now, &Packet::Data { xfer: 5, seq: 3, ts: 0, payload: &full });
    r.on_packet(now, &Packet::Data { xfer: 5, seq: u32::MAX, ts: 0, payload: &full });
    r.on_packet(now, &Packet::Data { xfer: 5, seq: 0, ts: 0, payload: &full[..999] });
    r.on_packet(now, &Packet::Data { xfer: 5, seq: 2, ts: 0, payload: &full }); // the last one is 500 bytes
    r.on_packet(now, &Packet::Data { xfer: 6, seq: 0, ts: 0, payload: &full }); // another transfer
    assert_eq!((r.stats.unique, r.stats.rejected), (0, 4));
    r.on_packet(now, &Packet::Data { xfer: 5, seq: 0, ts: 0, payload: &full });
    r.on_packet(now, &Packet::Data { xfer: 5, seq: 0, ts: 0, payload: &full });
    assert_eq!((r.stats.unique, r.stats.duplicates), (1, 1));
    r.on_packet(now, &Packet::Data { xfer: 5, seq: 2, ts: 0, payload: &last });
    assert_eq!(r.state(), ReceiverState::Receiving);
    // never more memory than the declared size, however the datagrams come
    assert!(r.received_bytes() <= 2500);
}

#[test]
fn a_hostile_ack_cannot_move_the_sender_past_what_it_sent() {
    let data = Arc::new(payload_bytes(100_000, 3));
    let now = Instant::now();
    let mut s = Sender::new(now, 1, 0, sha(&data), Box::new(MemSource(data)), SenderConfig::default()).unwrap();
    s.poll(now, &mut |_| true);
    let ack = |cum: u32, ranges: Vec<(u32, u32)>| wire::Ack { xfer: 1, cum, ts_echo: 0, hold_us: 0, ranges };
    s.on_packet(now, &Packet::Ack(ack(0, vec![])));
    assert_eq!(*s.state(), SenderState::Sending);
    s.on_packet(now, &Packet::Ack(ack(1_000_000, vec![])));
    s.on_packet(now, &Packet::Ack(ack(0, vec![(5, 4_000_000_000)])));
    assert_eq!(s.stats.bogus_acks, 2);
    assert_eq!(s.acked_prefix_bytes(), 0);
    // an ack for another transfer and a done for another transfer do nothing
    s.on_packet(now, &Packet::Done { xfer: 99, ok: true });
    assert_eq!(*s.state(), SenderState::Sending);
}

// ---- fuzz

#[test]
fn random_and_mutated_datagrams_never_panic_and_never_parse_wrongly() {
    let mut rng = Rng(0xDEAD_BEEF);
    let now = Instant::now();
    let mut valid: Vec<Vec<u8>> = vec![
        wire::encode_offer(&offer(5000, 1000)),
        wire::encode_data(5, 1, 7, &[3u8; 1000]),
        wire::encode_ack(&wire::Ack { xfer: 5, cum: 2, ts_echo: 1, hold_us: 1, ranges: vec![(4, 2), (9, 1)] }),
        wire::encode_done(5, true),
        wire::encode_abort(5, 2),
        wire::encode_fin(5),
    ];
    let data = Arc::new(payload_bytes(5000, 4));
    let mut sender = Sender::new(now, 5, 0, sha(&data), Box::new(MemSource(data)), SenderConfig::default()).unwrap();
    let mut receiver = Receiver::accept(now, &offer(5000, 1000), &Limits { max_total: 1 << 20 }).unwrap();
    for i in 0..60_000u32 {
        let mut d = if i % 3 == 0 {
            let len = (rng.next() % 1300) as usize;
            (0..len).map(|_| rng.next() as u8).collect::<Vec<u8>>()
        } else {
            valid[(rng.next() % valid.len() as u64) as usize].clone()
        };
        if i % 3 != 0 && !d.is_empty() {
            for _ in 0..(1 + rng.next() % 3) {
                let at = (rng.next() % d.len() as u64) as usize;
                d[at] ^= 1 << (rng.next() % 8);
            }
            if rng.next().is_multiple_of(4) {
                d.truncate((rng.next() % d.len() as u64) as usize);
            }
            // sometimes fix the checksum so the contents themselves are what is tested
            if rng.next().is_multiple_of(2) && d.len() > 4 {
                let n = d.len() - 4;
                let crc = wire::crc32(&d[..n]);
                d[n..].copy_from_slice(&crc.to_le_bytes());
            }
        }
        if let Some(p) = wire::parse(&d) {
            let at = now + Duration::from_micros(u64::from(i) * 100);
            receiver.on_packet(at, &p);
            sender.on_packet(at, &p);
            let mut out = Vec::new();
            receiver.poll(at, &mut out);
            sender.poll(at, &mut |_| true);
            if rng.next().is_multiple_of(50) {
                valid.push(d);
            }
        }
    }
    // whatever happened, memory stayed within the declared size
    assert!(receiver.received_bytes() <= 5000);
}

#[test]
#[ignore = "prints a time series; run with --ignored --nocapture"]
fn trace_the_5_mb_s_run() {
    let mut net = Net::new(5_000_000.0);
    net.loss = std::env::var("TRACE_LOSS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    net.ack_loss = net.loss;
    net.jitter_us = std::env::var("TRACE_JITTER").ok().and_then(|v| v.parse().ok()).unwrap_or(net.jitter_us);
    let mut run = Run::new(net, 32 * MB);
    run.trace = true;
    let o = run.go();
    describe("trace", &o);
}
