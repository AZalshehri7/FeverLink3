//! Live mode: the session controller that runs inside the game process, driven from the in-game window.
//!
//! The native module (`winhttp.dll`) starts [`spawn`] on a thread when the game was not started by the launcher
//! executable. Everything the launcher binary does for `host` and `join` happens here instead, on request of the
//! mod's window, so a player starts the game from Steam as usual and never sees a terminal:
//!
//! - the folder is fixed, `%LOCALAPPDATA%\FeverLink3\live`, so the mod finds it without an environment variable;
//! - the window appends commands to `ctl_cmd.log` (`host`, `join`, `leave`, `start`, `pause`, `speed`, `check`,
//!   `resync`; the payload is a Lua literal table, the line format is the file link's);
//! - the controller answers by rewriting `ctl_status.txt` (one Lua literal table) whenever something changed;
//! - the session itself is the same [`run_host`] / [`run_player`] the launcher uses, linked to the game through the
//!   same `to_game.log` / `from_game.log`, and `identity.txt` says who the game is (role `idle` until a session
//!   exists, then `host` or `player` with the id the host assigned).
//!
//! Everything is a function of the folder's files, so the tests drive it with files and need no game.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::lobby::JoinCode;
use feverlink3_core::session::HostOptions;
use serde_json::{json, Value};

use crate::gamelink::{self, Tail};
use crate::session::{self, Command, HostConfig, PlayerConfig, SessionHandle};
use crate::steam_transport::SteamEndpoint;
use crate::steamnet::CoreRef;
use crate::{luatext, net, resync, steam, steam_transport, steamnet};

/// The window's commands, appended by the mod.
pub const CMD_FILE: &str = "ctl_cmd.log";
/// The controller's state, rewritten by the controller.
pub const STATUS_FILE: &str = "ctl_status.txt";
/// The Steam API is looked up again every couple of seconds (for about five minutes) when it was not there yet.
const STEAM_LOAD_EVERY: Duration = Duration::from_secs(2);
const STEAM_LOAD_ATTEMPTS: u32 = 150;

/// The port a host listens on unless the window asks for another.
pub const DEFAULT_PORT: u16 = 28090;

/// The folder the mod looks in when the launcher did not hand it a session folder.
pub fn live_dir() -> Option<PathBuf> {
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

/// The controller. [`Controller::step`] is called every ~100 ms by [`spawn`], or by a test.
pub struct Controller {
    dir: PathBuf,
    default_name: String,
    save_dirs: Vec<PathBuf>,
    commands: Tail,
    state: State,
    steam: Option<steamnet::Probe>,
    /// The Steam core to use instead of the process's (the tests give it a fake Steam).
    core: Option<CoreRef>,
    /// How often and when the Steam API was looked for: it may not be loaded yet when the controller starts.
    steam_attempts: u32,
    steam_last_try: Instant,
    steam_note: Option<String>,
    steam_loop: Arc<Mutex<LoopCheck>>,
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
            commands,
            state: State::Idle,
            steam: core.clone().map(steamnet::Probe::on).or_else(steamnet::Probe::load),
            core,
            steam_attempts: 1,
            steam_last_try: Instant::now(),
            steam_note: None,
            steam_loop: Arc::default(),
            published: String::new(),
            version: 0,
        };
        c.publish();
        Ok(c)
    }

    /// Reads new commands, advances the state, rewrites the status file when it changed.
    pub fn step(&mut self) {
        let lines = self.commands.poll().unwrap_or_default();
        for line in lines {
            let params = luatext::lua_to_json(&line.payload)
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .unwrap_or(Value::Null);
            self.command(&line.kind, &params);
        }
        self.advance();
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
        self.publish();
    }

    fn command(&mut self, kind: &str, params: &Value) {
        match kind {
            "host" => self.host(params),
            "join" => self.join(params),
            "leave" => self.leave(),
            "steamtest" => {
                let text = params.get("id").and_then(Value::as_str).unwrap_or("").trim();
                // the window has no button for the transport self-check: typing `loop` where the Steam ID goes starts it
                if text.eq_ignore_ascii_case("loop") {
                    return self.steam_loop();
                }
                let id = steamnet::parse_steam_id(text);
                match (&mut self.steam, id) {
                    (Some(probe), Some(id)) => probe.test(id),
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
            let checks = steam_transport::loopback_check(&core);
            *shared.lock().unwrap_or_else(|p| p.into_inner()) = LoopCheck { running: false, checks };
        });
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
        // Players never open ports or touch their router: sessions travel over Steam's peer-to-peer networking (and
        // a relay as the fallback). The direct TCP transport is for development and tests and must be asked for by
        // name.
        match params.get("transport").and_then(Value::as_str).unwrap_or("auto") {
            "tcp" => self.host_tcp(params),
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
        let secret = SessionKey::new_secret();
        let key = SessionKey::derive(&secret, password.as_deref());
        let link = match gamelink::Session::open(&self.dir) {
            Ok(l) => l,
            Err(e) => return self.state = State::Failed(format!("cannot open the session folder: {e}")),
        };
        let config = HostConfig {
            bind: SocketAddr::from(([0, 0, 0, 0], 0)), // not used by Steam
            name,
            key,
            link,
            options: HostOptions::default(),
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
        self.state = State::Hosting(Box::new(Hosting {
            handle,
            secret,
            lan: Vec::new(),
            code,
            endpoints: Vec::new(),
            password: password.is_some(),
            steam_id: Some(steam_id),
        }));
    }

    /// Hosts over direct TCP (developer transport): the join code lists the machine's local addresses.
    fn host_tcp(&mut self, params: &Value) {
        let name = self.name_from(params);
        let port =
            params.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).unwrap_or(DEFAULT_PORT);
        let password = params.get("password").and_then(Value::as_str).filter(|p| !p.is_empty()).map(str::to_owned);
        let secret = SessionKey::new_secret();
        let key = SessionKey::derive(&secret, password.as_deref());
        let link = match gamelink::Session::open(&self.dir) {
            Ok(l) => l,
            Err(e) => return self.state = State::Failed(format!("cannot open the session folder: {e}")),
        };
        let started = session::run_host(HostConfig {
            bind: SocketAddr::from(([0, 0, 0, 0], port)),
            name,
            key,
            link,
            options: HostOptions::default(),
            save_dirs: self.save_dirs.clone(),
            echo_log: false,
        });
        let handle = match started {
            Ok(h) => h,
            Err(e) => return self.state = State::Failed(format!("cannot listen on port {port}: {e}")),
        };
        let bound = handle.local_addr().map_or(port, |a| a.port());
        let mut hosting = Hosting {
            handle,
            secret,
            lan: net::lan_endpoints(bound),
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
        thread::spawn(move || {
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
        });
        self.state = State::Joining { rx, progress };
    }

    fn leave(&mut self) {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::Hosting(h) => h.handle.stop(),
            State::Joined { handle } => handle.stop(),
            _ => {}
        }
        if let Ok(link) = gamelink::Session::open(&self.dir) {
            let _ = link.write_identity(0, &self.default_name, "idle");
        }
    }

    fn advance(&mut self) {
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
                add_session(&mut v, &h.handle);
            }
            State::Joined { handle } => {
                let s = handle.status();
                let state = if let Some(why) = &s.rejected {
                    v["message"] = json!(format!("the host rejected us: {why}"));
                    "failed"
                } else if let Some(why) = &s.error {
                    v["message"] = json!(why);
                    "failed"
                } else if s.connected {
                    "joined"
                } else {
                    "joining"
                };
                v["state"] = json!(state);
                add_session(&mut v, handle);
            }
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
    v["players"] = json!(s.players.iter().map(|p| json!({ "id": p.id, "name": p.name })).collect::<Vec<_>>());
    if let Some(sess) = &s.session {
        v["started"] = json!(sess.started);
        v["speed"] = json!(sess.speed);
    }
    if let Some(r) = &s.resync {
        v["resync"] = json!(r);
    }
    // ping and route per connection, for transports that measure them (Steam)
    v["links"] = json!(s
        .links
        .iter()
        .map(|l| json!({ "conn": l.conn, "ping": l.ping_ms, "route": l.route }))
        .collect::<Vec<_>>());
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
pub fn default_name() -> String {
    std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "player".into())
}

/// Starts the controller on a thread of its own. `None` when there is no folder to use. Never returns an error to
/// the game: whatever goes wrong here must not stop it from starting.
pub fn spawn() -> Option<thread::JoinHandle<()>> {
    let dir = live_dir()?;
    thread::Builder::new()
        .name("feverlink3-live".into())
        .spawn(move || {
            let save_dirs = resync::save_dirs(&steam::discover().user_dirs);
            let Ok(mut controller) = Controller::new(dir, default_name(), save_dirs) else { return };
            loop {
                controller.step();
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

        hw.send("host", "{[\"name\"]=\"Hosty\",[\"port\"]=0,[\"transport\"]=\"tcp\"}");
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
    fn a_bad_code_fails_with_a_message_and_the_window_can_try_again() {
        let t = tempfile::tempdir().unwrap();
        let mut c = Controller::new(t.path().to_path_buf(), "Pat".into(), Vec::new()).unwrap();
        let mut w = Window::new(t.path());
        w.send("join", "{[\"code\"]=\"NOT-A-CODE\"}");
        assert!(until(&mut [&mut c], Duration::from_secs(5), || w.status()["state"] == "failed"));
        assert!(w.status()["message"].as_str().unwrap().contains("not a valid join code"));
        w.send("host", "{[\"port\"]=0,[\"transport\"]=\"tcp\"}");
        assert!(until(&mut [&mut c], Duration::from_secs(10), || w.status()["state"] == "hosting"));
        // a second host request while hosting changes nothing
        let code = w.status()["code"].clone();
        w.send("host", "{[\"port\"]=0,[\"transport\"]=\"tcp\"}");
        c.step();
        assert_eq!(w.status()["code"], code);
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
}
