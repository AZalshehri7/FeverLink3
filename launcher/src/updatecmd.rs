//! `feverlink3 update`: the network fetch, the prompts and the file operations around the pure checks of
//! `feverlink3_launcher::update`. Nothing happens unless the player types the command, and nothing is installed
//! until the player types `yes` after seeing what was verified.
//!
//! ```text
//! feverlink3 update --check [--pre]     ask GitHub for the newest release and say whether it is newer; change nothing
//! feverlink3 update [--pre]             download, verify, show, ask, install, start check, keep or roll back
//! feverlink3 update --from DIR          the same with manifest.json, manifest.json.sig and the zip already in DIR
//! feverlink3 update --confirm           keep an overlay update after the game ran well with it
//! feverlink3 update --rollback          put the previous version back
//! feverlink3 update --probe             the start check: print `feverlink3 <version>` and exit (used by the updater)
//! ```
//!
//! The download uses Windows' own `curl.exe` (`%SystemRoot%\System32\curl.exe`, part of Windows 10 1803 and later)
//! with HTTPS only, also for redirects: no HTTP or TLS crate is added to the program for one download.

use std::ffi::OsString;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use feverlink3_launcher::update::{self, AfterStart, Decision, Journal, Layout, Pending, Probe, Version};
use feverlink3_launcher::{process, steam};

/// The version of this program.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const REPO: &str = "AZalshehri7/FeverLink3";
const MAX_JSON: u64 = 2 << 20;
const MAX_SIG: u64 = 4 << 10;
const MAX_ZIP: u64 = 256 << 20;
const PROBE_TIME: Duration = Duration::from_secs(30);

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    check: bool,
    pre: bool,
    from: Option<PathBuf>,
    confirm: bool,
    rollback: bool,
    probe: bool,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut o = Options::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => o.check = true,
            "--pre" => o.pre = true,
            "--confirm" => o.confirm = true,
            "--rollback" => o.rollback = true,
            "--probe" => o.probe = true,
            "--from" => o.from = Some(it.next().ok_or("--from needs a folder")?.into()),
            other => return Err(format!("unknown option {other:?}")),
        }
    }
    if [o.check, o.from.is_some(), o.confirm, o.rollback, o.probe].iter().filter(|b| **b).count() > 1 {
        return Err("use one of --check, --from, --confirm, --rollback, --probe".into());
    }
    Ok(o)
}

pub fn run(args: &[String]) -> ExitCode {
    let result = parse(args).and_then(|o| {
        if o.probe {
            println!("feverlink3 {VERSION}");
            Ok(())
        } else if o.confirm {
            confirm()
        } else if o.rollback {
            rollback()
        } else if o.check {
            check(o.pre)
        } else {
            install(o.pre, o.from.as_deref())
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("update: {e}");
            ExitCode::FAILURE
        }
    }
}

// ------------------------------------------------------------------------------------------------ where things are

fn exe_dir() -> Result<PathBuf, String> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .ok_or_else(|| "cannot find the folder of this program".to_string())
}

fn install_dir(layout: Layout) -> Result<PathBuf, String> {
    match layout {
        Layout::Portable => exe_dir(),
        Layout::Overlay => steam::discover().game_dir.ok_or_else(|| "Transport Fever 3 was not found".to_string()),
    }
}

/// The unfinished update, if any, in either place an update can go.
fn find_journal() -> Result<Option<Journal>, String> {
    for layout in [Layout::Portable, Layout::Overlay] {
        let Ok(dir) = install_dir(layout) else { continue };
        if let Some(j) = Journal::read(&dir).map_err(|e| e.to_string())? {
            return Ok(Some(j));
        }
    }
    Ok(None)
}

/// No game, no session, no other updater. Asked before anything is written.
fn guard() -> Result<(), String> {
    let image = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| update::EXE_NAME.into());
    let launchers = process::count_running(&image).ok_or("cannot list the running programs (tasklist)")?;
    update::may_update(process::any_game_running(), launchers.saturating_sub(1)).map_err(|e| e.to_string())
}

// --------------------------------------------------------------------------------------------------------- fetch

fn curl_exe() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
    PathBuf::from(root).join("System32").join("curl.exe")
}

/// The arguments of one download: HTTPS only (also after a redirect), TLS 1.2 or newer, a size and time limit,
/// a failure on any HTTP error status.
fn curl_args(url: &str, out: &Path, max_bytes: u64) -> Vec<OsString> {
    let mut a: Vec<OsString> = [
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--max-redirs",
        "5",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--tlsv1.2",
        "--max-time",
        "900",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    a.push("--max-filesize".into());
    a.push(max_bytes.to_string().into());
    a.push("--user-agent".into());
    a.push(format!("feverlink3-updater/{VERSION}").into());
    a.push("--output".into());
    a.push(out.as_os_str().to_owned());
    a.push(url.into());
    a
}

/// Downloads `url` into `dir/name` and returns the bytes.
fn fetch(url: &str, dir: &Path, name: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://") {
        return Err(format!("refusing a non-HTTPS address: {url}"));
    }
    let out = dir.join(name);
    let status = Command::new(curl_exe())
        .args(curl_args(url, &out, max_bytes))
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("cannot run curl.exe: {e}"))?;
    if !status.success() {
        return Err(format!("download of {url} failed ({status})"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&out).and_then(|f| f.take(max_bytes + 1).read_to_end(&mut bytes)).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{name} is larger than {max_bytes} bytes"));
    }
    Ok(bytes)
}

/// A temporary download folder, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<TempDir, String> {
        let p = std::env::temp_dir().join(format!("feverlink3_update_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).map_err(|e| e.to_string())?;
        Ok(TempDir(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The newest release GitHub offers that is newer than this program, if any. Newer releases the updater cannot
/// use (no signature asset, a bad tag) are printed with the reason, so "nothing newer" is never said falsely.
fn newest(pre: bool, tmp: &Path) -> Result<Option<update::Release>, String> {
    let url = if pre {
        format!("https://api.github.com/repos/{REPO}/releases?per_page=20")
    } else {
        format!("https://api.github.com/repos/{REPO}/releases/latest")
    };
    let json = fetch(&url, tmp, "release.json", MAX_JSON).map_err(|e| {
        if pre {
            e
        } else {
            format!("{e} (GitHub answers 404 while there is no full release yet; `--pre` includes pre-releases)")
        }
    })?;
    let json = String::from_utf8(json).map_err(|_| "the release list is not text")?;
    let objects: Vec<serde_json::Value> = if pre {
        serde_json::from_str(&json).map_err(|e| e.to_string())?
    } else {
        vec![serde_json::from_str(&json).map_err(|e| e.to_string())?]
    };
    let current = Version::parse(VERSION).map_err(|e| e.to_string())?;
    let mut best: Option<update::Release> = None;
    for obj in &objects {
        match update::check_update(VERSION, &obj.to_string(), pre) {
            Ok(Decision::Available(r)) => {
                if best.as_ref().is_none_or(|b| r.version > b.version) {
                    best = Some(*r);
                }
            }
            Ok(Decision::UpToDate) => {}
            Err(e) => {
                let tag = obj.get("tag_name").and_then(|t| t.as_str()).unwrap_or("?");
                if Version::parse(tag).is_ok_and(|v| v > current) {
                    println!("{tag} is newer but cannot be installed by the updater: {e}");
                }
            }
        }
    }
    Ok(best)
}

// ------------------------------------------------------------------------------------------------------ commands

fn check(pre: bool) -> Result<(), String> {
    let tmp = TempDir::new()?;
    match newest(pre, &tmp.0)? {
        Some(r) => println!(
            "{} is available (this is {VERSION}). `feverlink3 update{}` installs it.",
            r.tag,
            if pre { " --pre" } else { "" }
        ),
        None => println!(
            "No {}release the updater can install is newer than {VERSION}.",
            if pre { "" } else { "full (non-pre-) " }
        ),
    }
    Ok(())
}

fn install(pre: bool, from: Option<&Path>) -> Result<(), String> {
    match update::pending(find_journal()?.as_ref()) {
        Pending::Clear => {}
        Pending::RollBackInterrupted => {
            let j = find_journal()?.ok_or("journal vanished")?;
            guard()?;
            update::rollback(&j).map_err(|e| e.to_string())?;
            return Err(format!(
                "an earlier update to {} was interrupted before its start check and has been undone; run update again",
                j.to
            ));
        }
        Pending::AwaitingConfirmation => {
            return Err("an earlier update waits: start the game, then run `feverlink3 update --confirm` (it works) or `--rollback`".into())
        }
    }

    let tmp = TempDir::new()?;
    let current = Version::parse(VERSION).map_err(|e| e.to_string())?;
    // 1. the manifest and its signature, and nothing else until the signature is good
    let (release, manifest_json, sig) = match from {
        Some(dir) => {
            let read = |n: &str| std::fs::read(dir.join(n)).map_err(|e| format!("{n}: {e}"));
            (None, read("manifest.json")?, read("manifest.json.sig").unwrap_or_default())
        }
        None => {
            let Some(r) = newest(pre, &tmp.0)? else {
                println!("No release the updater can install is newer than {VERSION}; nothing was changed.");
                return Ok(());
            };
            let m = fetch(&r.manifest.url, &tmp.0, "manifest.json", MAX_JSON)?;
            let s = fetch(&r.signature.url, &tmp.0, "manifest.json.sig", MAX_SIG)?;
            (Some(r), m, s)
        }
    };
    let (manifest, key) =
        update::verify_signed_manifest(&manifest_json, &sig, update::TRUSTED_KEYS, update::now_unix())
            .map_err(|e| format!("the update is refused: {e}"))?;
    let new = Version::parse(&manifest.version).map_err(|e| e.to_string())?;
    if let Some(r) = &release {
        update::check_manifest_version(&manifest, r).map_err(|e| e.to_string())?;
    }
    if new <= current {
        return Err(format!("the signed manifest is version {}, not newer than {VERSION}", manifest.version));
    }
    let layout = update::layout_of(&manifest).map_err(|e| e.to_string())?;
    let dir = install_dir(layout)?;
    if update::is_skipped(&update::read_skipped(&dir), &new) {
        return Err(format!(
            "{} failed its start check on this PC before and is skipped (listed in {})",
            manifest.version,
            dir.join(update::SKIP_FILE).display()
        ));
    }
    guard()?;

    // 2. the zip, checked against the signed manifest
    let zip = match (&release, from) {
        (Some(r), _) => {
            if r.zip.size > MAX_ZIP {
                return Err(format!("{} is larger than {MAX_ZIP} bytes", r.zip.name));
            }
            fetch(&r.zip.url, &tmp.0, &r.zip.name, MAX_ZIP)?
        }
        (None, Some(d)) => {
            std::fs::read(d.join(&manifest.zip_name)).map_err(|e| format!("{}: {e}", manifest.zip_name))?
        }
        (None, None) => unreachable!("either a release or a folder"),
    };
    update::verify_zip(&zip, &manifest).map_err(|e| format!("the update is refused: {e}"))?;
    let plan = update::plan_swap(&zip, &manifest, &dir).map_err(|e| format!("the update is refused: {e}"))?;

    // 3. the player decides
    println!("FeverLink3 {VERSION} -> {}", manifest.version);
    println!("  signature: good, by the trusted key {:?}", key.name);
    println!("  zip SHA-256: {} (matches the signed manifest)", manifest.zip_sha256);
    println!("  {} files into {}", manifest.files.len(), dir.display());
    println!("  the files replaced are kept as .old until the new version is confirmed");
    print!("Type yes to install: ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).map_err(|e| e.to_string())?;
    // a PowerShell pipe may put a byte-order mark in front
    if answer.trim().trim_start_matches('\u{feff}') != "yes" {
        println!("Nothing was changed.");
        return Ok(());
    }

    // 4. swap, journal, start check
    guard()?;
    update::stage(&plan, &zip, &manifest).map_err(|e| e.to_string())?;
    let applied = update::apply(&plan).map_err(|e| format!("the swap failed and was undone: {e}"))?;
    let journal =
        Journal { from: VERSION.into(), to: manifest.version.clone(), layout, install_dir: dir.clone(), applied };
    if let Err(e) = journal.write() {
        let _ = update::rollback(&journal);
        return Err(format!("cannot write the update journal ({e}); the update was undone"));
    }
    match layout {
        Layout::Portable => {
            let probe = probe(&dir.join(update::EXE_NAME));
            match update::judge_start(&manifest.version, &probe) {
                AfterStart::Keep => {
                    let left = update::confirm(&journal).map_err(|e| e.to_string())?;
                    println!(
                        "Updated to {}. Run `feverlink3 install` to put the new DLL and mod into the game.",
                        manifest.version
                    );
                    for p in left {
                        println!("  (left behind, removed by the next update: {})", p.display());
                    }
                    Ok(())
                }
                AfterStart::RollBack(why) => {
                    update::rollback(&journal).map_err(|e| format!("{why}; the rollback failed too: {e}"))?;
                    Err(format!("{why}; {VERSION} is back in place and {} will not be offered again", manifest.version))
                }
            }
        }
        Layout::Overlay => {
            println!("Installed {} into the game folder. Start the game and try it, then:", manifest.version);
            println!("  feverlink3 update --confirm    keep it");
            println!("  feverlink3 update --rollback   put {VERSION} back");
            Ok(())
        }
    }
}

/// Starts the new launcher with `update --probe` and collects what it says, within [`PROBE_TIME`].
fn probe(exe: &Path) -> Probe {
    let mut child = match Command::new(exe)
        .args(["update", "--probe"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Probe::NotStarted(e.to_string()),
    };
    let mut out = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = out.as_mut() {
            let _ = o.take(64 << 10).read_to_string(&mut s);
        }
        s
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Probe::Exited { code: status.code(), stdout: reader.join().unwrap_or_default() };
            }
            Ok(None) if start.elapsed() < PROBE_TIME => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Probe::TimedOut;
            }
        }
    }
}

fn confirm() -> Result<(), String> {
    let j = find_journal()?.ok_or("there is no update waiting for confirmation")?;
    let left = update::confirm(&j).map_err(|e| e.to_string())?;
    println!("Kept {}.", j.to);
    for p in left {
        println!("  (left behind, removed by the next update: {})", p.display());
    }
    Ok(())
}

fn rollback() -> Result<(), String> {
    let j = find_journal()?.ok_or("there is no update to roll back (only the last, unconfirmed one can be)")?;
    guard()?;
    update::rollback(&j).map_err(|e| e.to_string())?;
    println!("{} is back in place; {} will not be offered again.", j.from, j.to);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn options() {
        assert_eq!(parse(&args(&[])).unwrap(), Options::default());
        assert!(parse(&args(&["--check", "--pre"])).unwrap().pre);
        assert_eq!(parse(&args(&["--from", "x"])).unwrap().from, Some(PathBuf::from("x")));
        assert!(parse(&args(&["--from"])).is_err());
        assert!(parse(&args(&["--check", "--rollback"])).is_err(), "one action at a time");
        assert!(parse(&args(&["--yes"])).is_err(), "there is no way to skip the question");
    }

    #[test]
    fn downloads_are_https_only_and_limited() {
        let a: Vec<String> = curl_args("https://x/y", Path::new("C:/t/o"), 1234)
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        let pair = |k: &str| a.iter().position(|x| x == k).map(|i| a[i + 1].clone());
        assert_eq!(pair("--proto").as_deref(), Some("=https"));
        assert_eq!(pair("--proto-redir").as_deref(), Some("=https"));
        assert_eq!(pair("--max-filesize").as_deref(), Some("1234"));
        assert!(a.contains(&"--fail".to_string()) && a.contains(&"--tlsv1.2".to_string()));
        assert_eq!(a.last().map(String::as_str), Some("https://x/y"));
        assert!(curl_exe().ends_with("System32/curl.exe") || curl_exe().ends_with(r"System32\curl.exe"));
        let tmp = tempfile::tempdir().unwrap();
        assert!(fetch("http://x/y", tmp.path(), "o", 10).is_err(), "refused before curl runs");
    }
}
