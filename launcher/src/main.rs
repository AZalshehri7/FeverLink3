//! Tiny command line for the platform layer. The real UI comes later.
//!
//! ```text
//! feverlink3 doctor                 what is found on this machine; changes nothing
//! feverlink3 install [--dry-run]    install the game-side pieces (or show the plan)
//! feverlink3 uninstall [--dry-run]  remove exactly those pieces (or show the plan)
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use feverlink3_launcher::install::{self, Payload};
use feverlink3_launcher::{process, steam, MOD_ID};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    match args.first().map(String::as_str) {
        Some("doctor") => doctor(),
        Some("install") => footprint(dry_run, false),
        Some("uninstall") => footprint(dry_run, true),
        _ => {
            eprintln!("usage: feverlink3 doctor | install [--dry-run] | uninstall [--dry-run]");
            ExitCode::from(2)
        }
    }
}

fn doctor() -> ExitCode {
    let info = steam::discover();
    println!("Steam:           {}", info.steam_path.as_deref().map_or("not found in the registry".into(), path_text));
    for lib in &info.libraries {
        println!("library:         {}", lib.display());
    }
    println!(
        "game:            {}",
        info.game_dir.as_deref().map_or("TransportFever3.exe not found in any library".into(), path_text)
    );
    if info.user_dirs.is_empty() {
        println!(
            "user folders:    none (userdata/<id>/3493540/local does not exist yet; the game creates it on first run)"
        );
    }
    for u in &info.user_dirs {
        let state = if install::mod_installed(u) { "mod installed" } else { "mod not installed" };
        println!("user folder:     {}  ({state})", u.display());
    }
    if let Some(game) = &info.game_dir {
        match install::dll_state(game) {
            Ok(state) => println!("winhttp.dll:     {state}"),
            Err(e) => println!("winhttp.dll:     unreadable ({e})"),
        }
        match install::appid_state(game) {
            Ok(install::AppIdState::Absent) => println!("steam_appid.txt: absent"),
            Ok(install::AppIdState::Ours) => println!("steam_appid.txt: present (3493540)"),
            Ok(install::AppIdState::Other(c)) => println!("steam_appid.txt: present with other content {c:?}"),
            Err(e) => println!("steam_appid.txt: unreadable ({e})"),
        }
        match install::recorded_launcher_path(game) {
            Some(p) => println!("launcher path:   {}", p.display()),
            None => println!("launcher path:   not recorded (feverlink3_path.txt absent)"),
        }
    }
    let (mod_src, dll_src) = locate_payload();
    println!("payload mod:     {}", mod_src.as_deref().map_or("not found next to this executable".into(), path_text));
    println!("payload dll:     {}", dll_src.as_deref().map_or("not found next to this executable".into(), path_text));
    println!("game running:    {}", if process::any_game_running() { "yes" } else { "no" });
    ExitCode::SUCCESS
}

fn footprint(dry_run: bool, remove: bool) -> ExitCode {
    let info = steam::discover();
    let Some(game_dir) = info.game_dir.as_deref() else {
        eprintln!("Transport Fever 3 was not found; run `feverlink3 doctor`.");
        return ExitCode::FAILURE;
    };
    if !dry_run && process::any_game_running() {
        eprintln!("Transport Fever 3 is running; close it first.");
        return ExitCode::FAILURE;
    }
    let report = if remove {
        install::uninstall(game_dir, &info.user_dirs, dry_run)
    } else {
        let (mod_src, dll_src) = locate_payload();
        let launcher = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("FeverLink3.exe"));
        let payload =
            Payload { mod_source: mod_src.as_deref(), native_dll: dll_src.as_deref(), launcher_path: &launcher };
        install::install(game_dir, &info.user_dirs, payload, dry_run)
    };
    print!("{report}");
    if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The mod folder and native module shipped with this executable. In a release zip both sit next to the exe
/// (`mod/feverlink3_1/`, `winhttp.dll`); in a source tree they are found by walking up to the repository root
/// (`mod/feverlink3_1/`, `native/target/release/feverlink3_native.dll`).
fn locate_payload() -> (Option<PathBuf>, Option<PathBuf>) {
    let start = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let mut mod_src = None;
    let mut dll_src = None;
    let mut dir = start;
    while let Some(d) = dir {
        if mod_src.is_none() && d.join("mod").join(MOD_ID).is_dir() {
            mod_src = Some(d.join("mod").join(MOD_ID));
        }
        if dll_src.is_none() {
            for candidate in
                [d.join("winhttp.dll"), d.join("native").join("target").join("release").join("feverlink3_native.dll")]
            {
                if candidate.is_file() {
                    dll_src = Some(candidate);
                    break;
                }
            }
        }
        if mod_src.is_some() && dll_src.is_some() {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    (mod_src, dll_src)
}

fn path_text(p: &Path) -> String {
    p.display().to_string()
}
