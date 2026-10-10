//! Puts text on the Windows clipboard, for the window's "Copy code" button. The game's text fields cannot copy
//! (Ctrl+C does nothing in them), so the in-game module does it for the player. Nothing is ever read from the
//! clipboard here; pasting works through the game's own Ctrl+V.

/// Copies `text` to the clipboard as Unicode text. `false` when the clipboard could not be opened (another program
/// holds it) or this is not Windows.
pub fn set_text(text: &str) -> bool {
    imp::set_text(text)
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::thread;
    use std::time::Duration;

    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalLock(mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(mem: *mut c_void) -> i32;
        fn GlobalFree(mem: *mut c_void) -> *mut c_void;
    }

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;
    /// Another program may hold the clipboard for a moment; ask a few times.
    const OPEN_ATTEMPTS: u32 = 10;

    pub fn set_text(text: &str) -> bool {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = wide.len() * std::mem::size_of::<u16>();
        // SAFETY: plain Win32 calls. The block is allocated moveable, locked only while it is filled, and handed to
        // the clipboard (which then owns it) only when SetClipboardData succeeds; otherwise it is freed here.
        unsafe {
            let mut opened = false;
            for _ in 0..OPEN_ATTEMPTS {
                if OpenClipboard(std::ptr::null_mut()) != 0 {
                    opened = true;
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            if !opened {
                return false;
            }
            let mut done = false;
            if EmptyClipboard() != 0 {
                let mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
                if !mem.is_null() {
                    let dst = GlobalLock(mem).cast::<u16>();
                    if dst.is_null() {
                        GlobalFree(mem);
                    } else {
                        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
                        GlobalUnlock(mem);
                        if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
                            GlobalFree(mem);
                        } else {
                            done = true;
                        }
                    }
                }
            }
            CloseClipboard();
            done
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn set_text(_text: &str) -> bool {
        false
    }
}
