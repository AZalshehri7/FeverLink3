//! Smooth pacing: the engine's batch interval stretched by a factor the mod sends (bug 4, the input barrier's stand-stills).
//!
//! Over the internet each game used to run at full speed into its stop (the input barrier, or the pacing limit past the slowest
//! peer) and then stand there until a peer's next clock moved it on: in the friend tests of 2026-10-16/17 a game stood still a
//! median 14 s a minute, single holds up to 16.6 s. The mod now measures, every frame, how much room is left before that stop and how
//! long the stop takes to move (`L.paceTarget` in `feverlink3_lockstep.lua`) and sends a factor from 1 to 2 (`pace {permille}`, 1000
//! to 2000). This module applies it to the engine's own batch interval, so the game runs a little slower as it nears the stop
//! instead of running into it and standing still.
//!
//! Where the interval lives (static analysis of build 40420, `TransportFever3.exe` read from disk, 2026-10-18): the main thread's
//! `CGame::Step` (RVA `0x11f3f0`) adds the frame's time to an accumulator and calls `CGame::Sync` (`0x11f690`, the call at
//! `0x11f446`) whenever the accumulator has passed the time of the last batch plus the interval:
//!
//! ```text
//! mov rax,[rcx+1F0h]  ; m_data
//! add [rax+1A0h],rdx  ; accumulated time (us, i64) += this frame's
//! ... movsxd rdi,[rdx+218h] ; add rdi,[rdx+1A8h]  ; the next batch time = last batch time (i64) + interval (i32 us)
//! call CGame::Sync    ; hands the simulation its next batch; al = 1 when it did
//! test al,al ; je ... ; mov [rax+1A8h],rdi        ; only then the last batch time moves on
//! ```
//!
//! `Sync` writes the interval itself (`mov [rcx+218h],eax` at `0x11f9d6`) from the engine's own estimate, 200 to 400 ms. So the
//! one place where a changed interval is consistent with everything around it is right after a `Sync` that returned true: the
//! module redirects that one call (a [`crate::build_table::CallPatchSite`], like the growth log's) and, after the real `Sync`,
//! writes `interval * factor` (`hooks.rs`, `pace_hook`). The engine still hands the simulation the same steps per batch; only the
//! wall time between batches grows, so every game runs the same steps at the same game times and nothing about the worlds changes
//! (the full-game rule). The idea, and the place (`guiFrameTime` at `m_data + 0x218`, the call of `Sync` in `Step`), come from
//! TPF3-MP (Juliansgith, MIT, its "batch interval" in `docs/HOOKS.md`) by way of MPFever 0.3.15 (MIT); the code here is our own.
//!
//! What is ours on top of it: the factor is never compounded (when `Sync` left the interval as the module wrote it, the base is the
//! one the module scaled, see [`scale`]), it is bounded to 1..2 on both sides of the link, it goes back to 1 by itself when the mod
//! stops refreshing it ([`STALE_MS`]), the layout is checked before every write, and a surprise switches the piece off for the rest
//! of the process ([`Pacer::disable`]).
//!
//! Everything here is pure or atomics: the hook (Windows only) reads and writes the game's memory and asks [`Pacer::decide`] what to
//! write; the service thread hands over the mod's factor ([`Pacer::set`]) and writes the minute's line ([`Pacer::minute_line`]).

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::Instant;

/// The hook's name in the plan, in `native.log` and in the `ready` event's hooks (the mod sends `pace` lines only when it sees it).
pub const HOOK_NAME: &str = "batch_pace";
/// `FEVERLINK3_SMOOTH_PACE=0` leaves the call alone (on by default).
pub const SMOOTH_PACE_VAR: &str = "FEVERLINK3_SMOOTH_PACE";
/// The factor 1, in thousandths.
pub const ONE: u32 = 1000;
/// The largest factor: half speed.
pub const MAX_PERMILLE: u32 = 2000;
/// A factor the mod has not refreshed for this long goes back to 1 (a mod that stopped, a game that left the session). The mod
/// refreshes it every second while it is not 1.
pub const STALE_MS: u64 = 3000;
/// The plausible range of the engine's interval (us). A value outside is not the field the module expects: left alone.
pub const MIN_INTERVAL_US: i32 = 20_000;
pub const MAX_INTERVAL_US: i32 = 2_000_000;
/// `CGame + 0x1f0`: the pointer to the engine's timing data.
pub const DATA_OFFSET: usize = 0x1f0;
/// `m_data + 0x1a0`: the accumulated time (us, i64); `+ 0x1a8`: the time of the last batch (us, i64).
pub const ACC_OFFSET: usize = 0x1a0;
pub const LAST_OFFSET: usize = 0x1a8;
/// `m_data + 0x218`: the batch interval (us, i32), `guiFrameTime`.
pub const INTERVAL_OFFSET: usize = 0x218;

/// Milliseconds on one monotonic clock for the hook and the service thread (since the first call).
pub fn clock_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// A factor the mod sent, held to 1..2. `None` when it is not a factor at all (0, absurd): the line is refused.
pub fn accept_permille(p: u64) -> Option<u32> {
    (u64::from(ONE / 2)..=u64::from(MAX_PERMILLE) * 4).contains(&p).then(|| (p as u32).clamp(ONE, MAX_PERMILLE))
}

/// The factor in force at `now`: the mod's, unless it is older than [`STALE_MS`].
pub fn effective(permille: u32, set_at: u64, now: u64) -> u32 {
    if permille <= ONE || now.saturating_sub(set_at) > STALE_MS {
        ONE
    } else {
        permille.min(MAX_PERMILLE)
    }
}

/// What to do with the interval the engine left after a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scale {
    /// The engine's own interval this batch (us): what the factor multiplies.
    pub base: i32,
    /// The value to write, `None` when the field already holds it.
    pub write: Option<i32>,
}

/// The interval to write after a batch. `engine`: what the field holds now; `written`: what the module wrote after the batch before
/// (0: nothing); `base`: the engine's interval the module scaled then. `Sync` sets the field from its own estimate, but when it left it
/// as written the base is the earlier one: the factor is applied to the engine's value, never to an already stretched one. `None`: the
/// field holds no plausible interval (not the layout expected) and is left alone.
pub fn scale(engine: i32, written: i32, base: i32, permille: u32) -> Option<Scale> {
    if !(MIN_INTERVAL_US..=MAX_INTERVAL_US).contains(&engine) {
        return None;
    }
    let base = if written != 0 && engine == written && (MIN_INTERVAL_US..=MAX_INTERVAL_US).contains(&base) {
        base
    } else {
        engine
    };
    let permille = permille.clamp(ONE, MAX_PERMILLE);
    let want = (i64::from(base) * i64::from(permille) / i64::from(ONE))
        .clamp(i64::from(base), 2 * i64::from(base))
        .clamp(i64::from(MIN_INTERVAL_US), i64::from(MAX_INTERVAL_US)) as i32;
    Some(Scale { base, write: (want != engine).then_some(want) })
}

/// The process-wide state of the piece: atomics only (the hook runs on the game's main thread, the service thread sets the factor).
#[derive(Debug)]
pub struct Pacer {
    permille: AtomicU32,
    set_at: AtomicU64,
    lines: AtomicU64,
    /// What the hook wrote last (0: nothing, or the field was given back to the engine) and the engine's interval it scaled.
    written: AtomicI32,
    base: AtomicI32,
    off: AtomicBool,
    off_why: OnceLock<String>,
    /// The minute's numbers: batches seen, batches stretched, the sum and the largest factor of the batches seen.
    batches: AtomicU64,
    stretched: AtomicU64,
    sum: AtomicU64,
    max: AtomicU32,
    minute_at: AtomicU64,
    minute_lines: AtomicU64,
    skipped: AtomicU64,
    first_logged: AtomicBool,
}

impl Pacer {
    pub const fn new() -> Pacer {
        Pacer {
            permille: AtomicU32::new(ONE),
            set_at: AtomicU64::new(0),
            lines: AtomicU64::new(0),
            written: AtomicI32::new(0),
            base: AtomicI32::new(0),
            off: AtomicBool::new(false),
            off_why: OnceLock::new(),
            batches: AtomicU64::new(0),
            stretched: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            max: AtomicU32::new(ONE),
            minute_at: AtomicU64::new(0),
            minute_lines: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            first_logged: AtomicBool::new(false),
        }
    }

    /// The mod's factor (already accepted), received at `now`.
    pub fn set(&self, permille: u32, now: u64) {
        self.permille.store(permille.clamp(ONE, MAX_PERMILLE), Relaxed);
        self.set_at.store(now, Relaxed);
        self.lines.fetch_add(1, Relaxed);
        self.minute_lines.fetch_add(1, Relaxed);
    }

    /// The factor in force at `now`.
    pub fn factor(&self, now: u64) -> u32 {
        effective(self.permille.load(Relaxed), self.set_at.load(Relaxed), now)
    }

    pub fn is_off(&self) -> bool {
        self.off.load(Relaxed)
    }

    /// The layout was not as expected: nothing is written again in this process.
    pub fn disable(&self, why: &str) {
        let _ = self.off_why.set(why.to_string());
        self.off.store(true, Relaxed);
    }

    /// Nothing to do after this batch (the factor is 1 and nothing is stretched): counted, and the hook reads no memory at all.
    pub fn idle(&self, now: u64) -> bool {
        let idle = self.is_off() || (self.factor(now) == ONE && self.written.load(Relaxed) == 0);
        if idle && !self.is_off() {
            self.count(ONE, false);
        }
        idle
    }

    /// The layout check failed this once (not the field expected): left alone, counted for the minute's line.
    pub fn implausible(&self) {
        self.skipped.fetch_add(1, Relaxed);
    }

    /// After a batch: the engine's interval `engine` (us) -> what to write, `None` to leave it. Counts the batch for the minute's line.
    pub fn decide(&self, engine: i32, now: u64) -> Option<i32> {
        if self.is_off() {
            return None;
        }
        let permille = self.factor(now);
        let written = self.written.load(Relaxed);
        // nothing stretched now or before: the engine's interval is left as it is, at no cost
        if permille == ONE && written == 0 {
            self.count(ONE, false);
            return None;
        }
        let Some(s) = scale(engine, written, self.base.load(Relaxed), permille) else {
            self.skipped.fetch_add(1, Relaxed);
            return None;
        };
        self.count(permille, permille > ONE);
        if permille == ONE {
            // back to the engine's own interval: written once, then the field is the engine's again
            self.written.store(0, Relaxed);
            self.base.store(0, Relaxed);
        } else {
            self.written.store(s.write.unwrap_or(engine), Relaxed);
            self.base.store(s.base, Relaxed);
        }
        s.write
    }

    fn count(&self, permille: u32, stretched: bool) {
        self.batches.fetch_add(1, Relaxed);
        self.sum.fetch_add(u64::from(permille), Relaxed);
        self.max.fetch_max(permille, Relaxed);
        if stretched {
            self.stretched.fetch_add(1, Relaxed);
        }
    }

    /// The first stretched batch, once (for `native.log`): the engine's interval and what was written.
    pub fn first_line(&self) -> Option<String> {
        if self.stretched.load(Relaxed) == 0 || self.first_logged.swap(true, Relaxed) {
            return None;
        }
        Some(format!(
            "{HOOK_NAME}: first batch stretched: engine interval {} us, written {} us",
            self.base.load(Relaxed),
            self.written.load(Relaxed)
        ))
    }

    /// Once a minute (`now` in [`clock_ms`]), when anything happened: the minute's batches and factors; the counts start over.
    pub fn minute_line(&self, now: u64) -> Option<String> {
        let at = self.minute_at.load(Relaxed);
        if at == 0 {
            self.minute_at.store(now.max(1), Relaxed);
            return None;
        }
        if now.saturating_sub(at) < 60_000 {
            return None;
        }
        self.minute_at.store(now, Relaxed);
        let batches = self.batches.swap(0, Relaxed);
        let stretched = self.stretched.swap(0, Relaxed);
        let sum = self.sum.swap(0, Relaxed);
        let max = self.max.swap(ONE, Relaxed);
        let lines = self.minute_lines.swap(0, Relaxed);
        let skipped = self.skipped.swap(0, Relaxed);
        if batches == 0 && lines == 0 && !self.is_off() {
            return None;
        }
        let mean = if batches > 0 { sum as f64 / batches as f64 / f64::from(ONE) } else { 1.0 };
        Some(format!(
            "{HOOK_NAME}: last {:.0} s: {batches} batches, {stretched} stretched, factor mean {mean:.2} max {:.2}, now {:.2}, {lines} pace line(s) from the mod, engine interval {} us{}{}",
            now.saturating_sub(at) as f64 / 1000.0,
            f64::from(max) / f64::from(ONE),
            f64::from(self.factor(now)) / f64::from(ONE),
            self.base.load(Relaxed),
            if skipped > 0 { format!(", {skipped} implausible interval(s) left alone") } else { String::new() },
            match self.off_why.get() {
                Some(why) => format!(", OFF: {why}"),
                None => String::new(),
            }
        ))
    }
}

impl Default for Pacer {
    fn default() -> Pacer {
        Pacer::new()
    }
}

/// The one pacer of the process.
pub static PACER: Pacer = Pacer::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_factor_is_held_to_one_to_two_and_absurd_ones_are_refused() {
        assert_eq!(accept_permille(1000), Some(1000));
        assert_eq!(accept_permille(1500), Some(1500));
        assert_eq!(accept_permille(2600), Some(2000), "more than half speed is never asked for: capped");
        assert_eq!(accept_permille(800), Some(1000), "never faster than the session's speed");
        assert_eq!(accept_permille(0), None);
        assert_eq!(accept_permille(100_000), None);
    }

    #[test]
    fn a_factor_not_refreshed_goes_back_to_one() {
        assert_eq!(effective(1500, 10_000, 10_000), 1500);
        assert_eq!(effective(1500, 10_000, 10_000 + STALE_MS), 1500);
        assert_eq!(effective(1500, 10_000, 10_001 + STALE_MS), ONE, "a mod that stopped refreshing: back to 1");
        assert_eq!(effective(900, 10_000, 10_000), ONE);
        assert_eq!(effective(5000, 10_000, 10_000), MAX_PERMILLE);
    }

    #[test]
    fn the_interval_is_scaled_from_the_engine_value_and_never_compounded() {
        // the engine's 250 ms, factor 1.5: 375 ms
        assert_eq!(scale(250_000, 0, 0, 1500), Some(Scale { base: 250_000, write: Some(375_000) }));
        // Sync left the field as written: the base stays 250 ms (not 375 * 1.5)
        assert_eq!(scale(375_000, 375_000, 250_000, 1500), Some(Scale { base: 250_000, write: None }));
        assert_eq!(scale(375_000, 375_000, 250_000, 2000), Some(Scale { base: 250_000, write: Some(500_000) }));
        // Sync wrote its own new estimate: that is the base
        assert_eq!(scale(300_000, 375_000, 250_000, 1500), Some(Scale { base: 300_000, write: Some(450_000) }));
        // back to 1 while the field is still stretched: the engine's interval again
        assert_eq!(scale(375_000, 375_000, 250_000, ONE), Some(Scale { base: 250_000, write: Some(250_000) }));
        // never more than twice the engine's interval, never outside the plausible range
        assert_eq!(scale(250_000, 0, 0, 9000).unwrap().write, Some(500_000));
        assert_eq!(scale(1_500_000, 0, 0, 2000).unwrap().write, Some(MAX_INTERVAL_US));
        assert_eq!(scale(10, 0, 0, 1500), None, "not an interval: left alone");
        assert_eq!(scale(3_000_000, 0, 0, 1500), None);
    }

    #[test]
    fn the_pacer_stretches_while_the_factor_is_fresh_and_gives_the_interval_back() {
        let p = Pacer::new();
        assert_eq!(p.decide(250_000, 100), None, "nothing asked: the engine's interval is untouched");
        p.set(1500, 1_000);
        assert_eq!(p.decide(250_000, 1_100), Some(375_000));
        // the next batch: Sync left the field as written
        assert_eq!(p.decide(375_000, 1_300), None);
        // Sync's own estimate changed
        assert_eq!(p.decide(200_000, 1_500), Some(300_000));
        p.set(2000, 1_600);
        assert_eq!(p.decide(300_000, 1_700), Some(400_000));
        // the mod stops refreshing: after STALE_MS the engine's own interval comes back, once
        assert_eq!(p.decide(400_000, 1_600 + STALE_MS + 1), Some(200_000));
        assert_eq!(p.decide(200_000, 1_600 + STALE_MS + 2), None);
        assert_eq!(p.decide(220_000, 1_600 + STALE_MS + 3), None, "the engine's new estimate is its own");
        assert!(p.first_line().unwrap().contains("first batch stretched"));
        assert!(p.first_line().is_none(), "once");
    }

    #[test]
    fn a_pacer_switched_off_writes_nothing() {
        let p = Pacer::new();
        p.set(2000, 0);
        p.disable("the timing data is not readable");
        assert_eq!(p.decide(250_000, 10), None);
        p.minute_line(1);
        let line = p.minute_line(61_001).expect("a line, the piece is off");
        assert!(line.contains("OFF: the timing data is not readable"), "{line}");
    }

    #[test]
    fn the_minute_line_says_how_much_was_stretched() {
        let p = Pacer::new();
        assert_eq!(p.minute_line(1_000), None, "the first call starts the minute");
        p.set(1500, 1_000);
        for i in 0..4 {
            let engine = if i == 0 { 250_000 } else { 375_000 };
            p.decide(engine, 1_100 + i);
        }
        assert_eq!(p.minute_line(30_000), None, "not a minute yet");
        let line = p.minute_line(61_000).expect("a minute");
        assert!(line.starts_with("batch_pace: last 60 s: 4 batches, 4 stretched, factor mean 1.50 max 1.50"), "{line}");
        assert!(line.contains("1 pace line(s) from the mod") && line.contains("engine interval 250000 us"), "{line}");
        assert_eq!(p.minute_line(121_000), None, "a quiet minute says nothing");
    }
}
