//! The latency logs through the real file link and a real connection (loopback): a clock and an action the mod stamps (`hop.g`)
//! reach the other game with every stamp of the way (its launcher's read, the host's, the receiving launcher's write, the offsets
//! and round trips), and the launchers and the host write their per-minute and per-ping lines. No game is started.

mod multi_support;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use feverlink3_core::session::HostOptions;
use multi_support::*;

const WAIT: Duration = Duration::from_secs(20);

fn wall() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

#[test]
fn a_stamped_clock_and_action_arrive_with_the_stamps_of_every_hop() {
    let root = tempfile::TempDir::new().unwrap();
    let key = key();
    let options = HostOptions { ping_seconds: 0.2, ..HostOptions::default() };
    let (host, mut host_game) = start_host(root.path(), &key, options, Vec::new());
    let (player, mut player_game) = start_player(root.path(), &key, "P", host.local_addr().unwrap());
    assert!(player.wait_for(WAIT, |s| s.connected && s.my_id.is_some()), "{:?}", player.status());
    host_game.announce();
    player_game.announce();
    // a few pings, so the player's clock offset is measured and reported back
    assert!(host.wait_for(WAIT, |s| s.clocks.len() == 2));
    std::thread::sleep(Duration::from_millis(700));

    let g = wall();
    player_game.write("clock", &format!("{{[\"t\"]=18800,[\"speed\"]=0,[\"ahead\"]=3,[\"hop\"]={{[\"g\"]={g}}}}}"));
    let seen = wait_games(WAIT, &mut [&mut host_game], |gs| gs[0].saw("peerclock", "[\"t\"]=18800"));
    assert!(seen, "the host's game gets the player's clock");
    let line =
        host_game.lines("peerclock").into_iter().find(|l| l.payload.contains("[\"t\"]=18800")).unwrap().payload.clone();
    for key in ["[\"g\"]=", "[\"l\"]=", "[\"h\"]=", "[\"so\"]=", "[\"sr\"]=", "[\"w\"]=", "[\"ro\"]=0", "[\"rr\"]=0"] {
        assert!(line.contains(key), "{key} in {line}");
    }
    assert_eq!(number_after(&line, "[\"g\"]="), Some(g));
    // one PC, one clock: every stamp is at or after the one before it, within the test's few seconds
    let stamps: Vec<i64> =
        ["[\"g\"]=", "[\"l\"]=", "[\"h\"]=", "[\"w\"]="].iter().map(|k| number_after(&line, k).unwrap()).collect();
    assert!(stamps.windows(2).all(|w| w[0] <= w[1] && w[1] - w[0] < 5000), "{stamps:?}");
    assert!(
        number_after(&line, "[\"so\"]=").is_some_and(|o| o.abs() <= 5),
        "one machine: no offset to speak of: {line}"
    );

    let g = wall();
    host_game.write(
        "action",
        &format!("{{[\"body\"]=\"\\\"x\\\"\",[\"seq\"]=1,[\"stamp\"]=30000,[\"hop\"]={{[\"g\"]={g}}}}}"),
    );
    let seen = wait_games(WAIT, &mut [&mut player_game], |gs| gs[0].lines("action").len() == 1);
    assert!(seen, "the player's game gets the host's action");
    let line = &player_game.lines("action")[0].payload;
    for key in ["[\"g\"]=", "[\"l\"]=", "[\"h\"]=", "[\"so\"]=0", "[\"w\"]=", "[\"ro\"]="] {
        assert!(line.contains(key), "{key} in {line}");
    }

    // a line without stamps (an older mod) is relayed as before, without any
    player_game.write("clock", "{[\"t\"]=19000,[\"speed\"]=0,[\"ahead\"]=3}");
    assert!(wait_games(WAIT, &mut [&mut host_game], |gs| gs[0].saw("peerclock", "[\"t\"]=19000")));
    let line = host_game.lines("peerclock").into_iter().find(|l| l.payload.contains("[\"t\"]=19000")).unwrap();
    assert!(!line.payload.contains("hop"), "{}", line.payload);

    assert!(
        host.logs().iter().any(|l| l.starts_with("link.ping ") && l.contains(" peer=1 ") && l.contains(" off_ms=")),
        "{:?}",
        host.logs()
    );
}
