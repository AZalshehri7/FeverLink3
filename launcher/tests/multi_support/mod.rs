//! Shared by the multi-player integration tests: a stand-in for the mod that talks through the real file link, a
//! forwarder that can cut the connections it carries, and the usual waiting. Nothing here starts the game.
#![allow(dead_code)]

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::session::HostOptions;
use feverlink3_launcher::gamelink::{self, Line, Tail};
use feverlink3_launcher::session::{run_host, run_player, HostConfig, PlayerConfig, SessionHandle};

/// The mod, reduced to what a session needs: it says hello with a clock, saves when the host asks, reloads (and says
/// hello again) when told to, and reports the same checksum as every other game.
pub struct Game {
    pub dir: PathBuf,
    pub save_dir: PathBuf,
    pub is_host: bool,
    save_bytes: Vec<u8>,
    next_seq: u64,
    tail: Tail,
    pub seen: Vec<Line>,
    handled: usize,
}

impl Game {
    pub fn new(dir: &Path, save_dir: &Path, is_host: bool, save_bytes: Vec<u8>) -> Game {
        Game {
            dir: dir.to_path_buf(),
            save_dir: save_dir.to_path_buf(),
            is_host,
            save_bytes,
            next_seq: 1,
            tail: Tail::new(dir.join(gamelink::TO_GAME)),
            seen: Vec::new(),
            handled: 0,
        }
    }

    pub fn write(&mut self, kind: &str, payload: &str) {
        let line = Line { seq: self.next_seq, kind: kind.into(), from: "0".into(), payload: payload.into() };
        self.next_seq += 1;
        let mut f = OpenOptions::new().create(true).append(true).open(self.dir.join(gamelink::FROM_GAME)).unwrap();
        f.write_all(gamelink::format_line(&line).as_bytes()).unwrap();
    }

    /// What the mod sends once it runs: hello, and a stopped clock.
    pub fn announce(&mut self) {
        self.write("hello", "{[\"time\"]=18600}");
        self.write("clock", "{[\"t\"]=18600,[\"speed\"]=0,[\"ahead\"]=3}");
    }

    /// A player action with a body of its own. The body is Lua text and reaches a game only if it is a literal (the
    /// launcher checks, `luatext::is_literal`), so the label is sent as a Lua string.
    pub fn act(&mut self, body: &str, seq: i64) {
        self.write(
            "action",
            &format!("{{[\"body\"]=\"\\\"{body}\\\"\",[\"seq\"]={seq},[\"stamp\"]={}}}", 30_000 + seq * 200),
        );
    }

    /// Reads new lines and reacts like the mod would.
    pub fn step(&mut self) {
        if let Ok(lines) = self.tail.poll() {
            self.seen.extend(lines);
        }
        while self.handled < self.seen.len() {
            let line = self.seen[self.handled].clone();
            self.handled += 1;
            match line.kind.as_str() {
                "resync_save" if self.is_host => {
                    let name = text_after(&line.payload, "[\"name\"]=\"").unwrap();
                    let id = number_after(&line.payload, "[\"id\"]=").unwrap();
                    fs::create_dir_all(&self.save_dir).unwrap();
                    fs::write(self.save_dir.join(format!("{name}.sav")), &self.save_bytes).unwrap();
                    self.write("save_done", &format!("{{[\"id\"]={id},[\"name\"]=\"{name}\",[\"ok\"]=true}}"));
                }
                "resync_load" => self.announce(),
                "checksum_request" => {
                    let n = number_after(&line.payload, "[\"n\"]=").unwrap();
                    let at = number_after(&line.payload, "[\"at\"]=").unwrap();
                    let parts = "{[\"roads\"]=\"r1\",[\"money\"]=\"m1\"}";
                    self.write("checksum_report", &format!("{{[\"n\"]={n},[\"time\"]={at},[\"parts\"]={parts}}}"));
                }
                _ => {}
            }
        }
    }

    pub fn saw(&self, kind: &str, needle: &str) -> bool {
        self.seen.iter().any(|l| l.kind == kind && l.payload.contains(needle))
    }

    pub fn lines(&self, kind: &str) -> Vec<&Line> {
        self.seen.iter().filter(|l| l.kind == kind).collect()
    }
}

pub fn number_after(payload: &str, key: &str) -> Option<i64> {
    let rest = &payload[payload.find(key)? + key.len()..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '-')).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

pub fn text_after(payload: &str, key: &str) -> Option<String> {
    let rest = &payload[payload.find(key)? + key.len()..];
    Some(rest[..rest.find('"')?].to_owned())
}

/// Polls `done` every 10 ms until it holds or `timeout` passes.
pub fn wait(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    done()
}

/// Steps every game while waiting.
pub fn wait_games(timeout: Duration, games: &mut [&mut Game], mut done: impl FnMut(&[&mut Game]) -> bool) -> bool {
    wait(timeout, || {
        for g in games.iter_mut() {
            g.step();
        }
        done(games)
    })
}

pub fn key() -> SessionKey {
    SessionKey::derive(&SessionKey::new_secret(), None)
}

pub fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

/// A host with its fake game. `options` are the host's own.
pub fn start_host(root: &Path, key: &SessionKey, options: HostOptions, save: Vec<u8>) -> (SessionHandle, Game) {
    let link = gamelink::Session::create_under(root, "host").unwrap();
    let dir = link.dir().to_path_buf();
    let saves = root.join("host-saves");
    let handle = run_host(HostConfig {
        bulk: Default::default(),
        bind: loopback(),
        name: "Host".into(),
        key: key.clone(),
        link,
        options,
        save_dirs: vec![saves.clone()],
        echo_log: false,
    })
    .unwrap();
    (handle, Game::new(&dir, &saves, true, save))
}

/// A player with its fake game, reconnecting quickly if its connection drops.
pub fn start_player(root: &Path, key: &SessionKey, name: &str, to: SocketAddr) -> (SessionHandle, Game) {
    start_player_with(root, key, name, to, Duration::from_millis(100))
}

/// As [`start_player`], trying again after `reconnect_interval` when the connection drops.
pub fn start_player_with(
    root: &Path,
    key: &SessionKey,
    name: &str,
    to: SocketAddr,
    reconnect_interval: Duration,
) -> (SessionHandle, Game) {
    start_player_window(root, key, name, to, reconnect_interval, Duration::from_secs(60))
}

/// As [`start_player_with`], giving up when the outage has lasted `reconnect_window`.
pub fn start_player_window(
    root: &Path,
    key: &SessionKey,
    name: &str,
    to: SocketAddr,
    reconnect_interval: Duration,
    reconnect_window: Duration,
) -> (SessionHandle, Game) {
    let link = gamelink::Session::create_under(root, name).unwrap();
    let dir = link.dir().to_path_buf();
    let saves = root.join(format!("{name}-saves"));
    let mut config = PlayerConfig::new(vec![to], name, key.clone(), link);
    config.save_dirs = vec![saves.clone()];
    config.reconnect_interval = reconnect_interval;
    config.reconnect_window = reconnect_window;
    let handle = run_player(config).unwrap();
    (handle, Game::new(&dir, &saves, false, Vec::new()))
}

/// A TCP forwarder in front of the host that can cut every connection it carries, so a test can drop one player's
/// link without touching the host or the other players.
pub struct Proxy {
    pub addr: SocketAddr,
    pairs: Arc<Mutex<Vec<(TcpStream, TcpStream)>>>,
    stop: Arc<AtomicBool>,
    swallow: Arc<AtomicBool>,
    refuse: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Proxy {
    pub fn start(target: SocketAddr) -> Proxy {
        let listener = TcpListener::bind(loopback()).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let pairs: Arc<Mutex<Vec<(TcpStream, TcpStream)>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let swallow = Arc::new(AtomicBool::new(false));
        let refuse = Arc::new(AtomicBool::new(false));
        let thread = {
            let (pairs, stop, swallow) = (Arc::clone(&pairs), Arc::clone(&stop), Arc::clone(&swallow));
            let refuse = Arc::clone(&refuse);
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((client, _)) => {
                            if refuse.load(Ordering::Acquire) {
                                let _ = client.shutdown(Shutdown::Both);
                                continue;
                            }
                            let _ = client.set_nonblocking(false);
                            let Ok(host) = TcpStream::connect(target) else { continue };
                            let _ = (client.set_nodelay(true), host.set_nodelay(true));
                            if let (Ok(c2), Ok(h2), Ok(c3), Ok(h3)) =
                                (client.try_clone(), host.try_clone(), client.try_clone(), host.try_clone())
                            {
                                pairs.lock().unwrap().push((c3, h3));
                                forward(client, h2, Arc::clone(&swallow));
                                forward(host, c2, Arc::clone(&swallow));
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
                        Err(_) => thread::sleep(Duration::from_millis(20)),
                    }
                }
            })
        };
        Proxy { addr, pairs, stop, swallow, refuse, thread: Some(thread) }
    }

    /// While on, a new connection is accepted and closed at once: a player trying again finds nobody home, so a test
    /// can look at the outage for as long as it likes.
    pub fn refuse(&self, on: bool) {
        self.refuse.store(on, Ordering::Release);
    }

    /// While on, everything either side sends is read and thrown away: the link looks alive and says nothing, as a
    /// pulled cable does before anyone notices.
    pub fn blackhole(&self, on: bool) {
        self.swallow.store(on, Ordering::Release);
    }

    /// Closes every connection carried so far, both ways. New connections are accepted as before.
    pub fn cut(&self) {
        for (client, host) in self.pairs.lock().unwrap().drain(..) {
            let _ = client.shutdown(Shutdown::Both);
            let _ = host.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.cut();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Copies `from` to `to` on a thread of its own until either ends, then closes both.
fn forward(mut from: TcpStream, mut to: TcpStream, swallow: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if !swallow.load(Ordering::Acquire) && to.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = from.shutdown(Shutdown::Both);
        let _ = to.shutdown(Shutdown::Both);
    });
}
