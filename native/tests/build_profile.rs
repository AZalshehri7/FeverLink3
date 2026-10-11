//! The release profile must unwind. With `panic = "abort"` every Rust frame is a terminate pad for a foreign (C++)
//! exception, so the first exception of the game that passed through a hook ended the process with "panic in a function
//! that cannot unwind" (the crash of 2026-10-11, `docs/NATIVE_LINK.md` section 10). The fake-engine tests cannot show
//! this (tests always unwind), so the setting itself is pinned here.

#[test]
fn the_release_profile_unwinds() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../Cargo.toml")).unwrap();
    let mut in_release = false;
    let mut panic_setting = None;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_release = line == "[profile.release]";
        } else if in_release && line.starts_with("panic") {
            panic_setting = Some(line.split('=').nth(1).unwrap().trim().trim_matches('"').to_string());
        }
    }
    assert_eq!(
        panic_setting.as_deref(),
        Some("unwind"),
        "[profile.release] panic must be \"unwind\": see docs/NATIVE_LINK.md section 10"
    );
}
