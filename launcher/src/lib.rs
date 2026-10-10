//! FeverLink3 launcher: the Windows platform layer.
//!
//! What lives here is everything the launcher needs from the machine it runs on and nothing else:
//!
//! - [`steam`]: where Steam, the game and the per-user game folders are (registry, `libraryfolders.vdf`).
//! - [`install`]: the game-side footprint (`docs/ARCHITECTURE.md` section 11) installed and removed idempotently,
//!   with a report of what was done, what was skipped and why.
//! - [`gamelink`]: the file link with one game instance: a per-session folder, `to_game.log` written by the
//!   launcher, `from_game.log` tailed by it, every line carrying a sequence number so history is never dispatched
//!   twice.
//! - [`update`]: update decision, signature and hash checks and the file-swap plan, over bytes (no network yet).
//! - [`process`]: starting `TransportFever3.exe` with the session environment.
//!
//! - [`net`], [`session`], [`luatext`]: TCP transport, the session threads that wire the core crate's state
//!   machines to sockets and to the game's files, and the payload translation between them.
//! - [`selftest`]: a host and two players with fake games over loopback.
//!
//! No UI: the window comes in a later stage. The protocol lives in the core crate. Every path and text
//! transformation is a pure function over its inputs so the tests run on a machine without Steam or the game.

pub mod gamelink;
pub mod install;
pub mod live;
pub mod luatext;
pub mod net;
pub mod process;
pub mod resync;
pub mod selftest;
pub mod session;
pub mod steam;
pub mod steam_transport;
pub mod steamnet;
pub mod update;

/// Steam app id of Transport Fever 3.
pub const APP_ID: &str = "3493540";

/// Folder name of the game-script mod under the user mods folder (`mods/feverlink3_1/`).
pub const MOD_ID: &str = "feverlink3_1";

/// Byte string the native module carries, by which the launcher tells its `winhttp.dll` from anyone else's.
/// The native crate is named `feverlink3_native`, and the PE export directory of the DLL keeps that internal
/// name even after the file is renamed `winhttp.dll` (checked on a stripped release build).
pub const NATIVE_MARKER: &[u8] = b"feverlink3_native";

/// Name of the game executable.
pub const GAME_EXE: &str = "TransportFever3.exe";
