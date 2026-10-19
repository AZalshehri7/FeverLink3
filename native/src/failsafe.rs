//! Fail loud, not open (review of TPF3-MP, 2026-10-19, idea 8).
//!
//! A hook that cannot do its part must not quietly let this game run differently from the others. Until now a panic in a hook's
//! bookkeeping (contained by `log::guarded`), a busy hold state, a full hold ring, a capture the engine refused, or a determinism patch
//! that could not be written all ended in a pass-through: the command reached the engine at the click instead of at its stamp, or this
//! game ran with the shipped tickCount, Lua seed or script pool while the others did not. The log said so at best; the session went on
//! with two worlds.
//!
//! Now each of those places raises a [`Fault`]: a log line (`FAIL-CLOSED [place] (action): detail`), and a `fault` event for the mod
//! (`docs/NATIVE_LINK.md`, module to mod), which tells the player in the game's chat and asks the session controller for a repair.
//! [`Action::Repair`]: one command went wrong; a repair (resync) makes the worlds equal again. [`Action::Stop`]: a determinism piece is
//! missing for the rest of this game run, so every repair would fork again: the player is told to leave and restart the game (the repair
//! is still asked for). A stop fault is reported once per place.
//!
//! [`Barrier`] is the last line: every function the game jumps to holds one, and a Rust panic that would unwind out of it into the
//! engine's C++ frames (where a `catch (...)` could swallow it and carry on half-done) ends the process with a log line instead. A C++
//! exception of the game passing through is not a Rust panic and passes as before.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The worlds may differ from now on: the host repairs them.
    Repair,
    /// This game cannot stay identical for the rest of its run: leave and restart (and repair meanwhile).
    Stop,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Repair => "repair",
            Action::Stop => "stop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    pub place: &'static str,
    pub detail: String,
    pub action: Action,
}

impl Fault {
    pub fn line(&self) -> String {
        format!("FAIL-CLOSED [{}] ({}): {}", self.place, self.action.name(), self.detail)
    }
}

/// Faults not yet written to the mod. More than this wait only when the link is gone; the rest are counted.
pub const MAX_QUEUED: usize = 32;

#[derive(Default)]
struct Inner {
    queue: VecDeque<Fault>,
    dropped: u64,
    stopped: Vec<&'static str>,
}

/// A queue of faults (the runtime has one; [`GLOBAL`] takes those of code without a runtime: install time, the id trace).
pub struct Faults {
    inner: Mutex<Option<Inner>>,
}

impl Faults {
    pub const fn new() -> Faults {
        Faults { inner: Mutex::new(None) }
    }

    fn lock(&self) -> MutexGuard<'_, Option<Inner>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Records a fault. Returns it when it is new (a stop fault of a place already reported is not), for the caller's log line.
    pub fn trip(&self, place: &'static str, action: Action, detail: impl Into<String>) -> Option<Fault> {
        let mut g = self.lock();
        let inner = g.get_or_insert_with(Inner::default);
        if action == Action::Stop {
            if inner.stopped.contains(&place) {
                return None;
            }
            inner.stopped.push(place);
        }
        let fault = Fault { place, detail: detail.into(), action };
        if inner.queue.len() >= MAX_QUEUED {
            inner.dropped += 1;
        } else {
            inner.queue.push_back(fault.clone());
        }
        Some(fault)
    }

    /// The faults to write now; a count of dropped ones rides on the last.
    pub fn drain(&self) -> Vec<Fault> {
        let mut g = self.lock();
        let Some(inner) = g.as_mut() else { return Vec::new() };
        let mut out: Vec<Fault> = inner.queue.drain(..).collect();
        if inner.dropped > 0 {
            if let Some(last) = out.last_mut() {
                last.detail.push_str(&format!(" ({} more fault(s) were not queued)", inner.dropped));
                inner.dropped = 0;
            }
        }
        out
    }

    /// Whether a stop fault was raised.
    pub fn stopped(&self) -> bool {
        self.lock().as_ref().is_some_and(|i| !i.stopped.is_empty())
    }
}

impl Default for Faults {
    fn default() -> Self {
        Faults::new()
    }
}

/// Faults of code that has no runtime at hand; the runtime's service thread writes them with its own.
pub static GLOBAL: Faults = Faults::new();

/// [`Faults::trip`] on [`GLOBAL`], with the log line.
pub fn global(place: &'static str, action: Action, detail: impl Into<String>) {
    if let Some(f) = GLOBAL.trip(place, action, detail) {
        #[cfg(windows)]
        crate::log::line(&f.line());
        #[cfg(not(windows))]
        let _ = f;
    }
}

/// The determinism patches the switches asked for and that are not in place: each is a stop fault (this game runs as shipped where
/// the others do not). `(name, asked, applied)`.
pub fn missing_patches(pieces: &[(&'static str, bool, bool)]) -> Vec<&'static str> {
    pieces.iter().filter(|(_, asked, applied)| *asked && !*applied).map(|(n, _, _)| *n).collect()
}

/// Held by every function the game's code jumps to. Dropped while a Rust panic unwinds (never while a C++ exception of the game
/// passes: that is not a Rust panic), it calls `escape`, which in the game logs and aborts: a Rust panic must not unwind into the
/// engine. It also counts the hook as entered and left ([`crate::hookwatch`]), so a game thread stuck inside one is named.
pub struct Barrier {
    site: &'static str,
    escape: fn(&'static str),
    _inside: Option<crate::hookwatch::Inside>,
}

impl Barrier {
    pub fn new(watch: &'static crate::hookwatch::Watch) -> Barrier {
        Barrier { site: watch.name, escape: abort_escape, _inside: Some(watch.enter()) }
    }

    /// With another escape and no watch (tests).
    pub fn with(site: &'static str, escape: fn(&'static str)) -> Barrier {
        Barrier { site, escape, _inside: None }
    }
}

impl Drop for Barrier {
    fn drop(&mut self) {
        if std::thread::panicking() {
            (self.escape)(self.site);
        }
    }
}

fn abort_escape(site: &'static str) {
    #[cfg(windows)]
    crate::log::line(&format!(
        "FATAL [{site}]: a panic of the native module reached the edge of a hook (the PANIC line above says where); the game is \
         stopped here rather than letting the panic unwind into the engine"
    ));
    #[cfg(not(windows))]
    let _ = site;
    std::process::abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn a_fault_is_queued_once_and_a_stop_fault_once_per_place() {
        let f = Faults::new();
        assert!(f.trip("add hook", Action::Repair, "one").is_some());
        assert!(f.trip("add hook", Action::Repair, "two").is_some(), "every one-off counts");
        assert!(f.trip("step seed", Action::Stop, "not applied").is_some());
        assert!(f.trip("step seed", Action::Stop, "again").is_none(), "a stop is said once");
        assert!(f.stopped());
        let d = f.drain();
        assert_eq!(d.iter().map(|x| x.place).collect::<Vec<_>>(), ["add hook", "add hook", "step seed"]);
        assert_eq!(d[2].line(), "FAIL-CLOSED [step seed] (stop): not applied");
        assert!(f.drain().is_empty());
    }

    #[test]
    fn a_full_queue_counts_what_it_drops_on_the_last_fault() {
        let f = Faults::new();
        for _ in 0..MAX_QUEUED + 3 {
            f.trip("x", Action::Repair, "d");
        }
        let d = f.drain();
        assert_eq!(d.len(), MAX_QUEUED);
        assert!(d.last().unwrap().detail.ends_with("(3 more fault(s) were not queued)"));
    }

    #[test]
    fn only_the_patches_asked_for_and_missing_are_faults() {
        assert_eq!(
            missing_patches(&[("poolpatch", true, true), ("lua seed", true, false), ("step seed", false, false)]),
            ["lua seed"]
        );
    }

    thread_local! {
        static ESCAPED: Cell<u32> = const { Cell::new(0) };
    }

    fn note(_: &'static str) {
        ESCAPED.with(|e| e.set(e.get() + 1));
    }

    #[test]
    fn the_barrier_acts_only_on_a_rust_panic_unwinding_through_it() {
        ESCAPED.with(|e| e.set(0));
        {
            let _b = Barrier::with("quiet", note);
        }
        assert_eq!(ESCAPED.with(|e| e.get()), 0, "a normal return");
        let r = std::panic::catch_unwind(|| {
            let _b = Barrier::with("hook", note);
            panic!("a bug in a hook");
        });
        assert!(r.is_err());
        assert_eq!(ESCAPED.with(|e| e.get()), 1, "the panic reached the edge of the hook");
    }
}
