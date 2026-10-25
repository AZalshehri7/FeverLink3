//! The game build (from the PE header of `TransportFever3.exe`) and the version of a launcher (from a marker in
//! its data).

use std::cmp::Ordering;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, Prefix};

use feverlink3_launcher::steam;
use feverlink3_launcher::update::Version;

/// What a game executable's PE header says about the build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BuildCheck {
    NoGame,
    Unreadable(String),
    Known { name: &'static str },
    Unknown { timestamp: u32 },
}

/// The game builds that were played and checked (the native module finds the places of other builds too, unconfirmed): the
/// generated list the native module reads as well (`build_table::VERIFIED`; the launcher cannot link the native crate, which
/// links the launcher). Lines `name timestamp hash`, `#` comments.
const VERIFIED: &str = include_str!("../../../tools/addr/out/verified-builds.txt");

fn known_builds() -> impl Iterator<Item = (u32, &'static str)> {
    VERIFIED.lines().filter(|l| !l.starts_with('#')).filter_map(|l| {
        let mut w = l.split_whitespace();
        let name = w.next()?;
        Some((u32::from_str_radix(w.next()?.strip_prefix("0x")?, 16).ok()?, name))
    })
}
/// Where the version of a launcher can be read without running it: this text sits in every launcher's data, the
/// version after the prefix and a NUL after the version. `feverlink3_path.txt` names the launcher that installed
/// FeverLink3; if that file still exists, its marker says which version put the files there.
const MARKER_PREFIX: &str = "feverlink3-launcher-version=";
static VERSION_MARKER: &str = concat!("feverlink3-launcher-version=", env!("CARGO_PKG_VERSION"), "\0");
/// Longest version text read after the marker.
const MAX_VERSION_LEN: usize = 64;
/// Most bytes read from a launcher looking for its marker: a few megabytes are normal, the limit only keeps a
/// wrong path in `feverlink3_path.txt` from making the wizard read something huge.
const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
/// The first page of the game executable holds the DOS and PE headers.
const HEADER_BYTES: u64 = 0x1000;

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

/// The link timestamp of a 64-bit PE image, read from its first bytes: the value `native/src/pe.rs` reads from the
/// running game and `build_table` is keyed by. `None` for anything else.
fn pe_timestamp(image: &[u8]) -> Option<u32> {
    if image.get(..2)? != b"MZ" {
        return None;
    }
    let pe = usize::try_from(u32_at(image, 0x3c)?).ok()?;
    if image.get(pe..pe.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    let coff = pe.checked_add(4)?;
    let timestamp = u32_at(image, coff.checked_add(4)?)?;
    // the optional header follows the 20-byte COFF header; PE32+ (0x20b) only, the game is 64-bit
    if u16_at(image, coff.checked_add(20)?)? != 0x20b {
        return None;
    }
    Some(timestamp)
}

/// The timestamp of the first verified build (tests: a game executable the wizard knows).
#[cfg(test)]
pub(super) fn first_known_build() -> u32 {
    known_builds().next().expect("a verified build").0
}

fn classify_build(timestamp: u32) -> BuildCheck {
    match known_builds().find(|(known, _)| *known == timestamp) {
        Some((_, name)) => BuildCheck::Known { name },
        None => BuildCheck::Unknown { timestamp },
    }
}

pub(super) fn check_build(game_dir: Option<&Path>) -> BuildCheck {
    let Some(dir) = game_dir else { return BuildCheck::NoGame };
    let mut header = Vec::new();
    let read = File::open(steam::game_exe(dir)).and_then(|f| f.take(HEADER_BYTES).read_to_end(&mut header));
    match read {
        Ok(_) => pe_timestamp(&header)
            .map_or_else(|| BuildCheck::Unreadable("not a 64-bit Windows program".into()), classify_build),
        Err(e) => BuildCheck::Unreadable(e.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------------------------------------------

/// The marker, passed through `black_box` so that no optimiser folds the text away: another launcher can only
/// find the version by scanning for it.
fn marker() -> &'static str {
    std::hint::black_box(VERSION_MARKER)
}

/// The version of this launcher, from the same text the scan of another launcher finds.
pub(super) fn own_version() -> &'static str {
    marker().strip_prefix(MARKER_PREFIX).and_then(|v| v.strip_suffix('\0')).unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// Every version a launcher's bytes announce: the text after each occurrence of the marker prefix that parses as a
/// version. The scan constant itself appears in the launcher too, followed by something else, so every occurrence
/// is tried.
fn version_markers(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    let prefix = MARKER_PREFIX.as_bytes();
    bytes.windows(prefix.len()).enumerate().filter(move |(_, window)| *window == prefix).filter_map(move |(at, _)| {
        let rest = bytes.get(at.checked_add(prefix.len())?..)?;
        let len = rest
            .iter()
            .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+')))
            .unwrap_or(rest.len());
        // one byte more than allowed is read, so that a runaway text is refused instead of cut short
        let text = std::str::from_utf8(rest.get(..len.min(MAX_VERSION_LEN + 1))?).ok()?;
        if text.len() > MAX_VERSION_LEN {
            return None;
        }
        Version::parse(text).ok().map(|_| text.to_owned())
    })
}

/// The first version a launcher's bytes announce.
fn find_version_marker(bytes: &[u8]) -> Option<String> {
    version_markers(bytes).next()
}

/// True for `\\server\share\...` (and its `\\?\UNC\` spelling): a network share.
fn is_network_path(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
    )
}

/// The version of the launcher recorded at install time, or `None` when that file is gone or unmarked. A record
/// that names this very executable needs no reading. One on a network share is not read: when the share is gone
/// Windows can take half a minute to say so, on every screen, and that is not worth a version number.
pub(super) fn read_launcher_version(recorded: &Path, own_exe: &Path) -> Option<String> {
    if same_path_text(recorded, own_exe) {
        return Some(own_version().to_owned());
    }
    if is_network_path(recorded) {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(recorded).ok()?.take(MAX_SCAN_BYTES).read_to_end(&mut bytes).ok()?;
    find_version_marker(&bytes)
}

fn same_path_text(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

/// How this folder's version relates to the installed one, when both are known and parse.
pub(super) fn version_order(this: &str, installed: Option<&str>) -> Option<Ordering> {
    Some(Version::parse(this).ok()?.cmp(&Version::parse(installed?).ok()?))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use feverlink3_launcher::GAME_EXE;
    use tempfile::TempDir;

    use super::*;
    use crate::wizard::testkit::pe_image;

    #[test]
    fn the_link_timestamp_is_read_from_a_64_bit_pe_header() {
        assert_eq!(pe_timestamp(&pe_image(0x1234_0427)), Some(0x1234_0427));
        assert_eq!(pe_timestamp(&pe_image(0)), Some(0));
        assert_eq!(pe_timestamp(b""), None);
        assert_eq!(pe_timestamp(b"MZ"), None);
        assert_eq!(pe_timestamp(b"not a program at all, but long enough to hold the fields"), None);
        let mut pe32 = pe_image(7);
        let magic_at = 0x80 + 4 + 20;
        pe32[magic_at..magic_at + 2].copy_from_slice(&0x10bu16.to_le_bytes());
        assert_eq!(pe_timestamp(&pe32), None, "32-bit images are not the game");
        let mut bad_signature = pe_image(7);
        bad_signature[0x80] = b'X';
        assert_eq!(pe_timestamp(&bad_signature), None);
        let mut far_header = pe_image(7);
        far_header[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(pe_timestamp(&far_header), None, "a header offset past the end of the data");
        assert_eq!(pe_timestamp(&pe_image(7)[..0x80 + 6]), None, "cut off inside the COFF header");
    }

    #[test]
    fn a_build_is_known_when_the_native_module_has_addresses_for_it() {
        let (ts, name) = known_builds().next().expect("a verified build");
        assert_eq!(classify_build(ts), BuildCheck::Known { name });
        assert_eq!(classify_build(ts + 1), BuildCheck::Unknown { timestamp: ts + 1 });
        assert_eq!(classify_build(0), BuildCheck::Unknown { timestamp: 0 });
    }

    #[test]
    fn the_build_comes_from_the_game_executable_in_the_game_folder() {
        let dir = TempDir::new().unwrap();
        assert_eq!(check_build(None), BuildCheck::NoGame);
        assert!(matches!(check_build(Some(dir.path())), BuildCheck::Unreadable(_)), "no executable");
        fs::write(dir.path().join(GAME_EXE), b"not a program").unwrap();
        assert!(matches!(check_build(Some(dir.path())), BuildCheck::Unreadable(why) if why.contains("64-bit")));
        let (ts, name) = known_builds().next().unwrap();
        fs::write(dir.path().join(GAME_EXE), pe_image(ts)).unwrap();
        assert_eq!(check_build(Some(dir.path())), BuildCheck::Known { name });
        fs::write(dir.path().join(GAME_EXE), pe_image(0x1234_5678)).unwrap();
        assert_eq!(check_build(Some(dir.path())), BuildCheck::Unknown { timestamp: 0x1234_5678 });
    }

    // -----------------------------------------------------------------------------------------------------------
    // Version
    // -----------------------------------------------------------------------------------------------------------

    #[test]
    fn the_version_marker_is_found_among_other_bytes() {
        let mut bytes = b"MZ\0\0 stuff ".to_vec();
        bytes.extend_from_slice(b"feverlink3-launcher-version=0.3.1-rc.2\0more stuff");
        assert_eq!(find_version_marker(&bytes), Some("0.3.1-rc.2".to_owned()));
        // the scan's own constant appears too, followed by something that is not a version: it is skipped
        let decoy = b"feverlink3-launcher-version=feverlink3-launcher-version=1.2.3\0";
        assert_eq!(find_version_marker(decoy), Some("1.2.3".to_owned()));
        assert_eq!(find_version_marker(b"feverlink3-launcher-version=\0"), None);
        assert_eq!(find_version_marker(b"feverlink3-launcher-version=banana\0"), None);
        assert_eq!(find_version_marker(b"feverlink3-launcher-version=1.2\0"), None);
        assert_eq!(
            find_version_marker(b"feverlink3-launcher-version=1.2.3"),
            Some("1.2.3".to_owned()),
            "ends at the data"
        );
        assert_eq!(find_version_marker(b""), None);
        assert_eq!(find_version_marker(b"no marker here"), None);
        let long = format!("feverlink3-launcher-version=1.2.3-{}\0", "a".repeat(200));
        assert_eq!(find_version_marker(long.as_bytes()), None, "a runaway version is not read");
    }

    #[test]
    fn this_launcher_carries_its_own_marker() {
        assert_eq!(own_version(), env!("CARGO_PKG_VERSION"));
        assert!(VERSION_MARKER.starts_with(MARKER_PREFIX));
        assert!(Version::parse(own_version()).is_ok());
        // what another launcher's wizard would find by reading this very program's file. A test build also holds
        // the sample markers of the tests above, so the real one is looked for among all of them.
        let bytes = fs::read(std::env::current_exe().unwrap()).unwrap();
        assert!(
            version_markers(&bytes).any(|v| v == own_version()),
            "{:?}",
            version_markers(&bytes).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_recorded_launchers_version_is_read_from_its_file() {
        let dir = TempDir::new().unwrap();
        let old = dir.path().join("old").join("feverlink3.exe");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        let mut bytes = vec![0x4d, 0x5a, 0, 1, 2];
        bytes.extend_from_slice(b"feverlink3-launcher-version=0.0.7\0");
        fs::write(&old, bytes).unwrap();
        let me = dir.path().join("new").join("feverlink3.exe");
        assert_eq!(read_launcher_version(&old, &me), Some("0.0.7".to_owned()));
        assert_eq!(read_launcher_version(&dir.path().join("gone.exe"), &me), None, "the folder was deleted");
        let unmarked = dir.path().join("unmarked.exe");
        fs::write(&unmarked, b"an older launcher without the marker").unwrap();
        assert_eq!(read_launcher_version(&unmarked, &me), None);
        // the record names the running launcher: no file is read at all (this path does not exist)
        assert_eq!(read_launcher_version(&me, &me), Some(own_version().to_owned()));
        let shouted = PathBuf::from(me.to_string_lossy().to_uppercase());
        assert_eq!(read_launcher_version(&shouted, &me), Some(own_version().to_owned()), "paths ignore case");
    }

    #[test]
    fn a_launcher_on_a_network_share_is_not_read() {
        for share in [r"\\server\share\FeverLink3\feverlink3.exe", r"\\?\UNC\server\share\feverlink3.exe"] {
            assert!(is_network_path(Path::new(share)), "{share}");
            assert_eq!(read_launcher_version(Path::new(share), Path::new(r"C:\here\feverlink3.exe")), None, "{share}");
        }
        for local in [r"C:\Users\Ann\feverlink3.exe", r"\\?\C:\Users\Ann\feverlink3.exe", r"feverlink3.exe", "/tmp/x"] {
            assert!(!is_network_path(Path::new(local)), "{local}");
        }
    }

    #[test]
    fn versions_are_ordered_when_both_are_known() {
        assert_eq!(version_order("0.2.0", Some("0.1.0")), Some(Ordering::Greater));
        assert_eq!(version_order("0.1.0", Some("0.1.0")), Some(Ordering::Equal));
        assert_eq!(version_order("0.1.0", Some("0.10.0")), Some(Ordering::Less));
        assert_eq!(version_order("1.0.0", Some("1.0.0-rc.1")), Some(Ordering::Greater));
        assert_eq!(version_order("0.1.0", None), None);
        assert_eq!(version_order("0.1.0", Some("garbage")), None);
    }
}
