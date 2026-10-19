//! Ed25519 keys and signatures. Key file: 64 hex characters (the 32-byte seed). Public key: 64 hex characters.
//! Signature file: `feverlink3-ed25519 <key id> <128 hex characters>`, the format of the launcher's
//! `update::signature_text`, so the launcher's own parser reads what this tool writes.

use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey};
use feverlink3_launcher::update::{self, TrustedKey};

pub const KEY_FILE: &str = "feverlink3_release.key";
pub const PUB_FILE: &str = "feverlink3_release.pub";

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return Err("bad hex".into());
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "bad hex".to_string())).collect()
}

pub fn signing_key_from_hex(s: &str) -> Result<SigningKey, String> {
    let seed: [u8; 32] = from_hex(s)?.try_into().map_err(|_| "key must be 32 bytes".to_string())?;
    Ok(SigningKey::from_bytes(&seed))
}

/// The signature file's text for `message`.
pub fn sign_bytes(key: &SigningKey, message: &[u8]) -> String {
    update::signature_text(&key.verifying_key().to_bytes(), &key.sign(message).to_bytes())
}

/// Creates `DIR/feverlink3_release.key` (secret) and `.pub`; returns the public key in hex.
pub fn keygen(out: &Path) -> Result<String, String> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| e.to_string())?;
    let key = SigningKey::from_bytes(&seed);
    let public = to_hex(key.verifying_key().as_bytes());
    fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let key_path = out.join(KEY_FILE);
    if key_path.exists() {
        return Err(format!("{} exists; refusing to overwrite a key", key_path.display()));
    }
    fs::write(&key_path, format!("{}\n", to_hex(&seed))).map_err(|e| e.to_string())?;
    fs::write(out.join(PUB_FILE), format!("{public}\n")).map_err(|e| e.to_string())?;
    Ok(public)
}

/// The key id the launcher shows for a public key (hex).
pub fn key_id_of(public_hex: &str) -> Result<String, String> {
    update::public_key_from_hex(public_hex).map(|k| update::key_id(&k)).map_err(|e| e.to_string())
}

/// Signs the manifest file, writing `<manifest>.sig` next to it.
pub fn sign_file(key_file: &Path, manifest: &Path) -> Result<PathBuf, String> {
    let key = signing_key_from_hex(&fs::read_to_string(key_file).map_err(|e| e.to_string())?)?;
    let data = fs::read(manifest).map_err(|e| e.to_string())?;
    let sig_path = sig_path_of(manifest)?;
    fs::write(&sig_path, sign_bytes(&key, &data)).map_err(|e| e.to_string())?;
    Ok(sig_path)
}

fn sig_path_of(manifest: &Path) -> Result<PathBuf, String> {
    let mut sig_name = manifest.file_name().ok_or("manifest has no file name")?.to_os_string();
    sig_name.push(".sig");
    Ok(manifest.with_file_name(sig_name))
}

/// Checks `<manifest>.sig` the way the launcher does: a trusted key of `keys` that has not expired at `now_unix`,
/// and the zip next to the manifest against the manifest's SHA-256. Returns the name of the key that signed it.
pub fn verify_file(manifest: &Path, keys: &[TrustedKey], now_unix: i64) -> Result<String, String> {
    let data = fs::read(manifest).map_err(|e| e.to_string())?;
    let sig = fs::read(sig_path_of(manifest)?).map_err(|e| e.to_string())?;
    let (m, key) = update::verify_signed_manifest(&data, &sig, keys, now_unix).map_err(|e| e.to_string())?;
    let zip_path = manifest.with_file_name(&m.zip_name);
    let zip = fs::read(&zip_path).map_err(|e| format!("{}: {e}", zip_path.display()))?;
    update::verify_zip(&zip, &m).map_err(|e| e.to_string())?;
    Ok(key.name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_round_trip_and_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let public = keygen(dir.path()).unwrap();
        assert!(keygen(dir.path()).is_err(), "never overwrites a key");
        let zip = b"zip bytes";
        fs::write(dir.path().join("FeverLink3-1.0.0.zip"), zip).unwrap();
        let manifest = dir.path().join("manifest.json");
        let json = format!(
            "{{\"version\":\"1.0.0\",\"files\":[],\"zip_name\":\"FeverLink3-1.0.0.zip\",\"zip_sha256\":\"{}\"}}\n",
            update::sha256_hex(zip)
        );
        fs::write(&manifest, &json).unwrap();
        let sig_path = sign_file(&dir.path().join(KEY_FILE), &manifest).unwrap();
        assert_eq!(sig_path, dir.path().join("manifest.json.sig"));
        let sig = fs::read_to_string(&sig_path).unwrap();
        assert!(sig.starts_with(&format!("{} {} ", update::SIGNATURE_TAG, key_id_of(&public).unwrap())), "{sig}");

        let public: &'static str = Box::leak(public.into_boxed_str());
        let trusted = [TrustedKey { name: "release", public_hex: public, expires: (2030, 1, 1) }];
        let now = 1_792_411_200; // 2026-10-19
        assert_eq!(verify_file(&manifest, &trusted, now).unwrap(), "release");

        // the launcher's checks, through this tool: an untrusted key, an expired key, a changed manifest, a changed zip
        let other = keygen(&dir.path().join("other")).unwrap();
        let other: &'static str = Box::leak(other.into_boxed_str());
        assert!(verify_file(&manifest, &[TrustedKey { public_hex: other, ..trusted[0] }], now).is_err());
        assert!(verify_file(&manifest, &[TrustedKey { expires: (2026, 1, 1), ..trusted[0] }], now).is_err());
        assert!(verify_file(&manifest, &[], now).is_err());
        fs::write(dir.path().join("FeverLink3-1.0.0.zip"), b"other zip").unwrap();
        assert!(verify_file(&manifest, &trusted, now).is_err());
        fs::write(dir.path().join("FeverLink3-1.0.0.zip"), zip).unwrap();
        fs::write(&manifest, json.replace("1.0.0\"", "1.0.1\"")).unwrap();
        assert!(verify_file(&manifest, &trusted, now).is_err());
    }
}
