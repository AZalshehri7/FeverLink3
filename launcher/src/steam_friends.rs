//! Steam invites through Steam's own interface: rich presence, the overlay's invite dialog, the "join requested"
//! callback and the launch command line. The admission that follows is `steam_admit.rs`; the design is
//! `docs/STEAM_TRANSPORT.md`, section 14, and the token `docs/PROTOCOL.md`, "Steam invites".
//!
//! Every call goes through the flat API of the game's own, already loaded `steam_api64.dll`, as the transport and
//! `steam_persona.rs` do; the session runs inside the game process, so every call is made under TF3's app id. The calls
//! and layouts, from the Steamworks documentation (partner.steamgames.com/doc/api/ISteamFriends, ISteamApps) and the
//! SDK headers (steam_api_flat.h, steam_api.h `CCallbackBase`, isteamfriends.h):
//!
//! - `bool SteamAPI_ISteamFriends_SetRichPresence(ISteamFriends*, const char *pchKey, const char *pchValue)`: key at
//!   most 63 bytes (`k_cchMaxRichPresenceKeyLength` 64), value at most 255 (`k_cchMaxRichPresenceValueLength` 256), at
//!   most 20 keys; "an empty string or NULL removes the key". Keys used: `connect` ("the command-line for how a friend
//!   can connect to a game": enables "Join game" and the friends list's "Invite to game") and `status` ("will show up
//!   in the 'view game info' dialog"). `steam_display` is not used: it must name a localization token the *app* has
//!   uploaded, and TF3's tokens are Urban Games'. We never call `ClearRichPresence`, which would also clear TF3's own
//!   keys.
//! - `void SteamAPI_ISteamFriends_ActivateGameOverlayInviteDialogConnectString(ISteamFriends*, const char*)`: the
//!   overlay's friend picker; the picked friends receive an invite carrying the string. Steam does **not** tell the game
//!   whom the player picked (no callback, no result), so the host cannot list the invited accounts: the invite carries
//!   an invite ticket instead (`feverlink3_core::admission`), and everyone else is a join request in the host's window.
//!   This function is in the SDK's headers and the game's `steam_api64.dll` exports it, but Valve's ISteamFriends page
//!   does not describe it (2026-10-19); the lobby form `ActivateGameOverlayInviteDialog(CSteamID)` is documented and
//!   not usable without a lobby.
//! - `GameRichPresenceJoinRequested_t`, callback id `k_iSteamFriendsCallbacks + 37` = 337: "Called when the user tries
//!   to join a game from their friends list" and after accepting an invite while the game runs; `CSteamID
//!   m_steamIDFriend` (8 bytes; invalid when not joined through a friend) then `char m_rgchConnect[256]`, the `connect`
//!   value; Windows packs callbacks at 8 (`VALVE_CALLBACK_PACK_LARGE`), so 264 bytes.
//! - `void SteamAPI_RegisterCallback(CCallbackBase*, int iCallback)`: TF3 pumps `SteamAPI_RunCallbacks` itself, so a
//!   general callback reaches us only through a registered `CCallbackBase` object, which Steam's dispatcher runs on
//!   TF3's thread. `CCallbackBase` is a vtable pointer, `uint8 m_nCallbackFlags`, `int m_iCallback` (16 bytes), with
//!   the virtual functions `Run(void*)`, `Run(void*, bool, SteamAPICall_t)` and `GetCallbackSizeBytes()`.
//! - `int SteamAPI_ISteamApps_GetLaunchCommandLine(ISteamApps*, char*, int)`: the command line of a `steam://run`
//!   launch. "In order for rich presence joins to go through this and not be placed on the OS command line, you must
//!   enable 'Use launch command line'", a setting of the app (Urban Games'), so the OS command line is read too.

use std::sync::Mutex;

use crate::steam_persona;

/// `k_iSteamFriendsCallbacks` (300) + 37.
pub const JOIN_REQUESTED_CALLBACK: i32 = 337;
/// `sizeof(GameRichPresenceJoinRequested_t)` on Windows: `CSteamID` (8) and `char[256]`, packed at 8.
pub const JOIN_REQUESTED_SIZE: usize = 8 + 256;
/// `k_cchMaxRichPresenceKeyLength` (with the NUL).
pub const MAX_KEY_BYTES: usize = 64;
/// `k_cchMaxRichPresenceValueLength` (with the NUL).
pub const MAX_VALUE_BYTES: usize = 256;
/// `k_cchMaxRichPresenceKeys`.
pub const MAX_KEYS: usize = 20;
/// Join requests kept between two looks of the controller; more are dropped (the newest count).
const INBOX_LIMIT: usize = 8;

/// The rich presence key Steam's "Join game" and "Invite to game" use.
pub const KEY_CONNECT: &str = "connect";
/// The rich presence key shown in "view game info".
pub const KEY_STATUS: &str = "status";

/// Where a join request came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinSource {
    /// Callback 337: the game was running when the player clicked "Join game" or accepted an invite.
    Running,
    /// Steam started the game with the connect string on its command line.
    CommandLine,
}

impl JoinSource {
    pub fn text(self) -> &'static str {
        match self {
            JoinSource::Running => "Steam (game running)",
            JoinSource::CommandLine => "the command line (game started by Steam)",
        }
    }
}

/// A "join" from Steam: the connect string as Steam handed it over, and the friend it came through when Steam said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRequest {
    pub friend: Option<u64>,
    pub connect: String,
    pub source: JoinSource,
}

/// What the controller needs of Steam's friends interface. The game's Steam in the game process, a model of it in the
/// tests ([`fake::FakeFriends`]).
pub trait Friends: Send {
    /// Steam is initialised and the interfaces were found.
    fn ready(&self) -> bool;
    /// `SetRichPresence`; an empty value removes the key. False when Steam refused or is not there.
    fn set_rich_presence(&self, key: &str, value: &str) -> bool;
    /// `ActivateGameOverlayInviteDialogConnectString`. An error when Steam or the function is not there; success says
    /// nothing about whether the overlay showed (it may be disabled) or whom the player invited.
    fn open_invite_dialog(&self, connect: &str) -> Result<(), String>;
    /// `GetFriendPersonaName`, cleaned like the player's own name.
    fn friend_name(&self, id: u64) -> Option<String>;
    /// Join requests delivered by callback 337 since the last call. Registers the callback first, once Steam is up.
    fn take_join_requests(&self) -> Vec<JoinRequest>;
    /// What the callback registration says, once (for the session log).
    fn take_note(&self) -> Option<String>;
    /// `GetLaunchCommandLine`, `None` when empty or Steam is not up.
    fn launch_command_line(&self) -> Option<String>;
    /// The process's own command line (where Steam puts a connect string on a cold start).
    fn process_command_line(&self) -> String;
}

/// A `GameRichPresenceJoinRequested_t` from its bytes: the friend (0 is "not through a friend") and the connect value up
/// to its NUL.
pub fn parse_join_requested(bytes: &[u8]) -> Option<(Option<u64>, String)> {
    if bytes.len() < JOIN_REQUESTED_SIZE {
        return None;
    }
    let friend = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
    let text = &bytes[8..JOIN_REQUESTED_SIZE];
    let end = text.iter().position(|b| *b == 0).unwrap_or(text.len());
    let connect = String::from_utf8_lossy(&text[..end]).into_owned();
    Some(((friend != 0).then_some(friend), connect))
}

/// Requests the callback delivered, waiting for the controller.
static INBOX: Mutex<Vec<JoinRequest>> = Mutex::new(Vec::new());

fn push_request(request: JoinRequest) {
    let mut inbox = INBOX.lock().unwrap_or_else(|p| p.into_inner());
    if inbox.len() >= INBOX_LIMIT {
        inbox.remove(0);
    }
    inbox.push(request);
}

fn take_requests() -> Vec<JoinRequest> {
    std::mem::take(&mut *INBOX.lock().unwrap_or_else(|p| p.into_inner()))
}

/// The game's Steam (Windows; elsewhere nothing is there).
pub fn native() -> Box<dyn Friends> {
    Box::new(imp::Native::default())
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    type HandleFn = unsafe extern "C" fn() -> i32;
    type FindFn = unsafe extern "C" fn(i32, *const c_char) -> *mut c_void;
    type SetPresenceFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> bool;
    type InviteDialogFn = unsafe extern "C" fn(*mut c_void, *const c_char);
    type FriendNameFn = unsafe extern "C" fn(*mut c_void, u64) -> *const c_char;
    type LaunchLineFn = unsafe extern "C" fn(*mut c_void, *mut c_char, i32) -> i32;
    type RegisterFn = unsafe extern "C" fn(*mut CallbackObject, i32);
    type RunFn = unsafe extern "C" fn(*mut CallbackObject, *mut c_void);
    type SizeFn = unsafe extern "C" fn(*mut CallbackObject) -> i32;

    /// The vtable of `CCallbackBase`. MSVC may order the two `Run` overloads either way, so both slots hold the same
    /// function: under the x64 calling convention both start with (`this`, `pvParam`), and a callback (not a call
    /// result) is run through `Run(void*)` anyway. `GetCallbackSizeBytes` is the third slot in both orders.
    #[repr(C)]
    struct CallbackVtable {
        run_a: RunFn,
        run_b: RunFn,
        size: SizeFn,
    }

    /// `CCallbackBase`: vtable pointer, `uint8 m_nCallbackFlags`, `int m_iCallback`. Steam writes the two fields.
    #[repr(C)]
    pub(super) struct CallbackObject {
        vtable: *const CallbackVtable,
        flags: u8,
        id: i32,
    }

    static VTABLE: CallbackVtable = CallbackVtable { run_a: run, run_b: run, size };

    /// Runs on TF3's thread inside its `SteamAPI_RunCallbacks`: copies the request and returns. No Steam call, no file,
    /// and nothing may unwind into the game.
    unsafe extern "C" fn run(_this: *mut CallbackObject, param: *mut c_void) {
        let _ = std::panic::catch_unwind(|| {
            if param.is_null() {
                return;
            }
            // SAFETY: Steam passes a GameRichPresenceJoinRequested_t, JOIN_REQUESTED_SIZE bytes (the size we report).
            let bytes = unsafe { std::slice::from_raw_parts(param as *const u8, JOIN_REQUESTED_SIZE) };
            if let Some((friend, connect)) = parse_join_requested(bytes) {
                push_request(JoinRequest { friend, connect, source: JoinSource::Running });
            }
        });
    }

    unsafe extern "C" fn size(_this: *mut CallbackObject) -> i32 {
        JOIN_REQUESTED_SIZE as i32
    }

    static REGISTERED: AtomicBool = AtomicBool::new(false);

    /// A function pointer `T` from a NUL-terminated export name.
    ///
    /// # Safety
    /// `module` must be a loaded module and `T` the exact type of the export.
    unsafe fn symbol<T: Copy>(module: *mut c_void, name: &str) -> Option<T> {
        if std::mem::size_of::<T>() != std::mem::size_of::<*mut c_void>() {
            return None;
        }
        // SAFETY: name is NUL-terminated (every caller passes a literal ending in "\0").
        let p = unsafe { GetProcAddress(module, name.as_ptr()) };
        if p.is_null() {
            return None;
        }
        // SAFETY: T is a function pointer type, the same size as a pointer (checked above).
        Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
    }

    #[derive(Default)]
    pub(in crate::steam_friends) struct Native {
        note: Mutex<Option<String>>,
    }

    /// The module, once Steam is initialised (user handle not 0).
    fn module() -> Option<(*mut c_void, i32, FindFn)> {
        // SAFETY: GetModuleHandleA with a NUL-terminated name; the two exports have the flat API's signatures.
        unsafe {
            let module = GetModuleHandleA(c"steam_api64.dll".as_ptr().cast());
            if module.is_null() {
                return None;
            }
            let user: HandleFn = symbol(module, "SteamAPI_GetHSteamUser\0")?;
            let find: FindFn = symbol(module, "SteamInternal_FindOrCreateUserInterface\0")?;
            let handle = user();
            (handle != 0).then_some((module, handle, find))
        }
    }

    /// `ISteamFriends*` and the module.
    fn friends() -> Option<(*mut c_void, *mut c_void)> {
        let (module, handle, find) = module()?;
        // SAFETY: the interface is asked for by its version name once Steam is initialised.
        let iface = unsafe { find(handle, c"SteamFriends017".as_ptr()) };
        (!iface.is_null()).then_some((module, iface))
    }

    impl Native {
        fn register(&self) {
            if REGISTERED.load(Ordering::Acquire) {
                return;
            }
            let Some((module, _, _)) = module() else { return };
            // SAFETY: the export's signature is steam_api.h's; the object is leaked, so it outlives every dispatch.
            let note = unsafe {
                match symbol::<RegisterFn>(module, "SteamAPI_RegisterCallback\0") {
                    Some(register) => {
                        let object = Box::leak(Box::new(CallbackObject {
                            vtable: &VTABLE,
                            flags: 0,
                            id: JOIN_REQUESTED_CALLBACK,
                        }));
                        register(object, JOIN_REQUESTED_CALLBACK);
                        format!("invites: Steam's join callback {JOIN_REQUESTED_CALLBACK} is registered")
                    }
                    None => "invites: steam_api64.dll has no SteamAPI_RegisterCallback; joins from a running game cannot arrive".to_owned(),
                }
            };
            REGISTERED.store(true, Ordering::Release);
            *self.note.lock().unwrap_or_else(|p| p.into_inner()) = Some(note);
        }
    }

    impl Friends for Native {
        fn ready(&self) -> bool {
            friends().is_some()
        }

        fn set_rich_presence(&self, key: &str, value: &str) -> bool {
            let (Ok(k), Ok(v)) = (CString::new(key), CString::new(value)) else { return false };
            let Some((module, iface)) = friends() else { return false };
            // SAFETY: flat API signature; both strings are NUL-terminated and live across the call.
            unsafe {
                let Some(set) = symbol::<SetPresenceFn>(module, "SteamAPI_ISteamFriends_SetRichPresence\0") else {
                    return false;
                };
                set(iface, k.as_ptr(), v.as_ptr())
            }
        }

        fn open_invite_dialog(&self, connect: &str) -> Result<(), String> {
            let text = CString::new(connect).map_err(|_| "the invite text has a NUL in it".to_owned())?;
            let (module, iface) = friends().ok_or("Steam is not running in this game")?;
            // SAFETY: flat API signature; the string is NUL-terminated and lives across the call.
            unsafe {
                let open: InviteDialogFn =
                    symbol(module, "SteamAPI_ISteamFriends_ActivateGameOverlayInviteDialogConnectString\0")
                        .ok_or("this game's Steam has no overlay invite dialog")?;
                open(iface, text.as_ptr());
            }
            Ok(())
        }

        fn friend_name(&self, id: u64) -> Option<String> {
            let (module, iface) = friends()?;
            // SAFETY: flat API signature (CSteamID passes as uint64); the returned string is copied at once.
            unsafe {
                let name: FriendNameFn = symbol(module, "SteamAPI_ISteamFriends_GetFriendPersonaName\0")?;
                let raw = name(iface, id);
                if raw.is_null() {
                    return None;
                }
                steam_persona::clean_name(CStr::from_ptr(raw).to_bytes())
            }
        }

        fn take_join_requests(&self) -> Vec<JoinRequest> {
            self.register();
            take_requests()
        }

        fn take_note(&self) -> Option<String> {
            self.note.lock().unwrap_or_else(|p| p.into_inner()).take()
        }

        fn launch_command_line(&self) -> Option<String> {
            let (module, handle, find) = module()?;
            // SAFETY: the interface by its version name; flat API signature; the buffer's size is passed.
            unsafe {
                let apps = find(handle, c"STEAMAPPS_INTERFACE_VERSION008".as_ptr());
                if apps.is_null() {
                    return None;
                }
                let get: LaunchLineFn = symbol(module, "SteamAPI_ISteamApps_GetLaunchCommandLine\0")?;
                let mut buf = vec![0u8; 1024];
                let n = get(apps, buf.as_mut_ptr().cast(), buf.len() as i32);
                let n = usize::try_from(n).ok()?.min(buf.len());
                let end = buf[..n].iter().position(|b| *b == 0).unwrap_or(n);
                let text = String::from_utf8_lossy(&buf[..end]).trim().to_owned();
                (!text.is_empty()).then_some(text)
            }
        }

        fn process_command_line(&self) -> String {
            std::env::args_os().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    #[derive(Default)]
    pub(in crate::steam_friends) struct Native;

    impl Friends for Native {
        fn ready(&self) -> bool {
            false
        }
        fn set_rich_presence(&self, _key: &str, _value: &str) -> bool {
            false
        }
        fn open_invite_dialog(&self, _connect: &str) -> Result<(), String> {
            Err("Steam is not running in this game".into())
        }
        fn friend_name(&self, _id: u64) -> Option<String> {
            None
        }
        fn take_join_requests(&self) -> Vec<JoinRequest> {
            take_requests()
        }
        fn take_note(&self) -> Option<String> {
            None
        }
        fn launch_command_line(&self) -> Option<String> {
            None
        }
        fn process_command_line(&self) -> String {
            String::new()
        }
    }
}

/// A model of Steam's friends interface for the tests, written from the documentation quoted at the top: what
/// `SetRichPresence` accepts and keeps, an invite dialog that only records the string (Steam tells the game nothing
/// more), and join requests delivered the way callback 337 delivers them.
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;

    #[derive(Default)]
    struct State {
        ready: bool,
        presence: BTreeMap<String, String>,
        dialogs: Vec<String>,
        names: HashMap<u64, String>,
        inbox: Vec<JoinRequest>,
        launch: Option<String>,
        command_line: String,
        /// Every value ever set, removals included, to prove what never reached Steam.
        history: Vec<(String, String)>,
    }

    /// Clones share one Steam client: the test keeps one, the controller gets another.
    #[derive(Clone, Default)]
    pub struct FakeFriends(Arc<Mutex<State>>);

    impl FakeFriends {
        pub fn new() -> FakeFriends {
            let friends = FakeFriends::default();
            friends.set_ready(true);
            friends
        }

        fn state(&self) -> std::sync::MutexGuard<'_, State> {
            self.0.lock().unwrap_or_else(|p| p.into_inner())
        }

        pub fn presence(&self, key: &str) -> Option<String> {
            self.state().presence.get(key).cloned()
        }

        pub fn history(&self) -> Vec<(String, String)> {
            self.state().history.clone()
        }

        pub fn dialogs(&self) -> Vec<String> {
            self.state().dialogs.clone()
        }

        pub fn set_name(&self, id: u64, name: &str) {
            self.state().names.insert(id, name.to_owned());
        }

        pub fn set_ready(&self, ready: bool) {
            self.state().ready = ready;
        }

        /// What Steam does when this player clicks "Join game" on a friend, or accepts an invite, while the game runs:
        /// callback 337 with the connect string.
        pub fn deliver_join(&self, friend: Option<u64>, connect: &str) {
            // m_rgchConnect holds at most 255 bytes and a NUL
            let connect: String = connect.chars().take(MAX_VALUE_BYTES - 1).collect();
            self.state().inbox.push(JoinRequest { friend, connect, source: JoinSource::Running });
        }

        /// What Steam does when the game was not running: it starts the game with the connect string on its command
        /// line (or in the launch command line, when the app enabled that).
        pub fn set_command_line(&self, line: &str) {
            self.state().command_line = line.to_owned();
        }

        pub fn set_launch_command_line(&self, line: Option<&str>) {
            self.state().launch = line.map(str::to_owned);
        }
    }

    impl Friends for FakeFriends {
        fn ready(&self) -> bool {
            self.state().ready
        }

        fn set_rich_presence(&self, key: &str, value: &str) -> bool {
            let mut s = self.state();
            if !s.ready || key.is_empty() || key.len() >= MAX_KEY_BYTES || value.len() >= MAX_VALUE_BYTES {
                return false;
            }
            s.history.push((key.to_owned(), value.to_owned()));
            if value.is_empty() {
                s.presence.remove(key);
                return true;
            }
            if !s.presence.contains_key(key) && s.presence.len() >= MAX_KEYS {
                return false;
            }
            s.presence.insert(key.to_owned(), value.to_owned());
            true
        }

        fn open_invite_dialog(&self, connect: &str) -> Result<(), String> {
            let mut s = self.state();
            if !s.ready {
                return Err("Steam is not running in this game".into());
            }
            s.dialogs.push(connect.to_owned());
            Ok(())
        }

        fn friend_name(&self, id: u64) -> Option<String> {
            let s = self.state();
            s.names.get(&id).filter(|_| s.ready).cloned()
        }

        fn take_join_requests(&self) -> Vec<JoinRequest> {
            std::mem::take(&mut self.state().inbox)
        }

        fn take_note(&self) -> Option<String> {
            None
        }

        fn launch_command_line(&self) -> Option<String> {
            let s = self.state();
            s.launch.clone().filter(|_| s.ready)
        }

        fn process_command_line(&self) -> String {
            self.state().command_line.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeFriends;
    use super::*;

    #[test]
    fn the_join_requested_callback_is_read_at_the_documented_offsets() {
        let mut b = vec![0u8; JOIN_REQUESTED_SIZE];
        b[0..8].copy_from_slice(&76561198000000042u64.to_le_bytes());
        let text = b"+feverlink3_join 1:76561198000000042:0123456789abcdef";
        b[8..8 + text.len()].copy_from_slice(text);
        let (friend, connect) = parse_join_requested(&b).expect("parsed");
        assert_eq!(friend, Some(76561198000000042));
        assert_eq!(connect.as_bytes(), text);
        // an invalid friend id (not joined through a friend) is no friend
        b[0..8].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(parse_join_requested(&b).expect("parsed").0, None);
        // a value that fills the whole array without a NUL is read to its end, never beyond
        b[8..].fill(b'x');
        assert_eq!(parse_join_requested(&b).expect("parsed").1.len(), 256);
        assert_eq!(parse_join_requested(&b[..JOIN_REQUESTED_SIZE - 1]), None);
    }

    #[test]
    fn the_model_keeps_steams_limits_for_rich_presence() {
        let steam = FakeFriends::new();
        assert!(steam.set_rich_presence("connect", "x"));
        assert_eq!(steam.presence("connect").as_deref(), Some("x"));
        assert!(steam.set_rich_presence("connect", ""), "an empty value removes the key");
        assert_eq!(steam.presence("connect"), None);
        assert!(!steam.set_rich_presence(&"k".repeat(MAX_KEY_BYTES), "x"));
        assert!(!steam.set_rich_presence("status", &"v".repeat(MAX_VALUE_BYTES)));
        for i in 0..MAX_KEYS {
            assert!(steam.set_rich_presence(&format!("k{i}"), "x"));
        }
        assert!(!steam.set_rich_presence("one_more", "x"));
        steam.set_ready(false);
        assert!(!steam.set_rich_presence("k1", "y"));
    }

    #[test]
    fn without_steam_in_the_process_nothing_is_there() {
        // the test process has no steam_api64.dll
        let steam = native();
        assert!(!steam.ready());
        assert!(!steam.set_rich_presence(KEY_CONNECT, "x"));
        assert!(steam.open_invite_dialog("x").is_err());
        assert_eq!(steam.launch_command_line(), None);
    }
}
