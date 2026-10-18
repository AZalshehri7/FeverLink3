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

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::dump::{ascii_string, pointer_like, u32_at, u64_at, Env, Mem, IMAGE_BOUND, PAYLOAD_SIZE};

/// `1` = commands the module held (their `held` and `apply`), `3` = also every other `build_proposal` the simulation applies.
pub const PORTABLE_VAR: &str = "FEVERLINK3_PORTABLE_DUMP";
pub const PORTABLE_FILE: &str = "native_portable.log";
/// `=0` leaves pointers to heap blocks as `P`; `=1` turns the graph on for the plain portable dump.
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
    /// `std::map` (0x10 bytes: head pointer, size) whose tree nodes (and the tree's sentinel, the same type) are described by the
    /// [`MapNode`]: left, parent, right, colour, isnil, the key at +0x20 and the value after it.
    Map(&'static MapNode),
    /// `std::unordered_map/set` (0x40 bytes) whose list nodes (and the list's sentinel, the same type) are described by the
    /// [`HashNode`]: next, prev, the value. The node size is the container's element type, so every block of its graph has a size known
    /// from the layout (no heap query). [b45, c10c, inj3: sentinels of 24 bytes for the four Proposal sets and `old2new`, 40 for
    /// `refundableEntities`]
    Hash(&'static HashNode),
    /// A flat hash set of 4-byte entity ids (0x20 bytes: control-byte pointer, slot pointer, size, capacity; one allocation of
    /// `capacity + 16` control bytes, padded to 4, then `capacity` slots). Its value is the set of ids; the control bytes are a hash of
    /// them made by the game that owns the table (b54: other bytes in the receiver's copy), so they are not part of the value.
    EntSet,
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
            Ty::Res | Ty::Hash(_) => 0x40,
            Ty::Vec(_) | Ty::EntVec | Ty::BlobVec => 0x18,
            Ty::Map(_) => 0x10,
            Ty::EntSet => 0x20,
            Ty::Raw(n) | Ty::Skip(n) | Ty::Pad(n) => n,
        }
    }
}

/// The node of an engine hash container's list: `size` bytes (next at +0, prev at +8, the value from +0x10), the byte ranges of
/// the value that are its padding (never constructed: residue of earlier users of the memory, not part of the value; written as zero),
/// and whether the value is known to hold no pointer (`literal`: its words are data, written as they are). A container whose value
/// type is not known is not `literal`: a value word that is an address in this game stays a `P` (the rebuild refuses it).
#[derive(Debug, PartialEq, Eq)]
pub struct HashNode {
    pub size: usize,
    pub pad: &'static [(usize, usize)],
    pub literal: bool,
}

/// `refundableEntities`: 40-byte nodes, a 24-byte value. Not typed: the engine's copy of an entry is a plain 24-byte copy (`0x4be210`:
/// `vmovups` of 16 bytes and `vmovsd` of 8), which does not tell data from a pointer; only the key's two 4-byte words at +0x10/+0x14 are
/// compared as such. The container is past the Proposal and never taken from the text (`inject::outside_proposal_container`).
pub static HASH_NODE_28: HashNode = HashNode { size: 0x28, pad: &[], literal: false };
/// A set of 4-byte values (`std::unordered_set<uint32>`): the value at +0x10, four bytes of padding at +0x14.
/// [owner airport run 2026-10-15 key 20 and friend test 2026-10-16 keys 20, 28, 29 and the friend's hold 4: every unresolved pointer of
/// those refusals was +0x10 of a `terrainAlignSkipEdges` node; the low halves of the 22 values of key 20 are 0..21, each once (two of
/// them only in the "pointers": 0x171_00000007 and 0x170_00000012), and the high halves are 0, 1, 0x12f, 0x170, 0x171, 0x2c8: the high
/// words of the heap addresses that memory held before. The halves above 0x10000 made the word look like a readable address on one game
/// and not on the other. Engine: the Proposal copy constructor (`0x4c6640`) copies the set with `0x4bda90`, whose node copy is
/// `mov ecx,[src+0x10]; mov [node+0x10],ecx` into an `operator new(0x18)` node: 4 bytes of value.]
pub static HASH_NODE_SET32: HashNode = HashNode { size: 0x18, pad: &[(0x14, 0x18)], literal: true };
/// A map of a 4-byte key to a 4-byte value (`std::unordered_map<uint32, uint32>`): key at +0x10, value at +0x14, no padding, no
/// pointer. [Engine: `hash90` and `hashd0` (StreetProposal copy constructor `0x4c78c0`) and `old2new` (Proposal copy constructor
/// `0x4c6640`) are all copied by the same function `0x4c31a0`, whose node insert `0x4bec80` hashes the 4 bytes at +0x10 (FNV-1a, byte
/// by byte), compares them as one dword and copies the 8-byte pair as a qword (`mov rcx,[src+0x10]; mov [node+0x10],rcx`) into an
/// `operator new(0x18)` node: 8 bytes of value whose first 4 are the key; an 8-byte pointer cannot fit beside it.]
pub static HASH_NODE_PAIR32: HashNode = HashNode { size: 0x18, pad: &[], literal: true };

/// The node of an engine `std::map` (MSVC: left +0, parent +8, right +0x10, colour +0x18, isnil +0x19, the value from +0x20, whose first
/// member is the key) and what its value is.
pub struct MapNode {
    pub size: usize,
    pub value: MapValue,
}

pub enum MapValue {
    /// `std::map<uint32, std::set<uint32>>`: the key at +0x20, the set (sentinel pointer +0x28, size +0x30).
    IdSet,
    /// A value of a known layout from +0x20 (key included): an [`Elem`] of `size - 0x20` bytes, written member by member as named
    /// lines `<map>[i].<member>` (in key order) and carried in the node's block by value (scalars as literals, vectors as `V` tokens
    /// whose elements are typed the same way): no word of it is ever a pointer of the originator.
    Record(&'static Elem),
}

// a node is one entry of the layout table: the same node is the same type
impl PartialEq for MapNode {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for MapNode {}
impl std::fmt::Debug for MapNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MapNode({:#x})", self.size)
    }
}

/// The six id maps of the Proposal (nodes, segments, edge objects; new to old and old to new), see [`Enc::register_map_nodes`].
pub static MAP_ID_SETS: MapNode = MapNode { size: 0x38, value: MapValue::IdSet };
/// `parcelsMap`: `std::map<Entity, Parcel>`, the parcels the build makes (keys `new#N`), nodes of 0x50 bytes. See [`PARCEL`].
pub static MAP_PARCELS: MapNode = MapNode { size: 0x50, value: MapValue::Record(&PARCEL) };

/// The byte offset of the slots of an [`Ty::EntSet`] table of `cap` slots from its control bytes: `cap` control bytes, the sentinel,
/// 15 cloned bytes for a 16-byte group read, padded to the 4-byte slot alignment. [b45: 20 for capacities 1 and 3]
pub const fn ent_set_slot_offset(cap: usize) -> usize {
    (cap + 16 + 3) & !3
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

/// `LaneConfig` (0x18 bytes): speed, width, height, forward flag (three padding bytes, residue 0x7f seen), the transport-mode bit set,
/// offset from the centre. [observed: a 4-lane street, 0x60 bytes: 13.89 / 3.0 / 0.15 / forward 0 / modes 3 / 0.15, then 5.0, -0.5 ...]
pub static LANE_CONFIG: Elem = Elem {
    name: "LaneConfig",
    stride: 0x18,
    fields: &[
        f(0, "speed", Ty::F32),
        f(4, "width", Ty::F32),
        f(8, "height", Ty::F32),
        f(0xc, "forward", Ty::U8),
        f(0xd, "padding", Ty::Pad(3)),
        f(0x10, "transportModes", Ty::U32),
        f(0x14, "offset", Ty::F32),
    ],
    anchors: &[],
};

/// `BaseEdge.objects` element (8 bytes): the edge object's entity and its `EdgeObjectType`. [observed: 8 and 0x10 byte vectors
/// holding an existing stop entity and the value 1 or 0]
pub static EDGE_OBJECT_REF: Elem =
    Elem { name: "EdgeObjectRef", stride: 8, fields: &[f(0, "entity", Ty::Ent), f(4, "type", Ty::U32)], anchors: &[] };

/// `LaneConnection` (0x14 bytes), in the order of the api type definition: incoming segment, lane, outgoing segment, lane, the two
/// flags (two padding bytes). [static: the offset of `laneConnections` is +0 in `BaseNodeConfig` (the binding table); no instance
/// with entries was seen in the spike dumps]
pub static LANE_CONNECTION: Elem = Elem {
    name: "LaneConnection",
    stride: 0x14,
    fields: &[
        f(0, "segment0", Ty::Ent),
        f(4, "lane0", Ty::I32),
        f(8, "segment1", Ty::Ent),
        f(0xc, "lane1", Ty::I32),
        f(0x10, "withRoad", Ty::U8),
        f(0x11, "withTram", Ty::U8),
        f(0x12, "padding", Ty::Pad(2)),
    ],
    anchors: &[],
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
        f(0x48, "objects", Ty::Vec(&EDGE_OBJECT_REF)),
        f(0x60, "edgeDecorations", Ty::BlobVec),
        f(0x78, "laneConfigs", Ty::Vec(&LANE_CONFIG)),
        f(0x98, "roadType", Ty::U32),
        f(0xa0, "roadTemplate", Ty::Res),
        f(0xe0, "roadStyle", Ty::Res),
        f(0x120, "type", Ty::U32),
        f(0x320, "playerOwned", Ty::Ent),
    ],
    anchors: &[(8, 0x10), (0xc, 0x1c)],
};

/// One entry of the proposal's `segmentTags`: a `std::string` (32 bytes: buffer or pointer, size, capacity), one per segment of a
/// construction's proposal, none in a road's. [a depot over four buildings and a street station: one entry, the empty string (seven
/// zero words and the capacity 15). rt5 R11 / rt6 R12 / rt7 R13 (2026-10-14): a modular train station has 24 entries of 16 characters
/// on the heap (capacity 31); read as words, the text carried the originator's string pointers, and the receiver's engine copy of
/// the injected Proposal read freed memory through them and crashed (memcpy under the Proposal copy constructor).]
pub static SEGMENT_TAG: Elem =
    Elem { name: "SegmentTag", stride: 0x20, fields: &[f(0, "value", Ty::Str)], anchors: &[] };

pub static NODE_CONFIG: Elem = Elem {
    name: "BaseNodeLaneConnectionAndEntity",
    stride: 0x80,
    // +0x18..0x38 is a flat hash set of segment ids (the api's `crosswalks`: its size is the crosswalk count the checksum reads).
    // +0x38..0x78 is NOT all residue: it holds the traffic-light state, which b63/b64 lost when it was typed as padding (the four
    // node configurations of the depot differed in the checksum: trafficLightPreference 2 on the originator, 0 on the receiver;
    // trafficLightType -1 against 0). [b39: +0x40 zero, +0x4c the preference 2, +0x50..+0x68 zero, +0x68 the type -1; ASCII or
    // pointer-like residue at +0x38, +0x48, +0x6c..+0x78 (b41, b46).] The rest of it is an unnamed region like any other: the
    // differential mask keeps the bytes the engine's copy constructor carries and zeroes the rest, and a foreign pointer is never carried.
    // The preference and the type are named members: left to the mask, the type was dropped in real run rt2 R3 (2026-10-14, a street
    // with a junction through a house: the text's +0x68 was zero for all four node configurations, the receiver's checksum had type 0
    // where every other node of both games has -1, nodeConfigs drifted), while the same build a run later (R4) carried it. A value the
    // world reads must not depend on what two scratch copies happen to agree on.
    fields: &[
        f(0, "laneConnections", Ty::Vec(&LANE_CONNECTION)),
        // the set (of segment ids; in b45 every segment that meets at the node). [b45 node new#1: control bytes 7a 23 51 ff + clones + 0x80 fill (19), one
        // pad byte, then the slots -4 -5 -3 at +20; size 3, capacity 3 at +0x28/+0x30; the old node 39878: 39880, -4, 40178, its two
        // old segments and the new one.] b57 to b62 carried only the 20 control bytes' length and rebuilt zeros with the slot pointer
        // past the block: the engine's copy then read 3 heap words as segment ids, and the receiver crashed one second later in a map
        // lookup (`map_util.h(22) Get`, Sim Pool thread).
        f(0x18, "crosswalks", Ty::EntSet),
        f(0x4c, "trafficLightPreference", Ty::U32),
        f(0x68, "trafficLightType", Ty::U32),
        f(0x78, "entity", Ty::Ent),
    ],
    anchors: &[],
};

pub static EDGE_OBJECT: Elem = Elem {
    name: "EdgeObject",
    stride: 0x1a0,
    fields: &[
        f(0, "resultEntity", Ty::Ent),
        f(4, "category", Ty::U32),
        f(8, "segmentEntity", Ty::Ent),
        // the position along the segment (0..1) the stop tool puts in the held command before the build runs; the tool's preview has
        // no model yet, so a stop's record takes model, name and this from the held command's input text [observed inj7 2026-10-14:
        // 0x3e2e9c82 = 0.1705189 at +0x28 in all three stop objects, the post-build record's param of the same stops]
        f(0x28, "param", Ty::F32),
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
        // the VALUES are 0-based indices into proposal.addedNodes (not entity ids; the text writes them as `ent:old#N`): the node of the
        // construction's own piece. [observed in 5 depots/stations: 1, 1, 1, 1, 0; always the added node nearest to the building]
        f(0xcd8, "frozenNodes", Ty::EntVec),
        f(0xe18, "name", Ty::Str),
        f(0xe38, "playerEntity", Ty::Ent),
        f(0xe3c, "setAsHeadquarterHack", Ty::U8),
    ],
    anchors: &[],
};

/// A `Vec3f` as the element of a vector (12 bytes, no padding).
pub static VEC3F: Elem = Elem { name: "Vec3f", stride: 12, fields: &[f(0, "v", Ty::V3)], anchors: &[] };

/// `Parcel::Face` (0x20 bytes): `points` (`std::vector<Vec3f>`), `collision` (the api's `{CollisionType : boolean}`, a 4-byte set of
/// flags), four bytes of padding. [Engine: the parcel copy `0x4c1df0` copies each face with `0xdf480` for the points (its element
/// count is `(end - begin) / 12`: `imul 0x2aaaaaaaaaaaaaab`, `sar 1`; the bytes by `memmove`) and `mov eax,[src+0x18]; mov
/// [dst+0x18],eax`, stride 0x20 (`add rdi,0x20`). Owner run 2026-10-18 (pre4, host hold 1): in each of the 13 nodes
/// (of 42) whose pointers native.log details, the face vector is 0x80 bytes (four faces) and the first face's points 0x30 (four points).]
pub static PARCEL_FACE: Elem = Elem {
    name: "ParcelFace",
    stride: 0x20,
    fields: &[f(0, "points", Ty::Vec(&VEC3F)), f(0x18, "collision", Ty::U32), f(0x1c, "padding", Ty::Pad(4))],
    anchors: &[],
};

/// The value of a `parcelsMap` node from +0x20 (0x30 bytes): the key (the parcel's entity) and the api's `Parcel` component in its
/// order (`streetSegment`, `left`, `curveParam`, `offset`, `faces`). [Engine: the Proposal copy constructor (`0x4c6640`) copies the
/// map at +0x2c8 from a 0x50-byte sentinel with `0x4bc770` -> `0x4bd1f0` (an `operator new(0x50)` node per entry, the value from +0x20
/// copied by `0x4c1df0`): `mov eax,[src]` (key), `[src+8]` (4 bytes), `movzx [src+0xc]` (1 byte), `[src+0x10]`, `[src+0x14]` (4 bytes
/// each), then the vector at +0x18 of 0x20-byte elements (`sar rbx,5`). The bytes +4..+8 and +0xd..+0x10 are never copied: padding.
/// Owner run 2026-10-18 (pre4, host hold 1, a street built by the street tool over town land): 42 entries, keys new#42..new#1; every
/// unresolved pointer of its refusal was one of +0x38/+0x40/+0x48 of a node, the faces vector (begin, end, capacity, end = capacity).
/// The run of 2026-10-14 (h3, a station over town land, the untyped graph): +0x28 0xffffffff/0xfffffffe (the new segments new#1/#2),
/// +0x2c 0 or 1 under residue bytes (0x435f4e00), +0x30 0.0855..0.83 (a curve parameter), +0x34 8.0.]
pub static PARCEL: Elem = Elem {
    name: "Parcel",
    stride: 0x30,
    fields: &[
        f(0, "key", Ty::Ent),
        f(4, "padding0", Ty::Pad(4)),
        f(8, "streetSegment", Ty::Ent),
        f(0xc, "left", Ty::U8),
        f(0xd, "padding1", Ty::Pad(3)),
        f(0x10, "curveParam", Ty::F32),
        f(0x14, "offset", Ty::F32),
        f(0x18, "faces", Ty::Vec(&PARCEL_FACE)),
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
    f(0x090, "proposal.hash90", Ty::Hash(&HASH_NODE_PAIR32)),
    f(0x0d0, "proposal.hashd0", Ty::Hash(&HASH_NODE_PAIR32)),
    f(0x110, "proposal.edgeObjectsToRemove", Ty::EntVec),
    f(0x128, "proposal.edgeObjectsToAdd", Ty::Vec(&EDGE_OBJECT)),
    f(0x140, "proposal.new2oldNodes", Ty::Map(&MAP_ID_SETS)),
    f(0x150, "proposal.old2newNodes", Ty::Map(&MAP_ID_SETS)),
    f(0x160, "proposal.new2oldSegments", Ty::Map(&MAP_ID_SETS)),
    f(0x170, "proposal.old2newSegments", Ty::Map(&MAP_ID_SETS)),
    f(0x180, "proposal.new2oldEdgeObjects", Ty::Map(&MAP_ID_SETS)),
    f(0x190, "proposal.old2newEdgeObjects", Ty::Map(&MAP_ID_SETS)),
    // the tail of the StreetProposal that the Lua binding does not register. The first 0x20 bytes are carried by the engine's own
    // copy constructor (0x4c78c0): an image pointer (constant) or a begin/end range of 20 bytes and two small integers (2, 3) in the
    // stop pair; the word at +0x20 (small integers, a pair of floats, 0x3f800060) is NOT carried (the copy has zeros, seen in the real
    // game) and the word at +0x28 differs (0 against 1): transient state, so 0x10 bytes of padding.
    // (b69: a depot over four town buildings had size 2, capacity 3 here and the typed copy dropped its two heap pointers as residue;
    // the engine's copy constructor of the receiver's rebuilt source then walked a null control row and the game crashed.) It is the
    // same kind of flat set of 4-byte ids as a node configuration's `crosswalks` (empty: the image's empty group, I+367b620).
    f(0x1a0, "proposal.idSet1a0", Ty::EntSet),
    f(0x1c0, "proposal.gap1c0", Ty::Pad(0x10)),
    // indices into addedNodes, see the construction's `frozenNodes`
    f(0x1d0, "proposal.frozenNodes", Ty::EntVec),
    f(0x1e8, "terrainAlignSkipEdges", Ty::Hash(&HASH_NODE_SET32)),
    f(0x228, "segmentTags", Ty::Vec(&SEGMENT_TAG)),
    f(0x240, "toRemove", Ty::EntVec),
    f(0x258, "toAdd", Ty::Vec(&CONSTRUCTION)),
    f(0x270, "old2new", Ty::Hash(&HASH_NODE_PAIR32)),
    // a vector of u32 (copied with sar 2 in the engine's copy constructor) and the parcels the build makes (see PARCEL)
    f(0x2b0, "parcelsToRemove", Ty::EntVec),
    f(0x2c8, "parcelsMap", Ty::Map(&MAP_PARCELS)),
    f(0x2d8, "terrain", Ty::Raw(0x80)),
    f(0x358, "context.gatherBuildings", Ty::U8),
    f(0x359, "context.gatherFields", Ty::U8),
    f(0x360, "context.checkTerrainAlignment", Ty::U8),
    f(0x362, "context.cleanupStreetGraph", Ty::U8),
    f(0x36c, "context.player", Ty::Ent),
    // an unordered container (load factor 1.0 at +0, bucket vector, mask 7, maxidx 8 seen at +0x30, +0x38 of it)
    f(0x370, "context.refundableEntities", Ty::Hash(&HASH_NODE_28)),
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
    /// Input level (what a SimpleProposal is made of): only the members the table names, no unnamed bytes, no raw blobs, no container
    /// graphs. The text the mod rebuilds a build from (`input.rs`).
    pub input_level: bool,
    /// Members whose heap graph is not followed (their pointers stay `P`): `toAdd` for the injection text, whose construction is
    /// the receiver's own (tens of thousands of blocks of the description that are never rebuilt).
    pub graph_skip: &'static [&'static str],
    /// Typed copy (`inject.rs`): pointers are followed only through typed members; in the bytes the table does not name every pointer
    /// to something other than the game's image is zeroed (it is a foreign address, owned or not), and an empty engine container writes no graph.
    pub typed: bool,
    /// The hard ceiling of the heap the structural dump may read behind unnamed bytes (vectors and ranges it recognises). The bound of
    /// one text is scaled from the proposal's own size ([`heap_budget`]) and never goes above this.
    pub max_heap_bytes: usize,
}

/// Bytes of heap the structural dump may read per byte of the proposal's own data (the payload and the blocks of its vectors).
pub const HEAP_PER_OWN_BYTE: usize = 64;
/// The least heap bound of a text: the fixed bound every text had before it was scaled (8 MB), so that no text is refused that was not
/// refused before.
pub const HEAP_FLOOR: usize = 8 << 20;

/// The heap the structural dump of a proposal of `own` bytes may read: [`HEAP_PER_OWN_BYTE`] per byte, at least [`HEAP_FLOOR`], at most
/// `ceiling`. The friend test (2026-10-16, key 108, a modular station's module over a town building) refused the host's own copy at the
/// fixed 8 MB while the friend's game injected the same text: the bound is a property of the proposal, the same on every game, and only
/// the bytes really read count against it (see [`Enc::raw_tokens`]).
pub fn heap_budget(own: usize, ceiling: usize) -> usize {
    own.saturating_mul(HEAP_PER_OWN_BYTE).max(HEAP_FLOOR).min(ceiling)
}

/// The proposal's own data: the payload and the element blocks of its vectors (the table's `Vec`, `EntVec` and `BlobVec` members, by
/// their begin and end; a triple that is not a vector counts nothing).
pub fn own_bytes(payload: &[u8], max_block_bytes: usize) -> usize {
    PAYLOAD.iter().filter(|f| matches!(f.ty, Ty::Vec(_) | Ty::EntVec | Ty::BlobVec)).fold(payload.len(), |n, f| {
        let (b, e) = (u64_at(payload, f.off), u64_at(payload, f.off + 8));
        let len = e.wrapping_sub(b) as usize;
        if b != 0 && b <= e && len <= max_block_bytes {
            n.saturating_add(len)
        } else {
            n
        }
    })
}

impl Settings {
    /// The settings of the input-level text that goes to the mod with every held build command.
    pub fn input_level() -> Settings {
        Settings {
            scripts: false,
            max_commands: u32::MAX,
            max_file_bytes: u64::MAX,
            max_text_bytes: 1 << 20,
            max_block_bytes: 8 << 20,
            max_elems: 20_000,
            max_depth: 6,
            graph: false,
            max_graph_blocks: 0,
            max_graph_block_bytes: 0,
            max_graph_bytes: 0,
            input_level: true,
            graph_skip: &[],
            typed: false,
            max_heap_bytes: 8 << 20,
        }
    }

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
            // the graph: on for the rebuild checks and the engine copy, opt-in for the plain dump
            graph: get(GRAPH_VAR).map_or(var != PORTABLE_VAR, |v| v.trim() != "0"),
            max_graph_blocks: 32_768,
            max_graph_block_bytes: 1 << 20,
            max_graph_bytes: 16 << 20,
            input_level: false,
            graph_skip: &[],
            typed: false,
            max_heap_bytes: 64 << 20,
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
    /// Heap bytes the structural dump read behind unnamed bytes, and the text's bound of them ([`heap_budget`]).
    pub heap_read: usize,
    pub heap_budget: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Portable {
    pub text: String,
    pub stats: Stats,
    /// Where the heap graph's blocks were first reached from: (member, blocks, bytes), the largest first.
    pub origins: Vec<(String, usize, usize)>,
    /// The first pointers that stayed `P`: (member they were found in, address). Addresses are for diagnostics only; they are
    /// not in the text.
    pub unresolved: Vec<(String, usize)>,
}

/// True for the lines of a portable text that are not named members: padding gaps, unregistered regions of the table
/// (`Raw` members), the heap graph and the graph of an engine container. Their bytes may differ between two equal objects.
pub fn is_unnamed_path(path: &str) -> bool {
    path.starts_with('~')
        || path.contains(".~")
        || path.starts_with("@B")
        || path.ends_with(".graph")
        || PAYLOAD.iter().any(|f| matches!(f.ty, Ty::Raw(_)) && f.name == path)
}

/// The unnamed regions of an object as bytes, by the path of their line (what [`EncodeOpts::collect`] gathers).
pub type Regions = BTreeMap<String, Vec<u8>>;
/// Which bytes of each unnamed region are carried (true) and which are residue (false), by the path of the region's line.
pub type Mask = BTreeMap<String, Vec<bool>>;

/// What a text is made with besides the settings.
#[derive(Default, Clone, Copy)]
pub struct EncodeOpts<'a> {
    /// Gather the bytes of every unnamed region (also the all-zero ones) here.
    pub collect: Option<&'a std::cell::RefCell<Regions>>,
    /// Typed copy: bytes of unnamed regions that the mask does not carry are written as zero (a region the mask does not know: all zero).
    pub mask: Option<&'a Mask>,
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
    opts: EncodeOpts<'a>,
    /// What the blocks found by walking a typed engine container are (see [`Kind`]).
    kinds: BTreeMap<usize, Kind>,
    env: Env,
    set: Settings,
    out: String,
    stats: Stats,
    /// entity id -> position (f32 bits), from the elements that locate their entities
    pos: HashMap<u32, [u32; 3]>,
    /// bytes of heap memory read for the structural dump so far (only reads that succeeded)
    raw_spent: usize,
    /// what `raw_spent` may reach: [`heap_budget`] of the proposal's [`own_bytes`]
    heap_budget: usize,
    own_bytes: usize,
    /// the heap graph: blocks reached by pointers, in the order they were found (block `n` is `blocks[n - 1]`)
    blocks: Vec<Blk>,
    by_addr: BTreeMap<usize, usize>,
    graph_bytes: usize,
    /// Blocks whose size the heap could not tell but that were found by walking an engine `std::map` (see [`Enc::register_map_nodes`]).
    known_sizes: BTreeMap<usize, usize>,
    /// Heads of the maps walked: their value area is never constructed (residue of earlier users of the memory, pointers among it), so
    /// the block is written with it zeroed.
    sentinels: std::collections::BTreeSet<usize>,
    /// the member being written (or the member a block was reached from), for the graph's accounting
    origin: String,
    unresolved: Vec<(String, usize)>,
    /// where the words being tokenised come from (`@B5`, `<member>.graph`), for the list of unresolved pointers
    label: String,
    /// inside an engine container's words and the blocks behind them: a pointer is a block of the graph or a `P`, never guessed as a range or vector
    container_mode: bool,
}

struct Blk {
    bytes: Vec<u8>,
    origin: String,
    kind: Option<Kind>,
}

/// A block of a typed engine container: which of its words are pointers (the rest are literals; padding is zeroed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The sentinel of a tree: left, parent, right; nothing else is constructed.
    Sentinel,
    /// A node of a `std::set<u32>`: left, parent, right, colour, isnil, key at +0x1c.
    SetNode,
    /// A node of the id maps: left, parent, right, colour, isnil, key at +0x20, the set (head pointer +0x28, size +0x30).
    MapNode,
    /// A node of a map whose value is a [`MapValue::Record`]: left, parent, right, colour, isnil, then the value carried by value
    /// ([`Enc::record_tokens`]).
    RecordNode(&'static MapNode),
    /// The sentinel of a hash container's list: next, prev; its value is never constructed (residue: b45 hashd0's pointed at a
    /// foreign block), so it is written as zero.
    ListSentinel,
    /// A node of a hash container's list: next, prev, the value, with the value's padding (the [`HashNode`]'s byte ranges) zeroed. Its
    /// words are literals; unless the node is `literal`, a word that is an address in this game is a `P`.
    ListNode(&'static HashNode),
    /// The bucket vector of a hash container: every word points at a node of its list (or the sentinel).
    Buckets,
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
        self.origin = crate::rebuild::member_of(path);
        match ty {
            Ty::EntSet => {
                let (ctrl, slots, size, cap) = (u64_at(b, 0), u64_at(b, 8), u64_at(b, 16), u64_at(b, 24));
                let image =
                    |p: u64| (p as usize) >= self.env.image_base && (p as usize) - self.env.image_base < IMAGE_BOUND;
                if ctrl == 0 && slots == 0 && size == 0 && cap == 0 {
                    // all zero (never seen in the game, whose empty table points at the image; synthetic payloads)
                    return self.line(&format!("{path}.#"), "set:null");
                }
                if cap == 0 {
                    // the empty table: its control pointer is the library's static empty group in the game's image
                    if size != 0 || !image(ctrl) {
                        return Err(format!("{path}: not an empty set (ctrl {ctrl:#x}, size {size})"));
                    }
                    return self
                        .line(&format!("{path}.#"), &format!("set:0,I+{:x}", ctrl as usize - self.env.image_base));
                }
                let set_err =
                    || format!("{path}: not a set (ctrl {ctrl:#x}, slots {slots:#x}, size {size}, capacity {cap})");
                if !pointer_like(ctrl) || !(cap + 1).is_power_of_two() || cap > 0xfff || size > cap {
                    return Err(set_err());
                }
                if slots != ctrl + ent_set_slot_offset(cap as usize) as u64 {
                    return Err(set_err());
                }
                // the slots are 4 bytes: the allocation, when the reader knows it (test arenas), is exactly control row + 4 x capacity
                // [b45: 32 bytes for capacity 3, 24 for capacity 1]
                if let Some(n) = self.mem.heap_size(ctrl as usize) {
                    if n != ent_set_slot_offset(cap as usize) + 4 * cap as usize {
                        return Err(format!(
                            "{path}: a table of {n} bytes for capacity {cap} does not hold 4-byte ids"
                        ));
                    }
                }
                let ctl = self.read(ctrl as usize, cap as usize + 1, path)?;
                if ctl[cap as usize] != 0xff {
                    return Err(format!("{path}: no sentinel after {cap} control bytes"));
                }
                let raw = self.read(slots as usize, cap as usize * 4, path)?;
                // a control byte with the top bit clear marks a full slot (0x80 empty, 0xfe deleted)
                let mut ids: Vec<u32> =
                    (0..cap as usize).filter(|&i| ctl[i] & 0x80 == 0).map(|i| u32_at(&raw, i * 4)).collect();
                if ids.len() as u64 != size {
                    return Err(format!("{path}: {} full slots for a size of {size}", ids.len()));
                }
                // the value is the set: in id order (the slot order is the owner's hash)
                ids.sort_unstable();
                self.line(&format!("{path}.#"), &format!("set:{size}"))?;
                for (i, v) in ids.into_iter().enumerate() {
                    let e = self.ent(v);
                    self.line(&format!("{path}[{i}]"), &e)?;
                }
                Ok(())
            }
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
                if !self.set.input_level && block.iter().any(|&x| x != 0) {
                    if self.set.typed {
                        self.line(&format!("{path}.raw"), &format!("raw:b:{}", hex(&block)))?;
                    } else {
                        self.label = format!("{path}.raw");
                        let t = self.raw_tokens(&block, depth + 1)?;
                        self.line(&format!("{path}.raw"), &format!("raw:{t}"))?;
                    }
                }
                Ok(())
            }
            Ty::Map(node) => {
                // std::map: head pointer and size; the nodes are heap blocks of the graph
                self.line(path, &format!("map:size={}", u64_at(b, 8)))?;
                if let Some((keys, values)) =
                    self.register_map_nodes(u64_at(b, 0) as usize, u64_at(b, 8) as usize, node)
                {
                    // the entries' keys (and, for the id maps, the set each key maps to), sorted: a named line, so that a rebuilt
                    // map's entries are part of the comparison (the nodes themselves are unnamed graph blocks)
                    self.line(&format!("{path}.keys"), &format!("keys:{keys}"))?;
                    // a typed value: every entry's members as named lines, in key order, so that the values are compared too. Not in the
                    // input level (the mod's text, bounded to 1 MB): the engine makes the parcels, a SimpleProposal has none, and their
                    // points (16 per parcel, 42 parcels for one street in the owner's pre4 run) would only fill it
                    if let (MapValue::Record(elem), false) = (&node.value, self.set.input_level) {
                        for (i, v) in values.iter().enumerate() {
                            self.record(&format!("{path}[{i}]"), v, elem.fields, elem.stride, depth + 1)?;
                        }
                    }
                }
                self.container_graph(path, b, depth, u64_at(b, 8))
            }
            Ty::Hash(node) => {
                // std::unordered_map/set: load factor (the other four bytes of its word are padding), then the list head, size,
                // bucket vector, mask and maximum index; the nodes are heap blocks of the graph
                self.line(path, &format!("hash:size={},load={:08x}", u64_at(b, 0x10), u32_at(b, 0)))?;
                self.register_hash_list(u64_at(b, 8) as usize, u64_at(b, 0x10) as usize, node);
                self.register_hash_buckets(
                    u64_at(b, 0x18) as usize,
                    u64_at(b, 0x20) as usize,
                    u64_at(b, 0x28) as usize,
                );
                self.container_graph(path, &b[8..], depth, u64_at(b, 0x10))
            }
            Ty::Raw(_) | Ty::Skip(_) | Ty::Pad(_) => Ok(()),
        }
    }

    /// The words of an engine container written structurally (`path.graph`): its pointers become blocks of the heap graph.
    fn container_graph(&mut self, path: &str, bytes: &[u8], depth: usize, entries: u64) -> R<()> {
        if self.set.input_level || bytes.iter().all(|&x| x == 0) || (self.set.typed && entries == 0) {
            return Ok(());
        }
        let was = std::mem::replace(&mut self.container_mode, true);
        self.label = format!("{path}.graph");
        let t = self.raw_tokens(bytes, depth + 1);
        self.container_mode = was;
        let t = t?;
        if t.split(' ').any(|x| x == "P") {
            self.stats.unread_entries += 1;
        }
        self.line(&format!("{path}.graph"), &format!("raw:{t}"))
    }

    /// The nodes of an engine `std::map` with entries, found by walking the tree itself: the heap index does not list every small block
    /// (b42, b43: nodes and the small sentinels of the sets behind the values stayed `P`), and a tree can be checked without asking the
    /// heap. MSVC's layout: a sentinel node (left = leftmost, parent = root, right = rightmost; `isnil` at +0x19 set) and nodes of
    /// the same size whose missing children point at the sentinel. Registered only when the walk finds exactly `size` readable
    /// nodes, each with `isnil` clear; else nothing is registered and the pointers stay `P` (and the rebuild refuses).
    ///
    /// The six id maps of the Proposal (nodes, segments, edge objects; new to old and old to new) are `std::map<u32, std::set<u32>>`:
    /// nodes of 0x38 bytes (key at +0x20, the set's head pointer at +0x28 and its size at +0x30; the engine's own copy code allocates 0x38
    /// for the node and builds the set copy from a 0x20-byte sentinel, `0x4bd430`/`0x4c2250`/`0x284fc0`), the set's nodes 0x20 bytes with
    /// the key at +0x1c. The Proposal's copy constructor (0x4c78c0) copies all six with the same function (0x4bc8d0) from a 0x38-byte
    /// sentinel each. Only the segment maps used to be typed: the node maps of a station configure (inj3, 2026-10-14) were walked as
    /// generic maps of 0x50-byte nodes with their values left to the graph, and the walk followed the sets and the words beside them
    /// into the world (32768 blocks, a 7.6 MB text). A map whose value is a [`MapValue::Record`] (`parcelsMap`) has its nodes typed as
    /// [`Kind::RecordNode`]: the value is carried by value, never as a pointer of the originator (owner run 2026-10-18, pre4: a street
    /// over town land had 42 parcels whose untyped nodes left 126 pointers of their face vectors unresolved, and the originator refused
    /// its own text). Returns the sorted keys line (with each set's keys for the id maps) and the entries' value bytes (from +0x20) in
    /// key order.
    fn register_map_nodes(
        &mut self,
        head: usize,
        size: usize,
        map: &'static MapNode,
    ) -> Option<(String, Vec<Vec<u8>>)> {
        if size == 0 {
            return None; // an empty engine map is the engine's own; nothing to describe
        }
        let sets = matches!(map.value, MapValue::IdSet);
        let node = map.size;
        let nodes = self.walk_tree(head, size, node)?;
        let mut register: Vec<(usize, usize)> = vec![(head, node)];
        let mut kinds: Vec<(usize, Kind)> = vec![(head, Kind::Sentinel)];
        let mut entries: Vec<(u32, String)> = Vec::new();
        let mut values: Vec<(u32, Vec<u8>)> = Vec::new();
        for (a, n) in &nodes {
            register.push((*a, node));
            kinds.push((*a, if sets { Kind::MapNode } else { Kind::RecordNode(map) }));
            let key = u32_at(n, 0x20);
            values.push((key, n[0x20..].to_vec()));
            let mut inner = String::new();
            if sets {
                let (h2, c2) = (u64_at(n, 0x28) as usize, u64_at(n, 0x30) as usize);
                let tree = self.walk_tree(h2, c2, 0x20)?;
                register.push((h2, 0x20));
                kinds.push((h2, Kind::Sentinel));
                register.extend(tree.iter().map(|(a2, _)| (*a2, 0x20)));
                kinds.extend(tree.iter().map(|(a2, _)| (*a2, Kind::SetNode)));
                let mut ks: Vec<u32> = tree.iter().map(|(_, b)| u32_at(b, 0x1c)).collect();
                ks.sort_unstable();
                inner = format!("=[{}]", ks.iter().map(|k| format!("{k:08x}")).collect::<Vec<_>>().join(";"));
            }
            entries.push((key, inner));
        }
        for (a, n) in register {
            self.known_sizes.insert(a, n);
        }
        // every map of the table is typed: its nodes are written word by word by kind
        self.kinds.extend(kinds);
        self.sentinels.insert(head);
        entries.sort();
        values.sort_by_key(|(k, _)| *k);
        Some((
            entries.iter().map(|(k, v)| format!("{k:08x}{v}")).collect::<Vec<_>>().join(","),
            values.into_iter().map(|(_, v)| v).collect(),
        ))
    }

    /// The bucket vector of an engine hash container (MSVC: begin, end, capacity at +0x18 of the member; two list pointers per bucket)
    /// is one block of `capacity - begin` bytes: its size is known from the container itself, so it does not depend on the heap index.
    /// C10 run inj8 (2026-10-14, a module added to a modular station): the heap index did not list the 128-byte bucket block of the
    /// Proposal's `old2new` (one entry, 8 buckets), its three words stayed `P` and the receiver refused the injection.
    fn register_hash_buckets(&mut self, begin: usize, end: usize, cap: usize) {
        let bytes = cap.wrapping_sub(begin);
        if pointer_like(begin as u64)
            && begin < end
            && end <= cap
            && bytes.is_multiple_of(16)
            && bytes <= self.set.max_graph_block_bytes
            && self.mem.read(begin, bytes).is_some()
        {
            self.known_sizes.entry(begin).or_insert(bytes);
            self.kinds.entry(begin).or_insert(Kind::Buckets);
        }
    }

    /// The list of an engine hash container (MSVC: a doubly linked list through a sentinel of the node type; `next` at +0, `prev` at
    /// +8, the value after them), walked from its sentinel: registered only when the walk comes back to the sentinel after exactly `size`
    /// readable nodes whose `prev` links agree; else nothing is registered and the pointers stay `P` (the rebuild refuses). The sizes
    /// come from the container's type, so the graph needs no heap index (the whole-heap walk it replaces took 220-320 ms per text and
    /// stopped at 3,145,728 blocks, missing blocks past it: b45 `refundableEntities` sentinel, inj7 its list head, inj3/inj8 buckets).
    fn register_hash_list(&mut self, head: usize, size: usize, list: &'static HashNode) {
        let node = list.size;
        if size > self.set.max_elems || !pointer_like(head as u64) || node < 0x10 {
            return;
        }
        let Some(h) = self.mem.read(head, node) else { return };
        let mut found: Vec<usize> = Vec::new();
        let (mut prev, mut at) = (head, u64_at(&h, 0) as usize);
        while at != head {
            if found.len() >= size || !pointer_like(at as u64) || found.contains(&at) {
                return;
            }
            let Some(n) = self.mem.read(at, node) else { return };
            if u64_at(&n, 8) as usize != prev {
                return;
            }
            found.push(at);
            (prev, at) = (at, u64_at(&n, 0) as usize);
        }
        if found.len() != size || u64_at(&h, 8) as usize != prev {
            return;
        }
        self.known_sizes.insert(head, node);
        self.kinds.insert(head, Kind::ListSentinel);
        for a in found {
            self.known_sizes.insert(a, node);
            self.kinds.insert(a, Kind::ListNode(list));
        }
    }

    /// The nodes of the tree at `head` (a sentinel of `node` bytes) as (address, bytes), when it holds exactly `size` of them. A set of
    /// size 0 is accepted when its sentinel is readable (the engine allocates one for every set).
    fn walk_tree(&self, head: usize, size: usize, node: usize) -> Option<Vec<(usize, Vec<u8>)>> {
        if size > 4096 || !pointer_like(head as u64) {
            return None;
        }
        let h = self.mem.read(head, node)?;
        if h[0x19] != 1 {
            return None;
        }
        let mut found: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut stack = if size == 0 { Vec::new() } else { vec![u64_at(&h, 8) as usize] };
        while let Some(a) = stack.pop() {
            if a == head {
                continue;
            }
            if !pointer_like(a as u64) || found.iter().any(|(f, _)| *f == a) || found.len() > size {
                return None;
            }
            let n = self.mem.read(a, node)?;
            if n[0x19] != 0 {
                return None;
            }
            stack.push(u64_at(&n, 0) as usize);
            stack.push(u64_at(&n, 0x10) as usize);
            found.push((a, n));
        }
        (found.len() == size).then_some(found)
    }

    /// A pointer to a live heap block as a token of the heap graph: `B<n>` (the start of block n) or `B<n>+<hex>` (inside it).
    /// The first pointer to a block reads it (its size comes from the container it was found in) and queues its own pointers.
    fn block_token(&mut self, p: usize) -> Option<String> {
        if !self.set.graph || self.set.graph_skip.contains(&self.origin.as_str()) {
            return None;
        }
        if let Some(&n) = self.by_addr.get(&p) {
            return Some(format!("B{n}"));
        }
        if let Some((&a, &n)) = self.by_addr.range(..p).next_back() {
            if p <= a + self.blocks[n - 1].bytes.len() {
                return Some(format!("B{n}+{:x}", p - a));
            }
        }
        // the size comes from the container the block was found in (map and set nodes, hash lists and buckets); a typed text never
        // asks the reader about the heap, and the live reader cannot answer anyway (no whole-heap index any more)
        let size = match self.known_sizes.get(&p) {
            Some(&n) => n,
            None if self.set.typed => return None,
            None => self.mem.heap_size(p)?,
        };
        if size > self.set.max_graph_block_bytes
            || self.blocks.len() >= self.set.max_graph_blocks
            || self.graph_bytes + size > self.set.max_graph_bytes
        {
            self.stats.graph_truncated += 1;
            return None;
        }
        let mut bytes = self.mem.read(p, size)?;
        if self.sentinels.contains(&p) && bytes.len() > 0x1a {
            bytes[0x1a..].fill(0); // a sentinel's key and value are never constructed
        }
        self.graph_bytes += size;
        let origin = self.origin.clone();
        let kind = self.kinds.get(&p).copied();
        self.blocks.push(Blk { bytes, origin, kind });
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
        if self.set.input_level {
            return Ok(());
        }
        self.origin = crate::rebuild::member_of(name);
        let mut owned = bytes[from..to].to_vec();
        if let Some(c) = self.opts.collect {
            // collecting the raw bytes for the differential mask: the text is thrown away, so nothing is tokenised or followed (b75: a
            // vector-like triple in a scratch copy's unnamed bytes made the structural walk read past its heap bound and the originator
            // shipped a text without a mask, every unnamed byte zero)
            c.borrow_mut().insert(name.to_string(), owned);
            return Ok(());
        }
        if self.set.typed {
            if let Some(mask) = self.opts.mask {
                match mask.get(name) {
                    Some(m) if m.len() == owned.len() => {
                        for (b, keep) in owned.iter_mut().zip(m) {
                            if !keep {
                                *b = 0;
                            }
                        }
                    }
                    _ => owned.fill(0),
                }
            }
            // a pointer to anything but the game's image is a foreign address in the receiving process, owned or not: no typed member
            // owns it, so it is not carried. Only a word that IS an address here (readable memory of this game) counts: two packed
            // 32-bit values look pointer-like too (b64: a node configuration's traffic-light preference 2 beside a zero is the word
            // 0x2_0000_0000, its type -1 beside a zero 0xffff_ffff), and they are data.
            // The words are read at the record's 8-byte boundaries, also in a region that starts between two (inj3, 2026-10-14: a stop's
            // EdgeObject region ~0xc carried the three words of a vector at +0x10..+0x28, the receiving game copied the injected Proposal
            // with the originator's heap addresses in it and its simulation thread died).
            let mut k = (8 - from % 8) % 8;
            while k + 8 <= owned.len() {
                let w = u64_at(&owned, k);
                let image = (w as usize) >= self.env.image_base && (w as usize) - self.env.image_base < IMAGE_BOUND;
                if pointer_like(w) && !image && self.mem.read(w as usize, 1).is_some() {
                    owned[k..k + 8].fill(0);
                }
                k += 8;
            }
        }
        let part = &owned[..];
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

    /// The tokens of a block of a typed engine container: pointers only at the words the kind names (blocks of the graph, or the game's
    /// image), every other word a literal, padding zeroed. Never a guess.
    fn typed_tokens(&mut self, bytes: &[u8], kind: Kind) -> R<String> {
        // a record node: the tree's links here, the value (from +0x20) by value below
        let (mut b, value) = match kind {
            Kind::RecordNode(_) if bytes.len() >= 0x20 => (bytes[..0x20].to_vec(), Some(&bytes[0x20..])),
            _ => (bytes.to_vec(), None),
        };
        let all: Vec<usize> = (0..b.len() / 8).map(|i| i * 8).collect();
        let (pad, ptrs): (&[(usize, usize)], &[usize]) = match kind {
            Kind::Sentinel => (&[(0x1a, usize::MAX)], &[0, 8, 0x10]),
            Kind::SetNode => (&[(0x1a, 0x1c)], &[0, 8, 0x10]),
            Kind::MapNode => (&[(0x1a, 0x20), (0x24, 0x28)], &[0, 8, 0x10, 0x28]),
            Kind::RecordNode(_) => (&[(0x1a, 0x20)], &[0, 8, 0x10]),
            Kind::ListSentinel => (&[(0x10, usize::MAX)], &[0, 8]),
            Kind::ListNode(node) => (node.pad, &[0, 8]),
            Kind::Buckets => (&[], &all),
        };
        for &(from, to) in pad {
            let to = to.min(b.len());
            if from < to {
                b[from..to].fill(0);
            }
        }
        let mut toks: Vec<String> = Vec::new();
        let mut i = 0;
        while i + 8 <= b.len() {
            let w = u64_at(&b, i);
            if w == 0 {
                let mut j = i;
                while j + 8 <= b.len() && u64_at(&b, j) == 0 {
                    j += 8;
                }
                toks.push(format!("z{}", (j - i) / 8));
                i = j;
                continue;
            }
            if ptrs.contains(&i) && pointer_like(w) {
                let p = w as usize;
                if p >= self.env.image_base && p - self.env.image_base < IMAGE_BOUND {
                    toks.push(format!("I+{:x}", p - self.env.image_base));
                } else if let Some(t) = self.block_token(p) {
                    toks.push(t);
                } else {
                    self.stats.heap_ptrs += 1;
                    if self.unresolved.len() < 40 {
                        self.unresolved.push((format!("{} {}+{:#x}", self.origin, self.label, i), p));
                    }
                    toks.push("P".into());
                }
            } else if matches!(kind, Kind::ListNode(node) if !node.literal)
                && pointer_like(w)
                && !((w as usize) >= self.env.image_base && (w as usize) - self.env.image_base < IMAGE_BOUND)
                && self.mem.read(w as usize, 1).is_some()
            {
                // a value word that is an address in this game: not carried as a literal (a foreign address in the receiver); the
                // rebuild refuses a `P`, so such a container goes the record path instead of a guess
                self.stats.heap_ptrs += 1;
                if self.unresolved.len() < 40 {
                    self.unresolved.push((format!("{} {}+{:#x}", self.origin, self.label, i), w as usize));
                }
                toks.push("P".into());
            } else {
                toks.push(format!("w:{w:016x}"));
            }
            i += 8;
        }
        if !b.len().is_multiple_of(8) {
            toks.push(format!("b:{}", hex(&b[b.len() - b.len() % 8..])));
        }
        if let (Kind::RecordNode(map), Some(value)) = (kind, value) {
            let MapValue::Record(elem) = map.value else { unreachable!("a record node is a record map's") };
            if value.len() != elem.stride {
                return Err(format!(
                    "{}: a node of {:#x} bytes for a value of {:#x}",
                    self.label,
                    bytes.len(),
                    elem.stride
                ));
            }
            let label = self.label.clone();
            toks.push(self.record_tokens(&label, value, elem, 1)?);
        }
        Ok(toks.join(" "))
    }

    /// The tokens of one record of `elem` carried by value inside a block of the graph: its scalar members as literal words, its padding
    /// (every byte no member names) zeroed, each vector member as a `V<bytes>[...]` token (the rebuild makes the vector's block from
    /// it) whose elements are typed the same way, an empty vector as three zero words (the engine's empty vector; its copy constructor
    /// copies nothing from it). No word is ever a pointer of the game that wrote the text. A member type that cannot be carried by
    /// value (a string, a container) or a vector that does not fit its element type refuses the text with the reason, never a guess.
    fn record_tokens(&mut self, path: &str, bytes: &[u8], elem: &'static Elem, depth: usize) -> R<String> {
        if depth > self.set.max_depth {
            return Err(format!("{path}: nesting is over the bound"));
        }
        let mut b = vec![0u8; elem.stride];
        let mut vecs: Vec<(usize, String)> = Vec::new();
        for fl in elem.fields {
            let end = fl.off + fl.ty.size();
            match fl.ty {
                Ty::U8 | Ty::U32 | Ty::I32 | Ty::F32 | Ty::Ent | Ty::V3 | Ty::M4 => {
                    b[fl.off..end].copy_from_slice(&bytes[fl.off..end])
                }
                Ty::Pad(_) => {}
                Ty::Vec(inner) => {
                    let p = format!("{path}.{}", fl.name);
                    let block = self.vec_block(&bytes[fl.off..end], inner.stride, &p)?;
                    if !fl.off.is_multiple_of(8) {
                        return Err(format!("layout table: {p} is not at a word of its record"));
                    }
                    let n = block.len() / inner.stride;
                    if n > self.set.max_elems {
                        return Err(format!("{p}: {n} elements are over the bound"));
                    }
                    let tok = if n == 0 {
                        "z3".to_string()
                    } else if inner.stride.is_multiple_of(8) {
                        let mut parts = Vec::with_capacity(n);
                        for (i, item) in block.chunks(inner.stride).enumerate() {
                            parts.push(self.record_tokens(&format!("{p}[{i}]"), item, inner, depth + 1)?);
                        }
                        format!("V{}[{}]", block.len(), parts.join(" "))
                    } else {
                        // elements that are not whole words (Vec3f): their bytes, members only, as one hex run
                        let mut all = Vec::with_capacity(block.len());
                        for (i, item) in block.chunks(inner.stride).enumerate() {
                            let t = self.record_tokens(&format!("{p}[{i}]"), item, inner, depth + 1)?;
                            let h = t
                                .strip_prefix("b:")
                                .ok_or_else(|| format!("layout table: {p} holds elements that are not plain bytes"))?;
                            all.push(h.to_string());
                        }
                        format!("V{}[b:{}]", block.len(), all.concat())
                    };
                    vecs.push((fl.off, tok));
                }
                _ => return Err(format!("layout table: {path}.{} cannot be carried by value", fl.name)),
            }
        }
        if vecs.is_empty() && !elem.stride.is_multiple_of(8) {
            return Ok(format!("b:{}", hex(&b)));
        }
        if !elem.stride.is_multiple_of(8) {
            return Err(format!(
                "layout table: {path} ({}) holds a vector but is not a whole number of words",
                elem.name
            ));
        }
        let mut toks: Vec<String> = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if let Some((_, t)) = vecs.iter().find(|(o, _)| *o == i) {
                toks.push(t.clone());
                i += 0x18;
                continue;
            }
            let w = u64_at(&b, i);
            toks.push(if w == 0 { "z1".to_string() } else { format!("w:{w:016x}") });
            i += 8;
        }
        Ok(toks.join(" "))
    }

    /// `len` bytes of heap were read for the structural dump: refused past the text's heap bound ([`heap_budget`]).
    fn spend(&mut self, len: usize) -> R<()> {
        self.raw_spent = self.raw_spent.saturating_add(len);
        if self.raw_spent > self.heap_budget {
            return Err(format!(
                "structural dump reads more heap than the bound ({} bytes read; the bound is {} bytes: {HEAP_PER_OWN_BYTE} x the proposal's own {} bytes, at least {HEAP_FLOOR}, at most {})",
                self.raw_spent, self.heap_budget, self.own_bytes, self.set.max_heap_bytes
            ));
        }
        Ok(())
    }

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
                    if len == 0 {
                        if self.mem.read(b as usize, 1).is_some() {
                            self.stats.vectors += 1;
                            toks.push("V0[]".into());
                            i += 3;
                            continue;
                        }
                    } else if let Some(block) = self.mem.read(b as usize, len) {
                        // only the bytes that were read count against the bound: three words that only look like a vector (residue,
                        // packed 32-bit values; unreadable in this game) used to count their whole span, so the verdict depended on the
                        // residue of the game that wrote the text (friend test 2026-10-16, key 108: refused on the host only)
                        self.spend(len)?;
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
                            self.spend(len)?;
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
                    if self.unresolved.len() < 40 {
                        self.unresolved.push((format!("{} {}+{:#x}", self.origin, self.label, i * 8), p));
                    }
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
    encode_with(mem, env, set, build, head, EncodeOpts::default())
}

/// [`encode`] with the options of a typed copy.
pub fn encode_with(
    mem: &dyn Mem,
    env: Env,
    set: Settings,
    build: &str,
    head: &Header,
    opts: EncodeOpts,
) -> Result<Portable, String> {
    if build != LAYOUT_BUILD {
        return Err(format!("layout table is for game build {LAYOUT_BUILD}, this is {build}"));
    }
    let payload = mem.read(head.payload, PAYLOAD_SIZE).ok_or("payload unreadable")?;
    if payload[0x9b8] != KIND_BUILD_PROPOSAL {
        return Err(format!("kind byte {} is not {KIND_BUILD_PROPOSAL}", payload[0x9b8]));
    }
    let own = own_bytes(&payload, set.max_block_bytes);
    let mut e = Enc {
        mem,
        opts,
        kinds: BTreeMap::new(),
        env,
        set,
        out: String::new(),
        stats: Stats::default(),
        pos: HashMap::new(),
        raw_spent: 0,
        heap_budget: heap_budget(own, set.max_heap_bytes),
        own_bytes: own,
        blocks: Vec::new(),
        by_addr: BTreeMap::new(),
        graph_bytes: 0,
        known_sizes: BTreeMap::new(),
        sentinels: Default::default(),
        origin: String::new(),
        unresolved: Vec::new(),
        label: String::new(),
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
        e.origin = e.blocks[done].origin.clone();
        e.label = format!("@B{}", done + 1);
        let tokens = match e.blocks[done].kind {
            Some(kind) => e.typed_tokens(&bytes, kind)?,
            None => e.raw_tokens(&bytes, 1)?,
        };
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
    e.stats.heap_read = e.raw_spent;
    e.stats.heap_budget = e.heap_budget;
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
    let mut by_origin: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for b in &e.blocks {
        let o = by_origin.entry(b.origin.clone()).or_default();
        o.0 += 1;
        o.1 += b.bytes.len();
    }
    let mut origins: Vec<(String, usize, usize)> = by_origin.into_iter().map(|(k, (n, by))| (k, n, by)).collect();
    origins.sort_by_key(|o| std::cmp::Reverse(o.2));
    let s = e.stats;
    let _ = writeln!(
        text,
        "=== end portable #{} lines={} strings={} vectors={} heap_ptrs_not_followed={} unlocated_refs={} unread_entries={} blocks={} block_bytes={} graph_truncated={} origins=[{}] ===",
        head.seq,
        s.lines,
        s.strings,
        s.vectors,
        s.heap_ptrs,
        s.unlocated_refs,
        s.unread_entries,
        s.blocks,
        s.block_bytes,
        s.graph_truncated,
        origins.iter().take(6).map(|(m, n, b)| format!("{m}:{n}/{b}")).collect::<Vec<_>>().join(" ")
    );
    Ok(Portable { text, stats: s, origins, unresolved: e.unresolved })
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
        // two lanes (real values of a town street) and one edge object reference
        let (lanes, objs) = (HEAP + 0x22000, HEAP + 0x23000);
        put(&mut s, 0x78, lanes as u64);
        put(&mut s, 0x80, (lanes + 0x30) as u64);
        put(&mut s, 0x88, (lanes + 0x30) as u64);
        put(&mut s, 0x48, objs as u64);
        put(&mut s, 0x50, (objs + 8) as u64);
        put(&mut s, 0x58, (objs + 8) as u64);
        let mut ln = vec![0u8; 0x30];
        for (i, (w, h, fwd, modes, off)) in
            [(3.0f32, 0.15f32, 0u8, 3u32, 0.15f32), (5.0, 0.0, 1, 28, -0.5)].iter().enumerate()
        {
            put32(&mut ln, i * 0x18, 13.89f32.to_bits());
            put32(&mut ln, i * 0x18 + 4, w.to_bits());
            put32(&mut ln, i * 0x18 + 8, h.to_bits());
            ln[i * 0x18 + 0xc] = *fwd;
            ln[i * 0x18 + 0xd] = 0x7f; // residue in the padding
            put32(&mut ln, i * 0x18 + 0x10, *modes);
            put32(&mut ln, i * 0x18 + 0x14, off.to_bits());
        }
        let mut ob = vec![0u8; 8];
        put32(&mut ob, 0, 39106);
        put32(&mut ob, 4, 1);
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
                (lanes, ln),
                (objs, ob),
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
    fn the_input_level_text_has_the_named_members_and_nothing_structural() {
        let (mem, at) = synthetic();
        let full = encode(&mem, Env { image_base: BASE }, settings(), "40420", &head(at)).expect("encodes");
        let p = encode(&mem, Env { image_base: BASE }, Settings::input_level(), "40420", &head(at)).expect("encodes");
        let l = lines(&p);
        for named in [
            "proposal.addedNodes.#	n:2",
            "toAdd[0].fileName	res:\"depots/road/road_depot/road_depot.con\"",
            "toRemove.#	n:2",
            "context.gatherBuildings	u8:01",
        ] {
            assert!(l.contains(&named), "{named} missing in {}", p.text);
        }
        assert!(!p.text.lines().skip(1).any(|x| x.contains("	raw:") || x.contains(".~")), "{}", p.text);
        assert!(p.text.len() < full.text.len(), "the construction's description is not carried");
    }

    #[test]
    fn lane_configs_and_edge_object_references_are_typed_members() {
        let (mem, at) = synthetic();
        let p = encode(&mem, Env { image_base: BASE }, Settings::input_level(), "40420", &head(at)).expect("encodes");
        let l = lines(&p);
        let f = |bits: f32| format!("f32:{:08x}", bits.to_bits());
        for (path, v) in [
            ("proposal.addedSegments[0].laneConfigs.#", "n:2".to_string()),
            ("proposal.addedSegments[0].laneConfigs[0].speed", f(13.89)),
            ("proposal.addedSegments[0].laneConfigs[0].width", f(3.0)),
            ("proposal.addedSegments[0].laneConfigs[0].forward", "u8:00".to_string()),
            ("proposal.addedSegments[0].laneConfigs[1].forward", "u8:01".to_string()),
            ("proposal.addedSegments[0].laneConfigs[1].transportModes", "u32:0000001c".to_string()),
            ("proposal.addedSegments[0].laneConfigs[1].offset", f(-0.5)),
            ("proposal.addedSegments[0].objects.#", "n:1".to_string()),
            ("proposal.addedSegments[0].objects[0].type", "u32:00000001".to_string()),
        ] {
            assert!(l.iter().any(|x| *x == format!("{path}	{v}")), "{path} {v} missing in {}", p.text);
        }
        assert!(
            l.iter().any(|x| x.starts_with("proposal.addedSegments[0].objects[0].entity	ent:old#39106")),
            "{}",
            p.text
        );
        // the padding residue is not part of the value
        assert!(!p.text.contains("7f"), "{}", p.text);
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

    /// The synthetic depot with `count` constructions (copies of its one), the first with two vectors of `len` readable zero bytes in
    /// its unnamed words and, beside them, three words that only look like vectors of almost 8 MB each (unreadable here: residue).
    fn heavy(count: usize, len: usize) -> (Fake, usize) {
        let (Fake(mut regions), at) = synthetic();
        let cons = regions.iter().find(|(a, _)| *a == HEAP + 0x9000).unwrap().1.clone();
        let (v1, v2, many) = (HEAP + 0x100_0000, HEAP + 0x200_0000, HEAP + 0x400_0000);
        let mut all = Vec::with_capacity(count * 0xe48);
        for i in 0..count {
            let mut c = cons.clone();
            if i == 0 {
                for (k, v) in [v1, v2].iter().enumerate() {
                    put(&mut c, 0x200 + k * 0x18, *v as u64);
                    put(&mut c, 0x208 + k * 0x18, (*v + len) as u64);
                    put(&mut c, 0x210 + k * 0x18, (*v + len) as u64);
                }
                for k in 0..3u64 {
                    let b = (k + 1) << 32; // packed 32-bit values: (0, k + 1), (0x7f0000, k + 1) ...
                    put(&mut c, 0x300 + k as usize * 0x18, b);
                    put(&mut c, 0x308 + k as usize * 0x18, b + 0x7f_0000);
                    put(&mut c, 0x310 + k as usize * 0x18, b + 0x7f_0000);
                }
            }
            all.extend_from_slice(&c);
        }
        let pay = regions.iter_mut().find(|(a, _)| *a == at).unwrap();
        put(&mut pay.1, 0x258, many as u64);
        put(&mut pay.1, 0x260, (many + count * 0xe48) as u64);
        put(&mut pay.1, 0x268, (many + count * 0xe48) as u64);
        regions.push((many, all));
        regions.push((v1, vec![0u8; len]));
        regions.push((v2, vec![0u8; len]));
        (Fake(regions), at)
    }

    #[test]
    fn the_heap_bound_counts_only_what_is_read_and_scales_with_the_proposal() {
        let plain = Settings { graph: false, ..settings() };
        // friend test 2026-10-16 key 108: the host refused its own copy, the friend injected it. Three words that only look like vectors
        // (unreadable) counted their whole span: 3 x 8 MB "read" without a byte read. They count nothing now.
        let (mem, at) = heavy(1, 0x1000);
        let p =
            encode(&mem, Env { image_base: BASE }, plain, "40420", &head(at)).expect("unreadable spans count nothing");
        assert_eq!(p.stats.heap_read, 0x2000, "only the two real vectors were read");
        assert_eq!(p.stats.heap_budget, HEAP_FLOOR, "a small proposal has the old bound");
        // a small proposal that really reads 10 MB behind its unnamed words is refused at the floor, and says why
        let (mem, at) = heavy(1, 5 << 20);
        let e = encode(&mem, Env { image_base: BASE }, plain, "40420", &head(at)).unwrap_err();
        assert!(e.starts_with("structural dump reads more heap than the bound"), "{e}");
        assert!(e.contains(&format!("the bound is {HEAP_FLOOR} bytes")), "{e}");
        // the same reads in a large proposal (200 constructions): its own bound is 64 x its size
        let (mem, at) = heavy(200, 5 << 20);
        let p = encode(&mem, Env { image_base: BASE }, plain, "40420", &head(at)).expect("a large proposal reads more");
        assert_eq!(p.stats.heap_read, 10 << 20);
        let own = PAYLOAD_SIZE + 200 * 0xe48 + 0x80 + 0x40 + 0x350 + 8;
        assert_eq!(p.stats.heap_budget, own * HEAP_PER_OWN_BYTE, "scaled from the proposal's own bytes");
        // and never past the hard ceiling
        let capped = Settings { max_heap_bytes: 9 << 20, ..plain };
        let e = encode(&mem, Env { image_base: BASE }, capped, "40420", &head(at)).unwrap_err();
        assert!(e.contains("at most 9437184"), "{e}");
        assert_eq!(heap_budget(usize::MAX, 64 << 20), 64 << 20);
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

    /// The rule behind `frozenNodes`: its values index `proposal.addedNodes`, and the segment(s) that touch that node are exactly the
    /// construction's own piece (entrance_new); every other added segment is a piece of a road the tool split, and
    /// `new2oldSegments` has one entry for each of those. Read from the real texts of the depots (skipped when they are absent).
    #[test]
    fn frozen_nodes_index_the_added_nodes_and_the_segments_at_that_node_are_the_constructions() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/research");
        let mut seen = 0;
        for f in ["b26-run-20261013/guest/native_input_1.txt", "b24-run-20261013/guest/native_input_1.txt"] {
            let Ok(t) = std::fs::read_to_string(root.join(f)) else { continue };
            let val = |k: &str| {
                t.lines().find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('\t'))).map(str::to_string)
            };
            let num = |k: &str| val(k).and_then(|v| v.rsplit(['#', ':']).next().and_then(|x| x.parse::<usize>().ok()));
            let frozen = num("proposal.frozenNodes[0]").expect("a frozen node");
            let node = val(&format!("proposal.addedNodes[{frozen}].entity")).expect("an added node at that index");
            let n_segs = num("proposal.addedSegments.#").expect("segments");
            let mut touching = Vec::new();
            for i in 0..n_segs {
                let at = |m: &str| val(&format!("proposal.addedSegments[{i}].{m}")).unwrap_or_default();
                if at("node0") == node || at("node1") == node {
                    touching.push(at("roadTemplate"));
                }
            }
            assert_eq!(touching.len(), 1, "{f}: {touching:?}");
            assert!(touching[0].contains("entrance_new"), "{f}: {touching:?}");
            let made = val("proposal.new2oldSegments")
                .and_then(|v| v.strip_prefix("map:size=").and_then(|x| x.parse::<usize>().ok()));
            assert_eq!(made, Some(n_segs - 1), "{f}: the tool made every other added segment");
            seen += 1;
        }
        eprintln!("frozen-node rule checked on {seen} real text(s)");
    }
}
