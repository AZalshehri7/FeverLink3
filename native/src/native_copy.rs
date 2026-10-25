//! Track A, step A1 (`FEVERLINK3_NATIVE_COPY`): the engine's own `Proposal` functions on scratch objects.
//!
//! The question: can a native module make an engine-owned `Proposal` equal to a held one, with the engine doing the
//! allocation and the deep copy (maps, hash tables, strings, vectors, elements), instead of us laying containers out
//! by hand? The functions are those of build 40420, found in the executable and compared byte for byte before use:
//!
//! - `0x4c6870` `Proposal::Proposal()`: default constructor (valid empty containers: the hash lists' 0x18-byte sentinels
//!   with mask 7, the `std::map` sentinel of 0x50 bytes).
//! - `0x48e220` `Proposal::~Proposal()`.
//! - `0x4c6640` `Proposal::Proposal(const Proposal&)`: a true deep copy (the source is only read).
//!
//! (The payload-level functions found earlier, `0x9e7b70`, `0x9e5220`, `0x4cebb0`, are MOVE operations: they steal the
//! source's containers. They are not used here; a held command is never given to them.)
//!
//! Three steps, each logged BEFORE the engine is called (the module has no exception or access-violation handler, so the
//! log shows where a crash happened), all on scratch memory that is never handed to the engine's command list:
//!
//! 1. construct an empty `Proposal` and destroy it;
//! 2. copy-construct from the held command's `Proposal` (read-only source) into scratch, write the copy as portable text,
//!    compare it with the held command's text, destroy the copy;
//! 3. build a source `Proposal` from the portable text (a default-constructed engine object whose vectors, strings and
//!    scalars are filled from the text by [`crate::rebuild`], containers left to the engine), copy-construct from that,
//!    write the copy as text and compare it with the held command's text. Refused when the text has pointers the
//!    rebuild cannot resolve.
//!
//! One general rule: the layout table of [`crate::portable`] decides what is built and compared, for every kind of build.

use crate::dump::{Env, Mem, PAYLOAD_SIZE};
use crate::portable::{self, Header, Settings, Ty, F};
use crate::rebuild::{self, Alloc};

pub const NATIVE_COPY_VAR: &str = "FEVERLINK3_NATIVE_COPY";
pub const NATIVE_COPY_FILE: &str = "native_copy.log";

/// Size of the `Proposal` object inside a payload (the Context starts after it).
pub const PROPOSAL_SIZE: usize = 0x358;

pub type CtorFn = unsafe extern "system-unwind" fn(*mut u8) -> *mut u8;
pub type DtorFn = unsafe extern "system-unwind" fn(*mut u8);
pub type CopyFn = unsafe extern "system-unwind" fn(*mut u8, *const u8) -> *mut u8;

/// The engine's `Proposal` functions.
#[derive(Clone, Copy)]
pub struct ProposalFns {
    pub ctor: CtorFn,
    pub dtor: DtorFn,
    pub copy: CopyFn,
    /// `Proposal& operator=(const Proposal&)`: a deep copy into an object that already exists.
    pub assign: CopyFn,
}

impl ProposalFns {
    /// The functions of the running game at the build table's places, after their first bytes are compared.
    #[cfg(windows)]
    pub fn at(base: usize, engine: &crate::build_table::EngineFns) -> Result<ProposalFns, String> {
        // the bytes the build search recorded at each place (it proved the whole function there)
        let check = |rva: Option<u32>, name: &str, what: &str| -> Result<usize, String> {
            let rva = crate::build_table::engine_rva(rva, what)?;
            let want = engine.prologue(name)?;
            let got = crate::dump::LiveMem
                .read(base + rva as usize, want.len())
                .ok_or_else(|| format!("{what} at {rva:#x} is not readable"))?;
            (got == want)
                .then_some(base + rva as usize)
                .ok_or_else(|| format!("{what} at {rva:#x} does not have the expected bytes"))
        };
        let ctor = check(engine.proposal_ctor, "proposal_ctor", "Proposal::Proposal()")?;
        let dtor = check(engine.proposal_dtor, "proposal_dtor", "Proposal::~Proposal()")?;
        let copy = check(engine.proposal_copy, "proposal_copy", "Proposal::Proposal(const Proposal&)")?;
        let assign = check(engine.proposal_assign, "proposal_assign", "Proposal::operator=(const Proposal&)")?;
        // SAFETY: the four addresses were just compared with the prologues of the engine functions of those signatures.
        unsafe {
            Ok(ProposalFns {
                ctor: core::mem::transmute::<usize, CtorFn>(ctor),
                dtor: core::mem::transmute::<usize, DtorFn>(dtor),
                copy: core::mem::transmute::<usize, CopyFn>(copy),
                assign: core::mem::transmute::<usize, CopyFn>(assign),
            })
        }
    }
}

/// A zeroed, aligned stand-in for a payload. It is a plain Rust buffer: nothing in the engine knows it, and only the
/// `Proposal` at its start is ever constructed in it.
#[repr(C, align(64))]
pub struct Scratch([u8; PAYLOAD_SIZE + 0x40]);

impl Scratch {
    pub fn new() -> Box<Scratch> {
        Box::new(Scratch([0u8; PAYLOAD_SIZE + 0x40]))
    }
    pub fn ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr()
    }
    pub fn addr(&self) -> usize {
        self.0.as_ptr() as usize
    }
    pub fn bytes(&self) -> &[u8] {
        &self.0[..PAYLOAD_SIZE]
    }
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.0[..PAYLOAD_SIZE]
    }
}

pub struct Ctx<'a> {
    pub fns: ProposalFns,
    pub alloc: &'a dyn Alloc,
    pub mem: &'a dyn Mem,
    pub base: usize,
    pub set: Settings,
    pub build_name: &'a str,
}

// ---- the dry pass
//
// The engine part of a build (the injection, the native copy, the rebuild check, the patch writes) runs outside `log::guarded`, so
// that an exception of the game reaches the game's handler (`log::guarded`). The same code also parses text that came from the other
// game, where a bug of ours is far likelier than an exception of the engine. So it runs twice: first DRY, inside a guard, with the
// engine's functions replaced by plain byte copies, a Rust arena for the game's heap and writes into live memory dropped; then for
// real. The code is deterministic on the same text, so a text that gets through the dry pass does not panic the parser in the real
// one; a panic in the dry pass is a refusal, as it was when the whole path was guarded.

/// Where the dry pass's arena places its blocks (far from the game's heaps; its blocks are read before live memory anyway).
pub const DRY_ARENA: usize = 0x7d00_0000_0000;

/// What the dry pass reads: its own arena first, else the memory the real pass would read.
pub struct DryMem<'a> {
    pub arena: &'a rebuild::Arena,
    pub live: &'a dyn Mem,
}

impl Mem for DryMem<'_> {
    fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
        self.arena.read(addr, len).or_else(|| self.live.read(addr, len))
    }
    fn heap_size(&self, addr: usize) -> Option<usize> {
        self.arena.heap_size(addr).or_else(|| self.live.heap_size(addr))
    }
}

thread_local! {
    /// The reader of the dry pass running on this thread (the stand-in copy reads its source through it).
    static DRY_READER: std::cell::Cell<Option<*const (dyn Mem + 'static)>> = const { std::cell::Cell::new(None) };
}

unsafe extern "system-unwind" fn dry_ctor(p: *mut u8) -> *mut u8 {
    p // the scratch buffers are zeroed: an "empty Proposal" of plain bytes
}

unsafe extern "system-unwind" fn dry_dtor(_p: *mut u8) {}

/// The stand-in for the copy constructor and the copy assignment: the source's bytes, read through the dry pass's reader.
unsafe extern "system-unwind" fn dry_copy(d: *mut u8, s: *const u8) -> *mut u8 {
    let reader = DRY_READER.with(|r| r.get());
    // SAFETY: set by `dry_pass` to a reader that outlives the pass, and cleared when it ends.
    let bytes = reader.and_then(|m| unsafe { (*m).read(s as usize, PROPOSAL_SIZE) });
    if let Some(b) = bytes {
        // SAFETY: `d` is a scratch buffer of the dry pass, larger than a Proposal.
        unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), d, PROPOSAL_SIZE) };
    }
    d
}

/// Drops every write into live memory (the real pass makes them).
pub struct DryPoke;

impl crate::cmd_patch::Poke for DryPoke {
    fn poke(&self, _addr: usize, _data: &[u8]) -> bool {
        true
    }
}

/// Runs `f` as the dry pass of a piece of engine work: inside `log::guarded`, with a [`Ctx`] whose functions, allocator and memory are
/// the stand-ins above (`live` is what the real pass reads). `Err` (the refusal) when `f` panicked; what `f` itself concluded is not
/// looked at (the stand-ins are not the engine: only a panic counts).
pub fn dry_pass(
    site: &'static str,
    live: &dyn Mem,
    base: usize,
    set: Settings,
    build_name: &str,
    f: impl FnOnce(&Ctx),
) -> Result<(), String> {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            let _ = DRY_READER.try_with(|r| r.set(None));
        }
    }
    let arena = rebuild::Arena::new(DRY_ARENA);
    let mem = DryMem { arena: &arena, live };
    let reader: *const (dyn Mem + '_) = &mem;
    // SAFETY: only the lifetime is erased; `Clear` takes the pointer away before `mem` goes out of scope.
    let reader: *const (dyn Mem + 'static) = unsafe { core::mem::transmute(reader) };
    DRY_READER.with(|r| r.set(Some(reader)));
    let _clear = Clear;
    let fns = ProposalFns { ctor: dry_ctor, dtor: dry_dtor, copy: dry_copy, assign: dry_copy };
    let ctx = Ctx { fns, alloc: &arena, mem: &mem, base, set, build_name };
    crate::log::guarded(site, || f(&ctx)).ok_or_else(|| {
        format!(
            "{site}: our own code panicked on this text (a bug of the module, the PANIC line in native.log says where); refused before any call into the game"
        )
    })
}

/// The entry count the text gives for an engine container (`map:size=N` or `hash:size=N,...`), 0 when it has no line.
pub fn container_entries(lines: &std::collections::BTreeMap<&str, &str>, name: &str) -> u64 {
    lines
        .get(name)
        .and_then(|v| v.split("size=").nth(1))
        .and_then(|v| v.split(',').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// True for the text of an unnamed region that the rebuild can make completely: no `P` (a pointer to something the text does not
/// describe) in it. Words, zero runs, image offsets, strings, vectors and references to blocks of the heap graph are fine.
pub fn raw_is_plain(value: &str) -> bool {
    let Some(body) = value.strip_prefix("raw:") else { return false };
    body.starts_with("b:") || !body.split(' ').any(|t| t == "P")
}

/// Whether the rebuild takes member `f` from the text into an engine-constructed Proposal (step 3's rule, shared with `inject.rs`).
pub(crate) fn selected(f: &F, lines: &std::collections::BTreeMap<&str, &str>) -> bool {
    if f.off >= PROPOSAL_SIZE {
        return true;
    }
    match f.ty {
        Ty::Map(_) | Ty::Hash(_) => {
            container_entries(lines, f.name) > 0 && lines.contains_key(format!("{}.graph", f.name).as_str())
        }
        Ty::Skip(_) => false,
        Ty::Raw(_) => lines.get(f.name).is_some_and(|v| raw_is_plain(v)),
        _ => true,
    }
}

/// Lines of the comparison split into those of named members and those of unnamed bytes.
fn split_diff(a: &str, b: &str) -> (usize, usize, usize, usize, Vec<String>) {
    let (x, y) = (rebuild::lines_of(a), rebuild::lines_of(b));
    let mut paths: Vec<&str> = x.keys().chain(y.keys()).copied().collect();
    paths.sort_unstable();
    paths.dedup();
    let (mut named_eq, mut named_diff, mut un_eq, mut un_diff) = (0, 0, 0, 0);
    let mut first: Vec<String> = Vec::new();
    for p in paths {
        // unnamed: padding gaps, and the heap graph behind container pointers (node padding differs on every run)
        let unnamed = portable::is_unnamed_path(p);
        let same = x.get(p) == y.get(p);
        match (unnamed, same) {
            (false, true) => named_eq += 1,
            (false, false) => {
                named_diff += 1;
                if first.len() < 12 {
                    first.push(format!(
                        "{p}: {} <> {}",
                        x.get(p).map_or("(absent)", |v| v).chars().take(120).collect::<String>(),
                        y.get(p).map_or("(absent)", |v| v).chars().take(120).collect::<String>()
                    ));
                }
            }
            (true, true) => un_eq += 1,
            (true, false) => un_diff += 1,
        }
    }
    (named_eq, named_diff, un_eq, un_diff, first)
}

pub(crate) fn describe_split(a: &str, b: &str) -> (usize, Vec<String>) {
    let (ne, nd, ue, ud, first) = split_diff(a, b);
    let blocks = |t: &str| t.lines().filter(|l| l.starts_with("@B")).count();
    let mut lines = vec![format!(
        "  named lines: {ne} equal, {nd} differ; unnamed bytes and heap graph: {ue} equal, {ud} differ (padding and residue are expected to differ); heap blocks: {} in the held command, {} in the copy",
        blocks(a),
        blocks(b)
    )];
    lines.extend(first.iter().map(|f| format!("  DIFF {f}")));
    (nd, lines)
}

/// What [`residue_mask`] found.
pub struct MaskInfo {
    pub mask: portable::Mask,
    /// Bytes of unnamed regions that both copies agree on / that differ (residue), and the number of regions.
    pub carried: usize,
    pub residue: usize,
    pub regions: usize,
    pub ms: u128,
}

impl MaskInfo {
    /// The carried byte ranges of the regions whose path starts with `prefix` (for the log), e.g. `toAdd[0].`.
    pub fn carried_ranges(&self, prefix: &str) -> String {
        let mut out = Vec::new();
        for (name, m) in self.mask.range(prefix.to_string()..).take_while(|(n, _)| n.starts_with(prefix)) {
            let base = name
                .rsplit_once(".~")
                .and_then(|(_, o)| usize::from_str_radix(o.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0);
            let mut k = 0;
            let mut ranges = Vec::new();
            while k < m.len() {
                if m[k] {
                    let from = k;
                    while k < m.len() && m[k] {
                        k += 1;
                    }
                    ranges.push(format!("{:#x}-{:#x}", base + from, base + k));
                } else {
                    k += 1;
                }
            }
            out.push(format!(
                "{}: {}",
                name.trim_start_matches(prefix),
                if ranges.is_empty() { "none".into() } else { ranges.join(",") }
            ));
        }
        out.join("; ")
    }
}

/// The differential residue mask of the held command at `held`: the engine's copy constructor makes two copies of its Proposal into
/// scratch buffers prefilled with 0x00 and with 0xFF; a byte of an unnamed region that both copies agree on was carried by the copy
/// (a value), one that differs was left alone by it (residue of the memory the original lives in). Both copies are destroyed on every
/// path, and nothing of them is handed to the engine. Read only on `held`.
///
/// LIMIT (b51 to b53): the mask cannot tell residue from a value for bytes that the engine's copy constructor copies VERBATIM from the
/// source (a memcpy of a whole element, padding included, puts the same residue into both scratch copies, so it looks carried). It
/// works for the bytes the copy constructor leaves alone (gaps of the Proposal object, unconstructed padding). Nothing may therefore
/// rest on it for the unnamed words of a construction entity: that proof rests on named members only (and on b45).
///
/// Calls the engine through `fns`: never inside `log::guarded` unless they are the dry pass's stand-ins (the text of the two copies in between
/// is guarded here).
pub fn residue_mask(
    fns: &ProposalFns,
    mem: &dyn Mem,
    base: usize,
    set: Settings,
    build_name: &str,
    held: usize,
    say: &mut dyn FnMut(&str),
) -> Result<MaskInfo, String> {
    let t0 = std::time::Instant::now();
    let src = mem.read(held, PAYLOAD_SIZE).ok_or("the held payload is not readable")?;
    let mut a = Scratch::new();
    let mut b = Scratch::new();
    b.bytes_mut().fill(0xFF);
    say("native inject: residue mask: calling the engine copy constructor twice (scratch prefilled 0x00 and 0xFF)");
    // SAFETY: `held` is a live payload (only read by a copy constructor); the destinations are scratch buffers.
    unsafe {
        (fns.copy)(a.ptr(), held as *const u8);
        (fns.copy)(b.ptr(), held as *const u8);
    }
    for c in [&mut a, &mut b] {
        c.bytes_mut()[PROPOSAL_SIZE..].copy_from_slice(&src[PROPOSAL_SIZE..]);
    }
    let plain = Settings { graph: false, typed: false, ..set };
    let collect = |s: &Scratch| -> Result<portable::Regions, String> {
        let cell = std::cell::RefCell::new(portable::Regions::new());
        let head = Header {
            seq: 0,
            event: "mask",
            hold_id: None,
            site_rva: None,
            tool: None,
            t_ms: 0,
            wall_ms: 0,
            payload: s.addr(),
        };
        portable::encode_with(
            mem,
            Env { image_base: base },
            plain,
            build_name,
            &head,
            portable::EncodeOpts { collect: Some(&cell), mask: None },
        )?;
        Ok(cell.into_inner())
    };
    // reading the copies is ours: contained, so that the copies are destroyed whatever happens to it
    let regions = crate::log::guarded("residue mask (text)", || (collect(&a), collect(&b)));
    let (ra, rb) = regions.unwrap_or_else(|| {
        let e = || Err("a panic was contained while the copies were read".to_string());
        (e(), e())
    });
    say("native inject: residue mask: calling the engine destructor on both copies");
    // SAFETY: both were constructed above and are destroyed once, on every path.
    unsafe {
        (fns.dtor)(a.ptr());
        (fns.dtor)(b.ptr());
    }
    let (ra, rb) = (ra?, rb?);
    let mut info = MaskInfo { mask: portable::Mask::new(), carried: 0, residue: 0, regions: 0, ms: 0 };
    for (name, x) in &ra {
        let m: Vec<bool> = match rb.get(name) {
            Some(y) if y.len() == x.len() => x.iter().zip(y).map(|(p, q)| p == q).collect(),
            _ => vec![false; x.len()],
        };
        info.carried += m.iter().filter(|k| **k).count();
        info.residue += m.iter().filter(|k| !**k).count();
        info.regions += 1;
        info.mask.insert(name.clone(), m);
    }
    info.ms = t0.elapsed().as_millis();
    Ok(info)
}

/// Every differing line of a comparison (named and unnamed), both sides in full up to 400 characters, for a report file.
pub(crate) fn diff_all(a: &str, b: &str) -> Vec<String> {
    let (x, y) = (rebuild::lines_of(a), rebuild::lines_of(b));
    let mut paths: Vec<&str> = x.keys().chain(y.keys()).copied().collect();
    paths.sort_unstable();
    paths.dedup();
    let cut = |v: Option<&&str>| v.map_or("(absent)".to_string(), |v| v.chars().take(400).collect::<String>());
    paths
        .into_iter()
        .filter(|p| x.get(p) != y.get(p))
        .map(|p| {
            format!(
                "{} {p}\n    original: {}\n    rebuilt:  {}",
                if portable::is_unnamed_path(p) { "unnamed" } else { "NAMED  " },
                cut(x.get(p)),
                cut(y.get(p))
            )
        })
        .collect()
}

/// The verdict of a comparison, from the counts: MATCH when no named member differs, else how many do.
fn verdict(named_diff: Result<usize, &str>, what: &str) -> String {
    let named_diff = match named_diff {
        Ok(n) => n,
        Err(e) => return format!("NOT COMPARED ({what} could not be written as text: {e})"),
    };
    if named_diff == 0 {
        format!("MATCH ({what} equals the held one in every named member; only unnamed bytes may differ)")
    } else {
        format!("DIFF ({what} differs from the held one in {named_diff} named line(s), listed above)")
    }
}

/// The text of a scratch Proposal. Reads only, and is contained (`log::guarded`): its callers hold engine objects that must be
/// destroyed whatever happens to the text.
pub(crate) fn encode_scratch(c: &Ctx, s: &Scratch, event: &str) -> Result<String, String> {
    let head =
        Header { seq: 0, event, hold_id: None, site_rva: None, tool: None, t_ms: 0, wall_ms: 0, payload: s.addr() };
    crate::log::guarded("scratch text", || {
        portable::encode(c.mem, Env { image_base: c.base }, c.set, c.build_name, &head).map(|p| p.text)
    })
    .unwrap_or_else(|| Err("a panic was contained while the object was written as text".into()))
}

/// The three steps for the held command whose payload is at `src`. `say` receives each line as soon as it is known; the
/// lines before an engine call say what is about to be called. Calls the engine through `c`: never inside `log::guarded` unless `c` is
/// the dry pass's ([`dry_pass`]).
pub fn run(c: &Ctx, src: usize, say: &mut dyn FnMut(&str)) -> String {
    let mut out = String::new();
    let mut put = |out: &mut String, line: &str| {
        out.push_str(line);
        out.push('\n');
        say(line);
    };
    let head =
        Header { seq: 0, event: "held", hold_id: None, site_rva: None, tool: None, t_ms: 0, wall_ms: 0, payload: src };
    let (src_text, src_origins, src_unresolved) =
        match portable::encode(c.mem, Env { image_base: c.base }, c.set, c.build_name, &head) {
            Ok(p) => (p.text, p.origins, p.unresolved),
            Err(e) => {
                put(&mut out, &format!("REFUSED: the held command cannot be written as portable text: {e}"));
                return out;
            }
        };
    let Some(src_bytes) = c.mem.read(src, PAYLOAD_SIZE) else {
        put(&mut out, "REFUSED: the held payload is not readable");
        return out;
    };

    // ---- step 1
    put(&mut out, "STEP 1: Proposal::Proposal() on scratch memory, then Proposal::~Proposal()");
    let mut s1 = Scratch::new();
    put(&mut out, "  calling the engine constructor");
    // SAFETY: a 64-aligned zeroed buffer larger than a Proposal; the signature is the verified function's.
    unsafe { (c.fns.ctor)(s1.ptr()) };
    s1.bytes_mut()[0x9b8] = portable::KIND_BUILD_PROPOSAL;
    match encode_scratch(c, &s1, "empty") {
        Ok(t) => {
            let nonempty = t.lines().filter(|l| l.contains("\tn:") && !l.contains("\tn:0")).count();
            put(
                &mut out,
                &format!("  constructed; written as text: {} lines, {nonempty} non-empty vectors", t.lines().count()),
            );
        }
        Err(e) => put(&mut out, &format!("  constructed, but the empty object does not decode: {e}")),
    }
    put(&mut out, "  calling the engine destructor");
    // SAFETY: the object was constructed above and is destroyed once.
    unsafe { (c.fns.dtor)(s1.ptr()) };
    put(&mut out, "STEP 1: OK (no crash)");

    // ---- step 2
    put(
        &mut out,
        "STEP 2: Proposal::Proposal(const Proposal&) from the held command's Proposal (read only) into scratch",
    );
    let mut s2 = Scratch::new();
    put(&mut out, "  calling the engine copy constructor");
    // SAFETY: `src` is the held payload (its Proposal is only read by a copy constructor); the destination is scratch.
    unsafe { (c.fns.copy)(s2.ptr(), src as *const u8) };
    // the Context and the rest are not part of the Proposal: take the scalars from the source bytes
    s2.bytes_mut()[PROPOSAL_SIZE..].copy_from_slice(&src_bytes[PROPOSAL_SIZE..]);
    let step2_diff = match encode_scratch(c, &s2, "copy") {
        Ok(t) => {
            let (nd, lines) = describe_split(&src_text, &t);
            for l in &lines {
                put(&mut out, l);
            }
            Ok(nd)
        }
        Err(e) => {
            put(&mut out, &format!("  the copy cannot be written as text: {e}"));
            Err(e)
        }
    };
    put(&mut out, "  calling the engine destructor on the copy");
    // SAFETY: constructed above, destroyed once. The Context bytes were copied from the source and are not destroyed.
    unsafe { (c.fns.dtor)(s2.ptr()) };
    put(
        &mut out,
        &format!(
            "STEP 2: {}",
            verdict(step2_diff.as_ref().map(|n| *n).map_err(|e| e.as_str()), "the engine's copy of the Proposal")
        ),
    );

    // ---- step 2b
    put(&mut out, "STEP 2b: Proposal::operator=(const Proposal&) from the held command's Proposal into an engine-constructed Proposal");
    let mut s2b = Scratch::new();
    put(&mut out, "  calling the engine constructor");
    // SAFETY: as in step 1.
    unsafe { (c.fns.ctor)(s2b.ptr()) };
    put(&mut out, "  calling the engine copy assignment");
    // SAFETY: the destination is a constructed Proposal on scratch, the source (the held payload) is only read.
    unsafe { (c.fns.assign)(s2b.ptr(), src as *const u8) };
    s2b.bytes_mut()[PROPOSAL_SIZE..].copy_from_slice(&src_bytes[PROPOSAL_SIZE..]);
    let step2b_diff = match encode_scratch(c, &s2b, "assigned") {
        Ok(t) => {
            let (nd, lines) = describe_split(&src_text, &t);
            for l in &lines {
                put(&mut out, l);
            }
            Ok(nd)
        }
        Err(e) => {
            put(&mut out, &format!("  the assigned object cannot be written as text: {e}"));
            Err(e)
        }
    };
    put(&mut out, "  calling the engine destructor");
    // SAFETY: constructed above, destroyed once.
    unsafe { (c.fns.dtor)(s2b.ptr()) };
    put(
        &mut out,
        &format!(
            "STEP 2b: {}",
            verdict(
                step2b_diff.as_ref().map(|n| *n).map_err(|e| e.as_str()),
                "the Proposal after the engine's copy assignment"
            )
        ),
    );

    // ---- step 3
    put(
        &mut out,
        "STEP 3: a source Proposal built from the portable text, copied by the engine, compared with the held command",
    );
    // Top-level members below the Context go into an engine-constructed Proposal, except containers without a heap graph in
    // the text (the engine's empty ones stay) and unnamed regions that hold more than plain words (a pointer, a vector, a string there is
    // left to the engine's default); the Context region and the flags are plain scalars.
    let lines = rebuild::lines_of(&src_text);
    let select = |f: &F| selected(f, &lines);
    let built = match rebuild::build_selected(&src_text, c.base, c.alloc, Some(&select)) {
        Ok(b) => b,
        Err(e) => {
            c.alloc.free_all();
            put(&mut out, &format!("STEP 3: REFUSED, the text cannot be built: {e}"));
            return out;
        }
    };
    let unresolved: Vec<&rebuild::Blocked> = built.blocked.iter().filter(|b| b.kind == "heap pointer").collect();
    if !unresolved.is_empty() {
        let n = unresolved.len();
        let mut by_member: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for u in &unresolved {
            *by_member.entry(rebuild::member_of(&u.path)).or_default() += 1;
        }
        let members: Vec<String> = by_member.iter().map(|(m, k)| format!("{m} x{k}")).collect();
        let first = unresolved.first().map_or(String::new(), |b| b.path.clone());
        c.alloc.free_all();
        put(&mut out, &format!("  text: {}", src_text.lines().last().unwrap_or("")));
        let ent: Vec<String> = lines
            .keys()
            .filter(|k| k.ends_with(".graph"))
            .filter_map(|k| {
                let name = &k[..k.len() - 6];
                let n = container_entries(&lines, name);
                (n > 0).then(|| format!("{name} has {n} entries"))
            })
            .collect();
        if !ent.is_empty() {
            put(&mut out, &format!("  containers with entries: {}", ent.join(", ")));
        }
        let origins: Vec<String> =
            src_origins.iter().take(6).map(|(m, n, b)| format!("{m} {n} blocks/{b} bytes")).collect();
        put(&mut out, &format!("  heap graph reached from: {}", origins.join("; ")));
        for (member, addr) in src_unresolved.iter().take(6) {
            put(&mut out, &format!("  pointer {addr:#x} in {member} stayed P"));
        }
        put(
            &mut out,
            &format!("STEP 3: SKIPPED, {n} heap pointers in the text cannot be resolved ({}; first at {first}); copying a null where the engine expects an object could crash the game", members.join(", ")),
        );
        return out;
    }
    let mut p = Scratch::new();
    put(&mut out, "  calling the engine constructor for the source");
    // SAFETY: as in step 1.
    unsafe { (c.fns.ctor)(p.ptr()) };
    // transplant: the selected top-level members below the Context move from the built bytes into the constructed object
    let mut moved = 0usize;
    for f in portable::PAYLOAD.iter().filter(|f| select(f) && f.off < PROPOSAL_SIZE) {
        let (o, n) = (f.off, f.ty.size());
        p.bytes_mut()[o..o + n].copy_from_slice(&built.payload[o..o + n]);
        moved += 1;
    }
    // the blocks the builder made now belong to the source object; its destructor frees them
    c.alloc.forget_all();
    put(&mut out, &format!("  source filled from the text: {moved} members, {} heap blocks owned by it", built.blocks));
    let mut d = Scratch::new();
    put(&mut out, "  calling the engine copy constructor on the source");
    // SAFETY: `p` holds a constructed Proposal (read only by the copy constructor); `d` is scratch.
    unsafe { (c.fns.copy)(d.ptr(), p.addr() as *const u8) };
    d.bytes_mut()[PROPOSAL_SIZE..].copy_from_slice(&built.payload[PROPOSAL_SIZE..]);
    d.bytes_mut()[0x9b8] = portable::KIND_BUILD_PROPOSAL;
    let step3_diff = match encode_scratch(c, &d, "rebuilt") {
        Ok(t) => {
            let (nd, lines) = describe_split(&src_text, &t);
            for l in &lines {
                put(&mut out, l);
            }
            Ok(nd)
        }
        Err(e) => {
            put(&mut out, &format!("  the copy cannot be written as text: {e}"));
            Err(e)
        }
    };
    put(&mut out, "  calling the engine destructor on the copy and on the source");
    // SAFETY: both were constructed above, each is destroyed once; the builder's blocks are freed with `p`.
    unsafe {
        (c.fns.dtor)(d.ptr());
        (c.fns.dtor)(p.ptr());
    }
    put(
        &mut out,
        &format!(
            "STEP 3: {}",
            verdict(
                step3_diff.as_ref().map(|n| *n).map_err(|e| e.as_str()),
                "the Proposal the engine made from the portable text"
            )
        ),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_reports_an_error_not_a_count() {
        let v = verdict(Err("text over the size bound"), "the copy");
        assert!(v.starts_with("NOT COMPARED") && v.contains("size bound") && !v.contains("1844"));
        assert!(verdict(Ok(0), "x").starts_with("MATCH") && verdict(Ok(3), "x").contains("3 named"));
    }
    use crate::portable::testdata::{synthetic, BASE};
    use crate::rebuild::Arena;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CTORS: AtomicUsize = AtomicUsize::new(0);
    static DTORS: AtomicUsize = AtomicUsize::new(0);
    static COPIES: AtomicUsize = AtomicUsize::new(0);

    /// Stand-ins for the engine: the constructor zeroes the Proposal, the copy is shallow, the destructor only counts.
    unsafe extern "system-unwind" fn fake_ctor(p: *mut u8) -> *mut u8 {
        CTORS.fetch_add(1, Ordering::SeqCst);
        unsafe { core::ptr::write_bytes(p, 0, PROPOSAL_SIZE) };
        p
    }
    /// Like the engine's constructor in the one respect the real commands show: the hash tables start with load factor 1.0.
    unsafe extern "system-unwind" fn fake_ctor_with_load_factors(p: *mut u8) -> *mut u8 {
        unsafe {
            fake_ctor(p);
            for off in [0xd0usize, 0x1e8, 0x270] {
                core::ptr::copy_nonoverlapping(1.0f32.to_le_bytes().as_ptr(), p.add(off), 4);
            }
        }
        p
    }
    unsafe extern "system-unwind" fn fake_dtor(_p: *mut u8) {
        DTORS.fetch_add(1, Ordering::SeqCst);
    }
    thread_local! {
        /// The arena a fake source address lives in (its blocks are not real memory).
        static WORLD: std::cell::Cell<*const Arena> = const { std::cell::Cell::new(core::ptr::null()) };
    }
    unsafe extern "system-unwind" fn fake_copy(d: *mut u8, s: *const u8) -> *mut u8 {
        COPIES.fetch_add(1, Ordering::SeqCst);
        let world = WORLD.with(|w| w.get());
        // SAFETY: the pointer is set by the test to an arena that outlives the call.
        let fake = (!world.is_null()).then(|| unsafe { (*world).read(s as usize, PROPOSAL_SIZE) }).flatten();
        match fake {
            Some(bytes) => unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), d, PROPOSAL_SIZE) },
            None => unsafe { core::ptr::copy_nonoverlapping(s, d, PROPOSAL_SIZE) },
        }
        d
    }

    /// The synthetic world (fake addresses, in an arena) plus the real memory of the scratch buffers.
    struct Both<'a>(&'a Arena);
    impl Mem for Both<'_> {
        fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
            self.0.read(addr, len).or_else(|| crate::dump::LiveMem.read(addr, len))
        }
    }

    /// As [`Both`], and the arena's blocks count as heap blocks (the untyped graph's fallback for blocks no container sizes).
    struct BothHeap<'a>(&'a Arena);
    impl Mem for BothHeap<'_> {
        fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
            self.0.read(addr, len).or_else(|| crate::dump::LiveMem.read(addr, len))
        }
        fn heap_size(&self, addr: usize) -> Option<usize> {
            self.0.heap_size(addr)
        }
    }

    fn settings() -> Settings {
        Settings::from_lookup(|_| Some("1".into())).unwrap()
    }

    fn world() -> (Arena, usize) {
        let (fake, at) = synthetic();
        let a = Arena::new(0x3000_0000_0000);
        for (addr, data) in fake.0 {
            a.put(addr, data);
        }
        (a, at)
    }

    #[test]
    fn the_three_steps_run_in_order_and_the_copies_equal_the_held_command() {
        let (arena, at) = world();
        WORLD.with(|w| w.set(&arena));
        WORLD.with(|w| w.set(&arena));
        let mem = Both(&arena);
        let (c0, d0, k0) = (CTORS.load(Ordering::SeqCst), DTORS.load(Ordering::SeqCst), COPIES.load(Ordering::SeqCst));
        let ctx = Ctx {
            fns: ProposalFns { ctor: fake_ctor, dtor: fake_dtor, copy: fake_copy, assign: fake_copy },
            alloc: &arena,
            mem: &mem,
            base: BASE,
            set: settings(),
            build_name: "40420",
        };
        let mut said: Vec<String> = Vec::new();
        let text = run(&ctx, at, &mut |l| said.push(l.to_string()));
        WORLD.with(|w| w.set(core::ptr::null()));
        assert!(text.contains("STEP 1: OK"), "{text}");
        assert!(text.contains("STEP 2: MATCH"), "{text}");
        // the synthetic construction holds a heap pointer the text cannot resolve: step 3 is refused, not tried
        assert!(text.contains("STEP 3: SKIPPED, 1 heap pointers"), "{text}");
        assert_eq!(said.len(), text.lines().count(), "every line was reported as it came");
        // the engine calls: step 1 construct/destroy, step 2 copy/destroy, step 2b construct/assign/destroy (step 3 was refused first)
        assert_eq!(CTORS.load(Ordering::SeqCst) - c0, 2);
        assert_eq!(COPIES.load(Ordering::SeqCst) - k0, 2);
        assert_eq!(DTORS.load(Ordering::SeqCst) - d0, 3);
        let ctor_line = said.iter().position(|l| l.contains("calling the engine constructor")).unwrap();
        let step1_ok = said.iter().position(|l| l.starts_with("STEP 1: OK")).unwrap();
        assert!(ctor_line < step1_ok, "the call is announced before it is made");
    }

    #[test]
    fn step_three_rebuilds_a_source_from_the_text_when_no_pointer_is_unresolved() {
        // a proposal without the construction's heap pointer: zero that word in the synthetic world
        let (arena, at) = world();
        WORLD.with(|w| w.set(&arena));
        let mem = Both(&arena);
        // the pointer word sits in the construction element, at 0x108 of it (see portable::testdata::synthetic)
        let begin = u64::from_le_bytes(arena.read(at + 0x258, 8).unwrap().try_into().unwrap()) as usize;
        arena.write(begin + 0x108, &[0u8; 8]);
        let (c0, d0, k0) = (CTORS.load(Ordering::SeqCst), DTORS.load(Ordering::SeqCst), COPIES.load(Ordering::SeqCst));
        let ctx = Ctx {
            fns: ProposalFns { ctor: fake_ctor, dtor: fake_dtor, copy: fake_copy, assign: fake_copy },
            alloc: &arena,
            mem: &mem,
            base: BASE,
            set: settings(),
            build_name: "40420",
        };
        let text = run(&ctx, at, &mut |_| {});
        WORLD.with(|w| w.set(core::ptr::null()));
        assert!(text.contains("STEP 3: MATCH"), "{text}");
        assert!(text.contains("source filled from the text"), "{text}");
        // steps 1 and 2: one constructor and one copy each; step 3 adds a constructor, a copy and two destructors
        assert_eq!(CTORS.load(Ordering::SeqCst) - c0, 3);
        assert_eq!(COPIES.load(Ordering::SeqCst) - k0, 3);
        assert_eq!(DTORS.load(Ordering::SeqCst) - d0, 5);
    }

    #[test]
    fn an_unreadable_or_foreign_payload_is_refused_before_anything_is_called() {
        let arena = Arena::new(0x3000_0000_0000);
        let mem = Both(&arena);
        let c0 = CTORS.load(Ordering::SeqCst);
        let ctx = Ctx {
            fns: ProposalFns { ctor: fake_ctor, dtor: fake_dtor, copy: fake_copy, assign: fake_copy },
            alloc: &arena,
            mem: &mem,
            base: BASE,
            set: settings(),
            build_name: "40420",
        };
        let text = run(&ctx, 0x5555_0000, &mut |_| {});
        assert!(text.starts_with("REFUSED"), "{text}");
        assert_eq!(CTORS.load(Ordering::SeqCst), c0);
    }

    #[test]
    fn the_real_commands_of_the_spike_run_go_through_the_steps_with_stand_in_engine_functions() {
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
            let (fake, at) = crate::portable::testdata::model_of(&format!("=== dump #{part}"));
            let arena = Arena::new(0x3000_0000_0000);
            for (addr, data) in fake.0 {
                arena.put(addr, data);
            }
            WORLD.with(|w| w.set(&arena));
            let mem = Both(&arena);
            let ctx = Ctx {
                fns: ProposalFns {
                    ctor: fake_ctor_with_load_factors,
                    dtor: fake_dtor,
                    copy: fake_copy,
                    assign: fake_copy,
                },
                alloc: &arena,
                mem: &mem,
                base: 0x7ff7_1400_0000,
                set: Settings { typed: true, ..settings() },
                build_name: "40420",
            };
            let text = run(&ctx, at, &mut |_| {});
            WORLD.with(|w| w.set(core::ptr::null()));
            assert!(text.contains("STEP 2: MATCH"), "dump #{n}: {text}");
            // the node configurations' residue words (b45/b46: ASCII text or pointer-like junk, never owned) are not carried in typed mode
            // (the injection's), so the stop pair and the stop bulldozing, which stayed SKIPPED on them, rebuild from their text as well
            assert!(!text.contains("STEP 3: DIFF") && !text.contains("nodeConfigsToAdd x"), "dump #{n}: {text}");
            if [1, 13, 19].contains(&n) {
                assert!(text.contains("STEP 3: MATCH"), "dump #{n}: {text}");
            }
            let _ = std::fmt::Write::write_fmt(
                &mut all,
                format_args!(
                    "== dump #{n}
{text}
"
                ),
            );
        }
        if let Some(out) = std::env::var_os("FL3_REBUILD_TEST_OUT") {
            let _ = std::fs::write(std::path::Path::new(&out).join("native_copy_report.txt"), all);
        }
    }

    #[test]
    fn engine_containers_with_entries_are_rebuilt_from_the_text_and_the_engine_copy_equals_the_held_command() {
        let (arena, at) = world();
        crate::rebuild::testgraph::add_containers(&arena, at);
        WORLD.with(|w| w.set(&arena));
        let mem = BothHeap(&arena);
        let (c0, d0, k0) = (CTORS.load(Ordering::SeqCst), DTORS.load(Ordering::SeqCst), COPIES.load(Ordering::SeqCst));
        let ctx = Ctx {
            fns: ProposalFns { ctor: fake_ctor, dtor: fake_dtor, copy: fake_copy, assign: fake_copy },
            alloc: &arena,
            mem: &mem,
            base: BASE,
            set: settings(),
            build_name: "40420",
        };
        let text = run(&ctx, at, &mut |_| {});
        WORLD.with(|w| w.set(core::ptr::null()));
        assert!(text.contains("STEP 2: MATCH"), "{text}");
        assert!(text.contains("STEP 3: MATCH"), "{text}");
        assert!(text.contains("heap blocks: 7 in the held command, 7 in the copy"), "{text}");
        assert_eq!(CTORS.load(Ordering::SeqCst) - c0, 3);
        assert_eq!(COPIES.load(Ordering::SeqCst) - k0, 3);
        assert_eq!(DTORS.load(Ordering::SeqCst) - d0, 5);
    }

    #[cfg(windows)]
    #[test]
    fn real_memory_with_a_real_heap_graph_is_written_from_the_container_alone_and_a_value_that_is_not_a_block_is_never_followed(
    ) {
        // a payload in plain Rust memory: an unordered_map member whose list head and bucket array are real heap blocks, and
        // two more words that look like pointers but are not blocks (the middle of a block, an address on the stack)
        let mut head = Box::new([0u8; 0x18]);
        let buckets = Box::new([0u8; 0x80]);
        let middle = head.as_ptr() as usize + 8;
        let on_stack = 0u64;
        let mut s = Scratch::new();
        let b = s.bytes_mut();
        b[0x9b8] = portable::KIND_BUILD_PROPOSAL;
        let put = |b: &mut [u8], off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_le_bytes());
        // the list head points to itself; the buckets' end pointer is one past the block
        let (h, bk) = (head.as_ptr() as u64, buckets.as_ptr() as u64);
        head[..8].copy_from_slice(&h.to_le_bytes());
        head[8..16].copy_from_slice(&h.to_le_bytes());
        put(b, 0x270, 1.0f32.to_bits() as u64);
        put(b, 0x278, h);
        put(b, 0x288, bk);
        put(b, 0x290, bk + 0x80);
        put(b, 0x298, bk + 0x80);
        put(b, 0x2a0, 7);
        put(b, 0x2a8, 8);
        // two unnamed words of the stop-pair kind: a pointer into the middle of a block and a stack address
        put(b, 0x2d8, middle as u64);
        put(b, 0x2e0, &on_stack as *const u64 as u64);
        let live = crate::dump::LiveMem;
        let head_ = Header {
            seq: 0,
            event: "t",
            hold_id: None,
            site_rva: None,
            tool: None,
            t_ms: 0,
            wall_ms: 0,
            payload: s.addr(),
        };
        let text =
            portable::encode(&live, Env { image_base: 0x7ff7_1400_0000 }, settings(), "40420", &head_).unwrap().text;
        assert!(text.contains("old2new.graph	raw:B1 "), "{text}");
        assert!(text.contains("@B1	size=24	"), "{text}");
        // the middle of a block and the stack address are not blocks: they stay `P`/words, nothing was asked about them
        assert!(!text.contains("B3"), "{text}");
    }

    #[test]
    fn an_unnamed_region_is_taken_into_the_source_unless_it_has_a_pointer_the_text_cannot_resolve() {
        assert!(raw_is_plain("raw:z4 w:0000000000000030 z1"));
        assert!(raw_is_plain("raw:z24 I+500 z369"));
        assert!(raw_is_plain("raw:b:0102"));
        assert!(!raw_is_plain("raw:z3 P z1"));
        assert!(raw_is_plain("raw:V8[w:0000000000000001]"));
        assert!(raw_is_plain("raw:S3:616263 z4"));
        assert!(raw_is_plain("raw:R0[] B3 B3+10"));
        assert!(!raw_is_plain("str:\"x\""));
    }

    #[cfg(windows)]
    #[test]
    fn the_engine_functions_are_used_only_where_the_expected_bytes_are() {
        let mut image = vec![0u8; 0x80];
        // all three prologues in one buffer at their RVA distances is impossible; check the refusal and the byte tables
        let engine = &crate::build_table::fixture().engine;
        let base = (image.as_mut_ptr() as usize).wrapping_sub(engine.proposal_ctor.unwrap() as usize);
        assert!(ProposalFns::at(base, engine).is_err(), "zeroes are not the constructor");
        assert!(ProposalFns::at(base, &crate::build_table::EngineFns::NONE)
            .err()
            .unwrap()
            .contains("not in the build table"));
        assert_eq!(engine.prologue("proposal_ctor").map(<[u8]>::len), Ok(23));
    }

    /// An engine copy that, like the real one, leaves the unconstructed bytes 0x1a0..0x1c0 (the gap) alone.
    unsafe extern "system-unwind" fn gap_leaving_copy(d: *mut u8, s: *const u8) -> *mut u8 {
        COPIES.fetch_add(1, Ordering::SeqCst);
        let world = WORLD.with(|w| w.get());
        // SAFETY: the pointer is set by the test to an arena that outlives the call.
        let bytes = unsafe { (*world).read(s as usize, PROPOSAL_SIZE) }.expect("the held payload");
        for (k, b) in bytes.iter().enumerate() {
            if !(0x2d8..0x2f8).contains(&k) {
                unsafe { *d.add(k) = *b };
            }
        }
        d
    }

    #[test]
    fn the_residue_mask_comes_from_two_copies_prefilled_differently_and_a_typed_text_carries_only_what_they_agree_on() {
        let (arena, at) = world();
        // a value in the gap the copy does not carry (residue), and a readable pointer-like word in it
        let readable = arena.add(&[7u8; 0x40]);
        arena.write(at + 0x2d8, &0x1111_2222_3333_4444u64.to_le_bytes());
        arena.write(at + 0x2e0, &(readable as u64).to_le_bytes());
        WORLD.with(|w| w.set(&arena));
        let mem = Both(&arena);
        let mut fns = ProposalFns { ctor: fake_ctor, dtor: fake_dtor, copy: gap_leaving_copy, assign: fake_copy };
        fns.dtor = fake_dtor;
        let (d0, k0) = (DTORS.load(Ordering::SeqCst), COPIES.load(Ordering::SeqCst));
        let typed = Settings { typed: true, graph_skip: &["toAdd"], ..settings() };
        let mut said = Vec::new();
        let info =
            residue_mask(&fns, &mem, BASE, typed, "40420", at, &mut |l| said.push(l.to_string())).expect("a mask");
        assert_eq!(
            (COPIES.load(Ordering::SeqCst) - k0, DTORS.load(Ordering::SeqCst) - d0),
            (2, 2),
            "two copies, both destroyed"
        );
        assert!(said[0].contains("twice") && said[1].contains("destructor"), "{said:?}");
        let gap = &info.mask["terrain"];
        assert!(
            gap[..0x20].iter().all(|k| !k),
            "the bytes the copy leaves alone differ between 0x00 and 0xFF: residue"
        );
        assert!(info.residue >= 0x20 && info.carried > 0 && info.regions > 0);
        let head =
            Header { seq: 0, event: "t", hold_id: None, site_rva: None, tool: None, t_ms: 0, wall_ms: 0, payload: at };
        let env = Env { image_base: BASE };
        let masked = portable::encode_with(
            &mem,
            env,
            typed,
            "40420",
            &head,
            portable::EncodeOpts { collect: None, mask: Some(&info.mask) },
        )
        .expect("encodes")
        .text;
        assert!(
            !masked.contains(
                "
terrain	"
            ),
            "residue is zero, and a zero region has no line: {masked}"
        );
        // typed without a mask: the value stays a literal, the pointer-like word is never followed or guessed (a foreign address is zero)
        let unmasked = portable::encode(&mem, env, typed, "40420", &head).expect("encodes").text;
        let line = unmasked.lines().find(|l| l.starts_with("terrain	")).expect("a line");
        assert!(
            line.contains("w:1111222233334444") && !line.contains(" P") && !line.contains('B') && !line.contains('R'),
            "{line}"
        );
        // untyped, as before: the same word is a guess (a range, a block, a P)
        let old =
            portable::encode(&mem, env, Settings { typed: false, ..typed }, "40420", &head).expect("encodes").text;
        let old_line = old.lines().find(|l| l.starts_with("terrain	")).expect("a line");
        assert_ne!(old_line, line, "{old_line}");
        WORLD.with(|w| w.set(core::ptr::null()));
        assert!(info.carried_ranges("").contains("terrain"), "{}", info.carried_ranges(""));
    }
}
