//! `manifest.json`: version, every file with size and SHA-256, and the zip's SHA-256. The launcher parses the
//! same shape (`launcher/src/update.rs`).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path inside the portable folder, forward slashes, no leading folder name.
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub files: Vec<FileEntry>,
    pub zip_name: String,
    pub zip_sha256: String,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl Manifest {
    /// Pretty JSON with a trailing newline; field and file order are fixed, so the bytes are reproducible.
    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("manifest serialises");
        s.push('\n');
        s
    }
}
