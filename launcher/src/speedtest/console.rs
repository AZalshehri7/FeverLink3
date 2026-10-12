//! The console program: banner, role, Steam start, the tester or responder loop, the files, the final keypress.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::cli::{choose_role, parse_args, Role, Source, USAGE};
use super::responder::run_responder;
use super::results::{log_line, results_text};
use super::startup::InitError;
use super::suite::{run_tester, Env, Timing, CLASSIC_MODES, MODES};
use super::{now_text, RESPONDER_FILE, RESULTS_FILE, VERSION};

static CANCELLED: AtomicBool = AtomicBool::new(false);

/// True after Ctrl+C (or closing the window with Ctrl+Break).
pub fn cancelled() -> bool {
    CANCELLED.load(Ordering::Relaxed)
}

#[cfg(windows)]
mod sys {
    use super::CANCELLED;
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
        fn GetStdHandle(which: u32) -> *mut c_void;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
    }
    extern "C" {
        fn _getch() -> i32;
    }

    unsafe extern "system" fn on_ctrl(kind: u32) -> i32 {
        // CTRL_C_EVENT 0, CTRL_BREAK_EVENT 1
        if kind <= 1 {
            CANCELLED.store(true, Ordering::Relaxed);
            1
        } else {
            0
        }
    }

    pub fn install_ctrl_c() {
        // SAFETY: registers a handler that only sets an atomic flag.
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) };
    }

    pub fn wait_for_key() {
        // SAFETY: standard handle query and a console key read, only when stdin is a console.
        unsafe {
            let mut mode = 0u32;
            if GetConsoleMode(GetStdHandle(-10i32 as u32), &mut mode) != 0 {
                _getch();
            }
        }
    }
}

#[cfg(not(windows))]
mod sys {
    pub fn install_ctrl_c() {}
    pub fn wait_for_key() {}
}

fn println_out(text: &str) {
    println!("{text}");
    let _ = std::io::stdout().flush();
}

fn finish(code: i32) -> i32 {
    if !cancelled() {
        println_out("\nPress any key to close this window.");
        sys::wait_for_key();
    }
    code
}

fn exe_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find the program's own folder: {e}"))?;
    exe.parent().map(Path::to_path_buf).ok_or_else(|| "cannot find the program's own folder".to_string())
}

/// The program. Returns the exit code.
pub fn main_entry() -> i32 {
    println_out(&format!("FeverLink3 Steam speed test, version {VERSION}"));
    println_out("This program only talks to the Steam network and to the one other PC taking part.\n");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_args(&args) {
        Ok(cli) => cli,
        Err(e) => {
            println_out(&format!("Problem with the command line: {e}\n\n{USAGE}"));
            return finish(2);
        }
    };
    if cli.help {
        println_out(USAGE);
        return 0;
    }
    if let Some(rate) = cli.rate {
        // read once by the Steam core when it starts (FEVERLINK3_STEAM_SENDRATE), so it is set before Steam is started
        std::env::set_var("FEVERLINK3_STEAM_SENDRATE", rate.to_string());
        println_out(&format!("Steam send rate for this run: {rate} bytes per second (--rate)."));
    }
    let dir = match exe_dir() {
        Ok(d) => d,
        Err(e) => {
            println_out(&e);
            return finish(1);
        }
    };
    let (role, source) = match choose_role(&cli, &dir) {
        Ok(r) => r,
        Err(e) => {
            println_out(&format!("Problem: {e}"));
            return finish(2);
        }
    };
    match (role, source) {
        (Role::Tester(peer), Source::PeerFile) => {
            println_out(&format!("Role: TESTER (peer.txt names Steam ID {peer})."))
        }
        (Role::Tester(peer), _) => println_out(&format!("Role: TESTER (command line names Steam ID {peer}).")),
        (Role::Responder, Source::CommandLine) => println_out("Role: RESPONDER (--listen)."),
        (Role::Responder, _) => println_out("Role: RESPONDER (there is no peer.txt in this folder)."),
    }
    sys::install_ctrl_c();
    let (core, app_id, pump) = match start_steam(&dir) {
        Ok(started) => started,
        Err(e) => {
            println_out(&format!("\nCannot start:\n{}", e.explain()));
            return finish(1);
        }
    };
    println_out(&format!("Steam started (app id {app_id}). Both PCs must use the same app id."));
    let mut say = |t: &str| println_out(t);
    let mut pump = pump;
    let mut env = Env { say: &mut say, pump: &mut pump, cancelled: &cancelled, timing: Timing::real() };
    let code = match role {
        Role::Tester(peer) => tester(&mut env, &core, peer, &dir, app_id, cli.quick, cli.classic),
        Role::Responder => responder(&mut env, &core, &dir),
    };
    finish(code)
}

type Pump = Box<dyn FnMut()>;

#[cfg(windows)]
fn start_steam(dir: &Path) -> Result<(crate::steamnet::CoreRef, u32, Pump), InitError> {
    let session = super::startup::start(dir)?;
    let core = crate::steamnet::global().ok_or(InitError::CoreNotStarted)?;
    let app_id = session.app_id;
    Ok((core, app_id, Box::new(move || session.pump())))
}

#[cfg(not(windows))]
fn start_steam(_dir: &Path) -> Result<(crate::steamnet::CoreRef, u32, Pump), InitError> {
    Err(InitError::Failed { code: 1, message: "this program runs on Windows only".into() })
}

fn tester(
    env: &mut Env<'_>,
    core: &crate::steamnet::CoreRef,
    peer: u64,
    dir: &Path,
    app_id: u32,
    quick: bool,
    classic: bool,
) -> i32 {
    let path = dir.join(RESULTS_FILE);
    let mut write_failed = false;
    let mut save = |report: &super::suite::TesterReport| {
        let text = results_text(report, &now_text(), app_id);
        if std::fs::write(&path, text.as_bytes()).is_err() && !write_failed {
            write_failed = true;
            println_out(&format!(
                "Warning: could not write {}. Move the program to a folder you may write to.",
                path.display()
            ));
        }
    };
    let modes = if quick {
        &CLASSIC_MODES[..1]
    } else if classic {
        CLASSIC_MODES
    } else {
        MODES
    };
    let report = run_tester(env, core, peer, modes, &mut save);
    println_out("");
    if report.aborted {
        println_out(&format!("Stopped. What was measured so far is in {}", path.display()));
        return 1;
    }
    if let Some(error) = &report.error {
        println_out(&format!("The test could not run:\n{error}"));
        println_out(&format!("\nPlease send {} anyway; it says what happened.", path.display()));
        return 1;
    }
    println_out(&format!("All tests are done. Please send this file: {}", path.display()));
    0
}

fn responder(env: &mut Env<'_>, core: &crate::steamnet::CoreRef, dir: &Path) -> i32 {
    let path = dir.join(RESPONDER_FILE);
    let mut log = |text: &str| {
        let line = log_line(&now_text(), text);
        println_out(&format!("  {}", line.trim_end()));
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(line.as_bytes());
        }
    };
    match run_responder(env, core, &mut log) {
        Ok(None) | Err(_) => {
            println_out(&format!("\nStopped. What this program answered is logged in {}", path.display()));
            0
        }
        Ok(Some(why)) => {
            println_out(&format!("\nCannot wait for testers:\n{why}"));
            1
        }
    }
}
