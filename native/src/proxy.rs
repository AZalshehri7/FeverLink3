//! The `winhttp.dll` slot: every function the game imports is implemented here and relays to the system's own
//! `winhttp.dll` in the Windows system folder. The game's networking is untouched.
//!
//! The list below is the import table of the current game builds. A game update that imports one more WinHTTP
//! function would fail to load with this module in place, so `tools/` must check the import list against every new
//! build and this list must grow with it (the remaining WinHTTP exports can be added at no cost).

use core::ffi::c_void;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, Ordering};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

type Hinternet = *mut c_void;
type Dword = u32;
type Bool = i32;
type Word = u16;
type Lpcwstr = *const u16;
type DwordPtr = usize;

static REAL: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

/// The system `winhttp.dll`, loaded by full path so that this module is never found instead of it.
fn real_module() -> *mut c_void {
    let m = REAL.load(Ordering::Acquire);
    if !m.is_null() {
        return m;
    }
    let mut path = [0u16; 300];
    // SAFETY: the buffer is 300 wide chars; GetSystemDirectoryW writes at most 260 and returns the length.
    let n = unsafe { GetSystemDirectoryW(path.as_mut_ptr(), 260) } as usize;
    if n == 0 || n >= 260 {
        return null_mut();
    }
    let tail: Vec<u16> = "\\winhttp.dll\0".encode_utf16().collect();
    path[n..n + tail.len()].copy_from_slice(&tail);
    // SAFETY: `path` is a NUL-terminated wide string.
    let m = unsafe { LoadLibraryW(path.as_ptr()) };
    if !m.is_null() {
        REAL.store(m, Ordering::Release);
    }
    m
}

fn real_proc(name: &'static [u8]) -> *mut c_void {
    let m = real_module();
    if m.is_null() {
        return null_mut();
    }
    // SAFETY: `name` is a NUL-terminated ASCII string (the macro appends the NUL).
    match unsafe { GetProcAddress(m, name.as_ptr()) } {
        Some(f) => f as *mut c_void,
        None => null_mut(),
    }
}

macro_rules! relay {
    ($name:ident ( $($arg:ident : $ty:ty),* ) -> $ret:ty, $fail:expr) => {
        #[no_mangle]
        pub unsafe extern "system" fn $name($($arg: $ty),*) -> $ret {
            type F = unsafe extern "system" fn($($ty),*) -> $ret;
            static CACHE: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
            let mut p = CACHE.load(Ordering::Acquire);
            if p.is_null() {
                p = real_proc(concat!(stringify!($name), "\0").as_bytes());
                if p.is_null() {
                    return $fail;
                }
                CACHE.store(p, Ordering::Release);
            }
            // SAFETY: `p` is the address of the system function of this exact name and signature.
            let f: F = unsafe { core::mem::transmute(p) };
            unsafe { f($($arg),*) }
        }
    };
}

relay!(WinHttpOpen(agent: Lpcwstr, access: Dword, proxy: Lpcwstr, bypass: Lpcwstr, flags: Dword) -> Hinternet, null_mut());
relay!(WinHttpConnect(session: Hinternet, server: Lpcwstr, port: Word, reserved: Dword) -> Hinternet, null_mut());
relay!(WinHttpOpenRequest(connect: Hinternet, verb: Lpcwstr, object: Lpcwstr, version: Lpcwstr, referrer: Lpcwstr, accept: *mut Lpcwstr, flags: Dword) -> Hinternet, null_mut());
relay!(WinHttpSetOption(h: Hinternet, option: Dword, buffer: *mut c_void, len: Dword) -> Bool, 0);
relay!(WinHttpReadData(h: Hinternet, buffer: *mut c_void, to_read: Dword, read: *mut Dword) -> Bool, 0);
relay!(WinHttpSetStatusCallback(h: Hinternet, callback: *mut c_void, flags: Dword, reserved: DwordPtr) -> *mut c_void, usize::MAX as *mut c_void);
relay!(WinHttpCloseHandle(h: Hinternet) -> Bool, 0);
relay!(WinHttpWriteData(h: Hinternet, buffer: *const c_void, to_write: Dword, written: *mut Dword) -> Bool, 0);
relay!(WinHttpQueryDataAvailable(h: Hinternet, available: *mut Dword) -> Bool, 0);
relay!(WinHttpSetTimeouts(h: Hinternet, resolve: i32, connect: i32, send: i32, receive: i32) -> Bool, 0);
relay!(WinHttpQueryHeaders(h: Hinternet, level: Dword, name: Lpcwstr, buffer: *mut c_void, len: *mut Dword, index: *mut Dword) -> Bool, 0);
relay!(WinHttpAddRequestHeaders(h: Hinternet, headers: Lpcwstr, len: Dword, modifiers: Dword) -> Bool, 0);
relay!(WinHttpSendRequest(h: Hinternet, headers: Lpcwstr, headers_len: Dword, optional: *mut c_void, optional_len: Dword, total_len: Dword, context: DwordPtr) -> Bool, 0);
relay!(WinHttpReceiveResponse(h: Hinternet, reserved: *mut c_void) -> Bool, 0);
