//! Eight games over loopback: the host and seven players, each with a launcher session and a fake game talking
//! through the real file link. They start a session, send actions that every other game must receive once and in
//! order, run a resync round, and a ninth is turned away. No game is started.

mod multi_support;

use std::fs;
use std::time::Duration;

use feverlink3_core::session::{ConnectionState, HostOptions, ResyncLimits};
use feverlink3_launcher::session::Command;
use multi_support::*;

const WAIT: Duration = Duration::from_secs(20);

#[test]
fn eight_games_start_play_resync_and_the_ninth_is_turned_away() {
    let root = tempfile::TempDir::new().unwrap();
    let key = key();
    // a save of two chunks and a bit, so the streaming to seven players really loops
    let save: Vec<u8> = (0..(ResyncLimits::CHUNK_SIZE + 4321)).map(|i| (i % 249) as u8).collect();
    let options = HostOptions { ping_seconds: 0.25, ..HostOptions::default() };
    let (host, mut host_game) = start_host(root.path(), &key, options, save.clone());
    let addr = host.local_addr().unwrap();

    let mut players = Vec::new();
    for i in 1..=7 {
        players.push(start_player(root.path(), &key, &format!("P{i}"), addr));
    }

    // ---- everyone is welcomed with an id of its own
    for (handle, _) in &players {
        assert!(handle.wait_for(WAIT, |s| s.connected && s.my_id.is_some()), "{:?}", handle.status());
    }
    let mut ids: Vec<u16> = players.iter().map(|(h, _)| h.status().my_id.unwrap()).collect();
    ids.sort_unstable();
    assert_eq!((1..=7).collect::<Vec<u16>>(), ids, "seven players: ids 1 to 7, each once");
    assert!(host.wait_for(WAIT, |s| s.players.len() == 8), "the host sees itself and seven players");

    // ---- the ninth game is told the session is full, and nobody else notices
    let (ninth, _) = start_player(root.path(), &key, "P8", addr);
    assert!(ninth.wait_for(WAIT, |s| s.finished), "{:?}", ninth.status());
    assert_eq!(Some("full (8 players)".to_owned()), ninth.status().rejected);
    assert_eq!(8, host.status().players.len());
    assert!(players.iter().all(|(h, _)| h.status().connected));

    // ---- every game says hello; the host starts the session; every game's file shows it
    host_game.announce();
    for (_, g) in &mut players {
        g.announce();
    }
    assert!(host.wait_for(WAIT, |s| s.clocks.len() == 8), "eight clocks: {:?}", host.status().clocks);
    host.command(Command::Start);
    {
        let mut games: Vec<&mut Game> =
            std::iter::once(&mut host_game).chain(players.iter_mut().map(|(_, g)| g)).collect();
        let started = wait_games(WAIT, &mut games, |gs| gs.iter().all(|g| g.saw("session", "[\"started\"]=true")));
        assert!(started, "every game was told the session started");
    }

    // ---- actions: every game gets every other game's, once, in the order the host relayed them
    for round in 0..3i64 {
        for (i, (_, g)) in players.iter_mut().enumerate() {
            g.act(&format!("p{}r{round}", i + 1), round * 10 + i as i64);
        }
        host_game.act(&format!("hr{round}"), 1000 + round);
    }
    let total = 3 * 8;
    {
        let mut games: Vec<&mut Game> =
            std::iter::once(&mut host_game).chain(players.iter_mut().map(|(_, g)| g)).collect();
        let delivered = wait_games(WAIT, &mut games, |gs| gs.iter().all(|g| g.lines("action").len() == total - 3));
        assert!(
            delivered,
            "each game hears the 21 actions of the other seven: {:?}",
            games.iter().map(|g| g.lines("action").len()).collect::<Vec<_>>()
        );
    }
    assert_eq!(total as u64, host.status().actions_relayed, "21 from the players and 3 from the host's game");
    // the order the host relayed the players' actions in is the order its own game saw them; everybody else saw the same
    // order, minus their own
    let relayed: Vec<(String, String)> =
        host_game.lines("action").iter().map(|l| (l.from.clone(), l.payload.clone())).collect();
    for (handle, g) in &players {
        // the seven connected at once, so which player got which id is up to the race
        let id = handle.status().my_id.unwrap().to_string();
        let heard: Vec<(String, String)> =
            g.lines("action").iter().filter(|l| l.from != "0").map(|l| (l.from.clone(), l.payload.clone())).collect();
        let want: Vec<(String, String)> = relayed.iter().filter(|(from, _)| *from != id).cloned().collect();
        assert_eq!(
            want, heard,
            "player {id} hears the other players' actions once each, in the host's order, not its own"
        );
        assert_eq!(3, g.lines("action").iter().filter(|l| l.from == "0").count(), "and the host's three");
    }

    // ---- round-trip times reach every roster
    assert!(
        wait(WAIT, || host.status().roster.len() == 8 && host.status().roster.iter().all(|e| e.rtt_ms.is_some())),
        "{:?}",
        host.status().roster
    );
    assert!(host.status().roster.iter().all(|e| e.state == ConnectionState::Connected));
    for (handle, _) in &players {
        assert!(
            wait(WAIT, || handle.status().roster.iter().filter(|e| e.rtt_ms.is_some()).count() == 8),
            "{:?}",
            handle.status().roster
        );
    }

    // ---- a resync round with all eight
    host.command(Command::Resync);
    {
        let mut games: Vec<&mut Game> =
            std::iter::once(&mut host_game).chain(players.iter_mut().map(|(_, g)| g)).collect();
        let done = wait_games(Duration::from_secs(40), &mut games, |_| host.status().resync.as_deref() == Some("Idle"));
        assert!(done, "the round ends; host log:\n{}\nstatus {:?}", host.logs().join("\n"), host.status().resync);
    }
    for i in 1..=7 {
        let received = root.path().join(format!("P{i}-saves")).join("feverlink3-resync-1.sav");
        assert_eq!(save, fs::read(&received).unwrap_or_default(), "P{i} has the host's save, byte for byte");
    }
    for (i, (_, g)) in players.iter().enumerate() {
        assert!(g.saw("resync_load", "feverlink3-resync-1"), "P{} was told to load it", i + 1);
    }
    let sync = host.status().last_sync.expect("the verification round");
    assert!(sync.identical() && sync.reporters.len() == 8, "{sync:?}");

    // ---- and the host can take a player out of the eight
    let leaver = players[2].0.status().my_id.unwrap();
    host.command(Command::Kick(leaver));
    assert!(players[2].0.wait_for(WAIT, |s| s.finished));
    assert_eq!(Some("kicked (removed by the host)".to_owned()), players[2].0.status().rejected);
    assert_eq!(0, players[2].0.status().reconnect_attempts, "a removed player does not knock again");
    assert!(host.wait_for(WAIT, |s| s.players.len() == 7 && s.roster.iter().all(|e| e.id != leaver)));

    for (handle, _) in &players {
        handle.stop();
    }
    host.stop();
}
