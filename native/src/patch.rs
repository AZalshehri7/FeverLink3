//! Rewriting bytes of the game's code in place (no detour, no trampoline): the single-worker script pool.
//!
//! A patch site records the exact bytes that must be there (`expected`) and the bytes of the same length that
//! replace them. [`AppliedPatch::apply`] checks the memory is readable and holds `expected`, lifts the page
//! protection, writes, flushes the instruction cache, puts the protection back, and reads the result back;
//! [`AppliedPatch::restore`] does the reverse and refuses to touch bytes somebody else changed since. Nothing is
//! written when anything is in doubt.
//!
//! Like a detour, a patch is made while no thread can be inside the patched bytes: the module applies it in
//! `DllMain`, before the game has started the code that runs there.

/// Why a patch was not applied or restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchError {
    /// The site is unusable: empty, or `expected` and `replacement` differ in length, or are equal.
    BadSite,
    /// The target is not committed, readable memory.
    NotReadable,
    /// The bytes in memory are not the expected ones: another build, or something else patched them.
    UnexpectedBytes { expected: Vec<u8>, found: Vec<u8> },
    /// A memory operation failed (code from the OS).
    Memory(u32),
    /// After the write the memory did not hold what was written; the original bytes were put back.
    VerifyFailed,
    /// On restore: the bytes are neither the replacement nor the original, so someone else changed them.
    Changed,
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatchError::BadSite => f.write_str("the patch site is not a same-length, different replacement"),
            PatchError::NotReadable => f.write_str("the target is not readable memory"),
            PatchError::UnexpectedBytes { expected, found } => {
                write!(f, "found {} where the table expects {}", crate::plan::hex(found), crate::plan::hex(expected))
            }
            PatchError::Memory(e) => write!(f, "a memory operation failed (error {e})"),
            PatchError::VerifyFailed => f.write_str("the written bytes did not read back"),
            PatchError::Changed => f.write_str("the bytes were changed by someone else"),
        }
    }
}

/// The site itself must make sense.
pub fn check_site(expected: &[u8], replacement: &[u8]) -> Result<(), PatchError> {
    if expected.is_empty() || expected.len() != replacement.len() || expected == replacement {
        return Err(PatchError::BadSite);
    }
    Ok(())
}

/// What is in memory must be exactly what the table expects.
pub fn check_found(found: &[u8], expected: &[u8]) -> Result<(), PatchError> {
    if found != expected {
        return Err(PatchError::UnexpectedBytes { expected: expected.to_vec(), found: found.to_vec() });
    }
    Ok(())
}

#[cfg(windows)]
pub use windows_impl::AppliedPatch;

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use crate::mem::{self, MemError};

    impl From<MemError> for PatchError {
        fn from(e: MemError) -> Self {
            match e {
                MemError::NotReadable => PatchError::NotReadable,
                MemError::Query(c) | MemError::Protect(c) | MemError::Alloc(c) => PatchError::Memory(c),
            }
        }
    }

    /// A patch that is in place and can be taken out again.
    #[derive(Debug)]
    pub struct AppliedPatch {
        target: *mut u8,
        original: Vec<u8>,
        replacement: Vec<u8>,
        applied: bool,
        protection_restored: bool,
    }

    // SAFETY: the pointer is an address in the game's code; nothing is borrowed.
    unsafe impl Send for AppliedPatch {}

    impl AppliedPatch {
        /// Rewrites `expected` to `replacement` at `target`, if and only if `expected` is what is there.
        ///
        /// # Safety
        /// `target` must be an address in this process and no thread may be executing inside
        /// `target .. target + expected.len()` while the write happens.
        pub unsafe fn apply(target: *mut u8, expected: &[u8], replacement: &[u8]) -> Result<AppliedPatch, PatchError> {
            check_site(expected, replacement)?;
            let found = unsafe { mem::read(target, expected.len()) }?;
            check_found(&found, expected)?;
            let written = unsafe { mem::write_code(target, &[(0, replacement)]) }?;
            if unsafe { mem::read(target, replacement.len()) }? != replacement {
                let _ = unsafe { mem::write_code(target, &[(0, expected)]) };
                return Err(PatchError::VerifyFailed);
            }
            Ok(AppliedPatch {
                target,
                original: expected.to_vec(),
                replacement: replacement.to_vec(),
                applied: true,
                protection_restored: written.protection_restored,
            })
        }

        pub fn is_applied(&self) -> bool {
            self.applied
        }

        /// False when the pages' earlier protection could not be put back after the write (the patch works anyway).
        pub fn protection_restored(&self) -> bool {
            self.protection_restored
        }

        pub fn target(&self) -> *mut u8 {
            self.target
        }

        /// Puts the original bytes back. Does nothing if the patch is not applied; refuses if the bytes are
        /// neither the replacement nor the original.
        ///
        /// # Safety
        /// As for [`apply`](Self::apply).
        pub unsafe fn restore(&mut self) -> Result<(), PatchError> {
            if !self.applied {
                return Ok(());
            }
            let now = unsafe { mem::read(self.target, self.original.len()) }?;
            if now == self.original {
                self.applied = false;
                return Ok(());
            }
            if now != self.replacement {
                return Err(PatchError::Changed);
            }
            unsafe { mem::write_code(self.target, &[(0, &self.original)]) }?;
            if unsafe { mem::read(self.target, self.original.len()) }? != self.original {
                return Err(PatchError::VerifyFailed);
            }
            self.applied = false;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_must_replace_bytes_with_other_bytes_of_the_same_length() {
        assert_eq!(check_site(&[1, 2], &[3, 4]), Ok(()));
        assert_eq!(check_site(&[], &[]), Err(PatchError::BadSite));
        assert_eq!(check_site(&[1, 2], &[3]), Err(PatchError::BadSite));
        assert_eq!(check_site(&[1, 2], &[1, 2]), Err(PatchError::BadSite), "a patch that changes nothing is a mistake");
    }

    #[test]
    fn found_bytes_must_match_exactly() {
        assert_eq!(check_found(&[1, 2, 3], &[1, 2, 3]), Ok(()));
        let e = check_found(&[1, 2, 4], &[1, 2, 3]).unwrap_err();
        assert!(matches!(e, PatchError::UnexpectedBytes { .. }));
        assert!(e.to_string().contains("01 02 04") && e.to_string().contains("01 02 03"));
        assert!(check_found(&[1, 2], &[1, 2, 3]).is_err(), "a short read is not a match");
    }

    #[test]
    fn the_pool_patch_of_every_known_build_is_a_valid_site() {
        for b in crate::build_table::BUILDS {
            if let Some(p) = &b.script_pool {
                assert_eq!(check_site(p.expected, p.replacement), Ok(()), "{}", b.name);
            }
        }
    }
}

/// The real patcher on memory with real page protections.
#[cfg(all(test, windows))]
mod on_memory {
    use super::*;
    use crate::build_table::select;
    use crate::mem::pages::Pages;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READ, PAGE_READONLY};

    fn site() -> (&'static [u8], &'static [u8]) {
        let p = select(0x6ac5_0427).unwrap().script_pool.as_ref().unwrap();
        (p.expected, p.replacement)
    }

    /// Two pages with different protections and the 22-byte region straddling their boundary.
    fn pages_with_the_region() -> (Pages, usize) {
        let (expected, _) = site();
        let at = 0x1000 - 9;
        let p = Pages::new(&[PAGE_EXECUTE_READ, PAGE_READONLY]);
        p.fill(at - 4, &[0xAA; 4]);
        p.fill(at, expected);
        p.fill(at + expected.len(), &[0xBB; 4]);
        (p, at)
    }

    #[test]
    fn apply_rewrites_the_region_and_restore_puts_it_back() {
        let (expected, replacement) = site();
        let (pages, at) = pages_with_the_region();
        let mut patch = unsafe { AppliedPatch::apply(pages.at(at), expected, replacement) }.unwrap();
        assert!(patch.is_applied() && patch.protection_restored());
        assert_eq!(pages.bytes(at, expected.len()), replacement);
        assert_eq!(pages.bytes(at - 4, 4), [0xAA; 4], "the neighbours are untouched");
        assert_eq!(pages.bytes(at + expected.len(), 4), [0xBB; 4]);
        assert_eq!((pages.protection(0), pages.protection(1)), (PAGE_EXECUTE_READ, PAGE_READONLY));

        unsafe { patch.restore() }.unwrap();
        assert!(!patch.is_applied());
        assert_eq!(pages.bytes(at, expected.len()), expected, "byte for byte the original");
        assert_eq!((pages.protection(0), pages.protection(1)), (PAGE_EXECUTE_READ, PAGE_READONLY));
        unsafe { patch.restore() }.unwrap(); // idempotent
    }

    #[test]
    fn a_region_that_is_not_the_expected_one_is_left_alone() {
        let (expected, replacement) = site();
        let (pages, at) = pages_with_the_region();
        let mut other = expected.to_vec();
        other[5] ^= 0xFF; // a different jge displacement: another build
        pages.fill(at, &other);
        let r = unsafe { AppliedPatch::apply(pages.at(at), expected, replacement) };
        assert!(matches!(r, Err(PatchError::UnexpectedBytes { .. })), "{r:?}");
        assert_eq!(pages.bytes(at, expected.len()), other, "nothing was written");
        assert_eq!((pages.protection(0), pages.protection(1)), (PAGE_EXECUTE_READ, PAGE_READONLY));
    }

    #[test]
    fn unreadable_or_missing_memory_is_an_error() {
        let (expected, replacement) = site();
        let pages = Pages::new(&[PAGE_EXECUTE_READ]);
        let r = unsafe { AppliedPatch::apply(pages.at(0x1000 - 4), expected, replacement) };
        assert_eq!(r.unwrap_err(), PatchError::NotReadable, "the region runs off the mapping");
        assert_eq!(
            unsafe { AppliedPatch::apply(core::ptr::null_mut(), expected, replacement) }.unwrap_err(),
            PatchError::NotReadable
        );
        assert_eq!(unsafe { AppliedPatch::apply(pages.at(0), expected, expected) }.unwrap_err(), PatchError::BadSite);
    }

    #[test]
    fn restore_refuses_bytes_somebody_else_changed() {
        let (expected, replacement) = site();
        let (pages, at) = pages_with_the_region();
        let mut patch = unsafe { AppliedPatch::apply(pages.at(at), expected, replacement) }.unwrap();
        pages.fill(at + 3, &[0x90]);
        let changed = pages.bytes(at, expected.len());
        assert_eq!(unsafe { patch.restore() }, Err(PatchError::Changed));
        assert_eq!(pages.bytes(at, expected.len()), changed, "left as found");
        assert!(patch.is_applied());
    }

    #[test]
    fn restore_after_the_original_is_back_by_other_means_is_fine() {
        let (expected, replacement) = site();
        let (pages, at) = pages_with_the_region();
        let mut patch = unsafe { AppliedPatch::apply(pages.at(at), expected, replacement) }.unwrap();
        pages.fill(at, expected);
        unsafe { patch.restore() }.unwrap();
        assert!(!patch.is_applied());
    }
}
