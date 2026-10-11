//! The standalone Steam speed test (`feverlink3-speedtest.exe`): the Steam P2P speed test of the launcher's connection
//! probe, run by a console program that needs only Steam, not Transport Fever 3.
//!
//! - [`cli`]: the command line and the two roles (tester when a peer is known, responder otherwise).
//! - [`startup`]: loads `steam_api64.dll` from the program's folder and initialises Steam (Windows only at run time;
//!   the error texts are plain functions and are tested everywhere).
//! - [`suite`]: the table of test modes and the tester, driven over a [`crate::steamnet::Probe`].
//! - [`responder`]: waits for testers and logs what it answered.
//! - [`results`]: the text of the results file.
//! - [`console`]: the console program around all of it.
//!
//! Everything except `startup::start` and the console calls takes a core, so the tests run it on the fake Steam.

pub mod cli;
pub mod console;
pub mod responder;
pub mod results;
pub mod startup;
pub mod suite;

use std::time::{SystemTime, UNIX_EPOCH};

/// The file name of the tester's results, in the program's folder.
pub const RESULTS_FILE: &str = "speedtest-results.txt";
/// The file name of the responder's log, in the program's folder.
pub const RESPONDER_FILE: &str = "speedtest-responder.txt";
/// The file that names the peer (its presence makes the program the tester).
pub const PEER_FILE: &str = "peer.txt";
/// The file Steam reads for the app id.
pub const APPID_FILE: &str = "steam_appid.txt";
/// Valve's Spacewar test app id; both PCs must use the same one.
pub const SPACEWAR_APP_ID: u32 = 480;
/// The program version, as in the results file.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `2026-10-11 14:03:22 UTC` for a Unix time in seconds.
pub fn utc_text(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Now, as [`utc_text`].
pub fn now_text() -> String {
    utc_text(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_date_text_is_right() {
        assert_eq!(utc_text(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_text(1_791_723_802), "2026-10-11 13:03:22 UTC");
        assert_eq!(utc_text(951_782_400), "2000-02-29 00:00:00 UTC");
    }
}
