//! What the wizard asks of the machine and does to it outside the install functions: the [`System`] trait, the
//! real PC behind it, and the programs it starts (Steam, Explorer, `clip.exe`).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use feverlink3_launcher::steam::{self, SteamInfo};
use feverlink3_launcher::{live, process};

use super::plans::steam_start_command;

/// Everything the wizard asks of the machine or does to it outside the install functions. The tests supply a fake
/// that works on temporary folders and records what it was asked to start.
pub(super) trait System {
    /// Steam, the game and the Steam user folders.
    fn discover(&self) -> SteamInfo;
    /// True when any Transport Fever 3 process runs.
    fn game_running(&self) -> bool;
    /// This executable.
    fn exe_path(&self) -> PathBuf;
    /// The mod folder and the native module shipped next to this executable.
    fn payload(&self) -> (Option<PathBuf>, Option<PathBuf>);
    /// `%LOCALAPPDATA%\FeverLink3`, whether or not it exists yet.
    fn local_dir(&self) -> Option<PathBuf>;
    /// The names that identify this person in paths and logs.
    fn user_names(&self) -> Vec<String>;
    fn now_unix(&self) -> u64;
    /// Asks Steam to start the game and returns without waiting for it.
    fn start_steam_game(&self, steam_path: &Path) -> io::Result<()>;
    fn open_folder(&self, dir: &Path) -> io::Result<()>;
    fn copy_to_clipboard(&self, text: &str) -> io::Result<()>;
}

/// A program of the Windows folder by absolute path. A bare name is looked up in this program's own folder first,
/// and a folder the player unzipped into should not get a say in which `clip.exe` runs.
pub(super) fn windows_program(relative: &[&str]) -> PathBuf {
    let mut path = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    path.extend(relative);
    path
}

/// The bytes `clip.exe` takes as Unicode text: UTF-16 little endian after a byte order mark, with Windows line
/// breaks. Without the mark it reads the bytes in the console's code page and garbles every non-ASCII character.
fn clipboard_bytes(text: &str) -> Vec<u8> {
    let windows_text = text.replace("\r\n", "\n").replace('\n', "\r\n");
    std::iter::once(0xFEFF_u16).chain(windows_text.encode_utf16()).flat_map(u16::to_le_bytes).collect()
}

/// This PC.
pub(super) struct RealSystem;

impl System for RealSystem {
    fn discover(&self) -> SteamInfo {
        steam::discover()
    }

    fn game_running(&self) -> bool {
        process::any_game_running()
    }

    fn exe_path(&self) -> PathBuf {
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("FeverLink3.exe"))
    }

    fn payload(&self) -> (Option<PathBuf>, Option<PathBuf>) {
        crate::locate_payload()
    }

    fn local_dir(&self) -> Option<PathBuf> {
        live::live_dir().and_then(|dir| dir.parent().map(Path::to_path_buf))
    }

    fn user_names(&self) -> Vec<String> {
        let profile_folder = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        ["USERNAME", "USER"].iter().filter_map(|key| std::env::var(key).ok()).chain(profile_folder).collect()
    }

    fn now_unix(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
    }

    fn start_steam_game(&self, steam_path: &Path) -> io::Result<()> {
        steam_start_command(steam_path).spawn().map(|_| ())
    }

    fn open_folder(&self, dir: &Path) -> io::Result<()> {
        // explorer.exe exits with 1 even when it opened the window, so only a failure to start counts
        Command::new(windows_program(&["explorer.exe"])).arg(dir).spawn().map(|_| ())
    }

    fn copy_to_clipboard(&self, text: &str) -> io::Result<()> {
        let mut child = Command::new(windows_program(&["System32", "clip.exe"]))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let written = match child.stdin.take() {
            // the pipe closes when it goes out of scope, which is what tells clip.exe the text is complete
            Some(mut pipe) => pipe.write_all(&clipboard_bytes(text)),
            None => Err(io::Error::other("no pipe to clip.exe")),
        };
        let status = child.wait()?;
        written?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("clip.exe ended with {status}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clipboard_gets_utf16_with_a_byte_order_mark_and_windows_line_breaks() {
        assert_eq!(clipboard_bytes("a\nb"), [0xFF, 0xFE, b'a', 0, b'\r', 0, b'\n', 0, b'b', 0]);
        assert_eq!(clipboard_bytes("a\r\nb"), clipboard_bytes("a\nb"), "no doubled returns");
        assert_eq!(clipboard_bytes(""), [0xFF, 0xFE]);
        assert_eq!(clipboard_bytes("\u{0639}"), [0xFF, 0xFE, 0x39, 0x06]);
        assert_eq!(clipboard_bytes("\u{1F680}"), [0xFF, 0xFE, 0x3D, 0xD8, 0x80, 0xDE], "a surrogate pair");
    }

    #[test]
    fn programs_of_the_windows_folder_are_started_by_absolute_path() {
        let clip = windows_program(&["System32", "clip.exe"]);
        assert!(clip.is_absolute(), "{}", clip.display());
        assert!(clip.ends_with(Path::new("System32").join("clip.exe")));
        assert!(windows_program(&["explorer.exe"]).ends_with("explorer.exe"));
    }
}
