//! Where a game thread is inside the module right now: an entered/left counter per hook and per blocking spot of the module's
//! own (a file write, a lock), dumped by the service thread while the game has not started its simulation.
//!
//! Why (labs L and L2 of 2026-10-19): with `FEVERLINK3_LOG_GROWTH_DEEP=1` one of the two games hung while it loaded the world, in
//! both runs. Its native module had started (hooks in place, the link open, `timing clock 0`), but the simulation never applied a
//! command and the mod wrote nothing; the process stayed alive until it was killed. Code reading found no lock cycle among the
//! module's own locks (none is held across a call into the game) and no fault in the call-site thunk of `town_field` (it keeps every
//! register and the stack as a plain `call` does), so the cause is not proven. These counters make the next run name the place: a
//! hook (or a write) whose `in` stays above 0 across dumps while its `entered` stops moving is where a game thread is stuck; no hook
//! inside means the game is stuck in its own code (or in a hook's call of the original: then the hook shows `in` too).
//!
//! Costs two relaxed atomic adds per hook call. Nothing here waits, allocates or takes a lock on the hook side.
//!
//! The dump (`native.log`, every [`DUMP_EVERY_MS`] until the first command is applied, at most [`MAX_DUMPS`] lines, written without
//! the log's lock so a writer stuck under it cannot silence it):
//!
//! ```text
//! hookwatch #<n> pid=<process> t=<ms since start> sim=not started: INSIDE town_field 1 (entered 3) | apply_hook 0/0, add_hook 0/0, ...
//! hookwatch: pid=<process> the simulation applied its first command at t=<ms>: dumps stop (<n> written)
//! ```

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

/// How often the service thread writes the dump while the simulation has not started.
pub const DUMP_EVERY_MS: u64 = 5_000;
/// Most dump lines per process (an hour at one per 5 s): `native.log` is capped and other lines matter more.
pub const MAX_DUMPS: u64 = 720;

/// One place a game thread can be inside the module.
#[derive(Debug)]
pub struct Watch {
    pub name: &'static str,
    entered: AtomicU64,
    left: AtomicU64,
}

impl Watch {
    pub const fn new(name: &'static str) -> Watch {
        Watch { name, entered: AtomicU64::new(0), left: AtomicU64::new(0) }
    }

    /// Counts the entry; the guard counts the exit when it is dropped (also while a panic or a C++ exception unwinds).
    pub fn enter(&'static self) -> Inside {
        self.entered.fetch_add(1, Relaxed);
        Inside(self)
    }

    /// `(entered, left)` as read now (`left` first, so `entered - left` never goes below 0 from a race).
    pub fn read(&self) -> (u64, u64) {
        let left = self.left.load(Relaxed);
        (self.entered.load(Relaxed).max(left), left)
    }
}

/// Inside a [`Watch`] until dropped.
#[must_use]
pub struct Inside(&'static Watch);

impl Drop for Inside {
    fn drop(&mut self) {
        self.0.left.fetch_add(1, Relaxed);
    }
}

// the functions the game jumps to (hooks.rs, ids.rs)
pub static ADD_HOOK: Watch = Watch::new("add_hook");
pub static APPLY_HOOK: Watch = Watch::new("apply_hook");
pub static CONVERT_HOOK: Watch = Watch::new("convert_hook");
pub static CONVERT_UPDATE_HOOK: Watch = Watch::new("convert_update_hook");
pub static PREVIEW_HOOK: Watch = Watch::new("preview_hook");
pub static PREVIEW_OWNER_HOOK: Watch = Watch::new("preview_owner_hook");
pub static TOWN_SEED_HOOK: Watch = Watch::new("town_seed_hook");
pub static CANDIDATES_HOOK: Watch = Watch::new("candidates_hook");
pub static EDGES_HOOK: Watch = Watch::new("edges_hook");
pub static SORT_HOOK: Watch = Watch::new("sort_hook");
pub static PICK_HOOK: Watch = Watch::new("pick_hook");
pub static FIELD_HOOK: Watch = Watch::new("field_hook");
pub static PACE_HOOK: Watch = Watch::new("pace_hook");
pub static FREE_ORDER_HOOK: Watch = Watch::new("free_order_hook");
pub static GROWTH_CMD_HOOK: Watch = Watch::new("growth_cmd_hook");
pub static GROWTH_HOOK: Watch = Watch::new("growth_hook");
pub static IDS_ADD_HOOK: Watch = Watch::new("ids add_hook");
pub static IDS_REMOVE_HOOK: Watch = Watch::new("ids remove_hook");
pub static IDS_STEP_HOOK: Watch = Watch::new("ids step_hook");
// the module's own spots where a game thread can wait: file writes and their locks
pub static GROWTH_FILE: Watch = Watch::new("growth.log write");
pub static IDORDER_LOCK: Watch = Watch::new("idorder state lock");
pub static IDORDER_FILE: Watch = Watch::new("idorder write");
pub static NATIVE_LOG: Watch = Watch::new("native.log write");

/// Every watch, in dump order.
pub static ALL: [&Watch; 23] = [
    &APPLY_HOOK,
    &ADD_HOOK,
    &CONVERT_HOOK,
    &CONVERT_UPDATE_HOOK,
    &PREVIEW_HOOK,
    &PREVIEW_OWNER_HOOK,
    &PACE_HOOK,
    &FREE_ORDER_HOOK,
    &IDS_ADD_HOOK,
    &IDS_REMOVE_HOOK,
    &IDS_STEP_HOOK,
    &TOWN_SEED_HOOK,
    &GROWTH_CMD_HOOK,
    &GROWTH_HOOK,
    &CANDIDATES_HOOK,
    &EDGES_HOOK,
    &SORT_HOOK,
    &PICK_HOOK,
    &FIELD_HOOK,
    &GROWTH_FILE,
    &IDORDER_LOCK,
    &IDORDER_FILE,
    &NATIVE_LOG,
];

/// The dump line of `watches`: the ones a thread is inside now first (`INSIDE name in (entered n)`), then the others as
/// `name entered/left`, or `none inside` when no thread is in any.
pub fn line(n: u64, now_ms: u64, watches: &[&Watch]) -> String {
    let counts: Vec<(&str, u64, u64)> = watches
        .iter()
        .map(|w| {
            let (e, l) = w.read();
            (w.name, e, l)
        })
        .collect();
    let inside: Vec<String> =
        counts.iter().filter(|(_, e, l)| e > l).map(|(name, e, l)| format!("{name} {} (entered {e})", e - l)).collect();
    let rest: Vec<String> =
        counts.iter().filter(|(_, e, l)| e <= l).map(|(name, e, l)| format!("{name} {e}/{l}")).collect();
    let head = if inside.is_empty() { "none inside".to_string() } else { format!("INSIDE {}", inside.join(", ")) };
    format!("hookwatch #{n} pid={} t={now_ms} sim=not started: {head} | {}", std::process::id(), rest.join(", "))
}

/// When the service thread dumps: every [`DUMP_EVERY_MS`] until the simulation applied a command, at most [`MAX_DUMPS`] times,
/// and the closing line once.
#[derive(Debug, Default)]
pub struct Schedule {
    last_ms: AtomicU64,
    written: AtomicU64,
    closed: AtomicBool,
}

impl Schedule {
    pub const fn new() -> Schedule {
        Schedule { last_ms: AtomicU64::new(0), written: AtomicU64::new(0), closed: AtomicBool::new(false) }
    }

    /// The line to write at `now_ms`, if any. `sim_started`: the simulation applied a command.
    pub fn due(&self, now_ms: u64, sim_started: bool, watches: &[&Watch]) -> Option<String> {
        if self.closed.load(Relaxed) {
            return None;
        }
        let n = self.written.load(Relaxed);
        if sim_started {
            self.closed.store(true, Relaxed);
            return Some(format!(
                "hookwatch: pid={} the simulation applied its first command by t={now_ms}: dumps stop ({n} written)",
                std::process::id()
            ));
        }
        if n >= MAX_DUMPS || now_ms.saturating_sub(self.last_ms.load(Relaxed)) < DUMP_EVERY_MS {
            return None;
        }
        self.last_ms.store(now_ms, Relaxed);
        self.written.store(n + 1, Relaxed);
        Some(line(n + 1, now_ms, watches))
    }
}

/// The service thread's schedule.
pub static SCHEDULE: Schedule = Schedule::new();

/// True once the simulation applied a command ([`APPLY_HOOK`] returned at least once).
pub fn sim_started() -> bool {
    APPLY_HOOK.read().1 > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    static A: Watch = Watch::new("a_hook");
    static B: Watch = Watch::new("b write");

    #[test]
    fn a_thread_inside_is_named_first_and_leaves_on_drop_and_unwind() {
        let w: [&Watch; 2] = [&A, &B];
        assert!(line(1, 0, &w).ends_with("none inside | a_hook 0/0, b write 0/0"), "{}", line(1, 0, &w));
        let g = B.enter();
        {
            let _x = A.enter();
        }
        assert_eq!(
            line(2, 10, &w),
            format!(
                "hookwatch #2 pid={} t=10 sim=not started: INSIDE b write 1 (entered 1) | a_hook 1/1",
                std::process::id()
            )
        );
        drop(g);
        // a panic (or an exception) unwinding through the hook counts it as left
        let _ = std::panic::catch_unwind(|| {
            let _x = A.enter();
            panic!("unwinds");
        });
        assert_eq!(A.read(), (2, 2));
        assert!(line(3, 20, &w).contains("none inside | a_hook 2/2, b write 1/1"));
    }

    #[test]
    fn the_schedule_dumps_every_5_s_until_the_simulation_starts_then_says_so_once() {
        static C: Watch = Watch::new("c");
        let s = Schedule::new();
        let w: [&Watch; 1] = [&C];
        assert!(s.due(1_000, false, &w).is_none(), "not yet 5 s");
        assert!(s
            .due(5_000, false, &w)
            .unwrap()
            .starts_with(&format!("hookwatch #1 pid={} t=5000 ", std::process::id())));
        assert!(s.due(9_999, false, &w).is_none());
        assert!(s.due(10_000, false, &w).unwrap().starts_with("hookwatch #2 "));
        assert_eq!(
            s.due(12_000, true, &w).unwrap(),
            format!(
                "hookwatch: pid={} the simulation applied its first command by t=12000: dumps stop (2 written)",
                std::process::id()
            )
        );
        assert!(s.due(20_000, true, &w).is_none() && s.due(30_000, false, &w).is_none(), "closed for good");
    }

    #[test]
    fn the_dumps_are_bounded() {
        static D: Watch = Watch::new("d");
        let s = Schedule::new();
        let w: [&Watch; 1] = [&D];
        let n = (1..=MAX_DUMPS + 10).filter_map(|i| s.due(i * DUMP_EVERY_MS, false, &w)).count() as u64;
        assert_eq!(n, MAX_DUMPS);
    }

    #[test]
    fn every_watch_is_listed_once() {
        let mut names: Vec<&str> = ALL.iter().map(|w| w.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL.len());
    }
}
