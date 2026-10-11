use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::messages::ChecksumReportMessage;

/// One part of the world whose hash differs between games, with each reporter's value (`None` = the reporter did
/// not hash that part).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumDifference {
    pub part: String,
    pub values: BTreeMap<u16, Option<String>>,
}

/// Result of one checksum round once every ready player has reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumOutcome {
    pub n: i32,
    pub time: i64,
    pub reporters: Vec<u16>,
    pub differences: Vec<ChecksumDifference>,
    /// Each reporter's own sim time, set only when the samples were taken at different times. Games at x3/x4 step
    /// coarsely, so a round stamped for one time is hashed at the first step at or after it, which differs between
    /// games. Such samples are of different worlds: they are not compared (`differences` stays empty).
    pub time_skew: Option<BTreeMap<u16, i64>>,
}

impl ChecksumOutcome {
    /// False when the samples were taken at different sim times: nothing can be said about them.
    pub fn comparable(&self) -> bool {
        self.time_skew.is_none()
    }

    pub fn identical(&self) -> bool {
        self.comparable() && self.differences.is_empty()
    }

    pub fn differing_parts(&self) -> impl Iterator<Item = &str> {
        self.differences.iter().map(|d| d.part.as_str())
    }
}

/// Collects the reports of checksum rounds and compares them. Pure; the host drives it.
#[derive(Debug, Default)]
pub struct ChecksumRounds {
    rounds: HashMap<i32, BTreeMap<u16, ChecksumReportMessage>>,
}

impl ChecksumRounds {
    const KEEP_ROUNDS: i32 = 20;
    /// Most parts a report may name. A world has about a dozen (money, towns, vehicles, ...) plus one per script.
    pub const MAX_PARTS: usize = 128;
    /// Longest part name or hash in a report (characters). Hashes are 8 hex digits.
    pub const MAX_TEXT: usize = 128;

    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a report is the size of a real one. The host keeps the reports of 20 rounds for every player until
    /// the round completes, so their size is what bounds that memory.
    pub fn acceptable(report: &ChecksumReportMessage) -> bool {
        let text_ok = |s: &String| s.len() <= Self::MAX_TEXT;
        report.parts.len() <= Self::MAX_PARTS
            && report.parts.iter().all(|(k, v)| text_ok(k) && text_ok(v))
            && report.authority.as_ref().is_none_or(|a| a.len() <= Self::MAX_PARTS && a.keys().all(text_ok))
    }

    pub fn open(&mut self, n: i32) {
        self.rounds.insert(n, BTreeMap::new());
    }

    /// Records a report. Returns the outcome when `expected` reporters have all reported, else `None`. Reports for
    /// unknown rounds are ignored. Rounds older than 20 are forgotten.
    pub fn report(&mut self, from: u16, report: ChecksumReportMessage, expected: &[u16]) -> Option<ChecksumOutcome> {
        let n = report.n;
        let round = self.rounds.get_mut(&n)?;
        round.insert(from, report);
        self.rounds.retain(|&k, _| k >= n.saturating_sub(Self::KEEP_ROUNDS));
        let round = self.rounds.get(&n)?;
        if !expected.iter().all(|id| round.contains_key(id)) {
            return None;
        }
        let round = self.rounds.remove(&n)?;
        Some(Self::compare(n, &round))
    }

    /// Rounds that have a report from every one of `expected` although the last report did not just arrive: the game
    /// they were still waiting for left. Each is removed and compared, oldest first. A round no one reported to waits.
    pub fn finished(&mut self, expected: &[u16]) -> Vec<ChecksumOutcome> {
        let mut done: Vec<i32> = self
            .rounds
            .iter()
            .filter(|(_, round)| !round.is_empty() && expected.iter().all(|id| round.contains_key(id)))
            .map(|(&n, _)| n)
            .collect();
        done.sort_unstable();
        done.into_iter().filter_map(|n| self.rounds.remove(&n).map(|round| Self::compare(n, &round))).collect()
    }

    pub fn compare(n: i32, reports: &BTreeMap<u16, ChecksumReportMessage>) -> ChecksumOutcome {
        let reporters: Vec<u16> = reports.keys().copied().collect();
        let time = reporters.first().map_or(0, |id| reports[id].time);
        if reports.values().any(|r| r.time != time) {
            let skew = reports.iter().map(|(&id, r)| (id, r.time)).collect();
            return ChecksumOutcome { n, time, reporters, differences: Vec::new(), time_skew: Some(skew) };
        }
        let parts: BTreeSet<&str> = reports.values().flat_map(|r| r.parts.keys().map(String::as_str)).collect();
        let mut differences = Vec::new();
        for part in parts {
            let values: BTreeMap<u16, Option<String>> =
                reporters.iter().map(|&id| (id, reports[&id].parts.get(part).cloned())).collect();
            let distinct: BTreeSet<&Option<String>> = values.values().collect();
            if distinct.len() > 1 {
                differences.push(ChecksumDifference { part: part.to_owned(), values });
            }
        }
        ChecksumOutcome { n, time, reporters, differences, time_skew: None }
    }
}
