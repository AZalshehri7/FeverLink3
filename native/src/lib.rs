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
pub mod detour;
pub mod pe;
pub mod scan;

#[cfg(windows)]
pub mod log;
#[cfg(windows)]
pub mod proxy;

/// Environment variable the launcher sets on the game process. Absent: the module is a plain proxy.
pub const SESSION_DIR_VAR: &str = "FEVERLINK3_SESSION_DIR";

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
            if let Some(dir) = std::env::var_os(SESSION_DIR_VAR) {
                log::init(std::path::PathBuf::from(dir).join("native.log"));
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
            }
        }
        TRUE
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
