//! The comprehensive speed test: one run, no key presses.
//!
//! - Stage 0: connect, take the idle round trip (the baseline) and the route.
//! - Stage 1, the rate ladder: 8 MB over the reliable stream at Steam send rates of 1, 2, 4, 8, 16 and 32 MiB/s, upload
//!   then download at each rate, climbing while the path keeps up and stopping at the first safety stop (round trip
//!   above 3x the baseline or baseline + 150 ms, a stall of 5 s, a gain under 10 %, a step that did not finish in 30 s).
//! - Stage 2: 32 MB each way at the best sustainable rate found, and 80 MB (the size of a big save) when that rate is
//!   4 MiB/s or more.
//! - Stage 3: 2 and 4 reliable connections at once, 8 MB split between them.
//! - The report: a table of every step, the best sustainable rate per direction, the time an 80 MB save takes at the
//!   default and at the best rate, and a verdict in plain English.
//!
//! The decisions are made on a [`Path`] (the thing that measures one step), so the tests drive them on a simulated
//! path; [`ProbePath`] is the real one on the Steam probe. Whatever happens (an error, Ctrl+C) the send rates are put
//! back to the default ([`Path::restore`]).

use std::time::{Duration, Instant};

use crate::steamnet::throughput::{self, Variant, MB, SAVE_MB, WIRE_FACTOR};
use crate::steamnet::{RunKind, RunSpec};

use super::suite::{Ctx, Outcome, Stop};

/// One MiB per second, the default send rate.
pub const MIB: i32 = 1 << 20;
/// The rates of the ladder, in order. Never above the last.
pub const RATES: [i32; 6] = [MIB, 2 * MIB, 4 * MIB, 8 * MIB, 16 * MIB, 32 * MIB];
/// Megabytes of one ladder step.
pub const STEP_MB: u32 = 8;
/// A step still running after this long is abandoned (the numbers so far stand).
pub const STEP_CAP: Duration = Duration::from_secs(30);
/// No data for this long is a stall.
pub const STALL_MS: u64 = 5_000;
/// A step must gain this much speed over the previous one to be worth the higher rate.
pub const MIN_GAIN: f64 = 1.10;
/// The ladder takes no new step after this much time.
pub const LADDER_BUDGET: Duration = Duration::from_secs(300);
/// Nothing new starts after this much time; the whole run aims at 8 minutes.
pub const TOTAL_BUDGET: Duration = Duration::from_secs(450);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dir {
    #[default]
    Up,
    Down,
}

impl Dir {
    pub fn text(self) -> &'static str {
        match self {
            Dir::Up => "up",
            Dir::Down => "down",
        }
    }
    fn idx(self) -> usize {
        self as usize
    }
}

/// What one step asks for.
#[derive(Debug, Clone)]
pub struct Req {
    pub stage: &'static str,
    pub dir: Dir,
    pub conns: u32,
    /// The send rate asked of each connection, bytes per second.
    pub rate: i32,
    pub mb: u32,
    pub cap: Duration,
}

/// What one step measured.
#[derive(Debug, Clone, Default)]
pub struct Step {
    pub stage: &'static str,
    pub dir: Dir,
    pub conns: u32,
    pub asked: i32,
    /// The rate Steam reported for the sender (ours for an upload, the peer's for a download).
    pub steam_rate: Option<u32>,
    /// A download whose peer did not answer the rate request.
    pub peer_unchanged: bool,
    pub mb: f64,
    /// Bytes per second as the receiver timed it.
    pub speed: Option<f64>,
    /// Round trip during the step: min, average, max (ms).
    pub rtt: Option<(u32, f64, u32)>,
    pub stalls: u32,
    pub longest_pause_ms: u64,
    pub route: String,
    /// The most reliable bytes Steam had queued.
    pub queue_peak: u32,
    /// The lowest delivery quality Steam reported, 0 to 1.
    pub quality_min: Option<f32>,
    pub capped: bool,
    pub failed: Option<String>,
    /// Why the ladder stopped climbing in this direction after this step.
    pub stop_note: Option<String>,
}

/// The idle measurement.
#[derive(Debug, Clone, Default)]
pub struct Baseline {
    pub rtt_ms: Option<u32>,
    pub route: String,
}

/// Measures steps. [`ProbePath`] is the real one.
pub trait Path {
    fn say(&mut self, text: &str);
    /// Time since the test started.
    fn elapsed(&self) -> Duration;
    fn baseline(&mut self) -> Result<Baseline, Stop>;
    fn measure(&mut self, req: &Req) -> Result<Step, Stop>;
    /// Whether more than one connection can be opened to the peer.
    fn parallel_available(&self) -> bool {
        true
    }
    /// Puts the send rates of both sides back to the default (called on every way out).
    fn restore(&mut self);
    /// Called with the summary after every step (the partial result for an abort).
    fn publish(&mut self, _summary: &Summary) {}
}

/// Everything the run found.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub baseline: Baseline,
    pub steps: Vec<Step>,
    /// Best sustainable rate per direction (up, down); the default when nothing better held.
    pub best: [i32; 2],
    pub notes: Vec<String>,
    pub parallel_note: String,
    pub finished: bool,
    pub default_rate: i32,
}

/// The round trip above which a step is unsafe: 3x the baseline or baseline + 150 ms, whichever comes first.
pub fn rtt_limit(baseline: u32) -> u32 {
    baseline.saturating_mul(3).min(baseline.saturating_add(150))
}

/// What to make of a step.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgement {
    /// The rate of this step can be kept (it did not break a safety rule and it was worth it).
    pub sustainable: bool,
    pub keep_climbing: bool,
    pub why: Option<String>,
}

/// The safety rules and the growth rule, applied to a finished step. `prev` is the previous step's speed in this
/// direction.
pub fn judge(prev: Option<f64>, step: &Step, baseline: Option<u32>) -> Judgement {
    let stop = |why: String| Judgement { sustainable: false, keep_climbing: false, why: Some(why) };
    if let Some(why) = &step.failed {
        return stop(format!("the step failed ({why})"));
    }
    if step.stalls > 0 {
        return stop(format!("a stall: no data for {} s", step.longest_pause_ms / 1000));
    }
    if let (Some(base), Some((_, _, max))) = (baseline, step.rtt) {
        let limit = rtt_limit(base);
        if max > limit {
            return stop(format!("the round trip rose to {max} ms (baseline {base} ms, limit {limit} ms)"));
        }
    }
    let Some(speed) = step.speed else { return stop("nothing could be timed".into()) };
    if let Some(p) = prev {
        if speed < p * MIN_GAIN {
            return Judgement {
                sustainable: false,
                keep_climbing: false,
                why: Some(format!("the speed grew less than 10 % ({} to {})", mbs(p), mbs(speed))),
            };
        }
    }
    if step.capped {
        return Judgement {
            sustainable: true,
            keep_climbing: false,
            why: Some(format!("the step did not finish in {} s", STEP_CAP.as_secs())),
        };
    }
    Judgement { sustainable: true, keep_climbing: true, why: None }
}

fn mbs(bytes_per_s: f64) -> String {
    format!("{:.2} MB/s", bytes_per_s / MB as f64)
}

fn mib_text(rate: i32) -> String {
    let m = f64::from(rate) / f64::from(MIB);
    if (m - m.round()).abs() < 0.005 {
        format!("{} MiB/s", m.round() as i64)
    } else {
        format!("{m:.2} MiB/s")
    }
}

/// The whole test on `path`. `Err(Stop)` after Ctrl+C; the rates are restored either way and `summary` holds what was
/// measured.
pub fn run<P: Path>(path: &mut P, summary: &mut Summary) -> Result<(), Stop> {
    let result = body(path, summary);
    path.restore();
    result
}

fn body<P: Path>(path: &mut P, s: &mut Summary) -> Result<(), Stop> {
    s.best = [s.default_rate; 2];
    // stage 0
    path.say("Stage 0: the idle round trip (3 s)...");
    s.baseline = path.baseline()?;
    path.say(&format!(
        "  route {}, idle round trip {}",
        if s.baseline.route.is_empty() { "unknown" } else { &s.baseline.route },
        s.baseline.rtt_ms.map_or_else(|| "unknown".to_string(), |r| format!("{r} ms"))
    ));
    let base = s.baseline.rtt_ms;
    // stage 1
    path.say("Stage 1: the rate ladder (8 MB at each send rate, up and down)...");
    let mut active = [true; 2];
    let mut prev: [Option<f64>; 2] = [None, None];
    for rate in RATES {
        if !active[0] && !active[1] {
            break;
        }
        if path.elapsed() > LADDER_BUDGET {
            s.notes.push("The ladder stopped early to keep the whole test short.".into());
            break;
        }
        for dir in [Dir::Up, Dir::Down] {
            if !active[dir.idx()] {
                continue;
            }
            let req = Req { stage: "ladder", dir, conns: 1, rate, mb: STEP_MB, cap: STEP_CAP };
            let mut step = path.measure(&req)?;
            let j = judge(prev[dir.idx()], &step, base);
            if rate == RATES[0] {
                // the default rate is the floor: it is "best" even when the step was disturbed
                s.best[dir.idx()] = rate;
            } else if j.sustainable {
                s.best[dir.idx()] = rate;
            }
            if j.sustainable {
                prev[dir.idx()] = step.speed;
            } else if rate == RATES[0] {
                prev[dir.idx()] = step.speed;
            }
            if !j.keep_climbing {
                active[dir.idx()] = false;
                step.stop_note = j.why.clone();
            } else if rate == *RATES.last().unwrap_or(&rate) {
                active[dir.idx()] = false;
                step.stop_note = Some("top of the ladder (32 MiB/s) reached".into());
            }
            path.say(&step_line(&step));
            if let Some(w) = &step.stop_note {
                path.say(&format!("  {}: no higher rate ({w})", step.dir.text()));
            }
            s.steps.push(step);
            path.publish(s);
        }
    }
    // stage 2
    path.say("Stage 2: validation at the best rates...");
    for dir in [Dir::Up, Dir::Down] {
        let best = s.best[dir.idx()];
        if best <= MIB {
            s.notes.push(format!("{}: the best rate is the default, no validation run.", dir.text()));
            continue;
        }
        for (mb, cap, label) in [(32u32, 45u64, "32 MB"), (80, 80, "80 MB")] {
            if mb == 80 && best < 4 * MIB {
                continue;
            }
            if path.elapsed() + Duration::from_secs(cap) > TOTAL_BUDGET {
                s.notes.push(format!("{} {label} run skipped (time).", dir.text()));
                continue;
            }
            let req =
                Req { stage: if mb == 32 { "validate" } else { "save size" }, dir, conns: 1, rate: best, mb, cap: Duration::from_secs(cap) };
            let step = path.measure(&req)?;
            path.say(&step_line(&step));
            s.steps.push(step);
            path.publish(s);
        }
    }
    // stage 3
    if !path.parallel_available() {
        s.parallel_note = "parallel connections: not available in this version".into();
    } else {
        path.say("Stage 3: parallel connections (8 MB split)...");
        'par: for n in [2u32, 4] {
            for dir in [Dir::Up, Dir::Down] {
                if path.elapsed() + STEP_CAP + Duration::from_secs(30) > TOTAL_BUDGET {
                    s.parallel_note = "parallel connections: skipped (time)".into();
                    break 'par;
                }
                // each connection keeps at least the default, together they reach about the best rate
                let rate = (s.best[dir.idx()] / n as i32).max(MIB);
                let req = Req { stage: "parallel", dir, conns: n, rate, mb: STEP_MB, cap: STEP_CAP };
                let step = path.measure(&req)?;
                path.say(&step_line(&step));
                let failed = step.failed.clone();
                s.steps.push(step);
                path.publish(s);
                if let Some(why) = failed {
                    s.parallel_note = format!("parallel connections: failed ({why})");
                    break 'par;
                }
            }
        }
    }
    s.finished = true;
    path.publish(s);
    Ok(())
}

/// One line for the console while the test runs.
pub fn step_line(s: &Step) -> String {
    let conns = if s.conns > 1 { format!(" x{}", s.conns) } else { String::new() };
    let what = match &s.failed {
        Some(why) => format!("failed: {why}"),
        None => format!(
            "{}{}{}",
            s.speed.map_or_else(|| "not timed".to_string(), mbs),
            s.rtt.map_or_else(String::new, |(a, avg, b)| format!(", ping {a}/{avg:.0}/{b} ms")),
            if s.stalls > 0 { ", STALL" } else { "" }
        ),
    };
    format!("  {} {}{} at {}: {what}", s.stage, s.dir.text(), conns, mib_text(s.asked))
}

// ---- the report

/// The step that measured `dir` at `rate` in `stage`s order: the latest with a speed.
fn speed_at(s: &Summary, dir: Dir, rate: i32, conns: u32) -> Option<f64> {
    s.steps
        .iter()
        .filter(|x| x.dir == dir && x.asked == rate && x.conns == conns && x.failed.is_none())
        .filter_map(|x| x.speed)
        .next_back()
}

/// The highest rate the speed followed (within 15 %) up to, per direction.
fn tracked_up_to(s: &Summary, dir: Dir) -> Option<i32> {
    s.steps
        .iter()
        .filter(|x| x.stage == "ladder" && x.dir == dir && x.failed.is_none())
        .filter(|x| x.speed.is_some_and(|v| v >= f64::from(x.asked) * 0.85))
        .map(|x| x.asked)
        .max()
}

fn rtt_text(r: Option<(u32, f64, u32)>) -> String {
    r.map_or_else(|| "?".to_string(), |(a, b, c)| format!("{a}/{b:.0}/{c}"))
}

/// All lines of the report: header, table, best rates, estimates, verdict.
pub fn report_lines(s: &Summary) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!(
        "Idle round trip: {}. Route at the start: {}.",
        s.baseline.rtt_ms.map_or_else(|| "unknown".to_string(), |r| format!("{r} ms")),
        if s.baseline.route.is_empty() { "unknown" } else { &s.baseline.route }
    ));
    if let Some(b) = s.baseline.rtt_ms {
        out.push(format!("Safety limit for the round trip during a step: {} ms.", rtt_limit(b)));
    }
    out.push(String::new());
    out.push("Every step (ping = min/avg/max in ms during the step; queue = most bytes Steam held back):".into());
    out.push(format!(
        "{:<9} {:<4} {:>5} {:>10} {:>10} {:>9} {:>13} {:>6} {:>10} {:>8}  {}",
        "stage", "dir", "conns", "asked", "Steam says", "MB/s", "ping", "stalls", "queue KiB", "quality", "route"
    ));
    for x in &s.steps {
        let steam = match (x.steam_rate, x.peer_unchanged) {
            (Some(r), _) => mib_text(r as i32),
            (None, true) => "peer old".to_string(),
            (None, false) => "?".to_string(),
        };
        let speed = match (&x.failed, x.speed) {
            (Some(_), _) => "failed".to_string(),
            (None, Some(v)) => format!("{:.2}{}", v / MB as f64, if x.capped { "*" } else { "" }),
            (None, None) => "?".to_string(),
        };
        out.push(format!(
            "{:<9} {:<4} {:>5} {:>10} {:>10} {:>9} {:>13} {:>6} {:>10} {:>8}  {}",
            x.stage,
            x.dir.text(),
            x.conns,
            mib_text(x.asked),
            steam,
            speed,
            rtt_text(x.rtt),
            x.stalls,
            x.queue_peak / 1024,
            x.quality_min.map_or_else(|| "?".to_string(), |q| format!("{:.1}%", q * 100.0)),
            if x.route.is_empty() { "?" } else { &x.route }
        ));
        if let Some(why) = &x.failed {
            out.push(format!("          failed: {why}"));
        }
        if let Some(why) = &x.stop_note {
            out.push(format!("          {}: no higher rate: {why}", x.dir.text()));
        }
    }
    out.push("(* = the step did not finish in its time limit; the speed is what had moved by then)".into());
    if s.steps.iter().any(|x| x.dir == Dir::Down && x.peer_unchanged) && !s.steps.iter().any(|x| x.dir == Dir::Down && x.steam_rate.is_some()) {
        out.push("The peer did not change its rate: the download steps are at the peer's default.".into());
    }
    out.push(if s.parallel_note.is_empty() {
        let p: Vec<_> = s.steps.iter().filter(|x| x.stage == "parallel").collect();
        if p.is_empty() { "Parallel connections: not run.".to_string() } else { "Parallel connections: see the table (stage parallel).".to_string() }
    } else {
        format!("{}.", s.parallel_note.trim_end_matches('.'))
    });
    for n in &s.notes {
        out.push(n.clone());
    }
    out.push(String::new());
    out.push(format!(
        "Best sustainable send rate: up (you to the peer) {}, down (the peer to you) {}.",
        mib_text(s.best[0]),
        mib_text(s.best[1])
    ));
    // the 80 MB save
    let wire = SAVE_MB * WIRE_FACTOR;
    for dir in [Dir::Up, Dir::Down] {
        let default_speed = speed_at(s, dir, s.default_rate, 1).unwrap_or(f64::from(s.default_rate));
        let best = s.best[dir.idx()];
        let best_speed = s
            .steps
            .iter()
            .filter(|x| x.dir == dir && x.asked == best && x.conns == 1 && x.failed.is_none() && x.mb >= 30.0)
            .filter_map(|x| x.speed)
            .next_back()
            .or_else(|| speed_at(s, dir, best, 1));
        let t = |rate: Option<f64>, mb: f64| throughput::estimate_secs(mb, rate).map_or_else(|| "unknown".to_string(), throughput::secs_text);
        out.push(format!(
            "An {SAVE_MB:.0} MB save, {}: at the default rate {} about {} ({} on the wire with base64, {} if binary); at the best rate {} about {} ({} if binary).",
            dir.text(),
            mib_text(s.default_rate),
            t(Some(default_speed), SAVE_MB),
            format_args!("{:.0} MB", wire),
            t(Some(default_speed), SAVE_MB / WIRE_FACTOR),
            mib_text(best),
            t(best_speed, SAVE_MB),
            t(best_speed, SAVE_MB / WIRE_FACTOR),
        ));
    }
    out.push(String::new());
    out.push("VERDICT".into());
    out.extend(verdict(s));
    out
}

/// The five lines of plain English.
pub fn verdict(s: &Summary) -> Vec<String> {
    let up = tracked_up_to(s, Dir::Up);
    let down = tracked_up_to(s, Dir::Down);
    let rate_or_none = |r: Option<i32>| r.map_or_else(|| "none of the rates".to_string(), mib_text);
    let mut lines = Vec::new();
    lines.push(format!(
        "1. Steam delivered about the rate we asked for up to {} when sending and up to {} when receiving.",
        rate_or_none(up),
        rate_or_none(down)
    ));
    let why = |dir: Dir| {
        s.steps
            .iter()
            .filter(|x| x.dir == dir && x.stage == "ladder")
            .find_map(|x| x.stop_note.clone())
            .unwrap_or_else(|| "no limit was found".to_string())
    };
    lines.push(format!("2. Above that, sending: {}; receiving: {}.", why(Dir::Up), why(Dir::Down)));
    let max_rtt = s.steps.iter().filter_map(|x| x.rtt.map(|r| r.2)).max();
    let stalls: u32 = s.steps.iter().map(|x| x.stalls).sum();
    let routes_changed = s.steps.iter().any(|x| x.route.contains("->"));
    lines.push(format!(
        "3. The idle round trip was {}, the highest during any step {}; stalls: {}; route changes: {}.",
        s.baseline.rtt_ms.map_or_else(|| "unknown".to_string(), |r| format!("{r} ms")),
        max_rtt.map_or_else(|| "unknown".to_string(), |r| format!("{r} ms")),
        stalls,
        if routes_changed { "yes" } else { "none" }
    ));
    let default_speed = |dir: Dir| speed_at(s, dir, s.default_rate, 1);
    let best_speed = |dir: Dir| speed_at(s, dir, s.best[dir.idx()], 1);
    let est = |v: Option<f64>| throughput::estimate_secs(SAVE_MB, v).map_or_else(|| "unknown".to_string(), throughput::secs_text);
    lines.push(format!(
        "4. An {SAVE_MB:.0} MB save takes about {} / {} (up / down) at the default rate and about {} / {} at the best rates.",
        est(default_speed(Dir::Up)),
        est(default_speed(Dir::Down)),
        est(best_speed(Dir::Up)),
        est(best_speed(Dir::Down))
    ));
    let rec = s.best[0].min(s.best[1]);
    lines.push(if rec <= s.default_rate {
        format!("5. Recommended default send rate: {} (nothing higher held up on this path).", mib_text(s.default_rate))
    } else {
        format!("5. Recommended default send rate: {} (the lower of the two best rates, without a safety stop).", mib_text(rec))
    });
    lines
}

/// The mode's outcome for the results file.
pub fn outcome(s: &Summary, partial: bool) -> Outcome {
    let mut lines = report_lines(s);
    if partial {
        lines.insert(0, "THE TEST WAS NOT FINISHED: what follows is what was measured until it stopped.".into());
    }
    Outcome { name: "Rate ladder (comprehensive speed test)".into(), ok: !s.steps.is_empty() || s.finished, lines }
}

// ---- the real path

/// Measures steps on the Steam probe of a [`Ctx`].
pub struct ProbePath<'a, 'b, 'c> {
    ctx: &'a mut Ctx<'b, 'c>,
    started: Instant,
    default_rate: i32,
}

impl<'a, 'b, 'c> ProbePath<'a, 'b, 'c> {
    pub fn new(ctx: &'a mut Ctx<'b, 'c>, default_rate: i32) -> Self {
        ProbePath { ctx, started: Instant::now(), default_rate }
    }
}

/// Collects what is seen while a step runs.
#[derive(Default)]
struct Sampler {
    pings: Vec<u32>,
    steam_rate: u32,
    queue: u32,
    quality: Option<f32>,
    route_first: String,
    route_notes: Vec<String>,
    route_now: String,
}

impl Sampler {
    fn take(&mut self, ctx: &Ctx<'_, '_>, at: f64) {
        let Some(test) = ctx.probe.report().test else { return };
        if let Some(p) = test.ping_ms {
            self.pings.push(p);
        }
        if let Some(r) = test.send_rate {
            self.steam_rate = self.steam_rate.max(r);
        }
        if !test.route.is_empty() {
            if self.route_first.is_empty() {
                self.route_first = test.route.clone();
            } else if test.route != self.route_now {
                self.route_notes.push(format!("{} -> {} at {:.0} s", self.route_now, test.route, at));
            }
            self.route_now = test.route;
        }
        if let Some(l) = ctx.probe.link_now() {
            self.queue = self.queue.max(l.pending).max(l.steam.pending_reliable);
            self.quality = Some(self.quality.map_or(l.quality, |q| q.min(l.quality)));
        }
    }

    fn rtt(&self) -> Option<(u32, f64, u32)> {
        let min = *self.pings.iter().min()?;
        let max = *self.pings.iter().max()?;
        Some((min, self.pings.iter().map(|p| f64::from(*p)).sum::<f64>() / self.pings.len() as f64, max))
    }

    fn route(&self) -> String {
        if self.route_notes.is_empty() {
            self.route_first.clone()
        } else {
            format!("{} ({})", self.route_first, self.route_notes.join(", "))
        }
    }
}

/// The rate the peer reported in the run's rate note ("peer set its send rate to N B/s (Steam reports M)").
pub fn peer_reported_rate(note: &str) -> Option<u32> {
    let rest = &note[note.find("peer set its send rate")?..];
    let tail = &rest[rest.find("Steam reports ")? + "Steam reports ".len()..];
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|r| *r > 0)
}

impl Path for ProbePath<'_, '_, '_> {
    fn say(&mut self, text: &str) {
        self.ctx.say(text);
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn baseline(&mut self) -> Result<Baseline, Stop> {
        let mut s = Sampler::default();
        let t0 = Instant::now();
        let wait = self.ctx.env.timing.link_wait;
        while t0.elapsed() < wait {
            self.ctx.tick()?;
            s.take(self.ctx, t0.elapsed().as_secs_f64());
        }
        let rtt = s.rtt().map(|r| r.0);
        Ok(Baseline { rtt_ms: rtt, route: s.route() })
    }

    fn measure(&mut self, req: &Req) -> Result<Step, Stop> {
        let mut step = Step {
            stage: req.stage,
            dir: req.dir,
            conns: req.conns,
            asked: req.rate,
            ..Step::default()
        };
        let spec = RunSpec {
            variant: Variant::Reliable(req.conns),
            total: u64::from(req.mb) * MB,
            rate: Some(req.rate),
            up: req.dir == Dir::Up,
            down: req.dir == Dir::Down,
            cap: req.cap,
        };
        let peer = self.ctx.peer;
        self.ctx.probe.start_run(peer, spec);
        let began = Instant::now();
        let mut s = Sampler::default();
        let mut next_sample = Instant::now();
        let mut last_say = Instant::now();
        let outcome = loop {
            if let Err(stop) = self.ctx.tick() {
                self.ctx.probe.stop_run();
                return Err(stop);
            }
            if Instant::now() >= next_sample {
                next_sample = Instant::now() + Duration::from_millis(250);
                s.take(self.ctx, began.elapsed().as_secs_f64());
            }
            if let Some(o) = self.ctx.probe.run_outcome() {
                break o;
            }
            if !self.ctx.probe.run_active() {
                step.failed = Some("the step could not be started (Steam is not ready or the connection is gone)".into());
                return Ok(step);
            }
            if began.elapsed() > req.cap * 2 + Duration::from_secs(40) {
                self.ctx.probe.stop_run();
            }
            let every = self.ctx.env.timing.progress_every;
            if last_say.elapsed() >= every.max(Duration::from_secs(5)) {
                last_say = Instant::now();
                if let Some(speed) = self.ctx.probe.report().speed {
                    self.ctx.say(&format!("    {}", speed.status));
                }
            }
        };
        step.rtt = s.rtt();
        step.queue_peak = s.queue;
        step.quality_min = s.quality;
        step.route = s.route();
        if let Some(why) = outcome.failure {
            step.failed = Some(why);
            return Ok(step);
        }
        let Some(RunKind::Reliable(res)) = outcome.kind else {
            step.failed = Some("no result".into());
            return Ok(step);
        };
        let d = if req.dir == Dir::Up { &res.up } else { &res.down };
        step.mb = d.bytes as f64 / MB as f64;
        step.speed = d.rate;
        step.capped = d.capped;
        step.queue_peak = step.queue_peak.max(d.peak_pending);
        step.longest_pause_ms = d.longest_pause_ms;
        step.stalls = u32::from(d.longest_pause_ms >= STALL_MS);
        if let Some(peak) = d.ping_peak_ms {
            step.rtt = Some(step.rtt.map_or((peak, f64::from(peak), peak), |(a, b, c)| (a.min(peak), b, c.max(peak))));
        }
        if !res.route_notes.is_empty() {
            step.route = format!("{} ({})", res.route, res.route_notes.join(", "));
        } else if step.route.is_empty() {
            step.route = res.route.clone();
        }
        match req.dir {
            Dir::Up => step.steam_rate = Some(s.steam_rate).filter(|r| *r > 0).or(res.rate_effective),
            Dir::Down => {
                step.steam_rate = peer_reported_rate(&res.rate_note);
                step.peer_unchanged = step.steam_rate.is_none();
            }
        }
        Ok(step)
    }

    fn restore(&mut self) {
        // ends a run in progress (which puts our rate back and tells the peer to stop and restore its own), then
        // gives the core a moment to carry it out
        self.ctx.probe.stop_run();
        for _ in 0..5 {
            self.ctx.probe.poll();
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.default_rate;
    }

    fn publish(&mut self, summary: &Summary) {
        self.ctx.partial = Some(outcome(summary, !summary.finished));
    }
}

/// The mode in the table: the comprehensive test.
pub fn ladder_mode(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
    let default_rate = ctx.default_rate;
    let mut summary = Summary { default_rate, ..Summary::default() };
    let result = {
        let mut path = ProbePath::new(ctx, default_rate);
        run(&mut path, &mut summary)
    };
    ctx.partial = Some(outcome(&summary, !summary.finished));
    result?;
    Ok(outcome(&summary, false))
}

#[cfg(test)]
mod tests;
