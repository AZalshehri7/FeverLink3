//! The portable form of the engine's build command (`FEVERLINK3_PORTABLE_DUMP`), spike S0 (`docs/research/SPIKE_NATIVE_COMMAND.md`).
//!
//! One general rule for every kind of build: the payload of a `build_proposal` command is decoded by a table of the
//! proposal's layout (member name, offset, kind; element types with their stride), not by tool or by build kind. A
//! member the table names is written as a typed value; every other byte of the payload is written structurally
//! (strings, vectors and zero runs recognised, heap pointers marked, image pointers as offsets), so nothing is
//! dropped silently. The result is one text, line by line `path TAB value [TAB comment]`:
//!
//! - `u8:xx`, `u32:xxxxxxxx`, `i32:xxxxxxxx`, `f32:xxxxxxxx` (exact bit patterns), `v3:a,b,c`, `m4:16 words`;
//! - `ent:new#N` for an entity the proposal creates (the tool's placeholder, -N), `ent:old#ID` for an existing one,
//!   with `@x,y,z` (f32 bits) when the proposal itself locates it (a node's position, a segment's end);
//! - `str:"text"` / `res:"text"` (a resource name is two strings, the text is in the second);
//! - `path.#` `n:count` for every vector, then `path[i].member` lines for its elements;
//! - `raw:` tokens for the bytes the table does not name: `zN` zero words, `w:HHHHHHHHHHHHHHHH`, `S<size>:<hex>`
//!   (a `std::string`), `V<bytes>[...]` (a vector and its contents), `I+<rva>` (a pointer into the game image),
//!   `P` (a pointer to heap memory that is not followed: not portable), `b:<hex>` for unaligned bytes.
//!
//! Read-only: the payload and everything behind its pointers is read through [`Mem`] (the guarded reader) and
//! bounded. A layout the module does not know (another game build, a kind byte other than 52, a container that
//! does not fit the table) is refused with a reason, never guessed. The text carries no heap address, so two games
//! that built the same thing produce the same text except where the games differ.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::dump::{ascii_string, pointer_like, u32_at, u64_at, Env, Mem, IMAGE_BOUND, PAYLOAD_SIZE};

/// `1` = commands the module held (their `held` and `apply`), `3` = also every other `build_proposal` the simulation applies.
pub const PORTABLE_VAR: &str = "FEVERLINK3_PORTABLE_DUMP";
pub const PORTABLE_FILE: &str = "native_portable.log";
/// `=0` leaves pointers to heap blocks as `P` (no `HeapSize` queries); `=1` turns the graph on for the plain portable dump.
pub const GRAPH_VAR: &str = "FEVERLINK3_PORTABLE_GRAPH";
/// Format version of the text.
pub const VERSION: u32 = 1;
/// The only game build whose layout table this is.
pub const LAYOUT_BUILD: &str = "40420";
/// The kind byte of a `build_proposal` command, at 0x9b8.
pub const KIND_BUILD_PROPOSAL: u8 = 52;

// ---------------------------------------------------------------------------------------------------------------
// the layout table (build 40420)
// ---------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub enum Ty {
    U8,
    U32,
    I32,
    F32,
    /// A 4-byte entity id.
    Ent,
    /// Vec3f.
    V3,
    /// Mat4f.
    M4,
    /// `std::string` (0x20 bytes).
    Str,
    /// `ResName` (0x40 bytes): two strings, the text in the second.
    Res,
    /// A vector (0x18 bytes) of elements of a known type.
    Vec(&'static Elem),
    /// A vector of 4-byte entity ids.
    EntVec,
    /// A vector of elements of an unknown type: kept structurally.
    BlobVec,
    /// `std::map` (0x10 bytes: head pointer, size).
    Map,
    /// `std::unordered_map/set` (0x40 bytes).
    Hash,
    /// Bytes of a known length kept structurally.
    Raw(usize),
    /// Bytes that are not part of the command's input (the engine's result): not written.
    Skip(usize),
    /// Padding inside an element: uninitialised residue (return addresses, stack words), never part of the value; not written.
    Pad(usize),
}

impl Ty {
    pub fn size(self) -> usize {
        match self {
            Ty::U8 => 1,
            Ty::U32 | Ty::I32 | Ty::F32 | Ty::Ent => 4,
            Ty::V3 => 12,
            Ty::M4 => 64,
            Ty::Str => 0x20,
            Ty::Res | Ty::Hash => 0x40,
            Ty::Vec(_) | Ty::EntVec | Ty::BlobVec => 0x18,
            Ty::Map => 0x10,
            Ty::Raw(n) | Ty::Skip(n) | Ty::Pad(n) => n,
        }
    }
}

pub struct F {
    pub off: usize,
    pub name: &'static str,
    pub ty: Ty,
}

const fn f(off: usize, name: &'static str, ty: Ty) -> F {
    F { off, name, ty }
}

/// An element type of a vector.
pub struct Elem {
    pub name: &'static str,
    pub stride: usize,
    pub fields: &'static [F],
    /// (entity offset, position offset): the element says where that entity is.
    pub anchors: &'static [(usize, usize)],
}

pub static NODE: Elem = Elem {
    name: "NodeAndEntity",
    stride: 0x40,
    // entity and position are the node; the other 0x30 bytes are residue (a return address into operator new, 0xaaaaaaab, stale
    // pointers; different on every game and every run, seen in the spike dumps)
    fields: &[f(0, "entity", Ty::Ent), f(4, "position", Ty::V3), f(0x10, "padding", Ty::Pad(0x30))],
    anchors: &[(0, 4)],
};

pub static SEGMENT: Elem = Elem {
    name: "SegmentAndEntity",
    stride: 0x350,
    fields: &[
        f(0, "entity", Ty::Ent),
        f(8, "node0", Ty::Ent),
        f(0xc, "node1", Ty::Ent),
        f(0x10, "position0", Ty::V3),
        f(0x1c, "position1", Ty::V3),
        f(0x28, "tangent0", Ty::V3),
        f(0x34, "tangent1", Ty::V3),
        f(0x40, "baseType", Ty::U32),
        f(0x44, "typeIndex", Ty::I32),
        f(0x48, "objects", Ty::BlobVec),
        f(0x60, "edgeDecorations", Ty::BlobVec),
        f(0x78, "laneConfigs", Ty::BlobVec),
        f(0x98, "roadType", Ty::U32),
        f(0xa0, "roadTemplate", Ty::Res),
        f(0xe0, "roadStyle", Ty::Res),
        f(0x120, "type", Ty::U32),
        f(0x320, "playerOwned", Ty::Ent),
    ],
    anchors: &[(8, 0x10), (0xc, 0x1c)],
};

pub static NODE_CONFIG: Elem =
    Elem { name: "BaseNodeLaneConnectionAndEntity", stride: 0x80, fields: &[f(0x78, "entity", Ty::Ent)], anchors: &[] };

pub static EDGE_OBJECT: Elem = Elem {
    name: "EdgeObject",
    stride: 0x1a0,
    fields: &[
        f(0, "resultEntity", Ty::Ent),
        f(4, "category", Ty::U32),
        f(8, "segmentEntity", Ty::Ent),
        f(0x90, "modelName", Ty::Str),
        f(0x168, "name", Ty::Str),
        f(0x188, "playerEntity", Ty::Ent),
    ],
    anchors: &[],
};

pub static CONSTRUCTION: Elem = Elem {
    name: "ConstructionEntity",
    stride: 0xe48,
    fields: &[
        f(0, "fileName", Ty::Res),
        f(0xc98, "transf", Ty::M4),
        f(0xcd8, "frozenNodes", Ty::EntVec),
        f(0xe18, "name", Ty::Str),
        f(0xe38, "playerEntity", Ty::Ent),
        f(0xe3c, "setAsHeadquarterHack", Ty::U8),
    ],
    anchors: &[],
};

/// The payload of a `build_proposal` command (`WorldBuildProposalCommandData` and the kind byte).
pub static PAYLOAD: &[F] = &[
    f(0x000, "proposal.addedNodes", Ty::Vec(&NODE)),
    f(0x018, "proposal.addedSegments", Ty::Vec(&SEGMENT)),
    f(0x030, "proposal.removedNodes", Ty::Vec(&NODE)),
    f(0x048, "proposal.removedSegments", Ty::Vec(&SEGMENT)),
    f(0x060, "proposal.nodeConfigsToAdd", Ty::Vec(&NODE_CONFIG)),
    f(0x078, "proposal.nodeConfigsToRemove", Ty::EntVec),
    f(0x090, "proposal.hash90", Ty::Hash),
    f(0x0d0, "proposal.hashd0", Ty::Hash),
    f(0x110, "proposal.edgeObjectsToRemove", Ty::EntVec),
    f(0x128, "proposal.edgeObjectsToAdd", Ty::Vec(&EDGE_OBJECT)),
    f(0x140, "proposal.new2oldNodes", Ty::Map),
    f(0x150, "proposal.old2newNodes", Ty::Map),
    f(0x160, "proposal.new2oldSegments", Ty::Map),
    f(0x170, "proposal.old2newSegments", Ty::Map),
    f(0x180, "proposal.new2oldEdgeObjects", Ty::Map),
    f(0x190, "proposal.old2newEdgeObjects", Ty::Map),
    // the tail of the StreetProposal that the Lua binding does not register. The first 0x20 bytes are carried by the engine's own
    // copy constructor (0x4c78c0): an image pointer (constant) or a begin/end range of 20 bytes and two small integers (2, 3) in the
    // stop pair; the word at +0x20 (small integers, a pair of floats, 0x3f800060) is NOT carried (the copy has zeros, seen in the real
    // game) and the word at +0x28 differs (0 against 1): transient state, so 0x10 bytes of padding.
    f(0x1a0, "proposal.gap1a0", Ty::Raw(0x20)),
    f(0x1c0, "proposal.gap1c0", Ty::Pad(0x10)),
    f(0x1d0, "proposal.frozenNodes", Ty::EntVec),
    f(0x1e8, "terrainAlignSkipEdges", Ty::Hash),
    f(0x228, "segmentTags", Ty::BlobVec),
    f(0x240, "toRemove", Ty::EntVec),
    f(0x258, "toAdd", Ty::Vec(&CONSTRUCTION)),
    f(0x270, "old2new", Ty::Hash),
    // a vector of u32 (copied with sar 2 in the engine's copy constructor) and a std::map whose sentinel is 0x50 bytes
    f(0x2b0, "parcelsToRemove", Ty::EntVec),
    f(0x2c8, "parcelsMap", Ty::Map),
    f(0x2d8, "terrain", Ty::Raw(0x80)),
    f(0x358, "context.gatherBuildings", Ty::U8),
    f(0x359, "context.gatherFields", Ty::U8),
    f(0x360, "context.checkTerrainAlignment", Ty::U8),
    f(0x362, "context.cleanupStreetGraph", Ty::U8),
    f(0x36c, "context.player", Ty::Ent),
    // an unordered container (load factor 1.0 at +0, bucket vector, mask 7, maxidx 8 seen at +0x30, +0x38 of it)
    f(0x370, "context.refundableEntities", Ty::Hash),
    f(0x3b2, "context.extendProposalRedoPillars", Ty::U8),
    f(0x3d0, "withCostRep", Ty::U8),
    f(0x3d1, "ignoreErrors", Ty::U8),
    f(0x3d2, "playerInitiated", Ty::U8),
    // the engine's result (ProposalData, resultEntities): filled by the simulation, not input
    f(0x3d8, "result", Ty::Skip(0x9b0 - 0x3d8)),
    f(0x9b0, "doDust", Ty::U8),
    f(0x9b8, "kind", Ty::U8),
];

// ---------------------------------------------------------------------------------------------------------------
// limits and results
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Also dump the `build_proposal` commands the simulation applies that the module did not hold (scripts, replays).
    pub scripts: bool,
    pub max_commands: u32,
    pub max_file_bytes: u64,
    /// One command's text is refused above this.
    pub max_text_bytes: usize,
    /// Most bytes of one heap block read.
    pub max_block_bytes: usize,
    pub max_elems: usize,
    pub max_depth: usize,
    /// Follow pointers to live heap blocks and write the blocks (the heap graph): `FEVERLINK3_PORTABLE_GRAPH=0` turns it off.
    pub graph: bool,
    /// Bounds of the heap graph: blocks, bytes of one block, bytes in all. A pointer past a bound stays a `P`.
    pub max_graph_blocks: usize,
    pub max_graph_block_bytes: usize,
    pub max_graph_bytes: usize,
}

impl Settings {
    /// `None` unless the variable is `1` or `3`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<Settings> {
        Settings::from_var(get, PORTABLE_VAR)
    }

    /// The same switch under another name (the rebuild self-check has its own).
    pub fn from_var(get: impl Fn(&str) -> Option<String>, var: &str) -> Option<Settings> {
        let scripts = match get(var)?.trim() {
            "1" => false,
            "3" => true,
            _ => return None,
        };
        Some(Settings {
            scripts,
            max_commands: 400,
            max_file_bytes: 200 << 20,
            max_text_bytes: 8 << 20,
            max_block_bytes: 8 << 20,
            max_elems: 20_000,
            max_depth: 6,
            // the graph asks the heap about pointers (HeapSize): on for the rebuild checks and the engine copy, opt-in for the plain dump
            graph: get(GRAPH_VAR).map_or(var != PORTABLE_VAR, |v| v.trim() != "0"),
            max_graph_blocks: 4096,
            max_graph_block_bytes: 1 << 20,
            max_graph_bytes: 8 << 20,
        })
    }
}

/// What the encoder noticed about the completeness of the text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub lines: usize,
    /// Heap pointers inside raw regions (not followed: not portable).
    pub heap_ptrs: usize,
    pub strings: usize,
    pub vectors: usize,
    /// Entity references to existing entities the proposal does not locate.
    pub unlocated_refs: usize,
    /// Containers whose entries the table cannot yet read (non-empty hash containers, unknown map entries).
    pub unread_entries: usize,
    /// Heap blocks written as `@B` lines and their bytes (the heap graph behind the pointers).
    pub blocks: usize,
    pub block_bytes: usize,
    /// Pointers left as `P` because a bound of the graph was reached.
    pub graph_truncated: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Portable {
    pub text: String,
    pub stats: Stats,
}

pub struct Header<'a> {
    pub seq: u32,
    pub event: &'a str,
    pub hold_id: Option<u64>,
    pub site_rva: Option<u32>,
    pub tool: Option<&'a str>,
    pub t_ms: u64,
    pub wall_ms: u128,
    pub payload: usize,
}

// ---------------------------------------------------------------------------------------------------------------
// the encoder
// ---------------------------------------------------------------------------------------------------------------

struct Enc<'a> {
    mem: &'a dyn Mem,
    env: Env,
    set: Settings,
    out: String,
    stats: Stats,
    /// entity id -> position (f32 bits), from the elements that locate their entities
    pos: HashMap<u32, [u32; 3]>,
    /// bytes of heap memory read for the structural dump so far
    raw_spent: usize,
    /// the heap graph: blocks reached by pointers, in the order they were found (block `n` is `blocks[n - 1]`)
    blocks: Vec<Blk>,
    by_addr: HashMap<usize, usize>,
    graph_bytes: usize,
    /// inside an engine container's words and the blocks behind them: a pointer is a block of the graph or a `P`, never guessed as a range or vector
    container_mode: bool,
}

struct Blk {
    addr: usize,
    bytes: Vec<u8>,
}

type R<T> = Result<T, String>;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A string with the characters that would break a line escaped.
pub fn escape(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() + 2);
    s.push('"');
    for &b in bytes {
        match b {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            0x20..=0x7e => s.push(b as char),
            _ => {
                let _ = write!(s, "\\x{b:02x}");
            }
        }
    }
    s.push('"');
    s
}

impl Enc<'_> {
    fn line(&mut self, path: &str, value: &str) -> R<()> {
        if self.out.len() > self.set.max_text_bytes {
            return Err("text over the size bound".into());
        }
        self.stats.lines += 1;
        let _ = writeln!(self.out, "{path}\t{value}");
        Ok(())
    }

    /// Exactly `len` bytes at `addr`, or an error.
    fn read(&self, addr: usize, len: usize, what: &str) -> R<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        if len > self.set.max_block_bytes {
            return Err(format!("{what}: block of {len:#x} bytes is over the bound"));
        }
        self.mem.read(addr, len).ok_or_else(|| format!("{what}: {len:#x} bytes at {addr:#x} are not readable"))
    }

    /// The (begin, end, capacity) of a vector, validated: returns the bytes of its elements.
    fn vec_block(&self, t: &[u8], stride: usize, path: &str) -> R<Vec<u8>> {
        let (b, e, c) = (u64_at(t, 0), u64_at(t, 8), u64_at(t, 16));
        if b == 0 {
            return if e == 0 && c == 0 {
                Ok(Vec::new())
            } else {
                Err(format!("{path}: null vector with end/capacity set"))
            };
        }
        if !pointer_like(b) || b > e || e > c || !b.is_multiple_of(4) {
            return Err(format!("{path}: not a vector triple ({b:#x} {e:#x} {c:#x})"));
        }
        let bytes = (e - b) as usize;
        if !bytes.is_multiple_of(stride) {
            return Err(format!("{path}: {bytes:#x} bytes is not a multiple of the element size {stride:#x}"));
        }
        self.read(b as usize, bytes, path)
    }

    fn ent(&mut self, v: u32) -> String {
        if v >= 0x8000_0000 {
            return format!("ent:new#{}", (v as i32).unsigned_abs());
        }
        match self.pos.get(&v) {
            Some(p) => format!("ent:old#{v}@{:08x},{:08x},{:08x}", p[0], p[1], p[2]),
            None => {
                if v != 0 {
                    self.stats.unlocated_refs += 1;
                }
                format!("ent:old#{v}")
            }
        }
    }

    fn string_parts(&self, b: &[u8], path: &str) -> R<Vec<u8>> {
        let (size, cap) = (u64_at(b, 0x10), u64_at(b, 0x18));
        if cap < 15 || size > cap || size > (1 << 20) {
            return Err(format!("{path}: not a std::string (size {size:#x}, cap {cap:#x})"));
        }
        if cap == 15 {
            return Ok(b[..size as usize].to_vec());
        }
        let p = u64_at(b, 0);
        if !pointer_like(p) {
            return Err(format!("{path}: std::string pointer {p:#x}"));
        }
        self.read(p as usize, size as usize, path)
    }

    fn scalar(&mut self, path: &str, ty: Ty, b: &[u8]) -> R<()> {
        let v = match ty {
            Ty::U8 => format!("u8:{:02x}", b[0]),
            Ty::U32 => format!("u32:{:08x}", u32_at(b, 0)),
            Ty::I32 => format!("i32:{:08x}", u32_at(b, 0)),
            Ty::F32 => format!("f32:{:08x}", u32_at(b, 0)),
            Ty::Ent => self.ent(u32_at(b, 0)),
            Ty::V3 => format!("v3:{:08x},{:08x},{:08x}", u32_at(b, 0), u32_at(b, 4), u32_at(b, 8)),
            Ty::M4 => {
                format!("m4:{}", (0..16).map(|i| format!("{:08x}", u32_at(b, i * 4))).collect::<Vec<_>>().join(","))
            }
            _ => unreachable!("not a scalar"),
        };
        self.line(path, &v)
    }

    fn field(&mut self, path: &str, ty: Ty, b: &[u8], depth: usize) -> R<()> {
        match ty {
            Ty::U8 | Ty::U32 | Ty::I32 | Ty::F32 | Ty::Ent | Ty::V3 | Ty::M4 => self.scalar(path, ty, b),
            Ty::Str => {
                let s = self.string_parts(b, path)?;
                self.stats.strings += 1;
                self.line(path, &format!("str:{}", escape(&s)))
            }
            Ty::Res => {
                let first = self.string_parts(&b[..0x20], path)?;
                let second = self.string_parts(&b[0x20..0x40], path)?;
                self.stats.strings += 2;
                self.line(path, &format!("res:{}", escape(&second)))?;
                if !first.is_empty() {
                    self.line(&format!("{path}.first"), &format!("res:{}", escape(&first)))?;
                }
                Ok(())
            }
            Ty::EntVec => {
                let block = self.vec_block(b, 4, path)?;
                self.stats.vectors += 1;
                self.line(&format!("{path}.#"), &format!("n:{}", block.len() / 4))?;
                for (i, c) in block.chunks(4).enumerate() {
                    let v = self.ent(u32_at(c, 0));
                    self.line(&format!("{path}[{i}]"), &v)?;
                }
                Ok(())
            }
            Ty::Vec(elem) => {
                let block = self.vec_block(b, elem.stride, path)?;
                let n = block.len() / elem.stride;
                if n > self.set.max_elems {
                    return Err(format!("{path}: {n} elements are over the bound"));
                }
                self.stats.vectors += 1;
                self.line(&format!("{path}.#"), &format!("n:{n}"))?;
                for i in 0..n {
                    let item = &block[i * elem.stride..(i + 1) * elem.stride];
                    self.record(&format!("{path}[{i}]"), item, elem.fields, elem.stride, depth + 1)?;
                }
                Ok(())
            }
            Ty::BlobVec => {
                let block = self.vec_block(b, 1, path)?;
                self.stats.vectors += 1;
                self.line(&format!("{path}.bytes"), &format!("n:{}", block.len()))?;
                if block.iter().any(|&x| x != 0) {
                    let t = self.raw_tokens(&block, depth + 1)?;
                    self.line(&format!("{path}.raw"), &format!("raw:{t}"))?;
                }
                Ok(())
            }
            Ty::Map => {
                // std::map: head pointer and size; the nodes are heap blocks of the graph
                self.line(path, &format!("map:size={}", u64_at(b, 8)))?;
                self.container_graph(path, b, depth)
            }
            Ty::Hash => {
                // std::unordered_map/set: load factor (the other four bytes of its word are padding), then the list head, size,
                // bucket vector, mask and maximum index; the nodes are heap blocks of the graph
                self.line(path, &format!("hash:size={},load={:08x}", u64_at(b, 0x10), u32_at(b, 0)))?;
                self.container_graph(path, &b[8..], depth)
            }
            Ty::Raw(_) | Ty::Skip(_) | Ty::Pad(_) => Ok(()),
        }
    }

    /// The words of an engine container written structurally (`path.graph`): its pointers become blocks of the heap graph.
    fn container_graph(&mut self, path: &str, bytes: &[u8], depth: usize) -> R<()> {
        if bytes.iter().all(|&x| x == 0) {
            return Ok(());
        }
        let was = std::mem::replace(&mut self.container_mode, true);
        let t = self.raw_tokens(bytes, depth + 1);
        self.container_mode = was;
        let t = t?;
        if t.split(' ').any(|x| x == "P") {
            self.stats.unread_entries += 1;
        }
        self.line(&format!("{path}.graph"), &format!("raw:{t}"))
    }

    /// A pointer to a live heap block as a token of the heap graph: `B<n>` (the start of block n) or `B<n>+<hex>` (inside it).
    /// The first pointer to a block reads it (its size comes from the heap) and queues its own pointers.
    fn block_token(&mut self, p: usize) -> Option<String> {
        if !self.set.graph {
            return None;
        }
        if let Some(&n) = self.by_addr.get(&p) {
            return Some(format!("B{n}"));
        }
        for (i, b) in self.blocks.iter().enumerate() {
            if p > b.addr && p <= b.addr + b.bytes.len() {
                return Some(format!("B{}+{:x}", i + 1, p - b.addr));
            }
        }
        let size = self.mem.heap_size(p)?;
        if size > self.set.max_graph_block_bytes
            || self.blocks.len() >= self.set.max_graph_blocks
            || self.graph_bytes + size > self.set.max_graph_bytes
        {
            self.stats.graph_truncated += 1;
            return None;
        }
        let bytes = self.mem.read(p, size)?;
        self.graph_bytes += size;
        self.blocks.push(Blk { addr: p, bytes });
        let n = self.blocks.len();
        self.by_addr.insert(p, n);
        Some(format!("B{n}"))
    }

    /// The members of a record of `size` bytes, and the structural dump of the bytes the table does not name.
    fn record(&mut self, path: &str, bytes: &[u8], fields: &[F], size: usize, depth: usize) -> R<()> {
        if depth > self.set.max_depth {
            return Err(format!("{path}: nesting is over the bound"));
        }
        let mut order: Vec<&F> = fields.iter().collect();
        order.sort_by_key(|f| f.off);
        let mut cursor = 0usize;
        for fl in order {
            if fl.off < cursor {
                return Err(format!("layout table: {} overlaps the previous member", fl.name));
            }
            if fl.off > cursor {
                self.gap(path, bytes, cursor, fl.off, depth)?;
            }
            let end = fl.off + fl.ty.size();
            if end > size {
                return Err(format!("layout table: {} ends past the record", fl.name));
            }
            let full = if path.is_empty() { fl.name.to_string() } else { format!("{path}.{}", fl.name) };
            if let Ty::Raw(_) = fl.ty {
                self.gap_named(&full, bytes, fl.off, end, depth)?;
            } else {
                self.field(&full, fl.ty, &bytes[fl.off..end], depth)?;
            }
            cursor = end;
        }
        if cursor < size {
            self.gap(path, bytes, cursor, size, depth)?;
        }
        Ok(())
    }

    fn gap(&mut self, path: &str, bytes: &[u8], from: usize, to: usize, depth: usize) -> R<()> {
        let name = if path.is_empty() { format!("~{from:#x}") } else { format!("{path}.~{from:#x}") };
        self.gap_named(&name, bytes, from, to, depth)
    }

    fn gap_named(&mut self, name: &str, bytes: &[u8], from: usize, to: usize, depth: usize) -> R<()> {
        let part = &bytes[from..to];
        if part.iter().all(|&b| b == 0) {
            return Ok(());
        }
        if from.is_multiple_of(8) && part.len().is_multiple_of(8) {
            let t = self.raw_tokens(part, depth + 1)?;
            self.line(name, &format!("raw:{t}"))
        } else {
            self.line(name, &format!("raw:b:{}", hex(part)))
        }
    }

    // ---- structural bytes

    /// Tokens for `bytes` (a multiple of 8 long, or the tail as `b:`).
    fn raw_tokens(&mut self, bytes: &[u8], depth: usize) -> R<String> {
        if depth > self.set.max_depth {
            return Err("raw nesting is over the bound".into());
        }
        let mut toks: Vec<String> = Vec::new();
        let words = bytes.len() / 8;
        let mut i = 0;
        while i < words {
            let w = u64_at(bytes, i * 8);
            if w == 0 {
                let mut j = i;
                while j < words && u64_at(bytes, j * 8) == 0 {
                    j += 1;
                }
                toks.push(format!("z{}", j - i));
                i = j;
                continue;
            }
            // a std::string: 16 bytes of buffer or pointer, size, capacity
            if i + 4 <= words {
                let (size, cap) = (u64_at(bytes, (i + 2) * 8), u64_at(bytes, (i + 3) * 8));
                if cap >= 15 && size <= cap && size <= (1 << 16) {
                    let inline =
                        cap == 15 && bytes[i * 8..i * 8 + size as usize].iter().all(|&b| (0x20..0x7f).contains(&b));
                    let heap = cap > 15 && cap % 16 == 15 && pointer_like(w);
                    let text = if inline {
                        Some(bytes[i * 8..i * 8 + size as usize].to_vec())
                    } else if heap {
                        self.mem.read(w as usize, size as usize).filter(|t| ascii_string(t, 0, Some(t.len())).is_some())
                    } else {
                        None
                    };
                    if let Some(t) = text {
                        if size > 0 || cap > 15 {
                            self.stats.strings += 1;
                            toks.push(format!("S{size}:{}", hex(&t)));
                            i += 4;
                            continue;
                        }
                    }
                }
            }
            // a vector: begin, end, capacity
            if i + 3 <= words && depth < self.set.max_depth && !self.container_mode {
                let (b, e, c) = (w, u64_at(bytes, (i + 1) * 8), u64_at(bytes, (i + 2) * 8));
                if pointer_like(b)
                    && b <= e
                    && e <= c
                    && b.is_multiple_of(4)
                    && (e - b) as usize <= self.set.max_block_bytes
                {
                    let len = (e - b) as usize;
                    self.raw_spent += len;
                    if self.raw_spent > self.set.max_text_bytes {
                        return Err("structural dump reads more heap than the bound".into());
                    }
                    if len == 0 {
                        if self.mem.read(b as usize, 1).is_some() {
                            self.stats.vectors += 1;
                            toks.push("V0[]".into());
                            i += 3;
                            continue;
                        }
                    } else if let Some(block) = self.mem.read(b as usize, len) {
                        self.stats.vectors += 1;
                        let inner = if len.is_multiple_of(8) {
                            self.raw_tokens(&block, depth + 1)?
                        } else {
                            format!("b:{}", hex(&block))
                        };
                        toks.push(format!("V{len}[{inner}]"));
                        i += 3;
                        continue;
                    }
                }
            }
            if pointer_like(w) {
                let p = w as usize;
                if p >= self.env.image_base && p - self.env.image_base < IMAGE_BOUND {
                    toks.push(format!("I+{:x}", p - self.env.image_base));
                    i += 1;
                    continue;
                }
                // with the heap graph on, a pointer to a live block is a block reference, whatever its neighbours are
                if let Some(t) = self.block_token(p) {
                    toks.push(t);
                    i += 1;
                    continue;
                }
                // a range: two pointers into one block, begin and end (no capacity word), a few KB at most
                if i + 2 <= words && depth < self.set.max_depth && !self.container_mode {
                    let e = u64_at(bytes, (i + 1) * 8);
                    if pointer_like(e) && w <= e && e - w <= 0x1000 && w.is_multiple_of(4) {
                        let len = (e - w) as usize;
                        let block =
                            if len == 0 { self.mem.read(p, 1).map(|_| Vec::new()) } else { self.mem.read(p, len) };
                        if let Some(block) = block {
                            self.raw_spent += len;
                            let inner = if len == 0 {
                                String::new()
                            } else if len.is_multiple_of(8) {
                                self.raw_tokens(&block, depth + 1)?
                            } else {
                                format!("b:{}", hex(&block))
                            };
                            toks.push(format!("R{len}[{inner}]"));
                            i += 2;
                            continue;
                        }
                    }
                }
                if self.mem.read(p, 1).is_some() {
                    self.stats.heap_ptrs += 1;
                    toks.push("P".into());
                    i += 1;
                    continue;
                }
            }
            toks.push(format!("w:{w:016x}"));
            i += 1;
        }
        if !bytes.len().is_multiple_of(8) {
            toks.push(format!("b:{}", hex(&bytes[words * 8..])));
        }
        Ok(toks.join(" "))
    }
}

/// True when every vector of items in the payload (the table's `Vec` and `EntVec` members: added and removed nodes,
/// segments, configs, edge objects, constructions, removals) is empty: a command that changes nothing. `None` when the
/// payload cannot be read. Used to keep no-op script commands out of the level-3 dumps; the rule is the table's, not a kind's.
pub fn proposal_is_empty(mem: &dyn Mem, payload: usize) -> Option<bool> {
    let bytes = mem.read(payload, PAYLOAD_SIZE)?;
    Some(
        PAYLOAD
            .iter()
            .filter(|f| matches!(f.ty, Ty::Vec(_) | Ty::EntVec))
            .all(|f| u64_at(&bytes, f.off) == u64_at(&bytes, f.off + 8)),
    )
}

/// The portable text of the `build_proposal` payload at `head.payload`, or why it is refused.
pub fn encode(mem: &dyn Mem, env: Env, set: Settings, build: &str, head: &Header) -> Result<Portable, String> {
    if build != LAYOUT_BUILD {
        return Err(format!("layout table is for game build {LAYOUT_BUILD}, this is {build}"));
    }
    let payload = mem.read(head.payload, PAYLOAD_SIZE).ok_or("payload unreadable")?;
    if payload[0x9b8] != KIND_BUILD_PROPOSAL {
        return Err(format!("kind byte {} is not {KIND_BUILD_PROPOSAL}", payload[0x9b8]));
    }
    let mut e = Enc {
        mem,
        env,
        set,
        out: String::new(),
        stats: Stats::default(),
        pos: HashMap::new(),
        raw_spent: 0,
        blocks: Vec::new(),
        by_addr: HashMap::new(),
        graph_bytes: 0,
        container_mode: false,
    };
    // pass 1: where the elements say their entities are
    for fl in PAYLOAD {
        if let Ty::Vec(elem) = fl.ty {
            if elem.anchors.is_empty() {
                continue;
            }
            let block = e.vec_block(&payload[fl.off..fl.off + 0x18], elem.stride, fl.name)?;
            for item in block.chunks(elem.stride) {
                for &(id_off, pos_off) in elem.anchors {
                    let id = u32_at(item, id_off);
                    if id != 0 && id < 0x8000_0000 {
                        e.pos.insert(id, [u32_at(item, pos_off), u32_at(item, pos_off + 4), u32_at(item, pos_off + 8)]);
                    }
                }
            }
        }
    }
    e.record("", &payload, PAYLOAD, PAYLOAD_SIZE, 0)?;
    // the heap graph: every block found, written in the order found (writing one can find more)
    let mut block_lines = String::new();
    let mut done = 0usize;
    while done < e.blocks.len() {
        let bytes = e.blocks[done].bytes.clone();
        e.container_mode = true;
        let tokens = e.raw_tokens(&bytes, 1)?;
        e.container_mode = false;
        let _ = writeln!(block_lines, "@B{}\tsize={}\traw:{}", done + 1, bytes.len(), tokens);
        done += 1;
        if block_lines.len() + e.out.len() > e.set.max_text_bytes {
            return Err("text over the size bound".into());
        }
    }
    e.stats.blocks = e.blocks.len();
    e.stats.block_bytes = e.graph_bytes;
    e.stats.lines += e.blocks.len();
    e.out.push_str(&block_lines);
    let mut text = String::new();
    let _ = writeln!(
        text,
        "=== portable v{VERSION} layout={LAYOUT_BUILD} event={} key={} site={} tool={} seq={} t_ms={} wall_ms={} payload={:#x} ===",
        head.event,
        head.hold_id.map_or("none".into(), |k| k.to_string()),
        head.site_rva.map_or("none".into(), |s| format!("{s:#x}")),
        head.tool.unwrap_or("none"),
        head.seq,
        head.t_ms,
        head.wall_ms,
        head.payload
    );
    text.push_str(&e.out);
    let s = e.stats;
    let _ = writeln!(
        text,
        "=== end portable #{} lines={} strings={} vectors={} heap_ptrs_not_followed={} unlocated_refs={} unread_entries={} blocks={} block_bytes={} graph_truncated={} ===",
        head.seq,
        s.lines,
        s.strings,
        s.vectors,
        s.heap_ptrs,
        s.unlocated_refs,
        s.unread_entries,
        s.blocks,
        s.block_bytes,
        s.graph_truncated
    );
    Ok(Portable { text, stats: s })
}

// ---------------------------------------------------------------------------------------------------------------
// the file
// ---------------------------------------------------------------------------------------------------------------

/// Where the portable texts go, and how many have been written.
pub struct Sink {
    pub settings: Settings,
    file: &'static str,
    dir: Mutex<Option<PathBuf>>,
    seq: AtomicU32,
    bytes: AtomicU64,
}

impl Sink {
    pub fn new(settings: Settings) -> Sink {
        Sink::named(settings, PORTABLE_FILE)
    }

    /// A sink that writes another file of the live folder.
    pub fn named(settings: Settings, file: &'static str) -> Sink {
        Sink { settings, file, dir: Mutex::new(None), seq: AtomicU32::new(0), bytes: AtomicU64::new(0) }
    }

    /// Chooses the folder; an earlier run's file is set aside.
    pub fn set_dir(&self, dir: &Path) {
        if self.dir.lock().unwrap_or_else(|p| p.into_inner()).as_deref() == Some(dir) {
            return;
        }
        let path = dir.join(self.file);
        if path.exists() {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
        *self.dir.lock().unwrap_or_else(|p| p.into_inner()) = Some(dir.to_path_buf());
    }

    pub fn has_dir(&self) -> bool {
        self.dir.lock().unwrap_or_else(|p| p.into_inner()).is_some()
    }

    /// A sequence number, or `None` when the command or byte budget is used up.
    pub fn begin(&self) -> Option<u32> {
        if self.bytes.load(Ordering::Relaxed) >= self.settings.max_file_bytes {
            return None;
        }
        let n = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        (n <= self.settings.max_commands).then_some(n)
    }

    pub fn write(&self, text: &str) -> std::io::Result<()> {
        use std::io::Write;
        let Some(dir) = self.dir.lock().unwrap_or_else(|p| p.into_inner()).clone() else {
            return Err(std::io::Error::other("no folder"));
        };
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(self.file))?;
        f.write_all(text.as_bytes())?;
        self.bytes.fetch_add(text.len() as u64, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod testdata {
    use super::*;

    pub(crate) struct Fake(pub(crate) Vec<(usize, Vec<u8>)>);

    impl Mem for Fake {
        fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
            self.0.iter().find_map(|(s, b)| {
                (addr >= *s && addr + len <= s + b.len()).then(|| b[addr - s..addr - s + len].to_vec())
            })
        }
    }

    pub(crate) fn put(buf: &mut [u8], off: usize, v: u64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn put32(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    pub(crate) const BASE: usize = 0x1_4000_0000;
    pub(crate) const HEAP: usize = 0x2000_0000_0000;

    pub(crate) fn head(payload: usize) -> Header<'static> {
        Header {
            seq: 1,
            event: "held",
            hold_id: Some(3),
            site_rva: Some(0x51b56c),
            tool: Some("ConstructionBuilder"),
            t_ms: 5,
            wall_ms: 6,
            payload,
        }
    }

    /// A std::string at `buf[off..off+0x20]`: inline when short, else pointing at `heap_at` (the caller adds the region).
    pub(crate) fn string(buf: &mut [u8], off: usize, text: &[u8], heap_at: usize) {
        if text.len() <= 15 {
            buf[off..off + text.len()].copy_from_slice(text);
            put(buf, off + 0x10, text.len() as u64);
            put(buf, off + 0x18, 15);
        } else {
            put(buf, off, heap_at as u64);
            put(buf, off + 0x10, text.len() as u64);
            put(buf, off + 0x18, (text.len() | 15) as u64);
        }
    }

    /// A depot-like proposal: 2 added nodes (placeholders -1 and -2), a removed node 39112, one added segment between
    /// -1 and the existing node 39106, a template resource name on the heap, a construction, toRemove of two buildings.
    pub(crate) fn synthetic() -> (Fake, usize) {
        let pay = HEAP;
        let nodes = HEAP + 0x1000;
        let rnodes = HEAP + 0x2000;
        let segs = HEAP + 0x3000;
        let rm = HEAP + 0x8000;
        let cons = HEAP + 0x9000;
        let tmpl = HEAP + 0x20000;
        let nm = HEAP + 0x21000;
        let mut p = vec![0u8; PAYLOAD_SIZE];
        put(&mut p, 0x00, nodes as u64);
        put(&mut p, 0x08, (nodes + 0x80) as u64);
        put(&mut p, 0x10, (nodes + 0x80) as u64);
        put(&mut p, 0x30, rnodes as u64);
        put(&mut p, 0x38, (rnodes + 0x40) as u64);
        put(&mut p, 0x40, (rnodes + 0x40) as u64);
        put(&mut p, 0x18, segs as u64);
        put(&mut p, 0x20, (segs + 0x350) as u64);
        put(&mut p, 0x28, (segs + 0x350) as u64);
        put(&mut p, 0x240, rm as u64);
        put(&mut p, 0x248, (rm + 8) as u64);
        put(&mut p, 0x250, (rm + 8) as u64);
        put(&mut p, 0x258, cons as u64);
        put(&mut p, 0x260, (cons + 0xe48) as u64);
        put(&mut p, 0x268, (cons + 0xe48) as u64);
        p[0x358] = 1;
        put32(&mut p, 0x36c, 34135);
        p[0x3d2] = 1;
        p[0x9b8] = 52;
        let mut n = vec![0u8; 0x80];
        put32(&mut n, 0, (-1i32) as u32);
        put32(&mut n, 4, 1388.276f32.to_bits());
        put32(&mut n, 8, 304.344f32.to_bits());
        put32(&mut n, 12, 5.374f32.to_bits());
        put32(&mut n, 0x40, (-2i32) as u32);
        put32(&mut n, 0x44, 1405.446f32.to_bits());
        let mut rn = vec![0u8; 0x40];
        put32(&mut rn, 0, 39112);
        put32(&mut rn, 4, 1391.5f32.to_bits());
        let mut s = vec![0u8; 0x350];
        put32(&mut s, 0, (-4i32) as u32);
        put32(&mut s, 8, 39106);
        put32(&mut s, 0xc, (-1i32) as u32);
        put32(&mut s, 0x10, 1369.414f32.to_bits());
        put32(&mut s, 0x1c, 1388.276f32.to_bits());
        put32(&mut s, 0x44, (-1i32) as u32);
        put32(&mut s, 0x98, 1);
        let t = b"infrastructure/street/town/town_new_small.street_template";
        string(&mut s, 0xa0, b"", 0);
        string(&mut s, 0xa0 + 0x20, t, tmpl);
        string(&mut s, 0xe0, b"", 0);
        string(&mut s, 0xe0 + 0x20, b"town_new.street", 0);
        let mut c = vec![0u8; 0xe48];
        string(&mut c, 0, b"", 0);
        string(&mut c, 0x20, b"depots/road/road_depot/road_depot.con", nm);
        string(&mut c, 0xe18, b"Road Depot", 0);
        put32(&mut c, 0xe38, 34135);
        // an image pointer and a heap pointer in the unnamed part
        put(&mut c, 0x100, (BASE + 0x500) as u64);
        put(&mut c, 0x108, nm as u64);
        let mut ids = vec![0u8; 8];
        put32(&mut ids, 0, 39259);
        put32(&mut ids, 4, 39481);
        let mut img = vec![0u8; 0x600];
        img[0x500..0x508].copy_from_slice(b"script\0\0");
        (
            Fake(vec![
                (pay, p),
                (nodes, n),
                (rnodes, rn),
                (segs, s),
                (rm, ids),
                (cons, c),
                (tmpl, t.iter().copied().chain([0]).collect()),
                (nm, b"depots/road/road_depot/road_depot.con\0".to_vec()),
                (BASE, img),
            ]),
            pay,
        )
    }

    /// The memory the decoder saw in `native_dump.log` (the dump of the earlier spike step), as regions: the payload
    /// from its HEX rows, every block from its word lines, vector ends and string sizes restored from their
    /// descriptions, string texts placed behind their pointers.
    pub(crate) fn model_of(dump: &str) -> (Fake, usize) {
        let mut regions: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut payload_at = 0usize;
        let mut payload = vec![0u8; PAYLOAD_SIZE];
        let mut block: Option<(usize, Vec<u8>)> = None;
        let num = |t: &str| usize::from_str_radix(t.trim_start_matches("0x"), 16).unwrap();
        let flush = |b: &mut Option<(usize, Vec<u8>)>, regions: &mut Vec<(usize, Vec<u8>)>| {
            if let Some(x) = b.take() {
                regions.push(x);
            }
        };
        for line in dump.lines() {
            if let Some(rest) = line.strip_prefix("=== dump #") {
                payload_at = num(rest.split("payload=").nth(1).unwrap().split_whitespace().next().unwrap());
            } else if let Some(rest) = line.strip_prefix("HEX +") {
                let (off, bytes) = rest.split_once(' ').unwrap();
                if let Some((a, _)) = off.split_once("..") {
                    let _ = a; // a zero range: the buffer is zero already
                } else {
                    for (i, h) in bytes.split(' ').enumerate() {
                        payload[num(off) + i] = u8::from_str_radix(h, 16).unwrap();
                    }
                }
            } else if line.starts_with('B') && line.contains(" @0x") {
                flush(&mut block, &mut regions);
                let addr = num(line.split(" @").nth(1).unwrap().split_whitespace().next().unwrap());
                let read = line.rsplit_once("(0x").map_or(0x1000, |(_, r)| num(r.split(' ').next().unwrap()));
                block = Some((addr, vec![0u8; read]));
            } else if let Some(rest) = line.strip_prefix("  +0x") {
                let Some((_, buf)) = block.as_mut() else { continue };
                if rest.contains("..") {
                    continue;
                }
                let mut it = rest.splitn(4, ' ');
                let off = num(it.next().unwrap());
                let (lo, hi) = (
                    u32::from_str_radix(it.next().unwrap(), 16).unwrap(),
                    u32::from_str_radix(it.next().unwrap(), 16).unwrap(),
                );
                if off + 8 > buf.len() {
                    continue;
                }
                buf[off..off + 4].copy_from_slice(&lo.to_le_bytes());
                buf[off + 4..off + 8].copy_from_slice(&hi.to_le_bytes());
                let desc = it.next().unwrap_or("").trim();
                let ptr = (u64::from(hi) << 32 | u64::from(lo)) as usize;
                if let Some(r) = desc.strip_prefix("vector 0x") {
                    let bytes = usize::from_str_radix(r.split(' ').next().unwrap(), 16).unwrap();
                    let cap = usize::from_str_radix(r.split("(cap 0x").nth(1).unwrap().split(')').next().unwrap(), 16)
                        .unwrap();
                    put(buf, off + 8, (ptr + bytes) as u64);
                    put(buf, off + 16, (ptr + cap) as u64);
                } else if let Some(r) = desc.strip_prefix("std::string \"") {
                    let (text, tail) = r.rsplit_once("\" (size ").unwrap();
                    let size: usize = tail.split(',').next().unwrap().trim().parse().unwrap();
                    let cap: usize =
                        tail.split("cap ").nth(1).unwrap().split(')').next().unwrap().trim().parse().unwrap();
                    put(buf, off + 16, size as u64);
                    put(buf, off + 24, cap as u64);
                    if cap == 15 {
                        buf[off..off + text.len()].copy_from_slice(text.as_bytes());
                    } else {
                        let mut t = text.as_bytes().to_vec();
                        t.push(0);
                        regions.push((ptr, t));
                    }
                } else if desc.starts_with("std::string \"\"") {
                    put(buf, off + 16, 0);
                    put(buf, off + 24, 15);
                }
            }
        }
        flush(&mut block, &mut regions);
        regions.push((payload_at, payload));
        (Fake(regions), payload_at)
    }
}

#[cfg(test)]
mod tests {
    use super::testdata::*;
    use super::*;
    use crate::testutil::TempDir;

    fn settings() -> Settings {
        Settings::from_lookup(|_| Some("1".into())).unwrap()
    }

    fn lines(p: &Portable) -> Vec<&str> {
        p.text.lines().collect()
    }

    #[test]
    fn the_switch_is_off_unless_it_is_one_or_three() {
        let get = |v: &'static str| move |n: &str| (n == PORTABLE_VAR).then(|| v.to_string());
        assert!(Settings::from_lookup(|_| None).is_none());
        assert!(Settings::from_lookup(get("0")).is_none());
        assert!(Settings::from_lookup(get("2")).is_none());
        assert!(!Settings::from_lookup(get("1")).unwrap().scripts);
        assert!(Settings::from_lookup(get("3")).unwrap().scripts);
    }

    #[test]
    fn a_proposal_is_written_by_table_with_ids_as_placeholders_or_located_references() {
        let (mem, at) = synthetic();
        let p = encode(&mem, Env { image_base: BASE }, settings(), "40420", &head(at)).expect("encodes");
        let l = lines(&p);
        assert!(
            l[0].starts_with("=== portable v1 layout=40420 event=held key=3 site=0x51b56c tool=ConstructionBuilder"),
            "{}",
            p.text
        );
        let has = |s: &str| l.contains(&s);
        assert!(has("proposal.addedNodes.#\tn:2"), "{}", p.text);
        assert!(has("proposal.addedNodes[0].entity\tent:new#1"), "{}", p.text);
        assert!(has("proposal.addedNodes[1].entity\tent:new#2"));
        assert!(has(&format!(
            "proposal.addedNodes[0].position\tv3:{:08x},{:08x},{:08x}",
            1388.276f32.to_bits(),
            304.344f32.to_bits(),
            5.374f32.to_bits()
        )));
        // the removed node is located by its own position; the existing node of the segment by the segment's end
        assert!(l.iter().any(|x| x.starts_with("proposal.removedNodes[0].entity\tent:old#39112@")), "{}", p.text);
        let end0 = format!("proposal.addedSegments[0].node0\tent:old#39106@{:08x}", 1369.414f32.to_bits());
        assert!(l.iter().any(|x| x.starts_with(&end0)), "{}", p.text);
        assert!(has("proposal.addedSegments[0].node1\tent:new#1"));
        assert!(has(
            "proposal.addedSegments[0].roadTemplate\tres:\"infrastructure/street/town/town_new_small.street_template\""
        ));
        // entity vector with ids nothing locates
        assert!(
            has("toRemove.#\tn:2") && has("toRemove[0]\tent:old#39259") && has("toRemove[1]\tent:old#39481"),
            "{}",
            p.text
        );
        // the two buildings and the player (context and construction): existing entities nothing in the proposal locates
        assert_eq!(p.stats.unlocated_refs, 4, "{}", p.text);
        // construction: named members, and the unnamed part structurally
        assert!(has("toAdd[0].fileName\tres:\"depots/road/road_depot/road_depot.con\""));
        assert!(has("toAdd[0].name\tstr:\"Road Depot\""));
        assert!(has("toAdd[0].playerEntity\tent:old#34135"));
        let raw = l.iter().find(|x| x.starts_with("toAdd[0].~0x")).expect("a raw region").to_string();
        assert!(raw.contains("I+500") && raw.contains(" P"), "image pointer as an offset, heap pointer marked: {raw}");
        assert!(has("context.gatherBuildings\tu8:01") && has("playerInitiated\tu8:01") && has("kind\tu8:34"));
        assert!(
            !p.text.lines().skip(1).any(|x| x.contains("0x2000") || x.contains("20000000")),
            "no heap address in the body"
        );
        assert!(l.last().unwrap().starts_with("=== end portable #1"));
        assert_eq!(p.stats.heap_ptrs, 1);
    }

    #[test]
    fn a_proposal_with_nothing_to_add_or_remove_is_empty() {
        let (mem, at) = synthetic();
        assert_eq!(proposal_is_empty(&mem, at), Some(false));
        let zero = Fake(vec![(at, vec![0u8; PAYLOAD_SIZE])]);
        assert_eq!(proposal_is_empty(&zero, at), Some(true));
        assert_eq!(proposal_is_empty(&Fake(vec![]), at), None);
        // only the removal of one building: not empty
        let mut one = vec![0u8; PAYLOAD_SIZE];
        put(&mut one, 0x240, HEAP as u64 + 0x100);
        put(&mut one, 0x248, HEAP as u64 + 0x104);
        assert_eq!(proposal_is_empty(&Fake(vec![(at, one)]), at), Some(false));
    }

    #[test]
    fn it_refuses_what_it_does_not_know() {
        let (mem, at) = synthetic();
        let env = Env { image_base: BASE };
        assert!(encode(&mem, env, settings(), "40421", &head(at)).unwrap_err().contains("game build 40420"));
        let (mut bad, at) = synthetic();
        bad.0[0].1[0x9b8] = 17;
        assert!(encode(&bad, env, settings(), "40420", &head(at)).unwrap_err().contains("kind byte"));
        // a vector whose size is not a multiple of the element
        let (mut bad, at) = synthetic();
        let end = u64::from_le_bytes(bad.0[0].1[0x08..0x10].try_into().unwrap()) - 4;
        bad.0[0].1[0x08..0x10].copy_from_slice(&end.to_le_bytes());
        assert!(encode(&bad, env, settings(), "40420", &head(at)).unwrap_err().contains("not a multiple"));
        // a vector over unreadable memory
        let (mut bad, at) = synthetic();
        bad.0[0].1[0x00..0x08].copy_from_slice(&0x7777_0000_0000u64.to_le_bytes());
        bad.0[0].1[0x08..0x10].copy_from_slice(&0x7777_0000_0080u64.to_le_bytes());
        bad.0[0].1[0x10..0x18].copy_from_slice(&0x7777_0000_0080u64.to_le_bytes());
        assert!(encode(&bad, env, settings(), "40420", &head(at)).unwrap_err().contains("not readable"));
        assert!(encode(&Fake(vec![]), env, settings(), "40420", &head(0x5555_0000))
            .unwrap_err()
            .contains("unreadable"));
    }

    #[test]
    fn the_text_is_bounded() {
        let (mem, at) = synthetic();
        let tiny = Settings { max_text_bytes: 100, ..settings() };
        assert!(encode(&mem, Env { image_base: BASE }, tiny, "40420", &head(at)).unwrap_err().contains("size bound"));
        let few = Settings { max_elems: 1, ..settings() };
        assert!(encode(&mem, Env { image_base: BASE }, few, "40420", &head(at)).is_err());
    }

    #[test]
    fn the_table_is_consistent() {
        // members do not overlap and fit their records
        for (name, fields, size) in [
            ("payload", PAYLOAD, PAYLOAD_SIZE),
            ("node", NODE.fields, NODE.stride),
            ("segment", SEGMENT.fields, SEGMENT.stride),
            ("node config", NODE_CONFIG.fields, NODE_CONFIG.stride),
            ("edge object", EDGE_OBJECT.fields, EDGE_OBJECT.stride),
            ("construction", CONSTRUCTION.fields, CONSTRUCTION.stride),
        ] {
            let mut v: Vec<&F> = fields.iter().collect();
            v.sort_by_key(|f| f.off);
            let mut end = 0;
            for fl in v {
                assert!(fl.off >= end, "{name}.{} overlaps", fl.name);
                end = fl.off + fl.ty.size();
                assert!(end <= size, "{name}.{} past the end", fl.name);
            }
        }
    }

    // ---- real data: the commands of the spike run (local research files; skipped when they are not there)

    #[test]
    fn the_real_commands_of_the_spike_run_are_encoded_or_refused_with_a_reason() {
        let Ok(dump) = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../docs/research/spike-dumps-20261013/guest/native_dump.log"
        )) else {
            return; // local research files are not in a clean checkout
        };
        // dump #1 is the plain road, #4 the bus stop pair, #13 the rail track, #19 the house bulldozing
        let mut by_seq: Vec<(u32, String)> = Vec::new();
        for part in dump.split("=== dump #").skip(1) {
            let n: u32 = part.split(' ').next().unwrap().parse().unwrap();
            by_seq.push((n, format!("=== dump #{part}")));
        }
        let mut done = 0;
        for (n, text) in by_seq.iter().filter(|(n, _)| [1, 4, 13, 19, 20].contains(n)) {
            let (mem, at) = model_of(text);
            let p = encode(&mem, Env { image_base: 0x7ff7_1400_0000 }, settings(), "40420", &head(at))
                .unwrap_or_else(|e| panic!("dump #{n} is refused: {e}"));
            if let Some(out) = std::env::var_os("FL3_PORTABLE_TEST_OUT") {
                let _ = std::fs::write(std::path::Path::new(&out).join(format!("portable_{n}.txt")), &p.text);
            }
            assert!(p.text.contains("kind	u8:34"), "dump #{n}");
            assert!(p.stats.lines > 20, "dump #{n}");
            done += 1;
            if *n == 1 {
                // the plain road: 2 new nodes, 1 new segment, 2 node configs to add, the player as the owner
                assert!(
                    p.text.contains("proposal.addedNodes.#	n:2") && p.text.contains("proposal.addedSegments.#	n:1"),
                    "{}",
                    p.text
                );
                assert!(p.text.contains("proposal.nodeConfigsToAdd.#	n:2"), "{}", p.text);
                assert!(p.text.contains("proposal.addedSegments[0].roadTemplate	res:\""), "{}", p.text);
            }
        }
        assert_eq!(done, 5);
    }

    #[test]
    fn the_sink_counts_commands_and_sets_an_old_file_aside() {
        let dir = TempDir::new("portable-sink");
        let s = Sink::new(Settings { max_commands: 2, ..settings() });
        assert!(s.write("x").is_err());
        s.set_dir(dir.path());
        assert_eq!((s.begin(), s.begin(), s.begin()), (Some(1), Some(2), None));
        s.write("first\n").unwrap();
        let again = Sink::new(settings());
        again.set_dir(dir.path());
        assert!(!dir.path().join(PORTABLE_FILE).exists());
        assert!(dir.path().join("native_portable.log.1").exists());
    }
}
