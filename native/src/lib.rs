//! FeverLink3 native module.
//!
//! Built as `winhttp.dll` and placed in the game folder, where Transport Fever 3 loads it itself because it
//! imports `WINHTTP.dll` and Windows searches the application folder first. Every WinHTTP function is passed
//! unchanged to the system library ([`proxy`]). The module does nothing else unless the launcher started the
//! game with `FEVERLINK3_SESSION_DIR` set (or the game itself hosts the session); then it recognises the game
//! build ([`build_table`]) and, for the pieces that are on by default (`FEVERLINK3_HOOKS`, `FEVERLINK3_HOLD`,
//! `FEVERLINK3_POOLPATCH`, `FEVERLINK3_LUA_SEED`; `=0` turns one off, see [`plan`]), installs the pieces that let construction commands
//! run at the same game time on every game ([`hooks`]), and talks to the Lua mod through two files in the
//! session folder ([`link`]). `docs/NATIVE_LINK.md` is the design.
//!
//! The pure parts (PE header parsing, signature scanning, detour byte maths, the build table, the control-file
//! codec, the hold/release state machine, the counters, the install plan) have no Windows dependency and are
//! unit-tested with `cargo test`. The unsafe parts are confined to [`proxy`], [`mem`], [`detour`], [`patch`],
//! [`engine`], [`hooks`] and the entry point; they are tested on fake game functions in executable memory.

#![cfg_attr(not(test), deny(unsafe_op_in_unsafe_fn))]

pub mod build_table;
pub mod cmd_patch;
pub mod counters;
pub mod detour;
pub mod dump;
pub mod failsafe;
pub mod freeorder;
pub mod growth;
pub mod hold;
pub mod hookwatch;
pub mod idorder;
pub mod ids;
pub mod inject;
pub mod input;
pub mod link;
pub mod native_copy;
pub mod pace;
pub mod patch;
pub mod pe;
pub mod plan;
pub mod portable;
pub mod rebuild;
pub mod scan;
pub mod search;
pub mod townseed;

#[cfg(windows)]
pub mod engine;
#[cfg(windows)]
pub mod hooks;
#[cfg(windows)]
pub mod log;
#[cfg(windows)]
pub mod mem;
#[cfg(windows)]
pub mod memguard;
#[cfg(windows)]
pub mod proxy;
#[cfg(windows)]
pub mod refund;
#[cfg(windows)]
pub mod runtime;
#[cfg(all(windows, target_arch = "x86_64"))]
pub mod selftest;

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

    /// Called by the loader under the loader lock: only cheap, non-blocking work belongs here. The hooks are
    /// written here on purpose: the process is still starting, so no thread is inside the functions being patched,
    /// which no later moment can promise. What waits (the control link) is done by a thread started from here.
    #[no_mangle]
    pub extern "system" fn DllMain(_module: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
        // Nothing may unwind into the loader: a panic is stopped here (every profile unwinds, root Cargo.toml), after
        // the panic hook installed by `attach` has written it to the log. `attach` never calls into the game, which
        // is what makes `catch_unwind` safe here (see `log::guarded`).
        let _ = std::panic::catch_unwind(|| attach(reason));
        TRUE
    }

    fn attach(reason: u32) {
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
                log::install_panic_hook();
                log::line(&format!("FeverLink3 native {} loaded", env!("CARGO_PKG_VERSION")));
                if live {
                    // First, while the game has not run any code of its own and so has read no mod: finish (or undo)
                    // the update an earlier run staged, then check for a new one on a thread of its own
                    // (launcher/src/autoupdate.rs: renames only here, never the network).
                    let game = std::env::current_exe().ok().and_then(|p| p.parent().map(std::path::Path::to_path_buf));
                    if let (Some(game), Some(settings)) = (game, feverlink3_launcher::autoupdate::settings_path()) {
                        feverlink3_launcher::autoupdate::start(game, settings, log::line);
                    }
                }
                match game_build() {
                    Some(ts) => match build_table::select(ts).map(|b| (b, false)).or_else(|| searched_build(ts)) {
                        Some((b, searched)) => {
                            if searched {
                                // not in the table, but every site proven by the address tool's proof (search.rs); the window
                                // says it is not yet confirmed, and a join compares the found addresses
                                let hash = search::row_hash(b);
                                log::line(&format!(
                                    "game build {} (timestamp {ts:#x}) is not in the build table: FOUND BY SEARCH (proof of                                      build {}), not yet confirmed; addresses {hash}; hooks available",
                                    b.name,
                                    search::Proof::parse(search::PROOF).map(|p| p.timestamp).unwrap_or(0)
                                ));
                                if live {
                                    feverlink3_launcher::live::set_searched_game(b.name.to_owned(), hash);
                                }
                            } else {
                                log::line(&format!("game build {} (timestamp {ts:#x}): hooks available", b.name));
                            }
                            // hooks, hold, pool patch and seed pin are on unless their variable says 0; done here, in the quiet
                            // moment before the game runs the code that is patched
                            let link = match &launched {
                                Some(dir) => Some(hooks::Link { dir: std::path::PathBuf::from(dir), live: false }),
                                None if live => {
                                    feverlink3_launcher::live::live_dir().map(|dir| hooks::Link { dir, live: true })
                                }
                                None => None,
                            };
                            hooks::attach_from_env(b, link, log::line);
                        }
                        None => {
                            // Friend session 2026-10-23: the game had updated itself to 40423 and both games ran like this,
                            // drifting, with nobody told. Now the session controller refuses to host, says so on the
                            // window's start screen, and a join it sends makes the host refuse it in words.
                            let build = game_build_number().unwrap_or_else(|| format!("{ts:#x}"));
                            log::line(&format!(
                                "UNSUPPORTED GAME BUILD {build} (timestamp {ts:#x}): this FeverLink3 knows only {}; hooks off, \
                                 hosting refused, joins refused by the host. An update of FeverLink3 is needed.",
                                build_table::BUILDS.iter().map(|b| b.name).collect::<Vec<_>>().join(", ")
                            ));
                            if live {
                                feverlink3_launcher::live::set_unsupported_game(build);
                            }
                        }
                    },
                    None => log::line("could not read the game's PE header"),
                }
                if live {
                    // the free-id queue's checksum part: the session controller runs in this process and adds it to every report
                    feverlink3_launcher::session::set_checksum_part(ids::checksum_part);
                    // not started by the launcher executable: the session runs in this process, driven by the mod's window
                    log::line(if feverlink3_launcher::live::spawn().is_some() {
                        "live session controller started"
                    } else {
                        "live session controller could not start"
                    });
                }
            }
        }
    }

    /// True when the host executable is the game: the proxy sits in the game folder, but be exact.
    fn is_game_process() -> bool {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.eq_ignore_ascii_case(feverlink3_launcher::GAME_EXE)))
            .unwrap_or(false)
    }

    /// The game's own build number, read from its mapped `.rdata` (only for the message about an unknown build).
    fn game_build_number() -> Option<String> {
        // SAFETY: as in `game_build`: the image's headers, then its `.rdata` section, which the loader mapped readable
        // (`VirtualSize` bytes from its RVA) for the whole process lifetime.
        unsafe {
            let base = GetModuleHandleW(core::ptr::null()) as *const u8;
            if base.is_null() {
                return None;
            }
            let rdata = pe::read_headers(base)?.section(".rdata")?.clone();
            pe::build_number(core::slice::from_raw_parts(base.add(rdata.rva as usize), rdata.size as usize))
        }
    }

    /// The row of a build the table does not know, found by repeating the address tool's proof on the mapped image
    /// (`search.rs`); `None` when anything is not proven, or `FEVERLINK3_BUILD_SEARCH=0`. Runs here, under the loader lock,
    /// because the hooks must be written before the game runs: a full search once per build, then the cache is checked.
    fn searched_build(ts: u32) -> Option<(&'static build_table::BuildAddrs, bool)> {
        if std::env::var("FEVERLINK3_BUILD_SEARCH").is_ok_and(|v| v == "0") {
            log::line("build search off (FEVERLINK3_BUILD_SEARCH=0)");
            return None;
        }
        // SAFETY: as in `game_build`: the image's headers; the sections are then copied with the guarded reader
        let (base, headers) = unsafe {
            let base = GetModuleHandleW(core::ptr::null()) as usize;
            if base == 0 {
                return None;
            }
            (base, pe::read_headers(base as *const u8)?)
        };
        let img = search::Image::from_sections(&headers, base as u64, |s| {
            let mut v = vec![0u8; s.size as usize];
            for (k, chunk) in v.chunks_mut(16 << 20).enumerate() {
                memguard::read_into(base + s.rva as usize + (k << 24), chunk).ok()?;
            }
            Some(v)
        })?;
        debug_assert_eq!(img.timestamp, ts);
        let dir = std::env::var_os("LOCALAPPDATA").map(|b| std::path::PathBuf::from(b).join("FeverLink3"));
        let found = search::find(&img, dir.as_deref(), log::line);
        if found.is_none() {
            log::line("build search: not every site is proven; the build stays unsupported");
        }
        found.map(|b| (b, true))
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
