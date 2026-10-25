//! Shared by the tests of the wizard: a PC made of temporary folders, facts built without touching the disk, and
//! a driver that runs the menu loop on scripted input.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};

use feverlink3_launcher::install::{self, AppIdState, DllState, Item, Outcome, Report, Step};
use feverlink3_launcher::steam::SteamInfo;
use feverlink3_launcher::{APP_ID, GAME_EXE, MOD_ID, NATIVE_MARKER};
use tempfile::TempDir;

use super::facts::{Facts, GameFiles, UserDir};
use super::run;
use super::system::System;
use super::versions::{own_version, BuildCheck};

// -----------------------------------------------------------------------------------------------------------
// Fixtures
// -----------------------------------------------------------------------------------------------------------

/// A minimal PE32+ header block with the given link timestamp: the fields `pe_timestamp` reads, nothing else.
pub(super) fn pe_image(timestamp: u32) -> Vec<u8> {
    let mut v = vec![0u8; 0x80];
    v[..2].copy_from_slice(b"MZ");
    v[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    v.extend_from_slice(b"PE\0\0");
    v.extend_from_slice(&0x8664u16.to_le_bytes()); // machine
    v.extend_from_slice(&0u16.to_le_bytes()); // sections
    v.extend_from_slice(&timestamp.to_le_bytes());
    v.extend_from_slice(&[0u8; 8]); // symbol table pointer and count
    v.extend_from_slice(&240u16.to_le_bytes()); // optional header size
    v.extend_from_slice(&0x22u16.to_le_bytes()); // characteristics
    v.extend_from_slice(&0x20bu16.to_le_bytes()); // PE32+ magic, the start of the optional header
    v.resize(v.len() + 238, 0);
    v
}

/// Facts of a healthy PC with nothing of FeverLink3 installed, built without touching the disk.
pub(super) fn base_facts() -> Facts {
    Facts {
        steam_path: Some(PathBuf::from(r"C:\Steam")),
        steam_exe: Some(PathBuf::from(r"C:\Steam\steam.exe")),
        libraries: vec![PathBuf::from(r"C:\Steam")],
        game_dir: Some(PathBuf::from(r"C:\Steam\steamapps\common\Transport Fever 3")),
        users: vec![UserDir { path: PathBuf::from(r"C:\Steam\userdata\1\3493540\local"), mod_installed: false }],
        game_files: Some(GameFiles { dll: Ok(DllState::Absent), appid: Ok(AppIdState::Absent), launcher: None }),
        build: BuildCheck::Known { name: "40420" },
        game_running: false,
        exe_path: PathBuf::from(r"C:\FeverLink3-0.1.0\feverlink3.exe"),
        payload_mod: Some(PathBuf::from(r"C:\FeverLink3-0.1.0\mod\feverlink3")),
        payload_dll: Some(PathBuf::from(r"C:\FeverLink3-0.1.0\winhttp.dll")),
        plan: None,
        installed_version: None,
        local_dir: Some(PathBuf::from(r"C:\Users\Ann\AppData\Local\FeverLink3")),
        local_dir_exists: true,
    }
}

/// The same PC after a complete install from this folder.
pub(super) fn installed_facts() -> Facts {
    let mut f = base_facts();
    f.game_files =
        Some(GameFiles { dll: Ok(DllState::Ours), appid: Ok(AppIdState::Ours), launcher: Some(f.exe_path.clone()) });
    f.users[0].mod_installed = true;
    f.installed_version = Some(own_version().to_owned());
    f
}

pub(super) fn step(item: Item, outcome: Outcome, reason: &str) -> Step {
    Step { item, path: PathBuf::from(r"C:\Game\file"), outcome, reason: reason.to_owned() }
}

pub(super) fn report(steps: Vec<Step>) -> Report {
    Report { dry_run: false, steps }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Call {
    Steam(PathBuf),
    Open(PathBuf),
    Clipboard(String),
}

/// A PC made of temporary folders: Steam with the game and one user folder, a release folder to install from,
/// and a `LocalAppData\FeverLink3` that does not exist until a test makes it. Starting a program is recorded,
/// never done.
pub(super) struct Pc {
    pub(super) root: TempDir,
    pub(super) steam: Option<PathBuf>,
    pub(super) game: Option<PathBuf>,
    pub(super) users: Vec<PathBuf>,
    pub(super) exe: PathBuf,
    pub(super) payload: (Option<PathBuf>, Option<PathBuf>),
    pub(super) local: Option<PathBuf>,
    pub(super) names: Vec<String>,
    pub(super) running: Cell<bool>,
    pub(super) clipboard_fails: Cell<bool>,
    pub(super) calls: RefCell<Vec<Call>>,
}

impl Pc {
    pub(super) fn new() -> Pc {
        let root = TempDir::new().unwrap();
        let steam = root.path().join("Steam");
        let game = steam.join("steamapps").join("common").join("Transport Fever 3");
        fs::create_dir_all(&game).unwrap();
        // a verified build, so the wizard knows it
        fs::write(game.join(GAME_EXE), pe_image(super::versions::first_known_build())).unwrap();
        fs::write(steam.join("steam.exe"), b"steam").unwrap();
        let user = steam.join("userdata").join("1").join(APP_ID).join("local");
        fs::create_dir_all(&user).unwrap();
        let mod_src = root.path().join("release").join("mod").join(MOD_ID);
        fs::create_dir_all(mod_src.join("content")).unwrap();
        fs::write(mod_src.join("mod.json"), "{}").unwrap();
        fs::write(mod_src.join("content").join("a.lua"), "a").unwrap();
        let dll_src = root.path().join("release").join("winhttp.dll");
        let mut dll = b"MZ fake native module ".to_vec();
        dll.extend_from_slice(NATIVE_MARKER);
        fs::write(&dll_src, dll).unwrap();
        let local = root.path().join("LocalAppData").join("FeverLink3");
        Pc {
            // not under a temporary folder, or the wizard would refuse to install from it
            exe: PathBuf::from("C:/FeverLink3-0.1.0/feverlink3.exe"),
            steam: Some(steam),
            game: Some(game),
            users: vec![user],
            payload: (Some(mod_src), Some(dll_src)),
            local: Some(local),
            names: vec!["Zara".to_owned()],
            running: Cell::new(false),
            clipboard_fails: Cell::new(false),
            calls: RefCell::new(Vec::new()),
            root,
        }
    }

    pub(super) fn game(&self) -> PathBuf {
        self.game.clone().unwrap()
    }

    pub(super) fn mod_src(&self) -> PathBuf {
        self.payload.0.clone().unwrap()
    }

    pub(super) fn installed_mod(&self) -> PathBuf {
        install::mod_dir(&self.users[0])
    }

    pub(super) fn local(&self) -> PathBuf {
        self.local.clone().unwrap()
    }

    pub(super) fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }
}

impl System for Pc {
    fn discover(&self) -> SteamInfo {
        SteamInfo {
            steam_path: self.steam.clone(),
            libraries: self.steam.iter().cloned().collect(),
            game_dir: self.game.clone(),
            user_dirs: self.users.clone(),
        }
    }

    fn game_running(&self) -> bool {
        self.running.get()
    }

    fn exe_path(&self) -> PathBuf {
        self.exe.clone()
    }

    fn payload(&self) -> (Option<PathBuf>, Option<PathBuf>) {
        self.payload.clone()
    }

    fn local_dir(&self) -> Option<PathBuf> {
        self.local.clone()
    }

    fn user_names(&self) -> Vec<String> {
        self.names.clone()
    }

    fn now_unix(&self) -> u64 {
        1_791_633_296
    }

    fn start_steam_game(&self, steam_path: &Path) -> io::Result<()> {
        self.calls.borrow_mut().push(Call::Steam(steam_path.to_path_buf()));
        Ok(())
    }

    fn open_folder(&self, dir: &Path) -> io::Result<()> {
        self.calls.borrow_mut().push(Call::Open(dir.to_path_buf()));
        Ok(())
    }

    fn copy_to_clipboard(&self, text: &str) -> io::Result<()> {
        if self.clipboard_fails.get() {
            return Err(io::Error::other("the clipboard is busy"));
        }
        self.calls.borrow_mut().push(Call::Clipboard(text.to_owned()));
        Ok(())
    }
}

/// Runs the menu loop on scripted input and returns everything it printed.
pub(super) fn drive(pc: &Pc, script: &str) -> String {
    let mut input = Cursor::new(script.as_bytes().to_vec());
    let mut out = Vec::new();
    run(pc, &mut input, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

pub(super) fn clipboard_text(pc: &Pc) -> String {
    pc.calls()
        .into_iter()
        .find_map(|call| match call {
            Call::Clipboard(text) => Some(text),
            _ => None,
        })
        .expect("something was copied to the clipboard")
}

pub(super) fn step_at(item: Item, path: &str, outcome: Outcome, reason: &str) -> Step {
    Step { item, path: PathBuf::from(path), outcome, reason: reason.to_owned() }
}
