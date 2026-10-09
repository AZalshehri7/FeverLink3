//! The console wizard: what `feverlink3.exe` does when it is started without arguments, which is what a double-click
//! is. A player who has never opened a terminal can install, check, start the game, find the logs and collect a
//! diagnostics text from one numbered menu, and the window never closes by itself.
//!
//! Layout of this file, top to bottom:
//!
//! - the facts about this PC ([`Facts`], one snapshot taken by [`gather`] through the [`System`] trait) and
//!   everything the wizard decides from them, as pure functions: the checks screen, what each menu choice may do,
//!   how a failed install is explained, the version comparison, the diagnostics text and its redaction;
//! - [`System`] and [`RealSystem`], the only parts that start programs (Steam, Explorer, `clip.exe`);
//! - [`run`], the menu loop over any input and output, so the tests drive it with scripted lines and a fake PC.
//!
//! Install and uninstall are `feverlink3_launcher::install`'s own functions: the wizard adds the questions asked
//! before and the plain-language explanation after, never a second way to change the game folder.

use std::cmp::Ordering;
use std::fs::File;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use feverlink3_core::lobby::JoinCode;
use feverlink3_launcher::install::{self, AppIdState, DllState, Item, Outcome, Payload, Report, Step};
use feverlink3_launcher::steam::{self, SteamInfo};
use feverlink3_launcher::update::Version;
use feverlink3_launcher::{live, process, APP_ID};

/// Game builds the native module has hook addresses for: the PE link timestamp and the game's own build number.
/// The launcher cannot link the native crate (the native crate links the launcher), so the list is copied from
/// `native/src/build_table.rs`; the test `known_builds_match_the_native_build_table` fails when they drift apart.
const KNOWN_BUILDS: &[(u32, &str)] = &[(0x6ac5_0427, "40420")];

/// Where the version of a launcher can be read without running it: this text sits in every launcher's data, the
/// version after the prefix and a NUL after the version. `feverlink3_path.txt` names the launcher that installed
/// FeverLink3; if that file still exists, its marker says which version put the files there.
const MARKER_PREFIX: &str = "feverlink3-launcher-version=";
static VERSION_MARKER: &str = concat!("feverlink3-launcher-version=", env!("CARGO_PKG_VERSION"), "\0");
/// Longest version text read after the marker.
const MAX_VERSION_LEN: usize = 64;
/// Most bytes read from a launcher looking for its marker: a few megabytes are normal, the limit only keeps a
/// wrong path in `feverlink3_path.txt` from making the wizard read something huge.
const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
/// The first page of the game executable holds the DOS and PE headers.
const HEADER_BYTES: u64 = 0x1000;

/// Logs the native module and the mod write, relative to `%LOCALAPPDATA%\FeverLink3` and its `live` folder.
const NATIVE_LOG: &str = "native.log";
const MOD_LOG: &str = "mod.log";
const LOG_TAIL_LINES: usize = 40;
/// How much of the end of a log is read: enough for 40 ordinary lines, bounded for a log that grew for months.
const LOG_TAIL_BYTES: u64 = 64 * 1024;
const LOG_LINE_CHARS: usize = 300;
/// Fewest base32 characters a join code can have (26 bytes: version, time, secret, endpoint count, check).
const MIN_JOIN_CODE_CHARS: usize = 42;

const STEAM_URL: &str = "https://store.steampowered.com/about/";
const RELEASES_URL: &str = "https://github.com/AZalshehri7/FeverLink3/releases";

// ---------------------------------------------------------------------------------------------------------------
// Menu
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Install,
    Uninstall,
    CheckVersion,
    StartGame,
    OpenLogs,
    Diagnostics,
    Exit,
}

/// The menu in the order it is drawn: the choice, the key that selects it, its label. One table for drawing and
/// parsing, so the two cannot disagree.
const MENU: &[(Choice, &str, &str)] = &[
    (Choice::Install, "1", "Install or repair FeverLink3"),
    (Choice::Uninstall, "2", "Uninstall FeverLink3"),
    (Choice::CheckVersion, "3", "Check this folder's version against the installed one"),
    (Choice::StartGame, "4", "Start Transport Fever 3 through Steam"),
    (Choice::OpenLogs, "5", "Open the log folder"),
    (Choice::Diagnostics, "6", "Copy a diagnostics text to the clipboard"),
    (Choice::Exit, "0", "Exit"),
];

/// What a typed line means.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Typed {
    Choice(Choice),
    /// Only Enter.
    Nothing,
    /// Anything else, as typed.
    Unknown(String),
}

/// Reads one typed line: the menu keys, `q`, `quit` and `exit` for leaving, a trailing `.` or `)` ignored, digits
/// from Arabic and full-width keyboard layouts accepted. Never fails; unknown text is handed back.
fn parse_choice(line: &str) -> Typed {
    let text = line.trim();
    if text.is_empty() {
        return Typed::Nothing;
    }
    let key = ascii_digits(text.trim_end_matches(['.', ')'])).to_lowercase();
    if let Some((choice, _, _)) = MENU.iter().find(|(_, k, _)| *k == key) {
        return Typed::Choice(*choice);
    }
    match key.as_str() {
        "q" | "quit" | "exit" => Typed::Choice(Choice::Exit),
        _ => Typed::Unknown(text.to_owned()),
    }
}

/// Arabic-Indic, extended Arabic-Indic and full-width digits as ASCII digits, everything else unchanged. A player
/// whose keyboard is set to Arabic can type `١` for `1`; the menu should not answer "not on the menu".
fn ascii_digits(text: &str) -> String {
    text.chars()
        .map(|c| {
            let code = u32::from(c);
            let value = match code {
                0x0660..=0x0669 => code - 0x0660,
                0x06F0..=0x06F9 => code - 0x06F0,
                0xFF10..=0xFF19 => code - 0xFF10,
                _ => return c,
            };
            char::from_digit(value, 10).unwrap_or(c)
        })
        .collect()
}

fn menu_lines() -> Vec<String> {
    let mut lines = vec!["What would you like to do?".to_owned()];
    lines.extend(MENU.iter().map(|(_, key, label)| format!("  {key}  {label}")));
    lines.push(String::new());
    lines.push("Type a number and press Enter.".to_owned());
    lines
}

// ---------------------------------------------------------------------------------------------------------------
// Facts about this PC
// ---------------------------------------------------------------------------------------------------------------

/// A Steam user's `userdata/<id>/3493540/local` folder and whether the mod is in it.
#[derive(Debug, Clone)]
struct UserDir {
    path: PathBuf,
    mod_installed: bool,
}

/// What the game folder holds of ours, as the doctor reports it. `Err` carries the operating system's text.
#[derive(Debug, Clone)]
struct GameFiles {
    dll: Result<DllState, String>,
    appid: Result<AppIdState, String>,
    /// The launcher path recorded in `feverlink3_path.txt`.
    launcher: Option<PathBuf>,
}

/// What a game executable's PE header says about the build.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BuildCheck {
    NoGame,
    Unreadable(String),
    Known { name: &'static str },
    Unknown { timestamp: u32 },
}

/// One snapshot of this PC, taken by [`gather`]. Every screen and decision is a function of it.
#[derive(Debug, Clone)]
struct Facts {
    steam_path: Option<PathBuf>,
    steam_exe: Option<PathBuf>,
    libraries: Vec<PathBuf>,
    game_dir: Option<PathBuf>,
    users: Vec<UserDir>,
    game_files: Option<GameFiles>,
    build: BuildCheck,
    game_running: bool,
    exe_path: PathBuf,
    payload_mod: Option<PathBuf>,
    payload_dll: Option<PathBuf>,
    /// What installing from this folder would do, worked out as a dry run; `None` without game folder or payload.
    plan: Option<Report>,
    /// The version of the launcher recorded in `feverlink3_path.txt`, when that file can still be read.
    installed_version: Option<String>,
    local_dir: Option<PathBuf>,
    local_dir_exists: bool,
}

/// Takes the snapshot. Reads files and asks the system, changes nothing; the dry-run install is how "what differs
/// from this folder" is known, so the comparison and the real install can never disagree.
fn gather(sys: &dyn System) -> Facts {
    let info = sys.discover();
    let exe_path = sys.exe_path();
    let (payload_mod, payload_dll) = sys.payload();
    let game_files = info.game_dir.as_deref().map(read_game_files);
    let installed_version = game_files
        .as_ref()
        .and_then(|g| g.launcher.as_deref())
        .and_then(|recorded| read_launcher_version(recorded, &exe_path));
    let plan = match (info.game_dir.as_deref(), payload_mod.as_deref(), payload_dll.as_deref()) {
        (Some(game), Some(mod_source), Some(native_dll)) => {
            let payload =
                Payload { mod_source: Some(mod_source), native_dll: Some(native_dll), launcher_path: &exe_path };
            Some(install::install(game, &info.user_dirs, payload, true))
        }
        _ => None,
    };
    let local_dir = sys.local_dir();
    Facts {
        steam_exe: info.steam_path.as_ref().map(|p| p.join("steam.exe")).filter(|p| p.is_file()),
        build: check_build(info.game_dir.as_deref()),
        users: info
            .user_dirs
            .iter()
            .map(|path| UserDir { path: path.clone(), mod_installed: install::mod_installed(path) })
            .collect(),
        game_running: sys.game_running(),
        local_dir_exists: local_dir.as_deref().is_some_and(Path::is_dir),
        local_dir,
        steam_path: info.steam_path,
        libraries: info.libraries,
        game_dir: info.game_dir,
        game_files,
        exe_path,
        payload_mod,
        payload_dll,
        plan,
        installed_version,
    }
}

fn read_game_files(game_dir: &Path) -> GameFiles {
    GameFiles {
        dll: install::dll_state(game_dir).map_err(|e| e.to_string()),
        appid: install::appid_state(game_dir).map_err(|e| e.to_string()),
        launcher: install::recorded_launcher_path(game_dir),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Game build
// ---------------------------------------------------------------------------------------------------------------

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

/// The link timestamp of a 64-bit PE image, read from its first bytes: the value `native/src/pe.rs` reads from the
/// running game and `build_table` is keyed by. `None` for anything else.
fn pe_timestamp(image: &[u8]) -> Option<u32> {
    if image.get(..2)? != b"MZ" {
        return None;
    }
    let pe = usize::try_from(u32_at(image, 0x3c)?).ok()?;
    if image.get(pe..pe.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    let coff = pe.checked_add(4)?;
    let timestamp = u32_at(image, coff.checked_add(4)?)?;
    // the optional header follows the 20-byte COFF header; PE32+ (0x20b) only, the game is 64-bit
    if u16_at(image, coff.checked_add(20)?)? != 0x20b {
        return None;
    }
    Some(timestamp)
}

fn classify_build(timestamp: u32) -> BuildCheck {
    match KNOWN_BUILDS.iter().find(|(known, _)| *known == timestamp) {
        Some((_, name)) => BuildCheck::Known { name },
        None => BuildCheck::Unknown { timestamp },
    }
}

fn check_build(game_dir: Option<&Path>) -> BuildCheck {
    let Some(dir) = game_dir else { return BuildCheck::NoGame };
    let mut header = Vec::new();
    let read = File::open(steam::game_exe(dir)).and_then(|f| f.take(HEADER_BYTES).read_to_end(&mut header));
    match read {
        Ok(_) => pe_timestamp(&header)
            .map_or_else(|| BuildCheck::Unreadable("not a 64-bit Windows program".into()), classify_build),
        Err(e) => BuildCheck::Unreadable(e.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------------------------------------------

/// The marker, passed through `black_box` so that no optimiser folds the text away: another launcher can only
/// find the version by scanning for it.
fn marker() -> &'static str {
    std::hint::black_box(VERSION_MARKER)
}

/// The version of this launcher, from the same text the scan of another launcher finds.
fn own_version() -> &'static str {
    marker().strip_prefix(MARKER_PREFIX).and_then(|v| v.strip_suffix('\0')).unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Every version a launcher's bytes announce: the text after each occurrence of the marker prefix that parses as a
/// version. The scan constant itself appears in the launcher too, followed by something else, so every occurrence
/// is tried.
fn version_markers(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    let prefix = MARKER_PREFIX.as_bytes();
    bytes.windows(prefix.len()).enumerate().filter(move |(_, window)| *window == prefix).filter_map(move |(at, _)| {
        let rest = bytes.get(at.checked_add(prefix.len())?..)?;
        let len = rest
            .iter()
            .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+')))
            .unwrap_or(rest.len());
        // one byte more than allowed is read, so that a runaway text is refused instead of cut short
        let text = std::str::from_utf8(rest.get(..len.min(MAX_VERSION_LEN + 1))?).ok()?;
        if text.len() > MAX_VERSION_LEN {
            return None;
        }
        Version::parse(text).ok().map(|_| text.to_owned())
    })
}

/// The first version a launcher's bytes announce.
fn find_version_marker(bytes: &[u8]) -> Option<String> {
    version_markers(bytes).next()
}

/// The version of the launcher recorded at install time, or `None` when that file is gone or unmarked. A record
/// that names this very executable needs no reading.
fn read_launcher_version(recorded: &Path, own_exe: &Path) -> Option<String> {
    if same_path_text(recorded, own_exe) {
        return Some(own_version().to_owned());
    }
    let mut bytes = Vec::new();
    File::open(recorded).ok()?.take(MAX_SCAN_BYTES).read_to_end(&mut bytes).ok()?;
    find_version_marker(&bytes)
}

fn same_path_text(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

/// How this folder's version relates to the installed one, when both are known and parse.
fn version_order(this: &str, installed: Option<&str>) -> Option<Ordering> {
    Some(Version::parse(this).ok()?.cmp(&Version::parse(installed?).ok()?))
}

// ---------------------------------------------------------------------------------------------------------------
// What is installed, and what this folder holds
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallState {
    /// No game folder to look in, or its files could not be read.
    Unknown,
    NotInstalled,
    /// The native module and the mod for every Steam user; the version that put them there, when it is known.
    Complete {
        version: Option<String>,
    },
    Partial {
        native_module: bool,
        mods: usize,
        users: usize,
    },
    /// A `winhttp.dll` that is not ours sits in the game folder.
    ForeignDll,
}

fn installed_state(f: &Facts) -> InstallState {
    let Some(files) = &f.game_files else { return InstallState::Unknown };
    let native_module = match &files.dll {
        Ok(DllState::Foreign) => return InstallState::ForeignDll,
        Ok(DllState::Ours) => true,
        Ok(DllState::Absent) => false,
        Err(_) => return InstallState::Unknown,
    };
    let users = f.users.len();
    let mods = f.users.iter().filter(|u| u.mod_installed).count();
    if native_module && users > 0 && mods == users {
        InstallState::Complete { version: f.installed_version.clone() }
    } else if !native_module && mods == 0 && files.launcher.is_none() {
        InstallState::NotInstalled
    } else {
        InstallState::Partial { native_module, mods, users }
    }
}

/// Whether the folder this executable sits in can be installed from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Package {
    Complete,
    /// Started from inside a zip file or another temporary folder, which Windows empties later.
    TemporaryFolder,
    Incomplete {
        mod_missing: bool,
        dll_missing: bool,
    },
}

fn package_state(f: &Facts) -> Package {
    if is_temporary_location(&f.exe_path) {
        Package::TemporaryFolder
    } else if f.payload_mod.is_none() || f.payload_dll.is_none() {
        Package::Incomplete { mod_missing: f.payload_mod.is_none(), dll_missing: f.payload_dll.is_none() }
    } else {
        Package::Complete
    }
}

/// True for the places Windows and the common unzip tools run a program from when it is double-clicked inside an
/// archive: `%TEMP%` (compared as `AppData\Local\Temp`, because the short and the long form of the user folder
/// both occur), Explorer's `Temp1_<name>.zip`, WinRAR's `Rar$...` and 7-Zip's `7zO...`.
fn is_temporary_location(exe: &Path) -> bool {
    let parts: Vec<String> = exe
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    let in_temp = parts.windows(3).any(|w| w[0] == "appdata" && w[1] == "local" && w[2] == "temp");
    in_temp || parts.iter().any(|p| is_extraction_folder(p))
}

fn is_extraction_folder(lowercase_name: &str) -> bool {
    let explorer = lowercase_name
        .strip_prefix("temp")
        .and_then(|rest| rest.split_once('_'))
        .is_some_and(|(n, name)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && name.ends_with(".zip"));
    explorer || lowercase_name.starts_with("rar$") || lowercase_name.starts_with("7zo")
}

// ---------------------------------------------------------------------------------------------------------------
// Problems and their plain-language messages
// ---------------------------------------------------------------------------------------------------------------

/// Everything that can stop an action, with what to do about it. Both the checks made before an action and the
/// failures an install report carries end up here, so every message exists once.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Problem {
    SteamMissing,
    SteamExeMissing(PathBuf),
    GameMissing,
    GameRunning,
    GameAlreadyRunning,
    TemporaryFolder,
    PackageIncomplete { mod_missing: bool, dll_missing: bool },
    PackageDamaged,
    ForeignDll(PathBuf),
    NoUserFolder,
    FilesInUse(PathBuf),
    PermissionDenied(PathBuf),
    DiskFull,
    NoLocalAppData,
    NoLogsYet(PathBuf),
    Other(String),
}

impl Problem {
    fn message(&self) -> String {
        match self {
            Problem::SteamMissing => format!(
                "Steam was not found on this PC. Install Steam ({STEAM_URL}), sign in, install Transport Fever 3 and \
                 start it once. Then run FeverLink3 again."
            ),
            Problem::SteamExeMissing(path) => format!(
                "Steam's program file was not found at {}. Start Steam once so that it repairs itself, or install it \
                 again, then try this again.",
                path.display()
            ),
            Problem::GameMissing => "Transport Fever 3 was not found in your Steam libraries. Install it from Steam \
                 (on a drive Steam uses), start it once, close it, then try this again."
                .to_owned(),
            Problem::GameRunning => "Transport Fever 3 is running, so its files are locked. Close the game completely \
                 first (use its Exit button; if the window is gone but the sound goes on, end TransportFever3.exe in \
                 the Task Manager), then try this again."
                .to_owned(),
            Problem::GameAlreadyRunning => {
                "Transport Fever 3 is already running. Switch to its window (Alt+Tab) instead.".to_owned()
            }
            Problem::TemporaryFolder => "This copy of FeverLink3 is running from inside the downloaded zip file (or \
                 another temporary folder), which Windows deletes later. Close this window, right-click the zip file, \
                 choose Extract All, and start feverlink3.exe from the new folder."
                .to_owned(),
            Problem::PackageIncomplete { mod_missing, dll_missing } => {
                let missing = match (mod_missing, dll_missing) {
                    (true, true) => "the mod folder (mod\\feverlink3_1) and winhttp.dll are",
                    (true, false) => "the mod folder (mod\\feverlink3_1) is",
                    _ => "winhttp.dll is",
                };
                format!(
                    "This FeverLink3 folder is incomplete: {missing} missing next to feverlink3.exe. Unzip the whole \
                     zip file (right-click it, Extract All) and start feverlink3.exe from the new folder."
                )
            }
            Problem::PackageDamaged => {
                "The winhttp.dll in this FeverLink3 folder is not FeverLink3's native module, or \
                 it is damaged. Download the zip file again and unzip it into a fresh folder."
                    .to_owned()
            }
            Problem::ForeignDll(path) => format!(
                "The game folder already contains a winhttp.dll that does not belong to FeverLink3 ({}), probably from \
                 another mod. FeverLink3 never overwrites it. If you do not need that other mod, remove or rename the \
                 file and choose 1 again.",
                path.display()
            ),
            Problem::NoUserFolder => {
                "Transport Fever 3 has not created its Steam user folder yet, so the mod could not \
                 be copied there. Start the game once from Steam, close it again, and choose 1 again."
                    .to_owned()
            }
            Problem::FilesInUse(path) => format!(
                "A file is in use by another program: {}. This is usually Transport Fever 3 (make sure it is fully \
                 closed) or an antivirus scan. Wait a minute and try again.",
                path.display()
            ),
            Problem::PermissionDenied(path) => format!(
                "Windows did not allow FeverLink3 to change {}. If Transport Fever 3 may still be running, even in the \
                 background, close it first. If the game is installed under Program Files, close this window, \
                 right-click feverlink3.exe and choose Run as administrator. Antivirus protection such as controlled \
                 folder access (ransomware protection) can also block it: allow feverlink3.exe there.",
                path.display()
            ),
            Problem::DiskFull => "The disk is full. Free some space and try again.".to_owned(),
            Problem::NoLocalAppData => "Windows did not say where your local application data folder is (the \
                 LOCALAPPDATA setting), so the log folder cannot be found."
                .to_owned(),
            Problem::NoLogsYet(path) => format!(
                "There are no logs yet: {} does not exist. FeverLink3 creates it the first time Transport Fever 3 \
                 starts with FeverLink3 installed. Install it (1), start the game (4), then try again.",
                path.display()
            ),
            Problem::Other(text) => format!("Something unexpected went wrong: {text}"),
        }
    }
}

/// The number in `(os error N)`, which Windows error texts carry whatever language the system speaks.
fn os_error_code(text: &str) -> Option<u32> {
    let (_, rest) = text.split_once("(os error ")?;
    rest.split_once(')')?.0.trim().parse().ok()
}

/// Sorts the failure text of an install step into a [`Problem`]. The report only keeps the error as text, so the
/// operating system's code is read from it first (the words are translated, the code is not), then the words
/// `install.rs` itself uses.
fn classify_reason(item: Item, reason: &str, path: &Path) -> Problem {
    match os_error_code(reason) {
        // sharing violation, lock violation, "the file has a user-mapped section open"
        Some(32 | 33 | 1224) => return Problem::FilesInUse(path.to_path_buf()),
        Some(5) => return Problem::PermissionDenied(path.to_path_buf()),
        Some(112) => return Problem::DiskFull,
        _ => {}
    }
    if reason.contains("is the game running") {
        Problem::FilesInUse(path.to_path_buf())
    } else if reason.contains("not ours") || reason.contains("not overwritten") {
        Problem::ForeignDll(path.to_path_buf())
    } else if reason.contains("lacks the marker") || reason.contains("could not read the native module") {
        Problem::PackageDamaged
    } else if reason.contains("source folder is missing") || reason.contains("shipped with this launcher") {
        Problem::PackageIncomplete { mod_missing: item == Item::Mod, dll_missing: item == Item::NativeDll }
    } else if reason.contains("no Steam user folder") {
        Problem::NoUserFolder
    } else {
        Problem::Other(format!("{item}: {reason}"))
    }
}

/// The problem a report step stands for. A failed step always is one; a skipped step is one when it skipped
/// something that was wanted (nothing to install from, or no folder to install the mod into).
fn step_problem(step: &Step) -> Option<Problem> {
    match step.outcome {
        Outcome::Failed => Some(classify_reason(step.item, &step.reason, &step.path)),
        Outcome::Skipped
            if step.reason.contains("no Steam user folder") || step.reason.contains("shipped with this launcher") =>
        {
            Some(classify_reason(step.item, &step.reason, &step.path))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Checks screen
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Good,
    Warn,
    Bad,
    Note,
}

impl Status {
    /// Plain ASCII, so that it reads the same in every console font.
    fn tag(self) -> &'static str {
        match self {
            Status::Good => "[ok]",
            Status::Warn => "[??]",
            Status::Bad => "[!!]",
            Status::Note => "[--]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Check {
    status: Status,
    label: &'static str,
    text: String,
}

fn check(status: Status, label: &'static str, text: impl Into<String>) -> Check {
    Check { status, label, text: text.into() }
}

/// The six lines of the checks screen: Steam, the game, its build, whether it runs, whether FeverLink3 is
/// installed, and whether this folder can be installed from.
fn checks(f: &Facts) -> Vec<Check> {
    vec![steam_check(f), game_check(f), build_check(f), running_check(f), installed_check(f), folder_check(f)]
}

fn steam_check(f: &Facts) -> Check {
    match &f.steam_path {
        Some(path) => check(Status::Good, "Steam", path.display().to_string()),
        None => check(Status::Bad, "Steam", format!("not found - install Steam first ({STEAM_URL})")),
    }
}

fn game_check(f: &Facts) -> Check {
    match (&f.game_dir, &f.steam_path) {
        (Some(dir), _) => check(Status::Good, "Transport Fever 3", dir.display().to_string()),
        (None, Some(_)) => check(
            Status::Bad,
            "Transport Fever 3",
            "not found in your Steam libraries - install it from Steam and start it once",
        ),
        (None, None) => check(Status::Bad, "Transport Fever 3", "not found (Steam is missing)"),
    }
}

fn build_check(f: &Facts) -> Check {
    match &f.build {
        BuildCheck::NoGame => check(Status::Note, "Game build", "not checked (the game was not found)"),
        BuildCheck::Unreadable(why) => check(Status::Warn, "Game build", format!("could not be read ({why})")),
        BuildCheck::Known { name } => {
            check(Status::Good, "Game build", format!("{name} - FeverLink3 knows this build"))
        }
        BuildCheck::Unknown { timestamp } => check(
            Status::Warn,
            "Game build",
            format!(
                "unknown (id {timestamp:#010x}) - FeverLink3 has no hook addresses for it, so the mod would run in a \
                 reduced mode. Tell the author the build number shown at the bottom right of the game's main menu"
            ),
        ),
    }
}

fn running_check(f: &Facts) -> Check {
    if f.game_running {
        check(Status::Warn, "Game running", "yes - close it before installing or uninstalling")
    } else {
        check(Status::Good, "Game running", "no")
    }
}

fn installed_check(f: &Facts) -> Check {
    const LABEL: &str = "FeverLink3";
    match installed_state(f) {
        InstallState::Unknown => check(Status::Note, LABEL, "unknown (there is no game folder to look in)"),
        InstallState::NotInstalled => check(Status::Note, LABEL, "not installed yet"),
        InstallState::Complete { version: Some(v) } => check(Status::Good, LABEL, format!("installed (version {v})")),
        InstallState::Complete { version: None } => check(Status::Good, LABEL, "installed (version not recorded)"),
        InstallState::Partial { native_module, mods, users } => check(
            Status::Warn,
            LABEL,
            format!(
                "only partly installed (native module: {}, mod: {mods} of {users} Steam users) - choose 1 to finish",
                if native_module { "yes" } else { "no" }
            ),
        ),
        InstallState::ForeignDll => check(
            Status::Bad,
            LABEL,
            "another program's winhttp.dll is in the game folder; FeverLink3 will not touch it",
        ),
    }
}

/// The folder this executable sits in.
fn folder_dir(f: &Facts) -> String {
    f.exe_path.parent().unwrap_or(&f.exe_path).display().to_string()
}

fn folder_check(f: &Facts) -> Check {
    match package_state(f) {
        Package::Complete => check(
            Status::Good,
            "This folder",
            format!("{} (version {}, all files present)", folder_dir(f), own_version()),
        ),
        Package::TemporaryFolder => {
            check(Status::Bad, "This folder", "inside a zip file or a temporary folder - unzip it first")
        }
        Package::Incomplete { .. } => check(Status::Bad, "This folder", "incomplete - unzip the whole zip file first"),
    }
}

fn render_checks(checks: &[Check]) -> Vec<String> {
    checks.iter().map(|c| format!("  {}  {:<18} {}", c.status.tag(), c.label, c.text)).collect()
}

/// What to do next, in one sentence, from the state of this PC: the first thing that is wrong, else the next
/// useful step.
fn next_step(f: &Facts) -> String {
    if f.steam_path.is_none() {
        return Problem::SteamMissing.message();
    }
    let Some(game_dir) = &f.game_dir else { return Problem::GameMissing.message() };
    match installed_state(f) {
        InstallState::ForeignDll => Problem::ForeignDll(game_dir.join(install::DLL_NAME)).message(),
        InstallState::Complete { .. } if f.game_running => {
            "Transport Fever 3 is running: in the game, click FeverLink3 in the bar at the bottom of the screen.".into()
        }
        InstallState::Complete { .. } if matches!(compare(f), Ok(Comparison::Differs(_))) => {
            "this folder holds different files than the installed ones. Choose 3 to compare them or 1 to update; \
             otherwise choose 4 to start Transport Fever 3."
                .into()
        }
        InstallState::Complete { .. } => {
            "choose 4 to start Transport Fever 3, then click FeverLink3 in the bar at the bottom of the game.".into()
        }
        state => match package_state(f) {
            Package::TemporaryFolder => Problem::TemporaryFolder.message(),
            Package::Incomplete { mod_missing, dll_missing } => {
                Problem::PackageIncomplete { mod_missing, dll_missing }.message()
            }
            Package::Complete if f.game_running => {
                "close Transport Fever 3, then choose 1 to install FeverLink3.".into()
            }
            Package::Complete if matches!(state, InstallState::Partial { .. }) => {
                "choose 1 to finish installing FeverLink3.".into()
            }
            Package::Complete => "choose 1 to install FeverLink3.".into(),
        },
    }
}

/// The whole screen the menu loop draws: banner, checks, next step, menu.
fn render_screen(f: &Facts) -> String {
    let mut lines = vec![
        format!("FeverLink3 {} - cooperative multiplayer for Transport Fever 3", own_version()),
        String::new(),
        "Checking this PC...".to_owned(),
    ];
    lines.extend(render_checks(&checks(f)));
    lines.push(String::new());
    lines.push(format!("Next step: {}", next_step(f)));
    lines.push(String::new());
    lines.extend(menu_lines());
    lines.join("\n") + "\n"
}

// ---------------------------------------------------------------------------------------------------------------
// What each choice may do
// ---------------------------------------------------------------------------------------------------------------

/// The folders an install works with; only exists when nothing stands in the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InstallPlan<'a> {
    game_dir: &'a Path,
    mod_source: &'a Path,
    native_dll: &'a Path,
}

/// Install needs Steam, the game, a complete and permanent folder to install from, a closed game (Windows locks
/// `winhttp.dll` while it runs) and no foreign `winhttp.dll` in the way. The first thing missing is the answer.
fn plan_install(f: &Facts) -> Result<InstallPlan<'_>, Problem> {
    if f.steam_path.is_none() {
        return Err(Problem::SteamMissing);
    }
    let Some(game_dir) = f.game_dir.as_deref() else { return Err(Problem::GameMissing) };
    match package_state(f) {
        Package::TemporaryFolder => return Err(Problem::TemporaryFolder),
        Package::Incomplete { mod_missing, dll_missing } => {
            return Err(Problem::PackageIncomplete { mod_missing, dll_missing })
        }
        Package::Complete => {}
    }
    if f.game_running {
        return Err(Problem::GameRunning);
    }
    if installed_state(f) == InstallState::ForeignDll {
        return Err(Problem::ForeignDll(game_dir.join(install::DLL_NAME)));
    }
    match (f.payload_mod.as_deref(), f.payload_dll.as_deref()) {
        (Some(mod_source), Some(native_dll)) => Ok(InstallPlan { game_dir, mod_source, native_dll }),
        _ => Err(Problem::PackageIncomplete {
            mod_missing: f.payload_mod.is_none(),
            dll_missing: f.payload_dll.is_none(),
        }),
    }
}

/// Uninstall needs the game folder (it holds most of what is removed) and a closed game.
fn plan_uninstall(f: &Facts) -> Result<&Path, Problem> {
    if f.steam_path.is_none() {
        return Err(Problem::SteamMissing);
    }
    let game_dir = f.game_dir.as_deref().ok_or(Problem::GameMissing)?;
    if f.game_running {
        return Err(Problem::GameRunning);
    }
    Ok(game_dir)
}

/// Starting through Steam needs Steam's program and a game that is not running yet. The game folder is not
/// required: Steam knows better where the game is, and says so itself when it is not installed.
fn plan_start(f: &Facts) -> Result<&Path, Problem> {
    let steam_path = f.steam_path.as_deref().ok_or(Problem::SteamMissing)?;
    if f.steam_exe.is_none() {
        return Err(Problem::SteamExeMissing(steam_path.join("steam.exe")));
    }
    if f.game_running {
        return Err(Problem::GameAlreadyRunning);
    }
    Ok(steam_path)
}

/// The log folder, when it exists.
fn plan_logs(f: &Facts) -> Result<&Path, Problem> {
    let dir = f.local_dir.as_deref().ok_or(Problem::NoLocalAppData)?;
    if f.local_dir_exists {
        Ok(dir)
    } else {
        Err(Problem::NoLogsYet(dir.to_path_buf()))
    }
}

/// `steam.exe -applaunch 3493540`, not yet started: Steam starts itself first if it is not running, then the game.
fn steam_start_command(steam_path: &Path) -> Command {
    let mut cmd = Command::new(steam_path.join("steam.exe"));
    cmd.current_dir(steam_path)
        .args(["-applaunch", APP_ID])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

// ---------------------------------------------------------------------------------------------------------------
// After an install or uninstall
// ---------------------------------------------------------------------------------------------------------------

/// The plain-language ending of an install or uninstall: what happened, and what to do about what did not.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Summary {
    ok: bool,
    headline: String,
    notes: Vec<String>,
}

/// The distinct values of `items`, in order.
fn distinct(items: impl IntoIterator<Item = Problem>) -> Vec<Problem> {
    let mut found: Vec<Problem> = Vec::new();
    for item in items {
        if !found.contains(&item) {
            found.push(item);
        }
    }
    found
}

/// The problems of the steps that failed.
fn failure_problems(report: &Report) -> Vec<Problem> {
    distinct(
        report
            .steps
            .iter()
            .filter(|s| s.outcome == Outcome::Failed)
            .map(|s| classify_reason(s.item, &s.reason, &s.path)),
    )
}

/// The problems of an install: the failures, and the steps skipped because there was nothing to install from or
/// no Steam user folder to install into, which leave the job half done without being errors.
fn install_problems(report: &Report) -> Vec<Problem> {
    distinct(report.steps.iter().filter_map(step_problem))
}

fn summarize_install(report: &Report) -> Summary {
    let problems = install_problems(report);
    if !problems.is_empty() {
        let failed = report.steps.iter().any(|s| s.outcome == Outcome::Failed);
        let headline = if failed {
            "FeverLink3 could not be installed completely."
        } else {
            "FeverLink3 is only partly installed."
        };
        return Summary {
            ok: false,
            headline: headline.to_owned(),
            notes: problems.iter().map(Problem::message).collect(),
        };
    }
    if report.steps.iter().all(|s| s.outcome == Outcome::Skipped) {
        return Summary {
            ok: true,
            headline: "FeverLink3 was already installed and up to date. Nothing was changed.".to_owned(),
            notes: Vec::new(),
        };
    }
    Summary {
        ok: true,
        headline: "FeverLink3 is installed.".to_owned(),
        notes: vec![
            "Choose 4 to start Transport Fever 3 through Steam.".to_owned(),
            "A new game needs the mod switched on once: open the Add Mods tab and activate FeverLink3 Multiplayer. \
             Then click FeverLink3 in the bar at the bottom of the screen."
                .to_owned(),
        ],
    }
}

fn summarize_uninstall(report: &Report) -> Summary {
    let problems = failure_problems(report);
    if !problems.is_empty() {
        return Summary {
            ok: false,
            headline: "FeverLink3 could not be removed completely.".to_owned(),
            notes: problems.iter().map(Problem::message).collect(),
        };
    }
    if report.steps.iter().all(|s| s.outcome == Outcome::Skipped) {
        return Summary {
            ok: true,
            headline: "FeverLink3 was not installed. Nothing was removed.".to_owned(),
            notes: Vec::new(),
        };
    }
    let mut notes = vec!["You can delete this FeverLink3 folder when you no longer need it.".to_owned()];
    if report.steps.iter().any(|s| s.item == Item::NativeDll && s.reason.contains("not ours")) {
        notes.push(
            "A winhttp.dll that does not belong to FeverLink3 was found in the game folder and left alone.".to_owned(),
        );
    }
    Summary {
        ok: true,
        headline: "FeverLink3 was removed. Transport Fever 3 starts exactly as before.".to_owned(),
        notes,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// This folder against the installed files
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Comparison {
    NotInstalled,
    Identical,
    /// The mod and the native module are the same; only helper files differ (the recorded launcher location).
    HelpersDiffer(Vec<&'static str>),
    /// What would be replaced by installing from this folder.
    Differs(Vec<&'static str>),
}

/// Compares by the dry-run install: what it would change is what differs. `Err` when there is nothing to compare.
fn compare(f: &Facts) -> Result<Comparison, Problem> {
    if f.steam_path.is_none() {
        return Err(Problem::SteamMissing);
    }
    if f.game_dir.is_none() {
        return Err(Problem::GameMissing);
    }
    let Some(plan) = &f.plan else {
        return Err(Problem::PackageIncomplete {
            mod_missing: f.payload_mod.is_none(),
            dll_missing: f.payload_dll.is_none(),
        });
    };
    if let Some(problem) = failure_problems(plan).into_iter().next() {
        return Err(problem);
    }
    if installed_state(f) == InstallState::NotInstalled {
        return Ok(Comparison::NotInstalled);
    }
    let mut pieces: Vec<&'static str> = Vec::new();
    let mut helpers: Vec<&'static str> = Vec::new();
    for step in plan.steps.iter().filter(|s| s.outcome == Outcome::Done) {
        let (list, name) = match step.item {
            Item::Mod => (&mut pieces, "the mod"),
            Item::NativeDll => (&mut pieces, "the native module (winhttp.dll)"),
            Item::PathFile => (&mut helpers, "the recorded launcher location"),
            Item::AppIdFile => (&mut helpers, "steam_appid.txt"),
        };
        if !list.contains(&name) {
            list.push(name);
        }
    }
    Ok(match (pieces.is_empty(), helpers.is_empty()) {
        (true, true) => Comparison::Identical,
        (true, false) => Comparison::HelpersDiffer(helpers),
        (false, _) => Comparison::Differs(pieces),
    })
}

fn installed_text(f: &Facts) -> String {
    match installed_state(f) {
        InstallState::Complete { version: Some(v) } => {
            let from = f.game_files.as_ref().and_then(|g| g.launcher.as_deref()).and_then(Path::parent);
            from.map_or_else(
                || format!("version {v}"),
                |dir| format!("version {v}  (installed from {})", dir.display()),
            )
        }
        InstallState::Complete { version: None } => {
            "installed, version not recorded (the folder that installed it is gone)".to_owned()
        }
        InstallState::NotInstalled => "not installed".to_owned(),
        InstallState::Partial { .. } => "only partly installed".to_owned(),
        InstallState::ForeignDll => "another program's winhttp.dll is in the game folder".to_owned(),
        InstallState::Unknown => "unknown (there is no game folder to look in)".to_owned(),
    }
}

/// The text of menu choice 3.
fn comparison_lines(f: &Facts) -> Vec<String> {
    let this = own_version();
    let verdict = match compare(f) {
        Err(problem) => problem.message(),
        Ok(Comparison::NotInstalled) => {
            "FeverLink3 is not installed. Choose 1 to install the version in this folder.".to_owned()
        }
        Ok(Comparison::Identical) => {
            "The installed files are identical to the ones in this folder. Nothing to do.".to_owned()
        }
        Ok(Comparison::HelpersDiffer(list)) => format!(
            "The installed mod and native module are identical to the ones in this folder. Only {} would be updated; \
             choose 1 to do that.",
            list.join(" and ")
        ),
        Ok(Comparison::Differs(pieces)) => {
            let what = pieces.join(" and ");
            match version_order(this, f.installed_version.as_deref()) {
                Some(Ordering::Greater) => {
                    format!("This folder is NEWER than what is installed. Installing from it updates {what}. Choose 1 to update.")
                }
                Some(Ordering::Less) => format!(
                    "This folder is OLDER than what is installed. Installing from it would replace newer files ({what}) \
                     with older ones. Use the newer FeverLink3 folder instead."
                ),
                Some(Ordering::Equal) => format!(
                    "The version number is the same, but {what} differ from the installed files. Choose 1 to make them match."
                ),
                None => format!(
                    "The installed files differ from the ones in this folder ({what}). The installed version is not \
                     known, so it cannot be said which is newer. Choose 1 to make the installed files match this folder."
                ),
            }
        }
    };
    vec![
        format!("This folder:  version {this}  ({})", folder_dir(f)),
        format!("Installed:    {}", installed_text(f)),
        String::new(),
        verdict,
        String::new(),
        format!("Looking for newer releases online is not built in yet; the newest zip file is on {RELEASES_URL}"),
    ]
}

// ---------------------------------------------------------------------------------------------------------------
// Doctor text
// ---------------------------------------------------------------------------------------------------------------

fn path_text(p: &Path) -> String {
    p.display().to_string()
}

/// What `feverlink3 doctor` prints: what this PC has of Steam, the game and FeverLink3, one fact per line, in the
/// format the command has always used.
fn doctor_text(f: &Facts) -> String {
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

/// The text of `feverlink3 doctor` for this PC.
pub fn doctor_report() -> String {
    doctor_text(&gather(&RealSystem))
}

// ---------------------------------------------------------------------------------------------------------------
// Diagnostics text
// ---------------------------------------------------------------------------------------------------------------

/// The end of a log file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LogTail {
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
fn build_diagnostics(f: &Facts, now_unix: u64, native: &LogTail, mod_log: &LogTail, user_names: &[String]) -> String {
    let mut lines = vec![
        format!("FeverLink3 diagnostics - launcher {}, {}", own_version(), format_utc(now_unix)),
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

// ---------------------------------------------------------------------------------------------------------------
// The machine
// ---------------------------------------------------------------------------------------------------------------

/// The `live` folder under `%LOCALAPPDATA%\FeverLink3` where the mod writes `mod.log` (`live::live_dir`).
const LIVE_SUBDIR: &str = "live";

/// Everything the wizard asks of the machine or does to it outside the install functions. The tests supply a fake
/// that works on temporary folders and records what it was asked to start.
trait System {
    /// Steam, the game and the Steam user folders.
    fn discover(&self) -> SteamInfo;
    /// True when any Transport Fever 3 process runs.
    fn game_running(&self) -> bool;
    /// This executable.
    fn exe_path(&self) -> PathBuf;
    /// The mod folder and the native module shipped next to this executable.
    fn payload(&self) -> (Option<PathBuf>, Option<PathBuf>);
    /// `%LOCALAPPDATA%\FeverLink3`, whether or not it exists yet.
    fn local_dir(&self) -> Option<PathBuf>;
    /// The names that identify this person in paths and logs.
    fn user_names(&self) -> Vec<String>;
    fn now_unix(&self) -> u64;
    /// Asks Steam to start the game and returns without waiting for it.
    fn start_steam_game(&self, steam_path: &Path) -> io::Result<()>;
    fn open_folder(&self, dir: &Path) -> io::Result<()>;
    fn copy_to_clipboard(&self, text: &str) -> io::Result<()>;
}

/// A program of the Windows folder by absolute path. A bare name is looked up in this program's own folder first,
/// and a folder the player unzipped into should not get a say in which `clip.exe` runs.
fn windows_program(relative: &[&str]) -> PathBuf {
    let mut path = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    path.extend(relative);
    path
}

/// The bytes `clip.exe` takes as Unicode text: UTF-16 little endian after a byte order mark, with Windows line
/// breaks. Without the mark it reads the bytes in the console's code page and garbles every non-ASCII character.
fn clipboard_bytes(text: &str) -> Vec<u8> {
    let windows_text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    std::iter::once(0xFEFF_u16).chain(windows_text.encode_utf16()).flat_map(u16::to_le_bytes).collect()
}

/// This PC.
struct RealSystem;

impl System for RealSystem {
    fn discover(&self) -> SteamInfo {
        steam::discover()
    }

    fn game_running(&self) -> bool {
        process::any_game_running()
    }

    fn exe_path(&self) -> PathBuf {
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("FeverLink3.exe"))
    }

    fn payload(&self) -> (Option<PathBuf>, Option<PathBuf>) {
        super::locate_payload()
    }

    fn local_dir(&self) -> Option<PathBuf> {
        live::live_dir().and_then(|dir| dir.parent().map(Path::to_path_buf))
    }

    fn user_names(&self) -> Vec<String> {
        let profile_folder = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        ["USERNAME", "USER"].iter().filter_map(|key| std::env::var(key).ok()).chain(profile_folder).collect()
    }

    fn now_unix(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
    }

    fn start_steam_game(&self, steam_path: &Path) -> io::Result<()> {
        steam_start_command(steam_path).spawn().map(|_| ())
    }

    fn open_folder(&self, dir: &Path) -> io::Result<()> {
        // explorer.exe exits with 1 even when it opened the window, so only a failure to start counts
        Command::new(windows_program(&["explorer.exe"])).arg(dir).spawn().map(|_| ())
    }

    fn copy_to_clipboard(&self, text: &str) -> io::Result<()> {
        let mut child = Command::new(windows_program(&["System32", "clip.exe"]))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let written = match child.stdin.take() {
            // the pipe closes when it goes out of scope, which is what tells clip.exe the text is complete
            Some(mut pipe) => pipe.write_all(&clipboard_bytes(text)),
            None => Err(io::Error::other("no pipe to clip.exe")),
        };
        let status = child.wait()?;
        written?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("clip.exe ended with {status}")))
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The menu loop
// ---------------------------------------------------------------------------------------------------------------

/// One line from the input, `None` at its end (a closed pipe, or Ctrl+Z in a console).
fn read_line(input: &mut dyn BufRead) -> io::Result<Option<String>> {
    let mut line = String::new();
    Ok((input.read_line(&mut line)? > 0).then_some(line))
}

fn say(out: &mut dyn Write, text: &str) -> io::Result<()> {
    writeln!(out, "{text}")
}

/// Asks a yes/no question whose default is no.
fn confirm(input: &mut dyn BufRead, out: &mut dyn Write, question: &str) -> io::Result<bool> {
    writeln!(out, "{question}")?;
    write!(out, "Type y and press Enter to continue, or just press Enter to cancel: ")?;
    out.flush()?;
    Ok(read_line(input)?.is_some_and(|line| matches!(line.trim().to_lowercase().as_str(), "y" | "yes")))
}

/// The ending of an install or uninstall, tagged like the checks so that success and trouble look different.
fn say_summary(out: &mut dyn Write, summary: &Summary) -> io::Result<()> {
    let status = if summary.ok { Status::Good } else { Status::Bad };
    writeln!(out, "\n{} {}", status.tag(), summary.headline)?;
    summary.notes.iter().try_for_each(|note| writeln!(out, "  - {note}"))
}

fn user_dirs(f: &Facts) -> Vec<PathBuf> {
    f.users.iter().map(|u| u.path.clone()).collect()
}

fn action_install(sys: &dyn System, input: &mut dyn BufRead, out: &mut dyn Write) -> io::Result<()> {
    let facts = gather(sys);
    let plan = match plan_install(&facts) {
        Ok(plan) => plan,
        Err(problem) => return say(out, &problem.message()),
    };
    // an old folder run by mistake must not quietly replace newer files
    if let (Some(Ordering::Less), Some(installed)) =
        (version_order(own_version(), facts.installed_version.as_deref()), facts.installed_version.as_deref())
    {
        let question = format!(
            "Version {installed} of FeverLink3 is installed, and this folder holds the older version {}. Installing \
             from this folder replaces the newer files with older ones.",
            own_version()
        );
        if !confirm(input, out, &question)? {
            return say(out, "Nothing was changed.");
        }
    }
    writeln!(out, "Installing FeverLink3 ...")?;
    let payload = Payload {
        mod_source: Some(plan.mod_source),
        native_dll: Some(plan.native_dll),
        launcher_path: &facts.exe_path,
    };
    let report = install::install(plan.game_dir, &user_dirs(&facts), payload, false);
    write!(out, "{report}")?;
    say_summary(out, &summarize_install(&report))
}

const UNINSTALL_QUESTION: &str = "This removes FeverLink3 from Transport Fever 3: the mod folder, winhttp.dll and \
    the two small files FeverLink3 put next to the game. Your savegames and settings are not touched.";

fn action_uninstall(sys: &dyn System, input: &mut dyn BufRead, out: &mut dyn Write) -> io::Result<()> {
    let facts = gather(sys);
    let game_dir = match plan_uninstall(&facts) {
        Ok(dir) => dir,
        Err(problem) => return say(out, &problem.message()),
    };
    if !confirm(input, out, UNINSTALL_QUESTION)? {
        return say(out, "Nothing was changed.");
    }
    writeln!(out, "Removing FeverLink3 ...")?;
    let report = install::uninstall(game_dir, &user_dirs(&facts), false);
    write!(out, "{report}")?;
    say_summary(out, &summarize_uninstall(&report))
}

fn action_start(sys: &dyn System, out: &mut dyn Write) -> io::Result<()> {
    let facts = gather(sys);
    let steam_path = match plan_start(&facts) {
        Ok(path) => path,
        Err(problem) => return say(out, &problem.message()),
    };
    match sys.start_steam_game(steam_path) {
        Ok(()) => {
            say(
                out,
                "Asked Steam to start Transport Fever 3. If Steam was not running it starts first, which can take a \
                 minute; sign in if it asks. You can leave this window open.",
            )?;
            if !matches!(installed_state(&facts), InstallState::Complete { .. }) {
                say(
                    out,
                    "Note: FeverLink3 is not fully installed, so the game starts without multiplayer. Choose 1 first \
                     if you want to play with others.",
                )?;
            }
            Ok(())
        }
        Err(e) => say(
            out,
            &format!("Steam could not be started ({e}). Start Steam yourself, then start Transport Fever 3 from its library."),
        ),
    }
}

fn action_logs(sys: &dyn System, out: &mut dyn Write) -> io::Result<()> {
    let facts = gather(sys);
    let dir = match plan_logs(&facts) {
        Ok(dir) => dir,
        Err(problem) => return say(out, &problem.message()),
    };
    match sys.open_folder(dir) {
        Ok(()) => say(out, &format!("Opened {}.", dir.display())),
        Err(e) => say(
            out,
            &format!(
                "The folder could not be opened ({e}). Paste this into the address bar of a File Explorer window: {}",
                dir.display()
            ),
        ),
    }
}

fn action_diagnostics(sys: &dyn System, out: &mut dyn Write) -> io::Result<()> {
    let facts = gather(sys);
    let tail = |file: Option<PathBuf>| file.map_or(LogTail::Missing, |path| read_log_tail(&path));
    let native = tail(facts.local_dir.as_ref().map(|dir| dir.join(NATIVE_LOG)));
    let mod_log = tail(facts.local_dir.as_ref().map(|dir| dir.join(LIVE_SUBDIR).join(MOD_LOG)));
    let text = build_diagnostics(&facts, sys.now_unix(), &native, &mod_log, &sys.user_names());
    match sys.copy_to_clipboard(&text) {
        Ok(()) => {
            say(
                out,
                &format!(
                    "Copied {} lines to the clipboard. Paste them into your message with Ctrl+V.",
                    text.lines().count()
                ),
            )?;
            say(
                out,
                "Your Windows user name is shown as <user>. The text still holds folder names, and the logs can hold \
                 network addresses; read it first if you like.",
            )
        }
        Err(e) => {
            say(
                out,
                &format!("The text could not be copied to the clipboard ({e}). Select it below and copy it from here:"),
            )?;
            say(out, "----------------------------------------")?;
            write!(out, "{text}")?;
            say(out, "----------------------------------------")
        }
    }
}

fn perform(choice: Choice, sys: &dyn System, input: &mut dyn BufRead, out: &mut dyn Write) -> io::Result<()> {
    match choice {
        Choice::Install => action_install(sys, input, out),
        Choice::Uninstall => action_uninstall(sys, input, out),
        Choice::CheckVersion => say(out, &comparison_lines(&gather(sys)).join("\n")),
        Choice::StartGame => action_start(sys, out),
        Choice::OpenLogs => action_logs(sys, out),
        Choice::Diagnostics => action_diagnostics(sys, out),
        Choice::Exit => Ok(()),
    }
}

/// The menu loop: draw the checks and the menu, read a choice, do it, wait for Enter, repeat. It ends at the
/// Exit choice or when the input ends, and every action is followed by a wait so that a window opened by a
/// double-click is never closed under the reader's eyes.
fn run(sys: &dyn System, input: &mut dyn BufRead, out: &mut dyn Write) -> io::Result<()> {
    loop {
        write!(out, "\n{}", render_screen(&gather(sys)))?;
        let choice = loop {
            write!(out, "> ")?;
            out.flush()?;
            let Some(line) = read_line(input)? else { return Ok(()) };
            match parse_choice(&line) {
                Typed::Choice(choice) => break choice,
                Typed::Nothing => {}
                Typed::Unknown(text) => {
                    writeln!(out, "\"{text}\" is not on the menu. Type one of the numbers above, for example 1.")?;
                }
            }
        };
        if choice == Choice::Exit {
            return Ok(());
        }
        writeln!(out)?;
        perform(choice, sys, input, out)?;
        write!(out, "\nPress Enter to go back to the menu.")?;
        out.flush()?;
        if read_line(input)?.is_none() {
            return Ok(());
        }
    }
}

/// Shows the error and keeps the window open until a key is pressed. `pause` reads the console itself, so this
/// also works from a panic hook that runs while the panicking thread holds the standard input.
fn wait_for_key() {
    let _ = Command::new(windows_program(&["System32", "cmd.exe"])).args(["/C", "pause"]).status();
}

/// The release profile aborts on a panic, which would close a double-clicked window without a word; this prints
/// what happened first and waits. Nothing in the wizard is meant to panic.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let _ = writeln!(
            io::stderr(),
            "\nFeverLink3 hit an unexpected problem and has to stop:\n{info}\nPlease tell whoever helps you what you \
             were doing when it happened."
        );
        wait_for_key();
    }));
}

/// `feverlink3.exe` without arguments: the wizard on this PC's console.
pub fn run_interactive() -> ExitCode {
    install_panic_hook();
    let result = {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let (mut input, mut out) = (stdin.lock(), stdout.lock());
        run(&RealSystem, &mut input, &mut out)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "\nFeverLink3 cannot go on: {e}");
            wait_for_key();
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::io::Cursor;
    use std::net::SocketAddr;

    use feverlink3_core::crypto::SessionKey;
    use feverlink3_launcher::{GAME_EXE, MOD_ID, NATIVE_MARKER};
    use tempfile::TempDir;

    use super::*;

    // -----------------------------------------------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------------------------------------------

    /// A minimal PE32+ header block with the given link timestamp: the fields `pe_timestamp` reads, nothing else.
    fn pe_image(timestamp: u32) -> Vec<u8> {
        let mut v = vec![0u8; 0x80];
        v[..2].copy_from_slice(b"MZ");
        v[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        v.extend_from_slice(b"PE\0\0");
        v.extend_from_slice(&0x8664u16.to_le_bytes()); // machine
        v.extend_from_slice(&0u16.to_le_bytes()); // sections
        v.extend_from_slice(&timestamp.to_le_bytes());
        v.extend_from_slice(&[0u8; 8]); // symbol table pointer and count
        v.extend_from_slice(&240u16.to_le_bytes()); // optional header size
        v.extend_from_slice(&0x22u16.to_le_bytes()); // characteristics
        v.extend_from_slice(&0x20bu16.to_le_bytes()); // PE32+ magic, the start of the optional header
        v.resize(v.len() + 238, 0);
        v
    }

    /// Facts of a healthy PC with nothing of FeverLink3 installed, built without touching the disk.
    fn base_facts() -> Facts {
        Facts {
            steam_path: Some(PathBuf::from(r"C:\Steam")),
            steam_exe: Some(PathBuf::from(r"C:\Steam\steam.exe")),
            libraries: vec![PathBuf::from(r"C:\Steam")],
            game_dir: Some(PathBuf::from(r"C:\Steam\steamapps\common\Transport Fever 3")),
            users: vec![UserDir { path: PathBuf::from(r"C:\Steam\userdata\1\3493540\local"), mod_installed: false }],
            game_files: Some(GameFiles { dll: Ok(DllState::Absent), appid: Ok(AppIdState::Absent), launcher: None }),
            build: BuildCheck::Known { name: "40420" },
            game_running: false,
            exe_path: PathBuf::from(r"C:\FeverLink3-0.1.0\feverlink3.exe"),
            payload_mod: Some(PathBuf::from(r"C:\FeverLink3-0.1.0\mod\feverlink3_1")),
            payload_dll: Some(PathBuf::from(r"C:\FeverLink3-0.1.0\winhttp.dll")),
            plan: None,
            installed_version: None,
            local_dir: Some(PathBuf::from(r"C:\Users\Ann\AppData\Local\FeverLink3")),
            local_dir_exists: true,
        }
    }

    /// The same PC after a complete install from this folder.
    fn installed_facts() -> Facts {
        let mut f = base_facts();
        f.game_files = Some(GameFiles {
            dll: Ok(DllState::Ours),
            appid: Ok(AppIdState::Ours),
            launcher: Some(f.exe_path.clone()),
        });
        f.users[0].mod_installed = true;
        f.installed_version = Some(own_version().to_owned());
        f
    }

    fn step(item: Item, outcome: Outcome, reason: &str) -> Step {
        Step { item, path: PathBuf::from(r"C:\Game\file"), outcome, reason: reason.to_owned() }
    }

    fn report(steps: Vec<Step>) -> Report {
        Report { dry_run: false, steps }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Steam(PathBuf),
        Open(PathBuf),
        Clipboard(String),
    }

    /// A PC made of temporary folders: Steam with the game and one user folder, a release folder to install from,
    /// and a `LocalAppData\FeverLink3` that does not exist until a test makes it. Starting a program is recorded,
    /// never done.
    struct Pc {
        root: TempDir,
        steam: Option<PathBuf>,
        game: Option<PathBuf>,
        users: Vec<PathBuf>,
        exe: PathBuf,
        payload: (Option<PathBuf>, Option<PathBuf>),
        local: Option<PathBuf>,
        names: Vec<String>,
        running: Cell<bool>,
        clipboard_fails: Cell<bool>,
        calls: RefCell<Vec<Call>>,
    }

    impl Pc {
        fn new() -> Pc {
            let root = TempDir::new().unwrap();
            let steam = root.path().join("Steam");
            let game = steam.join("steamapps").join("common").join("Transport Fever 3");
            fs::create_dir_all(&game).unwrap();
            fs::write(game.join(GAME_EXE), pe_image(0x6ac5_0427)).unwrap();
            fs::write(steam.join("steam.exe"), b"steam").unwrap();
            let user = steam.join("userdata").join("1").join(APP_ID).join("local");
            fs::create_dir_all(&user).unwrap();
            let mod_src = root.path().join("release").join("mod").join(MOD_ID);
            fs::create_dir_all(mod_src.join("content")).unwrap();
            fs::write(mod_src.join("mod.json"), "{}").unwrap();
            fs::write(mod_src.join("content").join("a.lua"), "a").unwrap();
            let dll_src = root.path().join("release").join("winhttp.dll");
            let mut dll = b"MZ fake native module ".to_vec();
            dll.extend_from_slice(NATIVE_MARKER);
            fs::write(&dll_src, dll).unwrap();
            let local = root.path().join("LocalAppData").join("FeverLink3");
            Pc {
                // not under a temporary folder, or the wizard would refuse to install from it
                exe: PathBuf::from("C:/FeverLink3-0.1.0/feverlink3.exe"),
                steam: Some(steam),
                game: Some(game),
                users: vec![user],
                payload: (Some(mod_src), Some(dll_src)),
                local: Some(local),
                names: vec!["Zara".to_owned()],
                running: Cell::new(false),
                clipboard_fails: Cell::new(false),
                calls: RefCell::new(Vec::new()),
                root,
            }
        }

        fn game(&self) -> PathBuf {
            self.game.clone().unwrap()
        }

        fn mod_src(&self) -> PathBuf {
            self.payload.0.clone().unwrap()
        }

        fn installed_mod(&self) -> PathBuf {
            install::mod_dir(&self.users[0])
        }

        fn local(&self) -> PathBuf {
            self.local.clone().unwrap()
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }
    }

    impl System for Pc {
        fn discover(&self) -> SteamInfo {
            SteamInfo {
                steam_path: self.steam.clone(),
                libraries: self.steam.iter().cloned().collect(),
                game_dir: self.game.clone(),
                user_dirs: self.users.clone(),
            }
        }

        fn game_running(&self) -> bool {
            self.running.get()
        }

        fn exe_path(&self) -> PathBuf {
            self.exe.clone()
        }

        fn payload(&self) -> (Option<PathBuf>, Option<PathBuf>) {
            self.payload.clone()
        }

        fn local_dir(&self) -> Option<PathBuf> {
            self.local.clone()
        }

        fn user_names(&self) -> Vec<String> {
            self.names.clone()
        }

        fn now_unix(&self) -> u64 {
            1_791_633_296
        }

        fn start_steam_game(&self, steam_path: &Path) -> io::Result<()> {
            self.calls.borrow_mut().push(Call::Steam(steam_path.to_path_buf()));
            Ok(())
        }

        fn open_folder(&self, dir: &Path) -> io::Result<()> {
            self.calls.borrow_mut().push(Call::Open(dir.to_path_buf()));
            Ok(())
        }

        fn copy_to_clipboard(&self, text: &str) -> io::Result<()> {
            if self.clipboard_fails.get() {
                return Err(io::Error::other("the clipboard is busy"));
            }
            self.calls.borrow_mut().push(Call::Clipboard(text.to_owned()));
            Ok(())
        }
    }

    /// Runs the menu loop on scripted input and returns everything it printed.
    fn drive(pc: &Pc, script: &str) -> String {
        let mut input = Cursor::new(script.as_bytes().to_vec());
        let mut out = Vec::new();
        run(pc, &mut input, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn clipboard_text(pc: &Pc) -> String {
        pc.calls()
            .into_iter()
            .find_map(|call| match call {
                Call::Clipboard(text) => Some(text),
                _ => None,
            })
            .expect("something was copied to the clipboard")
    }

    // -----------------------------------------------------------------------------------------------------------
    // Menu
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn menu_keys_parse_and_other_text_is_handed_back() {
        let keys = [
            ("1", Choice::Install),
            ("2", Choice::Uninstall),
            ("3", Choice::CheckVersion),
            ("4", Choice::StartGame),
            ("5", Choice::OpenLogs),
            ("6", Choice::Diagnostics),
            ("0", Choice::Exit),
        ];
        for (key, choice) in keys {
            assert_eq!(parse_choice(key), Typed::Choice(choice), "{key}");
        }
        assert_eq!(parse_choice("  2 \r\n"), Typed::Choice(Choice::Uninstall));
        assert_eq!(parse_choice("3."), Typed::Choice(Choice::CheckVersion));
        assert_eq!(parse_choice("4)"), Typed::Choice(Choice::StartGame));
        for word in ["q", "Q", "quit", "EXIT", " exit "] {
            assert_eq!(parse_choice(word), Typed::Choice(Choice::Exit), "{word}");
        }
        assert_eq!(parse_choice(""), Typed::Nothing);
        assert_eq!(parse_choice(" \r\n"), Typed::Nothing);
        for other in ["7", "10", "-1", "install", "1 2"] {
            assert_eq!(parse_choice(other), Typed::Unknown(other.to_owned()), "{other}");
        }
    }

    #[test]
    fn digits_typed_on_other_keyboard_layouts_count() {
        assert_eq!(parse_choice("\u{0661}"), Typed::Choice(Choice::Install), "Arabic-Indic 1");
        assert_eq!(parse_choice("\u{0663}"), Typed::Choice(Choice::CheckVersion), "Arabic-Indic 3");
        assert_eq!(parse_choice("\u{0660}"), Typed::Choice(Choice::Exit), "Arabic-Indic 0");
        assert_eq!(parse_choice("\u{06F4}"), Typed::Choice(Choice::StartGame), "Persian 4");
        assert_eq!(parse_choice("\u{FF16}"), Typed::Choice(Choice::Diagnostics), "full-width 6");
        assert_eq!(ascii_digits("a\u{0662}b"), "a2b");
    }

    #[test]
    fn the_menu_is_drawn_from_the_table_it_is_parsed_from() {
        let lines = menu_lines();
        for (choice, key, label) in MENU {
            assert!(lines.contains(&format!("  {key}  {label}")), "{label} is drawn");
            assert_eq!(parse_choice(key), Typed::Choice(*choice));
        }
        let keys: Vec<&str> = MENU.iter().map(|(_, key, _)| *key).collect();
        assert_eq!(keys, ["1", "2", "3", "4", "5", "6", "0"], "numbered in order, Exit last");
        let text = lines.join("\n");
        for wanted in [
            "Install or repair",
            "Uninstall",
            "Check this folder's version against the installed one",
            "Start Transport Fever 3 through Steam",
            "Open the log folder",
            "Copy a diagnostics text to the clipboard",
            "Exit",
        ] {
            assert!(text.contains(wanted), "{wanted}");
        }
    }

    // -----------------------------------------------------------------------------------------------------------
    // Game build
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn the_link_timestamp_is_read_from_a_64_bit_pe_header() {
        assert_eq!(pe_timestamp(&pe_image(0x6ac5_0427)), Some(0x6ac5_0427));
        assert_eq!(pe_timestamp(&pe_image(0)), Some(0));
        assert_eq!(pe_timestamp(b""), None);
        assert_eq!(pe_timestamp(b"MZ"), None);
        assert_eq!(pe_timestamp(b"not a program at all, but long enough to hold the fields"), None);
        let mut pe32 = pe_image(7);
        let magic_at = 0x80 + 4 + 20;
        pe32[magic_at..magic_at + 2].copy_from_slice(&0x10bu16.to_le_bytes());
        assert_eq!(pe_timestamp(&pe32), None, "32-bit images are not the game");
        let mut bad_signature = pe_image(7);
        bad_signature[0x80] = b'X';
        assert_eq!(pe_timestamp(&bad_signature), None);
        let mut far_header = pe_image(7);
        far_header[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(pe_timestamp(&far_header), None, "a header offset past the end of the data");
        assert_eq!(pe_timestamp(&pe_image(7)[..0x80 + 6]), None, "cut off inside the COFF header");
    }

    #[test]
    fn a_build_is_known_when_the_native_module_has_addresses_for_it() {
        assert_eq!(classify_build(0x6ac5_0427), BuildCheck::Known { name: "40420" });
        assert_eq!(classify_build(0x6ac5_0428), BuildCheck::Unknown { timestamp: 0x6ac5_0428 });
        assert_eq!(classify_build(0), BuildCheck::Unknown { timestamp: 0 });
    }

    #[test]
    fn the_build_comes_from_the_game_executable_in_the_game_folder() {
        let dir = TempDir::new().unwrap();
        assert_eq!(check_build(None), BuildCheck::NoGame);
        assert!(matches!(check_build(Some(dir.path())), BuildCheck::Unreadable(_)), "no executable");
        fs::write(dir.path().join(GAME_EXE), b"not a program").unwrap();
        assert!(matches!(check_build(Some(dir.path())), BuildCheck::Unreadable(why) if why.contains("64-bit")));
        fs::write(dir.path().join(GAME_EXE), pe_image(0x6ac5_0427)).unwrap();
        assert_eq!(check_build(Some(dir.path())), BuildCheck::Known { name: "40420" });
        fs::write(dir.path().join(GAME_EXE), pe_image(0x1234_5678)).unwrap();
        assert_eq!(check_build(Some(dir.path())), BuildCheck::Unknown { timestamp: 0x1234_5678 });
    }

    /// The `(timestamp, name)` rows of the `BUILDS` table in `native/src/build_table.rs`, read as text.
    fn native_table_rows(source: &str) -> Vec<(u32, String)> {
        let start = source.find("pub const BUILDS").expect("the BUILDS table");
        let end = source[start..].find("pub fn select(").map_or(source.len(), |at| start + at);
        let mut rest = &source[start..end];
        let mut rows = Vec::new();
        while let Some((_, after)) = rest.split_once("timestamp:") {
            let hex: String = after
                .trim_start()
                .strip_prefix("0x")
                .expect("a hexadecimal timestamp")
                .chars()
                .take_while(|c| c.is_ascii_hexdigit() || *c == '_')
                .filter(|c| *c != '_')
                .collect();
            let (_, after_name) = after.split_once("name:").expect("a name after the timestamp");
            let name = after_name.trim_start().strip_prefix('"').expect("a quoted name").split('"').next().unwrap();
            rows.push((u32::from_str_radix(&hex, 16).unwrap(), name.to_owned()));
            rest = after_name;
        }
        rows
    }

    #[test]
    fn known_builds_match_the_native_build_table() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("native").join("src").join("build_table.rs");
        let source = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut native = native_table_rows(&source);
        let mut ours: Vec<(u32, String)> = KNOWN_BUILDS.iter().map(|(ts, name)| (*ts, (*name).to_owned())).collect();
        native.sort();
        ours.sort();
        assert!(!native.is_empty(), "the table was not found");
        assert_eq!(
            ours, native,
            "KNOWN_BUILDS in launcher/src/wizard.rs must list exactly the builds of native/src/build_table.rs; \
             add or remove the row there too"
        );
    }

    #[test]
    fn the_table_reader_reads_rows_and_stops_before_the_test_table() {
        let source = "pub struct BuildAddrs { timestamp: u32, name: &'static str }\n\
                      pub const BUILDS: &[BuildAddrs] = &[\n\
                      // build 1: PE timestamp 0x10, linked then.\n\
                      BuildAddrs { timestamp: 0x6ac5_0427, name: \"40420\" },\n\
                      BuildAddrs {\n    timestamp: 0xdead_beef,\n    name: \"99\",\n    x: 1 },\n];\n\
                      pub fn select(t: u32) {}\n\
                      mod tests { static FAKE: &[BuildAddrs] = &[BuildAddrs { timestamp: 0x1234, name: \"test\" }]; }\n";
        assert_eq!(native_table_rows(source), vec![(0x6ac5_0427, "40420".to_owned()), (0xdead_beef, "99".to_owned())]);
    }

    // -----------------------------------------------------------------------------------------------------------
    // Version
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn the_version_marker_is_found_among_other_bytes() {
        let mut bytes = b"MZ\0\0 stuff ".to_vec();
        bytes.extend_from_slice(b"feverlink3-launcher-version=0.3.1-rc.2\0more stuff");
        assert_eq!(find_version_marker(&bytes), Some("0.3.1-rc.2".to_owned()));
        // the scan's own constant appears too, followed by something that is not a version: it is skipped
        let decoy = b"feverlink3-launcher-version=feverlink3-launcher-version=1.2.3\0";
        assert_eq!(find_version_marker(decoy), Some("1.2.3".to_owned()));
        assert_eq!(find_version_marker(b"feverlink3-launcher-version=\0"), None);
        assert_eq!(find_version_marker(b"feverlink3-launcher-version=banana\0"), None);
        assert_eq!(find_version_marker(b"feverlink3-launcher-version=1.2\0"), None);
        assert_eq!(
            find_version_marker(b"feverlink3-launcher-version=1.2.3"),
            Some("1.2.3".to_owned()),
            "ends at the data"
        );
        assert_eq!(find_version_marker(b""), None);
        assert_eq!(find_version_marker(b"no marker here"), None);
        let long = format!("feverlink3-launcher-version=1.2.3-{}\0", "a".repeat(200));
        assert_eq!(find_version_marker(long.as_bytes()), None, "a runaway version is not read");
    }

    #[test]
    fn this_launcher_carries_its_own_marker() {
        assert_eq!(own_version(), env!("CARGO_PKG_VERSION"));
        assert!(VERSION_MARKER.starts_with(MARKER_PREFIX));
        assert!(Version::parse(own_version()).is_ok());
        // what another launcher's wizard would find by reading this very program's file. A test build also holds
        // the sample markers of the tests above, so the real one is looked for among all of them.
        let bytes = fs::read(std::env::current_exe().unwrap()).unwrap();
        assert!(
            version_markers(&bytes).any(|v| v == own_version()),
            "{:?}",
            version_markers(&bytes).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_recorded_launchers_version_is_read_from_its_file() {
        let dir = TempDir::new().unwrap();
        let old = dir.path().join("old").join("feverlink3.exe");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        let mut bytes = vec![0x4d, 0x5a, 0, 1, 2];
        bytes.extend_from_slice(b"feverlink3-launcher-version=0.0.7\0");
        fs::write(&old, bytes).unwrap();
        let me = dir.path().join("new").join("feverlink3.exe");
        assert_eq!(read_launcher_version(&old, &me), Some("0.0.7".to_owned()));
        assert_eq!(read_launcher_version(&dir.path().join("gone.exe"), &me), None, "the folder was deleted");
        let unmarked = dir.path().join("unmarked.exe");
        fs::write(&unmarked, b"an older launcher without the marker").unwrap();
        assert_eq!(read_launcher_version(&unmarked, &me), None);
        // the record names the running launcher: no file is read at all (this path does not exist)
        assert_eq!(read_launcher_version(&me, &me), Some(own_version().to_owned()));
        let shouted = PathBuf::from(me.to_string_lossy().to_uppercase());
        assert_eq!(read_launcher_version(&shouted, &me), Some(own_version().to_owned()), "paths ignore case");
    }

    #[test]
    fn versions_are_ordered_when_both_are_known() {
        assert_eq!(version_order("0.2.0", Some("0.1.0")), Some(Ordering::Greater));
        assert_eq!(version_order("0.1.0", Some("0.1.0")), Some(Ordering::Equal));
        assert_eq!(version_order("0.1.0", Some("0.10.0")), Some(Ordering::Less));
        assert_eq!(version_order("1.0.0", Some("1.0.0-rc.1")), Some(Ordering::Greater));
        assert_eq!(version_order("0.1.0", None), None);
        assert_eq!(version_order("0.1.0", Some("garbage")), None);
    }

    // -----------------------------------------------------------------------------------------------------------
    // What is installed
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn installed_state_tells_not_installed_partial_complete_and_foreign_apart() {
        let mut f = base_facts();
        assert_eq!(installed_state(&f), InstallState::NotInstalled);

        f.game_files.as_mut().unwrap().dll = Ok(DllState::Ours);
        assert_eq!(installed_state(&f), InstallState::Partial { native_module: true, mods: 0, users: 1 });
        f.users[0].mod_installed = true;
        f.installed_version = Some("0.1.0".into());
        assert_eq!(installed_state(&f), InstallState::Complete { version: Some("0.1.0".into()) });

        f.game_files.as_mut().unwrap().dll = Ok(DllState::Absent);
        assert_eq!(installed_state(&f), InstallState::Partial { native_module: false, mods: 1, users: 1 });

        let mut only_path_file = base_facts();
        only_path_file.game_files.as_mut().unwrap().launcher = Some(PathBuf::from(r"C:\old\feverlink3.exe"));
        assert_eq!(installed_state(&only_path_file), InstallState::Partial { native_module: false, mods: 0, users: 1 });

        let mut foreign = base_facts();
        foreign.game_files.as_mut().unwrap().dll = Ok(DllState::Foreign);
        assert_eq!(installed_state(&foreign), InstallState::ForeignDll);

        let mut unreadable = base_facts();
        unreadable.game_files.as_mut().unwrap().dll = Err("denied".into());
        assert_eq!(installed_state(&unreadable), InstallState::Unknown);

        let mut no_game = base_facts();
        no_game.game_files = None;
        assert_eq!(installed_state(&no_game), InstallState::Unknown);

        let mut no_user_folder = installed_facts();
        no_user_folder.users.clear();
        assert_eq!(
            installed_state(&no_user_folder),
            InstallState::Partial { native_module: true, mods: 0, users: 0 },
            "the native module alone is not an install"
        );

        let mut two_users = installed_facts();
        two_users
            .users
            .push(UserDir { path: PathBuf::from(r"C:\Steam\userdata\2\3493540\local"), mod_installed: false });
        assert_eq!(installed_state(&two_users), InstallState::Partial { native_module: true, mods: 1, users: 2 });
    }

    #[test]
    fn temporary_places_are_recognised_but_ordinary_folders_are_not() {
        for temporary in [
            "C:/Users/Ann/AppData/Local/Temp/Temp1_FeverLink3-0.1.0.zip/FeverLink3-0.1.0/feverlink3.exe",
            "C:/Users/ANN~1/AppData/Local/TEMP/x/feverlink3.exe",
            "C:/Users/Ann/Downloads/Temp12_FeverLink3.ZIP/feverlink3.exe",
            "D:/Rar$EXa12345.678/feverlink3.exe",
            "D:/7zO8A3B2C1D/feverlink3.exe",
        ] {
            assert!(is_temporary_location(Path::new(temporary)), "{temporary}");
        }
        for ordinary in [
            "C:/Users/Ann/Downloads/FeverLink3-0.1.0/feverlink3.exe",
            "C:/Users/Ann/Desktop/Temp1_notes/feverlink3.exe",
            "C:/Users/Ann/Desktop/template_1.zip/feverlink3.exe",
            "C:/Users/Ann/AppData/Local/Programs/FeverLink3/feverlink3.exe",
            "C:/Temp/FeverLink3/feverlink3.exe",
            "feverlink3.exe",
        ] {
            assert!(!is_temporary_location(Path::new(ordinary)), "{ordinary}");
        }
    }

    #[test]
    fn the_package_is_complete_incomplete_or_temporary() {
        let mut f = base_facts();
        assert_eq!(package_state(&f), Package::Complete);
        f.payload_dll = None;
        assert_eq!(package_state(&f), Package::Incomplete { mod_missing: false, dll_missing: true });
        f.payload_mod = None;
        assert_eq!(package_state(&f), Package::Incomplete { mod_missing: true, dll_missing: true });
        f.exe_path = PathBuf::from("C:/Users/Ann/AppData/Local/Temp/Temp1_FeverLink3-0.1.0.zip/feverlink3.exe");
        assert_eq!(package_state(&f), Package::TemporaryFolder, "a temporary folder comes first");
    }

    // -----------------------------------------------------------------------------------------------------------
    // Checks screen
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn the_checks_of_a_healthy_pc_before_install() {
        let lines = render_checks(&checks(&base_facts()));
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0], r"  [ok]  Steam              C:\Steam");
        assert_eq!(lines[1], r"  [ok]  Transport Fever 3  C:\Steam\steamapps\common\Transport Fever 3");
        assert_eq!(lines[2], "  [ok]  Game build         40420 - FeverLink3 knows this build");
        assert_eq!(lines[3], "  [ok]  Game running       no");
        assert_eq!(lines[4], "  [--]  FeverLink3         not installed yet");
        assert_eq!(
            lines[5],
            format!(r"  [ok]  This folder        C:\FeverLink3-0.1.0 (version {}, all files present)", own_version())
        );
    }

    #[test]
    fn the_checks_say_what_is_wrong_in_plain_words() {
        let mut f = base_facts();
        f.steam_path = None;
        f.game_dir = None;
        f.game_files = None;
        f.build = BuildCheck::NoGame;
        let text = render_checks(&checks(&f)).join("\n");
        assert!(text.contains("[!!]  Steam"), "{text}");
        assert!(text.contains(STEAM_URL));
        assert!(text.contains("[!!]  Transport Fever 3  not found (Steam is missing)"), "{text}");
        assert!(text.contains("[--]  Game build         not checked"), "{text}");
        assert!(text.contains("[--]  FeverLink3         unknown"), "{text}");

        let mut game_missing = base_facts();
        game_missing.game_dir = None;
        assert!(render_checks(&checks(&game_missing)).join("\n").contains("not found in your Steam libraries"));

        let mut running = base_facts();
        running.game_running = true;
        assert!(render_checks(&checks(&running))[3].contains("[??]  Game running       yes - close it"));

        let mut unknown_build = base_facts();
        unknown_build.build = BuildCheck::Unknown { timestamp: 0x1234_abcd };
        let line = &render_checks(&checks(&unknown_build))[2];
        assert!(line.contains("[??]") && line.contains("0x1234abcd") && line.contains("reduced mode"), "{line}");

        let mut unreadable_build = base_facts();
        unreadable_build.build = BuildCheck::Unreadable("denied".into());
        assert!(render_checks(&checks(&unreadable_build))[2].contains("could not be read (denied)"));
    }

    #[test]
    fn the_install_line_shows_the_version_or_what_is_missing() {
        let line = |f: &Facts| installed_check(f).text;
        assert_eq!(line(&installed_facts()), format!("installed (version {})", own_version()));
        let mut unknown_version = installed_facts();
        unknown_version.installed_version = None;
        assert_eq!(line(&unknown_version), "installed (version not recorded)");
        let mut partial = installed_facts();
        partial.users[0].mod_installed = false;
        assert!(line(&partial).contains("native module: yes, mod: 0 of 1 Steam users"), "{}", line(&partial));
        let mut foreign = base_facts();
        foreign.game_files.as_mut().unwrap().dll = Ok(DllState::Foreign);
        assert_eq!(installed_check(&foreign).status, Status::Bad);
    }

    #[test]
    fn the_folder_line_warns_about_zip_and_incomplete_folders() {
        let mut f = base_facts();
        f.payload_mod = None;
        let check = folder_check(&f);
        assert_eq!((check.status, check.text.as_str()), (Status::Bad, "incomplete - unzip the whole zip file first"));
        f.exe_path = PathBuf::from("C:/Users/Ann/AppData/Local/Temp/Temp1_FeverLink3-0.1.0.zip/feverlink3.exe");
        assert!(folder_check(&f).text.contains("zip file or a temporary folder"));
    }

    #[test]
    fn next_step_names_the_first_thing_to_do() {
        let next = |f: &Facts| next_step(f);
        assert_eq!(next(&base_facts()), "choose 1 to install FeverLink3.");
        assert!(next(&installed_facts()).starts_with("choose 4 to start Transport Fever 3"));

        let mut no_steam = base_facts();
        no_steam.steam_path = None;
        assert_eq!(next(&no_steam), Problem::SteamMissing.message());
        let mut no_game = base_facts();
        no_game.game_dir = None;
        assert_eq!(next(&no_game), Problem::GameMissing.message());

        let mut running = base_facts();
        running.game_running = true;
        assert_eq!(next(&running), "close Transport Fever 3, then choose 1 to install FeverLink3.");
        let mut playing = installed_facts();
        playing.game_running = true;
        assert!(next(&playing).contains("click FeverLink3 in the bar at the bottom"));

        let mut partial = base_facts();
        partial.game_files.as_mut().unwrap().dll = Ok(DllState::Ours);
        assert_eq!(next(&partial), "choose 1 to finish installing FeverLink3.");

        let mut incomplete = base_facts();
        incomplete.payload_mod = None;
        assert_eq!(next(&incomplete), Problem::PackageIncomplete { mod_missing: true, dll_missing: false }.message());
        let mut zipped = base_facts();
        zipped.exe_path = PathBuf::from("C:/Users/Ann/AppData/Local/Temp/Temp1_FeverLink3-0.1.0.zip/feverlink3.exe");
        assert_eq!(next(&zipped), Problem::TemporaryFolder.message());

        let mut foreign = base_facts();
        foreign.game_files.as_mut().unwrap().dll = Ok(DllState::Foreign);
        assert!(next(&foreign).contains("does not belong to FeverLink3"));

        // installed, but this folder holds other files: the comparison is offered
        let mut changed = installed_facts();
        changed.plan = Some(report(vec![step(Item::Mod, Outcome::Done, "updated (mirror copy)")]));
        assert!(next(&changed).contains("Choose 3 to compare"), "{}", next(&changed));
    }

    fn step_at(item: Item, path: &str, outcome: Outcome, reason: &str) -> Step {
        Step { item, path: PathBuf::from(path), outcome, reason: reason.to_owned() }
    }

    #[test]
    fn the_screen_holds_the_banner_the_checks_the_next_step_and_the_menu() {
        let screen = render_screen(&base_facts());
        assert!(screen.starts_with(&format!("FeverLink3 {} - cooperative multiplayer", own_version())));
        let order: Vec<usize> =
            ["Checking this PC...", "[ok]  Steam", "Next step: choose 1", "What would you like to do?"]
                .iter()
                .map(|part| screen.find(part).unwrap_or_else(|| panic!("{part} is on the screen")))
                .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "in this order: {order:?}");
        assert!(screen.ends_with("Type a number and press Enter.\n"));
    }

    // -----------------------------------------------------------------------------------------------------------
    // What each choice may do
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn install_is_planned_only_when_nothing_stands_in_the_way() {
        let f = base_facts();
        let plan = plan_install(&f).unwrap();
        assert_eq!(plan.game_dir, f.game_dir.as_deref().unwrap());
        assert_eq!(plan.mod_source, f.payload_mod.as_deref().unwrap());
        assert_eq!(plan.native_dll, f.payload_dll.as_deref().unwrap());

        let blocked = |change: fn(&mut Facts)| {
            let mut f = base_facts();
            change(&mut f);
            plan_install(&f).unwrap_err()
        };
        assert_eq!(blocked(|f| f.steam_path = None), Problem::SteamMissing);
        assert_eq!(blocked(|f| f.game_dir = None), Problem::GameMissing);
        assert_eq!(blocked(|f| f.game_running = true), Problem::GameRunning);
        assert_eq!(
            blocked(|f| f.payload_mod = None),
            Problem::PackageIncomplete { mod_missing: true, dll_missing: false }
        );
        assert_eq!(
            blocked(|f| f.payload_dll = None),
            Problem::PackageIncomplete { mod_missing: false, dll_missing: true }
        );
        assert_eq!(blocked(|f| f.exe_path = PathBuf::from("D:/Rar$EXa1.2/feverlink3.exe")), Problem::TemporaryFolder);
        assert!(matches!(
            blocked(|f| f.game_files.as_mut().unwrap().dll = Ok(DllState::Foreign)),
            Problem::ForeignDll(path) if path.ends_with("winhttp.dll")
        ));
        // Steam wins over the game, the game over the folder, the folder over the running game
        let mut everything_wrong = base_facts();
        everything_wrong.steam_path = None;
        everything_wrong.game_running = true;
        everything_wrong.payload_mod = None;
        assert_eq!(plan_install(&everything_wrong).unwrap_err(), Problem::SteamMissing);
        everything_wrong.steam_path = base_facts().steam_path;
        assert!(matches!(plan_install(&everything_wrong).unwrap_err(), Problem::PackageIncomplete { .. }));
    }

    #[test]
    fn uninstall_needs_the_game_folder_and_a_closed_game() {
        let f = base_facts();
        assert_eq!(plan_uninstall(&f).unwrap(), f.game_dir.as_deref().unwrap());
        let mut no_steam = base_facts();
        no_steam.steam_path = None;
        assert_eq!(plan_uninstall(&no_steam).unwrap_err(), Problem::SteamMissing);
        let mut no_game = base_facts();
        no_game.game_dir = None;
        assert_eq!(plan_uninstall(&no_game).unwrap_err(), Problem::GameMissing);
        let mut running = base_facts();
        running.game_running = true;
        assert_eq!(plan_uninstall(&running).unwrap_err(), Problem::GameRunning);
        // an incomplete folder does not matter: uninstall needs nothing from it
        let mut incomplete = base_facts();
        incomplete.payload_mod = None;
        assert!(plan_uninstall(&incomplete).is_ok());
    }

    #[test]
    fn starting_through_steam_needs_steam_and_a_game_that_is_not_running() {
        let f = base_facts();
        assert_eq!(plan_start(&f).unwrap(), Path::new(r"C:\Steam"));
        let mut no_steam = base_facts();
        no_steam.steam_path = None;
        assert_eq!(plan_start(&no_steam).unwrap_err(), Problem::SteamMissing);
        let mut no_exe = base_facts();
        no_exe.steam_exe = None;
        assert_eq!(
            plan_start(&no_exe).unwrap_err(),
            Problem::SteamExeMissing(PathBuf::from(r"C:\Steam").join("steam.exe"))
        );
        let mut running = base_facts();
        running.game_running = true;
        assert_eq!(plan_start(&running).unwrap_err(), Problem::GameAlreadyRunning);
        // Steam knows where the game is; the wizard not finding it is no reason to refuse
        let mut game_not_found = base_facts();
        game_not_found.game_dir = None;
        assert!(plan_start(&game_not_found).is_ok());
    }

    #[test]
    fn the_log_folder_must_exist_to_be_opened() {
        let f = base_facts();
        assert_eq!(plan_logs(&f).unwrap(), f.local_dir.as_deref().unwrap());
        let mut none_yet = base_facts();
        none_yet.local_dir_exists = false;
        assert!(matches!(plan_logs(&none_yet).unwrap_err(), Problem::NoLogsYet(p) if p.ends_with("FeverLink3")));
        let mut no_appdata = base_facts();
        no_appdata.local_dir = None;
        assert_eq!(plan_logs(&no_appdata).unwrap_err(), Problem::NoLocalAppData);
    }

    #[test]
    fn steam_is_asked_to_launch_the_game_by_its_app_id() {
        let cmd = steam_start_command(Path::new(r"C:\Program Files (x86)\Steam"));
        assert_eq!(cmd.get_program(), Path::new(r"C:\Program Files (x86)\Steam").join("steam.exe").as_os_str());
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["-applaunch", "3493540"]);
        assert_eq!(cmd.get_current_dir(), Some(Path::new(r"C:\Program Files (x86)\Steam")));
    }

    #[test]
    fn programs_of_the_windows_folder_are_started_by_absolute_path() {
        let clip = windows_program(&["System32", "clip.exe"]);
        assert!(clip.is_absolute(), "{}", clip.display());
        assert!(clip.ends_with(Path::new("System32").join("clip.exe")));
        assert!(windows_program(&["explorer.exe"]).ends_with("explorer.exe"));
    }

    // -----------------------------------------------------------------------------------------------------------
    // Messages
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn every_problem_says_what_to_do_in_plain_words() {
        let path = PathBuf::from(r"C:\Game\winhttp.dll");
        let cases: Vec<(Problem, &[&str])> = vec![
            (Problem::SteamMissing, &["Steam was not found", STEAM_URL]),
            (Problem::SteamExeMissing(path.clone()), &["Steam's program file", "winhttp.dll"]),
            (Problem::GameMissing, &["Transport Fever 3 was not found", "Steam libraries"]),
            (Problem::GameRunning, &["is running", "Close the game completely", "Task Manager"]),
            (Problem::GameAlreadyRunning, &["already running", "Alt+Tab"]),
            (Problem::TemporaryFolder, &["inside the downloaded zip", "Extract All"]),
            (
                Problem::PackageIncomplete { mod_missing: true, dll_missing: true },
                &["mod folder", "winhttp.dll are missing", "Extract All"],
            ),
            (
                Problem::PackageIncomplete { mod_missing: true, dll_missing: false },
                &["mod folder (mod\\feverlink3_1) is missing"],
            ),
            (Problem::PackageIncomplete { mod_missing: false, dll_missing: true }, &["winhttp.dll is missing"]),
            (Problem::PackageDamaged, &["not FeverLink3's native module", "Download the zip file again"]),
            (Problem::ForeignDll(path.clone()), &["does not belong to FeverLink3", "never overwrites", "winhttp.dll"]),
            (Problem::NoUserFolder, &["has not created its Steam user folder", "Start the game once"]),
            (
                Problem::FilesInUse(path.clone()),
                &["in use by another program", "fully closed", "antivirus", "winhttp.dll"],
            ),
            (
                Problem::PermissionDenied(path.clone()),
                &["did not allow", "Run as administrator", "controlled folder access"],
            ),
            (Problem::DiskFull, &["disk is full"]),
            (Problem::NoLocalAppData, &["LOCALAPPDATA"]),
            (Problem::NoLogsYet(path.clone()), &["no logs yet", "start the game (4)"]),
            (Problem::Other("boom".into()), &["unexpected", "boom"]),
        ];
        for (problem, words) in cases {
            let message = problem.message();
            for word in words {
                assert!(message.contains(word), "{problem:?}: {message:?} lacks {word:?}");
            }
            assert!(message.is_ascii() || matches!(problem, Problem::Other(_)), "{message}");
        }
    }

    #[test]
    fn install_failures_are_sorted_by_the_operating_system_code_first() {
        let path = Path::new(r"C:\Game\winhttp.dll");
        let classify = |reason: &str| classify_reason(Item::NativeDll, reason, path);
        let in_use = Problem::FilesInUse(path.to_path_buf());
        // the words of the message change with the language of Windows; the code does not
        assert_eq!(
            classify("updated: The process cannot access the file because it is being used by another process. (os error 32)"),
            in_use
        );
        assert_eq!(classify("updated: Der Prozess kann nicht auf die Datei zugreifen (os error 32)"), in_use);
        assert_eq!(classify("updated: العملية لا يمكنها الوصول إلى الملف (os error 33)"), in_use);
        assert_eq!(
            classify("installed: The requested operation cannot be performed on a file with a user-mapped section open. (os error 1224)"),
            in_use
        );
        assert_eq!(
            classify("updated: Access is denied. (os error 5) (is the game running? close it and install again)"),
            Problem::PermissionDenied(path.to_path_buf())
        );
        assert_eq!(classify("installed: There is not enough space on the disk. (os error 112)"), Problem::DiskFull);
        // install.rs's own words, when there is no code
        assert_eq!(classify("removed: something (is the game running?)"), in_use);
        assert_eq!(
            classify("a winhttp.dll that is not ours is already in the game folder (another mod?); not overwritten"),
            Problem::ForeignDll(path.to_path_buf())
        );
        assert_eq!(
            classify("the shipped module lacks the marker \"feverlink3_native\"; refusing to install"),
            Problem::PackageDamaged
        );
        assert_eq!(classify("could not read the native module: x (os error 2)"), Problem::PackageDamaged);
        assert_eq!(
            classify_reason(Item::Mod, "mod source folder is missing", path),
            Problem::PackageIncomplete { mod_missing: true, dll_missing: false }
        );
        assert_eq!(
            classify("no native module shipped with this launcher"),
            Problem::PackageIncomplete { mod_missing: false, dll_missing: true }
        );
        assert_eq!(
            classify_reason(
                Item::Mod,
                "no Steam user folder userdata/<id>/3493540/local exists; start the game once",
                path
            ),
            Problem::NoUserFolder
        );
        assert_eq!(
            classify_reason(Item::PathFile, "written: strange", path),
            Problem::Other("feverlink3_path.txt: written: strange".into())
        );
        assert_eq!(os_error_code("x (os error 32) y"), Some(32));
        assert_eq!(os_error_code("no code here"), None);
        assert_eq!(os_error_code("(os error x)"), None);
        assert_eq!(os_error_code("(os error 5"), None);
    }

    #[test]
    fn a_clean_install_ends_with_what_to_do_next() {
        let r = report(vec![
            step(Item::Mod, Outcome::Done, "installed"),
            step(Item::AppIdFile, Outcome::Done, "created"),
            step(Item::NativeDll, Outcome::Done, "installed"),
            step(Item::PathFile, Outcome::Done, "written"),
        ]);
        let summary = summarize_install(&r);
        assert!(summary.ok);
        assert_eq!(summary.headline, "FeverLink3 is installed.");
        assert!(summary.notes.iter().any(|n| n.contains("Choose 4")));
        assert!(summary.notes.iter().any(|n| n.contains("Add Mods")));
    }

    #[test]
    fn an_install_that_changed_nothing_says_so() {
        let r = report(vec![
            step(Item::Mod, Outcome::Skipped, "already up to date"),
            step(Item::NativeDll, Outcome::Skipped, "already installed, identical"),
        ]);
        let summary = summarize_install(&r);
        assert!(summary.ok);
        assert!(summary.headline.contains("already installed and up to date"));
        assert!(summary.notes.is_empty());
    }

    #[test]
    fn an_install_without_a_steam_user_folder_is_only_partly_done() {
        let r = report(vec![
            step(
                Item::Mod,
                Outcome::Skipped,
                "no Steam user folder userdata/<id>/3493540/local exists; start the game once",
            ),
            step(Item::NativeDll, Outcome::Done, "installed"),
        ]);
        let summary = summarize_install(&r);
        assert!(!summary.ok);
        assert_eq!(summary.headline, "FeverLink3 is only partly installed.");
        assert_eq!(summary.notes, vec![Problem::NoUserFolder.message()]);
    }

    #[test]
    fn failed_steps_become_one_message_per_distinct_problem() {
        let r = report(vec![
            step_at(
                Item::Mod,
                r"C:\Steam\mods\feverlink3_1",
                Outcome::Failed,
                "installed: Access is denied. (os error 5)",
            ),
            step_at(
                Item::NativeDll,
                r"C:\Game\winhttp.dll",
                Outcome::Failed,
                "updated: Access is denied. (os error 5)",
            ),
            step(Item::PathFile, Outcome::Done, "written"),
        ]);
        let summary = summarize_install(&r);
        assert!(!summary.ok);
        assert_eq!(summary.headline, "FeverLink3 could not be installed completely.");
        assert_eq!(summary.notes.len(), 2, "two files, two paths, two messages");
        let same_path = report(vec![
            step(Item::NativeDll, Outcome::Failed, "x (os error 32)"),
            step(Item::NativeDll, Outcome::Failed, "y (os error 32)"),
        ]);
        assert_eq!(summarize_install(&same_path).notes.len(), 1, "the same problem is said once");
        let uninstall = summarize_uninstall(&r);
        assert!(!uninstall.ok);
        assert_eq!(uninstall.headline, "FeverLink3 could not be removed completely.");
    }

    #[test]
    fn an_uninstall_says_what_was_removed_or_that_there_was_nothing() {
        let removed =
            report(vec![step(Item::Mod, Outcome::Done, "removed"), step(Item::NativeDll, Outcome::Done, "removed")]);
        let summary = summarize_uninstall(&removed);
        assert!(summary.ok);
        assert!(summary.headline.contains("was removed") && summary.headline.contains("exactly as before"));
        assert!(summary.notes[0].contains("delete this FeverLink3 folder"));

        let nothing = report(vec![
            step(Item::Mod, Outcome::Skipped, "not installed"),
            step(Item::NativeDll, Outcome::Skipped, "not installed"),
        ]);
        let summary = summarize_uninstall(&nothing);
        assert!(summary.ok);
        assert_eq!(summary.headline, "FeverLink3 was not installed. Nothing was removed.");

        // the mod's "no Steam user folder" skip is not an error on the way out
        let no_users = report(vec![
            step(Item::Mod, Outcome::Skipped, "no Steam user folder found"),
            step(Item::NativeDll, Outcome::Done, "removed"),
        ]);
        assert!(summarize_uninstall(&no_users).ok);

        let foreign = report(vec![
            step(Item::NativeDll, Outcome::Skipped, "not ours; left alone"),
            step(Item::PathFile, Outcome::Done, "removed"),
        ]);
        let summary = summarize_uninstall(&foreign);
        assert!(summary.notes.iter().any(|n| n.contains("does not belong to FeverLink3")));
    }

    // -----------------------------------------------------------------------------------------------------------
    // This folder against the installed files
    // -----------------------------------------------------------------------------------------------------------

    fn with_plan(mut f: Facts, steps: Vec<Step>) -> Facts {
        f.plan = Some(Report { dry_run: true, steps });
        f
    }

    #[test]
    fn the_comparison_reads_the_dry_run() {
        let identical = vec![
            step(Item::Mod, Outcome::Skipped, "already up to date"),
            step(Item::AppIdFile, Outcome::Skipped, "already present"),
            step(Item::NativeDll, Outcome::Skipped, "already installed, identical"),
            step(Item::PathFile, Outcome::Skipped, "already up to date"),
        ];
        assert_eq!(compare(&with_plan(installed_facts(), identical.clone())), Ok(Comparison::Identical));

        let mut mod_changed = identical.clone();
        mod_changed[0] = step(Item::Mod, Outcome::Done, "updated (mirror copy)");
        mod_changed.push(step(Item::Mod, Outcome::Done, "updated (mirror copy)"));
        assert_eq!(
            compare(&with_plan(installed_facts(), mod_changed)),
            Ok(Comparison::Differs(vec!["the mod"])),
            "two Steam users, one piece"
        );

        let mut both = identical.clone();
        both[2] = step(Item::NativeDll, Outcome::Done, "updated");
        both[0] = step(Item::Mod, Outcome::Done, "updated (mirror copy)");
        assert_eq!(
            compare(&with_plan(installed_facts(), both)),
            Ok(Comparison::Differs(vec!["the mod", "the native module (winhttp.dll)"]))
        );

        let mut moved = identical.clone();
        moved[3] = step(Item::PathFile, Outcome::Done, "updated");
        moved[1] = step(Item::AppIdFile, Outcome::Done, "created");
        assert_eq!(
            compare(&with_plan(installed_facts(), moved)),
            Ok(Comparison::HelpersDiffer(vec!["steam_appid.txt", "the recorded launcher location"]))
        );

        let fresh =
            vec![step(Item::Mod, Outcome::Done, "installed"), step(Item::NativeDll, Outcome::Done, "installed")];
        assert_eq!(compare(&with_plan(base_facts(), fresh)), Ok(Comparison::NotInstalled));
    }

    #[test]
    fn the_comparison_explains_why_it_cannot_compare() {
        assert_eq!(
            compare(&{
                let mut f = base_facts();
                f.steam_path = None;
                f
            }),
            Err(Problem::SteamMissing)
        );
        assert_eq!(
            compare(&{
                let mut f = base_facts();
                f.game_dir = None;
                f
            }),
            Err(Problem::GameMissing)
        );
        let mut no_payload = base_facts();
        no_payload.payload_mod = None;
        assert_eq!(compare(&no_payload), Err(Problem::PackageIncomplete { mod_missing: true, dll_missing: false }));
        let foreign =
            with_plan(base_facts(), vec![step(Item::NativeDll, Outcome::Failed, "not ours; not overwritten")]);
        assert!(matches!(compare(&foreign), Err(Problem::ForeignDll(_))));
    }

    #[test]
    fn the_comparison_text_gives_a_verdict_in_words() {
        let identical = vec![step(Item::Mod, Outcome::Skipped, "x"), step(Item::NativeDll, Outcome::Skipped, "x")];
        let text = comparison_lines(&with_plan(installed_facts(), identical)).join("\n");
        assert!(text.contains(&format!("This folder:  version {}", own_version())), "{text}");
        assert!(text.contains(&format!("Installed:    version {}", own_version())), "{text}");
        assert!(text.contains("identical to the ones in this folder. Nothing to do."), "{text}");
        assert!(text.contains("not built in yet") && text.contains(RELEASES_URL), "no network yet: {text}");

        let differs = vec![step(Item::Mod, Outcome::Done, "updated")];
        let verdict = |installed_version: Option<&str>| {
            let mut f = with_plan(installed_facts(), differs.clone());
            f.installed_version = installed_version.map(str::to_owned);
            comparison_lines(&f).join("\n")
        };
        assert!(verdict(Some("0.0.1")).contains("This folder is NEWER than what is installed"));
        assert!(verdict(Some("99.0.0")).contains("This folder is OLDER than what is installed"));
        assert!(verdict(Some(own_version())).contains("The version number is the same, but the mod differ"));
        let unknown = verdict(None);
        assert!(unknown.contains("installed version is not known"), "{unknown}");
        assert!(unknown.contains("version not recorded"), "{unknown}");

        let helpers =
            comparison_lines(&with_plan(installed_facts(), vec![step(Item::PathFile, Outcome::Done, "updated")]))
                .join("\n");
        assert!(helpers.contains("Only the recorded launcher location would be updated"), "{helpers}");
        let fresh =
            comparison_lines(&with_plan(base_facts(), vec![step(Item::Mod, Outcome::Done, "installed")])).join("\n");
        assert!(fresh.contains("not installed. Choose 1"), "{fresh}");
        let mut no_game = base_facts();
        no_game.game_dir = None;
        assert!(comparison_lines(&no_game).join("\n").contains("Transport Fever 3 was not found"));
    }

    // -----------------------------------------------------------------------------------------------------------
    // Doctor text
    // -----------------------------------------------------------------------------------------------------------

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
        let n = names(&["AbdulazizAlshehri", "Abdul", "  ", "", "abdulazizalshehri"]);
        assert_eq!(
            redact_user(r"C:\Users\ABDULA~1\AppData\Local\Temp and AbdulazizAlshehri and Abdul", &n),
            r"C:\Users\<user>\AppData\Local\Temp and <user> and <user>"
        );
        assert_eq!(redact_user("ABDULA~12 end", &n), "<user> end");
        assert_eq!(redact_user("ABDULA~ end", &n), "ABDULA~ end", "a short form needs its digit");
        assert_eq!(redact_user("ABDULA end", &n), "ABDULA end");
        // the longest name wins where one contains the other
        assert_eq!(redact_user("AbdulazizAlshehri", &names(&["Abdul", "AbdulazizAlshehri"])), "<user>");
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

    // -----------------------------------------------------------------------------------------------------------
    // Clipboard bytes
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn the_clipboard_gets_utf16_with_a_byte_order_mark_and_windows_line_breaks() {
        assert_eq!(clipboard_bytes("a\nb"), [0xFF, 0xFE, b'a', 0, b'\r', 0, b'\n', 0, b'b', 0]);
        assert_eq!(clipboard_bytes("a\r\nb"), clipboard_bytes("a\nb"), "no doubled returns");
        assert_eq!(clipboard_bytes(""), [0xFF, 0xFE]);
        assert_eq!(clipboard_bytes("\u{0639}"), [0xFF, 0xFE, 0x39, 0x06]);
        assert_eq!(clipboard_bytes("\u{1F680}"), [0xFF, 0xFE, 0x3D, 0xD8, 0x80, 0xDE], "a surrogate pair");
    }

    // -----------------------------------------------------------------------------------------------------------
    // The snapshot of a PC
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn the_snapshot_of_a_fresh_pc_is_read_from_its_folders() {
        let pc = Pc::new();
        let f = gather(&pc);
        assert_eq!(f.steam_path, pc.steam);
        assert_eq!(f.game_dir, pc.game);
        assert_eq!(f.steam_exe, Some(pc.steam.clone().unwrap().join("steam.exe")));
        assert_eq!(f.build, BuildCheck::Known { name: "40420" });
        assert_eq!(f.users.len(), 1);
        assert!(!f.users[0].mod_installed);
        assert_eq!(installed_state(&f), InstallState::NotInstalled);
        assert_eq!(package_state(&f), Package::Complete);
        assert!(!f.game_running);
        assert!(!f.local_dir_exists, "nothing has created the log folder");
        let plan = f.plan.expect("a dry-run install is planned");
        assert!(plan.dry_run && plan.ok());
        assert!(plan.steps.iter().all(|s| s.outcome == Outcome::Done), "a fresh PC would get everything: {plan}");
        assert!(!pc.game().join(install::DLL_NAME).exists(), "gathering changes nothing");
    }

    #[test]
    fn the_snapshot_notices_what_is_missing() {
        let mut pc = Pc::new();
        pc.steam = None;
        pc.game = None;
        pc.users.clear();
        pc.payload = (None, None);
        let f = gather(&pc);
        assert!(f.steam_path.is_none() && f.game_dir.is_none() && f.steam_exe.is_none() && f.plan.is_none());
        assert_eq!(f.build, BuildCheck::NoGame);
        assert_eq!(installed_state(&f), InstallState::Unknown);

        let no_exe = Pc::new();
        fs::remove_file(no_exe.steam.clone().unwrap().join("steam.exe")).unwrap();
        no_exe.running.set(true);
        let f = gather(&no_exe);
        assert!(f.steam_exe.is_none() && f.game_running);
        fs::create_dir_all(no_exe.local()).unwrap();
        assert!(gather(&no_exe).local_dir_exists);
    }

    // -----------------------------------------------------------------------------------------------------------
    // The menu loop on a fake PC
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn the_menu_ends_quietly_when_the_input_ends() {
        let pc = Pc::new();
        let out = drive(&pc, "");
        assert!(out.contains("Checking this PC...") && out.contains("What would you like to do?"), "{out}");
        assert_eq!(out.matches("What would you like to do?").count(), 1, "drawn once, then the input ended");
        let after_choice = drive(&pc, "5\n");
        assert!(after_choice.contains("Press Enter to go back to the menu."), "{after_choice}");
    }

    #[test]
    fn exit_ends_the_loop_and_does_nothing_else() {
        let pc = Pc::new();
        let out = drive(&pc, "0\n1\n1\n");
        assert_eq!(out.matches("What would you like to do?").count(), 1);
        assert!(!pc.game().join(install::DLL_NAME).exists(), "the lines after Exit are never read");
        assert!(!drive(&pc, "q\n").contains("Press Enter"));
    }

    #[test]
    fn unknown_text_and_empty_lines_keep_the_menu_open() {
        let pc = Pc::new();
        let out = drive(&pc, "\nhello\n9\n\n0\n");
        assert!(out.contains("\"hello\" is not on the menu"), "{out}");
        assert!(out.contains("\"9\" is not on the menu"), "{out}");
        assert_eq!(out.matches("is not on the menu").count(), 2, "an empty line is not an error");
        assert_eq!(out.matches("What would you like to do?").count(), 1, "no redraw for a typing slip");
    }

    #[test]
    fn every_action_waits_for_enter_and_then_draws_the_menu_again() {
        for key in ["1", "3", "4", "5", "6"] {
            let pc = Pc::new();
            let out = drive(&pc, &format!("{key}\n\n0\n"));
            assert_eq!(out.matches("Press Enter to go back to the menu.").count(), 1, "{key}: {out}");
            assert_eq!(out.matches("What would you like to do?").count(), 2, "{key}");
            let wait = out.find("Press Enter").unwrap();
            assert!(out[wait..].contains("Checking this PC..."), "{key}: the menu follows the wait");
        }
        // uninstall asks first; the answer, then the wait
        let pc = Pc::new();
        let out = drive(&pc, "2\nn\n\n0\n");
        assert_eq!(out.matches("Press Enter to go back to the menu.").count(), 1, "{out}");
    }

    #[test]
    fn install_through_the_menu_puts_the_four_things_in_place() {
        let pc = Pc::new();
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("Installing FeverLink3 ..."), "{out}");
        assert!(out.contains("[ok] FeverLink3 is installed."), "{out}");
        assert!(out.contains("Add Mods"), "{out}");
        assert!(pc.game().join(install::DLL_NAME).is_file());
        assert!(pc.game().join(install::APPID_FILE).is_file());
        assert_eq!(fs::read_to_string(pc.game().join(install::PATH_FILE)).unwrap(), pc.exe.to_string_lossy());
        assert_eq!(fs::read_to_string(pc.installed_mod().join("mod.json")).unwrap(), "{}");
        assert_eq!(fs::read_to_string(pc.installed_mod().join("content").join("a.lua")).unwrap(), "a");
        // the next screen knows
        let second_screen = &out[out.find("Press Enter").unwrap()..];
        assert!(second_screen.contains(&format!("installed (version {})", own_version())), "{second_screen}");
        assert!(second_screen.contains("Next step: choose 4"), "{second_screen}");
    }

    #[test]
    fn installing_again_changes_nothing_and_says_so() {
        let pc = Pc::new();
        drive(&pc, "1\n\n0\n");
        let before = fs::read(pc.game().join(install::DLL_NAME)).unwrap();
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("already installed and up to date"), "{out}");
        assert!(!out.contains("DONE"), "{out}");
        assert_eq!(fs::read(pc.game().join(install::DLL_NAME)).unwrap(), before);
    }

    #[test]
    fn a_changed_mod_is_repaired_by_installing_again() {
        let pc = Pc::new();
        drive(&pc, "1\n\n0\n");
        fs::write(pc.installed_mod().join("content").join("a.lua"), "damaged").unwrap();
        fs::remove_file(pc.game().join(install::APPID_FILE)).unwrap();
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("FeverLink3 is installed."), "{out}");
        assert_eq!(fs::read_to_string(pc.installed_mod().join("content").join("a.lua")).unwrap(), "a");
        assert!(pc.game().join(install::APPID_FILE).is_file());
    }

    #[test]
    fn install_is_refused_while_the_game_runs() {
        let pc = Pc::new();
        pc.running.set(true);
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("Transport Fever 3 is running, so its files are locked"), "{out}");
        assert!(out.contains("[??]  Game running       yes"), "{out}");
        assert!(!out.contains("Installing FeverLink3"), "{out}");
        assert!(!pc.game().join(install::DLL_NAME).exists());
        assert!(!pc.game().join(install::PATH_FILE).exists());
        assert!(!pc.installed_mod().exists());
    }

    #[test]
    fn install_without_steam_or_game_explains_and_changes_nothing() {
        let mut pc = Pc::new();
        let game = pc.game();
        pc.steam = None;
        pc.game = None;
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("Steam was not found on this PC"), "{out}");
        assert!(!game.join(install::DLL_NAME).exists());

        let mut no_game = Pc::new();
        no_game.game = None;
        let out = drive(&no_game, "1\n\n0\n");
        assert!(out.contains("Transport Fever 3 was not found in your Steam libraries"), "{out}");
    }

    #[test]
    fn install_from_an_incomplete_or_zipped_folder_says_to_unzip() {
        let mut pc = Pc::new();
        pc.payload.0 = None;
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("This FeverLink3 folder is incomplete"), "{out}");
        assert!(out.contains("Extract All"), "{out}");
        assert!(!pc.game().join(install::DLL_NAME).exists(), "not even a partial install");
        assert!(!pc.game().join(install::APPID_FILE).exists());

        let mut zipped = Pc::new();
        zipped.exe =
            PathBuf::from("C:/Users/Ann/AppData/Local/Temp/Temp1_FeverLink3-0.1.0.zip/FeverLink3-0.1.0/feverlink3.exe");
        let out = drive(&zipped, "1\n\n0\n");
        assert!(out.contains("running from inside the downloaded zip file"), "{out}");
        assert!(!zipped.game().join(install::PATH_FILE).exists(), "no temporary path is recorded");
    }

    #[test]
    fn another_mods_winhttp_dll_stops_the_install_and_is_never_touched() {
        let pc = Pc::new();
        fs::write(pc.game().join(install::DLL_NAME), b"MZ another mod's proxy").unwrap();
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("does not belong to FeverLink3"), "{out}");
        assert!(out.contains("[!!]  FeverLink3"), "{out}");
        assert_eq!(fs::read(pc.game().join(install::DLL_NAME)).unwrap(), b"MZ another mod's proxy");
        assert!(!pc.installed_mod().exists(), "the mod is not installed next to a DLL that cannot load it");
    }

    #[test]
    fn without_a_steam_user_folder_the_install_is_partial_and_says_how_to_finish() {
        let mut pc = Pc::new();
        pc.users.clear();
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("[!!] FeverLink3 is only partly installed."), "{out}");
        assert!(out.contains("has not created its Steam user folder yet"), "{out}");
        assert!(pc.game().join(install::DLL_NAME).is_file());
        assert!(out.contains("only partly installed (native module: yes, mod: 0 of 0"), "{out}");
    }

    #[test]
    fn a_failing_write_is_explained_in_plain_words() {
        let pc = Pc::new();
        // a folder where the DLL belongs makes the write fail, as a lock or a permission problem would
        fs::create_dir_all(pc.game().join(install::DLL_NAME)).unwrap();
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("FeverLink3 could not be installed completely."), "{out}");
        assert!(out.contains("FAILED") || out.contains("[!!]"), "{out}");
    }

    #[test]
    fn uninstall_asks_first_and_removes_exactly_what_was_installed() {
        let pc = Pc::new();
        drive(&pc, "1\n\n0\n");
        let other_mod = install::mod_dir(&pc.users[0]).with_file_name("someone_elses_mod_1");
        fs::create_dir_all(&other_mod).unwrap();

        let declined = drive(&pc, "2\n\n\n0\n");
        assert!(declined.contains("Type y and press Enter to continue"), "{declined}");
        assert!(declined.contains("Nothing was changed."), "{declined}");
        assert!(pc.game().join(install::DLL_NAME).is_file(), "an empty answer means no");

        let removed = drive(&pc, "2\ny\n\n0\n");
        assert!(removed.contains("Removing FeverLink3 ..."), "{removed}");
        assert!(removed.contains("FeverLink3 was removed. Transport Fever 3 starts exactly as before."), "{removed}");
        assert!(!pc.game().join(install::DLL_NAME).exists());
        assert!(!pc.game().join(install::APPID_FILE).exists());
        assert!(!pc.game().join(install::PATH_FILE).exists());
        assert!(!pc.installed_mod().exists());
        assert!(other_mod.is_dir(), "another mod is left alone");
        assert!(pc.game().join(GAME_EXE).is_file());

        let again = drive(&pc, "2\nyes\n\n0\n");
        assert!(again.contains("FeverLink3 was not installed. Nothing was removed."), "{again}");
    }

    #[test]
    fn uninstall_is_refused_while_the_game_runs() {
        let pc = Pc::new();
        drive(&pc, "1\n\n0\n");
        pc.running.set(true);
        let out = drive(&pc, "2\ny\n\n0\n");
        assert!(out.contains("Transport Fever 3 is running"), "{out}");
        assert!(!out.contains("Type y and press Enter"), "no question for something that cannot be done: {out}");
        assert!(pc.game().join(install::DLL_NAME).is_file());
    }

    #[test]
    fn the_version_check_compares_this_folder_with_the_installed_files() {
        let pc = Pc::new();
        let fresh = drive(&pc, "3\n\n0\n");
        assert!(fresh.contains("FeverLink3 is not installed. Choose 1"), "{fresh}");

        drive(&pc, "1\n\n0\n");
        let same = drive(&pc, "3\n\n0\n");
        assert!(same.contains(&format!("This folder:  version {}", own_version())), "{same}");
        assert!(same.contains(&format!("Installed:    version {}", own_version())), "{same}");
        assert!(same.contains("identical to the ones in this folder. Nothing to do."), "{same}");

        fs::write(pc.mod_src().join("mod.json"), "{\"revision\":2}").unwrap();
        let changed = drive(&pc, "3\n\n0\n");
        assert!(changed.contains("The version number is the same, but the mod differ"), "{changed}");
        assert!(changed.contains("Choose 3 to compare"), "the screen offers it too: {changed}");

        let moved = {
            let mut moved = Pc::new();
            moved.exe = PathBuf::from("C:/Elsewhere/feverlink3.exe");
            moved
        };
        drive(&moved, "1\n\n0\n");
        assert_eq!(fs::read_to_string(moved.game().join(install::PATH_FILE)).unwrap(), "C:/Elsewhere/feverlink3.exe");
    }

    #[test]
    fn the_version_check_reads_the_installed_version_from_the_launcher_that_installed() {
        let pc = Pc::new();
        drive(&pc, "1\n\n0\n");
        // an earlier download put the files there; its launcher still exists and carries its marker
        let old = pc.root.path().join("old").join("feverlink3.exe");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, b"MZ feverlink3-launcher-version=0.0.5\0").unwrap();
        fs::write(pc.game().join(install::PATH_FILE), old.to_string_lossy().as_bytes()).unwrap();
        fs::write(pc.mod_src().join("mod.json"), "{\"revision\":2}").unwrap();

        let out = drive(&pc, "3\n\n0\n");
        assert!(out.contains("Installed:    version 0.0.5  (installed from"), "{out}");
        assert!(out.contains("This folder is NEWER than what is installed"), "{out}");
        assert!(out.contains("[ok]  FeverLink3         installed (version 0.0.5)"), "{out}");
    }

    #[test]
    fn an_older_folder_must_be_confirmed_before_it_replaces_newer_files() {
        let pc = Pc::new();
        drive(&pc, "1\n\n0\n");
        let newer = pc.root.path().join("newer").join("feverlink3.exe");
        fs::create_dir_all(newer.parent().unwrap()).unwrap();
        fs::write(&newer, b"MZ feverlink3-launcher-version=9.9.9\0").unwrap();
        fs::write(pc.game().join(install::PATH_FILE), newer.to_string_lossy().as_bytes()).unwrap();
        fs::write(pc.mod_src().join("mod.json"), "{\"old\":true}").unwrap();

        let refused = drive(&pc, "1\n\n\n0\n");
        assert!(refused.contains("Version 9.9.9 of FeverLink3 is installed"), "{refused}");
        assert!(refused.contains("Nothing was changed."), "{refused}");
        assert_eq!(fs::read_to_string(pc.installed_mod().join("mod.json")).unwrap(), "{}");

        let compare = drive(&pc, "3\n\n0\n");
        assert!(compare.contains("This folder is OLDER than what is installed"), "{compare}");

        let accepted = drive(&pc, "1\ny\n\n0\n");
        assert!(accepted.contains("FeverLink3 is installed."), "{accepted}");
        assert_eq!(fs::read_to_string(pc.installed_mod().join("mod.json")).unwrap(), "{\"old\":true}");
    }

    #[test]
    fn the_version_check_without_a_complete_folder_says_so() {
        let mut pc = Pc::new();
        pc.payload.1 = None;
        let out = drive(&pc, "3\n\n0\n");
        assert!(out.contains("winhttp.dll is missing next to feverlink3.exe"), "{out}");
        assert!(out.contains("Checking for newer releases") || out.contains("Looking for newer releases"), "{out}");
    }

    #[test]
    fn starting_the_game_goes_through_steam() {
        let pc = Pc::new();
        let out = drive(&pc, "4\n\n0\n");
        assert_eq!(pc.calls(), vec![Call::Steam(pc.steam.clone().unwrap())]);
        assert!(out.contains("Asked Steam to start Transport Fever 3"), "{out}");
        assert!(out.contains("not fully installed, so the game starts without multiplayer"), "{out}");

        drive(&pc, "1\n\n0\n");
        let installed = drive(&pc, "4\n\n0\n");
        assert_eq!(pc.calls().len(), 2);
        assert!(!installed.contains("not fully installed"), "{installed}");
    }

    #[test]
    fn starting_the_game_is_refused_when_it_cannot_work() {
        let pc = Pc::new();
        pc.running.set(true);
        let out = drive(&pc, "4\n\n0\n");
        assert!(out.contains("already running"), "{out}");
        assert!(pc.calls().is_empty());

        let mut no_steam = Pc::new();
        no_steam.steam = None;
        assert!(drive(&no_steam, "4\n\n0\n").contains("Steam was not found"));
        assert!(no_steam.calls().is_empty());

        let no_exe = Pc::new();
        fs::remove_file(no_exe.steam.clone().unwrap().join("steam.exe")).unwrap();
        assert!(drive(&no_exe, "4\n\n0\n").contains("Steam's program file was not found"));
        assert!(no_exe.calls().is_empty());
    }

    #[test]
    fn the_log_folder_opens_when_it_exists() {
        let pc = Pc::new();
        let missing = drive(&pc, "5\n\n0\n");
        assert!(missing.contains("There are no logs yet"), "{missing}");
        assert!(pc.calls().is_empty());

        fs::create_dir_all(pc.local()).unwrap();
        let out = drive(&pc, "5\n\n0\n");
        assert_eq!(pc.calls(), vec![Call::Open(pc.local())]);
        assert!(out.contains("Opened "), "{out}");

        let mut no_appdata = Pc::new();
        no_appdata.local = None;
        assert!(drive(&no_appdata, "5\n\n0\n").contains("LOCALAPPDATA"));
    }

    #[test]
    fn the_diagnostics_are_copied_with_the_user_name_replaced() {
        let pc = Pc::new();
        fs::create_dir_all(pc.local().join("live")).unwrap();
        let native: Vec<String> = (1..=60).map(|n| format!("native line {n} for Zara in C:\\Users\\Zara\\x")).collect();
        fs::write(pc.local().join("native.log"), native.join("\n") + "\n").unwrap();
        fs::write(pc.local().join("live").join("mod.log"), "0.00 [mod] started\n0.10 [mod] hello zara\n").unwrap();

        let out = drive(&pc, "6\n\n0\n");
        let text = clipboard_text(&pc);
        assert!(out.contains(&format!("Copied {} lines to the clipboard", text.lines().count())), "{out}");
        assert!(out.contains("Your Windows user name is shown as <user>"), "{out}");
        assert!(!text.to_lowercase().contains("zara"), "{text}");
        assert!(text.contains("native line 60 for <user> in C:\\Users\\<user>\\x"), "the last lines");
        assert!(!text.contains("native line 20 "), "only the last 40: {text}");
        assert!(text.contains("native line 21 "), "{text}");
        assert!(text.contains("0.10 [mod] hello <user>"));
        assert!(text.contains("== feverlink3 doctor =="));
        assert!(text.contains("2026-10-10 11:54:56 UTC"));
    }

    #[test]
    fn diagnostics_without_logs_still_work() {
        let pc = Pc::new();
        drive(&pc, "6\n\n0\n");
        let text = clipboard_text(&pc);
        assert_eq!(text.matches("(not there yet").count(), 2, "{text}");
        let mut no_appdata = Pc::new();
        no_appdata.local = None;
        drive(&no_appdata, "6\n\n0\n");
        assert_eq!(clipboard_text(&no_appdata).matches("(not there yet").count(), 2);
    }

    #[test]
    fn when_the_clipboard_fails_the_text_is_shown_instead() {
        let pc = Pc::new();
        pc.clipboard_fails.set(true);
        let out = drive(&pc, "6\n\n0\n");
        assert!(out.contains("could not be copied to the clipboard (the clipboard is busy)"), "{out}");
        assert!(out.contains("== feverlink3 doctor =="), "the text is on the screen: {out}");
        assert!(out.matches("----------------------------------------").count() == 2, "{out}");
        assert!(pc.calls().is_empty());
    }
}
