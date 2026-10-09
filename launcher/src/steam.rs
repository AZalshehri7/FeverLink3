//! Where Steam, the game and the per-user game folders are.
//!
//! Facts about the layout (all confirmed on a machine with the game installed unless stated otherwise):
//!
//! - Steam records its folder in the registry at `HKCU\Software\Valve\Steam` value `SteamPath`, with forward
//!   slashes (`c:/program files (x86)/steam`). `HKLM\SOFTWARE\WOW6432Node\Valve\Steam` value `InstallPath` is the
//!   machine-wide fallback.
//! - `<steam>/steamapps/libraryfolders.vdf` lists every library folder: a VDF document whose top-level key
//!   `libraryfolders` holds one table per library with a `path` entry whose backslashes are escaped (`C:\\Games`).
//!   Old Steam versions wrote `"1"  "D:\\Games"` directly; both shapes are accepted.
//! - The game lives at `<library>/steamapps/common/Transport Fever 3/TransportFever3.exe`.
//! - The per-user game folder is `<steam>/userdata/<steam id>/3493540/local/`, which holds `mods/`, `save/` and
//!   `settings.lua`. Finding from MPFever (MIT); the folder did not yet exist on the development machine, so its
//!   contents are known from MPFever only.
//!
//! Everything that transforms text or paths is a pure function; only [`discover`] and the `*_from_registry` /
//! `existing_*` functions touch the registry and the filesystem.

use std::path::{Path, PathBuf};

use crate::{APP_ID, GAME_EXE};

/// Folder name of the game under `steamapps/common`.
pub const GAME_FOLDER: &str = "Transport Fever 3";

// ---------------------------------------------------------------------------------------------------------------
// VDF
// ---------------------------------------------------------------------------------------------------------------

/// A node of a Valve KeyValues (VDF) document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vdf {
    Str(String),
    /// Ordered key/value pairs. Keys repeat in real files, so this is a list, not a map.
    Table(Vec<(String, Vdf)>),
}

impl Vdf {
    /// The child of a table with this key (case-insensitive, first match), if any.
    pub fn get(&self, key: &str) -> Option<&Vdf> {
        match self {
            Vdf::Table(items) => items.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v),
            Vdf::Str(_) => None,
        }
    }

    /// The string value of this node, if it is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Vdf::Str(s) => Some(s),
            Vdf::Table(_) => None,
        }
    }

    /// The entries of this node, if it is a table.
    pub fn entries(&self) -> &[(String, Vdf)] {
        match self {
            Vdf::Table(items) => items,
            Vdf::Str(_) => &[],
        }
    }
}

/// Parses a VDF document tolerantly: quoted and bare tokens, `\\ \" \n \t` escapes, `//` comments, missing
/// closing braces, a value without a key. Never fails: whatever is understood is returned as the root table.
pub fn parse_vdf(text: &str) -> Vdf {
    let tokens = tokenize(text);
    let mut pos = 0;
    Vdf::Table(parse_table(&tokens, &mut pos))
}

#[derive(Debug, PartialEq)]
enum Token {
    Open,
    Close,
    Text(String),
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => out.push(Token::Open),
            '}' => out.push(Token::Close),
            '"' => {
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None | Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(other) => s.push(other), // \\ and \" and anything unknown: keep the character
                            None => break,
                        },
                        Some(other) => s.push(other),
                    }
                }
                out.push(Token::Text(s));
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {}
            c => {
                let mut s = String::from(c);
                while let Some(&n) = chars.peek() {
                    if n.is_whitespace() || n == '{' || n == '}' || n == '"' {
                        break;
                    }
                    s.push(n);
                    chars.next();
                }
                out.push(Token::Text(s));
            }
        }
    }
    out
}

fn parse_table(tokens: &[Token], pos: &mut usize) -> Vec<(String, Vdf)> {
    let mut items = Vec::new();
    while *pos < tokens.len() {
        match &tokens[*pos] {
            Token::Close => {
                *pos += 1;
                return items;
            }
            Token::Open => {
                // a table without a key: keep it under an empty key rather than lose it
                *pos += 1;
                items.push((String::new(), Vdf::Table(parse_table(tokens, pos))));
            }
            Token::Text(key) => {
                *pos += 1;
                match tokens.get(*pos) {
                    Some(Token::Open) => {
                        *pos += 1;
                        items.push((key.clone(), Vdf::Table(parse_table(tokens, pos))));
                    }
                    Some(Token::Text(value)) => {
                        *pos += 1;
                        items.push((key.clone(), Vdf::Str(value.clone())));
                    }
                    Some(Token::Close) | None => {
                        // key without a value at the end of a table: record it empty
                        items.push((key.clone(), Vdf::Str(String::new())));
                    }
                }
            }
        }
    }
    items
}

// ---------------------------------------------------------------------------------------------------------------
// Steam layout, pure
// ---------------------------------------------------------------------------------------------------------------

/// Normalises a path as Steam writes it in the registry (forward slashes) to a native path.
pub fn normalize_steam_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim().trim_end_matches(['/', '\\']);
    PathBuf::from(trimmed.replace('/', std::path::MAIN_SEPARATOR_STR))
}

/// The library folders named by a `libraryfolders.vdf` text, the Steam folder itself first. Order is kept,
/// duplicates (compared case-insensitively) are dropped.
pub fn library_paths(vdf_text: &str, steam_path: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![steam_path.to_path_buf()];
    let root = parse_vdf(vdf_text);
    let folders = root.get("libraryfolders").unwrap_or(&root);
    for (key, value) in folders.entries() {
        let path = match value {
            // new shape: "0" { "path" "C:\\Games" ... }
            Vdf::Table(_) => value.get("path").and_then(Vdf::as_str),
            // old shape: "1" "D:\\Games"; skip the metadata keys of the old format
            Vdf::Str(s) if key.chars().all(|c| c.is_ascii_digit()) => Some(s.as_str()),
            Vdf::Str(_) => None,
        };
        if let Some(p) = path.filter(|p| !p.trim().is_empty()) {
            let candidate = normalize_steam_path(p);
            if !out.iter().any(|known| same_path_text(known, &candidate)) {
                out.push(candidate);
            }
        }
    }
    out
}

fn same_path_text(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

/// The game folder a library would hold: `<library>/steamapps/common/Transport Fever 3`.
pub fn game_dir_in_library(library: &Path) -> PathBuf {
    library.join("steamapps").join("common").join(GAME_FOLDER)
}

/// The game executable inside a game folder.
pub fn game_exe(game_dir: &Path) -> PathBuf {
    game_dir.join(GAME_EXE)
}

/// Picks the first library whose game folder holds the executable, using `exists` to ask the filesystem.
pub fn find_game_dir_with(libraries: &[PathBuf], exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    libraries.iter().map(|lib| game_dir_in_library(lib)).find(|dir| exists(&game_exe(dir)))
}

/// `<steam>/userdata/<id>/3493540/local`: the per-user folder the game keeps its mods, saves and settings in.
pub fn user_local_dir(steam_path: &Path, steam_user_id: &str) -> PathBuf {
    steam_path.join("userdata").join(steam_user_id).join(APP_ID).join("local")
}

/// The user mods folder inside a per-user folder.
pub fn mods_dir(user_local_dir: &Path) -> PathBuf {
    user_local_dir.join("mods")
}

/// The per-user folders of the given Steam user ids; `exists` filters to the ones present.
pub fn user_local_dirs_with(
    steam_path: &Path,
    user_ids: impl IntoIterator<Item = String>,
    exists: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    user_ids
        .into_iter()
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
        .map(|id| user_local_dir(steam_path, &id))
        .filter(|dir| exists(dir))
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------
// Steam layout, this machine
// ---------------------------------------------------------------------------------------------------------------

/// What was found on this machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SteamInfo {
    /// Steam's own folder, from the registry.
    pub steam_path: Option<PathBuf>,
    /// Every library folder, Steam's own first.
    pub libraries: Vec<PathBuf>,
    /// The folder holding `TransportFever3.exe`.
    pub game_dir: Option<PathBuf>,
    /// Every existing `userdata/<id>/3493540/local` folder.
    pub user_dirs: Vec<PathBuf>,
}

/// Reads the Steam folder from the registry: `HKCU\Software\Valve\Steam\SteamPath`, then
/// `HKLM\SOFTWARE\WOW6432Node\Valve\Steam\InstallPath`.
#[cfg(windows)]
pub fn steam_path_from_registry() -> Option<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let read = |root, sub: &str, name: &str| -> Option<String> {
        RegKey::predef(root).open_subkey(sub).ok()?.get_value::<String, _>(name).ok()
    };
    read(HKEY_CURRENT_USER, r"Software\Valve\Steam", "SteamPath")
        .or_else(|| read(HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath"))
        .filter(|s| !s.trim().is_empty())
        .map(|s| normalize_steam_path(&s))
}

#[cfg(not(windows))]
pub fn steam_path_from_registry() -> Option<PathBuf> {
    None
}

/// The library folders of a Steam install, read from its `libraryfolders.vdf` (the Steam folder alone if the
/// file is missing or unreadable).
pub fn existing_libraries(steam_path: &Path) -> Vec<PathBuf> {
    let vdf = steam_path.join("steamapps").join("libraryfolders.vdf");
    let text = std::fs::read_to_string(vdf).unwrap_or_default();
    library_paths(&text, steam_path)
}

/// The existing per-user folders of a Steam install.
pub fn existing_user_dirs(steam_path: &Path) -> Vec<PathBuf> {
    let ids = std::fs::read_dir(steam_path.join("userdata"))
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut dirs = user_local_dirs_with(steam_path, ids, |p| p.is_dir());
    dirs.sort();
    dirs
}

/// Everything this module can find on this machine. Never fails: absent pieces are `None` or empty.
pub fn discover() -> SteamInfo {
    let Some(steam_path) = steam_path_from_registry() else {
        return SteamInfo::default();
    };
    let libraries = existing_libraries(&steam_path);
    let game_dir = find_game_dir_with(&libraries, |p| p.is_file());
    let user_dirs = existing_user_dirs(&steam_path);
    SteamInfo { steam_path: Some(steam_path), libraries, game_dir, user_dirs }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
		"contentid"		"1505811561834066444"
		"apps"
		{
			"228980"		"977932114"
			"3493540"		"70854863925"
		}
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"   // a comment
		"label"		"Games \"big\" disk"
		"apps"
		{
		}
	}
}
"#;

    #[test]
    fn parses_nested_tables_and_escapes() {
        let root = parse_vdf(SAMPLE);
        let folders = root.get("libraryfolders").expect("root key");
        let first = folders.get("0").expect("library 0");
        assert_eq!(first.get("path").and_then(Vdf::as_str), Some(r"C:\Program Files (x86)\Steam"));
        assert_eq!(first.get("apps").unwrap().get("3493540").and_then(Vdf::as_str), Some("70854863925"));
        let second = folders.get("1").expect("library 1");
        assert_eq!(second.get("path").and_then(Vdf::as_str), Some(r"D:\SteamLibrary"));
        assert_eq!(second.get("label").and_then(Vdf::as_str), Some(r#"Games "big" disk"#));
        assert_eq!(second.get("apps"), Some(&Vdf::Table(vec![])));
    }

    #[test]
    fn parser_tolerates_broken_input() {
        assert_eq!(parse_vdf(""), Vdf::Table(vec![]));
        // unclosed table, key without value, bare tokens, unterminated string
        let root = parse_vdf("a { b c d } e\n{ f \"g");
        assert_eq!(root.get("a").unwrap().get("b").and_then(Vdf::as_str), Some("c"));
        assert_eq!(root.get("a").unwrap().get("d").and_then(Vdf::as_str), Some(""));
        assert_eq!(root.get("e").unwrap().get("f").and_then(Vdf::as_str), Some("g"));
        // a table with no key at all is kept under the empty key
        let root = parse_vdf("{ \"x\" \"1\" }");
        assert_eq!(root.get("").unwrap().get("x").and_then(Vdf::as_str), Some("1"));
    }

    #[test]
    fn escapes_n_and_t_and_unknown() {
        let root = parse_vdf(r#""k" "a\nb\tc\qd""#);
        assert_eq!(root.get("k").and_then(Vdf::as_str), Some("a\nb\tcqd"));
    }

    #[test]
    fn library_paths_steam_first_then_vdf_deduplicated() {
        let steam = PathBuf::from(r"C:\Program Files (x86)\Steam");
        let libs = library_paths(SAMPLE, &steam);
        assert_eq!(libs, vec![steam.clone(), PathBuf::from(r"D:\SteamLibrary")]);
        // the Steam folder listed in another case is still the same folder
        let libs = library_paths(SAMPLE, &PathBuf::from(r"c:\program files (x86)\steam"));
        assert_eq!(libs.len(), 2);
    }

    #[test]
    fn library_paths_old_format_and_missing_file() {
        let steam = PathBuf::from(r"C:\Steam");
        let old = "\"LibraryFolders\"\n{\n\t\"TimeNextStatsReport\"\t\"1\"\n\t\"ContentStatsID\"\t\"2\"\n\t\"1\"\t\"D:\\\\Games\"\n\t\"2\"\t\"E:\\\\More Games\"\n}\n";
        let libs = library_paths(old, &steam);
        assert_eq!(libs, vec![steam.clone(), PathBuf::from(r"D:\Games"), PathBuf::from(r"E:\More Games")]);
        assert_eq!(library_paths("", &steam), vec![steam]);
    }

    #[test]
    fn game_dir_is_found_in_the_library_that_has_the_exe() {
        let libs = vec![PathBuf::from(r"C:\Steam"), PathBuf::from(r"D:\SteamLibrary")];
        let found = find_game_dir_with(&libs, |p| {
            p == Path::new(r"D:\SteamLibrary\steamapps\common\Transport Fever 3\TransportFever3.exe")
        });
        assert_eq!(found, Some(PathBuf::from(r"D:\SteamLibrary\steamapps\common\Transport Fever 3")));
        assert_eq!(find_game_dir_with(&libs, |_| false), None);
    }

    #[test]
    fn user_local_dirs_use_the_app_id_and_skip_non_ids() {
        let steam = PathBuf::from(r"C:\Steam");
        let dirs = user_local_dirs_with(
            &steam,
            ["192289926".to_string(), "ac".to_string(), "7".to_string(), String::new()],
            |p| p.ends_with(r"192289926\3493540\local"),
        );
        assert_eq!(dirs, vec![PathBuf::from(r"C:\Steam\userdata\192289926\3493540\local")]);
        assert_eq!(mods_dir(&dirs[0]), PathBuf::from(r"C:\Steam\userdata\192289926\3493540\local\mods"));
    }

    #[test]
    fn registry_path_is_normalised() {
        assert_eq!(normalize_steam_path("c:/program files (x86)/steam"), PathBuf::from(r"c:\program files (x86)\steam"));
        assert_eq!(normalize_steam_path(r"C:\Steam\"), PathBuf::from(r"C:\Steam"));
    }
}
