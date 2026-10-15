//! A real foreign exception through the hooks: the SEH exception a C++ `throw` raises (code `0xE06D7363`, an MSVC throw
//! record for an `int`), raised by the fake engine, not the Rust panic the other tests use as its stand-in. A Rust panic
//! is caught by `catch_unwind`; a C++ exception is not: Rust's catch pad ends the process ("Rust cannot catch foreign
//! exceptions"). So the question "does a C++ exception of the game reach the game's own handler?" can only be asked in a
//! process of its own: the test runs this test binary again as a child, with one test selected.
//!
//! In the child, the exception is raised on a thread of its own made by `CreateThread` (the test harness and
//! `std::thread` wrap their work in `catch_unwind`, which would answer the question for us), and the process's
//! unhandled-exception filter stands in for the game's handler: reaching it ends the child with [`REACHED_TOP`].

use super::*;
use core::ffi::c_void;
use std::sync::atomic::AtomicBool;

/// The child's exit code when the exception reached the top of its thread, past every frame of the module.
const REACHED_TOP: u32 = 0x77;
const CHILD_VAR: &str = "FEVERLINK3_TEST_FOREIGN_CHILD";
const CHILD_TEST: &str = "runtime::tests::foreign::child";

/// The fake engine's move constructor raises the exception while set.
static ARMED: AtomicBool = AtomicBool::new(false);

#[link(name = "kernel32")]
unsafe extern "system-unwind" {
    fn RaiseException(code: u32, flags: u32, count: u32, args: *const usize);
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateThread(
        attributes: *const c_void,
        stack: usize,
        start: unsafe extern "system-unwind" fn(*mut c_void) -> u32,
        param: *mut c_void,
        flags: u32,
        id: *mut u32,
    ) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
    fn SetUnhandledExceptionFilter(filter: unsafe extern "system" fn(*const c_void) -> i32) -> usize;
    fn SetErrorMode(mode: u32) -> u32;
    fn ExitProcess(code: u32) -> !;
}

/// The records `_CxxThrowException` passes for `throw 42;` on x64: every pointer inside is an offset from the "image
/// base", which is this object itself.
#[repr(C)]
struct ThrowImage {
    // TypeDescriptor of `int`
    vftable: usize,
    spare: usize,
    name: [u8; 8],
    // CatchableType: simple type, the TypeDescriptor, no displacement, 4 bytes, no copy function
    ct_properties: u32,
    ct_type: i32,
    ct_mdisp: i32,
    ct_pdisp: i32,
    ct_vdisp: i32,
    ct_size: i32,
    ct_copy: i32,
    // CatchableTypeArray of one
    cta_count: i32,
    cta_first: i32,
    // ThrowInfo
    ti_attributes: u32,
    ti_unwind: i32,
    ti_forward: i32,
    ti_types: i32,
    // the thrown object
    object: i32,
}

/// Raises what `throw 42;` raises. Does not return: the exception is non-continuable.
pub(super) fn raise() {
    static VFTABLE: usize = 0;
    let image = Box::leak(Box::new(ThrowImage {
        vftable: &VFTABLE as *const usize as usize,
        spare: 0,
        name: *b".H\0\0\0\0\0\0",
        ct_properties: 1,
        ct_type: 0,
        ct_mdisp: 0,
        ct_pdisp: -1,
        ct_vdisp: 0,
        ct_size: 4,
        ct_copy: 0,
        cta_count: 1,
        cta_first: core::mem::offset_of!(ThrowImage, ct_properties) as i32,
        ti_attributes: 0,
        ti_unwind: 0,
        ti_forward: 0,
        ti_types: core::mem::offset_of!(ThrowImage, cta_count) as i32,
        object: 42,
    }));
    let base = image as *mut ThrowImage as usize;
    let args = [
        0x1993_0520usize,
        base + core::mem::offset_of!(ThrowImage, object),
        base + core::mem::offset_of!(ThrowImage, ti_attributes),
        base,
    ];
    // SAFETY: the four arguments of an MSVC C++ throw, pointing into the leaked object above.
    unsafe { RaiseException(0xE06D_7363, 1, 4, args.as_ptr()) };
}

/// Called by the fake engine's move constructor.
pub(super) fn raise_if_armed() {
    if ARMED.load(Relaxed) {
        raise();
    }
}

unsafe extern "system" fn reached_top(_info: *const c_void) -> i32 {
    // SAFETY: ends this child process; nothing is left to clean up.
    unsafe { ExitProcess(REACHED_TOP) }
}

/// What the child's own thread does: `guarded` raises the exception inside `log::guarded` (what the module must never
/// do), `capture` holds a tool's command whose capture (the engine's move constructor) throws.
unsafe extern "system-unwind" fn child_thread(param: *mut c_void) -> u32 {
    // SAFETY: the address of the mode string, alive while the parent thread waits for this one.
    let mode = unsafe { &*(param as *const String) };
    match mode.as_str() {
        "guarded" => {
            let _ = crate::log::guarded("foreign exception test", raise);
        }
        "capture" => {
            let dir = TempDir::new("rt-foreign");
            let rt = runtime(Some(dir.path()));
            ctl(dir.path(), HOLD_ON);
            rt.pump();
            ARMED.store(true, Relaxed);
            let mut c = Call::new(52, 1);
            // SAFETY: the arguments of a fake call owned by this frame.
            unsafe { rt.on_add(c.args(), at(SITE_A)) };
        }
        other => panic!("unknown mode {other}"),
    }
    0 // the exception was swallowed somewhere: the parent sees exit code 0
}

/// Not a test of its own: the body of the child process (a no-op in a normal test run).
#[test]
fn child() {
    let Ok(mode) = std::env::var(CHILD_VAR) else { return };
    // SAFETY: plain Win32 calls with valid arguments; the thread gets a pointer to `mode`, which outlives it (waited for).
    unsafe {
        SetErrorMode(0x0001 | 0x0002 | 0x8000); // no dialog for the crash the control case is
        SetUnhandledExceptionFilter(reached_top);
        let thread = CreateThread(
            core::ptr::null(),
            0,
            child_thread,
            &mode as *const String as *mut c_void,
            0,
            core::ptr::null_mut(),
        );
        assert!(!thread.is_null());
        WaitForSingleObject(thread, u32::MAX);
    }
}

/// Runs the child in `mode`; its exit code and its error output.
fn run_child(mode: &str) -> (u32, String) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([CHILD_TEST, "--exact", "--test-threads=1", "--nocapture"])
        .env(CHILD_VAR, mode)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if start.elapsed().as_secs() > 60 {
            let _ = child.kill();
            panic!("the {mode} child hung");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let out = child.wait_with_output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (status.code().unwrap_or(-1) as u32, text)
}

#[test]
fn a_cpp_exception_passes_the_add_hook_to_the_games_handler_where_catch_unwind_would_end_the_process() {
    // the control: inside log::guarded the exception never reaches the top, the process ends in Rust's catch pad
    let (code, text) = run_child("guarded");
    assert_ne!(code, REACHED_TOP, "catch_unwind let a C++ exception pass?\n{text}");
    assert_ne!(code, 0, "the C++ exception was swallowed\n{text}");
    // the add hook holding a tool's command: the engine's move constructor throws during the capture
    let (code, text) = run_child("capture");
    assert_eq!(code, REACHED_TOP, "exit code {code:#x}: the exception did not reach the game's handler\n{text}");
}
