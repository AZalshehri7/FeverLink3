//! Every integration test of the core in ONE test binary (`autotests = false` in core/Cargo.toml).
//!
//! Why: `cargo test` runs test binaries one after the other, each with its own threads. With one binary per file the
//! core's 27 files ran in series (about 22 s of a 41 s essential run on 2026-10-26, most binaries idle on one slow
//! test); in one binary they share the thread pool. It also links one executable instead of 27. The files stay where
//! they are and keep their own `mod common;` / `mod net;` (loaded once per file, as before).
//!
//! A new file in core/tests must be named in `suites!` below: `every_test_file_is_in_this_binary` fails otherwise, so a
//! file can never be skipped silently. One file alone: `cargo test -p feverlink3_core --test all model_tests::`.

// each file keeps its own `mod common;` / `mod net;`, so the same helper file is a module several times: on purpose
#![allow(clippy::duplicate_mod)]

macro_rules! suites {
    ($($m:ident = $p:literal),* $(,)?) => {
        $( #[path = $p] mod $m; )*
        const SUITES: &[&str] = &[$($p),*];
    };
}

suites! {
    frame_tests = "frame_tests.rs",
    hardening_tests = "hardening_tests.rs",
    hostile_input_tests = "hostile_input_tests.rs",
    join_code_relay_tests = "join_code_relay_tests.rs",
    join_code_steam_tests = "join_code_steam_tests.rs",
    join_code_tests = "join_code_tests.rs",
    lead_tests = "lead_tests.rs",
    limits_tests = "limits_tests.rs",
    link_latency_tests = "link_latency_tests.rs",
    liveness_tests = "liveness_tests.rs",
    model_tests = "model_tests.rs",
    mods_tests = "mods_tests.rs",
    permissions_tests = "permissions_tests.rs",
    players_tests = "players_tests.rs",
    policy_tests = "policy_tests.rs",
    reconnect_tests = "reconnect_tests.rs",
    relay_tests = "relay_tests.rs",
    report_tests = "report_tests.rs",
    resilience_tests = "resilience_tests.rs",
    resume_tests = "resume_tests.rs",
    resync_tests = "resync_tests.rs",
    robustness_tests = "robustness_tests.rs",
    sealed_channel_tests = "sealed_channel_tests.rs",
    session_tests = "session_tests.rs",
    stall_tests = "stall_tests.rs",
    wire_limits_tests = "wire_limits_tests.rs",
    wire_tests = "wire_tests.rs",
}

#[test]
fn every_test_file_is_in_this_binary() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut files: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".rs") && n != "all.rs")
        .collect();
    files.sort();
    let mut listed: Vec<String> = SUITES.iter().map(|s| s.to_string()).collect();
    listed.sort();
    assert_eq!(files, listed, "core/tests/*.rs and the suites! list of all.rs name the same files");
}
