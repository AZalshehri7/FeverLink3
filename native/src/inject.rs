//! `FEVERLINK3_NATIVE_INJECT` (track A, option A of `docs/research/DESIGN_DECISIONS.md`): the engine's own `Proposal`, made natively
//! from the portable text of the originator's held command and put into the receiving game's script command INSTEAD of what
//! `scripting::Convert` would make.
//!
//! The pieces are the ones A1 proved on a road (`native_copy.rs`, steps 2b and 3), used in this order:
//!
//! 1. [`build_verified`]: the text is built into an engine-constructed `Proposal` (the engine's default constructor makes the maps
//!    and hash containers; the vectors, strings and scalars come from the text through `rebuild`), the engine's copy constructor
//!    makes the Proposal that will be given away (a self-contained deep copy that owns its blocks), and that copy is written as
//!    text again and compared with the input: every named member must be equal (the A1 `MATCH` rule). Anything else is refused and
//!    nothing was handed to the game. Refused too: a text with heap pointers the rebuild cannot resolve.
//! 2. [`check_destination`]: the command the mod's script sent (a placeholder: kind `build_proposal`, no construction in it).
//! 3. [`swap_into`]: the engine's copy assignment (`0x4ced30`) from the verified Proposal into the command's own Proposal (the engine
//!    destroys what the placeholder held and allocates the new containers itself), then the verified Proposal is destroyed.
//!
//! Only the `Proposal` (the first 0x358 bytes of the payload) is replaced; the Context the mod gave the command stays (the mod ships and
//! sets the originator's Context flags itself). The move variants of the engine's functions are never used on a held command.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::cmd_patch::Poke;
use crate::dump::{pointer_like, u64_at, Mem, PAYLOAD_SIZE};
use crate::link::Event;
use crate::native_copy::{self as nc, Ctx, Scratch, PROPOSAL_SIZE};
use crate::portable::{self, Ty, F};
use crate::rebuild;

pub const INJECT_VAR: &str = "FEVERLINK3_NATIVE_INJECT";

/// The name in the `ready` event's `hooks` list when the order can be carried out.
pub const READY_NAME: &str = "inject_next";

/// How long an order waits for its command when it names no time.
pub const DEFAULT_TTL_MS: u64 = 10_000;
pub const MAX_TTL_MS: u64 = 60_000;

/// The text of a held command, written by the originator's module (`FEVERLINK3_NATIVE_INJECT=1`), for the mod to ship.
pub fn text_file_name(hold_id: u64) -> String {
    format!("native_text_{hold_id}.txt")
}

/// Separates the short reason of a refusal from the full report (`native_inject_report_<key>.txt`) in the error `build_verified` returns.
pub const REPORT_MARK: &str = "\u{1}REPORT\u{1}";

/// `native_inject_report_<key>.txt`: everything a refused injection knows (every unresolved pointer, every differing line).
pub fn report_file_name(key: u64) -> String {
    format!("native_inject_report_{key}.txt")
}

/// The text a receiver's mod writes before `inject_next` (atomically: a temporary name, then a rename).
pub fn inject_file_name(key: u64) -> String {
    format!("native_inject_{key}.txt")
}

/// The largest side file read.
pub const MAX_TEXT_BYTES: u64 = 64 << 20;

/// One armed order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub key: u64,
    pub armed_ms: u64,
    pub ttl_ms: u64,
}

/// The switch, the armed order, the events for the mod.
#[derive(Default)]
pub struct State {
    on: AtomicBool,
    pending: AtomicBool,
    order: Mutex<Option<Order>>,
    applies: Mutex<crate::cmd_patch::ApplyTrail>,
    events: Mutex<Vec<Event>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl State {
    pub fn enable(&self) {
        self.on.store(true, Ordering::Release);
    }

    pub fn enabled(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    /// An order is waiting (a cheap test for the hot path of the command list).
    pub fn pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    pub fn report(&self, e: Event) {
        lock(&self.events).push(e);
    }

    pub fn refuse(&self, reason: impl Into<String>) {
        self.report(Event::InjectRefused { reason: reason.into() });
    }

    /// The mod's `inject_next`. A second order replaces the first (reported as refused).
    pub fn arm(&self, key: u64, ttl: u64, now: u64) {
        if !self.enabled() {
            self.refuse(format!("{INJECT_VAR} is off"));
            return;
        }
        let ttl_ms = if ttl == 0 { DEFAULT_TTL_MS } else { ttl.min(MAX_TTL_MS) };
        *lock(&self.applies) = Default::default();
        if let Some(old) = lock(&self.order).replace(Order { key, armed_ms: now, ttl_ms }) {
            self.refuse(format!("order for key {} replaced by a newer inject_next before a command used it", old.key));
        }
        self.pending.store(true, Ordering::Release);
    }

    /// A `build_proposal` was applied by the simulation while an order is armed.
    pub fn note_apply(&self, held: bool, now: u64) {
        let armed = lock(&self.order).as_ref().map(|o| o.armed_ms);
        if let Some(armed) = armed {
            lock(&self.applies).note(held, now as i64 - armed as i64);
        }
    }

    fn expired(&self, key: u64, ttl_ms: u64) -> String {
        format!(
            "key {key}: expired, no script build_proposal arrived within {ttl_ms} ms; {}",
            lock(&self.applies).text()
        )
    }

    /// The armed order, once; an expired one is reported and dropped.
    pub fn take(&self, now: u64) -> Option<Order> {
        let order = lock(&self.order).take();
        self.pending.store(false, Ordering::Release);
        let order = order?;
        if now.saturating_sub(order.armed_ms) > order.ttl_ms {
            self.refuse(self.expired(order.key, order.ttl_ms));
            return None;
        }
        Some(order)
    }

    /// Expires an order nobody used (the service thread calls this) and hands out the events.
    pub fn drain_events(&self, now: u64) -> Vec<Event> {
        if self.pending() {
            let mut g = lock(&self.order);
            if g.as_ref().is_some_and(|o| now.saturating_sub(o.armed_ms) > o.ttl_ms) {
                let o = g.take().expect("checked");
                self.pending.store(false, Ordering::Release);
                drop(g);
                self.refuse(self.expired(o.key, o.ttl_ms));
            }
        }
        std::mem::take(&mut *lock(&self.events))
    }
}

/// A verified engine `Proposal`, ready to be given away. Owns an engine object: [`release`] it.
pub struct Built {
    pub proposal: Box<Scratch>,
    pub nodes: u64,
    pub segments: u64,
    pub constructions: u64,
    /// Heap blocks the rebuild made for the source (the source's destructor freed them).
    pub blocks: usize,
}

fn count(lines: &std::collections::BTreeMap<&str, &str>, name: &str) -> u64 {
    lines.get(name).and_then(|v| v.strip_prefix("n:")).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// How many constructions the text's proposal adds (`toAdd`).
pub fn construction_count(text: &str) -> u64 {
    count(&rebuild::lines_of(text), "toAdd.#")
}

const TO_ADD: usize = 0x258;
const CONSTRUCTION_STRIDE: usize = 0xe48;
const CONSTRUCTION_FROZEN: usize = 0xcd8;

/// The `frozenNodes` values of construction `i` in the text (`toAdd[i].frozenNodes[k]`, printed as `ent:old#N`: the value is the index).
fn frozen_of(lines: &std::collections::BTreeMap<&str, &str>, i: u64) -> Result<Vec<u32>, String> {
    let n = count(lines, &format!("toAdd[{i}].frozenNodes.#"));
    (0..n)
        .map(|k| {
            let v =
                lines.get(format!("toAdd[{i}].frozenNodes[{k}]").as_str()).ok_or("a frozenNodes value is missing")?;
            let digits = v.rsplit('#').next().unwrap_or("").split('@').next().unwrap_or("");
            digits.parse::<u32>().map_err(|_| format!("toAdd[{i}].frozenNodes[{k}] is {v:?}, not an index"))
        })
        .collect()
}

/// R4: the unnamed words of `toAdd[i]` that the receiver's own conversion carries (its differential mask on the donor) against the
/// originator's text (masked the same way before it was written). Equal, or the injection is refused with the lines that differ.
fn entity_equivalence(c: &Ctx, text: &str, donor: usize, say: &mut dyn FnMut(&str)) -> Result<(), String> {
    let info = nc::residue_mask(&c.fns, c.mem, c.base, c.set, c.build_name, donor, say)?;
    let head = portable::Header {
        seq: 0,
        event: "donor",
        hold_id: None,
        site_rva: None,
        tool: None,
        t_ms: 0,
        wall_ms: 0,
        payload: donor,
    };
    let set = portable::Settings { graph: false, ..c.set };
    let opts = portable::EncodeOpts { collect: None, mask: Some(&info.mask) };
    let donor_text =
        portable::encode_with(c.mem, crate::dump::Env { image_base: c.base }, set, c.build_name, &head, opts)?.text;
    let (theirs, mine) = (rebuild::lines_of(text), rebuild::lines_of(&donor_text));
    let entity = |p: &&str| p.starts_with("toAdd[") && p.contains(".~");
    let mut paths: Vec<&str> = theirs.keys().chain(mine.keys()).copied().filter(entity).collect();
    paths.sort_unstable();
    paths.dedup();
    let mut diffs = Vec::new();
    for p in paths {
        let (a, b) = (theirs.get(p).copied().unwrap_or("(absent)"), mine.get(p).copied().unwrap_or("(absent)"));
        if a != b {
            let (ta, tb): (Vec<&str>, Vec<&str>) = (a.split(' ').collect(), b.split(' ').collect());
            let first = (0..ta.len().max(tb.len())).find(|&i| ta.get(i) != tb.get(i)).unwrap_or(0);
            diffs.push(format!(
                "{p}: first difference at token {first}: originator {:?}, receiver's donor {:?}
    originator: {}
    donor:      {}",
                ta.get(first).unwrap_or(&"-"),
                tb.get(first).unwrap_or(&"-"),
                a.chars().take(400).collect::<String>(),
                b.chars().take(400).collect::<String>()
            ));
        }
    }
    say(&format!(
        "native inject: R4 construction equivalence: donor residue mask {} carried / {} residue bytes in {} regions ({} ms); {} differing region(s)",
        info.carried,
        info.residue,
        info.regions,
        info.ms,
        diffs.len()
    ));
    if diffs.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the receiver's own conversion of the construction differs from the originator's in {} unnamed region(s) both sides carry: {}{REPORT_MARK}{} region(s):
{}
",
        diffs.len(),
        diffs.iter().map(|d| d.split(':').next().unwrap_or("").to_string()).collect::<Vec<_>>().join(", "),
        diffs.len(),
        diffs.join("
")
    ))
}

/// Builds the engine `Proposal` of `text` and proves it equal to the text (see the module documentation). `say` gets one line per
/// stage, BEFORE the engine function it announces is called.
///
/// A text that adds constructions needs `donor`, the payload of the script command that carries the receiver's OWN conversion of the same
/// constructions (same file, parameters, transform): the construction's description is tens of thousands of heap blocks that are not
/// rebuilt from text. The street part (nodes, segments, node configurations, maps, `frozenNodes`) comes from the text; the engine's
/// copy constructor makes a copy of the donor's Proposal (read only), its `toAdd` is moved into the engine-made source, and the engine's
/// copy of that is what is returned; the constructions' `frozenNodes` are then set from the text. The proof compares every named
/// member, the constructions' too: a conversion that differs from the originator's in a named member is refused.
pub fn build_verified(
    c: &Ctx,
    poke: &dyn Poke,
    text: &str,
    donor: Option<usize>,
    say: &mut dyn FnMut(&str),
) -> Result<Built, String> {
    let lines = rebuild::lines_of(text);
    let cons = count(&lines, "toAdd.#");
    if cons > 0 && donor.is_none() {
        return Err(format!(
            "the text adds {cons} construction(s): it needs the script command's own conversion of them"
        ));
    }
    let hybrid = cons > 0;
    if let Some(donor) = donor.filter(|_| hybrid) {
        // R4: the construction stays the receiver's own, so what it carries must be what the originator's carried. Both sides' unnamed
        // words of the constructions are masked by a differential copy (residue is zero), and the masked lines must be equal.
        entity_equivalence(c, text, donor, say)?;
    }
    let take = |f: &F| !(hybrid && f.name == "toAdd") && nc::selected(f, &lines);
    let built = match rebuild::build_selected(text, c.base, c.alloc, Some(&take)) {
        Ok(b) => b,
        Err(e) => {
            c.alloc.free_all();
            return Err(format!("the text cannot be built: {e}"));
        }
    };
    let unresolved: Vec<&rebuild::Blocked> = built
        .blocked
        .iter()
        .filter(|b| b.kind == "heap pointer" && !(hybrid && rebuild::member_of(&b.path) == "toAdd"))
        .collect();
    if !unresolved.is_empty() {
        let n = unresolved.len();
        let all: Vec<String> = unresolved.iter().map(|b| b.path.clone()).collect();
        c.alloc.free_all();
        return Err(format!(
            "{n} heap pointers in the text cannot be resolved: {}{}; copying a null where the engine expects an object could crash the game{REPORT_MARK}{n} unresolved pointer(s), each at <member or @B block>+<byte offset in it>:\n{}\n",
            all.iter().take(20).cloned().collect::<Vec<_>>().join(", "),
            if n > 20 { format!(" and {} more", n - 20) } else { String::new() },
            all.join("\n")
        ));
    }
    // the engine-made source: its containers are the engine's, the rest comes from the text
    let mut p = Scratch::new();
    say("inject: calling the engine constructor for the source");
    // SAFETY: a zeroed 64-aligned buffer larger than a Proposal; the signature is the verified function's.
    unsafe { (c.fns.ctor)(p.ptr()) };
    for f in portable::PAYLOAD.iter().filter(|f| take(f) && f.off < PROPOSAL_SIZE) {
        let (o, n) = (f.off, f.ty.size());
        p.bytes_mut()[o..o + n].copy_from_slice(&built.payload[o..o + n]);
    }
    c.alloc.forget_all(); // the blocks belong to the source object now; its destructor frees them
    if let Some(donor) = donor.filter(|_| hybrid) {
        if p.bytes()[TO_ADD..TO_ADD + 24].iter().any(|&b| b != 0) {
            // SAFETY: constructed above, destroyed once.
            unsafe { (c.fns.dtor)(p.ptr()) };
            return Err("the engine-made source already holds constructions".into());
        }
        let mut z = Scratch::new();
        say("inject: calling the engine copy constructor on the script command's own Proposal (its conversion of the constructions)");
        // SAFETY: `donor` is the live command's payload (only read by a copy constructor); `z` is scratch.
        unsafe { (c.fns.copy)(z.ptr(), donor as *const u8) };
        // the constructions move into the source; the donor's copy keeps an empty list and is destroyed
        let (zb, pb) = (z.bytes_mut(), p.bytes_mut());
        pb[TO_ADD..TO_ADD + 24].copy_from_slice(&zb[TO_ADD..TO_ADD + 24]);
        zb[TO_ADD..TO_ADD + 24].fill(0);
        say("inject: calling the engine destructor on the donor's copy");
        // SAFETY: constructed above, destroyed once; its constructions now belong to `p`.
        unsafe { (c.fns.dtor)(z.ptr()) };
    }
    let mut d = Scratch::new();
    say("inject: calling the engine copy constructor on the source");
    // SAFETY: `p` holds a constructed Proposal (only read by the copy constructor); `d` is scratch.
    unsafe { (c.fns.copy)(d.ptr(), p.addr() as *const u8) };
    d.bytes_mut()[PROPOSAL_SIZE..].copy_from_slice(&built.payload[PROPOSAL_SIZE..]);
    d.bytes_mut()[0x9b8] = portable::KIND_BUILD_PROPOSAL;
    say("inject: calling the engine destructor on the source");
    // SAFETY: constructed above, destroyed once; its builder blocks (and the moved constructions) are freed with it.
    unsafe { (c.fns.dtor)(p.ptr()) };
    let fail = |d: &mut Scratch, why: String| {
        // SAFETY: constructed above, destroyed once.
        unsafe { (c.fns.dtor)(d.ptr()) };
        Err(why)
    };
    if hybrid {
        if let Err(why) = set_construction_frozen(c, poke, d.bytes(), &lines, cons) {
            c.alloc.free_all();
            return fail(&mut d, why);
        }
        c.alloc.forget_all(); // the constructions own the lists now
    }
    // the proof: the copy, written as text, equals the input in every named member
    let again = match nc::encode_scratch(c, &d, "inject") {
        Ok(t) => t,
        Err(e) => return fail(&mut d, format!("the engine's Proposal cannot be written as text to be checked: {e}")),
    };
    let (named_diff, detail) = nc::describe_split(text, &again);
    if named_diff != 0 {
        let all = nc::diff_all(text, &again);
        return fail(
            &mut d,
            format!(
                "the engine's Proposal differs from the text in {named_diff} named line(s): {}{REPORT_MARK}{} differing line(s) between the originator's text and the text of the engine-built Proposal:\n{}\n",
                detail.join(" | "),
                all.len(),
                all.join("\n")
            ),
        );
    }
    Ok(Built {
        proposal: d,
        nodes: count(&lines, "proposal.addedNodes.#"),
        segments: count(&lines, "proposal.addedSegments.#"),
        constructions: cons,
        blocks: built.blocks,
    })
}

/// Writes each construction's `frozenNodes` (a vector of u32 indices into `addedNodes`) as the text says: a new block of the game's
/// allocator, which the engine frees with its own `operator delete` when the Proposal dies; the construction's old block, if any, is
/// left alone (a few bytes, as in `cmd_patch`).
fn set_construction_frozen(
    c: &Ctx,
    poke: &dyn Poke,
    proposal: &[u8],
    lines: &std::collections::BTreeMap<&str, &str>,
    cons: u64,
) -> Result<(), String> {
    let (b, e) = (u64_at(proposal, TO_ADD) as usize, u64_at(proposal, TO_ADD + 8) as usize);
    if !pointer_like(b as u64) || e < b || (e - b) / CONSTRUCTION_STRIDE != cons as usize {
        return Err(format!(
            "the conversion in the script command has {} construction(s), the text has {cons}",
            if e >= b { (e - b) / CONSTRUCTION_STRIDE } else { 0 }
        ));
    }
    for i in 0..cons {
        let want = frozen_of(lines, i)?;
        let triple = b + i as usize * CONSTRUCTION_STRIDE + CONSTRUCTION_FROZEN;
        let t = c.mem.read(triple, 24).ok_or_else(|| format!("toAdd[{i}].frozenNodes is not readable"))?;
        let (tb, te, tc) = (u64_at(&t, 0), u64_at(&t, 8), u64_at(&t, 16));
        let ok = (tb == 0 && te == 0 && tc == 0)
            || (pointer_like(tb) && tb % 4 == 0 && tb <= te && te <= tc && (te - tb) % 4 == 0);
        if !ok {
            return Err(format!("toAdd[{i}].frozenNodes is not a vector of 4-byte values"));
        }
        let (nb, ne) = if want.is_empty() {
            (0u64, 0u64)
        } else {
            let at = c.alloc.alloc(4 * want.len()).ok_or("the game's allocator gave no block")?;
            let data: Vec<u8> = want.iter().flat_map(|v| v.to_le_bytes()).collect();
            if !c.alloc.write(at, &data) {
                return Err("a frozenNodes block could not be written".into());
            }
            (at as u64, (at + 4 * want.len()) as u64)
        };
        let mut out = Vec::with_capacity(24);
        for v in [nb, ne, ne] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        if !poke.poke(triple, &out) {
            return Err(format!("toAdd[{i}].frozenNodes could not be written"));
        }
    }
    Ok(())
}

/// Destroys a verified Proposal that was not (or no longer needs to be) handed over.
pub fn release(c: &Ctx, mut b: Built) {
    // SAFETY: `build_verified` returned a constructed Proposal; it is destroyed once, here.
    unsafe { (c.fns.dtor)(b.proposal.ptr()) };
}

fn sane(t: (u64, u64, u64), stride: usize) -> bool {
    let (b, e, c) = t;
    (b == 0 && e == 0 && c == 0) || (pointer_like(b) && b <= e && e <= c && ((e - b) as usize).is_multiple_of(stride))
}

/// The command the script sent must be a `build_proposal` with sane vectors, and carry exactly `constructions` constructions: none for a
/// text without (a placeholder), the receiver's own conversion of them for a text with.
pub fn check_destination(mem: &dyn Mem, dest: usize, constructions: u64) -> Result<(), String> {
    let bytes = mem.read(dest, PAYLOAD_SIZE).ok_or("the command's payload is not readable")?;
    if bytes[0x9b8] != portable::KIND_BUILD_PROPOSAL {
        return Err(format!("the command is kind {}, not build_proposal", bytes[0x9b8]));
    }
    let tri = |off: usize| (u64_at(&bytes, off), u64_at(&bytes, off + 8), u64_at(&bytes, off + 16));
    let to_add = tri(TO_ADD);
    if !sane(tri(0x000), 0x40) || !sane(tri(0x018), 0x350) || !sane(to_add, CONSTRUCTION_STRIDE) {
        return Err("the command's proposal has a vector that is not one".into());
    }
    let have = (to_add.1 - to_add.0) / CONSTRUCTION_STRIDE as u64;
    match (constructions, have) {
        (0, 0) => Ok(()),
        (0, _) => Err("the command already adds a construction: it is not a placeholder".into()),
        (want, have) if want == have => Ok(()),
        (want, have) => Err(format!("the command has {have} construction(s), the text has {want}")),
    }
}

/// Puts the verified Proposal into the command's own: the engine's copy assignment.
///
/// # Safety
/// `dest` is the payload of a live command whose first 0x358 bytes are a constructed `Proposal` ([`check_destination`] passed), on the
/// thread that queues it.
pub unsafe fn swap_into(c: &Ctx, b: &Built, dest: usize) {
    // SAFETY: as the caller guarantees; the source is only read.
    unsafe { (c.fns.assign)(dest as *mut u8, b.proposal.addr() as *const u8) };
}

/// True for the members `build_verified` would not rebuild; kept next to the rule it mirrors.
pub fn is_container(f: &F) -> bool {
    matches!(f.ty, Ty::Map | Ty::Hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dump::Env;
    use crate::native_copy::{CopyFn, CtorFn, DtorFn, ProposalFns};
    use crate::portable::testdata::{synthetic, BASE};
    use crate::rebuild::{Alloc, Arena};
    use std::sync::atomic::AtomicUsize;

    static CTORS: AtomicUsize = AtomicUsize::new(0);
    static DTORS: AtomicUsize = AtomicUsize::new(0);
    static COPIES: AtomicUsize = AtomicUsize::new(0);
    static ASSIGNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "system-unwind" fn ctor(p: *mut u8) -> *mut u8 {
        CTORS.fetch_add(1, Ordering::SeqCst);
        unsafe { core::ptr::write_bytes(p, 0, PROPOSAL_SIZE) };
        p
    }
    unsafe extern "system-unwind" fn dtor(_p: *mut u8) {
        DTORS.fetch_add(1, Ordering::SeqCst);
    }
    thread_local! {
        /// The arena a fake donor address lives in (its blocks are not real memory).
        static WORLD: std::cell::Cell<*const Arena> = const { std::cell::Cell::new(core::ptr::null()) };
    }
    unsafe extern "system-unwind" fn copy(d: *mut u8, s: *const u8) -> *mut u8 {
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

    /// Writes into the test arena (the construction's list lives there).
    struct ArenaPoke<'a>(&'a Arena);
    impl Poke for ArenaPoke<'_> {
        fn poke(&self, addr: usize, data: &[u8]) -> bool {
            self.0.write(addr, data)
        }
    }
    unsafe extern "system-unwind" fn assign(d: *mut u8, s: *const u8) -> *mut u8 {
        ASSIGNS.fetch_add(1, Ordering::SeqCst);
        unsafe { core::ptr::copy_nonoverlapping(s, d, PROPOSAL_SIZE) };
        d
    }

    struct Both<'a>(&'a Arena);
    impl Mem for Both<'_> {
        fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
            self.0.read(addr, len).or_else(|| crate::dump::LiveMem.read(addr, len))
        }
    }

    fn fns() -> ProposalFns {
        let (a, b, c, d): (CtorFn, DtorFn, CopyFn, CopyFn) = (ctor, dtor, copy, assign);
        ProposalFns { ctor: a, dtor: b, copy: c, assign: d }
    }

    fn settings() -> portable::Settings {
        portable::Settings::from_lookup(|_| Some("1".into())).unwrap()
    }

    /// The synthetic world and its text. `construction`: it keeps its construction, whose frozenNodes are `[1]` in the text and empty in the
    /// world afterwards (the donor's conversion has not got them); else the construction is taken out (a road).
    fn world_text(construction: bool) -> (Arena, usize, String) {
        let (fake, at) = synthetic();
        let a = Arena::new(0x3000_0000_0000);
        for (addr, data) in fake.0 {
            a.put(addr, data);
        }
        let begin = u64::from_le_bytes(a.read(at + 0x258, 8).unwrap().try_into().unwrap()) as usize;
        if !construction {
            a.write(at + 0x258, &[0u8; 24]);
        } else {
            let blk = a.add(&1u32.to_le_bytes());
            let mut t = Vec::new();
            for v in [blk as u64, blk as u64 + 4, blk as u64 + 4] {
                t.extend_from_slice(&v.to_le_bytes());
            }
            a.write(begin + 0xcd8, &t);
        }
        let head = portable::Header {
            seq: 0,
            event: "held",
            hold_id: None,
            site_rva: None,
            tool: None,
            t_ms: 0,
            wall_ms: 0,
            payload: at,
        };
        let text = portable::Header { ..head };
        let text =
            portable::encode(&Both(&a), Env { image_base: BASE }, graph_skip(settings()), "40420", &text).unwrap().text;
        if construction {
            a.write(begin + 0xcd8, &[0u8; 24]); // the donor's own conversion: no frozen nodes
        }
        (a, at, text)
    }

    fn graph_skip(s: portable::Settings) -> portable::Settings {
        portable::Settings { graph_skip: &["toAdd"], ..s }
    }

    fn placeholder(nodes: bool) -> Box<Scratch> {
        let mut s = Scratch::new();
        s.bytes_mut()[0x9b8] = portable::KIND_BUILD_PROPOSAL;
        if nodes {
            // a begin/end/cap triple of one 0x40 element somewhere plausible
            let b = 0x2000_0000_0000u64;
            s.bytes_mut()[0..8].copy_from_slice(&b.to_le_bytes());
            s.bytes_mut()[8..16].copy_from_slice(&(b + 0x40).to_le_bytes());
            s.bytes_mut()[16..24].copy_from_slice(&(b + 0x40).to_le_bytes());
        }
        s
    }

    #[test]
    fn a_road_text_is_built_proved_equal_and_swapped_into_a_placeholder() {
        let (arena, _, text) = world_text(false);
        assert_eq!(construction_count(&text), 0);
        let mem = Both(&arena);
        let ctx = Ctx { fns: fns(), alloc: &arena, mem: &mem, base: BASE, set: settings(), build_name: "40420" };
        let poke = ArenaPoke(&arena);
        let (c0, d0, k0) = (CTORS.load(Ordering::SeqCst), DTORS.load(Ordering::SeqCst), COPIES.load(Ordering::SeqCst));
        let mut said = Vec::new();
        let built = build_verified(&ctx, &poke, &text, None, &mut |l| said.push(l.to_string())).expect("proved equal");
        assert!(
            said[0].contains("constructor") && said[1].contains("copy constructor") && said[2].contains("destructor"),
            "{said:?}"
        );
        assert_eq!(
            (CTORS.load(Ordering::SeqCst) - c0, COPIES.load(Ordering::SeqCst) - k0, DTORS.load(Ordering::SeqCst) - d0),
            (1, 1, 1)
        );
        assert_eq!((built.nodes, built.segments, built.constructions), (2, 1, 0));
        let mut dest = placeholder(false);
        check_destination(&mem, dest.addr(), 0).expect("a placeholder");
        let a0 = ASSIGNS.load(Ordering::SeqCst);
        // SAFETY: the stand-in assignment copies 0x358 bytes between two buffers of this test.
        unsafe { swap_into(&ctx, &built, dest.addr()) };
        assert_eq!(ASSIGNS.load(Ordering::SeqCst) - a0, 1);
        assert_eq!(&dest.bytes()[..PROPOSAL_SIZE], &built.proposal.bytes()[..PROPOSAL_SIZE]);
        assert_eq!(dest.bytes()[0x9b8], portable::KIND_BUILD_PROPOSAL, "the kind byte is not touched");
        release(&ctx, built);
        assert_eq!(DTORS.load(Ordering::SeqCst) - d0, 2);
        let _ = dest.ptr();
    }

    #[test]
    fn a_depot_text_takes_the_street_from_the_text_and_the_construction_from_the_donor_and_sets_its_frozen_nodes() {
        let (arena, at, text) = world_text(true);
        assert_eq!(construction_count(&text), 1);
        assert!(text.contains("toAdd[0].frozenNodes.#\tn:1"), "{text}");
        WORLD.with(|w| w.set(&arena));
        let mem = Both(&arena);
        let ctx = Ctx { fns: fns(), alloc: &arena, mem: &mem, base: BASE, set: settings(), build_name: "40420" };
        let poke = ArenaPoke(&arena);
        let (c0, d0, k0) = (CTORS.load(Ordering::SeqCst), DTORS.load(Ordering::SeqCst), COPIES.load(Ordering::SeqCst));
        let mut said = Vec::new();
        let r = build_verified(&ctx, &poke, &text, Some(at), &mut |l| said.push(l.to_string()));
        WORLD.with(|w| w.set(core::ptr::null()));
        let built = r.expect("the proof passes: every named member, the construction's too");
        assert!(said.iter().any(|l| l.contains("script command's own Proposal")), "{said:?}");
        // source constructor, donor copy, source copy, destructors for the donor copy and the source
        assert_eq!(
            (CTORS.load(Ordering::SeqCst) - c0, COPIES.load(Ordering::SeqCst) - k0, DTORS.load(Ordering::SeqCst) - d0),
            (1, 4, 4) // the donor's two mask copies come first, then the donor copy and the source copy
        );
        assert_eq!((built.nodes, built.segments, built.constructions), (2, 1, 1));
        // the destination must carry the same number of constructions
        let mut dest = placeholder(false);
        assert!(check_destination(&mem, dest.addr(), 1).unwrap_err().contains("0 construction(s), the text has 1"));
        release(&ctx, built);
        let _ = dest.ptr();
    }

    #[test]
    fn a_depot_text_without_a_donor_is_refused_and_a_converted_construction_that_differs_is_not_accepted() {
        let (arena, at, text) = world_text(true);
        let mem = Both(&arena);
        let ctx = Ctx { fns: fns(), alloc: &arena, mem: &mem, base: BASE, set: settings(), build_name: "40420" };
        let poke = ArenaPoke(&arena);
        let c0 = CTORS.load(Ordering::SeqCst);
        let r = build_verified(&ctx, &poke, &text, None, &mut |_| {});
        assert!(matches!(&r, Err(e) if e.contains("needs the script command's own conversion")), "{:?}", r.err());
        assert_eq!(CTORS.load(Ordering::SeqCst), c0, "refused before any engine call");
        // R4: the donor's conversion carries a word the originator's did not (an unnamed word of the construction): refused before the
        // source is made, with the region named; the two mask copies are destroyed
        let begin = u64::from_le_bytes(arena.read(at + 0x258, 8).unwrap().try_into().unwrap()) as usize;
        arena.write(begin + 0xe3d, &[0x5a]);
        WORLD.with(|w| w.set(&arena));
        let (d0, c1) = (DTORS.load(Ordering::SeqCst), CTORS.load(Ordering::SeqCst));
        let r = build_verified(&ctx, &poke, &text, Some(at), &mut |_| {});
        let e = r.err().expect("refused");
        assert!(e.contains("toAdd[0].~0xe3d") && e.contains(REPORT_MARK), "{e}");
        assert_eq!((DTORS.load(Ordering::SeqCst) - d0, CTORS.load(Ordering::SeqCst) - c1), (2, 0));
        // the donor equal again, but a named member of the construction differs: the proof fails
        arena.write(begin + 0xe3d, &[0x00]);
        let other = text.replace("toAdd[0].setAsHeadquarterHack	u8:00", "toAdd[0].setAsHeadquarterHack	u8:01");
        let d0 = DTORS.load(Ordering::SeqCst);
        let r = build_verified(&ctx, &poke, &other, Some(at), &mut |_| {});
        WORLD.with(|w| w.set(core::ptr::null()));
        assert!(matches!(&r, Err(e) if e.contains("differs from the text")), "{:?}", r.err());
        assert_eq!(
            DTORS.load(Ordering::SeqCst) - d0,
            5,
            "the two mask copies, the donor's copy, the source and the failed copy are all destroyed"
        );
    }

    #[test]
    fn a_proposal_that_differs_from_the_text_is_destroyed_and_not_returned() {
        let (arena, _, text) = world_text(false);
        // the engine's "copy" loses a vector: the proof must fail
        unsafe extern "system-unwind" fn lossy(d: *mut u8, s: *const u8) -> *mut u8 {
            unsafe {
                core::ptr::copy_nonoverlapping(s, d, PROPOSAL_SIZE);
                core::ptr::write_bytes(d, 0, 24); // addedNodes emptied
            }
            d
        }
        let mem = Both(&arena);
        let mut f = fns();
        f.copy = lossy;
        let ctx = Ctx { fns: f, alloc: &arena, mem: &mem, base: BASE, set: settings(), build_name: "40420" };
        let (d0, a0) = (DTORS.load(Ordering::SeqCst), ASSIGNS.load(Ordering::SeqCst));
        let r = build_verified(&ctx, &ArenaPoke(&arena), &text, None, &mut |_| {});
        assert!(matches!(&r, Err(e) if e.contains("differs from the text")), "{:?}", r.err());
        assert_eq!(DTORS.load(Ordering::SeqCst) - d0, 2, "the source and the failed copy are both destroyed");
        assert_eq!(ASSIGNS.load(Ordering::SeqCst), a0, "nothing was assigned");
    }

    #[test]
    fn only_a_placeholder_build_proposal_is_a_destination() {
        let arena = Arena::new(0x3000_0000_0000);
        let mem = Both(&arena);
        let ok = placeholder(false);
        assert!(check_destination(&mem, ok.addr(), 0).is_ok());
        let mut other = placeholder(false);
        other.bytes_mut()[0x9b8] = 17;
        assert!(check_destination(&mem, other.addr(), 0).unwrap_err().contains("not build_proposal"));
        let mut cons = placeholder(true);
        // toAdd with one element
        cons.bytes_mut()[0x258..0x260].copy_from_slice(&0x2000_0000_1000u64.to_le_bytes());
        cons.bytes_mut()[0x260..0x268].copy_from_slice(&(0x2000_0000_1000u64 + 0xe48).to_le_bytes());
        cons.bytes_mut()[0x268..0x270].copy_from_slice(&(0x2000_0000_1000u64 + 0xe48).to_le_bytes());
        assert!(check_destination(&mem, cons.addr(), 0).unwrap_err().contains("already adds a construction"));
        let mut bad = placeholder(false);
        bad.bytes_mut()[0..8].copy_from_slice(&5u64.to_le_bytes());
        assert!(check_destination(&mem, bad.addr(), 0).unwrap_err().contains("not one"));
        assert!(check_destination(&mem, 0x10, 0).is_err());
    }

    #[test]
    fn an_order_is_taken_once_expires_and_is_replaced() {
        let s = State::default();
        s.arm(7, 0, 0);
        assert!(!s.pending(), "off: refused");
        assert!(matches!(s.drain_events(0).as_slice(), [Event::InjectRefused { reason }] if reason.contains("is off")));
        s.enable();
        s.arm(7, 0, 100);
        assert!(s.pending());
        assert_eq!(s.take(150), Some(Order { key: 7, armed_ms: 100, ttl_ms: DEFAULT_TTL_MS }));
        assert_eq!((s.take(150), s.pending()), (None, false));
        s.arm(8, 50, 0);
        s.arm(9, 50, 10);
        let ev = s.drain_events(10);
        assert!(
            matches!(ev.as_slice(), [Event::InjectRefused { reason }] if reason.contains("key 8") && reason.contains("replaced")),
            "{ev:?}"
        );
        assert!(s.take(100).is_none());
        let ev = s.drain_events(100);
        assert!(
            matches!(ev.as_slice(), [Event::InjectRefused { reason }] if reason.contains("key 9") && reason.contains("expired")),
            "{ev:?}"
        );
        s.arm(1, 0, 0);
        let ev = s.drain_events(DEFAULT_TTL_MS + 1);
        assert!(matches!(ev.as_slice(), [Event::InjectRefused { reason }] if reason.contains("expired")), "{ev:?}");
    }

    #[test]
    fn a_refusal_lists_every_unresolved_pointer_and_every_differing_line_in_the_report() {
        // a map with entries whose node graph is a bare P: unresolved, at an exact place
        let (arena, _, text) = world_text(false);
        let broken = text.replace(
            "proposal.new2oldNodes	map:size=0",
            "proposal.new2oldNodes	map:size=2
proposal.new2oldNodes.graph	raw:P w:0000000000000002",
        );
        let broken = if broken.contains("proposal.new2oldNodes.graph	raw:P w:0000000000000002") {
            broken
        } else {
            panic!("{text}")
        };
        let mem = Both(&arena);
        let ctx = Ctx { fns: fns(), alloc: &arena, mem: &mem, base: BASE, set: settings(), build_name: "40420" };
        let r = build_verified(&ctx, &ArenaPoke(&arena), &broken, None, &mut |_| {});
        let e = r.err().expect("refused");
        let (short, report) = e.split_once(REPORT_MARK).expect("a report follows the reason");
        assert!(short.contains("proposal.new2oldNodes.graph+0x0"), "{short}");
        assert!(
            report.contains("1 unresolved pointer(s)") && report.contains("proposal.new2oldNodes.graph+0x0"),
            "{report}"
        );
        // a proof that fails reports every differing line, named or not
        unsafe extern "system-unwind" fn lossy(d: *mut u8, s: *const u8) -> *mut u8 {
            unsafe {
                core::ptr::copy_nonoverlapping(s, d, PROPOSAL_SIZE);
                core::ptr::write_bytes(d, 0, 24);
            }
            d
        }
        let mut f = fns();
        f.copy = lossy;
        let (arena, _, text) = world_text(false);
        let mem = Both(&arena);
        let ctx = Ctx { fns: f, alloc: &arena, mem: &mem, base: BASE, set: settings(), build_name: "40420" };
        let e = build_verified(&ctx, &ArenaPoke(&arena), &text, None, &mut |_| {}).err().expect("refused");
        let (_, report) = e.split_once(REPORT_MARK).expect("a report");
        assert!(
            report.contains("NAMED   proposal.addedNodes.#")
                && report.contains("original: n:2")
                && report.contains("rebuilt:  n:0"),
            "{report}"
        );
    }
}
