//! Ed25519 keys and signatures. Key file: 64 hex characters (the 32-byte seed). Signature file: 128 hex
//! characters. Public key: 64 hex characters.

use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey};

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

pub fn sign_bytes(key: &SigningKey, message: &[u8]) -> String {
    to_hex(&key.sign(message).to_bytes())
}

#[cfg(test)]
pub fn verify_bytes(public_hex: &str, message: &[u8], sig_hex: &str) -> Result<(), String> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let public: [u8; 32] = from_hex(public_hex)?.try_into().map_err(|_| "public key must be 32 bytes".to_string())?;
    let public = VerifyingKey::from_bytes(&public).map_err(|e| e.to_string())?;
    let sig: [u8; 64] = from_hex(sig_hex)?.try_into().map_err(|_| "signature must be 64 bytes".to_string())?;
    public.verify(message, &Signature::from_bytes(&sig)).map_err(|e| e.to_string())
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

/// Signs the manifest file, writing `<manifest>.sig` next to it.
pub fn sign_file(key_file: &Path, manifest: &Path) -> Result<PathBuf, String> {
    let key = signing_key_from_hex(&fs::read_to_string(key_file).map_err(|e| e.to_string())?)?;
    let data = fs::read(manifest).map_err(|e| e.to_string())?;
    let mut sig_name = manifest.file_name().ok_or("manifest has no file name")?.to_os_string();
    sig_name.push(".sig");
    let sig_path = manifest.with_file_name(sig_name);
    fs::write(&sig_path, format!("{}\n", sign_bytes(&key, &data))).map_err(|e| e.to_string())?;
    Ok(sig_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_round_trip_and_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let public = keygen(dir.path()).unwrap();
        assert!(keygen(dir.path()).is_err(), "never overwrites a key");
        let manifest = dir.path().join("manifest.json");
        fs::write(&manifest, b"{\"version\":\"1.0.0\"}\n").unwrap();
        let sig_path = sign_file(&dir.path().join(KEY_FILE), &manifest).unwrap();
        assert_eq!(sig_path, dir.path().join("manifest.json.sig"));
        let sig = fs::read_to_string(&sig_path).unwrap();
        let data = fs::read(&manifest).unwrap();
        verify_bytes(&public, &data, &sig).unwrap();
        let mut tampered = data.clone();
        tampered[12] ^= 1;
        assert!(verify_bytes(&public, &tampered, &sig).is_err());
        let other = keygen(&dir.path().join("other")).unwrap();
        assert!(verify_bytes(&other, &data, &sig).is_err());
    }
}
