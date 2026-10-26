//! Every integration test of the launcher in ONE test binary (`autotests = false` in launcher/Cargo.toml), as
//! core/tests/all.rs (which says why): the seven files ran one binary after the other, about 10 s on 2026-10-26, the
//! slowest alone 3.6 s. The files keep their own `mod multi_support;`. Only robustness.rs's counting allocator becomes
//! the allocator of the whole binary; it counts per thread, so the other files' tests do not disturb its numbers.
//!
//! A new file in launcher/tests must be named in `suites!` below (`every_test_file_is_in_this_binary` fails otherwise).
//! One file alone: `cargo test -p feverlink3_launcher --test all selftest::`.

// each file keeps its own `mod multi_support;`, so the same helper file is a module several times: on purpose
#![allow(clippy::duplicate_mod)]

macro_rules! suites {
    ($($m:ident = $p:literal),* $(,)?) => {
        $( #[path = $p] mod $m; )*
        const SUITES: &[&str] = &[$($p),*];
    };
}

suites! {
    eight_players = "eight_players.rs",
    link_latency = "link_latency.rs",
    no_child_process = "no_child_process.rs",
    reconnect = "reconnect.rs",
    resync = "resync.rs",
    robustness = "robustness.rs",
    selftest = "selftest.rs",
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
    assert_eq!(files, listed, "launcher/tests/*.rs and the suites! list of all.rs name the same files");
}
