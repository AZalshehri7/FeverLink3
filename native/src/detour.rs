//! x86-64 inline detours: the byte maths is pure and tested; only [`install`] touches memory.
//!
//! A detour overwrites the first `stolen` bytes of a function with `jmp [rip+0]; dq hook` (14 bytes) and pads
//! the rest with `int3`. A trampoline holds the stolen bytes followed by the same kind of jump back to
//! `target + stolen`, so the hook can call the original. The stolen bytes must be whole instructions and must
//! not be position-dependent (no rip-relative operands, no relative jumps or calls): that is checked offline by
//! the address tool and recorded in the build table as the exact expected prologue, which [`install`] verifies
//! before writing. An unexpected prologue means a different build: nothing is patched.

pub const JMP_ABS_LEN: usize = 14;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetourError {
    /// The expected prologue is shorter than a 14-byte absolute jump.
    PrologueTooShort(usize),
    /// The bytes at the target differ from the expected prologue.
    UnexpectedBytes { expected: Vec<u8>, found: Vec<u8> },
    /// A memory operation failed (code from the OS).
    Memory(u32),
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

/// An installed detour: `original` is the trampoline, callable as the unhooked function.
#[derive(Debug)]
pub struct Detour {
    pub target: *mut u8,
    pub original: *const u8,
    pub stolen: Vec<u8>,
}

/// Installs a detour at `target`, whose first bytes must equal `expected`.
///
/// # Safety
/// `target` must point to executable code of a loaded module that no thread is executing inside the first
/// `expected.len()` bytes of while this runs (install at a quiet moment, before the hooked function is in use),
/// and `hook` must have the same calling convention and signature as the target.
#[cfg(windows)]
pub unsafe fn install(target: *mut u8, hook: usize, expected: &[u8]) -> Result<Detour, DetourError> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Memory::{
        VirtualAlloc, VirtualProtect, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    };

    let found = unsafe { core::slice::from_raw_parts(target, expected.len()) };
    check_prologue(found, expected)?;
    let patch = patch_bytes(hook as u64, expected.len())?;
    let tramp = trampoline_bytes(expected, target as u64);

    let mem = unsafe { VirtualAlloc(core::ptr::null(), tramp.len(), MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) }
        as *mut u8;
    if mem.is_null() {
        return Err(DetourError::Memory(unsafe { GetLastError() }));
    }
    unsafe { core::ptr::copy_nonoverlapping(tramp.as_ptr(), mem, tramp.len()) };
    let mut old = 0u32;
    if unsafe { VirtualProtect(mem as *const _, tramp.len(), PAGE_EXECUTE_READ, &mut old) } == 0 {
        return Err(DetourError::Memory(unsafe { GetLastError() }));
    }

    let mut prev = 0u32;
    if unsafe { VirtualProtect(target as *const _, patch.len(), PAGE_EXECUTE_READWRITE, &mut prev) } == 0 {
        return Err(DetourError::Memory(unsafe { GetLastError() }));
    }
    // The padding first, the jump last: a thread arriving between the two writes still sees a valid prologue
    // start or a complete jump, never half a jump.
    unsafe {
        core::ptr::copy_nonoverlapping(
            patch.as_ptr().add(JMP_ABS_LEN),
            target.add(JMP_ABS_LEN),
            patch.len() - JMP_ABS_LEN,
        );
        core::ptr::copy_nonoverlapping(patch.as_ptr(), target, JMP_ABS_LEN);
        VirtualProtect(target as *const _, patch.len(), prev, &mut prev);
    }
    Ok(Detour { target, original: mem as *const u8, stolen: expected.to_vec() })
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
}
