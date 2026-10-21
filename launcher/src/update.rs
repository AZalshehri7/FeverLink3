//! Update logic over bytes: no network, no UI (`docs/ARCHITECTURE.md` section 11). The network fetch and the
//! prompts are the launcher's `update` command (`src/updatecmd.rs`).
//!
//! The order a caller follows, each step a pure function here:
//!
//! 1. [`check_update`]: parse the GitHub Releases JSON, pick the assets, compare versions.
//! 2. Download `manifest.json` and `manifest.json.sig`.
//! 3. [`verify_signed_manifest`]: the signature over the exact manifest bytes, by a key of [`TRUSTED_KEYS`] that
//!    has not expired. Nothing downloaded is used before this passes.
//! 4. Download the zip; [`verify_zip`]: its SHA-256 against the signed manifest.
//! 5. [`may_update`]: never while a game or another launcher (a session) runs.
//! 6. [`plan_swap`] then [`stage`] then [`apply`]: extract into the install folder, retire the current files as
//!    `.old`, move the new ones in, roll back on failure. Nothing outside the install folder is ever touched.
//! 7. A [`Journal`] of what was swapped is written; after the start check ([`judge_start`]) it is either confirmed
//!    ([`confirm`], the `.old` files go) or undone ([`rollback`]) and the version is put on the skip list.
//!
//! The in-game automatic update (`src/autoupdate.rs`) uses the same checks through [`accept_manifest`], which decides
//! between the two trust modes: unsigned until 1.0.0 while [`TRUSTED_KEYS`] is empty (owner decision 2026-10-22),
//! signature required as soon as a key is listed.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A release signing key the launcher accepts.
///
/// Rotation: a new key is added here (and to the protected signing environment) while the old one is still valid;
/// releases are then signed with the new key; the old entry is removed, or simply expires. A launcher whose every
/// key has expired refuses all updates and the player downloads a new launcher by hand, which is the safe failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustedKey {
    /// Shown to the player ("release key 2026"); not used for matching.
    pub name: &'static str,
    /// The Ed25519 public key, 64 hex characters (`feverlink3_release keygen` prints it).
    pub public_hex: &'static str,
    /// Last day (UTC, inclusive) on which a signature by this key is accepted, as (year, month, day).
    pub expires: (i32, u32, u32),
}

/// The keys an update may be signed with.
///
/// EMPTY until 1.0.0 (owner decision 2026-10-22: releases are unsigned until then; `docs/RELEASING.md`, "The
/// signing key"). While it is empty the developer program's `update` command refuses every update, and the in-game
/// automatic update runs in the unsigned mode of [`accept_manifest`] (HTTPS from GitHub's API and the hash checks).
/// The moment a key is listed here, [`accept_manifest`] requires a good signature by it again.
/// The former placeholder key's private half was public (it is the test seed below), so it must never be trusted; a
/// test checks that it is not in this list.
pub const TRUSTED_KEYS: &[TrustedKey] = &[];

/// First word of `manifest.json.sig`: the format and the algorithm.
pub const SIGNATURE_TAG: &str = "feverlink3-ed25519";

/// Folder in the install folder where an update is extracted before the swap.
pub const STAGING_DIR: &str = "feverlink3_update_tmp";

/// The record of an applied update that is not confirmed yet, in the install folder.
pub const JOURNAL_FILE: &str = "feverlink3_update_journal.json";

/// Versions whose start check failed on this PC, one per line, in the install folder; they are not offered again.
pub const SKIP_FILE: &str = "feverlink3_update_skip.txt";

/// Name of the launcher executable inside a portable release.
pub const EXE_NAME: &str = "feverlink3.exe";

/// The native module inside an overlay release (the files a player copies into the game folder).
pub const DLL_NAME: &str = "winhttp.dll";

/// The folder of an overlay release's mod files.
pub const OVERLAY_MOD_PREFIX: &str = "mods/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    BadJson(String),
    BadVersion(String),
    MissingAsset(&'static str),
    AmbiguousAsset(&'static str),
    InsecureUrl(String),
    BadKey,
    /// `manifest.json.sig` is empty or missing: the release was published unsigned.
    Unsigned,
    /// The signature file is not `feverlink3-ed25519 <key id> <signature>`.
    BadSignatureFormat,
    /// Signed by a key this launcher does not trust (the key id is given).
    UnknownKey(String),
    /// Signed by a trusted key whose last valid day has passed (or the PC's clock is wrong).
    ExpiredKey {
        name: String,
        expired: String,
    },
    BadSignature,
    ManifestVersionMismatch {
        manifest: String,
        release: String,
    },
    /// The manifest names another zip than the one the release offers.
    ZipNameMismatch {
        manifest: String,
        release: String,
    },
    ZipHashMismatch,
    BadZip(String),
    UnsafePath(String),
    NotInManifest(String),
    MissingFromZip(String),
    FileMismatch(String),
    /// The manifest is neither a portable release (with `feverlink3.exe`) nor an overlay (`winhttp.dll` and `mods/`).
    UnknownLayout,
    /// Updating now could break a running game or session.
    Busy(String),
    Io(String),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for UpdateError {}

impl From<std::io::Error> for UpdateError {
    fn from(e: std::io::Error) -> Self {
        UpdateError::Io(e.to_string())
    }
}

type Result<T> = std::result::Result<T, UpdateError>;

// ---------------------------------------------------------------------------------------------- versions

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ident {
    Num(u64),
    Alpha(String),
}

impl Ord for Ident {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Ident::Num(a), Ident::Num(b)) => a.cmp(b),
            (Ident::Num(_), Ident::Alpha(_)) => std::cmp::Ordering::Less,
            (Ident::Alpha(_), Ident::Num(_)) => std::cmp::Ordering::Greater,
            (Ident::Alpha(a), Ident::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Ident {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Semantic version: `major.minor.patch[-pre.release][+build]`. A pre-release is lower than its release; build
/// metadata is ignored; a leading `v` is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: (u64, u64, u64),
    pre: Vec<Ident>,
}

impl Version {
    pub fn parse(text: &str) -> Result<Version> {
        let bad = || UpdateError::BadVersion(text.to_string());
        let t = text.trim();
        let t = t.strip_prefix('v').unwrap_or(t);
        let t = t.split('+').next().unwrap_or(t);
        let (core, pre) = match t.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (t, None),
        };
        let num = |s: &str| -> Result<u64> {
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0')) {
                return Err(bad());
            }
            s.parse().map_err(|_| bad())
        };
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 {
            return Err(bad());
        }
        let core = (num(parts[0])?, num(parts[1])?, num(parts[2])?);
        let mut idents = Vec::new();
        if let Some(pre) = pre {
            for id in pre.split('.') {
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return Err(bad());
                }
                idents.push(match id.bytes().all(|b| b.is_ascii_digit()) {
                    true => Ident::Num(id.parse().map_err(|_| bad())?),
                    false => Ident::Alpha(id.to_string()),
                });
            }
        }
        Ok(Version { core, pre: idents })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        self.core.cmp(&other.core).then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Equal,
            (true, false) => Greater,
            (false, true) => Less,
            (false, false) => self.pre.cmp(&other.pre),
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// ------------------------------------------------------------------------------------- GitHub release JSON

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub zip: Asset,
    pub manifest: Asset,
    /// `manifest.json.sig`: absent from the unsigned releases before 1.0.0.
    pub signature: Option<Asset>,
    /// `FeverLink3-<version>.zip.sha256`, the hash the release job published beside the zip.
    pub sha256: Option<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing newer (or the newest release is a draft or a disallowed pre-release).
    UpToDate,
    Available(Box<Release>),
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

fn pick(assets: &[ApiAsset], what: &'static str, matches: impl Fn(&str) -> bool) -> Result<Asset> {
    pick_optional(assets, what, matches)?.ok_or(UpdateError::MissingAsset(what))
}

/// The one asset that `matches`, `None` when there is none; two are an error, and so is an address that is not HTTPS.
fn pick_optional(assets: &[ApiAsset], what: &'static str, matches: impl Fn(&str) -> bool) -> Result<Option<Asset>> {
    let mut found = assets.iter().filter(|a| matches(&a.name));
    let Some(first) = found.next() else { return Ok(None) };
    if found.next().is_some() {
        return Err(UpdateError::AmbiguousAsset(what));
    }
    if !first.browser_download_url.starts_with("https://") {
        return Err(UpdateError::InsecureUrl(first.browser_download_url.clone()));
    }
    Ok(Some(Asset { name: first.name.clone(), url: first.browser_download_url.clone(), size: first.size }))
}

/// Parses one release object of the GitHub Releases API (`/releases/latest` or an element of `/releases`).
pub fn parse_release(json: &str) -> Result<(Release, bool)> {
    let api: ApiRelease = serde_json::from_str(json).map_err(|e| UpdateError::BadJson(e.to_string()))?;
    let version = Version::parse(&api.tag_name)?;
    let release = Release {
        tag: api.tag_name.clone(),
        version,
        zip: pick(&api.assets, "FeverLink3-*.zip", |n| n.starts_with("FeverLink3-") && n.ends_with(".zip"))?,
        manifest: pick(&api.assets, "manifest.json", |n| n == "manifest.json")?,
        signature: pick_optional(&api.assets, "manifest.json.sig", |n| n == "manifest.json.sig")?,
        sha256: pick_optional(&api.assets, "FeverLink3-*.zip.sha256", |n| {
            n.starts_with("FeverLink3-") && n.ends_with(".zip.sha256")
        })?,
    };
    Ok((release, api.draft || api.prerelease))
}

/// Decides whether `release_json` offers something newer than `current`. A draft is never offered; a pre-release
/// only when `allow_prerelease` (by tag or by GitHub's flag).
pub fn check_update(current: &str, release_json: &str, allow_prerelease: bool) -> Result<Decision> {
    let current = Version::parse(current)?;
    let api: ApiRelease = serde_json::from_str(release_json).map_err(|e| UpdateError::BadJson(e.to_string()))?;
    if api.draft {
        return Ok(Decision::UpToDate);
    }
    let (release, flagged) = parse_release(release_json)?;
    if (flagged || release.version.is_prerelease()) && !allow_prerelease {
        return Ok(Decision::UpToDate);
    }
    Ok(if release.version > current { Decision::Available(Box::new(release)) } else { Decision::UpToDate })
}

// ----------------------------------------------------------------------------------- manifest and hashing

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
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
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A public key from its 64 hex characters.
pub fn public_key_from_hex(hex: &str) -> Result<[u8; 32]> {
    from_hex(hex).and_then(|v| v.try_into().ok()).ok_or(UpdateError::BadKey)
}

/// The short name of a public key inside a signature file: the first 8 bytes of its SHA-256, in hex. It only
/// picks the key; the signature check is what proves anything.
pub fn key_id(public_key: &[u8; 32]) -> String {
    to_hex(&Sha256::digest(public_key)[..8])
}

/// The text of `manifest.json.sig`: `feverlink3-ed25519 <key id> <signature hex>` and a newline. Written by the
/// release tool, read by [`parse_signature`].
pub fn signature_text(public_key: &[u8; 32], signature: &[u8; 64]) -> String {
    format!("{SIGNATURE_TAG} {} {}\n", key_id(public_key), to_hex(signature))
}

/// Splits a signature file into (key id, signature). An empty file is [`UpdateError::Unsigned`]; anything that
/// is not exactly the three fields (a bare hex signature of the old format included) is refused.
pub fn parse_signature(text: &[u8]) -> Result<(String, [u8; 64])> {
    let text = std::str::from_utf8(text).map_err(|_| UpdateError::BadSignatureFormat)?.trim();
    if text.is_empty() {
        return Err(UpdateError::Unsigned);
    }
    let fields: Vec<&str> = text.split_ascii_whitespace().collect();
    let [tag, id, sig] = fields.as_slice() else { return Err(UpdateError::BadSignatureFormat) };
    if *tag != SIGNATURE_TAG || id.len() != 16 || from_hex(id).is_none() {
        return Err(UpdateError::BadSignatureFormat);
    }
    let sig: [u8; 64] = from_hex(sig).and_then(|v| v.try_into().ok()).ok_or(UpdateError::BadSignatureFormat)?;
    Ok((id.to_ascii_lowercase(), sig))
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian), so an expiry needs no date crate.
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = i64::from(y) - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl TrustedKey {
    /// The first second (Unix time, UTC) at which the key is no longer accepted: the day after `expires`.
    pub fn expires_at_unix(&self) -> i64 {
        let (y, m, d) = self.expires;
        (days_from_civil(y, m, d) + 1) * 86_400
    }

    fn expiry_text(&self) -> String {
        let (y, m, d) = self.expires;
        format!("{y:04}-{m:02}-{d:02}")
    }
}

/// Seconds since 1970 by this PC's clock (0 if the clock is before 1970, which makes nothing expire early).
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Checks the signature file over the exact `manifest_json` bytes against `keys` at time `now_unix`, and only
/// then parses the manifest. Refused: no signature, an unknown key, an expired key, a wrong signature. Returns the
/// manifest and the key that signed it.
pub fn verify_signed_manifest<'k>(
    manifest_json: &[u8],
    signature_file: &[u8],
    keys: &'k [TrustedKey],
    now_unix: i64,
) -> Result<(Manifest, &'k TrustedKey)> {
    let (id, sig) = parse_signature(signature_file)?;
    let mut found = None;
    for k in keys {
        let public = public_key_from_hex(k.public_hex)?;
        if key_id(&public) == id {
            found = Some((k, public));
            break;
        }
    }
    let (trusted, public) = found.ok_or(UpdateError::UnknownKey(id))?;
    if now_unix >= trusted.expires_at_unix() {
        return Err(UpdateError::ExpiredKey { name: trusted.name.to_string(), expired: trusted.expiry_text() });
    }
    let key = VerifyingKey::from_bytes(&public).map_err(|_| UpdateError::BadKey)?;
    key.verify_strict(manifest_json, &Signature::from_bytes(&sig)).map_err(|_| UpdateError::BadSignature)?;
    let manifest = serde_json::from_slice(manifest_json).map_err(|e| UpdateError::BadJson(e.to_string()))?;
    Ok((manifest, trusted))
}

/// The two trust modes of the in-game update, decided by `keys` (normally [`TRUSTED_KEYS`]):
///
/// - **Unsigned (no key listed; every release before 1.0.0, owner decision 2026-10-22).** The manifest is taken as
///   GitHub served it over HTTPS (certificate checked) from the repository's own release; a signature file, if one
///   is there, is ignored. What protects the player is then the release page itself plus the hash chain the caller
///   must check: the zip against this manifest and against the published `.sha256` ([`verify_zip`],
///   [`verify_sha256_file`]), every extracted file against this manifest ([`extract_verified`]), the version
///   ([`check_manifest_version`], strictly newer), the file names ([`check_auto_names`]). This does NOT protect
///   against someone who can publish a release on the repository (a stolen GitHub account or token).
/// - **Signed (at least one key listed).** The signature is required and checked as by [`verify_signed_manifest`];
///   a release without `manifest.json.sig` is refused ([`UpdateError::Unsigned`]).
pub fn accept_manifest(
    manifest_json: &[u8],
    signature_file: Option<&[u8]>,
    keys: &[TrustedKey],
    now_unix: i64,
) -> Result<Manifest> {
    if keys.is_empty() {
        return serde_json::from_slice(manifest_json).map_err(|e| UpdateError::BadJson(e.to_string()));
    }
    let sig = signature_file.ok_or(UpdateError::Unsigned)?;
    verify_signed_manifest(manifest_json, sig, keys, now_unix).map(|(m, _)| m)
}

/// The published `FeverLink3-<version>.zip.sha256` (`<hash>  <zip name>`, as `feverlink3_release package` writes
/// it) must name the manifest's zip and agree with the manifest's hash and with the downloaded zip's.
pub fn verify_sha256_file(text: &[u8], zip: &[u8], manifest: &Manifest) -> Result<()> {
    let text = std::str::from_utf8(text).map_err(|_| UpdateError::ZipHashMismatch)?;
    let mut fields = text.split_ascii_whitespace();
    let (Some(hash), name) = (fields.next(), fields.next()) else { return Err(UpdateError::ZipHashMismatch) };
    let name_ok = name.is_none_or(|n| n.trim_start_matches('*') == manifest.zip_name);
    let zip_hash = sha256_hex(zip);
    if !name_ok || !hash.eq_ignore_ascii_case(manifest.zip_sha256.trim()) || !hash.eq_ignore_ascii_case(&zip_hash) {
        return Err(UpdateError::ZipHashMismatch);
    }
    Ok(())
}

/// The folder of the mod in an overlay release and in the game folder.
pub const AUTO_MOD_DIR: &str = "mods/release/feverlink3";

/// The names an automatic update may carry: `winhttp.dll`, files under `mods/release/feverlink3/` and the
/// [`OVERLAY_DOCS`] (checked, never installed); each also passes [`safe_rel_path`]. Anything else (another mod's
/// folder, a program, a portable release) refuses the whole update.
pub fn check_auto_names(manifest: &Manifest) -> Result<()> {
    let prefix = format!("{AUTO_MOD_DIR}/");
    if !manifest.files.iter().any(|f| f.path == DLL_NAME) {
        return Err(UpdateError::UnknownLayout);
    }
    for f in &manifest.files {
        safe_rel_path(&f.path)?;
        let ok = f.path == DLL_NAME || f.path.starts_with(&prefix) || OVERLAY_DOCS.contains(&f.path.as_str());
        if !ok {
            return Err(UpdateError::UnsafePath(f.path.clone()));
        }
    }
    Ok(())
}

/// A signed manifest must describe the release it was published with: the same version and the same zip.
pub fn check_manifest_version(manifest: &Manifest, release: &Release) -> Result<()> {
    match Version::parse(&manifest.version) {
        Ok(v) if v == release.version => {}
        _ => {
            return Err(UpdateError::ManifestVersionMismatch {
                manifest: manifest.version.clone(),
                release: release.tag.clone(),
            })
        }
    }
    if manifest.zip_name != release.zip.name {
        return Err(UpdateError::ZipNameMismatch {
            manifest: manifest.zip_name.clone(),
            release: release.zip.name.clone(),
        });
    }
    Ok(())
}

/// The downloaded zip against the signed manifest's SHA-256.
pub fn verify_zip(zip: &[u8], manifest: &Manifest) -> Result<()> {
    if sha256_hex(zip).eq_ignore_ascii_case(manifest.zip_sha256.trim()) {
        Ok(())
    } else {
        Err(UpdateError::ZipHashMismatch)
    }
}

// ----------------------------------------------------------------------------------------------- swap plan

/// A zip entry name made safe: forward slashes only, no empty, `.`, `..` or drive components, no `:`, no
/// trailing dot or space, not absolute.
pub fn safe_rel_path(name: &str) -> Result<String> {
    let unsafe_path = || UpdateError::UnsafePath(name.to_string());
    if name.is_empty() || name.contains('\\') || name.contains('\0') || name.starts_with('/') {
        return Err(unsafe_path());
    }
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(':') || part.ends_with(['.', ' ']) {
            return Err(unsafe_path());
        }
    }
    // Belt and braces: the platform's own parser must see only plain names.
    if Path::new(name).components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(unsafe_path());
    }
    Ok(name.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Extract the zip entry `entry` (full name) into `staged`.
    Extract { entry: String, rel: String, staged: PathBuf },
    /// Rename an existing file out of the way (`name` to `name.old`).
    Retire { from: PathBuf, to: PathBuf },
    /// Move a staged file into place.
    Install { from: PathBuf, to: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapPlan {
    pub install_dir: PathBuf,
    pub staging_dir: PathBuf,
    /// Steps in order: all extractions, then retire + install per file (the exe last).
    pub steps: Vec<Step>,
}

fn within(dir: &Path, path: &Path) -> bool {
    path.starts_with(dir)
}

/// What a release zip holds, which decides where it is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layout {
    /// `feverlink3.exe` with the DLL and the mod beside it: installed into the folder of the running exe.
    Portable,
    /// `winhttp.dll` and `mods/...`, the files a player copies into the game folder: installed into the game folder,
    /// only while no game runs (Windows locks the DLL, and a game must never see half an update).
    Overlay,
}

/// The documents an overlay release carries beside the game files. They are covered by the signed zip hash but
/// never copied into the game folder (the game has its own files there, `LICENSE` among them perhaps).
pub const OVERLAY_DOCS: &[&str] = &["README.txt", "HOW-TO-PLAY.txt", "LICENSE", "THIRD_PARTY_NOTICES.md"];

/// Whether a file of the release is put into the install folder: everything of a portable release; of an overlay
/// only the DLL and the files under `mods/`.
fn installs(layout: Layout, rel: &str) -> bool {
    layout == Layout::Portable || rel == DLL_NAME || rel.starts_with(OVERLAY_MOD_PREFIX)
}

/// The layout a manifest describes. A portable release has the exe; an overlay has the DLL and otherwise only
/// files under `mods/` and the [`OVERLAY_DOCS`]. Anything else is refused.
pub fn layout_of(manifest: &Manifest) -> Result<Layout> {
    let has = |name: &str| manifest.files.iter().any(|f| f.path == name);
    if has(EXE_NAME) {
        Ok(Layout::Portable)
    } else if has(DLL_NAME)
        && manifest.files.iter().all(|f| installs(Layout::Overlay, &f.path) || OVERLAY_DOCS.contains(&f.path.as_str()))
    {
        Ok(Layout::Overlay)
    } else {
        Err(UpdateError::UnknownLayout)
    }
}

/// Validates every zip entry and builds the plan; performs no file operation. Rules: every name is under the
/// release's single top folder `FeverLink3-<version>/`, passes [`safe_rel_path`], appears in the manifest, and
/// no manifest file is missing from the zip; the layout is known. The program file (the exe of a portable release,
/// the DLL of an overlay) is swapped last. The documents of an overlay are checked but not installed.
pub fn plan_swap(zip: &[u8], manifest: &Manifest, install_dir: &Path) -> Result<SwapPlan> {
    let layout = layout_of(manifest)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(zip)).map_err(|e| UpdateError::BadZip(e.to_string()))?;
    let top = format!("FeverLink3-{}/", manifest.version);
    let listed: BTreeSet<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    let staging_dir = install_dir.join(STAGING_DIR);
    let mut seen = BTreeSet::new();
    let mut extracts = Vec::new();
    for i in 0..archive.len() {
        let file = archive.by_index(i).map_err(|e| UpdateError::BadZip(e.to_string()))?;
        let name = file.name().map_err(|e| UpdateError::BadZip(e.to_string()))?.into_owned();
        if file.is_dir() {
            continue;
        }
        let rel = name.strip_prefix(&top).ok_or_else(|| UpdateError::UnsafePath(name.clone()))?;
        let rel = safe_rel_path(rel).map_err(|_| UpdateError::UnsafePath(name.clone()))?;
        if !listed.contains(rel.as_str()) {
            return Err(UpdateError::NotInManifest(rel));
        }
        if !seen.insert(rel.to_ascii_lowercase()) {
            return Err(UpdateError::UnsafePath(format!("duplicate entry {name}")));
        }
        if installs(layout, &rel) {
            let staged = staging_dir.join(&rel);
            extracts.push(Step::Extract { entry: name, rel, staged });
        }
    }
    for f in &manifest.files {
        if !seen.contains(&f.path.to_ascii_lowercase()) {
            return Err(UpdateError::MissingFromZip(f.path.clone()));
        }
    }
    let last = match layout {
        Layout::Portable => EXE_NAME,
        Layout::Overlay => DLL_NAME,
    };

    let mut steps = extracts.clone();
    let mut order: Vec<&str> = listed.iter().copied().filter(|p| *p != last && installs(layout, p)).collect();
    order.push(last);
    for rel in order {
        let rel = safe_rel_path(rel)?;
        let target = install_dir.join(&rel);
        let staged = staging_dir.join(&rel);
        if !within(install_dir, &target) || !within(&staging_dir, &staged) {
            return Err(UpdateError::UnsafePath(rel));
        }
        let mut old = target.clone().into_os_string();
        old.push(".old");
        steps.push(Step::Retire { from: target.clone(), to: PathBuf::from(old) });
        steps.push(Step::Install { from: staged, to: target });
    }
    Ok(SwapPlan { install_dir: install_dir.to_path_buf(), staging_dir, steps })
}

/// Performs the `Extract` steps, checking each file's size and SHA-256 against the manifest while reading
/// (a lying size header cannot make it write more than the manifest allows).
pub fn stage(plan: &SwapPlan, zip: &[u8], manifest: &Manifest) -> Result<()> {
    extract_verified(plan, zip, manifest, &plan.staging_dir)
}

/// The `Extract` steps of `plan` into `dest` (`dest/<rel>`) instead of the plan's staging folder, with the same size
/// and SHA-256 check of every file against the manifest. The in-game update stages next to the game with it.
pub fn extract_verified(plan: &SwapPlan, zip: &[u8], manifest: &Manifest, dest: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip)).map_err(|e| UpdateError::BadZip(e.to_string()))?;
    for step in &plan.steps {
        let Step::Extract { entry, rel, .. } = step else { continue };
        let staged = dest.join(safe_rel_path(rel)?);
        let want =
            manifest.files.iter().find(|f| &f.path == rel).ok_or_else(|| UpdateError::NotInManifest(rel.clone()))?;
        let mut file = archive.by_name(entry).map_err(|e| UpdateError::BadZip(e.to_string()))?;
        let mut data = Vec::new();
        (&mut file).take(want.size + 1).read_to_end(&mut data)?;
        if data.len() as u64 != want.size || !sha256_hex(&data).eq_ignore_ascii_case(&want.sha256) {
            return Err(UpdateError::FileMismatch(rel.clone()));
        }
        if let Some(parent) = staged.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&staged, &data)?;
    }
    Ok(())
}

/// What [`apply`] changed: enough to undo it later ([`rollback`]) or to make it final ([`confirm`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    /// (file, its `.old` copy) for every file that existed before.
    pub retired: Vec<(PathBuf, PathBuf)>,
    /// Every file put in place, in order.
    pub installed: Vec<PathBuf>,
}

/// Performs the `Retire` and `Install` steps. On failure everything done so far is undone and the error is
/// returned. The staging folder is removed afterwards (best effort).
pub fn apply(plan: &SwapPlan) -> Result<Applied> {
    let mut done = Applied::default();
    let mut result = Ok(());
    for step in &plan.steps {
        let r: std::io::Result<()> = match step {
            Step::Extract { .. } => Ok(()),
            Step::Retire { from, to } => {
                if !from.exists() {
                    Ok(())
                } else {
                    let _ = fs::remove_file(to);
                    fs::rename(from, to).map(|()| done.retired.push((from.clone(), to.clone())))
                }
            }
            Step::Install { from, to } => (|| {
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(from, to)?;
                done.installed.push(to.clone());
                Ok(())
            })(),
        };
        if let Err(e) = r {
            result = Err(UpdateError::from(e));
            break;
        }
    }
    if result.is_err() {
        let _ = undo(&done);
    }
    let _ = fs::remove_dir_all(&plan.staging_dir);
    result.map(|()| done)
}

/// Removes every installed file, then moves every `.old` copy back. Keeps going past a failure and reports the
/// first one, so as much as possible is restored.
fn undo(done: &Applied) -> Result<()> {
    let mut first = Ok(());
    let mut note = |r: std::io::Result<()>| {
        if let (Err(e), true) = (r, first.is_ok()) {
            first = Err(UpdateError::from(e));
        }
    };
    for path in done.installed.iter().rev() {
        match fs::remove_file(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            r => note(r),
        }
    }
    for (from, old) in done.retired.iter().rev() {
        note(fs::rename(old, from));
    }
    first
}

// -------------------------------------------------------------------------------- journal, start check, rollback

/// An applied update that is not confirmed yet. Written to [`JOURNAL_FILE`] in the install folder right after the
/// swap; removed by [`confirm`] or [`rollback`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub from: String,
    pub to: String,
    pub layout: Layout,
    pub install_dir: PathBuf,
    pub applied: Applied,
}

impl Journal {
    pub fn path(install_dir: &Path) -> PathBuf {
        install_dir.join(JOURNAL_FILE)
    }

    pub fn write(&self) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| UpdateError::BadJson(e.to_string()))?;
        fs::write(Self::path(&self.install_dir), json)?;
        Ok(())
    }

    /// The journal of `install_dir`, `None` when there is none.
    pub fn read(install_dir: &Path) -> Result<Option<Journal>> {
        match fs::read(Self::path(install_dir)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| UpdateError::BadJson(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// Makes an update final: the journal is deleted, then the `.old` copies (best effort: the old launcher that ran
/// the update is still running from `feverlink3.exe.old` and Windows does not let it be deleted; the next update
/// replaces such a leftover). Returns the copies that could not be deleted.
pub fn confirm(journal: &Journal) -> Result<Vec<PathBuf>> {
    fs::remove_file(Journal::path(&journal.install_dir))?;
    Ok(journal
        .applied
        .retired
        .iter()
        .map(|(_, old)| old)
        .filter(|old| fs::remove_file(old).is_err() && old.exists())
        .cloned()
        .collect())
}

/// Puts the previous version back from its `.old` copies, deletes the journal and adds the version to the skip
/// list, so it is not offered again on this PC.
pub fn rollback(journal: &Journal) -> Result<()> {
    undo(&journal.applied)?;
    add_skipped(&journal.install_dir, &journal.to)?;
    let _ = fs::remove_file(Journal::path(&journal.install_dir));
    Ok(())
}

/// What the start check of a freshly installed launcher saw: it is started with `update --probe` and must print
/// `feverlink3 <version>` and exit with 0 within the time limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Exited { code: Option<i32>, stdout: String },
    TimedOut,
    NotStarted(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterStart {
    Keep,
    RollBack(String),
}

/// Keep the new version only when it started, said it is exactly the expected version and exited cleanly.
pub fn judge_start(expected: &str, probe: &Probe) -> AfterStart {
    let Ok(want) = Version::parse(expected) else { return AfterStart::RollBack(format!("bad version {expected}")) };
    match probe {
        Probe::NotStarted(why) => AfterStart::RollBack(format!("the new version did not start: {why}")),
        Probe::TimedOut => AfterStart::RollBack("the new version did not finish its start check in time".into()),
        Probe::Exited { code, stdout } => {
            let says =
                stdout.lines().find_map(|l| l.trim().strip_prefix("feverlink3 ")).map(|s| (s, Version::parse(s)));
            match (code, says) {
                (Some(0), Some((_, Ok(v)))) if v == want => AfterStart::Keep,
                (Some(0), Some((said, Ok(_)))) => {
                    AfterStart::RollBack(format!("the new program says it is {said}, expected {expected}"))
                }
                (Some(0), _) => AfterStart::RollBack("the new version did not report its version".into()),
                (code, _) => AfterStart::RollBack(format!("the new version exited with {code:?}")),
            }
        }
    }
}

/// What to do with a journal found before a new update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// No unfinished update.
    Clear,
    /// A portable update whose start check never ended (the updater was closed or crashed): undo it.
    RollBackInterrupted,
    /// An overlay update waits for the player to start the game and then confirm or roll back; nothing new is
    /// installed on top of it.
    AwaitingConfirmation,
}

pub fn pending(journal: Option<&Journal>) -> Pending {
    match journal.map(|j| j.layout) {
        None => Pending::Clear,
        Some(Layout::Portable) => Pending::RollBackInterrupted,
        Some(Layout::Overlay) => Pending::AwaitingConfirmation,
    }
}

/// Never update while a game runs (the DLL is loaded in it, and a live session runs inside it) or while another
/// launcher process runs (a `host` or `join` session, or a second updater).
pub fn may_update(game_running: bool, other_launchers: usize) -> Result<()> {
    if game_running {
        return Err(UpdateError::Busy("Transport Fever 3 is running; close the game first".into()));
    }
    if other_launchers > 0 {
        return Err(UpdateError::Busy(format!(
            "{other_launchers} other FeverLink3 program(s) running (a session?); close them first"
        )));
    }
    Ok(())
}

/// True when `version` is on the skip list text (one version per line).
pub fn is_skipped(skip_list: &str, version: &Version) -> bool {
    skip_list.lines().filter_map(|l| Version::parse(l).ok()).any(|v| &v == version)
}

/// The skip list of `install_dir` (empty when there is none).
pub fn read_skipped(install_dir: &Path) -> String {
    fs::read_to_string(install_dir.join(SKIP_FILE)).unwrap_or_default()
}

/// Puts `version` on the skip list of `install_dir` (once).
pub fn add_skipped(install_dir: &Path, version: &str) -> Result<()> {
    let mut list = read_skipped(install_dir);
    if Version::parse(version).is_ok_and(|v| is_skipped(&list, &v)) {
        return Ok(());
    }
    if !list.is_empty() && !list.ends_with('\n') {
        list.push('\n');
    }
    list.push_str(version);
    list.push('\n');
    fs::write(install_dir.join(SKIP_FILE), list)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::io::Write;

    /// Throwaway test seed (it was the private half of the former placeholder key, so it is public and must never
    /// be trusted: see `the_public_test_key_is_never_trusted`).
    const TEST_SEED_HEX: &str = "821878745124e1701fbf9b5e07d284d4524b37a76896352660a1e5a3eaca2cec";
    const TEST_PUBLIC_HEX: &str = "a61541546aecbc2d029027e3242983994e8ccaf2413d963089dc7f4919f3c348";

    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&from_hex(TEST_SEED_HEX).unwrap().try_into().unwrap())
    }

    fn other_key() -> SigningKey {
        SigningKey::from_bytes(&[9u8; 32])
    }

    /// The signature file the release tool writes, made with `key`.
    fn sig_file(key: &SigningKey, data: &[u8]) -> String {
        signature_text(&key.verifying_key().to_bytes(), &key.sign(data).to_bytes())
    }

    const TEST_TRUSTED: TrustedKey =
        TrustedKey { name: "test key", public_hex: TEST_PUBLIC_HEX, expires: (2030, 6, 30) };

    /// 2026-10-19 12:00 UTC.
    const NOW: i64 = 1_792_411_200;

    const RELEASE_JSON: &str = r#"{
      "url": "https://api.github.com/repos/AZalshehri7/FeverLink3/releases/1",
      "tag_name": "v1.2.0",
      "name": "FeverLink3 1.2.0",
      "draft": false,
      "prerelease": false,
      "body": "SHA-256: abc",
      "assets": [
        {"name": "FeverLink3-1.2.0.zip", "size": 2048, "browser_download_url": "https://github.com/AZalshehri7/FeverLink3/releases/download/v1.2.0/FeverLink3-1.2.0.zip"},
        {"name": "FeverLink3-1.2.0.zip.sha256", "size": 90, "browser_download_url": "https://github.com/AZalshehri7/FeverLink3/releases/download/v1.2.0/FeverLink3-1.2.0.zip.sha256"},
        {"name": "manifest.json", "size": 700, "browser_download_url": "https://github.com/AZalshehri7/FeverLink3/releases/download/v1.2.0/manifest.json"},
        {"name": "manifest.json.sig", "size": 129, "browser_download_url": "https://github.com/AZalshehri7/FeverLink3/releases/download/v1.2.0/manifest.json.sig"}
      ]
    }"#;

    #[test]
    fn test_public_key_matches_the_test_seed() {
        assert_eq!(test_key().verifying_key().to_bytes(), public_key_from_hex(TEST_PUBLIC_HEX).unwrap());
    }

    #[test]
    fn the_public_test_key_is_never_trusted_and_the_list_is_well_formed() {
        let test_id = key_id(&public_key_from_hex(TEST_PUBLIC_HEX).unwrap());
        let mut ids = BTreeSet::new();
        for k in TRUSTED_KEYS {
            let public = public_key_from_hex(k.public_hex).expect("64 hex characters");
            VerifyingKey::from_bytes(&public).expect("a valid Ed25519 point");
            assert_ne!(key_id(&public), test_id, "{}: the test key's private half is public", k.name);
            assert!(ids.insert(key_id(&public)), "{}: listed twice", k.name);
            let (y, m, d) = k.expires;
            assert!((1..=12).contains(&m) && (1..=31).contains(&d) && y >= 2026, "{}: bad expiry", k.name);
        }
    }

    #[test]
    fn civil_dates_to_unix_time() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2026, 10, 19) * 86_400, 1_792_368_000);
        let k = TrustedKey { name: "k", public_hex: TEST_PUBLIC_HEX, expires: (2026, 10, 19) };
        assert_eq!(k.expires_at_unix(), 1_792_368_000 + 86_400, "the whole last day is still valid");
        assert!(NOW < k.expires_at_unix());
    }

    #[test]
    fn parses_release_and_picks_assets() {
        let (r, flagged) = parse_release(RELEASE_JSON).unwrap();
        assert!(!flagged);
        assert_eq!(r.tag, "v1.2.0");
        assert_eq!(r.version, Version::parse("1.2.0").unwrap());
        assert_eq!(r.zip.name, "FeverLink3-1.2.0.zip");
        assert_eq!(r.zip.size, 2048);
        assert_eq!(r.manifest.name, "manifest.json");
        let sig = r.signature.as_ref().unwrap();
        assert_eq!(sig.name, "manifest.json.sig");
        assert!(sig.url.ends_with("/manifest.json.sig"));
        assert_eq!(r.sha256.as_ref().unwrap().name, "FeverLink3-1.2.0.zip.sha256");
    }

    #[test]
    fn asset_problems_are_errors() {
        // unsigned until 1.0.0: a release without the signature is read, without it
        let no_sig = RELEASE_JSON.replace("manifest.json.sig", "other.txt");
        assert_eq!(parse_release(&no_sig).unwrap().0.signature, None);
        let no_manifest = RELEASE_JSON.replace("\"manifest.json\"", "\"other.json\"");
        assert_eq!(parse_release(&no_manifest).unwrap_err(), UpdateError::MissingAsset("manifest.json"));
        let http = RELEASE_JSON.replacen(
            "https://github.com/AZalshehri7/FeverLink3/releases/download/v1.2.0/FeverLink3-1.2.0.zip\"",
            "http://x/FeverLink3-1.2.0.zip\"",
            1,
        );
        assert!(matches!(parse_release(&http), Err(UpdateError::InsecureUrl(_))));
        let two = RELEASE_JSON.replace("FeverLink3-1.2.0.zip.sha256", "FeverLink3-1.2.0-b.zip");
        assert_eq!(parse_release(&two).unwrap_err(), UpdateError::AmbiguousAsset("FeverLink3-*.zip"));
        assert!(matches!(parse_release("not json"), Err(UpdateError::BadJson(_))));
    }

    #[test]
    fn version_order() {
        let v = |s: &str| Version::parse(s).unwrap();
        assert!(v("1.2.0") > v("1.1.9"));
        assert!(v("1.10.0") > v("1.9.0"));
        assert!(v("2.0.0") > v("1.99.99"));
        assert!(v("1.0.0-rc.1") < v("1.0.0"), "pre-release is lower");
        assert!(v("1.0.0-alpha") < v("1.0.0-beta"));
        assert!(v("1.0.0-rc.2") < v("1.0.0-rc.10"));
        assert!(v("1.0.0-1") < v("1.0.0-alpha"));
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert_eq!(v("v1.0.0+build5"), v("1.0.0"));
        for bad in ["", "1", "1.2", "1.2.x", "01.2.3", "1.2.3-", "1.2.3-a..b", "1.2.3.4"] {
            assert!(Version::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn update_decisions() {
        assert!(matches!(check_update("1.1.0", RELEASE_JSON, false), Ok(Decision::Available(r)) if r.tag == "v1.2.0"));
        assert_eq!(check_update("1.2.0", RELEASE_JSON, false), Ok(Decision::UpToDate));
        assert_eq!(check_update("1.3.0", RELEASE_JSON, false), Ok(Decision::UpToDate));
        // installed pre-release of the same version is older than the release
        assert!(matches!(check_update("1.2.0-rc.1", RELEASE_JSON, false), Ok(Decision::Available(_))));
        let pre_flag = RELEASE_JSON.replace("\"prerelease\": false", "\"prerelease\": true");
        assert_eq!(check_update("1.0.0", &pre_flag, false), Ok(Decision::UpToDate));
        assert!(matches!(check_update("1.0.0", &pre_flag, true), Ok(Decision::Available(_))));
        let pre_tag = RELEASE_JSON.replace("v1.2.0", "v1.3.0-beta.1");
        assert_eq!(check_update("1.0.0", &pre_tag, false), Ok(Decision::UpToDate));
        let draft = RELEASE_JSON.replace("\"draft\": false", "\"draft\": true");
        assert_eq!(check_update("1.0.0", &draft, true), Ok(Decision::UpToDate));
    }

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opt = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            w.start_file(*name, opt).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn manifest_for(version: &str, files: &[(&str, &[u8])], zip: &[u8]) -> Manifest {
        Manifest {
            version: version.into(),
            files: files
                .iter()
                .map(|(p, d)| FileEntry { path: p.to_string(), size: d.len() as u64, sha256: sha256_hex(d) })
                .collect(),
            zip_name: format!("FeverLink3-{version}.zip"),
            zip_sha256: sha256_hex(zip),
        }
    }

    const FILES: &[(&str, &[u8])] =
        &[("feverlink3.exe", b"new exe"), ("mod/feverlink3/mod.json", b"{}"), ("winhttp.dll", b"dll")];

    fn good_zip() -> (Vec<u8>, Manifest) {
        let entries: Vec<(String, &[u8])> = FILES.iter().map(|(p, d)| (format!("FeverLink3-1.2.0/{p}"), *d)).collect();
        let refs: Vec<(&str, &[u8])> = entries.iter().map(|(p, d)| (p.as_str(), *d)).collect();
        let zip = make_zip(&refs);
        let m = manifest_for("1.2.0", FILES, &zip);
        (zip, m)
    }

    fn signed_manifest() -> (Manifest, Vec<u8>) {
        let (_, m) = good_zip();
        (m.clone(), serde_json::to_vec_pretty(&m).unwrap())
    }

    #[test]
    fn a_good_signature_by_a_trusted_key_is_accepted() {
        let (m, json) = signed_manifest();
        let sig = sig_file(&test_key(), &json);
        assert!(sig.starts_with("feverlink3-ed25519 ") && sig.ends_with('\n'));
        let (got, key) = verify_signed_manifest(&json, sig.as_bytes(), &[TEST_TRUSTED], NOW).unwrap();
        assert_eq!(got, m);
        assert_eq!(key.name, "test key");
        // a second, unrelated trusted key in the list changes nothing (rotation overlap)
        let other = TrustedKey {
            name: "next key",
            public_hex: Box::leak(to_hex(other_key().verifying_key().as_bytes()).into_boxed_str()),
            expires: (2031, 1, 1),
        };
        assert_eq!(verify_signed_manifest(&json, sig.as_bytes(), &[other, TEST_TRUSTED], NOW).unwrap().0, m);
    }

    #[test]
    fn a_bad_signature_is_refused() {
        let (_, json) = signed_manifest();
        let sig = sig_file(&test_key(), &json);
        let keys = [TEST_TRUSTED];
        let mut tampered = json.clone();
        tampered[20] ^= 1;
        assert_eq!(verify_signed_manifest(&tampered, sig.as_bytes(), &keys, NOW), Err(UpdateError::BadSignature));
        // a signature of other bytes under the right key id
        let wrong = sig_file(&test_key(), b"something else");
        assert_eq!(verify_signed_manifest(&json, wrong.as_bytes(), &keys, NOW), Err(UpdateError::BadSignature));
        // one flipped bit in the signature itself
        let (id, mut raw) = parse_signature(sig.as_bytes()).unwrap();
        raw[3] ^= 1;
        let flipped = format!("{SIGNATURE_TAG} {id} {}", to_hex(&raw));
        assert_eq!(verify_signed_manifest(&json, flipped.as_bytes(), &keys, NOW), Err(UpdateError::BadSignature));
    }

    #[test]
    fn unsigned_and_malformed_signature_files_are_refused() {
        let (_, json) = signed_manifest();
        let keys = [TEST_TRUSTED];
        for empty in [&b""[..], b"\n", b"  \r\n"] {
            assert_eq!(verify_signed_manifest(&json, empty, &keys, NOW), Err(UpdateError::Unsigned));
        }
        let sig = sig_file(&test_key(), &json);
        let bare_hex = sig.split_whitespace().nth(2).unwrap().to_string();
        for bad in [
            bare_hex.clone(),                                       // the old format: no tag, no key id
            sig.replace(SIGNATURE_TAG, "minisign"),                 // another tag
            sig.replacen(' ', "  x ", 1),                           // four fields
            format!("{SIGNATURE_TAG} abcd {bare_hex}"),             // short key id
            format!("{SIGNATURE_TAG} zzzzzzzzzzzzzzzz {bare_hex}"), // not hex
            sig.trim_end()[..sig.trim_end().len() - 2].to_string(), // short signature
        ] {
            assert_eq!(
                verify_signed_manifest(&json, bad.as_bytes(), &keys, NOW),
                Err(UpdateError::BadSignatureFormat),
                "{bad:?}"
            );
        }
        assert_eq!(verify_signed_manifest(&json, &[0xff, 0xfe], &keys, NOW), Err(UpdateError::BadSignatureFormat));
    }

    #[test]
    fn a_signature_by_an_untrusted_key_is_refused() {
        let (_, json) = signed_manifest();
        let sig = sig_file(&other_key(), &json);
        let id = key_id(&other_key().verifying_key().to_bytes());
        assert_eq!(
            verify_signed_manifest(&json, sig.as_bytes(), &[TEST_TRUSTED], NOW),
            Err(UpdateError::UnknownKey(id))
        );
        // with no trusted key at all (the state of the repository until the owner adds one) nothing is accepted
        let good = sig_file(&test_key(), &json);
        assert!(matches!(verify_signed_manifest(&json, good.as_bytes(), &[], NOW), Err(UpdateError::UnknownKey(_))));
        assert!(matches!(
            verify_signed_manifest(&json, good.as_bytes(), TRUSTED_KEYS, NOW),
            Err(UpdateError::UnknownKey(_))
        ));
        // a signature by another key that claims the trusted key's id
        let forged = format!(
            "{SIGNATURE_TAG} {} {}",
            key_id(&test_key().verifying_key().to_bytes()),
            to_hex(&other_key().sign(&json).to_bytes())
        );
        assert_eq!(
            verify_signed_manifest(&json, forged.as_bytes(), &[TEST_TRUSTED], NOW),
            Err(UpdateError::BadSignature)
        );
    }

    #[test]
    fn a_signature_by_an_expired_key_is_refused() {
        let (_, json) = signed_manifest();
        let sig = sig_file(&test_key(), &json);
        let old = TrustedKey { expires: (2026, 10, 18), ..TEST_TRUSTED };
        assert_eq!(
            verify_signed_manifest(&json, sig.as_bytes(), &[old], NOW),
            Err(UpdateError::ExpiredKey { name: "test key".into(), expired: "2026-10-18".into() })
        );
        // the last day itself is valid, the next second is not
        let today = TrustedKey { expires: (2026, 10, 19), ..TEST_TRUSTED };
        assert!(verify_signed_manifest(&json, sig.as_bytes(), &[today], NOW).is_ok());
        assert!(verify_signed_manifest(&json, sig.as_bytes(), &[today], today.expires_at_unix()).is_err());
    }

    #[test]
    fn manifest_version_and_zip_must_match_release() {
        let (release, _) = parse_release(RELEASE_JSON).unwrap();
        let (_, mut m) = good_zip();
        check_manifest_version(&m, &release).unwrap();
        m.zip_name = "FeverLink3-1.1.0.zip".into();
        assert!(matches!(check_manifest_version(&m, &release), Err(UpdateError::ZipNameMismatch { .. })));
        m.version = "1.1.0".into();
        assert!(matches!(check_manifest_version(&m, &release), Err(UpdateError::ManifestVersionMismatch { .. })));
    }

    #[test]
    fn zip_hash_rule() {
        let (zip, m) = good_zip();
        verify_zip(&zip, &m).unwrap();
        let mut bad = zip.clone();
        bad[30] ^= 1;
        assert_eq!(verify_zip(&bad, &m), Err(UpdateError::ZipHashMismatch));
    }

    #[test]
    fn trust_modes_unsigned_without_keys_signed_with_keys() {
        let (m, json) = signed_manifest();
        let good = sig_file(&test_key(), &json);
        // unsigned mode (no key listed, until 1.0.0): the manifest is accepted with or without a signature file
        assert_eq!(accept_manifest(&json, None, &[], NOW), Ok(m.clone()));
        assert_eq!(accept_manifest(&json, Some(b"junk"), &[], NOW), Ok(m.clone()));
        assert!(matches!(accept_manifest(b"not json", None, &[], NOW), Err(UpdateError::BadJson(_))));
        // signed mode (a key listed): required and checked
        let keys = [TEST_TRUSTED];
        assert_eq!(accept_manifest(&json, None, &keys, NOW), Err(UpdateError::Unsigned));
        assert_eq!(accept_manifest(&json, Some(b""), &keys, NOW), Err(UpdateError::Unsigned));
        assert_eq!(accept_manifest(&json, Some(good.as_bytes()), &keys, NOW), Ok(m));
        let mut tampered = json.clone();
        tampered[20] ^= 1;
        assert_eq!(accept_manifest(&tampered, Some(good.as_bytes()), &keys, NOW), Err(UpdateError::BadSignature));
        let by_other = sig_file(&other_key(), &json);
        assert!(matches!(
            accept_manifest(&json, Some(by_other.as_bytes()), &keys, NOW),
            Err(UpdateError::UnknownKey(_))
        ));
    }

    #[test]
    fn the_published_sha256_file_must_agree() {
        let (zip, m) = good_zip();
        let line = format!("{}  {}\n", m.zip_sha256, m.zip_name);
        verify_sha256_file(line.as_bytes(), &zip, &m).unwrap();
        verify_sha256_file(m.zip_sha256.to_uppercase().as_bytes(), &zip, &m).unwrap();
        let other = sha256_hex(b"other");
        for bad in
            [format!("{other}  {}", m.zip_name), format!("{}  FeverLink3-9.9.9.zip", m.zip_sha256), String::new()]
        {
            assert_eq!(verify_sha256_file(bad.as_bytes(), &zip, &m), Err(UpdateError::ZipHashMismatch), "{bad:?}");
        }
        // the file and the manifest agree, the downloaded zip does not
        let mut changed = zip.clone();
        changed[30] ^= 1;
        assert_eq!(verify_sha256_file(line.as_bytes(), &changed, &m), Err(UpdateError::ZipHashMismatch));
    }

    #[test]
    fn automatic_updates_carry_only_the_dll_and_our_mod() {
        let entry = |p: &str| FileEntry { path: p.into(), size: 1, sha256: sha256_hex(b"x") };
        let with = |paths: &[&str]| Manifest {
            version: "1.2.0".into(),
            files: paths.iter().map(|p| entry(p)).collect(),
            zip_name: "FeverLink3-1.2.0.zip".into(),
            zip_sha256: String::new(),
        };
        check_auto_names(&with(&["winhttp.dll", "mods/release/feverlink3/mod.lua", "README.txt", "LICENSE"])).unwrap();
        assert_eq!(check_auto_names(&with(&["mods/release/feverlink3/mod.lua"])), Err(UpdateError::UnknownLayout));
        for bad in [
            "mods/release/other/mod.lua",
            "mods/release/feverlink3/../../../evil.dll",
            "mods/release/feverlink3x/mod.lua",
            "feverlink3.exe",
            "TransportFever3.exe",
            "../winhttp.dll",
        ] {
            assert!(check_auto_names(&with(&["winhttp.dll", bad])).is_err(), "{bad}");
        }
    }

    #[test]
    fn path_rules_including_zip_slip() {
        for ok in ["feverlink3.exe", "mod/feverlink3/mod.json"] {
            assert!(safe_rel_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "../evil.dll",
            "a/../../evil",
            "a/..",
            "/abs",
            "C:/evil",
            "C:evil",
            "a\\b",
            "..\\evil",
            "a//b",
            "./a",
            "",
            "a/b:stream",
            "trailing.",
            "a/\0b",
        ] {
            assert!(safe_rel_path(bad).is_err(), "{bad:?}");
        }
    }

    fn plan_for(entries: &[(&str, &[u8])], manifest_files: &[(&str, &[u8])]) -> Result<SwapPlan> {
        let zip = make_zip(entries);
        let m = manifest_for("1.2.0", manifest_files, &zip);
        plan_swap(&zip, &m, Path::new("C:/install"))
    }

    #[test]
    fn zip_slip_and_foreign_entries_are_rejected() {
        let exe: (&str, &[u8]) = ("feverlink3.exe", b"x");
        for evil in [
            "FeverLink3-1.2.0/../evil.dll",
            "FeverLink3-1.2.0/mod/../../evil.dll",
            "../evil.dll",
            "/evil.dll",
            "C:/evil.dll",
            "FeverLink3-1.2.0/C:/evil.dll",
            "FeverLink3-1.2.0\\..\\evil.dll",
            "Other-1.2.0/feverlink3.exe",
        ] {
            let r = plan_for(&[("FeverLink3-1.2.0/feverlink3.exe", b"x"), (evil, b"x")], &[exe, ("evil.dll", b"x")]);
            assert!(matches!(r, Err(UpdateError::UnsafePath(_))), "{evil}: {r:?}");
        }
    }

    #[test]
    fn plan_requires_zip_and_manifest_to_agree() {
        let exe: (&str, &[u8]) = ("feverlink3.exe", b"x");
        // file in zip but not in manifest
        let r = plan_for(&[("FeverLink3-1.2.0/feverlink3.exe", b"x"), ("FeverLink3-1.2.0/extra.dll", b"x")], &[exe]);
        assert_eq!(r, Err(UpdateError::NotInManifest("extra.dll".into())));
        // file in manifest but not in zip
        let r = plan_for(&[("FeverLink3-1.2.0/feverlink3.exe", b"x")], &[exe, ("winhttp.dll", b"d")]);
        assert_eq!(r, Err(UpdateError::MissingFromZip("winhttp.dll".into())));
        // neither an exe nor an overlay (a DLL beside a file outside mods/)
        let r = plan_for(
            &[("FeverLink3-1.2.0/winhttp.dll", b"d"), ("FeverLink3-1.2.0/other.txt", b"t")],
            &[("winhttp.dll", b"d"), ("other.txt", b"t")],
        );
        assert_eq!(r, Err(UpdateError::UnknownLayout));
        let r = plan_for(&[("FeverLink3-1.2.0/mods/a.lua", b"a")], &[("mods/a.lua", b"a")]);
        assert_eq!(r, Err(UpdateError::UnknownLayout));
        // duplicate entry
        let r = plan_for(
            &[("FeverLink3-1.2.0/feverlink3.exe", b"x"), ("FeverLink3-1.2.0/FEVERLINK3.EXE", b"x")],
            &[exe, ("FEVERLINK3.EXE", b"x")],
        );
        assert!(matches!(r, Err(UpdateError::UnsafePath(_))));
        // not a zip
        let m = manifest_for("1.2.0", &[exe], b"");
        assert!(matches!(plan_swap(b"junk", &m, Path::new("C:/install")), Err(UpdateError::BadZip(_))));
    }

    #[test]
    fn plan_stays_inside_install_dir_and_swaps_exe_last() {
        let (zip, m) = good_zip();
        let dir = Path::new("C:/install");
        let plan = plan_swap(&zip, &m, dir).unwrap();
        for step in &plan.steps {
            let paths: Vec<&PathBuf> = match step {
                Step::Extract { staged, .. } => vec![staged],
                Step::Retire { from, to } | Step::Install { from, to } => vec![from, to],
            };
            for p in paths {
                assert!(p.starts_with(dir), "{p:?}");
            }
        }
        let installs: Vec<&PathBuf> =
            plan.steps.iter().filter_map(|s| if let Step::Install { to, .. } = s { Some(to) } else { None }).collect();
        assert_eq!(installs.len(), 3);
        assert_eq!(installs.last().unwrap(), &&dir.join("feverlink3.exe"));
        assert!(plan
            .steps
            .iter()
            .any(|s| matches!(s, Step::Retire { to, .. } if to == &dir.join("feverlink3.exe.old"))));
    }

    #[test]
    fn stage_and_apply_replace_files_and_keep_old() {
        let (zip, m) = good_zip();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("install");
        fs::create_dir_all(dir.join("mod/feverlink3")).unwrap();
        fs::write(dir.join("feverlink3.exe"), b"old exe").unwrap();
        fs::write(dir.join("settings.json"), b"keep me").unwrap();
        fs::write(tmp.path().join("outside.txt"), b"untouched").unwrap();

        let plan = plan_swap(&zip, &m, &dir).unwrap();
        stage(&plan, &zip, &m).unwrap();
        apply(&plan).unwrap();

        assert_eq!(fs::read(dir.join("feverlink3.exe")).unwrap(), b"new exe");
        assert_eq!(fs::read(dir.join("feverlink3.exe.old")).unwrap(), b"old exe");
        assert_eq!(fs::read(dir.join("winhttp.dll")).unwrap(), b"dll");
        assert_eq!(fs::read(dir.join("mod/feverlink3/mod.json")).unwrap(), b"{}");
        assert_eq!(fs::read(dir.join("settings.json")).unwrap(), b"keep me");
        assert_eq!(fs::read(tmp.path().join("outside.txt")).unwrap(), b"untouched");
        assert!(!dir.join(STAGING_DIR).exists());
    }

    #[test]
    fn stage_rejects_content_that_does_not_match_the_manifest() {
        let (zip, mut m) = good_zip();
        m.files[0].sha256 = sha256_hex(b"something else");
        let tmp = tempfile::tempdir().unwrap();
        let plan = plan_swap(&zip, &m, tmp.path()).unwrap();
        assert!(matches!(stage(&plan, &zip, &m), Err(UpdateError::FileMismatch(_))));
        let (zip, mut m) = good_zip();
        m.files[0].size -= 1;
        let plan = plan_swap(&zip, &m, tmp.path()).unwrap();
        assert!(matches!(stage(&plan, &zip, &m), Err(UpdateError::FileMismatch(_))));
    }

    #[test]
    fn failed_apply_rolls_back() {
        let (zip, m) = good_zip();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("feverlink3.exe"), b"old exe").unwrap();
        let plan = plan_swap(&zip, &m, dir).unwrap();
        stage(&plan, &zip, &m).unwrap();
        // Break the last install (the exe): its staged file disappears.
        fs::remove_file(plan.staging_dir.join("feverlink3.exe")).unwrap();
        assert!(apply(&plan).is_err());
        assert_eq!(fs::read(dir.join("feverlink3.exe")).unwrap(), b"old exe");
        assert!(!dir.join("winhttp.dll").exists());
        assert!(!dir.join("feverlink3.exe.old").exists());
    }

    const OVERLAY: &[(&str, &[u8])] = &[("mods/release/feverlink3/mod.lua", b"new mod"), ("winhttp.dll", b"new dll")];

    fn overlay_zip() -> (Vec<u8>, Manifest) {
        let entries: Vec<(String, &[u8])> =
            OVERLAY.iter().map(|(p, d)| (format!("FeverLink3-1.2.0/{p}"), *d)).collect();
        let refs: Vec<(&str, &[u8])> = entries.iter().map(|(p, d)| (p.as_str(), *d)).collect();
        let zip = make_zip(&refs);
        let m = manifest_for("1.2.0", OVERLAY, &zip);
        (zip, m)
    }

    #[test]
    fn layouts() {
        assert_eq!(layout_of(&good_zip().1), Ok(Layout::Portable));
        let (zip, m) = overlay_zip();
        assert_eq!(layout_of(&m), Ok(Layout::Overlay));
        let plan = plan_swap(&zip, &m, Path::new("C:/game")).unwrap();
        let last = plan.steps.iter().rev().find_map(|s| if let Step::Install { to, .. } = s { Some(to) } else { None });
        assert_eq!(last, Some(&Path::new("C:/game").join("winhttp.dll")), "the DLL is swapped last");
        let touched: Vec<String> = plan
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Extract { rel, .. } => Some(rel.clone()),
                Step::Install { to, .. } => Some(to.file_name().unwrap().to_string_lossy().into_owned()),
                Step::Retire { .. } => None,
            })
            .collect();
        assert!(
            !touched.iter().any(|t| t.contains("LICENSE") || t.contains("README")),
            "the documents never go into the game folder: {touched:?}"
        );
    }

    /// An overlay install over an older one, with a journal, as the `update` command leaves it.
    fn applied_overlay(dir: &Path) -> Journal {
        fs::create_dir_all(dir.join("mods/release/feverlink3")).unwrap();
        fs::write(dir.join("winhttp.dll"), b"old dll").unwrap();
        // no old mod.lua: the update adds it
        fs::write(dir.join("TransportFever3.exe"), b"game").unwrap();
        fs::write(dir.join("LICENSE"), b"the game's licence").unwrap();
        let (zip, m) = overlay_zip();
        let plan = plan_swap(&zip, &m, dir).unwrap();
        stage(&plan, &zip, &m).unwrap();
        let applied = apply(&plan).unwrap();
        let journal = Journal {
            from: "1.1.0".into(),
            to: "1.2.0".into(),
            layout: Layout::Overlay,
            install_dir: dir.into(),
            applied,
        };
        journal.write().unwrap();
        journal
    }

    #[test]
    fn rollback_restores_the_previous_version_and_skips_the_failed_one() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let journal = applied_overlay(dir);
        assert_eq!(Journal::read(dir).unwrap().as_ref(), Some(&journal), "the journal round-trips");
        assert_eq!(fs::read(dir.join("winhttp.dll")).unwrap(), b"new dll");

        rollback(&journal).unwrap();
        assert_eq!(fs::read(dir.join("winhttp.dll")).unwrap(), b"old dll");
        assert!(!dir.join("winhttp.dll.old").exists());
        assert!(!dir.join("mods/release/feverlink3/mod.lua").exists(), "a file the update added is removed");
        assert_eq!(fs::read(dir.join("TransportFever3.exe")).unwrap(), b"game", "nothing else is touched");
        assert_eq!(Journal::read(dir).unwrap(), None);
        assert!(is_skipped(&read_skipped(dir), &Version::parse("1.2.0").unwrap()));
        assert!(!is_skipped(&read_skipped(dir), &Version::parse("1.2.1").unwrap()));
    }

    #[test]
    fn confirm_keeps_the_new_version_and_drops_the_old_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let journal = applied_overlay(dir);
        assert!(dir.join("winhttp.dll.old").exists());
        assert_eq!(confirm(&journal).unwrap(), Vec::<PathBuf>::new());
        assert_eq!(fs::read(dir.join("winhttp.dll")).unwrap(), b"new dll");
        assert_eq!(fs::read(dir.join("mods/release/feverlink3/mod.lua")).unwrap(), b"new mod");
        assert_eq!(fs::read(dir.join("LICENSE")).unwrap(), b"the game's licence");
        assert!(!dir.join("README.txt").exists());
        assert!(!dir.join("winhttp.dll.old").exists());
        assert_eq!(Journal::read(dir).unwrap(), None);
        assert_eq!(read_skipped(dir), "");
    }

    #[test]
    fn start_check_decides_keep_or_rollback() {
        let exited = |code, out: &str| Probe::Exited { code, stdout: out.into() };
        assert_eq!(judge_start("1.2.0", &exited(Some(0), "feverlink3 1.2.0\n")), AfterStart::Keep);
        assert_eq!(judge_start("1.2.0-pre1", &exited(Some(0), "noise\r\nfeverlink3 1.2.0-pre1\r\n")), AfterStart::Keep);
        for (probe, why) in [
            (exited(Some(0), "feverlink3 1.1.0\n"), "an older program is still in place"),
            (exited(Some(0), "feverlink3 1.2.0-pre1\n"), "a pre-release is not the release"),
            (exited(Some(0), ""), "it said nothing"),
            (exited(Some(0), "feverlink3 garbage\n"), "an unreadable version"),
            (exited(Some(1), "feverlink3 1.2.0\n"), "a failure exit"),
            (exited(None, "feverlink3 1.2.0\n"), "killed"),
            (Probe::TimedOut, "hung"),
            (Probe::NotStarted("not a valid Win32 application".into()), "not startable"),
        ] {
            assert!(matches!(judge_start("1.2.0", &probe), AfterStart::RollBack(_)), "{why}");
        }
    }

    #[test]
    fn a_pending_journal_blocks_or_undoes_before_a_new_update() {
        let j = |layout| Journal {
            from: "1.1.0".into(),
            to: "1.2.0".into(),
            layout,
            install_dir: "C:/x".into(),
            applied: Applied::default(),
        };
        assert_eq!(pending(None), Pending::Clear);
        assert_eq!(pending(Some(&j(Layout::Portable))), Pending::RollBackInterrupted);
        assert_eq!(pending(Some(&j(Layout::Overlay))), Pending::AwaitingConfirmation);
    }

    #[test]
    fn never_during_a_game_or_a_session() {
        assert_eq!(may_update(false, 0), Ok(()));
        assert!(matches!(may_update(true, 0), Err(UpdateError::Busy(_))));
        assert!(matches!(may_update(false, 1), Err(UpdateError::Busy(_))));
        assert!(matches!(may_update(true, 2), Err(UpdateError::Busy(_))));
    }
}
