//! The player's Steam display name, used as the default name in the window instead of the Windows user name (which
//! would otherwise be shown to every other player and written to logs).
//!
//! It is read from the game's own, already loaded `steam_api64.dll` (flat API, the same way the Steam transport finds
//! its interfaces). Nothing here runs unless Steam is loaded and initialised; every failure is `None`, and the caller
//! keeps a neutral name until the real one is known.

/// The longest name the window and the session use, in characters.
pub const MAX_NAME_CHARS: usize = 24;

/// The name used until the Steam name is known (and when Steam is not there at all).
pub const FALLBACK_NAME: &str = "Player";

/// A display name from the bytes Steam returned: invalid UTF-8 is replaced, control characters are dropped, the
/// ends are trimmed and the length is limited. `None` when nothing is left.
pub fn clean_name(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_NAME_CHARS).collect())
}

/// This account's Steam display name, once Steam is initialised.
pub fn persona_name() -> Option<String> {
    imp::persona_name()
}

#[cfg(windows)]
mod imp {
    use std::ffi::{c_char, c_void, CStr};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    type HandleFn = unsafe extern "C" fn() -> i32;
    type FindFn = unsafe extern "C" fn(i32, *const c_char) -> *mut c_void;
    type NameFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;

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

    pub(super) fn persona_name() -> Option<String> {
        // SAFETY: the signatures are the flat API's (steam_api_flat.h); the interface is asked for by its version name
        // only once the user handle is not 0 (Steam initialised), and the returned string is copied before returning.
        unsafe {
            let module = GetModuleHandleA(c"steam_api64.dll".as_ptr().cast());
            if module.is_null() {
                return None;
            }
            let user: HandleFn = symbol(module, "SteamAPI_GetHSteamUser\0")?;
            let find: FindFn = symbol(module, "SteamInternal_FindOrCreateUserInterface\0")?;
            let name: NameFn = symbol(module, "SteamAPI_ISteamFriends_GetPersonaName\0")?;
            let handle = user();
            if handle == 0 {
                return None;
            }
            let friends = find(handle, c"SteamFriends017".as_ptr());
            if friends.is_null() {
                return None;
            }
            let raw = name(friends);
            if raw.is_null() {
                return None;
            }
            super::clean_name(CStr::from_ptr(raw).to_bytes())
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub(super) fn persona_name() -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_name_is_kept() {
        assert_eq!(clean_name(b"Oath Bringer").as_deref(), Some("Oath Bringer"));
    }

    #[test]
    fn control_characters_and_the_ends_are_cleaned() {
        assert_eq!(clean_name(b"  Ana\x00\x07\n ").as_deref(), Some("Ana"));
        assert_eq!(clean_name(b"\t \x01 ").as_deref(), None);
        assert_eq!(clean_name(b"").as_deref(), None);
    }

    #[test]
    fn invalid_utf8_is_replaced_not_rejected() {
        let name = clean_name(&[b'A', 0xFF, b'b']).unwrap();
        assert!(name.starts_with('A') && name.ends_with('b'), "{name}");
    }

    #[test]
    fn the_length_is_limited_in_characters_not_bytes() {
        let long = "é".repeat(40);
        let name = clean_name(long.as_bytes()).unwrap();
        assert_eq!(name.chars().count(), MAX_NAME_CHARS);
        assert_eq!(clean_name("x".repeat(100).as_bytes()).unwrap().len(), MAX_NAME_CHARS);
    }

    #[test]
    fn without_steam_there_is_no_name() {
        // the test process has no steam_api64.dll
        assert_eq!(persona_name(), None);
    }
}
