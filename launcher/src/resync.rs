//! The launcher's half of a resync round: finding the host game's save on disk, hashing and slicing it into the
//! chunks `core` seals, writing a received save where the game will look for it, and the permissions view the mod
//! reads. Everything here is plain file and byte handling so it runs in tests without a game.
//!
//! The save name is fixed by `core` (`feverlink3-resync-<id>`), so the host's game saves under it and a player's
//! launcher writes the received bytes under the same name; the `ResyncLoad` line then names a file that exists.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use feverlink3_core::session::{codec, Permissions, ResyncLimits};
use sha2::{Digest, Sha256};

/// The file name stem of round `id`'s save, as `core` names it.
pub fn save_name(id: i32) -> String {
    format!("feverlink3-resync-{id}")
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

/// Writes the received save as `<name>.sav` into every folder (created when missing), through a temporary file
/// and a rename so the game never sees half a save. Returns how many folders got it.
pub fn write_save(dirs: &[PathBuf], name: &str, bytes: &[u8]) -> io::Result<usize> {
    let mut written = 0;
    let mut last_error = None;
    for dir in dirs {
        let result = (|| {
            fs::create_dir_all(dir)?;
            let tmp = dir.join(format!("{name}.sav.tmp"));
            let mut f = File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            drop(f);
            fs::rename(&tmp, dir.join(format!("{name}.sav")))
        })();
        match result {
            Ok(()) => written += 1,
            Err(e) => last_error = Some(e),
        }
    }
    match (written, last_error) {
        (0, Some(e)) => Err(e),
        (0, None) => Err(io::Error::new(io::ErrorKind::NotFound, "no savegame folder to write to")),
        (n, _) => Ok(n),
    }
}

/// What the mod reads for one player: `{build, bulldoze, spend, other}` as booleans, from the host's
/// set-based [`Permissions`]. `None` when the payload is not a Permissions message.
pub fn permissions_for_game(payload: &[u8], player: u16) -> Option<Vec<u8>> {
    let p: Permissions = codec::try_decode(payload)?;
    let view = serde_json::json!({
        "build": p.allows_action("build", player),
        "bulldoze": p.allows_action("bulldoze", player),
        "spend": p.allows_action("spend", player),
        "other": true,
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
        assert_eq!(write_save(&[a.clone(), b.clone()], "n", b"bytes").unwrap(), 2);
        assert_eq!(fs::read(a.join("n.sav")).unwrap(), b"bytes");
        assert_eq!(fs::read(b.join("n.sav")).unwrap(), b"bytes");
        assert!(!a.join("n.sav.tmp").exists());
        assert!(write_save(&[], "n", b"x").is_err());
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
    }
}
