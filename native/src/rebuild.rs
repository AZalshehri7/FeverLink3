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
use std::collections::{BTreeMap, HashMap, VecDeque};
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
    /// Gives back every block still held; the count.
    fn free_all(&self) -> usize {
        0
    }
    /// Lets go of every block without freeing it: its owner (an engine object it was put into) frees it.
    fn forget_all(&self) {}
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

/// The one allocation of a flat hash set of 4-byte ids (see [`Ty::EntSet`]) holding `ids`, and its capacity. The smallest capacity
/// `2^k - 1` whose load bound (7/8) holds them. The ids fill the first slots and their control bytes say "full" (0x00); the real
/// hash bytes cannot be made here (they are the receiving game's hash), and need not be: this table is only ever read by the engine's
/// copy constructor, which walks the full slots and inserts each id into the engine's own table with that game's hash (b54: the
/// receiver's copy had other control bytes than the originator's). Empty slots 0x80, the sentinel 0xff, then the first 15 control
/// bytes cloned for a 16-byte group read.
pub fn ent_set_table(ids: &[u32]) -> (Vec<u8>, usize) {
    let mut cap = 1usize;
    while cap - cap / 8 < ids.len() {
        cap = cap * 2 + 1;
    }
    let off = portable::ent_set_slot_offset(cap);
    let mut block = vec![0u8; off + cap * 4];
    for (i, c) in block[..cap].iter_mut().enumerate() {
        *c = if i < ids.len() { 0x00 } else { 0x80 };
    }
    block[cap] = 0xff;
    for i in 0..15 {
        block[cap + 1 + i] = if i < cap { block[i] } else { 0x80 };
    }
    for (i, v) in ids.iter().enumerate() {
        block[off + i * 4..off + i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    (block, cap)
}

struct Builder<'a> {
    lines: HashMap<&'a str, &'a str>,
    base: usize,
    a: &'a dyn Alloc,
    blocked: Vec<Blocked>,
    blocks: usize,
    /// Which top-level members are built (all when `None`); a member that is not selected is not touched and allocates nothing.
    select: Option<&'a dyn Fn(&F) -> bool>,
    /// the heap graph of the text: block number -> (size, tokens)
    graph: HashMap<usize, (usize, &'a str)>,
    /// block number -> address of the block made for it (made when first referenced)
    made: HashMap<usize, usize>,
    /// blocks made and not yet filled
    fill: VecDeque<usize>,
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
            // the engine frees a string by its capacity (size | 15, plus the terminator)
            let addr =
                self.a.alloc((text.len() | 15) + 1).ok_or_else(|| format!("{path}: string allocation failed"))?;
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
            Ty::Skip(_) | Ty::Raw(_) | Ty::Pad(_) => Ok(()),
            Ty::EntSet => {
                let key = format!("{path}.#");
                let v = self.val(&key).ok_or_else(|| format!("missing line {key}"))?;
                let body = v.strip_prefix("set:").ok_or_else(|| format!("{key}: {v}"))?;
                buf[..0x20].fill(0);
                if body == "null" {
                    return Ok(());
                }
                if let Some(rva) = body.strip_prefix("0,I+") {
                    // the empty table points at the library's static empty group, as the engine's own empty sets do
                    let rva = usize::from_str_radix(rva, 16).map_err(|e| format!("{key}: {e}"))?;
                    buf[0..8].copy_from_slice(&((self.base + rva) as u64).to_le_bytes());
                    return Ok(());
                }
                let n: usize = body.parse().map_err(|_| format!("{key}: {v}"))?;
                if n == 0 || n > 0xe00 {
                    return Err(format!("{key}: a set of {n} ids"));
                }
                let mut ids = Vec::with_capacity(n);
                for i in 0..n {
                    let l = format!("{path}[{i}]");
                    ids.push(Self::ent(self.val(&l).ok_or_else(|| format!("missing line {l}"))?)?);
                }
                let (block, cap) = ent_set_table(&ids);
                let ctrl = self.block(path, &block)?.0;
                buf[0..8].copy_from_slice(&ctrl.to_le_bytes());
                buf[8..16].copy_from_slice(&(ctrl + portable::ent_set_slot_offset(cap) as u64).to_le_bytes());
                buf[16..24].copy_from_slice(&(n as u64).to_le_bytes());
                buf[24..32].copy_from_slice(&(cap as u64).to_le_bytes());
                Ok(())
            }
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
                if size == 0 {
                    // an empty container is the engine's own: its constructor makes the sentinel, nothing is described or blocked
                    return Ok(());
                }
                if let Some(g) = self.val(&format!("{path}.graph")) {
                    // the head pointer and the size, with the tree's nodes made as blocks of the heap graph
                    let bytes = self.raw_value(&format!("{path}.graph"), g)?;
                    if bytes.len() != 0x10 {
                        return Err(format!("{path}: map graph has {} bytes, not 16", bytes.len()));
                    }
                    buf[..0x10].copy_from_slice(&bytes);
                } else {
                    self.blocked.push(Blocked {
                        kind: "std::map",
                        path: path.to_string(),
                        why: "the text has no heap graph for it (the sentinel head node is not described)",
                    });
                }
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
                if size == 0 {
                    return Ok(());
                }
                if let Some(g) = self.val(&format!("{path}.graph")) {
                    // list head, size, bucket vector, mask and maximum index, with the list nodes as blocks of the heap graph
                    let bytes = self.raw_value(&format!("{path}.graph"), g)?;
                    if bytes.len() != 0x38 {
                        return Err(format!("{path}: hash graph has {} bytes, not 56", bytes.len()));
                    }
                    buf[8..0x40].copy_from_slice(&bytes);
                } else {
                    self.blocked.push(Blocked {
                        kind: "std::unordered_map",
                        path: path.to_string(),
                        why: "the text has no heap graph for it (the list sentinel and the bucket array are not described)",
                    });
                }
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
            if fl.off > cursor && !(path.is_empty() && self.select.is_some()) {
                self.gap(path, buf, cursor, fl.off)?;
            }
            let end = fl.off + fl.ty.size();
            if path.is_empty() && self.select.is_some_and(|sel| !sel(fl)) {
                cursor = end;
                continue;
            }
            let full = if path.is_empty() { fl.name.to_string() } else { format!("{path}.{}", fl.name) };
            if let Ty::Raw(_) = fl.ty {
                self.gap_named(&full, buf, fl.off, end)?;
            } else {
                self.field(&full, fl.ty, &mut buf[fl.off..end])?;
            }
            cursor = end;
        }
        if cursor < size && !(path.is_empty() && self.select.is_some()) {
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
                b'B' => {
                    i += 1;
                    let n = Self::number(s, &mut i)?;
                    let mut off = 0usize;
                    if b.get(i) == Some(&b'+') {
                        i += 1;
                        let end = s[i..].find(' ').map_or(s.len(), |p| i + p);
                        off = usize::from_str_radix(&s[i..end], 16).map_err(|e| e.to_string())?;
                        i = end;
                    }
                    let addr = self.block_addr(path, n)?;
                    out.extend_from_slice(&((addr + off) as u64).to_le_bytes());
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
                        path: format!("{path}+{:#x}", out.len()),
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

    /// The address of block `n` of the heap graph: made (from the allocator) the first time it is referenced.
    fn block_addr(&mut self, path: &str, n: usize) -> R<usize> {
        if let Some(&a) = self.made.get(&n) {
            return Ok(a);
        }
        let (size, _) = *self.graph.get(&n).ok_or_else(|| format!("{path}: block B{n} is not in the text"))?;
        let addr = self.a.alloc(size).ok_or_else(|| format!("{path}: allocation of block B{n} failed"))?;
        self.blocks += 1;
        self.made.insert(n, addr);
        self.fill.push_back(n);
        Ok(addr)
    }

    /// Writes the contents of every block made so far (writing one can make more).
    fn fill_blocks(&mut self) -> R<()> {
        while let Some(n) = self.fill.pop_front() {
            let (size, tokens) = self.graph[&n];
            let name = format!("@B{n}");
            let bytes = self.raw_value(&name, tokens)?;
            if bytes.len() != size {
                return Err(format!("{name}: {} bytes of tokens for a block of {size}", bytes.len()));
            }
            let addr = self.made[&n];
            if !self.a.write(addr, &bytes) {
                return Err(format!("{name}: writing the block failed"));
            }
        }
        Ok(())
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
    build_selected(text, image_base, a, None)
}

/// [`build`] for the top-level members `select` accepts only (`None`: all of them, and the unnamed bytes too).
pub fn build_selected(text: &str, image_base: usize, a: &dyn Alloc, select: Option<&dyn Fn(&F) -> bool>) -> R<Built> {
    let mut lines: HashMap<&str, &str> = HashMap::new();
    let mut graph: HashMap<usize, (usize, &str)> = HashMap::new();
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
            if let Some(n) = p.strip_prefix("@B") {
                // @B<n> TAB size=<bytes> TAB raw:<tokens>
                let (size, tokens) = v.split_once('\t').ok_or_else(|| format!("bad block line {p}"))?;
                let size: usize = size
                    .strip_prefix("size=")
                    .and_then(|x| x.parse().ok())
                    .ok_or_else(|| format!("bad size in {p}"))?;
                graph.insert(n.parse().map_err(|_| format!("bad block number in {p}"))?, (size, tokens));
            } else {
                lines.insert(p, v.split('\t').next().unwrap_or(v));
            }
        }
    }
    if !header_ok {
        return Err("no portable header".into());
    }
    let mut b = Builder {
        lines,
        base: image_base,
        a,
        blocked: Vec::new(),
        blocks: 0,
        select,
        graph,
        made: HashMap::new(),
        fill: VecDeque::new(),
    };
    let mut payload = vec![0u8; PAYLOAD_SIZE];
    b.fields("", &mut payload, PAYLOAD, PAYLOAD_SIZE)?;
    b.fill_blocks()?;
    Ok(Built { payload, blocked: b.blocked, blocks: b.blocks })
}

/// The member a portable line belongs to, for the per-member report.
pub fn member_of(path: &str) -> String {
    if path.starts_with('~') {
        return "(unnamed bytes)".into();
    }
    if path.starts_with("@B") {
        return "(heap graph)".into();
    }
    let path = path.strip_suffix(".graph").unwrap_or(path);
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

/// The `path -> value` lines of a portable text.
pub fn lines_of(text: &str) -> BTreeMap<&str, &str> {
    text.lines()
        .filter(|l| !l.starts_with("==="))
        .filter_map(|l| l.split_once('\t'))
        .map(|(p, v)| if p.starts_with("@B") { (p, v) } else { (p, v.split('\t').next().unwrap_or(v)) })
        .collect()
}

/// Compares two portable texts per member: the report lines (`MATCH` / `DIFF`), the members that match and that differ.
pub fn compare_texts(a: &str, b: &str) -> (String, usize, usize) {
    let (x, y) = (lines_of(a), lines_of(b));
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
    (out, matched, differing)
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
    let (cmp_text, matched, differing) = compare_texts(original_text, &again.text);
    let mut out = cmp_text;
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
    fn heap_size(&self, addr: usize) -> Option<usize> {
        self.blocks.borrow().iter().find(|(s, _)| *s == addr).map(|(_, b)| b.len())
    }

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
    /// (address handed out, address from operator new)
    taken: RefCell<Vec<(usize, usize)>>,
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
}

#[cfg(windows)]
impl Alloc for EngineAlloc {
    /// Gives everything back.
    fn free_all(&self) -> usize {
        let v: Vec<(usize, usize)> = self.taken.borrow_mut().drain(..).collect();
        for &(_, raw) in &v {
            // SAFETY: a block this allocator got from operator new and has not freed.
            unsafe { (self.delete_fn)(raw as *mut u8) };
        }
        v.len()
    }

    fn forget_all(&self) {
        self.taken.borrow_mut().clear();
    }

    fn alloc(&self, n: usize) -> Option<usize> {
        let n = n.max(1);
        // The engine's containers free a block of 0x1000 bytes or more through the pointer stored just below it (the STL's
        // aligned big allocation: request n + 0x27, align to 32, keep the raw pointer at [ptr - 8]); smaller ones directly.
        let (raw, p) = if n >= 0x1000 {
            // SAFETY: the game's operator new; it throws on failure (the caller is a guarded hook).
            let raw = unsafe { (self.new_fn)(n + 0x27) } as usize;
            if raw == 0 {
                return None;
            }
            let p = (raw + 0x27) & !0x1f;
            // SAFETY: inside the block just allocated (p - 8 >= raw + 0x1f).
            unsafe { core::ptr::write_unaligned((p - 8) as *mut usize, raw) };
            (raw, p)
        } else {
            // SAFETY: as above.
            let raw = unsafe { (self.new_fn)(n) } as usize;
            (raw, raw)
        };
        (raw != 0).then(|| {
            self.taken.borrow_mut().push((p, raw));
            p
        })
    }
    fn write(&self, addr: usize, data: &[u8]) -> bool {
        // only at the start of a block this allocator returned (the builder never writes anywhere else)
        if !self.taken.borrow().iter().any(|&(p, _)| p == addr) {
            return false;
        }
        // SAFETY: `addr` is the start of a block of at least `data.len()` bytes this allocator returned.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
        true
    }
}

/// A payload with engine-style containers that have entries, for the tests: an unordered map with a two-node list and a
/// bucket vector, and a map with a head and one node, all in the arena (whose blocks count as heap blocks).
#[cfg(test)]
pub(crate) mod testgraph {
    use super::*;

    fn w(buf: &mut [u8], off: usize, v: u64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn add_containers(arena: &Arena, payload: usize) {
        // list of the hash: head H, nodes N1 and N2 (next, prev, key, value)
        let (h, n1, n2) = (arena.add(&[0u8; 0x18]), arena.add(&[0u8; 0x30]), arena.add(&[0u8; 0x30]));
        let link = |a: usize, next: usize, prev: usize, key: u32| {
            let mut b = vec![0u8; if a == h { 0x18 } else { 0x30 }];
            w(&mut b, 0, next as u64);
            w(&mut b, 8, prev as u64);
            if a != h {
                b[0x10..0x14].copy_from_slice(&key.to_le_bytes());
                b[0x18..0x1c].copy_from_slice(&(key + 1).to_le_bytes());
            }
            assert!(arena.write(a, &b));
        };
        link(h, n1, n2, 0);
        link(n1, n2, h, 0xffff_fffd);
        link(n2, h, n1, 0xffff_fffc);
        // 16 bucket slots (first, last) of node pointers
        let mut slots = vec![0u8; 0x80];
        for (i, node) in [n1, n1, n2, n2, h, h].iter().enumerate() {
            w(&mut slots, i * 8, *node as u64);
        }
        let buckets = arena.add(&slots);
        let mut hash = vec![0u8; 0x40];
        hash[..4].copy_from_slice(&1.0f32.to_le_bytes());
        w(&mut hash, 8, h as u64);
        w(&mut hash, 0x10, 2);
        w(&mut hash, 0x18, buckets as u64);
        w(&mut hash, 0x20, (buckets + 0x80) as u64);
        w(&mut hash, 0x28, (buckets + 0x80) as u64);
        w(&mut hash, 0x30, 7);
        w(&mut hash, 0x38, 8);
        assert!(arena.write(payload + 0x270, &hash));
        // a map: head (left, parent, right, colour and isnil) and one node
        let (mh, mn) = (arena.add(&[0u8; 0x50]), arena.add(&[0u8; 0x50]));
        let mut head = vec![0u8; 0x50];
        for off in [0, 8, 0x10] {
            w(&mut head, off, mn as u64);
        }
        head[0x18] = 1;
        head[0x19] = 1;
        assert!(arena.write(mh, &head));
        let mut node = vec![0u8; 0x50];
        for off in [0, 8, 0x10] {
            w(&mut node, off, mh as u64);
        }
        node[0x20..0x24].copy_from_slice(&77u32.to_le_bytes());
        assert!(arena.write(mn, &node));
        let mut map = vec![0u8; 0x10];
        w(&mut map, 0, mh as u64);
        w(&mut map, 8, 1);
        assert!(arena.write(payload + 0x2c8, &map));
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

    /// The arena as plain memory: it cannot tell heap blocks (the graph is off), as for a reader without a heap.
    struct NoHeap<'a>(&'a Arena);
    impl Mem for NoHeap<'_> {
        fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
            self.0.read(addr, len)
        }
    }

    fn roundtrip_of(arena: &Arena, at: usize) -> (String, Report) {
        let plain = NoHeap(arena);
        let text = encode(&plain, Env { image_base: BASE }, settings(), "40420", &head(at)).expect("encodes").text;
        let payload = arena.read(at, PAYLOAD_SIZE).unwrap();
        let r = roundtrip(&text, &payload, BASE, settings(), arena, &plain, &|b| Some(arena.add(b))).expect("rebuilds");
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
        // empty engine containers are the engine's own: not blocked
        assert!(!r.text.contains("BLOCKED std::map") && !r.text.contains("BLOCKED std::unordered_map"), "{}", r.text);
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
    fn a_map_with_entries_is_walked_without_the_heap_index_and_its_keys_are_a_named_line() {
        let (fake, at) = synthetic();
        let arena = world(fake.0);
        super::testgraph::add_containers(&arena, at);
        let text =
            encode(&NoHeap(&arena), Env { image_base: BASE }, settings(), "40420", &head(at)).expect("encodes").text;
        // the map's sentinel and its one node are blocks although the heap knows no sizes; the key (77) is a named line
        assert!(text.contains("parcelsMap.keys	keys:0000004d"), "{text}");
        assert!(text.contains("parcelsMap.graph	raw:B"), "{text}");
        assert_eq!(text.lines().filter(|l| l.starts_with("@B") && l.contains("size=80")).count(), 2, "{text}");
        // a tree that does not hold `size` nodes is left alone (its pointers stay P)
        arena.write(at + 0x2c8 + 8, &5u64.to_le_bytes());
        let bad =
            encode(&NoHeap(&arena), Env { image_base: BASE }, settings(), "40420", &head(at)).expect("encodes").text;
        assert!(!bad.contains("parcelsMap.keys") && bad.contains("parcelsMap.graph	raw:P"), "{bad}");
    }

    #[test]
    fn containers_with_entries_are_followed_as_a_heap_graph_and_rebuilt_isomorphic() {
        let (fake, at) = synthetic();
        let arena = world(fake.0);
        super::testgraph::add_containers(&arena, at);
        let text = encode(&arena, Env { image_base: BASE }, settings(), "40420", &head(at)).expect("encodes").text;
        // the construction's name string, the hash list (head, two nodes, the bucket array) and the map (head, one node) are blocks
        assert_eq!(text.lines().filter(|l| l.starts_with("@B")).count(), 7, "{text}");
        assert!(text.contains("old2new.graph	raw:B2 w:0000000000000002 B"), "{text}");
        assert!(text.contains("parcelsMap.graph	raw:B"), "{text}");
        let payload = arena.read(at, PAYLOAD_SIZE).unwrap();
        let r =
            roundtrip(&text, &payload, BASE, settings(), &arena, &arena, &|b| Some(arena.add(b))).expect("rebuilds");
        // no container is blocked any more; what differs is only what the synthetic construction's pointer leaves
        assert!(!r.text.contains("BLOCKED std::map") || r.text.contains("BLOCKED std::map x6"), "{}", r.text);
        assert!(!r.text.contains("DIFF  old2new") && !r.text.contains("DIFF  parcelsMap"), "{}", r.text);
        assert!(r.text.contains("MATCH old2new") && r.text.contains("MATCH parcelsMap"), "{}", r.text);
        // the rebuilt graph is a different set of blocks with the same shape
        let rebuilt = build(&text, BASE, &arena).unwrap();
        let (old, new) = (u64_at(&payload, 0x270 + 8), u64_at(&rebuilt.payload, 0x270 + 8));
        assert_ne!(old, new);
        let head = arena.read(new as usize, 0x18).unwrap();
        let (first, last) = (u64_at(&head, 0) as usize, u64_at(&head, 8) as usize);
        let n1 = arena.read(first, 0x30).unwrap();
        assert_eq!(u64_at(&n1, 8), new, "the first node points back to the head");
        assert_eq!(u32::from_le_bytes(n1[0x10..0x14].try_into().unwrap()), 0xffff_fffd);
        let n2 = arena.read(last, 0x30).unwrap();
        assert_eq!(u64_at(&n2, 0), new, "the last node points on to the head");
        assert_eq!(u64_at(&n1, 0) as usize, last);
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

    #[test]
    fn a_hash_containers_bucket_vector_is_a_block_of_its_own_size_without_the_heap_index() {
        // inj8 C10: the 128-byte bucket block of `old2new` (8 buckets) was not in the heap index and its three words stayed P
        let (fake, at) = synthetic();
        let arena = world(fake.0);
        let buckets = arena.add(&[0u8; 0x80]);
        let mut h = vec![0u8; 0x40];
        h[0..4].copy_from_slice(&1.0f32.to_bits().to_le_bytes());
        for (off, v) in [(0x18, buckets), (0x20, buckets + 0x80), (0x28, buckets + 0x80)] {
            h[off..off + 8].copy_from_slice(&(v as u64).to_le_bytes());
        }
        h[0x30..0x38].copy_from_slice(&7u64.to_le_bytes());
        h[0x38..0x40].copy_from_slice(&8u64.to_le_bytes());
        assert!(arena.write(at + 0x270, &h));
        let text =
            encode(&NoHeap(&arena), Env { image_base: BASE }, settings(), "40420", &head(at)).expect("encodes").text;
        let line = text.lines().find(|l| l.starts_with("old2new.graph")).expect("a graph line");
        assert!(line.contains("B") && !line.contains('P'), "{line}");
    }

    #[test]
    fn the_id_maps_are_walked_with_their_sets_and_the_sentinels_residue_is_not_a_pointer() {
        // all six id maps are std::map<u32, std::set<u32>> (the Proposal copy constructor copies each with 0x4bc8d0); inj3: the node maps
        // walked as generic maps followed their sets into the world (a 7.6 MB text)
        for (off, name) in [(0x160usize, "proposal.new2oldSegments"), (0x150, "proposal.old2newNodes")] {
            let (fake, at) = synthetic();
            let arena = world(fake.0);
            // std::map<u32, std::set<u32>>: sentinel (0x38), one node (key at +0x20, set head at +0x28, set size at +0x30), the set's
            // sentinel and two nodes (0x20 bytes, key at +0x1c)
            let w = |b: &mut Vec<u8>, off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_le_bytes());
            let (hd, node) = (arena.add(&[0u8; 0x38]), arena.add(&[0u8; 0x38]));
            let (sh, s1, s2) = (arena.add(&[0u8; 0x20]), arena.add(&[0u8; 0x20]), arena.add(&[0u8; 0x20]));
            let residue = arena.add(&[7u8; 16]); // what the sentinel's unconstructed value area may still hold
            let mut h = vec![0u8; 0x38];
            for off in [0, 8, 0x10] {
                w(&mut h, off, node as u64);
            }
            h[0x18] = 1;
            h[0x19] = 1;
            w(&mut h, 0x28, residue as u64);
            w(&mut h, 0x30, 1);
            arena.write(hd, &h);
            let mut n = vec![0u8; 0x38];
            for off in [0, 8, 0x10] {
                w(&mut n, off, hd as u64);
            }
            n[0x20..0x24].copy_from_slice(&0xffff_fffbu32.to_le_bytes());
            n[0x1a..0x20].copy_from_slice(b"/town_"); // colour and isnil are at +0x18/+0x19; what follows is residue (b43: ASCII from a log buffer)
            n[0x24..0x28].copy_from_slice(b"stre");
            w(&mut n, 0x28, sh as u64);
            w(&mut n, 0x30, 2);
            arena.write(node, &n);
            let mut sent = vec![0u8; 0x20];
            for (off, v) in [(0, s1), (8, s2), (0x10, s2)] {
                w(&mut sent, off, v as u64);
            }
            sent[0x19] = 1;
            arena.write(sh, &sent);
            for (a, parent, left, right, key) in [(s1, s2, sh, sh, 7u32), (s2, sh, s1, sh, 9u32)] {
                let mut b = vec![0u8; 0x20];
                w(&mut b, 0, left as u64);
                w(&mut b, 8, parent as u64);
                w(&mut b, 0x10, right as u64);
                b[0x1c..0x20].copy_from_slice(&key.to_le_bytes());
                b[0x1a..0x1c].copy_from_slice(b"ab");
                arena.write(a, &b);
            }
            let mut map = vec![0u8; 0x10];
            w(&mut map, 0, hd as u64);
            w(&mut map, 8, 1);
            assert!(arena.write(at + off, &map));
            let text = encode(&NoHeap(&arena), Env { image_base: BASE }, settings(), "40420", &head(at))
                .expect("encodes")
                .text;
            assert!(text.contains(&format!("{name}.keys	keys:fffffffb=[00000007;00000009]")), "{text}");
            // the sentinel's residue pointer was zeroed, and the set's blocks are in the graph: nothing of this map stays P
            assert!(
                !text.contains("5f6e776f742f") && !text.contains("65727473"),
                "residue bytes are not carried: {text}"
            );
            let line = text.lines().find(|l| l.starts_with(&format!("{name}.graph"))).unwrap();
            assert!(!line.contains('P'), "{line}");
            let built = build_selected(&text, BASE, &arena, None).expect("builds");
            assert!(
                !built.blocked.iter().any(|b| b.kind == "heap pointer" && b.path.contains(name)),
                "{:?}",
                built.blocked
            );
        }
    }

    #[test]
    fn a_node_configuration_carries_its_crosswalk_set_and_traffic_light_state_and_never_a_residue_pointer() {
        let (fake, at) = synthetic();
        let arena = world(fake.0);
        let readable = arena.add(&[9u8; 0x40]); // a pointer-like word that points at readable memory, as in b46
        let ascii = u64::from_le_bytes(*b"/feverli");
        let mut elem = vec![0u8; 0x80];
        for off in [0x38usize, 0x70] {
            elem[off..off + 8].copy_from_slice(&(readable as u64).to_le_bytes());
        }
        elem[0x58..0x60].copy_from_slice(&ascii.to_le_bytes());
        // the traffic-light state of b39: preference 2 at +0x4c, type -1 at +0x68
        elem[0x4c..0x50].copy_from_slice(&2u32.to_le_bytes());
        elem[0x68..0x6c].copy_from_slice(&(-1i32).to_le_bytes());
        // the real table of b45 entry 0: control bytes 7a 23 51, sentinel, clones and 0x80 fill, one pad byte, the slots -4 -5 -3
        let real = parse_hex("7a2351ff7a2351808080808080808080808080").unwrap();
        let mut table = real.clone();
        table.push(0x73);
        for v in [-4i32, -5, -3] {
            table.extend_from_slice(&v.to_le_bytes());
        }
        let ctrl = arena.add(&table);
        elem[0x18..0x20].copy_from_slice(&(ctrl as u64).to_le_bytes());
        elem[0x20..0x28].copy_from_slice(&(ctrl as u64 + 20).to_le_bytes());
        elem[0x28..0x30].copy_from_slice(&3u64.to_le_bytes());
        elem[0x30..0x38].copy_from_slice(&3u64.to_le_bytes());
        elem[0x78..0x7c].copy_from_slice(&(-1i32).to_le_bytes());
        let blk = arena.add(&elem);
        let mut v = Vec::new();
        for x in [blk as u64, blk as u64 + 0x80, blk as u64 + 0x80] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        assert!(arena.write(at + 0x60, &v));
        // the injection's settings: typed (a foreign pointer in an unnamed word is not carried)
        let typed = Settings { typed: true, ..settings() };
        let e = encode(&NoHeap(&arena), Env { image_base: BASE }, typed, "40420", &head(at)).expect("encodes");
        // the traffic-light preference and type are named members (rt2 R3: left to the mask, the type was dropped), the residue
        // pointers of the unnamed words around them are not carried
        for l in ["trafficLightPreference	u32:00000002", "trafficLightType	u32:ffffffff"] {
            assert!(e.text.contains(&format!("proposal.nodeConfigsToAdd[0].{l}")), "{l}: {}", e.text);
        }
        for line in e.text.lines().filter(|l| l.starts_with("proposal.nodeConfigsToAdd[0].~")) {
            assert!(!line.contains('P') && !line.contains(&format!("{readable:x}")), "{line}");
        }
        assert!(e.unresolved.iter().all(|(m, _)| !m.contains("nodeConfigsToAdd")), "{:?}", e.unresolved);
        // the value is the set of ids in id order, not the control bytes (the owner's hash)
        for l in ["crosswalks.#	set:3", "crosswalks[0]	ent:new#5", "crosswalks[1]	ent:new#4", "crosswalks[2]	ent:new#3"]
        {
            assert!(e.text.contains(&format!("nodeConfigsToAdd[0].{l}")), "{l}: {}", e.text);
        }
        assert!(!e.text.contains("nodeConfigsToAdd[0].~0x28"), "size and capacity are the set's: {}", e.text);
        let built = build_selected(&e.text, BASE, &arena, None).expect("builds");
        let copy = &built.payload;
        let (b0, e0) = (u64_at(copy, 0x60), u64_at(copy, 0x68));
        assert!(b0 != 0 && e0 - b0 == 0x80, "the element vector is rebuilt");
        assert!(!built.blocked.iter().any(|b| b.path.contains("nodeConfigsToAdd")), "{:?}", built.blocked);
        let el0 = arena.read(b0 as usize, 0x80).unwrap();
        assert_eq!(
            (crate::dump::u32_at(&el0, 0x4c), crate::dump::u32_at(&el0, 0x68), u64_at(&el0, 0x38)),
            (2, u32::MAX, 0)
        );
        // the rebuilt table: one block, slots at +20, size 3, capacity 3, the ids in full slots, a walkable control row
        let el = arena.read(b0 as usize, 0x80).unwrap();
        let (c, sl) = (u64_at(&el, 0x18) as usize, u64_at(&el, 0x20) as usize);
        assert_eq!((sl - c, u64_at(&el, 0x28), u64_at(&el, 0x30)), (20, 3, 3));
        let t = arena.read(c, 32).unwrap();
        assert_eq!(&t[..4], &[0, 0, 0, 0xff]);
        assert!(t[4..7].iter().all(|&b| b == 0) && t[7..19].iter().all(|&b| b == 0x80), "{t:02x?}");
        let mut ids: Vec<i32> = (0..3).map(|i| crate::dump::u32_at(&t, 20 + i * 4) as i32).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![-5, -4, -3]);
        // and written as text again it is the same set
        assert!(arena.write(at + 0x60, &copy[0x60..0x78]));
        let e2 = encode(&NoHeap(&arena), Env { image_base: BASE }, typed, "40420", &head(at)).expect("encodes again");
        for l in ["crosswalks.#	set:3", "crosswalks[0]	ent:new#5", "crosswalks[2]	ent:new#3"] {
            assert!(e2.text.contains(&format!("nodeConfigsToAdd[0].{l}")), "{l}: {}", e2.text);
        }
    }

    #[test]
    fn a_foreign_pointer_in_an_unnamed_region_that_starts_between_two_words_is_not_carried() {
        // inj3 (2026-10-14): an EdgeObject's region ~0xc (after segmentEntity at +8) held a vector's three words at +0x10..+0x28
        let (fake, at) = synthetic();
        let arena = world(fake.0);
        let readable = arena.add(&[9u8; 0x40]);
        let mut elem = vec![0u8; 0x1a0];
        for off in [0x10usize, 0x18, 0x20] {
            elem[off..off + 8].copy_from_slice(&(readable as u64).to_le_bytes());
        }
        elem[0x0c..0x10].copy_from_slice(&0x3f3f_fab3u32.to_le_bytes()); // data beside it stays
        for s in [0x90usize, 0x168] {
            elem[s + 0x18..s + 0x20].copy_from_slice(&15u64.to_le_bytes()); // empty std::string (small buffer, capacity 15)
        }
        let blk = arena.add(&elem);
        let mut v = Vec::new();
        for x in [blk as u64, blk as u64 + 0x1a0, blk as u64 + 0x1a0] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        assert!(arena.write(at + 0x128, &v));
        let typed = Settings { typed: true, ..settings() };
        let e = encode(&NoHeap(&arena), Env { image_base: BASE }, typed, "40420", &head(at)).expect("encodes");
        let line = e
            .text
            .lines()
            .find(|l| l.starts_with("proposal.edgeObjectsToAdd[0].~0xc"))
            .unwrap_or_else(|| panic!("the region is written: {}", e.text));
        let hex: String = (readable as u64).to_le_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert!(!line.contains(&hex) && line.contains("b3fa3f3f"), "{line}");
    }

    #[test]
    fn an_entity_set_table_grows_by_the_load_bound_and_an_empty_set_points_at_the_image() {
        assert_eq!(ent_set_table(&[1]).1, 1);
        assert_eq!(ent_set_table(&[1, 2, 3, 4, 5, 6, 7]).1, 7);
        let (b, cap) = ent_set_table(&(0..8).collect::<Vec<u32>>());
        assert_eq!((cap, b.len()), (15, portable::ent_set_slot_offset(15) + 60));
        assert_eq!((b[7], b[8], b[15], b[16], b[16 + 7], b[16 + 8]), (0, 0x80, 0xff, 0, 0, 0x80));
        let arena = Arena::new(0x5000_0000);
        let mut b = Builder {
            lines: HashMap::from([("x.#", "set:0,I+367b620")]),
            base: BASE,
            a: &arena,
            blocked: Vec::new(),
            blocks: 0,
            select: None,
            graph: HashMap::new(),
            made: HashMap::new(),
            fill: VecDeque::new(),
        };
        let mut buf = [0xaau8; 0x20];
        b.field("x", Ty::EntSet, &mut buf).unwrap();
        assert_eq!((u64_at(&buf, 0), u64_at(&buf, 8), u64_at(&buf, 24)), ((BASE + 0x367b620) as u64, 0, 0));
    }
}
