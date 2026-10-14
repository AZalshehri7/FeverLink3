//! TEST-ONLY switches that take the stamp margin of builds away, to measure what it buys: `FEVERLINK3_TEST_NO_MARGIN` and
//! `FEVERLINK3_TEST_NO_LEAD`. The game's mod reads them (feverlink3_lockstep.lua `L.testMargin`) and acts on them; the session only
//! says in its log that they are set, as it does for [`crate::netdelay`], so a run's log shows it was a test.
//!
//! `FEVERLINK3_TEST_NO_MARGIN=1`: the measured extra of a build's stamp is 0 (no measured need, no headroom, no growth after a late
//! apply); `=extra:N`: a fixed extra of N steps instead. `FEVERLINK3_TEST_NO_LEAD=1`: the host's lead goes too, a build is stamped
//! one step past the fastest known clock. Unset (or any other value): nothing changes.

/// The margin switch.
pub const NO_MARGIN: &str = "FEVERLINK3_TEST_NO_MARGIN";
/// The lead switch (stage 2).
pub const NO_LEAD: &str = "FEVERLINK3_TEST_NO_LEAD";
/// The largest extra the mod accepts (L.MARGIN_MAX).
const MAX_EXTRA: u32 = 24;

/// The TEST line for these values of the two switches, as the mod reads them; `None` when neither is set.
pub fn note(no_margin: Option<&str>, no_lead: Option<&str>) -> Option<String> {
    let v = no_margin.map(str::trim).unwrap_or("");
    let mut extra = if v == "1" {
        Some(0)
    } else {
        v.strip_prefix("extra:").and_then(|n| n.parse::<u32>().ok()).map(|n| n.min(MAX_EXTRA))
    };
    let no_lead = no_lead.map(str::trim) == Some("1");
    if no_lead && extra.is_none() {
        extra = Some(0);
    }
    let extra = extra?;
    let lead = if no_lead { "no host lead (one step past the fastest clock)" } else { "the host's lead kept" };
    Some(format!(
        "TEST: {NO_MARGIN} / {NO_LEAD}: builds are stamped with a fixed extra of {extra} step(s), not the measured margin; {lead}"
    ))
}

/// The TEST line for this process's environment.
pub fn note_from_env() -> Option<String> {
    note(std::env::var(NO_MARGIN).ok().as_deref(), std::env::var(NO_LEAD).ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_other_values_say_nothing() {
        assert_eq!(None, note(None, None));
        assert_eq!(None, note(Some("0"), None));
        assert_eq!(None, note(Some(""), Some("0")));
        assert_eq!(None, note(Some("extra:x"), None));
    }

    #[test]
    fn the_line_names_the_extra_and_the_lead() {
        let zero = note(Some("1"), None).unwrap();
        assert!(
            zero.starts_with("TEST: ") && zero.contains("extra of 0 step(s)") && zero.contains("lead kept"),
            "{zero}"
        );
        assert!(note(Some("extra:3"), None).unwrap().contains("extra of 3 step(s)"));
        assert!(note(Some("extra:99"), None).unwrap().contains("extra of 24 step(s)"));
        let nolead = note(None, Some("1")).unwrap();
        assert!(nolead.contains("extra of 0 step(s)") && nolead.contains("no host lead"), "{nolead}");
    }
}
