//! Terrain height edits in a session (owner decisions of 2026-10-25, step 2 of the terrain design: raise, lower, flatten and smooth;
//! not Roughen, ground paint or trees). Lockstep like every build: no game applies an edit at the click, every game (the originator's
//! included) applies the same cells at the same stamp.
//!
//! 1. The originator: a terrain tool's command reaches the command list's `Add` (a script's own send from the simulation state never
//!    does) and is noted ([`State::note_tool`]). When the simulation applies it, its height grid is copied by value
//!    ([`capture`]) and the command's Proposal is emptied (the engine's own empty Proposal assigned into it, `inject::neutralise`), so the
//!    click applies nothing; it is not held (holding the terrain edit crashed the tool in `UI::TerrainModifier`: MPFever (MIT),
//!    `native/mpfever_native.cpp:309`) and the tool gets its normal callback. The cells go to `native_terrain_out_<id>.txt` and the
//!    `terrain` event; the mod ships them like any action.
//! 2. Every game at the stamp: the mod writes the cells to `native_terrain_in_<key>.txt`, then `terrain_apply {key, x0, y0, w, h}`, then
//!    sends the documented carrier `makeWorldBuildProposalCmd(Proposal{terrain.baseHeightMod = GridVec2f.new(x0, y0, w, h)}, ...)`. A
//!    script can make a grid of any size but cannot write its cells (`api/tealdef`: `GridVec2f` has `at`, no setter). The carrier's grid
//!    already holds `w * h` cells (the engine allocated them), so the cells are copied into that block in place, nothing is allocated,
//!    and the block is read back and compared with the text ([`write_cells`]). Anything else empties the carrier: nothing applied here
//!    (fail closed), answered `inject_refused`; success is `injected` (the same answers as `inject_next`, so the mod's verdict logic
//!    is the same).
//!
//! The layout, all proven before this module: the Proposal's `terrain` member is the 0x80 bytes at payload +0x2d8 (the binding's
//! offsets, `portable.rs`); its only Lua-visible member `baseHeightMod` is a grid `{i32 x0, y0, w, h; cell* begin, *end, *capacity}`
//! of 8-byte `{height, base}` float cells (MPFever `mpfever_native.cpp:582` and its `FindGrids`; the header is what Lua reads as
//! `x0, y0, width, height`, owner airport run 2026-10-15 `raw_builds.txt`: 15x15). Where in the 0x80 bytes the grid starts is not
//! assumed: the one place whose words form such a grid is taken, and a block with more than one is refused (the other ~0x58 bytes are
//! unknown, possibly trees or paint: not carried, never guessed).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::cmd_patch::Poke;
use crate::dump::{u64_at, Mem};

/// The name in the `ready` event's `hooks` list when terrain edits can be shared.
pub const READY_NAME: &str = "terrain_apply";

/// The Proposal's `terrain` member in a build command's payload (`portable.rs`: `f(0x2d8, "terrain", Ty::Raw(0x80))`).
pub const TERRAIN_OFF: usize = 0x2d8;
pub const TERRAIN_LEN: usize = 0x80;
/// `{i32 x0, y0, w, h; begin, end, capacity}`.
pub const GRID_LEN: usize = 0x28;
/// `{f32 height, f32 base}`.
pub const CELL_BYTES: u64 = 8;

/// The sanity bound of one edit (one brush tick): a seen tick is 15x15 (225 cells); this is over a hundred times that and still fits an
/// action (16 hex characters a cell, the mod's 480 KB). There is no cap below it yet (owner, 2026-10-25: set after a lab run has
/// measured real strokes; every edit's size is logged for that). Larger edits are refused, never cut.
pub const MAX_CELLS: u64 = 28_000;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

fn i32_at(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(b[off..off + 4].try_into().expect("4 bytes"))
}

/// The height grid of the build command at `payload`: `None` when its terrain member holds none (or an empty one), an error when the
/// member cannot be read or holds more than one thing shaped like a grid.
pub fn find_grid(mem: &dyn Mem, payload: usize) -> Result<Option<Grid>, String> {
    let block = mem.read(payload + TERRAIN_OFF, TERRAIN_LEN).ok_or("the command's terrain member cannot be read")?;
    let mut found = None;
    for off in (0..=TERRAIN_LEN - GRID_LEN).step_by(8) {
        let head = Header {
            x0: i32_at(&block, off),
            y0: i32_at(&block, off + 4),
            w: i32_at(&block, off + 8),
            h: i32_at(&block, off + 12),
        };
        if !(1..=MAX_SIDE).contains(&head.w) || !(1..=MAX_SIDE).contains(&head.h) {
            continue;
        }
        let (b, e, c) = (u64_at(&block, off + 0x10), u64_at(&block, off + 0x18), u64_at(&block, off + 0x20));
        if b == 0 || e <= b || c < e || (b | e) & 3 != 0 || e - b != head.cells() * CELL_BYTES {
            continue;
        }
        if found.is_some() {
            return Err(
                "the terrain member holds more than one grid: an edit of a kind this module does not carry".into()
            );
        }
        found = Some(Grid { head, begin: b as usize });
    }
    Ok(found)
}

/// FNV-1a 64 of the cells (the text's own check).
pub fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// The text of an edit: `feverlink3-terrain 1 <x0> <y0> <w> <h> <fnv hex>`, a newline, the cells as lowercase hex, a newline.
pub fn encode(head: Header, cells: &[u8]) -> String {
    let mut s = format!("feverlink3-terrain 1 {} {} {} {} {:016x}\n", head.x0, head.y0, head.w, head.h, fnv(cells));
    s.reserve(cells.len() * 2 + 1);
    for b in cells {
        s.push_str(&format!("{b:02x}"));
    }
    s.push('\n');
    s
}

/// The inverse of [`encode`]; every check (the header, the size bound, the length, the hex, the checksum).
pub fn decode(text: &str) -> Result<(Header, Vec<u8>), String> {
    let (first, rest) = text.split_once('\n').ok_or("no header line")?;
    let f: Vec<&str> = first.split(' ').collect();
    if f.len() != 7 || f[0] != "feverlink3-terrain" || f[1] != "1" {
        return Err(format!("not a terrain text: {:?}", first.chars().take(60).collect::<String>()));
    }
    let n = |i: usize| f[i].parse::<i32>().map_err(|_| format!("header field {i} is not a number"));
    let head = Header { x0: n(2)?, y0: n(3)?, w: n(4)?, h: n(5)? };
    if head.w < 1 || head.h < 1 {
        return Err("an empty grid".into());
    }
    if head.cells() > MAX_CELLS {
        return Err(format!("{}x{} cells is over the bound of {MAX_CELLS}", head.w, head.h));
    }
    let want = u64::from_str_radix(f[6], 16).map_err(|_| "the checksum is not hex".to_string())?;
    let hex = rest.trim_end_matches(['\n', '\r']);
    if hex.len() as u64 != head.cells() * CELL_BYTES * 2 {
        return Err(format!("{} hex digits for {} cells", hex.len(), head.cells()));
    }
    let cells = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| format!("not hex at {i}")))
        .collect::<Result<Vec<u8>, String>>()?;
    if fnv(&cells) != want {
        return Err("the checksum does not match the cells".into());
    }
    Ok((head, cells))
}

/// The originator: the grid and a copy of its cells; `None` when the command edits no heights.
pub fn capture(mem: &dyn Mem, payload: usize) -> Result<Option<(Header, Vec<u8>)>, String> {
    let Some(grid) = find_grid(mem, payload)? else { return Ok(None) };
    if grid.head.cells() > MAX_CELLS {
        return Err(format!(
            "an edit of {}x{} cells is over the bound of {MAX_CELLS} cells: refused",
            grid.head.w, grid.head.h
        ));
    }
    let cells = mem.read(grid.begin, grid.bytes()).ok_or("the edit's cells cannot be read")?;
    Ok(Some((grid.head, cells)))
}

/// Every game: the carrier's grid gets `cells`, and the block is read back and must equal them. `head` is the order's header.
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

/// One `terrain_apply` order: the cells (or why the text was refused) for the next carrier with this header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub key: u64,
    pub head: Header,
    pub cells: Result<Vec<u8>, String>,
    pub armed_ms: u64,
    pub ttl_ms: u64,
}

/// The module's terrain state: shared by the command list's `Add` (interface thread), the simulation's apply and the service thread.
#[derive(Default)]
pub struct State {
    share: AtomicBool,
    pending: AtomicBool,
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

    /// The command list's `Add` of a command with a height grid (only scripts' commands of the interface half and the tools pass `Add`).
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

    /// Decides for a `build_proposal` the simulation is about to apply, whose grid is `grid` (`player`: its playerInitiated flag).
    pub fn classify(&self, payload: usize, grid: Option<Header>, player: bool) -> Apply {
        let Some(head) = grid else {
            lock(&self.tools).retain(|&p| p != payload);
            return Apply::Pass;
        };
        {
            let mut q = lock(&self.orders);
            if let Some(i) = q.iter().position(|o| o.head == head) {
                let o = q.remove(i).expect("an index of the queue");
                self.pending.store(!q.is_empty(), Ordering::Release);
                lock(&self.tools).retain(|&p| p != payload);
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
        } else if player {
            Apply::Orphan
        } else {
            Apply::Pass // the game's own scripts (playerInitiated false) edit on every game alike
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rebuild::{Alloc, Arena};

    struct ArenaPoke<'a>(&'a Arena);
    impl Poke for ArenaPoke<'_> {
        fn poke(&self, addr: usize, data: &[u8]) -> bool {
            self.0.write(addr, data)
        }
    }

    /// A payload whose terrain member holds a grid at `at` (into the member) with `cells`.
    fn payload(arena: &Arena, head: Header, at: usize, cells: &[u8]) -> usize {
        let block = arena.add(cells);
        let mut p = vec![0u8; crate::dump::PAYLOAD_SIZE];
        let g = TERRAIN_OFF + at;
        for (i, v) in [head.x0, head.y0, head.w, head.h].iter().enumerate() {
            p[g + 4 * i..g + 4 * i + 4].copy_from_slice(&v.to_le_bytes());
        }
        for (i, v) in [block, block + cells.len(), block + cells.len()].iter().enumerate() {
            p[g + 0x10 + 8 * i..g + 0x18 + 8 * i].copy_from_slice(&(*v as u64).to_le_bytes());
        }
        arena.add(&p)
    }

    #[test]
    fn an_edit_is_captured_shipped_as_text_and_written_into_a_carrier_of_the_same_header_only() {
        let arena = Arena::new(0x1000_0000);
        let head = Header { x0: -311, y0: 137, w: 3, h: 2 };
        let cells: Vec<u8> = (0..48u8).collect();
        let tool = payload(&arena, head, 0x08, &cells);
        let (h, got) = capture(&arena, tool).unwrap().unwrap();
        assert_eq!((h, &got), (head, &cells));
        let text = encode(h, &got);
        assert_eq!(decode(&text).unwrap(), (head, cells.clone()));
        assert!(decode(&text.replace(" 3 2 ", " 3 3 ")).unwrap_err().contains("hex digits"));
        let bad = text.replacen("00010203", "00010204", 1);
        assert!(decode(&bad).unwrap_err().contains("checksum"));
        // the carrier: the script's grid of the same size (a constant fill), its cells become the edit's and are read back
        let carrier = payload(&arena, head, 0x08, &[0u8; 48]);
        write_cells(&arena, &ArenaPoke(&arena), carrier, head, &cells).unwrap();
        assert_eq!(capture(&arena, carrier).unwrap().unwrap().1, cells);
        let other = payload(&arena, Header { w: 2, h: 3, ..head }, 0x08, &[0u8; 48]);
        assert!(write_cells(&arena, &ArenaPoke(&arena), other, head, &cells).unwrap_err().contains("order's"));
        // no grid, two grids, too large
        let empty = arena.add(&vec![0u8; crate::dump::PAYLOAD_SIZE]);
        assert_eq!(find_grid(&arena, empty).unwrap(), None);
        let two = payload(&arena, head, 0x00, &cells);
        let second = payload(&arena, head, 0x00, &cells);
        let mut bytes = arena.read(two, crate::dump::PAYLOAD_SIZE).unwrap();
        let tail = arena.read(second + TERRAIN_OFF, GRID_LEN).unwrap();
        bytes[TERRAIN_OFF + 0x30..TERRAIN_OFF + 0x30 + GRID_LEN].copy_from_slice(&tail);
        let two = arena.add(&bytes);
        assert!(find_grid(&arena, two).unwrap_err().contains("more than one grid"));
        let big = Header { x0: 0, y0: 0, w: 200, h: 200 };
        let huge = payload(&arena, big, 0x08, &vec![0u8; (big.cells() * CELL_BYTES) as usize]);
        assert!(capture(&arena, huge).unwrap_err().contains("over the bound"));
    }

    #[test]
    fn the_apply_tells_tools_carriers_and_orphans_apart() {
        let s = State::default();
        let head = Header { x0: 1, y0: 2, w: 3, h: 3 };
        assert_eq!(s.classify(0x10, Some(head), true), Apply::Pass, "sharing off: as today");
        s.set_share(true);
        s.note_tool(0x10);
        assert_eq!(s.classify(0x10, Some(head), true), Apply::Tool);
        assert_eq!(s.classify(0x20, Some(head), true), Apply::Orphan);
        assert_eq!(s.classify(0x20, Some(head), false), Apply::Pass);
        s.arm(Order { key: 7, head, cells: Ok(vec![]), armed_ms: 0, ttl_ms: 100 });
        assert!(matches!(s.classify(0x30, Some(head), true), Apply::Carrier(o) if o.key == 7));
        assert!(!s.pending());
        s.arm(Order { key: 8, head, cells: Ok(vec![]), armed_ms: 0, ttl_ms: 100 });
        assert!(s.expire(50).is_empty());
        assert_eq!(s.expire(101).len(), 1);
        assert!(!s.pending());
    }
}
