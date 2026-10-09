//! Just enough PE parsing to identify the game build (link timestamp) and locate `.text` for signature scans.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub rva: u32,
    pub size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headers {
    pub timestamp: u32,
    pub size_of_headers: u32,
    pub sections: Vec<Section>,
}

impl Headers {
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Parses the DOS, COFF, optional (PE32+) and section headers from the first bytes of an image.
pub fn parse(image: &[u8]) -> Option<Headers> {
    if image.get(0..2)? != b"MZ" {
        return None;
    }
    let pe = u32_at(image, 0x3c)? as usize;
    if image.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let coff = pe + 4;
    let sections = u16_at(image, coff + 2)? as usize;
    let timestamp = u32_at(image, coff + 4)?;
    let opt_size = u16_at(image, coff + 16)? as usize;
    let opt = coff + 20;
    if u16_at(image, opt)? != 0x20b {
        return None; // PE32+ only: the game is 64-bit
    }
    let size_of_headers = u32_at(image, opt + 60)?;
    let mut out = Vec::with_capacity(sections);
    let mut at = opt + opt_size;
    for _ in 0..sections {
        image.get(at..at + 40)?; // a whole section header must be present
        let raw = &image[at..at + 8];
        let end = raw.iter().position(|&c| c == 0).unwrap_or(8);
        let name = String::from_utf8_lossy(&raw[..end]).into_owned();
        out.push(Section { name, size: u32_at(image, at + 8)?, rva: u32_at(image, at + 12)? });
        at += 40;
    }
    Some(Headers { timestamp, size_of_headers, sections: out })
}

/// Reads the headers of a mapped image.
///
/// # Safety
/// `base` must be the base address of a module mapped in this process; the first page is read, then up to
/// `SizeOfHeaders` bytes, which the loader always maps.
#[cfg(windows)]
pub unsafe fn read_headers(base: *const u8) -> Option<Headers> {
    let first = unsafe { core::slice::from_raw_parts(base, 0x1000) };
    let probe = parse(first)?;
    let len = (probe.size_of_headers as usize).max(0x1000);
    let all = unsafe { core::slice::from_raw_parts(base, len) };
    parse(all)
}

#[cfg(test)]
pub mod test_image {
    /// Builds a minimal PE32+ header block: one way to test the parser without a real executable.
    pub fn build(timestamp: u32, sections: &[(&str, u32, u32)]) -> Vec<u8> {
        let mut v = vec![0u8; 0x40];
        v[0] = b'M';
        v[1] = b'Z';
        let pe = 0x80u32;
        v[0x3c..0x40].copy_from_slice(&pe.to_le_bytes());
        v.resize(pe as usize, 0);
        v.extend_from_slice(b"PE\0\0");
        // COFF
        v.extend_from_slice(&0x8664u16.to_le_bytes()); // machine
        v.extend_from_slice(&(sections.len() as u16).to_le_bytes());
        v.extend_from_slice(&timestamp.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // symtab
        v.extend_from_slice(&0u32.to_le_bytes()); // nsyms
        let opt_size = 240u16;
        v.extend_from_slice(&opt_size.to_le_bytes());
        v.extend_from_slice(&0x22u16.to_le_bytes()); // characteristics
        // optional header (PE32+), 240 bytes
        let opt_start = v.len();
        v.resize(opt_start + opt_size as usize, 0);
        v[opt_start..opt_start + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        v[opt_start + 60..opt_start + 64].copy_from_slice(&0x400u32.to_le_bytes()); // SizeOfHeaders
        for (name, rva, size) in sections {
            let mut sec = [0u8; 40];
            sec[..name.len()].copy_from_slice(name.as_bytes());
            sec[8..12].copy_from_slice(&size.to_le_bytes());
            sec[12..16].copy_from_slice(&rva.to_le_bytes());
            v.extend_from_slice(&sec);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamp_and_sections() {
        let img = test_image::build(0x6701_2345, &[(".text", 0x1000, 0x2f0_0000), (".rdata", 0x2f1_0000, 0x1000)]);
        let h = parse(&img).expect("valid header");
        assert_eq!(h.timestamp, 0x6701_2345);
        assert_eq!(h.size_of_headers, 0x400);
        assert_eq!(h.sections.len(), 2);
        let text = h.section(".text").unwrap();
        assert_eq!((text.rva, text.size), (0x1000, 0x2f0_0000));
        assert!(h.section(".data").is_none());
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        assert!(parse(b"not a pe").is_none());
        let img = test_image::build(1, &[(".text", 0x1000, 10)]);
        assert!(parse(&img[..img.len() - 10]).is_none());
        let mut bad = img.clone();
        bad[0x80] = b'X';
        assert!(parse(&bad).is_none());
    }
}
