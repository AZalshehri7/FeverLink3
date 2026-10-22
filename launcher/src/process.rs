//! Starting the game with the session environment.
//!
//! `TransportFever3.exe` is started directly (not through Steam, so several copies can run on one PC and the
//! environment reaches the process; `steam_appid.txt` in the game folder makes Steam's API accept that, see
//! [`crate::install`]). The working directory is the game folder. The native module and the Lua mod find their
//! session through these variables:
//!
//! | Variable                 | Value                                        |
//! |--------------------------|----------------------------------------------|
//! | `FEVERLINK3_SESSION_DIR` | the session folder ([`crate::gamelink`])     |
//! | `FEVERLINK3_PLAYER_ID`   | the sender id the host assigned (0 = host)   |
//! | `FEVERLINK3_PLAYER_NAME` | the player's display name                    |
//! | `FEVERLINK3_ROLE`        | `host` or `player`                           |
//!
//! [`command`] builds the `Command` without running it, so it can be checked in tests; [`start`] runs it.
//!
//! Steam itself must never carry these variables: a Steam started from an environment holding them passes them on
//! to every game it starts later (friend test 2026-10-21: a test script's `steam.exe -applaunch` found Steam closed,
//! so Steam started with `FEVERLINK3_TRANSPORT`/`FEVERLINK3_LIVE_DIR` and the owner's next normal game inherited
//! them). [`without_feverlink3_env`] strips them from a Steam command; [`steam_running`] lets a direct game start
//! refuse while Steam is closed, because the game's Steam API could then start Steam with the session variables.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::GAME_EXE;

pub const ENV_SESSION_DIR: &str = "FEVERLINK3_SESSION_DIR";
pub const ENV_PLAYER_ID: &str = "FEVERLINK3_PLAYER_ID";
pub const ENV_PLAYER_NAME: &str = "FEVERLINK3_PLAYER_NAME";
pub const ENV_ROLE: &str = "FEVERLINK3_ROLE";

/// Which side of the session this game is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Player,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Host => "host",
            Role::Player => "player",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything a game start needs.
#[derive(Debug, Clone)]
pub struct Launch {
    /// Folder holding `TransportFever3.exe`; also the working directory.
    pub game_dir: PathBuf,
    /// The session folder.
    pub session_dir: PathBuf,
    pub player_id: u16,
    pub player_name: String,
    pub role: Role,
    /// Extra command-line arguments for the game, if any.
    pub args: Vec<String>,
}

/// The command that [`start`] runs, not yet started.
pub fn command(launch: &Launch) -> Command {
    let mut cmd = Command::new(launch.game_dir.join(GAME_EXE));
    cmd.current_dir(&launch.game_dir)
        .args(&launch.args)
        .env(ENV_SESSION_DIR, &launch.session_dir)
        .env(ENV_PLAYER_ID, launch.player_id.to_string())
        .env(ENV_PLAYER_NAME, launch.player_name.replace(['\n', '\r'], " "))
        .env(ENV_ROLE, launch.role.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Prefix of every variable FeverLink3 reads.
pub const ENV_PREFIX: &str = "FEVERLINK3_";

/// Steam's executable, for [`steam_running`].
pub const STEAM_EXE: &str = "steam.exe";

/// True for a `FEVERLINK3_*` variable name. Windows names are case-insensitive, so the comparison is too.
pub fn is_feverlink3_var(name: &OsStr) -> bool {
    name.to_string_lossy().get(..ENV_PREFIX.len()).is_some_and(|p| p.eq_ignore_ascii_case(ENV_PREFIX))
}

/// Removes every `FEVERLINK3_*` variable from what `cmd` passes on: the ones inherited from this process and the ones
/// set on `cmd`. For commands that may start Steam, which must stay clean (see the module text).
pub fn without_feverlink3_env(cmd: &mut Command) -> &mut Command {
    let mut names: Vec<OsString> = std::env::vars_os().map(|(k, _)| k).filter(|k| is_feverlink3_var(k)).collect();
    names.extend(cmd.get_envs().map(|(k, _)| k.to_os_string()).filter(|k| is_feverlink3_var(k)));
    for name in names {
        cmd.env_remove(name);
    }
    cmd
}

/// `Some(true)` while Steam runs, `Some(false)` when it does not, `None` when `tasklist` cannot be asked.
pub fn steam_running() -> Option<bool> {
    count_running(STEAM_EXE).map(|n| n > 0)
}

/// A started game.
#[derive(Debug)]
pub struct Game {
    child: Child,
}

impl Game {
    /// The process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// True while the process has not exited. Does not block.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The exit code once the process ended, `None` while it runs or if it was killed by a signal.
    pub fn exit_code(&mut self) -> Option<i32> {
        self.child.try_wait().ok().flatten().and_then(|s| s.code())
    }

    /// Waits for the process to end and returns its exit code.
    pub fn wait(&mut self) -> io::Result<Option<i32>> {
        Ok(self.child.wait()?.code())
    }
}

/// Starts the game and returns the handle. Fails with a clear message when the executable is missing.
pub fn start(launch: &Launch) -> io::Result<Game> {
    let exe = launch.game_dir.join(GAME_EXE);
    if !exe.is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} not found", exe.display())));
    }
    let child = command(launch).spawn()?;
    Ok(Game { child })
}

/// True when any `TransportFever3.exe` runs on this machine, whoever started it. Used before touching
/// `winhttp.dll`, which Windows locks while the game runs. Uses `tasklist`, which every Windows has, so
/// the crate stays free of unsafe code; a failure to ask counts as "not running".
#[cfg(windows)]
pub fn any_game_running() -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {GAME_EXE}"), "/NH", "/FO", "CSV"])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_ascii_lowercase().contains(&GAME_EXE.to_ascii_lowercase()),
        Err(_) => false,
    }
}

#[cfg(not(windows))]
pub fn any_game_running() -> bool {
    false
}

/// How many processes with this image name (for example `feverlink3.exe`) run, by `tasklist`. `None` when
/// `tasklist` cannot be asked, so a caller that must be sure (the updater) can refuse instead of guessing.
#[cfg(windows)]
pub fn count_running(image: &str) -> Option<usize> {
    let o = Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {image}"), "/NH", "/FO", "CSV"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    o.status.success().then(|| count_csv_rows(&String::from_utf8_lossy(&o.stdout), image))
}

#[cfg(not(windows))]
pub fn count_running(_image: &str) -> Option<usize> {
    None
}

/// Rows of `tasklist /FO CSV /NH` output whose first field is `image` (case-insensitive).
#[cfg_attr(not(windows), allow(dead_code))]
fn count_csv_rows(csv: &str, image: &str) -> usize {
    let want = format!("\"{}\"", image.to_ascii_lowercase());
    csv.lines().filter(|l| l.trim().to_ascii_lowercase().split(',').next() == Some(want.as_str())).count()
}

/// The game executable of a game folder, for callers that only need the path.
pub fn game_exe(game_dir: &Path) -> PathBuf {
    game_dir.join(GAME_EXE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feverlink3_variables_are_recognised_in_any_case() {
        assert!(is_feverlink3_var(OsStr::new("FEVERLINK3_LIVE_DIR")));
        assert!(is_feverlink3_var(OsStr::new("feverlink3_transport")));
        assert!(!is_feverlink3_var(OsStr::new("FEVERLINK3")));
        assert!(!is_feverlink3_var(OsStr::new("FL3_DEMO_DEV")));
        assert!(!is_feverlink3_var(OsStr::new("PATH")));
    }

    #[test]
    fn a_scrubbed_command_passes_on_no_feverlink3_variable() {
        let mut cmd = Command::new("steam.exe");
        cmd.env("FEVERLINK3_TRANSPORT", "tcp-loopback").env("OTHER", "kept");
        without_feverlink3_env(&mut cmd);
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("FEVERLINK3_TRANSPORT"), None)));
        assert!(envs.contains(&(OsStr::new("OTHER"), Some(OsStr::new("kept")))));
        // every inherited FEVERLINK3_* (when the test runs in such a shell) is removed as well
        for (k, _) in std::env::vars_os().filter(|(k, _)| is_feverlink3_var(k)) {
            assert!(envs.contains(&(k.as_os_str(), None)));
        }
    }

    fn launch() -> Launch {
        Launch {
            game_dir: PathBuf::from(r"C:\Games\Transport Fever 3"),
            session_dir: PathBuf::from(r"C:\Temp\feverlink3\host-1-abcd"),
            player_id: 0,
            player_name: "Aziz\nnewline".into(),
            role: Role::Host,
            args: vec!["-arg".into()],
        }
    }

    fn env_of<'a>(cmd: &'a Command, key: &str) -> Option<&'a OsStr> {
        cmd.get_envs().find(|(k, _)| *k == OsStr::new(key)).and_then(|(_, v)| v)
    }

    #[test]
    fn command_has_exe_working_dir_and_environment() {
        let l = launch();
        let cmd = command(&l);
        assert_eq!(cmd.get_program(), l.game_dir.join("TransportFever3.exe").as_os_str());
        assert_eq!(cmd.get_current_dir(), Some(l.game_dir.as_path()));
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), vec![OsStr::new("-arg")]);
        assert_eq!(env_of(&cmd, ENV_SESSION_DIR), Some(l.session_dir.as_os_str()));
        assert_eq!(env_of(&cmd, ENV_PLAYER_ID), Some(OsStr::new("0")));
        assert_eq!(env_of(&cmd, ENV_PLAYER_NAME), Some(OsStr::new("Aziz newline")));
        assert_eq!(env_of(&cmd, ENV_ROLE), Some(OsStr::new("host")));
    }

    #[test]
    fn roles() {
        assert_eq!(Role::Host.to_string(), "host");
        assert_eq!(Role::Player.as_str(), "player");
        let mut l = launch();
        l.role = Role::Player;
        l.player_id = 7;
        let cmd = command(&l);
        assert_eq!(env_of(&cmd, ENV_ROLE), Some(OsStr::new("player")));
        assert_eq!(env_of(&cmd, ENV_PLAYER_ID), Some(OsStr::new("7")));
    }

    #[test]
    fn tasklist_rows_are_counted_by_image_name() {
        let csv = "\"feverlink3.exe\",\"1200\",\"Console\",\"1\",\"9,000 K\"\r\n\
                   \"FeverLink3.exe\",\"1300\",\"Console\",\"1\",\"9,100 K\"\r\n\
                   \"feverlink3-speedtest.exe\",\"1400\",\"Console\",\"1\",\"1 K\"\r\n";
        assert_eq!(count_csv_rows(csv, "feverlink3.exe"), 2);
        // what tasklist prints when nothing matches
        assert_eq!(
            count_csv_rows("INFO: No tasks are running which match the specified criteria.\r\n", "feverlink3.exe"),
            0
        );
    }

    #[test]
    fn start_refuses_a_missing_exe_without_spawning() {
        let l = Launch { game_dir: PathBuf::from(r"C:\definitely\not\here"), ..launch() };
        let err = start(&l).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
