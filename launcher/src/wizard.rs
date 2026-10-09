//! The console wizard: what `feverlink3.exe` does when it is started without arguments, which is what a double-click
//! is. A player who has never opened a terminal can install, check, start the game, find the logs and collect a
//! diagnostics text from one numbered menu, and the window never closes by itself.
//!
//! The pieces, one module each:
//!
//! - [`menu`]: the numbered menu and what a typed line means;
//! - [`facts`] and [`versions`]: one snapshot of this PC, and the game build and launcher versions in it;
//! - [`plans`], [`problems`] and [`screen`]: what each choice may do before it starts, how trouble is explained in
//!   plain words, and the checks screen; all of them pure functions over the snapshot, so they are tested without
//!   Steam or the game;
//! - [`diagnostics`]: the `doctor` text and the diagnostics text, with the user name and join codes taken out;
//! - [`system`]: the machine behind a trait, the only code that starts programs (Steam, Explorer, `clip.exe`).
//!
//! This file holds the menu loop, [`run`], which works over any input and output: the tests drive it with scripted
//! lines and a fake PC made of temporary folders. Install and uninstall are `feverlink3_launcher::install`'s own
//! functions; the wizard adds the questions asked before and the plain-language explanation after, never a second
//! way to change the game folder.

mod diagnostics;
mod facts;
mod menu;
mod plans;
mod problems;
mod screen;
mod system;
#[cfg(test)]
mod testkit;
mod versions;

use std::cmp::Ordering;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use feverlink3_launcher::install::{self, Payload};

use diagnostics::{build_diagnostics, doctor_text, read_logs};
use facts::{gather, installed_state, Facts, InstallState};
use menu::{parse_choice, Choice, Typed};
use plans::{comparison_lines, plan_install, plan_logs, plan_start, plan_uninstall};
use problems::{summarize_install, summarize_uninstall, Summary};
use screen::{render_screen, Status};
use system::{windows_program, RealSystem, System};
use versions::{own_version, version_order};

/// The text of `feverlink3 doctor` for this PC.
pub fn doctor_report() -> String {
    doctor_text(&gather(&RealSystem))
}

/// One line from the input, `None` at its end (a closed pipe, or Ctrl+Z in a console).
fn read_line(input: &mut dyn BufRead) -> io::Result<Option<String>> {
    let mut line = String::new();
    Ok((input.read_line(&mut line)? > 0).then_some(line))
}

fn say(out: &mut dyn Write, text: &str) -> io::Result<()> {
    writeln!(out, "{text}")
}

/// Asks a yes/no question whose default is no.
fn confirm(input: &mut dyn BufRead, out: &mut dyn Write, question: &str) -> io::Result<bool> {
    writeln!(out, "{question}")?;
    write!(out, "Type y and press Enter to continue, or just press Enter to cancel: ")?;
    out.flush()?;
    Ok(read_line(input)?.is_some_and(|line| matches!(line.trim().to_lowercase().as_str(), "y" | "yes")))
}

/// The ending of an install or uninstall, tagged like the checks so that success and trouble look different.
fn say_summary(out: &mut dyn Write, summary: &Summary) -> io::Result<()> {
    let status = if summary.ok { Status::Good } else { Status::Bad };
    writeln!(out, "\n{} {}", status.tag(), summary.headline)?;
    summary.notes.iter().try_for_each(|note| writeln!(out, "  - {note}"))
}

fn user_dirs(f: &Facts) -> Vec<PathBuf> {
    f.users.iter().map(|u| u.path.clone()).collect()
}

fn action_install(sys: &dyn System, input: &mut dyn BufRead, out: &mut dyn Write) -> io::Result<()> {
    let facts = gather(sys);
    let plan = match plan_install(&facts) {
        Ok(plan) => plan,
        Err(problem) => return say(out, &problem.message()),
    };
    // an old folder run by mistake must not quietly replace newer files
    if let (Some(Ordering::Less), Some(installed)) =
        (version_order(own_version(), facts.installed_version.as_deref()), facts.installed_version.as_deref())
    {
        let question = format!(
            "Version {installed} of FeverLink3 is installed, and this folder holds the older version {}. Installing \
             from this folder replaces the newer files with older ones.",
            own_version()
        );
        if !confirm(input, out, &question)? {
            return say(out, "Nothing was changed.");
        }
    }
    writeln!(out, "Installing FeverLink3 ...")?;
    let payload = Payload {
        mod_source: Some(plan.mod_source),
        native_dll: Some(plan.native_dll),
        launcher_path: &facts.exe_path,
    };
    let report = install::install(plan.game_dir, &user_dirs(&facts), payload, false);
    write!(out, "{report}")?;
    say_summary(out, &summarize_install(&report))
}

const UNINSTALL_QUESTION: &str = "This removes FeverLink3 from Transport Fever 3: the mod folder, winhttp.dll and \
    the two small files FeverLink3 put next to the game. Your savegames and settings are not touched.";

fn action_uninstall(sys: &dyn System, input: &mut dyn BufRead, out: &mut dyn Write) -> io::Result<()> {
    let facts = gather(sys);
    let game_dir = match plan_uninstall(&facts) {
        Ok(dir) => dir,
        Err(problem) => return say(out, &problem.message()),
    };
    if !confirm(input, out, UNINSTALL_QUESTION)? {
        return say(out, "Nothing was changed.");
    }
    writeln!(out, "Removing FeverLink3 ...")?;
    let report = install::uninstall(game_dir, &user_dirs(&facts), false);
    write!(out, "{report}")?;
    say_summary(out, &summarize_uninstall(&report))
}

fn action_start(sys: &dyn System, out: &mut dyn Write) -> io::Result<()> {
    let facts = gather(sys);
    let steam_path = match plan_start(&facts) {
        Ok(path) => path,
        Err(problem) => return say(out, &problem.message()),
    };
    match sys.start_steam_game(steam_path) {
        Ok(()) => {
            say(
                out,
                "Asked Steam to start Transport Fever 3. If Steam was not running it starts first, which can take a \
                 minute; sign in if it asks. You can leave this window open.",
            )?;
            if !matches!(installed_state(&facts), InstallState::Complete { .. }) {
                say(
                    out,
                    "Note: FeverLink3 is not fully installed, so the game starts without multiplayer. Choose 1 first \
                     if you want to play with others.",
                )?;
            }
            Ok(())
        }
        Err(e) => say(
            out,
            &format!(
                "Steam could not be started ({e}). Start Steam yourself, then start Transport Fever 3 from its \
                 library."
            ),
        ),
    }
}

fn action_logs(sys: &dyn System, out: &mut dyn Write) -> io::Result<()> {
    let facts = gather(sys);
    let dir = match plan_logs(&facts) {
        Ok(dir) => dir,
        Err(problem) => return say(out, &problem.message()),
    };
    match sys.open_folder(dir) {
        Ok(()) => say(out, &format!("Opened {}.", dir.display())),
        Err(e) => say(
            out,
            &format!(
                "The folder could not be opened ({e}). Paste this into the address bar of a File Explorer window: {}",
                dir.display()
            ),
        ),
    }
}

fn action_diagnostics(sys: &dyn System, out: &mut dyn Write) -> io::Result<()> {
    let facts = gather(sys);
    let (native, mod_log) = read_logs(facts.local_dir.as_deref());
    let text = build_diagnostics(&facts, sys.now_unix(), &native, &mod_log, &sys.user_names());
    match sys.copy_to_clipboard(&text) {
        Ok(()) => {
            say(
                out,
                &format!(
                    "Copied {} lines to the clipboard. Paste them into your message with Ctrl+V.",
                    text.lines().count()
                ),
            )?;
            say(
                out,
                "Your Windows user name is shown as <user>. The text still holds folder names, and the logs can hold \
                 network addresses; read it first if you like.",
            )
        }
        Err(e) => {
            say(
                out,
                &format!("The text could not be copied to the clipboard ({e}). Select it below and copy it from here:"),
            )?;
            say(out, "----------------------------------------")?;
            write!(out, "{text}")?;
            say(out, "----------------------------------------")
        }
    }
}

fn perform(choice: Choice, sys: &dyn System, input: &mut dyn BufRead, out: &mut dyn Write) -> io::Result<()> {
    match choice {
        Choice::Install => action_install(sys, input, out),
        Choice::Uninstall => action_uninstall(sys, input, out),
        Choice::CheckVersion => say(out, &comparison_lines(&gather(sys)).join("\n")),
        Choice::StartGame => action_start(sys, out),
        Choice::OpenLogs => action_logs(sys, out),
        Choice::Diagnostics => action_diagnostics(sys, out),
        Choice::Exit => Ok(()),
    }
}

/// The menu loop: draw the checks and the menu, read a choice, do it, wait for Enter, repeat. It ends at the
/// Exit choice or when the input ends, and every action is followed by a wait so that a window opened by a
/// double-click is never closed under the reader's eyes.
fn run(sys: &dyn System, input: &mut dyn BufRead, out: &mut dyn Write) -> io::Result<()> {
    loop {
        write!(out, "\n{}", render_screen(&gather(sys)))?;
        let choice = loop {
            write!(out, "> ")?;
            out.flush()?;
            let Some(line) = read_line(input)? else { return Ok(()) };
            match parse_choice(&line) {
                Typed::Choice(choice) => break choice,
                Typed::Nothing => {}
                Typed::Unknown(text) => {
                    writeln!(out, "\"{text}\" is not on the menu. Type one of the numbers above, for example 1.")?;
                }
            }
        };
        if choice == Choice::Exit {
            return Ok(());
        }
        writeln!(out)?;
        perform(choice, sys, input, out)?;
        write!(out, "\nPress Enter to go back to the menu.")?;
        out.flush()?;
        if read_line(input)?.is_none() {
            return Ok(());
        }
    }
}

/// Shows the error and keeps the window open until a key is pressed. `pause` reads the console itself, so this
/// also works from a panic hook that runs while the panicking thread holds the standard input.
fn wait_for_key() {
    let _ = Command::new(windows_program(&["System32", "cmd.exe"])).args(["/C", "pause"]).status();
}

/// The release profile aborts on a panic, which would close a double-clicked window without a word; this prints
/// what happened first and waits. Nothing in the wizard is meant to panic.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let _ = writeln!(
            io::stderr(),
            "\nFeverLink3 hit an unexpected problem and has to stop:\n{info}\nPlease tell whoever helps you what you \
             were doing when it happened."
        );
        wait_for_key();
    }));
}

/// `feverlink3.exe` without arguments: the wizard on this PC's console.
pub fn run_interactive() -> ExitCode {
    install_panic_hook();
    let result = {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let (mut input, mut out) = (stdin.lock(), stdout.lock());
        run(&RealSystem, &mut input, &mut out)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "\nFeverLink3 cannot go on: {e}");
            wait_for_key();
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use feverlink3_launcher::install;
    use feverlink3_launcher::GAME_EXE;

    use super::testkit::*;
    use super::*;

    #[test]
    fn the_menu_ends_quietly_when_the_input_ends() {
        let pc = Pc::new();
        let out = drive(&pc, "");
        assert!(out.contains("Checking this PC...") && out.contains("What would you like to do?"), "{out}");
        assert_eq!(out.matches("What would you like to do?").count(), 1, "drawn once, then the input ended");
        let after_choice = drive(&pc, "5\n");
        assert!(after_choice.contains("Press Enter to go back to the menu."), "{after_choice}");
    }

    #[test]
    fn exit_ends_the_loop_and_does_nothing_else() {
        let pc = Pc::new();
        let out = drive(&pc, "0\n1\n1\n");
        assert_eq!(out.matches("What would you like to do?").count(), 1);
        assert!(!pc.game().join(install::DLL_NAME).exists(), "the lines after Exit are never read");
        assert!(!drive(&pc, "q\n").contains("Press Enter"));
    }

    #[test]
    fn unknown_text_and_empty_lines_keep_the_menu_open() {
        let pc = Pc::new();
        let out = drive(&pc, "\nhello\n9\n\n0\n");
        assert!(out.contains("\"hello\" is not on the menu"), "{out}");
        assert!(out.contains("\"9\" is not on the menu"), "{out}");
        assert_eq!(out.matches("is not on the menu").count(), 2, "an empty line is not an error");
        assert_eq!(out.matches("What would you like to do?").count(), 1, "no redraw for a typing slip");
    }

    #[test]
    fn every_action_waits_for_enter_and_then_draws_the_menu_again() {
        for key in ["1", "3", "4", "5", "6"] {
            let pc = Pc::new();
            let out = drive(&pc, &format!("{key}\n\n0\n"));
            assert_eq!(out.matches("Press Enter to go back to the menu.").count(), 1, "{key}: {out}");
            assert_eq!(out.matches("What would you like to do?").count(), 2, "{key}");
            let wait = out.find("Press Enter").unwrap();
            assert!(out[wait..].contains("Checking this PC..."), "{key}: the menu follows the wait");
        }
        // uninstall asks first; the answer, then the wait
        let pc = Pc::new();
        let out = drive(&pc, "2\nn\n\n0\n");
        assert_eq!(out.matches("Press Enter to go back to the menu.").count(), 1, "{out}");
    }

    #[test]
    fn install_through_the_menu_puts_the_four_things_in_place() {
        let pc = Pc::new();
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("Installing FeverLink3 ..."), "{out}");
        assert!(out.contains("[ok] FeverLink3 is installed."), "{out}");
        assert!(out.contains("Add Mods"), "{out}");
        assert!(pc.game().join(install::DLL_NAME).is_file());
        assert!(pc.game().join(install::APPID_FILE).is_file());
        assert_eq!(fs::read_to_string(pc.game().join(install::PATH_FILE)).unwrap(), pc.exe.to_string_lossy());
        assert_eq!(fs::read_to_string(pc.installed_mod().join("mod.json")).unwrap(), "{}");
        assert_eq!(fs::read_to_string(pc.installed_mod().join("content").join("a.lua")).unwrap(), "a");
        // the next screen knows
        let second_screen = &out[out.find("Press Enter").unwrap()..];
        assert!(second_screen.contains(&format!("installed (version {})", own_version())), "{second_screen}");
        assert!(second_screen.contains("Next step: choose 4"), "{second_screen}");
    }

    #[test]
    fn installing_again_changes_nothing_and_says_so() {
        let pc = Pc::new();
        drive(&pc, "1\n\n0\n");
        let before = fs::read(pc.game().join(install::DLL_NAME)).unwrap();
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("already installed and up to date"), "{out}");
        assert!(!out.contains("DONE"), "{out}");
        assert_eq!(fs::read(pc.game().join(install::DLL_NAME)).unwrap(), before);
    }

    #[test]
    fn a_changed_mod_is_repaired_by_installing_again() {
        let pc = Pc::new();
        drive(&pc, "1\n\n0\n");
        fs::write(pc.installed_mod().join("content").join("a.lua"), "damaged").unwrap();
        fs::remove_file(pc.game().join(install::APPID_FILE)).unwrap();
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("FeverLink3 is installed."), "{out}");
        assert_eq!(fs::read_to_string(pc.installed_mod().join("content").join("a.lua")).unwrap(), "a");
        assert!(pc.game().join(install::APPID_FILE).is_file());
    }

    #[test]
    fn install_is_refused_while_the_game_runs() {
        let pc = Pc::new();
        pc.running.set(true);
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("Transport Fever 3 is running, so its files are locked"), "{out}");
        assert!(out.contains("[??]  Game running       yes"), "{out}");
        assert!(!out.contains("Installing FeverLink3"), "{out}");
        assert!(!pc.game().join(install::DLL_NAME).exists());
        assert!(!pc.game().join(install::PATH_FILE).exists());
        assert!(!pc.installed_mod().exists());
    }

    #[test]
    fn install_without_steam_or_game_explains_and_changes_nothing() {
        let mut pc = Pc::new();
        let game = pc.game();
        pc.steam = None;
        pc.game = None;
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("Steam was not found on this PC"), "{out}");
        assert!(!game.join(install::DLL_NAME).exists());

        let mut no_game = Pc::new();
        no_game.game = None;
        let out = drive(&no_game, "1\n\n0\n");
        assert!(out.contains("Transport Fever 3 was not found in your Steam libraries"), "{out}");
    }

    #[test]
    fn install_from_an_incomplete_or_zipped_folder_says_to_unzip() {
        let mut pc = Pc::new();
        pc.payload.0 = None;
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("This FeverLink3 folder is incomplete"), "{out}");
        assert!(out.contains("Extract All"), "{out}");
        assert!(!pc.game().join(install::DLL_NAME).exists(), "not even a partial install");
        assert!(!pc.game().join(install::APPID_FILE).exists());

        let mut zipped = Pc::new();
        zipped.exe =
            PathBuf::from("C:/Users/Ann/AppData/Local/Temp/Temp1_FeverLink3-0.1.0.zip/FeverLink3-0.1.0/feverlink3.exe");
        let out = drive(&zipped, "1\n\n0\n");
        assert!(out.contains("running from inside the downloaded zip file"), "{out}");
        assert!(!zipped.game().join(install::PATH_FILE).exists(), "no temporary path is recorded");
    }

    #[test]
    fn another_mods_winhttp_dll_stops_the_install_and_is_never_touched() {
        let pc = Pc::new();
        fs::write(pc.game().join(install::DLL_NAME), b"MZ another mod's proxy").unwrap();
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("does not belong to FeverLink3"), "{out}");
        assert!(out.contains("[!!]  FeverLink3"), "{out}");
        assert_eq!(fs::read(pc.game().join(install::DLL_NAME)).unwrap(), b"MZ another mod's proxy");
        assert!(!pc.installed_mod().exists(), "the mod is not installed next to a DLL that cannot load it");
    }

    #[test]
    fn without_a_steam_user_folder_the_install_is_partial_and_says_how_to_finish() {
        let mut pc = Pc::new();
        pc.users.clear();
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("[!!] FeverLink3 is only partly installed."), "{out}");
        assert!(out.contains("has not created its Steam user folder yet"), "{out}");
        assert!(pc.game().join(install::DLL_NAME).is_file());
        assert!(out.contains("[??]  FeverLink3         only partly installed: the native module is there, but the game has not created its user folder yet"), "{out}");
    }

    #[test]
    fn a_failing_write_is_explained_in_plain_words() {
        let pc = Pc::new();
        // a folder where the DLL belongs makes the step fail, as a lock or a permission problem would
        fs::create_dir_all(pc.game().join(install::DLL_NAME)).unwrap();
        let out = drive(&pc, "1\n\n0\n");
        assert!(out.contains("FAILED"), "the report lists the failed step: {out}");
        assert!(out.contains("[!!] FeverLink3 could not be installed completely."), "{out}");
        let explained = out.contains("Windows did not allow FeverLink3 to change")
            || out.contains("Something unexpected went wrong: winhttp.dll");
        assert!(explained, "a plain-language explanation follows: {out}");
    }

    #[test]
    fn uninstall_asks_first_and_removes_exactly_what_was_installed() {
        let pc = Pc::new();
        drive(&pc, "1\n\n0\n");
        let other_mod = install::mod_dir(&pc.users[0]).with_file_name("someone_elses_mod_1");
        fs::create_dir_all(&other_mod).unwrap();

        let declined = drive(&pc, "2\n\n\n0\n");
        assert!(declined.contains("Type y and press Enter to continue"), "{declined}");
        assert!(declined.contains("Nothing was changed."), "{declined}");
        assert!(pc.game().join(install::DLL_NAME).is_file(), "an empty answer means no");

        let removed = drive(&pc, "2\ny\n\n0\n");
        assert!(removed.contains("Removing FeverLink3 ..."), "{removed}");
        assert!(removed.contains("FeverLink3 was removed. Transport Fever 3 starts exactly as before."), "{removed}");
        assert!(!pc.game().join(install::DLL_NAME).exists());
        assert!(!pc.game().join(install::APPID_FILE).exists());
        assert!(!pc.game().join(install::PATH_FILE).exists());
        assert!(!pc.installed_mod().exists());
        assert!(other_mod.is_dir(), "another mod is left alone");
        assert!(pc.game().join(GAME_EXE).is_file());

        let again = drive(&pc, "2\nyes\n\n0\n");
        assert!(again.contains("FeverLink3 was not installed. Nothing was removed."), "{again}");
    }

    #[test]
    fn uninstall_is_refused_while_the_game_runs() {
        let pc = Pc::new();
        drive(&pc, "1\n\n0\n");
        pc.running.set(true);
        let out = drive(&pc, "2\ny\n\n0\n");
        assert!(out.contains("Transport Fever 3 is running"), "{out}");
        assert!(!out.contains("Type y and press Enter"), "no question for something that cannot be done: {out}");
        assert!(pc.game().join(install::DLL_NAME).is_file());
    }

    #[test]
    fn the_version_check_compares_this_folder_with_the_installed_files() {
        let pc = Pc::new();
        let fresh = drive(&pc, "3\n\n0\n");
        assert!(fresh.contains("FeverLink3 is not installed. Choose 1"), "{fresh}");

        drive(&pc, "1\n\n0\n");
        let same = drive(&pc, "3\n\n0\n");
        assert!(same.contains(&format!("This folder:  version {}", own_version())), "{same}");
        assert!(same.contains(&format!("Installed:    version {}", own_version())), "{same}");
        assert!(same.contains("identical to the ones in this folder. Nothing to do."), "{same}");

        fs::write(pc.mod_src().join("mod.json"), "{\"revision\":2}").unwrap();
        let changed = drive(&pc, "3\n\n0\n");
        assert!(changed.contains("The version number is the same, but the mod differs from"), "{changed}");
        assert!(changed.contains("Choose 3 to compare"), "the screen offers it too: {changed}");

        let moved = {
            let mut moved = Pc::new();
            moved.exe = PathBuf::from("C:/Elsewhere/feverlink3.exe");
            moved
        };
        drive(&moved, "1\n\n0\n");
        assert_eq!(fs::read_to_string(moved.game().join(install::PATH_FILE)).unwrap(), "C:/Elsewhere/feverlink3.exe");
    }

    #[test]
    fn the_version_check_reads_the_installed_version_from_the_launcher_that_installed() {
        let pc = Pc::new();
        drive(&pc, "1\n\n0\n");
        // an earlier download put the files there; its launcher still exists and carries its marker
        let old = pc.root.path().join("old").join("feverlink3.exe");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, b"MZ feverlink3-launcher-version=0.0.5\0").unwrap();
        fs::write(pc.game().join(install::PATH_FILE), old.to_string_lossy().as_bytes()).unwrap();
        fs::write(pc.mod_src().join("mod.json"), "{\"revision\":2}").unwrap();

        let out = drive(&pc, "3\n\n0\n");
        assert!(out.contains("Installed:    version 0.0.5  (installed from"), "{out}");
        assert!(out.contains("This folder is NEWER than what is installed"), "{out}");
        assert!(out.contains("[ok]  FeverLink3         installed (version 0.0.5)"), "{out}");
    }

    #[test]
    fn an_older_folder_must_be_confirmed_before_it_replaces_newer_files() {
        let pc = Pc::new();
        drive(&pc, "1\n\n0\n");
        let newer = pc.root.path().join("newer").join("feverlink3.exe");
        fs::create_dir_all(newer.parent().unwrap()).unwrap();
        fs::write(&newer, b"MZ feverlink3-launcher-version=9.9.9\0").unwrap();
        fs::write(pc.game().join(install::PATH_FILE), newer.to_string_lossy().as_bytes()).unwrap();
        fs::write(pc.mod_src().join("mod.json"), "{\"old\":true}").unwrap();

        let refused = drive(&pc, "1\n\n\n0\n");
        assert!(refused.contains("Version 9.9.9 of FeverLink3 is installed"), "{refused}");
        assert!(refused.contains("Nothing was changed."), "{refused}");
        assert_eq!(fs::read_to_string(pc.installed_mod().join("mod.json")).unwrap(), "{}");

        let compare = drive(&pc, "3\n\n0\n");
        assert!(compare.contains("This folder is OLDER than what is installed"), "{compare}");

        let accepted = drive(&pc, "1\ny\n\n0\n");
        assert!(accepted.contains("FeverLink3 is installed."), "{accepted}");
        assert_eq!(fs::read_to_string(pc.installed_mod().join("mod.json")).unwrap(), "{\"old\":true}");
    }

    #[test]
    fn the_version_check_without_a_complete_folder_says_so() {
        let mut pc = Pc::new();
        pc.payload.1 = None;
        let out = drive(&pc, "3\n\n0\n");
        assert!(out.contains("winhttp.dll is missing next to feverlink3.exe"), "{out}");
        assert!(out.contains("Checking for newer releases") || out.contains("Looking for newer releases"), "{out}");
    }

    #[test]
    fn starting_the_game_goes_through_steam() {
        let pc = Pc::new();
        let out = drive(&pc, "4\n\n0\n");
        assert_eq!(pc.calls(), vec![Call::Steam(pc.steam.clone().unwrap())]);
        assert!(out.contains("Asked Steam to start Transport Fever 3"), "{out}");
        assert!(out.contains("not fully installed, so the game starts without multiplayer"), "{out}");

        drive(&pc, "1\n\n0\n");
        let installed = drive(&pc, "4\n\n0\n");
        assert_eq!(pc.calls().len(), 2);
        assert!(!installed.contains("not fully installed"), "{installed}");
    }

    #[test]
    fn starting_the_game_is_refused_when_it_cannot_work() {
        let pc = Pc::new();
        pc.running.set(true);
        let out = drive(&pc, "4\n\n0\n");
        assert!(out.contains("already running"), "{out}");
        assert!(pc.calls().is_empty());

        let mut no_steam = Pc::new();
        no_steam.steam = None;
        assert!(drive(&no_steam, "4\n\n0\n").contains("Steam was not found"));
        assert!(no_steam.calls().is_empty());

        let no_exe = Pc::new();
        fs::remove_file(no_exe.steam.clone().unwrap().join("steam.exe")).unwrap();
        assert!(drive(&no_exe, "4\n\n0\n").contains("Steam's program file was not found"));
        assert!(no_exe.calls().is_empty());
    }

    #[test]
    fn the_log_folder_opens_when_it_exists() {
        let pc = Pc::new();
        let missing = drive(&pc, "5\n\n0\n");
        assert!(missing.contains("There are no logs yet"), "{missing}");
        assert!(pc.calls().is_empty());

        fs::create_dir_all(pc.local()).unwrap();
        let out = drive(&pc, "5\n\n0\n");
        assert_eq!(pc.calls(), vec![Call::Open(pc.local())]);
        assert!(out.contains("Opened "), "{out}");

        let mut no_appdata = Pc::new();
        no_appdata.local = None;
        assert!(drive(&no_appdata, "5\n\n0\n").contains("LOCALAPPDATA"));
    }

    #[test]
    fn the_diagnostics_are_copied_with_the_user_name_replaced() {
        let pc = Pc::new();
        fs::create_dir_all(pc.local().join("live")).unwrap();
        let native: Vec<String> = (1..=60).map(|n| format!("native line {n} for Zara in C:\\Users\\Zara\\x")).collect();
        fs::write(pc.local().join("native.log"), native.join("\n") + "\n").unwrap();
        fs::write(pc.local().join("live").join("mod.log"), "0.00 [mod] started\n0.10 [mod] hello zara\n").unwrap();

        let out = drive(&pc, "6\n\n0\n");
        let text = clipboard_text(&pc);
        assert!(out.contains(&format!("Copied {} lines to the clipboard", text.lines().count())), "{out}");
        assert!(out.contains("Your Windows user name is shown as <user>"), "{out}");
        assert!(!text.to_lowercase().contains("zara"), "{text}");
        assert!(text.contains("native line 60 for <user> in C:\\Users\\<user>\\x"), "the last lines");
        assert!(!text.contains("native line 20 "), "only the last 40: {text}");
        assert!(text.contains("native line 21 "), "{text}");
        assert!(text.contains("0.10 [mod] hello <user>"));
        assert!(text.contains("== feverlink3 doctor =="));
        assert!(text.contains("2026-10-10 11:54:56 UTC"));
    }

    #[test]
    fn diagnostics_without_logs_still_work() {
        let pc = Pc::new();
        drive(&pc, "6\n\n0\n");
        let text = clipboard_text(&pc);
        assert_eq!(text.matches("(not there yet").count(), 2, "{text}");
        let mut no_appdata = Pc::new();
        no_appdata.local = None;
        drive(&no_appdata, "6\n\n0\n");
        assert_eq!(clipboard_text(&no_appdata).matches("(not there yet").count(), 2);
    }

    #[test]
    fn when_the_clipboard_fails_the_text_is_shown_instead() {
        let pc = Pc::new();
        pc.clipboard_fails.set(true);
        let out = drive(&pc, "6\n\n0\n");
        assert!(out.contains("could not be copied to the clipboard (the clipboard is busy)"), "{out}");
        assert!(out.contains("== feverlink3 doctor =="), "the text is on the screen: {out}");
        assert!(out.matches("----------------------------------------").count() == 2, "{out}");
        assert!(pc.calls().is_empty());
    }
}
