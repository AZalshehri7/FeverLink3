//! x86-64 inline detours: the byte maths is pure and tested; only [`Detour`] touches memory.
//!
//! A detour overwrites the first `stolen` bytes of a function with `jmp [rip+0]; dq hook` (14 bytes) and pads
//! the rest with `int3`. A trampoline holds the stolen bytes followed by the same kind of jump back to
//! `target + stolen`, so the hook can call the original. The stolen bytes must be whole instructions and must
//! not be position-dependent (no rip-relative operands, no relative jumps or calls): that is checked offline by
//! the address tool and recorded in the build table as the exact expected prologue, which [`Detour::prepare`]
//! verifies before anything is written. An unexpected prologue means a different build: nothing is patched.
//!
//! Installing is two steps so that the hook can never run before it knows its trampoline: [`Detour::prepare`]
//! checks the prologue and builds the trampoline, the caller publishes [`Detour::original`], and
//! [`Detour::commit`] writes the jump. A detour can be taken out again ([`Detour::uninstall`]); that is for the
//! moment right after installing (a later piece failed and the installation is rolled back) and for the tests,
//! not for a running game: rewriting a function other threads may be inside is not safe. A running module
//! switches its hooks to pass-through instead.

pub const JMP_ABS_LEN: usize = 14;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetourError {
    /// The expected prologue is shorter than a 14-byte absolute jump.
    PrologueTooShort(usize),
    /// The bytes at the target differ from the expected prologue.
    UnexpectedBytes { expected: Vec<u8>, found: Vec<u8> },
    /// The target is not committed, readable memory.
    NotReadable,
    /// A memory operation failed (code from the OS).
    Memory(u32),
    /// After the write the memory did not hold what was written; the original bytes were put back.
    VerifyFailed,
    /// The jump is already in place.
    AlreadyActive,
    /// The bytes at the target are neither the original prologue nor this detour's jump: someone else changed them.
    Changed,
}

impl std::fmt::Display for DetourError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DetourError::PrologueTooShort(n) => write!(f, "prologue of {n} bytes is shorter than a 14-byte jump"),
            DetourError::UnexpectedBytes { expected, found } => {
                write!(f, "found {} where the table expects {}", crate::plan::hex(found), crate::plan::hex(expected))
            }
            DetourError::NotReadable => f.write_str("the target is not readable memory"),
            DetourError::Memory(e) => write!(f, "a memory operation failed (error {e})"),
            DetourError::VerifyFailed => f.write_str("the written bytes did not read back"),
            DetourError::AlreadyActive => f.write_str("the detour is already installed"),
            DetourError::Changed => f.write_str("the function's bytes were changed by someone else"),
        }
    }
}

/// `FF 25 00 00 00 00` + absolute 64-bit address: jumps to `target` from anywhere.
pub fn jmp_abs(target: u64) -> [u8; JMP_ABS_LEN] {
    let mut b = [0u8; JMP_ABS_LEN];
    b[0] = 0xFF;
    b[1] = 0x25;
    b[6..].copy_from_slice(&target.to_le_bytes());
    b
}

/// The bytes written over the function start: the jump to `hook`, padded with `int3` to `stolen_len`.
pub fn patch_bytes(hook: u64, stolen_len: usize) -> Result<Vec<u8>, DetourError> {
    if stolen_len < JMP_ABS_LEN {
        return Err(DetourError::PrologueTooShort(stolen_len));
    }
    let mut v = jmp_abs(hook).to_vec();
    v.resize(stolen_len, 0xCC);
    Ok(v)
}

/// The trampoline: the stolen instructions, then a jump to the first byte after them in the original.
pub fn trampoline_bytes(stolen: &[u8], target: u64) -> Vec<u8> {
    let mut v = stolen.to_vec();
    v.extend_from_slice(&jmp_abs(target + stolen.len() as u64));
    v
}

/// Compares what is in memory with what the build table expects.
pub fn check_prologue(found: &[u8], expected: &[u8]) -> Result<(), DetourError> {
    if expected.len() < JMP_ABS_LEN {
        return Err(DetourError::PrologueTooShort(expected.len()));
    }
    if found.len() < expected.len() || &found[..expected.len()] != expected {
        return Err(DetourError::UnexpectedBytes {
            expected: expected.to_vec(),
            found: found[..found.len().min(expected.len())].to_vec(),
        });
    }
    Ok(())
}

#[cfg(windows)]
pub use windows_impl::{hook_caller, install, Detour};

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use crate::mem::{self, MemError};
    use core::ffi::c_void;
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;

    impl From<MemError> for DetourError {
        fn from(e: MemError) -> Self {
            match e {
                MemError::NotReadable => DetourError::NotReadable,
                MemError::Query(c) | MemError::Protect(c) | MemError::Alloc(c) => DetourError::Memory(c),
            }
        }
    }

    /// An inline detour of one function.
    #[derive(Debug)]
    pub struct Detour {
        target: *mut u8,
        /// The function's original first bytes.
        stolen: Vec<u8>,
        trampoline: *mut u8,
        /// The hook the jump leads to, once committed.
        hook: Option<usize>,
    }

    // SAFETY: the raw pointers are addresses of code (the game's function, our trampoline); nothing is borrowed.
    unsafe impl Send for Detour {}

    impl Detour {
        /// Checks that `target` holds `expected` and builds the trampoline. Nothing is written to the target.
        ///
        /// # Safety
        /// `target` must be the address of a function of this process, and `expected` its first bytes as whole,
        /// position-independent instructions (at least 14 bytes), as recorded in the build table.
        pub unsafe fn prepare(target: *mut u8, expected: &[u8]) -> Result<Detour, DetourError> {
            let found = unsafe { mem::read(target, expected.len()) }?;
            check_prologue(&found, expected)?;
            let trampoline = unsafe { mem::alloc_exec(&trampoline_bytes(expected, target as u64)) }?;
            Ok(Detour { target, stolen: expected.to_vec(), trampoline, hook: None })
        }

        /// The trampoline: callable as the unhooked function, with the function's own signature.
        pub fn original(&self) -> *const u8 {
            self.trampoline
        }

        pub fn target(&self) -> *mut u8 {
            self.target
        }

        pub fn is_active(&self) -> bool {
            self.hook.is_some()
        }

        /// Writes the jump to `hook`: the padding first and the jump last, then reads it back.
        ///
        /// # Safety
        /// No thread may be executing inside the first bytes of the target while this runs (install before the
        /// function is in use), and `hook` must have the target's calling convention and signature.
        pub unsafe fn commit(&mut self, hook: usize) -> Result<(), DetourError> {
            if self.is_active() {
                return Err(DetourError::AlreadyActive);
            }
            // nothing may have changed the function since prepare()
            let found = unsafe { mem::read(self.target, self.stolen.len()) }?;
            check_prologue(&found, &self.stolen)?;
            let patch = patch_bytes(hook as *const () as u64, self.stolen.len())?;
            unsafe {
                mem::write_code(self.target, &[(JMP_ABS_LEN, &patch[JMP_ABS_LEN..]), (0, &patch[..JMP_ABS_LEN])])
            }?;
            if unsafe { mem::read(self.target, patch.len()) }? != patch {
                let _ = unsafe { mem::write_code(self.target, &[(0, &self.stolen)]) };
                return Err(DetourError::VerifyFailed);
            }
            self.hook = Some(hook);
            Ok(())
        }

        /// Puts the original bytes back. Does nothing if the detour is not active; refuses (and leaves the bytes
        /// alone) if they are neither the original prologue nor this detour's jump.
        ///
        /// # Safety
        /// As for [`commit`](Self::commit): no thread may be inside the function's first bytes, and none may be
        /// about to enter the hook or the trampoline expecting the detour to stay.
        pub unsafe fn uninstall(&mut self) -> Result<(), DetourError> {
            let Some(hook) = self.hook else { return Ok(()) };
            let now = unsafe { mem::read(self.target, self.stolen.len()) }?;
            if now == self.stolen {
                self.hook = None;
                return Ok(());
            }
            if now != patch_bytes(hook as *const () as u64, self.stolen.len())? {
                return Err(DetourError::Changed);
            }
            let (head, tail) = self.stolen.split_at(JMP_ABS_LEN);
            unsafe { mem::write_code(self.target, &[(JMP_ABS_LEN, tail), (0, head)]) }?;
            if unsafe { mem::read(self.target, self.stolen.len()) }? != self.stolen {
                return Err(DetourError::VerifyFailed);
            }
            self.hook = None;
            Ok(())
        }
    }

    /// Prepares and commits in one go, for a hook that does not need its trampoline before it can be called.
    ///
    /// # Safety
    /// As for [`Detour::prepare`] and [`Detour::commit`].
    pub unsafe fn install(target: *mut u8, hook: usize, expected: &[u8]) -> Result<Detour, DetourError> {
        let mut d = unsafe { Detour::prepare(target, expected) }?;
        unsafe { d.commit(hook) }?;
        Ok(d)
    }

    /// The address the detoured function will return to: the instruction after the call that entered it.
    ///
    /// Call it straight from the hook function (which the detour's jump entered with the caller's return address
    /// still on the stack). The OS unwinds from here: frame 0 is the call inside this function, frame 1 the call
    /// inside the hook, frame 2 the call that entered the detoured function. `None` when the unwinder returns
    /// nothing. The fake-game tests pin the frame count; it must not be called from anywhere but the hook itself.
    #[inline(never)]
    pub fn hook_caller() -> Option<usize> {
        let mut frame: *mut c_void = core::ptr::null_mut();
        // SAFETY: one output slot for one frame; the hash pointer may be null.
        let got = unsafe { RtlCaptureStackBackTrace(2, 1, &mut frame, core::ptr::null_mut()) };
        (got == 1 && !frame.is_null()).then_some(frame as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_jump_encoding() {
        let j = jmp_abs(0x1122_3344_5566_7788);
        assert_eq!(&j[..6], &[0xFF, 0x25, 0, 0, 0, 0]);
        assert_eq!(u64::from_le_bytes(j[6..].try_into().unwrap()), 0x1122_3344_5566_7788);
    }

    #[test]
    fn patch_is_jump_plus_int3_padding() {
        let p = patch_bytes(0x1000, 16).unwrap();
        assert_eq!(p.len(), 16);
        assert_eq!(&p[..14], &jmp_abs(0x1000));
        assert_eq!(&p[14..], &[0xCC, 0xCC]);
        assert_eq!(patch_bytes(0x1000, 13), Err(DetourError::PrologueTooShort(13)));
    }

    #[test]
    fn trampoline_resumes_after_the_stolen_bytes() {
        let stolen = [0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x74, 0x24, 0x10, 0x57, 0x48, 0x83, 0xEC, 0x20];
        let t = trampoline_bytes(&stolen, 0x7FF6_0000_1000);
        assert_eq!(&t[..15], &stolen);
        assert_eq!(&t[15..], &jmp_abs(0x7FF6_0000_1000 + 15));
    }

    #[test]
    fn prologue_check() {
        let expected = [0x48u8; 14];
        assert!(check_prologue(&[0x48u8; 20], &expected).is_ok());
        assert!(matches!(check_prologue(&[0x48u8; 10], &expected), Err(DetourError::UnexpectedBytes { .. })));
        let mut other = [0x48u8; 14];
        other[5] = 0x90;
        assert!(matches!(check_prologue(&other, &expected), Err(DetourError::UnexpectedBytes { .. })));
        assert_eq!(check_prologue(&[0u8; 8], &[0u8; 8]), Err(DetourError::PrologueTooShort(8)));
    }

    #[test]
    fn errors_say_what_was_found() {
        let e = check_prologue(&[0x90; 14], &[0x48; 14]).unwrap_err();
        let text = e.to_string();
        assert!(text.contains("90 90") && text.contains("48 48"), "{text}");
    }
}

/// The real installer, run on fake game functions in executable memory: machine code written by hand, called
/// through function pointers, with Rust functions as the hooks.
#[cfg(all(test, windows, target_arch = "x86_64"))]
mod fake_game {
    use super::*;
    use crate::mem::pages::Pages;
    use crate::mem::protection_of;
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READ, PAGE_READONLY};

    /// `fn(a, b) -> a + b + 5`; the first 14 bytes (five whole instructions) are the prologue.
    const SUM: [u8; 16] = [
        0x55, // push rbp
        0x48, 0x89, 0xE5, // mov rbp, rsp
        0x48, 0x89, 0xC8, // mov rax, rcx
        0x48, 0x01, 0xD0, // add rax, rdx
        0x48, 0x83, 0xC0, 0x05, // add rax, 5
        0x5D, // pop rbp
        0xC3, // ret
    ];

    /// `fn(a, b, c, d, e) -> a + b + c + d + e` with `e` on the stack, like `CommandList::Add`'s fifth argument. The
    /// first 16 bytes build the frame; `e` is read after them, through the `rbp` they set up.
    const SUM5: &[u8] = &[
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
    const SUM5_PROLOGUE: usize = 16;

    /// A caller of a five-argument function, like a construction tool calling `CommandList::Add`:
    /// `fn(callee) -> callee(1, 2, 3, 4, 5)`. The return address of its `call` is at offset [`TOOL_RETURN`].
    const TOOL: [u8; 45] = [
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
    const TOOL_RETURN: usize = 40;

    type Sum = extern "system" fn(u64, u64) -> u64;
    type Sum5 = extern "system" fn(u64, u64, u64, u64, u64) -> u64;
    type Tool = extern "system" fn(usize) -> u64;

    fn sum_page() -> Pages {
        let p = Pages::new(&[PAGE_EXECUTE_READ]);
        p.fill(0, &SUM);
        p
    }

    #[test]
    fn the_hook_runs_the_original_runs_and_uninstall_puts_everything_back() {
        static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
        static CALLS: AtomicU64 = AtomicU64::new(0);
        extern "system" fn hook(a: u64, b: u64) -> u64 {
            CALLS.fetch_add(1, SeqCst);
            let orig: Sum = unsafe { core::mem::transmute(ORIGINAL.load(SeqCst)) };
            orig(a, b) * 10
        }

        let page = sum_page();
        let f: Sum = unsafe { core::mem::transmute(page.at(0)) };
        assert_eq!(f(1, 2), 8);

        let mut d = unsafe { Detour::prepare(page.at(0), &SUM[..14]) }.unwrap();
        assert!(!d.is_active());
        assert_eq!(page.bytes(0, 16), SUM, "prepare writes nothing to the target");
        ORIGINAL.store(d.original() as usize, SeqCst);
        unsafe { d.commit(hook as *const () as usize) }.unwrap();
        assert!(d.is_active());

        assert_eq!(f(1, 2), 80, "the hook ran, called the original through the trampoline, and changed the result");
        assert_eq!(CALLS.load(SeqCst), 1);
        let orig: Sum = unsafe { core::mem::transmute(d.original()) };
        assert_eq!(orig(3, 4), 12, "the trampoline is the unhooked function");
        assert_eq!(CALLS.load(SeqCst), 1, "and does not go through the hook");
        assert_eq!(page.bytes(0, 14), jmp_abs(hook as *const () as u64), "the jump is in place");
        assert_eq!(page.bytes(14, 2), SUM[14..], "the bytes after the stolen ones are untouched");
        assert_eq!(page.protection(0), PAGE_EXECUTE_READ, "the page protection is as it was");

        unsafe { d.uninstall() }.unwrap();
        assert!(!d.is_active());
        assert_eq!(page.bytes(0, 16), SUM, "every byte is the original again");
        assert_eq!(f(1, 2), 8);
        assert_eq!(CALLS.load(SeqCst), 1, "the hook is no longer called");
        assert_eq!(page.protection(0), PAGE_EXECUTE_READ);
        unsafe { d.uninstall() }.unwrap(); // a second uninstall is a no-op
    }

    #[test]
    fn install_is_prepare_and_commit() {
        extern "system" fn hook(a: u64, b: u64) -> u64 {
            a * 1000 + b
        }
        let page = sum_page();
        let d = unsafe { install(page.at(0), hook as *const () as usize, &SUM[..14]) }.unwrap();
        assert!(d.is_active());
        let f: Sum = unsafe { core::mem::transmute(page.at(0)) };
        assert_eq!(f(7, 9), 7009);
    }

    #[test]
    fn a_different_prologue_writes_nothing() {
        let page = sum_page();
        let mut wrong = SUM;
        wrong[6] = 0xC9; // mov rax, rcx -> mov rcx, rcx
        page.fill(0, &wrong);
        let r = unsafe { Detour::prepare(page.at(0), &SUM[..14]) };
        assert!(matches!(r, Err(DetourError::UnexpectedBytes { .. })), "{r:?}");
        assert_eq!(page.bytes(0, 16), wrong);
        assert_eq!(page.protection(0), PAGE_EXECUTE_READ);
        let short = unsafe { Detour::prepare(page.at(0), &SUM[..10]) };
        assert_eq!(short.unwrap_err(), DetourError::PrologueTooShort(10));
    }

    #[test]
    fn a_target_that_is_not_there_is_an_error_not_a_crash() {
        let page = sum_page();
        // the prologue would run off the end of the allocation
        let r = unsafe { Detour::prepare(page.at(0x1000 - 8), &SUM[..14]) };
        assert_eq!(r.unwrap_err(), DetourError::NotReadable);
        assert_eq!(
            unsafe { Detour::prepare(core::ptr::null_mut(), &SUM[..14]) }.unwrap_err(),
            DetourError::NotReadable
        );
    }

    #[test]
    fn committing_twice_or_after_a_change_is_refused() {
        extern "system" fn hook(a: u64, b: u64) -> u64 {
            a + b
        }
        let page = sum_page();
        let mut d = unsafe { Detour::prepare(page.at(0), &SUM[..14]) }.unwrap();
        // somebody else hooks the function between prepare and commit
        page.fill(0, &[0xE9, 1, 2, 3, 4]);
        assert!(matches!(unsafe { d.commit(hook as *const () as usize) }, Err(DetourError::UnexpectedBytes { .. })));
        assert!(!d.is_active());
        page.fill(0, &SUM);
        unsafe { d.commit(hook as *const () as usize) }.unwrap();
        assert_eq!(unsafe { d.commit(hook as *const () as usize) }, Err(DetourError::AlreadyActive));
    }

    #[test]
    fn uninstall_leaves_alone_a_function_someone_else_changed() {
        extern "system" fn hook(a: u64, b: u64) -> u64 {
            a + b
        }
        let page = sum_page();
        let mut d = unsafe { install(page.at(0), hook as *const () as usize, &SUM[..14]) }.unwrap();
        page.fill(0, &[0x90]); // a second hook starts by overwriting our jump
        let changed = page.bytes(0, 16);
        assert_eq!(unsafe { d.uninstall() }, Err(DetourError::Changed));
        assert_eq!(page.bytes(0, 16), changed, "nothing was touched");
        assert!(d.is_active(), "and it stays marked active");
    }

    #[test]
    fn a_stack_argument_and_the_frame_survive_the_detour() {
        // CommandList::Add has five arguments, the fifth on the stack: the hook must see it, and the original, run
        // through the trampoline with the stolen frame set-up, must still find it.
        static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
        static SEEN: AtomicU64 = AtomicU64::new(0);
        extern "system" fn hook(a: u64, b: u64, c: u64, d: u64, e: u64) -> u64 {
            SEEN.store(a * 10_000 + b * 1000 + c * 100 + d * 10 + e, SeqCst);
            let orig: Sum5 = unsafe { core::mem::transmute(ORIGINAL.load(SeqCst)) };
            orig(a, b, c, d, e) + 1000
        }
        let page = Pages::new(&[PAGE_EXECUTE_READ]);
        page.fill(0, SUM5);
        let f: Sum5 = unsafe { core::mem::transmute(page.at(0)) };
        assert_eq!(f(1, 2, 3, 4, 5), 15);
        let mut d = unsafe { Detour::prepare(page.at(0), &SUM5[..SUM5_PROLOGUE]) }.unwrap();
        ORIGINAL.store(d.original() as usize, SeqCst);
        unsafe { d.commit(hook as *const () as usize) }.unwrap();
        assert_eq!(f(1, 2, 3, 4, 5), 1015);
        assert_eq!(SEEN.load(SeqCst), 12345, "the hook saw all five arguments, the fifth from the stack");
    }

    #[test]
    fn the_hook_can_tell_which_call_site_called_the_function() {
        static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
        static CALLER: AtomicUsize = AtomicUsize::new(0);
        extern "system" fn hook(a: u64, b: u64, c: u64, d: u64, e: u64) -> u64 {
            CALLER.store(hook_caller().unwrap_or(0), SeqCst);
            let orig: Sum5 = unsafe { core::mem::transmute(ORIGINAL.load(SeqCst)) };
            orig(a, b, c, d, e)
        }
        let page = Pages::new(&[PAGE_EXECUTE_READ]);
        page.fill(0, SUM5);
        page.fill(0x100, &TOOL);
        let mut d = unsafe { Detour::prepare(page.at(0), &SUM5[..SUM5_PROLOGUE]) }.unwrap();
        ORIGINAL.store(d.original() as usize, SeqCst);
        unsafe { d.commit(hook as *const () as usize) }.unwrap();

        let tool: Tool = unsafe { core::mem::transmute(page.at(0x100)) };
        assert_eq!(tool(page.at(0) as usize), 15);
        assert_eq!(
            CALLER.load(SeqCst),
            page.at(0x100 + TOOL_RETURN) as usize,
            "the hook found the instruction after the tool's call: the return address of the detoured function"
        );

        // called from Rust instead, the caller is somewhere else entirely
        let f: Sum5 = unsafe { core::mem::transmute(page.at(0)) };
        f(1, 1, 1, 1, 1);
        let from_rust = CALLER.load(SeqCst);
        assert_ne!(from_rust, 0);
        assert_ne!(from_rust, page.at(0x100 + TOOL_RETURN) as usize);
        assert!(!(page.at(0x100) as usize..page.at(0x100 + TOOL.len()) as usize).contains(&from_rust));
    }

    #[test]
    fn the_page_stays_executable_and_unwritable_throughout() {
        extern "system" fn hook(a: u64, b: u64) -> u64 {
            a + b
        }
        let page = Pages::new(&[PAGE_EXECUTE_READ, PAGE_READONLY]);
        page.fill(0, &SUM);
        let mut d = unsafe { install(page.at(0), hook as *const () as usize, &SUM[..14]) }.unwrap();
        assert_eq!(page.protection(0), PAGE_EXECUTE_READ);
        assert_eq!(unsafe { protection_of(d.original()) }, Some(PAGE_EXECUTE_READ), "the trampoline is not writable");
        unsafe { d.uninstall() }.unwrap();
        assert_eq!(page.protection(1), PAGE_READONLY);
    }
}
