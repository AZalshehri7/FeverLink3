//! The Win32 resources of `feverlink3.exe`: icon, version information and application manifest, written as a
//! `.res` file in pure Rust.
//!
//! Why not `rc.exe`: it belongs to the Windows SDK, and a release must build from nothing but Rust and the MSVC
//! tools. `link.exe` takes a `.res` file as it is and turns it into an object with `cvtres.exe`, which ships with the
//! MSVC tools, so writing the file here is all that is needed. The format is the one `rc.exe` writes: a 32-byte empty
//! record, then one record per resource (sizes, a type and a name that are ordinals here, language, data), every
//! record padded to a multiple of four bytes.
//!
//! This file is shared: `build.rs` includes it to write the resources, and `main.rs` includes it in test builds so
//! that its tests run under `cargo test`. Each user needs only part of it, hence the `allow`.

#![allow(dead_code)]

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
const RT_MANIFEST: u16 = 24;
/// The id Windows looks for the manifest of a program under.
const MANIFEST_ID: u16 = 1;
/// English (United States), what `rc.exe` and the Microsoft toolchain use unless told otherwise.
const LANGUAGE: u16 = 0x0409;
/// Unicode, the code page of the string table of the version information.
const CODE_PAGE: u16 = 0x04B0;

/// What the version information says. The numbers are the four parts of `FileVersion` and `ProductVersion`.
pub struct VersionInfo {
    pub version: [u16; 4],
    /// A pre-release carries the `VS_FF_PRERELEASE` flag, which the file properties show.
    pub prerelease: bool,
    /// The string table: `(name, text)`, for example `("ProductName", "FeverLink3")`.
    pub strings: Vec<(String, String)>,
}

fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn align4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn length32(data: &[u8]) -> Result<u32, String> {
    u32::try_from(data.len()).map_err(|_| "a resource larger than 4 GiB".to_owned())
}

/// The record every `.res` file starts with: no data, type 0, name 0.
fn empty_record() -> Vec<u8> {
    let mut out = Vec::new();
    put32(&mut out, 0); // data size
    put32(&mut out, 32); // header size
    put16(&mut out, 0xFFFF);
    put16(&mut out, 0);
    put16(&mut out, 0xFFFF);
    put16(&mut out, 0);
    out.resize(32, 0); // data version, memory flags, language, version, characteristics
    out
}

/// Appends one resource whose type and name are ordinals.
fn record(out: &mut Vec<u8>, kind: u16, name: u16, data: &[u8]) -> Result<(), String> {
    put32(out, length32(data)?);
    put32(out, 32); // header size: two sizes, two ordinal pairs, data version, flags, language, version, characteristics
    put16(out, 0xFFFF);
    put16(out, kind);
    put16(out, 0xFFFF);
    put16(out, name);
    put32(out, 0); // data version
    put16(out, 0x0030); // memory flags: moveable and pure
    put16(out, LANGUAGE);
    put32(out, 0); // version
    put32(out, 0); // characteristics
    out.extend_from_slice(data);
    align4(out);
    Ok(())
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

/// The images of an `.ico` file: for each, the directory entry's width, height, colour count, planes and bit count,
/// and the image bytes (a bitmap without file header, or a PNG).
struct IconImage<'a> {
    entry: [u8; 4],
    planes: u16,
    bit_count: u16,
    data: &'a [u8],
}

fn parse_ico(ico: &[u8]) -> Result<Vec<IconImage<'_>>, String> {
    let bad = || "the icon file is not a valid .ico".to_owned();
    if u16_at(ico, 0) != Some(0) || u16_at(ico, 2) != Some(1) {
        return Err(bad());
    }
    let count = usize::from(u16_at(ico, 4).ok_or_else(bad)?);
    if count == 0 {
        return Err(bad());
    }
    (0..count)
        .map(|i| {
            let at = 6 + 16 * i;
            let entry = ico.get(at..at + 16).ok_or_else(bad)?;
            let size = usize::try_from(u32_at(entry, 8).ok_or_else(bad)?).map_err(|_| bad())?;
            let offset = usize::try_from(u32_at(entry, 12).ok_or_else(bad)?).map_err(|_| bad())?;
            let data = ico.get(offset..offset.checked_add(size).ok_or_else(bad)?).ok_or_else(bad)?;
            Ok(IconImage {
                entry: [entry[0], entry[1], entry[2], entry[3]],
                planes: u16_at(entry, 4).ok_or_else(bad)?,
                bit_count: u16_at(entry, 6).ok_or_else(bad)?,
                data,
            })
        })
        .collect()
}

/// The icon resources of an `.ico`: one `RT_ICON` per image (numbered from 1) and the `RT_GROUP_ICON` that lists
/// them, which is what Explorer looks for. The group is the first icon group of the file, so it is the program's icon.
fn add_icon(out: &mut Vec<u8>, ico: &[u8]) -> Result<(), String> {
    let images = parse_ico(ico)?;
    let mut group = Vec::new();
    put16(&mut group, 0);
    put16(&mut group, 1);
    put16(&mut group, u16::try_from(images.len()).map_err(|_| "too many icon images".to_owned())?);
    for (index, image) in images.iter().enumerate() {
        let id = u16::try_from(index + 1).map_err(|_| "too many icon images".to_owned())?;
        record(out, RT_ICON, id, image.data)?;
        group.extend_from_slice(&image.entry);
        put16(&mut group, image.planes);
        put16(&mut group, image.bit_count);
        put32(&mut group, length32(image.data)?);
        put16(&mut group, id);
    }
    record(out, RT_GROUP_ICON, 1, &group)
}

/// One node of a version resource: `wLength`, `wValueLength`, `wType`, the key as UTF-16, the value, the children.
/// The length covers the node and its children but not the padding after the last child; each child starts on a
/// four-byte boundary, which is how a reader finds the next sibling.
fn version_node(
    key: &str,
    kind: u16,
    value_length: u16,
    value: &[u8],
    children: &[Vec<u8>],
) -> Result<Vec<u8>, String> {
    let mut node = vec![0u8; 6];
    for unit in key.encode_utf16().chain([0]) {
        put16(&mut node, unit);
    }
    align4(&mut node);
    node.extend_from_slice(value);
    for child in children {
        align4(&mut node);
        node.extend_from_slice(child);
    }
    let length = u16::try_from(node.len()).map_err(|_| "a version node longer than 64 KiB".to_owned())?;
    node[0..2].copy_from_slice(&length.to_le_bytes());
    node[2..4].copy_from_slice(&value_length.to_le_bytes());
    node[4..6].copy_from_slice(&kind.to_le_bytes());
    Ok(node)
}

/// A `String` node: the text is the value, `wValueLength` counts characters including the terminating NUL.
fn version_string(name: &str, text: &str) -> Result<Vec<u8>, String> {
    let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
    let length = u16::try_from(units.len()).map_err(|_| "a version string longer than 32767 characters".to_owned())?;
    version_node(name, 1, length, &bytes, &[])
}

/// The `VS_VERSIONINFO` resource.
fn version_data(info: &VersionInfo) -> Result<Vec<u8>, String> {
    let [major, minor, patch, build] = info.version;
    let (ms, ls) = ((u32::from(major) << 16) | u32::from(minor), (u32::from(patch) << 16) | u32::from(build));
    let mut fixed = Vec::new();
    put32(&mut fixed, 0xFEEF_04BD); // signature
    put32(&mut fixed, 0x0001_0000); // structure version
    put32(&mut fixed, ms);
    put32(&mut fixed, ls);
    put32(&mut fixed, ms); // product version
    put32(&mut fixed, ls);
    put32(&mut fixed, 0x3F); // flags mask
    put32(&mut fixed, if info.prerelease { 0x2 } else { 0 }); // VS_FF_PRERELEASE
    put32(&mut fixed, 0x0004_0004); // VOS_NT_WINDOWS32
    put32(&mut fixed, 1); // VFT_APP
    put32(&mut fixed, 0); // subtype
    put32(&mut fixed, 0); // date
    put32(&mut fixed, 0);

    let strings = info.strings.iter().map(|(name, text)| version_string(name, text)).collect::<Result<Vec<_>, _>>()?;
    let table = version_node(&format!("{LANGUAGE:04X}{CODE_PAGE:04X}"), 1, 0, &[], &strings)?;
    let string_file_info = version_node("StringFileInfo", 1, 0, &[], &[table])?;
    let mut translation = Vec::new();
    put16(&mut translation, LANGUAGE);
    put16(&mut translation, CODE_PAGE);
    let var = version_node("Translation", 0, 4, &translation, &[])?;
    let var_file_info = version_node("VarFileInfo", 1, 0, &[], &[var])?;
    version_node("VS_VERSION_INFO", 0, 52, &fixed, &[string_file_info, var_file_info])
}

/// The whole `.res` file: the icon (when there is one), the version information and the manifest.
pub fn resource_file(ico: Option<&[u8]>, version: &VersionInfo, manifest: &str) -> Result<Vec<u8>, String> {
    let mut out = empty_record();
    if let Some(ico) = ico {
        add_icon(&mut out, ico)?;
    }
    record(&mut out, RT_VERSION, 1, &version_data(version)?)?;
    record(&mut out, RT_MANIFEST, MANIFEST_ID, manifest.as_bytes())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    /// One record read back from a `.res` file.
    #[derive(Debug, PartialEq, Eq)]
    struct Record {
        kind: u16,
        name: u16,
        language: u16,
        data: Vec<u8>,
    }

    /// Reads a `.res` file back the way `cvtres` does, checking the layout on the way: the empty first record, the
    /// header size, ordinal types and names, every record starting on a multiple of four.
    fn read_records(res: &[u8]) -> Vec<Record> {
        assert_eq!(res.get(..32), Some(&empty_record()[..]), "the empty record comes first");
        let mut at = 32;
        let mut records = Vec::new();
        while at < res.len() {
            assert_eq!(at % 4, 0, "a record starts on a four-byte boundary");
            let size = u32_at(res, at).unwrap() as usize;
            assert_eq!(u32_at(res, at + 4), Some(32), "header size");
            assert_eq!((u16_at(res, at + 8), u16_at(res, at + 12)), (Some(0xFFFF), Some(0xFFFF)), "ordinals");
            let data_at = at + 32;
            records.push(Record {
                kind: u16_at(res, at + 10).unwrap(),
                name: u16_at(res, at + 14).unwrap(),
                language: u16_at(res, at + 22).unwrap(),
                data: res[data_at..data_at + size].to_vec(),
            });
            at = (data_at + size + 3) & !3;
        }
        assert_eq!(at, res.len(), "no bytes after the last record");
        records
    }

    /// A node of a version resource as a reader sees it.
    #[derive(Debug)]
    struct Node {
        key: String,
        kind: u16,
        value: Vec<u8>,
        value_length: u16,
        children: Vec<Node>,
    }

    /// Reads the node starting at `at`, with its children. A child ends where its `wLength` says; the next sibling
    /// starts at the next multiple of four.
    fn read_node(bytes: &[u8], at: usize) -> (Node, usize) {
        assert_eq!(at % 4, 0, "a node starts on a four-byte boundary");
        let length = u16_at(bytes, at).unwrap() as usize;
        let value_length = u16_at(bytes, at + 2).unwrap();
        let kind = u16_at(bytes, at + 4).unwrap();
        let end = at + length;
        let mut cursor = at + 6;
        let mut key_units = Vec::new();
        while let Some(unit) = u16_at(bytes, cursor) {
            cursor += 2;
            if unit == 0 {
                break;
            }
            key_units.push(unit);
        }
        cursor = (cursor + 3) & !3;
        // text values count characters, binary ones bytes
        let value_bytes = if kind == 1 { usize::from(value_length) * 2 } else { usize::from(value_length) };
        let value = bytes[cursor..cursor + value_bytes].to_vec();
        cursor += value_bytes;
        let mut children = Vec::new();
        while (cursor + 3) & !3 < end {
            cursor = (cursor + 3) & !3;
            let (child, child_end) = read_node(bytes, cursor);
            children.push(child);
            cursor = child_end;
        }
        assert_eq!(cursor, end, "the children end exactly where the node ends");
        (Node { key: String::from_utf16(&key_units).unwrap(), kind, value, value_length, children }, end)
    }

    fn info() -> VersionInfo {
        VersionInfo {
            version: [1, 2, 3, 0],
            prerelease: false,
            strings: vec![
                ("ProductName".into(), "FeverLink3".into()),
                ("FileDescription".into(), "FeverLink3 launcher".into()),
                ("LegalCopyright".into(), "Copyright (c) the contributors, \u{0639}\u{0631}\u{0628}\u{064A}".into()),
            ],
        }
    }

    /// An `.ico` with the given images (width, height, bytes).
    fn make_ico(images: &[(u8, u8, &[u8])]) -> Vec<u8> {
        let mut out = vec![0, 0, 1, 0];
        out.extend_from_slice(&(images.len() as u16).to_le_bytes());
        let mut offset = 6 + 16 * images.len();
        for (w, h, data) in images {
            out.extend_from_slice(&[*w, *h, 0, 0, 1, 0, 32, 0]);
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += data.len();
        }
        for (_, _, data) in images {
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn a_res_file_starts_with_the_empty_record() {
        let res = resource_file(None, &info(), "<assembly/>").unwrap();
        assert_eq!(&res[..8], [0, 0, 0, 0, 0x20, 0, 0, 0]);
        assert_eq!(&res[8..16], [0xFF, 0xFF, 0, 0, 0xFF, 0xFF, 0, 0]);
        assert!(res[16..32].iter().all(|b| *b == 0));
    }

    #[test]
    fn the_resources_are_version_and_manifest_without_an_icon() {
        let records = read_records(&resource_file(None, &info(), "<assembly/>").unwrap());
        let kinds: Vec<(u16, u16)> = records.iter().map(|r| (r.kind, r.name)).collect();
        assert_eq!(kinds, [(RT_VERSION, 1), (RT_MANIFEST, 1)]);
        assert!(records.iter().all(|r| r.language == 0x0409));
        assert_eq!(records[1].data, b"<assembly/>");
    }

    #[test]
    fn an_icon_becomes_numbered_images_and_one_group() {
        let small = vec![7u8; 9]; // not a multiple of four on purpose
        let large = vec![9u8; 20];
        let ico = make_ico(&[(16, 16, &small), (0, 0, &large)]);
        let records = read_records(&resource_file(Some(&ico), &info(), "m").unwrap());
        let kinds: Vec<(u16, u16)> = records.iter().map(|r| (r.kind, r.name)).collect();
        assert_eq!(kinds, [(RT_ICON, 1), (RT_ICON, 2), (RT_GROUP_ICON, 1), (RT_VERSION, 1), (RT_MANIFEST, 1)]);
        assert_eq!(records[0].data, small);
        assert_eq!(records[1].data, large);
        let group = &records[2].data;
        assert_eq!(group.len(), 6 + 14 * 2);
        assert_eq!((u16_at(group, 0), u16_at(group, 2), u16_at(group, 4)), (Some(0), Some(1), Some(2)));
        // entry 1: 16x16, planes 1, 32 bit, 9 bytes, image id 1
        assert_eq!(&group[6..10], [16, 16, 0, 0]);
        assert_eq!(
            (u16_at(group, 10), u16_at(group, 12), u32_at(group, 14), u16_at(group, 18)),
            (Some(1), Some(32), Some(9), Some(1))
        );
        // entry 2: 256x256 is stored as 0x0, 20 bytes, image id 2
        assert_eq!(&group[20..24], [0, 0, 0, 0]);
        assert_eq!((u32_at(group, 28), u16_at(group, 32)), (Some(20), Some(2)));
    }

    #[test]
    fn a_broken_icon_is_refused() {
        let good = make_ico(&[(16, 16, &[1, 2, 3, 4])]);
        assert!(resource_file(Some(&good), &info(), "m").is_ok());
        let mut wrong_type = good.clone();
        wrong_type[2] = 2; // a cursor, not an icon
        let mut no_images = good.clone();
        no_images[4] = 0;
        let mut image_past_end = good.clone();
        image_past_end[6 + 8] = 200; // the image is claimed to be 200 bytes long
        let mut huge_offset = good.clone();
        huge_offset[6 + 12..6 + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        for broken in [&b""[..], &b"MZ"[..], &good[..10], &wrong_type, &no_images, &image_past_end, &huge_offset] {
            assert!(resource_file(Some(broken), &info(), "m").is_err(), "{broken:?}");
        }
    }

    #[test]
    fn the_version_information_reads_back() {
        let records = read_records(&resource_file(None, &info(), "m").unwrap());
        let data = &records[0].data;
        let (root, end) = read_node(data, 0);
        assert_eq!(end, data.len(), "the root covers the whole resource");
        assert_eq!((root.key.as_str(), root.kind, root.value_length), ("VS_VERSION_INFO", 0, 52));

        let fixed = &root.value;
        assert_eq!(u32_at(fixed, 0), Some(0xFEEF_04BD));
        assert_eq!(u32_at(fixed, 4), Some(0x0001_0000));
        assert_eq!(
            (u32_at(fixed, 8), u32_at(fixed, 12)),
            (Some(0x0001_0002), Some(0x0003_0000)),
            "file version 1.2.3.0"
        );
        assert_eq!((u32_at(fixed, 16), u32_at(fixed, 20)), (Some(0x0001_0002), Some(0x0003_0000)), "product version");
        assert_eq!((u32_at(fixed, 24), u32_at(fixed, 28)), (Some(0x3F), Some(0)), "mask, no pre-release flag");
        assert_eq!((u32_at(fixed, 32), u32_at(fixed, 36)), (Some(0x0004_0004), Some(1)), "NT, application");

        let names: Vec<&str> = root.children.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(names, ["StringFileInfo", "VarFileInfo"]);
        let table = &root.children[0].children[0];
        assert_eq!(table.key, "040904B0");
        let strings: BTreeMap<String, String> = table
            .children
            .iter()
            .map(|s| {
                let units: Vec<u16> = s.value.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                assert_eq!(units.last(), Some(&0), "text ends with a NUL that wValueLength counts");
                (s.key.clone(), String::from_utf16(&units[..units.len() - 1]).unwrap())
            })
            .collect();
        assert_eq!(strings["ProductName"], "FeverLink3");
        assert_eq!(strings["FileDescription"], "FeverLink3 launcher");
        assert_eq!(strings["LegalCopyright"], "Copyright (c) the contributors, \u{0639}\u{0631}\u{0628}\u{064A}");
        assert_eq!(strings.len(), 3);

        let translation = &root.children[1].children[0];
        assert_eq!(translation.key, "Translation");
        assert_eq!(translation.value, [0x09, 0x04, 0xB0, 0x04], "English (US), Unicode");
    }

    #[test]
    fn a_pre_release_sets_its_flag() {
        let mut pre = info();
        pre.prerelease = true;
        let records = read_records(&resource_file(None, &pre, "m").unwrap());
        let (root, _) = read_node(&records[0].data, 0);
        assert_eq!(u32_at(&root.value, 28), Some(0x2));
    }

    #[test]
    fn the_output_is_the_same_every_time() {
        let ico = make_ico(&[(16, 16, &[1, 2, 3])]);
        assert_eq!(resource_file(Some(&ico), &info(), "m").unwrap(), resource_file(Some(&ico), &info(), "m").unwrap());
    }

    #[test]
    fn the_committed_icon_and_manifest_make_a_valid_resource_file() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources");
        let ico = std::fs::read(dir.join("feverlink3.ico")).unwrap();
        let manifest = std::fs::read_to_string(dir.join("feverlink3.manifest")).unwrap();
        let images = parse_ico(&ico).unwrap();
        assert!(images.len() >= 4, "several sizes");
        assert!(images.iter().any(|i| i.entry[0] == 16) && images.iter().any(|i| i.entry[0] == 32));
        assert!(images.iter().any(|i| i.entry[0] == 0), "a 256 pixel image, stored as 0");

        let records = read_records(&resource_file(Some(&ico), &info(), &manifest).unwrap());
        assert_eq!(records.iter().filter(|r| r.kind == RT_ICON).count(), images.len());
        assert_eq!(records.iter().filter(|r| r.kind == RT_GROUP_ICON).count(), 1);
        let stored = records.iter().find(|r| r.kind == RT_MANIFEST).unwrap();
        assert_eq!(stored.data, manifest.as_bytes());

        // what Windows needs of the manifest: well-formed, run as the invoking user, no BOM
        assert!(manifest.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>"));
        assert!(manifest.trim_end().ends_with("</assembly>"));
        assert!(manifest.contains("requestedExecutionLevel level=\"asInvoker\""));
        assert!(manifest.contains("{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"), "Windows 10 and 11");
        let opens = manifest.matches('<').count();
        let closes = manifest.matches('>').count();
        assert_eq!(opens, closes, "every tag is closed");
    }
}
