//! The bulldoze refund, natively (owner decision 2026-10-18): the injected command's own Context `refundableEntities` is filled by
//! the engine's own insert function, with the entries every game resolved to its own entities.
//!
//! Why: MPFever measured that a `refundableEntities` list written into a Context from a SCRIPT gives no refund (the removal cost the
//! same with or without it). Our replays wrote the typed map from Lua (the game reads it back; no money effect proven). The tool's own
//! command carries the list in its payload (`engine::refundable_entities`, payload+0x370), so the receivers' rebuilt command gets the
//! same container content, made by the engine, in the command the simulation applies.
//!
//! The container is MSVC's `std::unordered_map<{entity, int}, {revision, int}>` (0x40 bytes: max load factor +0, list head +8, size
//! +0x10, bucket vector +0x18..+0x30, mask +0x30, buckets +0x38; list nodes of 0x28 bytes: next, prev, the 24-byte entry). Nothing of
//! it is laid out by hand: the engine's `_Hash::_Insert_range_unchecked` (`0x4be210` in build 40420) is called on it with a source range
//! that lives in Rust memory. The function was found where the bulldozer tool makes its own command (`0x594340`): the tool constructs
//! the Context's container on its stack (an `operator new(0x28)` sentinel, the bucket vector by `0x10d4d0`) and fills it with
//! `call 0x4be210(rcx = &container, rdx = source head->next, r8 = source head)`, the copy of the interface's list. The function walks
//! the source from `first` by each node's `next` until it reaches `last` (it reads only `next` and the 24 bytes at +0x10), hashes the
//! key with the container's own hasher, skips a key that is already there (unique keys), allocates each new node with the game's
//! `operator new(0x28)` (`0x318c530`, the allocator `rebuild::EngineAlloc` verifies), links it, and rehashes when the load factor
//! demands it. The new nodes belong to the container and the command's destructor frees them (`0x4c8bd0`, the hash destructor the
//! payload's destructor calls); the source range is never linked in and is freed by Rust after the call. So nothing is freed twice and
//! nothing the engine owns is freed by us.
//!
//! The source range is our own: a call of an engine function on our data. It is a plain list of nodes laid out as the engine's list
//! nodes are (the read side is proved by `engine::refundable_entities` on real commands), only read by the engine.
//!
//! The list alone pays nothing: the engine also needs the Context's refund window ([`WINDOW_OFFSET`]). Lab O (2026-10-19): the real
//! bulldozer removed a street 9.4 s after the real street tool built it; on both games, the originator included, the injected command's
//! container held the tool's own entry (`50458{-1}rev7,11,0/3875`, native.log "1 were there from the script's conversion") and the
//! removal still cost 1023 with no refund. Where the engine decides (build 40420, read-only disassembly): the build command's apply
//! (`0x9e2900`) passes the payload's Context (`lea rcx,[rdi+0x358]`) to the proposal evaluation (`0xa212d0` -> `0xa21460` ->
//! `0xa101b0`); for every removed entity the latter refunds only when `Context+0xc > 0.0` (`vcomiss xmm0,0; jbe` before each of its
//! three calls of the check, and in `0xa0fdd4`), and the check (`0xa119c0`) finds `{entity, key}` in `Context+0x18` (the list), compares
//! the entry's first revision word with the entity's current one (`0x2bbae90`) and wants `now - value < round(window * 5 + 0.5)`
//! updates (`now` from `0x2a96c0`, the counter `updateRefundableEntities` (`0xa11ad0`) stores as the value; 5 updates are a second
//! at the 200 ms step, so the window is in seconds). The tools' commands carry 60.0 there (every held text with a non-empty list:
//! `~0x363 raw:b:..00007042..`); a script command's Context has no Lua field for it and keeps 0.0 (every donor text). The injection
//! swaps only the Proposal into the script command, so the replay, the originator's own included, ran with window 0: no refund. The
//! text now names the window (`context.refundWindow`) and the injection writes the originator's value into the command's Context.

use crate::cmd_patch::Poke;
use crate::dump::{u64_at, Mem};
use crate::engine::{RefundEntry, REFUNDABLE_MAX, REFUNDABLE_NODE, REFUNDABLE_OFFSET, REFUNDABLE_VALUE};

/// The Context's refund window in a build command's payload (`Context+0xc`, the Context at +0x358): seconds of game time in which a
/// listed entity is refunded when it is removed; 0.0 refunds nothing (see the module comment).
pub const WINDOW_OFFSET: usize = 0x364;

/// The text's member of the window (`portable::PAYLOAD`).
pub const WINDOW_NAME: &str = "context.refundWindow";

/// The originator's refund window as its text names it: `None` when the text has no such line (a text of an older module: the command
/// keeps its own); `Err` when the value is not a finite, non-negative float (the order is refused).
pub fn window_of(text: &str) -> Result<Option<f32>, String> {
    let Some(v) = text.lines().find_map(|l| l.strip_prefix(WINDOW_NAME).and_then(|r| r.strip_prefix('\t'))) else {
        return Ok(None);
    };
    let w = v.trim_end().strip_prefix("f32:").and_then(|h| u32::from_str_radix(h, 16).ok()).map(f32::from_bits);
    match w {
        Some(w) if w.is_finite() && w >= 0.0 => Ok(Some(w)),
        _ => Err(format!("the text's {WINDOW_NAME} {v:?} is not a finite, non-negative f32")),
    }
}

/// Writes `window` into the command's Context (a plain float of the Context, no allocation); `Err` when the write failed.
pub fn set_window(poke: &dyn Poke, payload: usize, window: f32) -> Result<(), String> {
    if poke.poke(payload + WINDOW_OFFSET, &window.to_le_bytes()) {
        Ok(())
    } else {
        Err(format!("the Context's refund window at payload+{WINDOW_OFFSET:#x} could not be written"))
    }
}

/// `void _Insert_range_unchecked(this, first, last)`: `this` the container, `first` the first source node, `last` the end (a node
/// address that is never read as an entry).
pub type InsertRangeFn = unsafe extern "system-unwind" fn(*mut u8, *const u8, *const u8);

/// The engine's insert function of the refund container.
#[derive(Clone, Copy)]
pub struct RefundFns {
    pub insert: InsertRangeFn,
}

/// The two `call rel32` in the first 0xea bytes of `std::_Hash<...refundableEntities...>::_Insert_range_unchecked(this, first,
/// last)` (`build_table::EngineFns::refund_insert`, whose bytes the build search recorded after proving the whole function and the
/// start of every callee): the key hasher and `operator new(0x28)`. Their displacements are not compared; the second must lead
/// to the row's `engine_new`. Offsets in the reference build's code, which the search proved unchanged.
pub const INSERT_CALLS: [usize; 2] = [0x47, 0xc9];

/// True when `got` is `want` outside the displacements of [`INSERT_CALLS`].
pub fn same_code(want: &[u8], got: &[u8]) -> bool {
    got.len() == want.len()
        && (0..got.len()).all(|i| got[i] == want[i] || INSERT_CALLS.iter().any(|&c| i > c && i <= c + 4))
}

/// Where the `call rel32` at offset `call` of `code` (which starts at RVA `at`) leads.
pub fn call_target(code: &[u8], call: usize, at: u32) -> u32 {
    let rel = i32::from_le_bytes(code[call + 1..call + 5].try_into().expect("four bytes"));
    at.wrapping_add(call as u32 + 5).wrapping_add(rel as u32)
}

impl RefundFns {
    /// The function of the running game, after its bytes are compared with the recorded ones and its `operator new` call followed.
    pub fn at(base: usize, engine: &crate::build_table::EngineFns) -> Result<RefundFns, String> {
        let rva = crate::build_table::engine_rva(engine.refund_insert, "the refund list's insert function")?;
        let new = crate::build_table::engine_rva(engine.engine_new, "operator new")?;
        let want = engine.prologue("refund_insert")?;
        if want.len() <= INSERT_CALLS[1] + 4 {
            return Err("the refund list's insert function has too few recorded bytes".to_owned());
        }
        let got = crate::dump::LiveMem
            .read(base + rva as usize, want.len())
            .ok_or_else(|| format!("the refund list's insert function at {rva:#x} is not readable"))?;
        if !same_code(want, &got) {
            return Err(format!("the refund list's insert function at {rva:#x} does not have the expected bytes"));
        }
        if call_target(&got, INSERT_CALLS[1], rva) != new {
            return Err(format!(
                "the refund list's insert function at {rva:#x} does not call operator new at {new:#x}"
            ));
        }
        // SAFETY: the address was just compared with the bytes of the engine function of this signature and its calls followed.
        Ok(RefundFns { insert: unsafe { core::mem::transmute::<usize, InsertRangeFn>(base + rva as usize) } })
    }
}

/// The side file of an inject order with refund entries (`native_refund_<key>.txt`, written by the mod before the order): one line
/// per entry, `entity<TAB>key<TAB>rev0<TAB>rev1<TAB>rev2<TAB>value`, this game's own entity ids.
pub fn file_name(key: u64) -> String {
    format!("native_refund_{key}.txt")
}

/// The entries of a side file; exactly `want` of them (the order's `refund` count), no key `{entity, key}` twice.
pub fn parse_file(text: &str, want: u64) -> Result<Vec<RefundEntry>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().map(str::trim_end).filter(|l| !l.is_empty()).enumerate() {
        let w: Vec<&str> = line.split('\t').collect();
        let bad = || format!("refund entry {}: {line:?} is not entity, key, three revision integers, value", i + 1);
        if w.len() != 6 {
            return Err(bad());
        }
        let int = |s: &str| s.parse::<i64>().ok().filter(|v| (i32::MIN as i64..=u32::MAX as i64).contains(v));
        let v: Option<Vec<i64>> = w.iter().map(|s| int(s)).collect();
        let v = v.ok_or_else(bad)?;
        let small = |x: i64| i32::try_from(x).ok();
        let entity = u32::try_from(v[0]).map_err(|_| bad())?;
        let (key, r0, r1, r2, value) = (small(v[1]), small(v[2]), small(v[3]), small(v[4]), small(v[5]));
        match (key, r0, r1, r2, value) {
            (Some(key), Some(a), Some(b), Some(c), Some(value)) => {
                out.push(RefundEntry { entity, key, revision: [a, b, c], value })
            }
            _ => return Err(bad()),
        }
        if out.len() > REFUNDABLE_MAX {
            return Err(format!("more than {REFUNDABLE_MAX} refund entries"));
        }
    }
    if out.len() as u64 != want {
        return Err(format!("the refund side file has {} entries, the order says {want}", out.len()));
    }
    let mut keys: Vec<(u32, i32)> = out.iter().map(|e| (e.entity, e.key)).collect();
    keys.sort_unstable();
    if keys.windows(2).any(|p| p[0] == p[1]) {
        return Err("a refund key {entity, key} is named twice (the engine keeps one entry per key)".into());
    }
    Ok(out)
}

/// The source range given to the engine's insert: `n` list nodes and an end node, laid out as the engine's own nodes (next, prev, the
/// 24-byte entry at +0x10). Rust memory, only read by the engine; never linked into the container.
pub struct Chain {
    nodes: Vec<[u64; 5]>,
}

const _: () = assert!(core::mem::size_of::<[u64; 5]>() == REFUNDABLE_NODE && REFUNDABLE_VALUE == 0x10);

impl Chain {
    pub fn new(entries: &[RefundEntry]) -> Chain {
        let mut nodes = vec![[0u64; 5]; entries.len() + 1];
        let base = nodes.as_ptr() as u64;
        let addr = |i: usize| base + (i * REFUNDABLE_NODE) as u64;
        let n = nodes.len();
        for (i, node) in nodes.iter_mut().enumerate() {
            node[0] = addr((i + 1) % n); // next: the last entry's is the end node, the end node's the first (a closed list)
            node[1] = addr((i + n - 1) % n);
            if let Some(e) = entries.get(i) {
                let w = |lo: u32, hi: u32| lo as u64 | ((hi as u64) << 32);
                node[2] = w(e.entity, e.key as u32);
                node[3] = w(e.revision[0] as u32, e.revision[1] as u32);
                node[4] = w(e.revision[2] as u32, e.value as u32);
            }
        }
        Chain { nodes }
    }

    /// The first node the engine reads (the end node when there are none).
    pub fn first(&self) -> usize {
        self.nodes.as_ptr() as usize
    }

    /// The end of the range: compared by address, never read as an entry.
    pub fn end(&self) -> usize {
        self.first() + (self.nodes.len() - 1) * REFUNDABLE_NODE
    }

    /// Whether `addr` lies in this range's memory.
    pub fn holds(&self, addr: usize) -> bool {
        (self.first()..self.first() + self.nodes.len() * REFUNDABLE_NODE).contains(&addr)
    }
}

/// The container at `payload`+0x370, read through `mem`: its entries and the addresses of its list nodes. `Err` unless it is an
/// engine-made container: a positive load factor, a list that closes on its sentinel after exactly `size` nodes, `mask + 1` buckets
/// (a power of two, at least 8) and a bucket vector of two pointers per bucket.
pub fn read_container(mem: &dyn Mem, payload: usize) -> Result<(Vec<RefundEntry>, Vec<usize>), String> {
    let c = payload + REFUNDABLE_OFFSET;
    let h = mem.read(c, 0x40).ok_or("the Context's refundableEntities is not readable")?;
    let load = f32::from_le_bytes(h[0..4].try_into().unwrap_or([0; 4]));
    let (head, size) = (u64_at(&h, 8) as usize, u64_at(&h, 0x10) as usize);
    let (vb, ve) = (u64_at(&h, 0x18) as usize, u64_at(&h, 0x20) as usize);
    let (mask, buckets) = (u64_at(&h, 0x30) as usize, u64_at(&h, 0x38) as usize);
    if !(load.is_finite() && load > 0.0) {
        return Err(format!(
            "the Context's refundableEntities has no load factor ({load}): not an engine-made container"
        ));
    }
    if head == 0 || size > REFUNDABLE_MAX {
        return Err(format!("the Context's refundableEntities has no list (head {head:#x}, size {size})"));
    }
    if buckets < 8 || !buckets.is_power_of_two() || mask != buckets - 1 || vb == 0 || ve < vb || ve - vb != 16 * buckets
    {
        return Err(format!(
            "the Context's refundableEntities has no bucket table of an engine-made container (mask {mask:#x}, {buckets} buckets, vector {} bytes)",
            ve.wrapping_sub(vb)
        ));
    }
    let word = |at: usize| mem.read(at, 8).map(|b| u64_at(&b, 0) as usize);
    let mut entries = Vec::with_capacity(size);
    let mut addrs = Vec::with_capacity(size);
    let mut node = word(head).ok_or("the list's sentinel is not readable")?;
    for _ in 0..size {
        if node == head || node == 0 {
            return Err("the list ends before its size".into());
        }
        let n = mem.read(node, REFUNDABLE_NODE).ok_or("a list node is not readable")?;
        let w = |i: usize| {
            u32::from_le_bytes(n[REFUNDABLE_VALUE + 4 * i..REFUNDABLE_VALUE + 4 * i + 4].try_into().unwrap())
        };
        entries.push(RefundEntry {
            entity: w(0),
            key: w(1) as i32,
            revision: [w(2) as i32, w(3) as i32, w(4) as i32],
            value: w(5) as i32,
        });
        addrs.push(node);
        node = u64_at(&n, 0) as usize;
    }
    if node != head {
        return Err("the list does not close on its sentinel after its size".into());
    }
    Ok((entries, addrs))
}

/// What the insert is to do, decided before the engine is called ([`plan`]).
pub struct Plan {
    /// The entries the container does not have yet (by key): the source range.
    pub chain: Chain,
    pub missing: usize,
    /// Entries in the container before (the script's conversion of the mod's Context).
    pub before: usize,
    /// Of ours, the keys the container already has with other values (the engine keeps those; reported).
    pub differ: usize,
}

/// The checks before the insert (no engine call): the container is an engine-made one; which of `want` it lacks.
pub fn plan(mem: &dyn Mem, payload: usize, want: &[RefundEntry]) -> Result<Plan, String> {
    let (have, _) = read_container(mem, payload)?;
    if have.len() + want.len() > REFUNDABLE_MAX {
        return Err(format!("{} entries would exceed {REFUNDABLE_MAX}", have.len() + want.len()));
    }
    let find = |e: &RefundEntry| have.iter().find(|h| h.entity == e.entity && h.key == e.key);
    let missing: Vec<RefundEntry> = want.iter().filter(|e| find(e).is_none()).copied().collect();
    let differ = want.iter().filter(|e| find(e).is_some_and(|h| h != *e)).count();
    Ok(Plan { chain: Chain::new(&missing), missing: missing.len(), before: have.len(), differ })
}

/// The engine's insert of `plan`'s range into the container at `payload`+0x370 (nothing when nothing is missing).
///
/// # Safety
/// `payload` is a live build command's payload whose container [`plan`] accepted just now, on the thread that queues or applies it;
/// `fns` are the verified function (or a test stand-in). Calls the engine: never inside `log::guarded`.
pub unsafe fn insert(fns: &RefundFns, payload: usize, plan: &Plan) {
    if plan.missing == 0 {
        return;
    }
    // SAFETY: as the caller guarantees; the range is `plan.chain`, alive until the call returns, only read.
    unsafe {
        (fns.insert)(
            (payload + REFUNDABLE_OFFSET) as *mut u8,
            plan.chain.first() as *const u8,
            plan.chain.end() as *const u8,
        )
    };
}

/// The result of an insert, read back from the container.
#[derive(Debug, PartialEq, Eq)]
pub struct Done {
    pub before: usize,
    pub set: usize,
    pub differ: usize,
    pub after: usize,
}

/// After [`insert`]: every key of `want` is in the container, it holds exactly `before + missing` entries, and none of its nodes is
/// one of the source range's (the engine copied them into nodes of its own). `Err` otherwise (the order is refused).
pub fn check(mem: &dyn Mem, payload: usize, want: &[RefundEntry], plan: &Plan) -> Result<Done, String> {
    let (have, addrs) = read_container(mem, payload).map_err(|e| format!("after the insert: {e}"))?;
    if addrs.iter().any(|&a| plan.chain.holds(a)) {
        return Err("after the insert a node of the container is one of our source nodes".into());
    }
    if have.len() != plan.before + plan.missing {
        return Err(format!(
            "after the insert the container holds {} entries, {} expected ({} before + {} inserted)",
            have.len(),
            plan.before + plan.missing,
            plan.before,
            plan.missing
        ));
    }
    if let Some(e) = want.iter().find(|e| !have.iter().any(|h| h.entity == e.entity && h.key == e.key)) {
        return Err(format!("after the insert entity {} (key {}) is not in the container", e.entity, e.key));
    }
    Ok(Done { before: plan.before, set: plan.missing, differ: plan.differ, after: have.len() })
}

/// The entries as the log writes them (as the mod's `refund:` lines do): `entity{key}rev a,b,c/value`.
pub fn text(entries: &[RefundEntry]) -> String {
    entries
        .iter()
        .map(|e| {
            format!("{}{{{}}}rev{},{},{}/{}", e.entity, e.key, e.revision[0], e.revision[1], e.revision[2], e.value)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::dump::LiveMem;

    #[test]
    fn the_insert_is_compared_outside_its_two_call_displacements_which_are_followed() {
        let engine = &crate::build_table::fixture().engine;
        let code = engine.prologue("refund_insert").unwrap();
        assert!(same_code(code, code));
        // the second call is operator new
        assert_eq!(call_target(code, INSERT_CALLS[1], engine.refund_insert.unwrap()), engine.engine_new.unwrap());
        let mut moved = code.to_vec();
        moved[INSERT_CALLS[1] + 1] ^= 0x55;
        assert!(same_code(code, &moved), "a displacement may differ");
        moved[INSERT_CALLS[1]] = 0x90;
        assert!(!same_code(code, &moved), "the call itself may not");
        assert!(!same_code(code, &code[1..]));
    }

    /// A synthetic engine container in Rust memory, laid out as the engine's (`std::unordered_map`, 0x40 bytes at payload+0x370):
    /// a sentinel, list nodes, 8 buckets. The nodes the stand-in insert makes are kept here too, so the test frees them (as the
    /// command's destructor would).
    pub struct Synthetic {
        pub payload: Box<[u8; 0x9c0]>,
        pub sentinel: Box<[u64; 5]>,
        pub buckets: Box<[u64; 16]>,
        /// One box per node: the list links them by address, so a node must not move when the vector grows.
        #[allow(clippy::vec_box)]
        pub nodes: Vec<Box<[u64; 5]>>,
    }

    impl Synthetic {
        pub fn new(entries: &[RefundEntry]) -> Box<Synthetic> {
            let mut s = Box::new(Synthetic {
                payload: Box::new([0u8; 0x9c0]),
                sentinel: Box::new([0u64; 5]),
                buckets: Box::new([0u64; 16]),
                nodes: Vec::new(),
            });
            let head = s.sentinel.as_ptr() as u64;
            s.sentinel[0] = head;
            s.sentinel[1] = head;
            for b in s.buckets.iter_mut() {
                *b = head; // an empty bucket's two iterators are the list's end
            }
            let c = REFUNDABLE_OFFSET;
            let vb = s.buckets.as_ptr() as u64;
            s.payload[c..c + 4].copy_from_slice(&1.0f32.to_le_bytes());
            s.payload[c + 8..c + 16].copy_from_slice(&head.to_le_bytes());
            s.payload[c + 0x18..c + 0x20].copy_from_slice(&vb.to_le_bytes());
            s.payload[c + 0x20..c + 0x28].copy_from_slice(&(vb + 128).to_le_bytes());
            s.payload[c + 0x28..c + 0x30].copy_from_slice(&(vb + 128).to_le_bytes());
            s.payload[c + 0x30..c + 0x38].copy_from_slice(&7u64.to_le_bytes());
            s.payload[c + 0x38..c + 0x40].copy_from_slice(&8u64.to_le_bytes());
            for e in entries {
                s.append(*e);
            }
            s
        }

        pub fn addr(&self) -> usize {
            self.payload.as_ptr() as usize
        }

        /// Appends one node at the end of the list (as the engine's insert links a node of a new key), size + 1.
        pub fn append(&mut self, e: RefundEntry) {
            let head = self.sentinel.as_ptr() as u64;
            let last = self.sentinel[1];
            let w = |lo: u32, hi: u32| lo as u64 | ((hi as u64) << 32);
            let mut node = Box::new([
                head,
                last,
                w(e.entity, e.key as u32),
                w(e.revision[0] as u32, e.revision[1] as u32),
                w(e.revision[2] as u32, e.value as u32),
            ]);
            let at = node.as_mut_ptr() as u64;
            if last == head {
                self.sentinel[0] = at;
            } else {
                // SAFETY: `last` is a node of this container (a box in `nodes`).
                unsafe { *(last as *mut u64) = at };
            }
            self.sentinel[1] = at;
            self.nodes.push(node);
            let c = REFUNDABLE_OFFSET + 0x10;
            let size = u64_at(&self.payload[..], c) + 1;
            self.payload[c..c + 8].copy_from_slice(&size.to_le_bytes());
        }
    }

    thread_local! {
        /// The container the stand-in insert writes into (its `this` is the container inside the payload).
        static TARGET: std::cell::Cell<*mut Synthetic> = const { std::cell::Cell::new(core::ptr::null_mut()) };
        static CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// The engine's insert as far as its contract goes: walk `first` to `last` by `next`, copy each entry whose key is new into a node
    /// of the container's own (never the source node), unique keys.
    unsafe extern "system-unwind" fn fake_insert(this: *mut u8, first: *const u8, last: *const u8) {
        CALLS.with(|c| c.set(c.get() + 1));
        let s = TARGET.with(|t| t.get());
        // SAFETY: the test sets TARGET to the container whose payload `this` points into, alive during the call.
        let s = unsafe { &mut *s };
        assert_eq!(this as usize, s.addr() + REFUNDABLE_OFFSET, "called on the Context's container");
        let mut node = first;
        while node != last {
            // SAFETY: the source range is a `Chain`, alive during the call.
            let w = unsafe { core::slice::from_raw_parts(node as *const u64, 5) };
            let e = RefundEntry {
                entity: w[2] as u32,
                key: (w[2] >> 32) as u32 as i32,
                revision: [w[3] as u32 as i32, (w[3] >> 32) as u32 as i32, w[4] as u32 as i32],
                value: (w[4] >> 32) as u32 as i32,
            };
            let (have, _) = read_container(&LiveMem, s.addr()).expect("a container");
            if !have.iter().any(|h| h.entity == e.entity && h.key == e.key) {
                s.append(e);
            }
            node = w[0] as *const u8;
        }
    }

    pub fn fake_fns(target: &mut Synthetic) -> RefundFns {
        TARGET.with(|t| t.set(target));
        RefundFns { insert: fake_insert }
    }

    pub fn calls() -> usize {
        CALLS.with(|c| c.get())
    }

    pub fn entry(entity: u32, rev: i32, value: i32) -> RefundEntry {
        RefundEntry { entity, key: -1, revision: [rev, 11, 0], value }
    }

    #[test]
    fn the_source_range_is_laid_out_as_the_engines_list_nodes() {
        let e = [entry(0x1819f, 1, 0x153), entry(7, 3, 2895)];
        let c = Chain::new(&e);
        assert_eq!(c.end() - c.first(), 2 * REFUNDABLE_NODE);
        // the words of a real entry (friend test 2026-10-16 native_inject_4.txt): w:ffffffff0001819f w:0000000b00000001 w:0000015300000000
        let n0 = unsafe { core::slice::from_raw_parts(c.first() as *const u64, 5) };
        assert_eq!(
            (n0[0] as usize, n0[2], n0[3], n0[4]),
            (c.first() + REFUNDABLE_NODE, 0xffff_ffff_0001_819f, 0x0000_000b_0000_0001, 0x0000_0153_0000_0000)
        );
        let n1 = unsafe { core::slice::from_raw_parts((c.first() + REFUNDABLE_NODE) as *const u64, 5) };
        assert_eq!(n1[0] as usize, c.end(), "the last entry leads to the end node");
        assert!(c.holds(c.end()) && !c.holds(c.end() + REFUNDABLE_NODE));
        let empty = Chain::new(&[]);
        assert_eq!(empty.first(), empty.end(), "no entry: the range is empty");
    }

    #[test]
    fn the_engines_insert_fills_an_empty_container_and_the_check_reads_every_entry_back() {
        let want = [entry(50215, 3, 3143), entry(46947, 11, 2899)];
        let mut s = Synthetic::new(&[]);
        let fns = fake_fns(&mut s);
        let p = plan(&LiveMem, s.addr(), &want).expect("an engine-made container");
        assert_eq!((p.before, p.missing, p.differ), (0, 2, 0));
        let c0 = calls();
        unsafe { insert(&fns, s.addr(), &p) };
        assert_eq!(calls() - c0, 1, "one call of the engine's insert");
        let done = check(&LiveMem, s.addr(), &want, &p).expect("every entry is there");
        assert_eq!(done, Done { before: 0, set: 2, differ: 0, after: 2 });
        let (have, addrs) = read_container(&LiveMem, s.addr()).unwrap();
        assert_eq!(have, want.to_vec(), "the entries with their key integer, revision and value integer");
        assert!(addrs.iter().all(|&a| !p.chain.holds(a)), "the container's nodes are its own, never the source's");
        drop(p); // the source range is freed by Rust; the container (the command) keeps its nodes
        assert_eq!(read_container(&LiveMem, s.addr()).unwrap().0, want.to_vec());
    }

    #[test]
    fn entries_already_in_the_container_are_not_inserted_twice() {
        // the script's conversion of the mod's Context may already carry some (the typed map): unique keys, nothing doubled
        let want = [entry(5, 1, 10), entry(6, 1, 20)];
        let mut s = Synthetic::new(&[entry(5, 1, 10), RefundEntry { value: 99, ..entry(6, 1, 20) }]);
        let fns = fake_fns(&mut s);
        let p = plan(&LiveMem, s.addr(), &want).unwrap();
        assert_eq!((p.before, p.missing, p.differ), (2, 0, 1), "the second key is there with another value");
        let c0 = calls();
        unsafe { insert(&fns, s.addr(), &p) };
        assert_eq!(calls(), c0, "nothing missing: the engine is not called");
        assert_eq!(check(&LiveMem, s.addr(), &want, &p).unwrap(), Done { before: 2, set: 0, differ: 1, after: 2 });
        let mut s = Synthetic::new(&[entry(5, 1, 10)]);
        let fns = fake_fns(&mut s);
        let p = plan(&LiveMem, s.addr(), &want).unwrap();
        unsafe { insert(&fns, s.addr(), &p) };
        assert_eq!(check(&LiveMem, s.addr(), &want, &p).unwrap(), Done { before: 1, set: 1, differ: 0, after: 2 });
    }

    #[test]
    fn a_container_that_is_not_engine_made_is_refused_before_any_call() {
        let want = [entry(5, 1, 10)];
        let zero = Box::new([0u8; 0x9c0]);
        let refused = |addr: usize| plan(&LiveMem, addr, &want).err().unwrap_or_default();
        assert!(refused(zero.as_ptr() as usize).contains("no load factor"));
        let mut s = Synthetic::new(&[]);
        let c = REFUNDABLE_OFFSET + 0x38;
        s.payload[c..c + 8].copy_from_slice(&6u64.to_le_bytes());
        assert!(refused(s.addr()).contains("bucket table"));
        let mut s = Synthetic::new(&[entry(1, 1, 1)]);
        let c = REFUNDABLE_OFFSET + 0x10;
        s.payload[c..c + 8].copy_from_slice(&2u64.to_le_bytes());
        assert!(refused(s.addr()).contains("ends before its size"));
    }

    #[test]
    fn an_insert_that_did_not_take_every_entry_is_an_error() {
        unsafe extern "system-unwind" fn lazy(_this: *mut u8, _first: *const u8, _last: *const u8) {}
        let want = [entry(5, 1, 10)];
        let s = Synthetic::new(&[]);
        let p = plan(&LiveMem, s.addr(), &want).unwrap();
        unsafe { insert(&RefundFns { insert: lazy }, s.addr(), &p) };
        assert!(check(&LiveMem, s.addr(), &want, &p).unwrap_err().contains("0 entries, 1 expected"));
    }

    #[test]
    fn the_refund_window_is_read_from_the_text_and_written_into_the_context() {
        // the bulldozer's own command (held text, 2026-10-18 live/native_text_22.txt: `~0x363 raw:b:c4000070420000403f`) has 60.0
        let text =
            "context.cleanupStreetGraph\tu8:01\ncontext.refundWindow\tf32:42700000\ncontext.player\tent:old#34135\n";
        assert_eq!(window_of(text).unwrap(), Some(60.0));
        assert_eq!(window_of("context.player\tent:old#1\n").unwrap(), None, "an older text: the command keeps its own");
        assert_eq!(window_of("context.refundWindow\tf32:00000000\n").unwrap(), Some(0.0));
        for bad in ["f32:7fc00000", "f32:bf800000", "f32:7f800000", "u8:01", "f32:zz"] {
            assert!(window_of(&format!("context.refundWindow\t{bad}\n")).is_err(), "{bad} is refused");
        }
        // the write lands at Context+0xc of the payload and nowhere else
        struct Raw;
        impl Poke for Raw {
            fn poke(&self, addr: usize, data: &[u8]) -> bool {
                // SAFETY: the test pokes only into its own synthetic payload.
                unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
                true
            }
        }
        let mut s = Synthetic::new(&[entry(5, 1, 10)]);
        let before = s.payload.to_vec();
        set_window(&Raw, s.payload.as_mut_ptr() as usize, 60.0).unwrap();
        assert_eq!(&s.payload[WINDOW_OFFSET..WINDOW_OFFSET + 4], &60.0f32.to_le_bytes());
        let changed: Vec<usize> = (0..before.len()).filter(|&i| before[i] != s.payload[i]).collect();
        assert_eq!(changed, vec![WINDOW_OFFSET + 2, WINDOW_OFFSET + 3], "only the float's bytes (60.0 = 00 00 70 42)");
        assert_eq!(read_container(&LiveMem, s.addr()).unwrap().0, vec![entry(5, 1, 10)], "the list is untouched");
        struct Refuses;
        impl Poke for Refuses {
            fn poke(&self, _: usize, _: &[u8]) -> bool {
                false
            }
        }
        assert!(set_window(&Refuses, s.addr(), 60.0).is_err());
    }

    #[test]
    fn the_side_file_is_read_exactly() {
        let t = "50215\t-1\t3\t11\t0\t3143\n46947\t-1\t11\t11\t0\t2899\n";
        assert_eq!(parse_file(t, 2).unwrap(), vec![entry(50215, 3, 3143), entry(46947, 11, 2899)]);
        assert!(parse_file(t, 3).unwrap_err().contains("has 2 entries, the order says 3"));
        assert!(parse_file("1\t-1\t1\t11\t0\n", 1).unwrap_err().contains("is not entity"));
        assert!(parse_file("x\t-1\t1\t11\t0\t1\n", 1).is_err());
        assert!(parse_file("-1\t-1\t1\t11\t0\t1\n", 1).is_err(), "an entity id is not negative");
        assert!(parse_file("5\t-1\t1\t11\t0\t1\n5\t-1\t2\t11\t0\t1\n", 2).unwrap_err().contains("twice"));
        assert_eq!(text(&[entry(5, 3, 7)]), "5{-1}rev3,11,0/7");
    }
}
