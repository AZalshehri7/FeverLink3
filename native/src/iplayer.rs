//! `FEVERLINK3_INTERFACE_PLAYER` (on by default, `=0` off; separate companies, `docs/COMPANIES.md`): the game's interface treats
//! this game's own company as "me", while the simulation and every savegame keep the savegame's player.
//!
//! Why (two-window labs of 2026-10-28/30, build 40425, "Company for each player"): each player's company is a PLAYER entity of its
//! own and its builds already belong to it (`acting.rs`), but the interface still took the savegame's player `P` for "me": the
//! guest's bulldozer did not remove its own company's roads (no command at all) and `api.engine.util.vehicle.findBestDepotForLine`
//! answered -1 for the guest's own line.
//!
//! What the executable says (static research of build 40425, `docs/NATIVE_LINK.md` "Interface player"; NOT yet seen in the game):
//!
//! - The one source of "the local player" is `GameState+0x20c` (an entity id). `CGame` keeps two GameStates (`[CGame+0x1f0]+0x78`
//!   and `+0x80`, the simulation's index at `+0x98`) and a pointer to the one the interface reads, the front (`CGame+0x1e0`).
//!   `CGame::Sync` (0x11f6a0, main thread, called only from `CGame::Step` at 0x11f456, the call smooth pacing already redirects) makes
//!   the state the simulation just finished the front (0x11f899); `CGame::StartGameSim` (0x11f0a0) sets the first front (0x11f29c)
//!   after `Replicate(gameStates[0] -> gameStates[1])` (0x11f0d1).
//! - `GameState::Replicate` (0x255e70; its only callers are the simulation loop 0x11eaaa and StartGameSim) copies the front into the
//!   next working state on the simulation thread, `+0x20c` included (0x255ee9). `GameState::Save` writes the field into the savegame.
//! - The interface reads the front: live (Lua `getPlayer` of the interface state, `findBestDepotForLine`'s depot filter at 0x2694795),
//!   and once in the `UI::CGameUI` constructor (0x649d98), which copies it into every build tool (street tool `+0xc0`, bulldozer
//!   `+0x28`, its allowed-players vector `+0xa8`, `+0x944`). A later change does not reach the tools: they take it at the next world load.
//!
//! So one value `L` per game (the mod's order `interface_player`; unset: nothing is ever written) is written into the front after
//! each front assignment (after `Sync` and after `StartGameSim`), and after every `Replicate` the copy's `+0x20c` is set back to `P`,
//! so the simulation's states, the game scripts that read them and every savegame keep `P` on every game. `P` is read from the
//! loaded state (`gameStates[0]`) when `StartGameSim` begins, before anything of this module can have touched it.
//!
//! Everything here is pure or atomics: the hooks (`hooks.rs`) hand in a [`Memory`] of the game; the tests hand in a map.

use crate::build_table::{BuildAddrs, HookSite, Rva};
use crate::plan::Image;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed, Ordering::SeqCst};
use std::sync::Mutex;

/// The switch: on unless exactly `0`. Unset changes nothing by itself: the module writes only once the mod names a company.
pub const INTERFACE_VAR: &str = "FEVERLINK3_INTERFACE_PLAYER";
/// What the `ready` event lists when every site is in place (the mod sends `interface_player` only then).
pub const READY_NAME: &str = "interface_player";
/// The two detours' names in the plan and in `native.log`.
pub const START_HOOK: &str = "start_game_sim";
pub const REPLICATE_HOOK: &str = "replicate";
/// The redirected call of `CGame::Sync` when smooth pacing is off (with smooth pacing on, its hook does the front write too).
pub const SYNC_HOOK: &str = "interface_sync";

/// `CGame+0x1e0`: the GameState the interface reads (the front).
pub const FRONT_OFFSET: usize = 0x1e0;
/// `CGame+0x1f0`: the buffer object; `+0x78`: `gameStates[0]`, the state a load fills and StartGameSim replicates from.
pub const DATA_OFFSET: usize = 0x1f0;
pub const STATE0_OFFSET: usize = 0x78;
/// `GameState+0x20c`: the local player.
pub const PLAYER_OFFSET: usize = 0x20c;

/// Bytes that must be in the image for the offsets above to mean what they mean on this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Witness {
    pub rva: Rva,
    pub bytes: &'static [u8],
    pub what: &'static str,
}

/// The sites of one game build, read from its executable (static analysis, `docs/research/LOCAL_PLAYER_20261030.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub timestamp: u32,
    pub name: &'static str,
    /// `CGame::Sync`: must be the callee of the build table's `batch_pace` call (the only call of it, in `CGame::Step`).
    pub sync: Rva,
    pub start: HookSite,
    pub replicate: HookSite,
    pub witnesses: &'static [Witness],
}

/// The verified builds. A build without a row has the piece off (said in `native.log`), never a guess.
pub static ROWS: &[Row] = &[Row {
    timestamp: 0x6ad9_ce10,
    name: "40425",
    sync: 0x11f6a0,
    // mov [rsp+10h],rbx ; mov [rsp+18h],rbp ; push rsi ; push rdi ; push r12 ; push r14
    start: HookSite {
        rva: 0x11f0a0,
        prologue: &[0x48, 0x89, 0x5C, 0x24, 0x10, 0x48, 0x89, 0x6C, 0x24, 0x18, 0x56, 0x57, 0x41, 0x54, 0x41, 0x56],
    },
    // mov [rsp+8],rbx ; mov [rsp+10h],rbp ; mov [rsp+18h],rsi ; push rdi
    replicate: HookSite {
        rva: 0x255e70,
        prologue: &[0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x6C, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57],
    },
    witnesses: &[
        Witness {
            rva: 0x11f88d,
            // movsxd rcx,[r8+98h] ; mov rdx,[r8+rcx*8+78h] ; mov [rbx+1E0h],rdx
            bytes: &[
                0x49, 0x63, 0x88, 0x98, 0x00, 0x00, 0x00, 0x49, 0x8B, 0x54, 0xC8, 0x78, 0x48, 0x89, 0x93, 0xE0, 0x01,
                0x00, 0x00,
            ],
            what: "Sync makes gameStates[simIdx] the front (CGame+0x1e0)",
        },
        Witness {
            rva: 0x11f0bc,
            // mov rdx,[rcx+1F0h] ; mov rcx,[rdx+78h] ; xor r8d,r8d ; mov rdx,[rdx+80h] ; call Replicate
            bytes: &[
                0x48, 0x8B, 0x91, 0xF0, 0x01, 0x00, 0x00, 0x48, 0x8B, 0x4A, 0x78, 0x45, 0x33, 0xC0, 0x48, 0x8B, 0x92,
                0x80, 0x00, 0x00, 0x00, 0xE8, 0x9A, 0x6D, 0x13, 0x00,
            ],
            what: "StartGameSim replicates gameStates[0] ([CGame+0x1f0]+0x78) into gameStates[1]",
        },
        Witness {
            rva: 0x11f28e,
            // mov rax,[rdi+1F0h] ; mov rax,[rax+80h] ; mov [rdi+1E0h],rax
            bytes: &[
                0x48, 0x8B, 0x87, 0xF0, 0x01, 0x00, 0x00, 0x48, 0x8B, 0x80, 0x80, 0x00, 0x00, 0x00, 0x48, 0x89, 0x87,
                0xE0, 0x01, 0x00, 0x00,
            ],
            what: "StartGameSim sets the first front",
        },
        Witness {
            rva: 0x255ee9,
            // mov eax,[rsi+20Ch] ; mov [rdi+20Ch],eax
            bytes: &[0x8B, 0x86, 0x0C, 0x02, 0x00, 0x00, 0x89, 0x87, 0x0C, 0x02, 0x00, 0x00],
            what: "Replicate copies GameState+0x20c from its source to its destination",
        },
        Witness {
            rva: 0x649d98,
            // mov ecx,[rax+20Ch] ; mov [rbp-50h],ecx
            bytes: &[0x8B, 0x88, 0x0C, 0x02, 0x00, 0x00, 0x89, 0x4D, 0xB0],
            what: "the CGameUI constructor takes the tools' player from GameState+0x20c",
        },
    ],
}];

/// The row of a build, by its PE timestamp.
pub fn row_for(timestamp: u32) -> Option<&'static Row> {
    ROWS.iter().find(|r| r.timestamp == timestamp)
}

/// Whether the switch is on.
pub fn switch_on(get: impl Fn(&str) -> Option<String>) -> bool {
    get(INTERFACE_VAR).is_none_or(|v| v.trim() != "0")
}

fn same(image: &dyn Image, rva: Rva, want: &[u8], what: &str) -> Result<(), String> {
    match image.read(rva, want.len()) {
        Some(found) if found == want => Ok(()),
        Some(found) => Err(format!(
            "{what}: bytes at rva {rva:#x} are {} where the row expects {}",
            crate::plan::hex(&found),
            crate::plan::hex(want)
        )),
        None => Err(format!("{what}: {} bytes at rva {rva:#x} are not readable", want.len())),
    }
}

/// Every site of the piece checked against the image: the row of this build, the redirected call of `Sync` (the build table's
/// `batch_pace`, which must lead to the row's `Sync`), both prologues and every witness. Any difference: the piece stays off.
pub fn check(build: &BuildAddrs, image: &dyn Image) -> Result<&'static Row, String> {
    let row = row_for(build.timestamp)
        .ok_or_else(|| format!("no verified row for game build {} (timestamp {:#x})", build.name, build.timestamp))?;
    let call =
        build.batch_pace.as_ref().ok_or("the build table has no call of CGame::Sync (batch_pace) for this build")?;
    if call.callee.rva != row.sync {
        return Err(format!(
            "the build table's call of Sync leads to rva {:#x}, the row's Sync is at {:#x}",
            call.callee.rva, row.sync
        ));
    }
    same(image, call.rva, call.expected, "the call of CGame::Sync in CGame::Step")?;
    same(image, call.callee.rva, call.callee.prologue, "CGame::Sync")?;
    same(image, row.start.rva, row.start.prologue, "CGame::StartGameSim")?;
    same(image, row.replicate.rva, row.replicate.prologue, "GameState::Replicate")?;
    for w in row.witnesses {
        same(image, w.rva, w.bytes, w.what)?;
    }
    Ok(row)
}

/// The game's memory as the hooks see it. Every access may be refused (`None`, `false`): nothing is written on a refusal.
pub trait Memory {
    fn word(&self, addr: usize) -> Option<usize>;
    fn int(&self, addr: usize) -> Option<i32>;
    fn set_int(&self, addr: usize, value: i32) -> bool;
}

const VALID: u64 = 1 << 32;

fn pack(v: Option<u32>) -> u64 {
    v.map_or(0, |v| VALID | u64::from(v))
}

fn unpack(v: u64) -> Option<u32> {
    (v & VALID != 0).then_some(v as u32)
}

/// Why a world start or a write was refused (for the service thread's line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Refusal {
    None = 0,
    /// StartGameSim's `this` gave no readable `gameStates[0]`.
    NoState = 1,
    /// The loaded state names no entity (negative).
    NoPlayer = 2,
    /// The front pointer or its field was not readable, or not writable.
    Front = 3,
    /// A replicated state's field was not readable or not writable.
    Copy = 4,
}

impl Refusal {
    fn from_u32(v: u32) -> Refusal {
        match v {
            1 => Refusal::NoState,
            2 => Refusal::NoPlayer,
            3 => Refusal::Front,
            4 => Refusal::Copy,
            _ => Refusal::None,
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Refusal::None => "nothing",
            Refusal::NoState => {
                "the loaded state (gameStates[0] at [CGame+0x1f0]+0x78) was not readable at StartGameSim"
            }
            Refusal::NoPlayer => "the loaded state names no player entity (GameState+0x20c below 0)",
            Refusal::Front => "the front state (CGame+0x1e0) or its +0x20c was not readable and writable",
            Refusal::Copy => "a replicated state's +0x20c was not readable and writable",
        }
    }
}

/// What the service thread said last (it alone touches this).
#[derive(Debug, Default)]
struct Told {
    world: u64,
    front: u64,
    refusals: u64,
}

/// The process-wide state: atomics on the hook side (main thread: StartGameSim and Sync; simulation thread: Replicate), a lock only on
/// the service thread's side.
#[derive(Debug)]
pub struct State {
    /// Every site is in place (`hooks.rs` after the install). Off: the hooks only call the original.
    armed: AtomicBool,
    /// `L`, the mod's order (0: none).
    wanted: AtomicU64,
    /// `P` of the world being played, read when its StartGameSim began (0: none: nothing is written in this world).
    savegame: AtomicU64,
    /// `L` was written into a front of this world: from now until the next world every replicated state gets `P` back. Never reset
    /// within a world: a Replicate that read `L` from the front just before the main thread wrote `P` back must still be corrected.
    engaged: AtomicBool,
    /// StartGameSim calls (the world's number).
    world: AtomicU64,
    /// The value the last front write put there (0: none in this world), and the counts.
    front: AtomicU64,
    front_writes: AtomicU64,
    copy_writes: AtomicU64,
    refusals: AtomicU64,
    refusal: AtomicU32,
    /// A copy could not be corrected in this world (the fault is raised once per world).
    copy_faulted: AtomicBool,
    told: Mutex<Option<Told>>,
}

impl State {
    pub const fn new() -> State {
        State {
            armed: AtomicBool::new(false),
            wanted: AtomicU64::new(0),
            savegame: AtomicU64::new(0),
            engaged: AtomicBool::new(false),
            world: AtomicU64::new(0),
            front: AtomicU64::new(0),
            front_writes: AtomicU64::new(0),
            copy_writes: AtomicU64::new(0),
            refusals: AtomicU64::new(0),
            refusal: AtomicU32::new(0),
            copy_faulted: AtomicBool::new(false),
            told: Mutex::new(None),
        }
    }

    pub fn arm(&self) {
        self.armed.store(true, SeqCst);
    }

    pub fn armed(&self) -> bool {
        self.armed.load(SeqCst)
    }

    /// The mod's order: act as `entity`, or (None) as the savegame's player again. Kept across world loads: the next world's tools are
    /// made with it (the CGameUI constructor reads the front after StartGameSim).
    pub fn set(&self, entity: Option<u32>) {
        self.wanted.store(pack(entity), SeqCst);
    }

    pub fn wanted(&self) -> Option<u32> {
        unpack(self.wanted.load(SeqCst))
    }

    pub fn savegame(&self) -> Option<u32> {
        unpack(self.savegame.load(SeqCst))
    }

    fn refuse(&self, why: Refusal) {
        self.refusal.store(why as u32, Relaxed);
        self.refusals.fetch_add(1, Relaxed);
    }

    /// `CGame::StartGameSim` begins (main thread): a new world. `P` is read from the state the load filled, before StartGameSim
    /// replicates it and before this module writes anything into this world.
    pub fn world_start(&self, mem: &dyn Memory, cgame: usize) {
        self.world.fetch_add(1, SeqCst);
        self.engaged.store(false, SeqCst);
        self.copy_faulted.store(false, SeqCst);
        self.front.store(0, SeqCst);
        self.savegame.store(0, SeqCst);
        if !self.armed() {
            return;
        }
        let player = cgame
            .checked_add(DATA_OFFSET)
            .and_then(|a| mem.word(a))
            .and_then(|data| mem.word(data.checked_add(STATE0_OFFSET)?))
            .and_then(|state| mem.int(state.checked_add(PLAYER_OFFSET)?));
        match player {
            None => self.refuse(Refusal::NoState),
            Some(p) if p < 0 => self.refuse(Refusal::NoPlayer),
            Some(p) => self.savegame.store(pack(Some(p as u32)), SeqCst),
        }
    }

    /// What the front must hold now: `L` while ordered, `P` again once `L` was written in this world and the order is gone, nothing
    /// without a `P` of this world (then the copies could not be corrected).
    pub fn front_target(&self) -> Option<u32> {
        let p = self.savegame()?;
        match self.wanted() {
            Some(l) => Some(l),
            None if self.engaged.load(SeqCst) => Some(p),
            None => None,
        }
    }

    /// After a front assignment (main thread: `Sync` returned, `StartGameSim` returned): the front's `+0x20c` becomes the target.
    pub fn after_front(&self, mem: &dyn Memory, cgame: usize) {
        if !self.armed() {
            return;
        }
        let Some(target) = self.front_target() else { return };
        let Some(field) =
            cgame.checked_add(FRONT_OFFSET).and_then(|a| mem.word(a)).and_then(|f| f.checked_add(PLAYER_OFFSET))
        else {
            return self.refuse(Refusal::Front);
        };
        let Some(now) = mem.int(field) else { return self.refuse(Refusal::Front) };
        if now == target as i32 {
            return;
        }
        // the guard first: a Replicate that copies the value about to be written must already correct its copy
        if Some(target) != self.savegame() {
            self.engaged.store(true, SeqCst);
        }
        if !mem.set_int(field, target as i32) {
            return self.refuse(Refusal::Front);
        }
        self.front.store(pack(Some(target)), SeqCst);
        self.front_writes.fetch_add(1, Relaxed);
    }

    /// After `GameState::Replicate(src, dst)` (the simulation thread, or StartGameSim): the copy gets `P` back once `L` can be in a front
    /// of this world (ordered, or written before).
    pub fn after_replicate(&self, mem: &dyn Memory, dst: usize) {
        if !self.armed() || !(self.engaged.load(SeqCst) || self.wanted().is_some()) {
            return;
        }
        let Some(p) = self.savegame() else { return };
        let ok = match dst.checked_add(PLAYER_OFFSET).map(|f| (f, mem.int(f))) {
            Some((_, Some(v))) if v == p as i32 => true,
            Some((field, Some(_))) => {
                let written = mem.set_int(field, p as i32);
                if written {
                    self.copy_writes.fetch_add(1, Relaxed);
                }
                written
            }
            _ => false,
        };
        if !ok {
            self.refuse(Refusal::Copy);
            // fail loud (failsafe.rs): this simulation state may now hold the interface player, so this game's scripts and its next save
            // can differ from the others; the host's repair loads a world whose states are corrected again. Once per world.
            if !self.copy_faulted.swap(true, SeqCst) {
                crate::failsafe::global(
                    "interface player",
                    crate::failsafe::Action::Repair,
                    "a simulation state's local player could not be set back to the savegame's player after Replicate",
                );
            }
        }
    }

    /// The service thread's lines since it last asked, and whether the mod should hear the state again (`interface_player` event).
    pub fn news(&self) -> (Vec<String>, bool) {
        let mut g = self.told.lock().unwrap_or_else(|p| p.into_inner());
        let told = g.get_or_insert_with(Told::default);
        let mut lines = Vec::new();
        let mut event = false;
        let world = self.world.load(SeqCst);
        let p = self.savegame();
        if world != told.world {
            told.world = world;
            event = true;
            lines.push(match p {
                Some(p) => format!(
                    "interface player: world {world} started: savegame player {p} read from the loaded state (gameStates[0]+0x20c); the simulation's states keep it"
                ),
                None if self.armed() => format!(
                    "interface player: world {world} started WITHOUT a savegame player ({}): nothing is written in this world",
                    Refusal::from_u32(self.refusal.load(Relaxed)).text()
                ),
                None => format!("interface player: world {world} started (piece not armed: nothing is written)"),
            });
        }
        let front = self.front.load(SeqCst);
        if front != told.front {
            told.front = front;
            if let Some(v) = unpack(front) {
                event = true;
                lines.push(format!(
                    "interface player: front state +0x20c = {v} in world {world} (savegame player {}; {} front write(s), {} copy correction(s) so far)",
                    p.map_or("none".to_string(), |p| p.to_string()),
                    self.front_writes.load(Relaxed),
                    self.copy_writes.load(Relaxed)
                ));
            }
        }
        let refusals = self.refusals.load(Relaxed);
        if refusals != told.refusals {
            told.refusals = refusals;
            lines.push(format!(
                "interface player: REFUSED {} ({refusals} refusal(s) in this process): nothing was written there",
                Refusal::from_u32(self.refusal.load(Relaxed)).text()
            ));
        }
        (lines, event)
    }

    /// The event's fields: `L`, `P`, what the front holds by this module's last write, the world's number.
    pub fn snapshot(&self) -> (Option<u32>, Option<u32>, Option<u32>, u64) {
        (self.wanted(), self.savegame(), unpack(self.front.load(SeqCst)), self.world.load(SeqCst))
    }

    /// The `interface_player` event for the mod: the order, the savegame's player (the mod's canonical player while the front acts as a
    /// company), the front's value by this module, the world; `why` a refusal or "".
    pub fn event(&self, why: &str) -> crate::link::Event {
        let (player, savegame, front, world) = self.snapshot();
        let n = |v: Option<u32>| v.map_or(-1, i64::from);
        crate::link::Event::InterfacePlayer {
            player: n(player),
            savegame: n(savegame),
            front: n(front),
            world,
            why: why.to_string(),
        }
    }
}

impl Default for State {
    fn default() -> State {
        State::new()
    }
}

/// The one state of the process.
pub static STATE: State = State::new();

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A game's memory as a map: words and ints by address; an address that is not in it is not readable.
    #[derive(Default)]
    struct Fake {
        words: HashMap<usize, usize>,
        ints: RefCell<HashMap<usize, i32>>,
        read_only: Vec<usize>,
    }

    impl Memory for Fake {
        fn word(&self, addr: usize) -> Option<usize> {
            self.words.get(&addr).copied()
        }
        fn int(&self, addr: usize) -> Option<i32> {
            self.ints.borrow().get(&addr).copied()
        }
        fn set_int(&self, addr: usize, value: i32) -> bool {
            if self.read_only.contains(&addr) || !self.ints.borrow().contains_key(&addr) {
                return false;
            }
            self.ints.borrow_mut().insert(addr, value);
            true
        }
    }

    const GAME: usize = 0x10_0000;
    const DATA: usize = 0x20_0000;
    const S0: usize = 0x30_0000;
    const S1: usize = 0x40_0000;

    /// A CGame whose gameStates[0] (the loaded one) names `p` and whose front is `front`.
    fn game(p: i32, front: usize) -> Fake {
        let mut f = Fake::default();
        f.words.insert(GAME + DATA_OFFSET, DATA);
        f.words.insert(DATA + STATE0_OFFSET, S0);
        f.words.insert(GAME + FRONT_OFFSET, front);
        f.ints.borrow_mut().insert(S0 + PLAYER_OFFSET, p);
        f.ints.borrow_mut().insert(S1 + PLAYER_OFFSET, p);
        f
    }

    fn player(f: &Fake, state: usize) -> i32 {
        f.int(state + PLAYER_OFFSET).unwrap()
    }

    #[test]
    fn unset_writes_nothing_and_an_order_reaches_only_the_front_while_the_copies_keep_the_savegame_player() {
        let s = State::new();
        s.arm();
        let g = game(7, S1);
        s.world_start(&g, GAME);
        assert_eq!(s.savegame(), Some(7));
        s.after_front(&g, GAME);
        s.after_replicate(&g, S0);
        assert_eq!((player(&g, S0), player(&g, S1), s.snapshot().2), (7, 7, None), "no order: nothing written");
        s.set(Some(42));
        s.after_front(&g, GAME);
        assert_eq!(player(&g, S1), 42, "the front acts as the company");
        // the simulation replicates the front into the other state: the copy gets the savegame player back
        g.ints.borrow_mut().insert(S0 + PLAYER_OFFSET, 42);
        s.after_replicate(&g, S0);
        assert_eq!(player(&g, S0), 7);
        // the order is cleared: the front is given back, and a copy made from the old front is still corrected
        s.set(None);
        s.after_front(&g, GAME);
        assert_eq!(player(&g, S1), 7);
        g.ints.borrow_mut().insert(S0 + PLAYER_OFFSET, 42);
        s.after_replicate(&g, S0);
        assert_eq!(player(&g, S0), 7, "engaged for the rest of the world");
        let (lines, event) = s.news();
        assert!(event, "the mod hears the state");
        assert!(lines.iter().any(|l| l.contains("world 1 started: savegame player 7")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("front state +0x20c = 7")), "{lines:?}");
        assert_eq!(s.news(), (vec![], false), "said once");
    }

    #[test]
    fn nothing_is_written_without_the_savegame_player_or_when_not_armed_and_refusals_are_said() {
        let s = State::new();
        let g = game(7, S1);
        s.set(Some(42));
        s.world_start(&g, GAME);
        s.after_front(&g, GAME);
        assert_eq!((s.savegame(), player(&g, S1)), (None, 7), "not armed: not even P is read");
        s.arm();
        let g = game(-1, S1);
        s.world_start(&g, GAME);
        s.after_front(&g, GAME);
        assert_eq!(player(&g, S1), -1, "no P in this world: L is never written");
        let (lines, _) = s.news();
        assert!(lines.iter().any(|l| l.contains("WITHOUT a savegame player")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.starts_with("interface player: REFUSED the loaded state names no player")),
            "{lines:?}"
        );
        // a front that cannot be written: refused, nothing claimed
        let mut g = game(7, S1);
        g.read_only.push(S1 + PLAYER_OFFSET);
        s.world_start(&g, GAME);
        s.after_front(&g, GAME);
        assert_eq!((player(&g, S1), s.snapshot().2), (7, None));
        assert!(s.news().0.iter().any(|l| l.contains("REFUSED the front state")));
        // the order outlives the world: the next world's first front gets it at once
        let mut g = game(9, S1);
        s.world_start(&g, GAME);
        s.after_front(&g, GAME);
        assert_eq!((s.savegame(), player(&g, S1)), (Some(9), 42));
        // a copy that cannot be set back is a repair fault, once per world
        let _ = crate::failsafe::GLOBAL.drain();
        g.ints.borrow_mut().insert(S0 + PLAYER_OFFSET, 42);
        g.read_only.push(S0 + PLAYER_OFFSET);
        s.after_replicate(&g, S0);
        s.after_replicate(&g, S0);
        let faults = crate::failsafe::GLOBAL.drain();
        assert_eq!(faults.iter().filter(|f| f.place == "interface player").count(), 1, "{faults:?}");
    }

    #[test]
    fn the_row_needs_every_site_and_witness_and_the_switch_is_on_unless_0() {
        assert!(switch_on(|_| None) && switch_on(|_| Some("1".into())) && !switch_on(|_| Some(" 0".into())));
        let row = row_for(0x6ad9_ce10).expect("40425");
        assert!(row.start.prologue.len() >= crate::detour::JMP_ABS_LEN);
        assert!(row.replicate.prologue.len() >= crate::detour::JMP_ABS_LEN);
        // the build 40425 row as the search finds it (port report), on an image holding every expected byte
        let build = crate::build_table::parse_report(include_str!("../../tools/addr/out/port-6ac50427-6ad9ce10.txt"))
            .expect("the 40425 report");
        let call = build.batch_pace.clone().unwrap();
        let mut img = crate::plan::BytesImage::zeroed(0x10_0000, 0x260_0000);
        img.write(call.rva, call.expected);
        img.write(call.callee.rva, call.callee.prologue);
        img.write(row.start.rva, row.start.prologue);
        img.write(row.replicate.rva, row.replicate.prologue);
        for w in row.witnesses.iter().take(4) {
            img.write(w.rva, w.bytes);
        }
        let err = check(&build, &img).unwrap_err();
        assert!(err.contains("CGameUI constructor"), "a missing witness refuses: {err}");
        img.write(row.witnesses[4].rva, row.witnesses[4].bytes);
        assert_eq!(check(&build, &img), Ok(row));
        img.write(row.replicate.rva + 4, &[0x10]);
        assert!(check(&build, &img).unwrap_err().starts_with("GameState::Replicate: bytes at rva 0x255e70"));
        let other = crate::build_table::fixture();
        assert!(check(other, &img).unwrap_err().starts_with("no verified row for game build 40420"));
        // the plan: both detours, and the call of Sync redirected for the front write only when smooth pacing does not redirect it
        img.write(row.replicate.rva + 4, &[0x08]);
        use crate::plan::{plan, Feature, HookConfig};
        let cfg = HookConfig { hooks: true, interface_player: true, ..Default::default() };
        let features = |c: HookConfig| plan(&build, c, &img).steps.iter().map(|s| s.feature()).collect::<Vec<_>>();
        let off = features(cfg);
        assert!(off.contains(&Feature::InterfaceStart) && off.contains(&Feature::InterfaceReplicate));
        assert!(off.contains(&Feature::InterfaceSync) && !off.contains(&Feature::BatchPace), "{off:?}");
        let on = features(HookConfig { smooth_pace: true, ..cfg });
        assert!(on.contains(&Feature::BatchPace) && !on.contains(&Feature::InterfaceSync), "{on:?}");
        let none = features(HookConfig { interface_player: false, ..cfg });
        assert!(!none.contains(&Feature::InterfaceStart) && !none.contains(&Feature::InterfaceSync));
    }
}
