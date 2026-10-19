//! The mod-list fingerprint (`docs/PROTOCOL.md`, "Mod list"): the builds at join, the mods of the host's WORLD between
//! the save and the transfer; what refuses, what only warns, the readable diff, and old peers on either side.

mod common;

use common::{json, Harness};
use feverlink3_core::protocol::FrameCodec;
use feverlink3_core::session::mods::{diff, MAX_JOIN_MODS};
use feverlink3_core::session::*;

fn entry(id: &str, revision: i64, cosmetic: bool, files: Option<&str>) -> ModEntry {
    ModEntry { id: id.to_owned(), revision, cosmetic, files: files.map(str::to_owned) }
}

fn game(mods: Vec<ModEntry>) -> ModFingerprint {
    ModFingerprint { game: "1.4.2".into(), feverlink3: "0.2.0".into(), native: "ab12".into(), mods, unread: false }
        .normalized()
}

/// The host has three mods installed; its world uses `trains_pack` and `nice_ui` only (`unused_pack` is installed, not used).
fn host_mods() -> Vec<ModEntry> {
    vec![
        entry("trains_pack", 3, false, Some("f1")),
        entry("nice_ui", 2, true, Some("u1")),
        entry("unused_pack", 1, false, None),
    ]
}

const WORLD: [&str; 2] = ["trains_pack", "nice_ui"];

/// A started session with one player that joined with `guest`; a resync round runs up to the host's save, which reports
/// `world` as the mods it uses. Returns the harness, the player and the round id.
fn round(guest: Option<ModFingerprint>, world: Option<&[&str]>) -> (Harness, ConnectionId, i32) {
    let mut h = Harness::with(HostOptions { mods: Some(game(host_mods())), ..HostOptions::default() });
    let c = h.connect_with_mods("Pat", guest);
    h.hello_all(10_000, 0);
    h.host_call(HostSession::start);
    h.host_call(|host| host.set_speed(1));
    h.hello_all(10_000, 1);
    h.host_call(|host| host.start_resync("t"));
    let pause_at = h.host.state().pause_at.expect("paused");
    h.stop_all_at(pause_at);
    let id = h.host.resync_id().unwrap();
    let mods = world.map(|w| serde_json::json!(w));
    h.host_game(MessageKind::SaveDone, &json(&SaveDoneMessage { id, ok: true, detail: None, mods }));
    (h, c, id)
}

fn warnings(h: &Harness) -> Vec<String> {
    h.host.state().mod_warnings.clone()
}

#[test]
fn the_worlds_mods_equal_on_the_player_get_the_world_and_an_unused_host_mod_does_not_block() {
    let guest = vec![entry("trains_pack", 3, false, Some("f1")), entry("nice_ui", 2, true, Some("u1"))];
    let (mut h, c, id) = round(Some(game(guest)), Some(&WORLD));
    assert!(h.p(c).rejected.is_none(), "{:?}", h.host_log);
    let w: WelcomeMessage = h.p(c).last(MessageKind::Welcome);
    assert!(w.mods_checked);
    h.send_file(id, b"world");
    assert_eq!(h.p(c).saves.len(), 1, "the world went out");
    assert!(warnings(&h).is_empty(), "{:?}", warnings(&h));
}

#[test]
fn a_world_mod_with_another_revision_or_other_files_or_missing_removes_the_player_before_any_world_data() {
    let cases = [
        vec![entry("trains_pack", 2, false, Some("f1"))],
        vec![entry("trains_pack", 3, false, Some("f2"))],
        vec![entry("nice_ui", 2, true, Some("u1"))],
    ];
    for guest in cases {
        let (mut h, c, id) = round(Some(game(guest.clone())), Some(&WORLD));
        let r = h.p(c).rejected.clone().expect("removed");
        assert_eq!(r.reason, "mods", "{guest:?}");
        let world = r.mods.expect("the world's list for the diff");
        let ids: Vec<&str> = world.mods.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["nice_ui", "trains_pack"], "the world's mods, not every installed one");
        h.send_file(id, b"world");
        assert!(h.p(c).saves.is_empty(), "no world data reached it");
        assert_eq!(h.host.players().len(), 1);
    }
}

#[test]
fn cosmetic_differences_unknown_files_and_lists_that_cannot_be_read_warn_and_do_not_refuse() {
    // a cosmetic world mod with another revision, files unknown on one side, an extra mod of the player's
    let guest =
        vec![entry("trains_pack", 3, false, None), entry("nice_ui", 1, true, None), entry("my_extra", 1, false, None)];
    let (h, c, _) = round(Some(game(guest)), Some(&WORLD));
    assert!(h.p(c).rejected.is_none());
    assert!(warnings(&h).iter().any(|w| w.contains("nice_ui")), "{:?}", warnings(&h));
    // the player's game could not list its mods
    let unread = ModFingerprint { unread: true, ..game(Vec::new()) };
    let (h, c, _) = round(Some(unread), Some(&WORLD));
    assert!(h.p(c).rejected.is_none());
    assert!(warnings(&h).iter().any(|w| w.contains("could not list")), "{:?}", warnings(&h));
    // the host's world could not list its mods: said in every window, the world goes out
    let (mut h, c, id) = round(Some(game(Vec::new())), None);
    assert!(h.p(c).rejected.is_none());
    assert!(warnings(&h).iter().any(|w| w.contains("world could not list")), "{:?}", warnings(&h));
    h.send_file(id, b"world");
    assert_eq!(h.p(c).saves.len(), 1);
    let seen: SessionState = h.p(c).last(MessageKind::Session);
    assert_eq!(seen.mod_warnings, warnings(&h), "the player's window gets the warnings too");
}

#[test]
fn another_game_build_feverlink3_or_native_module_refuses_at_join() {
    for change in 0..3 {
        let mut guest = game(host_mods());
        match change {
            0 => guest.game = "1.4.3".into(),
            1 => guest.feverlink3 = "0.1.9".into(),
            _ => guest.native = "cd34".into(),
        }
        let mut h = Harness::with(HostOptions { mods: Some(game(host_mods())), ..HostOptions::default() });
        let c = h.connect_with_mods("Pat", Some(guest));
        assert_eq!(h.p(c).rejected.as_ref().map(|r| r.reason.as_str()), Some("mods"));
        assert_eq!(h.host.players().len(), 1);
    }
}

#[test]
fn old_peers_get_a_version_message() {
    let mut h = Harness::with(HostOptions { mods: Some(game(host_mods())), ..HostOptions::default() });
    let c = h.connect_with_mods("Old", None);
    let r = h.p(c).rejected.clone().expect("refused");
    assert_eq!(r.reason, "version");
    assert!(r.detail.unwrap().contains("compares mod lists"));

    let mut h = Harness::new(); // a host that predates the check: no mods_checked in its welcome
    let c = h.connect_with_mods("Pat", Some(game(host_mods())));
    assert_eq!(h.p(c).rejected.as_ref().map(|r| r.reason.as_str()), Some("version"));
    assert_eq!(h.p(c).count(MessageKind::Welcome), 0, "the game never hears of the session");
    assert_eq!(h.host.players().len(), 1, "the host freed the slot (Leave)");
}

#[test]
fn the_join_stays_inside_the_first_frame_limit_with_many_mods() {
    let many: Vec<ModEntry> =
        (0..2000).map(|i| entry(&format!("some_rather_long_mod_identifier_{i:05}"), 12, false, Some("ff"))).collect();
    let fp = game(many);
    let tags = fp.tags();
    assert!(tags.more);
    let join = JoinMessage { mods: Some(tags), ..JoinMessage::new(&"N".repeat(24), "0.1", vec![0; 32]) };
    let len = codec::encode(&join).len();
    assert!(len + 12 < FrameCodec::HANDSHAKE_MAX_LENGTH - 200, "{len}");
    assert_eq!(fp.tags().tags.len(), (MAX_JOIN_MODS * 8).div_ceil(3) * 4);
}

#[test]
fn the_players_diff_reads_like_the_window_shows_it() {
    let host = game(vec![
        entry("trains_pack", 3, false, Some("f1")),
        entry("maps", 1, false, Some("m1")),
        entry("nice_ui", 2, true, Some("u1")),
        entry("signals", 4, false, Some("s1")),
    ]);
    let mut mine = game(vec![
        entry("trains_pack", 2, false, Some("f1")),
        entry("nice_ui", 1, true, Some("u1")),
        entry("signals", 4, false, Some("s2")),
        entry("my_extra", 1, false, None),
    ]);
    mine.feverlink3 = "0.1.9".into();
    let lines: Vec<(bool, String)> = diff(&host, &mine).into_iter().map(|d| (d.blocks, d.text)).collect();
    assert_eq!(
        lines,
        vec![
            (true, "FeverLink3: host has 0.2.0, you have 0.1.9".to_owned()),
            (true, "missing maps (host has revision 1)".to_owned()),
            (true, "signals revision 4: same revision, different files".to_owned()),
            (true, "trains_pack: host has revision 3, you have revision 2".to_owned()),
            (false, "nice_ui: host has revision 2, you have revision 1 (cosmetic: warning only)".to_owned()),
            (false, "only you have my_extra; the host's world does not load them".to_owned()),
        ]
    );
}
