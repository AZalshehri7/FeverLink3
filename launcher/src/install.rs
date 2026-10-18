//! The game-side footprint: what the launcher puts on the player's PC and takes away again.
//!
//! `docs/ARCHITECTURE.md` section 11 promises three things and one button to remove them:
//!
//! 1. the native module, copied into the game folder as `winhttp.dll` (the game imports `WINHTTP.dll` and
//!    Windows searches the application folder first, so the game loads it itself);
//! 2. the mod, mirrored into the user mods folder as `mods/feverlink3/`;
//! 3. `feverlink3_path.txt` next to the game executable, holding the launcher's own path so a game started by
//!    Steam can find the launcher.
//!
//! Plus `steam_appid.txt` with the app id in the game folder, which Steam's API needs when the executable is
//! started directly rather than through Steam (finding from MPFever, MIT). It is only created when absent and
//! only removed when it holds exactly our app id.
//!
//! Rules: never overwrite a `winhttp.dll` that is not ours (another mod's proxy); never touch anything outside
//! these four paths (plus an old copy of the mod under an earlier folder name, [`crate::LEGACY_MOD_IDS`], which
//! install and uninstall remove so the game does not load two copies); every operation is idempotent; the result is a [`Report`] that says what happened to each.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::steam::mods_dir;
use crate::{APP_ID, LEGACY_MOD_IDS, MOD_ID, NATIVE_MARKER};

/// File name of the native module in the game folder.
pub const DLL_NAME: &str = "winhttp.dll";
/// Steam app id file in the game folder.
pub const APPID_FILE: &str = "steam_appid.txt";
/// File next to the game executable that holds the launcher's path.
pub const PATH_FILE: &str = "feverlink3_path.txt";

/// What the launcher has to offer for installation.
#[derive(Debug, Clone, Copy)]
pub struct Payload<'a> {
    /// The mod folder to mirror into `mods/feverlink3/`. `None`: not shipped with this launcher.
    pub mod_source: Option<&'a Path>,
    /// The native module file to copy as `winhttp.dll`. `None`: not shipped with this launcher.
    pub native_dll: Option<&'a Path>,
    /// What to write into `feverlink3_path.txt`: the launcher executable's own path.
    pub launcher_path: &'a Path,
}

/// One of the installed pieces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Mod,
    NativeDll,
    PathFile,
    AppIdFile,
}

impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Item::Mod => "mod",
            Item::NativeDll => "winhttp.dll",
            Item::PathFile => PATH_FILE,
            Item::AppIdFile => APPID_FILE,
        })
    }
}

/// What happened to one piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Changed (or, in a dry run, would be changed).
    Done,
    /// Nothing to do: already in the wanted state, or nothing to install.
    Skipped,
    /// Wanted but not possible; `reason` says why.
    Failed,
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub item: Item,
    pub path: PathBuf,
    pub outcome: Outcome,
    pub reason: String,
}

/// What an [`install`] or [`uninstall`] did, or would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub dry_run: bool,
    pub steps: Vec<Step>,
}

impl Report {
    /// True when no step failed.
    pub fn ok(&self) -> bool {
        self.steps.iter().all(|s| s.outcome != Outcome::Failed)
    }

    /// The steps that failed.
    pub fn failures(&self) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(|s| s.outcome == Outcome::Failed)
    }

    fn push(&mut self, item: Item, path: impl Into<PathBuf>, outcome: Outcome, reason: impl Into<String>) {
        self.steps.push(Step { item, path: path.into(), outcome, reason: reason.into() });
    }

    /// Records the result of an action: `Ok` is Done, `Err` is Failed with the error as reason.
    fn record(&mut self, item: Item, path: &Path, done: &str, result: io::Result<()>) {
        match result {
            Ok(()) => self.push(item, path, Outcome::Done, done),
            Err(e) => self.push(item, path, Outcome::Failed, format!("{done}: {e}")),
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for s in &self.steps {
            let tag = match (s.outcome, self.dry_run) {
                (Outcome::Done, true) => "WOULD ",
                (Outcome::Done, false) => "DONE  ",
                (Outcome::Skipped, _) => "SKIP  ",
                (Outcome::Failed, _) => "FAILED",
            };
            if s.path.as_os_str().is_empty() {
                writeln!(f, "{tag} {:<20} {}", s.item.to_string(), s.reason)?;
            } else {
                writeln!(f, "{tag} {:<20} {}  [{}]", s.item.to_string(), s.reason, s.path.display())?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Inspection
// ---------------------------------------------------------------------------------------------------------------

/// What is at `<game>/winhttp.dll`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DllState {
    Absent,
    /// Carries [`NATIVE_MARKER`]: ours, safe to replace or remove.
    Ours,
    /// Someone else's proxy. Never overwritten, never removed.
    Foreign,
}

impl fmt::Display for DllState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DllState::Absent => "absent",
            DllState::Ours => "ours",
            DllState::Foreign => "foreign",
        })
    }
}

/// True when `needle` occurs in `haystack`.
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Classifies DLL bytes by the marker string. Finding from MPFever (MIT): a marker in the module's own bytes is
/// what tells a mod's proxy from the system `winhttp.dll` or another mod's.
pub fn classify_dll(bytes: &[u8]) -> DllState {
    if contains(bytes, NATIVE_MARKER) {
        DllState::Ours
    } else {
        DllState::Foreign
    }
}

/// Classifies the `winhttp.dll` in a game folder.
pub fn dll_state(game_dir: &Path) -> io::Result<DllState> {
    match fs::read(game_dir.join(DLL_NAME)) {
        Ok(bytes) => Ok(classify_dll(&bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(DllState::Absent),
        Err(e) => Err(e),
    }
}

/// The installed mod folder of a per-user folder.
pub fn mod_dir(user_local_dir: &Path) -> PathBuf {
    mods_dir(user_local_dir).join(MOD_ID)
}

/// True when the mod folder exists in this per-user folder.
pub fn mod_installed(user_local_dir: &Path) -> bool {
    mod_dir(user_local_dir).is_dir()
}

/// What `steam_appid.txt` holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppIdState {
    Absent,
    Ours,
    Other(String),
}

/// Classifies the `steam_appid.txt` of a game folder.
pub fn appid_state(game_dir: &Path) -> io::Result<AppIdState> {
    match fs::read_to_string(game_dir.join(APPID_FILE)) {
        Ok(text) if text.trim() == APP_ID => Ok(AppIdState::Ours),
        Ok(text) => Ok(AppIdState::Other(text.trim().to_string())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(AppIdState::Absent),
        Err(e) => Err(e),
    }
}

/// The launcher path recorded next to the game executable, if any.
pub fn recorded_launcher_path(game_dir: &Path) -> Option<PathBuf> {
    fs::read_to_string(game_dir.join(PATH_FILE)).ok().map(|s| PathBuf::from(s.trim()))
}

// ---------------------------------------------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------------------------------------------

/// Installs the footprint. Idempotent: a second call with the same payload changes nothing and says so.
/// `dry_run` computes the same report without touching anything.
pub fn install(game_dir: &Path, user_dirs: &[PathBuf], payload: Payload<'_>, dry_run: bool) -> Report {
    let mut report = Report { dry_run, steps: Vec::new() };
    install_mod(&mut report, user_dirs, payload.mod_source, dry_run);
    install_appid(&mut report, game_dir, dry_run);
    install_dll(&mut report, game_dir, payload.native_dll, dry_run);
    install_path_file(&mut report, game_dir, payload.launcher_path, dry_run);
    report
}

fn install_mod(report: &mut Report, user_dirs: &[PathBuf], source: Option<&Path>, dry_run: bool) {
    let Some(src) = source else {
        report.push(Item::Mod, "", Outcome::Skipped, "no mod folder shipped with this launcher");
        return;
    };
    if !src.is_dir() {
        report.push(Item::Mod, src, Outcome::Failed, "mod source folder is missing");
        return;
    }
    if user_dirs.is_empty() {
        report.push(
            Item::Mod,
            "",
            Outcome::Skipped,
            format!(
                "no Steam user folder userdata/<id>/{APP_ID}/local exists; start the game once, then install again"
            ),
        );
        return;
    }
    for user_dir in user_dirs {
        let dst = mod_dir(user_dir);
        match trees_equal(src, &dst) {
            Ok(true) => report.push(Item::Mod, &dst, Outcome::Skipped, "already up to date"),
            Ok(false) => {
                let verb = if dst.exists() { "updated (mirror copy)" } else { "installed" };
                let result = if dry_run { Ok(()) } else { mirror_copy(src, &dst) };
                report.record(Item::Mod, &dst, verb, result);
            }
            Err(e) => report.push(Item::Mod, &dst, Outcome::Failed, format!("could not compare: {e}")),
        }
        remove_legacy_mods(report, user_dir, dry_run);
    }
}

/// The folders of earlier names of the mod ([`LEGACY_MOD_IDS`]) that exist in this per-user folder.
pub fn legacy_mod_dirs(user_local_dir: &Path) -> Vec<PathBuf> {
    LEGACY_MOD_IDS.iter().map(|id| mods_dir(user_local_dir).join(id)).filter(|d| d.is_dir()).collect()
}

/// Removes an old copy of the mod under an earlier name, so the game does not load it next to the current one.
/// Only reported when such a folder exists.
fn remove_legacy_mods(report: &mut Report, user_dir: &Path, dry_run: bool) {
    for old in legacy_mod_dirs(user_dir) {
        let result = if dry_run { Ok(()) } else { fs::remove_dir_all(&old) };
        report.record(Item::Mod, &old, &format!("removed the old mod folder (renamed to {MOD_ID})"), result);
    }
}

fn install_appid(report: &mut Report, game_dir: &Path, dry_run: bool) {
    let path = game_dir.join(APPID_FILE);
    match appid_state(game_dir) {
        Ok(AppIdState::Absent) => {
            let result = if dry_run { Ok(()) } else { fs::write(&path, APP_ID) };
            report.record(Item::AppIdFile, &path, "created", result);
        }
        Ok(AppIdState::Ours) => report.push(Item::AppIdFile, &path, Outcome::Skipped, "already present"),
        Ok(AppIdState::Other(content)) => report.push(
            Item::AppIdFile,
            &path,
            Outcome::Skipped,
            format!("already present with content {content:?}; left alone"),
        ),
        Err(e) => report.push(Item::AppIdFile, &path, Outcome::Failed, format!("could not read: {e}")),
    }
}

fn install_dll(report: &mut Report, game_dir: &Path, source: Option<&Path>, dry_run: bool) {
    let dst = game_dir.join(DLL_NAME);
    let Some(src) = source else {
        report.push(Item::NativeDll, &dst, Outcome::Skipped, "no native module shipped with this launcher");
        return;
    };
    let src_bytes = match fs::read(src) {
        Ok(b) => b,
        Err(e) => {
            report.push(Item::NativeDll, src, Outcome::Failed, format!("could not read the native module: {e}"));
            return;
        }
    };
    if classify_dll(&src_bytes) != DllState::Ours {
        report.push(
            Item::NativeDll,
            src,
            Outcome::Failed,
            format!(
                "the shipped module lacks the marker {:?}; refusing to install an unidentifiable DLL",
                String::from_utf8_lossy(NATIVE_MARKER)
            ),
        );
        return;
    }
    let present = match fs::read(&dst) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            report.push(Item::NativeDll, &dst, Outcome::Failed, format!("could not read: {e}"));
            return;
        }
    };
    let verb = match present {
        None => "installed",
        Some(ref cur) if *cur == src_bytes => {
            report.push(Item::NativeDll, &dst, Outcome::Skipped, "already installed, identical");
            return;
        }
        Some(ref cur) if classify_dll(cur) == DllState::Ours => "updated",
        Some(_) => {
            report.push(
                Item::NativeDll,
                &dst,
                Outcome::Failed,
                "a winhttp.dll that is not ours is already in the game folder (another mod?); not overwritten",
            );
            return;
        }
    };
    let result = if dry_run {
        Ok(())
    } else {
        fs::write(&dst, &src_bytes)
            .map_err(|e| io::Error::new(e.kind(), format!("{e} (is the game running? close it and install again)")))
    };
    report.record(Item::NativeDll, &dst, verb, result);
}

fn install_path_file(report: &mut Report, game_dir: &Path, launcher_path: &Path, dry_run: bool) {
    let path = game_dir.join(PATH_FILE);
    let wanted = launcher_path.to_string_lossy().into_owned();
    match fs::read_to_string(&path) {
        Ok(cur) if cur == wanted => report.push(Item::PathFile, &path, Outcome::Skipped, "already up to date"),
        Ok(_) | Err(_) => {
            let verb = if path.exists() { "updated" } else { "written" };
            let result = if dry_run { Ok(()) } else { fs::write(&path, wanted.as_bytes()) };
            report.record(Item::PathFile, &path, verb, result);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Uninstall
// ---------------------------------------------------------------------------------------------------------------

/// Removes exactly what [`install`] put there: our mod folder, our `winhttp.dll` (never a foreign one),
/// `feverlink3_path.txt`, and `steam_appid.txt` when it holds our app id. Idempotent.
pub fn uninstall(game_dir: &Path, user_dirs: &[PathBuf], dry_run: bool) -> Report {
    let mut report = Report { dry_run, steps: Vec::new() };

    if user_dirs.is_empty() {
        report.push(Item::Mod, "", Outcome::Skipped, "no Steam user folder found");
    }
    for user_dir in user_dirs {
        let dst = mod_dir(user_dir);
        if dst.is_dir() {
            let result = if dry_run { Ok(()) } else { fs::remove_dir_all(&dst) };
            report.record(Item::Mod, &dst, "removed", result);
        } else {
            report.push(Item::Mod, &dst, Outcome::Skipped, "not installed");
        }
        remove_legacy_mods(&mut report, user_dir, dry_run);
    }

    let dll = game_dir.join(DLL_NAME);
    match dll_state(game_dir) {
        Ok(DllState::Absent) => report.push(Item::NativeDll, &dll, Outcome::Skipped, "not installed"),
        Ok(DllState::Foreign) => report.push(Item::NativeDll, &dll, Outcome::Skipped, "not ours; left alone"),
        Ok(DllState::Ours) => {
            let result = if dry_run {
                Ok(())
            } else {
                fs::remove_file(&dll).map_err(|e| io::Error::new(e.kind(), format!("{e} (is the game running?)")))
            };
            report.record(Item::NativeDll, &dll, "removed", result);
        }
        Err(e) => report.push(Item::NativeDll, &dll, Outcome::Failed, format!("could not read: {e}")),
    }

    let path_file = game_dir.join(PATH_FILE);
    if path_file.is_file() {
        let result = if dry_run { Ok(()) } else { fs::remove_file(&path_file) };
        report.record(Item::PathFile, &path_file, "removed", result);
    } else {
        report.push(Item::PathFile, &path_file, Outcome::Skipped, "not installed");
    }

    let appid = game_dir.join(APPID_FILE);
    match appid_state(game_dir) {
        Ok(AppIdState::Ours) => {
            let result = if dry_run { Ok(()) } else { fs::remove_file(&appid) };
            report.record(Item::AppIdFile, &appid, "removed", result);
        }
        Ok(AppIdState::Absent) => report.push(Item::AppIdFile, &appid, Outcome::Skipped, "not installed"),
        Ok(AppIdState::Other(c)) => {
            report.push(Item::AppIdFile, &appid, Outcome::Skipped, format!("holds {c:?}, not our app id; left alone"))
        }
        Err(e) => report.push(Item::AppIdFile, &appid, Outcome::Failed, format!("could not read: {e}")),
    }

    report
}

// ---------------------------------------------------------------------------------------------------------------
// Folder helpers
// ---------------------------------------------------------------------------------------------------------------

/// Every regular file under `root`, keyed by its path relative to `root`.
fn collect_files(root: &Path) -> io::Result<BTreeMap<PathBuf, PathBuf>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(root).map_err(io::Error::other)?.to_path_buf();
                out.insert(rel, path);
            }
        }
    }
    Ok(out)
}

/// True when `dst` exists and holds exactly the files of `src` with identical bytes.
fn trees_equal(src: &Path, dst: &Path) -> io::Result<bool> {
    if !dst.is_dir() {
        return Ok(false);
    }
    let a = collect_files(src)?;
    let b = collect_files(dst)?;
    if a.len() != b.len() || a.keys().ne(b.keys()) {
        return Ok(false);
    }
    for (rel, src_file) in &a {
        if fs::read(src_file)? != fs::read(&b[rel])? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Replaces `dst` with a copy of `src`: files removed from the source disappear from the destination.
fn mirror_copy(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.exists() {
        fs::remove_dir_all(dst)?;
    }
    copy_tree(src, dst)
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// settings.lua (pure; not applied by `install` yet)
// ---------------------------------------------------------------------------------------------------------------

/// Returns `settings.lua` with the mod added to the game's default mod list, or `None` when nothing has to
/// change (already listed, or no `activeModsState = {` table found).
///
/// Finding from MPFever (MIT): the game's `settings.lua` under the per-user folder holds
/// `activeModsState = { "<mod id>", ... }`, the mods a new game starts with, and the game rewrites the file on
/// exit, so it may only be edited while the game is closed. Not confirmed on the development machine, where the
/// file did not exist; this is why `install` does not call it yet.
pub fn settings_with_mod_active(settings_lua: &str, mod_id: &str) -> Option<String> {
    let key = settings_lua.find("activeModsState")?;
    let open_rel = settings_lua[key..].find('{')?;
    let open = key + open_rel;
    let close_rel = settings_lua[open..].find('}')?;
    let close = open + close_rel;
    let body = &settings_lua[open + 1..close];
    if body.contains(&format!("\"{mod_id}\"")) {
        return None;
    }
    let mut out = String::with_capacity(settings_lua.len() + mod_id.len() + 4);
    out.push_str(&settings_lua[..=open]);
    out.push_str(&format!(" \"{mod_id}\","));
    out.push_str(&settings_lua[open + 1..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Fixture {
        _root: TempDir,
        game: PathBuf,
        users: Vec<PathBuf>,
        mod_src: PathBuf,
        dll_src: PathBuf,
        launcher: PathBuf,
    }

    fn fixture() -> Fixture {
        let root = TempDir::new().unwrap();
        let game = root.path().join("game");
        fs::create_dir_all(&game).unwrap();
        fs::write(game.join("TransportFever3.exe"), b"not really").unwrap();
        let users = vec![
            root.path().join("userdata").join("1").join(APP_ID).join("local"),
            root.path().join("userdata").join("2").join(APP_ID).join("local"),
        ];
        for u in &users {
            fs::create_dir_all(u).unwrap();
        }
        let mod_src = root.path().join("payload").join("mod").join(MOD_ID);
        fs::create_dir_all(mod_src.join("res").join("scripts")).unwrap();
        fs::write(mod_src.join("mod.lua"), "function data() return {} end").unwrap();
        fs::write(mod_src.join("res").join("scripts").join("a.lua"), "a").unwrap();
        let dll_src = root.path().join("payload").join("winhttp.dll");
        let mut dll = b"MZ fake dll ".to_vec();
        dll.extend_from_slice(NATIVE_MARKER);
        dll.extend_from_slice(b" v1");
        fs::write(&dll_src, dll).unwrap();
        let launcher = root.path().join("FeverLink3.exe");
        Fixture { _root: root, game, users, mod_src, dll_src, launcher }
    }

    impl Fixture {
        fn payload(&self) -> Payload<'_> {
            Payload { mod_source: Some(&self.mod_src), native_dll: Some(&self.dll_src), launcher_path: &self.launcher }
        }
        fn install(&self) -> Report {
            install(&self.game, &self.users, self.payload(), false)
        }
        fn outcomes(report: &Report, item: Item) -> Vec<Outcome> {
            report.steps.iter().filter(|s| s.item == item).map(|s| s.outcome).collect()
        }
    }

    #[test]
    fn fresh_install_puts_four_things_in_place_and_is_idempotent() {
        let f = fixture();
        let r = f.install();
        assert!(r.ok(), "{r}");
        assert_eq!(Fixture::outcomes(&r, Item::Mod), vec![Outcome::Done, Outcome::Done]);
        assert_eq!(Fixture::outcomes(&r, Item::AppIdFile), vec![Outcome::Done]);
        assert_eq!(Fixture::outcomes(&r, Item::NativeDll), vec![Outcome::Done]);
        assert_eq!(Fixture::outcomes(&r, Item::PathFile), vec![Outcome::Done]);

        for u in &f.users {
            assert!(mod_installed(u));
            assert_eq!(fs::read_to_string(mod_dir(u).join("res").join("scripts").join("a.lua")).unwrap(), "a");
        }
        assert_eq!(fs::read_to_string(f.game.join(APPID_FILE)).unwrap(), APP_ID);
        assert_eq!(dll_state(&f.game).unwrap(), DllState::Ours);
        assert_eq!(recorded_launcher_path(&f.game), Some(f.launcher.clone()));

        // second run: nothing changes, everything is reported as skipped
        let r2 = f.install();
        assert!(r2.ok());
        assert!(r2.steps.iter().all(|s| s.outcome == Outcome::Skipped), "{r2}");
    }

    #[test]
    fn dry_run_plans_without_touching_anything() {
        let f = fixture();
        let r = install(&f.game, &f.users, f.payload(), true);
        assert!(r.dry_run);
        assert_eq!(r.steps.iter().filter(|s| s.outcome == Outcome::Done).count(), 5);
        assert!(!f.game.join(APPID_FILE).exists());
        assert!(!f.game.join(DLL_NAME).exists());
        assert!(!f.game.join(PATH_FILE).exists());
        assert!(f.users.iter().all(|u| !mod_installed(u)));
        assert!(r.to_string().contains("WOULD "));
    }

    #[test]
    fn mod_update_is_a_mirror_copy_that_drops_stale_files() {
        let f = fixture();
        f.install();
        let stale = mod_dir(&f.users[0]).join("old.lua");
        fs::write(&stale, "stale").unwrap();
        fs::write(f.mod_src.join("mod.lua"), "changed").unwrap();
        let r = f.install();
        assert_eq!(Fixture::outcomes(&r, Item::Mod), vec![Outcome::Done, Outcome::Done]);
        assert!(!stale.exists());
        assert_eq!(fs::read_to_string(mod_dir(&f.users[1]).join("mod.lua")).unwrap(), "changed");
    }

    #[test]
    fn foreign_dll_is_never_overwritten_and_never_removed() {
        let f = fixture();
        fs::write(f.game.join(DLL_NAME), b"MZ someone else's proxy").unwrap();
        let r = f.install();
        assert!(!r.ok());
        let dll_step = r.steps.iter().find(|s| s.item == Item::NativeDll).unwrap();
        assert_eq!(dll_step.outcome, Outcome::Failed);
        assert!(dll_step.reason.contains("not ours"));
        assert_eq!(fs::read(f.game.join(DLL_NAME)).unwrap(), b"MZ someone else's proxy");
        assert_eq!(dll_state(&f.game).unwrap(), DllState::Foreign);

        let u = uninstall(&f.game, &f.users, false);
        assert!(u.ok());
        assert!(f.game.join(DLL_NAME).exists());
        let dll_step = u.steps.iter().find(|s| s.item == Item::NativeDll).unwrap();
        assert_eq!(dll_step.outcome, Outcome::Skipped);
    }

    #[test]
    fn our_older_dll_is_updated() {
        let f = fixture();
        let mut old = b"MZ old ".to_vec();
        old.extend_from_slice(NATIVE_MARKER);
        fs::write(f.game.join(DLL_NAME), &old).unwrap();
        let r = f.install();
        assert!(r.ok(), "{r}");
        let dll_step = r.steps.iter().find(|s| s.item == Item::NativeDll).unwrap();
        assert_eq!((dll_step.outcome, dll_step.reason.as_str()), (Outcome::Done, "updated"));
        assert_eq!(fs::read(f.game.join(DLL_NAME)).unwrap(), fs::read(&f.dll_src).unwrap());
    }

    #[test]
    fn a_payload_dll_without_marker_is_refused() {
        let f = fixture();
        fs::write(&f.dll_src, b"MZ unmarked").unwrap();
        let r = f.install();
        let dll_step = r.steps.iter().find(|s| s.item == Item::NativeDll).unwrap();
        assert_eq!(dll_step.outcome, Outcome::Failed);
        assert!(!f.game.join(DLL_NAME).exists());
    }

    #[test]
    fn existing_appid_file_is_left_alone() {
        let f = fixture();
        fs::write(f.game.join(APPID_FILE), "480").unwrap();
        let r = f.install();
        let step = r.steps.iter().find(|s| s.item == Item::AppIdFile).unwrap();
        assert_eq!(step.outcome, Outcome::Skipped);
        assert_eq!(fs::read_to_string(f.game.join(APPID_FILE)).unwrap(), "480");
        let u = uninstall(&f.game, &f.users, false);
        let step = u.steps.iter().find(|s| s.item == Item::AppIdFile).unwrap();
        assert_eq!(step.outcome, Outcome::Skipped);
        assert!(f.game.join(APPID_FILE).exists());
    }

    #[test]
    fn uninstall_removes_exactly_ours_and_is_idempotent() {
        let f = fixture();
        f.install();
        // something else in the mods folder must survive
        let other_mod = mods_dir(&f.users[0]).join("someone_else_1");
        fs::create_dir_all(&other_mod).unwrap();

        let u = uninstall(&f.game, &f.users, false);
        assert!(u.ok(), "{u}");
        assert!(u.steps.iter().all(|s| s.outcome == Outcome::Done));
        assert!(f.users.iter().all(|d| !mod_installed(d)));
        assert!(other_mod.is_dir());
        assert!(!f.game.join(DLL_NAME).exists());
        assert!(!f.game.join(PATH_FILE).exists());
        assert!(!f.game.join(APPID_FILE).exists());
        assert!(f.game.join("TransportFever3.exe").exists());

        let u2 = uninstall(&f.game, &f.users, false);
        assert!(u2.steps.iter().all(|s| s.outcome == Outcome::Skipped), "{u2}");
    }

    #[test]
    fn an_old_copy_under_the_earlier_folder_name_is_removed() {
        let f = fixture();
        let old = mods_dir(&f.users[0]).join(LEGACY_MOD_IDS[0]);
        fs::create_dir_all(old.join("content")).unwrap();
        fs::write(old.join("mod.json"), "{}").unwrap();
        let other_mod = mods_dir(&f.users[0]).join("someone_else_1");
        fs::create_dir_all(&other_mod).unwrap();

        let dry = install(&f.game, &f.users, f.payload(), true);
        assert!(dry.steps.iter().any(|s| s.path == old && s.outcome == Outcome::Done), "{dry}");
        assert!(old.is_dir(), "a dry run leaves it");

        let r = f.install();
        assert!(r.ok(), "{r}");
        let step = r.steps.iter().find(|s| s.path == old).unwrap();
        assert_eq!((step.item, step.outcome), (Item::Mod, Outcome::Done));
        assert!(step.reason.contains("old mod folder"), "{r}");
        assert!(!old.exists());
        assert!(mod_installed(&f.users[0]));
        assert!(other_mod.is_dir());
        // only reported while such a folder exists
        assert!(f.install().steps.iter().all(|s| s.outcome == Outcome::Skipped));

        // uninstall removes it too
        fs::create_dir_all(&old).unwrap();
        let u = uninstall(&f.game, &f.users, false);
        assert!(u.ok(), "{u}");
        assert!(!old.exists());
        assert!(other_mod.is_dir());
    }

    #[test]
    fn uninstall_dry_run_changes_nothing() {
        let f = fixture();
        f.install();
        let u = uninstall(&f.game, &f.users, true);
        assert_eq!(u.steps.iter().filter(|s| s.outcome == Outcome::Done).count(), 5);
        assert!(f.game.join(DLL_NAME).exists());
        assert!(f.users.iter().all(|d| mod_installed(d)));
    }

    #[test]
    fn install_without_user_folders_or_payload_reports_skips() {
        let f = fixture();
        let payload = Payload { mod_source: None, native_dll: None, launcher_path: &f.launcher };
        let r = install(&f.game, &[], payload, false);
        assert!(r.ok(), "{r}");
        assert_eq!(Fixture::outcomes(&r, Item::Mod), vec![Outcome::Skipped]);
        assert_eq!(Fixture::outcomes(&r, Item::NativeDll), vec![Outcome::Skipped]);
        assert_eq!(Fixture::outcomes(&r, Item::AppIdFile), vec![Outcome::Done]);
        let r = install(&f.game, &[], f.payload(), false);
        assert!(r.steps.iter().any(|s| s.item == Item::Mod && s.reason.contains("start the game once")));
    }

    #[test]
    fn path_file_is_rewritten_when_the_launcher_moved() {
        let f = fixture();
        f.install();
        let moved = f.launcher.with_file_name("Elsewhere.exe");
        let payload = Payload { launcher_path: &moved, ..f.payload() };
        let r = install(&f.game, &f.users, payload, false);
        let step = r.steps.iter().find(|s| s.item == Item::PathFile).unwrap();
        assert_eq!((step.outcome, step.reason.as_str()), (Outcome::Done, "updated"));
        assert_eq!(recorded_launcher_path(&f.game), Some(moved));
    }

    #[test]
    fn marker_search() {
        assert!(contains(b"xx feverlink3_native yy", NATIVE_MARKER));
        assert!(!contains(b"feverlink3_nativ", NATIVE_MARKER));
        assert!(!contains(b"", NATIVE_MARKER));
        assert_eq!(classify_dll(b"winhttp from microsoft"), DllState::Foreign);
    }

    #[test]
    fn settings_lua_activation() {
        let s = "a = 1,\nmainMenuState = {\n  activeModsState = { \"other_1\", },\n  x = 2,\n},\n";
        let out = settings_with_mod_active(s, MOD_ID).unwrap();
        assert!(out.contains("activeModsState = { \"feverlink3\", \"other_1\", }"));
        assert_eq!(settings_with_mod_active(&out, MOD_ID), None, "already active");
        assert_eq!(settings_with_mod_active("nothing here", MOD_ID), None);
        let empty = "activeModsState = {}";
        assert_eq!(settings_with_mod_active(empty, MOD_ID).unwrap(), "activeModsState = { \"feverlink3\",}");
    }
}
