//! `FEVERLINK3_NATIVE_PATCH` (track A, A2c): the members of a script-made `build_proposal` command that a `SimpleProposal` cannot carry.
//!
//! The engine converts a `SimpleProposal` itself (`scripting::Convert`, 0x10c6750). The proposal it makes has no `frozenNodes`,
//! so the construction's entrance piece it regenerates does not attach to the tool's connecting node. The mod therefore sends
//! the order `patch_next` (`link.rs`) just before it sends the command; at the command list's `Add` this module writes the
//! indices into the converted command (`proposal.frozenNodes` at 0x1d0 and `toAdd[0].frozenNodes` at 0xcd8: vectors of u32
//! indices into `proposal.addedNodes`, `docs/research/SPIKE_NATIVE_COMMAND.md` "Which segments does the construction own?").
//!
//! Everything is checked first and nothing is written unless all checks pass; every refusal leaves the command untouched and is
//! reported to the mod as `patch_refused` with the reason.
//!
//! Ownership: a list that was empty gets a block of the game's allocator (`operator new`, the engine's scheme, as the rebuild
//! uses); the command's Proposal now owns it and the engine frees it with its own `operator delete` when the command dies. A list
//! with capacity for the indices is written in place; one that is too small gets a new block and the old block is left alone (the
//! engine still frees it only if it is the vector's `begin`, which it no longer is: it leaks, a few bytes, documented).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::dump::{pointer_like, u64_at, Mem, PAYLOAD_SIZE};
use crate::link::{Event, MAX_PATCH_FROZEN};
use crate::portable::KIND_BUILD_PROPOSAL;
use crate::rebuild::Alloc;

pub const PATCH_VAR: &str = "FEVERLINK3_NATIVE_PATCH";

/// The name in the `ready` event's `hooks` list when the order can be carried out.
pub const READY_NAME: &str = "patch_next";

/// How long an order waits for its command when it names no time.
pub const DEFAULT_TTL_MS: u64 = 10_000;
/// The longest an order may wait.
pub const MAX_TTL_MS: u64 = 60_000;

const KIND_OFFSET: usize = 0x9b8;
const ADDED_NODES: usize = 0x000;
const NODE_STRIDE: usize = 0x40;
const PROPOSAL_FROZEN: usize = 0x1d0;
const TO_ADD: usize = 0x258;
const CONSTRUCTION_STRIDE: usize = 0xe48;
const CONSTRUCTION_FROZEN: usize = 0xcd8;

/// Whether `ready` lists [`READY_NAME`]: the switch is on, the layout is this build's, and the allocator's prologues hold (asked last).
pub fn announces(enabled: bool, build: &str, allocator_ok: impl FnOnce() -> bool) -> bool {
    enabled && build == crate::portable::LAYOUT_BUILD && allocator_ok()
}

/// One armed order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub frozen: Vec<u32>,
    pub adds: u32,
    pub armed_ms: u64,
    pub ttl_ms: u64,
}

/// The module's side: whether the switch is on, the armed order, the events for the mod.
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

    fn refuse(&self, reason: impl Into<String>) {
        lock(&self.events).push(Event::PatchRefused { reason: reason.into() });
    }

    /// The mod's `patch_next`. A second order replaces the first (the first is reported as refused).
    pub fn arm(&self, frozen: Vec<u32>, adds: u32, ttl: u64, now: u64) {
        if !self.enabled() {
            self.refuse("FEVERLINK3_NATIVE_PATCH is off");
            return;
        }
        let ttl_ms = if ttl == 0 { DEFAULT_TTL_MS } else { ttl.min(MAX_TTL_MS) };
        let old = lock(&self.order).replace(Order { frozen, adds, armed_ms: now, ttl_ms });
        if old.is_some() {
            self.refuse("replaced by a newer patch_next before a command used it");
        }
        self.pending.store(true, Ordering::Release);
    }

    /// The armed order, once: `None` when there is none or it has expired (which is reported).
    pub fn take(&self, now: u64) -> Option<Order> {
        let order = lock(&self.order).take();
        self.pending.store(false, Ordering::Release);
        let order = order?;
        if now.saturating_sub(order.armed_ms) > order.ttl_ms {
            self.refuse(format!("expired: no command arrived within {} ms", order.ttl_ms));
            return None;
        }
        Some(order)
    }

    pub fn report(&self, event: Event) {
        lock(&self.events).push(event);
    }

    /// Expires an order nobody used (the service thread calls this), and hands out the events.
    pub fn drain_events(&self, now: u64) -> Vec<Event> {
        if self.pending() {
            let mut g = lock(&self.order);
            if g.as_ref().is_some_and(|o| now.saturating_sub(o.armed_ms) > o.ttl_ms) {
                let o = g.take().expect("checked");
                self.pending.store(false, Ordering::Release);
                drop(g);
                self.refuse(format!("expired: no command arrived within {} ms", o.ttl_ms));
            }
        }
        std::mem::take(&mut *lock(&self.events))
    }
}

/// Writes into memory the engine owns (a command's payload and the list blocks in it).
pub trait Poke {
    fn poke(&self, addr: usize, data: &[u8]) -> bool;
}

/// What a successful patch did.
#[derive(Debug, PartialEq, Eq)]
pub struct Done {
    /// A new block was allocated for the proposal's list.
    pub allocated: bool,
}

/// One list of u32 about to be written.
struct Plan {
    /// Where the vector's three pointers are.
    triple: usize,
    /// Where the values go (an existing block with room, or a new one).
    at: usize,
    fresh: bool,
}

fn triple(bytes: &[u8], off: usize) -> Result<(u64, u64, u64), String> {
    Ok((u64_at(bytes, off), u64_at(bytes, off + 8), u64_at(bytes, off + 16)))
}

/// The checks and the writes. Reads `payload` through `mem`, takes a block from `alloc` for an empty list, writes through `poke`.
/// Nothing is written (and nothing stays allocated) unless every check passes.
pub fn apply(mem: &dyn Mem, poke: &dyn Poke, alloc: &dyn Alloc, payload: usize, order: &Order) -> Result<Done, String> {
    let n = order.frozen.len();
    if n == 0 || n > MAX_PATCH_FROZEN {
        return Err(format!("{n} frozen indices"));
    }
    let bytes = mem.read(payload, PAYLOAD_SIZE).ok_or("the command's payload is not readable")?;
    if bytes[KIND_OFFSET] != KIND_BUILD_PROPOSAL {
        return Err(format!("the command is kind {}, not build_proposal", bytes[KIND_OFFSET]));
    }
    // the added nodes: the indices count into this list
    let (b, e, c) = triple(&bytes, ADDED_NODES)?;
    let nodes = if b == 0 && e == 0 && c == 0 {
        0
    } else if pointer_like(b) && b <= e && e <= c && ((e - b) as usize).is_multiple_of(NODE_STRIDE) {
        (e - b) as usize / NODE_STRIDE
    } else {
        return Err("proposal.addedNodes is not a vector".into());
    };
    if nodes != order.adds as usize {
        return Err(format!("the command has {nodes} added nodes, the order says {}", order.adds));
    }
    if let Some(i) = order.frozen.iter().find(|&&i| i as usize >= nodes) {
        return Err(format!("frozen index {i} is not below the {nodes} added nodes"));
    }
    // the constructions
    let (tb, te, tc) = triple(&bytes, TO_ADD)?;
    if tb == 0 && te == 0 && tc == 0 {
        return Err("the command adds no construction".into());
    }
    if !(pointer_like(tb) && tb <= te && te <= tc && ((te - tb) as usize).is_multiple_of(CONSTRUCTION_STRIDE)) {
        return Err("proposal toAdd is not a vector".into());
    }
    if te == tb {
        return Err("the command adds no construction".into());
    }
    let first = tb as usize;
    // the two lists, as they are now
    let mut lists = Vec::new();
    for i in 0..2 {
        let (addr, raw) = if i == 0 {
            (payload + PROPOSAL_FROZEN, triple(&bytes, PROPOSAL_FROZEN)?)
        } else {
            let a = first + CONSTRUCTION_FROZEN;
            let t = mem.read(a, 24).ok_or("toAdd[0] is not readable")?;
            (a, triple(&t, 0)?)
        };
        let (b, e, c) = raw;
        let empty = b == 0 && e == 0 && c == 0;
        if !empty && !(pointer_like(b) && b % 4 == 0 && b <= e && e <= c && (e - b) % 4 == 0) {
            return Err(format!(
                "{} is not a vector of 4-byte values",
                if i == 0 { "proposal.frozenNodes" } else { "toAdd[0].frozenNodes" }
            ));
        }
        lists.push((addr, b, c));
    }
    // allocate what is needed; the old blocks are never touched here
    let mut plans = Vec::new();
    let mut fail = None;
    for &(triple, b, c) in &lists {
        if b != 0 && (c - b) as usize >= 4 * n {
            plans.push(Plan { triple, at: b as usize, fresh: false });
        } else {
            match alloc.alloc(4 * n) {
                Some(at) => plans.push(Plan { triple, at, fresh: true }),
                None => {
                    fail = Some("the game's allocator gave no block");
                    break;
                }
            }
        }
    }
    if let Some(why) = fail {
        alloc.free_all();
        return Err(why.into());
    }
    // the values go in first, the pointers last: a failure in between leaves a list that still says what it said
    let mut data = Vec::with_capacity(4 * n);
    for v in &order.frozen {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let mut ok = true;
    for p in &plans {
        ok &= if p.fresh { alloc.write(p.at, &data) } else { poke.poke(p.at, &data) };
    }
    if !ok {
        alloc.free_all();
        return Err("a list could not be written".into());
    }
    for (p, &(_, _, c)) in plans.iter().zip(&lists) {
        let begin = p.at as u64;
        let end = begin + 4 * n as u64;
        let cap = if p.fresh { end } else { c };
        let mut t = Vec::with_capacity(24);
        t.extend_from_slice(&begin.to_le_bytes());
        t.extend_from_slice(&end.to_le_bytes());
        t.extend_from_slice(&cap.to_le_bytes());
        if !poke.poke(p.triple, &t) {
            // the first list may be done already; its values are valid, so is its triple: keep both, report the second
            alloc.forget_all();
            return Err("a list's pointers could not be written".into());
        }
    }
    let allocated = plans.iter().any(|p| p.fresh);
    alloc.forget_all(); // the command's Proposal owns the blocks now
    Ok(Done { allocated })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dump::u32_at;
    use crate::rebuild::Arena;

    const HEAP: usize = 0x2000_0000_0000;

    impl Poke for Arena {
        fn poke(&self, addr: usize, data: &[u8]) -> bool {
            self.write(addr, data)
        }
    }

    fn put(buf: &mut [u8], off: usize, v: u64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// A payload with `nodes` added nodes, `cons` constructions; the frozen lists as given (begin, end, cap), 0 for empty.
    fn world(
        nodes: usize,
        cons: usize,
        proposal_list: Option<(usize, usize)>,
        entity_list: Option<(usize, usize)>,
    ) -> (Arena, usize) {
        let a = Arena::new(HEAP + 0x100_0000);
        let payload = HEAP;
        let mut p = vec![0u8; PAYLOAD_SIZE];
        p[KIND_OFFSET] = KIND_BUILD_PROPOSAL;
        let nb = a.add(&vec![0u8; nodes.max(1) * NODE_STRIDE]);
        if nodes > 0 {
            put(&mut p, ADDED_NODES, nb as u64);
            put(&mut p, ADDED_NODES + 8, (nb + nodes * NODE_STRIDE) as u64);
            put(&mut p, ADDED_NODES + 16, (nb + nodes * NODE_STRIDE) as u64);
        }
        let cb = a.add(&vec![0u8; cons.max(1) * CONSTRUCTION_STRIDE]);
        if cons > 0 {
            put(&mut p, TO_ADD, cb as u64);
            put(&mut p, TO_ADD + 8, (cb + cons * CONSTRUCTION_STRIDE) as u64);
            put(&mut p, TO_ADD + 16, (cb + cons * CONSTRUCTION_STRIDE) as u64);
        }
        if let Some((len, cap)) = proposal_list {
            let blk = a.add(&vec![0xeeu8; cap]);
            put(&mut p, PROPOSAL_FROZEN, blk as u64);
            put(&mut p, PROPOSAL_FROZEN + 8, (blk + len) as u64);
            put(&mut p, PROPOSAL_FROZEN + 16, (blk + cap) as u64);
        }
        if let Some((len, cap)) = entity_list {
            let blk = a.add(&vec![0xeeu8; cap]);
            let mut t = vec![0u8; 24];
            put(&mut t, 0, blk as u64);
            put(&mut t, 8, (blk + len) as u64);
            put(&mut t, 16, (blk + cap) as u64);
            a.write(cb + CONSTRUCTION_FROZEN, &t);
        }
        a.put(payload, p);
        (a, payload)
    }

    fn order(frozen: &[u32], adds: u32) -> Order {
        Order { frozen: frozen.to_vec(), adds, armed_ms: 0, ttl_ms: 1000 }
    }

    fn list(a: &Arena, addr: usize) -> (Vec<u32>, u64, u64, u64) {
        let t = a.read(addr, 24).unwrap();
        let (b, e, c) = (u64_at(&t, 0), u64_at(&t, 8), u64_at(&t, 16));
        let v = if b == 0 {
            vec![]
        } else {
            a.read(b as usize, (e - b) as usize).unwrap().chunks(4).map(|x| u32_at(x, 0)).collect()
        };
        (v, b, e, c)
    }

    type Listing = (Vec<u32>, u64, u64, u64);

    fn lists(a: &Arena, payload: usize) -> (Listing, Listing) {
        let p = a.read(payload, PAYLOAD_SIZE).unwrap();
        let first = u64_at(&p, TO_ADD) as usize;
        (list(a, payload + PROPOSAL_FROZEN), list(a, first + CONSTRUCTION_FROZEN))
    }

    #[test]
    fn empty_lists_get_new_blocks_with_the_indices() {
        let (a, p) = world(3, 1, None, None);
        let done = apply(&a, &a, &a, p, &order(&[1], 3)).expect("patched");
        assert_eq!(done, Done { allocated: true });
        let (x, y) = lists(&a, p);
        assert_eq!((x.0.clone(), y.0.clone()), (vec![1], vec![1]));
        assert_eq!(x.2 - x.1, 4);
        assert_eq!(x.3, x.2, "capacity equals the written size");
        assert_ne!(x.1, y.1, "each list has its own block");
    }

    #[test]
    fn a_list_with_room_is_written_in_place() {
        let (a, p) = world(2, 1, Some((4, 16)), Some((0, 16)));
        let before = lists(&a, p);
        let done = apply(&a, &a, &a, p, &order(&[0, 1], 2)).expect("patched");
        assert_eq!(done, Done { allocated: false });
        let after = lists(&a, p);
        assert_eq!(after.0 .0, vec![0, 1]);
        assert_eq!(after.1 .0, vec![0, 1]);
        assert_eq!((after.0 .1, after.0 .3), (before.0 .1, before.0 .3), "the engine's block and capacity are kept");
    }

    #[test]
    fn a_list_without_room_gets_a_new_block_and_the_old_one_is_left_alone() {
        let (a, p) = world(2, 1, Some((4, 4)), None);
        let before = lists(&a, p);
        let old = a.read(before.0 .1 as usize, 4).unwrap();
        let done = apply(&a, &a, &a, p, &order(&[0, 1], 2)).expect("patched");
        assert!(done.allocated);
        let after = lists(&a, p);
        assert_eq!(after.0 .0, vec![0, 1]);
        assert_ne!(after.0 .1, before.0 .1);
        assert_eq!(a.read(before.0 .1 as usize, 4).unwrap(), old, "the old block is untouched");
    }

    #[test]
    fn every_refusal_leaves_the_command_untouched() {
        let cases: [(Arena, usize, Order, &str); 6] = {
            let w = |n, c| world(n, c, None, None);
            let (a1, p1) = w(3, 1);
            let (a2, p2) = w(3, 1);
            let (a3, p3) = w(3, 0);
            let (a4, p4) = w(3, 1);
            let (a5, p5) = w(3, 1);
            let (mut a6, p6) = w(3, 1);
            // not a build command
            let mut raw = a6.read(p6, PAYLOAD_SIZE).unwrap();
            raw[KIND_OFFSET] = 7;
            a6 = {
                let b = Arena::new(HEAP + 0x200_0000);
                b.put(p6, raw);
                b
            };
            [
                (a1, p1, order(&[1], 4), "added nodes"),
                (a2, p2, order(&[3], 3), "not below"),
                (a3, p3, order(&[1], 3), "no construction"),
                (a4, p4, order(&[], 3), "frozen indices"),
                (a5, p5, order(&vec![0; 65], 3), "frozen indices"),
                (a6, p6, order(&[1], 3), "not build_proposal"),
            ]
        };
        for (a, p, o, why) in cases {
            let before = a.read(p, PAYLOAD_SIZE).unwrap();
            let err = apply(&a, &a, &a, p, &o).expect_err(why);
            assert!(err.contains(why), "{err} / {why}");
            assert_eq!(a.read(p, PAYLOAD_SIZE).unwrap(), before, "{why}");
        }
    }

    #[test]
    fn a_list_that_is_not_a_vector_is_refused_without_allocating() {
        let (a, p) = world(3, 1, None, None);
        let mut raw = a.read(p, PAYLOAD_SIZE).unwrap();
        put(&mut raw, PROPOSAL_FROZEN, 0x1234); // not a pointer, others zero
        a.write(p, &raw);
        let err = apply(&a, &a, &a, p, &order(&[1], 3)).unwrap_err();
        assert!(err.contains("not a vector of 4-byte values"), "{err}");
        assert_eq!(a.read(p, PAYLOAD_SIZE).unwrap(), raw);
    }

    #[test]
    fn ready_lists_patch_next_only_when_everything_holds() {
        let build = crate::portable::LAYOUT_BUILD;
        assert!(announces(true, build, || true));
        assert!(!announces(false, build, || panic!("not asked when the switch is off")));
        assert!(!announces(true, "other", || panic!("not asked for another build")));
        assert!(!announces(true, build, || false));
    }

    #[test]
    fn the_state_arms_expires_and_reports() {
        let s = State::default();
        s.arm(vec![1], 3, 0, 100);
        assert!(!s.pending(), "off: refused");
        assert!(matches!(s.drain_events(100).as_slice(), [Event::PatchRefused { reason }] if reason.contains("off")));
        s.enable();
        s.arm(vec![1], 3, 0, 100);
        assert!(s.pending());
        assert!(s.take(100 + DEFAULT_TTL_MS + 1).is_none());
        assert!(matches!(s.drain_events(0).as_slice(), [Event::PatchRefused { reason }] if reason.contains("expired")));
        s.arm(vec![1], 3, 50, 100);
        assert_eq!(s.take(120), Some(Order { frozen: vec![1], adds: 3, armed_ms: 100, ttl_ms: 50 }));
        assert!(!s.pending());
        // a newer order replaces; an unused one expires on the service thread
        s.arm(vec![1], 3, 10, 0);
        s.arm(vec![0], 2, 10, 5);
        let ev = s.drain_events(6);
        assert!(matches!(ev.as_slice(), [Event::PatchRefused { reason }] if reason.contains("replaced")));
        assert!(
            matches!(s.drain_events(100).as_slice(), [Event::PatchRefused { reason }] if reason.contains("expired"))
        );
        assert!(!s.pending());
    }
}
