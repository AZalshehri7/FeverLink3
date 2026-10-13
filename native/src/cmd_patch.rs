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

use crate::dump::{pointer_like, u32_at, u64_at, Mem, PAYLOAD_SIZE};
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

impl Order {
    /// How many nodes the order freezes.
    pub fn count(&self) -> usize {
        if self.at.is_empty() {
            self.frozen.len()
        } else {
            self.at.len()
        }
    }
}

/// One armed order.
#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    pub frozen: Vec<u32>,
    pub adds: u32,
    /// Positions of the nodes to freeze; when given they replace `frozen` and `adds` (resolved against the converted proposal).
    pub at: Vec<[f64; 3]>,
    pub armed_ms: u64,
    pub ttl_ms: u64,
}

/// The module's side: whether the switch is on, the armed order, the events for the mod.
#[derive(Default)]
pub struct State {
    on: AtomicBool,
    pending: AtomicBool,
    order: Mutex<Option<Order>>,
    /// Diagnostics for the armed order: Adds seen since it was armed (all, build_proposal, from a tool's site), and whether a skip was reported.
    seen: Mutex<Seen>,
    /// The last Adds of any kind, armed or not (so that an order that arrives late can be compared with what went before).
    trail: Mutex<Trail>,
    events: Mutex<Vec<Event>>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Seen {
    adds: u64,
    build_proposals: u64,
    from_tools: u64,
    skip_reported: bool,
    /// Converted proposals patched while the order was armed (`scripting::Convert`), and the events sent about conversions.
    converted: u64,
    convert_events: u64,
    /// Adds since arming by kind byte (`None` = unreadable).
    kinds: std::collections::BTreeMap<Option<u8>, u64>,
    /// The last few Adds since arming, as text.
    last: std::collections::VecDeque<String>,
    /// Shapes of the conversions already dumped for this order.
    shapes: std::collections::BTreeSet<String>,
    /// What went before the order: the build_proposal gap and the Adds just ahead of it.
    before: String,
}

/// One Add, as the trail remembers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddRec {
    pub kind: Option<u8>,
    pub rva: Option<u32>,
    pub ms: u64,
    pub held: bool,
}

impl AddRec {
    fn text(&self, rel_to: u64) -> String {
        let k = self.kind.map_or("?".to_string(), |k| k.to_string());
        let r = self.rva.map_or("-".to_string(), |r| format!("{r:x}"));
        let dt = self.ms as i64 - rel_to as i64;
        format!("k{k}@{r} {dt}ms{}", if self.held { " held" } else { "" })
    }
}

/// The most events one order's conversions may send (every conversion is in `native.log`).
pub const MAX_CONVERT_EVENTS: u64 = 16;

const TRAIL_LEN: usize = 64;

#[derive(Debug, Default)]
struct Trail {
    ring: std::collections::VecDeque<AddRec>,
    last_bp: Option<AddRec>,
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
    pub fn arm(&self, frozen: Vec<u32>, adds: u32, at: Vec<[f64; 3]>, ttl: u64, now: u64) {
        if !self.enabled() {
            self.refuse("FEVERLINK3_NATIVE_PATCH is off");
            return;
        }
        let ttl_ms = if ttl == 0 { DEFAULT_TTL_MS } else { ttl.min(MAX_TTL_MS) };
        let before = self.before_text(now);
        *lock(&self.seen) = Seen { before, ..Seen::default() };
        let old = lock(&self.order).replace(Order { frozen, adds, at, armed_ms: now, ttl_ms });
        if old.is_some() {
            self.refuse("replaced by a newer patch_next before a command used it");
        }
        self.pending.store(true, Ordering::Release);
    }

    /// Remembers an Add of any kind (called for every Add while the switch is on).
    pub fn note_add(&self, rec: AddRec, build_proposal: bool) {
        let mut t = lock(&self.trail);
        if t.ring.len() == TRAIL_LEN {
            t.ring.pop_front();
        }
        t.ring.push_back(rec);
        if build_proposal {
            t.last_bp = Some(rec);
        }
    }

    /// Milliseconds from the last build_proposal Add to `now` (for the log line at arming).
    pub fn ms_since_build_proposal(&self, now: u64) -> Option<u64> {
        lock(&self.trail).last_bp.map(|r| now.saturating_sub(r.ms))
    }

    fn before_text(&self, now: u64) -> String {
        let t = lock(&self.trail);
        let bp = t
            .last_bp
            .map_or("no build_proposal Add yet".to_string(), |r| format!("last build_proposal Add {}", r.text(now)));
        let recent: Vec<String> = t
            .ring
            .iter()
            .rev()
            .take(2000)
            .filter(|r| now.saturating_sub(r.ms) <= 2000)
            .take(6)
            .map(|r| r.text(now))
            .collect();
        format!("at arming: {bp}; Adds in the 2 s before (newest first): [{}]", recent.join(", "))
    }

    /// An `Add` went by while an order is armed (for the diagnostics of an expiry).
    pub fn saw_add(&self, rec: AddRec, build_proposal: bool, from_tool: bool, armed_ms: u64) {
        let mut g = lock(&self.seen);
        g.adds += 1;
        g.build_proposals += u64::from(build_proposal);
        g.from_tools += u64::from(from_tool);
        *g.kinds.entry(rec.kind).or_insert(0) += 1;
        if g.last.len() == 5 {
            g.last.pop_front();
        }
        g.last.push_back(rec.text(armed_ms));
    }

    /// When the armed order was armed.
    pub fn armed_ms(&self) -> Option<u64> {
        lock(&self.order).as_ref().map(|o| o.armed_ms)
    }

    /// `Some(reason)` the first time it is asked for the armed order (a skip is reported once per order, not once per command).
    pub fn skip_once(&self, reason: &str) -> Option<String> {
        let mut g = lock(&self.seen);
        if g.skip_reported {
            return None;
        }
        g.skip_reported = true;
        Some(reason.to_string())
    }

    /// An order nobody's `Add` took: a refusal, unless conversions were patched (then the proposal was handled and what is missing is
    /// the command's arrival at the command list, which is reported as a skip with the trail).
    fn expire(&self, ttl_ms: u64) {
        let converted = self.converted();
        if converted == 0 {
            self.refuse(self.expired_reason(ttl_ms));
        } else {
            let why = self.expired_reason(ttl_ms).replacen("expired: no command arrived", "expired after patching", 1);
            lock(&self.events).push(Event::PatchSkipped {
                reason: format!(
                    "{why}; {converted} converted proposal(s) were patched, but no build_proposal Add used the order"
                ),
                kind: None,
                site: None,
            });
        }
    }

    fn expired_reason(&self, ttl_ms: u64) -> String {
        let g = lock(&self.seen).clone();
        let kinds: Vec<String> =
            g.kinds.iter().map(|(k, n)| format!("{}x{n}", k.map_or("?".to_string(), |k| k.to_string()))).collect();
        format!(
            "expired: no command arrived within {ttl_ms} ms (since it was armed: {} Add(s), {} build_proposal, {} from construction tools; kinds [{}]; last [{}]; {})",
            g.adds,
            g.build_proposals,
            g.from_tools,
            kinds.join(" "),
            g.last.iter().cloned().collect::<Vec<_>>().join(", "),
            g.before
        )
    }

    /// The armed order without taking it (the conversion hook patches every matching conversion while it waits): `None` when there
    /// is none or it has expired (the service thread reports an expiry).
    pub fn peek(&self, now: u64) -> Option<Order> {
        lock(&self.order).as_ref().filter(|o| now.saturating_sub(o.armed_ms) <= o.ttl_ms).cloned()
    }

    /// True the first time a conversion of this `shape` is seen while the order is armed.
    pub fn new_shape(&self, shape: &str) -> bool {
        lock(&self.seen).shapes.insert(shape.to_string())
    }

    /// A converted proposal was patched.
    pub fn converted_one(&self) {
        lock(&self.seen).converted += 1;
    }

    /// How many converted proposals the armed order patched.
    pub fn converted(&self) -> u64 {
        lock(&self.seen).converted
    }

    /// An event about a conversion, at most [`MAX_CONVERT_EVENTS`] per order (the log has every call).
    pub fn convert_event(&self, event: Event) {
        let mut g = lock(&self.seen);
        if g.convert_events < MAX_CONVERT_EVENTS {
            g.convert_events += 1;
            drop(g);
            self.report(event);
        }
    }

    /// Drops the armed order without a verdict (a build_proposal Add passed after the conversion patched it).
    pub fn clear(&self) {
        lock(&self.order).take();
        self.pending.store(false, Ordering::Release);
    }

    /// The armed order, once: `None` when there is none or it has expired (which is reported).
    pub fn take(&self, now: u64) -> Option<Order> {
        let order = lock(&self.order).take();
        self.pending.store(false, Ordering::Release);
        let order = order?;
        if now.saturating_sub(order.armed_ms) > order.ttl_ms {
            self.expire(order.ttl_ms);
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
                self.expire(o.ttl_ms);
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
    /// The indices written (those of the order, or the ones its positions resolved to).
    pub frozen: Vec<u32>,
    /// The number of added nodes the proposal had.
    pub nodes: usize,
}

/// Matches one position to a node of `addedNodes` (3-D distance at most this many metres).
pub const AT_TOLERANCE_M: f64 = 0.05;

/// The index of the one added node at each position. A position that matches none: this is not the proposal (`NotThis`); a
/// position that matches several: the order cannot be carried out (`Refuse`).
fn resolve_positions(mem: &dyn Mem, nodes_at: usize, nodes: usize, at: &[[f64; 3]]) -> Result<Vec<u32>, Fail> {
    let raw = if nodes == 0 {
        Vec::new()
    } else {
        mem.read(nodes_at, nodes * NODE_STRIDE)
            .ok_or_else(|| Fail::NotThis("proposal.addedNodes is not readable".into()))?
    };
    let pos = |i: usize| -> [f64; 3] {
        let o = i * NODE_STRIDE + 4;
        [0, 4, 8].map(|d| f64::from(f32::from_bits(u32_at(&raw, o + d))))
    };
    let mut out = Vec::with_capacity(at.len());
    for (k, want) in at.iter().enumerate() {
        let hits: Vec<usize> = (0..nodes)
            .filter(|&i| {
                let p = pos(i);
                let d2: f64 = (0..3).map(|j| (p[j] - want[j]).powi(2)).sum();
                d2 <= AT_TOLERANCE_M * AT_TOLERANCE_M
            })
            .collect();
        match hits.as_slice() {
            [] => {
                return Err(Fail::NotThis(format!(
                    "no added node (of {nodes}) lies within {AT_TOLERANCE_M} m of at[{}] ({:.2}, {:.2}, {:.2})",
                    k + 1,
                    want[0],
                    want[1],
                    want[2]
                )))
            }
            [i] => out.push(*i as u32),
            more => {
                return Err(Fail::Refuse(format!(
                    "at[{}] ({:.2}, {:.2}, {:.2}) matches {} added nodes within {AT_TOLERANCE_M} m ({:?}): not unique",
                    k + 1,
                    want[0],
                    want[1],
                    want[2],
                    more.len(),
                    more
                )))
            }
        }
    }
    Ok(out)
}

/// One list of u32 about to be written.
struct Plan {
    /// Where the vector's three pointers are.
    triple: usize,
    /// Where the values go (an existing block with room, or a new one).
    at: usize,
    fresh: bool,
}

fn triple(bytes: &[u8], off: usize) -> (u64, u64, u64) {
    (u64_at(bytes, off), u64_at(bytes, off + 8), u64_at(bytes, off + 16))
}

/// Why a patch was not carried out.
#[derive(Debug, PartialEq, Eq)]
pub enum Fail {
    /// The order does not describe this proposal (another number of added nodes, no construction, not the expected layout): at
    /// the conversion this is a different build than the order's and the order keeps waiting.
    NotThis(String),
    /// The order cannot be carried out at all (bad order, allocator, a write failed).
    Refuse(String),
}

impl Fail {
    pub fn into_reason(self) -> String {
        match self {
            Fail::NotThis(r) | Fail::Refuse(r) => r,
        }
    }
}

/// Size of a `Proposal` (the part of a command's payload before the kind byte's neighbours): what [`apply_proposal`] reads.
pub const PROPOSAL_SIZE: usize = 0x358;

/// The checks and the writes on a command's payload. Reads `payload` through `mem`, takes a block from `alloc` for an empty list,
/// writes through `poke`. Nothing is written (and nothing stays allocated) unless every check passes.
pub fn apply(mem: &dyn Mem, poke: &dyn Poke, alloc: &dyn Alloc, payload: usize, order: &Order) -> Result<Done, String> {
    if order.count() == 0 || order.count() > MAX_PATCH_FROZEN {
        return Err(format!("{} frozen indices", order.count()));
    }
    let bytes = mem.read(payload, PAYLOAD_SIZE).ok_or("the command's payload is not readable")?;
    if bytes[KIND_OFFSET] != KIND_BUILD_PROPOSAL {
        return Err(format!("the command is kind {}, not build_proposal", bytes[KIND_OFFSET]));
    }
    apply_bytes(mem, poke, alloc, payload, &bytes, order, false).map_err(Fail::into_reason)
}

/// The number of added nodes of the proposal at `proposal`, when its `addedNodes` is a vector (for the log).
pub fn added_nodes(mem: &dyn Mem, proposal: usize) -> Option<usize> {
    let bytes = mem.read(proposal + ADDED_NODES, 24)?;
    let (b, e, c) = triple(&bytes, 0);
    if b == 0 && e == 0 && c == 0 {
        Some(0)
    } else if pointer_like(b) && b <= e && e <= c && ((e - b) as usize).is_multiple_of(NODE_STRIDE) {
        Some((e - b) as usize / NODE_STRIDE)
    } else {
        None
    }
}

/// Allocator and writer that give nothing: [`describes`] runs every check against them and writes nothing.
struct Nothing;

impl Alloc for Nothing {
    fn alloc(&self, _: usize) -> Option<usize> {
        None
    }
    fn write(&self, _: usize, _: &[u8]) -> bool {
        false
    }
}

impl Poke for Nothing {
    fn poke(&self, _: usize, _: &[u8]) -> bool {
        false
    }
}

/// Whether `order` describes the proposal at `proposal` (every check of [`apply_proposal`], no write, no allocation).
pub fn describes(mem: &dyn Mem, proposal: usize, order: &Order) -> Result<(), Fail> {
    match apply_proposal(mem, &Nothing, &Nothing, proposal, order) {
        Err(Fail::Refuse(r)) if r.starts_with("at[") => Err(Fail::Refuse(r)), // not unique: that is the order's answer
        Err(Fail::Refuse(_)) if order.count() > 0 && order.count() <= MAX_PATCH_FROZEN => Ok(()), // only the writing failed
        Err(f) => Err(f),
        Ok(_) => Ok(()),
    }
}

const ADDED_SEGMENTS: usize = 0x018;
const SEGMENT_STRIDE: usize = 0x350;
/// The most nodes / segments / indices a dump lists.
const DUMP_LIMIT: usize = 24;

/// A vector of `stride`-byte elements at `off` of the proposal bytes: (begin, count), `None` when it is not one.
fn vector_at(bytes: &[u8], off: usize, stride: usize) -> Option<(usize, usize)> {
    let (b, e, c) = triple(bytes, off);
    if b == 0 && e == 0 && c == 0 {
        return Some((0, 0));
    }
    (pointer_like(b) && b <= e && e <= c && ((e - b) as usize).is_multiple_of(stride))
        .then(|| (b as usize, (e - b) as usize / stride))
}

/// A list of u32 (`frozenNodes`) as text.
fn u32_list(mem: &dyn Mem, bytes: &[u8], off: usize) -> String {
    let (b, e, c) = triple(bytes, off);
    if b == 0 && e == 0 && c == 0 {
        return "empty".into();
    }
    if !(pointer_like(b) && b % 4 == 0 && b <= e && e <= c && (e - b) % 4 == 0) {
        return "not a vector".into();
    }
    let n = ((e - b) / 4) as usize;
    match mem.read(b as usize, 4 * n.min(DUMP_LIMIT)) {
        Some(v) => format!(
            "{:?}{}",
            v.chunks(4).map(|x| u32_at(x, 0)).collect::<Vec<_>>(),
            if n > DUMP_LIMIT { "+" } else { "" }
        ),
        None => "unreadable".into(),
    }
}

/// What a conversion produced, for the log and the `convert_dump` event: (a key that is equal for equal shapes, the text).
/// Read-only. `flag` is the "has a value" byte of the `optional<Proposal>`.
pub fn dump_proposal(mem: &dyn Mem, proposal: usize, flag: Option<u8>) -> (String, String) {
    let flag_text = match flag {
        Some(0) => "NO VALUE (Convert failed)".to_string(),
        Some(_) => "has value".to_string(),
        None => "flag unreadable".to_string(),
    };
    let Some(bytes) = mem.read(proposal, PROPOSAL_SIZE) else {
        let t = format!("{flag_text}; the proposal is not readable");
        return (t.clone(), t);
    };
    let mut text = flag_text;
    let mut key = text.clone();
    let mut ids = Vec::new();
    match vector_at(&bytes, ADDED_NODES, NODE_STRIDE) {
        None => text.push_str("; addedNodes: not a vector"),
        Some((at, n)) => {
            text.push_str(&format!("; addedNodes {n}:"));
            key.push_str(&format!("|nodes {n}"));
            let raw = if n == 0 { Some(Vec::new()) } else { mem.read(at, n.min(DUMP_LIMIT) * NODE_STRIDE) };
            if let Some(raw) = &raw {
                for i in 0..n.min(DUMP_LIMIT) {
                    let o = i * NODE_STRIDE;
                    let id = u32_at(raw, o) as i32;
                    ids.push(id);
                    let p = [4, 8, 12].map(|d| f32::from_bits(u32_at(raw, o + d)));
                    text.push_str(&format!(" [{i}] id {id} ({:.3}, {:.3}, {:.3})", p[0], p[1], p[2]));
                }
            }
        }
    }
    match vector_at(&bytes, ADDED_SEGMENTS, SEGMENT_STRIDE) {
        None => text.push_str("; addedSegments: not a vector"),
        Some((at, n)) => {
            text.push_str(&format!("; addedSegments {n}:"));
            key.push_str(&format!("|segments {n}"));
            let raw = if n == 0 { Some(Vec::new()) } else { mem.read(at, n.min(DUMP_LIMIT) * SEGMENT_STRIDE) };
            let node =
                |id: i32| ids.iter().position(|&x| x == id).map_or(format!("ent {id}"), |i| format!("node[{i}]"));
            if let Some(raw) = &raw {
                for i in 0..n.min(DUMP_LIMIT) {
                    let o = i * SEGMENT_STRIDE;
                    text.push_str(&format!(
                        " [{i}] id {} {} -> {} baseType {} typeIndex {} roadType {} type {}",
                        u32_at(raw, o) as i32,
                        node(u32_at(raw, o + 8) as i32),
                        node(u32_at(raw, o + 0xc) as i32),
                        u32_at(raw, o + 0x40),
                        u32_at(raw, o + 0x44) as i32,
                        u32_at(raw, o + 0x98),
                        u32_at(raw, o + 0x120),
                    ));
                }
            }
        }
    }
    let own = u32_list(mem, &bytes, PROPOSAL_FROZEN);
    text.push_str(&format!("; proposal.frozenNodes {own}"));
    key.push_str(&format!("|frozen {own}"));
    match vector_at(&bytes, TO_ADD, CONSTRUCTION_STRIDE) {
        None => text.push_str("; toAdd: not a vector"),
        Some((_, 0)) => {
            text.push_str("; toAdd: none");
            key.push_str("|toAdd 0");
        }
        Some((at, n)) => match mem.read(at + CONSTRUCTION_FROZEN, 24) {
            Some(t) => {
                let l = u32_list(mem, &t, 0);
                text.push_str(&format!("; toAdd {n}, toAdd[0].frozenNodes {l}"));
                key.push_str(&format!("|toAdd {n} {l}"));
            }
            None => text.push_str(&format!("; toAdd {n}, toAdd[0].frozenNodes unreadable")),
        },
    }
    (key, text)
}

/// The same on a bare `Proposal` (the result of `scripting::Convert`), which has no kind byte.
pub fn apply_proposal(
    mem: &dyn Mem,
    poke: &dyn Poke,
    alloc: &dyn Alloc,
    proposal: usize,
    order: &Order,
) -> Result<Done, Fail> {
    if order.count() == 0 || order.count() > MAX_PATCH_FROZEN {
        return Err(Fail::Refuse(format!("{} frozen indices", order.count())));
    }
    let bytes = mem.read(proposal, PROPOSAL_SIZE).ok_or(Fail::NotThis("the proposal is not readable".into()))?;
    apply_bytes(mem, poke, alloc, proposal, &bytes, order, false)
}

/// Only `proposal.frozenNodes` of a proposal that is still being built (the input of the construction update: there is no
/// construction in it yet, and `toAdd[0].frozenNodes` does not exist).
pub fn apply_proposal_frozen_only(
    mem: &dyn Mem,
    poke: &dyn Poke,
    alloc: &dyn Alloc,
    proposal: usize,
    order: &Order,
) -> Result<Done, Fail> {
    if order.count() == 0 || order.count() > MAX_PATCH_FROZEN {
        return Err(Fail::Refuse(format!("{} frozen indices", order.count())));
    }
    let bytes = mem.read(proposal, PROPOSAL_SIZE).ok_or(Fail::NotThis("the proposal is not readable".into()))?;
    apply_bytes(mem, poke, alloc, proposal, &bytes, order, true)
}

/// [`describes`] for [`apply_proposal_frozen_only`].
pub fn describes_frozen_only(mem: &dyn Mem, proposal: usize, order: &Order) -> Result<(), Fail> {
    match apply_proposal_frozen_only(mem, &Nothing, &Nothing, proposal, order) {
        Err(Fail::Refuse(r)) if r.starts_with("at[") => Err(Fail::Refuse(r)),
        Err(Fail::Refuse(_)) if order.count() > 0 && order.count() <= MAX_PATCH_FROZEN => Ok(()),
        Err(f) => Err(f),
        Ok(_) => Ok(()),
    }
}

fn apply_bytes(
    mem: &dyn Mem,
    poke: &dyn Poke,
    alloc: &dyn Alloc,
    payload: usize,
    bytes: &[u8],
    order: &Order,
    frozen_only: bool,
) -> Result<Done, Fail> {
    let not_this = |s: String| Fail::NotThis(s);
    let (b, e, c) = triple(bytes, ADDED_NODES);
    let nodes = if b == 0 && e == 0 && c == 0 {
        0
    } else if pointer_like(b) && b <= e && e <= c && ((e - b) as usize).is_multiple_of(NODE_STRIDE) {
        (e - b) as usize / NODE_STRIDE
    } else {
        return Err(not_this("proposal.addedNodes is not a vector".into()));
    };
    let frozen: Vec<u32> = if order.at.is_empty() {
        if nodes != order.adds as usize {
            return Err(not_this(format!("the command has {nodes} added nodes, the order says {}", order.adds)));
        }
        if let Some(i) = order.frozen.iter().find(|&&i| i as usize >= nodes) {
            return Err(not_this(format!("frozen index {i} is not below the {nodes} added nodes")));
        }
        order.frozen.clone()
    } else {
        resolve_positions(mem, b as usize, nodes, &order.at)?
    };
    let n = frozen.len();
    // the constructions (not for a proposal still being built)
    let (tb, te, tc) = triple(bytes, TO_ADD);
    if !frozen_only {
        if tb == 0 && te == 0 && tc == 0 {
            return Err(not_this("the command adds no construction".into()));
        }
        if !(pointer_like(tb) && tb <= te && te <= tc && ((te - tb) as usize).is_multiple_of(CONSTRUCTION_STRIDE)) {
            return Err(not_this("proposal toAdd is not a vector".into()));
        }
        if te == tb {
            return Err(not_this("the command adds no construction".into()));
        }
    }
    let first = tb as usize;
    // the two lists, as they are now
    let mut lists = Vec::new();
    for i in 0..(if frozen_only { 1 } else { 2 }) {
        let (addr, raw) = if i == 0 {
            (payload + PROPOSAL_FROZEN, triple(bytes, PROPOSAL_FROZEN))
        } else {
            let a = first + CONSTRUCTION_FROZEN;
            let t = mem.read(a, 24).ok_or_else(|| not_this("toAdd[0] is not readable".into()))?;
            (a, triple(&t, 0))
        };
        let (b, e, c) = raw;
        let empty = b == 0 && e == 0 && c == 0;
        if !empty && !(pointer_like(b) && b % 4 == 0 && b <= e && e <= c && (e - b) % 4 == 0) {
            return Err(not_this(format!(
                "{} is not a vector of 4-byte values",
                if i == 0 { "proposal.frozenNodes" } else { "toAdd[0].frozenNodes" }
            )));
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
        return Err(Fail::Refuse(why.into()));
    }
    // the values go in first, the pointers last: a failure in between leaves a list that still says what it said
    let mut data = Vec::with_capacity(4 * n);
    for v in &frozen {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let mut ok = true;
    for p in &plans {
        ok &= if p.fresh { alloc.write(p.at, &data) } else { poke.poke(p.at, &data) };
    }
    if !ok {
        alloc.free_all();
        return Err(Fail::Refuse("a list could not be written".into()));
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
            return Err(Fail::Refuse("a list's pointers could not be written".into()));
        }
    }
    let allocated = plans.iter().any(|p| p.fresh);
    alloc.forget_all(); // the command's Proposal owns the blocks now
    Ok(Done { allocated, frozen, nodes })
}

#[cfg(test)]
mod tests {
    use super::*;
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
        Order { frozen: frozen.to_vec(), adds, at: vec![], armed_ms: 0, ttl_ms: 1000 }
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
        assert!(done.allocated);
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
        assert!(!done.allocated);
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
        s.arm(vec![1], 3, vec![], 0, 100);
        assert!(!s.pending(), "off: refused");
        assert!(matches!(s.drain_events(100).as_slice(), [Event::PatchRefused { reason }] if reason.contains("off")));
        s.enable();
        s.arm(vec![1], 3, vec![], 0, 100);
        assert!(s.pending());
        assert!(s.take(100 + DEFAULT_TTL_MS + 1).is_none());
        assert!(matches!(s.drain_events(0).as_slice(), [Event::PatchRefused { reason }] if reason.contains("expired")));
        s.arm(vec![1], 3, vec![], 50, 100);
        assert_eq!(s.take(120), Some(Order { frozen: vec![1], adds: 3, at: vec![], armed_ms: 100, ttl_ms: 50 }));
        assert!(!s.pending());
        // a newer order replaces; an unused one expires on the service thread
        s.arm(vec![1], 3, vec![], 10, 0);
        s.arm(vec![0], 2, vec![], 10, 5);
        let ev = s.drain_events(6);
        assert!(matches!(ev.as_slice(), [Event::PatchRefused { reason }] if reason.contains("replaced")));
        assert!(
            matches!(s.drain_events(100).as_slice(), [Event::PatchRefused { reason }] if reason.contains("expired"))
        );
        assert!(!s.pending());
    }

    #[test]
    fn a_bare_proposal_is_patched_like_a_payload_and_needs_no_kind_byte() {
        let (a, p) = world(3, 1, None, None);
        assert!(a.write(p + KIND_OFFSET, &[0]));
        assert!(apply(&a, &a, &a, p, &order(&[1], 3)).is_err(), "as a command it is not a build_proposal");
        let done = apply_proposal(&a, &a, &a, p, &order(&[1], 3)).expect("patched");
        assert!(done.allocated);
        let (x, y) = lists(&a, p);
        assert_eq!((x.0, y.0), (vec![1], vec![1]));
        assert_eq!(added_nodes(&a, p), Some(3));
    }

    #[test]
    fn a_proposal_the_order_does_not_describe_is_not_this_one_and_stays_untouched() {
        for (nodes, cons, o) in [(3, 1, order(&[0], 2)), (2, 0, order(&[0], 2)), (3, 1, order(&[5], 3))] {
            let (a, p) = world(nodes, cons, None, None);
            let before = a.read(p, PROPOSAL_SIZE).unwrap();
            let r = apply_proposal(&a, &a, &a, p, &o);
            assert!(matches!(r, Err(Fail::NotThis(_))), "{nodes} {cons} {r:?}");
            assert_eq!(a.read(p, PROPOSAL_SIZE).unwrap(), before);
        }
        let (a, p) = world(2, 1, None, None);
        assert!(matches!(apply_proposal(&a, &a, &a, p, &order(&[], 2)), Err(Fail::Refuse(_))));
    }

    #[test]
    fn an_order_patched_by_conversions_expires_as_a_skip_not_a_refusal() {
        let s = State::default();
        s.enable();
        s.arm(vec![1], 2, vec![], 0, 0);
        assert!(s.peek(5).is_some() && s.pending(), "peeking does not take");
        s.converted_one();
        assert!(s.peek(DEFAULT_TTL_MS + 1).is_none());
        let ev = s.drain_events(DEFAULT_TTL_MS + 1);
        assert!(
            matches!(ev.as_slice(), [Event::PatchSkipped { reason, .. }] if reason.contains("expired after patching") && reason.contains("1 converted")),
            "{ev:?}"
        );
    }

    /// Puts node `i` of the world's addedNodes at (x, y, z) with entity id `-(i+1)`.
    fn place(a: &Arena, p: usize, i: usize, x: f32, y: f32, z: f32) {
        let nb = u64_at(&a.read(p, PROPOSAL_SIZE).unwrap(), ADDED_NODES) as usize;
        let mut n = Vec::new();
        n.extend_from_slice(&(-(i as i32) - 1).to_le_bytes());
        for v in [x, y, z] {
            n.extend_from_slice(&v.to_le_bytes());
        }
        assert!(a.write(nb + i * NODE_STRIDE, &n));
    }

    fn at_order(at: &[[f64; 3]]) -> Order {
        Order { frozen: vec![], adds: 0, at: at.to_vec(), armed_ms: 0, ttl_ms: 1000 }
    }

    #[test]
    fn positions_resolve_to_the_one_added_node_there_and_adds_is_not_checked() {
        let (a, p) = world(4, 1, None, None);
        for (i, x) in [10.0f32, 20.0, 30.0, 40.0].into_iter().enumerate() {
            place(&a, p, i, x, 5.0, 1.0);
        }
        // 4 nodes where an index order would have said 2: no `adds` check with positions
        let done = apply_proposal(&a, &a, &a, p, &at_order(&[[30.03, 5.0, 1.0], [10.0, 5.0, 1.04]])).expect("patched");
        assert_eq!((done.frozen.clone(), done.nodes), (vec![2, 0], 4));
        let (x, y) = lists(&a, p);
        assert_eq!((x.0, y.0), (vec![2, 0], vec![2, 0]));
    }

    #[test]
    fn a_position_with_no_node_is_not_this_proposal_and_a_shared_one_is_refused() {
        let (a, p) = world(3, 1, None, None);
        place(&a, p, 0, 1.0, 1.0, 1.0);
        place(&a, p, 1, 2.0, 2.0, 2.0);
        place(&a, p, 2, 2.0, 2.0, 2.02);
        let before = a.read(p, PROPOSAL_SIZE).unwrap();
        let r = apply_proposal(&a, &a, &a, p, &at_order(&[[9.0, 9.0, 9.0]]));
        assert!(matches!(&r, Err(Fail::NotThis(w)) if w.contains("at[1]")), "{r:?}");
        let r = apply_proposal(&a, &a, &a, p, &at_order(&[[1.0, 1.0, 1.0], [2.0, 2.0, 2.01]]));
        assert!(matches!(&r, Err(Fail::Refuse(w)) if w.contains("at[2]") && w.contains("not unique")), "{r:?}");
        assert!(matches!(describes(&a, p, &at_order(&[[2.0, 2.0, 2.01]])), Err(Fail::Refuse(_))));
        assert_eq!(a.read(p, PROPOSAL_SIZE).unwrap(), before, "nothing was written");
        let r = apply_proposal(&a, &a, &a, p, &at_order(&[[1.0, 1.0, 1.06]]));
        assert!(matches!(r, Err(Fail::NotThis(_))), "0.06 m is outside the tolerance");
    }

    #[test]
    fn the_dump_lists_nodes_segments_and_what_the_engine_left_in_the_frozen_lists() {
        let (a, p) = world(2, 1, Some((8, 8)), None);
        place(&a, p, 0, 1.5, 2.5, 3.5);
        place(&a, p, 1, 4.0, 5.0, 6.0);
        let (key, text) = dump_proposal(&a, p, Some(1));
        assert!(
            text.starts_with(
                "has value; addedNodes 2: [0] id -1 (1.500, 2.500, 3.500) [1] id -2 (4.000, 5.000, 6.000)"
            ),
            "{text}"
        );
        assert!(
            text.contains("addedSegments 0")
                && text.contains("proposal.frozenNodes [")
                && text.contains("toAdd 1, toAdd[0].frozenNodes empty"),
            "{text}"
        );
        place(&a, p, 0, 9.0, 9.0, 9.0);
        assert_eq!(dump_proposal(&a, p, Some(1)).0, key, "positions are not part of the shape");
        assert!(dump_proposal(&a, p, Some(0)).1.starts_with("NO VALUE (Convert failed)"));
    }

    #[test]
    fn a_proposal_still_being_built_gets_only_its_own_list_and_needs_no_construction() {
        // no toAdd yet: the update has not run
        let (a, p) = world(2, 0, None, None);
        let done = apply_proposal_frozen_only(&a, &a, &a, p, &order(&[1], 2)).expect("patched");
        assert_eq!((done.frozen, done.nodes, done.allocated), (vec![1], 2, true));
        let own = list(&a, p + PROPOSAL_FROZEN);
        assert_eq!(own.0, vec![1]);
        // the full form still refuses the same proposal
        let (b, q) = world(2, 0, None, None);
        assert!(matches!(apply_proposal(&b, &b, &b, q, &order(&[1], 2)), Err(Fail::NotThis(_))));
        // the order's checks hold for the input too: the node count it names
        let (c, r) = world(4, 0, None, None);
        assert!(matches!(apply_proposal_frozen_only(&c, &c, &c, r, &order(&[1], 2)), Err(Fail::NotThis(_))));
        assert!(describes_frozen_only(&a, p, &order(&[1], 2)).is_ok());
    }
}
