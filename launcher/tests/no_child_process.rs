//! No code that runs inside the game starts a program.
//!
//! Real game 2026-10-22 (0.2.0): the automatic update started `curl.exe` from the game process. A child process
//! started that way inherits every inheritable handle of the game; the game's rename of
//! `shader_cache\pipelines.cache.tmp` then failed "being used by another process" and the game stopped with
//! "Internal error". Since 0.2.1 the in-game code downloads with WinHTTP in the process (`src/https.rs`), opens folders
//! and pages through the shell (`src/live/report.rs`) and copies through the clipboard API (`src/clipboard.rs`).
//!
//! This check reads the sources of the library the game loads (`launcher/src`, `native/src`) and fails on any way to
//! start a process. Only the files of the developer program `feverlink3.exe`, which never runs inside the game, may
//! start programs; they are listed in [`DEVELOPER_ONLY`]. Comments are not code and are skipped.

use std::fs;
use std::path::{Path, PathBuf};

/// Files and folders (relative to the repository, `/`-separated) that only the developer program or a test runs.
const DEVELOPER_ONLY: &[&str] = &[
    // the `feverlink3` binary: its entry point, the update command, the menu
    "launcher/src/main.rs",
    "launcher/src/updatecmd.rs",
    "launcher/src/wizard.rs",
    "launcher/src/wizard/",
    "launcher/src/bin/",
    // starting the game and listing processes: called only by the binary (main.rs, updatecmd.rs, wizard)
    "launcher/src/process.rs",
    // the native tests start their own test executable
    "native/src/runtime/tests/",
];

/// The ways Rust code can start a program on Windows.
const FORBIDDEN: &[&str] = &[
    "process::Command",
    "Command::new",
    "CreateProcess",
    "WinExec",
    "ShellExecuteEx",
    "_wsystem",
    "libc::system",
    "process::{",
];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn code_inside_the_game_starts_no_program() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let mut files = Vec::new();
    sources(&repo.join("launcher/src"), &mut files);
    sources(&repo.join("native/src"), &mut files);
    assert!(files.len() > 50, "the sources were not found under {}", repo.display());
    let mut found = Vec::new();
    for file in &files {
        let rel = file.strip_prefix(&repo).unwrap().to_string_lossy().replace('\\', "/");
        if DEVELOPER_ONLY.iter().any(|d| if d.ends_with('/') { rel.starts_with(d) } else { rel == *d }) {
            continue;
        }
        let text = fs::read_to_string(file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            // `process::{` is only a problem when it imports `Command`
            for f in FORBIDDEN {
                if code.contains(f) && (*f != "process::{" || code.contains("Command")) {
                    found.push(format!("{rel}:{}: {}", n + 1, code));
                }
            }
        }
    }
    assert!(found.is_empty(), "code that may run inside the game starts a program:\n{}", found.join("\n"));
}

#[test]
fn the_check_finds_what_it_looks_for() {
    // the developer program does start programs: if the check could not see them there, it could not see them anywhere
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
    let text = fs::read_to_string(repo.join("launcher/src/process.rs")).unwrap();
    assert!(FORBIDDEN.iter().any(|f| text.contains(f)));
}
