use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::policy::ChecksumPolicy;

use super::messages::ChecksumReportMessage;

/// The free-id queue's part (added by the session controller from the native module's order trace, not by the mod): `<epoch>|<n>:<h>|...`
/// where `<epoch>` names the loaded world's starting queue and each `<n>:<h>` is the hash of the engine's id stream (ids handed out
/// and ids freed, in order) after its `n`-th op. Not a hash of one moment: two games read it a little apart, so it is compared at the
/// newest `n` every report names, and not at all when a game has no such part, another epoch, or no common `n` (unknown, not
/// different). Equal `n` with another hash: the next entity ids will differ (friend test 2026-10-21: the same build got other ids
/// 1-3 min after a fork the content lanes could not see).
pub const FREEQ_PART: &str = "freeq";

/// `(epoch, [(n, hash)])` of a `freeq` value, `None` when it does not read.
fn freeq_fields(v: &str) -> Option<(&str, BTreeMap<u64, &str>)> {
    let mut it = v.split('|');
    let epoch = it.next().filter(|e| !e.is_empty())?;
    let checks = it
        .map(|c| c.split_once(':').and_then(|(n, h)| Some((n.parse::<u64>().ok()?, h))))
        .collect::<Option<BTreeMap<u64, &str>>>()?;
    Some((epoch, checks))
}

/// Whether the `freeq` values of every reporter disagree: one epoch, a common checkpoint, and another hash there.
fn freeq_differs(values: &BTreeMap<u16, Option<String>>) -> bool {
    let Some(read) = values.values().map(|v| v.as_deref().and_then(freeq_fields)).collect::<Option<Vec<_>>>() else {
        return false;
    };
    let Some((epoch, first)) = read.first() else { return false };
    if read.iter().any(|(e, _)| e != epoch) {
        return false;
    }
    let common = first.keys().rev().find(|n| read.iter().all(|(_, c)| c.contains_key(n)));
    common.is_some_and(|n| read.iter().any(|(_, c)| c[n] != first[n]))
}

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
    /// False for a light round: the heavy lanes were not compared (see `ChecksumPolicy::HEAVY_PARTS`).
    pub full: bool,
    /// Parts some game could not hash in this round (its report named them not ready), with those games. Such a part is
    /// unknown, not different: it is left out of `differences` and not compared in this round (seen in a real game: a game
    /// that verified right after a reload, before its registry was read back from the loaded state, failed every repair
    /// with "companies"). A part that two games both hashed and that differs is still a difference.
    pub not_ready: BTreeMap<String, Vec<u16>>,
    /// For a differing part every game sent itemised values of (`ChecksumReportMessage::samples`, a detail round): the items
    /// whose values differ, each as "<item> <id>='<value>' ..." (`-` = the game has no such item), at most
    /// [`ChecksumRounds::MAX_SAMPLE_LINES`]. Only for the log: what is compared is the part's hash.
    pub samples: BTreeMap<String, Vec<String>>,
}

impl ChecksumOutcome {
    /// False when the samples were taken at different sim times: nothing can be said about them.
    pub fn comparable(&self) -> bool {
        self.time_skew.is_none()
    }

    /// Every part was hashed by every game (nothing in `not_ready`).
    pub fn ready(&self) -> bool {
        self.not_ready.is_empty()
    }

    /// Comparable, every part measured on every game, and no part differs.
    pub fn identical(&self) -> bool {
        self.comparable() && self.ready() && self.differences.is_empty()
    }

    pub fn differing_parts(&self) -> impl Iterator<Item = &str> {
        self.differences.iter().map(|d| d.part.as_str())
    }

    /// "companies on 1, townBuildings on 0, 1": the parts that were not measured, for a log line.
    pub fn describe_not_ready(&self) -> String {
        let named: Vec<String> = self
            .not_ready
            .iter()
            .map(|(part, ids)| {
                let ids: Vec<String> = ids.iter().map(u16::to_string).collect();
                format!("{part} on {}", ids.join(", "))
            })
            .collect();
        named.join("; ")
    }
}

/// Collects the reports of checksum rounds and compares them. Pure; the host drives it.
#[derive(Debug, Default)]
pub struct ChecksumRounds {
    rounds: HashMap<i32, BTreeMap<u16, ChecksumReportMessage>>,
    /// Rounds that were asked as light rounds.
    light: HashSet<i32>,
}

impl ChecksumRounds {
    const KEEP_ROUNDS: i32 = 20;
    /// Most parts a report may name. A world has about a dozen (money, towns, vehicles, ...) plus one per script.
    pub const MAX_PARTS: usize = 128;
    /// Longest part name or hash in a report (characters). Hashes are 8 hex digits.
    pub const MAX_TEXT: usize = 128;
    /// Most itemised values a report's samples may hold, over all its parts (the mod sends at most 400 names).
    pub const MAX_SAMPLES: usize = 512;
    /// Most differing items logged for one part.
    pub const MAX_SAMPLE_LINES: usize = 8;

    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a report is the size of a real one. The host keeps the reports of 20 rounds for every player until
    /// the round completes, so their size is what bounds that memory.
    pub fn acceptable(report: &ChecksumReportMessage) -> bool {
        let text_ok = |s: &String| s.len() <= Self::MAX_TEXT;
        report.parts.len() <= Self::MAX_PARTS
            && report.parts.iter().all(|(k, v)| text_ok(k) && text_ok(v))
            && report.not_ready.len() <= Self::MAX_PARTS
            && report.not_ready.iter().all(text_ok)
            && report.authority.as_ref().is_none_or(|a| a.len() <= Self::MAX_PARTS && a.keys().all(text_ok))
    }

    /// Samples within the bounds; a report whose samples are not loses them, never the report (they are only for the log).
    fn samples_ok(report: &ChecksumReportMessage) -> bool {
        let text_ok = |s: &String| s.len() <= Self::MAX_TEXT;
        report.samples.len() <= Self::MAX_PARTS
            && report.samples.values().map(BTreeMap::len).sum::<usize>() <= Self::MAX_SAMPLES
            && report.samples.iter().all(|(p, items)| text_ok(p) && items.iter().all(|(k, v)| text_ok(k) && text_ok(v)))
    }

    /// The items of `part` whose values differ between the games (`samples`), when every game sent that part's items; the
    /// items every game has come first, then those some game lacks. At most [`Self::MAX_SAMPLE_LINES`].
    fn sample_lines(part: &str, reports: &BTreeMap<u16, ChecksumReportMessage>) -> Option<Vec<String>> {
        let samples: Vec<(u16, &BTreeMap<String, String>)> =
            reports.iter().map(|(&id, r)| r.samples.get(part).map(|s| (id, s))).collect::<Option<_>>()?;
        let keys: BTreeSet<&String> = samples.iter().flat_map(|(_, s)| s.keys()).collect();
        let line = |k: &String| {
            let vals: Vec<Option<&String>> = samples.iter().map(|(_, s)| s.get(k)).collect();
            if vals.iter().all(|v| *v == vals[0]) {
                return None;
            }
            let each: Vec<String> = samples
                .iter()
                .zip(&vals)
                .map(|((id, _), v)| v.map_or_else(|| format!("{id}=-"), |v| format!("{id}='{v}'")))
                .collect();
            Some((vals.iter().any(Option::is_none), format!("{k} {}", each.join(" "))))
        };
        let mut lines: Vec<(bool, String)> = keys.into_iter().filter_map(line).collect();
        lines.sort_by_key(|(missing, _)| *missing);
        Some(lines.into_iter().take(Self::MAX_SAMPLE_LINES).map(|(_, l)| l).collect())
    }

    pub fn open(&mut self, n: i32) {
        self.rounds.insert(n, BTreeMap::new());
        self.light.remove(&n);
    }

    /// Opens a light round: the heavy lanes are dropped from every report (a game that does not know the flag sends them anyway,
    /// and a lane that only some games report would read as a difference).
    pub fn open_light(&mut self, n: i32) {
        self.rounds.insert(n, BTreeMap::new());
        self.light.insert(n);
    }

    /// Whether round `n` is still waiting for reports.
    pub fn is_open(&self, n: i32) -> bool {
        self.rounds.contains_key(&n)
    }

    /// Who of `expected` has not reported round `n` yet (empty when the round is not open).
    pub fn missing(&self, n: i32, expected: &[u16]) -> Vec<u16> {
        self.rounds
            .get(&n)
            .map_or_else(Vec::new, |round| expected.iter().copied().filter(|id| !round.contains_key(id)).collect())
    }

    /// Forgets round `n` without comparing it (it will never complete); later reports for it are ignored. True when it was open.
    pub fn drop_round(&mut self, n: i32) -> bool {
        self.light.remove(&n);
        self.rounds.remove(&n).is_some()
    }

    fn finish(&mut self, n: i32, round: BTreeMap<u16, ChecksumReportMessage>) -> ChecksumOutcome {
        let full = !self.light.remove(&n);
        let mut outcome = Self::compare(n, &round);
        outcome.full = full;
        outcome
    }

    /// Records a report. Returns the outcome when `expected` reporters have all reported, else `None`. Reports for
    /// unknown rounds are ignored. Rounds older than 20 are forgotten.
    pub fn report(&mut self, from: u16, report: ChecksumReportMessage, expected: &[u16]) -> Option<ChecksumOutcome> {
        let n = report.n;
        let round = self.rounds.get_mut(&n)?;
        let mut report = report;
        if !Self::samples_ok(&report) {
            report.samples.clear();
        }
        if self.light.contains(&n) {
            report.parts.retain(|part, _| !ChecksumPolicy::is_heavy_part(part));
            report.not_ready.retain(|part| !ChecksumPolicy::is_heavy_part(part));
        }
        round.insert(from, report);
        self.rounds.retain(|&k, _| k >= n.saturating_sub(Self::KEEP_ROUNDS));
        let round = self.rounds.get(&n)?;
        if !expected.iter().all(|id| round.contains_key(id)) {
            return None;
        }
        let round = self.rounds.remove(&n)?;
        Some(self.finish(n, round))
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
        done.into_iter().filter_map(|n| self.rounds.remove(&n).map(|round| self.finish(n, round))).collect()
    }

    pub fn compare(n: i32, reports: &BTreeMap<u16, ChecksumReportMessage>) -> ChecksumOutcome {
        let reporters: Vec<u16> = reports.keys().copied().collect();
        let time = reporters.first().map_or(0, |id| reports[id].time);
        if reports.values().any(|r| r.time != time) {
            let skew = reports.iter().map(|(&id, r)| (id, r.time)).collect();
            return ChecksumOutcome {
                n,
                time,
                reporters,
                differences: Vec::new(),
                time_skew: Some(skew),
                full: true,
                not_ready: BTreeMap::new(),
                samples: BTreeMap::new(),
            };
        }
        // a part one game could not hash is unknown in this round on every game: not compared, never a difference
        let mut not_ready: BTreeMap<String, Vec<u16>> = BTreeMap::new();
        for (&id, r) in reports {
            for part in r.not_ready.iter().collect::<BTreeSet<_>>() {
                not_ready.entry(part.clone()).or_default().push(id);
            }
        }
        let parts: BTreeSet<&str> = reports
            .values()
            .flat_map(|r| r.parts.keys().map(String::as_str))
            .filter(|p| !not_ready.contains_key(*p))
            .collect();
        let mut differences = Vec::new();
        for part in parts {
            let values: BTreeMap<u16, Option<String>> =
                reporters.iter().map(|&id| (id, reports[&id].parts.get(part).cloned())).collect();
            let distinct: BTreeSet<&Option<String>> = values.values().collect();
            if part == FREEQ_PART {
                if freeq_differs(&values) {
                    differences.push(ChecksumDifference { part: part.to_owned(), values });
                }
            } else if distinct.len() > 1 {
                differences.push(ChecksumDifference { part: part.to_owned(), values });
            }
        }
        let samples =
            differences.iter().filter_map(|d| Some((d.part.clone(), Self::sample_lines(&d.part, reports)?))).collect();
        ChecksumOutcome { n, time, reporters, differences, time_skew: None, full: true, not_ready, samples }
    }
}
