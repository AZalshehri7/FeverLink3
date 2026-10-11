//! The loopback self-test as an integration test: a host and two players with fake games, a wrong password and
//! a garbage line. See `feverlink3_launcher::selftest` for the checks. Never starts the game.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::session::HostOptions;
use feverlink3_launcher::gamelink::{self, Tail};
use feverlink3_launcher::session::{run_host, run_player, HostConfig, PlayerConfig};

#[test]
fn loopback_selftest_passes_in_under_ten_seconds() {
    let started = Instant::now();
    let report = feverlink3_launcher::selftest::run().unwrap_or_else(|e| panic!("{e}"));
    assert!(report.checks.len() >= 20, "{:#?}", report.checks);
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
}

/// The host goes away and comes back on the same port: the player tells its game `started=false`, keeps
/// trying, and joins again.
#[test]
fn a_player_reconnects_after_the_host_drops() {
    let root = tempfile::TempDir::new().unwrap();
    let secret = SessionKey::new_secret();
    let key = SessionKey::derive(&secret, None);
    let start_host = |bind: SocketAddr, name: &str| {
        let link = gamelink::Session::create_under(root.path(), name).unwrap();
        run_host(HostConfig {
            bulk: Default::default(),
            bind,
            name: name.into(),
            key: key.clone(),
            link,
            options: HostOptions::default(),
            save_dirs: Vec::new(),
            echo_log: false,
        })
        .unwrap()
    };

    let host = start_host(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), "Host");
    let addr = host.local_addr().unwrap();
    let link = gamelink::Session::create_under(root.path(), "Pat").unwrap();
    let game_files = link.dir().to_path_buf();
    let mut config = PlayerConfig::new(vec![addr], "Pat", key.clone(), link);
    config.reconnect_interval = Duration::from_millis(100);
    config.reconnect_window = Duration::from_secs(8);
    let player = run_player(config).unwrap();
    assert!(player.wait_for(Duration::from_secs(5), |s| s.connected));

    host.crash();
    assert!(player.wait_for(Duration::from_secs(5), |s| s.reconnecting && !s.connected));
    let mut tail = Tail::new(game_files.join(gamelink::TO_GAME));
    let lines = tail.poll().unwrap();
    assert!(
        lines.iter().any(|l| l.kind == "session" && l.payload.contains("[\"started\"]=false")),
        "the game is told the session stopped: {lines:?}"
    );
    assert!(!player.is_finished());

    let host = start_host(addr, "Host");
    assert!(player.wait_for(Duration::from_secs(5), |s| s.connected && !s.reconnecting && s.my_id.is_some()));
    assert!(host.wait_for(Duration::from_secs(5), |s| s.players.len() == 2));
    assert!(player.status().reconnect_attempts >= 1);
    player.stop();
    host.stop();
}

#[test]
fn a_player_gives_up_after_the_window() {
    let root = tempfile::TempDir::new().unwrap();
    let key = SessionKey::derive(&SessionKey::new_secret(), None);
    let host_link = gamelink::Session::create_under(root.path(), "Host").unwrap();
    let host = run_host(HostConfig {
        bulk: Default::default(),
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        name: "Host".into(),
        key: key.clone(),
        link: host_link,
        options: HostOptions::default(),
        save_dirs: Vec::new(),
        echo_log: false,
    })
    .unwrap();
    let link = gamelink::Session::create_under(root.path(), "Pat").unwrap();
    let mut config = PlayerConfig::new(vec![host.local_addr().unwrap()], "Pat", key, link);
    config.reconnect_interval = Duration::from_millis(50);
    config.reconnect_window = Duration::from_millis(400);
    config.connect_timeout = Duration::from_millis(500);
    let player = run_player(config).unwrap();
    assert!(player.wait_for(Duration::from_secs(5), |s| s.connected));
    host.crash();
    assert!(player.wait_for(Duration::from_secs(10), |s| s.finished));
    assert!(player.status().error.is_some_and(|e| e.contains("could not reconnect")));
}
