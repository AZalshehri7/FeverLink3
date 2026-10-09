//! One snapshot of this PC ([`Facts`]) and what is known from it about the installation and the folder
//! the program runs from.

use std::path::{Component, Path, PathBuf};

use feverlink3_launcher::install::{self, AppIdState, DllState, Payload, Report};

use super::system::System;
use super::versions::{check_build, read_launcher_version, BuildCheck};

/// A Steam user's `userdata/<id>/3493540/local` folder and whether the mod is in it.
#[derive(Debug, Clone)]
pub(super) struct UserDir {
    pub(super) path: PathBuf,
    pub(super) mod_installed: bool,
}

/// What the game folder holds of ours, as the doctor reports it. `Err` carries the operating system's text.
#[derive(Debug, Clone)]
pub(super) struct GameFiles {
    pub(super) dll: Result<DllState, String>,
    pub(super) appid: Result<AppIdState, String>,
    /// The launcher path recorded in `feverlink3_path.txt`.
    pub(super) launcher: Option<PathBuf>,
}

/// One snapshot of this PC, taken by [`gather`]. Every screen and decision is a function of it.
#[derive(Debug, Clone)]
pub(super) struct Facts {
    pub(super) steam_path: Option<PathBuf>,
    pub(super) steam_exe: Option<PathBuf>,
    pub(super) libraries: Vec<PathBuf>,
    pub(super) game_dir: Option<PathBuf>,
    pub(super) users: Vec<UserDir>,
    pub(super) game_files: Option<GameFiles>,
    pub(super) build: BuildCheck,
    pub(super) game_running: bool,
    pub(super) exe_path: PathBuf,
    pub(super) payload_mod: Option<PathBuf>,
    pub(super) payload_dll: Option<PathBuf>,
    /// What installing from this folder would do, worked out as a dry run; `None` without game folder or payload.
    pub(super) plan: Option<Report>,
    /// The version of the launcher recorded in `feverlink3_path.txt`, when that file can still be read.
    pub(super) installed_version: Option<String>,
    pub(super) local_dir: Option<PathBuf>,
    pub(super) local_dir_exists: bool,
}

/// Takes the snapshot. Reads files and asks the system, changes nothing; the dry-run install is how "what differs
/// from this folder" is known, so the comparison and the real install can never disagree.
pub(super) fn gather(sys: &dyn System) -> Facts {
    let info = sys.discover();
    let exe_path = sys.exe_path();
    let (payload_mod, payload_dll) = sys.payload();
    let game_files = info.game_dir.as_deref().map(read_game_files);
    let installed_version = game_files
        .as_ref()
        .and_then(|g| g.launcher.as_deref())
        .and_then(|recorded| read_launcher_version(recorded, &exe_path));
    let plan = match (info.game_dir.as_deref(), payload_mod.as_deref(), payload_dll.as_deref()) {
        (Some(game), Some(mod_source), Some(native_dll)) => {
            let payload =
                Payload { mod_source: Some(mod_source), native_dll: Some(native_dll), launcher_path: &exe_path };
            Some(install::install(game, &info.user_dirs, payload, true))
        }
        _ => None,
    };
    let local_dir = sys.local_dir();
    Facts {
        steam_exe: info.steam_path.as_ref().map(|p| p.join("steam.exe")).filter(|p| p.is_file()),
        build: check_build(info.game_dir.as_deref()),
        users: info
            .user_dirs
            .iter()
            .map(|path| UserDir { path: path.clone(), mod_installed: install::mod_installed(path) })
            .collect(),
        game_running: sys.game_running(),
        local_dir_exists: local_dir.as_deref().is_some_and(Path::is_dir),
        local_dir,
        steam_path: info.steam_path,
        libraries: info.libraries,
        game_dir: info.game_dir,
        game_files,
        exe_path,
        payload_mod,
        payload_dll,
        plan,
        installed_version,
    }
}

/// The folder this executable sits in.
pub(super) fn folder_dir(f: &Facts) -> String {
    f.exe_path.parent().unwrap_or(&f.exe_path).display().to_string()
}

fn read_game_files(game_dir: &Path) -> GameFiles {
    GameFiles {
        dll: install::dll_state(game_dir).map_err(|e| e.to_string()),
        appid: install::appid_state(game_dir).map_err(|e| e.to_string()),
        launcher: install::recorded_launcher_path(game_dir),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// What is installed, and what this folder holds
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InstallState {
    /// No game folder to look in, or its files could not be read.
    Unknown,
    NotInstalled,
    /// The native module and the mod for every Steam user; the version that put them there, when it is known.
    Complete {
        version: Option<String>,
    },
    Partial {
        native_module: bool,
        mods: usize,
        users: usize,
    },
    /// A `winhttp.dll` that is not ours sits in the game folder.
    ForeignDll,
}

pub(super) fn installed_state(f: &Facts) -> InstallState {
    let Some(files) = &f.game_files else { return InstallState::Unknown };
    let native_module = match &files.dll {
        Ok(DllState::Foreign) => return InstallState::ForeignDll,
        Ok(DllState::Ours) => true,
        Ok(DllState::Absent) => false,
        Err(_) => return InstallState::Unknown,
    };
    let users = f.users.len();
    let mods = f.users.iter().filter(|u| u.mod_installed).count();
    if native_module && users > 0 && mods == users {
        InstallState::Complete { version: f.installed_version.clone() }
    } else if !native_module && mods == 0 && files.launcher.is_none() {
        InstallState::NotInstalled
    } else {
        InstallState::Partial { native_module, mods, users }
    }
}

/// Whether the folder this executable sits in can be installed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Package {
    Complete,
    /// Started from inside a zip file or another temporary folder, which Windows empties later.
    TemporaryFolder,
    Incomplete {
        mod_missing: bool,
        dll_missing: bool,
    },
}

pub(super) fn package_state(f: &Facts) -> Package {
    if is_temporary_location(&f.exe_path) {
        Package::TemporaryFolder
    } else if f.payload_mod.is_none() || f.payload_dll.is_none() {
        Package::Incomplete { mod_missing: f.payload_mod.is_none(), dll_missing: f.payload_dll.is_none() }
    } else {
        Package::Complete
    }
}

/// True for the places Windows and the common unzip tools run a program from when it is double-clicked inside an
/// archive: `%TEMP%` (compared as `AppData\Local\Temp`, because the short and the long form of the user folder
/// both occur), Explorer's `Temp1_<name>.zip`, WinRAR's `Rar$...` and 7-Zip's `7zO...`.
fn is_temporary_location(exe: &Path) -> bool {
    let parts: Vec<String> = exe
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    let in_temp = parts.windows(3).any(|w| w[0] == "appdata" && w[1] == "local" && w[2] == "temp");
    in_temp || parts.iter().any(|p| is_extraction_folder(p))
}

fn is_extraction_folder(lowercase_name: &str) -> bool {
    let explorer = lowercase_name
        .strip_prefix("temp")
        .and_then(|rest| rest.split_once('_'))
        .is_some_and(|(n, name)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && name.ends_with(".zip"));
    explorer || lowercase_name.starts_with("rar$") || lowercase_name.starts_with("7zo")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use feverlink3_launcher::install::Outcome;

    use super::*;
    use crate::wizard::testkit::*;

    #[test]
    fn installed_state_tells_not_installed_partial_complete_and_foreign_apart() {
        let mut f = base_facts();
        assert_eq!(installed_state(&f), InstallState::NotInstalled);

        f.game_files.as_mut().unwrap().dll = Ok(DllState::Ours);
        assert_eq!(installed_state(&f), InstallState::Partial { native_module: true, mods: 0, users: 1 });
        f.users[0].mod_installed = true;
        f.installed_version = Some("0.1.0".into());
        assert_eq!(installed_state(&f), InstallState::Complete { version: Some("0.1.0".into()) });

        f.game_files.as_mut().unwrap().dll = Ok(DllState::Absent);
        assert_eq!(installed_state(&f), InstallState::Partial { native_module: false, mods: 1, users: 1 });

        let mut only_path_file = base_facts();
        only_path_file.game_files.as_mut().unwrap().launcher = Some(PathBuf::from(r"C:\old\feverlink3.exe"));
        assert_eq!(installed_state(&only_path_file), InstallState::Partial { native_module: false, mods: 0, users: 1 });

        let mut foreign = base_facts();
        foreign.game_files.as_mut().unwrap().dll = Ok(DllState::Foreign);
        assert_eq!(installed_state(&foreign), InstallState::ForeignDll);

        let mut unreadable = base_facts();
        unreadable.game_files.as_mut().unwrap().dll = Err("denied".into());
        assert_eq!(installed_state(&unreadable), InstallState::Unknown);

        let mut no_game = base_facts();
        no_game.game_files = None;
        assert_eq!(installed_state(&no_game), InstallState::Unknown);

        let mut no_user_folder = installed_facts();
        no_user_folder.users.clear();
        assert_eq!(
            installed_state(&no_user_folder),
            InstallState::Partial { native_module: true, mods: 0, users: 0 },
            "the native module alone is not an install"
        );

        let mut two_users = installed_facts();
        two_users
            .users
            .push(UserDir { path: PathBuf::from(r"C:\Steam\userdata\2\3493540\local"), mod_installed: false });
        assert_eq!(installed_state(&two_users), InstallState::Partial { native_module: true, mods: 1, users: 2 });
    }

    #[test]
    fn temporary_places_are_recognised_but_ordinary_folders_are_not() {
        for temporary in [
            "C:/Users/Ann/AppData/Local/Temp/Temp1_FeverLink3-0.1.0.zip/FeverLink3-0.1.0/feverlink3.exe",
            "C:/Users/ANN~1/AppData/Local/TEMP/x/feverlink3.exe",
            "C:/Users/Ann/Downloads/Temp12_FeverLink3.ZIP/feverlink3.exe",
            "D:/Rar$EXa12345.678/feverlink3.exe",
            "D:/7zO8A3B2C1D/feverlink3.exe",
        ] {
            assert!(is_temporary_location(Path::new(temporary)), "{temporary}");
        }
        for ordinary in [
            "C:/Users/Ann/Downloads/FeverLink3-0.1.0/feverlink3.exe",
            "C:/Users/Ann/Desktop/Temp1_notes/feverlink3.exe",
            "C:/Users/Ann/Desktop/template_1.zip/feverlink3.exe",
            "C:/Users/Ann/AppData/Local/Programs/FeverLink3/feverlink3.exe",
            "C:/Temp/FeverLink3/feverlink3.exe",
            "feverlink3.exe",
        ] {
            assert!(!is_temporary_location(Path::new(ordinary)), "{ordinary}");
        }
    }

    #[test]
    fn the_package_is_complete_incomplete_or_temporary() {
        let mut f = base_facts();
        assert_eq!(package_state(&f), Package::Complete);
        f.payload_dll = None;
        assert_eq!(package_state(&f), Package::Incomplete { mod_missing: false, dll_missing: true });
        f.payload_mod = None;
        assert_eq!(package_state(&f), Package::Incomplete { mod_missing: true, dll_missing: true });
        f.exe_path = PathBuf::from("C:/Users/Ann/AppData/Local/Temp/Temp1_FeverLink3-0.1.0.zip/feverlink3.exe");
        assert_eq!(package_state(&f), Package::TemporaryFolder, "a temporary folder comes first");
    }

    // -----------------------------------------------------------------------------------------------------------
    // The snapshot of a PC
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn the_snapshot_of_a_fresh_pc_is_read_from_its_folders() {
        let pc = Pc::new();
        let f = gather(&pc);
        assert_eq!(f.steam_path, pc.steam);
        assert_eq!(f.game_dir, pc.game);
        assert_eq!(f.steam_exe, Some(pc.steam.clone().unwrap().join("steam.exe")));
        assert_eq!(f.build, BuildCheck::Known { name: "40420" });
        assert_eq!(f.users.len(), 1);
        assert!(!f.users[0].mod_installed);
        assert_eq!(installed_state(&f), InstallState::NotInstalled);
        assert_eq!(package_state(&f), Package::Complete);
        assert!(!f.game_running);
        assert!(!f.local_dir_exists, "nothing has created the log folder");
        let plan = f.plan.expect("a dry-run install is planned");
        assert!(plan.dry_run && plan.ok());
        assert!(plan.steps.iter().all(|s| s.outcome == Outcome::Done), "a fresh PC would get everything: {plan}");
        assert!(!pc.game().join(install::DLL_NAME).exists(), "gathering changes nothing");
    }

    #[test]
    fn the_snapshot_notices_what_is_missing() {
        let mut pc = Pc::new();
        pc.steam = None;
        pc.game = None;
        pc.users.clear();
        pc.payload = (None, None);
        let f = gather(&pc);
        assert!(f.steam_path.is_none() && f.game_dir.is_none() && f.steam_exe.is_none() && f.plan.is_none());
        assert_eq!(f.build, BuildCheck::NoGame);
        assert_eq!(installed_state(&f), InstallState::Unknown);

        let no_exe = Pc::new();
        fs::remove_file(no_exe.steam.clone().unwrap().join("steam.exe")).unwrap();
        no_exe.running.set(true);
        let f = gather(&no_exe);
        assert!(f.steam_exe.is_none() && f.game_running);
        fs::create_dir_all(no_exe.local()).unwrap();
        assert!(gather(&no_exe).local_dir_exists);
    }
}
