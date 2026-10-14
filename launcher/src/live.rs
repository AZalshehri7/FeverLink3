//! Live mode: the session controller that runs inside the game process, driven from the in-game window.
//!
//! The native module (`winhttp.dll`) starts [`spawn`] on a thread when the game was not started by the launcher
//! executable. Everything the launcher binary does for `host` and `join` happens here instead, on request of the
//! mod's window, so a player starts the game from Steam as usual and never sees a terminal:
//!
//! - the folder is fixed, `%LOCALAPPDATA%\FeverLink3\live`, so the mod finds it without an environment variable;
//! - the window appends commands to `ctl_cmd.log` (`host`, `join`, `leave`, `start`, `pause`, `speed`, `check`,
//!   `resync`, `kick`, `permissions`; the payload is a Lua literal table, the line format is the file link's);
//! - the controller answers by rewriting `ctl_status.txt` (one Lua literal table) whenever something changed;
//!   `docs/LIVE_CONTROL.md` lists every command and every field;
//! - the session itself is the same [`run_host`] / [`run_player`] the launcher uses, linked to the game through the
//!   same `to_game.log` / `from_game.log`, and `identity.txt` says who the game is (role `idle` until a session
//!   exists, then `host` or `player` with the id the host assigned).
//!
//! A session ends when the local game's world is closed to the main menu: the mod writes to `from_game.log` while its
//! script runs, so a game that has spoken and then falls silent for [`HEARTBEAT_SECONDS`] (and is not in the middle of
//! a resync reload) has no world any more. A host then stops hosting and tells its players, a player leaves, and the
//! status carries a `notice` for the window.
//!
//! Everything is a function of the folder's files, so the tests drive it with files and need no game.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::lobby::JoinCode;
use feverlink3_core::session::{ConnectionState, HostOptions, Permissions, PlayerSet, RosterEntry};
use serde_json::{json, Value};

use crate::gamelink::{self, Tail};
use crate::guard;
use crate::session::{self, Command, HostConfig, PlayerConfig, SessionHandle, Status};
use crate::steam_transport::SteamEndpoint;
use crate::steamnet::CoreRef;
use crate::{clipboard, luatext, net, resync, steam, steam_persona, steam_transport, steamnet};

/// The window's commands, appended by the mod.
pub const CMD_FILE: &str = "ctl_cmd.log";
/// The controller's state, rewritten by the controller.
pub const STATUS_FILE: &str = "ctl_status.txt";
/// The session's log lines, written by the controller (see `Controller::write_session_log`).
pub const SESSION_LOG_FILE: &str = "session.log";
const SESSION_LOG_MAX: u64 = 2 << 20;
/// The Steam API is looked up again every couple of seconds (for about five minutes) when it was not there yet.
const STEAM_LOAD_EVERY: Duration = Duration::from_secs(2);
const STEAM_LOAD_ATTEMPTS: u32 = 150;

/// The port a host listens on unless the window asks for another.
pub const DEFAULT_PORT: u16 = 28090;

/// Seconds without a line from the game, after the game had spoken, before its world counts as closed and the session
/// ends. The mod appends a `clock` line to `from_game.log` at least every 30 frames (about half a second) while its
/// script runs, paused or not. But the game's pause menu (Escape) stops every game script while the world stays loaded
/// (real runs inj7 and s3, 2026-10-14: with 8 s the player who opened the menu "left" the session and its builds went
/// the old way afterwards), so silence alone says little about a closed world: the limit is two minutes, longer than a
/// visit to the menu. `FEVERLINK3_HEARTBEAT_SECONDS` changes it for tests; `0` turns the check off.
pub const HEARTBEAT_SECONDS: f64 = 120.0;
/// The environment variable that changes [`HEARTBEAT_SECONDS`].
pub const HEARTBEAT_ENV: &str = "FEVERLINK3_HEARTBEAT_SECONDS";
/// How long a world reload (the game was told to load the host's save, or the host's resync round runs) may silence the
/// mod before the silence counts again: a round that never ends must not keep a closed world alive for ever.
const RELOAD_GRACE: Duration = Duration::from_secs(300);
/// What the window shows when the heartbeat ended the session.
pub const WORLD_CLOSED_NOTICE: &str = "Your world was closed, so the session ended";

/// The heartbeat limit for an environment value: unset or not a number is [`HEARTBEAT_SECONDS`], zero or less is off
/// (`None`).
fn heartbeat_setting(value: Option<&str>) -> Option<Duration> {
    match value.and_then(|v| v.trim().parse::<f64>().ok()).filter(|s| !s.is_nan()) {
        None => Some(Duration::from_secs_f64(HEARTBEAT_SECONDS)),
        Some(s) if s <= 0.0 => None,
        Some(s) => Some(Duration::from_secs_f64(s.min(3600.0))),
    }
}

/// The folder the mod looks in when the launcher did not hand it a session folder.
///
/// `FEVERLINK3_LIVE_DIR` names another folder: a second game on the same PC (for tests and recordings) must not
/// share the first one's files.
pub fn live_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FEVERLINK3_LIVE_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("FeverLink3").join("live"))
}

enum State {
    Idle,
    Hosting(Box<Hosting>),
    /// `progress` is what the connection attempt says it is doing ("finding a route"), written by the join thread.
    Joining {
        rx: Receiver<Result<SessionHandle, String>>,
        progress: Arc<Mutex<String>>,
    },
    Joined {
        handle: SessionHandle,
    },
    Failed(String),
}

struct Hosting {
    handle: SessionHandle,
    /// What the window last set; each `permissions` command changes only the keys it names.
    permissions: Permissions,
    secret: [u8; SessionKey::SECRET_SIZE],
    lan: Vec<SocketAddr>,
    code: String,
    endpoints: Vec<String>,
    password: bool,
    /// The Steam ID the code carries: the session is hosted over Steam (no address, no port). `None` for TCP.
    steam_id: Option<u64>,
}

impl Hosting {
    /// The join code for the direct (developer) transport: the machine's local addresses and the secret.
    fn make_code(&mut self) {
        if let Ok(code) = JoinCode::create(&self.lan, self.secret, None) {
            self.code = code.encode();
            self.endpoints = self.lan.iter().map(ToString::to_string).collect();
        }
    }
}

/// The Steam transport self-check (`steamloop`): it runs on a thread of its own and leaves its lines here.
#[derive(Default)]
struct LoopCheck {
    running: bool,
    checks: Vec<steam_transport::Check>,
}

/// How the last command that can fail ended; the window shows `message` and tells its own command by `seq`.
#[derive(Debug, Clone)]
struct CommandResult {
    seq: u64,
    kind: String,
    ok: bool,
    message: String,
}

/// The pulse of the local game in a running session: whether `from_game.log` still grows.
///
/// The mod writes to that file while its script runs (it says hello once, then a `clock` line every half second at
/// least) and stops when the world is closed to the main menu. The session thread reads the file's lines; this only
/// looks at whether it changed, which is all a heartbeat needs.
struct Pulse {
    path: PathBuf,
    /// Length and modification time of the file at the last look. Any change is taken as a new line.
    mark: Option<(u64, Option<SystemTime>)>,
    /// A line has come since the session started. Until then the world may still be loading and nothing is judged.
    seen: bool,
    /// When the file last changed, or when the game was last excused for a reload.
    last: Instant,
    /// Since when the game has been excused for a reload (the silence of a resync load).
    excused_since: Option<Instant>,
}

impl Pulse {
    /// Starts watching now: what the file holds already (lines of an earlier session, the first of this one racing the
    /// controller) is the baseline, not a sign of life.
    fn start(path: PathBuf, now: Instant) -> Pulse {
        let mark = Pulse::look(&path);
        Pulse { path, mark, seen: false, last: now, excused_since: None }
    }

    fn look(path: &Path) -> Option<(u64, Option<SystemTime>)> {
        fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok()))
    }

    /// Looks at the file at `now`. True when the game has spoken in this session and then been silent for `limit`, with
    /// no reload to excuse the silence. `reloading` is true while the world is being replaced by a resync round
    /// (see [`game_is_reloading`]): the mod is silent then, and the silence starts counting again when the round ends.
    fn silent(&mut self, limit: Duration, reloading: bool, now: Instant) -> bool {
        let mark = Pulse::look(&self.path);
        if mark != self.mark {
            self.mark = mark;
            // a file that went missing is not a line; one that came back or grew is
            if mark.is_some() {
                self.seen = true;
                self.last = now;
            }
        }
        if reloading {
            let since = *self.excused_since.get_or_insert(now);
            if now.saturating_duration_since(since) < RELOAD_GRACE {
                self.last = now;
            }
        } else if self.excused_since.take().is_some() {
            self.last = now;
        }
        self.seen && now.saturating_duration_since(self.last) >= limit
    }
}

/// The mod's presence lock (`world.lock` in the session folder): the mod's interface script opens it once and never closes it, so the
/// handle lives exactly as long as the script's Lua state, which the game destroys with the world. The pause menu only stops the
/// script (the handle stays open); closing the world to the main menu frees it. A file that can be opened without sharing is held by
/// nobody: a positive sign that the world is closed, seconds after the fact instead of the heartbeat's two minutes.
pub const WORLD_LOCK_FILE: &str = "world.lock";

/// Watches [`WORLD_LOCK_FILE`]: true once the lock was seen held in this session and is free now.
struct PresenceLock {
    path: PathBuf,
    held_seen: bool,
    next_look: Instant,
}

impl PresenceLock {
    const EVERY: Duration = Duration::from_millis(500);

    fn new(path: PathBuf, now: Instant) -> PresenceLock {
        PresenceLock { path, held_seen: false, next_look: now }
    }

    /// `reloading`: a resync load replaces the script's state, so the lock is free for a moment; it must be seen held again first.
    fn world_gone(&mut self, reloading: bool, now: Instant) -> bool {
        if reloading {
            self.held_seen = false;
            return false;
        }
        if now < self.next_look {
            return false;
        }
        self.next_look = now + Self::EVERY;
        match lock_is_held(&self.path) {
            Some(true) => {
                self.held_seen = true;
                false
            }
            Some(false) => self.held_seen,
            None => false,
        }
    }
}

/// `Some(true)` when another handle holds the file (a sharing violation), `Some(false)` when it opens without sharing, `None` when it
/// does not exist or cannot be judged.
#[cfg(windows)]
fn lock_is_held(path: &Path) -> Option<bool> {
    use std::os::windows::fs::OpenOptionsExt;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    match fs::OpenOptions::new().read(true).share_mode(0).open(path) {
        Ok(_) => Some(false),
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Some(true),
        Err(_) => None,
    }
}

#[cfg(not(windows))]
fn lock_is_held(_path: &Path) -> Option<bool> {
    None
}

/// The local game's world is being replaced, or is about to be: it told to load a save and has not said hello since, or
/// (host) a resync round is running, in which it saves, may stall for a while on a big map and, with
/// `HostOptions::host_reloads`, loads too.
fn game_is_reloading(s: &Status) -> bool {
    s.reloading || s.resync.as_deref().is_some_and(|r| r != "Idle" && !r.starts_with("failed"))
}

/// The controller. [`Controller::step`] is called every ~100 ms by [`spawn`], or by a test.
pub struct Controller {
    dir: PathBuf,
    default_name: String,
    save_dirs: Vec<PathBuf>,
    host_options: HostOptions,
    commands: Tail,
    state: State,
    steam: Option<steamnet::Probe>,
    /// The Steam core to use instead of the process's (the tests give it a fake Steam).
    core: Option<CoreRef>,
    /// How often and when the Steam API was looked for: it may not be loaded yet when the controller starts.
    steam_attempts: u32,
    steam_last_try: Instant,
    /// The Steam display name replaces the neutral default as soon as Steam can say it (checked about once a second).
    name_from_steam: bool,
    persona_attempts: u32,
    persona_last: Instant,
    steam_note: Option<String>,
    steam_loop: Arc<Mutex<LoopCheck>>,
    /// Puts text on the clipboard (the window's "Copy code"); the tests replace it.
    clipboard: Box<dyn Fn(&str) -> bool + Send>,
    result: Option<CommandResult>,
    /// How many of the session's log lines were already written to `session.log`.
    log_cursor: u64,
    /// One line on how the last session ended by itself (the world was closed, the host ended it, ...), for the window's
    /// start screen. Cleared by the next `host`, `join` or `leave`.
    notice: Option<String>,
    /// How long the local game may be silent before its world counts as closed; `None` turns the check off.
    heartbeat: Option<Duration>,
    /// What the heartbeat reads as "now"; tests replace it with a clock they advance themselves.
    clock: Box<dyn Fn() -> Instant + Send>,
    /// Watching the game of the running session; `None` outside one.
    pulse: Option<Pulse>,
    /// The mod's presence lock, watched while a session runs (see [`PresenceLock`]).
    presence: Option<PresenceLock>,
    published: String,
    version: u64,
}

impl Controller {
    /// Prepares the folder (files of an earlier run are removed), says the game is idle, starts listening for
    /// commands.
    pub fn new(dir: PathBuf, default_name: String, save_dirs: Vec<PathBuf>) -> io::Result<Controller> {
        Self::build(dir, default_name, save_dirs, None)
    }

    /// As [`Controller::new`] with a Steam core of its own instead of the process's: for tests with a fake Steam.
    pub fn with_core(
        dir: PathBuf,
        default_name: String,
        save_dirs: Vec<PathBuf>,
        core: CoreRef,
    ) -> io::Result<Controller> {
        Self::build(dir, default_name, save_dirs, Some(core))
    }

    fn build(
        dir: PathBuf,
        default_name: String,
        save_dirs: Vec<PathBuf>,
        core: Option<CoreRef>,
    ) -> io::Result<Controller> {
        fs::create_dir_all(&dir)?;
        for entry in fs::read_dir(&dir)?.flatten() {
            if entry.path().is_file() {
                let _ = fs::remove_file(entry.path());
            }
        }
        let link = gamelink::Session::open(&dir)?;
        link.write_identity(0, &default_name, "idle")?;
        let commands = Tail::new(dir.join(CMD_FILE));
        let mut c = Controller {
            dir,
            default_name,
            save_dirs,
            // Every game, the host's included, loads the saved world in a resync round: a live world and a freshly
            // loaded one of the same save drift apart (lab runs E1 vs E2/E3), two fresh loads do not. The host loads
            // while the players still download. FEVERLINK3_HOST_RELOAD=0 keeps the host's live world (it drifts).
            host_options: HostOptions {
                host_reloads: !std::env::var("FEVERLINK3_HOST_RELOAD").is_ok_and(|v| v.trim() == "0"),
                host_loads_early: !std::env::var("FEVERLINK3_HOST_LOAD_EARLY").is_ok_and(|v| v.trim() == "0"),
                // FEVERLINK3_FULL_CHECKSUM_EVERY=<n> (default 6, 1 = every round is a full check)
                full_checksum_every: std::env::var("FEVERLINK3_FULL_CHECKSUM_EVERY")
                    .ok()
                    .and_then(|v| v.trim().parse::<u32>().ok())
                    .filter(|v| *v >= 1)
                    .unwrap_or(6),
                // FEVERLINK3_AUTO_RESYNC=0 (an experiment switch): differences are logged and the policy still says "a
                // resync is requested", but none is started by itself; only the window's Sync button starts one.
                auto_resync: !std::env::var("FEVERLINK3_AUTO_RESYNC").is_ok_and(|v| v.trim() == "0"),
                ..HostOptions::default()
            },
            commands,
            state: State::Idle,
            steam: core.clone().map(steamnet::Probe::on).or_else(steamnet::Probe::load),
            core,
            steam_attempts: 1,
            steam_last_try: Instant::now(),
            name_from_steam: false,
            persona_attempts: 0,
            persona_last: Instant::now(),
            steam_note: None,
            steam_loop: Arc::default(),
            clipboard: Box::new(clipboard::set_text),
            result: None,
            log_cursor: 0,
            notice: None,
            heartbeat: heartbeat_setting(std::env::var(HEARTBEAT_ENV).ok().as_deref()),
            clock: Box::new(Instant::now),
            pulse: None,
            presence: None,
            published: String::new(),
            version: 0,
        };
        c.publish();
        Ok(c)
    }

    /// How long the local game may be silent before the session ends because its world was closed, instead of
    /// [`HEARTBEAT_SECONDS`] (or the `FEVERLINK3_HEARTBEAT_SECONDS` value). Zero turns the check off. For tests.
    pub fn with_heartbeat(mut self, silence: Duration) -> Controller {
        self.heartbeat = (!silence.is_zero()).then_some(silence);
        self
    }

    /// The clock the heartbeat reads instead of the system's (tests drive it by hand, so no timing depends on how busy
    /// the machine is).
    pub fn with_clock(mut self, now: impl Fn() -> Instant + Send + 'static) -> Controller {
        self.clock = Box::new(now);
        self
    }

    /// Where "Copy code" puts the join code instead of the system clipboard (tests).
    pub fn with_clipboard(mut self, put: impl Fn(&str) -> bool + Send + 'static) -> Controller {
        self.clipboard = Box::new(put);
        self
    }

    /// The options every game this controller hosts will use (player limit, flood limit, ping and reconnect timing).
    /// The defaults are what a player gets; tests shorten the timings.
    pub fn with_host_options(mut self, options: HostOptions) -> Controller {
        self.host_options = options;
        self
    }

    /// Reads new commands, advances the state, rewrites the status file when it changed.
    pub fn step(&mut self) {
        // first, so that a command sees the state the window last saw, not the one before a session ended
        self.advance();
        let lines = self.commands.poll().unwrap_or_default();
        for line in lines {
            let params = luatext::lua_to_json(&line.payload)
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .unwrap_or(Value::Null);
            self.command(line.seq, &line.kind, &params);
        }
        self.advance();
        self.watch_game();
        if !self.name_from_steam && self.persona_attempts < 600 && self.persona_last.elapsed() >= Duration::from_secs(1)
        {
            self.persona_attempts += 1;
            self.persona_last = Instant::now();
            if let Some(name) = steam_persona::persona_name() {
                self.default_name = name;
                self.name_from_steam = true;
            }
        }
        if self.steam.is_none()
            && self.steam_attempts < STEAM_LOAD_ATTEMPTS
            && self.steam_last_try.elapsed() >= STEAM_LOAD_EVERY
        {
            self.steam = steamnet::Probe::load();
            self.steam_attempts += 1;
            self.steam_last_try = Instant::now();
        }
        if let Some(probe) = &mut self.steam {
            probe.poll();
        }
        self.log_speed_test();
        self.write_session_log();
        self.publish();
    }

    /// One line of the controller's own in `session.log`.
    fn append_session_log(&self, line: &str) {
        use std::io::Write;
        let path = self.dir.join(SESSION_LOG_FILE);
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Writes what the Steam speed test logged (the summary line of a finished or failed run, Steam's samples and status
    /// dumps) to `session.log`.
    fn log_speed_test(&mut self) {
        let Some(lines) = self.steam.as_ref().map(|p| p.take_log()).filter(|l| !l.is_empty()) else { return };
        let path = self.dir.join(SESSION_LOG_FILE);
        if fs::metadata(&path).map(|m| m.len() > SESSION_LOG_MAX).unwrap_or(false) {
            let _ = fs::remove_file(&path);
        }
        use std::io::Write;
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            for line in lines {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    /// Appends the session's new log lines (who joined, resync rounds, checksum differences, replay failures) to
    /// `session.log` in the live folder. In the game process nothing reads stderr, so this file is how a problem in a
    /// session can be looked at afterwards. It starts afresh with the controller and is cut when it gets large.
    fn write_session_log(&mut self) {
        let handle = match &self.state {
            State::Hosting(h) => &h.handle,
            State::Joined { handle } => handle,
            _ => return,
        };
        let (lines, total) = handle.logs_since(self.log_cursor);
        if lines.is_empty() {
            self.log_cursor = total;
            return;
        }
        let path = self.dir.join(SESSION_LOG_FILE);
        if fs::metadata(&path).map(|m| m.len() > SESSION_LOG_MAX).unwrap_or(false) {
            let _ = fs::remove_file(&path);
        }
        use std::io::Write;
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            for line in &lines {
                let _ = writeln!(f, "{line}");
            }
        }
        self.log_cursor = total;
    }

    /// After a panic in [`Controller::step`]: stops whatever session runs and shows the reason in the window, so the
    /// next request starts afresh. What the step had half done is thrown away, not trusted.
    pub fn recover(&mut self, why: &str) {
        let _ = guard::guarded(|| self.leave());
        self.state = State::Failed(format!("internal error: {why}"));
    }

    fn command(&mut self, seq: u64, kind: &str, params: &Value) {
        match kind {
            "host" => self.host(params),
            "join" => self.join(params),
            "leave" => self.leave(),
            "kick" => {
                let outcome = self.kick(params);
                self.note(seq, kind, outcome);
            }
            "permissions" => {
                let outcome = self.set_permissions(params);
                self.note(seq, kind, outcome);
            }
            "copy" => {
                // the game's text fields cannot copy, so the host's join code goes to the clipboard from here
                let outcome = match &self.state {
                    State::Hosting(h) if !h.code.is_empty() => {
                        if (self.clipboard)(&h.code) {
                            Ok("join code copied to the clipboard".to_owned())
                        } else {
                            Err("the clipboard is not available right now".to_owned())
                        }
                    }
                    _ => Err("there is no join code to copy".to_owned()),
                };
                self.note(seq, kind, outcome);
            }
            "steamtest" => {
                let text = params.get("id").and_then(Value::as_str).unwrap_or("").trim();
                // the window has no button for the transport self-check: typing `loop` where the Steam ID goes starts it
                if text.eq_ignore_ascii_case("loop") {
                    return self.steam_loop();
                }
                let id = steamnet::parse_steam_id(text);
                // `mb` asks for the speed test (8, 32 or 80 MB in the window; any number up to 128 is accepted)
                let speed_mb = params.get("mb").and_then(Value::as_f64).filter(|mb| *mb >= 1.0).map(|mb| mb as u32);
                match (&mut self.steam, id) {
                    (Some(probe), Some(id)) => match speed_mb {
                        Some(mb) => {
                            // `mode`: reliable (default), dgram, both, parallel (reliable on 1, 2 and 4 connections), auto (the resync's
                            // parallel connections, scaled to the size), all
                            let mode = params.get("mode").and_then(Value::as_str).unwrap_or("reliable");
                            probe.start_plan(id, steamnet::specs_for(mode, mb));
                        }
                        None => probe.test(id),
                    },
                    (Some(_), None) => self.steam_note = Some("that is not a Steam ID (17 digits)".into()),
                    (None, _) => self.steam_note = Some("Steam networking is not available in this game".into()),
                }
            }
            "steamloop" => self.steam_loop(),
            "start" | "pause" | "speed" | "check" | "resync" => {
                if let State::Hosting(h) = &self.state {
                    let handle = &h.handle;
                    match kind {
                        "start" => handle.command(Command::Start),
                        "pause" => handle.command(Command::Pause),
                        "speed" => {
                            let n = params.get("n").and_then(Value::as_i64).unwrap_or(1).clamp(1, 4);
                            handle.command(Command::Speed(n as i32));
                        }
                        "check" => handle.command(Command::Checksum),
                        _ => handle.command(Command::Resync),
                    }
                }
            }
            _ => {}
        }
    }

    /// The Steam core sessions use: the one given at construction, else the process's (when the game has Steam).
    fn steam_core(&self) -> Option<CoreRef> {
        self.core.clone().or_else(steamnet::global)
    }

    /// Starts the Steam transport self-check on a thread of its own, unless one is running.
    fn steam_loop(&mut self) {
        let Some(core) = self.steam.as_ref().and(self.steam_core()) else {
            self.steam_note = Some("Steam networking is not available in this game".into());
            return;
        };
        {
            let mut state = self.steam_loop.lock().unwrap_or_else(|p| p.into_inner());
            if state.running {
                return;
            }
            *state = LoopCheck { running: true, checks: Vec::new() };
        }
        let shared = Arc::clone(&self.steam_loop);
        thread::spawn(move || {
            // a panic must not leave the window waiting for a check that never ends
            let checks = guard::guarded(|| steam_transport::loopback_check(&core)).unwrap_or_default();
            *shared.lock().unwrap_or_else(|p| p.into_inner()) = LoopCheck { running: false, checks };
        });
    }

    /// Records how a command that can be refused ended, for the status.
    fn note(&mut self, seq: u64, kind: &str, outcome: Result<String, String>) {
        let (ok, message) = match outcome {
            Ok(message) => (true, message),
            Err(message) => (false, message),
        };
        self.result = Some(CommandResult { seq, kind: kind.to_owned(), ok, message });
    }

    /// `kick {id}`: removes a player, or releases the slot held for one that dropped.
    fn kick(&self, params: &Value) -> Result<String, String> {
        let State::Hosting(h) = &self.state else { return Err("only the host can remove a player".into()) };
        let id = params.get("id").and_then(player_id).ok_or("kick needs the id of a player")?;
        if id == 0 {
            return Err("the host cannot be removed".into());
        }
        let status = h.handle.status();
        let entry = status.roster.iter().find(|e| e.id == id).ok_or_else(|| format!("there is no player #{id}"))?;
        h.handle.command(Command::Kick(id));
        Ok(match entry.state {
            ConnectionState::Connected => format!("{} was removed", entry.name),
            ConnectionState::Away => format!("the slot of {} was released", entry.name),
        })
    }

    /// `permissions {build, bulldoze, spend, speed, other}`: each key named is replaced, the others stay as they are.
    fn set_permissions(&mut self, params: &Value) -> Result<String, String> {
        let State::Hosting(h) = &mut self.state else { return Err("only the host can change permissions".into()) };
        let next = permissions_with(&h.permissions, params)?;
        h.permissions = next.clone();
        h.handle.command(Command::SetPermissions(next));
        Ok("permissions sent to everyone".into())
    }

    fn name_from(&self, params: &Value) -> String {
        params
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map_or_else(|| self.default_name.clone(), |n| n.chars().take(24).collect())
    }

    fn idle_or_failed(&self) -> bool {
        matches!(self.state, State::Idle | State::Failed(_))
    }

    fn host(&mut self, params: &Value) {
        if !self.idle_or_failed() {
            return;
        }
        self.notice = None;
        self.pulse = None;
        self.presence = None;
        // Players never open ports or touch their router: sessions travel over Steam's peer-to-peer networking (and
        // a relay as the fallback). The direct TCP transport is for development and tests and must be asked for by
        // name.
        // `FEVERLINK3_TRANSPORT=tcp-loopback` makes the window's own Host button use the developer transport on this
        // PC only (two games on one PC cannot use Steam to reach each other: same account). Players never set it.
        let wanted = params
            .get("transport")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| std::env::var("FEVERLINK3_TRANSPORT").ok())
            .unwrap_or_else(|| "auto".into());
        match wanted.as_str() {
            "tcp" => self.host_tcp(params, false),
            "tcp-loopback" => self.host_tcp(params, true),
            "steam" | "auto" => self.host_steam(params),
            other => self.state = State::Failed(format!("unknown transport {other:?}; use \"steam\" or \"tcp\"")),
        }
    }

    /// Hosts over Steam: no address and no port. The join code carries this account's Steam ID.
    fn host_steam(&mut self, params: &Value) {
        let Some(core) = self.steam_core() else {
            self.state = State::Failed("Steam networking is not available in this game".into());
            return;
        };
        let status = core.status();
        let Some(steam_id) = status.steam_id.filter(|_| status.ready) else {
            self.state = State::Failed(format!("Steam is not ready yet ({}); try again in a moment", status.note));
            return;
        };
        let name = self.name_from(params);
        let password = params.get("password").and_then(Value::as_str).filter(|p| !p.is_empty()).map(str::to_owned);
        // inside the game a panic ends the process: no random numbers is a failed host, not a crash
        let Some(secret) = SessionKey::try_new_secret() else {
            self.state = State::Failed("this PC has no source of random numbers to make a session key".into());
            return;
        };
        let key = SessionKey::derive(&secret, password.as_deref());
        let link = match gamelink::Session::open(&self.dir) {
            Ok(l) => l,
            Err(e) => return self.state = State::Failed(format!("cannot open the session folder: {e}")),
        };
        let config = HostConfig {
            bulk: steamnet::bulk_io::BulkSettings::from_env(),
            bind: SocketAddr::from(([0, 0, 0, 0], 0)), // not used by Steam
            name,
            key,
            link,
            options: self.host_options.clone(),
            save_dirs: self.save_dirs.clone(),
            echo_log: false,
        };
        let handle = match steam_transport::run_steam_host_on(&core, config) {
            Ok(h) => h,
            Err(e) => return self.state = State::Failed(format!("cannot host over Steam: {e}")),
        };
        let code = match JoinCode::create(&[], secret, None) {
            Ok(code) => code.with_steam_id(steam_id).encode(),
            Err(e) => {
                handle.stop();
                return self.state = State::Failed(format!("cannot make a join code: {e}"));
            }
        };
        self.result = None;
        self.state = State::Hosting(Box::new(Hosting {
            handle,
            permissions: Permissions::default(),
            secret,
            lan: Vec::new(),
            code,
            endpoints: Vec::new(),
            password: password.is_some(),
            steam_id: Some(steam_id),
        }));
    }

    /// Hosts over direct TCP (developer transport): the join code lists the machine's local addresses.
    fn host_tcp(&mut self, params: &Value, force_loopback: bool) {
        let name = self.name_from(params);
        let port =
            params.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).unwrap_or(DEFAULT_PORT);
        let password = params.get("password").and_then(Value::as_str).filter(|p| !p.is_empty()).map(str::to_owned);
        // inside the game a panic ends the process: no random numbers is a failed host, not a crash
        let Some(secret) = SessionKey::try_new_secret() else {
            self.state = State::Failed("this PC has no source of random numbers to make a session key".into());
            return;
        };
        let key = SessionKey::derive(&secret, password.as_deref());
        let link = match gamelink::Session::open(&self.dir) {
            Ok(l) => l,
            Err(e) => return self.state = State::Failed(format!("cannot open the session folder: {e}")),
        };
        // `bind` = "loopback" keeps the developer transport on this PC only: no firewall prompt, no exposed port
        let loopback = force_loopback || params.get("bind").and_then(Value::as_str) == Some("loopback");
        let ip = if loopback { [127, 0, 0, 1] } else { [0, 0, 0, 0] };
        let started = session::run_host(HostConfig {
            bulk: steamnet::bulk_io::BulkSettings::from_env(),
            bind: SocketAddr::from((ip, port)),
            name,
            key,
            link,
            options: self.host_options.clone(),
            save_dirs: self.save_dirs.clone(),
            echo_log: false,
        });
        let handle = match started {
            Ok(h) => h,
            Err(e) => return self.state = State::Failed(format!("cannot listen on port {port}: {e}")),
        };
        let bound = handle.local_addr().map_or(port, |a| a.port());
        self.result = None;
        let mut hosting = Hosting {
            handle,
            permissions: Permissions::default(),
            secret,
            lan: if loopback { vec![SocketAddr::from(([127, 0, 0, 1], bound))] } else { net::lan_endpoints(bound) },
            code: String::new(),
            endpoints: Vec::new(),
            password: password.is_some(),
            steam_id: None,
        };
        hosting.make_code();
        if hosting.code.is_empty() {
            hosting.handle.stop();
            self.state = State::Failed("cannot make a join code".into());
        } else {
            self.state = State::Hosting(Box::new(hosting));
        }
    }

    fn join(&mut self, params: &Value) {
        if !self.idle_or_failed() {
            return;
        }
        self.notice = None;
        self.pulse = None;
        self.presence = None;
        let name = self.name_from(params);
        let password = params.get("password").and_then(Value::as_str).filter(|p| !p.is_empty()).map(str::to_owned);
        let text = params.get("code").and_then(Value::as_str).unwrap_or("");
        let Some(code) = JoinCode::try_decode(text.trim()) else {
            self.state = State::Failed("that is not a valid join code".into());
            return;
        };
        let key = SessionKey::derive(&code.secret, password.as_deref());
        let (dir, save_dirs) = (self.dir.clone(), self.save_dirs.clone());
        let (tx, rx) = mpsc::channel();
        let progress = Arc::new(Mutex::new(String::new()));
        let (core, said) = (self.steam_core(), Arc::clone(&progress));
        // run_player connects before it returns (5 s per TCP endpoint, up to 30 s for a Steam route): off the
        // controller's thread
        let connect = move || {
            let result = gamelink::Session::open(&dir)
                .map_err(|e| format!("cannot open the session folder: {e}"))
                .and_then(|link| match code.steam_id {
                    // a code with a Steam ID is joined over Steam, whatever else it lists
                    Some(id) => {
                        let core = core.ok_or("Steam networking is not available in this game")?;
                        let mut config = steam_transport::steam_player_config(&name, key, link);
                        config.save_dirs = save_dirs;
                        let mut say = |text: &str| *said.lock().unwrap_or_else(|p| p.into_inner()) = text.to_owned();
                        steam_transport::run_steam_player_on(&core, config, SteamEndpoint(id), &mut say)
                            .map_err(|e| format!("cannot reach the host over Steam: {e}"))
                    }
                    None => {
                        let mut config = PlayerConfig::new(code.endpoints.clone(), &name, key, link);
                        config.save_dirs = save_dirs;
                        session::run_player(config).map_err(|e| format!("cannot reach the host: {e}"))
                    }
                });
            let _ = tx.send(result);
        };
        self.state = match thread::Builder::new().name("feverlink3-join".into()).spawn(connect) {
            Ok(_) => State::Joining { rx, progress },
            Err(e) => State::Failed(format!("cannot start the connection: {e}")),
        };
    }

    fn leave(&mut self) {
        self.result = None;
        self.notice = None;
        self.pulse = None;
        self.presence = None;
        // stopping a host tells every player the host ended the session (`HostSession::end_session`); a player says
        // goodbye so that the host frees its slot at once
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Hosting(h) => h.handle.stop(),
            State::Joined { handle } => handle.stop(),
            _ => {}
        }
        if let Ok(link) = gamelink::Session::open(&self.dir) {
            let _ = link.write_identity(0, &self.default_name, "idle");
        }
    }

    /// The local game stopped talking: its world was closed to the main menu. A host stops hosting (its players are told
    /// the host ended the session) and a player leaves (the host frees its slot); the controller is idle again with a
    /// notice for the window.
    fn world_closed(&mut self) {
        self.leave();
        self.notice = Some(WORLD_CLOSED_NOTICE.to_owned());
    }

    /// The heartbeat: ends the session when the local game has spoken and then fell silent (see [`Pulse`]). Does
    /// nothing outside a session, before the game's first line, or while the game's world is being replaced by a resync.
    fn watch_game(&mut self) {
        let reloading = match &self.state {
            State::Hosting(h) => game_is_reloading(&h.handle.status()),
            State::Joined { handle } => game_is_reloading(&handle.status()),
            _ => {
                self.pulse = None;
                self.presence = None;
                self.presence = None;
                return;
            }
        };
        let now = (self.clock)();
        let lock_path = self.dir.join(WORLD_LOCK_FILE);
        let lock = self.presence.get_or_insert_with(|| PresenceLock::new(lock_path, now));
        if lock.world_gone(reloading, now) {
            self.append_session_log("the game's world is closed (the mod's presence lock is free): the session ends");
            self.world_closed();
            return;
        }
        let Some(limit) = self.heartbeat else { return };
        let path = self.dir.join(gamelink::FROM_GAME);
        let pulse = self.pulse.get_or_insert_with(|| Pulse::start(path, now));
        if pulse.silent(limit, reloading, now) {
            self.world_closed();
        }
    }

    fn advance(&mut self) {
        if let State::Joined { handle } = &self.state {
            // the host removed us, refused us, or we gave up reconnecting: the window must be able to start over
            let s = handle.status();
            // a reject or an error is the thread's last word: it ends in the next breath, so do not show a gap
            if s.finished || s.rejected.is_some() || s.error.is_some() {
                let why = ended_message(&s);
                self.notice = Some(sentence(&why));
                self.state = State::Failed(why);
                self.pulse = None;
                self.presence = None;
                self.presence = None;
                if let Ok(link) = gamelink::Session::open(&self.dir) {
                    let _ = link.write_identity(0, &self.default_name, "idle");
                }
            }
            return;
        }
        let State::Joining { rx, .. } = &self.state else { return };
        match rx.try_recv() {
            Ok(Ok(handle)) => self.state = State::Joined { handle },
            Ok(Err(why)) => self.state = State::Failed(why),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.state = State::Failed("joining was interrupted".into()),
        }
    }

    /// The status as the window reads it, without the version counter.
    fn status_json(&self) -> Value {
        let mut v = json!({ "state": "idle", "name": self.default_name });
        v["steam"] = self.steam_json();
        match &self.state {
            State::Idle => {}
            State::Failed(why) => {
                v["state"] = json!("failed");
                v["message"] = json!(why);
            }
            State::Joining { progress, .. } => {
                v["state"] = json!("joining");
                // what the connection attempt is doing: "contacting the host", "finding a route", ...
                let said = progress.lock().unwrap_or_else(|p| p.into_inner());
                if !said.is_empty() {
                    v["progress"] = json!(*said);
                }
            }
            State::Hosting(h) => {
                v["state"] = json!("hosting");
                v["code"] = json!(h.code);
                v["endpoints"] = json!(h.endpoints);
                v["password"] = json!(h.password);
                v["transport"] = json!(if h.steam_id.is_some() { "steam" } else { "tcp" });
                v["steamid"] = json!(h.steam_id.map(|id| id.to_string()));
                v["capacity"] = json!(self.host_options.max_players);
                add_session(&mut v, &h.handle);
            }
            State::Joined { handle } => {
                let s = handle.status();
                let state = if s.rejected.is_some() || s.error.is_some() {
                    // the next step turns this into State::Failed; the window must not see a gap
                    let why = ended_message(&s);
                    v["notice"] = json!(sentence(&why));
                    v["message"] = json!(why);
                    "failed"
                } else if s.connected {
                    "joined"
                } else {
                    "joining"
                };
                if s.reconnecting {
                    v["reconnecting"] = json!(true);
                    v["attempts"] = json!(s.reconnect_attempts);
                }
                v["state"] = json!(state);
                add_session(&mut v, handle);
            }
        }
        if let Some(r) = &self.result {
            v["result"] = json!({ "seq": r.seq, "kind": r.kind, "ok": r.ok, "message": r.message });
        }
        if let Some(notice) = &self.notice {
            v["notice"] = json!(notice);
        }
        v
    }

    fn publish(&mut self) {
        let mut v = self.status_json();
        let text = v.to_string();
        if text == self.published {
            return;
        }
        self.version += 1;
        v["version"] = json!(self.version);
        let Some(lua) = serde_json::to_vec(&v).ok().and_then(|b| luatext::json_to_lua(&b)) else { return };
        if write_replacing(&self.dir.join(STATUS_FILE), &lua).is_ok() {
            self.published = text;
        }
    }

    fn steam_json(&self) -> Value {
        let Some(probe) = &self.steam else {
            let state =
                if self.steam_attempts < STEAM_LOAD_ATTEMPTS { "looking for Steam..." } else { "not available" };
            return json!({ "state": state, "note": self.steam_note });
        };
        let r = probe.report();
        let mut v = json!({
            "state": r.state,
            "id": r.steam_id.map(|id| id.to_string()),
            "events": r.events,
            "pings": r.pings_received,
            "note": self.steam_note,
        });
        if let Some(t) = r.test {
            v["test"] = json!({
                "target": t.target.to_string(),
                "state": t.state,
                "pongs": t.pongs,
                "rtt": t.last_rtt_ms,
                "route": t.route,
                "ping": t.ping_ms,
                "rate": t.send_rate,
            });
        }
        if let Some(sp) = r.speed {
            v["speed"] = json!({ "running": sp.running, "status": sp.status, "lines": sp.lines });
        }
        v["peers"] = json!(r
            .peers
            .iter()
            .map(|p| json!({
                "id": p.steam_id.map(|id| id.to_string()),
                "route": p.route,
                "ping": p.ping_ms,
                "rate": p.send_rate,
            }))
            .collect::<Vec<_>>());
        let looped = self.steam_loop.lock().unwrap_or_else(|p| p.into_inner());
        if looped.running || !looped.checks.is_empty() {
            let checks = looped.checks.iter().map(|c| json!({ "name": c.name, "ok": c.ok, "detail": c.detail }));
            v["loop"] = json!({ "running": looped.running, "checks": checks.collect::<Vec<_>>() });
        }
        v
    }

    /// For tests: the current state's name.
    pub fn state_name(&self) -> &'static str {
        match self.status_json()["state"].as_str() {
            Some("hosting") => "hosting",
            Some("joined") => "joined",
            Some("joining") => "joining",
            Some("failed") => "failed",
            _ => "idle",
        }
    }
}

fn add_session(v: &mut Value, handle: &SessionHandle) {
    let s = handle.status();
    v["me"] = json!(s.my_id);
    v["players"] = json!(players_json(&s));
    v["permissions"] = permissions_json(&s.permissions);
    if let Some(sess) = &s.session {
        v["started"] = json!(sess.started);
        v["speed"] = json!(sess.speed);
    }
    if let Some(r) = &s.resync {
        v["resync"] = json!(r);
    }
    // what a sync round is doing right now, for the window (host and players): the host's phase, the chunks of the world
    // sent or received, and this game's own loading (it is silent while its world is replaced)
    let phase =
        s.session.as_ref().and_then(|sess| sess.resync.clone()).or_else(|| s.reloading.then(|| "loading".to_owned()));
    if let Some(phase) = phase {
        let mut sync = json!({ "phase": phase });
        if let Some((done, total)) = s.transfer {
            sync["done"] = json!(done);
            sync["total"] = json!(total);
        }
        if let Some(note) = &s.transfer_note {
            sync["note"] = json!(note);
        }
        v["sync"] = sync;
    }
    // ping and route per connection, for transports that measure them (Steam)
    v["links"] = json!(s
        .links
        .iter()
        .map(|l| json!({ "conn": l.conn, "ping": l.ping_ms, "route": l.route }))
        .collect::<Vec<_>>());
}

/// Every player of the session, away ones included. Before the first roster exists, the plain player list.
fn players_json(s: &Status) -> Vec<Value> {
    if s.roster.is_empty() {
        return s
            .players
            .iter()
            .map(|p| json!({ "id": p.id, "name": p.name, "role": role_of(p.id), "state": "connected", "ready": p.ready }))
            .collect();
    }
    s.roster.iter().map(player_json).collect()
}

fn role_of(id: u16) -> &'static str {
    if id == 0 {
        "host"
    } else {
        "player"
    }
}

fn player_json(e: &RosterEntry) -> Value {
    json!({
        "id": e.id,
        "name": e.name,
        "role": e.role(),
        "state": e.state.as_str(),
        "ready": e.ready,
        "rtt": e.rtt_ms.map(rtt_shown),
    })
}

/// A round-trip time as the window sees it, in whole milliseconds, rounded to 5 ms below 20 ms, 10 ms below 100 ms and
/// 25 ms above, so that the status file is not rewritten at every ping for a value that wobbles by a millisecond.
fn rtt_shown(ms: f64) -> u32 {
    let step = if ms < 20.0 {
        5.0
    } else if ms < 100.0 {
        10.0
    } else {
        25.0
    };
    ((ms.max(0.0) / step).round() * step) as u32
}

fn permissions_json(p: &Permissions) -> Value {
    json!({
        "build": set_json(&p.build),
        "bulldoze": set_json(&p.bulldoze),
        "spend": set_json(&p.spend),
        "speed": set_json(&p.speed),
        "other": set_json(&p.other),
    })
}

fn set_json(set: &PlayerSet) -> Value {
    match set {
        PlayerSet::Everyone => json!("everyone"),
        PlayerSet::HostOnly => json!("hostOnly"),
        PlayerSet::Only(ids) => json!({ "only": ids }),
    }
}

/// A player id from a command: a whole number that fits 16 bits (the Lua side may write it as `2.0`).
fn player_id(v: &Value) -> Option<u16> {
    let n = v.as_u64().or_else(|| v.as_f64().filter(|f| *f >= 0.0 && f.fract() == 0.0).map(|f| f as u64))?;
    u16::try_from(n).ok()
}

/// One permission value: `"everyone"`, `"hostOnly"` or `{only = {ids}}`.
fn player_set(v: &Value) -> Result<PlayerSet, String> {
    match v {
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "everyone" => Ok(PlayerSet::Everyone),
            "hostonly" | "host_only" | "host-only" => Ok(PlayerSet::HostOnly),
            _ => Err(format!("{s:?} is not everyone, hostOnly or {{only = ids}}")),
        },
        Value::Object(map) => {
            let mut ids = match map.get("only") {
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|i| player_id(i).ok_or_else(|| format!("{i} is not a player id")))
                    .collect::<Result<Vec<u16>, String>>()?,
                // an empty Lua table has no way to say it is a list
                Some(Value::Object(o)) if o.is_empty() => Vec::new(),
                _ => return Err("only needs a list of player ids".into()),
            };
            ids.sort_unstable();
            ids.dedup();
            Ok(PlayerSet::Only(ids))
        }
        _ => Err("must be everyone, hostOnly or {only = ids}".into()),
    }
}

/// `current` with every key `params` names replaced. At least one key is required.
fn permissions_with(current: &Permissions, params: &Value) -> Result<Permissions, String> {
    let mut next = current.clone();
    let mut named = 0;
    for (name, slot) in [
        ("build", &mut next.build),
        ("bulldoze", &mut next.bulldoze),
        ("spend", &mut next.spend),
        ("speed", &mut next.speed),
        ("other", &mut next.other),
    ] {
        if let Some(v) = params.get(name).filter(|v| !v.is_null()) {
            *slot = player_set(v).map_err(|why| format!("{name}: {why}"))?;
            named += 1;
        }
    }
    if named == 0 {
        return Err("name at least one of build, bulldoze, spend, speed, other".into());
    }
    Ok(next)
}

/// `text` with its first letter capitalised: a message in the style of `ended_message` as a line of its own.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

/// Why a joined session is over, in words for the window. `ended`, `kicked` and `flood` are the host ending it on
/// purpose: `ended` when the host stopped hosting or its world was closed, `kicked` when it removed this player.
fn ended_message(s: &Status) -> String {
    match (&s.rejected, &s.error) {
        (Some(why), _) if why.starts_with("ended") => "the host ended the session".to_owned(),
        (Some(why), _) if why.starts_with("kicked") => "the host removed you from the session".to_owned(),
        (Some(why), _) if why.starts_with("flood") => format!("the host disconnected you: {why}"),
        (Some(why), _) => format!("the host rejected us: {why}"),
        (None, Some(why)) => why.clone(),
        (None, None) => "the session ended".to_owned(),
    }
}

/// Writes through a temporary file and a rename; when the rename is refused (the mod has the file open) the
/// content is written in place, which the reader tolerates because it re-reads on the next poll.
fn write_replacing(path: &Path, text: &str) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    if fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
        fs::write(path, text)?;
    }
    Ok(())
}

/// The name the window offers before the player types one.
///
/// Not the Windows user name: that would be shown to every other player and written to logs. It is the neutral
/// [`steam_persona::FALLBACK_NAME`] until the controller reads the player's Steam name, which then replaces it.
pub fn default_name() -> String {
    steam_persona::FALLBACK_NAME.to_owned()
}

/// Starts the controller on a thread of its own. `None` when there is no folder to use. Never returns an error to
/// the game: whatever goes wrong here must not stop it from starting.
pub fn spawn() -> Option<thread::JoinHandle<()>> {
    let dir = live_dir()?;
    thread::Builder::new()
        .name("feverlink3-live".into())
        .spawn(move || {
            let started = guard::guarded(|| {
                let save_dirs = resync::save_dirs(&steam::discover().user_dirs);
                Controller::new(dir, default_name(), save_dirs)
            });
            let Ok(Ok(mut controller)) = started else { return };
            loop {
                // a panic in a step (only survivable when the build unwinds, see `guard`) stops the session and shows
                // in the window; the controller goes on
                if let Err(why) = guard::guarded(|| controller.step()) {
                    controller.recover(&why);
                }
                thread::sleep(Duration::from_millis(100));
            }
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamelink::Line;
    use crate::steamnet::fake::FakeMachine;
    use crate::steamnet::SteamCore;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::time::Instant;

    /// The window's side: appends a command line.
    struct Window {
        dir: PathBuf,
        seq: u64,
    }

    impl Window {
        fn new(dir: &Path) -> Window {
            Window { dir: dir.to_path_buf(), seq: 1 }
        }
        fn send(&mut self, kind: &str, payload: &str) {
            let line = Line { seq: self.seq, kind: kind.into(), from: "0".into(), payload: payload.into() };
            self.seq += 1;
            let mut f = OpenOptions::new().create(true).append(true).open(self.dir.join(CMD_FILE)).unwrap();
            f.write_all(gamelink::format_line(&line).as_bytes()).unwrap();
        }
        fn status(&self) -> Value {
            let text = fs::read_to_string(self.dir.join(STATUS_FILE)).unwrap_or_default();
            luatext::lua_to_json(&text).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
        }
    }

    fn until(controllers: &mut [&mut Controller], timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            for c in controllers.iter_mut() {
                c.step();
            }
            if done() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        done()
    }

    #[test]
    fn a_new_controller_is_idle_and_clears_the_folder() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("to_game.log"), "1\tsession\t0\t{}\n").unwrap();
        let c = Controller::new(t.path().to_path_buf(), "Pat".into(), Vec::new()).unwrap();
        assert_eq!(c.state_name(), "idle");
        assert!(!t.path().join("to_game.log").exists());
        let identity = fs::read_to_string(t.path().join("identity.txt")).unwrap();
        assert!(identity.contains("role=idle") && identity.contains("name=Pat"), "{identity}");
        let w = Window::new(t.path());
        assert_eq!(w.status()["state"], "idle");
        assert_eq!(w.status()["name"], "Pat");
    }

    #[test]
    fn host_then_join_then_leave_through_files_only() {
        let root = tempfile::tempdir().unwrap();
        let (hd, pd) = (root.path().join("host"), root.path().join("player"));
        let mut host = Controller::new(hd.clone(), "Host".into(), Vec::new()).unwrap();
        let mut player = Controller::new(pd.clone(), "Pat".into(), Vec::new()).unwrap();
        let (mut hw, mut pw) = (Window::new(&hd), Window::new(&pd));

        hw.send("host", "{[\"name\"]=\"Hosty\",[\"port\"]=0,[\"transport\"]=\"tcp\",[\"bind\"]=\"loopback\"}");
        assert!(until(&mut [&mut host], Duration::from_secs(10), || hw.status()["state"] == "hosting"));
        let status = hw.status();
        let code = status["code"].as_str().expect("a join code").to_owned();
        assert!(fs::read_to_string(hd.join("identity.txt")).unwrap().contains("role=host"));

        // the join code lists LAN addresses; the test reaches the listener over loopback
        let mut decoded = JoinCode::try_decode(&code).expect("a decodable code");
        let port = decoded.endpoints.first().map_or(0, SocketAddr::port);
        assert_ne!(port, 0, "the host listens somewhere");
        decoded.endpoints = vec![SocketAddr::from(([127, 0, 0, 1], port))];

        pw.send("join", &format!("{{[\"code\"]=\"{}\"}}", decoded.encode()));
        assert!(
            until(&mut [&mut host, &mut player], Duration::from_secs(15), || pw.status()["state"] == "joined"),
            "player status {:?}",
            pw.status()
        );
        assert!(fs::read_to_string(pd.join("identity.txt")).unwrap().contains("role=player"));
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(10), || {
            hw.status()["players"].as_array().is_some_and(|p| p.len() == 2)
        }));

        pw.send("leave", "{}");
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(5), || pw.status()["state"] == "idle"));
        hw.send("leave", "{}");
        assert!(until(&mut [&mut host], Duration::from_secs(5), || hw.status()["state"] == "idle"));
        assert!(fs::read_to_string(hd.join("identity.txt")).unwrap().contains("role=idle"));
    }

    /// A controller on a fake Steam machine of its own, with its window.
    struct SteamPc {
        machine: FakeMachine,
        _core: SteamCore,
        controller: Controller,
        window: Window,
        dir: PathBuf,
    }

    fn steam_pc(root: &Path, name: &str) -> SteamPc {
        let machine = FakeMachine::new();
        let core = machine.core();
        let dir = root.join(name);
        let controller = Controller::with_core(dir.clone(), name.into(), Vec::new(), core.handle()).unwrap();
        SteamPc { machine, _core: core, controller, window: Window::new(&dir), dir }
    }

    #[test]
    fn the_speed_test_runs_from_the_window_and_leaves_one_line_in_the_session_log() {
        let root = tempfile::tempdir().unwrap();
        let (mut friend, mut me) = (steam_pc(root.path(), "friend"), steam_pc(root.path(), "me"));
        let id = friend.machine.steam_id();
        me.window.send("steamtest", &format!("{{[\"id\"]=\"{id}\",[\"mb\"]=2}}"));
        assert!(until(&mut [&mut friend.controller, &mut me.controller], Duration::from_secs(20), || {
            me.window.status()["steam"]["speed"]["running"] == false
        }));
        let speed = me.window.status()["steam"]["speed"].clone();
        let lines: Vec<&str> = speed["lines"].as_array().expect("lines").iter().filter_map(|l| l.as_str()).collect();
        assert_eq!(lines.len(), 4, "{speed:?}");
        assert!(lines[0].starts_with("Speed test, 2 MB each way: you to friend "), "{lines:?}");
        assert!(speed["status"].as_str().is_some_and(|s| s == lines[0]));
        // one summary line, written once however long the controller keeps running
        until(&mut [&mut friend.controller, &mut me.controller], Duration::from_millis(500), || false);
        let log = fs::read_to_string(me.dir.join(SESSION_LOG_FILE)).expect("a session log");
        let summary: Vec<&str> = log.lines().filter(|l| l.starts_with("steam speed test: ")).collect();
        assert_eq!(summary.len(), 1, "{log}");
        assert!(summary[0].contains("2 MB each way") && summary[0].contains("80 MB save about"), "{log}");
        // only the tester logs a summary; the responder logs its own side (settings, what it sent, Steam's status)
        let theirs = fs::read_to_string(friend.dir.join(SESSION_LOG_FILE)).unwrap_or_default();
        assert!(!theirs.lines().any(|l| l.starts_with("steam speed test: ")), "{theirs}");
    }

    #[test]
    fn a_speed_test_without_a_valid_id_is_refused_in_words() {
        let root = tempfile::tempdir().unwrap();
        let mut me = steam_pc(root.path(), "me");
        me.window.send("steamtest", "{[\"id\"]=\"abc\",[\"mb\"]=8}");
        assert!(until(&mut [&mut me.controller], Duration::from_secs(5), || {
            me.window.status()["steam"]["note"].as_str().is_some_and(|n| n.contains("not a Steam ID"))
        }));
        assert!(me.window.status()["steam"]["speed"].is_null());
    }

    #[test]
    fn hosting_and_joining_over_steam_needs_no_address_and_no_port() {
        let root = tempfile::tempdir().unwrap();
        let (mut host, mut guest) = (steam_pc(root.path(), "host"), steam_pc(root.path(), "guest"));
        // the default transport is Steam, and the probe runs on the same fake Steam
        host.window.send("host", "{[\"name\"]=\"Hosty\"}");
        assert!(
            until(&mut [&mut host.controller], Duration::from_secs(10), || host.window.status()["state"] == "hosting")
        );
        let status = host.window.status();
        assert_eq!(status["transport"], "steam");
        assert_eq!(status["steamid"], host.machine.steam_id().to_string());
        // an empty list comes back from the Lua table as an empty table, not necessarily as an array
        assert!(status["endpoints"].as_array().is_none_or(Vec::is_empty), "no address, no port: {status:?}");
        let code = status["code"].as_str().expect("a join code").to_owned();
        let decoded = JoinCode::try_decode(&code).expect("a decodable code");
        assert_eq!((decoded.steam_id, decoded.endpoints.is_empty()), (Some(host.machine.steam_id()), true));
        assert!(fs::read_to_string(host.dir.join("identity.txt")).unwrap().contains("role=host"));

        host.machine.set_route_delay(Duration::from_millis(400));
        guest.window.send("join", &format!("{{[\"code\"]=\"{code}\"}}"));
        // while the route is searched the window can say so
        let mut seen = Vec::new();
        assert!(until(&mut [&mut host.controller, &mut guest.controller], Duration::from_secs(15), || {
            let status = guest.window.status();
            if let Some(text) = status["progress"].as_str() {
                if seen.last().map(String::as_str) != Some(text) {
                    seen.push(text.to_owned());
                }
            }
            status["state"] == "joined"
        }));
        assert!(seen.iter().any(|t| t == "finding a route"), "{seen:?}");
        assert!(fs::read_to_string(guest.dir.join("identity.txt")).unwrap().contains("role=player"));
        assert!(until(&mut [&mut host.controller, &mut guest.controller], Duration::from_secs(10), || {
            host.window.status()["players"].as_array().is_some_and(|p| p.len() == 2)
        }));

        // ping and route of the connection are in both windows' status
        let measured = |w: &Window| {
            w.status()["links"]
                .as_array()
                .is_some_and(|l| l.len() == 1 && l[0]["ping"] == 30 && l[0]["route"] == "direct")
        };
        assert!(until(&mut [&mut host.controller, &mut guest.controller], Duration::from_secs(10), || {
            measured(&host.window) && measured(&guest.window)
        }));

        guest.window.send("leave", "{}");
        assert!(until(&mut [&mut host.controller, &mut guest.controller], Duration::from_secs(5), || {
            guest.window.status()["state"] == "idle"
        }));
        host.window.send("leave", "{}");
        assert!(until(&mut [&mut host.controller], Duration::from_secs(5), || host.window.status()["state"] == "idle"));
    }

    #[test]
    fn hosting_over_steam_waits_for_steam_and_says_so() {
        let root = tempfile::tempdir().unwrap();
        let mut pc = steam_pc(root.path(), "host");
        pc.machine.set_ready(false);
        // wait until the core has noticed that Steam is not there
        thread::sleep(Duration::from_millis(100));
        pc.window.send("host", "{}");
        assert!(until(&mut [&mut pc.controller], Duration::from_secs(5), || pc.window.status()["state"] == "failed"));
        let message = pc.window.status()["message"].as_str().unwrap().to_owned();
        assert!(message.contains("Steam is not ready yet"), "{message}");
        // Steam comes up: the same request now works
        pc.machine.set_ready(true);
        let ready = Instant::now() + Duration::from_secs(5);
        while !pc._core.handle().status().ready && Instant::now() < ready {
            thread::sleep(Duration::from_millis(10));
        }
        pc.window.send("host", "{}");
        assert!(until(&mut [&mut pc.controller], Duration::from_secs(10), || pc.window.status()["state"] == "hosting"));
    }

    #[test]
    fn an_unknown_transport_and_a_steam_code_without_steam_fail_with_a_message() {
        let root = tempfile::tempdir().unwrap();
        let mut c = Controller::new(root.path().join("a"), "Pat".into(), Vec::new()).unwrap();
        let mut w = Window::new(&root.path().join("a"));
        w.send("host", "{[\"transport\"]=\"carrier pigeon\"}");
        c.step();
        assert!(w.status()["message"].as_str().unwrap().contains("unknown transport"));
        // a code that names a Steam ID cannot be joined where there is no Steam
        let code = JoinCode::create(&[], [7; 16], None).unwrap().with_steam_id(76561198000000001).encode();
        w.send("join", &format!("{{[\"code\"]=\"{code}\"}}"));
        assert!(until(&mut [&mut c], Duration::from_secs(5), || {
            w.status()["message"].as_str().is_some_and(|m| m.contains("Steam networking is not available"))
        }));
        assert_eq!(w.status()["state"], "failed");
        assert!(w.status()["message"].as_str().unwrap().contains("Steam networking is not available"));
    }

    #[test]
    fn copy_puts_the_hosts_join_code_on_the_clipboard_and_says_so() {
        let t = tempfile::tempdir().unwrap();
        let copied: Arc<Mutex<Vec<String>>> = Arc::default();
        let sink = Arc::clone(&copied);
        let mut c =
            Controller::new(t.path().to_path_buf(), "Pat".into(), Vec::new()).unwrap().with_clipboard(move |text| {
                sink.lock().unwrap().push(text.to_owned());
                true
            });
        let mut w = Window::new(t.path());

        // nothing to copy before hosting
        w.send("copy", "{}");
        let seq = w.seq - 1;
        assert!(until(&mut [&mut c], Duration::from_secs(5), || w.status()["result"]["seq"] == seq));
        assert_eq!(w.status()["result"]["ok"], false);
        assert!(copied.lock().unwrap().is_empty());

        w.send("host", "{[\"port\"]=0,[\"transport\"]=\"tcp\",[\"bind\"]=\"loopback\"}");
        assert!(until(&mut [&mut c], Duration::from_secs(10), || w.status()["state"] == "hosting"));
        let code = w.status()["code"].as_str().unwrap().to_owned();
        w.send("copy", "{}");
        let seq = w.seq - 1;
        assert!(until(&mut [&mut c], Duration::from_secs(5), || w.status()["result"]["seq"] == seq));
        let r = w.status()["result"].clone();
        assert_eq!((r["ok"].clone(), r["kind"].clone()), (json!(true), json!("copy")));
        assert_eq!(*copied.lock().unwrap(), vec![code]);
    }

    #[test]
    fn copy_reports_a_clipboard_that_cannot_be_opened() {
        let t = tempfile::tempdir().unwrap();
        let mut c =
            Controller::new(t.path().to_path_buf(), "Pat".into(), Vec::new()).unwrap().with_clipboard(|_| false);
        let mut w = Window::new(t.path());
        w.send("host", "{[\"port\"]=0,[\"transport\"]=\"tcp\",[\"bind\"]=\"loopback\"}");
        assert!(until(&mut [&mut c], Duration::from_secs(10), || w.status()["state"] == "hosting"));
        w.send("copy", "{}");
        let seq = w.seq - 1;
        assert!(until(&mut [&mut c], Duration::from_secs(5), || w.status()["result"]["seq"] == seq));
        let r = w.status()["result"].clone();
        assert_eq!(r["ok"], false);
        assert!(r["message"].as_str().unwrap().contains("clipboard"), "{r}");
    }

    #[test]
    fn a_bad_code_fails_with_a_message_and_the_window_can_try_again() {
        let t = tempfile::tempdir().unwrap();
        let mut c = Controller::new(t.path().to_path_buf(), "Pat".into(), Vec::new()).unwrap();
        let mut w = Window::new(t.path());
        w.send("join", "{[\"code\"]=\"NOT-A-CODE\"}");
        assert!(until(&mut [&mut c], Duration::from_secs(5), || w.status()["state"] == "failed"));
        assert!(w.status()["message"].as_str().unwrap().contains("not a valid join code"));
        w.send("host", "{[\"port\"]=0,[\"transport\"]=\"tcp\",[\"bind\"]=\"loopback\"}");
        assert!(until(&mut [&mut c], Duration::from_secs(10), || w.status()["state"] == "hosting"));
        // a second host request while hosting changes nothing
        let code = w.status()["code"].clone();
        w.send("host", "{[\"port\"]=0,[\"transport\"]=\"tcp\",[\"bind\"]=\"loopback\"}");
        c.step();
        assert_eq!(w.status()["code"], code);
    }

    #[test]
    fn a_controller_that_recovers_from_a_panic_has_stopped_its_session_and_can_host_again() {
        let t = tempfile::tempdir().unwrap();
        let mut c = Controller::new(t.path().to_path_buf(), "Pat".into(), Vec::new()).unwrap();
        let mut w = Window::new(t.path());
        w.send("host", "{[\"port\"]=0,[\"transport\"]=\"tcp\",[\"bind\"]=\"loopback\"}");
        assert!(until(&mut [&mut c], Duration::from_secs(10), || w.status()["state"] == "hosting"));
        c.recover("boom");
        c.step();
        let status = w.status();
        assert_eq!(status["state"], "failed");
        assert!(status["message"].as_str().unwrap().contains("boom"), "{status}");
        assert!(fs::read_to_string(t.path().join("identity.txt")).unwrap().contains("role=idle"));
        w.send("host", "{[\"port\"]=0,[\"transport\"]=\"tcp\",[\"bind\"]=\"loopback\"}");
        assert!(until(&mut [&mut c], Duration::from_secs(10), || w.status()["state"] == "hosting"));
    }

    #[test]
    fn the_steam_commands_say_so_when_the_game_has_no_steam() {
        let t = tempfile::tempdir().unwrap();
        let mut c = Controller::new(t.path().to_path_buf(), "Pat".into(), Vec::new()).unwrap();
        let mut w = Window::new(t.path());
        w.send("steamloop", "{}");
        assert!(until(&mut [&mut c], Duration::from_secs(5), || {
            w.status()["steam"]["note"].as_str().is_some_and(|n| n.contains("not available"))
        }));
        assert_eq!(w.status()["steam"]["state"], "looking for Steam...");
        // `loop` in the Steam ID field of the test starts the same check; a real ID is a normal test
        w.send("steamtest", "{[\"id\"]=\"76561198869158889\"}");
        w.send("steamtest", "{[\"id\"]=\" Loop \"}");
        c.step();
        assert_eq!(w.status()["steam"]["state"], "looking for Steam...");
    }

    #[test]
    fn status_version_only_moves_when_something_changed() {
        let t = tempfile::tempdir().unwrap();
        let mut c = Controller::new(t.path().to_path_buf(), "Pat".into(), Vec::new()).unwrap();
        let w = Window::new(t.path());
        let v1 = w.status()["version"].as_u64().unwrap();
        c.step();
        c.step();
        assert_eq!(w.status()["version"].as_u64().unwrap(), v1);
    }

    // ---------------------------------------------------------------- host controls

    /// A host controller that is hosting and a player controller that is idle, each with a folder of its own. The
    /// last value is the player's `join` payload, with the host's address replaced by loopback.
    fn start_pair(root: &Path, options: HostOptions) -> (Controller, Controller, Window, Window, String) {
        start_pair_saving(root, options, Vec::new(), Vec::new())
    }

    /// As [`start_pair`], with the folders the host's game saves to and the player's game loads from.
    fn start_pair_saving(
        root: &Path,
        options: HostOptions,
        host_saves: Vec<PathBuf>,
        player_saves: Vec<PathBuf>,
    ) -> (Controller, Controller, Window, Window, String) {
        let (hd, pd) = (root.join("host"), root.join("player"));
        let mut host = Controller::new(hd.clone(), "Host".into(), host_saves).unwrap().with_host_options(options);
        let player = Controller::new(pd.clone(), "Pat".into(), player_saves).unwrap();
        let (mut hw, pw) = (Window::new(&hd), Window::new(&pd));
        hw.send("host", "{[\"name\"]=\"Hosty\",[\"port\"]=0,[\"transport\"]=\"tcp\",[\"bind\"]=\"loopback\"}");
        assert!(until(&mut [&mut host], Duration::from_secs(10), || hw.status()["state"] == "hosting"));
        let code = hw.status()["code"].as_str().expect("a join code").to_owned();
        let mut decoded = JoinCode::try_decode(&code).expect("a decodable code");
        let port = decoded.endpoints.first().map_or(0, SocketAddr::port);
        decoded.endpoints = vec![SocketAddr::from(([127, 0, 0, 1], port))];
        (host, player, hw, pw, format!("{{[\"code\"]=\"{}\"}}", decoded.encode()))
    }

    fn quick() -> HostOptions {
        HostOptions { ping_seconds: 0.2, ..HostOptions::default() }
    }

    /// `{only = {}}` as the status file hands it back: Lua cannot tell an empty list from an empty object.
    fn only_nobody(v: &Value) -> bool {
        match &v["only"] {
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            _ => false,
        }
    }

    #[test]
    fn the_status_lists_every_player_with_role_state_and_round_trip_and_kick_removes_one() {
        let root = tempfile::tempdir().unwrap();
        let (mut host, mut player, mut hw, mut pw, join) = start_pair(root.path(), quick());
        pw.send("join", &join);
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(15), || pw.status()["state"] == "joined"));
        assert!(
            until(&mut [&mut host, &mut player], Duration::from_secs(10), || {
                let st = hw.status();
                st["players"].as_array().is_some_and(|p| p.len() == 2 && p[1]["rtt"].is_number())
            }),
            "{:?}",
            hw.status()
        );

        let st = hw.status();
        assert_eq!(st["capacity"], 8);
        let players = st["players"].as_array().unwrap();
        let field = |i: usize, key: &str| players[i][key].clone();
        assert_eq!((field(0, "id"), field(0, "name"), field(0, "role")), (json!(0), json!("Hosty"), json!("host")));
        assert_eq!((field(0, "state"), field(0, "rtt")), (json!("connected"), json!(0)));
        assert_eq!((field(1, "id"), field(1, "name"), field(1, "role")), (json!(1), json!("Pat"), json!("player")));
        assert_eq!(field(1, "state"), json!("connected"));
        assert!(field(1, "rtt").as_u64().is_some_and(|ms| ms < 100), "loopback is quick: {st}");

        // the player sees the same list, with the host's round-trip value for itself
        assert!(
            until(&mut [&mut host, &mut player], Duration::from_secs(10), || {
                let ps = pw.status();
                ps["players"].as_array().is_some_and(|p| p.len() == 2 && p.iter().all(|e| e["rtt"].is_number()))
            }),
            "{:?}",
            pw.status()
        );
        assert_eq!(pw.status()["players"][0]["role"], "host");

        // refusals say why and change nothing
        for (payload, wanted) in
            [("{[\"id\"]=0}", "cannot be removed"), ("{[\"id\"]=42}", "no player #42"), ("{}", "needs the id")]
        {
            hw.send("kick", payload);
            let seq = hw.seq - 1;
            assert!(
                until(&mut [&mut host, &mut player], Duration::from_secs(5), || hw.status()["result"]["seq"] == seq)
            );
            let r = hw.status()["result"].clone();
            assert_eq!((r["ok"].clone(), r["kind"].clone()), (json!(false), json!("kick")));
            assert!(r["message"].as_str().unwrap().contains(wanted), "{r}");
        }
        assert_eq!(hw.status()["players"].as_array().unwrap().len(), 2);

        // the real one
        hw.send("kick", "{[\"id\"]=1}");
        let seq = hw.seq - 1;
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(10), || {
            hw.status()["players"].as_array().is_some_and(|p| p.len() == 1) && pw.status()["state"] == "failed"
        }));
        let r = hw.status()["result"].clone();
        assert_eq!((r["ok"].clone(), r["seq"].clone()), (json!(true), json!(seq)));
        assert!(r["message"].as_str().unwrap().contains("Pat was removed"), "{r}");
        assert!(pw.status()["message"].as_str().unwrap().contains("removed you"), "{:?}", pw.status());
        assert_eq!(pw.status()["notice"], "The host removed you from the session");
        assert!(fs::read_to_string(root.path().join("player").join("identity.txt")).unwrap().contains("role=idle"));

        // a removed window is not stuck: it can join again, and gets a new id
        pw.send("join", &join);
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(15), || pw.status()["state"] == "joined"));
        assert_eq!(pw.status()["me"], 2);
    }

    #[test]
    fn only_a_host_can_kick_or_set_permissions() {
        let t = tempfile::tempdir().unwrap();
        let mut c = Controller::new(t.path().to_path_buf(), "Pat".into(), Vec::new()).unwrap();
        let mut w = Window::new(t.path());
        for (kind, payload) in [("kick", "{[\"id\"]=1}"), ("permissions", "{[\"build\"]=\"hostOnly\"}")] {
            w.send(kind, payload);
            let seq = w.seq - 1;
            assert!(until(&mut [&mut c], Duration::from_secs(5), || w.status()["result"]["seq"] == seq));
            let r = w.status()["result"].clone();
            assert_eq!((r["ok"].clone(), r["kind"].clone()), (json!(false), json!(kind)));
            assert!(r["message"].as_str().unwrap().contains("only the host"), "{r}");
        }
    }

    #[test]
    fn permissions_change_only_the_keys_named_and_reach_the_player_as_booleans() {
        let root = tempfile::tempdir().unwrap();
        let (mut host, mut player, mut hw, mut pw, join) = start_pair(root.path(), quick());
        pw.send("join", &join);
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(15), || pw.status()["state"] == "joined"));
        let everyone = json!({"build": "everyone", "bulldoze": "everyone", "spend": "everyone", "speed": "everyone", "other": "everyone"});
        assert_eq!(hw.status()["permissions"], everyone);

        hw.send("permissions", "{[\"build\"]=\"hostOnly\",[\"spend\"]={[\"only\"]={[1]=1,[2]=1}}}");
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(10), || hw.status()["permissions"]["build"]
            == "hostOnly"));
        let p = hw.status()["permissions"].clone();
        assert_eq!(p["spend"], json!({"only": [1]}), "ids are sorted and without repeats");
        assert_eq!(
            (p["bulldoze"].clone(), p["speed"].clone(), p["other"].clone()),
            (json!("everyone"), json!("everyone"), json!("everyone"))
        );
        assert_eq!(hw.status()["result"]["ok"], true);

        // another command names other keys: the first one's stay
        hw.send("permissions", "{[\"bulldoze\"]=\"hostOnly\",[\"other\"]={[\"only\"]={}},[\"speed\"]=\"hostOnly\"}");
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(10), || hw.status()["permissions"]
            ["bulldoze"]
            == "hostOnly"));
        let p = hw.status()["permissions"].clone();
        assert_eq!((p["build"].clone(), p["speed"].clone()), (json!("hostOnly"), json!("hostOnly")));
        assert!(only_nobody(&p["other"]), "{p}");

        // what the player's game was told: its own view of the sets
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(10), || only_nobody(
            &pw.status()["permissions"]["other"]
        )));
        let told = fs::read_to_string(root.path().join("player").join("to_game.log")).unwrap();
        let last = told.lines().rev().find(|l| l.contains("\tpermissions\t")).expect("a permissions line");
        for (key, allowed) in [("build", false), ("bulldoze", false), ("spend", true), ("other", false)] {
            assert!(last.contains(&format!("[\"{key}\"]={allowed}")), "{key}: {last}");
        }

        // refusals change nothing
        for (payload, wanted) in [
            ("{[\"build\"]=\"nobody\"}", "build"),
            ("{}", "at least one"),
            ("{[\"spend\"]={[\"only\"]=\"x\"}}", "spend"),
        ] {
            hw.send("permissions", payload);
            let seq = hw.seq - 1;
            assert!(
                until(&mut [&mut host, &mut player], Duration::from_secs(5), || hw.status()["result"]["seq"] == seq)
            );
            let r = hw.status()["result"].clone();
            assert_eq!(r["ok"], false);
            assert!(r["message"].as_str().unwrap().contains(wanted), "{r}");
        }
        assert_eq!(hw.status()["permissions"]["spend"], json!({"only": [1]}));
    }

    #[test]
    fn leaving_frees_the_slot_at_once_and_the_host_lists_no_ghost() {
        let root = tempfile::tempdir().unwrap();
        let (mut host, mut player, hw, mut pw, join) = start_pair(root.path(), quick());
        pw.send("join", &join);
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(15), || pw.status()["state"] == "joined"));
        // the player says goodbye on its way out; the host frees the slot at once and lists nobody else
        pw.send("leave", "{}");
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(5), || pw.status()["state"] == "idle"));
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(10), || hw.status()["players"]
            .as_array()
            .is_some_and(|p| p.len() == 1)));
        assert!(
            !hw.status()["players"].as_array().unwrap().iter().any(|p| p["state"] == "away"),
            "a leaver is gone, not away"
        );
    }

    #[test]
    fn the_capacity_in_the_status_follows_the_host_options() {
        let root = tempfile::tempdir().unwrap();
        let (_host, _player, hw, _pw, _join) = start_pair(root.path(), HostOptions { max_players: 3, ..quick() });
        assert_eq!(hw.status()["capacity"], 3);
    }

    // ---------------------------------------------------------------- the world is closed

    /// How long the games of these tests may be silent before their world counts as closed.
    const BEAT: Duration = Duration::from_millis(1000);
    /// One step of the heartbeat tests' own clock: every [`until_games`] / [`watch_games`] round is this long on the
    /// controllers' heartbeat clock and for the games, whatever the real time it takes. So "silent for `BEAT`" is a
    /// number of rounds, never a race against a busy machine.
    const TICK: Duration = Duration::from_millis(20);

    thread_local! {
        static TICKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        static ORIGIN: Instant = Instant::now();
    }

    /// The heartbeat clock of a test: its origin plus the rounds run so far on this thread.
    fn test_now() -> Instant {
        ORIGIN.with(|o| *o) + TICK * TICKS.with(|t| t.get())
    }

    fn tick_clock() {
        TICKS.with(|t| t.set(t.get() + 1));
    }
    const CLOCK: &str = "{[\"t\"]=18600,[\"speed\"]=0,[\"ahead\"]=3}";

    /// The mod, reduced to its pulse. While its world is open it appends a `clock` line to `from_game.log` about every
    /// 100 ms and answers a resync the way the real one does: the host's game saves, a player's game loads and is
    /// silent until it says hello, and every checksum is the same. A closed world reads and writes nothing.
    struct Game {
        dir: PathBuf,
        save_dir: PathBuf,
        is_host: bool,
        open: bool,
        seq: u64,
        to_game: Tail,
        /// The host's game takes a save request and never answers it: the round stays in `Saving`.
        hold_saves: bool,
        held_save: Option<i64>,
        /// How long a player's game is silent after it was told to load.
        load_silence: Duration,
        hello_at: Option<u32>,
        loads: u32,
        /// Rounds this game has run, and the round of its last `clock` line.
        round: u32,
        beat_round: u32,
    }

    impl Game {
        fn new(dir: &Path, is_host: bool) -> Game {
            Game {
                dir: dir.to_path_buf(),
                save_dir: dir.join("save"),
                is_host,
                open: true,
                seq: 1,
                to_game: Tail::new(dir.join(gamelink::TO_GAME)),
                hold_saves: false,
                held_save: None,
                load_silence: Duration::from_millis(500),
                hello_at: None,
                loads: 0,
                round: 0,
                beat_round: 0,
            }
        }

        fn say(&mut self, kind: &str, payload: &str) {
            let line = Line { seq: self.seq, kind: kind.into(), from: "0".into(), payload: payload.into() };
            self.seq += 1;
            let mut f = OpenOptions::new().create(true).append(true).open(self.dir.join(gamelink::FROM_GAME)).unwrap();
            f.write_all(gamelink::format_line(&line).as_bytes()).unwrap();
        }

        fn announce(&mut self) {
            self.say("hello", "{[\"time\"]=18600}");
            self.say("clock", CLOCK);
        }

        /// The save request the game took and did not answer is answered with a failure.
        fn fail_held_save(&mut self) {
            let id = self.held_save.take().expect("a save request is held");
            self.say(
                "save_done",
                &format!("{{[\"id\"]={id},[\"name\"]=\"x\",[\"ok\"]=false,[\"detail\"]=\"disk full\"}}"),
            );
        }

        fn step(&mut self) {
            if !self.open {
                return;
            }
            self.round += 1;
            for line in self.to_game.poll().unwrap_or_default() {
                match line.kind.as_str() {
                    "resync_save" if self.is_host => {
                        let name = text_after(&line.payload, "[\"name\"]=\"").unwrap();
                        let id = number_after(&line.payload, "[\"id\"]=").unwrap();
                        if self.hold_saves {
                            self.held_save = Some(id);
                        } else {
                            fs::create_dir_all(&self.save_dir).unwrap();
                            fs::write(self.save_dir.join(format!("{name}.sav")), b"a world").unwrap();
                            self.say("save_done", &format!("{{[\"id\"]={id},[\"name\"]=\"{name}\",[\"ok\"]=true}}"));
                        }
                    }
                    "resync_load" => {
                        self.loads += 1;
                        self.hello_at = Some(self.round + (self.load_silence.as_millis() / TICK.as_millis()) as u32);
                    }
                    "checksum_request" => {
                        let n = number_after(&line.payload, "[\"n\"]=").unwrap();
                        let at = number_after(&line.payload, "[\"at\"]=").unwrap();
                        let parts = "{[\"roads\"]=\"r1\",[\"money\"]=\"m1\"}";
                        self.say("checksum_report", &format!("{{[\"n\"]={n},[\"time\"]={at},[\"parts\"]={parts}}}"));
                    }
                    _ => {}
                }
            }
            if let Some(at) = self.hello_at {
                if self.round < at {
                    return; // loading: the world is not there
                }
                self.hello_at = None;
                self.announce();
            }
            if self.round - self.beat_round >= (Duration::from_millis(100).as_millis() / TICK.as_millis()) as u32 {
                self.beat_round = self.round;
                self.say("clock", CLOCK);
            }
        }
    }

    fn number_after(payload: &str, key: &str) -> Option<i64> {
        let rest = &payload[payload.find(key)? + key.len()..];
        let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '-')).unwrap_or(rest.len());
        rest[..end].parse().ok()
    }

    fn text_after(payload: &str, key: &str) -> Option<String> {
        let rest = &payload[payload.find(key)? + key.len()..];
        Some(rest[..rest.find('"')?].to_owned())
    }

    /// As [`until`], with the games running too.
    fn until_games(
        controllers: &mut [&mut Controller],
        games: &mut [&mut Game],
        timeout: Duration,
        mut done: impl FnMut() -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if round(controllers, games, &mut done) {
                return true;
            }
        }
        done()
    }

    /// One round: the games, then the controllers, one [`TICK`] later on the heartbeat clock.
    fn round(controllers: &mut [&mut Controller], games: &mut [&mut Game], done: &mut impl FnMut() -> bool) -> bool {
        tick_clock();
        for g in games.iter_mut() {
            g.step();
        }
        for c in controllers.iter_mut() {
            c.step();
        }
        let finished = done();
        if !finished {
            thread::sleep(Duration::from_millis(20));
        }
        finished
    }

    /// Runs the rounds of `span` on the heartbeat clock, however long that takes in real time. True when `done` held
    /// after any of them (so a negative check "nothing happened in a `span`" cannot be cut short by a busy machine).
    fn watch_games(
        controllers: &mut [&mut Controller],
        games: &mut [&mut Game],
        span: Duration,
        mut done: impl FnMut() -> bool,
    ) -> bool {
        let mut seen = false;
        for _ in 0..(span.as_millis() / TICK.as_millis()) {
            seen |= round(controllers, games, &mut done);
        }
        seen
    }

    fn loopback_host_payload() -> &'static str {
        "{[\"name\"]=\"Hosty\",[\"port\"]=0,[\"transport\"]=\"tcp\",[\"bind\"]=\"loopback\"}"
    }

    fn identity_of(dir: &Path) -> String {
        fs::read_to_string(dir.join("identity.txt")).unwrap()
    }

    #[test]
    fn a_host_whose_world_is_closed_stops_hosting_and_its_player_is_told_the_host_ended_it() {
        let root = tempfile::tempdir().unwrap();
        let (host, player, mut hw, mut pw, join) = start_pair(root.path(), quick());
        let (mut host, mut player) = (host.with_heartbeat(BEAT).with_clock(test_now), player.with_heartbeat(BEAT));
        let (mut hg, mut pg) =
            (Game::new(&root.path().join("host"), true), Game::new(&root.path().join("player"), false));
        pw.send("join", &join);
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(15), || pw.status()["state"] == "joined"));
        let alive = |hw: &Window, pw: &Window| hw.status()["state"] == "hosting" && pw.status()["state"] == "joined";

        // no line from either game yet: the worlds may still be loading, and nothing ends however long that takes
        assert!(
            !watch_games(&mut [&mut host, &mut player], &mut [], 2 * BEAT, || !alive(&hw, &pw)),
            "the session ended before its games had said anything"
        );
        // both games talk: still nothing ends
        assert!(
            !watch_games(&mut [&mut host, &mut player], &mut [&mut hg, &mut pg], 2 * BEAT, || !alive(&hw, &pw)),
            "{:?} {:?}",
            hw.status(),
            pw.status()
        );

        // the host closes its world: it stops hosting, and the player (whose game goes on) is told why
        hg.open = false;
        assert!(
            until_games(&mut [&mut host, &mut player], &mut [&mut pg], Duration::from_secs(10), || {
                hw.status()["state"] == "idle" && pw.status()["state"] == "failed"
            }),
            "host {:?} player {:?}",
            hw.status(),
            pw.status()
        );
        assert_eq!(hw.status()["notice"], WORLD_CLOSED_NOTICE);
        assert!(identity_of(&root.path().join("host")).contains("role=idle"));
        let ps = pw.status();
        assert_eq!(ps["message"], "the host ended the session", "{ps}");
        assert_eq!(ps["notice"], "The host ended the session", "{ps}");
        assert!(identity_of(&root.path().join("player")).contains("role=idle"));

        // the window can host again at once; the old notice is gone, and the lines the game wrote before are no
        // sign of life: nothing ends before the new session's game has said something
        hw.send("host", loopback_host_payload());
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(10), || hw.status()["state"] == "hosting"));
        assert!(hw.status()["notice"].is_null(), "{:?}", hw.status());
        assert!(
            !watch_games(&mut [&mut host, &mut player], &mut [], 2 * BEAT, || hw.status()["state"] != "hosting"),
            "an earlier session's lines ended the new one"
        );
    }

    #[test]
    fn a_player_whose_world_is_closed_leaves_and_the_host_goes_on() {
        let root = tempfile::tempdir().unwrap();
        let (host, player, hw, mut pw, join) = start_pair(root.path(), quick());
        let (mut host, mut player) =
            (host.with_heartbeat(BEAT).with_clock(test_now), player.with_heartbeat(BEAT).with_clock(test_now));
        let (mut hg, mut pg) =
            (Game::new(&root.path().join("host"), true), Game::new(&root.path().join("player"), false));
        pw.send("join", &join);
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(15), || pw.status()["state"] == "joined"));
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(10), || {
            hw.status()["players"].as_array().is_some_and(|p| p.len() == 2)
        }));
        assert!(!watch_games(&mut [&mut host, &mut player], &mut [&mut hg, &mut pg], 2 * BEAT, || {
            pw.status()["state"] != "joined"
        }));

        pg.open = false;
        assert!(
            until_games(&mut [&mut host, &mut player], &mut [&mut hg], Duration::from_secs(10), || {
                pw.status()["state"] == "idle"
            }),
            "{:?}",
            pw.status()
        );
        assert_eq!(pw.status()["notice"], WORLD_CLOSED_NOTICE);
        assert!(identity_of(&root.path().join("player")).contains("role=idle"));
        // the host frees the slot and keeps hosting: its own game is fine
        assert!(until_games(&mut [&mut host, &mut player], &mut [&mut hg], Duration::from_secs(10), || {
            hw.status()["players"].as_array().is_some_and(|p| p.len() == 1)
        }));
        assert!(!watch_games(&mut [&mut host, &mut player], &mut [&mut hg], 2 * BEAT, || {
            hw.status()["state"] != "hosting"
        }));
        assert!(hw.status()["notice"].is_null());
    }

    #[test]
    fn a_host_in_a_resync_round_is_not_taken_for_a_closed_world_until_the_round_is_over() {
        let t = tempfile::tempdir().unwrap();
        let mut host = Controller::new(t.path().to_path_buf(), "Host".into(), vec![t.path().join("save")])
            .unwrap()
            .with_heartbeat(BEAT)
            .with_clock(test_now);
        let mut hw = Window::new(t.path());
        hw.send("host", loopback_host_payload());
        assert!(until(&mut [&mut host], Duration::from_secs(10), || hw.status()["state"] == "hosting"));
        let mut hg = Game::new(t.path(), true);
        hg.hold_saves = true;
        hg.announce();
        assert!(until_games(&mut [&mut host], &mut [&mut hg], Duration::from_secs(10), || {
            hw.status()["players"][0]["ready"] == true
        }));
        hw.send("start", "{}");
        hw.send("resync", "{}");
        assert!(
            until_games(&mut [&mut host], &mut [&mut hg], Duration::from_secs(10), || hw.status()["resync"]
                == "Saving"),
            "{:?}",
            hw.status()
        );
        watch_games(&mut [&mut host], &mut [&mut hg], Duration::from_millis(300), || false);
        assert!(hg.held_save.is_some(), "the game took the save request");

        // the game says nothing for several times the limit (a big save, a world being replaced): the round excuses it
        hg.open = false;
        assert!(
            !watch_games(&mut [&mut host], &mut [], 2 * BEAT, || hw.status()["state"] != "hosting"),
            "ended during a round: {:?}",
            hw.status()
        );
        assert_eq!(hw.status()["resync"], "Saving");

        // the round fails: from then on the silence counts, from the end of the round
        hg.fail_held_save();
        assert!(until_games(&mut [&mut host], &mut [], Duration::from_secs(5), || {
            hw.status()["resync"].as_str().is_some_and(|r| r.starts_with("failed"))
        }));
        assert!(
            until_games(&mut [&mut host], &mut [], Duration::from_secs(10), || hw.status()["state"] == "idle"),
            "{:?}",
            hw.status()
        );
        assert_eq!(hw.status()["notice"], WORLD_CLOSED_NOTICE);
    }

    #[test]
    fn a_player_whose_game_reloads_for_a_resync_stays_in_the_session_and_leaves_when_its_world_is_closed() {
        let root = tempfile::tempdir().unwrap();
        let saves = (root.path().join("host-user").join("save"), root.path().join("player-user").join("save"));
        let (host, player, mut hw, mut pw, join) =
            start_pair_saving(root.path(), quick(), vec![saves.0.clone()], vec![saves.1.clone()]);
        let (mut host, mut player) =
            (host.with_heartbeat(BEAT).with_clock(test_now), player.with_heartbeat(BEAT).with_clock(test_now));
        let mut hg = Game::new(&root.path().join("host"), true);
        hg.save_dir = saves.0.clone();
        let mut pg = Game::new(&root.path().join("player"), false);
        // the load takes several times the limit, as a real one takes half a minute
        pg.load_silence = 3 * BEAT;
        pw.send("join", &join);
        assert!(until(&mut [&mut host, &mut player], Duration::from_secs(15), || pw.status()["state"] == "joined"));
        hg.announce();
        pg.announce();
        assert!(until_games(&mut [&mut host, &mut player], &mut [&mut hg, &mut pg], Duration::from_secs(10), || {
            hw.status()["players"].as_array().is_some_and(|p| p.len() == 2 && p.iter().all(|e| e["ready"] == true))
        }));
        watch_games(&mut [&mut host, &mut player], &mut [&mut hg, &mut pg], Duration::from_millis(500), || false);
        hw.send("start", "{}");
        assert!(until_games(&mut [&mut host, &mut player], &mut [&mut hg, &mut pg], Duration::from_secs(10), || {
            hw.status()["started"] == true
        }));

        hw.send("resync", "{}");
        let mut player_left = false;
        let done = until_games(&mut [&mut host, &mut player], &mut [&mut hg, &mut pg], Duration::from_secs(60), || {
            player_left |= pw.status()["state"] != "joined";
            hw.status()["resync"] == "Idle"
        });
        assert!(done, "the round did not end: {:?} / {:?}", hw.status(), pw.status());
        assert_eq!(pg.loads, 1, "the player's game was told to load the host's save");
        assert!(!player_left, "the player's silence while loading ended its session");
        assert_eq!(hw.status()["state"], "hosting");

        // afterwards the pulse counts again: a player whose world is closed now leaves
        pg.open = false;
        assert!(
            until_games(&mut [&mut host, &mut player], &mut [&mut hg], Duration::from_secs(10), || {
                pw.status()["state"] == "idle"
            }),
            "{:?}",
            pw.status()
        );
        assert_eq!(pw.status()["notice"], WORLD_CLOSED_NOTICE);
        assert_eq!(hw.status()["state"], "hosting");
    }

    // ---------------------------------------------------------------- the pieces

    #[test]
    fn the_heartbeat_setting_reads_the_environment_value() {
        let default = Some(Duration::from_secs_f64(HEARTBEAT_SECONDS));
        assert_eq!(heartbeat_setting(None), default);
        assert_eq!(heartbeat_setting(Some("")), default);
        assert_eq!(heartbeat_setting(Some("soon")), default);
        assert_eq!(heartbeat_setting(Some("nan")), default);
        assert_eq!(heartbeat_setting(Some(" 2.5 ")), Some(Duration::from_millis(2500)));
        assert_eq!(heartbeat_setting(Some("0")), None, "zero turns the check off");
        assert_eq!(heartbeat_setting(Some("-3")), None);
        assert_eq!(heartbeat_setting(Some("1e99")), Some(Duration::from_secs(3600)), "capped, never a panic");
    }

    #[test]
    fn the_pulse_judges_only_after_the_first_line_and_excuses_a_reload() {
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join(gamelink::FROM_GAME);
        let limit = Duration::from_secs(8);
        let append = |text: &str| {
            let mut f = OpenOptions::new().create(true).append(true).open(&log).unwrap();
            f.write_all(text.as_bytes()).unwrap();
        };
        // lines of an earlier session are the baseline, not a sign of life
        append("1\tclock\t0\t{}\n");
        let t0 = Instant::now();
        let mut pulse = Pulse::start(log.clone(), t0);
        assert!(!pulse.silent(limit, false, t0 + Duration::from_secs(3600)), "no verdict before the first line");

        append("2\tclock\t0\t{}\n");
        assert!(!pulse.silent(limit, false, t0 + Duration::from_secs(100)), "a line just came");
        assert!(!pulse.silent(limit, false, t0 + Duration::from_secs(107)));
        assert!(pulse.silent(limit, false, t0 + Duration::from_secs(108)), "eight seconds without a line");

        // a reload (a round) excuses the silence, however long it is within the grace, and the count starts again after
        let mut pulse = Pulse::start(log.clone(), t0);
        append("3\tclock\t0\t{}\n");
        assert!(!pulse.silent(limit, false, t0));
        assert!(!pulse.silent(limit, true, t0 + Duration::from_secs(60)));
        assert!(!pulse.silent(limit, true, t0 + Duration::from_secs(200)));
        assert!(!pulse.silent(limit, false, t0 + Duration::from_secs(205)), "the count starts when the round ends");
        assert!(pulse.silent(limit, false, t0 + Duration::from_secs(213)));

        // a round that never ends does not keep a closed world alive for ever
        let mut pulse = Pulse::start(log.clone(), t0);
        append("4\tclock\t0\t{}\n");
        assert!(!pulse.silent(limit, true, t0));
        assert!(!pulse.silent(limit, true, t0 + RELOAD_GRACE - Duration::from_secs(1)));
        assert!(pulse.silent(limit, true, t0 + RELOAD_GRACE + limit));
    }

    #[test]
    fn the_game_is_reloading_in_a_round_or_between_a_load_and_its_hello() {
        use crate::process::Role;
        let mut status = Status::new(Role::Host, "H");
        assert!(!game_is_reloading(&status));
        for round in ["Pausing", "Saving", "Transferring", "Loading", "Verifying"] {
            status.resync = Some(round.to_owned());
            assert!(game_is_reloading(&status), "{round}");
        }
        for over in ["Idle", "failed: the host's game could not save: disk full"] {
            status.resync = Some(over.to_owned());
            assert!(!game_is_reloading(&status), "{over}");
        }
        let mut player = Status::new(Role::Player, "P");
        assert!(!game_is_reloading(&player));
        player.reloading = true;
        assert!(game_is_reloading(&player));
    }

    #[test]
    fn messages_become_sentences() {
        assert_eq!(sentence("the host ended the session"), "The host ended the session");
        assert_eq!(sentence("Already one"), "Already one");
        assert_eq!(sentence(""), "");
        assert_eq!(sentence("é"), "É");
    }

    #[test]
    fn a_player_entry_has_the_fields_the_window_reads() {
        let away = RosterEntry { id: 2, name: "Bob".into(), ready: false, state: ConnectionState::Away, rtt_ms: None };
        assert_eq!(
            player_json(&away),
            json!({"id": 2, "name": "Bob", "role": "player", "state": "away", "ready": false, "rtt": null})
        );
        let host =
            RosterEntry { id: 0, name: "H".into(), ready: true, state: ConnectionState::Connected, rtt_ms: Some(0.0) };
        assert_eq!(player_json(&host)["role"], "host");
        assert_eq!(player_json(&host)["rtt"], 0);
        // the Lua side never sees a null
        let lua = luatext::json_to_lua(&serde_json::to_vec(&player_json(&away)).unwrap()).unwrap();
        assert!(!lua.contains("rtt") && lua.contains("[\"state\"]=\"away\""), "{lua}");
    }

    #[test]
    fn round_trip_times_are_rounded_so_a_wobble_does_not_rewrite_the_status() {
        let shown: Vec<u32> = [0.0, 0.4, 2.6, 7.4, 19.9, 23.0, 26.0, 99.0, 104.0, 112.4, 180.0, 1234.0]
            .into_iter()
            .map(rtt_shown)
            .collect();
        assert_eq!(vec![0, 0, 5, 5, 20, 20, 30, 100, 100, 100, 175, 1225], shown);
        assert_eq!(rtt_shown(-3.0), 0);
    }

    #[test]
    fn permission_values_come_in_three_shapes() {
        assert_eq!(player_set(&json!("everyone")), Ok(PlayerSet::Everyone));
        assert_eq!(player_set(&json!("hostOnly")), Ok(PlayerSet::HostOnly));
        assert_eq!(player_set(&json!("HOSTONLY")), Ok(PlayerSet::HostOnly));
        assert_eq!(player_set(&json!({"only": [3, 1, 3]})), Ok(PlayerSet::Only(vec![1, 3])));
        assert_eq!(
            player_set(&json!({"only": [2.0]})),
            Ok(PlayerSet::Only(vec![2])),
            "a Lua number may be written 2.0"
        );
        assert_eq!(player_set(&json!({"only": []})), Ok(PlayerSet::Only(vec![])));
        assert_eq!(
            player_set(&json!({"only": {}})),
            Ok(PlayerSet::Only(vec![])),
            "an empty Lua table is an object here"
        );
        for bad in [
            json!("nobody"),
            json!(7),
            json!(true),
            json!({}),
            json!({"only": "x"}),
            json!({"only": [-1]}),
            json!({"only": [70000]}),
            json!({"only": ["a"]}),
            json!({"only": [1.5]}),
        ] {
            assert!(player_set(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn permissions_with_replaces_only_what_is_named() {
        let current = Permissions { build: PlayerSet::HostOnly, ..Permissions::default() };
        let next = permissions_with(&current, &json!({"spend": "hostOnly", "extra": 1})).unwrap();
        assert_eq!(
            next,
            Permissions { build: PlayerSet::HostOnly, spend: PlayerSet::HostOnly, ..Permissions::default() }
        );
        let reset = permissions_with(&current, &json!({"build": "everyone"})).unwrap();
        assert_eq!(reset, Permissions::default());
        assert!(permissions_with(&current, &json!({})).is_err());
        assert!(permissions_with(&current, &Value::Null).is_err());
        assert!(permissions_with(&current, &json!({"build": null})).is_err(), "nil names nothing");
        let err = permissions_with(&current, &json!({"speed": "everyone", "bulldoze": "maybe"})).unwrap_err();
        assert!(err.starts_with("bulldoze:"), "{err}");
    }

    #[test]
    fn player_ids_from_commands() {
        assert_eq!(player_id(&json!(3)), Some(3));
        assert_eq!(player_id(&json!(3.0)), Some(3));
        assert_eq!(player_id(&json!(65535)), Some(65535));
        for bad in [json!(65536), json!(-1), json!(1.5), json!("1"), Value::Null] {
            assert_eq!(player_id(&bad), None, "{bad}");
        }
    }
}
