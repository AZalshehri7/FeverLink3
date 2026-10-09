//! A full resync round over loopback: the host's fake game saves, the launcher streams the file in chunks to the
//! player's launcher, which writes it where the player's game would read it, both fake games reload and report
//! identical checksums, and the round ends. Also the permissions line as the mod reads it. No game is started.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::session::{HostOptions, ResyncLimits};
use feverlink3_launcher::gamelink::{self, Line, Tail};
use feverlink3_launcher::session::{run_host, run_player, Command, HostConfig, PlayerConfig};

/// The mod, reduced to what a resync needs.
struct FakeGame {
    dir: PathBuf,
    save_dir: PathBuf,
    is_host: bool,
    next_seq: u64,
    tail: Tail,
    seen: Vec<Line>,
    handled: usize,
    save_bytes: Vec<u8>,
}

impl FakeGame {
    fn new(dir: &Path, save_dir: &Path, is_host: bool, save_bytes: Vec<u8>) -> FakeGame {
        FakeGame {
            dir: dir.to_path_buf(),
            save_dir: save_dir.to_path_buf(),
            is_host,
            next_seq: 1,
            tail: Tail::new(dir.join(gamelink::TO_GAME)),
            seen: Vec::new(),
            handled: 0,
            save_bytes,
        }
    }

    fn write(&mut self, kind: &str, payload: &str) {
        let line = Line { seq: self.next_seq, kind: kind.into(), from: "0".into(), payload: payload.into() };
        self.next_seq += 1;
        let mut f = OpenOptions::new().create(true).append(true).open(self.dir.join(gamelink::FROM_GAME)).unwrap();
        f.write_all(gamelink::format_line(&line).as_bytes()).unwrap();
    }

    fn announce(&mut self) {
        self.write("hello", "{[\"time\"]=18600}");
        self.write("clock", "{[\"t\"]=18600,[\"speed\"]=0,[\"ahead\"]=3}");
    }

    /// Reads new lines and reacts like the mod would.
    fn step(&mut self) {
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
                    self.write(
                        "checksum_report",
                        &format!(
                            "{{[\"n\"]={n},[\"time\"]={at},[\"parts\"]={{[\"roads\"]=\"r1\",[\"money\"]=\"m1\"}}}}"
                        ),
                    );
                }
                _ => {}
            }
        }
    }

    fn saw(&self, kind: &str, needle: &str) -> bool {
        self.seen.iter().any(|l| l.kind == kind && l.payload.contains(needle))
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

fn wait(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    done()
}

#[test]
fn a_resync_round_moves_the_hosts_save_to_the_player_and_ends() {
    let root = tempfile::TempDir::new().unwrap();
    let key = SessionKey::derive(&SessionKey::new_secret(), None);
    let host_saves = root.path().join("host-user").join("save");
    let player_saves = root.path().join("player-user").join("save");

    // a save of two and a bit chunks, so the streaming loop really loops
    let save: Vec<u8> = (0..(2 * ResyncLimits::CHUNK_SIZE + 4321)).map(|i| (i % 253) as u8).collect();

    let host_link = gamelink::Session::create_under(root.path(), "host").unwrap();
    let host_dir = host_link.dir().to_path_buf();
    let host = run_host(HostConfig {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        name: "Host".into(),
        key: key.clone(),
        link: host_link,
        options: HostOptions::default(),
        save_dirs: vec![host_saves.clone()],
        echo_log: false,
    })
    .unwrap();

    let player_link = gamelink::Session::create_under(root.path(), "pat").unwrap();
    let player_dir = player_link.dir().to_path_buf();
    let mut config = PlayerConfig::new(vec![host.local_addr().unwrap()], "Pat", key, player_link);
    config.save_dirs = vec![player_saves.clone()];
    let player = run_player(config).unwrap();
    assert!(player.wait_for(Duration::from_secs(10), |s| s.connected && s.my_id.is_some()));

    let mut host_game = FakeGame::new(&host_dir, &host_saves, true, save.clone());
    let mut player_game = FakeGame::new(&player_dir, &player_saves, false, Vec::new());
    host_game.announce();
    player_game.announce();
    assert!(host.wait_for(Duration::from_secs(10), |s| s.clocks.len() == 2), "both clocks known");

    host.command(Command::Start);
    assert!(host.wait_for(Duration::from_secs(10), |s| s.session.as_ref().is_some_and(|x| x.started)));

    host.command(Command::Resync);
    let done = wait(Duration::from_secs(30), || {
        host_game.step();
        player_game.step();
        host.status().resync.as_deref() == Some("Idle")
    });
    let logs = host.logs().join("\n");
    assert!(done, "the round reaches Idle; host log:\n{logs}\nstatus: {:?}", host.status().resync);

    // the player's launcher wrote the save under the name the core gave it, byte for byte
    let received = player_saves.join("feverlink3-resync-1.sav");
    assert_eq!(fs::read(&received).expect("received save on disk"), save);
    assert!(!player_saves.join("feverlink3-resync-1.sav.tmp").exists());

    // the player's game was told to load that name, after the file existed
    assert!(player_game.saw("resync_load", "feverlink3-resync-1"));
    // and the verification round compared identical worlds
    let sync = host.status().last_sync.expect("a checksum outcome");
    assert!(sync.identical(), "{sync:?}");

    player.stop();
    host.stop();
}

#[test]
fn permissions_reach_each_game_as_booleans_for_that_player() {
    let root = tempfile::TempDir::new().unwrap();
    let key = SessionKey::derive(&SessionKey::new_secret(), None);
    let host_link = gamelink::Session::create_under(root.path(), "host").unwrap();
    let host_dir = host_link.dir().to_path_buf();
    let host = run_host(HostConfig {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        name: "Host".into(),
        key: key.clone(),
        link: host_link,
        options: HostOptions::default(),
        save_dirs: Vec::new(),
        echo_log: false,
    })
    .unwrap();
    let player_link = gamelink::Session::create_under(root.path(), "pat").unwrap();
    let player_dir = player_link.dir().to_path_buf();
    let player = run_player(PlayerConfig::new(vec![host.local_addr().unwrap()], "Pat", key, player_link)).unwrap();
    assert!(player.wait_for(Duration::from_secs(10), |s| s.connected && s.my_id.is_some()));

    host.command(Command::SetPermissions(feverlink3_core::session::Permissions {
        build: feverlink3_core::session::PlayerSet::HostOnly,
        ..Default::default()
    }));

    let mut player_game = FakeGame::new(&player_dir, &player_dir, false, Vec::new());
    let mut host_game = FakeGame::new(&host_dir, &host_dir, true, Vec::new());
    assert!(wait(Duration::from_secs(10), || {
        player_game.step();
        host_game.step();
        player_game.saw("permissions", "[\"build\"]=false") && host_game.saw("permissions", "[\"build\"]=true")
    }));
    assert!(player_game.saw("permissions", "[\"spend\"]=true"));
    assert!(player_game.saw("permissions", "[\"other\"]=true"));
    player.stop();
    host.stop();
}
