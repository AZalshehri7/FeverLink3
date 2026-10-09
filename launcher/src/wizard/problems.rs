//! Everything that can stop an action, with its plain-language message; and the words that end an install or
//! an uninstall.

use std::path::{Path, PathBuf};

use feverlink3_launcher::install::{Item, Outcome, Report, Step};

pub(super) const STEAM_URL: &str = "https://store.steampowered.com/about/";

/// Everything that can stop an action, with what to do about it. Both the checks made before an action and the
/// failures an install report carries end up here, so every message exists once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Problem {
    SteamMissing,
    SteamExeMissing(PathBuf),
    GameMissing,
    GameRunning,
    GameAlreadyRunning,
    TemporaryFolder,
    PackageIncomplete { mod_missing: bool, dll_missing: bool },
    PackageDamaged,
    ForeignDll(PathBuf),
    NoUserFolder,
    FilesInUse(PathBuf),
    PermissionDenied(PathBuf),
    DiskFull,
    NoLocalAppData,
    NoLogsYet(PathBuf),
    Other(String),
}

impl Problem {
    pub(super) fn message(&self) -> String {
        match self {
            Problem::SteamMissing => format!(
                "Steam was not found on this PC. Install Steam ({STEAM_URL}), sign in, install Transport Fever 3 and \
                 start it once. Then run FeverLink3 again."
            ),
            Problem::SteamExeMissing(path) => format!(
                "Steam's program file was not found at {}. Start Steam once so that it repairs itself, or install it \
                 again, then try this again.",
                path.display()
            ),
            Problem::GameMissing => "Transport Fever 3 was not found in your Steam libraries. Install it from Steam \
                 (on a drive Steam uses), start it once, close it, then try this again."
                .to_owned(),
            Problem::GameRunning => "Transport Fever 3 is running, so its files are locked. Close the game completely \
                 first (use its Exit button; if the window is gone but the sound goes on, end TransportFever3.exe in \
                 the Task Manager), then try this again."
                .to_owned(),
            Problem::GameAlreadyRunning => {
                "Transport Fever 3 is already running. Switch to its window (Alt+Tab) instead.".to_owned()
            }
            Problem::TemporaryFolder => "This copy of FeverLink3 is running from inside the downloaded zip file (or \
                 another temporary folder), which Windows deletes later. Close this window, right-click the zip file, \
                 choose Extract All, and start feverlink3.exe from the new folder."
                .to_owned(),
            Problem::PackageIncomplete { mod_missing, dll_missing } => {
                let missing = match (mod_missing, dll_missing) {
                    (true, true) => "the mod folder (mod\\feverlink3_1) and winhttp.dll are",
                    (true, false) => "the mod folder (mod\\feverlink3_1) is",
                    _ => "winhttp.dll is",
                };
                format!(
                    "This FeverLink3 folder is incomplete: {missing} missing next to feverlink3.exe. Unzip the whole \
                     zip file (right-click it, Extract All) and start feverlink3.exe from the new folder."
                )
            }
            Problem::PackageDamaged => {
                "The winhttp.dll in this FeverLink3 folder is not FeverLink3's native module, or \
                 it is damaged. Download the zip file again and unzip it into a fresh folder."
                    .to_owned()
            }
            Problem::ForeignDll(path) => format!(
                "The game folder already contains a winhttp.dll that does not belong to FeverLink3 ({}), probably from \
                 another mod. FeverLink3 never overwrites it. If you do not need that other mod, remove or rename the \
                 file and choose 1 again.",
                path.display()
            ),
            Problem::NoUserFolder => {
                "Transport Fever 3 has not created its Steam user folder yet, so the mod could not \
                 be copied there. Start the game once from Steam, close it again, and choose 1 again."
                    .to_owned()
            }
            Problem::FilesInUse(path) => format!(
                "A file is in use by another program: {}. This is usually Transport Fever 3 (make sure it is fully \
                 closed) or an antivirus scan. Wait a minute and try again.",
                path.display()
            ),
            Problem::PermissionDenied(path) => format!(
                "Windows did not allow FeverLink3 to change {}. If Transport Fever 3 may still be running, even in the \
                 background, close it first. If the game is installed under Program Files, close this window, \
                 right-click feverlink3.exe and choose Run as administrator. Antivirus protection such as controlled \
                 folder access (ransomware protection) can also block it: allow feverlink3.exe there.",
                path.display()
            ),
            Problem::DiskFull => "The disk is full. Free some space and try again.".to_owned(),
            Problem::NoLocalAppData => "Windows did not say where your local application data folder is (the \
                 LOCALAPPDATA setting), so the log folder cannot be found."
                .to_owned(),
            Problem::NoLogsYet(path) => format!(
                "There are no logs yet: {} does not exist. FeverLink3 creates it the first time Transport Fever 3 \
                 starts with FeverLink3 installed. Install it (1), start the game (4), then try again.",
                path.display()
            ),
            Problem::Other(text) => format!("Something unexpected went wrong: {text}"),
        }
    }
}

/// The number in `(os error N)`, which Windows error texts carry whatever language the system speaks.
fn os_error_code(text: &str) -> Option<u32> {
    let (_, rest) = text.split_once("(os error ")?;
    rest.split_once(')')?.0.trim().parse().ok()
}

/// Sorts the failure text of an install step into a [`Problem`]. The report only keeps the error as text, so the
/// operating system's code is read from it first (the words are translated, the code is not), then the words
/// `install.rs` itself uses.
fn classify_reason(item: Item, reason: &str, path: &Path) -> Problem {
    match os_error_code(reason) {
        // sharing violation, lock violation, "the file has a user-mapped section open"
        Some(32 | 33 | 1224) => return Problem::FilesInUse(path.to_path_buf()),
        Some(5) => return Problem::PermissionDenied(path.to_path_buf()),
        Some(112) => return Problem::DiskFull,
        _ => {}
    }
    if reason.contains("is the game running") {
        Problem::FilesInUse(path.to_path_buf())
    } else if reason.contains("not ours") || reason.contains("not overwritten") {
        Problem::ForeignDll(path.to_path_buf())
    } else if reason.contains("lacks the marker") || reason.contains("could not read the native module") {
        Problem::PackageDamaged
    } else if reason.contains("source folder is missing") || reason.contains("shipped with this launcher") {
        Problem::PackageIncomplete { mod_missing: item == Item::Mod, dll_missing: item == Item::NativeDll }
    } else if reason.contains("no Steam user folder") {
        Problem::NoUserFolder
    } else {
        Problem::Other(format!("{item}: {reason}"))
    }
}

/// The problem a report step stands for. A failed step always is one; a skipped step is one when it skipped
/// something that was wanted (nothing to install from, or no folder to install the mod into).
fn step_problem(step: &Step) -> Option<Problem> {
    match step.outcome {
        Outcome::Failed => Some(classify_reason(step.item, &step.reason, &step.path)),
        Outcome::Skipped
            if step.reason.contains("no Steam user folder") || step.reason.contains("shipped with this launcher") =>
        {
            Some(classify_reason(step.item, &step.reason, &step.path))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// After an install or uninstall
// ---------------------------------------------------------------------------------------------------------------

/// The plain-language ending of an install or uninstall: what happened, and what to do about what did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Summary {
    pub(super) ok: bool,
    pub(super) headline: String,
    pub(super) notes: Vec<String>,
}

/// The distinct values of `items`, in order.
fn distinct(items: impl IntoIterator<Item = Problem>) -> Vec<Problem> {
    let mut found: Vec<Problem> = Vec::new();
    for item in items {
        if !found.contains(&item) {
            found.push(item);
        }
    }
    found
}

/// The problems of the steps that failed.
pub(super) fn failure_problems(report: &Report) -> Vec<Problem> {
    distinct(
        report
            .steps
            .iter()
            .filter(|s| s.outcome == Outcome::Failed)
            .map(|s| classify_reason(s.item, &s.reason, &s.path)),
    )
}

/// The problems of an install: the failures, and the steps skipped because there was nothing to install from or
/// no Steam user folder to install into, which leave the job half done without being errors.
fn install_problems(report: &Report) -> Vec<Problem> {
    distinct(report.steps.iter().filter_map(step_problem))
}

pub(super) fn summarize_install(report: &Report) -> Summary {
    let problems = install_problems(report);
    if !problems.is_empty() {
        let failed = report.steps.iter().any(|s| s.outcome == Outcome::Failed);
        let headline = if failed {
            "FeverLink3 could not be installed completely."
        } else {
            "FeverLink3 is only partly installed."
        };
        return Summary {
            ok: false,
            headline: headline.to_owned(),
            notes: problems.iter().map(Problem::message).collect(),
        };
    }
    if report.steps.iter().all(|s| s.outcome == Outcome::Skipped) {
        return Summary {
            ok: true,
            headline: "FeverLink3 was already installed and up to date. Nothing was changed.".to_owned(),
            notes: Vec::new(),
        };
    }
    Summary {
        ok: true,
        headline: "FeverLink3 is installed.".to_owned(),
        notes: vec![
            "Choose 4 to start Transport Fever 3 through Steam.".to_owned(),
            "A new game needs the mod switched on once: open the Add Mods tab and activate FeverLink3 Multiplayer. \
             Then click FeverLink3 in the bar at the bottom of the screen."
                .to_owned(),
        ],
    }
}

pub(super) fn summarize_uninstall(report: &Report) -> Summary {
    let problems = failure_problems(report);
    if !problems.is_empty() {
        return Summary {
            ok: false,
            headline: "FeverLink3 could not be removed completely.".to_owned(),
            notes: problems.iter().map(Problem::message).collect(),
        };
    }
    if report.steps.iter().all(|s| s.outcome == Outcome::Skipped) {
        return Summary {
            ok: true,
            headline: "FeverLink3 was not installed. Nothing was removed.".to_owned(),
            notes: Vec::new(),
        };
    }
    let mut notes = vec!["You can delete this FeverLink3 folder when you no longer need it.".to_owned()];
    if report.steps.iter().any(|s| s.item == Item::NativeDll && s.reason.contains("not ours")) {
        notes.push(
            "A winhttp.dll that does not belong to FeverLink3 was found in the game folder and left alone.".to_owned(),
        );
    }
    Summary {
        ok: true,
        headline: "FeverLink3 was removed. Transport Fever 3 starts exactly as before.".to_owned(),
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wizard::testkit::*;

    #[test]
    fn every_problem_says_what_to_do_in_plain_words() {
        let path = PathBuf::from(r"C:\Game\winhttp.dll");
        let cases: Vec<(Problem, &[&str])> = vec![
            (Problem::SteamMissing, &["Steam was not found", STEAM_URL]),
            (Problem::SteamExeMissing(path.clone()), &["Steam's program file", "winhttp.dll"]),
            (Problem::GameMissing, &["Transport Fever 3 was not found", "Steam libraries"]),
            (Problem::GameRunning, &["is running", "Close the game completely", "Task Manager"]),
            (Problem::GameAlreadyRunning, &["already running", "Alt+Tab"]),
            (Problem::TemporaryFolder, &["inside the downloaded zip", "Extract All"]),
            (
                Problem::PackageIncomplete { mod_missing: true, dll_missing: true },
                &["mod folder", "winhttp.dll are missing", "Extract All"],
            ),
            (
                Problem::PackageIncomplete { mod_missing: true, dll_missing: false },
                &["mod folder (mod\\feverlink3_1) is missing"],
            ),
            (Problem::PackageIncomplete { mod_missing: false, dll_missing: true }, &["winhttp.dll is missing"]),
            (Problem::PackageDamaged, &["not FeverLink3's native module", "Download the zip file again"]),
            (Problem::ForeignDll(path.clone()), &["does not belong to FeverLink3", "never overwrites", "winhttp.dll"]),
            (Problem::NoUserFolder, &["has not created its Steam user folder", "Start the game once"]),
            (
                Problem::FilesInUse(path.clone()),
                &["in use by another program", "fully closed", "antivirus", "winhttp.dll"],
            ),
            (
                Problem::PermissionDenied(path.clone()),
                &["did not allow", "Run as administrator", "controlled folder access"],
            ),
            (Problem::DiskFull, &["disk is full"]),
            (Problem::NoLocalAppData, &["LOCALAPPDATA"]),
            (Problem::NoLogsYet(path.clone()), &["no logs yet", "start the game (4)"]),
            (Problem::Other("boom".into()), &["unexpected", "boom"]),
        ];
        for (problem, words) in cases {
            let message = problem.message();
            for word in words {
                assert!(message.contains(word), "{problem:?}: {message:?} lacks {word:?}");
            }
            assert!(message.is_ascii() || matches!(problem, Problem::Other(_)), "{message}");
        }
    }

    #[test]
    fn install_failures_are_sorted_by_the_operating_system_code_first() {
        let path = Path::new(r"C:\Game\winhttp.dll");
        let classify = |reason: &str| classify_reason(Item::NativeDll, reason, path);
        let in_use = Problem::FilesInUse(path.to_path_buf());
        // the words of the message change with the language of Windows; the code does not
        assert_eq!(
            classify("updated: The process cannot access the file because it is being used by another process. (os error 32)"),
            in_use
        );
        assert_eq!(classify("updated: Der Prozess kann nicht auf die Datei zugreifen (os error 32)"), in_use);
        assert_eq!(classify("updated: العملية لا يمكنها الوصول إلى الملف (os error 33)"), in_use);
        assert_eq!(
            classify("installed: The requested operation cannot be performed on a file with a user-mapped section open. (os error 1224)"),
            in_use
        );
        assert_eq!(
            classify("updated: Access is denied. (os error 5) (is the game running? close it and install again)"),
            Problem::PermissionDenied(path.to_path_buf())
        );
        assert_eq!(classify("installed: There is not enough space on the disk. (os error 112)"), Problem::DiskFull);
        // install.rs's own words, when there is no code
        assert_eq!(classify("removed: something (is the game running?)"), in_use);
        assert_eq!(
            classify("a winhttp.dll that is not ours is already in the game folder (another mod?); not overwritten"),
            Problem::ForeignDll(path.to_path_buf())
        );
        assert_eq!(
            classify("the shipped module lacks the marker \"feverlink3_native\"; refusing to install"),
            Problem::PackageDamaged
        );
        assert_eq!(classify("could not read the native module: x (os error 2)"), Problem::PackageDamaged);
        assert_eq!(
            classify_reason(Item::Mod, "mod source folder is missing", path),
            Problem::PackageIncomplete { mod_missing: true, dll_missing: false }
        );
        assert_eq!(
            classify("no native module shipped with this launcher"),
            Problem::PackageIncomplete { mod_missing: false, dll_missing: true }
        );
        assert_eq!(
            classify_reason(
                Item::Mod,
                "no Steam user folder userdata/<id>/3493540/local exists; start the game once",
                path
            ),
            Problem::NoUserFolder
        );
        assert_eq!(
            classify_reason(Item::PathFile, "written: strange", path),
            Problem::Other("feverlink3_path.txt: written: strange".into())
        );
        assert_eq!(os_error_code("x (os error 32) y"), Some(32));
        assert_eq!(os_error_code("no code here"), None);
        assert_eq!(os_error_code("(os error x)"), None);
        assert_eq!(os_error_code("(os error 5"), None);
    }

    #[test]
    fn a_clean_install_ends_with_what_to_do_next() {
        let r = report(vec![
            step(Item::Mod, Outcome::Done, "installed"),
            step(Item::AppIdFile, Outcome::Done, "created"),
            step(Item::NativeDll, Outcome::Done, "installed"),
            step(Item::PathFile, Outcome::Done, "written"),
        ]);
        let summary = summarize_install(&r);
        assert!(summary.ok);
        assert_eq!(summary.headline, "FeverLink3 is installed.");
        assert!(summary.notes.iter().any(|n| n.contains("Choose 4")));
        assert!(summary.notes.iter().any(|n| n.contains("Add Mods")));
    }

    #[test]
    fn an_install_that_changed_nothing_says_so() {
        let r = report(vec![
            step(Item::Mod, Outcome::Skipped, "already up to date"),
            step(Item::NativeDll, Outcome::Skipped, "already installed, identical"),
        ]);
        let summary = summarize_install(&r);
        assert!(summary.ok);
        assert!(summary.headline.contains("already installed and up to date"));
        assert!(summary.notes.is_empty());
    }

    #[test]
    fn an_install_without_a_steam_user_folder_is_only_partly_done() {
        let r = report(vec![
            step(
                Item::Mod,
                Outcome::Skipped,
                "no Steam user folder userdata/<id>/3493540/local exists; start the game once",
            ),
            step(Item::NativeDll, Outcome::Done, "installed"),
        ]);
        let summary = summarize_install(&r);
        assert!(!summary.ok);
        assert_eq!(summary.headline, "FeverLink3 is only partly installed.");
        assert_eq!(summary.notes, vec![Problem::NoUserFolder.message()]);
    }

    #[test]
    fn failed_steps_become_one_message_per_distinct_problem() {
        let r = report(vec![
            step_at(
                Item::Mod,
                r"C:\Steam\mods\feverlink3_1",
                Outcome::Failed,
                "installed: Access is denied. (os error 5)",
            ),
            step_at(
                Item::NativeDll,
                r"C:\Game\winhttp.dll",
                Outcome::Failed,
                "updated: Access is denied. (os error 5)",
            ),
            step(Item::PathFile, Outcome::Done, "written"),
        ]);
        let summary = summarize_install(&r);
        assert!(!summary.ok);
        assert_eq!(summary.headline, "FeverLink3 could not be installed completely.");
        assert_eq!(summary.notes.len(), 2, "two files, two paths, two messages");
        let same_path = report(vec![
            step(Item::NativeDll, Outcome::Failed, "x (os error 32)"),
            step(Item::NativeDll, Outcome::Failed, "y (os error 32)"),
        ]);
        assert_eq!(summarize_install(&same_path).notes.len(), 1, "the same problem is said once");
        let uninstall = summarize_uninstall(&r);
        assert!(!uninstall.ok);
        assert_eq!(uninstall.headline, "FeverLink3 could not be removed completely.");
    }

    #[test]
    fn an_uninstall_says_what_was_removed_or_that_there_was_nothing() {
        let removed =
            report(vec![step(Item::Mod, Outcome::Done, "removed"), step(Item::NativeDll, Outcome::Done, "removed")]);
        let summary = summarize_uninstall(&removed);
        assert!(summary.ok);
        assert!(summary.headline.contains("was removed") && summary.headline.contains("exactly as before"));
        assert!(summary.notes[0].contains("delete this FeverLink3 folder"));

        let nothing = report(vec![
            step(Item::Mod, Outcome::Skipped, "not installed"),
            step(Item::NativeDll, Outcome::Skipped, "not installed"),
        ]);
        let summary = summarize_uninstall(&nothing);
        assert!(summary.ok);
        assert_eq!(summary.headline, "FeverLink3 was not installed. Nothing was removed.");

        // the mod's "no Steam user folder" skip is not an error on the way out
        let no_users = report(vec![
            step(Item::Mod, Outcome::Skipped, "no Steam user folder found"),
            step(Item::NativeDll, Outcome::Done, "removed"),
        ]);
        assert!(summarize_uninstall(&no_users).ok);

        let foreign = report(vec![
            step(Item::NativeDll, Outcome::Skipped, "not ours; left alone"),
            step(Item::PathFile, Outcome::Done, "removed"),
        ]);
        let summary = summarize_uninstall(&foreign);
        assert!(summary.notes.iter().any(|n| n.contains("does not belong to FeverLink3")));
    }
}
