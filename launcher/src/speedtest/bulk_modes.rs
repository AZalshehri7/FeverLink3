//! Test modes of the standalone program that use the bulk datagram protocol and the parallel connections: the same runs
//! as the game window's "Datagrams" and "Parallel" buttons (`steamnet::speed`), one table entry each.

use std::time::Instant;

use super::suite::{Ctx, Outcome, Stop};
use crate::steamnet::{RunKind, RunSpec};

/// One run of `spec` against the peer; the outcome carries the result block of the run (or why it failed).
pub fn run_spec(ctx: &mut Ctx<'_, '_>, name: &str, spec: RunSpec) -> Result<Outcome, Stop> {
    let fail = |why: String| Outcome { name: name.to_string(), ok: false, lines: vec![why] };
    let before = ctx.probe.run_outcome().map(|o| o.id);
    ctx.probe.start_run(ctx.peer, spec);
    let started = Instant::now();
    let mut last_say = Instant::now();
    loop {
        ctx.tick()?;
        if !ctx.probe.run_active() {
            return Ok(match ctx.probe.run_outcome().filter(|o| Some(o.id) != before) {
                Some(o) if o.failure.is_none() && o.kind.is_some() => {
                    let mut lines = o.lines;
                    // a datagram or parallel result block says what it was; the plain one keeps its format
                    if matches!(o.kind, Some(RunKind::Datagram(_))) || spec.conns() > 1 {
                        lines.push(format!("(log line: {})", o.log_line));
                    }
                    Outcome { name: name.to_string(), ok: true, lines }
                }
                Some(o) => fail(o.failure.unwrap_or_else(|| "the run produced no result".into())),
                None => fail("the test could not be started (Steam is not ready or the connection is gone)".into()),
            });
        }
        if last_say.elapsed() >= ctx.env.timing.progress_every {
            last_say = Instant::now();
            if let Some(speed) = ctx.probe.report().speed {
                ctx.say(&format!("  {}", speed.status));
            }
        }
        if started.elapsed() > ctx.env.timing.mode_max {
            return Ok(fail(format!("gave up after {} minutes", ctx.env.timing.mode_max.as_secs() / 60)));
        }
    }
}

fn datagrams(ctx: &mut Ctx<'_, '_>, mb: u32) -> Result<Outcome, Stop> {
    run_spec(ctx, &format!("Datagrams (own reliability), {mb} MB each way"), RunSpec::datagrams(mb))
}

pub fn datagrams_8(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
    datagrams(ctx, 8)
}

pub fn datagrams_32(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
    datagrams(ctx, 32)
}

pub fn datagrams_80(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
    datagrams(ctx, 80)
}

fn parallel(ctx: &mut Ctx<'_, '_>, conns: u32, mb: u32) -> Result<Outcome, Stop> {
    let spec = RunSpec { variant: crate::steamnet::throughput::Variant::Reliable(conns), ..RunSpec::reliable(mb) };
    run_spec(ctx, &format!("Reliable stream on {conns} connections at once, {mb} MB each way"), spec)
}

pub fn parallel_2_32(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
    parallel(ctx, 2, 32)
}

pub fn parallel_4_32(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
    parallel(ctx, 4, 32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speedtest::suite::{run_tester, Env, Mode, Timing};
    use crate::steamnet::fake::FakeMachine;
    use crate::steamnet::{CoreRef, Probe};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    fn d1(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
        run_spec(ctx, "datagrams 1 MB", RunSpec::datagrams(1))
    }
    fn p2(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
        parallel(ctx, 2, 1)
    }
    const MODES: &[Mode] = &[Mode { name: "d", run: d1 }, Mode { name: "p", run: p2 }];

    fn responder(core: CoreRef, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut probe = Probe::on(core);
            while !stop.load(Ordering::Relaxed) {
                probe.poll();
                thread::sleep(Duration::from_millis(2));
            }
        })
    }

    #[test]
    fn the_datagram_and_parallel_modes_run_against_a_responder_and_fill_the_results() {
        let (a, b) = (FakeMachine::new(), FakeMachine::new());
        let (core_a, core_b) = (a.core(), b.core());
        let stop = Arc::new(AtomicBool::new(false));
        let handle = responder(core_b.handle(), Arc::clone(&stop));
        let mut said = Vec::new();
        let mut say = |t: &str| said.push(t.to_string());
        let mut pump = || {};
        let cancelled = || false;
        let mut env = Env { say: &mut say, pump: &mut pump, cancelled: &cancelled, timing: Timing::quick() };
        let report = run_tester(&mut env, &core_a.handle(), b.steam_id(), MODES, &mut |_| {});
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        assert!(report.finished && report.outcomes.iter().all(|o| o.ok), "{report:?}");
        assert!(report.outcomes[0].lines[0].starts_with("Datagram speed test, 1 MB each way"), "{report:?}");
        assert!(report.outcomes[1].lines[0].contains("over 2 connections"), "{report:?}");
        assert_eq!(report.dumps.len(), 2);
    }
}
