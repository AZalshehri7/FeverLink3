//! Spike S1 (`FEVERLINK3_PORTABLE_REBUILD`): can a native module recreate a proposal exactly from its portable text?
//!
//! A self-check on one game, no network, nothing handed to the engine. The portable text of a held command
//! ([`crate::portable`]) is parsed and a payload is built again from it, by the same layout table, in memory
//! the module allocates with the game's own allocator (`operator new`; never Rust's, because the game's code frees
//! and walks such memory). The scratch payload itself is a plain Rust buffer the engine never sees. The rebuilt
//! payload is then written again as portable text and compared line by line with the original text, and its words
//! are compared with the original's (pointers excluded). Everything the module cannot create is not guessed but
//! listed as BLOCKED with the reason: that list is the answer to "how far does a native rebuild get".
//!
//! One general rule, as in the decoder: the table drives it for every kind of build; there is no per-tool code.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use crate::dump::{pointer_like, u64_at, Env, Mem, PAYLOAD_SIZE};
use crate::portable::{self, Elem, Header, Settings, Ty, F, PAYLOAD};

pub const REBUILD_VAR: &str = "FEVERLINK3_PORTABLE_REBUILD";
pub const REBUILD_FILE: &str = "native_rebuild.log";

/// `operator new(size_t)` of build 40420 (RVA); the first bytes are compared before it is ever called.
pub const ENGINE_NEW_RVA: u32 = 0x0318_c530;
pub const ENGINE_NEW_PROLOGUE: [u8; 11] = [0x40, 0x53, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0xd9, 0xeb, 0x0f];
/// `operator delete(void*)` (a `jmp` to the CRT's free), RVA and bytes.
pub const ENGINE_DELETE_RVA: u32 = 0x0318_c56c;
pub const ENGINE_DELETE_BYTES: [u8; 5] = [0xe9, 0xff, 0x06, 0x00, 0x00];

/// Where the rebuild gets memory and puts bytes. Methods take `&self`: the real one calls the game, the test one is an arena.
pub trait Alloc {
    /// `n` bytes (at least 1) or `None`.
    fn alloc(&self, n: usize) -> Option<usize>;
    /// Copies into memory this allocator handed out.
    fn write(&self, addr: usize, data: &[u8]) -> bool;
}

/// Something the module cannot create, with the member it concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub kind: &'static str,
    pub path: String,
    pub why: &'static str,
}

pub struct Built {
    pub payload: Vec<u8>,
    pub blocked: Vec<Blocked>,
    pub blocks: usize,
}

type R<T> = Result<T, String>;

struct Builder<'a> {
    lines: HashMap<&'a str, &'a str>,
    base: usize,
    a: &'a dyn Alloc,
    blocked: Vec<Blocked>,
    blocks: usize,
}

fn parse_hex(s: &str) -> R<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("odd hex length: {s}"));
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| format!("{e}: {s}"))).collect()
}

fn hex32(s: &str) -> R<u32> {
    u32::from_str_radix(s, 16).map_err(|e| format!("{e}: {s}"))
}

/// The bytes of a quoted, escaped string ([`portable::escape`]).
pub fn unescape(s: &str) -> R<Vec<u8>> {
    let inner =
        s.strip_prefix('"').and_then(|x| x.strip_suffix('"')).ok_or_else(|| format!("not a quoted string: {s}"))?;
    let b = inner.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            match b.get(i + 1) {
                Some(b'"') => out.push(b'"'),
                Some(b'\\') => out.push(b'\\'),
                Some(b'x') => {
                    out.push(
                        u8::from_str_radix(inner.get(i + 2..i + 4).ok_or("short \\x escape")?, 16)
                            .map_err(|e| e.to_string())?,
                    );
                    i += 2;
                }
                _ => return Err(format!("bad escape in {s}")),
            }
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(out)
}

impl<'a> Builder<'a> {
    fn val(&self, path: &str) -> Option<&'a str> {
        self.lines.get(path).copied()
    }

    fn count(&self, path: &str, suffix: &str) -> R<usize> {
        let key = format!("{path}.{suffix}");
        let v = self.val(&key).ok_or_else(|| format!("missing line {key}"))?;
        v.strip_prefix("n:").and_then(|x| x.parse().ok()).ok_or_else(|| format!("{key}: bad count {v}"))
    }

    fn block(&mut self, path: &str, data: &[u8]) -> R<(u64, u64, u64)> {
        if data.is_empty() {
            return Ok((0, 0, 0));
        }
        let addr =
            self.a.alloc(data.len()).ok_or_else(|| format!("{path}: allocation of {:#x} bytes failed", data.len()))?;
        if !self.a.write(addr, data) {
            return Err(format!("{path}: write into the new block failed"));
        }
        self.blocks += 1;
        Ok((addr as u64, (addr + data.len()) as u64, (addr + data.len()) as u64))
    }

    fn put_triple(buf: &mut [u8], t: (u64, u64, u64)) {
        buf[0..8].copy_from_slice(&t.0.to_le_bytes());
        buf[8..16].copy_from_slice(&t.1.to_le_bytes());
        buf[16..24].copy_from_slice(&t.2.to_le_bytes());
    }

    fn string_into(&mut self, path: &str, text: &[u8], out: &mut [u8]) -> R<()> {
        out[..0x20].fill(0);
        if text.len() <= 15 {
            out[..text.len()].copy_from_slice(text);
            out[0x18..0x20].copy_from_slice(&15u64.to_le_bytes());
        } else {
            let mut t = text.to_vec();
            t.push(0);
            let addr = self.a.alloc(t.len()).ok_or_else(|| format!("{path}: string allocation failed"))?;
            if !self.a.write(addr, &t) {
                return Err(format!("{path}: string write failed"));
            }
            self.blocks += 1;
            out[0..8].copy_from_slice(&(addr as u64).to_le_bytes());
            out[0x18..0x20].copy_from_slice(&((text.len() | 15) as u64).to_le_bytes());
        }
        out[0x10..0x18].copy_from_slice(&(text.len() as u64).to_le_bytes());
        Ok(())
    }

    fn ent(v: &str) -> R<u32> {
        if let Some(n) = v.strip_prefix("ent:new#") {
            return Ok((-(n.parse::<i32>().map_err(|e| e.to_string())?)) as u32);
        }
        let n = v.strip_prefix("ent:old#").ok_or_else(|| format!("not an entity: {v}"))?;
        n.split('@').next().unwrap_or("").parse::<u32>().map_err(|e| format!("{e}: {v}"))
    }

    fn field(&mut self, path: &str, ty: Ty, buf: &mut [u8]) -> R<()> {
        let v = self.val(path);
        match ty {
            Ty::Skip(_) | Ty::Raw(_) => Ok(()),
            Ty::U8 | Ty::U32 | Ty::I32 | Ty::F32 | Ty::Ent | Ty::V3 | Ty::M4 => {
                let v = v.ok_or_else(|| format!("missing line {path}"))?;
                match ty {
                    Ty::U8 => {
                        buf[0] =
                            u8::from_str_radix(v.strip_prefix("u8:").ok_or("u8")?, 16).map_err(|e| e.to_string())?
                    }
                    Ty::U32 | Ty::I32 | Ty::F32 => {
                        let (_, h) = v.split_once(':').ok_or_else(|| format!("{path}: {v}"))?;
                        buf[..4].copy_from_slice(&hex32(h)?.to_le_bytes());
                    }
                    Ty::Ent => buf[..4].copy_from_slice(&Self::ent(v)?.to_le_bytes()),
                    Ty::V3 | Ty::M4 => {
                        let (_, h) = v.split_once(':').ok_or_else(|| format!("{path}: {v}"))?;
                        for (i, w) in h.split(',').enumerate() {
                            buf[i * 4..i * 4 + 4].copy_from_slice(&hex32(w)?.to_le_bytes());
                        }
                    }
                    _ => unreachable!(),
                }
                Ok(())
            }
            Ty::Str => {
                let v = v.ok_or_else(|| format!("missing line {path}"))?;
                let t = unescape(v.strip_prefix("str:").ok_or("str")?)?;
                self.string_into(path, &t, buf)
            }
            Ty::Res => {
                let v = v.ok_or_else(|| format!("missing line {path}"))?;
                let second = unescape(v.strip_prefix("res:").ok_or("res")?)?;
                let first = match self.val(&format!("{path}.first")) {
                    Some(f) => unescape(f.strip_prefix("res:").ok_or("res.first")?)?,
                    None => Vec::new(),
                };
                let (a, b) = buf.split_at_mut(0x20);
                self.string_into(path, &first, a)?;
                self.string_into(path, &second, b)
            }
            Ty::EntVec => {
                let n = self.count(path, "#")?;
                let mut data = Vec::with_capacity(n * 4);
                for i in 0..n {
                    let l = format!("{path}[{i}]");
                    data.extend_from_slice(
                        &Self::ent(self.val(&l).ok_or_else(|| format!("missing line {l}"))?)?.to_le_bytes(),
                    );
                }
                let t = self.block(path, &data)?;
                Self::put_triple(buf, t);
                Ok(())
            }
            Ty::Vec(elem) => {
                let n = self.count(path, "#")?;
                let mut data = vec![0u8; n * elem.stride];
                for i in 0..n {
                    let item = &mut data[i * elem.stride..(i + 1) * elem.stride];
                    self.record(&format!("{path}[{i}]"), item, elem)?;
                }
                let t = self.block(path, &data)?;
                Self::put_triple(buf, t);
                Ok(())
            }
            Ty::BlobVec => {
                let n = self.count(path, "bytes")?;
                let mut data = vec![0u8; n];
                if let Some(raw) = self.val(&format!("{path}.raw")) {
                    let bytes = self.raw_value(&format!("{path}.raw"), raw)?;
                    if bytes.len() != n {
                        return Err(format!("{path}: raw has {} bytes, the vector {n}", bytes.len()));
                    }
                    data = bytes;
                }
                let t = self.block(path, &data)?;
                Self::put_triple(buf, t);
                Ok(())
            }
            Ty::Map => {
                let v = v.ok_or_else(|| format!("missing line {path}"))?;
                let size: u64 = v
                    .split("size=")
                    .nth(1)
                    .and_then(|x| x.split(',').next())
                    .and_then(|x| x.parse().ok())
                    .ok_or_else(|| format!("{path}: {v}"))?;
                buf[8..16].copy_from_slice(&size.to_le_bytes());
                self.blocked.push(Blocked {
                    kind: "std::map",
                    path: path.to_string(),
                    why: "the sentinel head node (size and constructor of the tree node) is not known; entries need red-black linkage",
                });
                Ok(())
            }
            Ty::Hash => {
                let v = v.ok_or_else(|| format!("missing line {path}"))?;
                let size: u64 = v
                    .split("size=")
                    .nth(1)
                    .and_then(|x| x.split(',').next())
                    .and_then(|x| x.parse().ok())
                    .ok_or_else(|| format!("{path}: {v}"))?;
                let load = hex32(v.split("load=").nth(1).ok_or("load")?)?;
                buf[0..4].copy_from_slice(&load.to_le_bytes());
                buf[0x10..0x18].copy_from_slice(&size.to_le_bytes());
                buf[0x30..0x38].copy_from_slice(&7u64.to_le_bytes());
                buf[0x38..0x40].copy_from_slice(&8u64.to_le_bytes());
                self.blocked.push(Blocked {
                    kind: "std::unordered_map",
                    path: path.to_string(),
                    why: "the list sentinel and the 16-slot bucket array are not created (node size and hashing unknown)",
                });
                Ok(())
            }
        }
    }

    /// A record of `elem`.
    fn record(&mut self, path: &str, buf: &mut [u8], elem: &Elem) -> R<()> {
        self.fields(path, buf, elem.fields, elem.stride)
    }

    fn fields(&mut self, path: &str, buf: &mut [u8], fields: &[F], size: usize) -> R<()> {
        let mut order: Vec<&F> = fields.iter().collect();
        order.sort_by_key(|f| f.off);
        let mut cursor = 0usize;
        for fl in order {
            if fl.off > cursor {
                self.gap(path, buf, cursor, fl.off)?;
            }
            let end = fl.off + fl.ty.size();
            let full = if path.is_empty() { fl.name.to_string() } else { format!("{path}.{}", fl.name) };
            if let Ty::Raw(_) = fl.ty {
                self.gap_named(&full, buf, fl.off, end)?;
            } else {
                self.field(&full, fl.ty, &mut buf[fl.off..end])?;
            }
            cursor = end;
        }
        if cursor < size {
            self.gap(path, buf, cursor, size)?;
        }
        Ok(())
    }

    fn gap(&mut self, path: &str, buf: &mut [u8], from: usize, to: usize) -> R<()> {
        let name = if path.is_empty() { format!("~{from:#x}") } else { format!("{path}.~{from:#x}") };
        self.gap_named(&name, buf, from, to)
    }

    fn gap_named(&mut self, name: &str, buf: &mut [u8], from: usize, to: usize) -> R<()> {
        let Some(v) = self.val(name) else { return Ok(()) };
        let bytes = self.raw_value(name, v)?;
        if bytes.len() != to - from {
            return Err(format!("{name}: raw has {:#x} bytes, the gap {:#x}", bytes.len(), to - from));
        }
        buf[from..to].copy_from_slice(&bytes);
        Ok(())
    }

    fn raw_value(&mut self, path: &str, v: &str) -> R<Vec<u8>> {
        let body = v.strip_prefix("raw:").ok_or_else(|| format!("{path}: not a raw value"))?;
        if let Some(h) = body.strip_prefix("b:") {
            return parse_hex(h);
        }
        self.tokens(path, body)
    }

    fn tokens(&mut self, path: &str, s: &str) -> R<Vec<u8>> {
        let b = s.as_bytes();
        let mut out: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b' ' {
                i += 1;
                continue;
            }
            let start = i;
            match b[i] {
                b'z' => {
                    i += 1;
                    let n = Self::number(s, &mut i)?;
                    out.resize(out.len() + n * 8, 0);
                }
                b'w' => {
                    let h = s.get(i + 2..i + 18).ok_or("short w token")?;
                    out.extend_from_slice(&u64::from_str_radix(h, 16).map_err(|e| e.to_string())?.to_le_bytes());
                    i += 18;
                }
                b'S' => {
                    i += 1;
                    let n = Self::number(s, &mut i)?;
                    i += 1; // ':'
                    let end = s[i..].find(' ').map_or(s.len(), |p| i + p);
                    let text = parse_hex(&s[i..end])?;
                    if text.len() != n {
                        return Err(format!("{path}: string token size {n} with {} bytes", text.len()));
                    }
                    let mut w = [0u8; 0x20];
                    self.string_into(path, &text, &mut w)?;
                    out.extend_from_slice(&w);
                    i = end;
                }
                b'V' => {
                    i += 1;
                    let n = Self::number(s, &mut i)?;
                    let (inner, next) = Self::bracketed(s, i)?;
                    let data =
                        if let Some(h) = inner.strip_prefix("b:") { parse_hex(h)? } else { self.tokens(path, inner)? };
                    if data.len() != n {
                        return Err(format!("{path}: vector token of {n} bytes holds {}", data.len()));
                    }
                    let mut t = [0u8; 24];
                    if n == 0 {
                        let a = self.a.alloc(8).ok_or("allocation failed")? as u64;
                        self.blocks += 1;
                        Self::put_triple(&mut t, (a, a, a));
                    } else {
                        let tr = self.block(path, &data)?;
                        Self::put_triple(&mut t, tr);
                    }
                    out.extend_from_slice(&t);
                    i = next;
                }
                b'R' => {
                    i += 1;
                    let n = Self::number(s, &mut i)?;
                    let (inner, next) = Self::bracketed(s, i)?;
                    let data = if inner.is_empty() {
                        Vec::new()
                    } else if let Some(h) = inner.strip_prefix("b:") {
                        parse_hex(h)?
                    } else {
                        self.tokens(path, inner)?
                    };
                    if data.len() != n {
                        return Err(format!("{path}: range token of {n} bytes holds {}", data.len()));
                    }
                    let a = if n == 0 {
                        self.blocks += 1;
                        self.a.alloc(8).ok_or("allocation failed")? as u64
                    } else {
                        self.block(path, &data)?.0
                    };
                    out.extend_from_slice(&a.to_le_bytes());
                    out.extend_from_slice(&(a + n as u64).to_le_bytes());
                    i = next;
                }
                b'I' => {
                    let end = s[i..].find(' ').map_or(s.len(), |p| i + p);
                    let rva = usize::from_str_radix(s.get(i + 2..end).ok_or("short I token")?, 16)
                        .map_err(|e| e.to_string())?;
                    out.extend_from_slice(&((self.base + rva) as u64).to_le_bytes());
                    i = end;
                }
                b'P' => {
                    self.blocked.push(Blocked {
                        kind: "heap pointer",
                        path: path.to_string(),
                        why: "points to an object the portable text does not describe (shared resource data, list or tree nodes, control blocks)",
                    });
                    out.extend_from_slice(&0u64.to_le_bytes());
                    i += 1;
                }
                b'b' => {
                    let end = s[i..].find(' ').map_or(s.len(), |p| i + p);
                    out.extend_from_slice(&parse_hex(s.get(i + 2..end).ok_or("short b token")?)?);
                    i = end;
                }
                c => return Err(format!("{path}: unknown token starting {:?} at {start}", c as char)),
            }
        }
        Ok(out)
    }

    /// The text between the `[` at `at` and its closing `]`, and the index after it.
    fn bracketed(s: &str, at: usize) -> R<(&str, usize)> {
        let b = s.as_bytes();
        if b.get(at) != Some(&b'[') {
            return Err("token without [".into());
        }
        let mut depth = 0usize;
        for (k, &c) in b.iter().enumerate().skip(at) {
            match c {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok((&s[at + 1..k], k + 1));
                    }
                }
                _ => {}
            }
        }
        Err("unclosed [".into())
    }

    fn number(s: &str, i: &mut usize) -> R<usize> {
        let b = s.as_bytes();
        let st = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        s[st..*i].parse().map_err(|_| format!("bad number in token near {st}"))
    }
}

/// Builds a payload from portable text (one command's block). Allocations go through `a`.
pub fn build(text: &str, image_base: usize, a: &dyn Alloc) -> R<Built> {
    let mut lines: HashMap<&str, &str> = HashMap::new();
    let mut header_ok = false;
    for l in text.lines() {
        if l.starts_with("=== portable v") {
            header_ok =
                l.starts_with(&format!("=== portable v{} layout={}", portable::VERSION, portable::LAYOUT_BUILD));
            if !header_ok {
                return Err(format!("not a portable text of this version and layout: {l}"));
            }
        } else if l.starts_with("===") {
            continue;
        } else if let Some((p, v)) = l.split_once('\t') {
            lines.insert(p, v.split('\t').next().unwrap_or(v));
        }
    }
    if !header_ok {
        return Err("no portable header".into());
    }
    let mut b = Builder { lines, base: image_base, a, blocked: Vec::new(), blocks: 0 };
    let mut payload = vec![0u8; PAYLOAD_SIZE];
    b.fields("", &mut payload, PAYLOAD, PAYLOAD_SIZE)?;
    Ok(Built { payload, blocked: b.blocked, blocks: b.blocks })
}

/// The member a portable line belongs to, for the per-member report.
pub fn member_of(path: &str) -> String {
    if path.starts_with('~') {
        return "(unnamed bytes)".into();
    }
    let head = path.split(['[', '#']).next().unwrap_or(path).trim_end_matches('.');
    let head = head.strip_suffix(".bytes").unwrap_or(head);
    let head = head.split(".~").next().unwrap_or(head);
    let mut seg = head.split('.');
    let first = seg.next().unwrap_or("");
    if first == "proposal" || first == "context" {
        match seg.next() {
            Some(s) => format!("{first}.{s}"),
            None => first.to_string(),
        }
    } else {
        first.to_string()
    }
}

fn body(text: &str) -> BTreeMap<&str, &str> {
    text.lines()
        .filter(|l| !l.starts_with("==="))
        .filter_map(|l| l.split_once('\t'))
        .map(|(p, v)| (p, v.split('\t').next().unwrap_or(v)))
        .collect()
}

/// What the self-check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub text: String,
    /// Members whose lines all match / that have at least one differing line.
    pub matched: usize,
    pub differing: usize,
    pub blocked: usize,
    pub exact: bool,
}

/// Rebuilds `original_text` (the portable text of the command at `original_payload`), writes the rebuilt payload as
/// text again and compares. `place` makes the rebuilt payload readable through `mem` and returns its address.
#[allow(clippy::too_many_arguments)]
pub fn roundtrip(
    original_text: &str,
    original_payload: &[u8],
    image_base: usize,
    set: Settings,
    a: &dyn Alloc,
    mem: &dyn Mem,
    place: &dyn Fn(&[u8]) -> Option<usize>,
) -> R<Report> {
    let built = build(original_text, image_base, a)?;
    let at = place(&built.payload).ok_or("the rebuilt payload could not be placed")?;
    let head = Header {
        seq: 0,
        event: "rebuilt",
        hold_id: None,
        site_rva: None,
        tool: None,
        t_ms: 0,
        wall_ms: 0,
        payload: at,
    };
    let again = portable::encode(mem, Env { image_base }, set, portable::LAYOUT_BUILD, &head)
        .map_err(|e| format!("the rebuilt payload cannot be written as text: {e}"))?;
    let (x, y) = (body(original_text), body(&again.text));
    let mut members: BTreeMap<String, (usize, usize, Option<String>)> = BTreeMap::new();
    let mut paths: Vec<&str> = x.keys().chain(y.keys()).copied().collect();
    paths.sort_unstable();
    paths.dedup();
    for p in paths {
        let e = members.entry(member_of(p)).or_default();
        if x.get(p) == y.get(p) {
            e.0 += 1;
        } else {
            e.1 += 1;
            if e.2.is_none() {
                e.2 = Some(format!(
                    "{p}: {} <> {}",
                    x.get(p).map_or("(absent)", |v| v),
                    y.get(p).map_or("(absent)", |v| v)
                ));
            }
        }
    }
    let mut out = String::new();
    let (mut matched, mut differing) = (0, 0);
    for (m, (ok, bad, first)) in &members {
        if *bad == 0 {
            matched += 1;
            let _ = writeln!(out, "MATCH {m} ({ok} lines)");
        } else {
            differing += 1;
            let f = first.as_deref().unwrap_or("");
            let _ = writeln!(
                out,
                "DIFF  {m} ({bad} of {} lines differ) first: {}",
                ok + bad,
                f.chars().take(300).collect::<String>()
            );
        }
    }
    // the words of the payload itself, pointers excluded, outside the engine's result part
    let mut word_diffs = Vec::new();
    let mut ptr_vs_null = 0usize;
    for off in (0..PAYLOAD_SIZE).step_by(8) {
        if (0x3d8..0x9b0).contains(&off) {
            continue;
        }
        let (o, r) = (u64_at(original_payload, off), u64_at(&built.payload, off));
        if o == r {
            continue;
        }
        match (pointer_like(o), pointer_like(r)) {
            (true, true) => {}
            (true, false) if r == 0 => ptr_vs_null += 1,
            _ => word_diffs.push(off),
        }
    }
    let _ = writeln!(
        out,
        "PAYLOAD WORDS (pointers excluded, result part skipped): {} differ{}; {} pointer words are null in the rebuild",
        word_diffs.len(),
        if word_diffs.is_empty() {
            String::new()
        } else {
            format!(" at {}", word_diffs.iter().map(|o| format!("{o:#x}")).collect::<Vec<_>>().join(" "))
        },
        ptr_vs_null
    );
    let mut kinds: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
    for bl in &built.blocked {
        let e = kinds.entry(bl.kind).or_insert((0, bl.why));
        e.0 += 1;
    }
    for (k, (n, why)) in &kinds {
        let first = built.blocked.iter().find(|b| b.kind == *k).map_or("", |b| b.path.as_str());
        let _ = writeln!(out, "BLOCKED {k} x{n} (first at {first}): {why}");
    }
    let mut by_member: BTreeMap<String, usize> = BTreeMap::new();
    for bl in built.blocked.iter().filter(|b| b.kind == "heap pointer") {
        *by_member.entry(member_of(&bl.path)).or_default() += 1;
    }
    if !by_member.is_empty() {
        let list: Vec<String> = by_member.iter().map(|(m, n)| format!("{m} x{n}")).collect();
        let _ = writeln!(out, "BLOCKED heap pointers by member: {}", list.join(", "));
    }
    let _ = writeln!(
        out,
        "BUILT {} heap blocks with the allocator; {} members match, {} differ, {} blocked structures",
        built.blocks,
        matched,
        differing,
        built.blocked.len()
    );
    let exact = differing == 0 && word_diffs.is_empty() && built.blocked.is_empty();
    Ok(Report { text: out, matched, differing, blocked: built.blocked.len(), exact })
}

/// An arena of blocks at fake addresses: the allocator and the memory of the tests.
pub struct Arena {
    next: RefCell<usize>,
    blocks: RefCell<Vec<(usize, Vec<u8>)>>,
}

impl Arena {
    pub fn new(start: usize) -> Arena {
        Arena { next: RefCell::new(start), blocks: RefCell::new(Vec::new()) }
    }
    pub fn add(&self, data: &[u8]) -> usize {
        let a = self.alloc(data.len().max(1)).unwrap_or(0);
        self.write(a, data);
        a
    }
    /// A block of the original world (already at its own address).
    pub fn put(&self, addr: usize, data: Vec<u8>) {
        self.blocks.borrow_mut().push((addr, data));
    }
}

impl Alloc for Arena {
    fn alloc(&self, n: usize) -> Option<usize> {
        let mut next = self.next.borrow_mut();
        let a = *next;
        *next += (n + 15) & !15;
        *next += 16;
        self.blocks.borrow_mut().push((a, vec![0u8; n]));
        Some(a)
    }
    fn write(&self, addr: usize, data: &[u8]) -> bool {
        let mut blocks = self.blocks.borrow_mut();
        match blocks.iter_mut().find(|(s, b)| addr >= *s && addr + data.len() <= *s + b.len()) {
            Some((s, b)) => {
                b[addr - *s..addr - *s + data.len()].copy_from_slice(data);
                true
            }
            None => false,
        }
    }
}

impl Mem for Arena {
    fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
        self.blocks.borrow().iter().find_map(|(s, b)| {
            (addr >= *s && addr + len <= *s + b.len()).then(|| b[addr - *s..addr - *s + len].to_vec())
        })
    }
}

/// The game's allocator (`operator new` / `operator delete`), after the first bytes of both are compared.
#[cfg(windows)]
pub struct EngineAlloc {
    new_fn: unsafe extern "system-unwind" fn(usize) -> *mut u8,
    delete_fn: unsafe extern "system-unwind" fn(*mut u8),
    taken: RefCell<Vec<usize>>,
}

#[cfg(windows)]
impl EngineAlloc {
    /// `None` (with the reason) unless both functions are what build 40420 has there.
    pub fn at(base: usize) -> Result<EngineAlloc, String> {
        let check = |rva: u32, want: &[u8]| -> Result<(), String> {
            let got = crate::dump::LiveMem
                .read(base + rva as usize, want.len())
                .ok_or_else(|| format!("{rva:#x} is not readable"))?;
            (got == want).then_some(()).ok_or_else(|| format!("{rva:#x} does not hold the expected bytes"))
        };
        check(ENGINE_NEW_RVA, &ENGINE_NEW_PROLOGUE)?;
        check(ENGINE_DELETE_RVA, &ENGINE_DELETE_BYTES)?;
        // SAFETY: both addresses were just compared with the bytes of `operator new(size_t)` and `operator delete(void*)`.
        unsafe {
            Ok(EngineAlloc {
                new_fn: core::mem::transmute::<usize, unsafe extern "system-unwind" fn(usize) -> *mut u8>(
                    base + ENGINE_NEW_RVA as usize,
                ),
                delete_fn: core::mem::transmute::<usize, unsafe extern "system-unwind" fn(*mut u8)>(
                    base + ENGINE_DELETE_RVA as usize,
                ),
                taken: RefCell::new(Vec::new()),
            })
        }
    }

    #[cfg(test)]
    fn with(
        new_fn: unsafe extern "system-unwind" fn(usize) -> *mut u8,
        delete_fn: unsafe extern "system-unwind" fn(*mut u8),
    ) -> EngineAlloc {
        EngineAlloc { new_fn, delete_fn, taken: RefCell::new(Vec::new()) }
    }

    /// Gives everything back.
    pub fn free_all(&self) -> usize {
        let v: Vec<usize> = self.taken.borrow_mut().drain(..).collect();
        for &a in &v {
            // SAFETY: a block this allocator handed out and has not freed.
            unsafe { (self.delete_fn)(a as *mut u8) };
        }
        v.len()
    }
}

#[cfg(windows)]
impl Alloc for EngineAlloc {
    fn alloc(&self, n: usize) -> Option<usize> {
        // SAFETY: the game's operator new; it throws on failure (the caller is a guarded hook).
        let p = unsafe { (self.new_fn)(n.max(1)) } as usize;
        (p != 0).then(|| {
            self.taken.borrow_mut().push(p);
            p
        })
    }
    fn write(&self, addr: usize, data: &[u8]) -> bool {
        // only at the start of a block this allocator returned (the builder never writes anywhere else)
        if !self.taken.borrow().contains(&addr) {
            return false;
        }
        // SAFETY: `addr` is the start of a block of at least `data.len()` bytes this allocator returned.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::testdata::{head, model_of, synthetic, BASE};
    use crate::portable::{encode, Settings};

    fn settings() -> Settings {
        Settings::from_lookup(|_| Some("1".into())).unwrap()
    }

    /// The original world in an arena, and an allocator for the rebuild in the same arena at other addresses.
    fn world(regions: Vec<(usize, Vec<u8>)>) -> Arena {
        let a = Arena::new(0x3000_0000_0000);
        for (addr, data) in regions {
            a.put(addr, data);
        }
        a
    }

    fn roundtrip_of(arena: &Arena, at: usize) -> (String, Report) {
        let text = encode(arena, Env { image_base: BASE }, settings(), "40420", &head(at)).expect("encodes").text;
        let payload = arena.read(at, PAYLOAD_SIZE).unwrap();
        let r = roundtrip(&text, &payload, BASE, settings(), arena, arena, &|b| Some(arena.add(b))).expect("rebuilds");
        (text, r)
    }

    #[test]
    fn escapes_round_trip() {
        for s in [&b"plain"[..], b"a\"b\\c", b"\x00\x01\xff end", b""] {
            assert_eq!(unescape(&portable::escape(s)).unwrap(), s);
        }
        assert!(unescape("no quotes").is_err());
    }

    #[test]
    fn a_synthetic_proposal_is_rebuilt_member_by_member_and_what_cannot_be_made_is_listed() {
        let (fake, at) = synthetic();
        let arena = world(fake.0);
        let (_, r) = roundtrip_of(&arena, at);
        for m in [
            "proposal.addedNodes",
            "proposal.addedSegments",
            "proposal.removedNodes",
            "toRemove",
            "context.player",
            "kind",
        ] {
            assert!(r.text.contains(&format!("MATCH {m} (")), "{m}: {}", r.text);
        }
        // the one member that differs is the construction, by the heap pointer in its unnamed part that the text cannot recreate
        assert!(
            r.text.contains("DIFF  toAdd (1 of ") && r.text.contains("raw:z24 I+500 P z369 <> raw:z24 I+500 z370"),
            "{}",
            r.text
        );
        assert_eq!(r.text.matches("DIFF").count(), 1, "{}", r.text);
        // the heap pointer inside the construction and the containers the table cannot create
        assert!(r.text.contains("BLOCKED heap pointer x1"), "{}", r.text);
        assert!(r.text.contains("BLOCKED std::map x6"), "{}", r.text);
        assert!(r.text.contains("BLOCKED std::unordered_map x"), "{}", r.text);
        assert!(!r.exact);
    }

    #[test]
    fn what_the_text_does_not_say_is_zero_and_a_changed_text_is_a_changed_rebuild() {
        let (fake, at) = synthetic();
        let arena = world(fake.0);
        let text = encode(&arena, Env { image_base: BASE }, settings(), "40420", &head(at)).unwrap().text;
        let edited = text.replace("toRemove[1]\tent:old#39481", "toRemove[1]\tent:old#39999");
        let built = build(&edited, BASE, &arena).unwrap();
        // toRemove: begin pointer of the vector at 0x240 leads to the two ids
        let begin = u64_at(&built.payload, 0x240) as usize;
        assert_eq!(arena.read(begin, 8).unwrap(), [39259u32.to_le_bytes(), 39999u32.to_le_bytes()].concat());
        assert_eq!(built.payload[0x9b8], 52);
    }

    #[test]
    fn a_text_of_another_layout_or_a_broken_line_is_an_error() {
        let arena = Arena::new(0x3000_0000_0000);
        assert!(build("=== portable v1 layout=40421 event=held ===\n", BASE, &arena).is_err());
        assert!(build("nothing\there\n", BASE, &arena).is_err());
        let (fake, at) = synthetic();
        let w = world(fake.0);
        let text = encode(&w, Env { image_base: BASE }, settings(), "40420", &head(at)).unwrap().text;
        let cut = text.replace("proposal.addedNodes.#\tn:2", "proposal.addedNodes.#\tn:x");
        assert!(build(&cut, BASE, &w).is_err());
    }

    #[test]
    fn member_names_group_lines() {
        assert_eq!(member_of("proposal.addedSegments[2].roadTemplate"), "proposal.addedSegments");
        assert_eq!(member_of("toAdd[0].~0x100"), "toAdd");
        assert_eq!(member_of("context.player"), "context.player");
        assert_eq!(member_of("segmentTags.bytes"), "segmentTags");
        assert_eq!(member_of("~0x35a"), "(unnamed bytes)");
        assert_eq!(member_of("proposal.edgeObjectsToAdd.#"), "proposal.edgeObjectsToAdd");
    }

    #[test]
    fn the_real_commands_of_the_spike_run_are_rebuilt_as_far_as_the_text_goes() {
        let Ok(dump) = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../docs/research/spike-dumps-20261013/guest/native_dump.log"
        )) else {
            return; // local research files are not in a clean checkout
        };
        let mut all = String::new();
        for part in dump.split("=== dump #").skip(1) {
            let n: u32 = part.split(' ').next().unwrap().parse().unwrap();
            if ![1, 4, 13, 19, 20].contains(&n) {
                continue;
            }
            let (fake, at) = model_of(&format!("=== dump #{part}"));
            let arena = world(fake.0);
            let (_, r) = roundtrip_of(&arena, at);
            let _ = writeln!(all, "== dump #{n}\n{}", r.text);
        }
        if let Some(out) = std::env::var_os("FL3_REBUILD_TEST_OUT") {
            let _ = std::fs::write(std::path::Path::new(&out).join("rebuild_report.txt"), all);
        }
    }

    #[cfg(windows)]
    mod engine_alloc {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static LIVE: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "system-unwind" fn fake_new(n: usize) -> *mut u8 {
            LIVE.fetch_add(1, Ordering::SeqCst);
            Box::into_raw(vec![0xAAu8; n].into_boxed_slice()) as *mut u8
        }
        unsafe extern "system-unwind" fn fake_delete(_p: *mut u8) {
            LIVE.fetch_sub(1, Ordering::SeqCst);
            // the test leaks the bytes on purpose: the length is not known here
        }

        #[test]
        fn the_engine_allocator_hands_out_blocks_writes_only_into_them_and_gives_them_back() {
            let a = EngineAlloc::with(fake_new, fake_delete);
            let p = a.alloc(16).unwrap();
            assert!(a.write(p, &[1, 2, 3, 4]));
            assert_eq!(crate::dump::LiveMem.read(p, 4).unwrap(), [1, 2, 3, 4]);
            assert!(!a.write(p + 1, &[9]), "only at the start of a block it returned");
            assert!(!a.write(0x1000, &[9]));
            let q = a.alloc(0).unwrap();
            assert_ne!(p, q);
            assert_eq!(LIVE.load(Ordering::SeqCst), 2);
            assert_eq!(a.free_all(), 2);
            assert_eq!(LIVE.load(Ordering::SeqCst), 0);
            assert_eq!(a.free_all(), 0, "nothing is freed twice");
        }

        #[test]
        fn the_functions_are_used_only_where_the_expected_bytes_are() {
            let mut image = vec![0u8; 0x400];
            let base = (image.as_mut_ptr() as usize).wrapping_sub(ENGINE_NEW_RVA as usize);
            // the delete function is a few bytes above operator new in the real image; put both into one buffer
            let delete_at = (ENGINE_DELETE_RVA - ENGINE_NEW_RVA) as usize;
            assert!(delete_at + 5 < image.len());
            assert!(EngineAlloc::at(base).is_err(), "zeroes are not operator new");
            image[..11].copy_from_slice(&ENGINE_NEW_PROLOGUE);
            assert!(EngineAlloc::at(base).is_err(), "operator delete is still missing");
            image[delete_at..delete_at + 5].copy_from_slice(&ENGINE_DELETE_BYTES);
            assert!(EngineAlloc::at(base).is_ok());
            image[3] ^= 1;
            assert!(EngineAlloc::at(base).is_err(), "a changed byte refuses it");
        }
    }
}
