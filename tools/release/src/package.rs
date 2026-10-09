//! Assembles `FeverLink3-V/` and zips it reproducibly: sorted entries, fixed timestamp, fixed permissions.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use crate::manifest::{sha256_hex, FileEntry, Manifest};

/// Where each input file comes from.
pub struct Inputs {
    pub exe: PathBuf,
    pub native_dll: PathBuf,
    pub mod_dir: PathBuf,
    pub readme: PathBuf,
    pub license: PathBuf,
    pub notices: PathBuf,
}

impl Inputs {
    pub fn from_root(root: &Path) -> Inputs {
        Inputs {
            exe: root.join("target/release/feverlink3.exe"),
            native_dll: root.join("target/release/feverlink3_native.dll"),
            mod_dir: root.join("mod/feverlink3_1"),
            readme: root.join("README.md"),
            license: root.join("LICENSE"),
            notices: root.join("THIRD_PARTY_NOTICES.md"),
        }
    }
}

pub struct Built {
    pub zip_path: PathBuf,
    pub manifest: Manifest,
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn collect_dir(dir: &Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = format!("{prefix}/{name}");
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            collect_dir(&entry.path(), &rel, out)?;
        } else {
            out.push((rel, read(&entry.path())?));
        }
    }
    Ok(())
}

pub fn valid_version(v: &str) -> bool {
    v.chars().next().is_some_and(|c| c.is_ascii_digit())
        && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+')
}

pub fn package(inputs: &Inputs, version: &str, out: &Path) -> Result<Built, String> {
    if !valid_version(version) {
        return Err(format!("bad version {version:?} (give it without the leading v)"));
    }
    let mut files: Vec<(String, Vec<u8>)> = vec![
        ("feverlink3.exe".into(), read(&inputs.exe)?),
        ("winhttp.dll".into(), read(&inputs.native_dll)?),
        ("README.txt".into(), read(&inputs.readme)?),
        ("LICENSE".into(), read(&inputs.license)?),
        ("THIRD_PARTY_NOTICES.md".into(), read(&inputs.notices)?),
    ];
    collect_dir(&inputs.mod_dir, "mod/feverlink3_1", &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let folder = format!("FeverLink3-{version}");
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default()) // 1980-01-01 00:00:00
        .unix_permissions(0o644);
    for (rel, data) in &files {
        writer.start_file(format!("{folder}/{rel}"), options).map_err(|e| e.to_string())?;
        writer.write_all(data).map_err(|e| e.to_string())?;
    }
    let zip_bytes = writer.finish().map_err(|e| e.to_string())?.into_inner();

    let zip_name = format!("{folder}.zip");
    let manifest = Manifest {
        version: version.to_string(),
        files: files
            .iter()
            .map(|(p, d)| FileEntry { path: p.clone(), size: d.len() as u64, sha256: sha256_hex(d) })
            .collect(),
        zip_name: zip_name.clone(),
        zip_sha256: sha256_hex(&zip_bytes),
    };

    fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let zip_path = out.join(&zip_name);
    fs::write(&zip_path, &zip_bytes).map_err(|e| e.to_string())?;
    fs::write(out.join(format!("{zip_name}.sha256")), format!("{}  {zip_name}\n", manifest.zip_sha256))
        .map_err(|e| e.to_string())?;
    fs::write(out.join("manifest.json"), manifest.to_json()).map_err(|e| e.to_string())?;
    Ok(Built { zip_path, manifest })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn fixture(root: &Path) -> Inputs {
        let w = |p: &str, c: &str| {
            let path = root.join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, c).unwrap();
        };
        w("target/release/feverlink3.exe", "exe");
        w("target/release/feverlink3_native.dll", "dll");
        w("README.md", "readme");
        w("LICENSE", "mit");
        w("THIRD_PARTY_NOTICES.md", "notices");
        w("mod/feverlink3_1/mod.json", "{}");
        w("mod/feverlink3_1/content/b.lua", "b");
        w("mod/feverlink3_1/content/a.lua", "a");
        Inputs::from_root(root)
    }

    #[test]
    fn contents_manifest_and_determinism() {
        let root = tempfile::tempdir().unwrap();
        let inputs = fixture(root.path());
        let (o1, o2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let b1 = package(&inputs, "1.2.3", o1.path()).unwrap();
        let b2 = package(&inputs, "1.2.3", o2.path()).unwrap();
        let (z1, z2) = (fs::read(&b1.zip_path).unwrap(), fs::read(&b2.zip_path).unwrap());
        assert_eq!(z1, z2, "two runs give identical zip bytes");

        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(z1.clone())).unwrap();
        let names: Vec<String> =
            (0..archive.len()).map(|i| archive.by_index(i).unwrap().name().unwrap().to_string()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        for want in [
            "FeverLink3-1.2.3/feverlink3.exe",
            "FeverLink3-1.2.3/winhttp.dll",
            "FeverLink3-1.2.3/README.txt",
            "FeverLink3-1.2.3/LICENSE",
            "FeverLink3-1.2.3/THIRD_PARTY_NOTICES.md",
            "FeverLink3-1.2.3/mod/feverlink3_1/mod.json",
            "FeverLink3-1.2.3/mod/feverlink3_1/content/a.lua",
        ] {
            assert!(names.iter().any(|n| n == want), "missing {want}");
        }
        let mut dll = String::new();
        archive.by_name("FeverLink3-1.2.3/winhttp.dll").unwrap().read_to_string(&mut dll).unwrap();
        assert_eq!(dll, "dll");
        assert_eq!(archive.by_index(0).unwrap().last_modified().unwrap(), DateTime::default());

        // manifest hashes
        let m = &b1.manifest;
        assert_eq!(m.zip_sha256, sha256_hex(&z1));
        assert_eq!(m.files.len(), names.len());
        let exe = m.files.iter().find(|f| f.path == "feverlink3.exe").unwrap();
        assert_eq!((exe.size, exe.sha256.as_str()), (3, sha256_hex(b"exe").as_str()));
        let json = fs::read_to_string(o1.path().join("manifest.json")).unwrap();
        let on_disk: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(&on_disk, m);
        let sha_line = fs::read_to_string(o1.path().join("FeverLink3-1.2.3.zip.sha256")).unwrap();
        assert_eq!(sha_line, format!("{}  FeverLink3-1.2.3.zip\n", m.zip_sha256));
    }

    #[test]
    fn rejects_bad_version_and_missing_input() {
        let root = tempfile::tempdir().unwrap();
        let inputs = fixture(root.path());
        let out = tempfile::tempdir().unwrap();
        assert!(package(&inputs, "../x", out.path()).is_err());
        assert!(package(&inputs, "v1.0.0", out.path()).is_err());
        fs::remove_file(&inputs.exe).unwrap();
        assert!(package(&inputs, "1.0.0", out.path()).is_err());
    }
}
