//! The speed test engine of the probe: one *run* moves data in the two directions between two PCs, over the reliable
//! stream (on one or several connections, at a chosen Steam send rate) or as bulk datagrams, and ends with a result.
//! The tester drives a run; the responder (the other PC's probe) answers the messages of `throughput`. Series of runs
//! (the rate ladder, the comparison of variants) are in `suite_all`.
//!
//! A run is a small state machine stepped by [`Probe::poll`] (every 100 ms in the game window, every 20 ms in the
//! program): connect (and open extra connections), apply the rate, upload, download, result. While it runs it samples
//! Steam's own view of the connection every second, the highest round trip of each direction and any route change,
//! and puts a line per sample and Steam's detailed status into the log (`Probe::take_log`).


use super::bulk::{Limits, ReceiverState, SenderConfig, SenderState};
use super::bulk_io::{recv_report, run_receiver, run_sender, RecvReport, SendJob, SendReport, SyntheticSource};
use super::throughput::{
    self, kv_get, DgramResult, DgramSide, Direction, Msg, SenderSummary, SpeedResult, SteamSamples, Variant,
};
use super::*;

/// The longest a direction of a plain run may take.
pub const LONG_CAP: Duration = Duration::from_secs(600);
/// Waiting for the other side's report after the data is in.
const REPORT_WAIT: Duration = Duration::from_secs(8);
/// How long the responder's answer to a rate request is waited for before the peer is called "old".
const RATE_ACK_WAIT: Duration = Duration::from_secs(3);
/// Lines kept in the log between two takes.
const LOG_LIMIT: usize = 4000;

/// What one run does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunSpec {
    pub variant: Variant,
    /// Bytes in each direction.
    pub total: u64,
    /// The Steam send rate (bytes per second) both sides set on their connections for the run, restored after.
    pub rate: Option<i32>,
    /// Run the upload (you to your friend) and the download (your friend to you).
    pub up: bool,
    pub down: bool,
    /// A direction still running after this long is stopped and reported as it stands.
    pub cap: Duration,
}

impl RunSpec {
    /// The plain reliable test of `mb` MB each way: Steam's configured rate, no time limit worth speaking of.
    pub fn reliable(mb: u32) -> RunSpec {
        RunSpec {
            variant: Variant::Reliable(1),
            total: (u64::from(mb.max(1)) * throughput::MB).min(throughput::MAX_TOTAL),
            rate: None,
            up: true,
            down: true,
            cap: LONG_CAP,
        }
    }

    pub fn datagrams(mb: u32) -> RunSpec {
        RunSpec { variant: Variant::Datagram, ..RunSpec::reliable(mb) }
    }

    pub fn conns(&self) -> u32 {
        match self.variant {
            Variant::Reliable(n) => n.clamp(1, throughput::MAX_CONNS),
            Variant::Datagram => 1,
        }
    }
}

/// The runs a mode word of the window stands for, of `mb` MB each way (see [`throughput::plan_for`]).
pub fn specs_for(mode: &str, mb: u32) -> Vec<RunSpec> {
    throughput::plan_for(mode)
        .into_iter()
        .map(|variant| RunSpec { variant, ..RunSpec::reliable(mb) })
        .collect()
}

#[derive(Debug, Clone)]
pub enum RunKind {
    Reliable(SpeedResult),
    Datagram(DgramResult),
}

/// A finished run.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub id: u64,
    pub spec: RunSpec,
    pub kind: Option<RunKind>,
    pub failure: Option<String>,
    /// The result lines (or the failure) as the window shows them.
    pub lines: Vec<String>,
    pub log_line: String,
}

/// Where a run is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Phase {
    /// Waiting for the test connection (and the extra ones).
    Connecting,
    /// We send, the friend times.
    Upload,
    /// The friend sends, we time.
    Download,
    Done,
    Failed(String),
}

#[derive(Default)]
pub(super) struct DgRun {
    pub up_spawned: bool,
    pub up_report: Option<(SendReport, Instant)>,
    pub up_dres: Option<Vec<(String, u64)>>,
    pub up_cancel: bool,
    pub down_cancel: bool,
    pub down_recv: Option<(RecvReport, Instant)>,
    pub down_dsend: Option<Vec<(String, u64)>>,
    pub down_progress: u64,
}

/// A speed test run we started.
pub(super) struct SpeedRun {
    pub id: u64,
    pub spec: RunSpec,
    pub target: u64,
    pub conn: Handle,
    pub extras: Vec<(Handle, bool)>,
    pub phase: Phase,
    pub started: Instant,
    pub phase_started: Instant,
    pub last_progress: Instant,
    /// Bytes the friend says arrived so far.
    pub up_progress: u64,
    pub up_stats: Option<throughput::SendStats>,
    /// The friend's timing of our upload: bytes, ms, longest gap, bytes after the first chunk.
    pub up_remote: Option<(u64, u64, u64, u64)>,
    pub down: Option<throughput::Receiver>,
    /// The sender's end message arrived: when, its peak queue and longest wait.
    pub end_info: Option<(Instant, u32, u64)>,
    pub result: Option<SpeedResult>,
    pub dgram: Option<DgramResult>,
    pub dg: DgRun,
    pub up_capped: bool,
    pub down_capped: bool,
    pub up_ping_peak: u32,
    pub down_ping_peak: u32,
    pub samples: SteamSamples,
    pub next_sample: Instant,
    pub next_dump: Instant,
    pub route_last: String,
    pub route_notes: Vec<String>,
    pub rate_notes: Vec<String>,
    pub rate_ack: Option<(u32, u32)>,
    pub down_requested_at: Option<Instant>,
    pub rate_unanswered_noted: bool,
    pub finalized: bool,
    /// Several connections: the data waits until the responder has said it is ready for it (the data of the extra
    /// connections could otherwise reach it before the announcement on the first one).
    pub sender_pending: bool,
}

impl SpeedRun {
    pub fn active(&self) -> bool {
        matches!(self.phase, Phase::Connecting | Phase::Upload | Phase::Download)
    }

    pub fn owns(&self, conn: Handle) -> bool {
        conn == self.conn || self.extras.iter().any(|(h, _)| *h == conn)
    }

    /// The connections the data goes over: the test connection and the extras that are up.
    pub fn conns(&self) -> Vec<Handle> {
        std::iter::once(self.conn).chain(self.extras.iter().filter(|(_, up)| *up).map(|(h, _)| *h)).collect()
    }
}

/// An upload from a friend that we are timing.
pub(super) struct Incoming {
    pub conn: Handle,
    pub remote: Option<u64>,
    pub rx: throughput::Receiver,
    pub end: Option<Instant>,
}

/// Speed tests a friend may have us send at once.
pub(super) const MAX_SENDERS: usize = 2;

/// The transfer ids of a run: the upload's and the download's differ, so a stale datagram of one never counts for the
/// other.
fn up_xfer(id: u64) -> u32 {
    ((id as u32) & 0x7FFF_FFFF).max(1)
}

fn down_xfer(id: u64) -> u32 {
    up_xfer(id) | 0x8000_0000
}

fn nz(v: u32) -> Option<u32> {
    (v > 0).then_some(v)
}

/// The sender configuration, with the environment's overrides (`FEVERLINK3_BULK_TARGET_MS`, `FEVERLINK3_BULK_MAX_RATE`).
pub fn bulk_config() -> SenderConfig {
    let mut cfg = SenderConfig::default();
    if let Some(ms) = std::env::var("FEVERLINK3_BULK_TARGET_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        cfg.cc.target_delay = Duration::from_millis(ms.clamp(5, 500));
    }
    if let Some(rate) = std::env::var("FEVERLINK3_BULK_MAX_RATE").ok().and_then(|v| v.trim().parse::<f64>().ok()) {
        cfg.cc.max_rate = rate.clamp(100_000.0, 200_000_000.0);
    }
    cfg
}

/// Work to do after the state is unlocked.
enum Action {
    Send(Handle, Vec<u8>),
    Sender { conns: Vec<Handle>, id: u64, total: u64, chunk: u64, ours: bool, rate: Option<i32> },
    DgSender { conn: Handle, id: u64, total: u64, ours: bool },
}

impl ProbeService {
    pub(super) fn log(&self, state: &mut ProbeState, text: String) {
        if state.log.len() >= LOG_LIMIT {
            state.log.remove(0);
        }
        state.log.push(text);
    }

    fn peer_remote(state: &ProbeState, conn: Handle) -> Option<u64> {
        state.peers.iter().find(|p| p.conn == conn).and_then(|p| p.steam_id)
    }

    /// The connections a responder streams over: the asking one and, for several, the others of the same account.
    fn group_conns(state: &ProbeState, conn: Handle, n: u32) -> Vec<Handle> {
        let mut out = vec![conn];
        if let Some(remote) = Self::peer_remote(state, conn) {
            for p in &state.peers {
                if out.len() >= n as usize {
                    break;
                }
                if p.steam_id == Some(remote) && p.conn != conn {
                    out.push(p.conn);
                }
            }
        }
        out
    }

    fn incoming_matches(state: &ProbeState, conn: Handle, id: u64) -> bool {
        state.incoming.as_ref().is_some_and(|i| {
            i.rx.id == id && (i.conn == conn || (i.remote.is_some() && Self::peer_remote(state, conn) == i.remote))
        })
    }

    /// A speed-test message from `conn`. Replies are sent after the state is unlocked.
    pub(super) fn tput(&self, conn: Handle, msg: Msg) {
        let now = Instant::now();
        let mut actions: Vec<Action> = Vec::new();
        {
            let mut guard = lock(&self.state);
            let state = &mut *guard;
            let legacy = self.legacy.load(std::sync::atomic::Ordering::Relaxed);
            match msg {
                Msg::Up { id, total, chunk, conns } => {
                    if total > 0 && total <= throughput::MAX_TOTAL && chunk > 0 {
                        let remote = Self::peer_remote(state, conn);
                        state.incoming =
                            Some(Incoming { conn, remote, rx: throughput::Receiver::new(id, total, now), end: None });
                        state
                            .events
                            .push(format!("a friend is testing the speed: {} MB coming", total / throughput::MB));
                        if conns > 1 {
                            actions.push(Action::Send(conn, throughput::encode(&Msg::Prog { id, bytes: 0 })));
                        }
                    }
                }
                Msg::Down { id, total, chunk, conns, rate } => {
                    if total > 0 && total <= throughput::MAX_TOTAL && state.senders < MAX_SENDERS {
                        state.senders += 1;
                        let group = Self::group_conns(state, conn, conns);
                        let asked = (!legacy).then_some(rate).and_then(nz).map(|r| r.min(i32::MAX as u32) as i32);
                        if let Some(r) = asked {
                            let got = self.set_rates(&group, r);
                            let notes = format!("test {id}: send rate set to {r} B/s for the test: {}", got.1);
                            self.log(state, format!("steam speed test (responder): {notes}"));
                            actions.push(Action::Send(
                                conn,
                                throughput::encode(&Msg::RateAck { id, asked: r as u32, got: got.0 }),
                            ));
                        }
                        actions.push(Action::Sender {
                            conns: group,
                            id,
                            total,
                            chunk,
                            ours: false,
                            rate: asked,
                        });
                        state.events.push(format!("sending {} MB for a friend's speed test", total / throughput::MB));
                    }
                }
                Msg::DgDown { id, total } => {
                    if total > 0 && total <= throughput::MAX_TOTAL && state.senders < MAX_SENDERS {
                        state.senders += 1;
                        actions.push(Action::DgSender { conn, id, total, ours: false });
                        state.events.push(format!(
                            "sending {} MB as datagrams for a friend's speed test",
                            total / throughput::MB
                        ));
                    }
                }
                Msg::Stop { id } => {
                    if !legacy {
                        state.stopped_ids.insert(id);
                    }
                }
                Msg::RateAck { id, asked, got } => {
                    if let Some(run) = state.speed.as_mut().filter(|r| r.id == id && r.owns(conn)) {
                        run.rate_ack = Some((asked, got));
                    }
                }
                Msg::Data { id, seq, len } => {
                    if Self::incoming_matches(state, conn, id) {
                        if let Some(inc) = state.incoming.as_mut() {
                            inc.rx.on_chunk_seq(now, seq, len);
                            if inc.rx.got > inc.rx.total.saturating_add(throughput::CHUNK_PAYLOAD as u64) {
                                state.incoming = None;
                            } else if inc.rx.progress_due(now) {
                                actions.push(Action::Send(
                                    conn,
                                    throughput::encode(&Msg::Prog { id, bytes: inc.rx.got }),
                                ));
                            }
                        }
                        if let Some(a) = Self::try_upres(state, now, false) {
                            actions.push(a);
                        }
                    } else if let Some(run) = state.speed.as_mut().filter(|r| r.owns(conn) && r.id == id) {
                        if run.phase == Phase::Download {
                            if let Some(rx) = run.down.as_mut() {
                                rx.on_chunk_seq(now, seq, len);
                                run.last_progress = now;
                            }
                        }
                        self.maybe_finish_download(state, now, false);
                    }
                }
                Msg::End { id, peak_pending, max_block_ms, .. } => {
                    if Self::incoming_matches(state, conn, id) {
                        if let Some(inc) = state.incoming.as_mut() {
                            inc.end = Some(now);
                        }
                        if let Some(a) = Self::try_upres(state, now, false) {
                            actions.push(a);
                        }
                    } else if let Some(run) = state.speed.as_mut().filter(|r| r.owns(conn) && r.id == id) {
                        run.end_info = Some((now, peak_pending, max_block_ms));
                        self.maybe_finish_download(state, now, false);
                    }
                }
                Msg::Prog { id, bytes } => {
                    if let Some(run) = state
                        .speed
                        .as_mut()
                        .filter(|r| r.owns(conn) && r.id == id && r.phase == Phase::Upload)
                    {
                        run.up_progress = bytes;
                        run.last_progress = now;
                        if run.sender_pending {
                            run.sender_pending = false;
                            actions.push(Action::Sender {
                                conns: run.conns(),
                                id,
                                total: run.spec.total,
                                chunk: throughput::CHUNK_PAYLOAD as u64,
                                ours: true,
                                rate: None,
                            });
                        }
                    }
                }
                Msg::UpRes { id, bytes, ms, gap_ms, steady } => {
                    if let Some(run) = state
                        .speed
                        .as_mut()
                        .filter(|r| r.owns(conn) && r.id == id && r.phase == Phase::Upload)
                    {
                        run.up_remote = Some((bytes, ms, gap_ms, steady));
                        run.last_progress = now;
                        self.after_upload(state, now, &mut actions);
                    }
                }
                Msg::Dres { id, kv } => {
                    if let Some(run) = state.speed.as_mut().filter(|r| r.owns(conn) && u64::from(up_xfer(r.id)) == id) {
                        run.dg.up_dres = Some(kv);
                        run.last_progress = now;
                    }
                }
                Msg::Dsend { id, kv } => {
                    if let Some(run) = state.speed.as_mut().filter(|r| r.owns(conn) && r.id == id) {
                        run.dg.down_dsend = Some(kv);
                    }
                }
            }
        }
        self.perform(actions);
    }

    /// Sets the Steam send rate of connections; the number Steam then reports for the first, and the text for the log.
    fn set_rates(&self, conns: &[Handle], rate: i32) -> (u32, String) {
        let mut notes = Vec::new();
        for &c in conns {
            notes.push(self.core.set_connection_send_rate(c, rate));
        }
        let got = conns.first().and_then(|&c| self.core.link(c)).map_or(0, |l| l.send_rate);
        (got, notes.join(" | "))
    }

    fn restore_rates(&self, conns: &[Handle]) -> String {
        let default = self.core.send_rate_setting();
        conns.iter().map(|&c| self.core.set_connection_send_rate(c, default)).collect::<Vec<_>>().join(" | ")
    }

    /// The responder's answer to a finished (or long overdue) upload, once all its data is in.
    fn try_upres(state: &mut ProbeState, now: Instant, force: bool) -> Option<Action> {
        let inc = state.incoming.as_ref()?;
        let end = inc.end?;
        let waited = now.saturating_duration_since(end) > Duration::from_secs(10);
        if inc.rx.got < inc.rx.total && !waited && !force {
            return None;
        }
        let inc = state.incoming.take()?;
        let rx = inc.rx;
        Some(Action::Send(
            inc.conn,
            throughput::encode(&Msg::UpRes {
                id: rx.id,
                bytes: rx.got,
                ms: rx.span_ms(),
                gap_ms: rx.max_gap_ms(),
                steady: rx.steady(),
            }),
        ))
    }

    /// Our upload is over (the friend's report is in, or we stopped it): the download, or the end.
    fn after_upload(&self, state: &mut ProbeState, now: Instant, actions: &mut Vec<Action>) {
        let Some(run) = state.speed.as_mut() else { return };
        if run.spec.down {
            self.start_download(run, now, actions);
        } else {
            self.finish_reliable(state, now);
        }
    }

    fn start_download(&self, run: &mut SpeedRun, now: Instant, actions: &mut Vec<Action>) {
        run.phase = Phase::Download;
        run.phase_started = now;
        run.last_progress = now;
        run.down_requested_at = Some(now);
        let conn = run.conn;
        match run.spec.variant {
            Variant::Reliable(_) => {
                run.down = Some(throughput::Receiver::new(run.id, run.spec.total, now));
                let msg = Msg::Down {
                    id: run.id,
                    total: run.spec.total,
                    chunk: throughput::CHUNK_PAYLOAD as u64,
                    conns: run.spec.conns(),
                    rate: run.spec.rate.map_or(0, |r| r as u32),
                };
                actions.push(Action::Send(conn, throughput::encode(&msg)));
            }
            Variant::Datagram => {
                actions.push(Action::Send(
                    conn,
                    throughput::encode(&Msg::DgDown { id: run.id, total: run.spec.total }),
                ));
            }
        }
    }

    /// The reliable download is complete (or we stopped it): the numbers go together.
    fn maybe_finish_download(&self, state: &mut ProbeState, now: Instant, force: bool) {
        let Some(run) = state.speed.as_mut().filter(|r| r.phase == Phase::Download) else { return };
        let Some(rx) = run.down.as_ref() else { return };
        let complete = run.end_info.is_some()
            && (rx.got >= run.spec.total
                || run.end_info.is_some_and(|(at, _, _)| now.saturating_duration_since(at) > Duration::from_secs(10)));
        if complete || force {
            self.finish_reliable(state, now);
        }
    }

    fn finish_reliable(&self, state: &mut ProbeState, now: Instant) {
        let rate_set = self.core.send_rate_setting();
        let (route, ping, effective) = state
            .test
            .as_ref()
            .map_or((String::new(), None, None), |t| (t.route.clone(), t.ping_ms, t.send_rate));
        let Some(run) = state.speed.as_mut() else { return };
        if run.spec.rate.is_some() && run.spec.down && !run.rate_unanswered_noted {
            // the responder answers a rate request before it streams, so no answer by now means an older program
            run.rate_unanswered_noted = true;
            run.rate_notes.push(match run.rate_ack {
                Some((asked, got)) => format!("peer set its send rate to {asked} B/s (Steam reports {got})"),
                None => "peer did not change its rate (an older program, or it ignored the request)".to_string(),
            });
        }
        let sent = run.up_stats.clone().unwrap_or_default();
        let up = match run.up_remote.filter(|_| run.spec.up) {
            Some((bytes, ms, gap, steady)) => Direction {
                bytes,
                rate: throughput::rate_of(steady, ms),
                secs: ms as f64 / 1000.0,
                peak_pending: sent.peak_pending,
                longest_pause_ms: gap.max(sent.max_block_ms),
                ping_peak_ms: nz(run.up_ping_peak),
                capped: run.up_capped,
            },
            None => Direction::default(),
        };
        let (peak, block_ms) = run.end_info.map_or((0, 0), |(_, p, b)| (p, b));
        let down = match run.down.as_ref().filter(|_| run.spec.down) {
            Some(rx) => Direction {
                bytes: rx.got,
                rate: rx.rate(),
                secs: rx.span_ms() as f64 / 1000.0,
                peak_pending: peak,
                longest_pause_ms: rx.max_gap_ms().max(block_ms),
                ping_peak_ms: nz(run.down_ping_peak),
                capped: run.down_capped,
            },
            None => Direction::default(),
        };
        let result = SpeedResult {
            total_bytes: run.spec.total,
            up,
            down,
            total_secs: now.saturating_duration_since(run.started).as_secs_f64(),
            ping_ms: ping,
            route,
            rate_set: run.spec.rate.unwrap_or(rate_set),
            rate_effective: effective,
            conns: run.spec.conns(),
            route_notes: run.route_notes.clone(),
            rate_note: run.rate_notes.join("; "),
            steam_line: run.samples.summary_line(),
        };
        run.result = Some(result);
        run.phase = Phase::Done;
        state.events.push("speed test finished".into());
    }

    /// Carries out what the locked part decided.
    fn perform(&self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Send(conn, bytes) => {
                    let _ = self.core.send(conn, &bytes);
                }
                Action::Sender { conns, id, total, chunk, ours, rate } => {
                    self.spawn_sender(conns, id, total, chunk, ours, rate);
                }
                Action::DgSender { conn, id, total, ours } => self.spawn_dgram_sender(conn, id, total, ours),
            }
        }
    }

    /// Streams `total` bytes over `conns` on a thread of its own. `ours` is a run we started (it stops when the run is
    /// replaced, fails or is capped); the other kind answers a friend, runs to the end, or until the friend says stop.
    fn spawn_sender(&self, conns: Vec<Handle>, id: u64, total: u64, chunk: u64, ours: bool, rate: Option<i32>) {
        let Some(me) = self.me.upgrade() else { return };
        let spawned = thread::Builder::new().name("feverlink3-steam-speed".into()).spawn(move || {
            let cancelled = || {
                let state = lock(&me.state);
                if ours {
                    !state.speed.as_ref().is_some_and(|r| r.id == id && r.phase == Phase::Upload && !r.up_capped)
                } else {
                    state.stopped_ids.contains(&id)
                }
            };
            let stats = throughput::stream(&me.core, &conns, id, total, chunk, &cancelled);
            let mut guard = lock(&me.state);
            let state = &mut *guard;
            if ours {
                if let Some(run) = state.speed.as_mut().filter(|r| r.id == id) {
                    if let (Some(e), Phase::Upload) = (&stats.error, &run.phase) {
                        if !run.up_capped {
                            run.phase = Phase::Failed(format!("sending stopped: {e}"));
                        }
                    }
                    run.up_stats = Some(stats);
                }
            } else {
                state.senders = state.senders.saturating_sub(1);
                state.stopped_ids.remove(&id);
                let mut text = format!(
                    "steam speed test (responder): sent {} MB in {} ms over {} connection(s), peak queue {} KiB, longest wait {} ms{}",
                    stats.sent / throughput::MB,
                    stats.elapsed_ms,
                    conns.len(),
                    stats.peak_pending / 1024,
                    stats.max_block_ms,
                    stats.error.as_ref().map_or(String::new(), |e| format!(", ERROR {e}"))
                );
                if rate.is_some() {
                    text.push_str(&format!("; rate restored: {}", me.restore_rates(&conns)));
                }
                me.log(state, text);
                if let Some(dump) = conns.first().and_then(|&c| me.core.detailed_status(c)) {
                    for l in dump.lines() {
                        me.log(state, format!("steam speed test (responder) status: {l}"));
                    }
                }
            }
        });
        if spawned.is_err() && !ours {
            let mut state = lock(&self.state);
            state.senders = state.senders.saturating_sub(1);
        }
    }

    /// Sends `total` synthetic bytes as bulk datagrams on a thread of its own.
    fn spawn_dgram_sender(&self, conn: Handle, id: u64, total: u64, ours: bool) {
        let Some(me) = self.me.upgrade() else { return };
        let xfer = if ours { up_xfer(id) } else { down_xfer(id) };
        let spawned = thread::Builder::new().name("feverlink3-steam-bulk-send".into()).spawn(move || {
            let sha = SyntheticSource::sha256(u64::from(xfer), total);
            let job = SendJob {
                xfer,
                ctx: 0,
                sha256: sha,
                source: Box::new(SyntheticSource { id: u64::from(xfer), len: total }),
                cfg: bulk_config(),
            };
            let cancelled = || {
                let state = lock(&me.state);
                if ours {
                    !state.speed.as_ref().is_some_and(|r| r.id == id && !r.dg.up_cancel && r.active())
                } else {
                    state.stopped_ids.contains(&id)
                }
            };
            let progress = |bytes: u64| {
                if ours {
                    let mut state = lock(&me.state);
                    if let Some(run) = state.speed.as_mut().filter(|r| r.id == id) {
                        if bytes > run.up_progress {
                            run.last_progress = Instant::now();
                        }
                        run.up_progress = bytes;
                    }
                }
            };
            let report = run_sender(&me.core, &me.hub, conn, job, &cancelled, &progress);
            let mut guard = lock(&me.state);
            let state = &mut *guard;
            let summary = SenderSummary::from_stats(&report.stats);
            for note in &report.rate_notes {
                me.log(state, format!("steam speed test (datagrams, {}): {note}", if ours { "tester" } else { "responder" }));
            }
            if ours {
                if let Some(run) = state.speed.as_mut().filter(|r| r.id == id) {
                    run.last_progress = Instant::now();
                    run.rate_notes.extend(report.rate_notes.iter().cloned());
                    run.dg.up_report = Some((report, Instant::now()));
                }
            } else {
                state.senders = state.senders.saturating_sub(1);
                state.stopped_ids.remove(&id);
                let ok = report.state == SenderState::Done;
                let mut kv = summary.to_kv();
                kv.push(("ok".into(), u64::from(ok)));
                let msg = throughput::encode(&Msg::Dsend { id, kv });
                let _ = me.core.send(conn, &msg);
                me.log(
                    state,
                    format!(
                        "steam speed test (responder, datagrams): sent {} MB, {}: {}",
                        total / throughput::MB,
                        if ok { "complete".to_string() } else { format!("{:?}", report.state) },
                        summary.text()
                    ),
                );
                if let Some(dump) = me.core.detailed_status(conn) {
                    for l in dump.lines() {
                        me.log(state, format!("steam speed test (responder) status: {l}"));
                    }
                }
            }
        });
        if spawned.is_err() && !ours {
            let mut state = lock(&self.state);
            state.senders = state.senders.saturating_sub(1);
        }
    }

    /// A bulk offer arrived on `conn` (registered in the hub as transfer `xfer`): ours (the download we asked for) or a
    /// friend's upload. Anything else is refused as busy.
    pub(super) fn accept_dgram(&self, conn: Handle, xfer: u32, offer: Vec<u8>, rx: std::sync::mpsc::Receiver<Vec<u8>>) {
        enum Role {
            Tester(u64),
            Responder,
        }
        let role = {
            let mut guard = lock(&self.state);
            let state = &mut *guard;
            let ours = state.speed.as_ref().filter(|r| {
                r.owns(conn)
                    && r.spec.variant == Variant::Datagram
                    && r.phase == Phase::Download
                    && down_xfer(r.id) == xfer
            });
            if let Some(run) = ours {
                Some(Role::Tester(run.id))
            } else if state.dg_in.is_none() && state.incoming.is_none() {
                state.dg_in = Some(xfer);
                state.events.push("a friend is testing the speed (datagrams)".into());
                Some(Role::Responder)
            } else {
                None
            }
        };
        let Some(role) = role else {
            self.hub.unregister(conn, xfer);
            let _ = self.core.send_datagram(conn, &super::bulk::wire::encode_abort(xfer, super::bulk::wire::reason::BUSY));
            return;
        };
        let Some(me) = self.me.upgrade() else { return };
        let _ = thread::Builder::new().name("feverlink3-steam-bulk-recv".into()).spawn(move || {
            let limits = Limits { max_total: throughput::MAX_TOTAL };
            let mut completed: Option<RecvReport> = None;
            let cancelled = || {
                let state = lock(&me.state);
                match role {
                    Role::Tester(id) => state.speed.as_ref().is_none_or(|r| r.id != id || r.dg.down_cancel || !r.active()),
                    Role::Responder => false,
                }
            };
            let progress = |bytes: u64| {
                if let Role::Tester(id) = role {
                    let mut state = lock(&me.state);
                    if let Some(run) = state.speed.as_mut().filter(|r| r.id == id) {
                        if bytes > run.dg.down_progress {
                            run.last_progress = Instant::now();
                        }
                        run.dg.down_progress = bytes;
                    }
                }
            };
            let mut on_complete = |r: &mut super::bulk::Receiver| {
                let report = recv_report(r);
                match role {
                    Role::Tester(id) => {
                        let mut state = lock(&me.state);
                        if let Some(run) = state.speed.as_mut().filter(|x| x.id == id) {
                            run.dg.down_recv = Some((report.clone(), Instant::now()));
                            run.last_progress = Instant::now();
                        }
                    }
                    Role::Responder => {
                        let ok = report.state == ReceiverState::Complete { ok: true };
                        me.send_dres(conn, &report, ok);
                    }
                }
                completed = Some(report);
            };
            let ended = run_receiver(&me.core, &me.hub, conn, &offer, rx, &limits, &cancelled, &progress, &mut on_complete);
            // not completed: say what arrived anyway
            if completed.is_none() {
                if let Some(report) = ended {
                    match role {
                        Role::Tester(id) => {
                            let mut state = lock(&me.state);
                            if let Some(run) = state.speed.as_mut().filter(|x| x.id == id) {
                                if run.dg.down_recv.is_none() {
                                    run.dg.down_recv = Some((report, Instant::now()));
                                }
                            }
                        }
                        Role::Responder => me.send_dres(conn, &report, false),
                    }
                }
            }
            if matches!(role, Role::Responder) {
                let mut state = lock(&me.state);
                state.dg_in = None;
                if let Some(dump) = me.core.detailed_status(conn) {
                    for l in dump.lines() {
                        me.log(&mut state, format!("steam speed test (responder) status: {l}"));
                    }
                }
            }
        });
    }

    fn send_dres(&self, conn: Handle, r: &RecvReport, ok: bool) {
        let kv = vec![
            ("ok".to_string(), u64::from(ok)),
            ("bytes".to_string(), r.bytes),
            ("ms".to_string(), r.span_ms),
            ("gap".to_string(), r.max_gap_ms),
            ("dups".to_string(), r.duplicates),
            ("acks".to_string(), r.acks_sent),
        ];
        // the upload's transfer id identifies the run to the tester (up_xfer of its run id)
        let _ = self.core.send(conn, &throughput::encode(&Msg::Dres { id: u64::from(r.xfer), kv }));
    }
}

impl ProbeService {
    /// Once a second: Steam's view of the connection, the highest round trip per direction, route changes, and now and
    /// then Steam's detailed status for the log.
    fn sample(&self, run: &mut SpeedRun, now: Instant, logs: &mut Vec<String>) {
        if now < run.next_sample {
            return;
        }
        run.next_sample = now + Duration::from_secs(1);
        let Some(link) = self.core.link(run.conn) else { return };
        let t = now.saturating_duration_since(run.started).as_secs_f64();
        run.samples.push(t, link);
        if let Some(ping) = link.ping_ms {
            match run.phase {
                Phase::Upload => run.up_ping_peak = run.up_ping_peak.max(ping),
                Phase::Download => run.down_ping_peak = run.down_ping_peak.max(ping),
                _ => {}
            }
        }
        let route = link.route.text().to_string();
        if run.route_last.is_empty() {
            run.route_last = route;
        } else if route != run.route_last {
            run.route_notes.push(format!("{} -> {route} at {t:.0} s", run.route_last));
            run.route_last = route;
        }
        if now >= run.next_dump && matches!(run.phase, Phase::Upload | Phase::Download) {
            run.next_dump = now + Duration::from_secs(20);
            if let Some(dump) = self.core.detailed_status(run.conn) {
                for l in dump.lines() {
                    logs.push(format!("steam speed test status (t={t:.0}s, {:?}): {l}", run.phase));
                }
            }
        }
    }

    /// One step of the run in progress.
    fn step_run(&self, state: &mut ProbeState, now: Instant, actions: &mut Vec<Action>) {
        if state.speed.as_ref().is_some_and(|r| !r.active() && !r.finalized) {
            self.finalize(state, now);
            return;
        }
        let test_info = state.test.as_ref().map(|t| (t.conn, t.connected, t.state.clone()));
        let mut logs = Vec::new();
        let mut after_upload = false;
        let mut force_download = false;
        let mut finish_dgram = false;
        {
            let Some(run) = state.speed.as_mut().filter(|r| r.active()) else { return };
            self.sample(run, now, &mut logs);
            let id = run.id;
            match run.phase.clone() {
                Phase::Connecting => match test_info {
                    Some((tconn, true, _)) => {
                        run.conn = tconn;
                        let need = run.spec.conns() as usize - 1;
                        if run.extras.len() < need {
                            let Some(owner) = self.me.upgrade() else { return };
                            for _ in run.extras.len()..need {
                                match self.core.connect(run.target, PROBE_PORT, owner.clone()) {
                                    Ok(h) => run.extras.push((h, false)),
                                    Err(e) => {
                                        run.phase = Phase::Failed(format!("could not open another connection: {e}"));
                                        break;
                                    }
                                }
                            }
                        } else if run.extras.iter().all(|(_, up)| *up) {
                            self.begin(run, now, actions, &mut logs);
                        } else if now.saturating_duration_since(run.started) > Duration::from_secs(60) {
                            run.phase = Phase::Failed("the extra connections did not open within 60 s".into());
                        }
                    }
                    Some((_, false, tstate)) if tstate.starts_with("failed") => {
                        run.phase = Phase::Failed(format!("no connection: {tstate}"));
                    }
                    Some(_) => {
                        if now.saturating_duration_since(run.started) > Duration::from_secs(60) {
                            run.phase = Phase::Failed("no connection after 60 s".into());
                        }
                    }
                    None => run.phase = Phase::Failed("no connection".into()),
                },
                Phase::Upload => {
                    let elapsed = now.saturating_duration_since(run.phase_started);
                    if run.sender_pending && elapsed > Duration::from_secs(3) {
                        run.sender_pending = false;
                        actions.push(Action::Sender {
                            conns: run.conns(),
                            id,
                            total: run.spec.total,
                            chunk: throughput::CHUNK_PAYLOAD as u64,
                            ours: true,
                            rate: None,
                        });
                    }
                    if elapsed > run.spec.cap && !run.up_capped {
                        run.up_capped = true;
                        logs.push(format!(
                            "steam speed test note: upload stopped after {} s (the time limit)",
                            elapsed.as_secs()
                        ));
                        match run.spec.variant {
                            Variant::Reliable(_) => {
                                let ms = elapsed.as_millis() as u64;
                                run.up_remote = Some((run.up_progress, ms, 0, run.up_progress));
                                after_upload = true;
                            }
                            Variant::Datagram => run.dg.up_cancel = true,
                        }
                    } else if now.saturating_duration_since(run.last_progress) > throughput::ABORT_AFTER {
                        run.phase = Phase::Failed(format!("no data for {} s", throughput::ABORT_AFTER.as_secs()));
                    } else if run.spec.variant == Variant::Datagram {
                        if let Some((rep, at)) = &run.dg.up_report {
                            let failed = matches!(rep.state, SenderState::Failed(_));
                            if run.dg.up_dres.is_some() || failed || now.saturating_duration_since(*at) > REPORT_WAIT {
                                if run.spec.down {
                                    self.start_download(run, now, actions);
                                } else {
                                    finish_dgram = true;
                                }
                            }
                        }
                    }
                }
                Phase::Download => {
                    let elapsed = now.saturating_duration_since(run.phase_started);
                    if elapsed > run.spec.cap && !run.down_capped {
                        run.down_capped = true;
                        logs.push(format!(
                            "steam speed test note: download stopped after {} s (the time limit)",
                            elapsed.as_secs()
                        ));
                        actions.push(Action::Send(run.conn, throughput::encode(&Msg::Stop { id })));
                        match run.spec.variant {
                            Variant::Reliable(_) => force_download = true,
                            Variant::Datagram => run.dg.down_cancel = true,
                        }
                    } else if now.saturating_duration_since(run.last_progress) > throughput::ABORT_AFTER {
                        run.phase = Phase::Failed(format!("no data for {} s", throughput::ABORT_AFTER.as_secs()));
                    }
                    if run.spec.rate.is_some() && !run.rate_unanswered_noted {
                        if let Some((asked, got)) = run.rate_ack {
                            run.rate_unanswered_noted = true;
                            run.rate_notes.push(format!("peer set its send rate to {asked} B/s (Steam reports {got})"));
                        } else if run
                            .down_requested_at
                            .is_some_and(|at| now.saturating_duration_since(at) > RATE_ACK_WAIT)
                        {
                            run.rate_unanswered_noted = true;
                            let note = "peer did not change its rate (an older program, or it ignored the request)";
                            run.rate_notes.push(note.into());
                            logs.push(format!("steam speed test note: {note}"));
                        }
                    }
                    if run.spec.variant == Variant::Datagram {
                        if let Some((_, at)) = &run.dg.down_recv {
                            if run.dg.down_dsend.is_some()
                                || run.down_capped
                                || now.saturating_duration_since(*at) > REPORT_WAIT
                            {
                                finish_dgram = true;
                            }
                        }
                    }
                }
                Phase::Done | Phase::Failed(_) => {}
            }
        }
        for l in logs {
            self.log(state, l);
        }
        if after_upload {
            self.after_upload(state, now, actions);
        }
        if finish_dgram {
            self.finish_dgram(state, now);
        } else if state
            .speed
            .as_ref()
            .is_some_and(|r| r.phase == Phase::Download && r.spec.variant != Variant::Datagram)
        {
            self.maybe_finish_download(state, now, force_download);
        }
    }

    /// All connections are up: apply the rate and start the first direction.
    fn begin(&self, run: &mut SpeedRun, now: Instant, actions: &mut Vec<Action>, logs: &mut Vec<String>) {
        run.phase_started = now;
        run.last_progress = now;
        if let (Some(rate), Variant::Reliable(_)) = (run.spec.rate, run.spec.variant) {
            let (got, notes) = self.set_rates(&run.conns(), rate);
            let note = format!("own send rate set to {rate} B/s, Steam reports {got} B/s ({notes})");
            logs.push(format!("steam speed test note: {note}"));
            run.rate_notes.push(note);
        }
        for h in run.conns() {
            if let Some(report) = self.core.config_report(h) {
                logs.push(format!("steam speed test note: connection {h} settings: {report}"));
            }
        }
        if let Some(dump) = self.core.detailed_status(run.conn) {
            for l in dump.lines() {
                logs.push(format!("steam speed test status (start): {l}"));
            }
        }
        if run.spec.up {
            run.phase = Phase::Upload;
            match run.spec.variant {
                Variant::Reliable(_) => {
                    let chunk = throughput::CHUNK_PAYLOAD as u64;
                    let msg = Msg::Up { id: run.id, total: run.spec.total, chunk, conns: run.spec.conns() };
                    actions.push(Action::Send(run.conn, throughput::encode(&msg)));
                    if run.spec.conns() > 1 {
                        run.sender_pending = true;
                    } else {
                        actions.push(Action::Sender {
                            conns: run.conns(),
                            id: run.id,
                            total: run.spec.total,
                            chunk,
                            ours: true,
                            rate: None,
                        });
                    }
                }
                Variant::Datagram => {
                    run.dg.up_spawned = true;
                    actions.push(Action::DgSender { conn: run.conn, id: run.id, total: run.spec.total, ours: true });
                }
            }
        } else if run.spec.down {
            self.start_download(run, now, actions);
        } else {
            run.phase = Phase::Failed("the run has no direction".into());
        }
    }

    fn finish_dgram(&self, state: &mut ProbeState, now: Instant) {
        let (route, ping) = state.test.as_ref().map_or((String::new(), None), |t| (t.route.clone(), t.ping_ms));
        let Some(run) = state.speed.as_mut() else { return };
        let up = if run.spec.up {
            let dres = run.dg.up_dres.clone().unwrap_or_default();
            let (sender_state, stats) = run
                .dg
                .up_report
                .as_ref()
                .map_or((SenderState::Failed("no report".into()), None), |(r, _)| {
                    (r.state.clone(), Some(r.stats.clone()))
                });
            let remote_ok = kv_get(&dres, "ok") == 1;
            let ok = sender_state == SenderState::Done && remote_ok;
            let why = match (&sender_state, remote_ok) {
                (SenderState::Failed(w), _) => w.clone(),
                (_, false) if run.dg.up_dres.is_none() => "the receiver's report never came".to_string(),
                (_, false) => "the receiver reported failure".to_string(),
                _ => String::new(),
            };
            let bytes = if dres.is_empty() { run.up_progress } else { kv_get(&dres, "bytes") };
            DgramSide {
                ok,
                why: if run.up_capped { String::new() } else { why },
                bytes,
                rate: throughput::rate_of(bytes, kv_get(&dres, "ms")),
                pause_ms: kv_get(&dres, "gap"),
                sender: stats.map(|s| SenderSummary::from_stats(&s)),
                ping_peak_ms: nz(run.up_ping_peak),
                capped: run.up_capped,
            }
        } else {
            DgramSide { ok: true, ..DgramSide::default() }
        };
        let down = if run.spec.down {
            match &run.dg.down_recv {
                Some((rep, _)) => {
                    let ok = rep.state == ReceiverState::Complete { ok: true };
                    let sender = run.dg.down_dsend.as_ref().map(|kv| SenderSummary::from_kv(kv));
                    DgramSide {
                        ok,
                        why: if ok || run.down_capped { String::new() } else { format!("{:?}", rep.state) },
                        bytes: rep.bytes,
                        rate: throughput::rate_of(rep.bytes, rep.span_ms),
                        pause_ms: rep.max_gap_ms,
                        sender,
                        ping_peak_ms: nz(run.down_ping_peak),
                        capped: run.down_capped,
                    }
                }
                None => DgramSide { ok: false, why: "nothing arrived".into(), ..DgramSide::default() },
            }
        } else {
            DgramSide { ok: true, ..DgramSide::default() }
        };
        let result = DgramResult {
            total_bytes: run.spec.total,
            up,
            down,
            total_secs: now.saturating_duration_since(run.started).as_secs_f64(),
            ping_ms: ping,
            route,
            route_notes: run.route_notes.clone(),
            steam: run.samples.clone(),
            rate_notes: run.rate_notes.clone(),
        };
        run.dgram = Some(result);
        run.phase = Phase::Done;
        state.events.push("speed test finished".into());
    }

    /// A run ended: restore what it changed, close what it opened, make the outcome and the log lines.
    fn finalize(&self, state: &mut ProbeState, _now: Instant) {
        let Some(run) = state.speed.as_mut() else { return };
        if run.active() || run.finalized {
            return;
        }
        run.finalized = true;
        let mut logs = Vec::new();
        if run.spec.rate.is_some() && matches!(run.spec.variant, Variant::Reliable(_)) {
            logs.push(format!("steam speed test note: rate restored: {}", self.restore_rates(&run.conns())));
        }
        for (h, _) in &run.extras {
            self.core.close(*h);
        }
        if let Some(dump) = self.core.detailed_status(run.conn) {
            for l in dump.lines() {
                logs.push(format!("steam speed test status (end): {l}"));
            }
        }
        let label = run.spec.variant.label();
        let outcome = match (&run.phase, run.result.as_ref(), run.dgram.as_ref()) {
            (Phase::Done, Some(r), _) => {
                logs.insert(0, r.log_line());
                RunOutcome {
                    id: run.id,
                    spec: run.spec,
                    kind: Some(RunKind::Reliable(r.clone())),
                    failure: None,
                    lines: r.lines(),
                    log_line: r.log_line(),
                }
            }
            (Phase::Done, None, Some(d)) => {
                logs.insert(0, d.log_line());
                RunOutcome {
                    id: run.id,
                    spec: run.spec,
                    kind: Some(RunKind::Datagram(d.clone())),
                    failure: None,
                    lines: d.lines(),
                    log_line: d.log_line(),
                }
            }
            (phase, _, _) => {
                let why = match phase {
                    Phase::Failed(w) => w.clone(),
                    _ => "no result".to_string(),
                };
                let line = format!("steam speed test failed: {why}");
                logs.insert(0, line.clone());
                RunOutcome {
                    id: run.id,
                    spec: run.spec,
                    kind: None,
                    failure: Some(why),
                    lines: Vec::new(),
                    log_line: line,
                }
            }
        };
        logs.extend(run.samples.log_lines(&label));
        for n in &run.rate_notes {
            logs.push(format!("steam speed test note: {n}"));
        }
        state.outcome = Some(outcome);
        for l in logs {
            self.log(state, l);
        }
    }
}

impl Probe {
    /// Starts a speed test run with the friend `steam_id`, connecting first when the connection test is not already
    /// on that friend. A run in progress is replaced.
    pub fn start_run(&mut self, steam_id: u64, spec: RunSpec) {
        {
            let mut state = lock(&self.service.state);
            state.plan.clear();
            state.plan_total = 0;
            state.plan_blocks.clear();
        }
        self.begin_run(steam_id, spec);
    }

    /// Starts a series of runs, one after another (stopping at the first that fails); the report shows the result
    /// block of each, in order.
    pub fn start_plan(&mut self, steam_id: u64, mut specs: Vec<RunSpec>) {
        if specs.is_empty() {
            return;
        }
        let first = specs.remove(0);
        {
            let mut state = lock(&self.service.state);
            state.plan = specs.into();
            state.plan_peer = steam_id;
            state.plan_total = state.plan.len() + 1;
            state.plan_blocks.clear();
        }
        self.begin_run(steam_id, first);
    }

    fn begin_run(&mut self, steam_id: u64, spec: RunSpec) {
        if !self.core.status().ready {
            return;
        }
        let reuse = lock(&self.service.state).test.as_ref().is_some_and(|t| t.target == steam_id && t.connected);
        if !reuse {
            self.test(steam_id);
        }
        let now = Instant::now();
        let mut guard = lock(&self.service.state);
        let state = &mut *guard;
        if state.test.is_none() {
            return;
        }
        if let Some(old) = state.speed.as_mut().filter(|r| r.active()) {
            old.phase = Phase::Failed("replaced by a new test".into());
            self.service.finalize(state, now);
        }
        state.outcome = None;
        state.events.push(format!("speed test of {} MB started", spec.total / throughput::MB));
        let id = now_ms().max(state.last_run_id + 1);
        state.last_run_id = id;
        state.speed = Some(SpeedRun {
            id,
            spec,
            target: steam_id,
            conn: 0,
            extras: Vec::new(),
            phase: Phase::Connecting,
            started: now,
            phase_started: now,
            last_progress: now,
            up_progress: 0,
            up_stats: None,
            up_remote: None,
            down: None,
            end_info: None,
            result: None,
            dg: DgRun::default(),
            dgram: None,
            up_capped: false,
            down_capped: false,
            up_ping_peak: 0,
            down_ping_peak: 0,
            samples: SteamSamples::default(),
            next_sample: now + Duration::from_secs(1),
            next_dump: now + Duration::from_secs(3),
            route_last: String::new(),
            route_notes: Vec::new(),
            rate_notes: Vec::new(),
            rate_ack: None,
            down_requested_at: None,
            rate_unanswered_noted: false,
            finalized: false,
            sender_pending: false,
        });
    }

    /// The plain reliable test of `mb` MB each way.
    pub fn speed_test(&mut self, steam_id: u64, mb: u32) {
        self.start_run(steam_id, RunSpec::reliable(mb));
    }

    /// Steps the run and the responder side; called by [`Probe::poll`].
    pub(super) fn drive_speed(&self) {
        let now = Instant::now();
        let mut actions = Vec::new();
        {
            let mut guard = lock(&self.service.state);
            let state = &mut *guard;
            if let Some(a) = ProbeService::try_upres(state, now, false) {
                actions.push(a);
            }
            self.service.step_run(state, now, &mut actions);
        }
        self.service.perform(actions);
    }

    /// The next run of a series, once the one before it has ended: collects its block.
    pub(super) fn advance_plan(&mut self) {
        let next = {
            let mut guard = lock(&self.service.state);
            let state = &mut *guard;
            let (Some(run), Some(outcome)) = (state.speed.as_ref(), state.outcome.clone()) else { return };
            if state.plan_total == 0 || !run.finalized || state.plan_collected == outcome.id {
                return;
            }
            state.plan_collected = outcome.id;
            let mut block = String::new();
            if state.plan_total > 1 {
                block.push_str(&format!("== {} ({} MB) ==\n", run.spec.variant.label(), run.spec.total / throughput::MB));
            }
            match &outcome.failure {
                None => block.push_str(&outcome.lines.join("\n")),
                Some(why) => {
                    block.push_str(&format!("Speed test failed: {why}"));
                    state.plan.clear();
                }
            }
            state.plan_blocks.push(block);
            state.plan.pop_front().map(|spec| (state.plan_peer, spec))
        };
        if let Some((peer, spec)) = next {
            self.begin_run(peer, spec);
        }
    }

    /// Whether a run is in progress.
    pub fn run_active(&self) -> bool {
        let state = lock(&self.service.state);
        state.speed.as_ref().is_some_and(|r| !r.finalized) || (state.plan_total > 0 && !state.plan.is_empty())
    }

    /// The outcome of the last run, once it has ended.
    pub fn run_outcome(&self) -> Option<RunOutcome> {
        lock(&self.service.state).outcome.clone()
    }

    /// The log lines made since the last call (speed test samples, Steam's status dumps, notes), oldest first.
    pub fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut lock(&self.service.state).log)
    }

    /// Steam's detailed status of the test connection, when there is one.
    pub fn detailed_status(&self) -> Option<String> {
        let conn = lock(&self.service.state).test.as_ref().filter(|t| t.connected).map(|t| t.conn)?;
        self.core.detailed_status(conn)
    }

    /// Stops the run in progress (Ctrl+C, a safety stop): it ends as failed ("stopped"), its own send rate is put
    /// back, the extra connections are closed and the peer is told to stop sending.
    pub fn stop_run(&mut self) {
        let now = Instant::now();
        let mut guard = lock(&self.service.state);
        let state = &mut *guard;
        let Some(run) = state.speed.as_mut().filter(|r| r.active()) else { return };
        let (conn, id) = (run.conn, run.id);
        run.phase = Phase::Failed("stopped".into());
        self.service.finalize(state, now);
        drop(guard);
        if conn != 0 {
            let _ = self.core.send(conn, &throughput::encode(&Msg::Stop { id }));
        }
    }

    /// The core's latest measurement of the test connection (route, ping, quality, queue), when it is up.
    pub fn link_now(&self) -> Option<LinkSnapshot> {
        let conn = lock(&self.service.state).test.as_ref().filter(|t| t.connected).map(|t| t.conn)?;
        self.core.link(conn)
    }

    /// Makes this probe behave like an older program that ignores the rate request and the stop message (tests).
    #[cfg(test)]
    pub(crate) fn act_like_an_old_program(&self) {
        self.service.legacy.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What a series of runs adds to the window's view: the blocks so far and how many runs are left.
pub(super) struct PlanView {
    pub blocks: Vec<String>,
    pub total: usize,
    pub left: usize,
    /// The last run's block is among `blocks`.
    pub collected: bool,
}

impl PlanView {
    pub fn of(state: &ProbeState) -> PlanView {
        PlanView {
            blocks: state.plan_blocks.clone(),
            total: state.plan_total,
            left: state.plan.len(),
            collected: state.outcome.as_ref().is_some_and(|o| o.id == state.plan_collected),
        }
    }
}

/// The window's view of a run.
pub(super) fn speed_report(run: &SpeedRun, outcome: Option<&RunOutcome>, plan: PlanView) -> SpeedReport {
    let mut report = single_report(run, outcome);
    if plan.total > 1 {
        let blocks: Vec<String> = plan.blocks.iter().flat_map(|b| b.lines().map(str::to_string).collect::<Vec<_>>()).collect();
        let finished = plan.left == 0 && run.finalized && plan.collected;
        if finished {
            report.running = false;
            report.status = blocks.first().cloned().unwrap_or_default();
            report.lines = blocks;
        } else {
            let done = plan.total - plan.left - usize::from(!run.finalized);
            report.running = true;
            report.status = format!("[run {} of {}] {}", done + 1, plan.total, report.status);
            let mut lines = blocks;
            lines.push(report.status.clone());
            report.lines = lines;
        }
    }
    report
}

fn single_report(run: &SpeedRun, outcome: Option<&RunOutcome>) -> SpeedReport {
    let now = Instant::now();
    let mb = |bytes: u64| bytes as f64 / throughput::MB as f64;
    let total = mb(run.spec.total);
    let name = match run.spec.variant {
        Variant::Datagram => "Datagram speed test",
        Variant::Reliable(_) => "Speed test",
    };
    let mut report =
        SpeedReport { id: run.id, running: !run.finalized, status: String::new(), lines: Vec::new(), log: None };
    let quiet = now.saturating_duration_since(run.last_progress);
    let stalled = if run.active() && quiet >= throughput::STALL {
        format!(" (no data for {} s)", quiet.as_secs())
    } else {
        String::new()
    };
    match &run.phase {
        Phase::Connecting => report.status = format!("{name} of {total:.0} MB: connecting...{stalled}"),
        Phase::Upload => {
            let secs = now.saturating_duration_since(run.phase_started).as_secs_f64().max(0.001);
            let rate = throughput::rate_text(Some(run.up_progress as f64 / secs));
            report.status =
                format!("{name}, sending to your friend: {:.1} of {total:.0} MB, {rate}{stalled}", mb(run.up_progress));
        }
        Phase::Download => {
            let got = match run.spec.variant {
                Variant::Datagram => run.dg.down_progress,
                Variant::Reliable(_) => run.down.as_ref().map_or(0, |d| d.got),
            };
            let secs = now.saturating_duration_since(run.phase_started).as_secs_f64().max(0.001);
            let rate = throughput::rate_text(Some(got as f64 / secs));
            report.status =
                format!("{name}, receiving from your friend: {:.1} of {total:.0} MB, {rate}{stalled}", mb(got));
        }
        Phase::Done | Phase::Failed(_) => {
            if let Some(o) = outcome.filter(|o| o.id == run.id) {
                match &o.failure {
                    None => {
                        report.lines = o.lines.clone();
                        report.status = report.lines.first().cloned().unwrap_or_default();
                    }
                    Some(why) => report.status = format!("{name} failed: {why}"),
                }
                report.log = Some(o.log_line.clone());
            } else if let Phase::Failed(why) = &run.phase {
                report.status = format!("{name} failed: {why}");
            } else {
                report.status = format!("{name}: finishing...");
            }
        }
    }
    report
}
