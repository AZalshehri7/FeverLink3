//! Trees, plants and rocks in a session: the asset brushes (`mixed_trees/plants/rocks_tool`) and the asset eraser (`vegetation_eraser_tool`),
//! owner decision of 2026-10-26 (option A of `docs/research/DESIGN_DECISIONS.md`). Lockstep like every build: no game applies a tick
//! at the click, every game (the originator's included) applies the same asset groups at the same stamp, built by the engine's own
//! element functions.
//!
//! What a tick is [the lab run landscape-probe-20261025 trees1; static research TREES_NATIVE_RESEARCH_20261026, build 40425]: a
//! `build_proposal` whose Proposal holds nothing but `toRemove` (the asset groups the brush merges, or the eraser thins), `toAdd` (one
//! nameless `ConstructionEntity` per group that results: `desc.autoRemovable` set, one subconstruction with one 0x48-byte constant element
//! and the group's models, each a `TransformedModel` {ResName, tag, transf, thin}) and, for the eraser, `old2new` (a removed group ->
//! the index of the group in `toAdd` that replaces it). The group's own transform is the identity: every model's transform holds its world
//! place. Which groups merge and where the trees go is decided before the proposal exists (the brush's random picks on the originator), so
//! the proposal itself is the record and the merge never runs on another game.
//!
//! 1. The originator: the tick's command reaches the command list's `Add` from a site that is not on the hold list and has the shape above
//!    ([`shape_of`]): it is noted ([`State::note_tool`]). When the simulation applies it, the proposal is read by value ([`capture`]: the
//!    removed ids, `old2new`, the flags, each group's cost, element and models), the command's Proposal is emptied (`inject::neutralise`,
//!    as the terrain path does: the tool is not held and gets its normal callback), and the record goes to `native_assets_out_<id>.txt`
//!    and the `assets` event. The mod turns the removed ids into references (asset group key + fingerprint) and ships it.
//! 2. Every game at the stamp: the mod checks the references against its own world, writes the text to `native_assets_in_<key>.txt`, then
//!    `assets_apply {key, rm, o2n}`, then sends the documented carrier `makeWorldBuildProposalCmd(Proposal{toRemove, old2new}, Context{player},
//!    ignoreErrors, true)`. A script cannot make a `ConstructionEntity` of a proposal (no `new` in the API), so the module fills `toAdd`
//!    ([`build_into`]): per group the engine's `ConstructionEntity()` on a local, `autoRemovable` and the cost set, the engine's resize of
//!    the subconstructions to one, its `emplace_back` of the element, its `assign` of the models (from sources the module made: the engine
//!    copies them with its own allocator), then its copy into `toAdd` (or its reallocating insert) and its destructor of the local. Every
//!    input is checked before the first engine call ([`decode`] bounds, [`check_carrier`]); a C++ exception is not something to catch. The
//!    filled `toAdd` is read back with [`capture`]'s own reader and must equal the record. Anything else empties the carrier: nothing applied
//!    here (fail closed), answered `inject_refused`, so the host sees differing verdicts and repairs.
//!
//! A single tree, plant or rock (the vegetation menu's model tiles: the construction builder, a tool on the hold list) is the same kind
//! of command [owner's two-window test 2026-10-26, held 1..6 at site 0x51c3ac, `native_text_<id>.txt`]: a Proposal of nothing but one
//! nameless, auto-removable group in `toAdd` (cost 100, as one brush tree), but with the group's own `transf` at its place and
//! `playerEntity` the command's `context.player`. So a group carries its transform and whether it is the carrier's player's
//! ([`Group::transf`], [`Group::owned`]), and the command hook does not hold a tool's command that [`capture`] reads as a tick
//! (`Runtime::assets_tool_tick`): it goes the brush's way, whichever tool made it.
//!
//! The offsets below are the engine's (the Lua binding of `ConstructionEntity`/`ConstructionResult`/`Subconstruction` and the two builders);
//! the build search proves at game start that `MakeRemoveAssetsProposal` and `MakeBuildAssetsProposal`, whose code uses exactly these
//! offsets and strides, are unchanged (`tools/addr/asset_sites.py`, the row's `asset_remover` / `asset_builder`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::cmd_patch::Poke;
use crate::dump::{pointer_like, u32_at, u64_at, Mem, PAYLOAD_SIZE};
use crate::portable::{self, Ty};

/// The name in the `ready` event's `hooks` list when ticks can be shared.
pub const READY_NAME: &str = "assets_apply";

/// Proposal members (`portable.rs`).
pub const TO_REMOVE: usize = 0x240;
pub const TO_ADD: usize = 0x258;
pub const OLD2NEW: usize = 0x270;
/// The command's flags: withCostRep, ignoreErrors, playerInitiated.
pub const FLAGS: usize = 0x3d0;
/// `context.player` of the command (`portable.rs`).
pub const PLAYER: usize = 0x36c;
/// `ConstructionEntity` (the toAdd stride; `add [rbx+8], 0xe48` in the remover).
pub const ENTITY: usize = 0xe48;
/// `desc.autoRemovable` (the binding of `ConstructionDesc`; the remover's `mov byte [local+0x4b1], 1`).
pub const AUTO_REMOVABLE: usize = 0x4b1;
/// `construction.cost` (ConstructionResult +0x128 at +0xa18; the builder's `add [local+0xb40], rax`).
pub const COST: usize = 0xb40;
/// `ConstructionEntity.transf` (Mat4f) and `playerEntity` (`portable.rs`; the binding, research section 2).
pub const GROUP_TRANSF: usize = 0xc98;
pub const GROUP_PLAYER: usize = 0xe38;
/// `construction.subconstructions` (ConstructionResult +0x18 at +0xa18; the remover's resize of `[local+0xa30]` to one).
pub const SUBCONS: usize = 0xa30;
/// `Subconstruction` (the resize's stride).
pub const SUBCON: usize = 0x318;
/// `Subconstruction.models` and the vector of the 0x48-byte element (the remover's `add rcx,0x140` / `add rcx,0x210`).
pub const MODELS: usize = 0x140;
pub const ELEMS: usize = 0x210;
pub const ELEM: usize = 0x48;
/// `TransformedModel` (`imul 0xc30c30c30c30c30d; sar 7`: a division by 0xa8): `id` (ResName, two strings) +0, `tag` +0x40, `transf`
/// +0x60, `thin` +0xa0.
pub const MODEL: usize = 0xa8;
const TAG: usize = 0x40;
const TRANSF: usize = 0x60;
const THIN: usize = 0xa0;

/// Bounds of one tick (a seen brush tick: 2 groups of 15 models, 3 removed). Anything larger is refused, never cut. The owner's
/// two-window test 2026-10-26 (a large brush held down): ticks of 76..159 groups were refused at the old bound of 64 and the
/// stroke's trees vanished; the real limit is the text's size ([`MAX_TEXT`]), so the group count only guards against garbage.
pub const MAX_GROUPS: usize = 1024;
pub const MAX_MODELS: usize = 4096;
pub const MAX_TOTAL_MODELS: usize = 20_000;
pub const MAX_REMOVE: usize = 256;
pub const MAX_STR: usize = 1024;
/// The largest tick text: it travels as one session message (Steam's 512 KiB, `steam_core::MAX_MESSAGE`) with its record around it.
/// About 220 bytes a model: some 1700 trees in one tick.
pub const MAX_TEXT: usize = 384 * 1024;

pub const DEFAULT_TTL_MS: u64 = 10_000;
pub const MAX_TTL_MS: u64 = 60_000;
pub const MAX_ORDERS: usize = 64;
const MAX_TOOLS: usize = 64;
/// Images of the originator's own groups kept for the lab's comparison with its rebuild ([`State::keep_images`]).
const MAX_IMAGES: usize = 16;

pub fn out_file_name(id: u64) -> String {
    format!("native_assets_out_{id}.txt")
}

pub fn in_file_name(key: u64) -> String {
    format!("native_assets_in_{key}.txt")
}

/// One model of a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// The ResName's two strings (the name is in the second; the first was empty in every model seen).
    pub res: [Vec<u8>; 2],
    pub tag: Vec<u8>,
    /// 16 floats, as the engine holds them.
    pub transf: [u8; 64],
    pub thin: u8,
}

/// The subconstruction's 0x48-byte element: `{u32, vector, vector, f32, f32, bool}` (the remover's constant `{0, empty, empty, 0.75, 2.5,
/// false}`); the two vectors must be empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elem {
    pub head: u32,
    pub a: u32,
    pub b: u32,
    pub flag: u8,
}

impl Elem {
    fn bytes(&self) -> [u8; ELEM] {
        let mut o = [0u8; ELEM];
        o[0..4].copy_from_slice(&self.head.to_le_bytes());
        o[0x38..0x3c].copy_from_slice(&self.a.to_le_bytes());
        o[0x3c..0x40].copy_from_slice(&self.b.to_le_bytes());
        o[0x40] = self.flag;
        o
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub cost: i64,
    pub elem: Elem,
    pub models: Vec<Model>,
    /// The group's own transform: the identity for a brush's group, the place of a single tree.
    pub transf: [u8; 64],
    /// `playerEntity` is the command's `context.player` (a single tree), not -1 (a brush's group). Every game's carrier has the same
    /// player (the mod's Context), so the flag travels, not an id.
    pub owned: bool,
}

/// One tick as it travels.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Record {
    /// withCostRep, ignoreErrors, playerInitiated of the tool's command.
    pub flags: [u8; 3],
    /// The originator's ids of the removed groups (the mod makes references of them; never used as ids on another game).
    pub remove: Vec<u32>,
    /// (removed id, index in `groups`).
    pub old2new: Vec<(u32, u32)>,
    pub groups: Vec<Group>,
}

impl Record {
    pub fn models(&self) -> usize {
        self.groups.iter().map(|g| g.models.len()).sum()
    }
}

// ------------------------------------------------------------------------------------------------------------------ reading

fn vec_of(b: &[u8], off: usize) -> (u64, u64, u64) {
    (u64_at(b, off), u64_at(b, off + 8), u64_at(b, off + 16))
}

/// Elements of a vector of `stride`, or why it is not one.
fn count(v: (u64, u64, u64), stride: usize, what: &str) -> Result<usize, String> {
    let (b, e, c) = v;
    if b == 0 && e == 0 && c == 0 {
        return Ok(0);
    }
    if !pointer_like(b)
        || e < b
        || c < e
        || !(e - b).is_multiple_of(stride as u64)
        || !(c - b).is_multiple_of(stride as u64)
    {
        return Err(format!("{what} is not a vector of {stride:#x}-byte elements ({b:#x} {e:#x} {c:#x})"));
    }
    Ok(((e - b) / stride as u64) as usize)
}

/// An MSVC `std::string` (buffer or pointer, size, capacity): its bytes, bounded, no NUL.
fn read_str(mem: &dyn Mem, at: usize) -> Result<Vec<u8>, String> {
    let b = mem.read(at, 0x20).ok_or("a string cannot be read")?;
    let (size, cap) = (u64_at(&b, 0x10) as usize, u64_at(&b, 0x18) as usize);
    if size > MAX_STR || size > cap {
        return Err(format!("a string of size {size} and capacity {cap} is not one the module carries"));
    }
    let text = if cap < 16 {
        b[..size].to_vec()
    } else {
        let p = u64_at(&b, 0);
        if !pointer_like(p) {
            return Err("a long string without its block".into());
        }
        mem.read(p as usize, size).ok_or("a string's block cannot be read")?
    };
    if text.contains(&0) {
        return Err("a string holds a NUL byte".into());
    }
    Ok(text)
}

fn finite(bytes: &[u8]) -> bool {
    bytes.chunks(4).all(|c| f32::from_le_bytes(c.try_into().unwrap_or([0; 4])).is_finite())
}

fn read_model(mem: &dyn Mem, at: usize) -> Result<Model, String> {
    let m = mem.read(at, MODEL).ok_or("a model cannot be read")?;
    let mut transf = [0u8; 64];
    transf.copy_from_slice(&m[TRANSF..TRANSF + 64]);
    if !finite(&transf) || m[THIN] > 1 {
        return Err("a model's transform or thin flag is not in the shape seen".into());
    }
    Ok(Model {
        res: [read_str(mem, at)?, read_str(mem, at + 0x20)?],
        tag: read_str(mem, at + TAG)?,
        transf,
        thin: m[THIN],
    })
}

/// One `ConstructionEntity` of `toAdd` as an asset group, or why it is not one. `player`: the command's `context.player`.
pub fn read_group(mem: &dyn Mem, at: usize, player: u32) -> Result<Group, String> {
    let e = mem.read(at, ENTITY).ok_or("a construction of toAdd cannot be read")?;
    if !read_str(mem, at)?.is_empty() || !read_str(mem, at + 0x20)?.is_empty() {
        return Err("a construction of toAdd names a file: not an asset group".into());
    }
    if e[AUTO_REMOVABLE] != 1 {
        return Err("a construction of toAdd is not auto-removable: not an asset group".into());
    }
    if count(vec_of(&e, SUBCONS), SUBCON, "its subconstructions")? != 1 {
        return Err("an asset group without exactly one subconstruction".into());
    }
    let sub_at = u64_at(&e, SUBCONS) as usize;
    let sub = mem.read(sub_at, SUBCON).ok_or("the subconstruction cannot be read")?;
    if count(vec_of(&sub, ELEMS), ELEM, "the subconstruction's element vector")? != 1 {
        return Err("a subconstruction without exactly one 0x48-byte element".into());
    }
    let el = mem.read(u64_at(&sub, ELEMS) as usize, ELEM).ok_or("the element cannot be read")?;
    if el[8..0x38].iter().any(|&b| b != 0) || !finite(&el[0x38..0x40]) || el[0x40] > 1 {
        return Err("the subconstruction's element is not in the shape seen (its vectors are not empty)".into());
    }
    let elem = Elem { head: u32_at(&el, 0), a: u32_at(&el, 0x38), b: u32_at(&el, 0x3c), flag: el[0x40] };
    let n = count(vec_of(&sub, MODELS), MODEL, "the models")?;
    if n == 0 || n > MAX_MODELS {
        return Err(format!("an asset group of {n} models is outside 1..={MAX_MODELS}"));
    }
    let base = u64_at(&sub, MODELS) as usize;
    let models = (0..n).map(|i| read_model(mem, base + i * MODEL)).collect::<Result<Vec<_>, _>>()?;
    let mut transf = [0u8; 64];
    transf.copy_from_slice(&e[GROUP_TRANSF..GROUP_TRANSF + 64]);
    if !finite(&transf) {
        return Err("an asset group's transform is not finite".into());
    }
    let owner = u32_at(&e, GROUP_PLAYER);
    let owned = owner != u32::MAX;
    if owned && owner != player {
        return Err(format!("an asset group of player {owner}, not the command's player {player}"));
    }
    Ok(Group { cost: u64_at(&e, COST) as i64, elem, models, transf, owned })
}

/// What the Proposal of a `build_proposal` payload holds when it is an asset tick: (removed, added), or `None` when any other member
/// holds something (a road, a construction tool, terrain, ...). The rule is the layout table's, not a tool's: every container of the
/// Proposal but `toRemove`, `toAdd` and `old2new` is empty and the terrain member is zero.
pub fn shape_of(bytes: &[u8]) -> Option<(usize, usize)> {
    for f in portable::PAYLOAD.iter().filter(|f| f.off < crate::native_copy::PROPOSAL_SIZE) {
        if matches!(f.name, "toRemove" | "toAdd" | "old2new") {
            continue;
        }
        let empty = match f.ty {
            Ty::Vec(_) | Ty::EntVec | Ty::BlobVec => u64_at(bytes, f.off) == u64_at(bytes, f.off + 8),
            Ty::Map(_) => u64_at(bytes, f.off + 8) == 0,
            Ty::Hash(_) | Ty::EntSet => u64_at(bytes, f.off + 0x10) == 0,
            Ty::Raw(n) => bytes[f.off..f.off + n].iter().all(|&b| b == 0),
            _ => true,
        };
        if !empty {
            return None;
        }
    }
    let rm = count(vec_of(bytes, TO_REMOVE), 4, "toRemove").ok()?;
    let add = count(vec_of(bytes, TO_ADD), ENTITY, "toAdd").ok()?;
    Some((rm, add))
}

/// The ids `toRemove` holds, in its order (empty when it cannot be read or is over the bound).
pub fn removed_ids(mem: &dyn Mem, bytes: &[u8]) -> Vec<u32> {
    match count(vec_of(bytes, TO_REMOVE), 4, "toRemove") {
        Ok(n) if n > 0 && n <= MAX_REMOVE => mem
            .read(u64_at(bytes, TO_REMOVE) as usize, 4 * n)
            .map(|b| (0..n).map(|i| u32_at(&b, 4 * i)).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The entries of `old2new` (an `unordered_map<u32, u32>`: the list's sentinel at +8 and its size at +0x10; nodes `{next, prev, key, value}`,
/// `portable::HASH_NODE_PAIR32`).
fn read_old2new(mem: &dyn Mem, bytes: &[u8]) -> Result<Vec<(u32, u32)>, String> {
    let (head, n) = (u64_at(bytes, OLD2NEW + 8), u64_at(bytes, OLD2NEW + 0x10) as usize);
    if n == 0 {
        return Ok(Vec::new());
    }
    if n > MAX_REMOVE || !pointer_like(head) {
        return Err(format!("old2new of {n} entries is not one the module carries"));
    }
    let mut out = Vec::with_capacity(n);
    let mut node = u64_at(&mem.read(head as usize, 8).ok_or("old2new cannot be read")?, 0);
    for _ in 0..n {
        let b = mem.read(node as usize, 0x18).ok_or("an old2new entry cannot be read")?;
        out.push((u32_at(&b, 0x10), u32_at(&b, 0x14)));
        node = u64_at(&b, 0);
    }
    if node != head {
        return Err("old2new's list does not end at its sentinel".into());
    }
    out.sort_unstable();
    Ok(out)
}

/// The originator: the tick of the command at `payload`, `None` when its Proposal is not an asset tick (or changes nothing).
pub fn capture(mem: &dyn Mem, payload: usize) -> Result<Option<Record>, String> {
    let bytes = mem.read(payload, PAYLOAD_SIZE).ok_or("the command cannot be read")?;
    let Some((rm, add)) = shape_of(&bytes) else { return Ok(None) };
    if rm == 0 && add == 0 {
        return Ok(None);
    }
    if rm > MAX_REMOVE || add > MAX_GROUPS {
        return Err(format!(
            "a tick that removes {rm} and adds {add} groups is over the bounds ({MAX_REMOVE}, {MAX_GROUPS}): refused"
        ));
    }
    let remove = removed_ids(mem, &bytes);
    if remove.len() != rm {
        return Err("toRemove cannot be read".into());
    }
    let base = u64_at(&bytes, TO_ADD) as usize;
    let player = u32_at(&bytes, PLAYER);
    let groups = (0..add).map(|i| read_group(mem, base + i * ENTITY, player)).collect::<Result<Vec<_>, _>>()?;
    let rec = Record {
        flags: [bytes[FLAGS], bytes[FLAGS + 1], bytes[FLAGS + 2]],
        remove,
        old2new: read_old2new(mem, &bytes)?,
        groups,
    };
    check(&rec)?;
    Ok(Some(rec))
}

/// The record's own rules (also every decoded text's): bounds, ids, indices, floats.
fn check(r: &Record) -> Result<(), String> {
    if r.groups.len() > MAX_GROUPS || r.remove.len() > MAX_REMOVE || r.models() > MAX_TOTAL_MODELS {
        return Err(format!(
            "a tick of {} groups, {} models, {} removals is over the bounds",
            r.groups.len(),
            r.models(),
            r.remove.len()
        ));
    }
    if r.groups.is_empty() && r.remove.is_empty() {
        return Err("a tick that changes nothing".into());
    }
    let mut ids = r.remove.clone();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != r.remove.len() {
        return Err("a tick removes one group twice".into());
    }
    for &(k, v) in &r.old2new {
        if !r.remove.contains(&k) || v as usize >= r.groups.len() {
            return Err(format!("old2new {k} -> {v} names no removed group or no added group"));
        }
    }
    if r.flags.iter().any(|&f| f > 1) {
        return Err("the command's flags are not booleans".into());
    }
    for g in &r.groups {
        if g.models.is_empty() || g.models.len() > MAX_MODELS || g.elem.flag > 1 {
            return Err("an asset group outside the bounds".into());
        }
        if !finite(&g.elem.a.to_le_bytes()) || !finite(&g.elem.b.to_le_bytes()) || !finite(&g.transf) {
            return Err("an element's numbers are not finite".into());
        }
        for m in &g.models {
            let strs = [&m.res[0], &m.res[1], &m.tag];
            if strs.iter().any(|s| s.len() > MAX_STR || s.contains(&0)) || !finite(&m.transf) || m.thin > 1 {
                return Err("a model outside the bounds".into());
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------------------------ the text

fn fnv(bytes: &[u8]) -> u64 {
    crate::terrain::fnv(bytes)
}

fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".into();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if s == "-" {
        return Ok(Vec::new());
    }
    if !s.len().is_multiple_of(2) || s.len() > 2 * MAX_STR + 128 {
        return Err("a hex field of the wrong length".into());
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "not hex".to_string())).collect()
}

/// `feverlink3-assets 3`, `flags a b c`, `remove n id..`, `old2new n id:index..`, then per group `group cost head a b flag models transf
/// owned` and one
/// `m res0 res1 tag transf thin` line per model (hex, `-` for an empty string), and `end <fnv of everything before>`.
pub fn encode(r: &Record) -> String {
    let mut s = String::from(
        "feverlink3-assets 3
",
    );
    s.push_str(&format!(
        "flags {} {} {}
",
        r.flags[0], r.flags[1], r.flags[2]
    ));
    let ids: Vec<String> = r.remove.iter().map(|i| i.to_string()).collect();
    s.push_str(&format!(
        "remove {}{}{}
",
        ids.len(),
        if ids.is_empty() { "" } else { " " },
        ids.join(" ")
    ));
    let o2n: Vec<String> = r.old2new.iter().map(|(k, v)| format!("{k}:{v}")).collect();
    s.push_str(&format!(
        "old2new {}{}{}
",
        o2n.len(),
        if o2n.is_empty() { "" } else { " " },
        o2n.join(" ")
    ));
    let mut dict = Dict::default();
    for g in &r.groups {
        s.push_str(&format!(
            "group {} {:08x} {:08x} {:08x} {} {} {} {}
",
            g.cost,
            g.elem.head,
            g.elem.a,
            g.elem.b,
            g.elem.flag,
            g.models.len(),
            dict.put(&g.transf),
            u8::from(g.owned)
        ));
        for m in &g.models {
            s.push_str(&format!(
                "m {} {} {} {} {}
",
                dict.put(&m.res[0]),
                dict.put(&m.res[1]),
                dict.put(&m.tag),
                b64(&m.transf),
                m.thin
            ));
        }
    }
    let sum = fnv(s.as_bytes());
    s.push_str(&format!(
        "end {sum:016x}
"
    ));
    s
}

/// Version 3 (owner 2026-10-26: a large brush's tick is too large in version 2, about 260 bytes a tree): a byte field that repeats
/// (the model's name, a group's transform) is written once as hex and later as `@<n>`, its order among the written fields; a model's
/// own transform is base64. Lossless: the decoder rebuilds the same bytes, and the read-back still compares them.
#[derive(Default)]
struct Dict {
    seen: Vec<Vec<u8>>,
}

impl Dict {
    fn put(&mut self, bytes: &[u8]) -> String {
        if bytes.is_empty() {
            return "-".into();
        }
        if let Some(i) = self.seen.iter().position(|b| b == bytes) {
            return format!("@{i}");
        }
        self.seen.push(bytes.to_vec());
        hex(bytes)
    }

    fn get(&mut self, s: &str) -> Result<Vec<u8>, String> {
        if let Some(i) = s.strip_prefix('@') {
            let i: usize = i.parse().map_err(|_| "a field reference".to_string())?;
            return self.seen.get(i).cloned().ok_or_else(|| "a field reference to nothing".to_string());
        }
        let b = unhex(s)?;
        if !b.is_empty() {
            if self.seen.len() >= MAX_TOTAL_MODELS {
                return Err("too many fields".into());
            }
            self.seen.push(b.clone());
        }
        Ok(b)
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 without padding.
fn b64(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n =
            (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for k in 0..=c.len() {
            s.push(B64[(n >> (18 - 6 * k)) as usize & 63] as char);
        }
    }
    s
}

fn unb64(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 4 == 1 || s.len() > 4 * MAX_STR {
        return Err("a base64 field of the wrong length".into());
    }
    let v = |c: u8| B64.iter().position(|&x| x == c).map(|p| p as u32).ok_or_else(|| "not base64".to_string());
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.as_bytes().chunks(4) {
        let mut n = 0u32;
        for (k, &ch) in c.iter().enumerate() {
            n |= v(ch)? << (18 - 6 * k);
        }
        for k in 0..c.len() - 1 {
            out.push((n >> (16 - 8 * k)) as u8);
        }
    }
    if b64(&out) != s {
        return Err("a base64 field that is not canonical".into());
    }
    Ok(out)
}

/// The inverse of [`encode`], with every check of [`check`] and the text's own checksum.
pub fn decode(text: &str) -> Result<Record, String> {
    if text.len() > MAX_TEXT {
        return Err(format!("an asset text of {} bytes is over the bound ({MAX_TEXT})", text.len()));
    }
    let body_end = text.rfind("end ").ok_or("an asset text without its end line")?;
    let (body, tail) = text.split_at(body_end);
    if tail.trim_end() != format!("end {:016x}", fnv(body.as_bytes())) {
        return Err("the checksum does not match the asset text".into());
    }
    let mut lines = body.lines();
    if lines.next() != Some("feverlink3-assets 3") {
        return Err("not an asset text (version 3)".into());
    }
    let mut dict = Dict::default();
    let num = |s: &str| s.parse::<u64>().map_err(|_| format!("{s:.20} is not a number"));
    let field = |l: Option<&str>, name: &str| -> Result<Vec<String>, String> {
        let l = l.ok_or_else(|| format!("no {name} line"))?;
        let f: Vec<String> = l.split(' ').map(str::to_owned).collect();
        (f[0] == name).then_some(f).ok_or_else(|| format!("{l:.30}: not the {name} line"))
    };
    let f = field(lines.next(), "flags")?;
    if f.len() != 4 {
        return Err("a flags line of the wrong length".into());
    }
    let mut r = Record::default();
    for i in 0..3 {
        r.flags[i] = u8::try_from(num(&f[i + 1])?).map_err(|_| "a flag out of range".to_string())?;
    }
    let f = field(lines.next(), "remove")?;
    let n = num(&f[1])? as usize;
    if n > MAX_REMOVE || f.len() != n + 2 {
        return Err("a remove line of the wrong length".into());
    }
    r.remove =
        f[2..].iter().map(|v| v.parse::<u32>().map_err(|_| "a removed id".to_string())).collect::<Result<_, _>>()?;
    let f = field(lines.next(), "old2new")?;
    let n = num(&f[1])? as usize;
    if n > MAX_REMOVE || f.len() != n + 2 {
        return Err("an old2new line of the wrong length".into());
    }
    for p in &f[2..] {
        let (k, v) = p.split_once(':').ok_or("an old2new entry")?;
        r.old2new.push((k.parse().map_err(|_| "an old2new id")?, v.parse().map_err(|_| "an old2new index")?));
    }
    while let Some(l) = lines.next() {
        let g: Vec<&str> = l.split(' ').collect();
        if g.len() != 9 || g[0] != "group" || r.groups.len() >= MAX_GROUPS {
            return Err(format!("{l:.30}: not a group line"));
        }
        let h = |s: &str| u32::from_str_radix(s, 16).map_err(|_| "a group's element".to_string());
        let elem =
            Elem { head: h(g[2])?, a: h(g[3])?, b: h(g[4])?, flag: u8::try_from(num(g[5])?).map_err(|_| "a flag")? };
        let transf: [u8; 64] =
            dict.get(g[7])?.try_into().map_err(|_| "a group's transform of the wrong length".to_string())?;
        let n = num(g[6])? as usize;
        if n == 0 || n > MAX_MODELS || r.models() + n > MAX_TOTAL_MODELS {
            return Err(format!("a group of {n} models is outside the bounds"));
        }
        let mut models = Vec::with_capacity(n);
        for _ in 0..n {
            let l = lines.next().ok_or("a group ends before its models")?;
            let m: Vec<&str> = l.split(' ').collect();
            if m.len() != 6 || m[0] != "m" {
                return Err(format!("{l:.30}: not a model line"));
            }
            let res = [dict.get(m[1])?, dict.get(m[2])?];
            let tag = dict.get(m[3])?;
            let transf: [u8; 64] =
                unb64(m[4])?.try_into().map_err(|_| "a transform of the wrong length".to_string())?;
            let thin = u8::try_from(num(m[5])?).map_err(|_| "a thin flag")?;
            models.push(Model { res, tag, transf, thin });
        }
        let owned = match g[8] {
            "0" => false,
            "1" => true,
            _ => return Err("a group's owner flag".into()),
        };
        r.groups.push(Group {
            cost: g[1].parse().map_err(|_| "a group's cost".to_string())?,
            elem,
            models,
            transf,
            owned,
        });
    }
    check(&r)?;
    Ok(r)
}

// ------------------------------------------------------------------------------------------------------------------ the carrier

/// The carrier the mod sent must be what the order and the record say: a player build of kind 52 whose Proposal removes `rm` entities, has
/// `o2n` entries in `old2new`, adds nothing and holds nothing else, with the tool's own flags. Checked before any engine call.
pub fn check_carrier(mem: &dyn Mem, payload: usize, rec: &Record) -> Result<(), String> {
    let bytes = mem.read(payload, PAYLOAD_SIZE).ok_or("the carrier cannot be read")?;
    if bytes[0x9b8] != portable::KIND_BUILD_PROPOSAL {
        return Err("the carrier is not a build_proposal".into());
    }
    let (rm, add) = shape_of(&bytes).ok_or("the carrier holds more than toRemove and old2new")?;
    if add != 0 {
        return Err("the carrier's toAdd is not empty".into());
    }
    if rm != rec.remove.len() || u64_at(&bytes, OLD2NEW + 0x10) as usize != rec.old2new.len() {
        return Err(format!(
            "the carrier removes {rm} with {} old2new entries, the tick {} with {}",
            u64_at(&bytes, OLD2NEW + 0x10),
            rec.remove.len(),
            rec.old2new.len()
        ));
    }
    let flags = [bytes[FLAGS], bytes[FLAGS + 1], bytes[FLAGS + 2]];
    if flags != rec.flags {
        return Err(format!(
            "the carrier's flags (withCostRep, ignoreErrors, playerInitiated) are {flags:?}, the tool's {:?}",
            rec.flags
        ));
    }
    Ok(())
}

/// The engine's element functions (`AssetFns` in the runtime; a fake one in the tests). Each is an engine function of that signature,
/// compared with the bytes the build search recorded before it is ever called.
///
/// # Safety
/// Every method calls engine code on raw addresses: each argument must be what that function takes (a block of [`ENTITY`] bytes for an
/// entity, the address of a live vector of that element type, a valid source), as [`build_into`] passes them.
#[allow(clippy::missing_safety_doc)] // the trait's own Safety section covers every method
pub trait Engine {
    /// `ConstructionEntity::ConstructionEntity()` on `at`.
    unsafe fn ctor(&self, at: *mut u8);
    unsafe fn dtor(&self, at: *mut u8);
    /// `ConstructionEntity(const ConstructionEntity&)` into the uninitialised `dst`.
    unsafe fn copy(&self, dst: *mut u8, src: *const u8);
    /// `std::vector<ConstructionEntity>::_Emplace_reallocate(where, const&)`.
    unsafe fn insert(&self, vec: *mut u8, at: *mut u8, val: *const u8);
    /// `std::vector<Subconstruction>::resize(n)`.
    unsafe fn resize(&self, vec: *mut u8, n: usize);
    /// `std::vector<element>::emplace_back(T&&)`.
    unsafe fn push(&self, vec: *mut u8, val: *mut u8);
    /// `std::vector<TransformedModel>::assign(first, count)` (a copy: the sources stay the module's).
    unsafe fn assign(&self, vec: *mut u8, first: *const u8, n: usize);
}

/// A local `ConstructionEntity` (the engine keeps it on its stack; here a zeroed, aligned Rust block).
#[repr(C, align(64))]
struct Local([u8; ENTITY]);

/// The sources of one group's models: TransformedModels in Rust memory whose strings are SSO or point at Rust blocks (kept alive here);
/// the engine's assign copies them with its own allocator.
struct Sources {
    elems: Vec<u8>,
    _blocks: Vec<Vec<u8>>,
}

fn put_str(dst: &mut [u8], s: &[u8], blocks: &mut Vec<Vec<u8>>) {
    if s.len() <= 15 {
        dst[..s.len()].copy_from_slice(s);
        dst[0x18..0x20].copy_from_slice(&15u64.to_le_bytes());
    } else {
        let mut b = s.to_vec();
        b.push(0);
        dst[..8].copy_from_slice(&(b.as_ptr() as u64).to_le_bytes());
        dst[0x18..0x20].copy_from_slice(&(s.len() as u64).to_le_bytes());
        blocks.push(b);
    }
    dst[0x10..0x18].copy_from_slice(&(s.len() as u64).to_le_bytes());
}

fn sources(models: &[Model]) -> Sources {
    let mut elems = vec![0u8; models.len() * MODEL];
    let mut blocks = Vec::new();
    for (i, m) in models.iter().enumerate() {
        let e = &mut elems[i * MODEL..(i + 1) * MODEL];
        put_str(&mut e[0..0x20], &m.res[0], &mut blocks);
        put_str(&mut e[0x20..0x40], &m.res[1], &mut blocks);
        put_str(&mut e[TAG..TAG + 0x20], &m.tag, &mut blocks);
        e[TRANSF..TRANSF + 64].copy_from_slice(&m.transf);
        e[THIN] = m.thin;
    }
    Sources { elems, _blocks: blocks }
}

/// The defaults the engine's constructor must leave in a local before the module sets anything, as the remover relies on them: no
/// subconstruction yet (it resizes from none to one) and no file name (the sizes of the ResName's two strings). A constructor that leaves
/// something else is not the one the module knows (refused before anything is copied into the carrier). The other defaults are not
/// checked here: the read-back of the carrier and, on the originator, the comparison with the tool's own groups show them.
fn defaults_seen(l: &[u8]) -> bool {
    vec_of(l, SUBCONS) == (0, 0, 0) && u64_at(l, 0x10) == 0 && u64_at(l, 0x30) == 0
}

/// Every game: builds the record's groups into the (checked) carrier's empty `toAdd`, as the engine's remover does, then reads `toAdd` back
/// and requires it to equal the record. On an error after the first group was copied in, the caller empties the carrier (its destructor
/// frees what the engine copied in).
///
/// # Safety
/// `payload` is the carrier the simulation is about to apply, accepted by [`check_carrier`] for `rec`; `eng` holds verified engine functions.
pub unsafe fn build_into(
    eng: &dyn Engine,
    mem: &dyn Mem,
    poke: &dyn Poke,
    payload: usize,
    rec: &Record,
) -> Result<(), String> {
    let vec = (payload + TO_ADD) as *mut u8;
    let player = mem.read(payload + PLAYER, 4).map(|b| u32_at(&b, 0)).ok_or("the carrier's player cannot be read")?;
    for (i, g) in rec.groups.iter().enumerate() {
        let src = sources(&g.models);
        let mut local = Box::new(Local([0u8; ENTITY]));
        let at = local.0.as_mut_ptr();
        // SAFETY: a zeroed, aligned block of the entity's size; constructed once here and destroyed once below.
        unsafe { eng.ctor(at) };
        if !defaults_seen(&local.0) {
            // SAFETY: constructed just above.
            unsafe { eng.dtor(at) };
            return Err(
                "the engine's ConstructionEntity() left other defaults than the ones known: nothing built".into()
            );
        }
        local.0[AUTO_REMOVABLE] = 1;
        local.0[COST..COST + 8].copy_from_slice(&g.cost.to_le_bytes());
        // plain members the entity's copy constructor copies as they are: the group's place, and its owner (the carrier's player)
        local.0[GROUP_TRANSF..GROUP_TRANSF + 64].copy_from_slice(&g.transf);
        let owner = if g.owned { player } else { u32::MAX };
        local.0[GROUP_PLAYER..GROUP_PLAYER + 4].copy_from_slice(&owner.to_le_bytes());
        // SAFETY: the subconstructions vector of the constructed local; then its one element's own vectors.
        unsafe { eng.resize(at.add(SUBCONS), 1) };
        let sub = u64_at(&local.0, SUBCONS) as usize;
        let ok = sub != 0 && u64_at(&local.0, SUBCONS + 8) as usize == sub + SUBCON;
        if ok {
            let mut el = g.elem.bytes();
            // SAFETY: the element vector of the subconstruction the resize made; `el` is a valid element with empty vectors (moved from).
            unsafe {
                eng.push((sub + ELEMS) as *mut u8, el.as_mut_ptr());
                eng.assign((sub + MODELS) as *mut u8, src.elems.as_ptr(), g.models.len());
            }
        }
        let placed = ok && {
            let v = mem.read(payload + TO_ADD, 24).ok_or("the carrier's toAdd cannot be read")?;
            let (_, e, c) = vec_of(&v, 0);
            if e != c && e != 0 {
                // SAFETY: room for one more element at the vector's end (the engine's own fast path, then `end += 0xe48`).
                unsafe { eng.copy(e as *mut u8, at) };
                poke.poke(payload + TO_ADD + 8, &(e + ENTITY as u64).to_le_bytes())
            } else {
                // SAFETY: the carrier's vector, its end, a constructed source.
                unsafe { eng.insert(vec, e as *mut u8, at) };
                true
            }
        };
        // SAFETY: the local constructed above, destroyed once.
        unsafe { eng.dtor(at) };
        drop(src);
        if !ok {
            return Err(format!("group {i}: the engine's resize did not make one subconstruction"));
        }
        if !placed {
            return Err(format!("group {i}: the carrier's toAdd end could not be written"));
        }
    }
    let back = capture_groups(mem, payload)?;
    if back != rec.groups {
        return Err(format!(
            "the carrier's toAdd read back ({} groups, {} models) differs from the tick ({} groups, {} models)",
            back.len(),
            back.iter().map(|g| g.models.len()).sum::<usize>(),
            rec.groups.len(),
            rec.models()
        ));
    }
    Ok(())
}

fn capture_groups(mem: &dyn Mem, payload: usize) -> Result<Vec<Group>, String> {
    let v = mem.read(payload + TO_ADD, 24).ok_or("the carrier's toAdd cannot be read back")?;
    let player =
        mem.read(payload + PLAYER, 4).map(|b| u32_at(&b, 0)).ok_or("the carrier's player cannot be read back")?;
    let n = count(vec_of(&v, 0), ENTITY, "the carrier's toAdd")?;
    let base = u64_at(&v, 0) as usize;
    (0..n.min(MAX_GROUPS + 1)).map(|i| read_group(mem, base + i * ENTITY, player)).collect()
}

// ------------------------------------------------------------------------------------------------------------------ the lab's image

/// A group's words, a heap pointer as `None` ([`image`]).
pub type Image = Vec<Option<u64>>;

/// The words of a group's entity, subconstruction and element, a heap pointer as `None` (its address differs on every game and run): what
/// the originator keeps of its own groups to compare with its rebuild ([`image_diff`], a diagnostic for the lab; padding residue of the
/// engine's copies may differ, so it never refuses).
pub fn image(mem: &dyn Mem, at: usize) -> Option<Image> {
    let e = mem.read(at, ENTITY)?;
    let sub = mem.read(u64_at(&e, SUBCONS) as usize, SUBCON)?;
    let el = mem.read(u64_at(&sub, ELEMS) as usize, ELEM)?;
    let words = |b: &[u8]| {
        (0..b.len() / 8).map(|i| u64_at(b, 8 * i)).map(|w| (!pointer_like(w)).then_some(w)).collect::<Vec<_>>()
    };
    Some([words(&e), words(&sub), words(&el)].concat())
}

/// The byte offsets (entity 0.., subconstruction from 0x10000, element from 0x20000) where two images differ, at most `max`.
pub fn image_diff(a: &[Option<u64>], b: &[Option<u64>], max: usize) -> Vec<String> {
    let at = |i: usize| {
        let (e, s) = (ENTITY / 8, SUBCON / 8);
        if i < e {
            format!("+{:#x}", 8 * i)
        } else if i < e + s {
            format!("sub+{:#x}", 8 * (i - e))
        } else {
            format!("elem+{:#x}", 8 * (i - e - s))
        }
    };
    a.iter()
        .zip(b)
        .enumerate()
        .filter(|(_, (x, y))| x != y)
        .take(max)
        .map(|(i, (x, y))| format!("{}:{x:x?}/{y:x?}", at(i)))
        .collect()
}

// ------------------------------------------------------------------------------------------------------------------ the state

/// One `assets_apply` order: the record (or why its text was refused) for the next carrier that removes exactly `ids` (this game's own
/// ids, in the order the mod put them in the carrier) and has `o2n` old2new entries: a positive identity, so that no other build that
/// only removes things (a bulldozer's replay) can take the order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub key: u64,
    pub ids: Vec<u32>,
    pub o2n: u64,
    pub rec: Result<Record, String>,
    /// The text's checksum: the originator finds its own kept images by it.
    pub sum: u64,
    pub armed_ms: u64,
    pub ttl_ms: u64,
}

/// What the apply hook does with one `build_proposal`.
#[derive(Debug, PartialEq, Eq)]
pub enum Apply {
    Pass,
    /// A tool's own tick: capture, empty, ship.
    Tool,
    Carrier(Order),
}

#[derive(Default)]
pub struct State {
    share: AtomicBool,
    pending: AtomicBool,
    next_out: AtomicU64,
    tools: Mutex<VecDeque<usize>>,
    orders: Mutex<VecDeque<Order>>,
    /// (text checksum, the images of its groups) of the originator's own ticks.
    images: Mutex<VecDeque<(u64, Vec<Image>)>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl State {
    pub fn share(&self) -> bool {
        self.share.load(Ordering::Acquire)
    }

    /// Returns whether the switch changed.
    pub fn set_share(&self, on: bool) -> bool {
        let was = self.share.swap(on, Ordering::AcqRel);
        if !on {
            lock(&self.tools).clear();
        }
        was != on
    }

    pub fn pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    pub fn next_out_id(&self) -> u64 {
        self.next_out.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// The command list's `Add` of an asset tick from a site off the hold list.
    pub fn note_tool(&self, payload: usize) {
        let mut t = lock(&self.tools);
        t.retain(|&p| p != payload);
        t.push_back(payload);
        while t.len() > MAX_TOOLS {
            t.pop_front();
        }
    }

    pub fn arm(&self, order: Order) -> Option<Order> {
        let mut q = lock(&self.orders);
        q.push_back(order);
        let dropped = (q.len() > MAX_ORDERS).then(|| q.pop_front()).flatten();
        self.pending.store(true, Ordering::Release);
        dropped
    }

    pub fn expire(&self, now: u64) -> Vec<Order> {
        let mut q = lock(&self.orders);
        let (keep, gone): (VecDeque<Order>, VecDeque<Order>) =
            q.drain(..).partition(|o| now.saturating_sub(o.armed_ms) <= o.ttl_ms);
        *q = keep;
        self.pending.store(!q.is_empty(), Ordering::Release);
        gone.into()
    }

    /// The oldest order that waits for a carrier that removes `ids` with `o2n` old2new entries.
    pub fn take_order(&self, ids: &[u32], o2n: usize) -> Option<Order> {
        let mut q = lock(&self.orders);
        let i = q.iter().position(|o| o.ids == ids && o.o2n as usize == o2n)?;
        let o = q.remove(i);
        self.pending.store(!q.is_empty(), Ordering::Release);
        o
    }

    /// Decides for a `build_proposal` the simulation is about to apply: `shape` from [`shape_of`] (`None`: not an asset tick), `ids` what it
    /// removes ([`removed_ids`]), `o2n` its old2new entries, `player` its playerInitiated flag.
    pub fn classify(
        &self,
        payload: usize,
        shape: Option<(usize, usize)>,
        ids: &[u32],
        o2n: usize,
        player: bool,
    ) -> Apply {
        let tool = {
            let mut t = lock(&self.tools);
            let at = t.iter().position(|&p| p == payload);
            at.map(|i| t.remove(i)).is_some()
        };
        let Some((rm, add)) = shape else { return Apply::Pass };
        // an order first: during a pause the interface half sends the mod's own carrier, which passes the command list's `Add` from a
        // site off the hold list like a tool's tick (as a terrain carrier does, terrain.rs) and must never be taken for one
        if add == 0 && player && self.pending() && ids.len() == rm {
            if let Some(o) = self.take_order(ids, o2n) {
                return Apply::Carrier(o);
            }
        }
        if tool && player && self.share() && (rm > 0 || add > 0) {
            return Apply::Tool;
        }
        Apply::Pass
    }

    pub fn keep_images(&self, sum: u64, images: Vec<Image>) {
        let mut q = lock(&self.images);
        q.push_back((sum, images));
        while q.len() > MAX_IMAGES {
            q.pop_front();
        }
    }

    pub fn take_images(&self, sum: u64) -> Option<Vec<Image>> {
        let mut q = lock(&self.images);
        let i = q.iter().position(|(s, _)| *s == sum)?;
        q.remove(i).map(|(_, v)| v)
    }
}

/// The checksum of a text (the key of the kept images).
pub fn text_sum(text: &str) -> u64 {
    fnv(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Memory of this process (the tests' blocks are Rust allocations); every address the tests read is one they made.
    struct Raw;
    impl Mem for Raw {
        fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
            if addr == 0 {
                return None;
            }
            // SAFETY: test-only, addresses of live test blocks.
            Some(unsafe { std::slice::from_raw_parts(addr as *const u8, len) }.to_vec())
        }
    }
    impl Poke for Raw {
        fn poke(&self, addr: usize, data: &[u8]) -> bool {
            // SAFETY: test-only, a live test block.
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
            true
        }
    }

    /// A block that lives to the end of the test.
    fn leak(n: usize) -> *mut u8 {
        Box::leak(vec![0u8; n.max(1)].into_boxed_slice()).as_mut_ptr()
    }
    fn put(at: *mut u8, off: usize, v: u64) {
        // SAFETY: test blocks.
        unsafe { std::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), at.add(off), 8) };
    }
    fn vector(at: *mut u8, off: usize, block: *mut u8, len: usize) {
        put(at, off, block as u64);
        put(at, off + 8, block as u64 + len as u64);
        put(at, off + 16, block as u64 + len as u64);
    }

    /// The engine as the module expects it, on Rust memory: the constructor leaves the known defaults, the vectors grow by copies.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<&'static str>>,
    }
    impl Engine for Fake {
        unsafe fn ctor(&self, at: *mut u8) {
            self.calls.borrow_mut().push("ctor");
            put(at, 0x18, 15);
            put(at, 0x38, 15);
        }
        unsafe fn dtor(&self, _at: *mut u8) {
            self.calls.borrow_mut().push("dtor");
        }
        unsafe fn copy(&self, dst: *mut u8, src: *const u8) {
            self.calls.borrow_mut().push("copy");
            // SAFETY: test blocks of an entity's size.
            unsafe { std::ptr::copy_nonoverlapping(src, dst, ENTITY) };
        }
        unsafe fn insert(&self, vec: *mut u8, _at: *mut u8, val: *const u8) {
            self.calls.borrow_mut().push("insert");
            let v = Raw.read(vec as usize, 24).unwrap();
            let n = (u64_at(&v, 8) - u64_at(&v, 0)) as usize / ENTITY;
            let block = leak((n + 2) * ENTITY);
            // SAFETY: test blocks.
            unsafe {
                if n > 0 {
                    std::ptr::copy_nonoverlapping(u64_at(&v, 0) as *const u8, block, n * ENTITY);
                }
                std::ptr::copy_nonoverlapping(val, block.add(n * ENTITY), ENTITY);
            }
            put(vec, 0, block as u64);
            put(vec, 8, block as u64 + ((n + 1) * ENTITY) as u64);
            put(vec, 16, block as u64 + ((n + 2) * ENTITY) as u64);
        }
        unsafe fn resize(&self, vec: *mut u8, n: usize) {
            self.calls.borrow_mut().push("resize");
            vector(vec, 0, leak(n * SUBCON), n * SUBCON);
        }
        unsafe fn push(&self, vec: *mut u8, val: *mut u8) {
            self.calls.borrow_mut().push("push");
            let b = leak(ELEM);
            // SAFETY: test blocks.
            unsafe { std::ptr::copy_nonoverlapping(val, b, ELEM) };
            vector(vec, 0, b, ELEM);
        }
        unsafe fn assign(&self, vec: *mut u8, first: *const u8, n: usize) {
            self.calls.borrow_mut().push("assign");
            let b = leak(n * MODEL);
            // the engine copies the strings into its own blocks: so does the fake (the sources are freed after the call)
            for i in 0..n {
                let m = read_model(&Raw, first as usize + i * MODEL).unwrap();
                let e = Box::leak(sources(std::slice::from_ref(&m)).elems.into_boxed_slice());
                for (s, off) in [(&m.res[0], 0usize), (&m.res[1], 0x20), (&m.tag, TAG)] {
                    if s.len() > 15 {
                        let keep = Box::leak(s.iter().copied().chain([0]).collect::<Vec<u8>>().into_boxed_slice());
                        e[off..off + 8].copy_from_slice(&(keep.as_ptr() as u64).to_le_bytes());
                    }
                }
                // SAFETY: test blocks.
                unsafe { std::ptr::copy_nonoverlapping(e.as_ptr(), b.add(i * MODEL), MODEL) };
            }
            vector(vec, 0, b, n * MODEL);
        }
    }

    fn model(name: &str, x: f32) -> Model {
        let mut transf = [0u8; 64];
        for (i, v) in [1.0f32, 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., x, 2.5, 7.0, 1.0].iter().enumerate() {
            transf[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
        }
        Model { res: [Vec::new(), name.as_bytes().to_vec()], tag: Vec::new(), transf, thin: 1 }
    }

    const PLAYER_ID: u32 = 35419;

    /// The tool's command as the brush makes it: two removed groups, one replaced (old2new), two new groups.
    fn tick(rec: &Record) -> usize {
        let p = leak(PAYLOAD_SIZE);
        // SAFETY: test block.
        unsafe {
            *p.add(0x9b8) = portable::KIND_BUILD_PROPOSAL;
            std::ptr::copy_nonoverlapping(PLAYER_ID.to_le_bytes().as_ptr(), p.add(PLAYER), 4);
        }
        for (i, f) in rec.flags.iter().enumerate() {
            // SAFETY: test block.
            unsafe { *p.add(FLAGS + i) = *f };
        }
        let rm = leak(4 * rec.remove.len());
        for (i, id) in rec.remove.iter().enumerate() {
            // SAFETY: test block.
            unsafe { std::ptr::copy_nonoverlapping(id.to_le_bytes().as_ptr(), rm.add(4 * i), 4) };
        }
        vector(p, TO_REMOVE, rm, 4 * rec.remove.len());
        // old2new: a sentinel and one node per entry
        let head = leak(0x18);
        let mut prev = head;
        for (k, v) in &rec.old2new {
            let node = leak(0x18);
            put(prev, 0, node as u64);
            put(node, 0x10, u64::from(*k) | (u64::from(*v) << 32));
            prev = node;
        }
        put(prev, 0, head as u64);
        put(p, OLD2NEW + 8, head as u64);
        put(p, OLD2NEW + 0x10, rec.old2new.len() as u64);
        let fake = Fake::default();
        // the tool's toAdd is built the way the carrier's is (the fake engine), which is also what the test checks against the record
        unsafe {
            build_into(&fake, &Raw, &Raw, p as usize, &Record { groups: rec.groups.clone(), ..Record::default() })
        }
        .unwrap();
        p as usize
    }

    #[test]
    fn a_tick_travels_as_text_and_is_built_into_its_carrier_by_the_engines_functions() {
        let long = "::/assets/vegetation/trees/brleaf_05/brleaf_05.mdl";
        let elem = Elem { head: 0, a: 0.75f32.to_bits(), b: 2.5f32.to_bits(), flag: 0 };
        let mut id = [0u8; 64];
        for i in [0, 5, 10, 15] {
            id[4 * i..4 * i + 4].copy_from_slice(&1.0f32.to_le_bytes());
        }
        let rec = Record {
            flags: [1, 1, 1],
            remove: vec![39569, 20718],
            old2new: vec![(20718, 1)],
            groups: vec![
                Group {
                    cost: 1200,
                    elem,
                    models: vec![model(long, 1.0), model("rock.mdl", 2.0)],
                    transf: id,
                    owned: false,
                },
                Group { cost: 0, elem, models: vec![model(long, 3.0)], transf: id, owned: false },
                // a single tree (the construction builder): the group at its place, the command's player's
                Group {
                    cost: 100,
                    elem,
                    models: vec![model(long, 0.0)],
                    transf: model("", -194.5).transf,
                    owned: true,
                },
            ],
        };
        let tool = tick(&rec);
        let bytes = Raw.read(tool, PAYLOAD_SIZE).unwrap();
        assert_eq!(shape_of(&bytes), Some((2, 3)));
        let got = capture(&Raw, tool).unwrap().unwrap();
        assert_eq!(got, rec);
        let text = encode(&got);
        assert_eq!(decode(&text).unwrap(), rec);
        assert_eq!(text.matches(&hex(long.as_bytes())).count(), 1, "a repeated model name is written once (version 3)");
        assert!(text.contains(" @0 ") && text.contains(" @1 "), "later uses refer back: {text}");
        for n in 0..70 {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37 + n) as u8).collect();
            assert_eq!(unb64(&b64(&bytes)).unwrap(), bytes);
        }
        assert!(
            unb64("A").is_err() && unb64("AB=C").is_err() && unb64("AB").is_err(),
            "short, foreign or not canonical"
        );
        assert!(decode(&text.replacen("group 1200", "group 1201", 1)).unwrap_err().contains("checksum"));
        assert!(
            decode(&" ".repeat(MAX_TEXT + 1)).unwrap_err().contains("over the bound"),
            "a text too large for one message"
        );
        // the carrier: the mod's Proposal{toRemove, old2new}, toAdd empty; built, read back equal, the engine called in the remover's order
        let carrier = tick(&Record { groups: Vec::new(), ..rec.clone() });
        check_carrier(&Raw, carrier, &rec).unwrap();
        let fake = Fake::default();
        unsafe { build_into(&fake, &Raw, &Raw, carrier, &rec) }.unwrap();
        assert_eq!(capture(&Raw, carrier).unwrap().unwrap(), rec);
        assert_eq!(
            fake.calls.borrow().as_slice(),
            [
                "ctor", "resize", "push", "assign", "insert", "dtor", "ctor", "resize", "push", "assign", "copy",
                "dtor", "ctor", "resize", "push", "assign", "insert", "dtor"
            ]
        );
        // another player's carrier reads the owned group back as foreign: refused
        // SAFETY: test block.
        unsafe { std::ptr::copy_nonoverlapping(7u32.to_le_bytes().as_ptr(), (carrier as *mut u8).add(PLAYER), 4) };
        assert!(capture(&Raw, carrier).unwrap_err().contains("not the command's player"));
        // a carrier with other flags, or one that already adds, is refused before any engine call
        let other = tick(&Record { flags: [1, 0, 1], groups: Vec::new(), ..rec.clone() });
        assert!(check_carrier(&Raw, other, &rec).unwrap_err().contains("flags"));
        assert!(check_carrier(&Raw, tool, &rec).unwrap_err().contains("toAdd"));
        // a road beside it is not an asset tick
        let mut road = bytes.clone();
        road[8..16].copy_from_slice(&1u64.to_le_bytes());
        assert_eq!(shape_of(&road), None);
    }

    #[test]
    fn the_apply_tells_tools_and_carriers_apart() {
        let s = State::default();
        assert_eq!(s.classify(0x10, Some((1, 2)), &[5], 0, true), Apply::Pass, "not noted at Add");
        s.note_tool(0x10);
        assert_eq!(s.classify(0x10, Some((1, 2)), &[5], 0, true), Apply::Pass, "sharing off: as today");
        s.set_share(true);
        s.note_tool(0x10);
        assert_eq!(s.classify(0x10, None, &[5], 0, true), Apply::Pass, "not an asset tick");
        s.note_tool(0x10);
        assert_eq!(
            s.classify(0x10, Some((1, 2)), &[5], 0, false),
            Apply::Pass,
            "a script's (reforestation's) flag: never a tick"
        );
        s.note_tool(0x10);
        assert_eq!(s.classify(0x10, Some((1, 2)), &[5], 0, true), Apply::Tool);
        let order = |key, ids: &[u32]| Order {
            key,
            ids: ids.to_vec(),
            o2n: 1,
            rec: Err("x".into()),
            sum: 0,
            armed_ms: 0,
            ttl_ms: 100,
        };
        s.arm(order(7, &[1, 2, 3]));
        s.arm(order(8, &[4, 5]));
        assert_eq!(s.classify(0x20, Some((2, 0)), &[4, 5], 1, false), Apply::Pass, "not a player build");
        assert_eq!(
            s.classify(0x20, Some((2, 0)), &[5, 4], 1, true),
            Apply::Pass,
            "other entities: a bulldozer's replay keeps its own way"
        );
        assert!(matches!(s.classify(0x20, Some((2, 0)), &[4, 5], 1, true), Apply::Carrier(o) if o.key == 8));
        assert_eq!(s.expire(101).len(), 1);
        assert!(!s.pending());
    }
}
