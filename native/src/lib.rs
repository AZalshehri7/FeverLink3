//! FeverLink3 native module.
//!
//! Built as `winhttp.dll` and placed in the game folder, where Transport Fever 3 loads it itself because it
//! imports `WINHTTP.dll` and Windows searches the application folder first. Every WinHTTP function is passed
//! unchanged to the system library ([`proxy`]). The module does nothing else unless the launcher started the
//! game with `FEVERLINK3_SESSION_DIR` set; then it recognises the game build ([`build_table`]), installs the hooks
//! that let construction commands run at the same game time on every game, and talks to the Lua mod through
//! files in that folder.
//!
//! The pure parts (PE header parsing, signature scanning, detour byte maths, the build table) have no Windows
//! dependency and are unit-tested with `cargo test`. The unsafe parts are confined to [`proxy`], [`detour::install`]
//! and the entry point.

#![cfg_attr(not(test), deny(unsafe_op_in_unsafe_fn))]

pub mod build_table;
pub mod counters;
pub mod detour;
pub mod hold;
pub mod link;
pub mod pe;
pub mod plan;
pub mod scan;

#[cfg(windows)]
pub mod log;
#[cfg(windows)]
pub mod proxy;

/// Environment variable the launcher sets on the game process. Absent: the module is a plain proxy.
pub const SESSION_DIR_VAR: &str = "FEVERLINK3_SESSION_DIR";

/// Shared by the tests of the modules that touch files.
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh folder under the system temp folder, removed when dropped.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> TempDir {
            static N: AtomicU32 = AtomicU32::new(0);
            let name = format!("fl3-native-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
            let dir = std::env::temp_dir().join(name);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create a temp folder");
            TempDir(dir)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(windows)]
mod entry {
    use super::*;
    use core::ffi::c_void;
    use windows_sys::Win32::Foundation::{BOOL, HMODULE, TRUE};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

    /// Called by the loader under the loader lock: only cheap, non-blocking work belongs here. Hooks that need
    /// the game to be further along are installed from a thread started here (later stage).
    #[no_mangle]
    pub extern "system" fn DllMain(_module: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
        if reason == DLL_PROCESS_ATTACH {
            let launched = std::env::var_os(SESSION_DIR_VAR);
            let live = launched.is_none() && is_game_process();
            let log_path = match &launched {
                Some(dir) => Some(std::path::PathBuf::from(dir).join("native.log")),
                None if live => {
                    feverlink3_launcher::live::live_dir().and_then(|d| d.parent().map(|p| p.join("native.log")))
                }
                None => None,
            };
            if let Some(path) = log_path {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                log::init(path);
                log::line(&format!("FeverLink3 native {} loaded", env!("CARGO_PKG_VERSION")));
                match game_build() {
                    Some(ts) => match build_table::select(ts) {
                        Some(b) => log::line(&format!("game build {} (timestamp {ts:#x}): hooks available", b.name)),
                        None => {
                            log::line(&format!("unknown game build (timestamp {ts:#x}): hooks off, mod runs degraded"))
                        }
                    },
                    None => log::line("could not read the game's PE header"),
                }
                if live {
                    // not started by the launcher executable: the session runs in this process, driven by the mod's window
                    log::line(if feverlink3_launcher::live::spawn().is_some() {
                        "live session controller started"
                    } else {
                        "live session controller could not start"
                    });
                }
            }
        }
        TRUE
    }

    /// True when the host executable is the game: the proxy sits in the game folder, but be exact.
    fn is_game_process() -> bool {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.eq_ignore_ascii_case(feverlink3_launcher::GAME_EXE)))
            .unwrap_or(false)
    }

    /// The link timestamp of the running executable, which identifies the game build.
    fn game_build() -> Option<u32> {
        // SAFETY: GetModuleHandleW(null) is the base of the executable's image, whose headers are mapped and
        // readable for the whole process lifetime; `pe::read_headers` only touches the header pages.
        unsafe {
            let base = GetModuleHandleW(core::ptr::null()) as *const u8;
            if base.is_null() {
                return None;
            }
            pe::read_headers(base).map(|h| h.timestamp)
        }
    }
}
