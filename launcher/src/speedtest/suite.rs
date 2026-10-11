//! The tester: waits for Steam, connects to the peer, runs the table of test modes in order.

use std::thread;
use std::time::{Duration, Instant};

use crate::steamnet::{CoreRef, Probe};

use super::bulk_modes;

/// Waiting times and the polling step; [`Timing::real`] for the program, [`Timing::quick`] in the tests.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Sleep between steps of the loops.
    pub poll: Duration,
    /// How long Steam may take to become ready.
    pub ready_wait: Duration,
    /// How long one attempt to reach the other PC lasts.
    pub connect_window: Duration,
    /// Attempts to reach the other PC (the program says so before each retry).
    pub connect_attempts: u32,
    /// Pause after a failed connection before connecting again within a window.
    pub retry_pause: Duration,
    /// Longest a single test mode may take.
    pub mode_max: Duration,
    /// Longest wait for a started speed test to appear.
    pub start_wait: Duration,
    /// How often progress is printed.
    pub progress_every: Duration,
    /// How long to wait for Steam's first ping measurement after connecting.
    pub link_wait: Duration,
}

impl Timing {
    pub fn real() -> Timing {
        Timing {
            poll: Duration::from_millis(20),
            ready_wait: Duration::from_secs(90),
            connect_window: Duration::from_secs(120),
            connect_attempts: 2,
            retry_pause: Duration::from_secs(5),
            mode_max: Duration::from_secs(20 * 60),
            start_wait: Duration::from_secs(5),
            progress_every: Duration::from_secs(2),
            link_wait: Duration::from_secs(3),
        }
    }

    #[cfg(test)]
    pub fn quick() -> Timing {
        Timing {
            poll: Duration::from_millis(2),
            ready_wait: Duration::from_secs(3),
            connect_window: Duration::from_millis(600),
            connect_attempts: 2,
            retry_pause: Duration::from_millis(50),
            mode_max: Duration::from_secs(30),
            start_wait: Duration::from_millis(500),
            progress_every: Duration::from_millis(20),
            link_wait: Duration::from_millis(300),
        }
    }
}

/// What the loops need from the program around them: where text goes, what to do each step, whether to stop.
pub struct Env<'a> {
    pub say: &'a mut dyn FnMut(&str),
    /// Called every step (the program runs `SteamAPI_RunCallbacks` here).
    pub pump: &'a mut dyn FnMut(),
    /// True once the player pressed Ctrl+C.
    pub cancelled: &'a dyn Fn() -> bool,
    pub timing: Timing,
}

impl Env<'_> {
    pub fn say(&mut self, text: &str) {
        (self.say)(text);
    }
}

/// The player aborted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stop;

/// What a test mode brings back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub name: String,
    pub ok: bool,
    /// The result block when `ok`, otherwise the reason(s) it failed.
    pub lines: Vec<String>,
}

/// Everything a mode may use.
pub struct Ctx<'a, 'b> {
    pub probe: &'a mut Probe,
    pub peer: u64,
    pub env: &'a mut Env<'b>,
    /// The send rate the connections have by default (what a mode restores after it changed them).
    pub default_rate: i32,
    /// What a long mode has measured so far; kept in the report when the player stops the run.
    pub partial: Option<Outcome>,
}

impl Ctx<'_, '_> {
    /// One step of waiting: callbacks, the probe, a short sleep; `Err` after Ctrl+C.
    pub fn tick(&mut self) -> Result<(), Stop> {
        if (self.env.cancelled)() {
            return Err(Stop);
        }
        (self.env.pump)();
        self.probe.poll();
        thread::sleep(self.env.timing.poll);
        Ok(())
    }

    pub fn say(&mut self, text: &str) {
        self.env.say(text);
    }
}

/// One entry of the table of test modes.
pub struct Mode {
    pub name: &'static str,
    pub run: fn(&mut Ctx<'_, '_>) -> Result<Outcome, Stop>,
}

/// The reliable-stream test (the launcher's speed test) of `mb` MB in each direction.
pub fn reliable(ctx: &mut Ctx<'_, '_>, name: &str, mb: u32) -> Result<Outcome, Stop> {
    let fail = |why: String| Outcome { name: name.to_string(), ok: false, lines: vec![why] };
    let before = ctx.probe.report().speed.map(|s| s.id);
    ctx.probe.speed_test(ctx.peer, mb);
    let started = Instant::now();
    let Some(first) = ctx.probe.report().speed.filter(|s| Some(s.id) != before) else {
        return Ok(fail("the test could not be started (Steam is not ready or the connection is gone)".into()));
    };
    let id = first.id;
    let mut last_say = Instant::now();
    loop {
        ctx.tick()?;
        let Some(speed) = ctx.probe.report().speed.filter(|s| s.id == id) else {
            return Ok(fail("the test was replaced".into()));
        };
        if !speed.running {
            return Ok(if speed.log.is_some() && !speed.lines.is_empty() {
                Outcome { name: name.to_string(), ok: true, lines: speed.lines }
            } else {
                fail(speed.status)
            });
        }
        if last_say.elapsed() >= ctx.env.timing.progress_every {
            last_say = Instant::now();
            ctx.say(&format!("  {}", speed.status));
        }
        if started.elapsed() > ctx.env.timing.mode_max {
            return Ok(fail(format!("gave up after {} minutes", ctx.env.timing.mode_max.as_secs() / 60)));
        }
    }
}

fn reliable_8(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
    reliable(ctx, "Reliable stream, 8 MB each way", 8)
}
fn reliable_32(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
    reliable(ctx, "Reliable stream, 32 MB each way", 32)
}
fn reliable_80(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
    reliable(ctx, "Reliable stream, 80 MB each way (the size of a big savegame)", 80)
}

/// The test modes, in the order they run. Another mode is one more line: `Mode { name, run }`, where `run` takes the
/// [`Ctx`] (the probe, the peer, the console) and returns an [`Outcome`].
pub const MODES: &[Mode] = &[
    Mode { name: "rate ladder (comprehensive)", run: super::ladder::ladder_mode },
    Mode { name: "datagrams 32 MB", run: bulk_modes::datagrams_32 },
];

/// The earlier tests, behind `--classic`: the reliable stream at the configured rate, 8, 32 and 80 MB each way.
pub const CLASSIC_MODES: &[Mode] = &[
    Mode { name: "reliable 8 MB", run: reliable_8 },
    Mode { name: "reliable 32 MB", run: reliable_32 },
    Mode { name: "reliable 80 MB", run: reliable_80 },
    Mode { name: "datagrams 8 MB", run: bulk_modes::datagrams_8 },
    Mode { name: "datagrams 32 MB", run: bulk_modes::datagrams_32 },
    Mode { name: "datagrams 80 MB", run: bulk_modes::datagrams_80 },
    Mode { name: "parallel x2, 32 MB", run: bulk_modes::parallel_2_32 },
    Mode { name: "parallel x4, 32 MB", run: bulk_modes::parallel_4_32 },
];

/// How the tester got on.
#[derive(Debug, Clone, Default)]
pub struct TesterReport {
    pub own_id: Option<u64>,
    pub peer: u64,
    pub route: String,
    pub ping_ms: Option<u32>,
    pub connected: bool,
    pub outcomes: Vec<Outcome>,
    /// Steam status dumps, when the core can make them (later).
    pub dumps: Vec<String>,
    /// Why the run ended early, in plain English.
    pub error: Option<String>,
    pub aborted: bool,
    pub finished: bool,
}

/// Waits until Steam's networking is ready; `Ok(Err(text))` when it never is.
pub fn wait_ready(env: &mut Env<'_>, probe: &mut Probe, core: &CoreRef) -> Result<Result<Option<u64>, String>, Stop> {
    let started = Instant::now();
    let mut last_say = Instant::now();
    loop {
        if (env.cancelled)() {
            return Err(Stop);
        }
        (env.pump)();
        probe.poll();
        let status = core.status();
        if status.ready {
            return Ok(Ok(status.steam_id));
        }
        if started.elapsed() > env.timing.ready_wait {
            return Ok(Err(format!(
                "Steam started, but its network is not ready after {} seconds ({}). Check that Steam is online (not in Offline Mode) and that you are logged in, then run the program again.",
                env.timing.ready_wait.as_secs(),
                status.note
            )));
        }
        if last_say.elapsed() >= Duration::from_secs(10).min(env.timing.ready_wait) {
            last_say = Instant::now();
            env.say(&format!("Waiting for Steam's network ({})...", status.note));
        }
        thread::sleep(env.timing.poll);
    }
}

/// Connects to the peer, retrying; `Ok(Err(text))` when the other PC never answers.
fn connect(ctx: &mut Ctx<'_, '_>) -> Result<Result<(), String>, Stop> {
    let timing = ctx.env.timing;
    for attempt in 1..=timing.connect_attempts {
        ctx.probe.test(ctx.peer);
        ctx.say("Waiting for the other PC to start its program...");
        let window = Instant::now();
        let mut last_say = Instant::now();
        let mut told_failure = false;
        let mut retry_at: Option<Instant> = None;
        while window.elapsed() < timing.connect_window {
            ctx.tick()?;
            let Some(test) = ctx.probe.report().test else {
                ctx.probe.test(ctx.peer);
                continue;
            };
            if test.state == "connected" {
                return Ok(Ok(()));
            }
            if test.state.starts_with("failed") {
                if !told_failure {
                    told_failure = true;
                    ctx.say(&format!("  The other PC has not answered yet ({}). I will keep trying.", test.state));
                }
                let at = *retry_at.get_or_insert_with(|| Instant::now() + timing.retry_pause);
                if Instant::now() >= at {
                    retry_at = None;
                    told_failure = false;
                    ctx.probe.test(ctx.peer);
                }
            }
            if last_say.elapsed() >= Duration::from_secs(15).min(timing.connect_window) {
                last_say = Instant::now();
                ctx.say(&format!(
                    "  Still waiting for the other PC ({} s of {} s)...",
                    window.elapsed().as_secs(),
                    timing.connect_window.as_secs()
                ));
            }
        }
        if attempt < timing.connect_attempts {
            ctx.say("No answer after the waiting time. Trying again...");
        }
    }
    Ok(Err(format!(
        "The other PC did not answer ({} attempts of {} seconds). Check that its program is running, that both PCs use the same steam_appid.txt (480), and that the Steam ID in peer.txt is the other player's. Then run this program again.",
        timing.connect_attempts,
        timing.connect_window.as_secs()
    )))
}

/// Runs the modes against `peer`. `save` is called after every mode and at the end (it writes the results file).
pub fn run_tester(
    env: &mut Env<'_>,
    core: &CoreRef,
    peer: u64,
    modes: &[Mode],
    save: &mut dyn FnMut(&TesterReport),
) -> TesterReport {
    let mut report = TesterReport { peer, ..TesterReport::default() };
    let mut probe = Probe::on(core.clone());
    match drive(env, &mut probe, core, modes, save, &mut report) {
        Ok(()) => report.finished = report.error.is_none(),
        Err(Stop) => {
            report.aborted = true;
            env.say("Stopped by Ctrl+C.");
        }
    }
    save(&report);
    drop(probe);
    report
}

fn drive(
    env: &mut Env<'_>,
    probe: &mut Probe,
    core: &CoreRef,
    modes: &[Mode],
    save: &mut dyn FnMut(&TesterReport),
    report: &mut TesterReport,
) -> Result<(), Stop> {
    let peer = report.peer;
    env.say("Starting Steam networking...");
    match wait_ready(env, probe, core)? {
        Ok(id) => report.own_id = id,
        Err(why) => {
            report.error = Some(why);
            return Ok(());
        }
    }
    if let Some(id) = report.own_id {
        env.say(&format!("Your Steam ID: {id}"));
        if id == peer {
            report.error = Some(
                "The Steam ID to test against is your own. peer.txt must hold the OTHER player's SteamID64.".into(),
            );
            return Ok(());
        }
    }
    env.say(&format!("Connecting to Steam ID {peer}..."));
    let mut ctx = Ctx { probe, peer, env, default_rate: core.send_rate_setting(), partial: None };
    if let Err(why) = connect(&mut ctx)? {
        report.error = Some(why);
        return Ok(());
    }
    report.connected = true;
    // Steam measures the ping a moment after the connection is up
    let wait = Instant::now();
    while wait.elapsed() < ctx.env.timing.link_wait {
        ctx.tick()?;
        if ctx.probe.report().test.is_some_and(|t| t.ping_ms.is_some()) {
            break;
        }
    }
    if let Some(test) = ctx.probe.report().test {
        report.route = test.route;
        report.ping_ms = test.ping_ms;
    }
    ctx.say(&format!(
        "Connected. Route: {}, round trip {}.",
        if report.route.is_empty() { "unknown" } else { &report.route },
        report.ping_ms.map_or_else(|| "unknown".to_string(), |p| format!("{p} ms"))
    ));
    for (n, mode) in modes.iter().enumerate() {
        ctx.say(&format!("Test {} of {}: {}", n + 1, modes.len(), mode.name));
        let outcome = match (mode.run)(&mut ctx) {
            Ok(outcome) => outcome,
            Err(Stop) => {
                if let Some(partial) = ctx.partial.take() {
                    report.outcomes.push(partial);
                }
                return Err(Stop);
            }
        };
        if outcome.ok {
            for line in &outcome.lines {
                ctx.say(&format!("  {line}"));
            }
        } else {
            ctx.say(&format!("  This test failed: {}", outcome.lines.join(" ")));
        }
        // what the test logged (Steam's samples every second, its detailed status at the start, now and then and at the
        // end, the settings read back from the connection) goes to the results file
        let log = ctx.probe.take_log();
        if !log.is_empty() {
            report.dumps.push(format!(
                "--- {} ---
{}",
                mode.name,
                log.join(
                    "
"
                )
            ));
        }
        report.outcomes.push(outcome);
        save(report);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steamnet::fake::FakeMachine;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn mb1(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
        reliable(ctx, "Reliable stream, 1 MB each way", 1)
    }
    fn mb2(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
        reliable(ctx, "Reliable stream, 2 MB each way", 2)
    }
    const TEST_MODES: &[Mode] = &[Mode { name: "one", run: mb1 }, Mode { name: "two", run: mb2 }];

    /// A responder on `machine`, polled on a thread until `stop` is set.
    fn responder_thread(core: CoreRef, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut probe = Probe::on(core);
            while !stop.load(Ordering::Relaxed) {
                probe.poll();
                thread::sleep(Duration::from_millis(2));
            }
        })
    }

    fn run(
        core: &CoreRef,
        peer: u64,
        modes: &[Mode],
        timing: Timing,
        cancel: &AtomicBool,
    ) -> (TesterReport, Vec<String>, usize) {
        let mut said = Vec::new();
        let mut saves = 0;
        let mut say = |t: &str| said.push(t.to_string());
        let mut pump = || {};
        let cancelled = || cancel.load(Ordering::Relaxed);
        let mut env = Env { say: &mut say, pump: &mut pump, cancelled: &cancelled, timing };
        let report = run_tester(&mut env, core, peer, modes, &mut |_| saves += 1);
        (report, said, saves)
    }

    #[test]
    fn the_default_is_the_ladder_then_datagrams_and_classic_lists_every_mode() {
        let names: Vec<_> = MODES.iter().map(|m| m.name).collect();
        assert_eq!(names, ["rate ladder (comprehensive)", "datagrams 32 MB"]);
        let classic: Vec<_> = CLASSIC_MODES.iter().map(|m| m.name).collect();
        assert_eq!(
            classic,
            [
                "reliable 8 MB",
                "reliable 32 MB",
                "reliable 80 MB",
                "datagrams 8 MB",
                "datagrams 32 MB",
                "datagrams 80 MB",
                "parallel x2, 32 MB",
                "parallel x4, 32 MB"
            ]
        );
    }

    #[test]
    fn the_tester_runs_every_mode_against_a_responder() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let (core_a, core_b) = (a.core(), b.core());
        let stop = Arc::new(AtomicBool::new(false));
        let responder = responder_thread(core_b.handle(), Arc::clone(&stop));
        let cancel = AtomicBool::new(false);
        let (report, said, saves) = run(&core_a.handle(), b.steam_id(), TEST_MODES, Timing::quick(), &cancel);
        stop.store(true, Ordering::Relaxed);
        responder.join().unwrap();
        assert!(report.finished && report.connected && report.error.is_none(), "{report:?} {said:?}");
        assert_eq!(report.outcomes.len(), 2);
        assert!(report.outcomes.iter().all(|o| o.ok && o.lines.len() == 4), "{report:?}");
        assert!(report.outcomes[0].lines[0].starts_with("Speed test, 1 MB each way"), "{report:?}");
        assert_eq!(report.own_id, Some(a.steam_id()));
        assert_eq!(report.dumps.len(), 2, "one log block per mode: {:?}", report.dumps);
        assert!(
            report.dumps[0].starts_with(
                "--- one ---
"
            ),
            "{:?}",
            report.dumps
        );
        assert!(
            report.dumps[0].contains("steam speed test status (start): fake detailed status"),
            "{:?}",
            report.dumps
        );
        assert!(report.dumps[0].contains("settings: SendRateMin asked"), "{:?}", report.dumps);
        assert_eq!(report.route, "direct");
        assert_eq!(saves, 3, "saved after each mode and at the end");
        assert!(said.iter().any(|l| l.starts_with("Your Steam ID: ")), "{said:?}");
        assert!(said.iter().any(|l| l == "Test 2 of 2: two"), "{said:?}");
    }

    #[test]
    fn a_missing_other_side_times_out_with_a_clear_message() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let core_a = a.core();
        b.set_reachable(false);
        let cancel = AtomicBool::new(false);
        let (report, said, _) = run(&core_a.handle(), b.steam_id(), TEST_MODES, Timing::quick(), &cancel);
        let error = report.error.expect("an error");
        assert!(error.starts_with("The other PC did not answer (2 attempts of 0 seconds)"), "{error}");
        assert!(!report.connected && report.outcomes.is_empty());
        assert!(said.iter().any(|l| l.contains("Waiting for the other PC to start its program")), "{said:?}");
        assert!(said.iter().any(|l| l.contains("Trying again")), "{said:?}");
    }

    #[test]
    fn the_peer_may_start_late_within_the_waiting_time() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let (core_a, core_b) = (a.core(), b.core());
        b.set_reachable(false);
        let stop = Arc::new(AtomicBool::new(false));
        let late = {
            let (stop, core) = (Arc::clone(&stop), core_b.handle());
            let machine_b_id = b.steam_id();
            thread::spawn(move || {
                let _ = machine_b_id;
                thread::sleep(Duration::from_millis(150));
                responder_thread(core, stop).join().unwrap();
            })
        };
        let timing = Timing {
            retry_pause: Duration::from_millis(30),
            connect_window: Duration::from_secs(5),
            ..Timing::quick()
        };
        let reach = {
            let b_ref = &b;
            thread::scope(|s| {
                s.spawn(|| {
                    thread::sleep(Duration::from_millis(100));
                    b_ref.set_reachable(true);
                });
                let cancel = AtomicBool::new(false);
                run(&core_a.handle(), b_ref.steam_id(), &TEST_MODES[..1], timing, &cancel)
            })
        };
        stop.store(true, Ordering::Relaxed);
        late.join().unwrap();
        assert!(reach.0.finished, "{:?} {:?}", reach.0, reach.1);
    }

    #[test]
    fn ctrl_c_stops_cleanly_and_still_saves() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let core_a = a.core();
        b.set_reachable(false);
        let cancel = AtomicBool::new(true);
        let (report, said, saves) = run(&core_a.handle(), b.steam_id(), TEST_MODES, Timing::quick(), &cancel);
        assert!(report.aborted && !report.finished);
        assert!(said.iter().any(|l| l == "Stopped by Ctrl+C."), "{said:?}");
        assert_eq!(saves, 1);
    }

    #[test]
    fn testing_against_your_own_id_is_refused() {
        let a = FakeMachine::new();
        let core_a = a.core();
        let cancel = AtomicBool::new(false);
        let (report, _, _) = run(&core_a.handle(), a.steam_id(), TEST_MODES, Timing::quick(), &cancel);
        assert!(report.error.expect("an error").contains("is your own"));
    }

    #[test]
    fn steam_that_never_gets_ready_is_explained() {
        let a = FakeMachine::new();
        a.set_ready(false);
        let core = a.core_with(crate::steamnet::fake::fast());
        let timing = Timing { ready_wait: Duration::from_millis(200), ..Timing::quick() };
        let cancel = AtomicBool::new(false);
        let (report, _, _) = run(&core.handle(), 76561190000000001, TEST_MODES, timing, &cancel);
        let error = report.error.expect("an error");
        assert!(error.contains("its network is not ready") && error.contains("Offline Mode"), "{error}");
    }
}
