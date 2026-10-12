use super::*;
use crate::speedtest::suite::{run_tester, Env, Mode, Timing};
use crate::steamnet::fake::FakeMachine;
use crate::steamnet::{CoreRef, Probe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

/// A path that carries at most `cap` bytes/s, inflates the round trip above `inflate_above` and stalls above `stall_above`.
struct Sim {
    cap: f64,
    base: u32,
    inflate_above: i32,
    stall_above: i32,
    clock: Duration,
    restored: u32,
    old_peer: bool,
    abort_at: Option<usize>,
    parallel: bool,
    calls: Vec<Req>,
    published: usize,
    auto_lines: Option<Vec<String>>,
    auto_asked: Vec<u32>,
}

impl Sim {
    fn new(cap_mb: f64) -> Sim {
        Sim {
            cap: cap_mb * MB as f64,
            base: 230,
            inflate_above: i32::MAX,
            stall_above: i32::MAX,
            clock: Duration::ZERO,
            restored: 0,
            old_peer: false,
            abort_at: None,
            parallel: true,
            calls: Vec::new(),
            published: 0,
            auto_lines: None,
            auto_asked: Vec::new(),
        }
    }
}

impl Path for Sim {
    fn say(&mut self, _: &str) {}
    fn elapsed(&self) -> Duration {
        self.clock
    }
    fn baseline(&mut self) -> Result<Baseline, Stop> {
        Ok(Baseline { rtt_ms: Some(self.base), route: "direct".into() })
    }
    fn measure(&mut self, req: &Req) -> Result<Step, Stop> {
        if self.abort_at == Some(self.calls.len()) {
            return Err(Stop);
        }
        self.calls.push(req.clone());
        let asked = f64::from(req.rate) * f64::from(req.conns);
        let mut speed = (asked * 0.97).min(self.cap);
        let mut steam = Some(req.rate as u32);
        let mut unchanged = false;
        if self.old_peer && req.dir == Dir::Down {
            speed = speed.min(f64::from(MIB) * 0.97);
            steam = None;
            unchanged = true;
        }
        let bytes = f64::from(req.mb) * MB as f64;
        self.clock += Duration::from_secs_f64(bytes / speed);
        let rtt = if req.rate > self.inflate_above {
            (self.base, f64::from(self.base) * 2.0, self.base * 4)
        } else {
            (self.base, f64::from(self.base) + 5.0, self.base + 20)
        };
        let stall = req.rate > self.stall_above;
        Ok(Step {
            stage: req.stage,
            dir: req.dir,
            conns: req.conns,
            asked: req.rate,
            steam_rate: steam,
            peer_unchanged: unchanged,
            mb: f64::from(req.mb),
            speed: Some(speed),
            rtt: Some(rtt),
            stalls: u32::from(stall),
            longest_pause_ms: if stall { 7000 } else { 100 },
            route: "direct".into(),
            ..Step::default()
        })
    }
    fn parallel_available(&self) -> bool {
        self.parallel
    }
    fn auto(&mut self, mb: u32) -> Result<Option<Vec<String>>, Stop> {
        if self.auto_lines.is_some() {
            self.auto_asked.push(mb);
        }
        Ok(self.auto_lines.clone())
    }
    fn restore(&mut self) {
        self.restored += 1;
    }
    fn publish(&mut self, _: &Summary) {
        self.published += 1;
    }
}

fn go(sim: &mut Sim) -> (Summary, Result<(), Stop>) {
    let mut s = Summary { default_rate: MIB, ..Summary::default() };
    let r = run(sim, &mut s);
    (s, r)
}

fn ladder_rates(s: &Summary, dir: Dir) -> Vec<i32> {
    s.steps.iter().filter(|x| x.stage == "ladder" && x.dir == dir).map(|x| x.asked).collect()
}

#[test]
fn the_rtt_limit_is_three_times_or_150_ms_over_the_baseline() {
    assert_eq!(rtt_limit(230), 380);
    assert_eq!(rtt_limit(20), 60);
    assert_eq!(rtt_limit(0), 0);
}

#[test]
fn the_ladder_climbs_while_the_path_follows_and_stops_when_it_does_not() {
    let mut sim = Sim::new(3.5);
    let (s, r) = go(&mut sim);
    assert!(r.is_ok() && s.finished);
    // 1, 2, 4 grow (1.0, 2.0, 3.5 MB/s); 4 MiB/s is the top of the ladder
    assert_eq!(ladder_rates(&s, Dir::Up), [MIB, 2 * MIB, 4 * MIB]);
    assert_eq!(s.best, [4 * MIB, 4 * MIB]);
    let last = s.steps.iter().rfind(|x| x.stage == "ladder" && x.dir == Dir::Up).unwrap();
    assert!(last.stop_note.as_deref().unwrap().contains("top of the ladder (4 MiB/s)"), "{last:?}");
    assert!(sim.restored >= 1);
    assert!(sim.calls.iter().all(|c| c.rate <= 4 * MIB));

    // a path that stops growing at 2 MiB/s: the 4 MiB/s step is the one that does not grow
    let mut sim = Sim::new(2.0);
    let (s, _) = go(&mut sim);
    let last = s.steps.iter().rfind(|x| x.stage == "ladder" && x.dir == Dir::Up).unwrap();
    assert_eq!(last.asked, 4 * MIB);
    assert!(last.stop_note.as_deref().unwrap().contains("less than 10 %"), "{last:?}");
}

#[test]
fn validation_runs_at_the_best_rate_and_the_save_size_only_from_4_mib() {
    let mut sim = Sim::new(3.5);
    let (s, _) = go(&mut sim);
    let stages: Vec<_> = s.steps.iter().filter(|x| x.dir == Dir::Up).map(|x| (x.stage, x.mb as u32, x.asked)).collect();
    assert!(stages.contains(&("validate", 32, 4 * MIB)), "{stages:?}");
    assert!(stages.contains(&("save size", 80, 4 * MIB)), "{stages:?}");
    // a path that holds only 2 MiB/s: validation at 2 MiB/s, no 80 MB run
    let mut sim = Sim::new(2.2);
    let (s, _) = go(&mut sim);
    assert_eq!(s.best, [2 * MIB, 2 * MIB]);
    assert!(s.steps.iter().any(|x| x.stage == "validate" && x.asked == 2 * MIB));
    assert!(!s.steps.iter().any(|x| x.stage == "save size"));
}

#[test]
fn a_path_that_carries_only_the_default_gets_no_validation() {
    let mut sim = Sim::new(1.0);
    let (s, _) = go(&mut sim);
    assert_eq!(s.best, [MIB, MIB]);
    assert_eq!(ladder_rates(&s, Dir::Up), [MIB, 2 * MIB]);
    assert!(!s.steps.iter().any(|x| x.stage == "validate" || x.stage == "save size"));
    assert!(s.notes.iter().any(|n| n.contains("no validation run")));
    let verdict = verdict(&s).join("\n");
    assert!(verdict.contains("Recommended default send rate: 1 MiB/s"), "{verdict}");
}

#[test]
fn a_round_trip_above_the_limit_stops_the_climb_and_is_not_sustainable() {
    let mut sim = Sim::new(100.0);
    sim.inflate_above = 2 * MIB;
    let (s, _) = go(&mut sim);
    assert_eq!(ladder_rates(&s, Dir::Up), [MIB, 2 * MIB, 4 * MIB]);
    assert_eq!(s.best, [2 * MIB, 2 * MIB], "the unsafe step is not the best");
    let stop = s.steps.iter().find(|x| x.asked == 4 * MIB).unwrap().stop_note.clone().unwrap();
    assert!(stop.contains("round trip rose to 920 ms") && stop.contains("limit 380 ms"), "{stop}");
}

#[test]
fn a_stall_stops_the_climb() {
    let mut sim = Sim::new(100.0);
    sim.stall_above = 2 * MIB;
    let (s, _) = go(&mut sim);
    assert_eq!(s.best, [2 * MIB, 2 * MIB]);
    let note = s.steps.iter().find(|x| x.asked == 4 * MIB).unwrap().stop_note.clone().unwrap();
    assert!(note.contains("a stall: no data for 7 s"), "{note}");
}

#[test]
fn the_ladder_never_goes_above_4_mib_and_ends_there() {
    let mut sim = Sim::new(1000.0);
    let (s, _) = go(&mut sim);
    assert_eq!(ladder_rates(&s, Dir::Down).last(), Some(&(4 * MIB)));
    assert_eq!(s.best, [4 * MIB, 4 * MIB]);
    assert!(sim.calls.iter().all(|c| c.rate <= 4 * MIB));
    let last = s.steps.iter().rfind(|x| x.stage == "ladder").unwrap();
    assert!(last.stop_note.as_deref().unwrap().contains("top of the ladder"));
}

#[test]
fn an_abort_restores_the_rates_and_keeps_what_was_measured() {
    let mut sim = Sim::new(100.0);
    sim.abort_at = Some(3);
    let (s, r) = go(&mut sim);
    assert_eq!(r, Err(Stop));
    assert_eq!(sim.restored, 1);
    assert_eq!(s.steps.len(), 3);
    assert!(!s.finished);
    let o = outcome(&s, true);
    assert!(o.lines[0].contains("NOT FINISHED"), "{:?}", o.lines);
    assert!(o.lines.iter().any(|l| l.starts_with("ladder")), "{:?}", o.lines);
    assert!(sim.published >= 3);
}

#[test]
fn an_old_peer_is_reported_not_fatal() {
    let mut sim = Sim::new(100.0);
    sim.old_peer = true;
    let (s, r) = go(&mut sim);
    assert!(r.is_ok());
    // the downloads cannot grow past the peer's default: the climb stops there
    assert_eq!(s.best[1], MIB);
    let text = report_lines(&s).join("\n");
    assert!(text.contains("The peer did not change its rate: the download steps are at the peer's default."), "{text}");
    assert!(text.contains("peer old"));
}

#[test]
fn parallel_connections_are_run_or_declared_unavailable() {
    let mut sim = Sim::new(100.0);
    sim.parallel = false;
    let (s, _) = go(&mut sim);
    assert_eq!(s.parallel_note, "parallel connections: not available in this version");
    assert!(report_lines(&s).join("\n").contains("parallel connections: not available in this version"));
    // a line of 3.5 MB/s: x2 and x4 scale, x8 does not, so x16 is skipped
    let mut sim = Sim::new(3.5);
    let (s, _) = go(&mut sim);
    let par: Vec<_> = s.steps.iter().filter(|x| x.stage == "parallel").map(|x| (x.conns, x.dir)).collect();
    assert_eq!(par, [(2, Dir::Up), (2, Dir::Down), (4, Dir::Up), (4, Dir::Down), (8, Dir::Up), (8, Dir::Down)]);
    assert!(s.steps.iter().filter(|x| x.stage == "parallel").all(|x| x.asked == MIB), "1 MiB/s on every connection");
    assert!(s.notes.iter().any(|n| n.contains("up x8: did not scale")), "{:?}", s.notes);
    // a line that scales all the way: 2, 4, 8 and 16, both ways
    let mut sim = Sim::new(100.0);
    let (s, _) = go(&mut sim);
    let par: Vec<u32> = s.steps.iter().filter(|x| x.stage == "parallel" && x.dir == Dir::Up).map(|x| x.conns).collect();
    assert_eq!(par, [2, 4, 8, 16]);
}

#[test]
fn the_auto_run_ends_the_test_and_its_block_is_in_the_report() {
    let mut sim = Sim::new(100.0);
    let (s, _) = go(&mut sim);
    assert!(s.auto.is_empty(), "this path has no Auto run");
    assert!(s.notes.iter().any(|n| n.contains("Auto (scaled): not available")));
    sim.auto_lines = Some(vec!["Auto (scaled) speed test, 80 MB each way: ...".into()]);
    let mut s = Summary { default_rate: MIB, ..Summary::default() };
    run(&mut sim, &mut s).unwrap();
    assert_eq!(sim.auto_asked, vec![80]);
    let text = report_lines(&s).join(
        "
",
    );
    assert!(text.contains("Auto (scaled), 80 MB each way (the way a resync save is sent):"), "{text}");
    assert!(text.contains("  Auto (scaled) speed test, 80 MB each way: ..."), "{text}");
    // out of time: skipped, and said
    let mut sim = Sim::new(100.0);
    sim.clock = TOTAL_BUDGET;
    sim.auto_lines = Some(vec!["x".into()]);
    let (s, _) = go(&mut sim);
    assert!(sim.auto_asked.is_empty());
    assert!(s.notes.iter().any(|n| n.contains("Auto (scaled) run was skipped (time)")), "{:?}", s.notes);
}

#[test]
fn a_slow_run_shortens_itself() {
    let mut sim = Sim::new(100.0);
    sim.clock = TOTAL_BUDGET;
    let (s, _) = go(&mut sim);
    assert!(s.notes.iter().any(|n| n.contains("stopped early")), "{:?}", s.notes);
    assert!(s.steps.iter().all(|x| x.stage != "validate"));
}

#[test]
fn the_report_has_the_table_the_best_rates_the_estimates_and_five_verdict_lines() {
    let mut sim = Sim::new(3.5);
    let (s, _) = go(&mut sim);
    let lines = report_lines(&s);
    let text = lines.join("\n");
    for needle in [
        "Idle round trip: 230 ms. Route at the start: direct.",
        "Safety limit for the round trip during a step: 380 ms.",
        "stage     dir  conns      asked Steam says",
        "Best sustainable send rate: up (you to the peer) 4 MiB/s, down (the peer to you) 4 MiB/s.",
        "An 80 MB save, up:",
        "107 MB on the wire with base64",
        "VERDICT",
    ] {
        assert!(text.contains(needle), "missing {needle:?} in\n{text}");
    }
    let verdict_start = lines.iter().position(|l| l == "VERDICT").unwrap();
    assert_eq!(lines.len() - verdict_start - 1, 5, "{lines:?}");
    assert!(lines[verdict_start + 5].starts_with("5. Recommended default send rate: 4 MiB/s"), "{lines:?}");
}

#[test]
fn judging_a_step() {
    let ok = Step { speed: Some(2e6), rtt: Some((230, 240.0, 300)), ..Step::default() };
    assert_eq!(judge(Some(1e6), &ok, Some(230)), Judgement { sustainable: true, keep_climbing: true, why: None });
    let slow = Step { speed: Some(1.05e6), ..ok.clone() };
    assert!(!judge(Some(1e6), &slow, Some(230)).keep_climbing);
    let failed = Step { failed: Some("x".into()), ..ok.clone() };
    assert!(!judge(None, &failed, None).sustainable);
    let capped = Step { capped: true, ..ok.clone() };
    let j = judge(None, &capped, None);
    assert!(j.sustainable && !j.keep_climbing);
    // no baseline: the round trip rule cannot apply
    let high = Step { rtt: Some((500, 600.0, 900)), ..ok };
    assert!(judge(None, &high, None).sustainable);
    assert!(!judge(None, &high, Some(230)).sustainable);
}

#[test]
fn the_peer_rate_is_read_from_the_run_note() {
    let note = "own send rate set to 4194304 B/s, Steam reports 4194304 B/s (x); peer set its send rate to 4194304 B/s (Steam reports 4194304)";
    assert_eq!(peer_reported_rate(note), Some(4_194_304));
    assert_eq!(
        peer_reported_rate(
            "own send rate set to 5 B/s, Steam reports 5 B/s; peer did not change its rate (an older program)"
        ),
        None
    );
    assert_eq!(peer_reported_rate(""), None);
}

// ---- on the fake Steam

fn responder_thread(core: CoreRef, stop: Arc<AtomicBool>, old: bool) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut probe = Probe::on(core);
        if old {
            probe.act_like_an_old_program();
        }
        while !stop.load(Ordering::Relaxed) {
            probe.poll();
            thread::sleep(Duration::from_millis(2));
        }
    })
}

type FakeRun = (crate::speedtest::suite::TesterReport, Vec<(u32, i32, i32)>, i32);

fn tester_on_fake(old: bool, cancel_after: Option<Duration>) -> FakeRun {
    let (a, b) = (FakeMachine::new(), FakeMachine::new());
    let (core_a, core_b) = (a.core(), b.core());
    let stop = Arc::new(AtomicBool::new(false));
    let responder = responder_thread(core_b.handle(), Arc::clone(&stop), old);
    let started = Instant::now();
    let cancelled = || cancel_after.is_some_and(|d| started.elapsed() > d);
    let mut say = |_: &str| {};
    let mut pump = || {};
    let timing = Timing { link_wait: Duration::from_millis(100), ..Timing::quick() };
    let mut env = Env { say: &mut say, pump: &mut pump, cancelled: &cancelled, timing };
    const MODES: &[Mode] = &[Mode { name: "ladder", run: ladder_mode }];
    let handle = core_a.handle();
    let report = run_tester(&mut env, &handle, b.steam_id(), MODES, &mut |_| {});
    stop.store(true, Ordering::Relaxed);
    responder.join().unwrap();
    (report, a.connection_config(), handle.send_rate_setting())
}

#[test]
fn the_whole_ladder_runs_on_the_fake_steam_and_leaves_the_default_rate() {
    let (report, config, default) = tester_on_fake(false, None);
    assert!(report.finished && report.error.is_none(), "{report:?}");
    let outcome = &report.outcomes[0];
    assert!(outcome.ok);
    let text = outcome.lines.join("\n");
    assert!(text.contains("VERDICT") && text.contains("Best sustainable send rate"), "{text}");
    assert!(text.contains("ladder    up"), "{text}");
    // every send rate that was set on a connection ends at the default
    let last_min = config.iter().rfind(|c| c.1 == 10).map(|c| c.2);
    assert_eq!(last_min, Some(default), "{config:?}");
}

#[test]
fn an_old_peer_does_not_break_the_ladder_on_the_fake_steam() {
    let (report, _, _) = tester_on_fake(true, None);
    assert!(report.finished && report.error.is_none(), "{report:?}");
    let text = report.outcomes[0].lines.join("\n");
    assert!(text.contains("The peer did not change its rate"), "{text}");
}

#[test]
fn ctrl_c_during_the_ladder_keeps_the_partial_result_and_restores() {
    let (report, config, default) = tester_on_fake(false, Some(Duration::from_millis(400)));
    assert!(report.aborted);
    let o = report.outcomes.first().expect("the partial result is kept");
    assert!(o.lines[0].contains("NOT FINISHED"), "{:?}", o.lines);
    let last_min = config.iter().rfind(|c| c.1 == 10).map(|c| c.2);
    assert_eq!(last_min, Some(default), "{config:?}");
}
