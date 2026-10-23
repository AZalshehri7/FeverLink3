//! The window's "Report a problem" (`report {note}`, `docs/LIVE_CONTROL.md`). In a session the request goes to the host, which
//! asks every game for its part (`core/src/session/report.rs`); outside one this game makes a report of its own logs alone.
//! The files, the redaction and the zips are `crate::report`; this is the controller's part: who builds what and when, on a
//! thread of its own (reading and zipping logs takes a second or two, the controller's step must not wait), and what the
//! window shows (`report` in the status).
//!
//! The player who asked gets the bundle on the Desktop (or `Documents\FeverLink3\reports`), its folder opens and the browser
//! opens a prefilled GitHub issue. Nothing is uploaded anywhere: the player attaches the zip to the issue themselves.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use feverlink3_core::session::{notices, CollectedReport, HostSession, ReportAskMessage, ReportLimits};
use serde_json::{json, Value};

use super::{Controller, State};
use crate::guard;
use crate::report::{self, redact::Redactor, BundleInfo, BundlePlayer, Sources};
use crate::session::{Command, ReportEvent, SessionHandle};

/// How long a player who asked waits for the host's bundle before it makes a report of its own logs instead: the host waits
/// up to three minutes for a part that is on its way, then sends a bundle of up to 24 MB.
const BUNDLE_WAIT: Duration = Duration::from_secs(300);

/// What a report thread hands back to the controller.
enum Done {
    /// This game's part of report transfer `id`, for the session.
    Part {
        id: i32,
        bytes: Vec<u8>,
    },
    /// Host: the bundle a player asked for, to send to it; `kept`: where the host's own copy went.
    Send {
        id: i32,
        to: u16,
        bytes: Vec<u8>,
        kept: Result<PathBuf, String>,
    },
    /// The copy of the player who asked is saved.
    Saved {
        report: String,
        path: PathBuf,
        contents: Vec<String>,
    },
    Failed(String),
}

/// What opens a folder or an address for the player (the tests give one that only records).
pub type Opener = Box<dyn Fn(&OpenTarget) + Send>;

/// A folder to show with the report selected, or an address for the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenTarget {
    Folder(PathBuf),
    Address(String),
}

/// The controller's report state.
pub(super) struct Reporter {
    tx: Sender<Done>,
    rx: Receiver<Done>,
    /// What the window shows: `making` (this game alone), `collecting` (the host gathers every game's logs), `done`, `failed`,
    /// `busy` (the host refused: another player's report runs; the button is back).
    view: Option<View>,
    /// This player asked the host and waits for the bundle since then.
    waiting: Option<(Instant, String)>,
    pub(super) opener: Opener,
    /// Where the player's copy goes instead of the Desktop (tests); the host keeps its copy of a player's report in the last.
    pub(super) dirs: Option<Vec<PathBuf>>,
}

struct View {
    state: &'static str,
    message: String,
    file: Option<String>,
    contents: Vec<String>,
}

impl Reporter {
    pub(super) fn new() -> Reporter {
        let (tx, rx) = mpsc::channel();
        Reporter { tx, rx, view: None, waiting: None, opener: Box::new(default_open), dirs: None }
    }

    /// The folders the player's copy may go to, the first that takes it.
    fn save_dirs(&self) -> Vec<PathBuf> {
        self.dirs.clone().unwrap_or_else(report::report_dirs)
    }

    /// The folder the host keeps its copy of a player's report in.
    fn kept_dir(&self) -> Option<PathBuf> {
        match &self.dirs {
            Some(d) => d.last().cloned(),
            None => report::kept_dir(),
        }
    }

    fn busy(&self) -> bool {
        self.view.as_ref().is_some_and(|v| v.state == "making" || v.state == "collecting")
    }

    fn show(&mut self, state: &'static str, message: impl Into<String>) {
        self.view = Some(View { state, message: message.into(), file: None, contents: Vec::new() });
    }

    /// The `report` field of the status, when there is something to show.
    pub(super) fn json(&self) -> Option<Value> {
        self.view
            .as_ref()
            .map(|v| json!({ "state": v.state, "message": v.message, "file": v.file, "contents": v.contents }))
    }

    /// Runs `job` on a thread of its own; its result (or its panic, as a failure) comes back through the channel.
    fn spawn(&self, job: impl FnOnce() -> Done + Send + 'static) {
        let tx = self.tx.clone();
        let spawned = thread::Builder::new().name("feverlink3-report".into()).spawn(move || {
            let done = guard::guarded(job).unwrap_or_else(|why| Done::Failed(format!("internal error: {why}")));
            let _ = tx.send(done);
        });
        if let Err(e) = spawned {
            let _ = self.tx.send(Done::Failed(format!("no thread for the report: {e}")));
        }
    }
}

/// Opens a folder (the report selected) or an address with what Windows has for it; nothing in the tests.
///
/// Through the shell's own functions, never by starting a program from here: a child process of the game inherits the
/// game's open files (`crate::https`, the crash of 2026-10-22). The shell hands the folder to Explorer and the address
/// to the browser itself. On a thread of its own, which sets up COM as the shell functions need.
fn default_open(target: &OpenTarget) {
    if cfg!(test) {
        return;
    }
    let target = target.clone();
    let _ = thread::Builder::new().name("feverlink3-open".into()).spawn(move || {
        let _ = guard::guarded(|| shell::open(&target));
    });
}

#[cfg(windows)]
mod shell {
    use super::OpenTarget;
    use std::ffi::c_void;

    // shell32 and ole32 are loaded here, by full path, when a report is opened: imported by the module they would load
    // with the game, and a system DLL loaded with the game that imports `winhttp.dll` would be handed our proxy
    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    type CoInitializeEx = unsafe extern "system" fn(*const c_void, u32) -> i32;
    type CoUninitialize = unsafe extern "system" fn();
    type IlCreateFromPathW = unsafe extern "system" fn(*const u16) -> *mut c_void;
    type IlFree = unsafe extern "system" fn(*mut c_void);
    type ShOpenFolderAndSelectItems = unsafe extern "system" fn(*const c_void, u32, *const *const c_void, u32) -> i32;
    type ShellExecuteW =
        unsafe extern "system" fn(*mut c_void, *const u16, *const u16, *const u16, *const u16, i32) -> *mut c_void;

    const COINIT_APARTMENTTHREADED: u32 = 0x2;
    const COINIT_DISABLE_OLE1DDE: u32 = 0x4;
    const SW_SHOWNORMAL: i32 = 1;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `<system folder>\<dll>`, loaded (once per process; Windows counts the loads) and never freed.
    fn system_dll(dll: &str) -> *mut c_void {
        let mut dir = [0u16; 300];
        // SAFETY: the buffer holds 300 wide characters; at most 260 are asked for, the length is returned.
        let n = unsafe { GetSystemDirectoryW(dir.as_mut_ptr(), 260) } as usize;
        if n == 0 || n >= 260 {
            return std::ptr::null_mut();
        }
        let mut path: Vec<u16> = dir[..n].to_vec();
        path.extend(format!("\\{dll}\0").encode_utf16());
        // SAFETY: a NUL-terminated full path.
        unsafe { LoadLibraryW(path.as_ptr()) }
    }

    /// The function `name` (NUL-terminated) of `module` as `F`, the type the Windows documentation gives it.
    ///
    /// # Safety
    /// `F` must be that function's pointer type.
    unsafe fn proc<F: Copy>(module: *mut c_void, name: &str) -> Option<F> {
        if module.is_null() {
            return None;
        }
        // SAFETY: a loaded module and a NUL-terminated name; the caller names the right type.
        let p = unsafe { GetProcAddress(module, name.as_ptr()) };
        (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) })
    }

    pub fn open(target: &OpenTarget) {
        let (ole, shell) = (system_dll("ole32.dll"), system_dll("shell32.dll"));
        // SAFETY: each type is the documented signature of the function of that name
        let (init, uninit, create, free, select, execute) = unsafe {
            (
                proc::<CoInitializeEx>(ole, "CoInitializeEx\0"),
                proc::<CoUninitialize>(ole, "CoUninitialize\0"),
                proc::<IlCreateFromPathW>(shell, "ILCreateFromPathW\0"),
                proc::<IlFree>(shell, "ILFree\0"),
                proc::<ShOpenFolderAndSelectItems>(shell, "SHOpenFolderAndSelectItems\0"),
                proc::<ShellExecuteW>(shell, "ShellExecuteW\0"),
            )
        };
        let (Some(init), Some(uninit), Some(create), Some(free), Some(select), Some(execute)) =
            (init, uninit, create, free, select, execute)
        else {
            return;
        };
        // SAFETY: plain shell calls on this thread's own COM apartment; every string is NUL-terminated and lives across
        // its call, the item list is freed once.
        unsafe {
            let com = init(std::ptr::null(), COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
            match target {
                OpenTarget::Folder(file) => {
                    // the file's own item with no children: Explorer opens its folder with the file selected
                    let path = wide(&file.to_string_lossy());
                    let item = create(path.as_ptr());
                    if !item.is_null() {
                        select(item, 0, std::ptr::null(), 0);
                        free(item);
                    }
                }
                // only web addresses (the issue page): never a file or a program by this path
                OpenTarget::Address(url) if url.starts_with("https://") => {
                    let (verb, url) = (wide("open"), wide(url));
                    let none = std::ptr::null();
                    execute(std::ptr::null_mut(), verb.as_ptr(), url.as_ptr(), none, none, SW_SHOWNORMAL);
                }
                OpenTarget::Address(_) => {}
            }
            if com >= 0 {
                uninit();
            }
        }
    }
}

#[cfg(not(windows))]
mod shell {
    pub fn open(_target: &super::OpenTarget) {}
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Six random hex digits, the name of a report made without a host.
fn local_report_id() -> String {
    format!("{:06x}", rand::random::<u32>() & 0xff_ffff)
}

/// Everything a part needs from the controller, taken on its thread so the report thread reads only files.
struct PartJob {
    sources: Sources,
    info: Vec<String>,
    redactor: Redactor,
    own_steam_id: Option<u64>,
}

impl PartJob {
    fn build(self, max_bytes: u64) -> std::io::Result<(Vec<u8>, Vec<String>)> {
        let (files, notes) = report::collect(&self.sources, SystemTime::now());
        report::build_part(files, notes, &self.info, &self.redactor, self.own_steam_id, max_bytes)
    }
}

impl Controller {
    /// The window's `report {note}`.
    pub(super) fn report_command(&mut self, params: &Value) {
        self.remember_mods(params);
        let note: String = params
            .get("note")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .chars()
            .take(ReportLimits::NOTE_CHARS)
            .collect();
        if self.reporter.busy() {
            return;
        }
        match &self.state {
            State::Hosting(h) => {
                h.handle.command(Command::Report(note));
                self.reporter.show("collecting", "Collecting the logs of every player (up to a minute)...");
            }
            State::Joined { handle } if handle.status().connected => {
                handle.command(Command::Report(note.clone()));
                self.reporter.waiting = Some((Instant::now(), note));
                self.reporter.show("collecting", "The host collects the logs of every player (up to a minute)...");
            }
            _ => self.local_report(&note, None),
        }
    }

    /// A report of this game's logs alone: outside a session, or when the host's bundle did not come (`why`).
    fn local_report(&mut self, note: &str, why: Option<&str>) {
        let report = local_report_id();
        let job = self.part_job(&report, &format!("{report}-salt-{}", rand::random::<u64>()));
        let name = self.default_name.clone();
        let note = note.to_owned();
        let missing = why.map(str::to_owned);
        let dirs = self.reporter.save_dirs();
        self.reporter.show("making", "Making a bug report of your game's logs...");
        self.append_session_log(&format!("report {report}: a report of this game's logs alone"));
        self.reporter.spawn(move || {
            let max = ReportLimits::part_budget(1);
            let (part, _) = match job.build(max) {
                Ok(p) => p,
                Err(e) => return Done::Failed(format!("the logs could not be packed: {e}")),
            };
            let mut players =
                vec![BundlePlayer { slot: 1, name: name.clone(), host: false, part: Some(part), missing: None }];
            if let Some(why) = missing {
                players.push(BundlePlayer {
                    slot: 2,
                    name: "the other players".into(),
                    host: false,
                    part: None,
                    missing: Some(why),
                });
            }
            let info = BundleInfo { report: report.clone(), created_unix: now_unix(), asked_by: name, note, players };
            save_for_player(&info, &dirs)
        });
    }

    /// What a part of this game needs, read now.
    fn part_job(&self, report: &str, salt: &str) -> PartJob {
        let user_dirs = self.user_dirs();
        let own_steam_id = self.steam.as_ref().and_then(|p| p.report().steam_id);
        PartJob {
            sources: Sources { live_dir: self.dir.clone(), user_dirs: user_dirs.clone() },
            info: self.report_info(report),
            redactor: Redactor::new(report::user_names(), salt, report::accounts_of(&user_dirs)),
            own_steam_id,
        }
    }

    /// Each step: the session's report events, the threads' results, and a bundle that does not come.
    pub(super) fn report_step(&mut self) {
        let events = match &self.state {
            State::Hosting(h) => h.handle.take_report_events(),
            State::Joined { handle } => handle.take_report_events(),
            _ => Vec::new(),
        };
        for event in events {
            match event {
                ReportEvent::BuildPart(ask) => self.build_part_for(ask),
                ReportEvent::Collected(collected) => self.bundle_collected(collected),
                ReportEvent::Busy { id, by_name } => {
                    // the host answered at once: no bundle comes for this request, so no fallback either
                    self.reporter.waiting = None;
                    self.append_session_log(&format!(
                        "report: refused by the host, report {id} (asked by {by_name}) is still being made"
                    ));
                    self.reporter.show("busy", notices::report_busy(&by_name));
                }
                ReportEvent::Bundle { id, report, bytes } => {
                    self.reporter.waiting = None;
                    self.append_session_log(&format!("report {id}: the bundle arrived ({} bytes)", bytes.len()));
                    let report = if report.is_empty() { local_report_id() } else { report };
                    let dirs = self.reporter.save_dirs();
                    self.reporter.spawn(move || {
                        let name = report::bundle_file_name(now_unix(), &report);
                        match report::save_bundle(&dirs, &name, &bytes) {
                            Ok(path) => Done::Saved { report, path, contents: report::zip_contents(&bytes) },
                            Err(e) => Done::Failed(format!("the report could not be saved: {e}")),
                        }
                    });
                }
            }
        }
        while let Ok(done) = self.reporter.rx.try_recv() {
            self.report_done(done);
        }
        let in_session = matches!(self.state, State::Hosting(_) | State::Joined { .. });
        let collecting = self.reporter.view.as_ref().is_some_and(|v| v.state == "collecting");
        if collecting && !in_session && self.reporter.waiting.is_none() {
            // the host's session ended before every part was in
            self.reporter
                .show("failed", "The session ended before the bug report was finished. Press the button again.");
        }
        // the host never sent the bundle (it left, or runs an older FeverLink3): this game's logs alone, said so
        let late =
            self.reporter.waiting.as_ref().is_some_and(|(since, _)| since.elapsed() >= BUNDLE_WAIT || !in_session);
        if late {
            let note = self.reporter.waiting.take().map(|(_, n)| n).unwrap_or_default();
            self.append_session_log(
                "report: the host's bundle did not come; making a report of this game's logs alone",
            );
            self.local_report(&note, Some("the host did not send the bundle; only this game's logs are in it"));
        }
    }

    /// The host asks this game for its part: built on a thread, handed to the session.
    fn build_part_for(&mut self, ask: ReportAskMessage) {
        let job = self.part_job(&ask.report, &ask.salt);
        let (id, max) = (ask.id, ask.max_bytes);
        self.append_session_log(&format!("report {id}: {} made a bug report; packing this game's logs", ask.by_name));
        self.reporter.spawn(move || match job.build(max) {
            Ok((bytes, _)) => Done::Part { id, bytes },
            Err(e) => Done::Failed(format!("this game's logs could not be packed: {e}")),
        });
    }

    /// Host: every part is in or missing. The bundle goes to the host's own Desktop when the host asked, else to the player who
    /// asked (and a copy stays in the host's `Documents\FeverLink3\reports`).
    fn bundle_collected(&mut self, collected: CollectedReport) {
        let ask = collected.ask.clone();
        let players: Vec<BundlePlayer> = collected
            .parts
            .into_iter()
            .enumerate()
            .map(|(i, p)| BundlePlayer {
                slot: i + 1,
                name: p.name,
                host: p.player == HostSession::HOST_ID,
                part: p.bytes,
                missing: p.missing,
            })
            .collect();
        let info = BundleInfo {
            report: ask.report.clone(),
            created_unix: now_unix(),
            asked_by: ask.by_name.clone(),
            note: collected.note,
            players,
        };
        if ask.by == HostSession::HOST_ID {
            let dirs = self.reporter.save_dirs();
            self.reporter.spawn(move || save_for_player(&info, &dirs));
            return;
        }
        let kept_dir = self.reporter.kept_dir();
        self.reporter.spawn(move || {
            let bytes = match report::make_bundle(&info) {
                Ok((bytes, _)) => bytes,
                Err(e) => return Done::Failed(format!("the bundle could not be made: {e}")),
            };
            let name = report::bundle_file_name(info.created_unix, &info.report);
            let kept = kept_dir
                .ok_or_else(|| "no Documents folder".to_owned())
                .and_then(|dir| report::save_bundle(&[dir], &name, &bytes).map_err(|e| e.to_string()));
            Done::Send { id: ask.id, to: ask.by, bytes, kept }
        });
    }

    fn report_done(&mut self, done: Done) {
        match done {
            Done::Part { id, bytes } => {
                self.append_session_log(&format!("report {id}: this game's part is packed ({} bytes)", bytes.len()));
                match &self.state {
                    State::Hosting(h) => h.handle.command(Command::ReportPart { id, bytes }),
                    State::Joined { handle } => handle.command(Command::ReportPart { id, bytes }),
                    _ => {}
                }
            }
            Done::Send { id, to, bytes, kept } => {
                let kept = match kept {
                    Ok(path) => format!("a copy is in {}", path.display()),
                    Err(why) => format!("no copy kept here ({why})"),
                };
                self.append_session_log(&format!("report {id}: bundle made ({} bytes); {kept}", bytes.len()));
                if let State::Hosting(h) = &self.state {
                    h.handle.command(Command::ReportSend { id, to, bytes });
                }
            }
            Done::Saved { report, path, contents } => {
                self.append_session_log(&format!("report {report}: saved as {}", path.display()));
                (self.reporter.opener)(&OpenTarget::Folder(path.clone()));
                (self.reporter.opener)(&OpenTarget::Address(report::issue_url(&report, env!("CARGO_PKG_VERSION"))));
                let file = path.file_name().map(|n| n.to_string_lossy().into_owned());
                let folder = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                self.reporter.view = Some(View {
                    state: "done",
                    message: format!(
                        "Report {report} saved in {folder}. Drag the zip into the GitHub page that opened (nothing was sent anywhere)."
                    ),
                    file,
                    contents,
                });
            }
            Done::Failed(why) => {
                self.append_session_log(&format!("report: {why}"));
                self.reporter.waiting = None;
                self.reporter.show("failed", format!("The bug report could not be made: {why}"));
            }
        }
    }

    /// The part's `report.txt` lines about this game: versions, mods, the session as this game sees it.
    pub(super) fn report_info(&self, report: &str) -> Vec<String> {
        let mut lines = vec![
            "FeverLink3 bug report: this game's part".to_owned(),
            format!("report id: {report}"),
            format!("made: {} UTC", report::stamp_text(now_unix(), true)),
            format!("FeverLink3: {} (the controller and native module of this game)", env!("CARGO_PKG_VERSION")),
            format!("game build: {}", self.mod_params.get("game").and_then(Value::as_str).unwrap_or("unknown")),
            format!("newest save file: {}", newest_save(&self.save_dirs)),
        ];
        let (role, handle, transport) = match &self.state {
            State::Hosting(h) => {
                ("host", Some(&h.handle), if h.steam_id.is_some() { "Steam" } else { "TCP (developer)" })
            }
            State::Joined { handle } => ("player", Some(handle), ""),
            State::Joining { .. } => ("joining", None, ""),
            State::Failed(_) => ("not in a session (the last one failed)", None, ""),
            State::Idle => ("not in a session", None, ""),
        };
        lines.push(format!("role: {role}"));
        if let Some(handle) = handle {
            lines.extend(session_lines(handle, transport));
        }
        match self.mod_params.get("mods").and_then(Value::as_array) {
            Some(mods) => {
                lines.push(format!("mods ({}):", mods.len()));
                for m in mods {
                    let id = m.get("id").and_then(Value::as_str).unwrap_or("?");
                    let rev = m.get("revision").and_then(Value::as_i64).unwrap_or(0);
                    let cosmetic =
                        if m.get("cosmetic").and_then(Value::as_bool) == Some(true) { " (cosmetic)" } else { "" };
                    lines.push(format!("  {id} revision {rev}{cosmetic}"));
                }
            }
            None => lines.push("mods: not known (the window sent no list)".to_owned()),
        }
        lines
    }
}

/// The session as this game sees it: players, transport, ping, the last sync and its cause, the last mismatch.
fn session_lines(handle: &SessionHandle, transport: &str) -> Vec<String> {
    let s = handle.status();
    let mut lines = vec![format!("my player id: {}", s.my_id.map_or("none yet".into(), |i| i.to_string()))];
    lines.push(format!("players: {}", s.roster.len()));
    for e in &s.roster {
        let ping = e.rtt_ms.map_or("no ping yet".into(), |ms| format!("{ms:.0} ms"));
        lines.push(format!("  #{} {} ({:?}, {ping})", e.id, e.name, e.state));
    }
    let route = s.links.iter().filter_map(|l| l.route.clone()).collect::<Vec<_>>().join(", ");
    let transport = match (transport, route.is_empty()) {
        ("", true) => "unknown".to_owned(),
        ("", false) => format!("Steam ({route})"),
        (t, true) => t.to_owned(),
        (t, false) => format!("{t} ({route})"),
    };
    lines.push(format!("transport: {transport}"));
    let pings: Vec<String> = s.links.iter().filter_map(|l| l.ping_ms.map(|p| format!("{p} ms"))).collect();
    lines.push(format!("ping (transport): {}", if pings.is_empty() { "-".to_owned() } else { pings.join(", ") }));
    lines.push(match &s.last_sync {
        Some(r) if !r.compared => format!("last checksum: round {} at t={} not compared", r.n, r.time),
        Some(r) if r.differing.is_empty() => format!("last checksum: round {} at t={} identical", r.n, r.time),
        Some(r) => format!("last checksum: round {} at t={} differs in {}", r.n, r.time, r.differing.join(", ")),
        None => "last checksum: none seen here".to_owned(),
    });
    lines.push(format!("sync (resync) state: {}", s.resync.as_deref().unwrap_or("none this session")));
    let logs = handle.logs();
    let last = |needle: &str| logs.iter().rev().find(|l| l.contains(needle)).cloned();
    lines.push(format!(
        "last sync cause: {}",
        last("resync requested:").unwrap_or_else(|| "none in the last 500 lines".into())
    ));
    lines.push(format!(
        "last checksum mismatch: {}",
        last(" differs in ").unwrap_or_else(|| "none in the last 500 lines".into())
    ));
    lines
}

/// The newest `.sav` in the save folders and when it was written: the world most likely loaded (the game does not tell the
/// controller its save's name).
fn newest_save(dirs: &[PathBuf]) -> String {
    let newest = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "sav"))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max_by_key(|(t, _)| *t);
    match newest {
        Some((t, p)) => {
            let unix = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let name = p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            format!("{name} (written {} UTC)", report::stamp_text(unix, true))
        }
        None => "none found".to_owned(),
    }
}

/// The bundle made and saved where the player who asked finds it.
fn save_for_player(info: &BundleInfo, dirs: &[PathBuf]) -> Done {
    let (bytes, contents) = match report::make_bundle(info) {
        Ok(b) => b,
        Err(e) => return Done::Failed(format!("the bundle could not be made: {e}")),
    };
    let name = report::bundle_file_name(info.created_unix, &info.report);
    match report::save_bundle(dirs, &name, &bytes) {
        Ok(path) => {
            let mut all = vec![report::REPORT_FILE.to_owned()];
            all.extend(contents);
            Done::Saved { report: info.report.clone(), path, contents: all }
        }
        Err(e) => Done::Failed(format!("the report could not be saved: {e}")),
    }
}

/// For the tests: the report's opener.
#[cfg(test)]
pub(super) fn recording_opener() -> (Opener, std::sync::Arc<std::sync::Mutex<Vec<OpenTarget>>>) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = std::sync::Arc::clone(&seen);
    (Box::new(move |t: &OpenTarget| log.lock().unwrap().push(t.clone())), seen)
}
