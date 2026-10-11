//! The pure part of the town-growth diagnostic and seed experiment (`FEVERLINK3_LOG_GROWTH=1`,
//! `FEVERLINK3_GROWTH_SEED_MODE=derive`, `docs/NATIVE_LINK.md` section 11).
//!
//! The hook itself is in `hooks.rs`; this is the part that needs no Windows: what the switches say, which calls are
//! logged, how the replacement seed is made and how a call is turned into a log line.
//!
//! What the game does (build 40420, `docs/research/TOWN_RNG_STATE.md`): the `TownUpdateSize` command handler makes
//! one 32-bit `std::minstd_rand` state per command, seeded from a hash of the game time, and passes a pointer to it
//! to `TownDeveloper::Develop`, which draws growth decisions from it. The experiment changes only that one number,
//! before `Develop` reads it: which random numbers are drawn. Growth, ratings and every rule stay the game's own.
//!
//! Log lines (all fields are things two games can agree on; `n` and `caller` are the only ones that need not):
//!
//! ```text
//! growth n=<counter> town=<entity> seed=0x<original> [t=<game time>] [used=0x<derived>] caller=<rva>
//! ```
//!
//! Two games are compared by cutting the counter (`cut -d' ' -f3-`) and sorting, because `n` is a per-process count.

use core::sync::atomic::{AtomicU64, Ordering};

/// Prefix of every line, so the log can be filtered with `grep '^growth '`.
pub const PREFIX: &str = "growth";

pub const LOG_GROWTH_VAR: &str = "FEVERLINK3_LOG_GROWTH";
pub const MODE_VAR: &str = "FEVERLINK3_GROWTH_SEED_MODE";
pub const AFTER_VAR: &str = "FEVERLINK3_GROWTH_SEED_AFTER";
pub const AFTER_GAMETIME_VAR: &str = "FEVERLINK3_GROWTH_SEED_AFTER_GAMETIME";
pub const QUANTUM_VAR: &str = "FEVERLINK3_GROWTH_SEED_QUANTUM_MS";

/// The default quantum of the derived seed's time input.
pub const DEFAULT_QUANTUM_MS: u32 = 1000;

/// The first calls that are all logged; after them one in [`SAMPLE_EVERY`] (chosen by content, see [`sampled`]).
pub const LOG_FIRST: u64 = 2000;
pub const SAMPLE_EVERY: u64 = 50;

/// The modulus of the game's `minstd_rand`: valid states are `1..=0x7ffffffe`.
pub const MINSTD_MODULUS: u32 = 0x7fff_ffff;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Log only (or nothing, without `FEVERLINK3_LOG_GROWTH`).
    #[default]
    Off,
    /// After the switch, the seed the original reads is replaced by [`derive`].
    Derive,
}

/// What the growth switches say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// `FEVERLINK3_LOG_GROWTH=1` (implied by `Mode::Derive`).
    pub log: bool,
    pub mode: Mode,
    /// Wall seconds since the hook was installed before the switch (default 0).
    pub after_wall_s: u64,
    /// Game seconds (the game-time field divided by 1000) before the switch; `None` = not required.
    pub after_game_s: Option<u64>,
    /// The derived seed uses the game time rounded down to a multiple of this many milliseconds.
    pub quantum_ms: u32,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { log: false, mode: Mode::Off, after_wall_s: 0, after_game_s: None, quantum_ms: DEFAULT_QUANTUM_MS }
    }
}

impl Settings {
    /// True when the growth hook is wanted at all.
    pub fn active(&self) -> bool {
        self.log || self.mode == Mode::Derive
    }

    /// Reads the switches through `get`. A value that does not parse is left at its default and named in the
    /// returned complaints (the caller logs them); nothing is ever refused loudly.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> (Settings, Vec<String>) {
        let mut s = Settings::default();
        let mut complaints = Vec::new();
        s.log = get(LOG_GROWTH_VAR).is_some_and(|v| v.trim() == "1");
        if let Some(v) = get(MODE_VAR).filter(|v| !v.trim().is_empty()) {
            match v.trim().to_ascii_lowercase().as_str() {
                "off" => {}
                "derive" => s.mode = Mode::Derive,
                other => complaints.push(format!("{MODE_VAR}={other:?} ignored: only `off` and `derive` exist (off)")),
            }
        }
        let mut number = |var: &str, what: &str| -> Option<u64> {
            let v = get(var).filter(|v| !v.trim().is_empty())?;
            match v.trim().parse::<u32>() {
                Ok(n) => Some(u64::from(n)),
                Err(_) => {
                    complaints.push(format!("{var}={:?} ignored: not a whole number of {what}", v.trim()));
                    None
                }
            }
        };
        if let Some(n) = number(AFTER_VAR, "seconds") {
            s.after_wall_s = n;
        }
        s.after_game_s = number(AFTER_GAMETIME_VAR, "game seconds");
        if let Some(q) = number(QUANTUM_VAR, "milliseconds") {
            if q == 0 {
                complaints
                    .push(format!("{QUANTUM_VAR}=0 ignored: the quantum must be at least 1 ms ({DEFAULT_QUANTUM_MS})"));
            } else {
                s.quantum_ms = q as u32;
            }
        }
        (s, complaints)
    }

    /// For the `hooks: switches` line; empty when the growth hook is not wanted.
    pub fn describe(&self) -> String {
        if !self.active() {
            return String::new();
        }
        let mode = match self.mode {
            Mode::Off => "off",
            Mode::Derive => "derive",
        };
        let game = self.after_game_s.map(|g| format!(" after_gametime={g}s")).unwrap_or_default();
        format!(" growth=on seed_mode={mode} after={}s{game} quantum={}ms", self.after_wall_s, self.quantum_ms)
    }

    /// True once the seed is to be replaced: the wall time since the hook was installed has passed
    /// `after_wall_s` and, when a game-time threshold is set, the game time (milliseconds) has reached it. With a
    /// threshold and no readable game time the answer is no. The derived seed needs the game time, so without a
    /// readable one the answer is no either.
    pub fn switch_due(&self, wall_elapsed_s: u64, game_time_ms: Option<u32>) -> bool {
        if self.mode != Mode::Derive || wall_elapsed_s < self.after_wall_s {
            return false;
        }
        match (self.after_game_s, game_time_ms) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(g), Some(t)) => u64::from(t) >= g.saturating_mul(1000),
        }
    }
}

/// Game time rounded down to a multiple of `quantum_ms` (at least 1).
pub fn quantize(time_ms: u32, quantum_ms: u32) -> u32 {
    let q = quantum_ms.max(1);
    time_ms - time_ms % q
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The replacement seed: FNV-1a (64 bit) over the four little-endian bytes of the town's entity id and the four of
/// the quantized game time, folded to 32 bits and mapped into `1..=0x7ffffffe`, the range of `minstd_rand` states.
/// A pure function of (town, floor(time / quantum) * quantum): two games that run the same growth command within
/// the same quantum draw the same numbers, whatever else differs between them.
pub fn derive(town: u32, time_ms: u32, quantum_ms: u32) -> u32 {
    let t = quantize(time_ms, quantum_ms);
    let mut h = FNV_OFFSET;
    for b in town.to_le_bytes().into_iter().chain(t.to_le_bytes()) {
        h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
    }
    let folded = (h ^ (h >> 32)) as u32;
    folded % (MINSTD_MODULUS - 1) + 1
}

/// Whether call number `n` is logged: the first [`LOG_FIRST`] all, then about one in [`SAMPLE_EVERY`]. The later
/// ones are chosen by the content of the call (`town`, original `seed`), not by `n`, so two games that make the
/// same calls log the same ones even when their counters differ.
pub fn sampled(n: u64, town: u32, seed: u32) -> bool {
    n <= LOG_FIRST || {
        let mut h = FNV_OFFSET;
        for b in town.to_le_bytes().into_iter().chain(seed.to_le_bytes()) {
            h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
        }
        (h >> 20).is_multiple_of(SAMPLE_EVERY)
    }
}

/// Calls counted so far; the first call is number 1.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub const fn new() -> Counter {
        Counter(AtomicU64::new(0))
    }

    pub fn reset(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// The log line of one call. `time` is the game time when it could be read, `used` the seed the original was
/// called with when it differs from `seed`.
pub fn line(n: u64, town: u32, seed: u32, time: Option<u32>, used: Option<u32>, caller: &str) -> String {
    let time = time.map(|t| format!(" t={t}")).unwrap_or_default();
    let used = used.map(|u| format!(" used={u:#010x}")).unwrap_or_default();
    format!("{PREFIX} n={n} town={town} seed={seed:#010x}{time}{used} caller={caller}")
}

/// The line that says the seed is being replaced from now on.
pub fn switch_line(n: u64, wall_s: u64, time: Option<u32>, s: &Settings) -> String {
    let t = time.map_or("unknown".to_string(), |t| t.to_string());
    format!(
        "growth-switch n={n}: from now on the seed is derived from (town, game time rounded down to {} ms); wall={wall_s}s \
         game_time={t} (thresholds: wall>={}s, game>={})",
        s.quantum_ms,
        s.after_wall_s,
        s.after_game_s.map_or("none".to_string(), |g| format!("{g}s"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn nothing_set_means_nothing_wanted() {
        let (s, c) = Settings::from_lookup(env(&[]));
        assert_eq!(s, Settings::default());
        assert!(!s.active() && c.is_empty() && s.describe().is_empty());
    }

    #[test]
    fn the_log_switch_wants_exactly_one() {
        assert!(Settings::from_lookup(env(&[(LOG_GROWTH_VAR, "1")])).0.active());
        assert!(!Settings::from_lookup(env(&[(LOG_GROWTH_VAR, "true")])).0.active());
        assert!(!Settings::from_lookup(env(&[(LOG_GROWTH_VAR, "0")])).0.active());
    }

    #[test]
    fn derive_mode_implies_the_hook_and_off_is_the_default() {
        let (s, c) = Settings::from_lookup(env(&[(MODE_VAR, " Derive ")]));
        assert!(s.active() && s.mode == Mode::Derive && c.is_empty() && !s.log);
        let (s, _) = Settings::from_lookup(env(&[(MODE_VAR, "off")]));
        assert!(!s.active());
        let (s, c) = Settings::from_lookup(env(&[(MODE_VAR, "sideways"), (LOG_GROWTH_VAR, "1")]));
        assert_eq!(s.mode, Mode::Off);
        assert!(s.log && c.len() == 1 && c[0].contains("sideways"), "{c:?}");
    }

    #[test]
    fn numbers_parse_and_bad_ones_are_named_and_ignored() {
        let (s, c) = Settings::from_lookup(env(&[
            (MODE_VAR, "derive"),
            (AFTER_VAR, "30"),
            (AFTER_GAMETIME_VAR, "600"),
            (QUANTUM_VAR, "250"),
        ]));
        assert_eq!((s.after_wall_s, s.after_game_s, s.quantum_ms), (30, Some(600), 250));
        assert!(c.is_empty());
        assert_eq!(s.describe(), " growth=on seed_mode=derive after=30s after_gametime=600s quantum=250ms");
        let (s, c) = Settings::from_lookup(env(&[
            (MODE_VAR, "derive"),
            (AFTER_VAR, "-1"),
            (AFTER_GAMETIME_VAR, "soon"),
            (QUANTUM_VAR, "0"),
        ]));
        assert_eq!((s.after_wall_s, s.after_game_s, s.quantum_ms), (0, None, DEFAULT_QUANTUM_MS));
        assert_eq!(c.len(), 3, "{c:?}");
    }

    #[test]
    fn the_switch_waits_for_wall_time_and_game_time() {
        let s = Settings { mode: Mode::Derive, after_wall_s: 10, after_game_s: Some(60), ..Default::default() };
        assert!(!s.switch_due(9, Some(90_000)), "wall time not yet");
        assert!(!s.switch_due(10, Some(59_999)), "game time not yet");
        assert!(s.switch_due(10, Some(60_000)));
        assert!(!s.switch_due(100, None), "a game-time threshold without a readable game time never fires");
        let s = Settings { mode: Mode::Derive, ..Default::default() };
        assert!(s.switch_due(0, Some(0)), "default: from the first call that has a game time");
        assert!(!s.switch_due(0, None), "no game time, no derived seed");
        assert!(!Settings::default().switch_due(1000, Some(u32::MAX)), "mode off never switches");
    }

    #[test]
    fn the_time_rounds_down_to_the_quantum() {
        assert_eq!(quantize(1999, 1000), 1000);
        assert_eq!(quantize(2000, 1000), 2000);
        assert_eq!(quantize(1234, 1), 1234);
        assert_eq!(quantize(1234, 0), 1234, "a zero quantum cannot divide by zero");
    }

    #[test]
    fn two_calls_in_one_quantum_derive_the_same_seed_and_towns_differ() {
        let a = derive(77, 5_100, 1000);
        assert_eq!(a, derive(77, 5_400, 1000), "300 ms apart, same quantum");
        assert_eq!(a, derive(77, 5_999, 1000));
        assert_ne!(a, derive(77, 6_000, 1000), "next quantum");
        assert_ne!(a, derive(78, 5_100, 1000), "another town");
        assert_ne!(derive(77, 5_100, 250), derive(77, 5_300, 250), "a finer quantum separates them");
    }

    #[test]
    fn a_derived_seed_is_always_a_valid_minstd_state() {
        for town in [0u32, 1, 2, 0xffff_ffff, 123_456] {
            for t in (0..2_000_000u32).step_by(7919) {
                let d = derive(town, t, 1000);
                assert!((1..=0x7fff_fffe).contains(&d), "{town} {t}: {d:#x}");
            }
        }
    }

    #[test]
    fn the_derived_seed_has_fixed_values() {
        // pins the function: a change here changes which numbers every future run draws
        assert_eq!(derive(0, 0, 1000), derive(0, 999, 1000));
        let v = [derive(1, 0, 1000), derive(1, 1000, 1000), derive(2, 0, 1000)];
        assert!(v[0] != v[1] && v[0] != v[2] && v[1] != v[2]);
        assert_eq!(v, [derive(1, 0, 1000), derive(1, 1000, 1000), derive(2, 0, 1000)]);
    }

    #[test]
    fn the_first_calls_are_all_logged_and_later_ones_by_content() {
        assert!((1..=LOG_FIRST).all(|n| sampled(n, 5, 6)));
        let later: u64 =
            (0..20_000u32).map(|i| u64::from(sampled(LOG_FIRST + 1, i % 40, i.wrapping_mul(2_654_435_761)))).sum();
        assert!((200..800).contains(&later), "about 1 in 50 of 20000, got {later}");
        // the same content gives the same decision whatever the counter says
        assert_eq!(sampled(5000, 9, 123), sampled(9000, 9, 123));
    }

    #[test]
    fn a_line_has_the_fields_in_a_fixed_order() {
        assert_eq!(
            line(7, 42, 0xdead_beef, None, None, "0x9e1470"),
            "growth n=7 town=42 seed=0xdeadbeef caller=0x9e1470"
        );
        assert_eq!(
            line(8, 42, 5, Some(12_345), Some(0x1234), "?"),
            "growth n=8 town=42 seed=0x00000005 t=12345 used=0x00001234 caller=?"
        );
        let cut = |s: &str| s.splitn(3, ' ').nth(2).unwrap().to_string();
        assert_eq!(cut(&line(1, 4, 5, Some(6), None, "x")), cut(&line(99, 4, 5, Some(6), None, "x")));
    }
}
