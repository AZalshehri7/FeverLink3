//! The build table's row of a game build the table does not know, found at game start by repeating the address tool's proof.
//!
//! `tools/addr/port_build.py` carries a row from one executable to the next by comparing whole functions. Its `--export-proof`
//! writes everything that comparison needs from the reference build (here 40425, `tools/addr/out/proof-6ad9ce10.bin`, embedded
//! below): every function the row depends on as a pattern (relink fields wildcarded; for the few places the module only calls
//! through, also a "shape" pattern with every 32-bit displacement wildcarded), where each site sits in its function, the first
//! [`PROBE`] bytes of each callee, the neighbour functions that single out one of several copies, the 22 layout witnesses, and
//! where the preview listener's offsets are read. [`search`] repeats the proof on the running game's image with no
//! disassembler: patterns are tried only at the function starts of the image's unwind table (a function without an unwind entry
//! in the reference build, a leaf, is looked for in all of `.text`); each must match exactly once, or the neighbour-shift rule
//! must pick one copy; callees must start with the same code; all witnesses must match exactly. Any failure is `None`: the
//! game stays degraded exactly as an unknown build always did (no partial row, nothing guessed).
//!
//! What the search does NOT prove that the table does: a human looked at the port report, and the owner played the build.
//! The window says the build was found by search and is not yet confirmed; a host and a player must have found the same
//! addresses ([`row_hash`], carried by the join).
//!
//! The format (little-endian) is documented in `Proof.write` of `port_build.py`.

use crate::build_table::{
    self as bt, BuildAddrs, CallPatchSite, CallSite, EngineFns, HookSite, PatchSite, PreviewOwnerSite, Rva,
};
use crate::pe;
use std::collections::HashMap;

/// The proof of the reference build, written by `port_build.py --export-proof`.
pub const PROOF: &[u8] = include_bytes!("../../tools/addr/out/proof-6ad9ce10.bin");
const MAGIC: &[u8; 8] = b"FL3PROOF";
const VERSION: u32 = 1;
/// Bytes compared at the start of each callee (`port_build.CALLEE_PROBE`).
pub const PROBE: usize = 32;
const NO_PATTERN: u16 = 0xFFFF;

struct Pattern {
    rva: u32,
    /// The reference function starts at an unwind-table entry: only function starts are tried.
    at_start: bool,
    bytes: Vec<u8>,
    /// Bit `i` (low bit first): cell `i` is fixed.
    mask: Vec<u8>,
    left: Vec<u16>,
    right: Vec<u16>,
    calls: Vec<(u32, [u8; PROBE], [u8; PROBE / 8])>,
}

fn fixed(mask: &[u8], i: usize) -> bool {
    mask[i >> 3] >> (i & 7) & 1 == 1
}

fn masked_eq(bytes: &[u8], mask: &[u8], hay: &[u8]) -> bool {
    hay.len() >= bytes.len() && bytes.iter().zip(hay).enumerate().all(|(i, (b, h))| !fixed(mask, i) || b == h)
}

impl Pattern {
    fn matches(&self, hay: &[u8]) -> bool {
        masked_eq(&self.bytes, &self.mask, hay)
    }
}

struct Listener {
    lea: (u32, u32),
    exact: u16,
    shape: u16,
    offsets: Vec<(String, Vec<(u32, u8)>)>,
    emits: Vec<u32>,
}

enum Kind {
    Func,
    Inside,
    Ret,
    Call { callee: u16, listener: Option<Listener> },
    Slot { lea: (u32, u32), callback: u16 },
    Witness,
}

struct Site {
    name: String,
    kind: Kind,
    exact: u16,
    shape: u16,
    offset: u32,
    n: u32,
}

/// A parsed proof.
pub struct Proof {
    /// The PE timestamp of the reference build (its row is in [`bt::BUILDS`]).
    pub timestamp: u32,
    pats: Vec<Pattern>,
    sites: Vec<Site>,
}

struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn name(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
    fn list16(&mut self) -> Option<Vec<u16>> {
        let n = self.u8()? as usize;
        (0..n).map(|_| self.u16()).collect()
    }
}

impl Proof {
    pub fn parse(b: &[u8]) -> Option<Proof> {
        let mut r = Rd { b, at: 0 };
        if r.take(8)? != MAGIC || r.u32()? != VERSION {
            return None;
        }
        let timestamp = r.u32()?;
        let n = r.u32()? as usize;
        let mut pats: Vec<Pattern> = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            let rva = r.u32()?;
            let flags = r.u8()?;
            let len = r.u32()? as usize;
            let bytes = if flags & 4 != 0 {
                let same = pats.get(r.u16()? as usize)?;
                same.bytes.clone()
            } else {
                r.take(len)?.to_vec()
            };
            if bytes.len() != len || len == 0 {
                return None;
            }
            let mask = r.take(len.div_ceil(8))?.to_vec();
            let (left, right) = (r.list16()?, r.list16()?);
            let calls = (0..r.u16()?)
                .map(|_| Some((r.u32()?, r.take(PROBE)?.try_into().ok()?, r.take(PROBE / 8)?.try_into().ok()?)))
                .collect::<Option<Vec<_>>>()?;
            pats.push(Pattern { rva, at_start: flags & 1 != 0, bytes, mask, left, right, calls });
        }
        let ok = |i: u16| (i as usize) < pats.len();
        if !pats.iter().all(|p| p.left.iter().chain(&p.right).all(|&i| ok(i))) {
            return None;
        }
        let mut sites = Vec::new();
        for _ in 0..r.u32()? {
            let name = r.name()?;
            let kind = r.u8()?;
            let (exact, shape, offset, n) = (r.u16()?, r.u16()?, r.u32()?, r.u32()?);
            let kind = match kind {
                0 => Kind::Func,
                1 => Kind::Inside,
                2 => Kind::Ret,
                3 => {
                    let callee = r.u16()?;
                    let listener = match r.u8()? {
                        0 => None,
                        1 => {
                            let lea = (r.u32()?, r.u32()?);
                            let (exact, shape) = (r.u16()?, r.u16()?);
                            let offsets = (0..r.u8()?)
                                .map(|_| {
                                    let name = r.name()?;
                                    let at =
                                        (0..r.u8()?).map(|_| Some((r.u32()?, r.u8()?))).collect::<Option<Vec<_>>>()?;
                                    Some((name, at))
                                })
                                .collect::<Option<Vec<_>>>()?;
                            let emits = (0..r.u8()?).map(|_| r.u32()).collect::<Option<Vec<_>>>()?;
                            if !ok(exact) || !ok(shape) {
                                return None;
                            }
                            Some(Listener { lea, exact, shape, offsets, emits })
                        }
                        _ => return None,
                    };
                    if !ok(callee) {
                        return None;
                    }
                    Kind::Call { callee, listener }
                }
                4 => {
                    let lea = (r.u32()?, r.u32()?);
                    let callback = r.u16()?;
                    if !ok(callback) {
                        return None;
                    }
                    Kind::Slot { lea, callback }
                }
                5 => Kind::Witness,
                _ => return None,
            };
            if !ok(exact) || !(shape == NO_PATTERN || ok(shape)) {
                return None;
            }
            sites.push(Site { name, kind, exact, shape, offset, n });
        }
        (r.at == b.len()).then_some(Proof { timestamp, pats, sites })
    }
}

/// The parts of an executable image the search reads: `.text`, `.rdata` (the vtables), the unwind table's function starts.
pub struct Image {
    pub timestamp: u32,
    /// What the absolute pointers in the data are relative to: the load address in the game, `ImageBase` in a file.
    pub va_base: u64,
    /// The game's own build number (`pe::build_number`), when the data holds it.
    pub build: Option<String>,
    text_rva: u32,
    text: Vec<u8>,
    rdata: (u32, Vec<u8>),
    starts: Vec<u32>,
}

impl Image {
    /// From the headers and a reader of one section's mapped bytes (`VirtualSize` long).
    pub fn from_sections(
        h: &pe::Headers,
        va_base: u64,
        mut read: impl FnMut(&pe::Section) -> Option<Vec<u8>>,
    ) -> Option<Image> {
        let text_s = h.section(".text")?;
        let rdata_s = h.section(".rdata")?;
        let pdata = read(h.section(".pdata")?)?;
        let (text, rdata) = (read(text_s)?, read(rdata_s)?);
        let end = text_s.rva.checked_add(text.len() as u32)?;
        let mut starts: Vec<u32> = pdata
            .as_chunks::<12>()
            .0
            .iter()
            .map(|e| u32::from_le_bytes([e[0], e[1], e[2], e[3]]))
            .filter(|&b| b >= text_s.rva && b < end)
            .collect();
        starts.sort_unstable();
        starts.dedup();
        Some(Image {
            timestamp: h.timestamp,
            va_base,
            build: pe::build_number(&rdata),
            text_rva: text_s.rva,
            text,
            rdata: (rdata_s.rva, rdata),
            starts,
        })
    }

    /// An executable file read as data (tests and the timing check): each section from its file bytes, zero-padded.
    pub fn from_file(file: &[u8]) -> Option<Image> {
        let h = pe::parse(file)?;
        Image::from_sections(&h, h.image_base, |s| {
            let raw = file.get(s.raw_ptr as usize..)?;
            let mut v = raw[..raw.len().min(s.raw_size.min(s.size) as usize)].to_vec();
            v.resize(s.size as usize, 0);
            Some(v)
        })
    }

    fn text_from(&self, rva: u32) -> Option<&[u8]> {
        self.text.get(rva.checked_sub(self.text_rva)? as usize..)
    }

    fn in_text(&self, rva: u64) -> bool {
        rva >= u64::from(self.text_rva) && rva < u64::from(self.text_rva) + self.text.len() as u64
    }

    fn code(&self, rva: u32, n: usize) -> Option<&[u8]> {
        self.text_from(rva)?.get(..n)
    }

    fn rel32_target(&self, at: u32) -> Option<u32> {
        let c = self.code(at, 5)?;
        if c[0] != 0xE8 {
            return None;
        }
        let t = i64::from(at) + 5 + i64::from(i32::from_le_bytes([c[1], c[2], c[3], c[4]]));
        u32::try_from(t).ok().filter(|&t| self.in_text(u64::from(t)))
    }

    /// Slot 2 (`_Do_call`) of the vtable a `lea r,[rip+d]` loads: displacement at `func + lea.0`, instruction end `func + lea.1`.
    /// Every slot 0..5 must point into `.text`, as `port_build.vtables` requires.
    fn vtable_slot2(&self, func: u32, lea: (u32, u32)) -> Option<u32> {
        let d = self.code(func.checked_add(lea.0)?, 4)?;
        let vt = i64::from(func) + i64::from(lea.1) + i64::from(i32::from_le_bytes(d.try_into().ok()?));
        let off = usize::try_from(vt - i64::from(self.rdata.0)).ok()?;
        let mut slots = [0u32; 5];
        for (k, s) in slots.iter_mut().enumerate() {
            let q = u64::from_le_bytes(self.rdata.1.get(off + 8 * k..off + 8 * k + 8)?.try_into().ok()?);
            let rva = q.checked_sub(self.va_base)?;
            if !self.in_text(rva) {
                return None;
            }
            *s = rva as u32;
        }
        Some(slots[2])
    }
}

/// Every place each pattern matches: function starts for the patterns of functions with an unwind entry (an index of the first
/// four bytes), all of `.text` for the others.
fn all_hits(p: &Proof, img: &Image) -> Vec<Vec<u32>> {
    let mut hits = vec![Vec::new(); p.pats.len()];
    let mut index: HashMap<[u8; 4], Vec<usize>> = HashMap::new();
    let mut loose = Vec::new();
    for (i, pat) in p.pats.iter().enumerate() {
        if !pat.at_start {
            continue;
        }
        if pat.bytes.len() >= 4 && (0..4).all(|k| fixed(&pat.mask, k)) {
            index.entry(pat.bytes[..4].try_into().unwrap()).or_default().push(i);
        } else {
            loose.push(i);
        }
    }
    for &s in &img.starts {
        let Some(hay) = img.text_from(s) else { continue };
        let keyed = hay.get(..4).and_then(|k| index.get(k)).map(Vec::as_slice).unwrap_or(&[]);
        for &i in keyed.iter().chain(&loose) {
            if p.pats[i].matches(hay) {
                hits[i].push(s);
            }
        }
    }
    for (i, pat) in p.pats.iter().enumerate().filter(|(_, pat)| !pat.at_start) {
        // the longest run of fixed bytes is the anchor
        let (mut best, mut run) = ((0, 0), None::<usize>);
        for k in 0..=pat.bytes.len() {
            match (k < pat.bytes.len() && fixed(&pat.mask, k), run) {
                (true, None) => run = Some(k),
                (false, Some(a)) => {
                    if k - a > best.1 {
                        best = (a, k - a);
                    }
                    run = None;
                }
                _ => {}
            }
        }
        let (a, needle) = (best.0, &pat.bytes[best.0..best.0 + best.1]);
        let text = &img.text;
        let mut from = a;
        while let Some(k) = text.get(from..).and_then(|t| t.iter().position(|&b| b == needle[0])) {
            let at = from + k;
            if text[at..].starts_with(needle) && pat.matches(&text[at - a..]) {
                hits[i].push(img.text_rva + (at - a) as u32);
            }
            from = at + 1;
        }
    }
    hits
}

/// Pattern index (the exact one) -> where it was found and whether by its shape pattern: what the cache keeps.
pub type Mapping = HashMap<u16, (u32, bool)>;

/// One run of the proof: a full search (`hits`), or a check of a cached mapping (`cache`).
struct Run<'a> {
    p: &'a Proof,
    img: &'a Image,
    hits: Option<Vec<Vec<u32>>>,
    cache: Option<&'a Mapping>,
    /// Pattern (exact) -> where it was found and whether by shape: what the cache keeps.
    mapped: Mapping,
}

impl Run<'_> {
    /// `port_build.Porter.map_function`: exact first; one match, or several copies of which exactly one sits at the shift of
    /// the nearest neighbours that match once; else (no exact match at all) the same with the shape pattern, if allowed.
    fn map(&mut self, exact: u16, shape: u16) -> Option<(u32, bool)> {
        let found = if let Some(cache) = self.cache {
            let (rva, sh) = *cache.get(&exact)?;
            let pat = if sh { (shape != NO_PATTERN).then_some(shape)? } else { exact };
            self.p.pats[pat as usize].matches(self.img.text_from(rva)?).then_some((rva, sh))?
        } else {
            let hits = self.hits.get_or_insert_with(|| all_hits(self.p, self.img));
            let mut out = None;
            for (pat, sh) in [(exact, false), (shape, true)] {
                if pat == NO_PATTERN {
                    break;
                }
                let h = &hits[pat as usize];
                if h.len() == 1 {
                    out = Some((h[0], sh));
                    break;
                }
                if h.len() > 1 {
                    let me = &self.p.pats[pat as usize];
                    let shifts: Vec<i64> = [&me.left, &me.right]
                        .iter()
                        .filter_map(|side| side.iter().find(|&&n| hits[n as usize].len() == 1))
                        .map(|&n| i64::from(hits[n as usize][0]) - i64::from(self.p.pats[n as usize].rva))
                        .collect();
                    let at: Vec<u32> =
                        h.iter().copied().filter(|&x| shifts.contains(&(i64::from(x) - i64::from(me.rva)))).collect();
                    // several copies that the neighbours do not single out: not located (no shape fallback, as port_build)
                    out = (at.len() == 1).then(|| (at[0], sh));
                    break;
                }
            }
            out?
        };
        self.mapped.insert(exact, found);
        Some(found)
    }

    /// `port_build.Porter.callees_agree`: every `call rel32` of the function leads to code that starts as the old callee did.
    fn callees_agree(&self, pat: u16, at: u32) -> bool {
        self.p.pats[pat as usize].calls.iter().all(|(off, probe, mask)| {
            at.checked_add(*off)
                .and_then(|c| self.img.rel32_target(c))
                .and_then(|t| self.img.code(t, PROBE))
                .is_some_and(|code| masked_eq(probe, mask, code))
        })
    }
}

#[derive(Default)]
struct Found {
    rva: HashMap<String, u32>,
    bytes: HashMap<String, Vec<u8>>,
    offsets: HashMap<String, usize>,
}

/// Every site in the proof's order; the first that is not proven is named.
fn run_sites(run: &mut Run) -> Result<Found, String> {
    let mut f = Found::default();
    for s in &run.p.sites {
        one_site(run, &mut f, s).ok_or_else(|| s.name.clone())?;
    }
    Ok(f)
}

fn one_site(run: &mut Run, f: &mut Found, s: &Site) -> Option<()> {
    let (p, img) = (run.p, run.img);
    let n = s.n as usize;
    let rva = match &s.kind {
        Kind::Func | Kind::Witness => {
            let (at, _) = run.map(s.exact, NO_PATTERN)?;
            if !run.callees_agree(s.exact, at) {
                return None;
            }
            at
        }
        Kind::Inside => run.map(s.exact, NO_PATTERN)?.0.checked_add(s.offset)?,
        Kind::Ret => {
            let at = run.map(s.exact, s.shape)?.0.checked_add(s.offset)?;
            if img.rel32_target(at)? != *f.rva.get("command_list_add")? {
                return None;
            }
            at + 5
        }
        Kind::Call { callee, listener } => {
            let at = run.map(s.exact, s.shape)?.0.checked_add(s.offset)?;
            let nt = img.rel32_target(at)?;
            if !p.pats[*callee as usize].matches(img.text_from(nt)?) {
                return None;
            }
            // a callee in several copies is fixed by the call itself; one that maps by its own body must map there
            if run.map(*callee, NO_PATTERN).is_some_and(|(x, _)| x != nt) {
                return None;
            }
            f.rva.insert(format!("{}.callee", s.name), nt);
            f.bytes.insert(format!("{}.callee", s.name), img.code(nt, n)?.to_vec());
            if let Some(l) = listener {
                let slot = img.vtable_slot2(nt, l.lea)?;
                if run.map(l.exact, l.shape)?.0 != slot {
                    return None;
                }
                let code = img.text_from(slot)?;
                for (name, places) in &l.offsets {
                    let mut seen = places.iter().map(|&(o, z)| {
                        let b = code.get(o as usize..o as usize + z as usize)?;
                        Some(if z == 1 {
                            i64::from(b[0] as i8)
                        } else {
                            i64::from(i32::from_le_bytes(b.try_into().ok()?))
                        })
                    });
                    let first = seen.next()??;
                    if first < 0 || !seen.all(|v| v == Some(first)) {
                        return None;
                    }
                    f.offsets.insert(name.clone(), first as usize);
                }
                let event = *f.rva.get("preview_event")?;
                if !l.emits.iter().any(|&o| img.rel32_target(slot + o) == Some(event)) {
                    return None;
                }
            }
            f.bytes.insert(s.name.clone(), img.code(at, 5)?.to_vec());
            f.rva.insert(s.name.clone(), at);
            return Some(());
        }
        Kind::Slot { lea, callback } => {
            let host = run.map(s.exact, s.shape)?.0;
            let d = img.vtable_slot2(host, *lea)?;
            if !p.pats[*callback as usize].matches(img.text_from(d)?) {
                return None;
            }
            d
        }
    };
    if n > 0 {
        f.bytes.insert(s.name.clone(), img.code(rva, n)?.to_vec());
    }
    f.rva.insert(s.name.clone(), rva);
    Some(())
}

fn leak(b: &[u8]) -> &'static [u8] {
    Box::leak(b.to_vec().into_boxed_slice())
}

/// The new row: the reference row with every searched field replaced; `None` when any field is missing.
fn row(reference: &BuildAddrs, img: &Image, f: &Found) -> Option<BuildAddrs> {
    let rva = |k: &str| f.rva.get(k).copied();
    let bytes = |k: &str| f.bytes.get(k).map(|b| leak(b));
    let hook = |k: &str| Some(HookSite { rva: rva(k)?, prologue: bytes(k)? });
    let call = |k: &str| Some(CallSite { rva: rva(k)?, prologue: bytes(k)? });
    let call_patch = |k: &str| {
        let c = format!("{k}.callee");
        Some(CallPatchSite {
            rva: rva(k)?,
            expected: bytes(k)?,
            callee: CallSite { rva: rva(&c)?, prologue: bytes(&c)? },
        })
    };
    let patch = |k: &str, like: &Option<PatchSite>| {
        let r = like.as_ref()?;
        let expected = bytes(k)?;
        (expected.len() == r.expected.len()).then_some(PatchSite { rva: rva(k)?, expected, replacement: r.replacement })
    };
    let list = |group: &str, len: usize| -> Option<&'static [Rva]> {
        let v = (0..len).map(|i| rva(&format!("{group}[{i}]"))).collect::<Option<Vec<_>>>()?;
        if f.rva.contains_key(&format!("{group}[{len}]")) {
            return None;
        }
        Some(Box::leak(v.into_boxed_slice()))
    };
    // the tickCount getter becomes the updateCount getter by its one displacement byte, as in every build so far
    let tick = {
        let expected = bytes("tick_count_getter")?;
        if expected.len() != 27 || expected[bt::TICK_GETTER_DISP_AT] != bt::TICK_COUNT_OFFSET {
            return None;
        }
        let mut replacement = expected.to_vec();
        replacement[bt::TICK_GETTER_DISP_AT] = bt::UPDATE_COUNT_OFFSET;
        PatchSite { rva: rva("tick_count_getter")?, expected, replacement: leak(&replacement) }
    };
    let po = reference.preview_owner.as_ref()?;
    let off = |k: &str| f.offsets.get(k).copied();
    // an engine function that is not found fails the row too (`Some(None)` is never produced)
    let e = |k: &str| rva(k).map(Some);
    Some(BuildAddrs {
        timestamp: img.timestamp,
        name: Box::leak(img.build.clone().unwrap_or_else(|| format!("{:#x}", img.timestamp)).into_boxed_str()),
        command_list_add: Some(hook("command_list_add")?),
        script_convert: Some(hook("script_convert")?),
        convert_update: Some(hook("convert_update")?),
        preview_event: Some(hook("preview_event")?),
        preview_owner: Some(PreviewOwnerSite {
            call: call_patch("preview_owner")?,
            y_a: off("y_a")?,
            y_b: off("y_b")?,
            z_getter_slot: po.z_getter_slot,
            r_c: off("r_c")?,
            r_dispatcher: off("r_dispatcher")?,
            ui_e: off("ui_e")?,
        }),
        apply_command: Some(hook("apply_command")?),
        script_pool: Some(patch("script_pool", &reference.script_pool)?),
        town_seed: Some(hook("town_seed")?),
        town_develop: Some(hook("town_develop")?),
        town_update_size: Some(hook("town_update_size")?),
        town_candidates: Some(hook("town_candidates")?),
        town_edges: Some(hook("town_edges")?),
        town_sort_call: Some(call_patch("town_sort_call")?),
        town_pick_call: Some(call_patch("town_pick_call")?),
        town_field_call: Some(call_patch("town_field_call")?),
        batch_pace: Some(call_patch("batch_pace")?),
        free_order: Some(call_patch("free_order")?),
        game_time_getter: Some(call("game_time_getter")?),
        tick_count_getter: Some(tick),
        lua_makeseed: Some(patch("lua_makeseed", &reference.lua_makeseed)?),
        command_move: Some(call("command_move")?),
        command_destroy: Some(call("command_destroy")?),
        handle_destroy: Some(call("handle_destroy")?),
        ui_add_sites: list("ui_add_sites", reference.ui_add_sites.len())?,
        fire_done_calls: list("fire_done_calls", reference.fire_done_calls.len())?,
        // read by command_list_add and the dispatcher, both matched exactly
        kind_offset: reference.kind_offset,
        kinds: reference.kinds,
        engine: EngineFns {
            proposal_ctor: e("proposal_ctor")?,
            proposal_dtor: e("proposal_dtor")?,
            proposal_copy: e("proposal_copy")?,
            proposal_assign: e("proposal_assign")?,
            engine_new: e("engine_new")?,
            engine_delete: e("engine_delete")?,
            refund_insert: e("refund_insert")?,
            add_entity: e("add_entity")?,
            remove_entity: e("remove_entity")?,
            game_time_advance: e("game_time_advance")?,
            component_lookup: e("component_lookup")?,
        },
        // every witness matched exactly (a site that is not found fails the whole search)
        layout: reference.layout,
    })
}

/// The searched row and the mapping the cache keeps; `cache`: check that mapping instead of searching.
pub fn search(proof: &Proof, img: &Image, cache: Option<&Mapping>) -> Result<(BuildAddrs, Mapping), String> {
    let reference = bt::select(proof.timestamp).ok_or("the proof's reference build is not in the table")?;
    let mut run = Run { p: proof, img, hits: None, cache, mapped: HashMap::new() };
    let found = run_sites(&mut run).map_err(|site| format!("site {site} is not proven"))?;
    let row = row(reference, img, &found).ok_or("the proof does not give every field of the row")?;
    Ok((row, run.mapped))
}

/// FNV-1a 64 of the row's every field: two games that found the same addresses have the same value (the join compares it).
pub fn row_hash(b: &BuildAddrs) -> String {
    let h = format!("{b:?}")
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, c| (h ^ u64::from(c)).wrapping_mul(0x100_0000_01b3));
    format!("{h:016x}")
}

fn proof_id() -> String {
    let h = PROOF.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &c| (h ^ u64::from(c)).wrapping_mul(0x100_0000_01b3));
    format!("{h:016x}")
}

/// The cache file's text: the proof it belongs to, then one line per mapped pattern.
pub fn cache_text(mapped: &Mapping) -> String {
    let mut lines: Vec<String> = mapped.iter().map(|(p, (r, s))| format!("{p} {r:x} {}", u8::from(*s))).collect();
    lines.sort();
    format!("FL3SEARCH 1 {}\n{}\n", proof_id(), lines.join("\n"))
}

pub fn parse_cache(text: &str) -> Option<Mapping> {
    let mut lines = text.lines();
    if lines.next()? != format!("FL3SEARCH 1 {}", proof_id()) {
        return None;
    }
    lines
        .map(|l| {
            let mut w = l.split(' ');
            let p = w.next()?.parse().ok()?;
            let r = u32::from_str_radix(w.next()?, 16).ok()?;
            let s = match w.next()? {
                "0" => false,
                "1" => true,
                _ => return None,
            };
            w.next().is_none().then_some((p, (r, s)))
        })
        .collect()
}

/// The row of the running game, found by search (the cache in `dir` first, re-checked; a full search when it fails).
pub fn find(img: &Image, dir: Option<&std::path::Path>, log: fn(&str)) -> Option<&'static BuildAddrs> {
    let proof = Proof::parse(PROOF)?;
    let path = dir.map(|d| d.join(format!("build-search-{:08x}.txt", img.timestamp)));
    let t0 = std::time::Instant::now();
    if let Some(cache) = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| parse_cache(&t)) {
        if let Ok((b, _)) = search(&proof, img, Some(&cache)) {
            log(&format!(
                "build search: the cached addresses checked again against the proof ({} ms)",
                t0.elapsed().as_millis()
            ));
            return Some(Box::leak(Box::new(b)));
        }
        log("build search: the cached addresses do not match the proof any more; searching again");
        if let Some(p) = &path {
            let _ = std::fs::remove_file(p);
        }
    }
    let (b, mapped) = search(&proof, img, None).map_err(|why| log(&format!("build search: {why}"))).ok()?;
    log(&format!("build search: every site proven in {} ms", t0.elapsed().as_millis()));
    if let Some(p) = &path {
        let _ = std::fs::write(p, cache_text(&mapped));
    }
    Some(Box::leak(Box::new(b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_proof_parses_and_belongs_to_a_known_row() {
        let p = Proof::parse(PROOF).expect("proof");
        assert!(bt::select(p.timestamp).is_some());
        assert_eq!(p.sites.iter().filter(|s| matches!(s.kind, Kind::Witness)).count(), 22);
    }

    /// A fake image whose `.text` is the proof's own patterns, each at its reference RVA (wildcards zero, callees and vtables
    /// absent), is enough to show that one changed byte in a hooked function fails the whole search.
    #[test]
    fn one_changed_byte_in_a_hooked_function_fails_the_whole_search() {
        let p = Proof::parse(PROOF).unwrap();
        let s = p.sites.iter().find(|s| s.name == "command_list_add").unwrap();
        let pat = &p.pats[s.exact as usize];
        let text_rva = pat.rva - 0x10;
        let mut text = vec![0xCCu8; pat.bytes.len() + 0x20];
        text[0x10..0x10 + pat.bytes.len()].copy_from_slice(&pat.bytes);
        let img = |text: Vec<u8>| Image {
            timestamp: 1,
            va_base: 0x1_4000_0000,
            build: None,
            text_rva,
            text,
            rdata: (0, Vec::new()),
            starts: vec![pat.rva],
        };
        let mut run = Run { p: &p, img: &img(text.clone()), hits: None, cache: None, mapped: HashMap::new() };
        assert_eq!(run.map(s.exact, NO_PATTERN), Some((pat.rva, false)), "the unchanged function is found");
        let k = (0..pat.bytes.len()).rev().find(|&k| fixed(&pat.mask, k)).unwrap();
        text[0x10 + k] ^= 1;
        let changed = img(text);
        let mut run = Run { p: &p, img: &changed, hits: None, cache: None, mapped: HashMap::new() };
        assert_eq!(run.map(s.exact, NO_PATTERN), None);
        assert_eq!(search(&p, &changed, None).err().as_deref(), Some("site command_list_add is not proven"));
    }

    /// The real executables (read as data, never run; not in the repository): `FEVERLINK3_EXE_40420` and `FEVERLINK3_EXE_40425`.
    /// The embedded 40425 proof run on 40420 must give the 40420 row exactly, and the 40420 proof (`FEVERLINK3_PROOF_40420`,
    /// `port_build.py <40420 exe> --export-proof`) run on 40425 the 40425 row. Prints the search time.
    #[test]
    #[ignore]
    fn the_search_reproduces_the_rows_of_40420_and_40425() {
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
        let exe = |k: &str| Image::from_file(&std::fs::read(var(k)).unwrap()).unwrap();
        let cases = [
            (PROOF.to_vec(), exe("FEVERLINK3_EXE_40420"), 0x6ac5_0427),
            (std::fs::read(var("FEVERLINK3_PROOF_40420")).unwrap(), exe("FEVERLINK3_EXE_40425"), 0x6ad9_ce10),
        ];
        for (proof, img, want) in cases {
            let proof = Proof::parse(&proof).unwrap();
            let t = std::time::Instant::now();
            let (b, mapped) = search(&proof, &img, None).unwrap();
            println!(
                "proof {:#x} on {want:#x}: {} ms over {} MB .text",
                proof.timestamp,
                t.elapsed().as_millis(),
                img.text.len() >> 20
            );
            assert_eq!(&b, bt::select(want).unwrap());
            let t = std::time::Instant::now();
            let cache = parse_cache(&cache_text(&mapped)).unwrap();
            assert_eq!(&search(&proof, &img, Some(&cache)).unwrap().0, bt::select(want).unwrap());
            println!("  the cached mapping checked again in {} ms", t.elapsed().as_millis());
        }
    }
}
