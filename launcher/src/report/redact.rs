//! Taking personal data out of text before it leaves the PC: the Windows user name and join codes (the diagnostics text of
//! the launcher's menu and every file of a bug report), and for a bug report also Steam IDs, Steam account numbers and IP
//! addresses ([`Redactor`]). Pure functions over text, so the tests need no game.

use feverlink3_core::lobby::JoinCode;
use sha2::{Digest, Sha256};

/// Fewest base32 characters a join code can have (26 bytes: version, time, secret, endpoint count, check).
pub const MIN_JOIN_CODE_CHARS: usize = 42;

/// A name to take out of a text: the name itself, or its 8.3 short form (`EXAMPL~1`), which Windows shows for
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
pub fn redact_user(text: &str, names: &[String]) -> String {
    redact_user_as(text, names, "<user>")
}

/// As [`redact_user`] with another replacement (a bug report writes `%USER%`, which reads as the folder it stands for).
pub fn redact_user_as(text: &str, names: &[String], with: &str) -> String {
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
                out.push_str(with);
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
pub fn redact_join_codes(text: &str) -> String {
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

/// What stands for a Steam account number (the `userdata/<n>` folder, the game's `userId`).
pub const ACCOUNT_MASK: &str = "%STEAMACCOUNT%";
/// What stands for an IPv4 or IPv6 address that is not loopback.
pub const IPV4_MASK: &str = "<ipv4>";
pub const IPV6_MASK: &str = "<ipv6>";

/// The redaction of a bug report's text files, applied on the PC that wrote them before anything is zipped or sent:
///
/// - join codes (whoever has one can join the session) are removed;
/// - a Steam ID (17 digits, `76561...`) becomes `steam-<8 hex>`, a hash of the ID and the report's random salt: the same ID gets
///   the same tag in every player's part of one report, the next report gives other tags, and the salt is not in the bundle.
///   Each part names its own game's tag, and the host's bundle turns those into `player-N`;
/// - a Steam account number (the `userdata\<n>` folder name, the game's `userId`) becomes [`ACCOUNT_MASK`];
/// - IPv4 and IPv6 addresses are masked, except loopback and the unspecified address, which say nothing about anyone;
/// - the Windows user name (in paths, and anywhere else as a whole word) becomes `%USER%`.
#[derive(Debug, Clone)]
pub struct Redactor {
    user_names: Vec<String>,
    salt: String,
    accounts: Vec<String>,
}

impl Redactor {
    /// `user_names`: the Windows user name and the profile folder's name; `accounts`: this PC's Steam account numbers.
    pub fn new(user_names: Vec<String>, salt: &str, accounts: Vec<String>) -> Redactor {
        let accounts =
            accounts.into_iter().filter(|a| !a.is_empty() && a.bytes().all(|b| b.is_ascii_digit())).collect();
        Redactor { user_names, salt: salt.to_owned(), accounts }
    }

    /// The tag that stands for Steam ID `id` in this report.
    pub fn steam_tag(&self, id: u64) -> String {
        let digest = Sha256::new()
            .chain_update(self.salt.as_bytes())
            .chain_update(b":")
            .chain_update(id.to_string().as_bytes())
            .finalize();
        let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
        format!("steam-{hex}")
    }

    /// `text` with everything personal taken out (the list above). Join codes go first: a code with a replaced piece inside
    /// would no longer be recognised.
    pub fn text(&self, text: &str) -> String {
        let t = redact_join_codes(text);
        let t = self.numbers(&t);
        let t = redact_ipv4(&t);
        let t = redact_ipv6(&t);
        redact_user_as(&t, &self.user_names, "%USER%")
    }

    /// Steam IDs and account numbers: every maximal run of digits is looked at once.
    fn numbers(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(|c: char| c.is_ascii_digit()) {
            let (before, from) = rest.split_at(start);
            out.push_str(before);
            let len = from.find(|c: char| !c.is_ascii_digit()).unwrap_or(from.len());
            let (run, after) = from.split_at(len);
            let prev = out.chars().next_back();
            let next = after.chars().next();
            let alone = prev.is_none_or(|c| !c.is_alphanumeric()) && next.is_none_or(|c| !c.is_alphanumeric());
            if run.len() == 17 && run.starts_with("76561") {
                out.push_str(&run.parse::<u64>().map_or_else(|_| run.to_owned(), |id| self.steam_tag(id)));
            } else if ends_with_userdata(&out) || (alone && self.accounts.iter().any(|a| a == run)) {
                out.push_str(ACCOUNT_MASK);
            } else {
                out.push_str(run);
            }
            rest = after;
        }
        out.push_str(rest);
        out
    }
}

/// `text` ends in `userdata\` or `userdata/` (any case): the number that follows is a Steam account.
fn ends_with_userdata(text: &str) -> bool {
    let tail = text.get(text.len().saturating_sub(9)..).unwrap_or("");
    tail.len() == 9 && tail[..8].eq_ignore_ascii_case("userdata") && matches!(tail.as_bytes()[8], b'\\' | b'/')
}

/// `text` with every dotted IPv4 address but loopback (`127.x`) and `0.0.0.0` masked: four numbers of at most 255 with no digit
/// or dot right before or after (so a version `1.2.3.4.5` stays).
pub fn redact_ipv4(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        let starts = b[i].is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_digit() || b[i - 1] == b'.'));
        if let Some((len, keep)) = starts.then(|| ipv4_at(&b[i..])).flatten() {
            let end = i + len;
            let more = b
                .get(end)
                .is_some_and(|c| c.is_ascii_digit() || (*c == b'.' && b.get(end + 1).is_some_and(u8::is_ascii_digit)));
            if !more {
                out.push_str(&text[copied..i]);
                out.push_str(if keep { &text[i..end] } else { IPV4_MASK });
                copied = end;
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    out
}

/// The length of the dotted quad at the start of `b`, and whether it is loopback or unspecified.
fn ipv4_at(b: &[u8]) -> Option<(usize, bool)> {
    let mut at = 0;
    let mut parts = [0u32; 4];
    for (n, part) in parts.iter_mut().enumerate() {
        if n > 0 {
            if b.get(at) != Some(&b'.') {
                return None;
            }
            at += 1;
        }
        let digits = b.get(at..)?.iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 || digits > 3 {
            return None;
        }
        *part = std::str::from_utf8(b.get(at..at + digits)?).ok()?.parse().ok()?;
        if *part > 255 {
            return None;
        }
        at += digits;
    }
    Some((at, parts[0] == 127 || parts == [0, 0, 0, 0]))
}

/// `text` with every IPv6 address but `::1` and `::` masked: a run of hex digits and colons (at least two) standing alone (no
/// letter, digit, `_` or `:` right before, no letter, digit or `_` right after) that the standard library parses as an address.
/// Times (`12:34:56`) and C++ names (`std::vector`) do not parse or do not stand alone.
pub fn redact_ipv6(text: &str) -> String {
    let b = text.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let hexish = |c: u8| c.is_ascii_hexdigit() || c == b':';
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        if !(hexish(b[i]) && (i == 0 || !(word(b[i - 1]) || b[i - 1] == b':'))) {
            i += 1;
            continue;
        }
        let end = i + b[i..].iter().take_while(|c| hexish(**c)).count();
        let run = &text[i..end];
        let alone = b.get(end).is_none_or(|c| !word(*c));
        if alone && run.matches(':').count() >= 2 {
            if let Ok(addr) = run.parse::<std::net::Ipv6Addr>() {
                out.push_str(&text[copied..i]);
                out.push_str(if addr.is_loopback() || addr.is_unspecified() { run } else { IPV6_MASK });
                copied = end;
            }
        }
        i = end;
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r() -> Redactor {
        Redactor::new(vec!["Ann".into()], "salt", vec!["123456789".into()])
    }

    #[test]
    fn steam_ids_become_tags_that_are_stable_within_a_report_and_differ_between_reports() {
        let a = r();
        let text = "host 76561190000000001 joined; peer=76561190000000002, again 76561190000000001.";
        let (t1, t2) = (a.steam_tag(76561190000000001), a.steam_tag(76561190000000002));
        assert_eq!(format!("host {t1} joined; peer={t2}, again {t1}."), a.text(text));
        assert!(t1.starts_with("steam-") && t1.len() == 14 && t1 != t2);
        assert_ne!(t1, Redactor::new(vec![], "other salt", vec![]).steam_tag(76561190000000001));
        // other numbers stay: 17 digits that are no Steam ID, a longer run, a hash, a build
        for keep in ["12345678901234567", "765611900000000011", "a1b2c3d4", "40420"] {
            assert_eq!(keep, a.text(keep));
        }
    }

    #[test]
    fn the_steam_account_number_and_the_user_name_are_masked_in_paths_and_in_the_game_files() {
        let a = r();
        assert_eq!(
            r"C:\Program Files (x86)\Steam\userdata\%STEAMACCOUNT%\3493540\local and C:\Users\%USER%\AppData",
            a.text(r"C:\Program Files (x86)\Steam\userdata\123456789\3493540\local and C:\Users\ann\AppData")
        );
        assert_eq!(r#""userId": "%STEAMACCOUNT%""#, a.text(r#""userId": "123456789""#));
        assert_eq!("UserData/%STEAMACCOUNT%/x", a.text("UserData/987654321/x"), "another account of the PC too");
        assert_eq!("t=1234567890 x123456789", a.text("t=1234567890 x123456789"), "only the whole number");
    }

    #[test]
    fn ip_addresses_are_masked_but_loopback_versions_and_times_stay() {
        assert_eq!("from <ipv4>:28090 and <ipv4>", redact_ipv4("from 192.168.1.20:28090 and 8.8.8.8"));
        assert_eq!("127.0.0.1:28090 0.0.0.0", redact_ipv4("127.0.0.1:28090 0.0.0.0"));
        for keep in ["1.2.3.4.5", "v1.2.3", "300.1.1.1", "2026.10.21", "12:34:56.789"] {
            assert_eq!(keep, redact_ipv4(keep));
        }
        assert_eq!("[<ipv6>]:28090 and <ipv6>.", redact_ipv6("[2001:db8::7]:28090 and fe80::1:2:3."));
        assert_eq!("::1 and :: stay", redact_ipv6("::1 and :: stay"));
        for keep in ["12:34:56", "std::vector", "a::b::c", "time 2026-10-21 12:22:04Z", "ab:cd", "dead::beefy"] {
            assert_eq!(keep, redact_ipv6(keep));
        }
        assert_eq!("::ffff:<ipv4>", r().text("::ffff:10.0.0.1"));
    }

    #[test]
    fn a_join_code_is_removed_before_anything_inside_it_is_touched() {
        let endpoint = "10.0.0.5:28090".parse().unwrap();
        let code = JoinCode::create(&[endpoint], [3u8; 16], Some(1_700_000_000)).unwrap().encode();
        assert_eq!("join code <join code removed> from %USER%", r().text(&format!("join code {code} from Ann")));
    }
}
