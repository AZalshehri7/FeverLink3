//! The real session code (`run_host_over`, `run_player_over`) over the Steam transport on the in-memory Steam: a
//! host and players with fake games, the handshake, the relay of actions, a rejected player, a resync round that
//! moves a multi-chunk savegame, reconnecting, a stop in the middle of a long route search, and a full table of
//! eight players. Nothing here starts the game or touches a real Steam.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use feverlink3_core::session::{HostOptions, ResyncLimits};

use super::*;
use crate::gamelink::{self, Line, Tail};
use crate::session::{run_host_over, run_player_over, Command};
use crate::steamnet::fake::FakeMachine;
use crate::steamnet::bulk_io::{BulkMode, BulkSettings};
use crate::steamnet::SteamCore;

const WAIT: Duration = Duration::from_secs(20);

/// The mod, reduced to what these tests need: it writes lines the way the mod does and reacts to a resync.
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
                    let parts = "{[\"roads\"]=\"r1\",[\"money\"]=\"m1\"}";
                    self.write("checksum_report", &format!("{{[\"n\"]={n},[\"time\"]={at},[\"parts\"]={parts}}}"));
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

/// A host session on its own machine, listening through the Steam transport.
struct Table {
    root: tempfile::TempDir,
    key: SessionKey,
    machine: FakeMachine,
    core: SteamCore,
    handle: SessionHandle,
    game: FakeGame,
}

impl Table {
    fn new(options: HostOptions, save: Vec<u8>) -> Table {
        Table::new_with(options, save, BulkSettings { mode: BulkMode::Off, ..BulkSettings::default() })
    }

    /// As `new`; `bulk` makes the host send saves to players that can take them as bulk datagrams.
    fn new_with(options: HostOptions, save: Vec<u8>, bulk: BulkSettings) -> Table {
        let machine = FakeMachine::new();
        let core = machine.core();
        let root = tempfile::TempDir::new().unwrap();
        let key = SessionKey::derive(&SessionKey::new_secret(), None);
        let (handle, game) = Table::start_host(root.path(), &core, &key, options, save, "host", bulk);
        Table { root, key, machine, core, handle, game }
    }

    /// Starts a host session on `core` (a restart after a stop uses the same core and so the same Steam ID).
    fn start_host(
        root: &Path,
        core: &SteamCore,
        key: &SessionKey,
        options: HostOptions,
        save: Vec<u8>,
        folder: &str,
        bulk: BulkSettings,
    ) -> (SessionHandle, FakeGame) {
        let saves = root.join(format!("{folder}-user")).join("save");
        let link = gamelink::Session::create_under(root, folder).unwrap();
        let dir = link.dir().to_path_buf();
        let core_ref = core.handle();
        let handle = run_host_over(
            HostConfig {
                bulk,
                bind: SocketAddr::from(([127, 0, 0, 1], 0)),
                name: "Host".into(),
                key: key.clone(),
                link,
                options,
                save_dirs: vec![saves.clone()],
                echo_log: false,
            },
            move |tx| SteamHostNet::listen_on(&core_ref, tx, WAIT, MAX_CONNECTIONS),
        )
        .unwrap();
        let game = FakeGame::new(&dir, &saves, true, save);
        (handle, game)
    }

    fn endpoint(&self) -> SteamEndpoint {
        SteamEndpoint(self.machine.steam_id())
    }

    fn player(&self, name: &str) -> Player {
        self.player_with(name, &self.key, |_| {})
    }

    fn player_with(&self, name: &str, key: &SessionKey, tune: impl FnOnce(&mut PlayerConfig)) -> Player {
        Player::start(self.root.path(), self.endpoint(), name, key, tune).unwrap_or_else(|e| panic!("{name}: {e}"))
    }
}

/// A player session on a machine of its own.
struct Player {
    _machine: FakeMachine,
    _core: SteamCore,
    handle: SessionHandle,
    game: FakeGame,
    saves: PathBuf,
}

impl Player {
    fn start(
        root: &Path,
        host: SteamEndpoint,
        name: &str,
        key: &SessionKey,
        tune: impl FnOnce(&mut PlayerConfig),
    ) -> io::Result<Player> {
        let machine = FakeMachine::new();
        let core = machine.core();
        let saves = root.join(format!("{name}-user")).join("save");
        let link = gamelink::Session::create_under(root, name)?;
        let dir = link.dir().to_path_buf();
        let mut config = steam_player_config(name, key.clone(), link);
        config.save_dirs = vec![saves.clone()];
        config.reconnect_interval = Duration::from_millis(100);
        config.reconnect_window = Duration::from_secs(30);
        tune(&mut config);
        let handle = run_player_over(config, steam_connector(core.handle(), host), &mut |_| {})?;
        let game = FakeGame::new(&dir, &saves, false, Vec::new());
        Ok(Player { _machine: machine, _core: core, handle, game, saves })
    }
}

#[test]
fn a_session_runs_end_to_end_over_steam() {
    let mut table = Table::new(HostOptions::default(), Vec::new());
    let (mut alice, mut bob) = (table.player("Alice"), table.player("Bob"));

    // welcomed with distinct ids; the host sees itself and both players
    for p in [&alice, &bob] {
        assert!(p.handle.wait_for(WAIT, |s| s.connected && s.my_id.is_some()), "{:?}", p.handle.logs());
    }
    let ids = [alice.handle.status().my_id.unwrap(), bob.handle.status().my_id.unwrap()];
    assert!(ids[0] != ids[1] && ids.iter().all(|&i| i != 0));
    assert!(table.handle.wait_for(WAIT, |s| s.players.len() == 3));

    // what the transport measured is in the status: one entry per connection on the host, one on a player
    let direct = |l: &crate::net::LinkInfo| l.route.as_deref() == Some("direct") && l.ping_ms == Some(30);
    assert!(
        table.handle.wait_for(WAIT, |s| s.links.len() == 2 && s.links.iter().all(direct)),
        "{:?}",
        table.handle.status().links
    );
    assert!(alice.handle.wait_for(WAIT, |s| s.links.len() == 1 && direct(&s.links[0])));
    let log = table.handle.logs().join("\n");
    assert!(log.contains(&format!("listening on Steam (Steam ID {}, virtual port 0)", table.endpoint())), "{log}");
    assert!(alice.handle.logs().join("\n").contains(&format!("connected to Steam ID {}", table.endpoint())));

    // the session state reaches every game
    table.game.write("hello", "{[\"time\"]=0}");
    alice.game.write("hello", "{[\"time\"]=0}");
    bob.game.write("hello", "{[\"time\"]=0}");
    table.handle.command(Command::Start);
    assert!(wait(WAIT, || {
        table.game.step();
        alice.game.step();
        bob.game.step();
        [&table.game, &alice.game, &bob.game].iter().all(|g| g.saw("session", "[\"started\"]=true"))
    }));

    // an action goes to everyone else with the sender filled in by the host, and never back to its author
    // (a body reaches a game only as a Lua literal: 1 is one)
    let claim_other = format!("{{[\"body\"]=\"1\",[\"from\"]={},[\"seq\"]=1,[\"stamp\"]=2400}}", ids[1]);
    alice.game.write("action", &claim_other);
    let want = format!("[\"from\"]={}", ids[0]);
    assert!(wait(WAIT, || {
        table.game.step();
        bob.game.step();
        table.game.saw("action", &want) && bob.game.saw("action", &want)
    }));
    thread::sleep(Duration::from_millis(100));
    alice.game.step();
    assert!(!alice.game.saw("action", ""), "the action does not come back");
    assert_eq!(table.handle.status().actions_relayed, 1);

    // leaving: the host hears about it
    alice.handle.stop();
    assert!(table.handle.wait_for(WAIT, |s| s.players.len() == 2));
    bob.handle.stop();
    table.handle.stop();
}

#[test]
fn a_rejected_player_learns_why_even_with_a_slow_wire() {
    let table = Table::new(HostOptions::default(), Vec::new());
    // the reject frame is still on its way when the host closes the connection: the close must wait for it
    table.machine.set_wire_delay(Duration::from_millis(60));
    let stranger = SessionKey::derive(&SessionKey::new_secret(), None);
    let mallory = table.player_with("Mallory", &stranger, |_| {});
    assert!(mallory.handle.wait_for(WAIT, |s| s.finished), "{:?}", mallory.handle.logs());
    let status = mallory.handle.status();
    assert_eq!(status.rejected.as_deref(), Some("bad-proof"), "{:?}", mallory.handle.logs());
    assert!(status.my_id.is_none());
    assert_eq!(table.handle.status().players.len(), 1, "the stranger never became a player");
    assert!(table.machine.closes().iter().all(|(_, linger)| *linger), "the host's closes linger");
}

#[test]
fn the_first_connection_reports_what_it_is_doing_to_the_caller() {
    let table = Table::new(HostOptions::default(), Vec::new());
    table.machine.set_route_delay(Duration::from_millis(250));
    let machine = FakeMachine::new();
    let core = machine.core();
    let link = gamelink::Session::create_under(table.root.path(), "Pat").unwrap();
    let config = steam_player_config("Pat", table.key.clone(), link);
    assert_eq!(config.connect_timeout, CONNECT_TIMEOUT, "a route can take 10 to 20 s");
    let mut texts: Vec<String> = Vec::new();
    let handle =
        run_player_over(config, steam_connector(core.handle(), table.endpoint()), &mut |t| texts.push(t.to_owned()))
            .unwrap();
    assert_eq!(texts, vec![STATUS_CONTACTING, STATUS_FINDING_ROUTE, "connected, direct"]);
    assert!(handle.wait_for(WAIT, |s| s.connected));
    handle.stop();
}

/// Moves a savegame of `chunks` whole chunks and a bit through a resync round and checks it arrives byte for byte.
/// The player stops reading until the host has run into Steam's 512 KiB send buffer, so the transport really has to
/// wait for room (no timing involved), then reads on.
fn resync_round_moves_a_save(chunks: usize) {
    let save: Vec<u8> = (0..(chunks * ResyncLimits::CHUNK_SIZE + 4321)).map(|i| (i % 253) as u8).collect();
    let mut table = Table::new(HostOptions::default(), save.clone());
    let mut player = table.player("Pat");
    assert!(player.handle.wait_for(WAIT, |s| s.connected && s.my_id.is_some()), "{:?}", player.handle.logs());

    table.game.announce();
    player.game.announce();
    assert!(table.handle.wait_for(WAIT, |s| s.clocks.len() == 2), "both clocks known");
    table.handle.command(Command::Start);
    assert!(table.handle.wait_for(WAIT, |s| s.session.as_ref().is_some_and(|x| x.started)));

    player._machine.stall_receive(true);
    table.handle.command(Command::Resync);
    let done = wait(Duration::from_secs(300), || {
        table.game.step();
        player.game.step();
        if table.machine.full_sends() > 0 {
            player._machine.stall_receive(false);
        }
        table.handle.status().resync.as_deref() == Some("Idle")
    });
    let logs = table.handle.logs().join("\n");
    assert!(done, "the round reaches Idle; host log:\n{logs}\nstatus: {:?}", table.handle.status().resync);

    // the player's launcher wrote the save under the name the core gave it, byte for byte
    let received = player.saves.join("feverlink3-resync-1.sav");
    assert!(fs::read(&received).expect("received save on disk") == save, "the save arrived intact");
    assert!(player.game.saw("resync_load", "feverlink3-resync-1"));
    assert!(table.handle.status().last_sync.expect("a checksum outcome").identical());

    // the biggest message the host handed to Steam was a full chunk: a bit over 350 KB, inside the 512 KiB limit
    let largest = table.machine.largest_sent();
    assert!(largest > 349_000 && largest + 100_000 < MAX_MESSAGE, "largest Steam message: {largest} bytes");
    assert!(table.machine.full_sends() > 0, "the send buffer filled and the transport waited");
    player.handle.stop();
    table.handle.stop();
}

#[test]
fn a_resync_round_moves_a_multi_chunk_save_over_steam() {
    resync_round_moves_a_save(3);
}

/// The size of the real save: about 31 MB, 119 chunks, through hashing, sealing, Steam's send buffer, reassembly
/// and the hash check. Ten seconds in a debug build.
#[test]
fn a_resync_round_moves_a_savegame_of_the_real_size_over_steam() {
    resync_round_moves_a_save(31 * 1024 * 1024 / ResyncLimits::CHUNK_SIZE);
}

#[test]
fn a_player_reconnects_over_steam_after_the_host_stops_and_returns() {
    let table = Table::new(HostOptions::default(), Vec::new());
    let player = table.player("Pat");
    assert!(player.handle.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));
    let Table { root, key, machine, core, handle, game: _ } = table;
    // the host's process dies: nobody says goodbye (a host that stops on purpose does, and its players end their session
    // instead of reconnecting, see the test below)
    handle.crash();
    assert!(player.handle.wait_for(WAIT, |s| s.reconnecting && !s.connected), "{:?}", player.handle.logs());
    let mut tail = Tail::new(player.game.dir.join(gamelink::TO_GAME));
    let lines = tail.poll().unwrap();
    assert!(
        lines.iter().any(|l| l.kind == "session" && l.payload.contains("[\"started\"]=false")),
        "the game is told the session stopped: {lines:?}"
    );
    assert!(!player.handle.is_finished());
    assert!(player.handle.logs().join("\n").contains("connection closed: closed by the other side"));

    // the host comes back on the same Steam ID
    let (host, ..) = Table::start_host(root.path(), &core, &key, HostOptions::default(), Vec::new(), "host2", BulkSettings::default());
    assert!(player.handle.wait_for(WAIT, |s| s.connected && !s.reconnecting && s.my_id.is_some()));
    assert!(host.wait_for(WAIT, |s| s.players.len() == 2));
    assert!(player.handle.status().reconnect_attempts >= 1);
    player.handle.stop();
    host.stop();
    drop((machine, core));
}

#[test]
fn a_host_that_stops_over_steam_tells_its_player_who_ends_its_session_instead_of_reconnecting() {
    let table = Table::new(HostOptions::default(), Vec::new());
    let player = table.player("Pat");
    assert!(player.handle.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));
    table.handle.stop();
    assert!(player.handle.wait_for(WAIT, |s| s.finished), "{:?}", player.handle.logs());
    let s = player.handle.status();
    assert_eq!(Some("ended (the host ended the session)".to_owned()), s.rejected);
    assert_eq!((0, false), (s.reconnect_attempts, s.reconnecting));
}

#[test]
fn a_player_whose_steam_connection_dies_comes_back_as_itself() {
    let table = Table::new(HostOptions::default(), Vec::new());
    let player = table.player("Pat");
    assert!(player.handle.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));
    let id = player.handle.status().my_id.unwrap();
    let other = table.player("Bob");
    assert!(other.handle.wait_for(WAIT, |s| s.connected && s.my_id.is_some()));

    // the network under the host's connections dies (Steam reports a timeout on both ends)
    table.machine.break_connections();
    assert!(player.handle.wait_for(WAIT, |s| s.reconnecting || s.my_id.is_none()), "{:?}", player.handle.logs());
    // her launcher connects again by itself and the host gives her the slot she held: same id, no new player
    assert!(
        player.handle.wait_for(WAIT, |s| s.connected && !s.reconnecting && s.my_id == Some(id)),
        "{:?}\n{:?}",
        player.handle.status().my_id,
        player.handle.logs()
    );
    assert!(player.handle.status().reconnect_attempts >= 1);
    assert!(other.handle.wait_for(WAIT, |s| s.connected && !s.reconnecting));
    let roster = table.handle.status().roster;
    let ids: Vec<u16> = roster.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), 3, "the host and two players, not three players: {roster:?}");
    assert!(
        table.handle.wait_for(WAIT, |s| s
            .roster
            .iter()
            .all(|e| e.state == feverlink3_core::session::ConnectionState::Connected)),
        "{:?}",
        table.handle.status().roster
    );
    assert!(table.handle.logs().iter().any(|l| l.contains("Pat is back")), "{:?}", table.handle.logs());
}

#[test]
fn stopping_a_player_does_not_wait_for_a_route_that_never_comes() {
    let table = Table::new(HostOptions::default(), Vec::new());
    let player = table.player_with("Pat", &table.key, |c| c.connect_timeout = Duration::from_secs(60));
    assert!(player.handle.wait_for(WAIT, |s| s.connected));
    // the host disappears from the network: every attempt to reconnect hangs in "contacting the host"
    table.machine.set_reachable(false);
    table.handle.crash();
    assert!(player.handle.wait_for(WAIT, |s| s.reconnecting));
    assert!(wait(WAIT, || player.handle.logs().iter().any(|l| l.contains("reconnect attempt 1: contacting the host"))));
    let started = Instant::now();
    player.handle.stop();
    assert!(started.elapsed() < Duration::from_secs(5), "stop took {:?}", started.elapsed());
    assert!(player.handle.is_finished());
}

#[test]
fn a_full_table_of_eight_games_works_and_one_more_is_told_the_table_is_full() {
    let mut table = Table::new(HostOptions::default(), Vec::new());
    // the table holds eight games: the host and seven players
    let mut players: Vec<Player> = (0..7).map(|i| table.player(&format!("P{i}"))).collect();
    for p in &players {
        assert!(p.handle.wait_for(WAIT, |s| s.connected && s.my_id.is_some()), "{:?}", p.handle.logs());
    }
    assert!(table.handle.wait_for(WAIT, |s| s.players.len() == 8), "the host and seven players");
    assert!(table.handle.wait_for(WAIT, |s| s.links.len() == 7));
    let mut ids: Vec<u16> = players.iter().map(|p| p.handle.status().my_id.unwrap()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 7, "seven distinct ids");

    // one action from one player reaches the other seven and the host
    // (a body reaches a game only as a Lua literal: 77 is one)
    players[0].game.write("action", "{[\"body\"]=\"77\",[\"seq\"]=1,[\"stamp\"]=2400}");
    assert!(wait(WAIT, || {
        table.game.step();
        players.iter_mut().skip(1).for_each(|p| p.game.step());
        table.game.saw("action", "\"77\"") && players.iter().skip(1).all(|p| p.game.saw("action", "\"77\""))
    }));

    // the next one is welcomed by Steam and turned away by the session
    let ninth = table.player("P8");
    assert!(ninth.handle.wait_for(WAIT, |s| s.finished), "{:?}", ninth.handle.logs());
    assert_eq!(ninth.handle.status().rejected.as_deref(), Some("full (8 players)"));
    assert_eq!(table.handle.status().players.len(), 8);
    for p in &players {
        assert!(p.handle.status().connected, "the others are untouched");
    }
}

#[test]
fn what_the_session_log_says_about_a_failed_first_connection() {
    // a host that is not there: the caller gets the reason at once, nothing is started
    let target = FakeMachine::new();
    let _core = target.core();
    let root = tempfile::TempDir::new().unwrap();
    let key = SessionKey::derive(&SessionKey::new_secret(), None);
    let err = Player::start(root.path(), SteamEndpoint(target.steam_id()), "Pat", &key, |_| {}).err().expect("refused");
    assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    assert!(err.to_string().contains("could not connect to Steam ID"), "{err}");
}

/// A resync round with `FEVERLINK3_BULK=dgram` on the host: the save goes to the player as bulk datagrams, no chunk
/// frame is sent, and it arrives byte for byte. With `lose_all`, every datagram of the host is lost: the transfer
/// stalls, the host falls back to chunks after the stall time, and the save arrives all the same.
fn resync_round_moves_a_save_by_bulk(chunks: usize, lose_all: bool) -> (Table, Player) {
    let settings = BulkSettings { mode: BulkMode::Dgram, ..BulkSettings::default() };
    resync_round(chunks, settings, |table, _| {
        if lose_all {
            table.machine.lose_every_nth_unreliable(1);
        }
    })
}

/// A resync round with the host set to `bulk`; `setup` runs once the player has joined, before the round starts.
fn resync_round(chunks: usize, bulk: BulkSettings, setup: impl FnOnce(&Table, &Player)) -> (Table, Player) {
    let save: Vec<u8> = (0..(chunks * ResyncLimits::CHUNK_SIZE + 4321)).map(|i| (i % 249) as u8).collect();
    let mut table = Table::new_with(HostOptions::default(), save.clone(), bulk);
    let mut player = table.player("Pat");
    assert!(player.handle.wait_for(WAIT, |s| s.connected && s.my_id.is_some()), "{:?}", player.handle.logs());
    setup(&table, &player);
    table.game.announce();
    player.game.announce();
    assert!(table.handle.wait_for(WAIT, |s| s.clocks.len() == 2), "both clocks known");
    table.handle.command(Command::Start);
    assert!(table.handle.wait_for(WAIT, |s| s.session.as_ref().is_some_and(|x| x.started)));
    table.handle.command(Command::Resync);
    let done = wait(Duration::from_secs(120), || {
        table.game.step();
        player.game.step();
        table.handle.status().resync.as_deref() == Some("Idle")
    });
    let logs = table.handle.logs().join("\n");
    assert!(done, "the round reaches Idle; host log:\n{logs}\nstatus: {:?}", table.handle.status().resync);
    let received = player.saves.join("feverlink3-resync-1.sav");
    assert!(fs::read(&received).expect("received save on disk") == save, "the save arrived intact");
    assert!(player.game.saw("resync_load", "feverlink3-resync-1"));
    assert!(table.handle.status().last_sync.expect("a checksum outcome").identical());
    (table, player)
}

fn par_settings(connect_timeout: Duration) -> BulkSettings {
    let par = crate::steamnet::par::ParConfig {
        min_size: 1_000_000,
        forced: Some(3),
        connect_timeout,
        ..crate::steamnet::par::ParConfig::default()
    };
    BulkSettings { mode: BulkMode::Par, par }
}

#[test]
fn a_resync_save_goes_over_parallel_connections_and_no_chunk_is_sent_on_the_main_connection() {
    let (table, player) = resync_round(7, par_settings(Duration::from_secs(20)), |_, _| {});
    let host_log = table.handle.logs().join("\n");
    assert!(host_log.contains("over parallel Steam connections"), "{host_log}");
    assert!(host_log.contains("has the save (parallel connections)"), "{host_log}");
    assert!(host_log.contains("3 connection"), "{host_log}");
    // nothing as big as a ResyncChunk frame (350 KB) went over the session's stream: the largest message is a
    // 128 KiB binary chunk of the parallel transfer
    let largest = table.machine.largest_sent();
    assert!(largest > 130_000 && largest < 140_000, "largest message {largest}");
    let player_log = player.handle.logs().join("\n");
    assert!(player_log.contains("received over parallel connections"), "{player_log}");
    assert!(!player.saves.join("feverlink3-resync-1.sav.part").exists());
    // the window's numbers were published while it ran or are cleared at the end
    assert!(table.handle.status().transfer.is_none() && table.handle.status().transfer_note.is_none());
    player.handle.stop();
    table.handle.stop();
}

#[test]
fn when_the_extra_connections_cannot_be_opened_the_player_gets_the_chunks_instead() {
    let (table, player) = resync_round(7, par_settings(Duration::from_secs(2)), |_, player| {
        // from now on nobody reaches the player: new Steam connections to it hang, the session's stays up
        player._machine.set_reachable(false);
    });
    let host_log = table.handle.logs().join("\n");
    assert!(host_log.contains("bulk transfer to Pat failed ("), "{host_log}");
    assert!(host_log.contains("within 2 s"), "the cause is logged: {host_log}");
    assert!(host_log.contains("sending the chunks instead"), "{host_log}");
    assert!(table.machine.largest_sent() > 349_000, "the chunks went over the session's stream");
    player.handle.stop();
    table.handle.stop();
}

#[test]
fn a_resync_save_goes_to_a_capable_player_as_bulk_datagrams_and_no_chunk_is_sent() {
    let (table, player) = resync_round_moves_a_save_by_bulk(3, false);
    let host_log = table.handle.logs().join("\n");
    assert!(host_log.contains("sending the save to 1 player(s) as bulk datagrams"), "{host_log}");
    assert!(host_log.contains("has the save (bulk datagrams)"), "{host_log}");
    // nothing as big as a chunk (350 KB) went over the reliable stream, and the datagrams were really sent
    assert!(table.machine.largest_sent() < 100_000, "largest message {}", table.machine.largest_sent());
    let (sent, _, _) = table.machine.unreliable_counts();
    assert!(sent > 700, "{sent} datagrams");
    let player_log = player.handle.logs().join("\n");
    assert!(player_log.contains("save received as bulk datagrams"), "{player_log}");
    player.handle.stop();
    table.handle.stop();
}

#[test]
fn a_bulk_transfer_that_stalls_falls_back_to_chunks_for_that_player() {
    let (table, player) = resync_round_moves_a_save_by_bulk(2, true);
    let host_log = table.handle.logs().join("\n");
    assert!(
        host_log.contains("bulk transfer to Pat failed (no progress for 10 s); sending the chunks instead"),
        "{host_log}"
    );
    assert!(table.machine.largest_sent() > 349_000, "the chunks went over the reliable stream");
    player.handle.stop();
    table.handle.stop();
}
