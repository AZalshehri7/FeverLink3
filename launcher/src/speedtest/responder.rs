//! The responder: answers every tester that connects, one after another, and logs what it did.

use crate::steamnet::{CoreRef, Probe};

use super::suite::{wait_ready, Env, Stop};

/// The lines in `now` that were not in `before`: `before` and `now` are two views of a ring of recent events (oldest
/// first), so the new ones follow the longest stretch where the end of `before` equals the start of `now`.
pub fn new_events<'a>(before: &[String], now: &'a [String]) -> &'a [String] {
    let max = before.len().min(now.len());
    for k in (0..=max).rev() {
        if before[before.len() - k..] == now[..k] {
            return &now[k..];
        }
    }
    now
}

/// Runs until Ctrl+C. `log` gets every line worth keeping (the program prints it and appends it to the log file).
/// `Ok(Some(text))` when it could not start.
pub fn run_responder(env: &mut Env<'_>, core: &CoreRef, log: &mut dyn FnMut(&str)) -> Result<Option<String>, Stop> {
    let mut probe = Probe::on(core.clone());
    env.say("Starting Steam networking...");
    let own = match wait_ready(env, &mut probe, core)? {
        Ok(id) => id,
        Err(why) => return Ok(Some(why)),
    };
    if let Some(id) = own {
        env.say(&format!("Your Steam ID: {id}"));
        env.say("Give this number to the person who runs the test.");
    }
    log(&format!("responder started, Steam ID {}", own.map_or_else(|| "unknown".to_string(), |i| i.to_string())));
    env.say("Waiting for a tester to connect. Leave this window open; press Ctrl+C to stop.");
    let mut seen_events: Vec<String> = Vec::new();
    let mut seen_peers: Vec<Option<u64>> = Vec::new();
    loop {
        if (env.cancelled)() {
            log("responder stopped");
            return Ok(None);
        }
        (env.pump)();
        probe.poll();
        let report = probe.report();
        for event in new_events(&seen_events, &report.events) {
            // the ready/listening notes are for the window of the game; keep what concerns testers
            log(event);
        }
        seen_events = report.events.clone();
        for line in probe.take_log() {
            log(&line);
        }
        let peers: Vec<(Option<u64>, String, Option<u32>)> =
            report.peers.iter().map(|p| (p.steam_id, p.route.clone(), p.ping_ms)).collect();
        let ids: Vec<Option<u64>> = peers.iter().map(|p| p.0).collect();
        for (id, route, ping) in &peers {
            if !seen_peers.contains(id) {
                log(&format!(
                    "tester {} connected, route {}, round trip {}",
                    id.map_or_else(|| "unknown".to_string(), |i| i.to_string()),
                    if route.is_empty() { "unknown" } else { route },
                    ping.map_or_else(|| "unknown".to_string(), |p| format!("{p} ms"))
                ));
            }
        }
        for id in &seen_peers {
            if !ids.contains(id) {
                log(&format!("tester {} disconnected", id.map_or_else(|| "unknown".to_string(), |i| i.to_string())));
            }
        }
        seen_peers = ids;
        std::thread::sleep(env.timing.poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speedtest::suite::{reliable, run_tester, Ctx, Mode, Outcome, Timing};
    use crate::steamnet::fake::FakeMachine;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    fn s(list: &[&str]) -> Vec<String> {
        list.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn new_events_follow_the_overlap() {
        assert_eq!(new_events(&s(&[]), &s(&["a"])), &s(&["a"])[..]);
        assert_eq!(new_events(&s(&["a", "b"]), &s(&["a", "b"])), &[] as &[String]);
        assert_eq!(new_events(&s(&["a", "b"]), &s(&["b", "c", "d"])), &s(&["c", "d"])[..]);
        assert_eq!(new_events(&s(&["a", "b"]), &s(&["x", "y"])), &s(&["x", "y"])[..]);
    }

    fn one_mb(ctx: &mut Ctx<'_, '_>) -> Result<Outcome, Stop> {
        reliable(ctx, "one", 1)
    }
    const ONE: &[Mode] = &[Mode { name: "one", run: one_mb }];

    #[test]
    fn the_responder_answers_several_testers_one_after_another_and_logs_them() {
        let (resp, t1, t2) = (FakeMachine::new(), FakeMachine::new(), FakeMachine::new());
        let (core_r, core_1, core_2) = (resp.core(), t1.core(), t2.core());
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let (lines, stop, core) = (Arc::clone(&lines), Arc::clone(&stop), core_r.handle());
            thread::spawn(move || {
                let mut say = |_: &str| {};
                let mut pump = || {};
                let cancelled = || stop.load(Ordering::Relaxed);
                let mut env = Env { say: &mut say, pump: &mut pump, cancelled: &cancelled, timing: Timing::quick() };
                let mut log = |l: &str| lines.lock().unwrap().push(l.to_string());
                run_responder(&mut env, &core, &mut log)
            })
        };
        for (core, machine) in [(&core_1, &t1), (&core_2, &t2)] {
            let mut say = |_: &str| {};
            let mut pump = || {};
            let cancelled = || false;
            let mut env = Env { say: &mut say, pump: &mut pump, cancelled: &cancelled, timing: Timing::quick() };
            let _ = machine;
            let report = run_tester(&mut env, &core.handle(), resp.steam_id(), ONE, &mut |_| {});
            assert!(report.finished, "{report:?}");
        }
        thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
        assert_eq!(handle.join().unwrap().unwrap(), None);
        let lines = lines.lock().unwrap().clone();
        assert!(lines[0].starts_with("responder started, Steam ID "), "{lines:?}");
        for t in [&t1, &t2] {
            assert!(lines.iter().any(|l| l.starts_with(&format!("tester {} connected", t.steam_id()))), "{lines:?}");
        }
        assert_eq!(lines.iter().filter(|l| l.starts_with("a friend is testing the speed")).count(), 2, "{lines:?}");
        assert!(lines.last().unwrap() == "responder stopped", "{lines:?}");
    }
}
