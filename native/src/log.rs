//! Append-only text log in the session folder. Silent when the module runs as a plain proxy.
//!
//! It is also where a panic leaves its message ([`install_panic_hook`]): with `panic = "abort"` the process ends
//! right after the hook, and this file is all that is left to say why.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};

static PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

/// The log stops growing at this size, so a hook that logs for every command cannot fill a disk in a long session.
const MAX_BYTES: u64 = 2 << 20;

pub fn init(path: PathBuf) {
    // a log that grew large in earlier runs is set aside, so this run starts small and its first lines are kept
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES / 2) {
        let _ = fs::rename(&path, path.with_extension("log.1"));
    }
    let cell = PATH.get_or_init(|| Mutex::new(None));
    // a poisoned lock holds a path as good as any other
    *cell.lock().unwrap_or_else(|p| p.into_inner()) = Some(path);
}

pub fn line(text: &str) {
    let Some(cell) = PATH.get() else { return };
    let guard = cell.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(path) = guard.as_ref() {
        append(path, text);
    }
}

fn append(path: &Path, text: &str) {
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else { return };
    let len = file.metadata().map_or(0, |m| m.len());
    if len >= MAX_BYTES {
        return;
    }
    let _ = writeln!(file, "{text}");
    if len + text.len() as u64 + 1 >= MAX_BYTES {
        let _ = writeln!(file, "(log full: further lines are dropped until the next start)");
    }
}

/// The log's lock, without waiting: a panic can happen while the thread that panics holds it, and a hook that
/// waited for it would hang the process instead of ending it.
fn try_guard(cell: &Mutex<Option<PathBuf>>) -> Option<MutexGuard<'_, Option<PathBuf>>> {
    match cell.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// The text a panic carries: what `panic!` and `assert!` produce, else a placeholder.
fn panic_text(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_owned());
    let place = info.location().map_or_else(String::new, |l| format!(" at {}:{}", l.file(), l.line()));
    let thread = std::thread::current();
    format!("PANIC in thread '{}'{place}: {message}", thread.name().unwrap_or("unnamed"))
}

/// Makes a panic write its message and place to the log before anything else happens to the process. Nothing else
/// in the process installs a hook (the game is native code), so this replaces the default one.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let text = panic_text(info);
        if let Some(cell) = PATH.get() {
            if let Some(guard) = try_guard(cell) {
                if let Some(path) = guard.as_ref() {
                    append(path, &text);
                }
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as TestLock;

    /// The log path and the panic hook are process-wide: tests that use them take turns.
    static ONE_AT_A_TIME: TestLock<()> = TestLock::new(());

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn lines_are_appended_and_the_log_stops_at_its_size() {
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let d = dir();
        let path = d.path().join("native.log");
        init(path.clone());
        line("first");
        line("second");
        assert_eq!(fs::read_to_string(&path).unwrap(), "first\nsecond\n");
        let long = "x".repeat(64 * 1024);
        for _ in 0..(MAX_BYTES / long.len() as u64 + 20) {
            line(&long);
        }
        let size = fs::metadata(&path).unwrap().len();
        assert!((MAX_BYTES..MAX_BYTES + 128 * 1024).contains(&size), "{size}");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("(log full: further lines are dropped until the next start)\n"));
        line("after the end");
        assert_eq!(fs::metadata(&path).unwrap().len(), size, "nothing is added to a full log");
    }

    #[test]
    fn a_large_old_log_is_set_aside_at_start() {
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let d = dir();
        let path = d.path().join("native.log");
        fs::write(&path, vec![b'o'; (MAX_BYTES / 2) as usize + 1]).unwrap();
        init(path.clone());
        line("new run");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new run\n");
        assert_eq!(fs::metadata(path.with_extension("log.1")).unwrap().len(), MAX_BYTES / 2 + 1);
        // a small one stays
        fs::write(&path, "short\n").unwrap();
        init(path.clone());
        line("again");
        assert_eq!(fs::read_to_string(&path).unwrap(), "short\nagain\n");
    }

    #[test]
    fn a_panic_leaves_its_message_thread_and_place_in_the_log() {
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let d = dir();
        let path = d.path().join("native.log");
        init(path.clone());
        install_panic_hook();
        let named = std::thread::Builder::new().name("feverlink3-test".into()).spawn(|| {
            let n = 3;
            if n > 2 {
                panic!("boom {n}");
            }
        });
        assert!(named.unwrap().join().is_err());
        let _ = std::panic::catch_unwind(|| std::panic::panic_any(7u8));
        let _ = std::panic::take_hook(); // back to the default for the rest of the test run
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("PANIC in thread 'feverlink3-test' at native"), "{text}");
        assert!(text.contains("log.rs:") && text.contains(": boom 3\n"), "{text}");
        assert!(text.contains("(no message)"), "{text}");
    }

    #[test]
    fn a_panic_while_the_log_is_locked_does_not_wait_for_it() {
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let d = dir();
        init(d.path().join("native.log"));
        let cell = PATH.get().unwrap();
        let held = cell.lock().unwrap_or_else(|p| p.into_inner());
        assert!(try_guard(cell).is_none(), "a hook that waited here would hang the process");
        drop(held);
        assert!(try_guard(cell).is_some());
    }
}
