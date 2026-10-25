//! This game's mod-list fingerprint for the join (`docs/PROTOCOL.md`, "Mod list"; the comparison is in
//! `feverlink3_core::session::mods`).
//!
//! The list itself comes from the game: the window reads the documented mod repository
//! (`app.getUserProfile():getModRep()`, feverlink3_hud.lua `H.modList`) and sends it with its `host` / `join` command as
//! `mods = { {id, revision, cosmetic}, ... }` and `game = getBuildVersion()`. Only the game knows which mods it has; no
//! documented call lists the ones ACTIVE in the running world, so these are the installed ones.
//!
//! What is added here, from files: a hash of each mod's files, found by the `modId` of the `mod.json` in a mod folder (the
//! wiki's mod definition page: `mod.json`, `content/`, `_metadata/`). Folders searched, first match wins, in the precedence
//! tf3-docs (mod-structure) gives ("the staging area wins over a manual installation, and that wins over a mod.io
//! subscription"): each user folder's `staging_area/`, its `mods/`, then the game's `mods/` (its `release/` subfolder holds
//! the shipped script mods). The folder of mod.io downloads is not documented: those mods carry no hash and are compared by
//! revision only. And the builds: FeverLink3's version, the SHA-256 of the native module file next to the game.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use feverlink3_core::session::{ModEntry, ModFingerprint};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// A file larger than this is hashed by its path and size only (textures and meshes can be hundreds of MB; a different
/// one almost always has another size).
pub const CONTENT_MAX_FILE: u64 = 4 << 20;
/// Content read for one mod at most; files after it count by path and size only. The order is the sorted path list, so
/// two equal folders read the same files.
pub const CONTENT_MAX_TOTAL: u64 = 256 << 20;
/// Files looked at in one mod at most (a broken folder must not stall a join).
const MAX_FILES: usize = 50_000;

/// The fingerprint from the window's command parameters (`mods`, `game`). Without a list (the window's Lua state could not
/// read the mod repository) it is marked `unread`: the builds are still compared, the mods are not, and every window says so.
pub fn from_params(params: &Value, user_dirs: &[PathBuf], game_dir: Option<&Path>) -> ModFingerprint {
    // an empty Lua table comes through the text link as `{}`
    let empty = Vec::new();
    let (list, unread) = match params.get("mods") {
        Some(Value::Array(a)) => (a, false),
        Some(Value::Object(o)) if o.is_empty() => (&empty, false),
        _ => (&empty, true),
    };
    let folders = mod_folders(user_dirs, game_dir);
    let mods = list
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_owned();
            let revision = m.get("revision").and_then(Value::as_i64).unwrap_or(0);
            let cosmetic = m.get("cosmetic").and_then(Value::as_bool).unwrap_or(false);
            let files = folders.iter().find(|(fid, _)| *fid == id).and_then(|(_, dir)| files_hash(dir));
            Some(ModEntry { id, revision, cosmetic, files })
        })
        .collect();
    let native = game_dir.and_then(|d| file_sha256(&d.join("winhttp.dll"))).unwrap_or_default();
    let fp = ModFingerprint {
        game: params.get("game").and_then(Value::as_str).unwrap_or("").to_owned(),
        feverlink3: env!("CARGO_PKG_VERSION").to_owned(),
        native,
        // set by the controller for a game its native module does not support (live.rs)
        unsupported: String::new(),
        // the build's addresses found by search (native/src/search.rs): host and player must have found the same
        searched: crate::live::searched_game_tag(),
        mods,
        unread,
    };
    fp.normalized()
}

/// The game's folder: the one of the running executable (the controller runs inside the game process).
pub fn running_game_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// Every mod folder with a readable `mod.json`, as (modId, folder), in precedence order.
pub fn mod_folders(user_dirs: &[PathBuf], game_dir: Option<&Path>) -> Vec<(String, PathBuf)> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for u in user_dirs {
        roots.push(u.join("staging_area"));
    }
    for u in user_dirs {
        roots.push(u.join("mods"));
    }
    if let Some(g) = game_dir {
        roots.push(g.join("mods"));
        roots.push(g.join("mods").join("release"));
    }
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else { continue };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for dir in dirs {
            if let Some(id) = mod_id(&dir) {
                out.push((id, dir));
            }
        }
    }
    out
}

fn mod_id(dir: &Path) -> Option<String> {
    let text = fs::read(dir.join("mod.json")).ok()?;
    let v: Value = serde_json::from_slice(&text).ok()?;
    v.get("modId")?.as_str().map(str::to_owned)
}

/// The hash of a mod's files (hex): every file but `_metadata/` (presentation, and the game writes its mod.io link there),
/// sorted by relative path with `/`, each as path, size and, within the limits above, its content's SHA-256.
pub fn files_hash(dir: &Path) -> Option<String> {
    let mut files = Vec::new();
    collect(dir, dir, &mut files);
    if files.is_empty() {
        return None;
    }
    files.sort();
    let mut h = Sha256::new();
    let mut read = 0u64;
    for (rel, path, size) in files.iter().take(MAX_FILES) {
        h.update(rel.as_bytes());
        h.update([0]);
        h.update(size.to_be_bytes());
        if *size <= CONTENT_MAX_FILE && read + size <= CONTENT_MAX_TOTAL {
            read += size;
            match file_sha256(path) {
                Some(c) => h.update(c.as_bytes()),
                None => h.update(b"unreadable"),
            }
        }
    }
    Some(hex(&h.finalize()))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf, u64)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        if out.len() >= MAX_FILES {
            return;
        }
        let path = e.path();
        let Ok(meta) = e.metadata() else { continue };
        let rel = path.strip_prefix(root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        if meta.is_dir() {
            if !rel.eq_ignore_ascii_case("_metadata") {
                collect(root, &path, out);
            }
        } else if meta.is_file() {
            out.push((rel, path, meta.len()));
        }
    }
}

fn file_sha256(path: &Path) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_mod(root: &Path, folder: &str, id: &str, script: &str) -> PathBuf {
        let dir = root.join(folder);
        fs::create_dir_all(dir.join("content")).unwrap();
        fs::create_dir_all(dir.join("_metadata")).unwrap();
        fs::write(dir.join("mod.json"), format!(r#"{{"modId": "{id}", "revision": 2}}"#)).unwrap();
        fs::write(dir.join("content").join("a.lua"), script).unwrap();
        fs::write(dir.join("_metadata").join("modinfo.json"), "{}").unwrap();
        dir
    }

    #[test]
    fn the_hash_follows_the_content_not_the_metadata_or_the_folder() {
        let t = tempfile::tempdir().unwrap();
        let a = write_mod(t.path(), "one", "x", "return 1");
        let b = write_mod(t.path(), "two", "x", "return 1");
        let c = write_mod(t.path(), "three", "x", "return 2");
        fs::write(b.join("_metadata").join("mod.io_fileid.txt"), "123").unwrap();
        assert_eq!(files_hash(&a), files_hash(&b));
        assert_ne!(files_hash(&a), files_hash(&c));
        assert_eq!(files_hash(&t.path().join("none")), None);
    }

    #[test]
    fn the_window_list_gets_hashes_from_the_folders_in_precedence_order() {
        let t = tempfile::tempdir().unwrap();
        let user = t.path().join("local");
        let game = t.path().join("game");
        write_mod(&user.join("staging_area"), "dev_copy", "x", "staged");
        write_mod(&user.join("mods"), "x_1", "x", "manual");
        write_mod(&game.join("mods").join("release"), "feverlink3", "feverlink3", "ours");
        fs::write(game.join("winhttp.dll"), b"module").unwrap();
        let params = json!({
            "game": "1.4.2",
            "mods": [
                {"id": "x", "revision": 2},
                {"id": "feverlink3", "revision": 1},
                {"id": "from_modio", "revision": 5, "cosmetic": true},
                {"id": ""}
            ]
        });
        let fp = from_params(&params, std::slice::from_ref(&user), Some(&game));
        assert_eq!(fp.game, "1.4.2");
        assert_eq!(fp.native, file_sha256(&game.join("winhttp.dll")).unwrap());
        let ids: Vec<&str> = fp.mods.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["feverlink3", "from_modio", "x"], "sorted, the base game's empty id left out");
        assert_eq!(fp.mods[2].files, files_hash(&user.join("staging_area").join("dev_copy")), "staging wins");
        assert!(fp.mods[0].files.is_some());
        assert_eq!((fp.mods[1].files.clone(), fp.mods[1].cosmetic), (None, true), "no folder: revision only");
        let none = from_params(&json!({"name": "a"}), &[user], Some(&game));
        assert!(none.unread && none.mods.is_empty(), "no list from the window: said, not compared");
    }
}
