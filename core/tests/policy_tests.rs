use std::collections::BTreeMap;

use feverlink3_core::session::*;

fn outcome(parts: &[&str]) -> ChecksumOutcome {
    let differences = parts
        .iter()
        .map(|p| ChecksumDifference {
            part: (*p).to_owned(),
            values: BTreeMap::from([(0, Some("a".to_owned())), (1, Some("b".to_owned()))]),
        })
        .collect();
    ChecksumOutcome { n: 1, time: 0, reporters: vec![0, 1], differences }
}

fn policy() -> ChecksumPolicy {
    ChecksumPolicy::new(ChecksumPolicyConfig::default())
}

#[test]
fn identical_outcomes_never_request() {
    let mut p = policy();
    for t in 0..100 {
        assert_eq!(None, p.on_outcome(f64::from(t) * 10.0, &outcome(&[])));
    }
}

#[test]
fn money_is_never_a_reason_even_forever() {
    let mut p = policy();
    for t in 0..200 {
        assert_eq!(None, p.on_outcome(f64::from(t) * 10.0, &outcome(&["money"])));
    }
}

#[test]
fn money_does_not_hide_other_differences() {
    let mut p = policy();
    assert_eq!(None, p.on_outcome(0.0, &outcome(&["money", "roads"])));
    assert!(p.on_outcome(10.0, &outcome(&["roads", "money"])).is_some());
}

#[test]
fn drift_parts_are_the_named_ones_and_script_prefixed_ones() {
    for part in ["vehicles", "onboard", "persons", "stocks", "nodeConfigs", "script:trains", "script:"] {
        assert!(ChecksumPolicy::is_drift_part(part), "{part}");
    }
    for part in ["roads", "lines", "money", "scripts", "Vehicles", "constructions"] {
        assert!(!ChecksumPolicy::is_drift_part(part), "{part}");
    }
}

#[test]
fn drift_reloads_only_after_300_seconds_of_continuous_drift() {
    let mut p = policy();
    assert_eq!(None, p.on_outcome(1000.0, &outcome(&["persons"])));
    assert_eq!(None, p.on_outcome(1150.0, &outcome(&["persons"])));
    assert_eq!(None, p.on_outcome(1299.9, &outcome(&["persons"])));
    let reason = p.on_outcome(1300.0, &outcome(&["persons"])).expect("300 s reached");
    assert!(reason.contains("persons"), "{reason}");
}

#[test]
fn drift_that_clears_starts_over() {
    let mut p = policy();
    p.on_outcome(0.0, &outcome(&["vehicles"]));
    p.on_outcome(200.0, &outcome(&[]));
    assert_eq!(None, p.on_outcome(400.0, &outcome(&["vehicles"])));
    assert_eq!(None, p.on_outcome(699.0, &outcome(&["vehicles"])));
    assert!(p.on_outcome(700.0, &outcome(&["vehicles"])).is_some());
}

#[test]
fn script_parts_drift_like_the_others() {
    let mut p = policy();
    p.on_outcome(0.0, &outcome(&["script:buses"]));
    assert!(p.on_outcome(300.0, &outcome(&["script:buses", "stocks"])).is_some());
}

#[test]
fn other_parts_reload_on_the_second_consecutive_round_with_the_same_set() {
    let mut p = policy();
    assert_eq!(None, p.on_outcome(0.0, &outcome(&["roads", "lines"])));
    let reason = p.on_outcome(10.0, &outcome(&["lines", "roads"])).expect("second round");
    assert!(reason.contains("lines, roads"), "{reason}");
}

#[test]
fn a_changed_set_or_a_clean_round_resets_the_streak() {
    let mut p = policy();
    p.on_outcome(0.0, &outcome(&["roads"]));
    assert_eq!(None, p.on_outcome(10.0, &outcome(&["roads", "lines"])));
    assert_eq!(None, p.on_outcome(20.0, &outcome(&[])));
    assert_eq!(None, p.on_outcome(30.0, &outcome(&["roads", "lines"])));
    assert!(p.on_outcome(40.0, &outcome(&["roads", "lines"])).is_some());
}

#[test]
fn the_decision_resets_the_counters() {
    let mut p = policy();
    p.on_outcome(0.0, &outcome(&["roads"]));
    assert!(p.on_outcome(10.0, &outcome(&["roads"])).is_some());
    assert_eq!(None, p.on_outcome(20.0, &outcome(&["roads"])));
    assert!(p.on_outcome(30.0, &outcome(&["roads"])).is_some());
}

#[test]
fn cooldown_of_120_seconds_after_a_resync_holds_the_decision_back() {
    let mut p = policy();
    p.resync_finished(1000.0);
    assert!(p.in_cooldown(1119.0));
    assert!(!p.in_cooldown(1120.0));
    p.on_outcome(1010.0, &outcome(&["roads"]));
    assert_eq!(None, p.on_outcome(1020.0, &outcome(&["roads"])), "cooldown");
    // the evidence is kept: the next differing round after the cooldown decides at once
    assert!(p.on_outcome(1130.0, &outcome(&["roads"])).is_some());
}

#[test]
fn replay_failure_asks_at_once_unless_cooling_down() {
    let mut p = policy();
    assert_eq!(Some("replay failed".to_owned()), p.on_replay_failed(5.0));
    p.resync_finished(10.0);
    assert_eq!(None, p.on_replay_failed(129.0));
    assert!(p.on_replay_failed(130.0).is_some());
}

#[test]
fn a_finished_resync_forgets_the_counted_evidence() {
    let mut p = policy();
    p.on_outcome(0.0, &outcome(&["roads"]));
    p.resync_finished(5.0);
    assert_eq!(None, p.on_outcome(200.0, &outcome(&["roads"])), "first round after the resync");
}

#[test]
fn thresholds_are_configurable() {
    let config = ChecksumPolicyConfig { drift_seconds: 10.0, consecutive_rounds: 3, cooldown_seconds: 0.0 };
    let mut p = ChecksumPolicy::new(config);
    p.on_outcome(0.0, &outcome(&["roads"]));
    assert_eq!(None, p.on_outcome(1.0, &outcome(&["roads"])));
    assert!(p.on_outcome(2.0, &outcome(&["roads"])).is_some());
    p.on_outcome(100.0, &outcome(&["stocks"]));
    assert!(p.on_outcome(110.0, &outcome(&["stocks"])).is_some());
}
