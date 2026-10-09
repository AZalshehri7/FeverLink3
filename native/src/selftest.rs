//! A check made once at start-up, in the binary that will actually run in the game, that the detour machinery and
//! the caller identification work.
//!
//! `cargo test` proves them in a debug build. The shipped DLL is built with size optimisation and link-time
//! optimisation, where the compiler could change how many frames sit between the hook and the code that called the
//! detoured function. So before any hook is written into the game, the module hooks a tiny function of its own
//! (machine code written here, in memory it allocates) the same way, calls it from a tiny fake "tool" and checks
//! what the hook saw: the five arguments, one of them from the stack, the result, and the return address of the
//! call. If anything differs the real hooks are not installed.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::Mutex;

use crate::detour::{hook_caller, Detour};
use crate::mem;

/// `fn(a, b, c, d, e) -> a + b + c + d + e` with `e` on the stack, like `CommandList::Add`'s fifth argument. The
/// first 16 bytes build the frame; `e` is read after them through the `rbp` they set up, so the trampoline has to
/// run them correctly for the original to find its argument.
pub(crate) const SUM5: &[u8] = &[
    0x55, // push rbp
    0x48, 0x89, 0xE5, // mov rbp, rsp
    0x48, 0x89, 0xC8, // mov rax, rcx
    0x48, 0x01, 0xD0, // add rax, rdx
    0x4C, 0x01, 0xC0, // add rax, r8
    0x4C, 0x01, 0xC8, // add rax, r9
    0x48, 0x03, 0x45, 0x30, // add rax, [rbp+0x30]
    0x5D, // pop rbp
    0xC3, // ret
];
/// Bytes of [`SUM5`] that are whole instructions and position independent.
pub(crate) const SUM5_PROLOGUE: usize = 16;

/// A caller of a five-argument function, like a construction tool calling `CommandList::Add`:
/// `fn(callee) -> callee(1, 2, 3, 4, 5)`. The return address of its `call` is at offset [`TOOL_RETURN`].
pub(crate) const TOOL: [u8; 45] = [
    0x48, 0x83, 0xEC, 0x38, // sub rsp, 0x38
    0x48, 0x89, 0xC8, // mov rax, rcx
    0xB9, 0x01, 0, 0, 0, // mov ecx, 1
    0xBA, 0x02, 0, 0, 0, // mov edx, 2
    0x41, 0xB8, 0x03, 0, 0, 0, // mov r8d, 3
    0x41, 0xB9, 0x04, 0, 0, 0, // mov r9d, 4
    0x48, 0xC7, 0x44, 0x24, 0x20, 0x05, 0, 0, 0, // mov qword [rsp+0x20], 5
    0xFF, 0xD0, // call rax
    0x48, 0x83, 0xC4, 0x38, // add rsp, 0x38
    0xC3, // ret
];
pub(crate) const TOOL_RETURN: usize = 40;

type Sum5 = extern "system" fn(u64, u64, u64, u64, u64) -> u64;
type Tool = extern "system" fn(usize) -> u64;

static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static SEEN: AtomicU64 = AtomicU64::new(0);
static CALLER: AtomicUsize = AtomicUsize::new(0);
/// The probe's statics are shared: one run at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

extern "system" fn probe(a: u64, b: u64, c: u64, d: u64, e: u64) -> u64 {
    CALLER.store(hook_caller().unwrap_or(0), SeqCst);
    SEEN.store(a * 10_000 + b * 1000 + c * 100 + d * 10 + e, SeqCst);
    // SAFETY: the trampoline of the probed function, published before the jump was written.
    let original: Sum5 = unsafe { core::mem::transmute::<usize, Sum5>(ORIGINAL.load(SeqCst)) };
    original(a, b, c, d, e) + 1000
}

/// What the self-test found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Installing a detour, calling through it and the trampoline, and taking it out again.
    pub mechanics: Result<(), String>,
    /// Identifying the call site of a detoured function.
    pub caller: Result<(), String>,
}

/// Runs the self-test. Allocates 4 KiB of executable memory that is never freed.
pub fn run() -> Report {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let mut code = vec![0xCC; 0x100 + TOOL.len()];
    code[..SUM5.len()].copy_from_slice(SUM5);
    code[0x100..].copy_from_slice(&TOOL);
    // SAFETY: position-independent machine code written above.
    let base = match unsafe { mem::alloc_exec(&code) } {
        Ok(p) => p,
        Err(e) => {
            return Report { mechanics: Err(format!("no executable memory: {e}")), caller: Err("not run".into()) }
        }
    };
    CALLER.store(0, SeqCst);
    SEEN.store(0, SeqCst);
    // SAFETY: `base` holds SUM5; nothing runs it yet.
    let mut detour = match unsafe { Detour::prepare(base, &SUM5[..SUM5_PROLOGUE]) } {
        Ok(d) => d,
        Err(e) => return Report { mechanics: Err(format!("prepare: {e}")), caller: Err("not run".into()) },
    };
    ORIGINAL.store(detour.original() as usize, SeqCst);
    // SAFETY: no thread but this one can reach the probe function.
    if let Err(e) = unsafe { detour.commit(probe as *const () as usize) } {
        return Report { mechanics: Err(format!("commit: {e}")), caller: Err("not run".into()) };
    }
    // SAFETY: the tool is the machine code at +0x100, called with the probed function's address.
    let tool: Tool = unsafe { core::mem::transmute::<*mut u8, Tool>(base.add(0x100)) };
    let result = tool(base as usize);
    let (seen, caller) = (SEEN.load(SeqCst), CALLER.load(SeqCst));
    // SAFETY: still only this thread.
    let undone = unsafe { detour.uninstall() };

    let mut mechanics = Ok(());
    if result != 1015 {
        mechanics = Err(format!("the hooked function returned {result}, expected 1015"));
    } else if seen != 12345 {
        mechanics = Err(format!("the hook saw the arguments as {seen}, expected 12345"));
    } else if let Err(e) = undone {
        mechanics = Err(format!("uninstall: {e}"));
    }
    // SAFETY: reading back the probed function's own bytes.
    if mechanics.is_ok() && unsafe { mem::read(base, SUM5.len()) }.map(|b| b != SUM5).unwrap_or(true) {
        mechanics = Err("the probed function's bytes were not restored".into());
    }
    let expected = base as usize + 0x100 + TOOL_RETURN;
    let caller = if caller == expected {
        Ok(())
    } else {
        Err(format!("the hook identified the caller as {caller:#x}, expected {expected:#x}"))
    };
    Report { mechanics, caller }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_self_test_passes_here() {
        assert_eq!(run(), Report { mechanics: Ok(()), caller: Ok(()) });
    }

    #[test]
    fn it_can_be_run_again_and_in_parallel() {
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| assert_eq!(run(), Report { mechanics: Ok(()), caller: Ok(()) }));
            }
        });
    }
}
