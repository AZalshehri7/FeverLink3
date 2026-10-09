//! The checks screen: six lines about this PC, the next step, and the whole screen the menu loop draws.

use feverlink3_launcher::install;

use super::facts::{folder_dir, installed_state, package_state, Facts, InstallState, Package};
use super::menu::menu_lines;
use super::plans::{compare, Comparison};
use super::problems::{Problem, STEAM_URL};
use super::versions::{own_version, BuildCheck};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Good,
    Warn,
    Bad,
    Note,
}

impl Status {
    /// Plain ASCII, so that it reads the same in every console font.
    pub(super) fn tag(self) -> &'static str {
        match self {
            Status::Good => "[ok]",
            Status::Warn => "[??]",
            Status::Bad => "[!!]",
            Status::Note => "[--]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Check {
    status: Status,
    label: &'static str,
    text: String,
}

fn check(status: Status, label: &'static str, text: impl Into<String>) -> Check {
    Check { status, label, text: text.into() }
}

/// The six lines of the checks screen: Steam, the game, its build, whether it runs, whether FeverLink3 is
/// installed, and whether this folder can be installed from.
pub(super) fn checks(f: &Facts) -> Vec<Check> {
    vec![steam_check(f), game_check(f), build_check(f), running_check(f), installed_check(f), folder_check(f)]
}

fn steam_check(f: &Facts) -> Check {
    match &f.steam_path {
        Some(path) => check(Status::Good, "Steam", path.display().to_string()),
        None => check(Status::Bad, "Steam", format!("not found - install Steam first ({STEAM_URL})")),
    }
}

fn game_check(f: &Facts) -> Check {
    match (&f.game_dir, &f.steam_path) {
        (Some(dir), _) => check(Status::Good, "Transport Fever 3", dir.display().to_string()),
        (None, Some(_)) => check(
            Status::Bad,
            "Transport Fever 3",
            "not found in your Steam libraries - install it from Steam and start it once",
        ),
        (None, None) => check(Status::Bad, "Transport Fever 3", "not found (Steam is missing)"),
    }
}

fn build_check(f: &Facts) -> Check {
    match &f.build {
        BuildCheck::NoGame => check(Status::Note, "Game build", "not checked (the game was not found)"),
        BuildCheck::Unreadable(why) => check(Status::Warn, "Game build", format!("could not be read ({why})")),
        BuildCheck::Known { name } => {
            check(Status::Good, "Game build", format!("{name} - FeverLink3 knows this build"))
        }
        BuildCheck::Unknown { timestamp } => check(
            Status::Warn,
            "Game build",
            format!(
                "unknown (id {timestamp:#010x}) - FeverLink3 has no hook addresses for it, so the mod would run in a \
                 reduced mode. Tell the author the build number shown at the bottom right of the game's main menu"
            ),
        ),
    }
}

fn running_check(f: &Facts) -> Check {
    if f.game_running {
        check(Status::Warn, "Game running", "yes - close it before installing or uninstalling")
    } else {
        check(Status::Good, "Game running", "no")
    }
}

fn installed_check(f: &Facts) -> Check {
    const LABEL: &str = "FeverLink3";
    match installed_state(f) {
        InstallState::Unknown => check(Status::Note, LABEL, "unknown (there is no game folder to look in)"),
        InstallState::NotInstalled => check(Status::Note, LABEL, "not installed yet"),
        InstallState::Complete { version: Some(v) } => check(Status::Good, LABEL, format!("installed (version {v})")),
        InstallState::Complete { version: None } => check(Status::Good, LABEL, "installed (version not recorded)"),
        InstallState::Partial { native_module, mods, users } => check(
            Status::Warn,
            LABEL,
            format!(
                "only partly installed (native module: {}, mod: {mods} of {users} Steam users) - choose 1 to finish",
                if native_module { "yes" } else { "no" }
            ),
        ),
        InstallState::ForeignDll => check(
            Status::Bad,
            LABEL,
            "another program's winhttp.dll is in the game folder; FeverLink3 will not touch it",
        ),
    }
}

fn folder_check(f: &Facts) -> Check {
    match package_state(f) {
        Package::Complete => check(
            Status::Good,
            "This folder",
            format!("{} (version {}, all files present)", folder_dir(f), own_version()),
        ),
        Package::TemporaryFolder => {
            check(Status::Bad, "This folder", "inside a zip file or a temporary folder - unzip it first")
        }
        Package::Incomplete { .. } => check(Status::Bad, "This folder", "incomplete - unzip the whole zip file first"),
    }
}

pub(super) fn render_checks(checks: &[Check]) -> Vec<String> {
    checks.iter().map(|c| format!("  {}  {:<18} {}", c.status.tag(), c.label, c.text)).collect()
}

/// What to do next, in one sentence, from the state of this PC: the first thing that is wrong, else the next
/// useful step.
fn next_step(f: &Facts) -> String {
    if f.steam_path.is_none() {
        return Problem::SteamMissing.message();
    }
    let Some(game_dir) = &f.game_dir else { return Problem::GameMissing.message() };
    match installed_state(f) {
        InstallState::ForeignDll => Problem::ForeignDll(game_dir.join(install::DLL_NAME)).message(),
        InstallState::Complete { .. } if f.game_running => {
            "Transport Fever 3 is running: in the game, click FeverLink3 in the bar at the bottom of the screen.".into()
        }
        InstallState::Complete { .. } if matches!(compare(f), Ok(Comparison::Differs(_))) => {
            "this folder holds different files than the installed ones. Choose 3 to compare them or 1 to update; \
             otherwise choose 4 to start Transport Fever 3."
                .into()
        }
        InstallState::Complete { .. } => {
            "choose 4 to start Transport Fever 3, then click FeverLink3 in the bar at the bottom of the game.".into()
        }
        state => match package_state(f) {
            Package::TemporaryFolder => Problem::TemporaryFolder.message(),
            Package::Incomplete { mod_missing, dll_missing } => {
                Problem::PackageIncomplete { mod_missing, dll_missing }.message()
            }
            Package::Complete if f.game_running => {
                "close Transport Fever 3, then choose 1 to install FeverLink3.".into()
            }
            Package::Complete if matches!(state, InstallState::Partial { .. }) => {
                "choose 1 to finish installing FeverLink3.".into()
            }
            Package::Complete => "choose 1 to install FeverLink3.".into(),
        },
    }
}

/// The whole screen the menu loop draws: banner, checks, next step, menu.
pub(super) fn render_screen(f: &Facts) -> String {
    let mut lines = vec![
        format!("FeverLink3 {} - cooperative multiplayer for Transport Fever 3", own_version()),
        String::new(),
        "Checking this PC...".to_owned(),
    ];
    lines.extend(render_checks(&checks(f)));
    lines.push(String::new());
    lines.push(format!("Next step: {}", next_step(f)));
    lines.push(String::new());
    lines.extend(menu_lines());
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use feverlink3_launcher::install::{DllState, Item, Outcome};

    use super::*;
    use crate::wizard::testkit::*;

    #[test]
    fn the_checks_of_a_healthy_pc_before_install() {
        let lines = render_checks(&checks(&base_facts()));
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0], r"  [ok]  Steam              C:\Steam");
        assert_eq!(lines[1], r"  [ok]  Transport Fever 3  C:\Steam\steamapps\common\Transport Fever 3");
        assert_eq!(lines[2], "  [ok]  Game build         40420 - FeverLink3 knows this build");
        assert_eq!(lines[3], "  [ok]  Game running       no");
        assert_eq!(lines[4], "  [--]  FeverLink3         not installed yet");
        assert_eq!(
            lines[5],
            format!(r"  [ok]  This folder        C:\FeverLink3-0.1.0 (version {}, all files present)", own_version())
        );
    }

    #[test]
    fn the_checks_say_what_is_wrong_in_plain_words() {
        let mut f = base_facts();
        f.steam_path = None;
        f.game_dir = None;
        f.game_files = None;
        f.build = BuildCheck::NoGame;
        let text = render_checks(&checks(&f)).join("\n");
        assert!(text.contains("[!!]  Steam"), "{text}");
        assert!(text.contains(STEAM_URL));
        assert!(text.contains("[!!]  Transport Fever 3  not found (Steam is missing)"), "{text}");
        assert!(text.contains("[--]  Game build         not checked"), "{text}");
        assert!(text.contains("[--]  FeverLink3         unknown"), "{text}");

        let mut game_missing = base_facts();
        game_missing.game_dir = None;
        assert!(render_checks(&checks(&game_missing)).join("\n").contains("not found in your Steam libraries"));

        let mut running = base_facts();
        running.game_running = true;
        assert!(render_checks(&checks(&running))[3].contains("[??]  Game running       yes - close it"));

        let mut unknown_build = base_facts();
        unknown_build.build = BuildCheck::Unknown { timestamp: 0x1234_abcd };
        let line = &render_checks(&checks(&unknown_build))[2];
        assert!(line.contains("[??]") && line.contains("0x1234abcd") && line.contains("reduced mode"), "{line}");

        let mut unreadable_build = base_facts();
        unreadable_build.build = BuildCheck::Unreadable("denied".into());
        assert!(render_checks(&checks(&unreadable_build))[2].contains("could not be read (denied)"));
    }

    #[test]
    fn the_install_line_shows_the_version_or_what_is_missing() {
        let line = |f: &Facts| installed_check(f).text;
        assert_eq!(line(&installed_facts()), format!("installed (version {})", own_version()));
        let mut unknown_version = installed_facts();
        unknown_version.installed_version = None;
        assert_eq!(line(&unknown_version), "installed (version not recorded)");
        let mut partial = installed_facts();
        partial.users[0].mod_installed = false;
        assert!(line(&partial).contains("native module: yes, mod: 0 of 1 Steam users"), "{}", line(&partial));
        let mut foreign = base_facts();
        foreign.game_files.as_mut().unwrap().dll = Ok(DllState::Foreign);
        assert_eq!(installed_check(&foreign).status, Status::Bad);
    }

    #[test]
    fn the_folder_line_warns_about_zip_and_incomplete_folders() {
        let mut f = base_facts();
        f.payload_mod = None;
        let check = folder_check(&f);
        assert_eq!((check.status, check.text.as_str()), (Status::Bad, "incomplete - unzip the whole zip file first"));
        f.exe_path = PathBuf::from("C:/Users/Ann/AppData/Local/Temp/Temp1_FeverLink3-0.1.0.zip/feverlink3.exe");
        assert!(folder_check(&f).text.contains("zip file or a temporary folder"));
    }

    #[test]
    fn next_step_names_the_first_thing_to_do() {
        let next = |f: &Facts| next_step(f);
        assert_eq!(next(&base_facts()), "choose 1 to install FeverLink3.");
        assert!(next(&installed_facts()).starts_with("choose 4 to start Transport Fever 3"));

        let mut no_steam = base_facts();
        no_steam.steam_path = None;
        assert_eq!(next(&no_steam), Problem::SteamMissing.message());
        let mut no_game = base_facts();
        no_game.game_dir = None;
        assert_eq!(next(&no_game), Problem::GameMissing.message());

        let mut running = base_facts();
        running.game_running = true;
        assert_eq!(next(&running), "close Transport Fever 3, then choose 1 to install FeverLink3.");
        let mut playing = installed_facts();
        playing.game_running = true;
        assert!(next(&playing).contains("click FeverLink3 in the bar at the bottom"));

        let mut partial = base_facts();
        partial.game_files.as_mut().unwrap().dll = Ok(DllState::Ours);
        assert_eq!(next(&partial), "choose 1 to finish installing FeverLink3.");

        let mut incomplete = base_facts();
        incomplete.payload_mod = None;
        assert_eq!(next(&incomplete), Problem::PackageIncomplete { mod_missing: true, dll_missing: false }.message());
        let mut zipped = base_facts();
        zipped.exe_path = PathBuf::from("C:/Users/Ann/AppData/Local/Temp/Temp1_FeverLink3-0.1.0.zip/feverlink3.exe");
        assert_eq!(next(&zipped), Problem::TemporaryFolder.message());

        let mut foreign = base_facts();
        foreign.game_files.as_mut().unwrap().dll = Ok(DllState::Foreign);
        assert!(next(&foreign).contains("does not belong to FeverLink3"));

        // installed, but this folder holds other files: the comparison is offered
        let mut changed = installed_facts();
        changed.plan = Some(report(vec![step(Item::Mod, Outcome::Done, "updated (mirror copy)")]));
        assert!(next(&changed).contains("Choose 3 to compare"), "{}", next(&changed));
    }

    #[test]
    fn the_screen_holds_the_banner_the_checks_the_next_step_and_the_menu() {
        let screen = render_screen(&base_facts());
        assert!(screen.starts_with(&format!("FeverLink3 {} - cooperative multiplayer", own_version())));
        let order: Vec<usize> =
            ["Checking this PC...", "[ok]  Steam", "Next step: choose 1", "What would you like to do?"]
                .iter()
                .map(|part| screen.find(part).unwrap_or_else(|| panic!("{part} is on the screen")))
                .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "in this order: {order:?}");
        assert!(screen.ends_with("Type a number and press Enter.\n"));
    }
}
