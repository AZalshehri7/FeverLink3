//! Bug reports, the launcher's half: what one game's part holds (its logs, redacted on its own PC), the zip that carries it,
//! the bundle the host writes from every part, and where the player who asked finds it (the Desktop, a prefilled GitHub
//! issue). The session's half (who asks whom, the chunks, the timeout) is `core/src/session/report.rs`; the window's side is
//! `live.rs` (`report` command, `report` status). Nothing here sends anything anywhere by itself.
//!
//! Size: a bundle stays under [`ReportLimits::MAX_BUNDLE_BYTES`] (GitHub takes attachments up to 25 MB). Each part gets an equal
//! share; a part or bundle that is still too big has its biggest files cut from the front (the end of a log is what matters),
//! and its `report.txt` says so.

pub mod redact;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use feverlink3_core::session::ReportLimits;

use crate::gamelink;
use redact::Redactor;

/// The repository's new-issue page; the bug report form is `.github/ISSUE_TEMPLATE/bug_report.yml`.
pub const ISSUE_URL: &str = "https://github.com/AZalshehri7/FeverLink3/issues/new";
/// How much of the end of the id-order trace a part takes (the owner wants it; the whole file grows to tens of MB).
pub const IDORDER_TAIL: u64 = 8 << 20;
/// Any other log is read from its end up to this much: a log that grew for months must not fill the memory.
pub const LOG_TAIL: u64 = 32 << 20;
/// Crash files older than this are another day's crash and stay out.
pub const CRASH_RECENT: Duration = Duration::from_secs(24 * 3600);
/// The entry of a part that names its own game's Steam ID tag (see [`redact::Redactor`]); the bundle reads and drops it.
pub const SELF_TAG_FILE: &str = "selftag.txt";
/// The part's and the bundle's own description.
pub const REPORT_FILE: &str = "report.txt";

/// Where one game's logs are: its live folder (`%LOCALAPPDATA%\FeverLink3\<live>`; `native.log` and the id-order trace are in
/// the folder above it) and its Steam user folders (`userdata/<n>/3493540/local`, whose `crash_dump` has the game's crash files).
#[derive(Debug, Clone)]
pub struct Sources {
    pub live_dir: PathBuf,
    pub user_dirs: Vec<PathBuf>,
}

/// A file of a part or bundle: its name in the zip and its bytes.
pub type Entry = (String, Vec<u8>);

/// The last `max` bytes of a file, from the start of a line when it was cut; `None` when it cannot be read.
fn read_tail(path: &Path, max: u64) -> Option<(Vec<u8>, bool)> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
    if start > 0 {
        let cut = bytes.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
        bytes.drain(..cut);
    }
    Some((bytes, start > 0))
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn recent(path: &Path, now: SystemTime) -> bool {
    modified(path).is_some_and(|t| now.duration_since(t).map_or(true, |age| age <= CRASH_RECENT))
}

/// The files of this game's part, as they are on disk (not redacted yet), with a word on each that was cut. Never a savegame
/// and never a crash dump (`.dmp`).
pub fn collect(src: &Sources, now: SystemTime) -> (Vec<Entry>, Vec<String>) {
    let mut files: Vec<Entry> = Vec::new();
    let mut notes = Vec::new();
    let mut take = |name: String, path: &Path, max: u64, files: &mut Vec<Entry>| {
        if let Some((bytes, cut)) = read_tail(path, max) {
            if cut {
                notes.push(format!("{name}: only the last {} MB", max >> 20));
            }
            files.push((name, bytes));
        }
    };
    let live = &src.live_dir;
    // the logs that go on in generations (mod.log, native_events.log): the newest and the one before it
    for base in ["mod.log", "native_events.log"] {
        let path = live.join(base);
        let newest = gamelink::newest_generation(&path);
        for gen in newest.saturating_sub(1).max(1)..=newest {
            let file = gamelink::generation_path(&path, gen);
            let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            take(name, &file, LOG_TAIL, &mut files);
        }
    }
    for name in ["session.log", crate::live::STATUS_FILE] {
        take(name.to_owned(), &live.join(name), LOG_TAIL, &mut files);
    }
    if let Some(local) = live.parent() {
        for name in ["native.log", "native.log.1"] {
            take(name.to_owned(), &local.join(name), LOG_TAIL, &mut files);
        }
        let live_name = live.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let idorder = format!("idorder_{live_name}.log");
        take(idorder.clone(), &local.join(&idorder), IDORDER_TAIL, &mut files);
    }
    for user in &src.user_dirs {
        let dir = user.join("crash_dump");
        let mut crash: Vec<PathBuf> = ["error.json", "stdout.txt"].iter().map(|n| dir.join(n)).collect();
        // the newest `<uid>_<n>.txt` and `.json`: the game's report of its last error
        for ext in ["txt", "json"] {
            let newest = fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    p.extension().is_some_and(|e| e == ext) && name.contains('_') && name != "error.json"
                })
                .max_by_key(|p| modified(p).unwrap_or(UNIX_EPOCH));
            crash.extend(newest);
        }
        for path in crash.iter().filter(|p| recent(p, now)) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            take(format!("crash_dump-{name}"), path, LOG_TAIL, &mut files);
        }
    }
    (files, notes)
}

/// One game's part: its files redacted, cut to `max_bytes` if need be, zipped with its `report.txt` (`info`: the lines the
/// caller knows about this game) and the tag of its own Steam ID. Returns the zip and what it holds, for the window.
pub fn build_part(
    files: Vec<Entry>,
    notes: Vec<String>,
    info: &[String],
    redactor: &Redactor,
    own_steam_id: Option<u64>,
    max_bytes: u64,
) -> io::Result<(Vec<u8>, Vec<String>)> {
    let files: Vec<Entry> = files
        .into_iter()
        .map(|(name, bytes)| (name, redactor.text(&String::from_utf8_lossy(&bytes)).into_bytes()))
        .collect();
    let mut notes = notes;
    let room = max_bytes.saturating_sub(64 * 1024);
    let (mut files, cut) = fit(files, room)?;
    notes.extend(cut);
    let mut text = info.to_vec();
    text.push(String::new());
    text.push("files:".to_owned());
    text.extend(files.iter().map(|(n, b)| format!("  {n} ({})", size_text(b.len() as u64))));
    if !notes.is_empty() {
        text.push("cut to fit:".to_owned());
        text.extend(notes.iter().map(|n| format!("  {n}")));
    }
    let contents: Vec<String> = files.iter().map(|(n, b)| format!("{n} ({})", size_text(b.len() as u64))).collect();
    files.insert(0, (REPORT_FILE.to_owned(), redactor.text(&(text.join("\n") + "\n")).into_bytes()));
    if let Some(id) = own_steam_id {
        files.push((SELF_TAG_FILE.to_owned(), redactor.steam_tag(id).into_bytes()));
    }
    Ok((zip(&files)?, contents))
}

/// The files zipped (deflate).
pub fn zip(files: &[Entry]) -> io::Result<Vec<u8>> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in files {
        w.start_file(name.as_str(), options).map_err(io::Error::other)?;
        w.write_all(bytes)?;
    }
    Ok(w.finish().map_err(io::Error::other)?.into_inner())
}

/// The marker a cut file starts with.
fn cut_marker(bytes: usize) -> String {
    format!(
        "[FeverLink3 bug report: the first {} of this file were cut to keep the report under 24 MB]\n",
        size_text(bytes as u64)
    )
}

/// The files, cut until their zip is at most `cap` bytes: the biggest file loses its beginning (from a line start), as much
/// as the zip's measured ratio says is needed, a few times at most. Returns the files and a line per cut.
pub fn fit(mut files: Vec<Entry>, cap: u64) -> io::Result<(Vec<Entry>, Vec<String>)> {
    let mut notes = Vec::new();
    for _ in 0..12 {
        let size = zip(&files)?.len() as u64;
        if size <= cap {
            return Ok((files, notes));
        }
        let raw: u64 = files.iter().map(|(_, b)| b.len() as u64).sum::<u64>().max(1);
        let ratio = (size as f64 / raw as f64).max(0.01);
        let need = ((size - cap) as f64 / ratio * 1.2) as usize + 64 * 1024;
        let Some((name, bytes)) = files.iter_mut().max_by_key(|(_, b)| b.len()) else { break };
        let len = bytes.len();
        // at most half of the file at a time: the next round measures again
        let cut = need.min(len / 2).max(1).min(len);
        let start = bytes[cut..].iter().position(|b| *b == b'\n').map_or(len, |i| cut + i + 1);
        let mut kept = cut_marker(start).into_bytes();
        kept.extend_from_slice(&bytes[start..]);
        *bytes = kept;
        notes.push(format!("{name}: the first {} cut", size_text(start as u64)));
    }
    let size = zip(&files)?.len() as u64;
    if size <= cap {
        return Ok((files, notes));
    }
    Err(io::Error::other(format!("the files do not fit {} even when cut", size_text(cap))))
}

/// `1.2 MB`, `340 KB`, `12 bytes`.
pub fn size_text(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{} KB", n / 1000)
    } else {
        format!("{n} bytes")
    }
}

/// A part read back by the host: its files, and the tag its game's own Steam ID has in this report. A part is another
/// player's data: entries are limited in number and size (a zip bomb stops at the limits), names are reduced to a plain
/// file name.
pub fn read_part(bytes: &[u8]) -> io::Result<(Vec<Entry>, Option<String>)> {
    const MOST_ENTRIES: usize = 64;
    const MOST_ENTRY: u64 = 48 << 20;
    const MOST_TOTAL: u64 = 160 << 20;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(io::Error::other)?;
    let mut files = Vec::new();
    let mut tag = None;
    let mut total = 0u64;
    for i in 0..archive.len().min(MOST_ENTRIES) {
        let mut entry = archive.by_index(i).map_err(io::Error::other)?;
        if entry.is_dir() {
            continue;
        }
        let name = safe_name(&entry.name().map(|n| n.into_owned()).unwrap_or_default());
        let mut data = Vec::new();
        (&mut entry).take(MOST_ENTRY.min(MOST_TOTAL - total)).read_to_end(&mut data)?;
        total += data.len() as u64;
        if name == SELF_TAG_FILE {
            let t = String::from_utf8_lossy(&data).trim().to_owned();
            if t.starts_with("steam-") && t.len() <= 32 {
                tag = Some(t);
            }
        } else {
            files.push((name, data));
        }
        if total >= MOST_TOTAL {
            break;
        }
    }
    Ok((files, tag))
}

/// A zip entry's name as a plain file name: the last path piece, only letters, digits and `._-`.
fn safe_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String =
        last.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).take(80).collect();
    let clean = clean.trim_start_matches('.').to_owned();
    if clean.is_empty() {
        "file".to_owned()
    } else {
        clean
    }
}

/// One game in a bundle: `slot` is its N in `player-N` (1 = the host).
#[derive(Debug, Clone)]
pub struct BundlePlayer {
    pub slot: usize,
    pub name: String,
    pub host: bool,
    pub part: Option<Vec<u8>>,
    pub missing: Option<String>,
}

/// What the bundle's own `report.txt` says besides the players.
#[derive(Debug, Clone)]
pub struct BundleInfo {
    pub report: String,
    pub created_unix: u64,
    pub asked_by: String,
    pub note: String,
    pub players: Vec<BundlePlayer>,
}

/// The bundle: one folder per player (`player-1` = the host), each player's own Steam ID tag turned into `player-N` in every
/// file, a top-level `report.txt`, at most [`ReportLimits::MAX_BUNDLE_BYTES`]. Returns the zip and what it holds.
pub fn make_bundle(info: &BundleInfo) -> io::Result<(Vec<u8>, Vec<String>)> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut tags: BTreeMap<String, String> = BTreeMap::new();
    let mut lines = vec![
        "FeverLink3 bug report".to_owned(),
        format!("report id: {}", info.report),
        format!("made: {} UTC", stamp_text(info.created_unix, true)),
        format!("asked by: {}", info.asked_by),
        format!("note: {}", if info.note.trim().is_empty() { "(none)" } else { info.note.trim() }),
        String::new(),
        "players:".to_owned(),
    ];
    let mut missing = Vec::new();
    for p in &info.players {
        let label = format!("player-{}", p.slot);
        let role = if p.host { "host" } else { "player" };
        let state = match (&p.part, &p.missing) {
            (Some(_), _) => "logs included".to_owned(),
            (None, why) => {
                let why = why.clone().unwrap_or_else(|| "no part".to_owned());
                missing.push(format!("{label} ({}): {why}", p.name));
                format!("MISSING: {why}")
            }
        };
        lines.push(format!("  {label} ({role}) {}: {state}", p.name));
        let Some(bytes) = &p.part else { continue };
        match read_part(bytes) {
            Ok((files, tag)) => {
                if let Some(tag) = tag {
                    tags.insert(tag, label.clone());
                }
                entries.extend(files.into_iter().map(|(n, b)| (format!("{label}/{n}"), b)));
            }
            Err(e) => missing.push(format!("{label} ({}): its part could not be read: {e}", p.name)),
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "missing parts: {}",
        if missing.is_empty() { "none".to_owned() } else { missing.len().to_string() }
    ));
    lines.extend(missing.iter().map(|m| format!("  {m}")));
    // each game's own tag becomes its player-N everywhere (the tags are random hex: a plain replace cannot hit anything else)
    for (_, bytes) in entries.iter_mut() {
        let mut text = String::from_utf8_lossy(bytes).into_owned();
        let mut changed = false;
        for (tag, label) in &tags {
            if text.contains(tag.as_str()) {
                text = text.replace(tag.as_str(), label);
                changed = true;
            }
        }
        if changed {
            *bytes = text.into_bytes();
        }
    }
    let cap = ReportLimits::MAX_BUNDLE_BYTES - 64 * 1024;
    let (mut entries, cut) = fit(entries, cap)?;
    if !cut.is_empty() {
        lines.push(String::new());
        lines.push("cut to fit in the bundle:".to_owned());
        lines.extend(cut.iter().map(|c| format!("  {c}")));
    }
    let contents: Vec<String> = entries.iter().map(|(n, b)| format!("{n} ({})", size_text(b.len() as u64))).collect();
    entries.insert(0, (REPORT_FILE.to_owned(), (lines.join("\n") + "\n").into_bytes()));
    Ok((zip(&entries)?, contents))
}

/// The entries of a zip with their sizes, for the window ("player-1/mod.log (1.2 MB)"); empty when it is not a zip.
pub fn zip_contents(bytes: &[u8]) -> Vec<String> {
    let Ok(mut archive) = zip::ZipArchive::new(Cursor::new(bytes)) else { return Vec::new() };
    (0..archive.len())
        .filter_map(|i| {
            let e = archive.by_index(i).ok()?;
            Some(format!("{} ({})", e.name().ok()?, size_text(e.size())))
        })
        .collect()
}

/// `20261021-142233` (`file`) or `2026-10-21 14:22:33` from unix seconds, UTC (the civil-from-days algorithm of Howard
/// Hinnant; no date crate for a file name).
pub fn stamp_text(unix: u64, readable: bool) -> String {
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
    let (h, m, s) = (rest / 3_600, rest % 3_600 / 60, rest % 60);
    if readable {
        format!("{year:04}-{month:02}-{day:02} {h:02}:{m:02}:{s:02}")
    } else {
        format!("{year:04}{month:02}{day:02}-{h:02}{m:02}{s:02}")
    }
}

/// `FeverLink3-report-<YYYYMMDD-HHMMSS>-<report>.zip` (UTC).
pub fn bundle_file_name(created_unix: u64, report: &str) -> String {
    let id: String = report.chars().filter(char::is_ascii_alphanumeric).take(16).collect();
    format!("FeverLink3-report-{}-{id}.zip", stamp_text(created_unix, false))
}

/// The prefilled issue (`.github/ISSUE_TEMPLATE/bug_report.yml`): the form's title, version and report id. The note stays out of
/// the address (it can be long, and the player writes the issue in their own words).
pub fn issue_url(report: &str, version: &str) -> String {
    format!(
        "{ISSUE_URL}?template=bug_report.yml&title={}&version={}&report_id={}",
        url_encode(&format!("Bug report {report}")),
        url_encode(version),
        url_encode(report)
    )
}

/// Percent-encoding of everything but the unreserved characters (RFC 3986).
pub fn url_encode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Where the player's copy goes: the Desktop (as Windows names it for this user, a OneDrive Desktop too), else
/// `Documents\FeverLink3\reports`. The first folder that exists or can be made.
pub fn report_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(desktop) = shell_folder("Desktop") {
        dirs.push(desktop);
    }
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    if let Some(p) = &profile {
        dirs.push(p.join("Desktop"));
    }
    let documents = shell_folder("Personal").or_else(|| profile.map(|p| p.join("Documents")));
    if let Some(d) = documents {
        dirs.push(d.join("FeverLink3").join("reports"));
    }
    dirs
}

/// The folder the host keeps its own copy of a report a player asked for (never its Desktop: the report is the other
/// player's): `Documents\FeverLink3\reports`.
pub fn kept_dir() -> Option<PathBuf> {
    report_dirs().into_iter().last()
}

/// A user shell folder from the registry (`HKCU\...\Explorer\User Shell Folders`), its `%VARIABLES%` expanded.
#[cfg(windows)]
fn shell_folder(name: &str) -> Option<PathBuf> {
    use winreg::enums::HKEY_CURRENT_USER;
    let key = winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders")
        .ok()?;
    let raw: String = key.get_value(name).ok()?;
    let expanded = expand_env(&raw);
    (!expanded.contains('%')).then(|| PathBuf::from(expanded))
}

#[cfg(not(windows))]
fn shell_folder(_name: &str) -> Option<PathBuf> {
    None
}

/// `%NAME%` replaced by the environment variable's value; an unknown one stays as it is.
pub fn expand_env(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Writes the bundle into the first of `dirs` that takes it. Returns its path.
pub fn save_bundle(dirs: &[PathBuf], name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no folder to save the report in");
    for dir in dirs {
        let path = dir.join(name);
        let written = fs::create_dir_all(dir).and_then(|()| {
            let tmp = dir.join(format!("{name}.tmp"));
            fs::write(&tmp, bytes)?;
            fs::rename(&tmp, &path)
        });
        match written {
            Ok(()) => return Ok(path),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The Steam account numbers of the user folders (`userdata/<n>/3493540/local`).
pub fn accounts_of(user_dirs: &[PathBuf]) -> Vec<String> {
    user_dirs
        .iter()
        .filter_map(|d| d.parent()?.parent()?.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        .collect()
}

/// The Windows user's names to take out: `USERNAME`, `USER` and the profile folder's name.
pub fn user_names() -> Vec<String> {
    let profile_folder = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    ["USERNAME", "USER"].iter().filter_map(|key| std::env::var(key).ok()).chain(profile_folder).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn salted() -> Redactor {
        Redactor::new(vec!["Ann".into()], "s1", vec!["123456789".into()])
    }

    fn unzip(bytes: &[u8]) -> BTreeMap<String, String> {
        let mut a = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        (0..a.len())
            .map(|i| {
                let mut e = a.by_index(i).unwrap();
                let mut s = String::new();
                e.read_to_string(&mut s).unwrap();
                (e.name().unwrap().into_owned(), s)
            })
            .collect()
    }

    #[test]
    fn a_part_holds_the_logs_of_its_game_redacted_and_never_a_save_or_a_dump() {
        let root = tempfile::tempdir().unwrap();
        let local = root.path().join("FeverLink3");
        let live = local.join("live2");
        let user = root.path().join("userdata").join("123456789").join("3493540").join("local");
        let crash = user.join("crash_dump");
        for d in [&live, &crash, &user.join("save")] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(live.join("mod.log"), "opened C:\\Users\\Ann\\x from 10.1.2.3 as 76561190000000007\n").unwrap();
        fs::write(live.join("mod.gen"), "2").unwrap();
        fs::write(live.join("mod.2.log"), "newest mod line\n").unwrap();
        fs::write(live.join("session.log"), "checksum 4 differs in lane:towns\n").unwrap();
        fs::write(local.join("native.log"), "native\n").unwrap();
        fs::write(local.join("idorder_live2.log"), "x".repeat(100)).unwrap();
        fs::write(local.join("idorder_live1.log"), "another game's trace").unwrap();
        fs::write(crash.join("error.json"), r#"{"userId": "123456789", "message": "boom"}"#).unwrap();
        fs::write(crash.join("abc_12.txt"), "stack").unwrap();
        fs::write(crash.join("abc.dmp"), "dump").unwrap();
        fs::write(user.join("save").join("world.sav"), "save").unwrap();
        let src = Sources { live_dir: live, user_dirs: vec![user] };
        let (files, notes) = collect(&src, SystemTime::now());
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            vec![
                "mod.log",
                "mod.2.log",
                "session.log",
                "native.log",
                "idorder_live2.log",
                "crash_dump-error.json",
                "crash_dump-abc_12.txt"
            ],
            names
        );
        assert!(notes.is_empty());
        let r = salted();
        let info = vec!["role: player".to_owned()];
        let (zip, contents) = build_part(files, notes, &info, &r, Some(76561190000000007), 1 << 20).unwrap();
        let got = unzip(&zip);
        let tag = r.steam_tag(76561190000000007);
        assert_eq!(format!("opened C:\\Users\\%USER%\\x from <ipv4> as {tag}\n"), got["mod.log"]);
        assert_eq!(r#"{"userId": "%STEAMACCOUNT%", "message": "boom"}"#, got["crash_dump-error.json"]);
        assert_eq!(tag, got[SELF_TAG_FILE]);
        assert!(got[REPORT_FILE].starts_with("role: player\n\nfiles:\n  mod.log ("), "{}", got[REPORT_FILE]);
        assert_eq!(7, contents.len());
        assert!(!got.keys().any(|n| n.ends_with(".dmp") || n.ends_with(".sav")));
    }

    #[test]
    fn files_too_big_for_the_limit_lose_their_beginning_and_say_so() {
        // noise that does not compress: a few MB that must fit in one
        let mut seed = 7u64;
        let noisy: Vec<u8> = (0..3_000_000)
            .map(|i| {
                if i % 80 == 79 {
                    return b'\n';
                }
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                b'a' + (seed >> 60) as u8
            })
            .collect();
        let files = vec![("big.log".to_owned(), noisy.clone()), ("small.log".to_owned(), b"keep me\n".to_vec())];
        let (fitted, notes) = fit(files, 1_000_000).unwrap();
        assert!(zip(&fitted).unwrap().len() <= 1_000_000);
        assert_eq!(b"keep me\n".to_vec(), fitted[1].1, "the biggest file is cut, the small one stays whole");
        let big = String::from_utf8(fitted[0].1.clone()).unwrap();
        assert!(big.starts_with("[FeverLink3 bug report: the first "), "{}", &big[..80]);
        let kept = &big.as_bytes()[big.find('\n').unwrap() + 1..];
        assert!(kept.len() > 100_000 && noisy.ends_with(kept), "the end of the log is kept");
        assert!(!notes.is_empty() && notes[0].starts_with("big.log: the first "));
        // what fits stays as it is
        let (same, none) = fit(vec![("a".into(), b"abc".to_vec())], 10_000).unwrap();
        assert_eq!((vec![("a".to_owned(), b"abc".to_vec())], Vec::<String>::new()), (same, none));
    }

    #[test]
    fn the_bundle_has_a_folder_per_player_names_each_players_tag_and_lists_the_missing() {
        let r = salted();
        let (host_id, guest_id, other) = (76561190000000001u64, 76561190000000002u64, 76561190000000099u64);
        let line = |me: u64| {
            vec![("session.log".to_owned(), format!("me {me}, host {host_id}, friend {other}\n").into_bytes())]
        };
        let (host_part, _) =
            build_part(line(host_id), vec![], &["role: host".into()], &r, Some(host_id), 1 << 20).unwrap();
        let (guest_part, _) =
            build_part(line(guest_id), vec![], &["role: player".into()], &r, Some(guest_id), 1 << 20).unwrap();
        let info = BundleInfo {
            report: "a1b2c3".into(),
            created_unix: 1_792_000_000,
            asked_by: "Pat".into(),
            note: "the bridge vanished".into(),
            players: vec![
                BundlePlayer { slot: 1, name: "Hosty".into(), host: true, part: Some(host_part), missing: None },
                BundlePlayer { slot: 2, name: "Pat".into(), host: false, part: Some(guest_part), missing: None },
                BundlePlayer {
                    slot: 3,
                    name: "Kim".into(),
                    host: false,
                    part: None,
                    missing: Some("it did not answer within 60 s".into()),
                },
                BundlePlayer {
                    slot: 4,
                    name: "Lee".into(),
                    host: false,
                    part: Some(b"not a zip".to_vec()),
                    missing: None,
                },
            ],
        };
        let (bundle, contents) = make_bundle(&info).unwrap();
        let got = unzip(&bundle);
        let friend = r.steam_tag(other);
        assert_eq!(format!("me player-1, host player-1, friend {friend}\n"), got["player-1/session.log"]);
        assert_eq!(format!("me player-2, host player-1, friend {friend}\n"), got["player-2/session.log"]);
        assert!(got.contains_key("player-2/report.txt") && !got.keys().any(|k| k.contains(SELF_TAG_FILE)));
        let top = &got[REPORT_FILE];
        for want in [
            "report id: a1b2c3",
            "made: 2026-10-14 ",
            "note: the bridge vanished",
            "player-1 (host) Hosty: logs included",
            "player-3 (player) Kim: MISSING: it did not answer within 60 s",
            "missing parts: 2",
            "player-4 (Lee): its part could not be read",
        ] {
            assert!(top.contains(want), "{want:?} in\n{top}");
        }
        assert!(contents.iter().any(|c| c.starts_with("player-2/session.log (")));
        assert_eq!("FeverLink3-report-20261014-174640-a1b2c3.zip", bundle_file_name(1_792_000_000, "a1b2c3"));
    }

    #[test]
    fn a_hostile_part_cannot_name_a_path_or_grow_without_limit() {
        assert_eq!("passwd", safe_name("../../etc/passwd"));
        assert_eq!("file", safe_name("..\\"));
        assert_eq!("a_b.log", safe_name("C:\\x\\a b.log"));
        let part =
            zip(&[("../x.log".to_owned(), b"hi".to_vec()), (SELF_TAG_FILE.to_owned(), b"not a tag".to_vec())]).unwrap();
        let (files, tag) = read_part(&part).unwrap();
        assert_eq!((vec![("x.log".to_owned(), b"hi".to_vec())], None), (files, tag));
    }

    #[test]
    fn the_issue_address_is_short_and_prefilled() {
        let url = issue_url("a1b2c3", "0.1.1-pre9");
        assert_eq!(
            "https://github.com/AZalshehri7/FeverLink3/issues/new?template=bug_report.yml&title=Bug%20report%20a1b2c3&version=0.1.1-pre9&report_id=a1b2c3",
            url
        );
        assert_eq!("a%26b%3Dc%20%C3%A9", url_encode("a&b=c é"));
        std::env::set_var("FEVERLINK3_TEST_EXPAND", "X");
        assert_eq!("X\\Desktop %NOPE_NOT_SET% 50%", expand_env("%FEVERLINK3_TEST_EXPAND%\\Desktop %NOPE_NOT_SET% 50%"));
        let accounts = accounts_of(&[PathBuf::from("C:/Steam/userdata/123/3493540/local")]);
        assert_eq!(vec!["123".to_owned()], accounts);
    }

    #[test]
    fn a_bundle_is_saved_to_the_first_folder_that_takes_it() {
        let root = tempfile::tempdir().unwrap();
        let blocked = root.path().join("file-not-folder");
        fs::write(&blocked, "x").unwrap();
        let good = root.path().join("reports");
        let path = save_bundle(&[blocked, good.clone()], "r.zip", b"zip").unwrap();
        assert_eq!(good.join("r.zip"), path);
        assert_eq!(b"zip".to_vec(), fs::read(path).unwrap());
    }
}
