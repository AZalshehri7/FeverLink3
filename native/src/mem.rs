//! Reading and writing the game's code safely: what is mapped, what protection it has, and writing with the
//! protection lifted and then put back exactly as it was.
//!
//! Windows only. Everything that patches code goes through here, so the rules are in one place:
//!
//! - nothing is read without asking the OS whether the range is committed and readable ([`readable`]);
//! - a write lifts the protection region by region, writes, flushes the instruction cache and restores each
//!   region's own earlier protection ([`write_code`]); a range that spans pages with different protections is
//!   handled, and a failed restore is reported, not hidden;
//! - executable memory for trampolines is allocated writable, filled, and only then made executable
//!   ([`alloc_exec`]).

use core::ffi::c_void;

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualProtect, VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ,
    PAGE_EXECUTE_READWRITE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// `PAGE_READONLY | PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE |
/// PAGE_EXECUTE_WRITECOPY`: the protections that allow an ordinary read. (`PAGE_GUARD` and `PAGE_NOACCESS` are
/// absent on purpose: touching them raises an exception.)
const READABLE: u32 = 0x02 | 0x04 | 0x08 | 0x20 | 0x40 | 0x80;
const PAGE_GUARD: u32 = 0x100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemError {
    /// Part of the range is not committed, or not readable.
    NotReadable,
    /// `VirtualQuery` failed (Windows error code).
    Query(u32),
    /// `VirtualProtect` failed (Windows error code).
    Protect(u32),
    /// `VirtualAlloc` failed (Windows error code).
    Alloc(u32),
}

impl std::fmt::Display for MemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemError::NotReadable => f.write_str("memory is not committed or not readable"),
            MemError::Query(e) => write!(f, "VirtualQuery failed (error {e})"),
            MemError::Protect(e) => write!(f, "VirtualProtect failed (error {e})"),
            MemError::Alloc(e) => write!(f, "VirtualAlloc failed (error {e})"),
        }
    }
}

/// One stretch of a range with a single protection: (start, length, protection).
type Region = (usize, usize, u32);

/// Splits `[addr, addr + len)` into stretches of uniform protection. Fails if any part is not committed.
///
/// # Safety
/// Only asks the OS about addresses; any `addr` is allowed.
unsafe fn regions(addr: usize, len: usize) -> Result<Vec<Region>, MemError> {
    let end = addr.checked_add(len).ok_or(MemError::NotReadable)?;
    let mut out = Vec::new();
    let mut cur = addr;
    while cur < end {
        // SAFETY: `mbi` is a plain struct of the size passed; VirtualQuery writes at most that many bytes.
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
        let got =
            unsafe { VirtualQuery(cur as *const c_void, &mut mbi, core::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
        if got == 0 {
            return Err(MemError::Query(unsafe { GetLastError() }));
        }
        if mbi.State != MEM_COMMIT {
            return Err(MemError::NotReadable);
        }
        let region_end = (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize);
        let seg_end = region_end.min(end);
        if seg_end <= cur {
            return Err(MemError::Query(0));
        }
        out.push((cur, seg_end - cur, mbi.Protect));
        cur = seg_end;
    }
    Ok(out)
}

/// True when every byte of the range is committed and an ordinary read of it cannot fault.
///
/// # Safety
/// Only asks the OS about addresses; any arguments are allowed.
pub unsafe fn readable(addr: *const u8, len: usize) -> bool {
    match unsafe { regions(addr as usize, len) } {
        Ok(rs) => rs.iter().all(|&(_, _, p)| p & READABLE != 0 && p & PAGE_GUARD == 0),
        Err(_) => false,
    }
}

/// Copies out a range after checking it is readable.
///
/// # Safety
/// Any arguments are allowed; the range is checked before it is read. (Another thread unmapping the range between
/// the check and the copy is not guarded against: the ranges here are the game's own code.)
pub unsafe fn read(addr: *const u8, len: usize) -> Result<Vec<u8>, MemError> {
    if !unsafe { readable(addr, len) } {
        return Err(MemError::NotReadable);
    }
    // SAFETY: just checked committed and readable.
    Ok(unsafe { core::slice::from_raw_parts(addr, len) }.to_vec())
}

/// The protection of the page at `addr`, for tests and diagnostics.
///
/// # Safety
/// Only asks the OS about an address; any value is allowed.
pub unsafe fn protection_of(addr: *const u8) -> Option<u32> {
    unsafe { regions(addr as usize, 1) }.ok().and_then(|r| r.first().map(|&(_, _, p)| p))
}

/// What a [`write_code`] left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    /// False when a region's earlier protection could not be put back (the bytes are written regardless).
    pub protection_restored: bool,
}

fn set_protection(region: Region, protection: u32) -> Result<u32, MemError> {
    let mut previous = 0u32;
    // SAFETY: the region was reported committed by VirtualQuery a moment ago; VirtualProtect only changes
    // attributes of whole pages and writes the previous protection into `previous`.
    if unsafe { VirtualProtect(region.0 as *const c_void, region.1, protection, &mut previous) } == 0 {
        return Err(MemError::Protect(unsafe { GetLastError() }));
    }
    Ok(previous)
}

/// Writes `segments` (offset from `addr`, bytes) in the given order with the protection of the covered pages
/// lifted, flushes the instruction cache, and puts every region's earlier protection back.
///
/// The order matters to the caller (a jump is written last so another thread never runs half of it); all
/// segments are written under one protection change.
///
/// # Safety
/// `addr .. addr + the end of the last segment` must be code or data of this process that nothing is executing
/// inside while it is rewritten (see [`crate::detour`]); the range is checked to be committed first.
pub unsafe fn write_code(addr: *mut u8, segments: &[(usize, &[u8])]) -> Result<Written, MemError> {
    let total = segments.iter().map(|(off, b)| off + b.len()).max().unwrap_or(0);
    if total == 0 {
        return Ok(Written { protection_restored: true });
    }
    let regs = unsafe { regions(addr as usize, total) }?;
    let mut lifted: Vec<Region> = Vec::with_capacity(regs.len());
    for &(start, len, protection) in &regs {
        if let Err(e) = set_protection((start, len, protection), PAGE_EXECUTE_READWRITE) {
            for &r in &lifted {
                let _ = set_protection(r, r.2);
            }
            return Err(e);
        }
        lifted.push((start, len, protection));
    }
    for &(off, bytes) in segments {
        // SAFETY: the range was committed and is now writable; the caller guarantees nothing runs inside it.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr.add(off), bytes.len()) };
    }
    // SAFETY: flushing the cache of our own process over the range just written.
    unsafe { FlushInstructionCache(GetCurrentProcess(), addr as *const c_void, total) };
    let mut restored = true;
    for &r in &lifted {
        restored &= set_protection(r, r.2).is_ok();
    }
    Ok(Written { protection_restored: restored })
}

/// Copies `bytes` into fresh executable memory (never writable and executable at the same time once this returns)
/// and returns its address. The memory is never freed: trampolines live as long as the process.
///
/// # Safety
/// `bytes` must be machine code that is correct at whatever address the OS picks (absolute jumps only).
pub unsafe fn alloc_exec(bytes: &[u8]) -> Result<*mut u8, MemError> {
    if bytes.is_empty() {
        return Err(MemError::Alloc(0));
    }
    // SAFETY: a fresh private allocation of `bytes.len()` bytes; the copy stays inside it.
    let mem =
        unsafe { VirtualAlloc(core::ptr::null(), bytes.len(), MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) } as *mut u8;
    if mem.is_null() {
        return Err(MemError::Alloc(unsafe { GetLastError() }));
    }
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), mem, bytes.len()) };
    let mut previous = 0u32;
    if unsafe { VirtualProtect(mem as *const c_void, bytes.len(), PAGE_EXECUTE_READ, &mut previous) } == 0 {
        return Err(MemError::Protect(unsafe { GetLastError() }));
    }
    unsafe { FlushInstructionCache(GetCurrentProcess(), mem as *const c_void, bytes.len()) };
    Ok(mem)
}

/// Pages of test memory with chosen protections, released on drop.
#[cfg(test)]
pub(crate) mod pages {
    use super::*;
    use windows_sys::Win32::System::Memory::{VirtualFree, MEM_RELEASE};

    pub struct Pages {
        pub base: *mut u8,
    }

    impl Pages {
        /// One zeroed page per entry, with `protections[i]` on page `i`.
        pub fn new(protections: &[u32]) -> Pages {
            let len = protections.len() * 0x1000;
            // SAFETY: a fresh allocation; each protection change covers one page of it.
            let base =
                unsafe { VirtualAlloc(core::ptr::null(), len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) } as *mut u8;
            assert!(!base.is_null(), "VirtualAlloc failed");
            for (i, &p) in protections.iter().enumerate() {
                let mut old = 0u32;
                let ok = unsafe { VirtualProtect(base.add(i * 0x1000) as *const c_void, 0x1000, p, &mut old) };
                assert!(ok != 0, "VirtualProtect failed");
            }
            Pages { base }
        }

        /// Fills memory regardless of the protection (lifts it, writes, puts it back).
        pub fn fill(&self, offset: usize, bytes: &[u8]) {
            // SAFETY: inside the allocation; nothing executes here.
            unsafe { write_code(self.base, &[(offset, bytes)]) }.expect("fill");
        }

        pub fn at(&self, offset: usize) -> *mut u8 {
            // SAFETY: stays inside the allocation in every caller.
            unsafe { self.base.add(offset) }
        }

        pub fn bytes(&self, offset: usize, len: usize) -> Vec<u8> {
            // SAFETY: a checked read of memory this struct owns.
            unsafe { read(self.at(offset), len) }.expect("readable")
        }

        pub fn protection(&self, page: usize) -> u32 {
            // SAFETY: an address inside the allocation.
            unsafe { protection_of(self.at(page * 0x1000)) }.expect("protection")
        }
    }

    impl Drop for Pages {
        fn drop(&mut self) {
            // SAFETY: releasing the allocation made in `new`.
            unsafe { VirtualFree(self.base as *mut c_void, 0, MEM_RELEASE) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pages::Pages;
    use super::*;
    use windows_sys::Win32::System::Memory::{PAGE_NOACCESS, PAGE_READONLY};

    #[test]
    fn readable_says_yes_for_mapped_pages_and_no_for_the_rest() {
        let p = Pages::new(&[PAGE_READONLY, PAGE_EXECUTE_READ, PAGE_NOACCESS]);
        unsafe {
            assert!(readable(p.at(0), 0x1000));
            assert!(readable(p.at(0xff0), 0x20), "across two readable pages");
            assert!(!readable(p.at(0x1ff0), 0x20), "into a no-access page");
            assert!(!readable(p.at(0x2000), 1), "a no-access page");
            assert!(!readable(p.at(0x2ff0), 0x20), "past the end of the allocation");
            assert!(!readable(core::ptr::null(), 1));
            assert!(!readable(usize::MAX as *const u8, 8), "address overflow is not a panic");
            assert!(read(p.at(0x2000), 4).is_err());
        }
    }

    #[test]
    fn a_write_across_pages_with_different_protections_restores_each() {
        let p = Pages::new(&[PAGE_EXECUTE_READ, PAGE_READONLY, PAGE_EXECUTE_READ]);
        let before: Vec<u32> = (0..3).map(|i| p.protection(i)).collect();
        assert_eq!(before, [PAGE_EXECUTE_READ, PAGE_READONLY, PAGE_EXECUTE_READ]);
        let data: Vec<u8> = (0..0x20u8).collect();
        let w = unsafe { write_code(p.at(0xff0), &[(0, &data[..0x10]), (0x10, &data[0x10..])]) }.unwrap();
        assert!(w.protection_restored);
        assert_eq!(p.bytes(0xff0, 0x20), data);
        assert_eq!((0..3).map(|i| p.protection(i)).collect::<Vec<_>>(), before, "every page is as it was");
    }

    #[test]
    fn segments_are_written_in_the_order_given() {
        let p = Pages::new(&[PAGE_EXECUTE_READ]);
        unsafe { write_code(p.at(0), &[(0, &[1, 1, 1, 1]), (2, &[2, 2])]) }.unwrap();
        assert_eq!(p.bytes(0, 4), [1, 1, 2, 2], "the later segment wins where they overlap");
    }

    #[test]
    fn writing_past_the_mapping_fails_cleanly_and_writes_nothing() {
        let q = Pages::new(&[PAGE_EXECUTE_READ]);
        assert_eq!(
            unsafe { write_code(q.at(0xff8), &[(0, &[9; 16])]) },
            Err(MemError::NotReadable),
            "runs off the end"
        );
        assert_eq!(q.protection(0), PAGE_EXECUTE_READ);
        assert_eq!(q.bytes(0xff8, 8), [0; 8], "nothing was written");
        assert_eq!(unsafe { write_code(core::ptr::null_mut(), &[(0, &[1])]) }, Err(MemError::NotReadable));
    }

    #[test]
    fn a_no_access_page_can_be_written_and_stays_no_access() {
        // committed memory is writable by this function whatever its protection; reading is checked separately
        let p = Pages::new(&[PAGE_NOACCESS]);
        let w = unsafe { write_code(p.at(0), &[(0, &[7, 7])]) }.unwrap();
        assert!(w.protection_restored);
        assert_eq!(p.protection(0), PAGE_NOACCESS);
        assert!(unsafe { read(p.at(0), 2) }.is_err(), "and it still cannot be read");
    }

    #[test]
    fn an_empty_write_does_nothing() {
        assert_eq!(unsafe { write_code(core::ptr::null_mut(), &[]) }, Ok(Written { protection_restored: true }));
    }

    #[test]
    fn executable_memory_comes_back_executable_and_not_writable() {
        let code = [0xB8, 0x2A, 0, 0, 0, 0xC3]; // mov eax, 42 ; ret
        let mem = unsafe { alloc_exec(&code) }.unwrap();
        assert_eq!(unsafe { protection_of(mem) }, Some(PAGE_EXECUTE_READ));
        assert_eq!(unsafe { read(mem, 6) }.unwrap(), code);
        let f: extern "system" fn() -> u32 = unsafe { core::mem::transmute(mem) };
        assert_eq!(f(), 42);
        assert!(unsafe { alloc_exec(&[]) }.is_err());
    }
}
