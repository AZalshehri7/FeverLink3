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
    /// building): a light round leaves them out. `townBuildings:<town>` is the per-town part of `townBuildings`.
    pub const HEAVY_PARTS: [&'static str; 4] = ["edges", "constructions", "nodeConfigs", "townBuildings"];
    pub const MONEY_PARTS: [&'static str; 3] = ["money", "balance", "loan"];

    pub fn new(config: ChecksumPolicyConfig) -> Self {
        Self { config, ..Self::default() }
    }

    pub fn config(&self) -> &ChecksumPolicyConfig {
        &self.config
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

        // A light round did not measure the heavy lanes: what an earlier full round found there stands until the next full round
        // (a streak made of heavy parts neither grows nor resets on a light round).
        let all_heavy =
            |s: &Option<BTreeSet<String>>| s.as_ref().is_some_and(|s| s.iter().all(|p| Self::is_heavy_part(p)));
        let keep_drift = !outcome.full && drift.is_empty() && all_heavy(&self.last_drift);
        let keep_other = !outcome.full && other.is_empty() && all_heavy(&self.last_other);
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
