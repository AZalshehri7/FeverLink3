//! When a checksum difference becomes a resync. Pure: the host feeds it outcomes and a clock and it answers with a
//! reason or nothing. See `docs/PROTOCOL.md`, "Resync policy".

use std::collections::BTreeSet;

use super::checksum::ChecksumOutcome;

/// Thresholds. The defaults are the protocol's.
#[derive(Debug, Clone, PartialEq)]
pub struct ChecksumPolicyConfig {
    /// Drift parts must differ continuously this long (seconds).
    pub drift_seconds: f64,
    /// Rounds in a row with the same differing set before an ordinary difference reloads.
    pub consecutive_rounds: u32,
    /// Minimum seconds between the end of one resync and the next policy-driven one.
    pub cooldown_seconds: f64,
}

impl Default for ChecksumPolicyConfig {
    fn default() -> Self {
        Self { drift_seconds: 300.0, consecutive_rounds: 2, cooldown_seconds: 120.0 }
    }
}

#[derive(Debug, Default)]
pub struct ChecksumPolicy {
    config: ChecksumPolicyConfig,
    drift_since: Option<f64>,
    /// The drift parts of the last round that judged them (kept across light rounds, which do not measure the heavy ones).
    last_drift: Option<BTreeSet<String>>,
    last_other: Option<BTreeSet<String>>,
    streak: u32,
    last_resync_end: Option<f64>,
}

impl ChecksumPolicy {
    /// Parts that drift without anyone doing anything (the engine is not bit-exact for them); `script:*` too.
    pub const DRIFT_PARTS: [&'static str; 5] = ["vehicles", "onboard", "persons", "stocks", "nodeConfigs"];
    /// Corrected through Authority, never a reason to reload. The lanes the script reports are `balance` and `loan`
    /// (`money` is the older name); a rule written for "money" alone let a difference of 180 in `balance` reload the world.
    /// Lanes that cost much game thread time in a big world (the sum of one component read per edge, node, construction and town
    /// building): a running full round reads them spread over the steps, and a light round leaves them out. Every regular round is
    /// full by default since 2026-10-18 (`HostOptions::full_checksum_every`); light rounds remain for the lab switch and for an
    /// older host. `townBuildings:<town>` is the per-town part of `townBuildings`.
    /// `ids` (the entity ids of edges, constructions and town buildings) is heavy too, and an ordinary part: ids that differ mean a build
    /// was applied at another step on one game, which the stamp rule should never let happen; the repair is a resync.
    pub const HEAVY_PARTS: [&'static str; 5] = ["edges", "constructions", "nodeConfigs", "townBuildings", "ids"];
    pub const MONEY_PARTS: [&'static str; 3] = ["money", "balance", "loan"];

    pub fn new(config: ChecksumPolicyConfig) -> Self {
        Self { config, ..Self::default() }
    }

    pub fn config(&self) -> &ChecksumPolicyConfig {
        &self.config
    }

    /// The fastest game speed: a batch of the engine is at most this many steps.
    pub const SPREAD_MAX_STEPS: u32 = 4;

    /// Steps between two chunks of a spread full round (`ChecksumRequestMessage::spread`) at game speed `speed`: one engine batch,
    /// so the chunks land where the batches fall anyway and the landing marks of the chunks never slow the game; 0 for a light or a
    /// paused round, which are hashed whole at their stamp (light ones cost a few ms; at a pause the simulation does not run).
    /// Real runs 2026-10-15/16: a full round read about 6500 entities in 370-480 ms (1.0 s at worst) of one step.
    pub fn spread_steps(full: bool, paused: bool, speed: i32) -> u32 {
        if !full || paused {
            return 0;
        }
        u32::try_from(speed).unwrap_or(1).clamp(1, Self::SPREAD_MAX_STEPS)
    }

    /// The games' own chunk (the mod's `L.CHECKSUM_CHUNK`), used until a cost was measured.
    pub const CHUNK_DEFAULT: u32 = 200;
    pub const CHUNK_MIN: u32 = 50;
    pub const CHUNK_MAX: u32 = 4000;
    /// What one chunk should cost on the slowest game: 40 % of the step budget, room for a slower moment (GC, a busy step).
    pub const CHUNK_TARGET_MS: u32 = 12;
    /// The mod's `L.CHECKSUM_FRAME_BUDGET_MS`: what one step of a spread round should cost at most.
    pub const STEP_BUDGET_MS: u32 = 30;
    /// Rounds of each game whose cost counts (the dearest of them).
    pub const COST_ROUNDS: usize = 3;

    /// Entities a chunk of a spread round reads, from the dearest cost per entity (ns) the games reported, so the slowest game's
    /// chunk costs about `CHUNK_TARGET_MS`. The same number goes to every game (each cuts its lists at the same places, so the
    /// comparison stays exact). Grows at most twice per round (a cheap round after a dear one is no proof), shrinks at once.
    /// Measured 2026-10-18/19: 17-30 us an entity on the lab PC and in the friend test (step max 4-8 ms for 200): about 400-700
    /// entities a chunk, 3-4x fewer chunks: a round of the friend-test world (7400 entities) in about 15 batches instead of 38.
    pub fn chunk_for(slowest_ns: Option<u32>, previous: u32) -> u32 {
        let previous = if previous == 0 { Self::CHUNK_DEFAULT } else { previous };
        let Some(ns) = slowest_ns.filter(|&ns| ns > 0) else { return previous };
        let wanted = (u64::from(Self::CHUNK_TARGET_MS) * 1_000_000 / u64::from(ns))
            .clamp(u64::from(Self::CHUNK_MIN), u64::from(Self::CHUNK_MAX));
        let wanted = u32::try_from(wanted).unwrap_or(Self::CHUNK_MAX);
        wanted.min(previous.saturating_mul(2))
    }

    pub fn is_heavy_part(part: &str) -> bool {
        Self::HEAVY_PARTS.contains(&part) || part.starts_with("townBuildings:")
    }

    pub fn is_money_part(part: &str) -> bool {
        Self::MONEY_PARTS.contains(&part)
    }

    pub fn is_drift_part(part: &str) -> bool {
        Self::DRIFT_PARTS.contains(&part) || part.starts_with("script:")
    }

    /// The lanes of one checksum round for a log line: each name, with what happens to it when it is not an ordinary
    /// part ("stations, vehicles (drift), money (corrected)").
    pub fn describe_parts<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
        let named: Vec<String> = parts
            .into_iter()
            .map(|p| {
                if Self::is_money_part(p) {
                    format!("{p} (corrected)")
                } else if Self::is_drift_part(p) {
                    format!("{p} (drift)")
                } else {
                    p.to_owned()
                }
            })
            .collect();
        named.join(", ")
    }

    pub fn in_cooldown(&self, now: f64) -> bool {
        self.last_resync_end.is_some_and(|end| now - end < self.config.cooldown_seconds)
    }

    /// Judges one finished checksum round. `Some(reason)` when the games should be resynchronised now.
    pub fn on_outcome(&mut self, now: f64, outcome: &ChecksumOutcome) -> Option<String> {
        if !outcome.comparable() {
            // sampled at different sim times (coarse steps at x3/x4): neither a match nor a desync; streaks stay as they are
            return None;
        }
        let parts: BTreeSet<String> =
            outcome.differing_parts().filter(|p| !Self::is_money_part(p)).map(str::to_owned).collect();
        let drift: BTreeSet<String> = parts.iter().filter(|p| Self::is_drift_part(p)).cloned().collect();
        let other: BTreeSet<String> = parts.iter().filter(|p| !Self::is_drift_part(p)).cloned().collect();

        // A light round did not measure the heavy lanes, and a part some game reported not ready was not measured either: what an
        // earlier round found there stands until a round measures it again (a streak made of unmeasured parts neither grows nor
        // resets).
        let unmeasured = |p: &str| (!outcome.full && Self::is_heavy_part(p)) || outcome.not_ready.contains_key(p);
        let all_unmeasured = |s: &Option<BTreeSet<String>>| s.as_ref().is_some_and(|s| s.iter().all(|p| unmeasured(p)));
        let keep_drift = drift.is_empty() && all_unmeasured(&self.last_drift);
        let keep_other = other.is_empty() && all_unmeasured(&self.last_other);
        if keep_drift {
            // unchanged
        } else if drift.is_empty() {
            self.drift_since = None;
            self.last_drift = None;
        } else {
            if self.drift_since.is_none() {
                self.drift_since = Some(now);
            }
            self.last_drift = Some(drift.clone());
        }
        if keep_other {
            // unchanged
        } else if other.is_empty() {
            self.last_other = None;
            self.streak = 0;
        } else if self.last_other.as_ref() == Some(&other) {
            self.streak += 1;
        } else {
            self.last_other = Some(other.clone());
            self.streak = 1;
        }

        let reason = if !other.is_empty() && self.streak >= self.config.consecutive_rounds {
            Some(format!("{} differ in {} checks in a row", join(&other), self.streak))
        } else {
            self.drift_since.filter(|since| now - since >= self.config.drift_seconds).map(|since| {
                format!("{} drifted for {:.0} s", join(self.last_drift.as_ref().unwrap_or(&drift)), now - since)
            })
        };
        let reason = reason?;
        if self.in_cooldown(now) {
            return None;
        }
        self.reset();
        Some(reason)
    }

    /// A game failed to replay an action: resync at once, cooldown permitting.
    pub fn on_replay_failed(&mut self, now: f64) -> Option<String> {
        if self.in_cooldown(now) {
            return None;
        }
        self.reset();
        Some("replay failed".to_owned())
    }

    /// The games' verdicts of action `key` at its stamp differ (one applied it, another refused it): a fork, resync at once,
    /// cooldown permitting.
    pub fn on_verdicts_differ(&mut self, now: f64, key: &str) -> Option<String> {
        if self.in_cooldown(now) {
            return None;
        }
        self.reset();
        Some(format!("verdicts of {key} differ"))
    }

    /// A resync round ended (done or failed): starts the cooldown and forgets what was counted.
    pub fn resync_finished(&mut self, now: f64) {
        self.reset();
        self.last_resync_end = Some(now);
    }

    fn reset(&mut self) {
        self.drift_since = None;
        self.last_drift = None;
        self.last_other = None;
        self.streak = 0;
    }
}

fn join(set: &BTreeSet<String>) -> String {
    set.iter().map(String::as_str).collect::<Vec<_>>().join(", ")
}
