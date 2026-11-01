//! The launcher's half of a resync round: finding the host game's save on disk, hashing and slicing it into the
//! chunks `core` seals, writing a received save where the game will look for it, and the permissions view the mod
//! reads. Everything here is plain file and byte handling so it runs in tests without a game.
//!
//! The save name is fixed by `core` (`feverlink3-resync-<id>`), so the host's game saves under it and a player's
//! launcher writes the received bytes under the same name; the `ResyncLoad` line then names a file that exists.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use feverlink3_core::session::{codec, Permissions, ResyncLimits};
use sha2::{Digest, Sha256};

/// The file name stem of round `id`'s save, as `core` names it.
pub fn save_name(id: i32) -> String {
    format!("feverlink3-resync-{id}")
}

/// As [`save_name`] for the transfer context a parallel bulk transfer carries (the round id).
pub fn save_name_for(ctx: u32) -> String {
    save_name(ctx as i32)
}

/// The `save` folders of the given per-user folders (`userdata/<id>/3493540/local`).
pub fn save_dirs(user_dirs: &[PathBuf]) -> Vec<PathBuf> {
    user_dirs.iter().map(|d| d.join("save")).collect()
}

/// The newest `<name>.sav` in any of the folders.
pub fn find_save(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    dirs.iter()
        .map(|d| d.join(format!("{name}.sav")))
        .filter(|p| p.is_file())
        .max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH))
}

/// SHA-256 (lower-case hex) and length of a file, streamed: the save is never held whole in memory.
pub fn hash_file(path: &Path) -> io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut len = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len += n as u64;
    }
    Ok((hex(&hasher.finalize()), len))
}

/// Chunk `index` of a file of `len` bytes, read from disk at its offset.
pub fn read_chunk(path: &Path, len: u64, index: u32) -> io::Result<Vec<u8>> {
    let size = ResyncLimits::chunk_len(len, index).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("chunk {index} is outside a file of {len} bytes"))
    })?;
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(u64::from(index) * ResyncLimits::CHUNK_SIZE as u64))?;
    let mut data = vec![0u8; size];
    file.read_exact(&mut data)?;
    Ok(data)
}

/// What [`write_save`] or [`copy_save`] did: how many folders hold this round's save, and one line per folder that
/// does not (for the log). A folder in `missed` has lost its older file of the name unless its line says otherwise.
#[derive(Debug)]
pub struct Stored {
    pub held: usize,
    pub missed: Vec<String>,
}

/// Writes the received save as `<name>.sav` into every folder (created when missing), through a temporary file
/// and a rename so the game never sees half a save. Ok when at least one folder holds it; fails only when none does,
/// so the load that names it would open nothing or an older file. A folder that cannot get it loses its older file of
/// that name: names restart at round 1 every session, so a stale `<name>.sav` would be another session's world (review
/// 2026-10-31).
pub fn write_save(dirs: &[PathBuf], name: &str, bytes: &[u8]) -> io::Result<Stored> {
    let expected = (hex(&Sha256::digest(bytes)), bytes.len() as u64);
    let stored = store_in(dirs, name, 0, &|| Ok(expected.clone()), &|tmp| {
        let mut f = File::create(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()
    });
    if stored.held == 0 {
        let why = if stored.missed.is_empty() {
            "no savegame folder to write to".to_owned()
        } else {
            stored.missed.join("; ")
        };
        return Err(io::Error::other(why));
    }
    Ok(stored)
}

/// As [`write_save`] from a file already on disk (a save received over parallel connections, which lands in the first
/// folder) into the other folders `dirs`. `src` itself counts as a folder holding the save, so a copy that fails
/// everywhere else still leaves the round loadable from the first folder.
pub fn copy_save(src: &Path, dirs: &[PathBuf], name: &str) -> Stored {
    // hashed only when a folder already has a file of the same size (see `holds_same`)
    let expected = std::cell::OnceCell::new();
    let expected =
        || expected.get_or_init(|| hash_file(src).map_err(|e| e.to_string())).clone().map_err(io::Error::other);
    store_in(dirs, name, 1, &expected, &|tmp| fs::copy(src, tmp).map(|_| ()))
}

/// Whether `save` already holds exactly the bytes being stored (`expected` gives their SHA-256 and length). Two game
/// windows on one PC share a savegame folder, and the host's game may be loading that very file while the player's
/// launcher stores it: such a file is this round's save, not a stale one, even when it cannot be replaced.
fn holds_same(save: &Path, expected: &dyn Fn() -> io::Result<(String, u64)>) -> bool {
    let Ok(len) = fs::metadata(save).map(|m| m.len()) else { return false };
    matches!(expected(), Ok((sha, want)) if want == len && hash_file(save).is_ok_and(|(h, _)| h == sha))
}

/// The files the game writes next to `<name>.sav` (its script and preview images). They describe that save, so they go
/// when an older file of the name is replaced or removed (`is_resync_file` lists the same tails).
const COMPANIONS: [&str; 3] = [".sav.lua", ".sav.jpg", ".sav.png"];

/// Puts `<name>.sav` into every folder: `fill` writes the temporary file, a rename makes it the save. `held` counts
/// folders that hold it already. See [`write_save`].
fn store_in(
    dirs: &[PathBuf],
    name: &str,
    held: usize,
    expected: &dyn Fn() -> io::Result<(String, u64)>,
    fill: &dyn Fn(&Path) -> io::Result<()>,
) -> Stored {
    let mut stored = Stored { held, missed: Vec::new() };
    for dir in dirs {
        let save = dir.join(format!("{name}.sav"));
        // a folder that has these bytes already is left as it is, companions included: they belong to this save
        if holds_same(&save, expected) {
            stored.held += 1;
            continue;
        }
        let older = save.exists();
        let tmp = dir.join(format!("{name}.sav.tmp"));
        let result = fs::create_dir_all(dir).and_then(|()| fill(&tmp)).and_then(|()| fs::rename(&tmp, &save));
        if older {
            for tail in COMPANIONS {
                let _ = fs::remove_file(dir.join(format!("{name}{tail}")));
            }
        }
        match result {
            Ok(()) => stored.held += 1,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                let line = match older.then(|| fs::remove_file(&save)) {
                    Some(Err(kept)) => format!(
                        "{} is an older file of that name and could not be replaced ({e}) or removed ({kept}); a game that loads from this folder may open it",
                        save.display()
                    ),
                    _ => format!("{}: {e}", dir.display()),
                };
                stored.missed.push(line);
            }
        }
    }
    stored
}

/// How long a resync file stays before [`prune_saves`] deletes it. Another session on this PC (a second game window)
/// may still be sending or loading its round's file; a round takes at most its save, transfer and load timeouts (well
/// under an hour), so a file older than this belongs to no running round.
pub const PRUNE_AGE: Duration = Duration::from_secs(3600);

/// Whether `file` is one of FeverLink3's own resync files: exactly `feverlink3-resync-<digits>.sav`, or that name with
/// our temporary suffix (`.tmp`, `.part`) or a companion file the game writes next to a save (`.lua`, `.jpg`, `.png`).
/// Nothing else in a savegame folder matches, so the player's own saves are never touched.
pub fn is_resync_file(file: &str) -> bool {
    let Some((digits, tail)) = file.strip_prefix("feverlink3-resync-").and_then(|rest| rest.split_once(".sav")) else {
        return false;
    };
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && matches!(tail, "" | ".tmp" | ".part" | ".lua" | ".jpg" | ".png")
}

/// Deletes the resync files ([`is_resync_file`]) last changed more than `older_than` before `now` from the folders. A
/// session start does this: the files of earlier sessions (one save per round, often 100 MB and more) were never deleted
/// and filled every Steam account's savegame folder and the game's load list (review 2026-10-31). Returns how many went.
pub fn prune_saves(dirs: &[PathBuf], older_than: Duration, now: SystemTime) -> usize {
    let mut removed = 0;
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let named = entry.file_name().to_str().is_some_and(is_resync_file);
            if !named || !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(now);
            let old = now.duration_since(modified).is_ok_and(|age| age > older_than);
            if old && fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Whether the file was last changed before `since`, with a margin for file systems that keep coarse times (FAT: 2 s):
/// a file the game saved in this round is never older than the round.
pub fn older_than_round(path: &Path, since: SystemTime) -> io::Result<bool> {
    let modified = fs::metadata(path)?.modified()?;
    Ok(modified + Duration::from_secs(2) < since)
}

/// What the mod reads for one player: `{build, bulldoze, spend, other}` as booleans, from the host's
/// set-based [`Permissions`]. `None` when the payload is not a Permissions message.
pub fn permissions_for_game(payload: &[u8], player: u16) -> Option<Vec<u8>> {
    let p: Permissions = codec::try_decode(payload)?;
    let view = serde_json::json!({
        "build": p.allows_action("build", player),
        "bulldoze": p.allows_action("bulldoze", player),
        "spend": p.allows_action("spend", player),
        "other": p.allows_action("other", player),
    });
    serde_json::to_vec(&view).ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn name_matches_core() {
        assert_eq!(save_name(7), "feverlink3-resync-7");
    }

    #[test]
    fn save_dirs_join_save() {
        let d = save_dirs(&[PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(d, vec![PathBuf::from("a").join("save"), PathBuf::from("b").join("save")]);
    }

    #[test]
    fn finds_the_newest_copy_and_none_when_absent() {
        let t = tmp();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        assert!(find_save(&[a.clone(), b.clone()], "x").is_none());
        fs::write(a.join("x.sav"), b"old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        fs::write(b.join("x.sav"), b"new").unwrap();
        assert_eq!(find_save(&[a, b.clone()], "x"), Some(b.join("x.sav")));
    }

    #[test]
    fn hash_is_streamed_and_correct() {
        let t = tmp();
        let p = t.path().join("f");
        fs::write(&p, b"abc").unwrap();
        let (h, len) = hash_file(&p).unwrap();
        assert_eq!(h, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(len, 3);
        let big = vec![7u8; 3 * ResyncLimits::CHUNK_SIZE + 5];
        fs::write(&p, &big).unwrap();
        assert_eq!(hash_file(&p).unwrap().1, big.len() as u64);
    }

    #[test]
    fn chunks_reassemble_to_the_file() {
        let t = tmp();
        let p = t.path().join("f");
        let data: Vec<u8> = (0..(2 * ResyncLimits::CHUNK_SIZE + 123)).map(|i| (i % 251) as u8).collect();
        fs::write(&p, &data).unwrap();
        let len = data.len() as u64;
        let count = ResyncLimits::chunk_count(len).unwrap();
        assert_eq!(count, 3);
        let mut out = Vec::new();
        for i in 0..count {
            out.extend(read_chunk(&p, len, i).unwrap());
        }
        assert_eq!(out, data);
        assert!(read_chunk(&p, len, count).is_err());
    }

    #[test]
    fn write_save_creates_folders_and_leaves_no_temp_file() {
        let t = tmp();
        let (a, b) = (t.path().join("u1").join("save"), t.path().join("u2").join("save"));
        assert_eq!(write_save(&[a.clone(), b.clone()], "n", b"bytes").unwrap().held, 2);
        assert_eq!(fs::read(a.join("n.sav")).unwrap(), b"bytes");
        assert_eq!(fs::read(b.join("n.sav")).unwrap(), b"bytes");
        assert!(!a.join("n.sav.tmp").exists());
        assert!(write_save(&[], "n", b"x").is_err());
    }

    #[test]
    fn a_folder_that_cannot_take_the_save_loses_its_older_one() {
        let t = tmp();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("n.sav"), b"an earlier session's world").unwrap();
        fs::write(b.join("n.sav"), b"another earlier world").unwrap();
        fs::write(b.join("n.sav.lua"), b"that world's script").unwrap();
        // the temporary file cannot be made in `a`: the write fails there
        fs::create_dir_all(a.join("n.sav.tmp")).unwrap();
        let stored = write_save(&[a.clone(), b.clone()], "n", b"new").unwrap();
        assert_eq!((stored.held, stored.missed.len()), (1, 1));
        assert!(!a.join("n.sav").exists(), "no stale save is left under the round's name");
        assert_eq!(fs::read(b.join("n.sav")).unwrap(), b"new");
        assert!(!b.join("n.sav.lua").exists(), "the replaced save's companion goes with it");
        assert!(write_save(std::slice::from_ref(&a), "n", b"new").is_err());
        // a copy from the first folder counts that folder: failing in every other one still leaves the round loadable
        fs::write(a.join("n.sav"), b"stale").unwrap();
        let copied = copy_save(&b.join("n.sav"), std::slice::from_ref(&a), "n");
        assert_eq!((copied.held, copied.missed.len()), (1, 1));
        assert!(!a.join("n.sav").exists());
        assert_eq!(copy_save(&b.join("n.sav"), &[], "n").held, 1);
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_save_counts_only_when_it_holds_the_same_bytes() {
        use std::os::windows::fs::OpenOptionsExt;
        let t = tmp();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("n.sav"), b"round").unwrap();
        fs::write(a.join("n.sav.lua"), b"its script").unwrap();
        // a game loading the file (a second window on this PC) opened it without delete sharing: it cannot be replaced
        let _game = File::options().read(true).share_mode(1).open(a.join("n.sav")).unwrap();
        assert!(fs::remove_file(a.join("n.sav")).is_err(), "the test file is locked");
        let stored = write_save(std::slice::from_ref(&a), "n", b"round").unwrap();
        assert_eq!((stored.held, stored.missed.len()), (1, 0));
        assert!(a.join("n.sav.lua").exists(), "this save's companion stays");
        // a locked file with other bytes is stale: no folder holds the round alone, and with another folder it is named
        assert!(write_save(std::slice::from_ref(&a), "n", b"other").is_err());
        let stored = write_save(&[a.clone(), b.clone()], "n", b"other").unwrap();
        assert_eq!(stored.held, 1);
        assert!(stored.missed[0].contains("could not be replaced"), "{:?}", stored.missed);
    }

    #[test]
    fn only_old_resync_files_are_pruned() {
        assert!(is_resync_file("feverlink3-resync-12.sav"));
        assert!(is_resync_file("feverlink3-resync-1.sav.part"));
        assert!(is_resync_file("feverlink3-resync-1.sav.lua"));
        for other in [
            "feverlink3-resync-.sav",
            "feverlink3-resync-1a.sav",
            "feverlink3-resync-1.save",
            "feverlink3-resync-1.sav.bak",
            "my feverlink3-resync-1.sav",
            "autosave.sav",
        ] {
            assert!(!is_resync_file(other), "{other}");
        }
        let t = tmp();
        let dir = t.path().join("save");
        fs::create_dir_all(&dir).unwrap();
        let keep = ["autosave.sav", "feverlink3-resync-1.save", "feverlink3-resync-3.sav"];
        for f in ["feverlink3-resync-1.sav", "feverlink3-resync-1.sav.lua", keep[0], keep[1], keep[2]] {
            fs::write(dir.join(f), b"x").unwrap();
        }
        fs::create_dir_all(dir.join("feverlink3-resync-2.sav")).unwrap();
        let now = SystemTime::now() + 2 * PRUNE_AGE;
        // feverlink3-resync-3.sav is recent: another session on this PC may still use it
        let recent = File::options().write(true).open(dir.join(keep[2])).unwrap();
        recent.set_modified(now).unwrap();
        assert_eq!(prune_saves(&[dir.clone(), t.path().join("missing")], PRUNE_AGE, now), 2);
        for f in keep {
            assert!(dir.join(f).exists(), "{f} is kept");
        }
        assert!(dir.join("feverlink3-resync-2.sav").is_dir(), "a folder is never removed");
        assert!(!dir.join("feverlink3-resync-1.sav").exists());
    }

    #[test]
    fn permissions_are_translated_per_player() {
        let payload = br#"{"build":{"only":[2]},"bulldoze":"hostOnly","spend":"everyone","speed":"everyone"}"#;
        let for_player = |id: u16| {
            let v: serde_json::Value = serde_json::from_slice(&permissions_for_game(payload, id).unwrap()).unwrap();
            v
        };
        let two = for_player(2);
        assert_eq!(two["build"], true);
        assert_eq!(two["bulldoze"], false);
        assert_eq!(two["spend"], true);
        assert_eq!(two["other"], true);
        let three = for_player(3);
        assert_eq!(three["build"], false);
        // the host is allowed everything
        let host = for_player(0);
        assert_eq!(host["build"], true);
        assert_eq!(host["bulldoze"], true);
        assert!(permissions_for_game(b"not json", 1).is_none());

        // `other` is a set like the rest
        let payload = br#"{"other":{"only":[2]}}"#;
        let other = |id: u16| {
            let v: serde_json::Value = serde_json::from_slice(&permissions_for_game(payload, id).unwrap()).unwrap();
            v["other"].clone()
        };
        assert_eq!((other(0), other(2), other(3)), (true.into(), true.into(), false.into()));
    }
}
