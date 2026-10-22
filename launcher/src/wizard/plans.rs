//! What each menu choice may do (the checks made before it), and the comparison of this folder with the
//! installed files.

use std::cmp::Ordering;
use std::path::Path;
use std::process::{Command, Stdio};

use feverlink3_launcher::install::{self, Item, Outcome};
use feverlink3_launcher::{process, APP_ID};

use super::facts::{folder_dir, installed_state, package_state, Facts, InstallState, Package};
use super::problems::{classify_reason, failure_problems, Problem};
use super::versions::{own_version, version_order};

const RELEASES_URL: &str = "https://github.com/AZalshehri7/FeverLink3/releases";

/// The folders an install works with; only exists when nothing stands in the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InstallPlan<'a> {
    pub(super) game_dir: &'a Path,
    pub(super) mod_source: &'a Path,
    pub(super) native_dll: &'a Path,
}

/// Install needs Steam, the game, a complete and permanent folder to install from, a closed game (Windows locks
/// `winhttp.dll` while it runs) and no foreign `winhttp.dll` in the way. The first thing missing is the answer.
pub(super) fn plan_install(f: &Facts) -> Result<InstallPlan<'_>, Problem> {
    if f.steam_path.is_none() {
        return Err(Problem::SteamMissing);
    }
    let Some(game_dir) = f.game_dir.as_deref() else { return Err(Problem::GameMissing) };
    match package_state(f) {
        Package::TemporaryFolder => return Err(Problem::TemporaryFolder),
        Package::Incomplete { mod_missing, dll_missing } => {
            return Err(Problem::PackageIncomplete { mod_missing, dll_missing })
        }
        Package::Complete => {}
    }
    if f.game_running {
        return Err(Problem::GameRunning);
    }
    match installed_state(f) {
        InstallState::ForeignDll => return Err(Problem::ForeignDll(game_dir.join(install::DLL_NAME))),
        // not knowing whether the module there is ours, nothing is installed next to it: the cause (in use, no
        // permission) is told instead of leaving half an install behind
        InstallState::Unreadable(why) => {
            return Err(classify_reason(Item::NativeDll, &why, &game_dir.join(install::DLL_NAME)));
        }
        _ => {}
    }
    match (f.payload_mod.as_deref(), f.payload_dll.as_deref()) {
        (Some(mod_source), Some(native_dll)) => Ok(InstallPlan { game_dir, mod_source, native_dll }),
        _ => Err(Problem::PackageIncomplete {
            mod_missing: f.payload_mod.is_none(),
            dll_missing: f.payload_dll.is_none(),
        }),
    }
}

/// Uninstall needs the game folder (it holds most of what is removed) and a closed game.
pub(super) fn plan_uninstall(f: &Facts) -> Result<&Path, Problem> {
    if f.steam_path.is_none() {
        return Err(Problem::SteamMissing);
    }
    let game_dir = f.game_dir.as_deref().ok_or(Problem::GameMissing)?;
    if f.game_running {
        return Err(Problem::GameRunning);
    }
    Ok(game_dir)
}

/// Starting through Steam needs Steam's program and a game that is not running yet. The game folder is not
/// required: Steam knows better where the game is, and says so itself when it is not installed.
pub(super) fn plan_start(f: &Facts) -> Result<&Path, Problem> {
    let steam_path = f.steam_path.as_deref().ok_or(Problem::SteamMissing)?;
    if f.steam_exe.is_none() {
        return Err(Problem::SteamExeMissing(steam_path.join("steam.exe")));
    }
    if f.game_running {
        return Err(Problem::GameAlreadyRunning);
    }
    Ok(steam_path)
}

/// The log folder, when it exists.
pub(super) fn plan_logs(f: &Facts) -> Result<&Path, Problem> {
    let dir = f.local_dir.as_deref().ok_or(Problem::NoLocalAppData)?;
    if f.local_dir_exists {
        Ok(dir)
    } else {
        Err(Problem::NoLogsYet(dir.to_path_buf()))
    }
}

/// `steam.exe -applaunch 3493540`, not yet started: Steam starts itself first if it is not running, then the game.
/// Without any `FEVERLINK3_*` variable: a Steam started here keeps its environment for every later game.
pub(super) fn steam_start_command(steam_path: &Path) -> Command {
    let mut cmd = Command::new(steam_path.join("steam.exe"));
    process::without_feverlink3_env(&mut cmd);
    cmd.current_dir(steam_path)
        .args(["-applaunch", APP_ID])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

// ---------------------------------------------------------------------------------------------------------------
// This folder against the installed files
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Comparison {
    NotInstalled,
    Identical,
    /// The mod and the native module are the same; only helper files differ (the recorded launcher location).
    HelpersDiffer(Vec<&'static str>),
    /// What would be replaced by installing from this folder.
    Differs(Vec<&'static str>),
}

/// Compares by the dry-run install: what it would change is what differs. `Err` when there is nothing to compare.
pub(super) fn compare(f: &Facts) -> Result<Comparison, Problem> {
    if f.steam_path.is_none() {
        return Err(Problem::SteamMissing);
    }
    if f.game_dir.is_none() {
        return Err(Problem::GameMissing);
    }
    let Some(plan) = &f.plan else {
        return Err(Problem::PackageIncomplete {
            mod_missing: f.payload_mod.is_none(),
            dll_missing: f.payload_dll.is_none(),
        });
    };
    if let Some(problem) = failure_problems(plan).into_iter().next() {
        return Err(problem);
    }
    if installed_state(f) == InstallState::NotInstalled {
        return Ok(Comparison::NotInstalled);
    }
    let mut pieces: Vec<&'static str> = Vec::new();
    let mut helpers: Vec<&'static str> = Vec::new();
    for step in plan.steps.iter().filter(|s| s.outcome == Outcome::Done) {
        let (list, name) = match step.item {
            Item::Mod => (&mut pieces, "the mod"),
            Item::NativeDll => (&mut pieces, "the native module (winhttp.dll)"),
            Item::PathFile => (&mut helpers, "the recorded launcher location"),
            Item::AppIdFile => (&mut helpers, "steam_appid.txt"),
        };
        if !list.contains(&name) {
            list.push(name);
        }
    }
    Ok(match (pieces.is_empty(), helpers.is_empty()) {
        (true, true) => Comparison::Identical,
        (true, false) => Comparison::HelpersDiffer(helpers),
        (false, _) => Comparison::Differs(pieces),
    })
}

fn installed_text(f: &Facts) -> String {
    match installed_state(f) {
        InstallState::Complete { version: Some(v) } => {
            let from = f.game_files.as_ref().and_then(|g| g.launcher.as_deref()).and_then(Path::parent);
            from.map_or_else(
                || format!("version {v}"),
                |dir| format!("version {v}  (installed from {})", dir.display()),
            )
        }
        InstallState::Complete { version: None } => {
            "installed, but its version is not known (the folder it was installed from is gone, or too old to say)"
                .to_owned()
        }
        InstallState::NotInstalled => "not installed".to_owned(),
        InstallState::Partial { .. } => "only partly installed".to_owned(),
        InstallState::ForeignDll => "another program's winhttp.dll is in the game folder".to_owned(),
        InstallState::NoGameFolder => "unknown (there is no game folder to look in)".to_owned(),
        InstallState::Unreadable(_) => "unknown (winhttp.dll in the game folder cannot be read)".to_owned(),
    }
}

/// The text of menu choice 3.
pub(super) fn comparison_lines(f: &Facts) -> Vec<String> {
    let this = own_version();
    let verdict = match compare(f) {
        Err(problem) => problem.message(),
        Ok(Comparison::NotInstalled) => {
            "FeverLink3 is not installed. Choose 1 to install the version in this folder.".to_owned()
        }
        Ok(Comparison::Identical) => {
            "The installed files are identical to the ones in this folder. Nothing to do.".to_owned()
        }
        Ok(Comparison::HelpersDiffer(list)) => format!(
            "The installed mod and native module are identical to the ones in this folder. Only {} would be updated; \
             choose 1 to do that.",
            list.join(" and ")
        ),
        Ok(Comparison::Differs(pieces)) => {
            let what = pieces.join(" and ");
            let differ = if pieces.len() == 1 { "differs" } else { "differ" };
            match version_order(this, f.installed_version.as_deref()) {
                Some(Ordering::Greater) => format!(
                    "This folder is NEWER than what is installed. Installing from it updates {what}. Choose 1 to \
                     update."
                ),
                Some(Ordering::Less) => format!(
                    "This folder is OLDER than what is installed. Installing from it would replace newer files \
                     ({what}) with older ones. Use the newer FeverLink3 folder instead."
                ),
                Some(Ordering::Equal) => format!(
                    "The version number is the same, but {what} {differ} from the installed files. Choose 1 to make \
                     the installed files match this folder."
                ),
                None => format!(
                    "The installed files differ from the ones in this folder ({what}). The installed version is not \
                     known, so it cannot be said which is newer. Choose 1 to make the installed files match this \
                     folder."
                ),
            }
        }
    };
    vec![
        format!("This folder:  version {this}  ({})", folder_dir(f)),
        format!("Installed:    {}", installed_text(f)),
        String::new(),
        verdict,
        String::new(),
        format!("Looking for newer releases online is not built in yet; the newest zip file is on {RELEASES_URL}"),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use feverlink3_launcher::install::{DllState, Report, Step};

    use super::*;
    use crate::wizard::testkit::*;

    #[test]
    fn install_is_planned_only_when_nothing_stands_in_the_way() {
        let f = base_facts();
        let plan = plan_install(&f).unwrap();
        assert_eq!(plan.game_dir, f.game_dir.as_deref().unwrap());
        assert_eq!(plan.mod_source, f.payload_mod.as_deref().unwrap());
        assert_eq!(plan.native_dll, f.payload_dll.as_deref().unwrap());

        let blocked = |change: fn(&mut Facts)| {
            let mut f = base_facts();
            change(&mut f);
            plan_install(&f).unwrap_err()
        };
        assert_eq!(blocked(|f| f.steam_path = None), Problem::SteamMissing);
        assert_eq!(blocked(|f| f.game_dir = None), Problem::GameMissing);
        assert_eq!(blocked(|f| f.game_running = true), Problem::GameRunning);
        assert_eq!(
            blocked(|f| f.payload_mod = None),
            Problem::PackageIncomplete { mod_missing: true, dll_missing: false }
        );
        assert_eq!(
            blocked(|f| f.payload_dll = None),
            Problem::PackageIncomplete { mod_missing: false, dll_missing: true }
        );
        assert_eq!(blocked(|f| f.exe_path = PathBuf::from("D:/Rar$EXa1.2/feverlink3.exe")), Problem::TemporaryFolder);
        assert!(matches!(
            blocked(|f| f.game_files.as_mut().unwrap().dll = Ok(DllState::Foreign)),
            Problem::ForeignDll(path) if path.ends_with("winhttp.dll")
        ));
        // a winhttp.dll that cannot be read is not installed over: the reason is told, nothing is half done
        assert!(matches!(
            blocked(|f| f.game_files.as_mut().unwrap().dll =
                Err("The process cannot access the file because it is being used by another process. (os error 32)".into())),
            Problem::FilesInUse(path) if path.ends_with("winhttp.dll")
        ));
        assert!(matches!(
            blocked(|f| f.game_files.as_mut().unwrap().dll = Err("Access is denied. (os error 5)".into())),
            Problem::PermissionDenied(path) if path.ends_with("winhttp.dll")
        ));
        assert!(matches!(
            blocked(|f| f.game_files.as_mut().unwrap().dll = Err("something odd".into())),
            Problem::Other(text) if text.contains("something odd")
        ));
        // Steam wins over the game, the game over the folder, the folder over the running game
        let mut everything_wrong = base_facts();
        everything_wrong.steam_path = None;
        everything_wrong.game_running = true;
        everything_wrong.payload_mod = None;
        assert_eq!(plan_install(&everything_wrong).unwrap_err(), Problem::SteamMissing);
        everything_wrong.steam_path = base_facts().steam_path;
        assert!(matches!(plan_install(&everything_wrong).unwrap_err(), Problem::PackageIncomplete { .. }));
    }

    #[test]
    fn uninstall_needs_the_game_folder_and_a_closed_game() {
        let f = base_facts();
        assert_eq!(plan_uninstall(&f).unwrap(), f.game_dir.as_deref().unwrap());
        let mut no_steam = base_facts();
        no_steam.steam_path = None;
        assert_eq!(plan_uninstall(&no_steam).unwrap_err(), Problem::SteamMissing);
        let mut no_game = base_facts();
        no_game.game_dir = None;
        assert_eq!(plan_uninstall(&no_game).unwrap_err(), Problem::GameMissing);
        let mut running = base_facts();
        running.game_running = true;
        assert_eq!(plan_uninstall(&running).unwrap_err(), Problem::GameRunning);
        // an incomplete folder does not matter: uninstall needs nothing from it
        let mut incomplete = base_facts();
        incomplete.payload_mod = None;
        assert!(plan_uninstall(&incomplete).is_ok());
    }

    #[test]
    fn starting_through_steam_needs_steam_and_a_game_that_is_not_running() {
        let f = base_facts();
        assert_eq!(plan_start(&f).unwrap(), Path::new(r"C:\Steam"));
        let mut no_steam = base_facts();
        no_steam.steam_path = None;
        assert_eq!(plan_start(&no_steam).unwrap_err(), Problem::SteamMissing);
        let mut no_exe = base_facts();
        no_exe.steam_exe = None;
        assert_eq!(
            plan_start(&no_exe).unwrap_err(),
            Problem::SteamExeMissing(PathBuf::from(r"C:\Steam").join("steam.exe"))
        );
        let mut running = base_facts();
        running.game_running = true;
        assert_eq!(plan_start(&running).unwrap_err(), Problem::GameAlreadyRunning);
        // Steam knows where the game is; the wizard not finding it is no reason to refuse
        let mut game_not_found = base_facts();
        game_not_found.game_dir = None;
        assert!(plan_start(&game_not_found).is_ok());
    }

    #[test]
    fn the_log_folder_must_exist_to_be_opened() {
        let f = base_facts();
        assert_eq!(plan_logs(&f).unwrap(), f.local_dir.as_deref().unwrap());
        let mut none_yet = base_facts();
        none_yet.local_dir_exists = false;
        assert!(matches!(plan_logs(&none_yet).unwrap_err(), Problem::NoLogsYet(p) if p.ends_with("FeverLink3")));
        let mut no_appdata = base_facts();
        no_appdata.local_dir = None;
        assert_eq!(plan_logs(&no_appdata).unwrap_err(), Problem::NoLocalAppData);
    }

    #[test]
    fn steam_is_asked_to_launch_the_game_by_its_app_id() {
        let cmd = steam_start_command(Path::new(r"C:\Program Files (x86)\Steam"));
        assert_eq!(cmd.get_program(), Path::new(r"C:\Program Files (x86)\Steam").join("steam.exe").as_os_str());
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["-applaunch", "3493540"]);
        assert!(cmd.get_envs().all(|(k, v)| v.is_none() || !process::is_feverlink3_var(k)));
        assert_eq!(cmd.get_current_dir(), Some(Path::new(r"C:\Program Files (x86)\Steam")));
    }

    // -----------------------------------------------------------------------------------------------------------
    // This folder against the installed files
    // -----------------------------------------------------------------------------------------------------------

    fn with_plan(mut f: Facts, steps: Vec<Step>) -> Facts {
        f.plan = Some(Report { dry_run: true, steps });
        f
    }

    #[test]
    fn the_comparison_reads_the_dry_run() {
        let identical = vec![
            step(Item::Mod, Outcome::Skipped, "already up to date"),
            step(Item::AppIdFile, Outcome::Skipped, "already present"),
            step(Item::NativeDll, Outcome::Skipped, "already installed, identical"),
            step(Item::PathFile, Outcome::Skipped, "already up to date"),
        ];
        assert_eq!(compare(&with_plan(installed_facts(), identical.clone())), Ok(Comparison::Identical));

        let mut mod_changed = identical.clone();
        mod_changed[0] = step(Item::Mod, Outcome::Done, "updated (mirror copy)");
        mod_changed.push(step(Item::Mod, Outcome::Done, "updated (mirror copy)"));
        assert_eq!(
            compare(&with_plan(installed_facts(), mod_changed)),
            Ok(Comparison::Differs(vec!["the mod"])),
            "two Steam users, one piece"
        );

        let mut both = identical.clone();
        both[2] = step(Item::NativeDll, Outcome::Done, "updated");
        both[0] = step(Item::Mod, Outcome::Done, "updated (mirror copy)");
        assert_eq!(
            compare(&with_plan(installed_facts(), both)),
            Ok(Comparison::Differs(vec!["the mod", "the native module (winhttp.dll)"]))
        );

        let mut moved = identical.clone();
        moved[3] = step(Item::PathFile, Outcome::Done, "updated");
        moved[1] = step(Item::AppIdFile, Outcome::Done, "created");
        assert_eq!(
            compare(&with_plan(installed_facts(), moved)),
            Ok(Comparison::HelpersDiffer(vec!["steam_appid.txt", "the recorded launcher location"]))
        );

        let fresh =
            vec![step(Item::Mod, Outcome::Done, "installed"), step(Item::NativeDll, Outcome::Done, "installed")];
        assert_eq!(compare(&with_plan(base_facts(), fresh)), Ok(Comparison::NotInstalled));
    }

    #[test]
    fn the_comparison_explains_why_it_cannot_compare() {
        assert_eq!(
            compare(&{
                let mut f = base_facts();
                f.steam_path = None;
                f
            }),
            Err(Problem::SteamMissing)
        );
        assert_eq!(
            compare(&{
                let mut f = base_facts();
                f.game_dir = None;
                f
            }),
            Err(Problem::GameMissing)
        );
        let mut no_payload = base_facts();
        no_payload.payload_mod = None;
        assert_eq!(compare(&no_payload), Err(Problem::PackageIncomplete { mod_missing: true, dll_missing: false }));
        let foreign =
            with_plan(base_facts(), vec![step(Item::NativeDll, Outcome::Failed, "not ours; not overwritten")]);
        assert!(matches!(compare(&foreign), Err(Problem::ForeignDll(_))));
    }

    #[test]
    fn the_comparison_text_gives_a_verdict_in_words() {
        let identical = vec![step(Item::Mod, Outcome::Skipped, "x"), step(Item::NativeDll, Outcome::Skipped, "x")];
        let text = comparison_lines(&with_plan(installed_facts(), identical)).join("\n");
        assert!(text.contains(&format!("This folder:  version {}", own_version())), "{text}");
        assert!(text.contains(&format!("Installed:    version {}", own_version())), "{text}");
        assert!(text.contains("identical to the ones in this folder. Nothing to do."), "{text}");
        assert!(text.contains("not built in yet") && text.contains(RELEASES_URL), "no network yet: {text}");

        let differs = vec![step(Item::Mod, Outcome::Done, "updated")];
        let verdict = |installed_version: Option<&str>| {
            let mut f = with_plan(installed_facts(), differs.clone());
            f.installed_version = installed_version.map(str::to_owned);
            comparison_lines(&f).join("\n")
        };
        assert!(verdict(Some("0.0.1")).contains("This folder is NEWER than what is installed"));
        assert!(verdict(Some("99.0.0")).contains("This folder is OLDER than what is installed"));
        assert!(verdict(Some(own_version())).contains("The version number is the same, but the mod differs from"));
        let two = vec![step(Item::Mod, Outcome::Done, "updated"), step(Item::NativeDll, Outcome::Done, "updated")];
        let mut both = with_plan(installed_facts(), two);
        both.installed_version = Some(own_version().to_owned());
        let text = comparison_lines(&both).join("\n");
        assert!(text.contains("but the mod and the native module (winhttp.dll) differ from"), "{text}");
        let unknown = verdict(None);
        assert!(unknown.contains("installed version is not known"), "{unknown}");
        assert!(unknown.contains("its version is not known"), "{unknown}");

        let helpers =
            comparison_lines(&with_plan(installed_facts(), vec![step(Item::PathFile, Outcome::Done, "updated")]))
                .join("\n");
        assert!(helpers.contains("Only the recorded launcher location would be updated"), "{helpers}");
        let fresh =
            comparison_lines(&with_plan(base_facts(), vec![step(Item::Mod, Outcome::Done, "installed")])).join("\n");
        assert!(fresh.contains("not installed. Choose 1"), "{fresh}");
        let mut no_game = base_facts();
        no_game.game_dir = None;
        assert!(comparison_lines(&no_game).join("\n").contains("Transport Fever 3 was not found"));
    }
}
