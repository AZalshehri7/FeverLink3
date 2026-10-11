//! Command line of the launcher. The real UI comes later.
//!
//! ```text
//! feverlink3                        no arguments, which is what a double-click is: the menu in `wizard.rs`
//! feverlink3 host [--port 28090] [--password P] [--name N] [--launch]
//! feverlink3 join CODE [--password P] [--name N] [--launch]
//! feverlink3 selftest               a host and two players over loopback, fake games, no game started
//! feverlink3 doctor                 what is found on this machine; changes nothing
//! feverlink3 install [--dry-run]    install the game-side pieces (or show the plan)
//! feverlink3 uninstall [--dry-run]  remove exactly those pieces (or show the plan)
//! ```
//!
//! `host` and `join` run until the session ends or `quit` is typed. The game is started only with `--launch`.
//! While a host runs, these commands can be typed: `start`, `pause`, `speed N` (1 to 4), `check`, `resync`, `status`, `quit`.

mod wizard;
// shared with build.rs, which writes the icon, version information and manifest of this executable; the tests of
// that code run here
#[cfg(test)]
#[path = "../resources/winres.rs"]
mod winres;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Duration;

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::lobby::JoinCode;
use feverlink3_core::session::HostOptions;
use feverlink3_launcher::install::{self, Payload};
use feverlink3_launcher::process::{self, Launch, Role};
use feverlink3_launcher::session::{self, Command, HostConfig, PlayerConfig, SessionHandle, Status};
use feverlink3_launcher::{gamelink, net, selftest, steam, MOD_ID};

const USAGE: &str = "usage: feverlink3 host [--port 28090] [--password P] [--name N] [--launch]\n       \
                     feverlink3 join CODE [--password P] [--name N] [--launch]\n       \
                     feverlink3 selftest | doctor | install [--dry-run] | uninstall [--dry-run]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    match args.first().map(String::as_str) {
        // a double-click starts the program without arguments; the console must not close under the player's eyes
        None => wizard::run_interactive(),
        Some("doctor") => doctor(),
        Some("install") => footprint(dry_run, false),
        Some("uninstall") => footprint(dry_run, true),
        Some("selftest" | "--selftest") => run_selftest(),
        Some("host") => fail_with(host(&args[1..])),
        Some("join") => fail_with(join(&args[1..])),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn fail_with(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// host / join
// ---------------------------------------------------------------------------------------------------------------

/// The options of `host` and `join`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    positional: Vec<String>,
    port: Option<u16>,
    password: Option<String>,
    name: Option<String>,
    launch: bool,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut o = Options::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| it.next().cloned().ok_or_else(|| format!("{flag} needs a value\n{USAGE}"));
        match arg.as_str() {
            "--port" => o.port = Some(value("--port")?.parse().map_err(|_| "--port must be a number from 1 to 65535")?),
            "--password" => o.password = Some(value("--password")?),
            "--name" => o.name = Some(value("--name")?),
            "--launch" => o.launch = true,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}\n{USAGE}")),
            _ => o.positional.push(arg.clone()),
        }
    }
    Ok(o)
}

/// The `save` folders of every Steam user of this machine, where the game keeps savegames (empty without Steam).
fn game_save_dirs() -> Vec<PathBuf> {
    feverlink3_launcher::resync::save_dirs(&steam::discover().user_dirs)
}

fn default_name() -> String {
    feverlink3_launcher::steam_persona::FALLBACK_NAME.to_owned()
}

/// Lines typed on stdin, from a thread of their own so the main thread can watch the session.
fn stdin_lines() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
            if tx.send(line.trim().to_owned()).is_err() {
                break;
            }
            line.clear();
        }
    });
    rx
}

fn print_status(s: &Status) {
    println!("{} ({}), id {:?}, {}", s.name, s.role, s.my_id, if s.connected { "connected" } else { "not connected" });
    if let Some(session) = &s.session {
        println!(
            "session: started {}, speed {}, pause at {:?}, lead {}",
            session.started, session.speed, session.pause_at, session.lead
        );
    }
    for p in &s.players {
        let clock = s.clocks.iter().find(|c| c.id == p.id).map_or(String::new(), |c| format!(", clock {}", c.t));
        println!("  #{} {}{}{}", p.id, p.name, if p.ready { " (ready)" } else { "" }, clock);
    }
    if let Some(sync) = &s.last_sync {
        println!(
            "last sync: round {}, {}",
            sync.n,
            if sync.identical() { "identical".into() } else { sync.differing.join(", ") }
        );
    }
    println!("actions relayed {}, refused {}", s.actions_relayed, s.dropped);
}

/// Runs until the session ends or `quit`. Host commands are read from stdin.
fn supervise(handle: &SessionHandle, host: bool) {
    let lines = stdin_lines();
    loop {
        if handle.is_finished() {
            return;
        }
        match lines.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                let mut words = line.split_whitespace();
                match (words.next(), words.next().and_then(|w| w.parse::<i32>().ok())) {
                    (Some("quit" | "exit"), _) => return,
                    (Some("status"), _) => print_status(&handle.status()),
                    (Some("start"), _) if host => handle.command(Command::Start),
                    (Some("pause"), _) if host => handle.command(Command::Pause),
                    (Some("speed"), Some(n)) if host => handle.command(Command::Speed(n)),
                    (Some("check"), _) if host => handle.command(Command::Checksum),
                    (Some("resync"), _) if host => handle.command(Command::Resync),
                    (Some(other), _) => eprintln!(
                        "unknown command {other:?}; try status, quit{}",
                        if host { ", start, pause, speed N, check, resync" } else { "" }
                    ),
                    (None, _) => {}
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn start_game(role: Role, id: u16, name: &str, session_dir: &Path) -> Result<(), String> {
    let info = steam::discover();
    let game_dir = info.game_dir.ok_or("Transport Fever 3 was not found; run `feverlink3 doctor`.")?;
    let launch = Launch {
        game_dir,
        session_dir: session_dir.to_path_buf(),
        player_id: id,
        player_name: name.to_owned(),
        role,
        args: Vec::new(),
    };
    let game = process::start(&launch).map_err(|e| format!("cannot start the game: {e}"))?;
    println!("game started (pid {})", game.pid());
    Ok(())
}

fn host(args: &[String]) -> Result<(), String> {
    let o = parse_options(args)?;
    if !o.positional.is_empty() {
        return Err(format!("unexpected argument {:?}\n{USAGE}", o.positional[0]));
    }
    let port = o.port.unwrap_or(28090);
    let name = o.name.unwrap_or_else(default_name);
    let secret = SessionKey::new_secret();
    let key = SessionKey::derive(&secret, o.password.as_deref());
    let link = gamelink::Session::create(&name).map_err(|e| format!("cannot create the session folder: {e}"))?;
    let session_dir = link.dir().to_path_buf();
    let handle = session::run_host(HostConfig {
        bulk: feverlink3_launcher::steamnet::bulk_io::enabled(),
        bind: SocketAddr::from(([0, 0, 0, 0], port)),
        name: name.clone(),
        key,
        link,
        options: HostOptions::default(),
        save_dirs: game_save_dirs(),
        echo_log: true,
    })
    .map_err(|e| format!("cannot listen on port {port}: {e}"))?;
    let bound = handle.local_addr().map_or(port, |a| a.port());
    let endpoints = net::lan_endpoints(bound);
    let code = JoinCode::create(&endpoints, secret, None).map_err(|e| e.to_string())?;
    println!("Join code (share it{}):\n{}", if o.password.is_some() { " and the password" } else { "" }, code.encode());
    for ep in &endpoints {
        println!("  endpoint {ep}");
    }
    println!("Direct TCP is the developer transport (same PC or LAN). Players on other networks join through the in-game window, over Steam.");
    println!("Session folder: {}", session_dir.display());
    if o.launch {
        start_game(Role::Host, 0, &name, &session_dir)?;
    } else {
        println!(
            "The game is not started (use --launch). Commands: start, pause, speed N, check, resync, status, quit."
        );
    }
    supervise(&handle, true);
    handle.stop();
    Ok(())
}

fn join(args: &[String]) -> Result<(), String> {
    let o = parse_options(args)?;
    let [code_text] = o.positional.as_slice() else {
        return Err(format!("join needs exactly one join code\n{USAGE}"));
    };
    if o.port.is_some() {
        return Err("--port is for host".into());
    }
    let code = JoinCode::try_decode(code_text).ok_or("that is not a valid join code")?;
    let name = o.name.unwrap_or_else(default_name);
    let key = SessionKey::derive(&code.secret, o.password.as_deref());
    let link = gamelink::Session::create(&name).map_err(|e| format!("cannot create the session folder: {e}"))?;
    let session_dir = link.dir().to_path_buf();
    let mut config = PlayerConfig::new(code.endpoints.clone(), &name, key, link);
    config.echo_log = true;
    config.save_dirs = game_save_dirs();
    let handle = session::run_player(config).map_err(|e| format!("cannot reach the host: {e}"))?;
    println!("Session folder: {}", session_dir.display());
    if o.launch {
        // the game needs the id the host assigns, so wait for the welcome
        let welcomed =
            handle.wait_for(Duration::from_secs(15), |s| s.my_id.is_some() || s.rejected.is_some() || s.finished);
        let status = handle.status();
        match (welcomed, status.my_id, status.rejected) {
            (_, _, Some(why)) => return Err(format!("the host rejected us: {why}")),
            (true, Some(id), None) => start_game(Role::Player, id, &status.name, &session_dir)?,
            _ => return Err("the host did not welcome us within 15 s".into()),
        }
    } else {
        println!("The game is not started (use --launch). Commands: status, quit.");
    }
    supervise(&handle, false);
    let status = handle.status();
    handle.stop();
    match status.rejected {
        Some(why) => Err(format!("the host rejected us: {why}")),
        None => status.error.map_or(Ok(()), Err),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// selftest
// ---------------------------------------------------------------------------------------------------------------

fn run_selftest() -> ExitCode {
    match selftest::run() {
        Ok(report) => {
            for check in &report.checks {
                println!("ok   {check}");
            }
            println!("selftest passed: {} checks in {:.1} s", report.checks.len(), report.elapsed.as_secs_f32());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("selftest {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// doctor / install / uninstall
// ---------------------------------------------------------------------------------------------------------------

/// What is found on this machine. The text is built by the `wizard` module, which also puts it into its diagnostics.
fn doctor() -> ExitCode {
    print!("{}", wizard::doctor_report());
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
/// (`mod/feverlink3_1/`, `target/release/feverlink3_native.dll`).
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
            for candidate in [d.join("winhttp.dll"), d.join("target").join("release").join("feverlink3_native.dll")] {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn options_parse() {
        let o = parse_options(&args(&["--port", "1234", "--password", "pw", "--name", "Ann", "--launch"])).unwrap();
        assert_eq!(
            o,
            Options {
                positional: vec![],
                port: Some(1234),
                password: Some("pw".into()),
                name: Some("Ann".into()),
                launch: true
            }
        );
        let o = parse_options(&args(&["CODE", "--name", "B"])).unwrap();
        assert_eq!(o.positional, vec!["CODE".to_owned()]);
        assert!(!o.launch, "the game starts only with --launch");
        assert!(parse_options(&args(&["--port"])).is_err());
        assert!(parse_options(&args(&["--port", "x"])).is_err());
        assert!(parse_options(&args(&["--port", "70000"])).is_err());
        assert!(parse_options(&args(&["--bogus"])).is_err());
    }
}
