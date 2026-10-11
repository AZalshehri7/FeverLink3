//! Append-only text log in the session folder. Silent when the module runs as a plain proxy.
//!
//! It is also where a panic leaves its message ([`install_panic_hook`]): with `panic = "abort"` the process ends
//! right after the hook, and this file is all that is left to say why.

use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

static PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
/// The same path for the panic hook, which must not take a lock: a panic can happen on a thread that holds
/// [`PATH`] (or while another thread holds it for a slow write), and a hook that gave up on the lock would lose the
/// one message that explains the crash. Leaked on purpose (a path, once per [`init`]), never freed.
static PANIC_PATH: AtomicPtr<PathBuf> = AtomicPtr::new(core::ptr::null_mut());

/// The log stops growing at this size, so a hook that logs for every command cannot fill a disk in a long session.
const MAX_BYTES: u64 = 2 << 20;

pub fn init(path: PathBuf) {
    // a log that grew large in earlier runs is set aside, so this run starts small and its first lines are kept
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES / 2) {
        let _ = fs::rename(&path, path.with_extension("log.1"));
    }
    let cell = PATH.get_or_init(|| Mutex::new(None));
    // a poisoned lock holds a path as good as any other
    PANIC_PATH.store(Box::into_raw(Box::new(path.clone())), Ordering::Release);
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

/// Most backtrace frames written for one panic.
const BACKTRACE_FRAMES: usize = 48;

thread_local! {
    /// True while this thread is inside the panic hook: a panic raised while the hook runs (formatting, allocation)
    /// must not start the hook again.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// Panics seen so far in this process (the number in the message: the lowest is the original cause).
static PANICS: AtomicU32 = AtomicU32::new(0);

/// Writes one line to the log without taking the lock. One `writeln!` to a file opened for append is atomic enough
/// for a diagnostic line, even next to the other writers (and the other game, which shares the file).
fn append_unlocked(text: &str) {
    let p = PANIC_PATH.load(Ordering::Acquire);
    // SAFETY: only ever set from `Box::into_raw` in `init` and never freed.
    if let Some(path) = unsafe { p.as_ref() } {
        append(path, text);
    }
}

/// `SizeOfImage` of the PE image at `base` (this DLL, so its headers are mapped); 0 when the headers look wrong.
fn image_size(base: usize) -> usize {
    // SAFETY: `base` is the base of a module of this process, whose first page is mapped and readable; the NT header
    // offset is bounded before it is followed.
    unsafe {
        let dos = base as *const u8;
        if *dos != b'M' || *dos.add(1) != b'Z' {
            return 0;
        }
        let nt = core::ptr::read_unaligned(dos.add(0x3c) as *const u32) as usize;
        if nt > 0x1000 {
            return 0;
        }
        // optional header starts 0x18 after the NT signature; SizeOfImage is 0x38 into it
        core::ptr::read_unaligned(dos.add(nt + 0x18 + 0x38) as *const u32) as usize
    }
}

/// The return addresses of the calling thread, innermost first, as one log line. Frames inside this DLL are written
/// as `dll+0xRVA` (look them up in the build's PDB), others as plain addresses.
fn backtrace_line() -> String {
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    use windows_sys::Win32::System::LibraryLoader::{
        GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    let mut frames = [core::ptr::null_mut::<core::ffi::c_void>(); BACKTRACE_FRAMES];
    // SAFETY: the array holds BACKTRACE_FRAMES slots; the hash pointer may be null.
    let got =
        unsafe { RtlCaptureStackBackTrace(0, BACKTRACE_FRAMES as u32, frames.as_mut_ptr(), core::ptr::null_mut()) };
    let mut module = core::ptr::null_mut();
    // SAFETY: FROM_ADDRESS with an address inside this module, which takes no reference.
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            backtrace_line as *const () as *const u16,
            &mut module,
        )
    };
    let base = if ok != 0 { module as usize } else { 0 };
    let size = if base != 0 { image_size(base) } else { 0 };
    let mut out = format!("  backtrace (dll base {base:#x}):");
    for f in frames.iter().take(got as usize) {
        let a = *f as usize;
        if base != 0 && a >= base && a - base < size {
            out.push_str(&format!(" dll+{:#x}", a - base));
        } else {
            out.push_str(&format!(" {a:#x}"));
        }
    }
    out
}

/// The text a panic carries: what `panic!` and `assert!` produce, else a placeholder.
fn panic_text(info: &std::panic::PanicHookInfo<'_>, n: u32) -> String {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_owned());
    let place = info.location().map_or_else(String::new, |l| format!(" at {}:{}", l.file(), l.line()));
    let thread = std::thread::current();
    format!("PANIC #{n} in thread '{}'{place}: {message}", thread.name().unwrap_or("unnamed"))
}

/// Makes a panic write its message, place and a backtrace to the log before anything else happens to the process.
/// Nothing else in the process installs a hook (the game is native code), so this replaces the default one.
///
/// It takes no lock (the path is read from an atomic), so it cannot lose its message because some thread holds the
/// log; a panic inside the hook itself writes a short note and returns instead of recursing.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        // `try_with` fails while the thread's locals are being torn down: treat that as nested
        let nested = IN_HOOK.try_with(|c| c.replace(true)).unwrap_or(true);
        if nested {
            append_unlocked("PANIC raised while a panic was being reported (nested); the first one is above");
            return;
        }
        let n = PANICS.fetch_add(1, Ordering::Relaxed) + 1;
        append_unlocked(&panic_text(info, n));
        append_unlocked(&backtrace_line());
        let _ = IN_HOOK.try_with(|c| c.set(false));
    }));
}

/// Runs `f`, a piece of this module's own bookkeeping inside a hook or an exported function, so that a panic in it
/// cannot take the game down: `None` (and a log line, the first few times) instead of the result. The panic hook has
/// already written the message, place and backtrace. Under `panic = "abort"` there is nothing to catch and the
/// process ends after the hook; under `unwind` this is what makes the hooks fail open.
///
/// Never wrap a call into the game with it: a C++ exception of the game passing through `catch_unwind` is not a
/// Rust panic and ends the process. Call the game outside, after the guard returns.
pub fn guarded<R>(site: &'static str, f: impl FnOnce() -> R) -> Option<R> {
    static CONTAINED: AtomicU32 = AtomicU32::new(0);
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => Some(r),
        Err(_) => {
            let n = CONTAINED.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 20 {
                append_unlocked(&format!("{site}: a panic was contained (#{n}); the game goes on without this step"));
            }
            None
        }
    }
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
        assert!(text.contains("in thread 'feverlink3-test' at native"), "{text}");
        assert!(text.contains("PANIC #") && text.contains("log.rs:") && text.contains(": boom 3\n"), "{text}");
        assert!(text.contains("(no message)"), "{text}");
        assert!(text.contains("  backtrace (dll base "), "every panic is followed by its backtrace: {text}");
    }

    #[test]
    fn a_panic_while_the_log_is_locked_still_writes_its_message() {
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let d = dir();
        let path = d.path().join("native.log");
        init(path.clone());
        install_panic_hook();
        // another thread is in the middle of a log write (or died holding the lock): the hook must neither wait
        // for it nor drop the message
        let held = PATH.get().unwrap().lock().unwrap_or_else(|p| p.into_inner());
        let result = std::thread::spawn(|| panic!("while locked")).join();
        drop(held);
        let _ = std::panic::take_hook();
        assert!(result.is_err());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(": while locked\n"), "{text}");
    }

    #[test]
    fn a_panic_inside_the_hook_does_not_start_it_again() {
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let d = dir();
        let path = d.path().join("native.log");
        init(path.clone());
        install_panic_hook();
        // a panic payload whose formatting panics would re-enter the hook: simulate by flagging this thread as
        // already inside it
        IN_HOOK.with(|c| c.set(true));
        let _ = std::panic::catch_unwind(|| panic!("nested"));
        IN_HOOK.with(|c| c.set(false));
        let _ = std::panic::take_hook();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("nested); the first one is above"), "{text}");
        assert!(!text.contains(": nested\n"), "the nested panic is not formatted: {text}");
    }

    #[test]
    fn guarded_contains_a_panic_and_returns_the_value_otherwise() {
        let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
        let d = dir();
        let path = d.path().join("native.log");
        init(path.clone());
        assert_eq!(guarded("test", || 5), Some(5));
        let none: Option<u8> = guarded("test site", || panic!("inside"));
        assert_eq!(none, None);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("test site: a panic was contained"), "{text}");
    }
}
