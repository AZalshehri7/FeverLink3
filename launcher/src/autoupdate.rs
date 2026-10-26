//! The automatic update inside the game (owner decision 2026-10-22): players install by copying `winhttp.dll` and
//! `mods\release\feverlink3` into the game folder and never run `feverlink3.exe`, so the module updates itself.
//!
//! # When what happens
//!
//! 1. **At every game start, in `DllMain`** (native `lib.rs`, before the game has run any of its own code, so before
//!    it reads a mod): [`at_start`] finishes or undoes what an earlier run left in `<game>\feverlink3_update\`. File
//!    renames only: no network, no waiting.
//! 2. **Once per start, on a thread of its own** ([`start`]), not before [`FIRST_CHECK_AFTER`] in the game: unless the
//!    player turned it off, the module asks GitHub
//!    for the newest full release (sooner, and once more, when a newer host refuses the player: [`check_now`]). If it is newer, it is downloaded, checked ([`stage_update`], the trust model below)
//!    and staged in `<game>\feverlink3_update\<version>\`. Never while a session runs: the thread waits until the
//!    player is out of the session before it downloads or touches a file.
//! 3. **Still in that run**, the new `winhttp.dll` is put in place: Windows lets a loaded DLL be renamed, so the
//!    running one becomes `winhttp.dll.old-<version>` and the new one takes its name. The running game keeps the old
//!    DLL in memory and the old mod it has already read: old with old, sessions stay possible. The window says
//!    "restart the game to finish".
//! 4. **At the next start** the game loads the NEW DLL, whose `DllMain` (step 1) swaps the mod folder before the game
//!    reads mods: `mods\release\feverlink3` moves to `feverlink3_update\backup-<old>\` and the staged folder takes its
//!    place (two renames on one volume). That start runs new with new. After [`HEALTHY_AFTER`] in the game the backup,
//!    the old DLL and the staging folder are deleted ([`confirm`]).
//!
//! Why this order: the native module is the first of our code to run, and the only moment the mod files can change
//! under a game that has not read them is its `DllMain`; a DLL cannot replace itself in its own process, and file
//! work at process exit (`DLL_PROCESS_DETACH`) is unsafe. Swapping the DLL early (step 3) and the mod late (step 4)
//! costs the player ONE restart, and no run ever mixes a new mod with an old DLL in a session.
//!
//! If a step cannot complete (the mod folder is locked, the staged files are gone, the new version did not reach
//! [`HEALTHY_AFTER`] in [`START_ATTEMPTS`] starts), the previous files are put back ([`rollback`]), the version goes on
//! the skip list (`feverlink3_update_skip.txt` in the game folder) and the old version tells the player once. A run
//! in which the files on disk no longer match the DLL in memory ([`Status::restart`]) refuses to host or join until
//! the game is restarted.
//!
//! Compatibility rule for every later version: its `DllMain` must keep calling [`at_start`] with this state file,
//! because the version that downloads an update relies on the NEW version to finish it.
//!
//! # Trust model until 1.0.0 (owner decision 2026-10-22: releases are unsigned until then)
//!
//! An update is accepted only when all of these hold (see also `docs/SECURITY_FOR_USERS.md`, "Automatic updates"):
//!
//! - it comes from [`API_LATEST`] (the repository's newest full release: GitHub never lists a draft or a pre-release
//!   there, and [`update::check_update`] refuses both again), and every file from
//!   `https://github.com/AZalshehri7/FeverLink3/releases/download/<that tag>/`; HTTPS only, also for redirects, with
//!   the certificate checked, GitHub's hosts only (WinHTTP inside the process, [`crate::https`]; never a helper
//!   program, see [`FIRST_CHECK_AFTER`]);
//! - the version is strictly newer than the running one and is the manifest's version;
//! - the zip's SHA-256 equals the manifest's `zip_sha256` AND the published `.sha256` file;
//! - every extracted file has the size and SHA-256 the manifest gives;
//! - every name is `winhttp.dll`, under `mods/release/feverlink3/`, or one of the documents (never installed), with no
//!   `..`, drive, absolute or alternate-stream part ([`update::check_auto_names`], [`update::plan_swap`]).
//!
//! What it does not protect against: whoever can publish a release on the repository (a stolen GitHub account or
//! token) can publish an update every player installs. That is the price of no signature before 1.0.0. As soon as a
//! key is listed in [`update::TRUSTED_KEYS`], [`update::accept_manifest`] requires a good signature again.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::update::{self, Decision, TrustedKey, Version};

/// The version of this module.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// True in a module the release workflow built (`FEVERLINK3_OFFICIAL_BUILD=1`). Only those update themselves: a module
/// built on a developer's PC must never replace itself with GitHub's release, unless [`PRETEND_ENV`] or
/// [`PRETEND_FILE`] asks for exactly that (the real test, `docs/RELEASING.md`).
pub const OFFICIAL_BUILD: bool = match option_env!("FEVERLINK3_OFFICIAL_BUILD") {
    Some(v) => v.len() == 1 && v.as_bytes()[0] == b'1',
    None => false,
};

/// The only address asked for a release.
pub const API_LATEST: &str = "https://api.github.com/repos/AZalshehri7/FeverLink3/releases/latest";

/// Every file of a release must come from below this address followed by the release's tag.
pub const DOWNLOAD_PREFIX: &str = "https://github.com/AZalshehri7/FeverLink3/releases/download/";

/// The folder next to the game (`<game>\feverlink3_update\`): the state file, the staged versions, the backup.
pub const UPDATE_DIR: &str = "feverlink3_update";

const STATE_FILE: &str = "state.json";

/// A test switch: the module believes it is this version when it compares with GitHub (`FEVERLINK3_PRETEND_VERSION=0.1.9`).
pub const PRETEND_ENV: &str = "FEVERLINK3_PRETEND_VERSION";

/// The same switch as a file in the game folder (one line, a version). Deleted once an update was staged, so the
/// updated game does not update again.
pub const PRETEND_FILE: &str = "feverlink3_pretend_version.txt";

/// The setting file, in `%LOCALAPPDATA%\FeverLink3\` (not in `live\`, which is emptied at every start).
pub const SETTINGS_FILE: &str = "settings.txt";

/// How many starts a new version has to stay up for [`HEALTHY_AFTER`] before it is rolled back.
pub const START_ATTEMPTS: u32 = 2;

/// The first update check waits this long after the module loaded, so the game has finished starting (its shader
/// cache, its first files) before anything of the update runs. Real game 2026-10-22, 0.2.0: the check began at once
/// and started `curl.exe`, which inherited the game's open files; 3 s after the start the game's rename of
/// `shader_cache\pipelines.cache.tmp` failed "being used by another process" and the game showed "Internal error".
/// The download now runs inside the process (no child inherits anything), and it also waits.
pub const FIRST_CHECK_AFTER: Duration = Duration::from_secs(120);

/// A player refused by a host whose FeverLink3 is newer ([`check_now`], issue #4: a player on 0.2.7 refused by a 0.2.8
/// host 108 s after the start saw no update, the check waited for 120 s) checks at once, but never before this. The
/// clash of [`FIRST_CHECK_AFTER`] came 3 s after the start, from a child process that no longer exists; 30 s is ten
/// times that, and the same time after which a start counts as good ([`HEALTHY_AFTER`]).
pub const EARLY_CHECK_AFTER: Duration = Duration::from_secs(30);

/// A start counts as good when the game is still running this long after the module loaded.
pub const HEALTHY_AFTER: Duration = Duration::from_secs(30);

const MAX_JSON: u64 = 2 << 20;
const MAX_SMALL: u64 = 4 << 10;
const MAX_ZIP: u64 = 256 << 20;

/// What an earlier run left to do, in `feverlink3_update\state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Pending {
    /// Downloaded and checked; the new DLL is in place, the mod waits in `feverlink3_update\<to>\`.
    Staged { from: String, to: String },
    /// The new DLL started and swapped the mod in; not yet confirmed.
    Started { from: String, to: String, attempts: u32 },
    /// The new version was taken back; the old one says so once.
    RolledBack { from: String, to: String, reason: String },
}

/// The places in the game folder.
#[derive(Debug, Clone)]
pub struct Paths {
    pub game: PathBuf,
}

impl Paths {
    pub fn new(game: &Path) -> Paths {
        Paths { game: game.to_path_buf() }
    }
    fn dir(&self) -> PathBuf {
        self.game.join(UPDATE_DIR)
    }
    fn state(&self) -> PathBuf {
        self.dir().join(STATE_FILE)
    }
    fn stage(&self, to: &str) -> PathBuf {
        self.dir().join(to)
    }
    fn staged_mod(&self, to: &str) -> PathBuf {
        self.stage(to).join(update::AUTO_MOD_DIR)
    }
    fn live_mod(&self) -> PathBuf {
        self.game.join(update::AUTO_MOD_DIR)
    }
    fn backup_mod(&self, from: &str) -> PathBuf {
        self.dir().join(format!("backup-{from}")).join("feverlink3")
    }
    fn failed_mod(&self, to: &str) -> PathBuf {
        self.dir().join(format!("failed-{to}")).join("feverlink3")
    }
    fn dll(&self) -> PathBuf {
        self.game.join(update::DLL_NAME)
    }
    fn dll_new(&self) -> PathBuf {
        self.game.join(format!("{}.new", update::DLL_NAME))
    }
    fn dll_old(&self, from: &str) -> PathBuf {
        self.game.join(format!("{}.old-{from}", update::DLL_NAME))
    }
    fn dll_failed(&self, to: &str) -> PathBuf {
        self.game.join(format!("{}.failed-{to}", update::DLL_NAME))
    }
    fn downloads(&self) -> PathBuf {
        self.dir().join("download")
    }

    pub fn read_state(&self) -> Option<Pending> {
        fs::read(self.state()).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }

    fn write_state(&self, p: &Pending) -> Result<(), String> {
        fs::create_dir_all(self.dir()).map_err(|e| e.to_string())?;
        let json = serde_json::to_vec_pretty(p).map_err(|e| e.to_string())?;
        let tmp = self.dir().join("state.tmp");
        fs::write(&tmp, json).and_then(|()| fs::rename(&tmp, self.state())).map_err(|e| e.to_string())
    }

    /// Removes the state file, then the update folder when nothing else is left in it (after a confirm, a rollback that
    /// was reported, or a dropped update: the folder must not stay behind empty, real game 2026-10-22).
    fn clear_state(&self) {
        let _ = fs::remove_file(self.state());
        self.remove_dir_if_empty();
    }

    /// `remove_dir` refuses a folder that still holds something (a staged version, a backup, a failed copy).
    fn remove_dir_if_empty(&self) {
        let _ = fs::remove_dir(self.dir());
    }
}

// --------------------------------------------------------------------------------------------- at the game's start

/// What [`at_start`] found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// Nothing pending: the update check may run.
    Clear,
    /// This start is the new version's and the mod was swapped in: [`confirm`] after [`HEALTHY_AFTER`].
    Confirm { from: String, to: String },
    /// Something failed in this start and the previous version's files were put back: the DLL in memory and the files
    /// on disk differ, so no session until the game is restarted. `detail`: the technical cause, for `native.log` only.
    Restart { failed: String, kept: String, reason: String, detail: String },
    /// The previous run took a failed version back: say so once.
    Failed { failed: String, kept: String, reason: String },
}

/// Finishes or undoes the update an earlier run left. Called from `DllMain` with the version of the running module:
/// renames only, never the network. Never fails: a problem is the [`Start`] it returns.
pub fn at_start(paths: &Paths, running: &str) -> Start {
    let Some(state) = paths.read_state() else {
        // a state file that cannot be read is dropped with the half-staged files around it
        if paths.state().exists() {
            paths.clear_state();
        }
        return Start::Clear;
    };
    match state {
        Pending::Staged { from, to } if to == running => match swap_mod_in(paths, &from, &to) {
            Ok(()) => {
                let _ = paths.write_state(&Pending::Started { from: from.clone(), to: to.clone(), attempts: 1 });
                Start::Confirm { from, to }
            }
            Err(why) => {
                // the player's words are short (the window's line); the technical cause goes to the log
                let reason = "its files could not be put in place".to_owned();
                rollback(paths, &from, &to, &reason);
                Start::Restart { failed: to, kept: from, reason, detail: why }
            }
        },
        Pending::Started { from, to, attempts } if to == running => {
            if attempts >= START_ATTEMPTS {
                let reason = "it did not start properly".to_owned();
                let detail = format!("{attempts} starts without reaching the healthy mark");
                rollback(paths, &from, &to, &reason);
                Start::Restart { failed: to, kept: from, reason, detail }
            } else {
                let _ =
                    paths.write_state(&Pending::Started { from: from.clone(), to: to.clone(), attempts: attempts + 1 });
                Start::Confirm { from, to }
            }
        }
        Pending::RolledBack { from, to, reason } if from == running => {
            let _ = fs::remove_file(paths.dll_failed(&to));
            let _ = fs::remove_dir_all(paths.dir().join(format!("failed-{to}")));
            let _ = fs::remove_dir_all(paths.stage(&to));
            paths.clear_state();
            Start::Failed { failed: to, kept: from, reason }
        }
        // the running module is not the one the state is about (its DLL swap did not happen, or someone replaced the
        // files by hand): what was staged is dropped and the check runs again
        Pending::Staged { to, .. } | Pending::Started { to, .. } | Pending::RolledBack { to, .. } => {
            let _ = fs::remove_file(paths.dll_new());
            let _ = fs::remove_dir_all(paths.stage(&to));
            paths.clear_state();
            Start::Clear
        }
    }
}

/// The live mod folder moves to the backup, the staged one takes its place; undone when the second rename fails.
fn swap_mod_in(paths: &Paths, from: &str, to: &str) -> Result<(), String> {
    let (live, staged, backup) = (paths.live_mod(), paths.staged_mod(to), paths.backup_mod(from));
    if !staged.is_dir() {
        return Err("the staged files are missing".into());
    }
    let _ = fs::remove_dir_all(&backup);
    if let Some(parent) = backup.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let had_live = live.exists();
    if had_live {
        fs::rename(&live, &backup).map_err(|e| e.to_string())?;
    }
    if let Err(e) = fs::rename(&staged, &live) {
        if had_live {
            let _ = fs::rename(&backup, &live);
        }
        return Err(e.to_string());
    }
    Ok(())
}

/// Puts the previous version back: the mod from its backup (when it was swapped), the DLL from its `.old-<from>` copy
/// (the new one, perhaps loaded right now, is renamed away: Windows allows that). The failed version goes on the skip
/// list and the state says [`Pending::RolledBack`], so the old version tells the player at its next start.
pub fn rollback(paths: &Paths, from: &str, to: &str, reason: &str) {
    let (live, backup, failed) = (paths.live_mod(), paths.backup_mod(from), paths.failed_mod(to));
    if backup.is_dir() {
        let _ = fs::remove_dir_all(&failed);
        if let Some(parent) = failed.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if !live.exists() || fs::rename(&live, &failed).is_ok() {
            let _ = fs::rename(&backup, &live);
        }
        // the backup's own folder is empty now (its `feverlink3` moved back); `remove_dir` keeps it when it is not
        if let Some(parent) = backup.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
    let old = paths.dll_old(from);
    if old.exists() {
        let _ = fs::remove_file(paths.dll_failed(to));
        if fs::rename(paths.dll(), paths.dll_failed(to)).is_ok() || !paths.dll().exists() {
            let _ = fs::rename(&old, paths.dll());
        }
    }
    let _ = update::add_skipped(&paths.game, to);
    let _ = paths.write_state(&Pending::RolledBack { from: from.into(), to: to.into(), reason: reason.into() });
}

/// Makes the update final after a good start: the backup mod, the old DLL, the staging folder and the state go.
pub fn confirm(paths: &Paths, from: &str, to: &str) {
    let _ = fs::remove_dir_all(paths.dir().join(format!("backup-{from}")));
    let _ = fs::remove_file(paths.dll_old(from));
    let _ = fs::remove_dir_all(paths.stage(to));
    let _ = fs::remove_dir_all(paths.downloads());
    paths.clear_state();
}

// ------------------------------------------------------------------------------------------- check and stage

/// One download: the bytes at `url`, at most `max_bytes`.
pub trait Fetch {
    fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String>;
}

/// Downloads inside this process with the system's WinHTTP ([`crate::https`]): HTTPS only, GitHub's hosts only (also
/// after a redirect), TLS 1.2 or newer, the certificate checked by Windows, size and time limits. Never a helper
/// program: a child of the game inherits the game's open files (the crash of 2026-10-22, `crate::https`).
pub struct InProcess;

impl Fetch for InProcess {
    fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
        crate::https::get(url, max_bytes)
    }
}

/// Why an update was not installed, in plain words for the player (the detail goes to the log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub plain: String,
    pub detail: String,
}

fn refused(plain: &str, detail: impl std::fmt::Display) -> Refused {
    Refused { plain: plain.into(), detail: detail.to_string() }
}

/// What [`stage_update`] needs besides the files: the version to compare with (the real one, or a pretended one), the
/// real version (file names), the keys and the clock.
pub struct Check<'a> {
    pub compare_with: &'a str,
    pub running: &'a str,
    pub keys: &'a [TrustedKey],
    pub now_unix: i64,
}

/// Asks GitHub for the newest full release; when it is newer, downloads, checks (the trust model of this module's
/// documentation), stages it in `feverlink3_update\<version>\` and puts the new DLL in place. `downloading` is called
/// with the version once the download starts; `may_touch_files` is asked right before the DLL is swapped (false while
/// a session runs: the swap waits). Returns the staged version, `None` when there is nothing newer.
pub fn stage_update(
    paths: &Paths,
    fetch: &dyn Fetch,
    check: &Check,
    downloading: &mut dyn FnMut(&str),
    may_touch_files: &dyn Fn() -> bool,
) -> Result<Option<String>, Refused> {
    const NET: &str = "GitHub could not be reached";
    // short: the window's line is "Update to <version> failed: <this>. You keep <current>."
    const BAD: &str = "the download did not match the release";
    let json = fetch.get(API_LATEST, MAX_JSON).map_err(|e| refused(NET, e))?;
    let json = String::from_utf8(json).map_err(|e| refused(NET, e))?;
    let release = match update::check_update(check.compare_with, &json, false) {
        Ok(Decision::UpToDate) => return Ok(None),
        Ok(Decision::Available(r)) => *r,
        Err(e) => return Err(refused("the newest release could not be read", e)),
    };
    let to = version_text(&release);
    if update::is_skipped(&update::read_skipped(&paths.game), &release.version) {
        return Ok(None);
    }
    let from_tag = format!("{DOWNLOAD_PREFIX}{}/", release.tag);
    let sha_asset = release.sha256.clone().ok_or_else(|| refused(BAD, "the release has no .sha256 file"))?;
    let mut assets = vec![&release.zip, &release.manifest, &sha_asset];
    assets.extend(release.signature.as_ref());
    if let Some(a) = assets.iter().find(|a| !a.url.starts_with(&from_tag)) {
        return Err(refused(BAD, format!("{} is not a file of the release: {}", a.name, a.url)));
    }
    if release.zip.size > MAX_ZIP {
        return Err(refused(BAD, format!("{} is larger than {MAX_ZIP} bytes", release.zip.name)));
    }
    downloading(&to);

    let manifest_json = fetch.get(&release.manifest.url, MAX_JSON).map_err(|e| refused(NET, e))?;
    let sig = match (&release.signature, check.keys.is_empty()) {
        (Some(a), false) => Some(fetch.get(&a.url, MAX_SMALL).map_err(|e| refused(NET, e))?),
        _ => None,
    };
    let manifest = update::accept_manifest(&manifest_json, sig.as_deref(), check.keys, check.now_unix)
        .map_err(|e| refused("the release is not signed", e))?;
    update::check_manifest_version(&manifest, &release).map_err(|e| refused(BAD, e))?;
    let compare = Version::parse(check.compare_with).map_err(|e| refused(BAD, e))?;
    if release.version <= compare {
        return Ok(None);
    }
    update::check_auto_names(&manifest).map_err(|e| refused(BAD, e))?;
    let zip = fetch.get(&release.zip.url, MAX_ZIP).map_err(|e| refused(NET, e))?;
    let sha_file = fetch.get(&sha_asset.url, MAX_SMALL).map_err(|e| refused(NET, e))?;
    update::verify_zip(&zip, &manifest).map_err(|e| refused(BAD, e))?;
    update::verify_sha256_file(&sha_file, &zip, &manifest).map_err(|e| refused(BAD, e))?;
    let plan = update::plan_swap(&zip, &manifest, &paths.game).map_err(|e| refused(BAD, e))?;
    let stage = paths.stage(&to);
    let _ = fs::remove_dir_all(&stage);
    update::extract_verified(&plan, &zip, &manifest, &stage).map_err(|e| match e {
        update::UpdateError::Io(io) => refused("the files could not be saved", io),
        other => refused(BAD, other),
    })?;

    // the files are checked and staged; the DLL is swapped only outside a session
    while !may_touch_files() {
        std::thread::sleep(Duration::from_secs(2));
    }
    let save = "the files could not be saved";
    paths.write_state(&Pending::Staged { from: check.running.into(), to: to.clone() }).map_err(|e| refused(save, e))?;
    if let Err(e) = swap_dll(paths, check.running, &stage.join(update::DLL_NAME)) {
        paths.clear_state();
        let _ = fs::remove_dir_all(&stage);
        return Err(refused("winhttp.dll could not be replaced", e));
    }
    Ok(Some(to))
}

/// The new DLL into place: copied next to the game as `winhttp.dll.new`, the current one (loaded by this game:
/// Windows allows the rename) renamed to `winhttp.dll.old-<running>`, the new one renamed to `winhttp.dll`. The two
/// renames are back to back; when the second fails the first is undone.
fn swap_dll(paths: &Paths, running: &str, staged_dll: &Path) -> Result<(), String> {
    let (dll, new, old) = (paths.dll(), paths.dll_new(), paths.dll_old(running));
    fs::copy(staged_dll, &new).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(&old);
    let had = dll.exists();
    if had {
        if let Err(e) = fs::rename(&dll, &old) {
            let _ = fs::remove_file(&new);
            return Err(e.to_string());
        }
    }
    if let Err(e) = fs::rename(&new, &dll) {
        if had {
            let _ = fs::rename(&old, &dll);
        }
        let _ = fs::remove_file(&new);
        return Err(e.to_string());
    }
    Ok(())
}

/// The version as the manifest and the folder names write it: the tag without its `v`.
fn version_text(release: &update::Release) -> String {
    release.tag.strip_prefix('v').unwrap_or(&release.tag).to_owned()
}

// --------------------------------------------------------------------------------- the setting and the status

/// Whether the player left automatic updates on (the default). `settings.txt`, one `key=value` per line.
pub fn read_enabled(settings: &Path) -> bool {
    let text = fs::read_to_string(settings).unwrap_or_default();
    !text.lines().any(|l| l.trim().eq_ignore_ascii_case("autoupdate=off"))
}

/// Writes the setting, keeping the file's other lines.
pub fn write_enabled(settings: &Path, on: bool) -> Result<(), String> {
    let text = fs::read_to_string(settings).unwrap_or_default();
    let mut lines: Vec<&str> =
        text.lines().filter(|l| !l.trim().to_ascii_lowercase().starts_with("autoupdate=")).collect();
    lines.push(if on { "autoupdate=on" } else { "autoupdate=off" });
    if let Some(parent) = settings.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(settings, lines.join("\n") + "\n").map_err(|e| e.to_string())
}

/// `%LOCALAPPDATA%\FeverLink3\settings.txt`: next to the live folder (which is emptied at every start), shared by
/// the games of one Windows user.
pub fn settings_path() -> Option<PathBuf> {
    crate::live::live_dir().and_then(|d| d.parent().map(|p| p.join(SETTINGS_FILE)))
}

/// The version to compare with GitHub's: [`PRETEND_ENV`], else [`PRETEND_FILE`] in the game folder, else `None`.
pub fn pretend_version(game: &Path) -> Option<String> {
    let from_env = std::env::var(PRETEND_ENV).ok();
    let from_file = || fs::read_to_string(game.join(PRETEND_FILE)).ok();
    from_env.or_else(from_file).map(|s| s.trim().to_owned()).filter(|s| Version::parse(s).is_ok())
}

/// What the window shows (`update` in `ctl_status.txt`); the words are the mod's (`feverlink3_text.lua`, `T.UPDATE`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    /// "" (nothing to say), "checking" (asked by [`check_now`]), "behind" (the host is newer and no update comes:
    /// download it by hand), "downloading", "ready" (restart to finish), "updated" (this start is the new version),
    /// "failed".
    pub state: String,
    /// The new version.
    pub version: String,
    /// The version the player keeps (failed) or runs.
    pub current: String,
    /// Why it failed, in plain words.
    pub reason: String,
    /// The files on disk no longer match the module in memory: no session until the game is restarted.
    pub restart: bool,
    /// The player's setting.
    pub auto: bool,
    /// False in a module not built by the release workflow (a developer's): it never updates itself.
    pub official: bool,
}

static STATUS: Mutex<Option<Status>> = Mutex::new(None);
static SESSION_BUSY: AtomicBool = AtomicBool::new(false);
static SETTINGS: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Between [`check_now`] and the check thread: `armed` while the thread may still check (on, an official build, no
/// update staged), `asked` when a check is wanted now, `used` once an ask was taken (one per start).
struct Wake {
    armed: bool,
    asked: bool,
    used: bool,
}
static WAKE: Mutex<Wake> = Mutex::new(Wake { armed: false, asked: false, used: false });
static WAKE_CV: Condvar = Condvar::new();

fn wake() -> std::sync::MutexGuard<'static, Wake> {
    WAKE.lock().unwrap_or_else(|p| p.into_inner())
}

/// `host` when that FeverLink3 version is strictly newer than `ours`; an unreadable version is never newer.
pub fn host_newer(host: &str, ours: &str) -> Option<String> {
    let (Ok(h), Ok(o)) = (Version::parse(host), Version::parse(ours)) else { return None };
    (h > o).then(|| host.trim().to_owned())
}

/// The host refused this game and runs FeverLink3 `host`, this game `ours`: when the host's is newer, the update check
/// runs now instead of at [`FIRST_CHECK_AFTER`] (not before [`EARLY_CHECK_AFTER`], still never during a session),
/// at most once per start, also after an earlier check of this start found nothing. True when a check was asked; the
/// window says so, or says to download it by hand when no check comes (turned off, a developer's build, used).
pub fn check_now(host: &str, ours: &str) -> bool {
    let Some(newer) = host_newer(host, ours) else { return false };
    let asked = {
        let mut w = wake();
        let asked = w.armed && !w.used;
        if asked {
            w.asked = true;
            w.used = true;
            WAKE_CV.notify_all();
        }
        asked
    };
    let mut s = STATUS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(s) = s.as_mut().filter(|s| s.state.is_empty() || s.state == "behind") {
        s.state = if asked { "checking" } else { "behind" }.into();
        s.version = newer;
    }
    asked
}

/// How long the check thread still waits, `elapsed` after the load: until [`FIRST_CHECK_AFTER`] for the start's
/// check, until [`EARLY_CHECK_AFTER`] when asked, without end (`None`) for an ask after the start's check.
fn wait_left(elapsed: Duration, asked: bool, checked: bool) -> Option<Duration> {
    match (asked, checked) {
        (true, _) => Some(EARLY_CHECK_AFTER.saturating_sub(elapsed)),
        (false, false) => Some(FIRST_CHECK_AFTER.saturating_sub(elapsed)),
        (false, true) => None,
    }
}

/// Waits for the thread's turn; true when an ask of [`check_now`] started it.
fn wait_turn(loaded: std::time::Instant, checked: bool) -> bool {
    let mut w = wake();
    loop {
        match wait_left(loaded.elapsed(), w.asked, checked) {
            Some(d) if d.is_zero() => break,
            Some(d) => w = WAKE_CV.wait_timeout(w, d).unwrap_or_else(|p| p.into_inner()).0,
            None => w = WAKE_CV.wait(w).unwrap_or_else(|p| p.into_inner()),
        }
    }
    std::mem::take(&mut w.asked)
}

fn set_status(f: impl FnOnce(&mut Status)) {
    let mut s = STATUS.lock().unwrap_or_else(|p| p.into_inner());
    f(s.get_or_insert_with(Status::default));
}

/// The status for the window; `null` when the automatic update is not running in this process.
pub fn status_json() -> Value {
    let s = STATUS.lock().unwrap_or_else(|p| p.into_inner());
    s.as_ref().map_or(Value::Null, |s| json!(s))
}

/// Why a host or join is refused in a run that must restart first (the window shows its own words, `T.UPDATE`).
pub const RESTART_FIRST: &str =
    "FeverLink3 was changed on disk while this game runs: restart the game before you play together";

/// True while this run must not start a session (see [`Status::restart`]).
pub fn restart_needed() -> bool {
    STATUS.lock().unwrap_or_else(|p| p.into_inner()).as_ref().is_some_and(|s| s.restart)
}

/// The session controller says whether a session runs; the update waits for it to end before it touches anything.
pub fn set_session_busy(busy: bool) {
    SESSION_BUSY.store(busy, Ordering::Relaxed);
}

/// The window's switch. Takes effect at the next game start.
pub fn set_enabled(on: bool) -> Result<String, String> {
    let path = SETTINGS.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let path = path.ok_or("automatic updates run only inside the game")?;
    write_enabled(&path, on)?;
    set_status(|s| s.auto = on);
    Ok(if on { "automatic updates are on" } else { "automatic updates are off" }.to_owned())
}

/// The game-side entry: [`at_start`] now (called from `DllMain`), then the check on a thread of its own. `game`: the
/// folder of `TransportFever3.exe`; `settings`: the [`SETTINGS_FILE`]; `log`: a line for `native.log`.
pub fn start(game: PathBuf, settings: PathBuf, log: fn(&str)) {
    let paths = Paths::new(&game);
    let enabled = read_enabled(&settings);
    *SETTINGS.lock().unwrap_or_else(|p| p.into_inner()) = Some(settings);
    let begun = at_start(&paths, VERSION);
    log(&format!("update: at start {begun:?} (official build {OFFICIAL_BUILD}, automatic updates {enabled})"));
    set_status(|s| {
        s.auto = enabled;
        s.official = OFFICIAL_BUILD;
        s.current = VERSION.into();
        match &begun {
            Start::Restart { failed, kept, reason, .. } | Start::Failed { failed, kept, reason } => {
                s.state = "failed".into();
                s.version = failed.clone();
                s.current = kept.clone();
                s.reason = reason.clone();
                s.restart = matches!(begun, Start::Restart { .. });
            }
            Start::Confirm { .. } | Start::Clear => {}
        }
    });
    let loaded = std::time::Instant::now();
    let _ = std::thread::Builder::new().name("feverlink3-update".into()).spawn(move || {
        let _ = crate::guard::guarded(|| run(paths, begun, enabled, loaded, log));
    });
}

fn run(paths: Paths, begun: Start, enabled: bool, loaded: std::time::Instant, log: fn(&str)) {
    match begun {
        Start::Confirm { from, to } => {
            std::thread::sleep(HEALTHY_AFTER);
            confirm(&paths, &from, &to);
            log(&format!("update: {to} started well; the backup of {from} was removed"));
            set_status(|s| {
                s.state = "updated".into();
                s.version = to;
            });
            return;
        }
        Start::Restart { .. } => return,
        Start::Clear | Start::Failed { .. } => {}
    }
    let pretend = pretend_version(&paths.game);
    if !enabled || !(OFFICIAL_BUILD || pretend.is_some()) {
        log("update: no check (turned off, or a module not built by the release workflow)");
        return;
    }
    // an update the developer program installed and that waits for its confirmation comes first
    if update::Journal::read(&paths.game).ok().flatten().is_some() {
        log("update: no check (an update of the feverlink3 program waits for --confirm or --rollback)");
        return;
    }
    let compare_with = pretend.clone().unwrap_or_else(|| VERSION.to_owned());
    wake().armed = true;
    let mut checked = false;
    // the start's check, then at most one more when a newer host refuses us after it (check_now)
    while check_once(&paths, &compare_with, pretend.is_some(), loaded, checked, log) {
        checked = true;
    }
    wake().armed = false;
}

/// One check: waits for its turn and for the end of a session, then asks GitHub. True when the thread should stay
/// for an ask of [`check_now`] (nothing was staged and this check was not the asked one).
fn check_once(
    paths: &Paths,
    compare_with: &str,
    pretend: bool,
    loaded: std::time::Instant,
    checked: bool,
    log: fn(&str),
) -> bool {
    // the game first finishes starting (FIRST_CHECK_AFTER); `start` runs in DllMain, so the clock starts at the load
    let asked = wait_turn(loaded, checked);
    if asked {
        log("update: checking now: the host runs a newer FeverLink3");
    }
    while SESSION_BUSY.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(5));
    }
    let check = Check { compare_with, running: VERSION, keys: update::TRUSTED_KEYS, now_unix: update::now_unix() };
    let fetch = InProcess;
    let mut downloading = |v: &str| {
        log(&format!("update: downloading {v} (compared with {compare_with})"));
        set_status(|s| {
            s.state = "downloading".into();
            s.version = v.into();
        });
    };
    let idle = || !SESSION_BUSY.load(Ordering::Relaxed);
    let result = stage_update(paths, &fetch, &check, &mut downloading, &idle);
    let _ = fs::remove_dir_all(paths.downloads());
    paths.remove_dir_if_empty();
    // a check asked by check_now that brings nothing: the window says to download it by hand
    let nothing = || {
        set_status(|s| {
            if asked && s.state == "checking" {
                s.state = "behind".into();
            }
        })
    };
    match result {
        Ok(None) => {
            log("update: up to date");
            nothing();
        }
        Ok(Some(to)) => {
            if pretend {
                let _ = fs::remove_file(paths.game.join(PRETEND_FILE));
            }
            log(&format!("update: {to} staged and winhttp.dll replaced; the mod is swapped at the next start"));
            set_status(|s| {
                s.state = "ready".into();
                s.version = to;
            });
            return false;
        }
        Err(r) => {
            log(&format!("update: refused or failed: {} ({})", r.plain, r.detail));
            set_status(|s| {
                // a failed check before any download says nothing: the player loses nothing
                if s.state == "downloading" {
                    s.state = "failed".into();
                    s.reason = r.plain;
                }
            });
            nothing();
        }
    }
    !asked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::{sha256_hex, signature_text, FileEntry, Manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::HashMap;
    use std::io::{Cursor, Write};

    /// A release on a fake GitHub: URL -> bytes.
    struct FakeGitHub(HashMap<String, Vec<u8>>);

    impl Fetch for FakeGitHub {
        fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
            let b = self.0.get(url).cloned().ok_or_else(|| format!("404 {url}"))?;
            if b.len() as u64 > max_bytes {
                return Err("too large".into());
            }
            Ok(b)
        }
    }

    const NEW: &[(&str, &[u8])] = &[
        ("winhttp.dll", b"new dll"),
        ("mods/release/feverlink3/mod.lua", b"new mod"),
        ("mods/release/feverlink3/content/a.lua", b"new a"),
        ("README.txt", b"readme"),
    ];

    fn zip_of(version: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in files {
            w.start_file(format!("FeverLink3-{version}/{name}"), zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn manifest_of(version: &str, files: &[(&str, &[u8])], zip: &[u8]) -> Manifest {
        Manifest {
            version: version.into(),
            files: files
                .iter()
                .map(|(p, d)| FileEntry { path: (*p).into(), size: d.len() as u64, sha256: sha256_hex(d) })
                .collect(),
            zip_name: format!("FeverLink3-{version}.zip"),
            zip_sha256: sha256_hex(zip),
        }
    }

    fn url(tag: &str, name: &str) -> String {
        format!("{DOWNLOAD_PREFIX}{tag}/{name}")
    }

    /// GitHub with release `version` of `files`; `edit` may change the manifest before it is published.
    fn github(
        version: &str,
        files: &[(&str, &[u8])],
        signer: Option<&SigningKey>,
        edit: impl Fn(&mut Manifest),
    ) -> FakeGitHub {
        github_with_zip(version, files, zip_of(version, files), signer, edit)
    }

    /// The same with a zip made by the caller; the manifest's hashes are the zip's and `files`'.
    fn github_with_zip(
        version: &str,
        files: &[(&str, &[u8])],
        zip: Vec<u8>,
        signer: Option<&SigningKey>,
        edit: impl Fn(&mut Manifest),
    ) -> FakeGitHub {
        let tag = format!("v{version}");
        let mut m = manifest_of(version, files, &zip);
        edit(&mut m);
        let mjson = serde_json::to_vec_pretty(&m).unwrap();
        let zip_name = format!("FeverLink3-{version}.zip");
        let mut assets = vec![
            json!({"name": zip_name, "size": zip.len(), "browser_download_url": url(&tag, &zip_name)}),
            json!({"name": format!("{zip_name}.sha256"), "size": 90, "browser_download_url": url(&tag, &format!("{zip_name}.sha256"))}),
            json!({"name": "manifest.json", "size": mjson.len(), "browser_download_url": url(&tag, "manifest.json")}),
        ];
        let mut map = HashMap::new();
        if let Some(k) = signer {
            let sig = signature_text(&k.verifying_key().to_bytes(), &k.sign(&mjson).to_bytes());
            assets.push(json!({"name": "manifest.json.sig", "size": sig.len(), "browser_download_url": url(&tag, "manifest.json.sig")}));
            map.insert(url(&tag, "manifest.json.sig"), sig.into_bytes());
        }
        let release = json!({"tag_name": tag, "draft": false, "prerelease": false, "assets": assets});
        map.insert(API_LATEST.to_owned(), release.to_string().into_bytes());
        map.insert(
            url(&tag, &format!("{zip_name}.sha256")),
            format!("{}  {zip_name}\n", sha256_hex(&zip)).into_bytes(),
        );
        map.insert(url(&tag, "manifest.json"), mjson);
        map.insert(url(&tag, &zip_name), zip);
        FakeGitHub(map)
    }

    /// A game folder with the old version installed.
    fn game() -> (tempfile::TempDir, Paths) {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("TransportFever3.exe"), b"game").unwrap();
        fs::write(t.path().join("winhttp.dll"), b"old dll").unwrap();
        fs::create_dir_all(t.path().join("mods/release/feverlink3")).unwrap();
        fs::write(t.path().join("mods/release/feverlink3/mod.lua"), b"old mod").unwrap();
        fs::create_dir_all(t.path().join("mods/release/other")).unwrap();
        fs::write(t.path().join("mods/release/other/mod.lua"), b"other mod").unwrap();
        let p = Paths::new(t.path());
        (t, p)
    }

    fn read(p: &Paths, rel: &str) -> Vec<u8> {
        fs::read(p.game.join(rel)).unwrap_or_default()
    }

    const OLD: &str = "0.2.0";
    const UNSIGNED: Check = Check { compare_with: OLD, running: OLD, keys: &[], now_unix: 1_792_411_200 };

    fn stage(p: &Paths, gh: &FakeGitHub, check: &Check) -> Result<Option<String>, Refused> {
        stage_update(p, gh, check, &mut |_| {}, &|| true)
    }

    fn untouched(p: &Paths) {
        assert_eq!(read(p, "winhttp.dll"), b"old dll");
        assert_eq!(read(p, "mods/release/feverlink3/mod.lua"), b"old mod");
        assert_eq!(p.read_state(), None);
    }

    #[test]
    fn unsigned_update_is_staged_then_finished_at_the_next_start_then_confirmed() {
        let (_t, p) = game();
        let gh = github("0.2.1", NEW, None, |_| {});
        assert_eq!(stage(&p, &gh, &UNSIGNED), Ok(Some("0.2.1".into())));
        // in this run: the DLL is swapped, the mod waits (the game has read the old one)
        assert_eq!(read(&p, "winhttp.dll"), b"new dll");
        assert_eq!(read(&p, "winhttp.dll.old-0.2.0"), b"old dll");
        assert_eq!(read(&p, "mods/release/feverlink3/mod.lua"), b"old mod");
        assert_eq!(p.read_state(), Some(Pending::Staged { from: OLD.into(), to: "0.2.1".into() }));
        assert!(!p.game.join("README.txt").exists(), "documents are not installed");

        // the next start loads the new DLL, which swaps the mod before the game reads it
        assert_eq!(at_start(&p, "0.2.1"), Start::Confirm { from: OLD.into(), to: "0.2.1".into() });
        assert_eq!(read(&p, "mods/release/feverlink3/mod.lua"), b"new mod");
        assert_eq!(read(&p, "mods/release/feverlink3/content/a.lua"), b"new a");
        assert_eq!(read(&p, "feverlink3_update/backup-0.2.0/feverlink3/mod.lua"), b"old mod");
        assert_eq!(read(&p, "mods/release/other/mod.lua"), b"other mod", "another mod is never touched");

        confirm(&p, OLD, "0.2.1");
        assert!(!p.game.join("winhttp.dll.old-0.2.0").exists());
        assert!(!p.game.join("feverlink3_update/backup-0.2.0").exists());
        assert!(!p.game.join("feverlink3_update/0.2.1").exists());
        assert_eq!(p.read_state(), None);
        assert!(!p.dir().exists(), "the empty update folder goes too");
        assert_eq!(at_start(&p, "0.2.1"), Start::Clear);
    }

    #[test]
    fn nothing_newer_changes_nothing() {
        let (_t, p) = game();
        for v in ["0.2.0", "0.1.9"] {
            assert_eq!(stage(&p, &github(v, NEW, None, |_| {}), &UNSIGNED), Ok(None), "{v}");
        }
        untouched(&p);
        // pretending to be older (the real test) makes the same release newer
        let pretend = Check { compare_with: "0.1.9", ..UNSIGNED };
        assert_eq!(stage(&p, &github("0.2.0", NEW, None, |_| {}), &pretend), Ok(Some("0.2.0".into())));
    }

    #[test]
    fn hash_mismatches_are_refused_and_nothing_changes() {
        let (_t, p) = game();
        // a file whose content is not what the manifest says
        let bad_file = github("0.2.1", NEW, None, |m| m.files[1].sha256 = sha256_hex(b"other"));
        // the manifest's zip hash is wrong (the .sha256 file has the real one)
        let bad_zip = github("0.2.1", NEW, None, |m| m.zip_sha256 = sha256_hex(b"other"));
        // a lying size
        let bad_size = github("0.2.1", NEW, None, |m| m.files[0].size += 1);
        for gh in [bad_file, bad_zip, bad_size] {
            let r = stage(&p, &gh, &UNSIGNED).unwrap_err();
            assert_eq!(r.plain, "the download did not match the release", "{r:?}");
            untouched(&p);
        }
        // the published .sha256 disagrees with zip and manifest
        let mut gh = github("0.2.1", NEW, None, |_| {});
        gh.0.insert(
            url("v0.2.1", "FeverLink3-0.2.1.zip.sha256"),
            format!("{}  FeverLink3-0.2.1.zip", sha256_hex(b"x")).into(),
        );
        assert!(stage(&p, &gh, &UNSIGNED).is_err());
        untouched(&p);
        // no .sha256 at all
        let mut gh = github("0.2.1", NEW, None, |_| {});
        let api = String::from_utf8(gh.0[API_LATEST].clone()).unwrap().replace(".zip.sha256", ".zip.txt");
        gh.0.insert(API_LATEST.into(), api.into_bytes());
        assert!(stage(&p, &gh, &UNSIGNED).is_err());
        untouched(&p);
    }

    #[test]
    fn path_traversal_and_foreign_files_are_refused() {
        let (_t, p) = game();
        for evil in ["mods/release/feverlink3/../../../evil.dll", "mods/release/other/mod.lua", "TransportFever3.exe"] {
            let files: Vec<(&str, &[u8])> = vec![("winhttp.dll", b"new dll"), (evil, b"evil")];
            let r = stage(&p, &github("0.2.1", &files, None, |_| {}), &UNSIGNED);
            assert!(r.is_err(), "{evil}");
            untouched(&p);
            assert_eq!(read(&p, "TransportFever3.exe"), b"game");
            assert!(!p.game.parent().unwrap().join("evil.dll").exists());
        }
        // zip entries outside the release folder, in a zip whose hashes are all correct (zip slip)
        for evil in
            ["../evil.dll", "FeverLink3-0.2.1/../evil.dll", "FeverLink3-0.2.1/mods/../../evil.dll", "C:/evil.dll"]
        {
            let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (name, data) in
                NEW.iter().map(|(n, d)| (format!("FeverLink3-0.2.1/{n}"), *d)).chain([(evil.to_owned(), &b"x"[..])])
            {
                w.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
            let gh = github_with_zip("0.2.1", NEW, w.finish().unwrap().into_inner(), None, |_| {});
            let r = stage(&p, &gh, &UNSIGNED).unwrap_err();
            assert_eq!(r.plain, "the download did not match the release", "{evil}: {r:?}");
            untouched(&p);
            assert!(!p.game.parent().unwrap().join("evil.dll").exists());
        }
    }

    #[test]
    fn files_must_come_from_the_release_and_never_over_http() {
        let (_t, p) = game();
        let gh = github("0.2.1", NEW, None, |_| {});
        let api = String::from_utf8(gh.0[API_LATEST].clone()).unwrap();
        for (from, to) in [
            (
                "https://github.com/AZalshehri7/FeverLink3/releases/download/v0.2.1/manifest.json",
                "https://evil.example/manifest.json",
            ),
            (
                "https://github.com/AZalshehri7/FeverLink3/releases/download/v0.2.1/manifest.json",
                "http://github.com/AZalshehri7/FeverLink3/releases/download/v0.2.1/manifest.json",
            ),
            (
                "https://github.com/AZalshehri7/FeverLink3/releases/download/v0.2.1/FeverLink3-0.2.1.zip\"",
                "https://github.com/AZalshehri7/FeverLink3/releases/download/v0.2.0/FeverLink3-0.2.1.zip\"",
            ),
        ] {
            let mut g = FakeGitHub(gh.0.clone());
            g.0.insert(API_LATEST.into(), api.replace(from, to).into_bytes());
            assert!(stage(&p, &g, &UNSIGNED).is_err(), "{to}");
            untouched(&p);
        }
        // a pre-release or a draft is never taken, even when GitHub would list it
        for flag in ["\"prerelease\":true", "\"draft\":true"] {
            let mut g = FakeGitHub(gh.0.clone());
            let field = flag.split(':').next().unwrap();
            g.0.insert(API_LATEST.into(), api.replace(&format!("{field}:false"), flag).into_bytes());
            assert_eq!(stage(&p, &g, &UNSIGNED), Ok(None), "{flag}");
        }
    }

    #[test]
    fn signed_mode_requires_the_signature_of_a_listed_key() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let public: &'static str = Box::leak(
            key.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>().into_boxed_str(),
        );
        let keys = [TrustedKey { name: "test", public_hex: public, expires: (2099, 1, 1) }];
        let signed = Check { keys: &keys, ..UNSIGNED };
        let (_t, p) = game();
        // an unsigned release is refused once a key is listed
        let r = stage(&p, &github("0.2.1", NEW, None, |_| {}), &signed).unwrap_err();
        assert_eq!(r.plain, "the release is not signed");
        untouched(&p);
        // signed by another key
        let other = SigningKey::from_bytes(&[8u8; 32]);
        assert!(stage(&p, &github("0.2.1", NEW, Some(&other), |_| {}), &signed).is_err());
        untouched(&p);
        // signed by the listed key
        assert_eq!(stage(&p, &github("0.2.1", NEW, Some(&key), |_| {}), &signed), Ok(Some("0.2.1".into())));
    }

    #[test]
    fn a_version_that_does_not_start_is_rolled_back_and_skipped() {
        let (_t, p) = game();
        stage(&p, &github("0.2.1", NEW, None, |_| {}), &UNSIGNED).unwrap();
        // two starts of the new version that never reached the healthy mark
        assert!(matches!(at_start(&p, "0.2.1"), Start::Confirm { .. }));
        assert!(matches!(at_start(&p, "0.2.1"), Start::Confirm { .. }));
        // the third start takes it back: this run (new DLL in memory, old files on disk) must not play
        let r = at_start(&p, "0.2.1");
        assert!(matches!(&r, Start::Restart { failed, kept, .. } if failed == "0.2.1" && kept == OLD), "{r:?}");
        assert_eq!(read(&p, "winhttp.dll"), b"old dll");
        assert_eq!(read(&p, "mods/release/feverlink3/mod.lua"), b"old mod");
        assert!(update::is_skipped(&update::read_skipped(&p.game), &Version::parse("0.2.1").unwrap()));
        // the old version starts and says so once
        assert!(matches!(at_start(&p, OLD), Start::Failed { .. }));
        assert!(!p.dir().exists(), "the empty update folder goes after the rollback was reported");
        assert_eq!(at_start(&p, OLD), Start::Clear);
        assert!(!p.game.join("winhttp.dll.failed-0.2.1").exists());
        // the skipped version is not offered again
        assert_eq!(stage(&p, &github("0.2.1", NEW, None, |_| {}), &UNSIGNED), Ok(None));
        untouched(&p);
    }

    #[test]
    fn a_missing_staged_mod_rolls_back_at_once() {
        let (_t, p) = game();
        stage(&p, &github("0.2.1", NEW, None, |_| {}), &UNSIGNED).unwrap();
        fs::remove_dir_all(p.staged_mod("0.2.1")).unwrap();
        assert!(matches!(at_start(&p, "0.2.1"), Start::Restart { .. }));
        assert_eq!(read(&p, "winhttp.dll"), b"old dll");
        assert_eq!(read(&p, "mods/release/feverlink3/mod.lua"), b"old mod");
    }

    #[test]
    fn a_staged_update_the_running_module_does_not_belong_to_is_dropped() {
        let (_t, p) = game();
        stage(&p, &github("0.2.1", NEW, None, |_| {}), &UNSIGNED).unwrap();
        // the old DLL started after all (someone put it back): the staged files go, the check runs again
        assert_eq!(at_start(&p, OLD), Start::Clear);
        assert_eq!(p.read_state(), None);
        assert!(!p.stage("0.2.1").exists());
        assert!(!p.dir().exists());
        assert_eq!(read(&p, "mods/release/feverlink3/mod.lua"), b"old mod");
        // a damaged state file is dropped too; a folder that still holds something stays
        fs::create_dir_all(p.dir().join("other")).unwrap();
        fs::write(p.state(), b"{not json").unwrap();
        assert_eq!(at_start(&p, OLD), Start::Clear);
        assert!(!p.state().exists());
        assert!(p.dir().join("other").exists());
    }

    #[test]
    fn the_dll_waits_while_a_session_runs() {
        let (_t, p) = game();
        let asked = std::cell::Cell::new(0);
        let busy_twice = || {
            asked.set(asked.get() + 1);
            asked.get() > 2
        };
        let gh = github("0.2.1", NEW, None, |_| {});
        let mut seen = String::new();
        let r = stage_update(&p, &gh, &UNSIGNED, &mut |v| seen = v.to_owned(), &busy_twice);
        assert_eq!(r, Ok(Some("0.2.1".into())));
        assert_eq!(seen, "0.2.1");
        assert_eq!(asked.get(), 3);
    }

    #[test]
    fn the_setting_defaults_to_on_and_keeps_other_lines() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("FeverLink3").join(SETTINGS_FILE);
        assert!(read_enabled(&f));
        write_enabled(&f, false).unwrap();
        assert!(!read_enabled(&f));
        fs::write(&f, "other=1\nautoupdate=off\n").unwrap();
        write_enabled(&f, true).unwrap();
        assert!(read_enabled(&f));
        assert_eq!(fs::read_to_string(&f).unwrap(), "other=1\nautoupdate=on\n");
    }

    #[test]
    fn a_newer_host_asks_for_one_early_check() {
        assert_eq!(host_newer("0.2.8", "0.2.7"), Some("0.2.8".into()));
        assert_eq!(host_newer("0.10.0", "0.9.9"), Some("0.10.0".into()), "numbers, not text");
        for (host, ours) in
            [("0.2.7", "0.2.8"), ("0.2.8", "0.2.8"), ("", "0.2.8"), ("dev", "0.2.8"), ("0.2.8-pre1", "0.2.8")]
        {
            assert_eq!(host_newer(host, ours), None, "{host} against {ours}");
        }
        // updates off or a developer's build: the thread never arms, nothing is asked
        assert!(!check_now("0.2.8", "0.2.7"));
        wake().armed = true;
        // a refusal for other mods (same FeverLink3) or by an older host asks nothing
        assert!(!check_now("0.2.8", "0.2.8") && !check_now("0.2.6", "0.2.7"));
        assert!(!wake().asked);
        assert!(check_now("0.2.8", "0.2.7"));
        assert!(wake().asked);
        assert!(!check_now("0.2.8", "0.2.7"), "one ask per start");
        *wake() = Wake { armed: false, asked: false, used: false };
    }

    #[test]
    fn an_early_check_keeps_its_floor() {
        let s = Duration::from_secs;
        assert_eq!(wait_left(s(10), false, false), Some(s(110)), "the start's check");
        assert_eq!(wait_left(s(10), true, false), Some(s(20)), "asked: not before EARLY_CHECK_AFTER");
        assert_eq!(wait_left(s(108), true, false), Some(s(0)), "asked later: at once");
        assert_eq!(wait_left(s(200), false, true), None, "after the start's check: only an ask");
        assert_eq!(wait_left(s(200), true, true), Some(s(0)));
    }
}
