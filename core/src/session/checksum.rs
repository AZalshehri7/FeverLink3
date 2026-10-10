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
}

impl ChecksumOutcome {
    pub fn identical(&self) -> bool {
        self.differences.is_empty()
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

    pub fn new() -> Self {
        Self::default()
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
        self.rounds.retain(|&k, _| k >= n - Self::KEEP_ROUNDS);
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
        let time = reporters.first().map_or(0, |id| reports[id].time);
        ChecksumOutcome { n, time, reporters, differences }
    }
}
