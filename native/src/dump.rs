//! A dev-only dump of the engine's build command (`FEVERLINK3_DUMP_COMMANDS=1`), to learn the layout of the
//! proposal object behind a `build_proposal` command (kind 52). Read-only: it reads the command's payload and the
//! memory it points to through a guarded reader, and writes text to `native_dump.log` in the game's live folder. It
//! never writes into the game, never changes a command, and is not created at all unless the variable is set.
//! `docs/research/SPIKE_NATIVE_COMMAND.md` is the reason it exists and says how to read its output.
//!
//! The decoder is pure (it reads through [`Mem`]), so a synthetic buffer tests it; [`LiveMem`] is the real reader.
//!
//! What a dump holds, for one command at one moment (`held`: the tool's command, taken out of the engine's hands;
//! `apply`: the simulation is about to apply it; `applied`: the simulation has just applied it, so the result
//! part of the payload is filled):
//!
//! - a header: sequence number, event, the mod's key (the hold id, when the module held the command), the tool
//!   site and tool index, the kind, two clocks and the thread;
//! - `HEX`: the payload's bytes (0x9c0 of them, the engine's fixed command size), zero rows folded;
//! - `KNOWN`: the named members at the offsets the executable's own Lua binding registers (static analysis of build
//!   40420, see the research note), each with the container's decoded header (vector begin, end, capacity);
//! - `B<n>` blocks: the payload itself (`B0`) and every heap block reachable from it by pointers, to a depth, each
//!   as 8-byte words classified as pointer (image offset, string, vector triple, `std::string`), entity-looking
//!   id, float, placeholder (small negative) or small integer.
//!
//! The classification is a heuristic and says so (`?`): a 4-byte word in 1000..4,000,000 may be an entity id or an
//! integer, one that is a plausible float magnitude is shown as a float. The raw words are always printed next to it.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

/// Switch: `1` = the normal dump, `2` = deeper and larger.
pub const DUMP_VAR: &str = "FEVERLINK3_DUMP_COMMANDS";
/// Where it goes, next to `native_ctl.log`.
pub const DUMP_FILE: &str = "native_dump.log";
/// One summary line per dump, to find the right one in the big file.
pub const INDEX_FILE: &str = "native_dump_index.log";

/// Size of the engine's command payload object (the kind byte is the last used one, at 0x9b8).
pub const PAYLOAD_SIZE: usize = 0x9c0;
/// The image is no larger than this (the game's executable is 70 MB); a pointer inside is shown as an offset into it.
pub const IMAGE_BOUND: usize = 0x1000_0000;

/// The interface tools whose `CommandList::Add` call sites are held, by index in the build table's `ui_add_sites`
/// (names from the address tool's report, `tools/addr/out/6ac50427.txt`; an index past the list is "?").
pub const TOOL_NAMES: [&str; 12] = [
    "Bulldozer",
    "ConstructionBuilder",
    "tool#2",
    "LaneModifier#3",
    "LaneModifier#4",
    "LaneModifier#5",
    "ModuleBuilder",
    "StreetBuilder",
    "StreetTerminalTool",
    "TrackModifier",
    "EntityWindow#10",
    "EntityWindow#11",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Pointer levels followed from the payload (the payload is level 0).
    pub depth: usize,
    /// Most blocks decoded per dump.
    pub max_blocks: usize,
    /// Bytes decoded of an ordinary block, and of a vector the layout names as large (a construction entity is 3.6 KB).
    pub block_bytes: usize,
    pub big_block_bytes: usize,
    /// One dump's text stops here.
    pub max_dump_bytes: usize,
    /// Dumps of commands the module held, and of commands the simulation applied (a before and an after each).
    pub max_held: u32,
    pub max_applied: u32,
    /// The file stops growing here.
    pub max_file_bytes: u64,
    /// Level 3: also dump script commands (no hold key). Otherwise only commands held for a tool and their application.
    pub scripts: bool,
}

impl Settings {
    /// `None` unless the variable is `1` or `2`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<Settings> {
        let level = match get(DUMP_VAR)?.trim() {
            "1" => 1,
            "2" => 2,
            "3" => 3,
            _ => return None,
        };
        let deep = level >= 2;
        Some(Settings {
            depth: if deep { 6 } else { 4 },
            max_blocks: if deep { 1500 } else { 500 },
            block_bytes: if deep { 2048 } else { 512 },
            big_block_bytes: if deep { 16384 } else { 8192 },
            max_dump_bytes: if deep { 600 << 10 } else { 512 << 10 },
            max_held: 60,
            // an apply and an applied dump each count: 60 builds
            max_applied: 120,
            max_file_bytes: 200 << 20,
            scripts: level == 3,
        })
    }
}

// ---------------------------------------------------------------------------------------------------------------
// reading memory
// ---------------------------------------------------------------------------------------------------------------

/// A reader that never faults: `None` when any byte is not readable.
pub trait Mem {
    fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>>;

    /// The size of the allocation that STARTS at `addr`, for readers that own their blocks and can say it without asking the OS
    /// (the test arenas). `None` for anything else; the live reader always answers `None`. A typed text never asks: every block of its
    /// graph has a size known from the container it was found in.
    ///
    /// The game's reader once asked the heap about each pointer (`HeapValidate`, `HeapSize`): a real run crashed in `HeapSize` on a
    /// 16-byte aligned address with a readable header (`ntdll` +0x91919, null segment), because the heap functions are not safe for a
    /// value that is not the start of a block. The replacement, a whole-heap index (`HeapWalk` under `HeapLock` before every text), took
    /// 220-320 ms per text and stopped at its capacity of 3,145,728 blocks in every real run, so blocks past it were missed (r1-r6,
    /// 2026-10-14). Both are gone: the containers give the sizes.
    fn heap_size(&self, _addr: usize) -> Option<usize> {
        None
    }
}

/// `want` bytes at `addr`, or as many as are readable up to the end of the page, or nothing.
pub fn read_upto(mem: &dyn Mem, addr: usize, want: usize) -> Option<Vec<u8>> {
    if want == 0 {
        return None;
    }
    mem.read(addr, want).or_else(|| {
        let to_page_end = 0x1000 - (addr & 0xfff);
        (to_page_end < want).then(|| mem.read(addr, to_page_end)).flatten()
    })
}

/// The process's own memory, read through [`crate::mem`] (every read checks the range first).
#[cfg(windows)]
pub struct LiveMem;

#[cfg(windows)]
impl Mem for LiveMem {
    fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
        if addr < 0x1_0000 {
            return None;
        }
        // SAFETY: `mem::read` asks the OS whether the range is committed and readable before it copies.
        unsafe { crate::mem::read(addr as *const u8, len) }.ok()
    }
}

// ---------------------------------------------------------------------------------------------------------------
// the layout known from the executable (build 40420)
// ---------------------------------------------------------------------------------------------------------------

/// What a member of the layout is, as far as the binding says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `Vector<T>`: three pointers (begin, end, capacity). `T` is named; `min` is a lower bound of its size.
    Vec { elem: &'static str, min: usize, big: bool },
    /// A 0x10-byte map object (the `old2new`/`new2old` members; its type is not known).
    Map16,
    /// Opaque bytes of a known length (a hash set or map of 0x40 bytes, a grid, a sub-struct).
    Opaque(usize),
    /// A single byte flag.
    Flag,
    /// A 4-byte entity id.
    Entity,
}

pub struct Field {
    pub off: usize,
    pub name: &'static str,
    pub shape: Shape,
}

const fn v(off: usize, name: &'static str, elem: &'static str, min: usize) -> Field {
    Field { off, name, shape: Shape::Vec { elem, min, big: false } }
}

/// The payload of a `build_proposal` command (`WorldBuildProposalCommandData` followed by the kind byte):
/// offsets read out of the executable's Lua binding (`RegisterUsertypesTransport`, 0x22c5200) and out of the
/// apply handler (0x9e2900); the Context ones agree with MPFever's independent findings (+0x358, +0x360, +0x362,
/// +0x36c). Element sizes are lower bounds from the last member of each element type, not proven strides.
pub const PAYLOAD_FIELDS: &[Field] = &[
    v(0x000, "proposal.addedNodes", "NodeAndEntity", 0x08),
    v(0x018, "proposal.addedSegments", "SegmentAndEntity", 0x330),
    v(0x030, "proposal.removedNodes", "NodeAndEntity", 0x08),
    v(0x048, "proposal.removedSegments", "SegmentAndEntity", 0x330),
    v(0x060, "proposal.nodeConfigsToAdd", "BaseNodeLaneConnectionAndEntity", 0x7c),
    v(0x078, "proposal.nodeConfigsToRemove", "Entity", 4),
    Field { off: 0x090, name: "proposal.(0x90..0x110 not registered)", shape: Shape::Opaque(0x80) },
    v(0x110, "proposal.edgeObjectsToRemove", "Entity?", 4),
    v(0x128, "proposal.edgeObjectsToAdd", "EdgeObject", 0x190),
    Field { off: 0x140, name: "proposal.new2oldNodes", shape: Shape::Map16 },
    Field { off: 0x150, name: "proposal.old2newNodes", shape: Shape::Map16 },
    Field { off: 0x160, name: "proposal.new2oldSegments", shape: Shape::Map16 },
    Field { off: 0x170, name: "proposal.old2newSegments", shape: Shape::Map16 },
    Field { off: 0x180, name: "proposal.new2oldEdgeObjects", shape: Shape::Map16 },
    Field { off: 0x190, name: "proposal.old2newEdgeObjects", shape: Shape::Map16 },
    Field { off: 0x1a0, name: "proposal.(0x1a0..0x1d0 not registered)", shape: Shape::Opaque(0x30) },
    v(0x1d0, "proposal.frozenNodes", "Entity", 4),
    Field { off: 0x1e8, name: "terrainAlignSkipEdges", shape: Shape::Opaque(0x40) },
    v(0x228, "segmentTags", "?", 4),
    v(0x240, "toRemove", "Entity", 4),
    Field { off: 0x258, name: "toAdd", shape: Shape::Vec { elem: "ConstructionEntity", min: 0xe40, big: true } },
    Field { off: 0x270, name: "old2new", shape: Shape::Opaque(0x40) },
    Field { off: 0x2b0, name: "parcelsToRemove", shape: Shape::Opaque(0x28) },
    Field { off: 0x2d8, name: "terrain", shape: Shape::Opaque(0x80) },
    Field { off: 0x358, name: "context.gatherBuildings", shape: Shape::Flag },
    Field { off: 0x359, name: "context.gatherFields", shape: Shape::Flag },
    Field { off: 0x360, name: "context.checkTerrainAlignment", shape: Shape::Flag },
    Field { off: 0x362, name: "context.cleanupStreetGraph", shape: Shape::Flag },
    Field { off: 0x36c, name: "context.player", shape: Shape::Entity },
    v(0x370, "context.refundableEntities", "?", 8),
    Field { off: 0x3b2, name: "context.extendProposalRedoPillars", shape: Shape::Flag },
    Field { off: 0x3d0, name: "withCostRep", shape: Shape::Flag },
    Field { off: 0x3d1, name: "ignoreErrors", shape: Shape::Flag },
    Field { off: 0x3d2, name: "playerInitiated", shape: Shape::Flag },
    // resultProposalData starts at 0x3d8 (the apply handler stores the engine's result there); offsets inside it
    // from the binding of ProposalData, added to 0x3d8
    Field { off: 0x3d8 + 0x1e8, name: "result.parallelProposal", shape: Shape::Opaque(0x110) },
    Field { off: 0x3d8 + 0x2f8, name: "result.collisionInfo", shape: Shape::Opaque(0x128) },
    v(0x3d8 + 0x3c0, "result.terrainPreviewHeightModLocations", "Vec3f", 12),
    v(0x3d8 + 0x3d8, "result.terrainPreviewHeightModCutFill", "Vec2f", 8),
    v(0x3d8 + 0x420, "result.toAddData", "ConstructionData", 8),
    Field { off: 0x3d8 + 0x468, name: "result.entity2tn", shape: Shape::Opaque(0x40) },
    Field { off: 0x3d8 + 0x4a8, name: "result.tpNetLinkProposal", shape: Shape::Opaque(0x90) },
    Field { off: 0x3d8 + 0x538, name: "result.costs", shape: Shape::Opaque(8) },
    Field { off: 0x3d8 + 0x570, name: "result.errorState.critical", shape: Shape::Flag },
    v(0x3d8 + 0x578, "result.errorState.messages", "std::string", 0x20),
    v(0x3d8 + 0x590, "result.errorState.infos", "std::string", 0x20),
    v(0x3d8 + 0x5a8, "result.errorState.warnings", "std::string", 0x20),
    v(0x998, "resultEntities?", "{Entity,Revision}?", 8),
    Field { off: 0x9b0, name: "doDust?", shape: Shape::Flag },
    Field { off: 0x9b8, name: "kind", shape: Shape::Flag },
];

/// Member names of the element types, at their offsets inside one element (from the same binding).
pub fn element_field(elem: &str, off: usize) -> Option<&'static str> {
    let table: &[(usize, &str)] = match elem {
        "NodeAndEntity" => &[(0, "entity"), (4, "comp.position")],
        // comp is a BaseEdge at +8; its members from the binding at 0x176b554 (they chain exactly: node0, node1,
        // position0, position1, tangent0, tangent1, type, typeIndex are 4/4/12/12/12/12/4/4 bytes)
        "SegmentAndEntity" => &[
            (0, "entity"),
            (8, "comp.node0"),
            (0xc, "comp.node1"),
            (0x10, "comp.position0"),
            (0x1c, "comp.position1"),
            (0x28, "comp.tangent0"),
            (0x34, "comp.tangent1"),
            (0x40, "comp.type"),
            (0x44, "comp.typeIndex"),
            (0x48, "comp.objects"),
            (0x60, "comp.edgeDecorations"),
            (0x78, "comp.laneConfigs"),
            (0x90, "comp.roadDevelopmentLocked"),
            (0x94, "comp.distance"),
            (0x98, "comp.roadType"),
            (0xa0, "comp.roadTemplate"),
            (0xe0, "comp.roadStyle"),
            (0x120, "type"),
            (0x128, "streetEdge"),
            (0x320, "playerOwned"),
            (0x328, "emissionEmitter"),
        ],
        "BaseNodeLaneConnectionAndEntity" => &[(0, "comp"), (0x78, "entity")],
        "EdgeObject" => &[
            (0, "resultEntity"),
            (4, "category"),
            (8, "segmentEntity"),
            (0x160, "oneWay"),
            (0x161, "left"),
            (0x168, "name"),
            (0x188, "playerEntity"),
        ],
        "ConstructionEntity" => &[
            (0, "desc"),
            // the construction component is embedded at +0xa18 (0x280 bytes up to transf); its members are those of the
            // engine's Construction component (binding at 0x1885d30, offsets added to 0xa18): inferred from the size match
            (0xa18, "construction"),
            (0xa18 + 0x40, "construction.params"),
            (0xa18 + 0x58, "construction.transf"),
            (0xa18 + 0xa0, "construction.frozenNodes"),
            (0xa18 + 0xb8, "construction.frozenEdges"),
            (0xa18 + 0xd0, "construction.depots"),
            (0xa18 + 0xe8, "construction.stations"),
            (0xa18 + 0x100, "construction.industries"),
            (0xa18 + 0x118, "construction.townBuildings"),
            (0xa18 + 0x130, "construction.subconstructions"),
            (0xa18 + 0x148, "construction.persistentMetadata"),
            (0xc98, "transf"),
            (0xcd8, "frozenNodes"),
            (0xcf0, "segmentsBefore"),
            (0xe18, "name"),
            (0xe38, "playerEntity"),
            (0xe3c, "setAsHeadquarterHack"),
        ],
        _ => return None,
    };
    table.iter().find(|(o, _)| *o == off).map(|(_, n)| *n)
}

// ---------------------------------------------------------------------------------------------------------------
// word classification
// ---------------------------------------------------------------------------------------------------------------

/// Entity ids in the saves seen so far are 5 to 6 digits (towns 62469); this is the range called "entity-looking".
pub const ENTITY_LO: u32 = 1000;
pub const ENTITY_HI: u32 = 4_000_000;

/// A 4-byte word, classified.
pub fn classify_u32(w: u32) -> String {
    if w == 0 {
        return "0".into();
    }
    if w < ENTITY_LO {
        return format!("i{w}");
    }
    if w >= 0xffff_fc00 {
        return format!("ph{}", w as i32); // a small negative number: the placeholder ids of a script proposal
    }
    if w <= ENTITY_HI {
        return format!("ent?{w}");
    }
    let f = f32::from_bits(w);
    let magnitude = f.abs();
    if f.is_finite() && (0.0001..1.0e7).contains(&magnitude) {
        return format!("f{f:.3}");
    }
    format!("x{w:08x}")
}

/// Longest string looked for behind a pointer or inside a buffer.
const STRING_MAX: usize = 96;

/// `bytes` as a printable ASCII string of at least `min` characters ending at a NUL (or at the end when `exact`).
pub fn ascii_string(bytes: &[u8], min: usize, exact: Option<usize>) -> Option<String> {
    let end = match exact {
        Some(n) => {
            if n > bytes.len() {
                return None;
            }
            n
        }
        None => bytes.iter().position(|&b| b == 0)?,
    };
    if end < min || end > STRING_MAX {
        return None;
    }
    let s = &bytes[..end];
    s.iter().all(|&b| (0x20..0x7f).contains(&b)).then(|| String::from_utf8_lossy(s).into_owned())
}

#[derive(Debug, Clone, Copy)]
pub struct Env {
    pub image_base: usize,
}

impl Env {
    fn in_image(&self, p: usize) -> bool {
        p >= self.image_base && p - self.image_base < IMAGE_BOUND
    }
}

pub(crate) fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap_or([0; 8]))
}

pub(crate) fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap_or([0; 4]))
}

/// A user-space address that is plausibly a pointer.
pub(crate) fn pointer_like(w: u64) -> bool {
    (0x1_0000..0x0000_8000_0000_0000).contains(&w)
}

/// What a block holds, when the container it was found in names the element type.
#[derive(Clone, Copy)]
struct Context {
    elem: Option<&'static str>,
}

/// A block found and not yet decoded.
struct Pending {
    number: usize,
    addr: usize,
    len: usize,
    depth: usize,
    ctx: Context,
    from: String,
}

/// The decoder: blocks found by following pointers from a root, in breadth-first order.
struct Decoder<'a> {
    mem: &'a dyn Mem,
    env: Env,
    set: Settings,
    out: String,
    /// address -> block number
    seen: HashMap<usize, usize>,
    queue: VecDeque<Pending>,
    next: usize,
    truncated: bool,
}

impl Decoder<'_> {
    fn room(&mut self) -> bool {
        if self.out.len() >= self.set.max_dump_bytes {
            if !self.truncated {
                self.truncated = true;
                self.out.push_str("(dump text cap reached: the rest is dropped)\n");
            }
            return false;
        }
        true
    }

    /// Registers a block to decode later, once; returns its number (new or existing), or `None` past the limits.
    fn block_for(&mut self, addr: usize, len: usize, depth: usize, ctx: Context, from: String) -> Option<usize> {
        if let Some(&n) = self.seen.get(&addr) {
            return Some(n);
        }
        if depth > self.set.depth || self.next >= self.set.max_blocks {
            return None;
        }
        let number = self.next;
        self.next += 1;
        self.seen.insert(addr, number);
        self.queue.push_back(Pending { number, addr, len, depth, ctx, from });
        Some(number)
    }

    /// Describes the 8-byte word at `off` of `buf`, scheduling blocks for the pointers and containers it finds.
    /// Returns the description and how many 8-byte words it consumed (a vector triple takes 3, a string 4).
    fn describe(&mut self, buf: &[u8], off: usize, depth: usize, field: Option<&Field>) -> (String, usize) {
        let w = u64_at(buf, off);
        let have = buf.len() - off;
        // an MSVC std::string: 16 bytes of buffer or pointer, size, capacity >= 15
        if have >= 32 {
            let (size, cap) = (u64_at(buf, off + 16), u64_at(buf, off + 24));
            if cap >= 15 && size <= cap && size <= 4096 && (cap == 15 || pointer_like(w)) {
                let text = if cap == 15 {
                    ascii_string(&buf[off..off + 16], 0, Some(size as usize))
                } else {
                    let want = (size as usize).min(STRING_MAX);
                    read_upto(self.mem, w as usize, want).and_then(|b| ascii_string(&b, 0, Some(b.len().min(want))))
                };
                if let Some(t) = text {
                    return (format!("std::string \"{t}\" (size {size}, cap {cap})"), 4);
                }
            }
        }
        // a vector: begin <= end <= capacity, all pointer-like, begin readable
        if have >= 24 {
            let (b, e, c) = (w, u64_at(buf, off + 8), u64_at(buf, off + 16));
            if pointer_like(b) && b <= e && e <= c && c - b < (1 << 30) && b.is_multiple_of(4) {
                let bytes = (e - b) as usize;
                if read_upto(self.mem, b as usize, bytes.clamp(1, 8)).is_some() {
                    let (elem, min, big) = match field.map(|f| f.shape) {
                        Some(Shape::Vec { elem, min, big }) => (elem, min, big),
                        _ => ("?", 0, false),
                    };
                    let cap = if big { self.set.big_block_bytes } else { self.set.block_bytes };
                    let n = if bytes == 0 {
                        None
                    } else {
                        let ctx = Context { elem: (elem != "?").then_some(elem) };
                        self.block_for(b as usize, bytes.min(cap), depth + 1, ctx, format!("vector@{off:#x}"))
                    };
                    let count = if min > 0 && bytes.is_multiple_of(min) {
                        format!(", {} x {:#x}", bytes / min, min)
                    } else {
                        String::new()
                    };
                    let target = n.map_or(String::new(), |n| format!(" -> B{n}"));
                    return (format!("vector {bytes:#x} bytes (cap {:#x}){count}{target}", c - b), 3);
                }
            }
        }
        // a pointer
        if pointer_like(w) {
            let p = w as usize;
            if self.env.in_image(p) {
                let rva = p - self.env.image_base;
                let text = read_upto(self.mem, p, STRING_MAX).and_then(|b| ascii_string(&b, 3, None));
                return (
                    match text {
                        Some(t) => format!("img+{rva:#x} \"{t}\""),
                        None => format!("img+{rva:#x}"),
                    },
                    1,
                );
            }
            if read_upto(self.mem, p, 8).is_some() {
                let text = read_upto(self.mem, p, STRING_MAX).and_then(|b| ascii_string(&b, 4, None));
                let target = if let Some(t) = text {
                    format!("ptr \"{t}\"")
                } else {
                    let ctx = Context { elem: None };
                    match self.block_for(p, self.set.block_bytes, depth + 1, ctx, format!("pointer@{off:#x}")) {
                        Some(n) => format!("ptr -> B{n}"),
                        None => "ptr (not followed)".into(),
                    }
                };
                return (target, 1);
            }
        }
        // two 4-byte words
        let (lo, hi) = (u32_at(buf, off), u32_at(buf, off + 4));
        if hi == 0 {
            return (classify_u32(lo), 1);
        }
        (format!("{} {}", classify_u32(lo), classify_u32(hi)), 1)
    }

    /// Decodes one block's bytes into lines.
    fn block(&mut self, p: Pending) {
        let Some(buf) = read_upto(self.mem, p.addr, p.len) else {
            let _ = writeln!(self.out, "B{} @{:#x} depth {} from {}: unreadable", p.number, p.addr, p.depth, p.from);
            return;
        };
        let _ = writeln!(
            self.out,
            "B{} @{:#x} depth {} from {} ({:#x} bytes read)",
            p.number,
            p.addr,
            p.depth,
            p.from,
            buf.len()
        );
        // a vector of 12 bytes ends inside a word: pad to whole words so the last one is shown
        let mut buf = buf;
        buf.resize(buf.len().div_ceil(8) * 8, 0);
        self.words(&buf, 0, p.depth, p.ctx, None);
    }

    /// The word lines of a buffer from `start`; `fields` names the root's members, `ctx` the element type of a block.
    fn words(&mut self, buf: &[u8], start: usize, depth: usize, ctx: Context, fields: Option<&[Field]>) {
        let mut off = start & !7;
        let mut zero_from: Option<usize> = None;
        while off + 8 <= buf.len() {
            if !self.room() {
                return;
            }
            // the members that start inside this word (flags and ids are not 8-aligned)
            let mut names: Vec<&'static str> = Vec::new();
            let mut head: Option<&Field> = None;
            for at in off..off + 8 {
                match (fields, ctx.elem) {
                    (Some(fs), _) => {
                        for f in fs.iter().filter(|f| f.off == at) {
                            names.push(f.name);
                            if at == off {
                                head = Some(f);
                            }
                        }
                    }
                    (None, Some(e)) => names.extend(element_field(e, at)),
                    _ => {}
                }
            }
            if u64_at(buf, off) == 0 && names.is_empty() {
                zero_from.get_or_insert(off);
                off += 8;
                continue;
            }
            if let Some(z) = zero_from.take() {
                let _ = writeln!(self.out, "  +{z:#06x}..+{:#06x} zero", off - 8);
            }
            let (desc, used) = self.describe(buf, off, depth, head);
            let tag = if names.is_empty() { String::new() } else { format!("  <{}>", names.join(", ")) };
            let _ =
                writeln!(self.out, "  +{off:#06x} {:08x} {:08x}  {desc}{tag}", u32_at(buf, off), u32_at(buf, off + 4));
            off += 8 * used.max(1);
        }
        if let Some(z) = zero_from {
            let _ = writeln!(self.out, "  +{z:#06x}..+{:#06x} zero", (buf.len() & !7).saturating_sub(8));
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// one dump
// ---------------------------------------------------------------------------------------------------------------

pub struct Header<'a> {
    pub seq: u32,
    pub event: &'a str,
    /// The mod's key for the command: the hold id, when the module held it.
    pub hold_id: Option<u64>,
    /// Return address (RVA) of the `CommandList::Add` call that queued it, and the tool it belongs to.
    pub site_rva: Option<u32>,
    pub tool: Option<usize>,
    pub kind: u8,
    pub kind_name: &'a str,
    /// Milliseconds since the module started, and the wall clock (ms since the epoch).
    pub t_ms: u64,
    pub wall_ms: u128,
    pub thread: u32,
    pub payload: usize,
    /// The `apply` dump this `applied` dump follows.
    pub group: Option<u32>,
}

/// Which part of the payload is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// All of it.
    Whole,
    /// Only the result part (from the flags before `resultProposalData` to the end): what the engine filled in.
    Result,
}

/// The text of one dump of the payload at `head.payload`. Bounded by `set`.
pub fn render(mem: &dyn Mem, env: Env, set: Settings, head: &Header, scope: Scope) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "=== dump #{} event={} key={} site={} tool={} kind={} ({}) t_ms={} wall_ms={} thread={} payload={:#x} scope={:?} group={} ===",
        head.seq,
        head.event,
        head.hold_id.map_or("none".to_string(), |k| k.to_string()),
        head.site_rva.map_or("none".to_string(), |s| format!("{s:#x}")),
        head.tool.map_or("none".to_string(), |t| format!("{t} {}", TOOL_NAMES.get(t).copied().unwrap_or("?"))),
        head.kind,
        head.kind_name,
        head.t_ms,
        head.wall_ms,
        head.thread,
        head.payload,
        scope,
        head.group.map_or("none".to_string(), |g| g.to_string()),
    );
    let Some(payload) = mem.read(head.payload, PAYLOAD_SIZE) else {
        out.push_str("payload unreadable\n");
        return out;
    };
    let (from, to) = match scope {
        Scope::Whole => (0, PAYLOAD_SIZE),
        Scope::Result => (0x3c8, PAYLOAD_SIZE),
    };
    // HEX
    let mut off = from & !15;
    let mut zero_from: Option<usize> = None;
    while off < to {
        let row = &payload[off..(off + 16).min(PAYLOAD_SIZE)];
        if row.iter().all(|&b| b == 0) {
            zero_from.get_or_insert(off);
        } else {
            if let Some(z) = zero_from.take() {
                let _ = writeln!(out, "HEX +{z:#06x}..+{:#06x} zero", off - 1);
            }
            let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
            let _ = writeln!(out, "HEX +{off:#06x} {}", hex.join(" "));
        }
        off += 16;
    }
    if let Some(z) = zero_from {
        let _ = writeln!(out, "HEX +{z:#06x}..+{:#06x} zero", to - 1);
    }
    // KNOWN: the members at the registered offsets, containers decoded
    out.push_str("KNOWN (offsets from the executable's Lua binding, build 40420; element sizes are lower bounds)\n");
    for f in PAYLOAD_FIELDS.iter().filter(|f| f.off >= from.saturating_sub(0x10) && f.off < to) {
        let line = match f.shape {
            Shape::Vec { elem, min, .. } => {
                let (b, e, c) = (u64_at(&payload, f.off), u64_at(&payload, f.off + 8), u64_at(&payload, f.off + 16));
                let bytes = e.wrapping_sub(b);
                let ok = pointer_like(b) && b <= e && e <= c;
                if b == 0 && e == 0 {
                    "empty (null)".to_string()
                } else if ok {
                    let div = if min > 0 && bytes.is_multiple_of(min as u64) {
                        format!("; {} x {:#x} if the element is that size", bytes / min as u64, min)
                    } else {
                        format!("; {:#x} is not a multiple of {:#x}", bytes, min)
                    };
                    format!("vector<{elem}> begin {b:#x} bytes {bytes:#x} cap {:#x}{div}", c - b)
                } else {
                    format!("vector<{elem}> NOT a plausible triple {b:#x} {e:#x} {c:#x}")
                }
            }
            Shape::Map16 => format!("map16 {:016x} {:016x}", u64_at(&payload, f.off), u64_at(&payload, f.off + 8)),
            Shape::Opaque(n) => {
                let nz = payload[f.off..(f.off + n).min(PAYLOAD_SIZE)].iter().filter(|&&b| b != 0).count();
                format!("opaque {n:#x} bytes, {nz} non-zero")
            }
            Shape::Flag => format!("{}", payload[f.off]),
            Shape::Entity => format!("{} ({})", u32_at(&payload, f.off), classify_u32(u32_at(&payload, f.off))),
        };
        let _ = writeln!(out, "KNOWN +{:#06x} {}: {}", f.off, f.name, line);
    }
    // blocks
    let mut d = Decoder { mem, env, set, out, seen: HashMap::new(), queue: VecDeque::new(), next: 1, truncated: false };
    d.seen.insert(head.payload, 0);
    let _ = writeln!(d.out, "B0 @{:#x} depth 0 (the payload; {:#x}..{:#x})", head.payload, from, to);
    d.words(&payload[..to], from, 0, Context { elem: None }, Some(PAYLOAD_FIELDS));
    while let Some(pending) = d.queue.pop_front() {
        if !d.room() {
            break;
        }
        d.block(pending);
    }
    let _ = writeln!(d.out, "=== end of dump #{} ({} block(s) found, the payload included) ===", head.seq, d.next);
    d.out
}

// ---------------------------------------------------------------------------------------------------------------
// the file
// ---------------------------------------------------------------------------------------------------------------

/// The dump state of a running module: where it writes, how much it has written, how many dumps it has made.
pub struct Dumper {
    pub settings: Settings,
    dir: Mutex<Option<PathBuf>>,
    seq: AtomicU32,
    held: AtomicU32,
    applied: AtomicU32,
    bytes: AtomicU64,
}

/// Which budget a dump counts against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    Held,
    Applied,
}

impl Dumper {
    pub fn new(settings: Settings) -> Dumper {
        Dumper {
            settings,
            dir: Mutex::new(None),
            seq: AtomicU32::new(0),
            held: AtomicU32::new(0),
            applied: AtomicU32::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    /// Chooses the folder; an earlier run's file is set aside so this run's dumps start the file.
    pub fn set_dir(&self, dir: &Path) {
        if self.dir.lock().unwrap_or_else(|p| p.into_inner()).as_deref() == Some(dir) {
            return;
        }
        for file in [DUMP_FILE, INDEX_FILE] {
            let path = dir.join(file);
            if path.exists() {
                let _ = std::fs::rename(&path, path.with_extension("log.1"));
            }
        }
        *self.dir.lock().unwrap_or_else(|p| p.into_inner()) = Some(dir.to_path_buf());
    }

    pub fn has_dir(&self) -> bool {
        self.dir.lock().unwrap_or_else(|p| p.into_inner()).is_some()
    }

    /// Takes a sequence number if the budget allows another dump; `None` when it is used up.
    pub fn begin(&self, budget: Budget) -> Option<u32> {
        let (counter, max) = match budget {
            Budget::Held => (&self.held, self.settings.max_held),
            Budget::Applied => (&self.applied, self.settings.max_applied),
        };
        if self.bytes.load(Ordering::Relaxed) >= self.settings.max_file_bytes {
            return None;
        }
        if counter.fetch_add(1, Ordering::Relaxed) >= max {
            counter.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(self.seq.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Appends a text to the file (opened per dump: they are rare, and the session folder may be cleared).
    pub fn write(&self, text: &str) -> std::io::Result<()> {
        self.append(DUMP_FILE, text)?;
        self.bytes.fetch_add(text.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    /// Appends one line to the index file.
    pub fn write_index(&self, line: &str) -> std::io::Result<()> {
        self.append(
            INDEX_FILE,
            &format!(
                "{line}
"
            ),
        )
    }

    fn append(&self, file: &str, text: &str) -> std::io::Result<()> {
        use std::io::Write;
        let Some(dir) = self.dir.lock().unwrap_or_else(|p| p.into_inner()).clone() else {
            return Err(std::io::Error::other("no folder"));
        };
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(file))?;
        f.write_all(text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Memory made of separate regions, like a process.
    struct Fake(Vec<(usize, Vec<u8>)>);

    impl Mem for Fake {
        fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
            self.0.iter().find_map(|(s, b)| {
                (addr >= *s && addr + len <= s + b.len()).then(|| b[addr - s..addr - s + len].to_vec())
            })
        }
    }

    fn put(buf: &mut [u8], off: usize, v: u64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    fn settings() -> Settings {
        Settings::from_lookup(|_| Some("1".into())).unwrap()
    }

    const BASE: usize = 0x1_4000_0000;
    const HEAP: usize = 0x2000_0000_0000;

    fn head(payload: usize) -> Header<'static> {
        Header {
            seq: 1,
            event: "held",
            hold_id: Some(7),
            site_rva: Some(0x4d396f),
            tool: Some(0),
            kind: 52,
            kind_name: "build_proposal",
            t_ms: 1234,
            wall_ms: 99,
            thread: 4,
            payload,
            group: None,
        }
    }

    #[test]
    fn the_switch_is_off_unless_it_is_one_or_two() {
        let get = |v: &'static str| move |n: &str| (n == DUMP_VAR).then(|| v.to_string());
        assert!(Settings::from_lookup(|_| None).is_none());
        assert!(Settings::from_lookup(get("0")).is_none());
        assert!(Settings::from_lookup(get("yes")).is_none());
        assert!(!Settings::from_lookup(get("2")).unwrap().scripts);
        assert!(Settings::from_lookup(get("3")).unwrap().scripts);
        let (one, two) = (Settings::from_lookup(get("1")).unwrap(), Settings::from_lookup(get(" 2 ")).unwrap());
        assert!(two.depth > one.depth && two.max_dump_bytes > one.max_dump_bytes);
    }

    #[test]
    fn four_byte_words_are_told_apart() {
        assert_eq!(classify_u32(0), "0");
        assert_eq!(classify_u32(7), "i7");
        assert_eq!(classify_u32(62469), "ent?62469");
        assert_eq!(classify_u32((-3i32) as u32), "ph-3");
        assert_eq!(classify_u32(12.5f32.to_bits()), "f12.500");
        assert_eq!(classify_u32((-0.25f32).to_bits()), "f-0.250");
        assert_eq!(classify_u32(0x7fff_ffff), "x7fffffff");
    }

    #[test]
    fn strings_need_printable_text() {
        assert_eq!(ascii_string(b"abc\0xyz", 3, None).as_deref(), Some("abc"));
        assert_eq!(ascii_string(b"ab\0", 3, None), None);
        assert_eq!(ascii_string(b"a\x01c\0", 1, None), None);
        assert_eq!(ascii_string(b"hello", 0, Some(5)).as_deref(), Some("hello"));
        assert_eq!(ascii_string(b"hello", 0, Some(9)), None);
    }

    /// A payload with a toRemove vector of entity ids, a toAdd vector whose first element holds a std::string
    /// pointing at a file name and a pointer cycle, a kind byte and an image string.
    fn synthetic() -> (Fake, usize) {
        let payload_at = HEAP;
        let ids_at = HEAP + 0x10000;
        let con_at = HEAP + 0x20000;
        let name_at = HEAP + 0x30000;
        let img_str = BASE + 0x500;
        let mut p = vec![0u8; PAYLOAD_SIZE];
        // toRemove: three ids
        put(&mut p, 0x240, ids_at as u64);
        put(&mut p, 0x248, (ids_at + 12) as u64);
        put(&mut p, 0x250, (ids_at + 16) as u64);
        // toAdd: one 0xe40-byte element
        put(&mut p, 0x258, con_at as u64);
        put(&mut p, 0x260, (con_at + 0xe40) as u64);
        put(&mut p, 0x268, (con_at + 0xe40) as u64);
        // context: player entity, flags
        p[0x358] = 1;
        p[0x36c..0x370].copy_from_slice(&62469u32.to_le_bytes());
        p[0x3d1] = 1;
        p[0x9b8] = 52;
        // an image pointer in an unregistered slot
        put(&mut p, 0x90, img_str as u64);
        let mut ids = vec![0u8; 16];
        for (i, id) in [62470u32, (-1i32) as u32, 12.5f32.to_bits()].iter().enumerate() {
            ids[i * 4..i * 4 + 4].copy_from_slice(&id.to_le_bytes());
        }
        let mut con = vec![0u8; 0xe40];
        // at 0: a std::string (heap) "path/to/x.con"; at 8 of the second: a pointer back to the element (a cycle)
        put(&mut con, 0, name_at as u64);
        put(&mut con, 16, 13);
        put(&mut con, 24, 31);
        put(&mut con, 0x100, con_at as u64);
        let name = b"path/to/x.con\0".to_vec();
        let mut img = vec![0u8; 0x600];
        img[0x500..0x500 + 8].copy_from_slice(b"mesh.msh");
        (Fake(vec![(payload_at, p), (ids_at, ids), (con_at, con), (name_at, name), (BASE, img)]), payload_at)
    }

    #[test]
    fn a_synthetic_payload_is_decoded_with_names_vectors_strings_and_ids() {
        let (mem, at) = synthetic();
        let text = render(&mem, Env { image_base: BASE }, settings(), &head(at), Scope::Whole);
        assert!(
            text.starts_with("=== dump #1 event=held key=7 site=0x4d396f tool=0 Bulldozer kind=52 (build_proposal)")
        );
        assert!(text.contains("HEX +0x09b0"), "{text}");
        assert!(text.contains("KNOWN +0x0240 toRemove: vector<Entity> begin"), "{text}");
        assert!(
            text.contains("KNOWN +0x0258 toAdd: vector<ConstructionEntity>") && text.contains("1 x 0xe40"),
            "{text}"
        );
        assert!(text.contains("KNOWN +0x036c context.player: 62469"), "{text}");
        assert!(text.contains("<toRemove>") && text.contains("vector 0xc bytes"), "{text}");
        // the ids block: an entity, a placeholder and a float
        assert!(text.contains("ent?62470 ph-1"), "{text}");
        assert!(text.contains("f12.500"), "{text}");
        // the element's own names and the string behind its pointer
        assert!(text.contains("<desc>"), "{text}");
        assert!(text.contains("std::string \"path/to/x.con\" (size 13, cap 31)"), "{text}");
        // an image pointer is an offset, with its text
        assert!(text.contains("img+0x500 \"mesh.msh\""), "{text}");
        assert!(text.contains("=== end of dump #1"), "{text}");
    }

    #[test]
    fn a_pointer_cycle_and_unreadable_targets_end_cleanly() {
        let (mut mem, at) = synthetic();
        // a pointer to unmapped memory in the payload, and the payload pointing at itself
        mem.0[0].1[0x40..0x48].copy_from_slice(&0x7777_0000_0000u64.to_le_bytes());
        mem.0[0].1[0x48..0x50].copy_from_slice(&(at as u64).to_le_bytes());
        let text = render(&mem, Env { image_base: BASE }, settings(), &head(at), Scope::Whole);
        assert!(text.contains("=== end of dump"), "{text}");
        assert!(text.matches("B0 @").count() == 1, "the payload is decoded once");
    }

    #[test]
    fn an_unreadable_payload_is_reported_not_followed() {
        let text = render(&Fake(vec![]), Env { image_base: BASE }, settings(), &head(0x5555_0000), Scope::Whole);
        assert!(text.contains("payload unreadable"));
    }

    #[test]
    fn the_text_is_bounded() {
        let (mem, at) = synthetic();
        let tiny = Settings { max_dump_bytes: 1, ..settings() };
        let text = render(&mem, Env { image_base: BASE }, tiny, &head(at), Scope::Whole);
        assert!(text.contains("dump text cap reached"), "{text}");
        assert!(text.len() < 30_000, "{}", text.len());
        let shallow = Settings { depth: 0, ..settings() };
        let text = render(&mem, Env { image_base: BASE }, shallow, &head(at), Scope::Whole);
        assert!(!text.contains("B1 @"), "no block below depth 0 is followed: {text}");
    }

    #[test]
    fn the_result_scope_leaves_the_proposal_part_out() {
        let (mem, at) = synthetic();
        let text = render(&mem, Env { image_base: BASE }, settings(), &head(at), Scope::Result);
        assert!(!text.contains("KNOWN +0x0240"), "{text}");
        assert!(text.contains("KNOWN +0x0948 result.errorState.critical"), "{text}");
    }

    #[test]
    fn the_dumper_counts_budgets_and_writes_a_file_set_aside_on_the_next_run() {
        let dir = TempDir::new("dump-file");
        let set = Settings { max_held: 2, max_applied: 1, ..settings() };
        let d = Dumper::new(set);
        assert!(d.write("x").is_err(), "no folder yet");
        d.set_dir(dir.path());
        assert_eq!((d.begin(Budget::Held), d.begin(Budget::Held), d.begin(Budget::Held)), (Some(1), Some(2), None));
        assert_eq!((d.begin(Budget::Applied), d.begin(Budget::Applied)), (Some(3), None));
        d.write("first\n").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join(DUMP_FILE)).unwrap(), "first\n");
        let again = Dumper::new(set);
        again.set_dir(dir.path());
        assert!(!dir.path().join(DUMP_FILE).exists());
        assert_eq!(std::fs::read_to_string(dir.path().join("native_dump.log.1")).unwrap(), "first\n");
    }

    #[test]
    fn the_file_stops_growing() {
        let dir = TempDir::new("dump-cap");
        let d = Dumper::new(Settings { max_file_bytes: 10, ..settings() });
        d.set_dir(dir.path());
        assert!(d.begin(Budget::Held).is_some());
        d.write("0123456789ab").unwrap();
        assert_eq!(d.begin(Budget::Held), None);
    }
}
