//! Starting Steam without a game: load `steam_api64.dll` from the program's folder and initialise it.
//!
//! The DLL is loaded with the full path of the program's folder; once loaded, `steamnet::global()` finds it by name
//! (`GetModuleHandleA("steam_api64.dll")`), exactly as it does inside the game, and starts the same core on it.
//! Initialisation uses `SteamAPI_InitFlat` (present in the SDK that ships with the game), or `SteamAPI_Init` when an
//! older DLL has no `InitFlat`. The app id comes from `steam_appid.txt` (480, Valve's Spacewar, when it is missing).

use std::path::{Path, PathBuf};

use super::{APPID_FILE, SPACEWAR_APP_ID};

/// Everything that can go wrong before the speed test starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitError {
    /// `steam_api64.dll` is not in the program's folder.
    DllMissing(PathBuf),
    /// Windows refused to load it (code from `GetLastError`).
    DllNotLoadable { path: PathBuf, code: i32 },
    /// The DLL lacks a function the speed test needs (an unusual DLL).
    MissingExport(&'static str),
    /// `SteamAPI_IsSteamRunning` said no, or the init result said there is no Steam client.
    SteamNotRunning,
    /// The init call failed with this result code and message.
    Failed { code: i32, message: String },
    /// `steam_appid.txt` could not be read, written or understood.
    AppIdFile(String),
    /// The Steam networking core could not start.
    CoreNotStarted,
}

impl InitError {
    /// What to tell the player, in plain English, with what to do next.
    pub fn explain(&self) -> String {
        match self {
            InitError::DllMissing(path) => format!(
                "steam_api64.dll was not found at {}.\nIt must sit in the same folder as this program. Unzip the whole package into one folder and start the program from there (do not run it from inside the zip file).",
                path.display()
            ),
            InitError::DllNotLoadable { path, code } => format!(
                "Windows could not load {} (error {code}).\nThe file may be damaged or blocked: unzip the package again, and if Windows asked about the files being from the internet, allow them. This program needs the 64-bit Windows version of the file.",
                path.display()
            ),
            InitError::MissingExport(name) => format!(
                "The steam_api64.dll in this folder does not have the function {name}, so it is not the file this program expects.\nUse the steam_api64.dll that came in the package."
            ),
            InitError::SteamNotRunning => "Steam is not running.\nStart the Steam program, wait until you are logged in (your name shows at the top right), then run this program again.".into(),
            InitError::Failed { code, message } => {
                let what = match code {
                    2 => "Steam is not running.",
                    3 => "Your Steam program is too old for this test. Let Steam update itself (restart Steam), then run this program again.",
                    _ => "Steam did not start the test. This usually means Steam is not logged in, is in Offline Mode, or is still starting.",
                };
                let message = if message.trim().is_empty() { "no message".to_string() } else { message.trim().to_string() };
                format!("{what}\nSteam said: {message} (result {code}).\nCheck that Steam is running and logged in and not in Offline Mode (Steam menu, Go Online), then run this program again.")
            }
            InitError::AppIdFile(why) => format!(
                "There is a problem with {APPID_FILE}: {why}\nDelete the file and run the program again; it will create a new one."
            ),
            InitError::CoreNotStarted => "The program could not start its Steam networking part (the helper thread did not start). Close other programs and try again.".into(),
        }
    }
}

/// Reads `steam_appid.txt` in `dir`, writing it with 480 when it is missing. Returns the app id.
pub fn ensure_app_id_file(dir: &Path) -> Result<u32, InitError> {
    let path = dir.join(APPID_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            text.trim_start_matches('\u{feff}').trim().parse::<u32>().ok().filter(|id| *id != 0).ok_or_else(|| {
                InitError::AppIdFile(format!("it should contain only a number such as {SPACEWAR_APP_ID}"))
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&path, format!("{SPACEWAR_APP_ID}\r\n"))
                .map_err(|e| InitError::AppIdFile(format!("it is missing and could not be created in this folder ({e}); move the program to a folder you may write to, such as Documents")))?;
            Ok(SPACEWAR_APP_ID)
        }
        Err(e) => Err(InitError::AppIdFile(format!("it could not be read ({e})"))),
    }
}

/// Steam started from our folder.
#[cfg(windows)]
pub struct SteamSession {
    pub app_id: u32,
    run_callbacks: unsafe extern "C" fn(),
}

#[cfg(windows)]
impl SteamSession {
    /// `SteamAPI_RunCallbacks`: call it often (each step of the loops does).
    pub fn pump(&self) {
        // SAFETY: the function was taken from the DLL we initialised, and it takes no arguments.
        unsafe { (self.run_callbacks)() }
    }
}

/// Loads the DLL from `dir`, writes `steam_appid.txt` when missing and initialises Steam.
#[cfg(windows)]
pub fn start(dir: &Path) -> Result<SteamSession, InitError> {
    use std::ffi::{c_char, c_void};
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    let app_id = ensure_app_id_file(dir)?;
    let dll = dir.join("steam_api64.dll");
    if !dll.is_file() {
        return Err(InitError::DllMissing(dll));
    }
    // Steam looks for steam_appid.txt next to the program or in the current folder; the environment names it too.
    let _ = std::env::set_current_dir(dir);
    std::env::set_var("SteamAppId", app_id.to_string());
    std::env::set_var("SteamGameId", app_id.to_string());
    let wide: Vec<u16> = dll.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: a NUL-terminated wide path; loading a DLL runs its DllMain, which is Valve's.
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        return Err(InitError::DllNotLoadable {
            path: dll,
            code: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        });
    }
    // SAFETY: GetProcAddress with a NUL-terminated name; the signatures are steam_api_flat.h's.
    let get = |name: &'static str| -> *mut c_void { unsafe { GetProcAddress(module, name.as_ptr()) } };
    let run_callbacks = get("SteamAPI_RunCallbacks\0");
    if run_callbacks.is_null() {
        return Err(InitError::MissingExport("SteamAPI_RunCallbacks"));
    }
    let is_running = get("SteamAPI_IsSteamRunning\0");
    if !is_running.is_null() {
        // SAFETY: `bool SteamAPI_IsSteamRunning()`.
        let running = unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> bool>(is_running)() };
        if !running {
            return Err(InitError::SteamNotRunning);
        }
    }
    let init_flat = get("SteamAPI_InitFlat\0");
    if !init_flat.is_null() {
        let mut message = [0u8; 1024];
        // SAFETY: `ESteamAPIInitResult SteamAPI_InitFlat(SteamErrMsg *pOutErrMsg)`; SteamErrMsg is char[1024].
        let result = unsafe {
            std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_char) -> i32>(init_flat)(
                message.as_mut_ptr().cast(),
            )
        };
        if result != 0 {
            let end = message.iter().position(|b| *b == 0).unwrap_or(message.len());
            return Err(match result {
                2 => InitError::SteamNotRunning,
                _ => InitError::Failed { code: result, message: String::from_utf8_lossy(&message[..end]).into_owned() },
            });
        }
    } else {
        let init = get("SteamAPI_Init\0");
        if init.is_null() {
            return Err(InitError::MissingExport("SteamAPI_InitFlat"));
        }
        // SAFETY: `bool SteamAPI_Init()`.
        let ok = unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> bool>(init)() };
        if !ok {
            return Err(InitError::Failed { code: 1, message: "SteamAPI_Init returned false".into() });
        }
    }
    // SAFETY: `void SteamAPI_RunCallbacks()`.
    let run_callbacks = unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn()>(run_callbacks) };
    Ok(SteamSession { app_id, run_callbacks })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_app_id_file_is_created_with_480() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(ensure_app_id_file(dir.path()).unwrap(), 480);
        assert_eq!(std::fs::read_to_string(dir.path().join(APPID_FILE)).unwrap().trim(), "480");
    }

    #[test]
    fn an_existing_app_id_file_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(APPID_FILE), "\u{feff}480\r\n").unwrap();
        assert_eq!(ensure_app_id_file(dir.path()).unwrap(), 480);
        std::fs::write(dir.path().join(APPID_FILE), "730").unwrap();
        assert_eq!(ensure_app_id_file(dir.path()).unwrap(), 730);
    }

    #[test]
    fn a_broken_app_id_file_is_explained() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(APPID_FILE), "hello").unwrap();
        let text = ensure_app_id_file(dir.path()).unwrap_err().explain();
        assert!(text.contains("steam_appid.txt") && text.contains("only a number"), "{text}");
    }

    #[test]
    fn every_failure_has_a_plain_english_next_step() {
        let cases = [
            (InitError::DllMissing(PathBuf::from("C:/x/steam_api64.dll")), "same folder as this program"),
            (InitError::DllNotLoadable { path: PathBuf::from("a"), code: 193 }, "error 193"),
            (InitError::MissingExport("SteamAPI_InitFlat"), "SteamAPI_InitFlat"),
            (InitError::SteamNotRunning, "Start the Steam program"),
            (InitError::Failed { code: 1, message: "Not logged on".into() }, "Steam said: Not logged on"),
            (InitError::Failed { code: 3, message: String::new() }, "too old"),
            (InitError::Failed { code: 2, message: "x".into() }, "Steam is not running"),
            (InitError::CoreNotStarted, "networking part"),
        ];
        for (error, needle) in cases {
            let text = error.explain();
            assert!(text.contains(needle), "{text}");
            assert!(text.is_ascii(), "{text}");
        }
    }

    #[test]
    fn a_missing_dll_is_reported_before_anything_else() {
        #[cfg(windows)]
        {
            let dir = tempfile::tempdir().unwrap();
            assert!(matches!(start(dir.path()), Err(InitError::DllMissing(_))));
            assert!(dir.path().join(APPID_FILE).is_file(), "steam_appid.txt is written first");
        }
    }
}
