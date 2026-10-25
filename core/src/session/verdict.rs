//! Every game's verdict of an action at its stamp: applied or refused. A refusal decided at the stamp from the game's own world is
//! deterministic, so identical worlds refuse an action identically and nothing needs repairing; only games whose verdicts DIFFER (one
//! applied the build, another refused it) have forked. Lab run 2026-10-18 (A-stress): a town replaced an industrial building during a
//! build's lead, both games refused the build at its stamp ("toRemove[0] names construction 39513, which does not exist here"), and each
//! asked for a resync of two identical worlds. See `docs/PROTOCOL.md`, "Action verdicts".

use std::collections::{BTreeMap, VecDeque};

/// How many actions the book keeps open at once (one whose verdicts never all arrive, a game that left, is dropped oldest first).
pub const VERDICTS_KEPT: usize = 256;

/// One game's verdict of one action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub applied: bool,
    /// Why it was refused (empty for an applied one).
    pub detail: String,
}

/// What a new verdict tells about its action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictOutcome {
    /// Not every expected game has answered yet, and those that did agree.
    Waiting,
    /// Every expected game answered, all the same way: nothing to repair.
    Agreed { applied: bool },
    /// Two games answered differently: their worlds forked at this action. Said once per action, as soon as it is known.
    Differ { applied: Vec<u16>, refused: Vec<(u16, String)> },
}

/// The verdicts of the actions not yet answered by every game, keyed by `(originator, seq)`.
#[derive(Debug, Default)]
pub struct VerdictBook {
    open: BTreeMap<(u16, i64), BTreeMap<u16, Verdict>>,
    order: VecDeque<(u16, i64)>,
}

impl VerdictBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `game`'s verdict of `action`. `expected` are the games whose verdicts close the action (those with a world).
    /// A later verdict of the same game replaces its earlier one.
    pub fn record(&mut self, action: (u16, i64), game: u16, verdict: Verdict, expected: &[u16]) -> VerdictOutcome {
        if !self.open.contains_key(&action) {
            self.order.push_back(action);
            while self.order.len() > VERDICTS_KEPT {
                if let Some(old) = self.order.pop_front() {
                    self.open.remove(&old);
                }
            }
        }
        let by = self.open.entry(action).or_default();
        by.insert(game, verdict);
        let applied = by.values().filter(|v| v.applied).count();
        // An applied verdict's detail is its result where the mod has one to compare (a terrain height edit's proof of the heights after
        // it): two games that both applied an action with different results have forked as much as one that refused it.
        let mut results = by.values().filter(|v| v.applied).map(|v| &v.detail);
        let first = results.next();
        let results_differ = first.is_some_and(|f| results.any(|d| d != f));
        let differ = (applied > 0 && applied < by.len()) || results_differ;
        let complete = expected.iter().all(|g| by.contains_key(g));
        if !differ && !complete {
            return VerdictOutcome::Waiting;
        }
        let by = self.close(action);
        if differ {
            VerdictOutcome::Differ {
                applied: by.iter().filter(|(_, v)| v.applied).map(|(g, _)| *g).collect(),
                refused: by.into_iter().filter(|(_, v)| !v.applied).map(|(g, v)| (g, v.detail)).collect(),
            }
        } else {
            VerdictOutcome::Agreed { applied: applied > 0 }
        }
    }

    /// Forgets every open action (a resync gives every game the same world again).
    pub fn clear(&mut self) {
        self.open.clear();
        self.order.clear();
    }

    /// How many actions wait for verdicts.
    pub fn len(&self) -> usize {
        self.open.len()
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    fn close(&mut self, action: (u16, i64)) -> BTreeMap<u16, Verdict> {
        self.order.retain(|a| *a != action);
        self.open.remove(&action).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied() -> Verdict {
        Verdict { applied: true, detail: String::new() }
    }

    fn refused(why: &str) -> Verdict {
        Verdict { applied: false, detail: why.to_owned() }
    }

    #[test]
    fn every_game_refusing_alike_closes_the_action_without_a_difference() {
        let mut b = VerdictBook::new();
        assert_eq!(VerdictOutcome::Waiting, b.record((1, 12), 1, refused("gone"), &[0, 1]));
        assert_eq!(VerdictOutcome::Agreed { applied: false }, b.record((1, 12), 0, refused("gone"), &[0, 1]));
        assert!(b.is_empty());
    }

    #[test]
    fn one_game_applying_what_another_refused_is_a_fork_at_once() {
        let mut b = VerdictBook::new();
        assert_eq!(VerdictOutcome::Waiting, b.record((1, 3), 1, applied(), &[0, 1, 2]));
        let out = b.record((1, 3), 0, refused("gone"), &[0, 1, 2]);
        assert_eq!(VerdictOutcome::Differ { applied: vec![1], refused: vec![(0, "gone".to_owned())] }, out);
        assert!(b.is_empty(), "said once per action");
    }

    #[test]
    fn two_games_that_applied_with_different_results_have_forked() {
        let mut b = VerdictBook::new();
        let with = |d: &str| Verdict { applied: true, detail: d.to_owned() };
        b.record((0, 1), 0, with("terrain heights 225:1a"), &[0, 1]);
        assert_eq!(
            VerdictOutcome::Agreed { applied: true },
            b.record((0, 1), 1, with("terrain heights 225:1a"), &[0, 1])
        );
        b.record((0, 2), 0, with("terrain heights 225:1a"), &[0, 1]);
        assert_eq!(
            VerdictOutcome::Differ { applied: vec![0, 1], refused: vec![] },
            b.record((0, 2), 1, with("terrain heights 225:2b"), &[0, 1])
        );
    }

    #[test]
    fn all_applied_closes_quietly() {
        let mut b = VerdictBook::new();
        b.record((0, 1), 0, applied(), &[0, 1]);
        assert_eq!(VerdictOutcome::Agreed { applied: true }, b.record((0, 1), 1, applied(), &[0, 1]));
    }

    #[test]
    fn a_later_verdict_of_the_same_game_replaces_its_first() {
        let mut b = VerdictBook::new();
        b.record((0, 1), 0, applied(), &[0, 1, 2]);
        b.record((0, 1), 0, refused("module"), &[0, 1, 2]);
        assert_eq!(VerdictOutcome::Waiting, b.record((0, 1), 1, refused("module"), &[0, 1, 2]));
    }

    #[test]
    fn the_book_keeps_a_bounded_number_of_open_actions() {
        let mut b = VerdictBook::new();
        for seq in 0..(VERDICTS_KEPT as i64 + 10) {
            b.record((0, seq), 0, applied(), &[0, 1]);
        }
        assert_eq!(VERDICTS_KEPT, b.len());
    }
}
