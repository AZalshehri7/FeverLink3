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
        if let Some(old) = lock(&self.order).replace(Order { key, armed_ms: now, ttl_ms }) {
            self.refuse(format!("order for key {} replaced by a newer inject_next before a command used it", old.key));
        }
        self.pending.store(true, Ordering::Release);
    }

    /// The armed order, once; an expired one is reported and dropped.
    pub fn take(&self, now: u64) -> Option<Order> {
        let order = lock(&self.order).take();
        self.pending.store(false, Ordering::Release);
        let order = order?;
        if now.saturating_sub(order.armed_ms) > order.ttl_ms {
            self.refuse(format!(
                "key {}: expired, no script build_proposal arrived within {} ms",
                order.key, order.ttl_ms
            ));
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
                self.refuse(format!("key {}: expired, no script build_proposal arrived within {} ms", o.key, o.ttl_ms));
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

/// Builds the engine `Proposal` of `text` and proves it equal to the text (see the module documentation). `say` gets one line per
/// stage, BEFORE the engine function it announces is called.
pub fn build_verified(c: &Ctx, text: &str, say: &mut dyn FnMut(&str)) -> Result<Built, String> {
    let lines = rebuild::lines_of(text);
    let select = |f: &F| nc::selected(f, &lines);
    let built = match rebuild::build_selected(text, c.base, c.alloc, Some(&select)) {
        Ok(b) => b,
        Err(e) => {
            c.alloc.free_all();
            return Err(format!("the text cannot be built: {e}"));
        }
    };
    let unresolved: Vec<&rebuild::Blocked> = built.blocked.iter().filter(|b| b.kind == "heap pointer").collect();
    if let Some(first) = unresolved.first() {
        let n = unresolved.len();
        let path = first.path.clone();
        c.alloc.free_all();
        return Err(format!("{n} heap pointers in the text cannot be resolved (first at {path}); copying a null where the engine expects an object could crash the game"));
    }
    // the engine-made source: its containers are the engine's, the rest comes from the text
    let mut p = Scratch::new();
    say("inject: calling the engine constructor for the source");
    // SAFETY: a zeroed 64-aligned buffer larger than a Proposal; the signature is the verified function's.
    unsafe { (c.fns.ctor)(p.ptr()) };
    for f in portable::PAYLOAD.iter().filter(|f| nc::selected(f, &lines) && f.off < PROPOSAL_SIZE) {
        let (o, n) = (f.off, f.ty.size());
        p.bytes_mut()[o..o + n].copy_from_slice(&built.payload[o..o + n]);
    }
    c.alloc.forget_all(); // the blocks belong to the source object now; its destructor frees them
    let mut d = Scratch::new();
    say("inject: calling the engine copy constructor on the source");
    // SAFETY: `p` holds a constructed Proposal (only read by the copy constructor); `d` is scratch.
    unsafe { (c.fns.copy)(d.ptr(), p.addr() as *const u8) };
    d.bytes_mut()[PROPOSAL_SIZE..].copy_from_slice(&built.payload[PROPOSAL_SIZE..]);
    d.bytes_mut()[0x9b8] = portable::KIND_BUILD_PROPOSAL;
    say("inject: calling the engine destructor on the source");
    // SAFETY: constructed above, destroyed once; its builder blocks are freed with it.
    unsafe { (c.fns.dtor)(p.ptr()) };
    // the proof: the copy, written as text, equals the input in every named member
    let again = match nc::encode_scratch(c, &d, "inject") {
        Ok(t) => t,
        Err(e) => {
            // SAFETY: constructed above, destroyed once.
            unsafe { (c.fns.dtor)(d.ptr()) };
            return Err(format!("the engine's Proposal cannot be written as text to be checked: {e}"));
        }
    };
    let (named_diff, detail) = nc::describe_split(text, &again);
    if named_diff != 0 {
        // SAFETY: as above.
        unsafe { (c.fns.dtor)(d.ptr()) };
        return Err(format!(
            "the engine's Proposal differs from the text in {named_diff} named line(s): {}",
            detail.join(" | ")
        ));
    }
    Ok(Built {
        proposal: d,
        nodes: count(&lines, "proposal.addedNodes.#"),
        segments: count(&lines, "proposal.addedSegments.#"),
        constructions: count(&lines, "toAdd.#"),
        blocks: built.blocks,
    })
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

/// The command the script sent must be a `build_proposal` carrying no construction (a placeholder), with sane vectors.
pub fn check_destination(mem: &dyn Mem, dest: usize) -> Result<(), String> {
    let bytes = mem.read(dest, PAYLOAD_SIZE).ok_or("the command's payload is not readable")?;
    if bytes[0x9b8] != portable::KIND_BUILD_PROPOSAL {
        return Err(format!("the command is kind {}, not build_proposal", bytes[0x9b8]));
    }
    let tri = |off: usize| (u64_at(&bytes, off), u64_at(&bytes, off + 8), u64_at(&bytes, off + 16));
    let to_add = tri(0x258);
    if !sane(tri(0x000), 0x40) || !sane(tri(0x018), 0x350) || !sane(to_add, 0xe48) {
        return Err("the command's proposal has a vector that is not one".into());
    }
    if to_add.1 != to_add.0 {
        return Err("the command already adds a construction: it is not a placeholder".into());
    }
    Ok(())
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
    unsafe extern "system-unwind" fn copy(d: *mut u8, s: *const u8) -> *mut u8 {
        COPIES.fetch_add(1, Ordering::SeqCst);
        unsafe { core::ptr::copy_nonoverlapping(s, d, PROPOSAL_SIZE) };
        d
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

    /// The synthetic road-like world, its construction pointer cleared (as in native_copy's step 3 test) and its text.
    fn world_text() -> (Arena, String) {
        let (fake, at) = synthetic();
        let a = Arena::new(0x3000_0000_0000);
        for (addr, data) in fake.0 {
            a.put(addr, data);
        }
        let begin = u64::from_le_bytes(a.read(at + 0x258, 8).unwrap().try_into().unwrap()) as usize;
        a.write(begin + 0x108, &[0u8; 8]);
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
        let text = portable::encode(&Both(&a), Env { image_base: BASE }, settings(), "40420", &head).unwrap().text;
        (a, text)
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
    fn a_text_is_built_proved_equal_and_swapped_into_a_placeholder() {
        let (arena, text) = world_text();
        let mem = Both(&arena);
        let ctx = Ctx { fns: fns(), alloc: &arena, mem: &mem, base: BASE, set: settings(), build_name: "40420" };
        let (c0, d0, k0) = (CTORS.load(Ordering::SeqCst), DTORS.load(Ordering::SeqCst), COPIES.load(Ordering::SeqCst));
        let mut said = Vec::new();
        let built = build_verified(&ctx, &text, &mut |l| said.push(l.to_string())).expect("proved equal");
        assert!(
            said[0].contains("constructor") && said[1].contains("copy constructor") && said[2].contains("destructor"),
            "{said:?}"
        );
        assert_eq!(
            (CTORS.load(Ordering::SeqCst) - c0, COPIES.load(Ordering::SeqCst) - k0, DTORS.load(Ordering::SeqCst) - d0),
            (1, 1, 1)
        );
        assert_eq!((built.nodes, built.segments, built.constructions), (2, 1, 1));
        let mut dest = placeholder(false);
        check_destination(&mem, dest.addr()).expect("a placeholder");
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
    fn a_text_with_a_pointer_the_rebuild_cannot_resolve_is_refused_before_any_engine_call() {
        let (fake, at) = synthetic(); // the construction keeps its heap pointer here
        let a = Arena::new(0x3000_0000_0000);
        for (addr, data) in fake.0 {
            a.put(addr, data);
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
        let mem = Both(&a);
        let text = portable::encode(&mem, Env { image_base: BASE }, settings(), "40420", &head).unwrap().text;
        let ctx = Ctx { fns: fns(), alloc: &a, mem: &mem, base: BASE, set: settings(), build_name: "40420" };
        let c0 = CTORS.load(Ordering::SeqCst);
        let r = build_verified(&ctx, &text, &mut |_| {});
        assert!(matches!(&r, Err(e) if e.contains("heap pointers")), "{:?}", r.err());
        assert_eq!(CTORS.load(Ordering::SeqCst), c0);
    }

    #[test]
    fn a_proposal_that_differs_from_the_text_is_destroyed_and_not_returned() {
        let (arena, text) = world_text();
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
        let r = build_verified(&ctx, &text, &mut |_| {});
        assert!(matches!(&r, Err(e) if e.contains("differs from the text")), "{:?}", r.err());
        assert_eq!(DTORS.load(Ordering::SeqCst) - d0, 2, "the source and the failed copy are both destroyed");
        assert_eq!(ASSIGNS.load(Ordering::SeqCst), a0, "nothing was assigned");
    }

    #[test]
    fn only_a_placeholder_build_proposal_is_a_destination() {
        let arena = Arena::new(0x3000_0000_0000);
        let mem = Both(&arena);
        let ok = placeholder(false);
        assert!(check_destination(&mem, ok.addr()).is_ok());
        let mut other = placeholder(false);
        other.bytes_mut()[0x9b8] = 17;
        assert!(check_destination(&mem, other.addr()).unwrap_err().contains("not build_proposal"));
        let mut cons = placeholder(true);
        // toAdd with one element
        cons.bytes_mut()[0x258..0x260].copy_from_slice(&0x2000_0000_1000u64.to_le_bytes());
        cons.bytes_mut()[0x260..0x268].copy_from_slice(&(0x2000_0000_1000u64 + 0xe48).to_le_bytes());
        cons.bytes_mut()[0x268..0x270].copy_from_slice(&(0x2000_0000_1000u64 + 0xe48).to_le_bytes());
        assert!(check_destination(&mem, cons.addr()).unwrap_err().contains("already adds a construction"));
        let mut bad = placeholder(false);
        bad.bytes_mut()[0..8].copy_from_slice(&5u64.to_le_bytes());
        assert!(check_destination(&mem, bad.addr()).unwrap_err().contains("not one"));
        assert!(check_destination(&mem, 0x10).is_err());
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
}
