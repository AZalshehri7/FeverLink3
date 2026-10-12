//! The text of the results file (UTF-8, plain English).

use super::suite::TesterReport;
use super::{SPACEWAR_APP_ID, VERSION};

fn id_text(id: Option<u64>) -> String {
    id.map_or_else(|| "unknown".to_string(), |i| i.to_string())
}

/// The whole results file for `report`, written at `when` (a UTC text like `2026-10-11 12:03:22 UTC`).
pub fn results_text(report: &TesterReport, when: &str, app_id: u32) -> String {
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push_str("\r\n");
    };
    line("FeverLink3 Steam speed test - results".into());
    line(format!("Date: {when}"));
    line(format!("Program version: {VERSION}"));
    line(format!("Steam app id: {app_id}{}", if app_id == SPACEWAR_APP_ID { " (Spacewar test app)" } else { "" }));
    line(format!("Own Steam ID: {}", id_text(report.own_id)));
    line(format!("Peer Steam ID: {}", report.peer));
    if report.connected {
        line(format!("Route: {}", if report.route.is_empty() { "unknown" } else { &report.route }));
        line(format!(
            "Ping (Steam's round trip): {}",
            report.ping_ms.map_or_else(|| "unknown".into(), |p| format!("{p} ms"))
        ));
    } else {
        line("Route: not connected".into());
    }
    let state = if report.aborted {
        "stopped by the player (Ctrl+C) before the end"
    } else if let Some(error) = &report.error {
        error.as_str()
    } else if report.finished {
        "all tests ran"
    } else {
        "not finished"
    };
    line(format!("State: {state}"));
    for outcome in &report.outcomes {
        line(String::new());
        line(format!("=== {} ===", outcome.name));
        if outcome.ok {
            for l in &outcome.lines {
                line(l.clone());
            }
        } else {
            line(format!("FAILED: {}", outcome.lines.join(" ")));
        }
    }
    if !report.dumps.is_empty() {
        line(String::new());
        line("=== Steam status ===".into());
        for dump in &report.dumps {
            for l in dump.lines() {
                line(l.to_string());
            }
        }
    }
    out
}

/// One line of the responder's log file.
pub fn log_line(when: &str, text: &str) -> String {
    format!("{when}  {text}\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speedtest::suite::Outcome;

    fn report() -> TesterReport {
        TesterReport {
            own_id: Some(76561190000000001),
            peer: 76561190000000002,
            route: "direct".into(),
            ping_ms: Some(31),
            connected: true,
            outcomes: vec![
                Outcome {
                    name: "Reliable stream, 8 MB each way".into(),
                    ok: true,
                    lines: vec!["l1".into(), "l2".into()],
                },
                Outcome {
                    name: "Reliable stream, 32 MB each way".into(),
                    ok: false,
                    lines: vec!["no data for 60 s".into()],
                },
            ],
            finished: true,
            ..TesterReport::default()
        }
    }

    #[test]
    fn the_results_file_has_everything_the_maintainer_needs() {
        let text = results_text(&report(), "2026-10-11 12:03:22 UTC", 480);
        for needle in [
            "Date: 2026-10-11 12:03:22 UTC",
            "Program version: ",
            "Steam app id: 480 (Spacewar test app)",
            "Own Steam ID: 76561190000000001",
            "Peer Steam ID: 76561190000000002",
            "Route: direct",
            "Ping (Steam's round trip): 31 ms",
            "State: all tests ran",
            "=== Reliable stream, 8 MB each way ===\r\nl1\r\nl2",
            "=== Reliable stream, 32 MB each way ===\r\nFAILED: no data for 60 s",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
    }

    #[test]
    fn an_early_end_is_stated() {
        let mut r = TesterReport {
            peer: 76561190000000002,
            error: Some("The other PC did not answer".into()),
            ..TesterReport::default()
        };
        let text = results_text(&r, "t", 480);
        assert!(text.contains("Route: not connected") && text.contains("State: The other PC did not answer"), "{text}");
        r.error = None;
        r.aborted = true;
        assert!(results_text(&r, "t", 480).contains("stopped by the player"));
    }

    #[test]
    fn dumps_are_included_when_there_are_any() {
        let mut r = report();
        r.dumps.push("status line a\nstatus line b".into());
        let text = results_text(&r, "t", 480);
        assert!(text.contains("=== Steam status ===\r\nstatus line a\r\nstatus line b"), "{text}");
    }
}
