//! The `doctor` text and the diagnostics text of menu choice 6: logs, the date, and the redaction of the Windows
//! user name and of join codes.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use feverlink3_core::lobby::JoinCode;
use feverlink3_launcher::install::AppIdState;

use super::facts::Facts;
use super::screen::{checks, license_line, render_checks};
use super::versions::own_version;

/// The log of the native module, in `%LOCALAPPDATA%\FeverLink3`.
const NATIVE_LOG: &str = "native.log";
/// The mod's log, in the `live` folder under it (`live::live_dir`).
const LIVE_SUBDIR: &str = "live";
const MOD_LOG: &str = "mod.log";
const LOG_TAIL_LINES: usize = 40;
/// How much of the end of a log is read: enough for 40 ordinary lines, bounded for a log that grew for months.
const LOG_TAIL_BYTES: u64 = 64 * 1024;
const LOG_LINE_CHARS: usize = 300;
/// Fewest base32 characters a join code can have (26 bytes: version, time, secret, endpoint count, check).
const MIN_JOIN_CODE_CHARS: usize = 42;

fn path_text(p: &Path) -> String {
    p.display().to_string()
}

/// What `feverlink3 doctor` prints: what this PC has of Steam, the game and FeverLink3, one fact per line, in the
/// format the command has always used.
pub(super) fn doctor_text(f: &Facts) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!(
        "Steam:           {}",
        f.steam_path.as_deref().map_or("not found in the registry".into(), path_text)
    ));
    lines.extend(f.libraries.iter().map(|lib| format!("library:         {}", lib.display())));
    lines.push(format!(
        "game:            {}",
        f.game_dir.as_deref().map_or("TransportFever3.exe not found in any library".into(), path_text)
    ));
    if f.users.is_empty() {
        lines.push(
            "user folders:    none (userdata/<id>/3493540/local does not exist yet; the game creates it on first run)"
                .to_owned(),
        );
    }
    for user in &f.users {
        let state = if user.mod_installed { "mod installed" } else { "mod not installed" };
        lines.push(format!("user folder:     {}  ({state})", user.path.display()));
    }
    if let Some(files) = &f.game_files {
        lines.push(match &files.dll {
            Ok(state) => format!("winhttp.dll:     {state}"),
            Err(e) => format!("winhttp.dll:     unreadable ({e})"),
        });
        lines.push(match &files.appid {
            Ok(AppIdState::Absent) => "steam_appid.txt: absent".to_owned(),
            Ok(AppIdState::Ours) => "steam_appid.txt: present (3493540)".to_owned(),
            Ok(AppIdState::Other(c)) => format!("steam_appid.txt: present with other content {c:?}"),
            Err(e) => format!("steam_appid.txt: unreadable ({e})"),
        });
        lines.push(match &files.launcher {
            Some(p) => format!("launcher path:   {}", p.display()),
            None => "launcher path:   not recorded (feverlink3_path.txt absent)".to_owned(),
        });
    }
    lines.push(format!(
        "payload mod:     {}",
        f.payload_mod.as_deref().map_or("not found next to this executable".into(), path_text)
    ));
    lines.push(format!(
        "payload dll:     {}",
        f.payload_dll.as_deref().map_or("not found next to this executable".into(), path_text)
    ));
    lines.push(format!("game running:    {}", if f.game_running { "yes" } else { "no" }));
    lines.join("\n") + "\n"
}

// ---------------------------------------------------------------------------------------------------------------
// Diagnostics text
// ---------------------------------------------------------------------------------------------------------------

/// The end of a log file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LogTail {
    Lines(Vec<String>),
    Missing,
    Unreadable(String),
}

/// The last `count` lines of `text`, each cut to `max_chars` characters (a log line can be a whole serialised
/// world; a diagnostics text people paste into a chat must stay readable).
fn tail_lines(text: &str, count: usize, max_chars: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let first = lines.len().saturating_sub(count);
    lines
        .get(first..)
        .unwrap_or_default()
        .iter()
        .map(|line| match line.char_indices().nth(max_chars) {
            Some((cut, _)) => format!("{} ... (line cut)", line.get(..cut).unwrap_or(line)),
            None => (*line).to_owned(),
        })
        .collect()
}

/// The last `max_bytes` of a file as text. A read that starts inside the file starts inside a line too, so the
/// fragment before the first line break is dropped.
fn read_tail(path: &Path, max_bytes: u64) -> io::Result<String> {
    let mut file = File::open(path)?;
    let start = file.metadata()?.len().saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(if start > 0 { text.split_once('\n').map_or_else(String::new, |(_, rest)| rest.to_owned()) } else { text })
}

fn read_log_tail(path: &Path) -> LogTail {
    match read_tail(path, LOG_TAIL_BYTES) {
        Ok(text) => LogTail::Lines(tail_lines(&text, LOG_TAIL_LINES, LOG_LINE_CHARS)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => LogTail::Missing,
        Err(e) => LogTail::Unreadable(e.to_string()),
    }
}

/// The ends of the two logs the diagnostics hold, given `%LOCALAPPDATA%\FeverLink3` (`None`: not known).
pub(super) fn read_logs(local_dir: Option<&Path>) -> (LogTail, LogTail) {
    match local_dir {
        Some(dir) => {
            // mod.log goes on in mod.2.log, mod.3.log ... (the mod's C.logger, `mod.gen` names the newest)
            let mod_log = dir.join(LIVE_SUBDIR).join(MOD_LOG);
            let newest = crate::gamelink::generation_path(&mod_log, crate::gamelink::newest_generation(&mod_log));
            (read_log_tail(&dir.join(NATIVE_LOG)), read_log_tail(&newest))
        }
        None => (LogTail::Missing, LogTail::Missing),
    }
}

/// `2026-10-10 12:34:56 UTC` from unix seconds (the civil-from-days algorithm of Howard Hinnant; no date crate for
/// one line of a diagnostics text).
fn format_utc(unix: u64) -> String {
    let secs = i64::try_from(unix).unwrap_or(i64::MAX / 4);
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC", rest / 3_600, rest % 3_600 / 60, rest % 60)
}

fn log_section(title: &str, tail: &LogTail) -> Vec<String> {
    match tail {
        LogTail::Lines(lines) if lines.is_empty() => vec![format!("== {title} =="), "(empty)".to_owned()],
        LogTail::Lines(lines) => {
            let mut out = vec![format!("== {title} (last {} lines) ==", lines.len())];
            out.extend(lines.iter().cloned());
            out
        }
        LogTail::Missing => vec![
            format!("== {title} =="),
            "(not there yet: the game creates it the first time it starts with FeverLink3 installed)".to_owned(),
        ],
        LogTail::Unreadable(why) => vec![format!("== {title} =="), format!("(could not be read: {why})")],
    }
}

/// The diagnostics text of menu choice 6: the checks, the doctor output and the end of the two logs, with the
/// Windows user name and any join code taken out. `%LOCALAPPDATA%` stays unexpanded in the titles.
pub(super) fn build_diagnostics(
    f: &Facts,
    now_unix: u64,
    native: &LogTail,
    mod_log: &LogTail,
    user_names: &[String],
) -> String {
    let mut lines = vec![
        format!("FeverLink3 diagnostics - launcher {}, {}", own_version(), format_utc(now_unix)),
        license_line(),
        String::new(),
        "== Checks ==".to_owned(),
    ];
    lines.extend(render_checks(&checks(f)));
    lines.push(String::new());
    lines.push("== feverlink3 doctor ==".to_owned());
    lines.extend(doctor_text(f).lines().map(str::to_owned));
    lines.push(String::new());
    lines.extend(log_section("%LOCALAPPDATA%\\FeverLink3\\native.log", native));
    lines.push(String::new());
    lines.extend(log_section("%LOCALAPPDATA%\\FeverLink3\\live\\mod.log", mod_log));
    redact_join_codes(&redact_user(&(lines.join("\n") + "\n"), user_names))
}

// ---------------------------------------------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------------------------------------------

/// A name to take out of a text: the name itself, or its 8.3 short form (`ABDULA~1`), which Windows shows for
/// long user folder names in `%TEMP%` and other environment variables.
struct UserPattern {
    name: Vec<char>,
    short_form: bool,
}

impl UserPattern {
    /// The length in characters of the match at `at`, when there is one that starts and ends on a word boundary
    /// (so that `Ann` is found in `C:\Users\Ann\Desktop` but not in `Annual`).
    fn match_len(&self, chars: &[char], at: usize) -> Option<usize> {
        let rest = chars.get(at..)?;
        let head = rest.get(..self.name.len())?;
        if !head.iter().zip(&self.name).all(|(a, b)| same_letter(*a, *b)) {
            return None;
        }
        let mut len = self.name.len();
        if self.short_form {
            if rest.get(len) != Some(&'~') {
                return None;
            }
            len += 1;
            let digits = rest.get(len..)?.iter().take_while(|c| c.is_ascii_digit()).count();
            if digits == 0 {
                return None;
            }
            len += digits;
        }
        rest.get(len).is_none_or(|c| !c.is_alphanumeric()).then_some(len)
    }
}

/// Equal ignoring case, letter by letter: Windows paths are case-insensitive and the registry shows them in
/// lower case. Comparing per character keeps the lengths aligned for every alphabet.
fn same_letter(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// The patterns for the names, longest first so that a long name wins over a short one inside it.
fn user_patterns(names: &[String]) -> Vec<UserPattern> {
    let mut patterns: Vec<UserPattern> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for raw in names {
        let name = raw.trim();
        if name.is_empty() || seen.contains(&name.to_lowercase()) {
            continue;
        }
        seen.push(name.to_lowercase());
        patterns.push(UserPattern { name: name.chars().collect(), short_form: false });
        let short: Vec<char> = name.chars().filter(char::is_ascii_alphanumeric).take(6).collect();
        if short.len() == 6 {
            patterns.push(UserPattern { name: short, short_form: true });
        }
    }
    patterns.sort_by_key(|p| std::cmp::Reverse(p.name.len()));
    patterns
}

/// `text` with the Windows user name (and its short form) replaced by `<user>`, case-insensitively.
fn redact_user(text: &str, names: &[String]) -> String {
    let patterns = user_patterns(names);
    if patterns.is_empty() {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(&c) = chars.get(at) {
        let starts_word = at == 0 || chars.get(at - 1).is_none_or(|p| !p.is_alphanumeric());
        match starts_word.then(|| patterns.iter().find_map(|p| p.match_len(&chars, at))).flatten() {
            Some(len) => {
                out.push_str("<user>");
                at += len;
            }
            None => {
                out.push(c);
                at += 1;
            }
        }
    }
    out
}

/// `text` with every join code taken out. A join code is the credential of a session (whoever has it can join), and
/// a diagnostics text is made to be pasted into chats. Candidates are runs of base32 letters; only a run that
/// decodes as a join code (checksum included) is replaced, so ordinary words and hashes stay.
fn redact_join_codes(text: &str) -> String {
    fn flush(out: &mut String, run: &mut String) {
        let letters = run.chars().filter(|c| *c != '-').count();
        if letters >= MIN_JOIN_CODE_CHARS && JoinCode::try_decode(run).is_some() {
            out.push_str("<join code removed>");
        } else {
            out.push_str(run);
        }
        run.clear();
    }
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for c in text.chars() {
        if c.is_ascii_alphabetic() || ('2'..='7').contains(&c) || c == '-' {
            run.push(c);
        } else {
            flush(&mut out, &mut run);
            out.push(c);
        }
    }
    flush(&mut out, &mut run);
    out
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use feverlink3_core::crypto::SessionKey;
    use feverlink3_launcher::install::DllState;
    use tempfile::TempDir;

    use super::*;
    use crate::wizard::testkit::*;

    #[test]
    fn the_doctor_text_keeps_the_format_the_command_always_had() {
        let expected = [
            r"Steam:           C:\Steam",
            r"library:         C:\Steam",
            r"game:            C:\Steam\steamapps\common\Transport Fever 3",
            r"user folder:     C:\Steam\userdata\1\3493540\local  (mod not installed)",
            "winhttp.dll:     absent",
            "steam_appid.txt: absent",
            "launcher path:   not recorded (feverlink3_path.txt absent)",
            r"payload mod:     C:\FeverLink3-0.1.0\mod\feverlink3_1",
            r"payload dll:     C:\FeverLink3-0.1.0\winhttp.dll",
            "game running:    no",
        ]
        .join("\n")
            + "\n";
        assert_eq!(doctor_text(&base_facts()), expected);

        let mut installed = installed_facts();
        installed.game_running = true;
        let text = doctor_text(&installed);
        assert!(text.contains(r"user folder:     C:\Steam\userdata\1\3493540\local  (mod installed)"));
        assert!(text.contains("winhttp.dll:     ours\n"));
        assert!(text.contains("steam_appid.txt: present (3493540)\n"));
        assert!(text.contains(r"launcher path:   C:\FeverLink3-0.1.0\feverlink3.exe"));
        assert!(text.ends_with("game running:    yes\n"));
    }

    #[test]
    fn the_doctor_text_of_a_pc_without_steam_or_game() {
        let mut f = base_facts();
        f.steam_path = None;
        f.libraries.clear();
        f.game_dir = None;
        f.game_files = None;
        f.users.clear();
        f.payload_mod = None;
        f.payload_dll = None;
        let expected = [
            "Steam:           not found in the registry",
            "game:            TransportFever3.exe not found in any library",
            "user folders:    none (userdata/<id>/3493540/local does not exist yet; the game creates it on first run)",
            "payload mod:     not found next to this executable",
            "payload dll:     not found next to this executable",
            "game running:    no",
        ]
        .join("\n")
            + "\n";
        assert_eq!(doctor_text(&f), expected);
    }

    #[test]
    fn the_doctor_text_reports_unreadable_files_and_foreign_content() {
        let mut f = base_facts();
        let files = f.game_files.as_mut().unwrap();
        files.dll = Err("Access is denied. (os error 5)".into());
        files.appid = Ok(AppIdState::Other("480".into()));
        let text = doctor_text(&f);
        assert!(text.contains("winhttp.dll:     unreadable (Access is denied. (os error 5))\n"), "{text}");
        assert!(text.contains("steam_appid.txt: present with other content \"480\"\n"), "{text}");
        f.game_files.as_mut().unwrap().appid = Err("nope".into());
        assert!(doctor_text(&f).contains("steam_appid.txt: unreadable (nope)\n"));
        f.game_files.as_mut().unwrap().dll = Ok(DllState::Foreign);
        assert!(doctor_text(&f).contains("winhttp.dll:     foreign\n"));
    }

    // -----------------------------------------------------------------------------------------------------------
    // Diagnostics text
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn the_last_lines_of_a_log_are_kept_and_long_lines_are_cut() {
        let text = "one\r\ntwo\nthree\nfour\n";
        assert_eq!(tail_lines(text, 2, 100), ["three", "four"]);
        assert_eq!(tail_lines(text, 10, 100), ["one", "two", "three", "four"]);
        assert_eq!(tail_lines(text, 0, 100), Vec::<String>::new());
        assert_eq!(tail_lines("", 5, 100), Vec::<String>::new());
        assert_eq!(tail_lines("abcdefghij", 5, 4), ["abcd ... (line cut)"]);
        assert_eq!(tail_lines("abcd", 5, 4), ["abcd"]);
        // cut on characters, not on bytes
        assert_eq!(tail_lines("\u{0639}\u{0639}\u{0639}\u{0639}\u{0639}", 1, 2), ["\u{0639}\u{0639} ... (line cut)"]);
    }

    #[test]
    fn a_log_is_read_from_its_end() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("native.log");
        let lines: Vec<String> = (1..=200).map(|n| format!("line number {n:03}")).collect();
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        // the whole file fits
        assert_eq!(read_tail(&path, 1 << 20).unwrap().lines().count(), 200);
        // a small window starts inside a line: that fragment is dropped, the rest is whole lines
        let tail = read_tail(&path, 100).unwrap();
        assert!(tail.lines().all(|l| l.starts_with("line number ") && l.len() == 15), "{tail:?}");
        assert!(tail.ends_with("line number 200\n"));
        assert!(tail.lines().count() >= 5);
        assert_eq!(read_tail(&dir.path().join("none.log"), 100).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_missing_empty_or_unreadable_log_is_described_not_hidden() {
        let dir = TempDir::new().unwrap();
        assert_eq!(read_log_tail(&dir.path().join("none.log")), LogTail::Missing);
        fs::write(dir.path().join("empty.log"), "").unwrap();
        assert_eq!(read_log_tail(&dir.path().join("empty.log")), LogTail::Lines(Vec::new()));
        fs::write(dir.path().join("a.log"), "first\nsecond\n").unwrap();
        assert_eq!(read_log_tail(&dir.path().join("a.log")), LogTail::Lines(vec!["first".into(), "second".into()]));
        // a folder where the file should be cannot be read as a file
        assert!(matches!(read_log_tail(dir.path()), LogTail::Unreadable(_)));
        assert!(log_section("x.log", &LogTail::Missing).join("\n").contains("not there yet"));
        assert!(log_section("x.log", &LogTail::Lines(vec![])).join("\n").contains("(empty)"));
        assert!(log_section("x.log", &LogTail::Unreadable("denied".into()))
            .join("\n")
            .contains("could not be read: denied"));
        assert_eq!(
            log_section("x.log", &LogTail::Lines(vec!["a".into(), "b".into()])),
            ["== x.log (last 2 lines) ==", "a", "b"]
        );
    }

    #[test]
    fn unix_time_is_shown_as_a_utc_date() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00:00 UTC", "a leap day");
        assert_eq!(format_utc(1_000_000_000), "2001-09-09 01:46:40 UTC");
        assert_eq!(format_utc(1_234_567_890), "2009-02-13 23:31:30 UTC");
        assert_eq!(format_utc(1_791_633_296), "2026-10-10 11:54:56 UTC");
        assert_eq!(format_utc(4_102_444_800), "2100-01-01 00:00:00 UTC");
        assert!(format_utc(u64::MAX).ends_with(" UTC"), "no overflow");
    }

    #[test]
    fn the_diagnostics_hold_checks_doctor_and_both_logs() {
        let f = installed_facts();
        let native =
            LogTail::Lines(vec!["FeverLink3 native 0.1.0 loaded".into(), "game build 40420: hooks available".into()]);
        let mod_log = LogTail::Missing;
        let text = build_diagnostics(&f, 1_791_633_296, &native, &mod_log, &[]);
        let at = |part: &str| text.find(part).unwrap_or_else(|| panic!("{part:?} is in\n{text}"));
        assert!(text
            .starts_with(&format!("FeverLink3 diagnostics - launcher {}, 2026-10-10 11:54:56 UTC\n", own_version())));
        assert!(at("== Checks ==") < at("== feverlink3 doctor ==") && at("== feverlink3 doctor ==") < at("native.log"));
        assert!(at("native.log") < at("live\\mod.log"));
        assert!(text.lines().nth(1).is_some_and(|l| l.starts_with("License: ")), "the second line names the license");
        assert!(text.contains("[ok]  Steam"));
        assert!(text.contains("winhttp.dll:     ours"));
        assert!(text
            .contains("== %LOCALAPPDATA%\\FeverLink3\\native.log (last 2 lines) ==\nFeverLink3 native 0.1.0 loaded\n"));
        assert!(text.contains("== %LOCALAPPDATA%\\FeverLink3\\live\\mod.log ==\n(not there yet"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn the_user_name_never_reaches_the_diagnostics() {
        let mut f = installed_facts();
        f.exe_path = PathBuf::from(r"C:\Users\Zara Quill\Downloads\FeverLink3-0.1.0\feverlink3.exe");
        f.payload_mod = Some(PathBuf::from(r"c:\users\zara quill\downloads\feverlink3-0.1.0\mod\feverlink3_1"));
        f.libraries = vec![PathBuf::from(r"C:\Users\ZARAQU~1\steam")];
        let log = LogTail::Lines(vec![r"opened C:\Users\Zara Quill\AppData\Local\FeverLink3 as Zara Quill".into()]);
        let names = vec!["Zara Quill".to_owned()];
        let text = build_diagnostics(&f, 0, &log, &log, &names);
        for forbidden in ["Zara", "zara", "ZARA", "Quill"] {
            assert!(!text.contains(forbidden), "{forbidden} is still in\n{text}");
        }
        assert!(text.contains(r"C:\Users\<user>\Downloads\FeverLink3-0.1.0"));
        assert!(text.contains(r"opened C:\Users\<user>\AppData\Local\FeverLink3 as <user>"));
        assert!(text.contains(r"C:\Users\<user>\steam"), "the short form too");
    }

    // -----------------------------------------------------------------------------------------------------------
    // Redaction
    // -----------------------------------------------------------------------------------------------------------

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_user_name_is_replaced_whatever_its_case() {
        let n = names(&["Ann"]);
        assert_eq!(redact_user(r"C:\Users\Ann\Desktop", &n), r"C:\Users\<user>\Desktop");
        assert_eq!(redact_user(r"c:/users/ANN/desktop", &n), r"c:/users/<user>/desktop");
        assert_eq!(redact_user("name=Ann, Ann", &n), "name=<user>, <user>");
        assert_eq!(redact_user("Ann", &n), "<user>");
        assert_eq!(redact_user("", &n), "");
        assert_eq!(redact_user("nothing to see", &n), "nothing to see");
    }

    #[test]
    fn the_name_inside_another_word_is_left_alone() {
        let n = names(&["Ann"]);
        assert_eq!(
            redact_user("Annual report, Joanne, banana, Ann_Smith", &n),
            "Annual report, Joanne, banana, <user>_Smith"
        );
        assert_eq!(redact_user("Ann3", &n), "Ann3");
    }

    #[test]
    fn several_names_and_the_short_form_are_replaced() {
        let n = names(&["ExampleUser", "Abdul", "  ", "", "abdulazizalshehri"]);
        assert_eq!(
            redact_user(r"C:\Users\ABDULA~1\AppData\Local\Temp and ExampleUser and Abdul", &n),
            r"C:\Users\<user>\AppData\Local\Temp and <user> and <user>"
        );
        assert_eq!(redact_user("ABDULA~12 end", &n), "<user> end");
        assert_eq!(redact_user("ABDULA~ end", &n), "ABDULA~ end", "a short form needs its digit");
        assert_eq!(redact_user("ABDULA end", &n), "ABDULA end");
        // the longest name wins where one contains the other
        assert_eq!(redact_user("ExampleUser", &names(&["Abdul", "ExampleUser"])), "<user>");
        // spaces in the name; no short form for a name with fewer than six letters
        assert_eq!(redact_user("John Smith and JOHN SMITH", &names(&["John Smith"])), "<user> and <user>");
        assert_eq!(redact_user("JOHNSM~1 and JOHNSM", &names(&["John Smith"])), "<user> and JOHNSM");
    }

    #[test]
    fn names_in_other_alphabets_are_replaced_too() {
        let n = names(&["\u{0639}\u{0628}\u{062F}\u{0627}\u{0644}\u{0639}\u{0632}\u{064A}\u{0632}", "Zo\u{00EB}"]);
        let text = "C:\\Users\\\u{0639}\u{0628}\u{062F}\u{0627}\u{0644}\u{0639}\u{0632}\u{064A}\u{0632}\\Desktop and ZO\u{00CB}";
        assert_eq!(redact_user(text, &n), "C:\\Users\\<user>\\Desktop and <user>");
    }

    /// A join code made the way the host makes one.
    fn join_code_text() -> String {
        let endpoints = [SocketAddr::from(([192, 168, 1, 5], 28090)), SocketAddr::from(([203, 0, 113, 7], 28090))];
        JoinCode::create(&endpoints, [7u8; SessionKey::SECRET_SIZE], Some(1_700_000_000)).unwrap().encode()
    }

    #[test]
    fn a_join_code_is_taken_out_wherever_it_is() {
        let code = join_code_text();
        assert!(code.len() >= MIN_JOIN_CODE_CHARS, "{code}");
        assert_eq!(redact_join_codes(&format!("code={code} ok")), "code=<join code removed> ok");
        assert_eq!(redact_join_codes(&format!("\"{code}\"")), "\"<join code removed>\"");
        assert_eq!(redact_join_codes(&code.to_lowercase()), "<join code removed>");
        let grouped: Vec<String> = code.as_bytes().chunks(5).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
        assert_eq!(redact_join_codes(&grouped.join("-")), "<join code removed>", "a code shown in groups");
        assert_eq!(redact_join_codes(&format!("{code}\n{code}")), "<join code removed>\n<join code removed>");
        assert_eq!(redact_join_codes(""), "");
    }

    /// What a host makes today: no endpoints at all, the host's Steam ID as an extension of the code.
    #[test]
    fn a_join_code_that_carries_a_steam_id_is_taken_out_too() {
        let code = JoinCode::create(&[], [9u8; SessionKey::SECRET_SIZE], Some(1_700_000_000))
            .unwrap()
            .with_steam_id(76_561_198_000_000_000)
            .encode();
        assert!(JoinCode::try_decode(&code).is_some(), "the fixture is a real code: {code}");
        assert!(code.len() >= MIN_JOIN_CODE_CHARS);
        assert_eq!(redact_join_codes(&format!("host code {code}.")), "host code <join code removed>.");
    }

    #[test]
    fn text_that_only_looks_like_a_join_code_is_kept() {
        let code = join_code_text();
        let mut broken = code.clone();
        broken.replace_range(10..11, if &code[10..11] == "A" { "B" } else { "A" });
        let lookalikes = [
            broken,
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567ABCDEFGHIJKLMNOPQRSTUVWXYZ234567".to_owned(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned(),
            "FeverLink3 native 0.1.0 loaded, game build 40420: hooks available".to_owned(),
            "A-B-C".to_owned(),
        ];
        for text in lookalikes {
            assert_eq!(redact_join_codes(&text), text);
        }
    }

    #[test]
    fn the_diagnostics_never_hold_a_join_code() {
        let code = join_code_text();
        let log = LogTail::Lines(vec![format!("hosting, code {code}"), "player joined".into()]);
        let text = build_diagnostics(&base_facts(), 0, &log, &LogTail::Missing, &[]);
        assert!(!text.contains(&code));
        assert!(text.contains("hosting, code <join code removed>"));
    }
}
