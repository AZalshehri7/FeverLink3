//! Terrain edits in a session: heights (raise, lower, flatten, smooth, Roughen) and ground paint (owner decisions of 2026-10-25).
//! Lockstep like every build: no game applies an edit at the click, every game (the originator's included) applies the same data at the
//! same stamp.
//!
//! 1. The originator: a terrain tool's command reaches the command list's `Add` (a script's own send from the simulation state never
//!    does) and is noted ([`State::note_tool`]). When the simulation applies it, its terrain member is copied by value ([`capture`]) and
//!    the command's Proposal is emptied (the engine's own empty Proposal assigned into it, `inject::neutralise`), so the click applies
//!    nothing; it is not held (holding the terrain edit crashed the tool in `UI::TerrainModifier`: MPFever (MIT),
//!    `native/mpfever_native.cpp:309`) and the tool gets its normal callback. The data goes to `native_terrain_out_<id>.txt` and the
//!    `terrain` event; the mod ships it like any action.
//! 2. Every game at the stamp: the mod writes the text to `native_terrain_in_<key>.txt`, then `terrain_apply {key, x0, y0, w, h}`, then
//!    sends the documented carrier `makeWorldBuildProposalCmd(Proposal, Context{player}, false, true)`: with a height edit its
//!    `terrain.baseHeightMod = GridVec2f.new(x0, y0, w, h)` (a script can make a grid of any size but cannot write its cells: `GridVec2f`
//!    has `at`, no setter), so the engine allocated the cells and they are copied in place ([`write_cells`]); a paint-only edit's carrier
//!    is an empty Proposal (`w = h = 0` in the order) and the paint grids are made in it with the game's own allocator ([`write_paint`],
//!    the allocator `inject` already uses). Everything written is read back and compared with the text. Anything else empties the carrier:
//!    nothing applied here (fail closed), answered `inject_refused`; success is `injected` (the answers of `inject_next`).
//!
//! The layout of the Proposal's `terrain` member (0x80 bytes at payload +0x2d8, the binding's offsets, `portable.rs`):
//!   +0x00 `baseHeightMod`: a grid `{i32 x0, y0, w, h; cell* begin, *end, *capacity}` of 8-byte `{height, base}` float cells (MPFever
//!         `mpfever_native.cpp:582`; the header is what Lua reads; the owner's runs of 2026-10-25: always at +0x00, the rest zero);
//!   +0x28 the ground paint: a grid of the same shape with 1-byte cells (`w * h` bytes), and
//!   +0x50 its mask: the same header, then a `std::vector<bool>` (`{u32* begin, *end, *capacity}` of 32-bit words, and the bit count at
//!         +0x78 = `w * h`). [The lab run landscape-probe-20261025 run3, ground paint tool, 3 strokes: e.g. header -575,-500 130x120,
//!         15600 cell bytes (0xff where untouched), 1952 mask bytes (488 words), bit count 15600; the height grid zero.]
//! Any other shape is refused with its bytes in the reason, never guessed (the tree, plant and rock brushes do not use this member:
//! their commands add constructions, which this module does not carry).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::cmd_patch::Poke;
use crate::dump::{u64_at, Mem};
use crate::rebuild::Alloc;

/// The name in the `ready` event's `hooks` list when terrain edits can be shared.
pub const READY_NAME: &str = "terrain_apply";

/// The Proposal's `terrain` member in a build command's payload (`portable.rs`: `f(0x2d8, "terrain", Ty::Raw(0x80))`).
pub const TERRAIN_OFF: usize = 0x2d8;
pub const TERRAIN_LEN: usize = 0x80;
/// `{i32 x0, y0, w, h; begin, end, capacity}`.
pub const GRID_LEN: usize = 0x28;
/// `{f32 height, f32 base}`.
pub const CELL_BYTES: u64 = 8;
/// The ground paint grid and its mask, inside the terrain member.
pub const PAINT_OFF: usize = 0x28;
pub const MASK_OFF: usize = 0x50;

/// The sanity bound of one edit's height cells: a seen stroke is up to 44x62 (2728 cells, the owner's run of 2026-10-25); this still fits
/// an action (16 hex characters a cell, the mod's 480 KB). There is no cap below it yet (owner, 2026-10-25: set after the lab measures
/// strokes; every edit's size is logged for that). Larger edits are refused, never cut.
pub const MAX_CELLS: u64 = 28_000;
/// The same for paint cells (1 byte, 2 hex characters each, and 1 bit of mask): a seen tick is 130x120 (15600 cells).
pub const MAX_PAINT_CELLS: u64 = 200_000;
/// A grid side the engine's maps never exceed (MPFever's bound): a header beyond it is not a grid.
const MAX_SIDE: i32 = 16_384;

pub const DEFAULT_TTL_MS: u64 = 10_000;
pub const MAX_TTL_MS: u64 = 60_000;
/// Orders that wait at once (one per brush tick due in one step).
pub const MAX_ORDERS: usize = 64;
/// Tool commands noted at `Add` and not yet applied.
const MAX_TOOLS: usize = 64;

pub fn out_file_name(id: u64) -> String {
    format!("native_terrain_out_{id}.txt")
}

pub fn in_file_name(key: u64) -> String {
    format!("native_terrain_in_{key}.txt")
}

/// What Lua reads as `baseHeightMod.x0, y0, width, height`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub x0: i32,
    pub y0: i32,
    pub w: i32,
    pub h: i32,
}

impl Header {
    pub fn cells(&self) -> u64 {
        u64::from(self.w.unsigned_abs()) * u64::from(self.h.unsigned_abs())
    }
    fn at(b: &[u8], off: usize) -> Header {
        Header { x0: i32_at(b, off), y0: i32_at(b, off + 4), w: i32_at(b, off + 8), h: i32_at(b, off + 12) }
    }
    fn bytes(&self) -> [u8; 16] {
        let mut o = [0u8; 16];
        for (i, v) in [self.x0, self.y0, self.w, self.h].iter().enumerate() {
            o[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
        }
        o
    }
    fn sane(&self) -> bool {
        (1..=MAX_SIDE).contains(&self.w) && (1..=MAX_SIDE).contains(&self.h)
    }
}

/// A height grid found in a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub head: Header,
    /// The cells' block.
    pub begin: usize,
}

impl Grid {
    pub fn bytes(&self) -> usize {
        (self.head.cells() * CELL_BYTES) as usize
    }
}

/// A ground paint edit: the 1-byte cells, the mask's 32-bit words and its bit count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paint {
    pub head: Header,
    pub cells: Vec<u8>,
    pub words: Vec<u8>,
    pub bits: u64,
}

/// What one terrain command edits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Edit {
    pub height: Option<(Header, Vec<u8>)>,
    pub paint: Option<Paint>,
}

impl Edit {
    /// The header the carrier's height grid has (`w = h = 0`: no height grid, the carrier is an empty Proposal).
    pub fn carrier_head(&self) -> Header {
        self.height.as_ref().map(|(h, _)| *h).unwrap_or_default()
    }
}

fn i32_at(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(b[off..off + 4].try_into().expect("4 bytes"))
}

/// The height grid of the build command at `payload` (at +0x00 of its terrain member): `None` when there is none (or an empty one).
pub fn find_grid(mem: &dyn Mem, payload: usize) -> Result<Option<Grid>, String> {
    let block = mem.read(payload + TERRAIN_OFF, TERRAIN_LEN).ok_or("the command's terrain member cannot be read")?;
    height_of(&block)
}

fn height_of(block: &[u8]) -> Result<Option<Grid>, String> {
    if block[..GRID_LEN].iter().all(|&b| b == 0) {
        return Ok(None);
    }
    let head = Header::at(block, 0);
    let (b, e, c) = (u64_at(block, 0x10), u64_at(block, 0x18), u64_at(block, 0x20));
    if head.w == 0 && head.h == 0 && b == e {
        return Ok(None); // an empty grid (w = h = 0, no cells): no height edit
    }
    if !head.sane() || b == 0 || e < b || c < e || (b | e) & 3 != 0 || e - b != head.cells() * CELL_BYTES {
        return Err(format!("the height grid is not in the shape seen ({})", words_text(&block[..GRID_LEN], 0)));
    }
    Ok(Some(Grid { head, begin: b as usize }))
}

fn words_text(b: &[u8], base: usize) -> String {
    (0..b.len()).step_by(8).map(|o| format!("+{:#x}={:016x}", base + o, u64_at(b, o))).collect::<Vec<_>>().join(" ")
}

/// The paint part of the terrain member: `None` when all of +0x28..+0x80 is zero.
fn paint_of(mem: &dyn Mem, block: &[u8]) -> Result<Option<Paint>, String> {
    let part = &block[PAINT_OFF..TERRAIN_LEN];
    if part.iter().all(|&b| b == 0) {
        return Ok(None);
    }
    let bad = || format!("the ground paint is not in the shape seen ({})", words_text(part, PAINT_OFF));
    let head = Header::at(block, PAINT_OFF);
    let (cb, ce, cc) =
        (u64_at(block, PAINT_OFF + 0x10), u64_at(block, PAINT_OFF + 0x18), u64_at(block, PAINT_OFF + 0x20));
    let (wb, we, wc) = (u64_at(block, MASK_OFF + 0x10), u64_at(block, MASK_OFF + 0x18), u64_at(block, MASK_OFF + 0x20));
    let bits = u64_at(block, MASK_OFF + 0x28);
    let n = head.cells();
    if !head.sane()
        || Header::at(block, MASK_OFF) != head
        || cb == 0
        || ce < cb
        || cc < ce
        || ce - cb != n
        || bits != n
        || wb == 0
        || we < wb
        || wc < we
        || we - wb != n.div_ceil(32) * 4
    {
        return Err(bad());
    }
    if n > MAX_PAINT_CELLS {
        return Err(format!(
            "a ground paint edit of {}x{} cells is over the bound of {MAX_PAINT_CELLS} cells: refused",
            head.w, head.h
        ));
    }
    let cells = mem.read(cb as usize, n as usize).ok_or("the paint cells cannot be read")?;
    let words = mem.read(wb as usize, (we - wb) as usize).ok_or("the paint mask cannot be read")?;
    Ok(Some(Paint { head, cells, words, bits }))
}

/// FNV-1a 64 (the text's own check).
pub fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn unhex(s: &str, len: u64, what: &str) -> Result<Vec<u8>, String> {
    if s.len() as u64 != len * 2 {
        return Err(format!("{}: {} hex digits for {len} bytes", what, s.len()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| format!("{what}: not hex at {i}")))
        .collect()
}

/// The text of an edit: `feverlink3-terrain 2`, then for a height edit `height x0 y0 w h <fnv>` and its cells as hex, and for a paint
/// edit `paint x0 y0 w h <bits> <fnv of cells and words>`, its cells as hex and its mask words as hex; one line each.
pub fn encode(edit: &Edit) -> String {
    let mut s = String::from("feverlink3-terrain 2\n");
    if let Some((h, c)) = &edit.height {
        s.push_str(&format!("height {} {} {} {} {:016x}\n{}\n", h.x0, h.y0, h.w, h.h, fnv(c), hex(c)));
    }
    if let Some(p) = &edit.paint {
        let sum = fnv(&[p.cells.as_slice(), p.words.as_slice()].concat());
        s.push_str(&format!(
            "paint {} {} {} {} {} {sum:016x}\n{}\n{}\n",
            p.head.x0,
            p.head.y0,
            p.head.w,
            p.head.h,
            p.bits,
            hex(&p.cells),
            hex(&p.words)
        ));
    }
    s
}

/// The inverse of [`encode`]; every check (the headers, the size bounds, the lengths, the hex, the checksums).
pub fn decode(text: &str) -> Result<Edit, String> {
    let mut lines = text.lines();
    if lines.next() != Some("feverlink3-terrain 2") {
        return Err("not a terrain text (version 2)".into());
    }
    let mut edit = Edit::default();
    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(' ').collect();
        let n = |i: usize| {
            f.get(i).and_then(|v| v.parse::<i32>().ok()).ok_or_else(|| format!("{line:.40}: field {i} is not a number"))
        };
        match f[0] {
            "height" if f.len() == 6 && edit.height.is_none() => {
                let head = Header { x0: n(1)?, y0: n(2)?, w: n(3)?, h: n(4)? };
                if !head.sane() || head.cells() > MAX_CELLS {
                    return Err(format!(
                        "a height grid of {}x{} is outside the bound of {MAX_CELLS} cells",
                        head.w, head.h
                    ));
                }
                let cells = unhex(lines.next().unwrap_or(""), head.cells() * CELL_BYTES, "height cells")?;
                if format!("{:016x}", fnv(&cells)) != f[5] {
                    return Err("the checksum does not match the height cells".into());
                }
                edit.height = Some((head, cells));
            }
            "paint" if f.len() == 7 && edit.paint.is_none() => {
                let head = Header { x0: n(1)?, y0: n(2)?, w: n(3)?, h: n(4)? };
                let bits = f[5].parse::<u64>().map_err(|_| "the paint bit count is not a number".to_string())?;
                if !head.sane() || head.cells() > MAX_PAINT_CELLS || bits != head.cells() {
                    return Err(format!("a paint grid of {}x{} ({bits} bits) is outside the bound", head.w, head.h));
                }
                let cells = unhex(lines.next().unwrap_or(""), head.cells(), "paint cells")?;
                let words = unhex(lines.next().unwrap_or(""), bits.div_ceil(32) * 4, "paint mask")?;
                if format!("{:016x}", fnv(&[cells.as_slice(), words.as_slice()].concat())) != f[6] {
                    return Err("the checksum does not match the paint".into());
                }
                edit.paint = Some(Paint { head, cells, words, bits });
            }
            _ => return Err(format!("not a terrain line: {line:.40}")),
        }
    }
    if edit.height.is_none() && edit.paint.is_none() {
        return Err("a terrain text with no edit".into());
    }
    Ok(edit)
}

/// The originator: what the command's terrain member edits (heights at +0x00, ground paint at +0x28); `None` when it edits nothing.
/// Only these two parts travel and the carrier on every game holds nothing else, so the member must be exactly them: a shape other than
/// the one seen is refused with its bytes, never guessed. Roughen is carried like the other height tools: its cells are copied as they
/// are, so any randomness of the tool ran on the originator only.
pub fn capture(mem: &dyn Mem, payload: usize) -> Result<Option<Edit>, String> {
    let block = mem.read(payload + TERRAIN_OFF, TERRAIN_LEN).ok_or("the command's terrain member cannot be read")?;
    let mut edit = Edit::default();
    if let Some(grid) = height_of(&block)? {
        if grid.head.cells() > MAX_CELLS {
            return Err(format!(
                "an edit of {}x{} cells is over the bound of {MAX_CELLS} cells: refused",
                grid.head.w, grid.head.h
            ));
        }
        let cells = mem.read(grid.begin, grid.bytes()).ok_or("the edit's cells cannot be read")?;
        edit.height = Some((grid.head, cells));
    }
    edit.paint = paint_of(mem, &block)?;
    Ok((edit.height.is_some() || edit.paint.is_some()).then_some(edit))
}

/// Every game: the carrier's height grid gets `cells`, and the block is read back and must equal them. `head` is the order's header.
pub fn write_cells(mem: &dyn Mem, poke: &dyn Poke, payload: usize, head: Header, cells: &[u8]) -> Result<(), String> {
    let grid = find_grid(mem, payload)?.ok_or("the carrier holds no height grid")?;
    if grid.head != head {
        return Err(format!("the carrier's grid is {:?}, the order's {head:?}", grid.head));
    }
    if grid.bytes() != cells.len() {
        return Err(format!("the carrier's grid has {} bytes, the edit {}", grid.bytes(), cells.len()));
    }
    if !poke.poke(grid.begin, cells) {
        return Err("the carrier's cells could not be written".into());
    }
    match mem.read(grid.begin, cells.len()) {
        Some(back) if back == cells => Ok(()),
        Some(_) => Err("the carrier's cells read back differ from the edit".into()),
        None => Err("the carrier's cells cannot be read back".into()),
    }
}

/// Every game: the carrier's (empty) paint part gets the paint grid and its mask, in blocks of the game's allocator `alloc` (the engine's
/// Proposal destructor frees them like its own), and everything is read back. On a refusal after an allocation the blocks are given back.
pub fn write_paint(
    mem: &dyn Mem,
    poke: &dyn Poke,
    alloc: &dyn Alloc,
    payload: usize,
    paint: &Paint,
) -> Result<(), String> {
    let at = payload + TERRAIN_OFF + PAINT_OFF;
    let now = mem.read(at, TERRAIN_LEN - PAINT_OFF).ok_or("the carrier's terrain member cannot be read")?;
    if now.iter().any(|&b| b != 0) {
        return Err("the carrier already holds ground paint".into());
    }
    let written = (|| {
        let cb = alloc.alloc(paint.cells.len()).ok_or("no block for the paint cells")?;
        let wb = alloc.alloc(paint.words.len()).ok_or("no block for the paint mask")?;
        if !alloc.write(cb, &paint.cells) || !alloc.write(wb, &paint.words) {
            return Err("the paint blocks could not be filled".to_string());
        }
        let mut part = vec![0u8; TERRAIN_LEN - PAINT_OFF];
        part[0..16].copy_from_slice(&paint.head.bytes());
        for (i, v) in [cb, cb + paint.cells.len(), cb + paint.cells.len()].iter().enumerate() {
            part[0x10 + 8 * i..0x18 + 8 * i].copy_from_slice(&(*v as u64).to_le_bytes());
        }
        let m = MASK_OFF - PAINT_OFF;
        part[m..m + 16].copy_from_slice(&paint.head.bytes());
        for (i, v) in [wb, wb + paint.words.len(), wb + paint.words.len()].iter().enumerate() {
            part[m + 0x10 + 8 * i..m + 0x18 + 8 * i].copy_from_slice(&(*v as u64).to_le_bytes());
        }
        part[m + 0x28..m + 0x30].copy_from_slice(&paint.bits.to_le_bytes());
        if !poke.poke(at, &part) {
            return Err("the carrier's paint part could not be written".to_string());
        }
        Ok(part)
    })();
    let part = match written {
        Ok(p) => p,
        Err(e) => {
            alloc.free_all();
            return Err(e);
        }
    };
    // from here the carrier owns the blocks (its destructor frees them), whatever the read-back says
    alloc.forget_all();
    let block =
        mem.read(payload + TERRAIN_OFF, TERRAIN_LEN).ok_or("the carrier's terrain member cannot be read back")?;
    if block[PAINT_OFF..] != part[..] || paint_of(mem, &block)?.as_ref() != Some(paint) {
        return Err("the carrier's paint read back differs from the edit".into());
    }
    Ok(())
}

/// A diagnostic of a tool's command whose terrain member is not all zero (the tree, plant and rock brushes, the eraser and ground paint
/// put their edits somewhere the Lua view does not show: in the owner's run 2026-10-25 the tree brush's command had every vector of the
/// proposal empty). The 0x80 bytes as words, and for every word that points at readable memory the first bytes there; a triple of words
/// shaped like a vector (begin <= end <= capacity) shows its length and up to `max` bytes of its elements. Read-only; `None` when the
/// member is all zero (every street build).
pub fn probe_text(mem: &dyn Mem, payload: usize, max: usize) -> Option<String> {
    let block = mem.read(payload + TERRAIN_OFF, TERRAIN_LEN)?;
    if block.iter().all(|&b| b == 0) {
        return None;
    }
    let hexs = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let mut out = String::new();
    let words: Vec<u64> = (0..TERRAIN_LEN).step_by(8).map(|o| u64_at(&block, o)).collect();
    out.push_str("words:");
    for (i, w) in words.iter().enumerate() {
        out.push_str(&format!(" +{:#04x}={w:016x}", i * 8));
    }
    for (i, &w) in words.iter().enumerate() {
        if !crate::dump::pointer_like(w) {
            continue;
        }
        let next = words.get(i + 1).copied().unwrap_or(0);
        let cap = words.get(i + 2).copied().unwrap_or(0);
        if next >= w && cap >= next && crate::dump::pointer_like(next) && next - w <= 64 << 20 {
            let n = ((next - w) as usize).min(max);
            let bytes = if n > 0 { mem.read(w as usize, n) } else { Some(Vec::new()) };
            out.push_str(&format!(
                "\n  vector at +{:#04x}: {} bytes{}: {}",
                i * 8,
                next - w,
                if n < (next - w) as usize { " (head)" } else { "" },
                bytes.map(|b| hexs(&b)).unwrap_or_else(|| "unreadable".into())
            ));
        } else if let Some(b) = mem.read(w as usize, 0x80) {
            out.push_str(&format!("\n  points at +{:#04x}: {}", i * 8, hexs(&b)));
        }
    }
    Some(out)
}

/// One `terrain_apply` order: the edit (or why its text was refused) for the next carrier whose height grid has `head` (`w = h = 0`:
/// a paint-only edit, whose carrier is an empty Proposal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub key: u64,
    pub head: Header,
    pub edit: Result<Edit, String>,
    pub armed_ms: u64,
    pub ttl_ms: u64,
}

/// The module's terrain state: shared by the command list's `Add` (interface thread), the simulation's apply and the service thread.
#[derive(Default)]
pub struct State {
    share: AtomicBool,
    pending: AtomicBool,
    /// Diagnostics written ([`probe_text`]), bounded per game run.
    pub probes: AtomicU64,
    next_out: AtomicU64,
    tools: Mutex<VecDeque<usize>>,
    orders: Mutex<VecDeque<Order>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// What the apply hook does with one `build_proposal` command.
#[derive(Debug, PartialEq, Eq)]
pub enum Apply {
    /// Not a terrain edit this module deals with: applied as it is.
    Pass,
    /// A terrain tool's own command: capture, empty, ship.
    Tool,
    /// A carrier for this order.
    Carrier(Order),
    /// A player-initiated script edit with no order while sharing is on: emptied (it would apply on this game alone).
    Orphan,
}

/// What the apply hook read of a `build_proposal` command for [`State::classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Seen {
    /// Its height grid's header, if it has one.
    pub height: Option<Header>,
    /// Its terrain member holds ground paint (or anything at all beside a height grid).
    pub paint: bool,
    /// Its Proposal is empty: no vector of it holds anything and its terrain member is zero (a paint-only edit's carrier).
    pub empty: bool,
    /// Its playerInitiated flag.
    pub player: bool,
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

    /// The command list's `Add` of a command with a terrain edit (only scripts' commands of the interface half and the tools pass `Add`).
    pub fn note_tool(&self, payload: usize) {
        let mut t = lock(&self.tools);
        t.retain(|&p| p != payload);
        t.push_back(payload);
        while t.len() > MAX_TOOLS {
            t.pop_front();
        }
    }

    /// Arms an order; the oldest is refused (returned) when too many wait.
    pub fn arm(&self, order: Order) -> Option<Order> {
        let mut q = lock(&self.orders);
        q.push_back(order);
        let dropped = (q.len() > MAX_ORDERS).then(|| q.pop_front()).flatten();
        self.pending.store(true, Ordering::Release);
        dropped
    }

    /// The orders whose time ran out.
    pub fn expire(&self, now: u64) -> Vec<Order> {
        let mut q = lock(&self.orders);
        let (keep, gone): (VecDeque<Order>, VecDeque<Order>) =
            q.drain(..).partition(|o| now.saturating_sub(o.armed_ms) <= o.ttl_ms);
        *q = keep;
        self.pending.store(!q.is_empty(), Ordering::Release);
        gone.into()
    }

    fn take_order(&self, payload: usize, head: Header) -> Option<Order> {
        let mut q = lock(&self.orders);
        let i = q.iter().position(|o| o.head == head)?;
        let o = q.remove(i).expect("an index of the queue");
        self.pending.store(!q.is_empty(), Ordering::Release);
        lock(&self.tools).retain(|&p| p != payload);
        Some(o)
    }

    /// Decides for a `build_proposal` the simulation is about to apply.
    pub fn classify(&self, payload: usize, seen: Seen) -> Apply {
        if seen.height.is_none() && !seen.paint {
            lock(&self.tools).retain(|&p| p != payload);
            // a paint-only edit's carrier: the mod's own empty player build, sent right after its order
            if seen.empty && seen.player {
                if let Some(o) = self.take_order(payload, Header::default()) {
                    return Apply::Carrier(o);
                }
            }
            return Apply::Pass;
        }
        if let (Some(head), false) = (seen.height, seen.paint) {
            if let Some(o) = self.take_order(payload, head) {
                return Apply::Carrier(o);
            }
        }
        let tool = {
            let mut t = lock(&self.tools);
            let at = t.iter().position(|&p| p == payload);
            at.map(|i| t.remove(i)).is_some()
        };
        if !self.share() {
            return Apply::Pass;
        }
        if tool {
            Apply::Tool
        } else if seen.player {
            Apply::Orphan
        } else {
            Apply::Pass // the game's own scripts (playerInitiated false) edit on every game alike
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rebuild::Arena;

    struct ArenaPoke<'a>(&'a Arena);
    impl Poke for ArenaPoke<'_> {
        fn poke(&self, addr: usize, data: &[u8]) -> bool {
            self.0.write(addr, data)
        }
    }

    fn put_grid(arena: &Arena, p: &mut [u8], at: usize, head: Header, cells: &[u8]) {
        let block = arena.add(cells);
        let g = TERRAIN_OFF + at;
        p[g..g + 16].copy_from_slice(&head.bytes());
        for (i, v) in [block, block + cells.len(), block + cells.len()].iter().enumerate() {
            p[g + 0x10 + 8 * i..g + 0x18 + 8 * i].copy_from_slice(&(*v as u64).to_le_bytes());
        }
    }

    /// A payload with a height grid of `cells` (none when empty) and, with `paint`, a paint grid and mask.
    fn payload(arena: &Arena, head: Header, cells: &[u8], paint: Option<&Paint>) -> usize {
        let mut p = vec![0u8; crate::dump::PAYLOAD_SIZE];
        if !cells.is_empty() {
            put_grid(arena, &mut p, 0, head, cells);
        }
        if let Some(pt) = paint {
            put_grid(arena, &mut p, PAINT_OFF, pt.head, &pt.cells);
            put_grid(arena, &mut p, MASK_OFF, pt.head, &pt.words);
            p[TERRAIN_OFF + MASK_OFF + 0x28..TERRAIN_OFF + MASK_OFF + 0x30].copy_from_slice(&pt.bits.to_le_bytes());
        }
        arena.add(&p)
    }

    #[test]
    fn height_and_paint_edits_travel_as_text_and_are_written_into_their_carriers() {
        let arena = Arena::new(0x1000_0000);
        let head = Header { x0: -311, y0: 137, w: 3, h: 2 };
        let cells: Vec<u8> = (0..48u8).collect();
        let tool = payload(&arena, head, &cells, None);
        let edit = capture(&arena, tool).unwrap().unwrap();
        assert_eq!(edit.height, Some((head, cells.clone())));
        let text = encode(&edit);
        assert_eq!(decode(&text).unwrap(), edit);
        assert!(decode(&text.replacen("00010203", "00010204", 1)).unwrap_err().contains("checksum"));
        // the carrier: the script's grid of the same size (a constant fill), its cells become the edit's and are read back
        let carrier = payload(&arena, head, &[0u8; 48], None);
        write_cells(&arena, &ArenaPoke(&arena), carrier, head, &cells).unwrap();
        assert_eq!(capture(&arena, carrier).unwrap().unwrap().height.unwrap().1, cells);
        let other = payload(&arena, Header { w: 2, h: 3, ..head }, &[0u8; 48], None);
        assert!(write_cells(&arena, &ArenaPoke(&arena), other, head, &cells).unwrap_err().contains("order's"));
        // ground paint (the shape of the lab run: 0xff where untouched, a mask of 32-bit words, the bit count)
        let ph = Header { x0: -575, y0: -500, w: 5, h: 7 };
        let paint = Paint {
            head: ph,
            cells: (0..35u8).map(|i| if i % 3 == 0 { 2 } else { 0xff }).collect(),
            words: vec![0x49, 0x92, 0x24, 0x09, 0, 0, 0, 0],
            bits: 35,
        };
        let painted = payload(&arena, Header::default(), &[], Some(&paint));
        let edit = capture(&arena, painted).unwrap().unwrap();
        assert_eq!((edit.height.as_ref(), edit.paint.as_ref()), (None, Some(&paint)));
        assert_eq!(edit.carrier_head(), Header::default(), "a paint-only edit's carrier has no height grid");
        assert_eq!(decode(&encode(&edit)).unwrap(), edit);
        let empty = arena.add(&vec![0u8; crate::dump::PAYLOAD_SIZE]);
        write_paint(&arena, &ArenaPoke(&arena), &arena, empty, &paint).unwrap();
        assert_eq!(capture(&arena, empty).unwrap().unwrap().paint, Some(paint.clone()));
        assert!(write_paint(&arena, &ArenaPoke(&arena), &arena, empty, &paint).unwrap_err().contains("already holds"));
        // a shape not seen (the mask's bit count off), a seed beside the grid, a too large edit: refused with the bytes
        let mut odd = arena.read(painted, crate::dump::PAYLOAD_SIZE).unwrap();
        odd[TERRAIN_OFF + MASK_OFF + 0x28] = 36;
        assert!(capture(&arena, arena.add(&odd)).unwrap_err().contains("not in the shape seen"));
        let big = Header { x0: 0, y0: 0, w: 200, h: 200 };
        let huge = payload(&arena, big, &vec![0u8; (big.cells() * CELL_BYTES) as usize], None);
        assert!(capture(&arena, huge).unwrap_err().contains("over the bound"));
    }

    #[test]
    fn the_apply_tells_tools_carriers_and_orphans_apart() {
        let s = State::default();
        let head = Header { x0: 1, y0: 2, w: 3, h: 3 };
        let height = Seen { height: Some(head), player: true, ..Seen::default() };
        assert_eq!(s.classify(0x10, height), Apply::Pass, "sharing off: as today");
        s.set_share(true);
        s.note_tool(0x10);
        assert_eq!(s.classify(0x10, height), Apply::Tool);
        assert_eq!(s.classify(0x20, height), Apply::Orphan);
        assert_eq!(s.classify(0x20, Seen { player: false, ..height }), Apply::Pass);
        s.arm(Order { key: 7, head, edit: Err("x".into()), armed_ms: 0, ttl_ms: 100 });
        assert!(matches!(s.classify(0x30, height), Apply::Carrier(o) if o.key == 7));
        assert!(!s.pending());
        // a paint-only edit: its carrier is the next empty player build
        s.arm(Order { key: 8, head: Header::default(), edit: Err("x".into()), armed_ms: 0, ttl_ms: 100 });
        assert_eq!(
            s.classify(0x40, Seen { player: true, ..Seen::default() }),
            Apply::Pass,
            "not empty: a build of another kind"
        );
        assert!(
            matches!(s.classify(0x40, Seen { player: true, empty: true, ..Seen::default() }), Apply::Carrier(o) if o.key == 8)
        );
        s.note_tool(0x50);
        assert_eq!(s.classify(0x50, Seen { paint: true, player: true, ..Seen::default() }), Apply::Tool);
        s.arm(Order { key: 9, head, edit: Err("x".into()), armed_ms: 0, ttl_ms: 100 });
        assert!(s.expire(50).is_empty());
        assert_eq!(s.expire(101).len(), 1);
        assert!(!s.pending());
    }
}
