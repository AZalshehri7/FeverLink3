//! Guarded reads of the engine's heap: a bad pointer becomes a refused read (and so a refused build), never a crash of the game.
//!
//! Why (review of TPF3-MP, 2026-10-19, idea 12): the native copy and the inject path follow the engine's own pointers through a
//! held command (vectors, lists, hash nodes, the Context's refund list), and the capture reads the command's flags. Until now every
//! read asked the OS first (`VirtualQuery` per read, about a microsecond, two syscalls for a range that crosses a region) and then read
//! the memory: correct for a pointer that was never valid, but a block the engine frees and decommits between the question and the read
//! (the simulation thread runs while the interface thread reads) still faulted and took the game down. The id-order trace read the
//! free-id deque without any check at all.
//!
//! The design, in three layers, cheapest first:
//!
//! 1. [`check`]: a pure test of the address itself: not null and above the first 64 KB (never mapped on Windows), the whole range in
//!    user space without overflow, a sane length, and the value's natural alignment for a typed read ([`read_value`]).
//! 2. A per-thread cache of the last [`CACHE_SLOTS`] regions the OS reported committed and readable ([`RegionCache`]): a read inside one
//!    of them asks nothing. A miss asks `VirtualQuery` for each region of the range and caches the readable ones. Only positive answers
//!    are cached (a region committed later must not stay refused: a false refusal on one game would itself fork the worlds).
//! 3. The copy itself is a small leaf routine of our own (x86-64 assembly, below) that the module's vectored exception handler knows: an
//!    access fault (or a guard page, or an in-page error) whose instruction pointer lies inside that routine resumes at the routine's
//!    failure exit, which returns 0. Every other exception of the process is left to the next handler untouched, so the game's own
//!    crash handling is unchanged. A cached region that went away (the race above) is a refused read, and that thread's cache is
//!    cleared.
//!
//! Without the handler (it could not be registered, or another architecture) the reads are as before: the OS is asked for every range.
//!
//! The cost is measured by `a_cached_read_costs_less_than_the_os_question_it_replaces` (`--nocapture` prints it).

use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT};

/// Nothing below this is ever mapped in a Windows process (the first 64 KB are reserved).
pub const LOWEST: usize = 0x1_0000;
/// The end of user space on x64 Windows (128 TB): a "pointer" above it is a value.
pub const USER_END: usize = 0x7FFF_FFFF_0000;
/// No single read is longer than this (the largest the module makes: a vector of segments or a dump block, a few MB).
pub const MAX_LEN: usize = 64 << 20;
/// Regions remembered per thread.
pub const CACHE_SLOTS: usize = 8;

/// Why a read was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Null or inside the first 64 KB.
    Null,
    /// A typed read at an address that is not a multiple of the type's alignment.
    Misaligned,
    /// The range leaves user space or overflows.
    OutOfRange,
    /// Longer than [`MAX_LEN`] (or empty).
    BadLength,
    /// The OS says part of the range is not committed, not readable, or a guard page.
    NotMapped,
    /// The read faulted although the region was believed readable (it went away meanwhile): contained by the handler.
    Fault,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refused::Null => "a null pointer",
            Refused::Misaligned => "a misaligned pointer",
            Refused::OutOfRange => "outside user space",
            Refused::BadLength => "a length that is not plausible",
            Refused::NotMapped => "memory that is not committed and readable",
            Refused::Fault => "memory that went away while it was read (fault contained)",
        })
    }
}

/// Layer 1: the address alone. `align` is 1 for byte buffers.
pub fn check(addr: usize, len: usize, align: usize) -> Result<(), Refused> {
    if len == 0 || len > MAX_LEN {
        return Err(Refused::BadLength);
    }
    if addr < LOWEST {
        return Err(Refused::Null);
    }
    if align > 1 && !addr.is_multiple_of(align) {
        return Err(Refused::Misaligned);
    }
    match addr.checked_add(len) {
        Some(end) if end <= USER_END => Ok(()),
        _ => Err(Refused::OutOfRange),
    }
}

/// The regions a thread knows to be readable, `[start, end)`, replaced round-robin. Pure (unit-tested without the OS).
#[derive(Debug, Clone, Copy)]
pub struct RegionCache {
    slots: [(usize, usize); CACHE_SLOTS],
    next: usize,
}

impl RegionCache {
    pub const fn new() -> RegionCache {
        RegionCache { slots: [(0, 0); CACHE_SLOTS], next: 0 }
    }

    /// Whether one remembered region holds the whole of `[addr, end)`.
    pub fn covers(&self, addr: usize, end: usize) -> bool {
        self.slots.iter().any(|&(s, e)| s <= addr && end <= e && s < e)
    }

    pub fn insert(&mut self, start: usize, end: usize) {
        if start >= end || self.slots.contains(&(start, end)) {
            return;
        }
        self.slots[self.next] = (start, end);
        self.next = (self.next + 1) % CACHE_SLOTS;
    }

    pub fn clear(&mut self) {
        *self = RegionCache::new();
    }
}

impl Default for RegionCache {
    fn default() -> Self {
        RegionCache::new()
    }
}

thread_local! {
    static CACHE: RefCell<RegionCache> = const { RefCell::new(RegionCache::new()) };
}

/// Counters for the log and the tests (whole process).
static FAULTS: AtomicU64 = AtomicU64::new(0);
static LAST_FAULT_ADDR: AtomicUsize = AtomicUsize::new(0);
static QUERIES: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);

/// What the guard has done so far in this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Faults the handler contained.
    pub faults: u64,
    /// The address of the newest one.
    pub last_fault: usize,
    /// `VirtualQuery` calls (cache misses).
    pub queries: u64,
    /// Reads refused by the address check or the OS (not faults).
    pub refused: u64,
}

pub fn stats() -> Stats {
    Stats {
        faults: FAULTS.load(Ordering::Relaxed),
        last_fault: LAST_FAULT_ADDR.load(Ordering::Relaxed),
        queries: QUERIES.load(Ordering::Relaxed),
        refused: REFUSED.load(Ordering::Relaxed),
    }
}

/// `PAGE_READONLY | PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY`.
const READABLE: u32 = 0x02 | 0x04 | 0x08 | 0x20 | 0x40 | 0x80;
const PAGE_GUARD: u32 = 0x100;

/// Layer 2: the OS, for a range no cached region covers; the readable regions it reports are cached.
fn os_says_readable(cache: &mut RegionCache, addr: usize, end: usize) -> bool {
    let mut cur = addr;
    let mut found: [(usize, usize); 4] = [(0, 0); 4];
    let mut n = 0;
    while cur < end {
        QUERIES.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `mbi` is a plain struct of the size passed; VirtualQuery only reads the address space.
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { core::mem::zeroed() };
        let got = unsafe { VirtualQuery(cur as *const _, &mut mbi, core::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
        if got == 0 || mbi.State != MEM_COMMIT || mbi.Protect & READABLE == 0 || mbi.Protect & PAGE_GUARD != 0 {
            return false;
        }
        let start = mbi.BaseAddress as usize;
        let region_end = start.saturating_add(mbi.RegionSize);
        if region_end <= cur {
            return false;
        }
        if n < found.len() {
            found[n] = (start, region_end);
            n += 1;
        }
        cur = region_end;
    }
    for &(s, e) in &found[..n] {
        cache.insert(s, e);
    }
    true
}

/// Layers 1 and 2 for `[addr, addr + len)`.
fn admitted(addr: usize, len: usize, align: usize) -> Result<(), Refused> {
    check(addr, len, align)?;
    let end = addr + len;
    let ok = CACHE
        .try_with(|c| match c.try_borrow_mut() {
            Ok(mut c) => c.covers(addr, end) || os_says_readable(&mut c, addr, end),
            // re-entered on this thread (cannot happen: nothing in here reads guarded memory); ask the OS without the cache
            Err(_) => os_says_readable(&mut RegionCache::new(), addr, end),
        })
        .unwrap_or_else(|_| os_says_readable(&mut RegionCache::new(), addr, end));
    if ok {
        Ok(())
    } else {
        Err(Refused::NotMapped)
    }
}

fn forget_this_threads_regions() {
    let _ = CACHE.try_with(|c| {
        if let Ok(mut c) = c.try_borrow_mut() {
            c.clear();
        }
    });
}

/// Copies `out.len()` bytes from `addr`. `Err` when any layer refuses; `out` is then unspecified.
pub fn read_into(addr: usize, out: &mut [u8]) -> Result<(), Refused> {
    read_aligned_into(addr, out, 1)
}

fn read_aligned_into(addr: usize, out: &mut [u8], align: usize) -> Result<(), Refused> {
    if let Err(r) = admitted(addr, out.len(), align) {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return Err(r);
    }
    if copy::tolerant() {
        // SAFETY: `out` is ours and `out.len()` long; the source is only read, by the leaf routine the handler knows.
        if unsafe { copy::guarded(out.as_mut_ptr(), addr as *const u8, out.len()) } {
            return Ok(());
        }
        forget_this_threads_regions();
        report_fault(addr, out.len());
        return Err(Refused::Fault);
    }
    // no handler: as the module read before, after the OS said the range is readable
    // SAFETY: the OS reported every byte committed and readable a moment ago (another thread freeing it meanwhile is not guarded).
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len()) };
    Ok(())
}

/// The log line of a contained fault (the first 20, then every 100th): the caller's refusal says which build it cost.
fn report_fault(addr: usize, len: usize) {
    let n = FAULTS.load(Ordering::Relaxed);
    if n <= 20 || n.is_multiple_of(100) {
        crate::log::line(&format!(
            "memguard: a read of {len} bytes at {addr:#x} FAULTED (the memory went away while it was read); contained, the read is refused (fault #{n})"
        ));
    }
}

/// `len` bytes at `addr`, or why not.
pub fn read(addr: usize, len: usize) -> Result<Vec<u8>, Refused> {
    let mut v = vec![0u8; len];
    read_into(addr, &mut v)?;
    Ok(v)
}

/// The plain values a typed read may produce: every bit pattern is a valid value.
pub trait Plain: Copy + Default {}
impl Plain for u8 {}
impl Plain for u16 {}
impl Plain for u32 {}
impl Plain for i32 {}
impl Plain for u64 {}
impl Plain for usize {}
impl Plain for [u32; 6] {}
impl Plain for [u64; 3] {}

/// One value of type `T` at `addr`, which must be aligned for `T` (the engine's fields are).
pub fn read_value<T: Plain>(addr: usize) -> Option<T> {
    let mut v = T::default();
    // SAFETY: `T` is plain (any bytes are a value); the slice covers exactly `v`.
    let bytes = unsafe { core::slice::from_raw_parts_mut(&mut v as *mut T as *mut u8, core::mem::size_of::<T>()) };
    read_aligned_into(addr, bytes, core::mem::align_of::<T>()).ok().map(|()| v)
}

/// A byte at `addr`.
pub fn byte(addr: usize) -> Option<u8> {
    read_value::<u8>(addr)
}

/// A 64-bit word at `addr`, at any alignment (a field of an object the caller does not know to be aligned, such as a command in a
/// test's byte buffer; a pointer the word holds is checked when it is followed).
pub fn word(addr: usize) -> Option<u64> {
    let mut b = [0u8; 8];
    read_into(addr, &mut b).ok().map(|()| u64::from_le_bytes(b))
}

#[cfg(target_arch = "x86_64")]
mod copy {
    use super::*;
    use core::sync::atomic::AtomicU8;
    use windows_sys::Win32::Foundation::{
        EXCEPTION_ACCESS_VIOLATION, EXCEPTION_IN_PAGE_ERROR, STATUS_GUARD_PAGE_VIOLATION,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
    };
    use windows_sys::Win32::System::Memory::VirtualProtect;

    // The leaf routine: `fl3_guarded_copy(dst rcx, src rdx, len r8) -> rax` (1 = copied). It touches no stack and no register the
    // Windows x64 convention asks to keep, so resuming it at `fl3_guarded_copy_fault` after a fault (the handler sets rip there)
    // returns to the caller as from any call: the return address is still at [rsp]. Eight bytes at a time, then the tail.
    core::arch::global_asm!(
        ".text",
        ".p2align 4",
        ".globl fl3_guarded_copy",
        ".globl fl3_guarded_copy_begin",
        "fl3_guarded_copy:",
        "fl3_guarded_copy_begin:",
        "mov r9, r8",
        "shr r9, 3",
        "jz 3f",
        "2:",
        "mov rax, qword ptr [rdx]",
        "mov qword ptr [rcx], rax",
        "add rdx, 8",
        "add rcx, 8",
        "dec r9",
        "jnz 2b",
        "3:",
        "and r8, 7",
        "jz 5f",
        "4:",
        "mov al, byte ptr [rdx]",
        "mov byte ptr [rcx], al",
        "inc rdx",
        "inc rcx",
        "dec r8",
        "jnz 4b",
        "5:",
        "mov eax, 1",
        "ret",
        ".globl fl3_guarded_copy_fault",
        "fl3_guarded_copy_fault:",
        "xor eax, eax",
        "ret",
    );

    extern "C" {
        fn fl3_guarded_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
        static fl3_guarded_copy_begin: u8;
        static fl3_guarded_copy_fault: u8;
    }

    /// The routine's first instruction, taken from a label (a function's address may be a linker thunk in front of it).
    fn start() -> usize {
        core::ptr::addr_of!(fl3_guarded_copy_begin) as usize
    }

    fn fault_exit() -> usize {
        core::ptr::addr_of!(fl3_guarded_copy_fault) as usize
    }

    /// 0: not tried yet, 1: registered, 2: could not be registered.
    static STATE: AtomicU8 = AtomicU8::new(0);

    /// Whether faults in the leaf routine are contained (registers the handler on first use).
    pub fn tolerant() -> bool {
        match STATE.load(Ordering::Acquire) {
            1 => true,
            2 => false,
            _ => {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    // SAFETY: `handler` matches the signature and lives for the process (the module is pinned).
                    let h = unsafe { AddVectoredExceptionHandler(1, Some(handler)) };
                    STATE.store(if h.is_null() { 2 } else { 1 }, Ordering::Release);
                });
                STATE.load(Ordering::Acquire) == 1
            }
        }
    }

    /// The vectored handler: acts only on a memory fault raised by an instruction of the leaf routine, and resumes it at its failure
    /// exit. A guard page the read tripped is armed again (the OS takes the guard off when it raises the exception; a thread's stack
    /// guard left disarmed would break that thread's stack growth later).
    unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the OS passes valid exception and context records.
        unsafe {
            let rec = (*info).ExceptionRecord;
            let ctx = (*info).ContextRecord;
            if rec.is_null() || ctx.is_null() {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            let code = (*rec).ExceptionCode;
            if code != EXCEPTION_ACCESS_VIOLATION
                && code != STATUS_GUARD_PAGE_VIOLATION
                && code != EXCEPTION_IN_PAGE_ERROR
            {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            let ip = (*ctx).Rip as usize;
            if ip < start() || ip >= fault_exit() {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            let addr = if (*rec).NumberParameters >= 2 { (*rec).ExceptionInformation[1] } else { 0 };
            if code == STATUS_GUARD_PAGE_VIOLATION && addr != 0 {
                let mut mbi: MEMORY_BASIC_INFORMATION = core::mem::zeroed();
                if VirtualQuery(addr as *const _, &mut mbi, core::mem::size_of::<MEMORY_BASIC_INFORMATION>()) != 0 {
                    let mut old = 0u32;
                    VirtualProtect(addr as *const _, 1, mbi.Protect | PAGE_GUARD, &mut old);
                }
            }
            FAULTS.fetch_add(1, Ordering::Relaxed);
            LAST_FAULT_ADDR.store(addr, Ordering::Relaxed);
            (*ctx).Rip = fault_exit() as u64;
            (*ctx).Rax = 0;
            EXCEPTION_CONTINUE_EXECUTION
        }
    }

    /// The copy through the leaf routine: false when it faulted.
    ///
    /// # Safety
    /// `dst` is valid for `len` bytes of writes (only the source may fault).
    pub unsafe fn guarded(dst: *mut u8, src: *const u8, len: usize) -> bool {
        unsafe { fl3_guarded_copy(dst, src, len) == 1 }
    }

    /// The leaf routine without layers 1 and 2 (tests: a fault that only the handler can catch).
    #[cfg(test)]
    pub unsafe fn raw(dst: *mut u8, src: *const u8, len: usize) -> bool {
        assert!(tolerant());
        unsafe { guarded(dst, src, len) }
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod copy {
    pub fn tolerant() -> bool {
        false
    }

    pub unsafe fn guarded(_dst: *mut u8, _src: *const u8, _len: usize) -> bool {
        false
    }
}

/// Whether faults are contained in this process (the handler is registered).
pub fn fault_tolerant() -> bool {
    copy::tolerant()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::pages::Pages;
    use windows_sys::Win32::System::Memory::{
        VirtualAlloc, VirtualFree, VirtualProtect, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS,
        PAGE_READONLY, PAGE_READWRITE,
    };

    #[test]
    fn the_address_check_refuses_null_low_misaligned_overflowing_and_kernel_ranges() {
        assert_eq!(check(0, 8, 8), Err(Refused::Null));
        assert_eq!(check(0xfff8, 8, 8), Err(Refused::Null), "the first 64 KB are never mapped");
        assert_eq!(check(0x1_0004, 8, 8), Err(Refused::Misaligned));
        assert_eq!(check(0x1_0004, 8, 1), Ok(()), "a byte buffer has no alignment");
        assert_eq!(check(usize::MAX - 3, 8, 1), Err(Refused::OutOfRange));
        assert_eq!(check(0xffff_8000_0000_0000, 8, 8), Err(Refused::OutOfRange), "a kernel address");
        assert_eq!(check(USER_END - 4, 8, 1), Err(Refused::OutOfRange));
        assert_eq!(check(0x1_0000, 0, 1), Err(Refused::BadLength));
        assert_eq!(check(0x1_0000, MAX_LEN + 1, 1), Err(Refused::BadLength));
        assert_eq!(check(0x7ff6_0000_0000, 0x9c0, 1), Ok(()));
    }

    #[test]
    fn the_region_cache_covers_whole_ranges_only_and_forgets_round_robin() {
        let mut c = RegionCache::new();
        assert!(!c.covers(0x1000, 0x1008));
        c.insert(0x1000, 0x3000);
        assert!(c.covers(0x1000, 0x3000));
        assert!(c.covers(0x2ff8, 0x3000));
        assert!(!c.covers(0x2ff8, 0x3008), "a range that runs past the region");
        c.insert(0x1000, 0x3000);
        for i in 0..CACHE_SLOTS {
            c.insert(0x10_0000 * (i + 1), 0x10_0000 * (i + 1) + 0x1000);
        }
        assert!(!c.covers(0x1000, 0x1008), "the oldest region made room");
        c.clear();
        assert!(!c.covers(0x10_0000, 0x10_0008));
        c.insert(5, 5);
        assert!(!c.covers(5, 5), "an empty region is never kept");
    }

    #[test]
    fn readable_memory_reads_whole_and_a_value_reads_typed() {
        let p = Pages::new(&[PAGE_READWRITE, PAGE_READONLY]);
        let data: Vec<u8> = (0..32u8).collect();
        p.fill(0xff0, &data);
        assert_eq!(read(p.at(0xff0) as usize, 32).unwrap(), data, "across two readable pages");
        assert_eq!(read_value::<u64>(p.at(0xff8) as usize), Some(u64::from_le_bytes(data[8..16].try_into().unwrap())));
        assert_eq!(read_value::<u8>(p.at(0xff3) as usize), Some(3));
        assert_eq!(read_value::<u64>(p.at(0xff4) as usize), None, "misaligned");
        assert_eq!(read_value::<u64>(0), None);
    }

    #[test]
    fn memory_that_is_not_mapped_is_refused_without_a_fault() {
        let p = Pages::new(&[PAGE_READWRITE, PAGE_NOACCESS]);
        let before = stats().faults;
        assert_eq!(read(p.at(0x1000) as usize, 8), Err(Refused::NotMapped), "a no-access page");
        assert_eq!(read(p.at(0xff8) as usize, 16), Err(Refused::NotMapped), "running into it");
        // reserved, never committed
        let r = unsafe { VirtualAlloc(core::ptr::null(), 0x1000, MEM_RESERVE, PAGE_NOACCESS) } as usize;
        assert_ne!(r, 0);
        assert_eq!(read(r, 8), Err(Refused::NotMapped));
        unsafe { VirtualFree(r as *mut _, 0, MEM_RELEASE) };
        assert_eq!(stats().faults, before, "the OS said no: nothing was touched");
    }

    #[test]
    fn a_region_that_goes_away_after_it_was_cached_faults_and_the_fault_is_contained() {
        assert!(fault_tolerant(), "the handler is registered in a test process");
        let base = unsafe { VirtualAlloc(core::ptr::null(), 0x2000, MEM_RESERVE | 0x1000, PAGE_READWRITE) } as usize;
        assert_ne!(base, 0);
        unsafe { core::ptr::write_bytes(base as *mut u8, 7, 0x2000) };
        assert_eq!(read(base, 8).unwrap(), [7; 8], "read once: the region is cached now");
        // the engine frees the block and the heap decommits the page between our question and the read
        assert_ne!(unsafe { VirtualFree(base as *mut _, 0x1000, MEM_DECOMMIT) }, 0);
        let before = stats().faults;
        assert_eq!(read(base, 8), Err(Refused::Fault), "a fault, contained");
        assert_eq!(stats().faults, before + 1);
        assert_eq!(stats().last_fault, base);
        // the thread's cache was cleared: the next read asks the OS and is refused without a fault
        assert_eq!(read(base, 8), Err(Refused::NotMapped));
        assert_eq!(stats().faults, before + 1);
        assert_eq!(read(base + 0x1000, 8).unwrap(), [7; 8], "the page still committed reads");
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
    }

    #[test]
    fn a_held_command_whose_memory_went_away_is_a_refusal_of_that_build_not_a_crash() {
        use crate::dump::{LiveMem, Mem, PAYLOAD_SIZE};
        let base = unsafe { VirtualAlloc(core::ptr::null(), 0x1000, MEM_RESERVE | 0x1000, PAGE_READWRITE) } as usize;
        assert_ne!(base, 0);
        assert!(LiveMem.read(base, PAYLOAD_SIZE).is_some(), "cached as readable");
        assert_ne!(unsafe { VirtualFree(base as *mut _, 0x1000, MEM_DECOMMIT) }, 0);
        // the inject path's first look at a command (and the capture's flags) read through the same reader
        let why = crate::inject::is_proposal(&LiveMem, base).unwrap_err();
        assert!(why.contains("not readable"), "{why}");
        assert_eq!(unsafe { crate::engine::build_flags(base) }, None);
        assert_eq!(unsafe { crate::engine::payload_of(base as *const u8) }, None);
        unsafe { VirtualFree(base as *mut _, 0, MEM_RELEASE) };
    }

    #[test]
    fn a_page_made_inaccessible_after_it_was_cached_faults_contained() {
        let p = Pages::new(&[PAGE_READWRITE]);
        p.fill(0, &[9; 16]);
        assert_eq!(read(p.at(0) as usize, 16).unwrap(), [9; 16]);
        let mut old = 0u32;
        assert_ne!(unsafe { VirtualProtect(p.at(0) as *const _, 0x1000, PAGE_NOACCESS, &mut old) }, 0);
        assert_eq!(read(p.at(0) as usize, 16), Err(Refused::Fault));
    }

    #[test]
    fn a_guard_page_hit_by_a_stale_cache_entry_is_refused_and_armed_again() {
        let p = Pages::new(&[PAGE_READWRITE]);
        assert!(read(p.at(0) as usize, 8).is_ok());
        let mut old = 0u32;
        assert_ne!(unsafe { VirtualProtect(p.at(0) as *const _, 0x1000, PAGE_READWRITE | 0x100, &mut old) }, 0);
        assert_eq!(read(p.at(0) as usize, 8), Err(Refused::Fault));
        assert_eq!(p.protection(0), PAGE_READWRITE | 0x100, "the guard is back as the page's owner set it");
        assert_eq!(read(p.at(0) as usize, 8), Err(Refused::NotMapped), "now the OS is asked, and says guard");
    }

    #[test]
    fn only_faults_inside_the_leaf_routine_are_handled() {
        let mut out = [0u8; 8];
        // the raw routine on an address no layer checked: only the handler stands between this and a crash of the test process
        assert!(!unsafe { copy::raw(out.as_mut_ptr(), 0x10 as *const u8, 8) });
        assert!(!unsafe { copy::raw(out.as_mut_ptr(), 0xffff_8000_0000_0000u64 as *const u8, 8) }, "a kernel address");
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        let mut out = [0u8; 11];
        assert!(unsafe { copy::raw(out.as_mut_ptr(), src.as_ptr(), 11) });
        assert_eq!(out, src, "eight at a time, then the tail");
    }

    /// The cost (review idea 12: keep the capture's 2-12 ms). Prints the numbers with `--nocapture`.
    #[test]
    fn a_cached_read_costs_less_than_the_os_question_it_replaces() {
        let p = Pages::new(&[PAGE_READWRITE; 4]);
        let n = 100_000usize;
        let at = |i: usize| p.at((i * 24) % 0x3fe0) as usize;
        let t = std::time::Instant::now();
        for i in 0..n {
            std::hint::black_box(unsafe { crate::mem::read(at(i) as *const u8, 24) }.unwrap());
        }
        let old = t.elapsed().as_nanos() as f64 / n as f64;
        let t = std::time::Instant::now();
        for i in 0..n {
            std::hint::black_box(read(at(i), 24).unwrap());
        }
        let new = t.elapsed().as_nanos() as f64 / n as f64;
        let mut v = 0u64;
        let t = std::time::Instant::now();
        for i in 0..n {
            v ^= read_value::<u64>(at(i) & !7).unwrap();
        }
        std::hint::black_box(v);
        let typed = t.elapsed().as_nanos() as f64 / n as f64;
        let m = 2_000usize;
        let t = std::time::Instant::now();
        for _ in 0..m {
            assert!(!unsafe { copy::raw([0u8; 8].as_mut_ptr(), 0x10 as *const u8, 8) });
        }
        let fault = t.elapsed().as_nanos() as f64 / m as f64;
        println!(
            "memguard cost per 24-byte read: VirtualQuery+copy {old:.0} ns, guarded (cached) {new:.0} ns, typed u64 {typed:.0} ns; a contained fault {fault:.0} ns"
        );
        assert!(new < old, "the cached read must not cost more than the read it replaces ({new:.0} vs {old:.0} ns)");
    }
}
