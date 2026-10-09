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
use std::thread;
use std::time::Duration;

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::lobby::JoinCode;
use feverlink3_core::session::HostOptions;
use serde_json::{json, Value};

use crate::gamelink::{self, Tail};
use crate::session::{self, Command, HostConfig, PlayerConfig, SessionHandle};
use crate::{luatext, net, resync, steam, steamnet};

/// The window's commands, appended by the mod.
pub const CMD_FILE: &str = "ctl_cmd.log";
/// The controller's state, rewritten by the controller.
pub const STATUS_FILE: &str = "ctl_status.txt";
/// The port a host listens on unless the window asks for another.
pub const DEFAULT_PORT: u16 = 28090;

/// The folder the mod looks in when the launcher did not hand it a session folder.
pub fn live_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("FeverLink3").join("live"))
}

enum State {
    Idle,
    Hosting(Box<Hosting>),
    Joining { rx: Receiver<Result<SessionHandle, String>> },
    Joined { handle: SessionHandle },
    Failed(String),
}

struct Hosting {
    handle: SessionHandle,
    secret: [u8; SessionKey::SECRET_SIZE],
    lan: Vec<SocketAddr>,
    code: String,
    endpoints: Vec<String>,
    password: bool,
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

/// The controller. [`Controller::step`] is called every ~100 ms by [`spawn`], or by a test.
pub struct Controller {
    dir: PathBuf,
    default_name: String,
    save_dirs: Vec<PathBuf>,
    commands: Tail,
    state: State,
    steam: Option<steamnet::Probe>,
    steam_note: Option<String>,
    published: String,
    version: u64,
}

impl Controller {
    /// Prepares the folder (files of an earlier run are removed), says the game is idle, starts listening for
    /// commands.
    pub fn new(dir: PathBuf, default_name: String, save_dirs: Vec<PathBuf>) -> io::Result<Controller> {
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
            steam: steamnet::Probe::load(),
            steam_note: None,
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
                let id = params.get("id").and_then(Value::as_str).and_then(steamnet::parse_steam_id);
                match (&mut self.steam, id) {
                    (Some(probe), Some(id)) => probe.test(id),
                    (Some(_), None) => self.steam_note = Some("that is not a Steam ID (17 digits)".into()),
                    (None, _) => self.steam_note = Some("Steam networking is not available in this game".into()),
                }
            }
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
        // a relay as the fallback). Until that transport is wired in, the only one is the direct TCP transport, which
        // is for development and tests and must be asked for by name.
        let transport = params.get("transport").and_then(Value::as_str).unwrap_or("auto");
        if transport != "tcp" {
            self.state = State::Failed(
                "Hosting over Steam is not finished in this build yet; the Steam connection test below works".into(),
            );
            return;
        }
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
        // run_player connects before it returns (5 s per endpoint at most): off the controller's thread
        thread::spawn(move || {
            let result = gamelink::Session::open(&dir)
                .map_err(|e| format!("cannot open the session folder: {e}"))
                .and_then(|link| {
                    let mut config = PlayerConfig::new(code.endpoints.clone(), &name, key, link);
                    config.save_dirs = save_dirs;
                    session::run_player(config).map_err(|e| format!("cannot reach the host: {e}"))
                });
            let _ = tx.send(result);
        });
        self.state = State::Joining { rx };
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
        let State::Joining { rx } = &self.state else { return };
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
            State::Joining { .. } => v["state"] = json!("joining"),
            State::Hosting(h) => {
                v["state"] = json!("hosting");
                v["code"] = json!(h.code);
                v["endpoints"] = json!(h.endpoints);
                v["password"] = json!(h.password);
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
            return json!({ "state": "not available", "note": self.steam_note });
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
            });
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
