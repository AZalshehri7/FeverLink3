//! The unsafe edge of the module: the two functions the game's code is made to jump to, the installation of every
//! piece with its rollback, and the service thread (`docs/NATIVE_LINK.md` section 2).
//!
//! Nothing here decides anything: [`plan::plan`] decides what to install from the switches, the build table and
//! the bytes in memory, [`execute`] carries the plan out, and [`Runtime`] does the work while the game runs. What
//! this module adds is the order of things and the way back:
//!
//! 1. The pool patch is made first and is independent of the rest.
//! 2. Every detour is *prepared* (prologue verified once more, trampoline built) before any is written.
//! 3. The trampolines are published to the runtime, and only then are the jumps written, so a hook can never run
//!    before it knows what to call.
//! 4. If writing any jump fails, every jump already written is taken out again, newest first. The module then runs
//!    without hooks, which is the game as shipped.
//!
//! All of this happens in `DllMain`, while the process is still starting and no thread is inside the functions
//! being patched. After that the module never rewrites game code again: a misbehaving mod is handled by the
//! hooks passing everything through (holding off, silence), not by uninstalling.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GetModuleHandleW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
};

use crate::build_table::{BuildAddrs, Rva};
use crate::detour::{hook_caller, Detour};
use crate::engine::{AddArgs, EngineFns};
use crate::growth::{self, Mode, Settings};
use crate::hold::HoldConfig;
use crate::mem;
use crate::patch::AppliedPatch;
use crate::plan::{self, Feature, HookConfig, Image, Plan, Skip, Step};
use crate::runtime::{Init, Runtime};
#[cfg(target_arch = "x86_64")]
use crate::selftest;
use crate::townseed::{self, Counter};

/// The live runtime the two hook functions work on. Set before any jump is written.
static RUNTIME: OnceLock<Runtime> = OnceLock::new();
/// What is installed, kept for the life of the process.
static INSTALLED: Mutex<Option<Installed>> = Mutex::new(None);

/// The function the game's `CommandList::Add` is made to jump to. Declared as one that may unwind: a C++ exception
/// from the engine can pass through it, and the machine-level ABI is the same.
unsafe extern "system-unwind" fn add_hook(
    list: *mut u8,
    out: *mut u8,
    cmd: *mut u8,
    done: *mut u8,
    progress: *mut u8,
) -> *mut u8 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::ADD_HOOK);
    // first, before anything can change the frames above us: the return address of the call that got here
    let caller = hook_caller();
    match RUNTIME.get() {
        // SAFETY: the arguments of the live call this function was jumped to from.
        Some(rt) => unsafe { rt.on_add(AddArgs { list, out, cmd, done, progress }, caller) },
        None => out,
    }
}

/// The function the game's command application is made to jump to (may unwind, like `add_hook`).
unsafe extern "system-unwind" fn apply_hook(sim: *mut u8, cmd: *mut u8) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::APPLY_HOOK);
    match RUNTIME.get() {
        // SAFETY: the arguments of the live call this function was jumped to from.
        Some(rt) => unsafe { rt.on_apply(sim, cmd) },
        None => 0,
    }
}

/// The function `scripting::Convert(out optional<Proposal>*, state*, SimpleProposal*, ..)` is made to jump to: calls the original with
/// every slot as given (four registers, four stack slots), then lets the runtime write the armed `patch_next` order's frozenNodes
/// into the proposal it produced. Returns what the original returned. The runtime's bookkeeping is contained (`log::guarded`, inside
/// `on_convert`); the calls into the game (the original, the allocator of the write) are outside it and may unwind.
#[allow(clippy::too_many_arguments)]
unsafe extern "system-unwind" fn convert_hook(
    rcx: u64,
    rdx: u64,
    r8: u64,
    r9: u64,
    a5: u64,
    a6: u64,
    a7: u64,
    a8: u64,
) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::CONVERT_HOOK);
    // first, before anything can change the frames above us
    let caller = hook_caller();
    let original = CONVERT_ORIGINAL.load(Ordering::Acquire);
    if original == 0 {
        return 0; // unreachable once installed
    }
    type Original = unsafe extern "system-unwind" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
    // SAFETY: the trampoline for `Convert`; the extra slots are the caller's own stack words, passed on untouched.
    let original: Original = unsafe { core::mem::transmute(original) };
    let ret = unsafe { original(rcx, rdx, r8, r9, a5, a6, a7, a8) };
    if let Some(rt) = RUNTIME.get() {
        // what the conversion builds, while inject orders wait (positive identity of the mod's command; reads only, contained)
        rt.inject_on_convert(rcx as usize);
        // guards its own bookkeeping: the write calls the game's allocator, which must not run inside a guard
        rt.on_convert(rcx as usize, caller);
    }
    ret
}

/// The function the construction update inside `Convert` (`0x10c6020(bool, state, construction, ctx, a5, Proposal* being built)`) is
/// made to jump to: lets the runtime put the armed order's frozenNodes into the proposal BEFORE the update adds the construction's
/// own entrance street, calls the original with every slot as given, and lets the runtime log what the update left.
unsafe extern "system-unwind" fn convert_update_hook(rcx: u64, rdx: u64, r8: u64, r9: u64, a5: u64, a6: u64) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::CONVERT_UPDATE_HOOK);
    let original = CONVERT_UPDATE_ORIGINAL.load(Ordering::Acquire);
    if original == 0 {
        return 0; // unreachable once installed
    }
    // guards its own bookkeeping: the write calls the game's allocator, which must not run inside a guard
    let armed = RUNTIME.get().is_some_and(|rt| rt.before_convert_update(a6 as usize));
    type Original = unsafe extern "system-unwind" fn(u64, u64, u64, u64, u64, u64) -> u64;
    // SAFETY: the trampoline for the update, which takes these six slots.
    let original: Original = unsafe { core::mem::transmute(original) };
    let ret = unsafe { original(rcx, rdx, r8, r9, a5, a6) };
    if armed {
        if let Some(rt) = RUNTIME.get() {
            crate::log::guarded("convert update hook", || rt.after_convert_update(a6 as usize));
        }
    }
    ret
}

/// The function the interface's `builder.proposalCreate` emitter is made to jump to: keeps its first five arguments (the interface and
/// script objects of the newest tool preview, [`Runtime::note_preview_args`]) and calls the original with all nine slots as given.
#[allow(clippy::too_many_arguments)]
unsafe extern "system-unwind" fn preview_hook(
    a: u64,
    b: u64,
    c: u64,
    d: u64,
    e: u64,
    tool: u64,
    proposal: u64,
    data: u64,
    out: u64,
) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::PREVIEW_HOOK);
    let original = PREVIEW_ORIGINAL.load(Ordering::Acquire);
    if original == 0 {
        return 0; // unreachable once installed
    }
    if let Some(rt) = RUNTIME.get() {
        crate::log::guarded("preview hook", || rt.note_preview_args([a, b, c, d, e]));
        // once per game interface: the arguments computed from its listener's captures compared with these (calls the engine's getter
        // the listener itself has just called, so in this plain frame)
        // SAFETY: on the interface thread inside the listener's own call, where the captures it was made with are alive.
        unsafe { rt.check_preview_owner([a, b, c, d, e]) };
    }
    type Original = unsafe extern "system-unwind" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
    // SAFETY: the trampoline of the emitter, which takes these nine slots.
    let original: Original = unsafe { core::mem::transmute(original) };
    unsafe { original(a, b, c, d, e, tool, proposal, data, out) }
}

/// The unhooked `builder.proposalCreate` emitter (the trampoline), published before the jump is written; 0 until then.
static PREVIEW_ORIGINAL: AtomicUsize = AtomicUsize::new(0);

/// Where the redirected call in `UI::CGameUI::CGameUI` leads after [`preview_owner_hook`]: the listener's `std::function` constructor
/// (`0x642dc0`); 0 until published (before the call is patched).
static PREVIEW_OWNER_CALLEE: AtomicUsize = AtomicUsize::new(0);

/// The call `std::function(lambda)` at `0x64cb41` in `UI::CGameUI::CGameUI` (once per loaded world, `build_table::PreviewOwnerSite`),
/// reached through a thunk: `captures` points at the builder-preview listener's three captures, which are kept
/// ([`Runtime::note_preview_owner`], guarded reads only); then the real constructor (a leaf that copies them) runs with both arguments.
unsafe extern "system-unwind" fn preview_owner_hook(function: u64, captures: u64) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::PREVIEW_OWNER_HOOK);
    let callee = PREVIEW_OWNER_CALLEE.load(Ordering::Acquire);
    if callee == 0 {
        return function; // unreachable once installed
    }
    if let Some(rt) = RUNTIME.get() {
        crate::log::guarded("preview owner hook", || rt.note_preview_owner(captures as usize));
    }
    type Original = unsafe extern "system-unwind" fn(u64, u64) -> u64;
    // SAFETY: the real constructor at the call's original target, which takes these two slots.
    let original: Original = unsafe { core::mem::transmute(callee) };
    unsafe { original(function, captures) }
}

/// The unhooked town-seed function (the trampoline), published before the jump is written; 0 until then.
static TOWN_SEED_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
/// Base address of the image, for turning the caller into an RVA in the log; 0 until armed.
static TOWN_SEED_BASE: AtomicUsize = AtomicUsize::new(0);
/// The log function (`fn(&str)` as an address); 0 until armed.
static TOWN_SEED_LOG: AtomicUsize = AtomicUsize::new(0);
static TOWN_SEED_COUNTER: Counter = Counter::new();
/// The forced seed (`FEVERLINK3_FORCE_TOWN_SEED`): bit 32 set means "force", the low 32 bits are the value.
static TOWN_SEED_FORCE: AtomicU64 = AtomicU64::new(0);
const FORCE_FLAG: u64 = 1 << 32;

/// The largest image the caller's RVA is believed for (the game's executable is 70 MB).
const TOWN_SEED_IMAGE_BOUND: usize = 0x1000_0000;

/// Tells the town-seed hook where to log and what the image base is. Called once, before the jump is written.
fn arm_town_seed(base: usize, log: fn(&str), force: Option<u32>) {
    TOWN_SEED_FORCE.store(force.map_or(0, |v| FORCE_FLAG | u64::from(v)), Ordering::Release);
    TOWN_SEED_COUNTER.reset();
    TOWN_SEED_BASE.store(base, Ordering::Release);
    TOWN_SEED_LOG.store(log as usize, Ordering::Release);
}

/// The function the game's town-task seeding function is made to jump to: logs, then calls the original with the
/// same twelve argument slots (four in registers, eight on the stack) and returns what it returns, unchanged.
///
/// The game's function is `sub_8d4270(out, a2, a3, a4, a5..a8, seed, a10, a11, a12)`; the seed is the ninth
/// argument, `[rsp+0x48]` at its entry. Slots are passed through as raw 64-bit values (three of them carry floats,
/// two pointers), so the original sees exactly the bits the caller wrote. Reads nothing behind any pointer.
#[allow(clippy::too_many_arguments)]
unsafe extern "system-unwind" fn town_seed_hook(
    out: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
    a7: u64,
    a8: u64,
    seed: u64,
    a10: u64,
    a11: u64,
    a12: u64,
) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::TOWN_SEED_HOOK);
    // first, before anything can change the frames above us: the return address of the call that got here
    let caller = hook_caller();
    let original = TOWN_SEED_ORIGINAL.load(Ordering::Acquire);
    if original == 0 {
        return out;
    }
    let forced = TOWN_SEED_FORCE.load(Ordering::Acquire);
    let forced = (forced & FORCE_FLAG != 0).then_some(forced as u32);
    let log = TOWN_SEED_LOG.load(Ordering::Acquire);
    if log != 0 {
        // The log line is our own bookkeeping, on the game's thread: a failure in it (a panic, under `unwind`) is
        // contained and the original is called all the same. The call below is not inside the guard, so a C++
        // exception of the game passes through it.
        crate::log::guarded("town_seed hook", || {
            // SAFETY: stored by `arm_town_seed` from a `fn(&str)`.
            let log: fn(&str) = unsafe { core::mem::transmute(log) };
            let caller = townseed::caller_field(caller, TOWN_SEED_BASE.load(Ordering::Acquire), TOWN_SEED_IMAGE_BOUND);
            log(&townseed::line(TOWN_SEED_COUNTER.next(), seed as u32, forced, &caller, a3 as u32, a4 as u32));
        });
    }
    // The ninth slot is an ordinary parameter of this function: the call below writes the outgoing copy of it to the
    // original's `[rsp+0x48]`, so replacing the value here is what the original reads. The other slots pass as given.
    let seed = forced.map_or(seed, u64::from);
    type Original = unsafe extern "system-unwind" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
    // SAFETY: the trampoline `Detour::prepare` built for the function this hook replaces, which takes these slots.
    let original: Original = unsafe { core::mem::transmute(original) };
    unsafe { original(out, a2, a3, a4, a5, a6, a7, a8, seed, a10, a11, a12) }
}

// ---- town growth (`growth.rs`, `docs/NATIVE_LINK.md` section 11)

/// The unhooked `TownDeveloper::Develop` and `TownUpdateSize` handler (trampolines); 0 until published.
static GROWTH_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
/// The unhooked `scripting::Convert` (the trampoline), published before the jump is written; 0 until then.
static CONVERT_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
/// The unhooked construction update `Convert` runs (the trampoline); 0 until published.
static CONVERT_UPDATE_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static GROWTH_CMD_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
/// Address of the game's `GameTime` getter; 0 when the game time cannot be read.
static GROWTH_GETTER: AtomicUsize = AtomicUsize::new(0);
static GROWTH_BASE: AtomicUsize = AtomicUsize::new(0);
static GROWTH_LOG: AtomicUsize = AtomicUsize::new(0);
static GROWTH_COUNTER: growth::Counter = growth::Counter::new();
/// The game time of the `TownUpdateSize` command being executed: `TIME_VALID | time`, 0 outside such a command.
static GROWTH_TIME: AtomicU64 = AtomicU64::new(0);
const TIME_VALID: u64 = 1 << 32;
/// Set once (until re-armed): the seed is replaced from now on.
static GROWTH_SWITCHED: AtomicBool = AtomicBool::new(false);
static GROWTH_DERIVE: AtomicBool = AtomicBool::new(false);
static GROWTH_AFTER_WALL_S: AtomicU64 = AtomicU64::new(0);
/// Game seconds before the switch; `u64::MAX` = no threshold.
static GROWTH_AFTER_GAME_S: AtomicU64 = AtomicU64::new(u64::MAX);
static GROWTH_QUANTUM_MS: AtomicU32 = AtomicU32::new(growth::DEFAULT_QUANTUM_MS);
/// The towns whose every call is logged (`FEVERLINK3_TOWN_TRACE`).
static GROWTH_TRACE: Mutex<growth::Trace> = Mutex::new(growth::Trace::NONE);
/// The game's own live folder: `growth.log` is appended there, one file per game (`native.log` is shared).
static GROWTH_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
/// When the hook was armed, in nanoseconds after `GROWTH_ORIGIN`.
static GROWTH_START_NS: AtomicU64 = AtomicU64::new(0);
static GROWTH_ORIGIN: OnceLock<Instant> = OnceLock::new();

fn growth_now_ns() -> u64 {
    GROWTH_ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

fn growth_wall_s() -> u64 {
    growth_now_ns().saturating_sub(GROWTH_START_NS.load(Ordering::Acquire)) / 1_000_000_000
}

/// Tells the growth hooks where to log, what they may do and where the game-time getter is (0 = none). Called once,
/// before the jumps are written.
fn arm_growth(base: usize, log: fn(&str), settings: &Settings, getter: usize, dir: Option<PathBuf>) {
    GROWTH_COUNTER.reset();
    *GROWTH_TRACE.lock().unwrap_or_else(|p| p.into_inner()) = settings.trace;
    *GROWTH_DIR.lock().unwrap_or_else(|p| p.into_inner()) = dir;
    GROWTH_TIME.store(0, Ordering::Release);
    GROWTH_DEEP.store(settings.deep, Ordering::Release);
    DEEP_CMD_N.store(0, Ordering::Release);
    DEEP_ORPHANS.store(0, Ordering::Release);
    *DEEP_TICK.lock().unwrap_or_else(|p| p.into_inner()) =
        DeepTick { valid: false, t: None, c: 0, per_town: Vec::new() };
    GROWTH_SWITCHED.store(false, Ordering::Release);
    GROWTH_DERIVE.store(settings.mode == Mode::Derive, Ordering::Release);
    GROWTH_AFTER_WALL_S.store(settings.after_wall_s, Ordering::Release);
    GROWTH_AFTER_GAME_S.store(settings.after_game_s.unwrap_or(u64::MAX), Ordering::Release);
    GROWTH_QUANTUM_MS.store(settings.quantum_ms, Ordering::Release);
    GROWTH_START_NS.store(growth_now_ns(), Ordering::Release);
    GROWTH_GETTER.store(getter, Ordering::Release);
    GROWTH_BASE.store(base, Ordering::Release);
    GROWTH_LOG.store(log as usize, Ordering::Release);
}

fn growth_settings() -> Settings {
    let game = GROWTH_AFTER_GAME_S.load(Ordering::Acquire);
    Settings {
        log: true,
        mode: if GROWTH_DERIVE.load(Ordering::Acquire) { Mode::Derive } else { Mode::Off },
        after_wall_s: GROWTH_AFTER_WALL_S.load(Ordering::Acquire),
        after_game_s: (game != u64::MAX).then_some(game),
        quantum_ms: GROWTH_QUANTUM_MS.load(Ordering::Acquire),
        trace: *GROWTH_TRACE.lock().unwrap_or_else(|p| p.into_inner()),
        deep: GROWTH_DEEP.load(Ordering::Acquire),
    }
}

/// Appends one line to `growth.log` in the game's live folder (opened per line: a handful per second at most, and the
/// session controller may clear the folder, which an open handle would not survive). Errors are ignored.
fn growth_file_line(line: &str) {
    let dir = GROWTH_DIR.lock().unwrap_or_else(|p| p.into_inner()).clone();
    if let Some(dir) = dir {
        // no lock is held here: the folder was cloned out of it
        let _inside = crate::hookwatch::GROWTH_FILE.enter();
        use std::io::Write;
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(growth::LOG_FILE)) {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// Clears the game time of the running command when the handler returns or unwinds.
struct ClearGrowthTime;

impl Drop for ClearGrowthTime {
    fn drop(&mut self) {
        GROWTH_TIME.store(0, Ordering::Release);
    }
}

// ---- the deep growth log (`FEVERLINK3_LOG_GROWTH_DEEP=1`, `docs/NATIVE_LINK.md` section 11.5). Read-only: it reads the
// arguments of calls and the memory they point to, and writes lines; it never writes into the game.

static GROWTH_DEEP: AtomicBool = AtomicBool::new(false);
/// The unhooked candidate gather (`0x9677c0`) and edge selection (`0x95d100`); 0 until published.
static CANDS_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static EDGES_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
/// `TownUpdateSize` commands seen (for the sampling).
static DEEP_CMD_N: AtomicU64 = AtomicU64::new(0);
/// Gathers and selections made outside any `Develop` that were logged.
static DEEP_ORPHANS: AtomicU64 = AtomicU64::new(0);

/// The per-tick counters that give a command its place: `c` over all towns, `k` per town.
struct DeepTick {
    valid: bool,
    t: Option<u32>,
    c: u32,
    per_town: Vec<(u32, u32)>,
}

static DEEP_TICK: Mutex<DeepTick> = Mutex::new(DeepTick { valid: false, t: None, c: 0, per_town: Vec::new() });

/// `(c, k)` of the next `TownUpdateSize` command of `town` at tick `t`; both count from 0 within a tick.
fn deep_next_index(t: Option<u32>, town: u32) -> (u32, u32) {
    let mut s = DEEP_TICK.lock().unwrap_or_else(|p| p.into_inner());
    if !s.valid || s.t != t {
        s.valid = true;
        s.t = t;
        s.c = 0;
        s.per_town.clear();
    }
    let c = s.c;
    s.c += 1;
    let k = match s.per_town.iter_mut().find(|(id, _)| *id == town) {
        Some((_, n)) => {
            let k = *n;
            *n += 1;
            k
        }
        None => {
            s.per_town.push((town, 1));
            0
        }
    };
    (c, k)
}

/// The `TownUpdateSize` command being executed on this thread.
#[derive(Clone, Copy)]
struct DeepCmd {
    t: Option<u32>,
    k: u32,
    want: bool,
}

/// The `Develop` call being executed on this thread, with the counters of the gathers and selections inside it.
#[derive(Clone, Copy)]
struct DeepDev {
    town: u32,
    t: Option<u32>,
    k: u32,
    want: bool,
    d: u64,
    cand_j: u32,
    edge_j: u32,
    field_j: u32,
}

#[derive(Clone, Copy)]
struct DeepCtx {
    cmd: Option<DeepCmd>,
    dev: Option<DeepDev>,
}

const DEEP_NONE: DeepCtx = DeepCtx { cmd: None, dev: None };

thread_local! {
    static DEEP_CTX: Cell<DeepCtx> = const { Cell::new(DEEP_NONE) };
}

fn deep_ctx() -> DeepCtx {
    DEEP_CTX.try_with(Cell::get).unwrap_or(DEEP_NONE)
}

fn deep_ctx_update(f: impl FnOnce(&mut DeepCtx)) {
    let _ = DEEP_CTX.try_with(|c| {
        let mut v = c.get();
        f(&mut v);
        c.set(v);
    });
}

/// Clears the command of this thread when the handler returns or unwinds.
struct DeepCmdGuard;

impl Drop for DeepCmdGuard {
    fn drop(&mut self) {
        deep_ctx_update(|c| c.cmd = None);
    }
}

/// Puts back the `Develop` that was running before (none, normally) when this one returns or unwinds.
struct DeepDevGuard(Option<DeepDev>);

impl Drop for DeepDevGuard {
    fn drop(&mut self) {
        let prev = self.0;
        deep_ctx_update(|c| c.dev = prev);
    }
}

/// The u32 at `addr` when the address is aligned and the bytes are committed and readable. Through the guarded reader
/// (`memguard`): the deep log reads the engine's heap on the engine's own threads, and a block freed between the OS's answer and
/// the read was an access fault on a game thread (which a worker's own exception handling may swallow half-done) for a log line.
fn deep_read_u32(addr: u64) -> Option<u32> {
    crate::memguard::read_value::<u32>(usize::try_from(addr).ok()?)
}

/// Where the lines of the current thread belong: the `Develop` it is in, else a numbered orphan (the first
/// [`growth::ORPHAN_MAX`] only). The second value is the call's index `j`, the third whether to log it.
fn deep_place(is_cand: bool) -> Option<(growth::DeepKey, u32, bool)> {
    let pid = std::process::id();
    match deep_ctx().dev {
        Some(d) => {
            deep_ctx_update(|c| {
                if let Some(d) = c.dev.as_mut() {
                    if is_cand {
                        d.cand_j += 1;
                    } else {
                        d.edge_j += 1;
                    }
                }
            });
            let j = if is_cand { d.cand_j } else { d.edge_j };
            Some((growth::DeepKey { pid, t: d.t, town: d.town, k: d.k }, j, d.want))
        }
        None => {
            let n = DEEP_ORPHANS.fetch_add(1, Ordering::Relaxed);
            (n < growth::ORPHAN_MAX).then_some((growth::DeepKey { pid, t: None, town: 0, k: 0 }, n as u32, true))
        }
    }
}

/// `TownUpdateSize` entry: logs the payload `{town, 3 floats, flag}` and makes the command known to the calls inside it.
/// The payload layout is that of the handler at `0x9e12b0` (`mov r8d,[r15]`, `vmovsd [r15+4]`, `mov ecx,[r15+0xc]`,
/// `movzx r9d,byte [r15+0x10]`).
fn deep_payload(payload: u64, time: Option<u32>) -> Option<DeepCmdGuard> {
    let p = crate::memguard::read(usize::try_from(payload).ok()?, 0x11).ok()?;
    let u = |at: usize| u32::from_le_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]]);
    let (town, b1, b2, b3, flag) = (u(0), u(4), u(8), u(0xc), p[0x10]);
    let n = DEEP_CMD_N.fetch_add(1, Ordering::Relaxed) + 1;
    let traced = GROWTH_TRACE.lock().unwrap_or_else(|p| p.into_inner()).contains(town);
    let want = traced || growth::sampled(n, town, time.unwrap_or(0));
    let (c, k) = deep_next_index(time, town);
    deep_ctx_update(|ctx| ctx.cmd = Some(DeepCmd { t: time, k, want }));
    if want {
        let key = growth::DeepKey { pid: std::process::id(), t: time, town, k };
        growth_file_line(&growth::payload_line(&key, c, [b1, b2, b3], flag));
    }
    Some(DeepCmdGuard)
}

/// `Develop` entry: remembers which call this is, for the gathers and selections inside it.
fn deep_dev_enter(town: u32, seed: u32, n: u64, time: Option<u32>) -> DeepDevGuard {
    let ctx = deep_ctx();
    let (t, k, want) = match ctx.cmd {
        Some(c) => (c.t, c.k, c.want),
        None => {
            let traced = GROWTH_TRACE.lock().unwrap_or_else(|p| p.into_inner()).contains(town);
            (time, 0, traced || growth::sampled(n, town, seed))
        }
    };
    deep_ctx_update(|c| c.dev = Some(DeepDev { town, t, k, want, d: n, cand_j: 0, edge_j: 0, field_j: 0 }));
    DeepDevGuard(ctx.dev)
}

/// `Develop` exit: the generator state it started from and the one it ended with.
fn deep_dev_exit(state: u64, start: u32) {
    let Some(d) = deep_ctx().dev else { return };
    if !d.want {
        return;
    }
    if let Some(end) = deep_read_u32(state) {
        let key = growth::DeepKey { pid: std::process::id(), t: d.t, town: d.town, k: d.k };
        growth_file_line(&growth::dev_line(&key, d.d, start, end));
    }
}

/// After a gather: the count and the hashes of its records.
fn deep_candidates(out: u64) {
    let Some((key, j, want)) = deep_place(true) else { return };
    if !want || out == 0 {
        return;
    }
    // `out` is the vector the gather just filled
    let Ok(hdr) = crate::memguard::read(out as usize, 16) else { return };
    let begin = u64::from_le_bytes(hdr[..8].try_into().unwrap_or([0; 8]));
    let end = u64::from_le_bytes(hdr[8..].try_into().unwrap_or([0; 8]));
    let empty = growth::digest_records(&[]);
    if begin == end {
        growth_file_line(&growth::cand_line(&key, j, &empty, false));
        return;
    }
    let len = end.wrapping_sub(begin);
    if end < begin || len % growth::RECORD_LEN as u64 != 0 {
        growth_file_line(&growth::cand_line(&key, j, &empty, true));
        return;
    }
    let n = len / growth::RECORD_LEN as u64;
    if n as usize > growth::MAX_RECORDS {
        growth_file_line(&growth::cand_line(&key, j, &growth::Digest { n, ..empty }, true));
        return;
    }
    match crate::memguard::read(begin as usize, len as usize) {
        Ok(bytes) => growth_file_line(&growth::cand_line(&key, j, &growth::digest_records(&bytes), false)),
        Err(_) => growth_file_line(&growth::cand_line(&key, j, &growth::Digest { n, ..empty }, true)),
    }
}

/// The candidate gather `(out, centre*, radius [xmm2], ctx [r9], 5 stack arguments) -> out`. Calls the original with
/// every slot as given (the float stays in `xmm2`), then logs the vector it filled. Changes nothing.
#[allow(clippy::too_many_arguments)]
unsafe extern "system-unwind" fn candidates_hook(
    rcx: u64,
    rdx: u64,
    radius: f32,
    r9: u64,
    a5: u64,
    a6: u64,
    a7: u64,
    a8: u64,
    a9: u64,
) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::CANDIDATES_HOOK);
    let original = CANDS_ORIGINAL.load(Ordering::Acquire);
    if original == 0 {
        return rcx;
    }
    type Original = unsafe extern "system-unwind" fn(u64, u64, f32, u64, u64, u64, u64, u64, u64) -> u64;
    // SAFETY: the trampoline for the gather this hook replaces, which takes these slots.
    let original: Original = unsafe { core::mem::transmute(original) };
    let ret = unsafe { original(rcx, rdx, radius, r9, a5, a6, a7, a8, a9) };
    if GROWTH_DEEP.load(Ordering::Acquire) {
        crate::log::guarded("growth deep candidates", || deep_candidates(rcx));
    }
    ret
}

/// The edge selection `(centre*, radius [xmm1], world, ctx [r9], arg5, u32* rng) -> void`: logs the rng state before and
/// after. Calls the original with every slot as given. Changes nothing.
unsafe extern "system-unwind" fn edges_hook(rcx: u64, radius: f32, r8: u64, r9: u64, a5: u64, rng: u64) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::EDGES_HOOK);
    let original = EDGES_ORIGINAL.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    let deep = GROWTH_DEEP.load(Ordering::Acquire);
    let before = if deep { crate::log::guarded("growth deep edge", || deep_read_u32(rng)).flatten() } else { None };
    type Original = unsafe extern "system-unwind" fn(u64, f32, u64, u64, u64, u64) -> u64;
    // SAFETY: the trampoline for the selection this hook replaces, which takes these slots.
    let original: Original = unsafe { core::mem::transmute(original) };
    let ret = unsafe { original(rcx, radius, r8, r9, a5, rng) };
    if deep {
        crate::log::guarded("growth deep edge", || {
            let Some((key, j, want)) = deep_place(false) else { return };
            if want {
                growth_file_line(&growth::edge_line(&key, j, before, deep_read_u32(rng)));
            }
        });
    }
    ret
}

/// Where the redirected `call`s lead after the hooks: the real `std::sort` (`0x966150`) and `RandomInt` (`0x3055870`);
/// 0 until published (before the call is patched).
static SORT_CALLEE: AtomicUsize = AtomicUsize::new(0);
static PICK_CALLEE: AtomicUsize = AtomicUsize::new(0);

/// [`deep_place`] without counting: the place the next gather or selection of this thread will be logged at.
fn deep_peek(is_cand: bool) -> Option<(growth::DeepKey, u32, bool)> {
    let pid = std::process::id();
    match deep_ctx().dev {
        Some(d) => {
            let j = if is_cand { d.cand_j } else { d.edge_j };
            Some((growth::DeepKey { pid, t: d.t, town: d.town, k: d.k }, j, d.want))
        }
        None => {
            let n = DEEP_ORPHANS.load(Ordering::Relaxed);
            (n < growth::ORPHAN_MAX).then_some((growth::DeepKey { pid, t: None, town: 0, k: 0 }, n as u32, true))
        }
    }
}

/// The bytes of a vector `[begin, end)` of elements of `size` bytes: `Ok(bytes)`, or `Err(count)` when it is too large to
/// hash or not readable (the count is 0 if the bounds make no sense).
fn deep_vector(begin: u64, end: u64, size: u64, max: usize) -> Result<Vec<u8>, u64> {
    if begin == end {
        return Ok(Vec::new());
    }
    let len = end.wrapping_sub(begin);
    if end < begin || !len.is_multiple_of(size) {
        return Err(0);
    }
    let n = len / size;
    if n as usize > max {
        return Err(n);
    }
    crate::memguard::read(begin as usize, len as usize).map_err(|_| n)
}

/// What is known before the sort.
struct SortPre {
    key: growth::DeepKey,
    j: u32,
    digest: growth::Digest,
    capped: bool,
}

/// Before `std::sort`: the records in the order they were gathered (positions only).
fn deep_sort_before(begin: u64, end: u64) -> Option<SortPre> {
    let (key, j, want) = deep_peek(true)?;
    if !want {
        return None;
    }
    let empty = growth::digest_records(&[]);
    Some(match deep_vector(begin, end, growth::RECORD_LEN as u64, growth::MAX_RECORDS) {
        Ok(bytes) => SortPre { key, j, digest: growth::digest_positions(&bytes), capped: false },
        Err(n) => SortPre { key, j, digest: growth::Digest { n, ..empty }, capped: true },
    })
}

/// After `std::sort`: the number of neighbours with exactly equal squared distance to the centre (`*(ctx+0x20)`, the
/// comparator's centre), then the line.
fn deep_sort_after(pre: SortPre, begin: u64, end: u64, ctx: u64) {
    let mut ties = None;
    if !pre.capped {
        // the centre is a pointer to two floats at `ctx+0x20`: read the pointer, then the floats
        let ptr = ctx.checked_add(0x20).and_then(|a| crate::memguard::word(a as usize));
        if let (Some(p), Ok(bytes)) = (ptr, deep_vector(begin, end, growth::RECORD_LEN as u64, growth::MAX_RECORDS)) {
            if let (Some(cx), Some(cy)) = (deep_read_u32(p), deep_read_u32(p.wrapping_add(4))) {
                let (cx, cy) = (f32::from_bits(cx), f32::from_bits(cy));
                let mut last: Option<f32> = None;
                let mut n = 0u64;
                for rec in bytes.as_chunks::<{ growth::RECORD_LEN }>().0 {
                    let x = f32::from_bits(u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]));
                    let y = f32::from_bits(u32::from_le_bytes([rec[8], rec[9], rec[10], rec[11]]));
                    let (dx, dy) = (x - cx, y - cy);
                    let d = dy * dy + dx * dx;
                    if last == Some(d) {
                        n += 1;
                    }
                    last = Some(d);
                }
                ties = Some(n);
            }
        }
    }
    growth_file_line(&growth::sort_line(&pre.key, pre.j, &pre.digest, ties, pre.capped));
}

/// What is known before the draw of the country edge.
struct PickPre {
    key: growth::DeepKey,
    j: u32,
    digest: growth::Digest,
    capped: bool,
    before: Option<u32>,
}

/// Before `RandomInt` at `0x95d627`: the id vector `[frame-0x40, frame-0x38)` of `0x95d100` and the rng state.
fn deep_pick_before(rng: u64, frame: u64) -> Option<PickPre> {
    let (key, j, want) = deep_peek(false)?;
    if !want {
        return None;
    }
    let before = deep_read_u32(rng);
    let empty = growth::digest_ids(&[]);
    let hdr = frame.checked_sub(0x40).and_then(|a| crate::memguard::read(a as usize, 16).ok());
    let Some(hdr) = hdr else {
        return Some(PickPre { key, j, digest: empty, capped: true, before });
    };
    let begin = u64::from_le_bytes(hdr[..8].try_into().unwrap_or([0; 8]));
    let end = u64::from_le_bytes(hdr[8..].try_into().unwrap_or([0; 8]));
    Some(match deep_vector(begin, end, 4, growth::MAX_RECORDS) {
        Ok(bytes) => PickPre { key, j, digest: growth::digest_ids(&bytes), capped: false, before },
        Err(n) => PickPre { key, j, digest: growth::Digest { n, ..empty }, capped: true, before },
    })
}

/// After the draw: the rng state and the index drawn.
fn deep_pick_after(pre: PickPre, rng: u64, idx: u32) {
    let after = deep_read_u32(rng);
    growth_file_line(&growth::pick_line(&pre.key, pre.j, &pre.digest, pre.before, after, idx, pre.capped));
}

/// The call `std::sort(begin, end, count, ctx)` at `0x967c16`, reached through a thunk the module placed at the call.
/// Logs the records as gathered, calls the real sort with every argument as given, then logs the ties. Changes nothing.
unsafe extern "system-unwind" fn sort_hook(begin: u64, end: u64, count: u64, ctx: u64) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::SORT_HOOK);
    let callee = SORT_CALLEE.load(Ordering::Acquire);
    if callee == 0 {
        return 0;
    }
    let pre = if GROWTH_DEEP.load(Ordering::Acquire) {
        crate::log::guarded("growth deep sort", || deep_sort_before(begin, end)).flatten()
    } else {
        None
    };
    type Original = unsafe extern "system-unwind" fn(u64, u64, u64, u64) -> u64;
    // SAFETY: the real function at the call's original target, which takes these four slots.
    let original: Original = unsafe { core::mem::transmute(callee) };
    let ret = unsafe { original(begin, end, count, ctx) };
    if let Some(pre) = pre {
        crate::log::guarded("growth deep sort", || deep_sort_after(pre, begin, end, ctx));
    }
    ret
}

/// The call `RandomInt(rng, 0, n)` at `0x95d627`, reached through a thunk that also passes the caller's frame pointer
/// (`rbp`, where the id vector lives) in `r9`, which the callee does not use. Logs the candidates, calls the real
/// function with `(rng, lo, n)` as given and logs the draw. Changes nothing.
unsafe extern "system-unwind" fn pick_hook(rng: u64, lo: u64, n: u64, frame: u64) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::PICK_HOOK);
    let callee = PICK_CALLEE.load(Ordering::Acquire);
    if callee == 0 {
        return 0;
    }
    let pre = if GROWTH_DEEP.load(Ordering::Acquire) {
        crate::log::guarded("growth deep pick", || deep_pick_before(rng, frame)).flatten()
    } else {
        None
    };
    type Original = unsafe extern "system-unwind" fn(u64, u64, u64) -> u64;
    // SAFETY: the real function at the call's original target, which takes these three slots.
    let original: Original = unsafe { core::mem::transmute(callee) };
    let ret = unsafe { original(rng, lo, n) };
    if let Some(pre) = pre {
        crate::log::guarded("growth deep pick", || deep_pick_after(pre, rng, ret as u32));
    }
    ret
}

/// Where the redirected river field lookup leads after [`field_hook`]: the real `RiverField::At` (`0x2b7a1b0`); 0 until published.
static FIELD_CALLEE: AtomicUsize = AtomicUsize::new(0);
/// Lookups outside any `Develop` that were logged (another caller of the open pass, a script's develop command).
static FIELD_ORPHANS: AtomicU64 = AtomicU64::new(0);
/// The field objects seen, in the order first seen: the line's `buf` (one field per engine, and the game runs two).
static FIELD_OBJECTS: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// Where a river field line belongs (as [`deep_place`], with its own counter `j`).
fn deep_place_field() -> Option<(growth::DeepKey, u32, bool)> {
    let pid = std::process::id();
    match deep_ctx().dev {
        Some(d) => {
            deep_ctx_update(|c| {
                if let Some(d) = c.dev.as_mut() {
                    d.field_j += 1;
                }
            });
            Some((growth::DeepKey { pid, t: d.t, town: d.town, k: d.k }, d.field_j, d.want))
        }
        None => {
            let n = FIELD_ORPHANS.fetch_add(1, Ordering::Relaxed);
            (n < growth::ORPHAN_MAX).then_some((growth::DeepKey { pid, t: None, town: 0, k: 0 }, n as u32, true))
        }
    }
}

/// The u64 at `addr` when the address is aligned and the bytes are committed and readable.
fn deep_read_u64(addr: u64) -> Option<u64> {
    crate::memguard::read_value::<u64>(usize::try_from(addr).ok()?)
}

/// The cache size of a river field (`std::map` at `+0x18`: head pointer, then the size at `+0x20`).
fn field_entries(field: u64) -> Option<u64> {
    deep_read_u64(field.checked_add(0x20)?)
}

/// After the lookup: the line, from the cache size before (`before`) and the field, point and answer as they are now.
fn deep_field(field: u64, point: u64, out: u64, before: Option<u64>) {
    let Some((key, j, want)) = deep_place_field() else { return };
    if !want {
        return;
    }
    let (Some(px), Some(py)) = (deep_read_u32(point), deep_read_u32(point.wrapping_add(4))) else { return };
    let entries = field_entries(field);
    let sources = match (deep_read_u64(field), deep_read_u64(field.wrapping_add(8))) {
        (Some(b), Some(e)) if e >= b && (e - b).is_multiple_of(growth::FIELD_SOURCE_LEN) => {
            Some((e - b) / growth::FIELD_SOURCE_LEN)
        }
        _ => None,
    };
    let mut answer = [0u32; 5];
    let mut read_all = true;
    for (i, slot) in answer.iter_mut().enumerate() {
        match deep_read_u32(out.wrapping_add(4 * i as u64)) {
            Some(v) => *slot = v,
            None => read_all = false,
        }
    }
    let buffer = {
        let mut seen = FIELD_OBJECTS.lock().unwrap_or_else(|p| p.into_inner());
        match seen.iter().position(|&f| f == field) {
            Some(i) => i,
            None => {
                seen.push(field);
                seen.len() - 1
            }
        }
    };
    let call = growth::FieldCall {
        point: (px, py),
        hit: growth::field_hit(before, entries),
        entries,
        sources,
        answer: read_all.then_some(answer),
        buffer,
    };
    growth_file_line(&growth::field_line(&key, j, &call));
}

/// The call `RiverField::At(field, out, point, flag)` at `0x96962e` (the street developer's open pass bends a new street's end
/// towards the river field), reached through a thunk the module placed at the call. Reads the cache size, calls the real `At` with
/// every register argument as given (it takes no stack argument), then logs the point, the answer and whether it came from the
/// cache. Changes nothing (TOWN_CACHE_20261019: the cache is per engine and not saved).
unsafe extern "system-unwind" fn field_hook(field: u64, out: u64, point: u64, flag: u64) -> u64 {
    // the only function the game jumps to that had no barrier (7337e4e): a panic here unwound into the street developer
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::FIELD_HOOK);
    let callee = FIELD_CALLEE.load(Ordering::Acquire);
    if callee == 0 {
        return out;
    }
    let deep = GROWTH_DEEP.load(Ordering::Acquire);
    let before = if deep { crate::log::guarded("growth deep field", || field_entries(field)).flatten() } else { None };
    type Original = unsafe extern "system-unwind" fn(u64, u64, u64, u64) -> u64;
    // SAFETY: the real function at the call's original target, which takes these four register slots.
    let original: Original = unsafe { core::mem::transmute(callee) };
    let ret = unsafe { original(field, out, point, flag) };
    if deep {
        crate::log::guarded("growth deep field", || deep_field(field, point, out, before));
    }
    ret
}

/// Where the redirected call of `CGame::Sync` (`0x11f690`) leads after [`pace_hook`]; 0 until published (before the call is patched).
static PACE_CALLEE: AtomicUsize = AtomicUsize::new(0);

/// The call `CGame::Sync(cgame, arg)` at `0x11f446` in `CGame::Step` (main thread, every frame), reached through a thunk the module
/// placed at the call (smooth pacing, `pace.rs`). Calls the real `Sync` with every register argument as given (the caller sets two;
/// `r8`/`r9` are passed through unchanged, and `Step` reserves no stack argument), and after a `Sync` that handed the simulation a
/// batch (`al` = 1) stretches the batch interval by the mod's factor. The real `Sync` runs in this plain frame, outside any guard
/// (log.rs THE RULE); only the module's own reads and the one write after it are guarded.
unsafe extern "system-unwind" fn pace_hook(cgame: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::PACE_HOOK);
    let callee = PACE_CALLEE.load(Ordering::Acquire);
    if callee == 0 {
        return 0;
    }
    type Original = unsafe extern "system-unwind" fn(u64, u64, u64, u64) -> u64;
    // SAFETY: the real function at the call's original target; it takes at most these four register slots.
    let original: Original = unsafe { core::mem::transmute(callee) };
    let ret = unsafe { original(cgame, arg2, arg3, arg4) };
    if ret & 0xFF != 0 {
        let now = crate::pace::clock_ms();
        // SAFETY: every address is checked with the OS before it is read or written (pace_after_sync).
        crate::log::guarded("batch pace", || unsafe { pace_after_sync(&crate::pace::PACER, cgame as usize, now) });
    }
    ret
}

/// Where the redirected free-id append leads after [`free_order_hook`]: the real `deque<int>::_Insert_range` (`0x2bb62a0`); 0 until
/// published (before the call is patched).
static FREE_ORDER_CALLEE: AtomicUsize = AtomicUsize::new(0);

/// The call `deque<int>::_Insert_range(queue, &ret, where, first, last)` at `0x2bba103` in `ecs::Engine::EndModification` (the
/// simulation's threads, once per modification), reached through a thunk the module placed at the call (`FEVERLINK3_FREE_ORDER`,
/// `freeorder.rs`). `[first, last)` is the modification's own vector of removed ids, read by nothing after this call: it is sorted
/// by entity id in place, then the real insert runs with every argument as given (four registers and the fifth on the stack, where
/// the thunk's plain jump leaves it). The insert allocates through the game's `operator new`, so it runs in this plain frame, outside
/// any guard (log.rs THE RULE); only the sort, which calls nothing of the game, is guarded.
unsafe extern "system-unwind" fn free_order_hook(queue: u64, ret: u64, at: u64, first: u64, last: u64) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::FREE_ORDER_HOOK);
    let callee = FREE_ORDER_CALLEE.load(Ordering::Acquire);
    if callee == 0 {
        return ret;
    }
    // SAFETY: nothing is written before the guarded reader read the whole range (sort_freed).
    crate::log::guarded("free order", || unsafe { sort_freed(first, last) });
    type Original = unsafe extern "system-unwind" fn(u64, u64, u64, u64, u64) -> u64;
    // SAFETY: the real function at the call's original target, which takes these five slots.
    let original: Original = unsafe { core::mem::transmute(callee) };
    let r = unsafe { original(queue, ret, at, first, last) };
    // the order trace takes the appended ids here, with this thread and step (ids.rs; read-only, guarded inside, nothing when off)
    crate::ids::freed(queue);
    r
}

/// Sorts the batch of freed ids `[first, last)` by entity id where it lies. A range that is not a readable run of ids is left as it
/// is and raises a stop fault: this game would then reuse ids in another order than the others for the rest of its run.
///
/// # Safety
/// Any range is allowed: it is checked (`freeorder::batch_len`) and read whole through the guarded reader before the sorted ids are
/// written back over it; a range the reader accepts is the engine's own heap vector, which is writable.
unsafe fn sort_freed(first: u64, last: u64) {
    let refuse = |why: String| {
        crate::failsafe::global(
            "free order",
            crate::failsafe::Action::Stop,
            format!("a batch of freed ids was not sorted ({why}): this game reuses ids in the engine's order, the others in id order"),
        )
    };
    let n = match crate::freeorder::batch_len(first, last) {
        Ok(n) => n,
        Err(why) => return refuse(why),
    };
    if n < 2 {
        crate::freeorder::STATS.note(n, false);
        return;
    }
    let bytes = match crate::memguard::read(first as usize, n * 4) {
        Ok(b) => b,
        Err(why) => return refuse(format!("{n} ids at {first:#x} not readable: {why:?}")),
    };
    let mut ids: Vec<i32> = bytes.as_chunks::<4>().0.iter().map(|c| i32::from_le_bytes(*c)).collect();
    let changed = crate::freeorder::canonical(&mut ids);
    if changed {
        // SAFETY: the n ids just read whole, 4-byte aligned (batch_len); the engine's vector on its heap.
        unsafe { core::ptr::copy_nonoverlapping(ids.as_ptr(), first as *mut i32, n) };
    }
    crate::freeorder::STATS.note(n, changed);
}

/// After a `Sync` that handed out a batch: the engine's interval at `[[cgame+0x1f0]+0x218]` (i32 us) is replaced by what the pacer
/// says (`pace.rs`). The layout is checked every time: a pointer or a field that is not there switches the piece off for good (logged
/// by the service thread's minute line), an implausible value is left alone this once. Calls nothing of the game.
///
/// # Safety
/// Any `cgame` is allowed: nothing is read or written before the OS said the range is committed and readable (and writable).
unsafe fn pace_after_sync(pacer: &crate::pace::Pacer, cgame: usize, now: u64) {
    use crate::pace::{ACC_OFFSET, DATA_OFFSET, INTERVAL_OFFSET, LAST_OFFSET};
    if pacer.idle(now) {
        return;
    }
    let Some(data_at) = cgame.checked_add(DATA_OFFSET).filter(|_| cgame != 0) else {
        return pacer.disable("Sync was called without a game object");
    };
    // SAFETY: the OS is asked first.
    if !unsafe { mem::readable(data_at as *const u8, 8) } {
        return pacer.disable("the game object's timing pointer is not readable");
    }
    let data = unsafe { core::ptr::read_unaligned(data_at as *const usize) };
    let fields_ok = data != 0
        && data & 7 == 0
        && data.checked_add(INTERVAL_OFFSET + 4).is_some()
        && unsafe { mem::readable((data + ACC_OFFSET) as *const u8, 16) }
        && unsafe { mem::writable((data + INTERVAL_OFFSET) as *const u8, 4) };
    if !fields_ok {
        return pacer.disable("the timing data is not at [game+0x1f0] as on build 40420");
    }
    // SAFETY: checked readable (and the interval writable) just above; the main thread is the one that runs Step and Sync.
    let acc = unsafe { core::ptr::read_unaligned((data + ACC_OFFSET) as *const i64) };
    let last = unsafe { core::ptr::read_unaligned((data + LAST_OFFSET) as *const i64) };
    let interval_at = (data + INTERVAL_OFFSET) as *mut i32;
    let engine = unsafe { core::ptr::read_volatile(interval_at) };
    if acc < last {
        return pacer.implausible(); // Step calls Sync only once the accumulated time passed the last batch: not the fields expected
    }
    if let Some(write) = pacer.decide(engine, now) {
        unsafe { core::ptr::write_volatile(interval_at, write) };
    }
}

/// The `TownUpdateSize` command handler `(this, payload) -> bool`: reads the game time the way the handler itself
/// does a few instructions later (`[[this+8]+0x28]` is the `GameTime` system, `0x2a96c0` the getter), keeps it for
/// `Develop` to see, and calls the original with the arguments as given. Changes nothing.
unsafe extern "system-unwind" fn growth_cmd_hook(this: u64, payload: u64) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::GROWTH_CMD_HOOK);
    let original = GROWTH_CMD_ORIGINAL.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    let _clear = ClearGrowthTime;
    let getter = GROWTH_GETTER.load(Ordering::Acquire);
    let mut time = None;
    if getter != 0 && this != 0 {
        // SAFETY: the handler's own first loads (`mov rax,[rbp+8]`, `mov rcx,[rax+0x28]`), with its `this`.
        let ctx = unsafe { core::ptr::read_unaligned((this + 8) as *const u64) };
        if ctx != 0 {
            let system = unsafe { core::ptr::read_unaligned((ctx + 0x28) as *const u64) };
            if system != 0 {
                // SAFETY: the address of the getter the planner checked; it takes the system and returns the dword.
                let get: unsafe extern "system-unwind" fn(u64) -> u32 = unsafe { core::mem::transmute(getter) };
                let t = unsafe { get(system) };
                GROWTH_TIME.store(TIME_VALID | u64::from(t), Ordering::Release);
                time = Some(t);
            }
        }
    }
    // the payload and the place of this command, for the lines of the calls inside it (own bookkeeping, contained)
    let _deep_cmd = if GROWTH_DEEP.load(Ordering::Acquire) {
        crate::log::guarded("growth deep payload", || deep_payload(payload, time)).flatten()
    } else {
        None
    };
    type Original = unsafe extern "system-unwind" fn(u64, u64) -> u64;
    // SAFETY: the trampoline for the handler this hook replaces.
    let original: Original = unsafe { core::mem::transmute(original) };
    unsafe { original(this, payload) }
}

/// The function `TownDeveloper::Develop` is made to jump to. The game's function is
/// `Develop(this, world, town entity (r8d), flag (r9b), u32* minstd state, arg 6, arg 7)`: the fifth argument is
/// `[rsp+0x28]` at entry, a pointer to the one 32-bit state of the generator, which the function copies to a local
/// and draws from through the pointer. Logs the call, in `derive` mode (after the switch, for a call whose game time
/// is known) overwrites the u32 behind the pointer, and calls the original with every slot as given.
///
/// The original's result is ignored by its only growth caller; it is forwarded as received.
unsafe extern "system-unwind" fn growth_hook(rcx: u64, rdx: u64, r8: u64, r9: u64, a5: u64, a6: u64, a7: u64) -> u64 {
    let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::GROWTH_HOOK);
    let original = GROWTH_ORIGINAL.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    let log = GROWTH_LOG.load(Ordering::Acquire);
    // a null or misaligned pointer is not the state we know: pass the call through untouched
    if log != 0 && a5 != 0 && a5.is_multiple_of(4) {
        let state = a5 as *mut u32;
        let town = r8 as u32;
        // SAFETY: the pointer the caller passes is its live generator state (a local of the command handler).
        let seed = unsafe { core::ptr::read_volatile(state) };
        let n = GROWTH_COUNTER.next();
        let packed = GROWTH_TIME.load(Ordering::Acquire);
        let time = (packed & TIME_VALID != 0).then_some(packed as u32);
        let settings = growth_settings();

        let mut announce = None;
        if settings.mode == Mode::Derive
            && !GROWTH_SWITCHED.load(Ordering::Acquire)
            && settings.switch_due(growth_wall_s(), time)
            && GROWTH_SWITCHED.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok()
        {
            announce = Some(growth::switch_line(n, growth_wall_s(), time, &settings));
        }
        let used = match time {
            Some(t) if settings.mode == Mode::Derive && GROWTH_SWITCHED.load(Ordering::Acquire) => {
                Some(growth::derive(town, t, growth::quantum_ticks(settings.quantum_ms)))
            }
            _ => None,
        };
        if let Some(u) = used {
            // SAFETY: as above; the original reads its draws through this pointer, so this is what it uses.
            unsafe { core::ptr::write_volatile(state, u) };
        }
        let traced = settings.trace.contains(town);
        let want_line = announce.is_some() || growth::sampled(n, town, seed);
        // from the hook's own frame, as the other hooks do (and only when a line is written: it walks the stack)
        let caller = if want_line || traced { hook_caller() } else { None };
        let caller = townseed::caller_field(caller, GROWTH_BASE.load(Ordering::Acquire), TOWN_SEED_IMAGE_BOUND);
        if let Some(a) = &announce {
            crate::log::guarded("growth hook", || {
                // SAFETY: stored by `arm_growth` from a `fn(&str)`.
                let log: fn(&str) = unsafe { core::mem::transmute(log) };
                log(a);
                growth_file_line(a);
            });
        }
        let deep = settings.deep;
        let _dev = if deep {
            crate::log::guarded("growth deep develop", || deep_dev_enter(town, seed, n, time))
        } else {
            None
        };
        type Original = unsafe extern "system-unwind" fn(u64, u64, u64, u64, u64, u64, u64) -> u64;
        // SAFETY: the trampoline for `Develop`, which takes these seven slots.
        let original: Original = unsafe { core::mem::transmute(original) };
        let ret = unsafe { original(rcx, rdx, r8, r9, a5, a6, a7) };
        if deep {
            // the state the original ended with: how many numbers it drew shows in it
            crate::log::guarded("growth deep develop", || deep_dev_exit(a5, used.unwrap_or(seed)));
        }
        if want_line || traced {
            // Our own bookkeeping, contained; the call into the game above is outside the guard. The line is written
            // after the call because it carries the return value.
            crate::log::guarded("growth hook", || {
                let call = growth::Call {
                    n,
                    pid: std::process::id(),
                    town,
                    seed,
                    time,
                    used,
                    r9b: r9 as u8,
                    a6,
                    a7,
                    ret,
                    caller: &caller,
                };
                let text = growth::line(&call);
                if want_line {
                    // SAFETY: stored by `arm_growth` from a `fn(&str)`.
                    let log: fn(&str) = unsafe { core::mem::transmute(log) };
                    log(&text);
                }
                growth_file_line(&text);
            });
        }
        return ret;
    }
    type Original = unsafe extern "system-unwind" fn(u64, u64, u64, u64, u64, u64, u64) -> u64;
    // SAFETY: the trampoline for `Develop`, which takes these seven slots.
    let original: Original = unsafe { core::mem::transmute(original) };
    unsafe { original(rcx, rdx, r8, r9, a5, a6, a7) }
}

/// The diagnostic hooks: logs and experiments that are not among the pieces the mod's `ready` event lists.
fn is_diagnostic(name: &str) -> bool {
    matches!(
        name,
        "town_seed"
            | "town_develop"
            | "town_update_size"
            | "town_candidates"
            | "town_edges"
            | "town_sort"
            | "town_pick"
            | "town_field"
    )
}

/// Where the jumps lead.
#[derive(Debug, Clone, Copy)]
pub struct Targets {
    pub apply: usize,
    pub add: usize,
    pub seed: usize,
    pub growth: usize,
    pub growth_cmd: usize,
    pub cands: usize,
    pub edges: usize,
    pub sort: usize,
    pub pick: usize,
    /// The call-site hook of the river field lookup ([`field_hook`], deep growth log).
    pub field: usize,
    pub convert: usize,
    pub convert_update: usize,
    pub preview: usize,
    /// The call-site hook of smooth pacing ([`pace_hook`]).
    pub pace: usize,
    /// The call-site hook of the free-id order ([`free_order_hook`]).
    pub free_order: usize,
    /// The call-site hook where the game interface makes its builder-preview listener ([`preview_owner_hook`]).
    pub preview_owner: usize,
}

/// A view of this process's image for the planner.
struct ProcessImage {
    base: usize,
}

impl Image for ProcessImage {
    fn read(&self, rva: Rva, len: usize) -> Option<Vec<u8>> {
        let at = self.base.checked_add(rva as usize)?;
        // SAFETY: the range is checked to be committed and readable before it is read.
        unsafe { mem::read(at as *const u8, len) }.ok()
    }
}

/// What an installation left in place.
#[derive(Default)]
pub struct Installed {
    detours: Vec<(&'static str, Detour)>,
    patch: Option<AppliedPatch>,
    /// Redirected `call` instructions (the deep growth log): the displacement was rewritten to lead to a thunk.
    call_patches: Vec<(&'static str, AppliedPatch)>,
    /// The pinned Lua string-hash seed.
    seed_patch: Option<AppliedPatch>,
    /// The tickCount getter that returns the step count.
    step_seed_patch: Option<AppliedPatch>,
}

impl Installed {
    pub fn hook_names(&self) -> Vec<&'static str> {
        self.detours.iter().map(|(n, _)| *n).chain(self.call_patches.iter().map(|(n, _)| *n)).collect()
    }

    pub fn poolpatch(&self) -> bool {
        self.patch.as_ref().is_some_and(AppliedPatch::is_applied)
    }

    pub fn lua_seed(&self) -> bool {
        self.seed_patch.as_ref().is_some_and(AppliedPatch::is_applied)
    }

    pub fn step_seed(&self) -> bool {
        self.step_seed_patch.as_ref().is_some_and(AppliedPatch::is_applied)
    }

    /// The free-id append in `EndModification` leads to the sort.
    pub fn free_order(&self) -> bool {
        self.call_patches.iter().any(|(n, p)| *n == crate::freeorder::HOOK_NAME && p.is_applied())
    }

    pub fn is_empty(&self) -> bool {
        self.detours.is_empty()
            && self.patch.is_none()
            && self.call_patches.is_empty()
            && self.seed_patch.is_none()
            && self.step_seed_patch.is_none()
    }

    /// Takes everything out again, newest first: the jumps, then the patch.
    ///
    /// For the moment right after installing (a later piece failed) and for tests. A running game must not have
    /// its code rewritten under it; see the module documentation.
    ///
    /// # Safety
    /// No thread may be inside the first bytes of a hooked function or inside the patched region.
    pub unsafe fn uninstall_all(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        while let Some((name, mut p)) = self.call_patches.pop() {
            match unsafe { p.restore() } {
                Ok(()) => lines.push(format!("hooks: {name}: call restored, original bytes back")),
                Err(e) => lines.push(format!("hooks: {name}: call NOT restored: {e}")),
            }
        }
        while let Some((name, mut d)) = self.detours.pop() {
            match unsafe { d.uninstall() } {
                Ok(()) => lines.push(format!("hooks: {name}: jump removed, original bytes restored")),
                Err(e) => lines.push(format!("hooks: {name}: jump NOT removed: {e}")),
            }
        }
        if let Some(mut p) = self.step_seed_patch.take() {
            match unsafe { p.restore() } {
                Ok(()) => lines.push("hooks: tick_count_getter: original bytes restored".to_string()),
                Err(e) => lines.push(format!("hooks: tick_count_getter: original bytes NOT restored: {e}")),
            }
        }
        if let Some(mut p) = self.seed_patch.take() {
            match unsafe { p.restore() } {
                Ok(()) => lines.push("hooks: lua_makeseed: original bytes restored".to_string()),
                Err(e) => lines.push(format!("hooks: lua_makeseed: original bytes NOT restored: {e}")),
            }
        }
        if let Some(mut p) = self.patch.take() {
            match unsafe { p.restore() } {
                Ok(()) => lines.push("hooks: script_pool: original bytes restored".to_string()),
                Err(e) => lines.push(format!("hooks: script_pool: original bytes NOT restored: {e}")),
            }
        }
        lines
    }
}

/// A point in [`execute`] that a test can act at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Every detour is prepared and the trampolines are published; no jump is written yet.
    Prepared,
}

/// Carries a plan out. Returns what is in place and the lines for the log.
///
/// # Safety
/// `base` is the address of the image the plan was made for, and no thread is executing inside the functions or
/// the region to be rewritten (the module calls this from `DllMain`).
pub unsafe fn execute(
    plan: &Plan,
    base: usize,
    rt: &Runtime,
    targets: &Targets,
    observer: &mut dyn FnMut(Stage),
) -> (Installed, Vec<String>) {
    let mut lines = Vec::new();
    let mut inst = Installed::default();

    // 1. the pool patch, independent of the detours
    for step in &plan.steps {
        if let Step::Patch { feature, name, rva, expected, replacement } = step {
            let at = (base + *rva as usize) as *mut u8;
            match unsafe { AppliedPatch::apply(at, expected, replacement) } {
                Ok(p) if *feature == Feature::LuaSeed => {
                    lines.push(format!(
                        "lua seed pinned to {:#010x} (lua_makeseed: {} bytes at rva {rva:#x}, page protection {})",
                        crate::build_table::LUA_SEED,
                        expected.len(),
                        if p.protection_restored() { "restored" } else { "NOT restored" }
                    ));
                    inst.seed_patch = Some(p);
                }
                Ok(p) if *feature == Feature::StepSeed => {
                    lines.push(format!(
                        "simulation seeds read the step count: tickCount getter returns updateCount (tick_count_getter: {} bytes at rva {rva:#x}, page protection {})",
                        expected.len(),
                        if p.protection_restored() { "restored" } else { "NOT restored" }
                    ));
                    inst.step_seed_patch = Some(p);
                }
                Ok(p) => {
                    lines.push(format!(
                        "hooks: {name}: applied, {} bytes at rva {rva:#x}, page protection {}",
                        expected.len(),
                        if p.protection_restored() { "restored" } else { "NOT restored" }
                    ));
                    inst.patch = Some(p);
                }
                Err(e) if *feature == Feature::LuaSeed => {
                    lines.push(format!("lua seed NOT pinned, the game keeps its own seed: {e}"))
                }
                Err(e) if *feature == Feature::StepSeed => {
                    lines.push(format!("simulation seeds NOT moved to the step count, tickCount as shipped: {e}"))
                }
                Err(e) => lines.push(format!("hooks: {name}: NOT applied: {e}")),
            }
        }
    }

    // 2. prepare every detour: nothing is written to the game yet
    let mut prepared: Vec<(&'static str, usize, Detour)> = Vec::new();
    for step in &plan.steps {
        if let Step::Detour { feature, name, rva, expected } = step {
            let at = (base + *rva as usize) as *mut u8;
            let hook = match feature {
                Feature::Counters => targets.apply,
                Feature::TownSeed => targets.seed,
                Feature::Growth => targets.growth,
                Feature::GrowthCmd => targets.growth_cmd,
                Feature::GrowthCands => targets.cands,
                Feature::GrowthEdges => targets.edges,
                Feature::Convert => targets.convert,
                Feature::ConvertUpdate => targets.convert_update,
                Feature::PreviewEvent => targets.preview,
                _ => targets.add,
            };
            match unsafe { Detour::prepare(at, expected) } {
                Ok(d) => prepared.push((name, hook, d)),
                Err(e) => lines.push(format!("hooks: {name}: NOT installed, prologue check failed: {e}")),
            }
        }
    }

    // 3. publish the trampolines before any jump exists
    let original_of = |wanted: &str| {
        prepared.iter().find(|(n, _, _)| *n == wanted).map(|(_, _, d)| d.original() as usize).unwrap_or(0)
    };
    rt.set_original_addresses(original_of("command_list_add"), original_of("apply_command"));
    TOWN_SEED_ORIGINAL.store(original_of("town_seed"), Ordering::Release);
    GROWTH_ORIGINAL.store(original_of("town_develop"), Ordering::Release);
    GROWTH_CMD_ORIGINAL.store(original_of("town_update_size"), Ordering::Release);
    CANDS_ORIGINAL.store(original_of("town_candidates"), Ordering::Release);
    EDGES_ORIGINAL.store(original_of("town_edges"), Ordering::Release);
    CONVERT_ORIGINAL.store(original_of("script_convert"), Ordering::Release);
    CONVERT_UPDATE_ORIGINAL.store(original_of("script_convert_update"), Ordering::Release);
    PREVIEW_ORIGINAL.store(original_of("preview_event"), Ordering::Release);
    rt.set_preview_original(original_of("preview_event"));
    observer(Stage::Prepared);

    // 4. write the jumps; if one fails, take the others out again
    let mut written: Vec<(&'static str, Detour)> = Vec::new();
    let mut failure = None;
    for (name, hook, mut d) in prepared {
        match unsafe { d.commit(hook) } {
            Ok(()) => {
                lines.push(format!("hooks: {name}: jump written, original reachable at {:#x}", d.original() as usize));
                written.push((name, d));
            }
            Err(e) => {
                failure = Some(format!("hooks: {name}: jump NOT written: {e}"));
                break;
            }
        }
    }
    if let Some(why) = failure {
        lines.push(why);
        let mut undo = Installed { detours: written, ..Default::default() };
        lines.push("hooks: rolling back the jumps already written".to_string());
        lines.extend(unsafe { undo.uninstall_all() });
    } else {
        inst.detours = written;
        // 5. the call-site redirects (the deep log, read-only; smooth pacing): each independent, a failure costs only that one
        for step in &plan.steps {
            let Step::CallPatch { feature, name, rva, expected, callee } = step else { continue };
            let (hook, prefix): (usize, &[u8]) = match feature {
                Feature::GrowthSort => (targets.sort, &[]),
                Feature::BatchPace => (targets.pace, &[]),
                Feature::FreeOrder => (targets.free_order, &[]),
                Feature::PreviewOwner => (targets.preview_owner, &[]),
                Feature::GrowthField => (targets.field, &[]),
                // `mov r9, rbp`: the caller's frame pointer as a fourth argument the callee ignores
                _ => (targets.pick, &[0x4C, 0x8B, 0xCD]),
            };
            let callee_addr = base + *callee as usize;
            match feature {
                Feature::GrowthSort => SORT_CALLEE.store(callee_addr, Ordering::Release),
                Feature::BatchPace => PACE_CALLEE.store(callee_addr, Ordering::Release),
                Feature::FreeOrder => FREE_ORDER_CALLEE.store(callee_addr, Ordering::Release),
                Feature::PreviewOwner => PREVIEW_OWNER_CALLEE.store(callee_addr, Ordering::Release),
                Feature::GrowthField => FIELD_CALLEE.store(callee_addr, Ordering::Release),
                _ => PICK_CALLEE.store(callee_addr, Ordering::Release),
            }
            let mut thunk = prefix.to_vec();
            thunk.extend_from_slice(&crate::detour::jmp_abs(hook as u64));
            let at = base + *rva as usize;
            // SAFETY: executable memory with absolute jumps only; the thunk is never freed.
            let thunk_at = match unsafe { mem::alloc_exec_near(at, &thunk) } {
                Ok(p) => p as usize,
                Err(e) => {
                    lines.push(format!("hooks: {name}: NOT installed, no executable memory near the call: {e:?}"));
                    continue;
                }
            };
            let Ok(rel) = i32::try_from(thunk_at as i64 - (at as i64 + 5)) else {
                lines.push(format!("hooks: {name}: NOT installed, the thunk is out of reach of a rel32 call"));
                continue;
            };
            let mut replacement = vec![0xE8];
            replacement.extend_from_slice(&rel.to_le_bytes());
            match unsafe { AppliedPatch::apply(at as *mut u8, expected, &replacement) } {
                Ok(p) => {
                    lines.push(format!("hooks: {name}: call at rva {rva:#x} redirected to a thunk at {thunk_at:#x}"));
                    if *feature == Feature::FreeOrder {
                        lines.push(format!(
                            "free-id order: freed ids join the free-id queue sorted by entity id (EndModification's append, call at rva {rva:#x}; {}=0 off, must match on every game)",
                            crate::freeorder::FREE_ORDER_VAR
                        ));
                    }
                    inst.call_patches.push((name, p));
                }
                Err(e) => lines.push(format!("hooks: {name}: NOT installed, the call was not patched: {e}")),
            }
        }
    }
    (inst, lines)
}

/// Where the module reads the control file and writes the events.
#[derive(Debug, Clone)]
pub struct Link {
    pub dir: PathBuf,
    /// The folder belongs to the in-process session controller, which clears it when it starts.
    pub live: bool,
}

/// Plans the installation on this process's image and drops what the start-up self-test does not vouch for.
pub fn plan_for_process(build: &BuildAddrs, cfg: HookConfig, base: usize, log: fn(&str)) -> Plan {
    let mut p = plan::plan(build, cfg, &ProcessImage { base });
    if p.steps.iter().any(|s| matches!(s, Step::Detour { .. })) {
        #[cfg(target_arch = "x86_64")]
        apply_selftest(&mut p, &selftest::run(), log);
    }
    p
}

/// Takes out of the plan what a failed start-up self-test does not vouch for: every detour if the machinery
/// itself failed, the hold hook if only the call-site identification failed. The pool patch needs neither.
#[cfg(target_arch = "x86_64")]
fn apply_selftest(p: &mut Plan, report: &selftest::Report, log: fn(&str)) {
    let mut drop_all = false;
    let mut drop_hold = false;
    match &report.mechanics {
        Ok(()) => log("hooks: self-test: detour, trampoline and stack argument work in this binary"),
        Err(e) => {
            log(&format!("hooks: self-test FAILED, no jump will be written: {e}"));
            drop_all = true;
        }
    }
    match &report.caller {
        Ok(()) => log("hooks: self-test: the hook identifies the call site of a detoured function"),
        Err(e) => {
            log(&format!("hooks: self-test: the call site was not identified, hold hook off: {e}"));
            drop_hold = true;
        }
    }
    let why = |f: Feature| match f {
        Feature::Hold => "start-up self-test of caller identification failed",
        _ => "start-up self-test of the detour machinery failed",
    };
    let mut skipped = Vec::new();
    p.steps.retain(|s| {
        let remove = matches!(s, Step::Detour { feature, .. } if drop_all || (drop_hold && matches!(feature, Feature::Hold | Feature::PreviewEvent)))
            || matches!(s, Step::CallPatch { feature, .. } if drop_all || (drop_hold && *feature == Feature::PreviewOwner));
        if remove {
            skipped.push(Skip { feature: s.feature(), name: s.name(), reason: why(s.feature()).to_string() });
        }
        !remove
    });
    p.skipped.extend(skipped);
    if !p.has(Feature::Hold) {
        p.engine = None;
    }
}

/// Reads the switches from the environment and installs what they ask for.
pub fn attach_from_env(build: &'static BuildAddrs, link: Option<Link>, log: fn(&str)) {
    let get = |name: &str| std::env::var(name).ok();
    if let Err(reason) = plan::forced_seed(get) {
        log(&format!("hooks: {}: ignored, {reason}", plan::FORCE_TOWN_SEED_VAR));
    }
    for complaint in growth::Settings::from_lookup(get).1 {
        log(&format!("hooks: {complaint}"));
    }
    let cfg = HookConfig::from_lookup(get);
    if !cfg.luaseed && std::env::var(plan::LUA_SEED_LOG_VAR).is_ok_and(|v| v.trim() == "1") {
        log("lua seed: not pinned (FEVERLINK3_LUA_SEED=0), the game keeps its own per-process seed");
    }
    attach(build, cfg, link, log);
}

/// Installs what `cfg` asks for on this build, checks it, and starts the service thread. Never panics and never
/// leaves a half-installed state: anything in doubt is logged and skipped.
pub fn attach(build: &'static BuildAddrs, cfg: HookConfig, link: Option<Link>, log: fn(&str)) {
    if !cfg.any() {
        return;
    }
    // SAFETY: the base of this process's own executable.
    let base = unsafe { GetModuleHandleW(core::ptr::null()) } as usize;
    if base == 0 {
        log("hooks: cannot find the game's image: nothing installed");
        return;
    }
    // diagnostic, off unless FEVERLINK3_ID_TRACE=1
    let ids_dir = link.as_ref().map(|l| l.dir.clone()).or_else(feverlink3_launcher::live::live_dir);
    crate::ids::install(build, base, ids_dir, log);
    attach_at(base, build, cfg, link, log);
}

/// [`attach`] for an image at `base`.
fn attach_at(base: usize, build: &'static BuildAddrs, cfg: HookConfig, link: Option<Link>, log: fn(&str)) {
    let since = SystemTime::now();
    log(&format!("hooks: switches {}", cfg.describe()));
    let plan = plan_for_process(build, cfg, base, log);
    for l in plan.report() {
        log(&l);
    }
    if plan.steps.is_empty() {
        missing_patches_fail_loud(&cfg, None);
        log("hooks: nothing to install");
        return;
    }

    let engine = match (plan.has(Feature::Hold), plan.engine, plan.kind_offset) {
        // SAFETY: the plan verified the prologues of the three helpers against the image.
        (true, Some(e), Some(k)) => {
            let mut fns = unsafe { EngineFns::at(base, e.command_move, e.command_destroy, e.handle_destroy, k) };
            if cfg.fire_done {
                let calls: Vec<usize> = build.fire_done_calls.iter().map(|&r| base + r as usize).collect();
                fns.fire_calls = Box::leak(calls.into_boxed_slice());
                fns.fire_done = !fns.fire_calls.is_empty();
                log(&format!(
                    "hooks: fire_done: {} (tool callbacks that may be fired: {})",
                    if fns.fire_done { "on" } else { "off: this build has no verified callback" },
                    build.fire_done_calls.len()
                ));
            }
            Some(fns)
        }
        _ => None,
    };
    // the town-seed log is not one of the pieces the mod's `ready` event lists
    let planned: Vec<&'static str> = plan
        .steps
        .iter()
        .filter(|s| matches!(s, Step::Detour { .. }) && !is_diagnostic(s.name()))
        .map(Step::name)
        .collect();
    let rt = Runtime::new(Init {
        build,
        base,
        hold: HoldConfig::default(),
        engine,
        log,
        hooks: planned,
        poolpatch: false,
        version: env!("CARGO_PKG_VERSION"),
    });
    if RUNTIME.set(rt).is_err() {
        log("hooks: already attached");
        return;
    }
    let Some(rt) = RUNTIME.get() else { return };
    // dev-only (FEVERLINK3_DUMP_COMMANDS=1|2): decode the build commands the module holds and the simulation applies
    if let Some(settings) = crate::dump::Settings::from_lookup(|n| std::env::var(n).ok()) {
        let dir = link.as_ref().map(|l| l.dir.clone()).or_else(feverlink3_launcher::live::live_dir);
        rt.enable_dump(settings, dir.as_deref());
    }
    if let Some(settings) = crate::portable::Settings::from_lookup(|n| std::env::var(n).ok()) {
        let dir = link.as_ref().map(|l| l.dir.clone()).or_else(feverlink3_launcher::live::live_dir);
        rt.enable_portable(settings, dir.as_deref());
    }
    if let Some(settings) = crate::portable::Settings::from_var(|n| std::env::var(n).ok(), crate::rebuild::REBUILD_VAR)
    {
        let dir = link.as_ref().map(|l| l.dir.clone()).or_else(feverlink3_launcher::live::live_dir);
        rt.enable_rebuild(settings, dir.as_deref());
    }
    if let Some(settings) =
        crate::portable::Settings::from_var(|n| std::env::var(n).ok(), crate::native_copy::NATIVE_COPY_VAR)
    {
        let dir = link.as_ref().map(|l| l.dir.clone()).or_else(feverlink3_launcher::live::live_dir);
        rt.enable_native_copy(settings, dir.as_deref());
    }
    if std::env::var(crate::cmd_patch::PATCH_VAR).is_ok_and(|v| v == "1") {
        rt.enable_native_patch();
    }
    // the native copy is the default build path (owner, 2026-10-14): unset means on, `FEVERLINK3_NATIVE_INJECT=0` switches it off
    if let Some(settings) = crate::portable::Settings::from_var(
        |n| match std::env::var(n) {
            Ok(v) => Some(v),
            Err(_) if n == crate::inject::INJECT_VAR => Some("1".to_string()),
            Err(_) => None,
        },
        crate::inject::INJECT_VAR,
    ) {
        rt.enable_native_inject(settings);
    }
    let targets = Targets {
        apply: apply_hook as *const () as usize,
        add: add_hook as *const () as usize,
        seed: town_seed_hook as *const () as usize,
        growth: growth_hook as *const () as usize,
        growth_cmd: growth_cmd_hook as *const () as usize,
        cands: candidates_hook as *const () as usize,
        edges: edges_hook as *const () as usize,
        sort: sort_hook as *const () as usize,
        pick: pick_hook as *const () as usize,
        field: field_hook as *const () as usize,
        convert: convert_hook as *const () as usize,
        convert_update: convert_update_hook as *const () as usize,
        preview: preview_hook as *const () as usize,
        pace: pace_hook as *const () as usize,
        free_order: free_order_hook as *const () as usize,
        preview_owner: preview_owner_hook as *const () as usize,
    };
    if plan.has(Feature::TownSeed) {
        arm_town_seed(base, log, cfg.force_seed);
    }
    if plan.has(Feature::Growth) {
        let getter =
            if plan.has(Feature::GrowthCmd) { plan.game_time_getter.map_or(0, |r| base + r as usize) } else { 0 };
        // the game's own live folder: per game, unlike native.log
        let dir = link.as_ref().map(|l| l.dir.clone()).or_else(feverlink3_launcher::live::live_dir);
        arm_growth(base, log, &cfg.growth, getter, dir);
    }
    // SAFETY: called from DllMain, before the game runs the functions being patched.
    let (installed, lines) = unsafe { execute(&plan, base, rt, &targets, &mut |_| {}) };
    for l in lines {
        log(&l);
    }
    missing_patches_fail_loud(&cfg, Some(&installed));
    let mod_hooks: Vec<&'static str> = installed.hook_names().into_iter().filter(|n| !is_diagnostic(n)).collect();
    let seed_logged = installed.hook_names().contains(&"town_seed");
    let growth_logged = installed.hook_names().contains(&"town_develop");
    let time_read = installed.hook_names().contains(&"town_update_size");
    let cands_on = installed.hook_names().contains(&"town_candidates");
    let edges_on = installed.hook_names().contains(&"town_edges");
    let sort_on = installed.hook_names().contains(&"town_sort");
    let pick_on = installed.hook_names().contains(&"town_pick");
    let field_on = installed.hook_names().contains(&"town_field");
    rt.set_installed(mod_hooks.clone(), installed.poolpatch());
    let anything = !installed.is_empty();
    let hooked = !mod_hooks.is_empty();
    let mod_pieces = hooked || installed.poolpatch();
    if let Ok(mut slot) = INSTALLED.lock() {
        *slot = Some(installed);
    }
    if !anything {
        log("hooks: nothing is installed");
        return;
    }
    pin_module();
    if seed_logged {
        log("hooks: town_seed: MAP GENERATION seed log (not town growth): one line per call of the location factory's constructor as 'mapgen-seed n=.. seed=.. caller=..'");
        if let Some(v) = cfg.force_seed {
            log(&format!("hooks: town_seed: EXPERIMENT: replacing the seed argument with {v:#010x} on every call"));
        } else {
            log("hooks: town_seed: read-only");
        }
    }
    if growth_logged {
        let g = &cfg.growth;
        log("hooks: town_develop: TOWN GROWTH log: 'growth n=.. pid=.. town=.. seed=.. [t=<tick of 200 ms>] [used=..] r9b=.. a6=.. a7=.. ret=.. caller=..' (first 2000 calls, then about 1 in 50; every call of the towns in FEVERLINK3_TOWN_TRACE); also appended to growth.log in this game's live folder");
        if g.deep {
            log(&format!(
                "hooks: town_develop: DEEP growth log (FEVERLINK3_LOG_GROWTH_DEEP): 'gdeep-payload|dev|cand|edge|field ...' lines in growth.log only (payload: {}, rng end state: yes, candidates: {}, edge rng: {}, sort input: {}, edge pick: {}, river field cache: {}); read-only",
                if time_read { "yes" } else { "NO, the TownUpdateSize hook is missing" },
                if cands_on { "yes" } else { "NO, not installed" },
                if edges_on { "yes" } else { "NO, not installed" },
                if sort_on { "yes" } else { "NO, not installed" },
                if pick_on { "yes" } else { "NO, not installed" },
                if field_on { "yes" } else { "NO, not installed" },
            ));
        }
        if !time_read {
            log(
                "hooks: town_develop: the game time cannot be read on this build: no t= field, and no seed replacement",
            );
        }
        match g.mode {
            Mode::Derive => log(&format!(
                "hooks: town_develop: EXPERIMENT growth seed mode derive: after wall>={}s and game>={} the seed Develop reads is replaced by derive(town, game time rounded down to {} ms); only which random numbers are drawn changes",
                g.after_wall_s,
                g.after_game_s.map_or("none".to_string(), |v| format!("{v}s")),
                g.quantum_ms
            )),
            Mode::Off => log("hooks: town_develop: read-only"),
        }
    }
    if !mod_pieces {
        // only the diagnostic hooks: nothing for the control link to do
        log("hooks: installed (diagnostic logs only; nothing else in the game is changed, no control link)");
        return;
    }
    log(&installed_line(hooked));
    if let Some(link) = link {
        spawn_service(rt, link, since, log);
    } else {
        log("hooks: no session folder: counters only in memory, no control link");
    }
}

/// Fail loud (`failsafe.rs`): a determinism patch the switches ask for (each is on unless its variable says 0, and must match on every
/// game) that is not in place makes this game run as shipped where the others do not, for the whole run: a stop fault, which the mod
/// shows the player and the host's repair cannot cure. Before 2026-10-19 this was one log line ("NOT applied") and the session went on.
fn missing_patches_fail_loud(cfg: &HookConfig, installed: Option<&Installed>) {
    let pieces = [
        ("script pool patch", cfg.poolpatch, installed.is_some_and(Installed::poolpatch)),
        ("lua seed patch", cfg.luaseed, installed.is_some_and(Installed::lua_seed)),
        ("step seed patch", cfg.step_seed, installed.is_some_and(Installed::step_seed)),
        ("free order patch", cfg.free_order, installed.is_some_and(Installed::free_order)),
    ];
    for name in crate::failsafe::missing_patches(&pieces) {
        crate::failsafe::global(
            name,
            crate::failsafe::Action::Stop,
            "asked for by the switches but NOT in place (the lines above say why): this game runs as shipped where the others do \
             not; leave the session and restart the game",
        );
    }
}

/// The line that says the installation is done.
fn installed_line(jumps: bool) -> String {
    if jumps {
        "hooks: installed (jumps in place; they pass everything through until the mod switches holding on)".into()
    } else {
        "hooks: installed (patch only)".into()
    }
}

/// Keeps this DLL loaded for the life of the process: the hooks point into it, so it must never be unloaded.
fn pin_module() {
    let mut module = core::ptr::null_mut();
    let anchor = add_hook as *const () as *const u16;
    // SAFETY: FROM_ADDRESS with the address of a function in this DLL; PIN makes the load count permanent.
    let ok = unsafe {
        GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN, anchor, &mut module)
    };
    if ok == 0 {
        crate::log::line("hooks: could not pin the module in memory");
    }
}

/// True when `path` exists and was last written no earlier than a second before `since`.
fn written_since(path: &Path, since: SystemTime) -> bool {
    std::fs::metadata(path).and_then(|m| m.modified()).is_ok_and(|t| t + Duration::from_secs(1) >= since)
}

/// Waits until `path` has been written at or after `since`; `false` if `timeout` passes first.
fn wait_until_written(path: &Path, since: SystemTime, timeout: Duration, poll: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if written_since(path, since) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(poll);
    }
}

/// The `hookwatch` dump, when due: every 5 s until the simulation applied its first command (labs L/L2 of 2026-10-19, a game that
/// hung while loading with the deep growth log on). Written without the log's lock.
fn hookwatch_tick() {
    let started = crate::hookwatch::sim_started();
    if let Some(l) = crate::hookwatch::SCHEDULE.due(crate::pace::clock_ms(), started, &crate::hookwatch::ALL) {
        crate::log::line_unlocked(&l);
    }
}

/// The service thread: opens the control link once the session folder is ready, then pumps every 20 ms.
fn spawn_service(rt: &'static Runtime, link: Link, since: SystemTime, log: fn(&str)) {
    let spawned = std::thread::Builder::new().name("feverlink3-native".into()).spawn(move || {
        if link.live {
            // the in-process controller clears the folder when it starts and then writes identity.txt: wait for
            // a file newer than this process, or the first events would be deleted under us
            let identity = link.dir.join("identity.txt");
            let deadline = Instant::now() + Duration::from_secs(120);
            let mut announced = false;
            while !announced && Instant::now() < deadline {
                hookwatch_tick();
                announced = wait_until_written(&identity, since, Duration::from_secs(1), Duration::from_millis(100));
            }
            if !announced {
                log("link: the session controller did not announce itself within 120 s; opening the folder anyway");
            }
        }
        if let Err(e) = rt.open_link(&link.dir) {
            log(&format!("link: cannot open {}: {e}", link.dir.display()));
            // no link, but the dump still names a hook a game thread is stuck in
            while !crate::hookwatch::sim_started() {
                hookwatch_tick();
                std::thread::sleep(Duration::from_secs(1));
            }
            hookwatch_tick();
            return;
        }
        loop {
            // before the round: a round that waits on something a stuck hook holds must not silence the dump that names it
            hookwatch_tick();
            // a failure in one round (a panic, under `unwind`) costs that round, not the link
            rt.pump_round();
            std::thread::park_timeout(Duration::from_millis(20));
        }
    });
    match spawned {
        Ok(handle) => rt.set_waker(handle.thread().clone()),
        Err(e) => log(&format!("link: cannot start the service thread: {e}")),
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use crate::build_table::{CallSite, HookSite, PatchSite};
    use crate::growth::{Mode, Settings};
    use crate::mem::pages::Pages;
    use crate::selftest::{SUM5, SUM5_PROLOGUE};
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
    use windows_sys::Win32::System::Memory::PAGE_EXECUTE_READ;

    /// `fn(a, b) -> a + b + 5`, a 14-byte prologue: stands in for the apply function.
    const SUM: [u8; 16] =
        [0x55, 0x48, 0x89, 0xE5, 0x48, 0x89, 0xC8, 0x48, 0x01, 0xD0, 0x48, 0x83, 0xC0, 0x05, 0x5D, 0xC3];
    const APPLY_RVA: u32 = 0x100;
    const ADD_RVA: u32 = 0x200;
    const POOL_RVA: u32 = 0x300;
    const MOVE_RVA: u32 = 0x400;
    const SEED_RVA: u32 = 0x600;
    /// A fake of the town task's seeding function: `fn(a1..a12) -> a1 + a5 + a9` in 32 bits, reading its ninth
    /// argument from the stack as the real one does: `mov eax,[rsp+48h] ; add eax,[rsp+28h] ; add eax,ecx ; nop4 ; ret`.
    const SEED_FN: [u8; 15] =
        [0x8B, 0x44, 0x24, 0x48, 0x03, 0x44, 0x24, 0x28, 0x01, 0xC8, 0x90, 0x90, 0x90, 0x90, 0xC3];
    const LUASEED_RVA: u32 = 0x380;
    const SEED_BEFORE: &[u8] = &[0x40, 0x53, 0x48, 0x83, 0xEC, 0x50, 0x48, 0x8B, 0x05, 1, 2, 3, 4, 0x48];
    static SEED_AFTER: [u8; 14] = crate::build_table::lua_seed_patch(crate::build_table::LUA_SEED);
    const POOL_BEFORE: &[u8] = &[0x83, 0xF8, 0x08, 0x0F, 0x8D, 0x25, 0x01, 0x00];
    const TICK_RVA: u32 = 0x3c0;
    /// A fake tickCount getter over a fake component in rcx: `mov eax,[rcx+3Ch] ; ret` (the real one looks the component up first).
    const TICK_BEFORE: &[u8] = &[0x8B, 0x41, 0x3C, 0xC3];
    const TICK_AFTER: &[u8] = &[0x8B, 0x41, 0x40, 0xC3];
    const POOL_AFTER: &[u8] = &[0x83, 0xF8, 0x08, 0x66, 0x0F, 0x1F, 0x44, 0x00];

    fn fake_build() -> &'static BuildAddrs {
        static APPLY: HookSite = HookSite { rva: APPLY_RVA, prologue: &SUM };
        let add: &'static HookSite = Box::leak(Box::new(HookSite { rva: ADD_RVA, prologue: &SUM5[..SUM5_PROLOGUE] }));
        let helper: &'static CallSite = Box::leak(Box::new(CallSite {
            rva: MOVE_RVA,
            prologue: &[0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97],
        }));
        let pool: &'static PatchSite =
            Box::leak(Box::new(PatchSite { rva: POOL_RVA, expected: POOL_BEFORE, replacement: POOL_AFTER }));
        let seed: &'static PatchSite =
            Box::leak(Box::new(PatchSite { rva: LUASEED_RVA, expected: SEED_BEFORE, replacement: &SEED_AFTER }));
        let tick: &'static PatchSite =
            Box::leak(Box::new(PatchSite { rva: TICK_RVA, expected: TICK_BEFORE, replacement: TICK_AFTER }));
        Box::leak(Box::new(BuildAddrs {
            timestamp: 9,
            name: "fake",
            command_list_add: Some(add.clone()),
            apply_command: Some(APPLY.clone()),
            town_seed: Some(HookSite { rva: SEED_RVA, prologue: &SEED_FN[..14] }),
            town_develop: Some(HookSite { rva: DEVELOP_RVA, prologue: &DEVELOP_FN[..15] }),
            town_update_size: Some(HookSite {
                rva: CMD_RVA,
                prologue: Box::leak(handler_fn().to_vec().into_boxed_slice()),
            }),
            script_convert: None,
            convert_update: None,
            preview_event: None,
            preview_owner: None,
            town_candidates: Some(HookSite {
                rva: CANDS_RVA,
                prologue: Box::leak(stub(cands_probe as *const () as usize)[..14].to_vec().into_boxed_slice()),
            }),
            town_edges: Some(HookSite {
                rva: EDGES_RVA,
                prologue: Box::leak(stub(edges_probe as *const () as usize)[..14].to_vec().into_boxed_slice()),
            }),
            town_sort_call: Some(call_site(SORT_CALL_RVA, SORT_CALLEE_RVA, sort_callee_probe as *const () as usize)),
            town_pick_call: Some(call_site(PICK_CALL_RVA, PICK_CALLEE_RVA, pick_callee_probe as *const () as usize)),
            town_field_call: None,
            batch_pace: None,
            free_order: None,
            game_time_getter: Some(CallSite { rva: GETTER_RVA, prologue: &GETTER_FN }),
            script_pool: Some(pool.clone()),
            lua_makeseed: Some(seed.clone()),
            tick_count_getter: Some(tick.clone()),
            command_move: Some(helper.clone()),
            command_destroy: Some(helper.clone()),
            handle_destroy: Some(helper.clone()),
            ui_add_sites: &[0x500],
            fire_done_calls: &[],
            kind_offset: Some(0x9b8),
            kinds: &[("build_proposal", 52)],
            engine: crate::build_table::EngineFns::NONE,
            layout: None,
        }))
    }

    /// A fake game image: one executable page with the fake functions at their RVAs; `base` is the page.
    fn fake_game() -> Pages {
        let g = Pages::new(&[PAGE_EXECUTE_READ]);
        g.fill(APPLY_RVA as usize, &SUM);
        g.fill(ADD_RVA as usize, SUM5);
        g.fill(POOL_RVA as usize, POOL_BEFORE);
        g.fill(SEED_RVA as usize, &SEED_FN);
        g.fill(DEVELOP_RVA as usize, &DEVELOP_FN);
        g.fill(CMD_RVA as usize, &handler_fn());
        g.fill(CANDS_RVA as usize, &stub(cands_probe as *const () as usize));
        let rel = |from: u32, to: u32| (i64::from(to) - i64::from(from) - 5) as i32;
        let mut sort_caller = vec![0x48, 0x83, 0xEC, 0x28, 0xE8];
        sort_caller.extend_from_slice(&rel(SORT_CALL_RVA, SORT_CALLEE_RVA).to_le_bytes());
        sort_caller.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28, 0xC3]);
        g.fill(SORT_CALLER_RVA as usize, &sort_caller);
        g.fill(SORT_CALLEE_RVA as usize, &stub(sort_callee_probe as *const () as usize));
        // push rbp ; mov rbp,rcx ; mov rcx,rdx ; xor edx,edx ; sub rsp,20h ; call ; add rsp,20h ; pop rbp ; ret
        let mut pick_caller = vec![0x55, 0x48, 0x89, 0xCD, 0x48, 0x89, 0xD1, 0x31, 0xD2, 0x48, 0x83, 0xEC, 0x20, 0xE8];
        pick_caller.extend_from_slice(&rel(PICK_CALL_RVA, PICK_CALLEE_RVA).to_le_bytes());
        pick_caller.extend_from_slice(&[0x48, 0x83, 0xC4, 0x20, 0x5D, 0xC3]);
        g.fill(PICK_CALLER_RVA as usize, &pick_caller);
        g.fill(PICK_CALLEE_RVA as usize, &stub(pick_callee_probe as *const () as usize));
        g.fill(EDGES_RVA as usize, &stub(edges_probe as *const () as usize));
        g.fill(GETTER_RVA as usize, &GETTER_FN);
        g.fill(LUASEED_RVA as usize, SEED_BEFORE);
        g.fill(TICK_RVA as usize, TICK_BEFORE);
        g.fill(MOVE_RVA as usize, &[0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97]);
        g
    }

    fn quiet(_: &str) {}

    fn runtime(build: &'static BuildAddrs, base: usize) -> Runtime {
        Runtime::new(Init {
            build,
            base,
            hold: HoldConfig::default(),
            engine: None,
            log: quiet,
            hooks: vec![],
            poolpatch: false,
            version: "test",
        })
    }

    fn everything() -> HookConfig {
        HookConfig { hooks: true, hold: true, poolpatch: true, ..Default::default() }
    }

    // the hooks of these tests: they only prove that the game's functions lead to them and that the published
    // trampolines reach the originals
    static APPLY_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
    static ADD_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
    static HITS: AtomicU64 = AtomicU64::new(0);

    extern "system" fn test_apply(a: u64, b: u64) -> u64 {
        HITS.fetch_add(1, SeqCst);
        let original: extern "system" fn(u64, u64) -> u64 =
            unsafe { core::mem::transmute(APPLY_ORIGINAL.load(SeqCst)) };
        original(a, b) * 10
    }

    extern "system" fn test_add(a: u64, b: u64, c: u64, d: u64, e: u64) -> u64 {
        HITS.fetch_add(100, SeqCst);
        let original: extern "system" fn(u64, u64, u64, u64, u64) -> u64 =
            unsafe { core::mem::transmute(ADD_ORIGINAL.load(SeqCst)) };
        original(a, b, c, d, e) * 10
    }

    fn targets() -> Targets {
        Targets {
            apply: test_apply as *const () as usize,
            add: test_add as *const () as usize,
            seed: town_seed_hook as *const () as usize,
            growth: growth_hook as *const () as usize,
            growth_cmd: growth_cmd_hook as *const () as usize,
            cands: candidates_hook as *const () as usize,
            edges: edges_hook as *const () as usize,
            sort: sort_hook as *const () as usize,
            pick: pick_hook as *const () as usize,
            field: field_hook as *const () as usize,
            convert: convert_hook as *const () as usize,
            convert_update: convert_update_hook as *const () as usize,
            preview: preview_hook as *const () as usize,
            pace: pace_hook as *const () as usize,
            free_order: free_order_hook as *const () as usize,
            preview_owner: preview_owner_hook as *const () as usize,
        }
    }

    fn apply_fn(g: &Pages) -> extern "system" fn(u64, u64) -> u64 {
        unsafe { core::mem::transmute(g.at(APPLY_RVA as usize)) }
    }

    fn add_fn(g: &Pages) -> extern "system" fn(u64, u64, u64, u64, u64) -> u64 {
        unsafe { core::mem::transmute(g.at(ADD_RVA as usize)) }
    }

    // the tests of one process share the statics above: run them one at a time
    static SERIAL: Mutex<()> = Mutex::new(());

    // ---- smooth pacing: the write after a batch, on a fake game object

    /// A fake `CGame` whose `[+0x1f0]` leads to fake timing data: accumulated time, last batch time, interval (us).
    struct FakeTiming {
        game: Box<[u64; 0x40]>,
        data: Box<[u64; 0x48]>,
    }

    impl FakeTiming {
        fn new(acc: i64, last: i64, interval: i32) -> FakeTiming {
            let mut t = FakeTiming { game: Box::new([0; 0x40]), data: Box::new([0; 0x48]) };
            t.game[crate::pace::DATA_OFFSET / 8] = t.data.as_ptr() as u64;
            t.data[crate::pace::ACC_OFFSET / 8] = acc as u64;
            t.data[crate::pace::LAST_OFFSET / 8] = last as u64;
            t.set_interval(interval);
            t
        }
        fn interval(&self) -> i32 {
            self.data[crate::pace::INTERVAL_OFFSET / 8] as u32 as i32
        }
        fn set_interval(&mut self, v: i32) {
            let w = &mut self.data[crate::pace::INTERVAL_OFFSET / 8];
            *w = (*w & !0xFFFF_FFFF) | u64::from(v as u32);
        }
        fn game(&self) -> usize {
            self.game.as_ptr() as usize
        }
    }

    #[test]
    fn after_a_batch_the_interval_is_stretched_by_the_fresh_factor_and_given_back() {
        let p = crate::pace::Pacer::new();
        let mut t = FakeTiming::new(5_000_000, 4_750_000, 250_000);
        unsafe { pace_after_sync(&p, t.game(), 10) };
        assert_eq!(t.interval(), 250_000, "factor 1: nothing written");
        p.set(1600, 100);
        unsafe { pace_after_sync(&p, t.game(), 200) };
        assert_eq!(t.interval(), 400_000);
        // Sync left it: not compounded
        unsafe { pace_after_sync(&p, t.game(), 300) };
        assert_eq!(t.interval(), 400_000);
        // Sync set its own estimate
        t.set_interval(300_000);
        unsafe { pace_after_sync(&p, t.game(), 400) };
        assert_eq!(t.interval(), 480_000);
        // the mod stopped refreshing: the engine's interval back
        unsafe { pace_after_sync(&p, t.game(), 100 + crate::pace::STALE_MS + 1) };
        assert_eq!(t.interval(), 300_000);
        assert!(!p.is_off());
    }

    #[test]
    fn a_layout_that_is_not_the_expected_one_switches_smooth_pacing_off_without_touching_anything() {
        let p = crate::pace::Pacer::new();
        p.set(2000, 0);
        unsafe { pace_after_sync(&p, 0, 1) };
        assert!(p.is_off(), "no game object");
        let p = crate::pace::Pacer::new();
        p.set(2000, 0);
        let mut t = FakeTiming::new(1, 2, 250_000);
        unsafe { pace_after_sync(&p, t.game(), 1) };
        assert_eq!(t.interval(), 250_000, "accumulated time before the last batch: left alone");
        assert!(!p.is_off());
        t.set_interval(7);
        t.data[crate::pace::ACC_OFFSET / 8] = 10;
        unsafe { pace_after_sync(&p, t.game(), 2) };
        assert_eq!(t.interval(), 7, "not an interval: left alone");
        // a pointer into unmapped memory
        t.game[crate::pace::DATA_OFFSET / 8] = 0x10;
        unsafe { pace_after_sync(&p, t.game(), 3) };
        assert!(p.is_off(), "unreadable timing data: off for good");
    }

    static FAKE_SYNC_ARGS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    static FAKE_SYNC_RESULT: AtomicU64 = AtomicU64::new(0);

    unsafe extern "system-unwind" fn fake_sync(a: u64, b: u64, c: u64, d: u64) -> u64 {
        FAKE_SYNC_ARGS.lock().unwrap_or_else(|p| p.into_inner()).extend([a, b, c, d]);
        FAKE_SYNC_RESULT.load(SeqCst)
    }

    #[test]
    fn the_pace_hook_calls_sync_with_its_arguments_and_returns_its_result() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let before = PACE_CALLEE.swap(fake_sync as *const () as usize, SeqCst);
        crate::pace::PACER.set(1000, crate::pace::clock_ms());
        let t = FakeTiming::new(5_000_000, 4_750_000, 250_000);
        // a Sync that handed out no batch (al = 0, the upper bits whatever they are): nothing is touched
        FAKE_SYNC_RESULT.store(0x1234_5600, SeqCst);
        assert_eq!(unsafe { pace_hook(t.game() as u64, 2, 3, 4) }, 0x1234_5600);
        FAKE_SYNC_RESULT.store(1, SeqCst);
        assert_eq!(unsafe { pace_hook(t.game() as u64, 5, 6, 7) }, 1);
        assert_eq!(*FAKE_SYNC_ARGS.lock().unwrap(), [t.game() as u64, 2, 3, 4, t.game() as u64, 5, 6, 7]);
        assert_eq!(t.interval(), 250_000, "factor 1: the engine's interval");
        PACE_CALLEE.store(before, SeqCst);
        FAKE_SYNC_ARGS.lock().unwrap().clear();
    }

    // ---- the free-id order: the hook in front of the queue append, on a fake insert

    /// What the fake insert saw: its five arguments and the ids in `[first, last)` at the moment it ran.
    static FAKE_INSERT_SEEN: Mutex<Vec<(Vec<u64>, Vec<i32>)>> = Mutex::new(Vec::new());

    /// A fake `deque<int>::_Insert_range(queue, &ret, where, first, last) -> &ret` that appends the range to a fake queue (`queue`
    /// points at a `Vec<i32>`), in range order, as the real one does.
    unsafe extern "system-unwind" fn fake_insert(queue: u64, ret: u64, at: u64, first: u64, last: u64) -> u64 {
        let n = ((last - first) / 4) as usize;
        let ids =
            if n == 0 { Vec::new() } else { unsafe { std::slice::from_raw_parts(first as *const i32, n) }.to_vec() };
        unsafe { (*(queue as *mut Vec<i32>)).extend_from_slice(&ids) };
        FAKE_INSERT_SEEN.lock().unwrap_or_else(|p| p.into_inner()).push((vec![queue, ret, at, first, last], ids));
        ret
    }

    /// One `EndModification` of a fake engine: the removed ids in the engine's order go through the hook into the queue.
    fn end_modification(queue: &mut Vec<i32>, removed: &mut [i32]) -> u64 {
        let range = removed.as_mut_ptr_range();
        let (first, last) = (range.start as u64, range.end as u64);
        unsafe { free_order_hook(queue as *mut Vec<i32> as u64, 0x5eed, queue.len() as u64, first, last) }
    }

    #[test]
    fn the_free_order_hook_sorts_each_batch_before_the_real_insert_and_passes_every_argument() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let before = FREE_ORDER_CALLEE.swap(fake_insert as *const () as usize, SeqCst);
        FAKE_INSERT_SEEN.lock().unwrap().clear();
        // the M lab of 2026-10-19: the same ids in two games' history orders give one queue
        let (mut q1, mut q2) = (vec![3, 4], vec![3, 4]);
        let mut b1 = [50859, 49942, 48057, 50861];
        let mut b2 = [49942, 48057, 50861, 50859];
        assert_eq!(end_modification(&mut q1, &mut b1), 0x5eed, "the insert's result comes back");
        end_modification(&mut q2, &mut b2);
        assert_eq!(q1, [3, 4, 48057, 49942, 50859, 50861]);
        assert_eq!(q1, q2);
        assert_eq!(b1, [48057, 49942, 50859, 50861], "sorted where it lies: the engine frees the vector right after");
        let seen = FAKE_INSERT_SEEN.lock().unwrap().clone();
        let range = b1.as_ptr_range();
        assert_eq!(seen[0].0[1..], [0x5eed, 2, range.start as u64, range.end as u64], "every argument as given");
        assert_eq!(seen[0].1, [48057, 49942, 50859, 50861], "the insert sees the sorted range");
        // an empty batch and a single id pass as they are
        let mut none: [i32; 0] = [];
        end_modification(&mut q1, &mut none);
        let mut one = [7];
        end_modification(&mut q1, &mut one);
        assert_eq!(q1[6..], [7]);
        FREE_ORDER_CALLEE.store(before, SeqCst);
        FAKE_INSERT_SEEN.lock().unwrap().clear();
    }

    #[test]
    fn a_range_that_is_not_a_batch_of_ids_is_left_alone_and_is_a_stop_fault() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        // unaligned, backwards, and unmapped: nothing is written, the game goes on, the player is told to restart
        let mut ids = [9i32, 8, 7];
        let p = ids.as_mut_ptr() as u64;
        unsafe { sort_freed(p + 1, p + 9) };
        unsafe { sort_freed(p + 8, p) };
        unsafe { sort_freed(0x10, 0x20) };
        assert_eq!(ids, [9, 8, 7]);
        assert!(crate::failsafe::GLOBAL.stopped(), "a stop fault was raised");
        unsafe { sort_freed(p, p + 12) };
        assert_eq!(ids, [7, 8, 9]);
    }

    #[test]
    fn a_full_installation_hooks_both_functions_and_patches_the_pool_and_comes_out_again() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let rt = runtime(build, base);
        let plan = plan::plan(build, everything(), &ProcessImage { base });
        assert_eq!(plan.steps.len(), 3, "{:?}", plan.report());

        let (mut inst, lines) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        let (add_orig, apply_orig) = rt.original_addresses();
        APPLY_ORIGINAL.store(apply_orig, SeqCst);
        ADD_ORIGINAL.store(add_orig, SeqCst);
        assert_ne!((add_orig, apply_orig), (0, 0), "{lines:?}");
        assert_eq!(inst.hook_names(), ["apply_command", "command_list_add"]);
        assert!(inst.poolpatch());
        assert!(lines.iter().any(|l| l.contains("script_pool: applied")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("apply_command: jump written")), "{lines:?}");

        HITS.store(0, SeqCst);
        assert_eq!(
            apply_fn(&game)(1, 2),
            80,
            "the game's apply function leads to the hook, whose original still works"
        );
        assert_eq!(add_fn(&game)(1, 2, 3, 4, 5), 150);
        assert_eq!(HITS.load(SeqCst), 101);
        assert_eq!(game.bytes(POOL_RVA as usize, POOL_AFTER.len()), POOL_AFTER);
        assert_eq!(game.protection(0), PAGE_EXECUTE_READ);

        let undone = unsafe { inst.uninstall_all() };
        assert_eq!(undone.len(), 3, "{undone:?}");
        assert!(inst.is_empty());
        assert_eq!(game.bytes(APPLY_RVA as usize, 16), SUM, "byte for byte as the game shipped it");
        assert_eq!(game.bytes(ADD_RVA as usize, SUM5.len()), SUM5);
        assert_eq!(game.bytes(POOL_RVA as usize, POOL_BEFORE.len()), POOL_BEFORE);
        assert_eq!(apply_fn(&game)(1, 2), 8);
        assert_eq!(add_fn(&game)(1, 2, 3, 4, 5), 15);
        assert_eq!(game.protection(0), PAGE_EXECUTE_READ);
    }

    static SEED_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn seed_log(line: &str) {
        SEED_LINES.lock().unwrap_or_else(|p| p.into_inner()).push(line.to_string());
    }

    type SeedFn = extern "system" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;

    fn seed_fn(g: &Pages) -> SeedFn {
        unsafe { core::mem::transmute(g.at(SEED_RVA as usize)) }
    }

    #[test]
    fn the_town_seed_hook_logs_the_ninth_argument_and_changes_nothing() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let rt = runtime(build, base);
        let cfg = HookConfig { town_seed: true, ..Default::default() };
        let plan = plan::plan(build, cfg, &ProcessImage { base });
        assert_eq!(plan.steps.len(), 1, "{:?}", plan.report());
        let before = seed_fn(&game)(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12);
        assert_eq!(before, 15, "the fake: a1 + a5 + a9");

        SEED_LINES.lock().unwrap_or_else(|p| p.into_inner()).clear();
        arm_town_seed(base, seed_log, None);
        let (mut inst, lines) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        assert_eq!(inst.hook_names(), ["town_seed"], "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("town_seed: jump written")), "{lines:?}");
        assert_eq!(game.bytes(APPLY_RVA as usize, 16), SUM, "the other functions are untouched");
        assert_eq!(game.bytes(ADD_RVA as usize, SUM5.len()), SUM5);

        // same result as unhooked; the high half of the seed slot is garbage the log must ignore
        let hooked = seed_fn(&game)(1, 2, 3, 4, 5, 6, 7, 8, 0xAAAA_BBBB_0000_0009, 10, 11, 12);
        assert_eq!(hooked, before);
        assert_eq!(seed_fn(&game)(100, 2, 0x1_0000_0007, 4, 5, 6, 7, 8, 1, 10, 11, 12), 106);
        let logged = SEED_LINES.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(logged.len(), 2, "{logged:?}");
        assert!(logged[0].starts_with("mapgen-seed n=1 seed=0x00000009 caller="), "{}", logged[0]);
        assert!(logged[0].ends_with(" a3=0x3 a4=0x4"), "{}", logged[0]);
        assert!(logged[1].starts_with("mapgen-seed n=2 seed=0x00000001 caller="), "{}", logged[1]);
        assert!(logged[1].contains(" a3=0x7 "), "{}", logged[1]);

        unsafe { inst.uninstall_all() };
        assert_eq!(game.bytes(SEED_RVA as usize, SEED_FN.len()), SEED_FN, "byte for byte as the game shipped it");
        assert_eq!(seed_fn(&game)(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12), 15);
    }

    #[test]
    fn a_forced_town_seed_is_what_the_original_reads_from_its_ninth_slot() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let rt = runtime(build, base);
        let cfg = HookConfig { town_seed: true, force_seed: Some(0x100), ..Default::default() };
        let plan = plan::plan(build, cfg, &ProcessImage { base });
        assert_eq!(plan.steps.len(), 1, "{:?}", plan.report());

        SEED_LINES.lock().unwrap_or_else(|p| p.into_inner()).clear();
        arm_town_seed(base, seed_log, cfg.force_seed);
        let (mut inst, lines) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        assert_eq!(inst.hook_names(), ["town_seed"], "{lines:?}");

        // the fake returns a1 + a5 + the ninth argument read from [rsp+0x48] on entry: the original sees 0x100,
        // not the caller's value, the garbage in the high half of the slot is gone, a1 and a5 pass as given
        assert_eq!(seed_fn(&game)(1, 2, 3, 4, 5, 6, 7, 8, 0xAAAA_BBBB_0000_0009, 10, 11, 12), 1 + 5 + 0x100);
        assert_eq!(seed_fn(&game)(100, 2, 3, 4, 50, 6, 7, 8, 1, 10, 11, 12), 100 + 50 + 0x100);
        let logged = SEED_LINES.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(logged.len(), 2, "{logged:?}");
        assert!(logged[0].starts_with("mapgen-seed n=1 seed=0x00000009 forced=0x00000100 caller="), "{}", logged[0]);
        assert!(logged[1].starts_with("mapgen-seed n=2 seed=0x00000001 forced=0x00000100 caller="), "{}", logged[1]);

        // re-armed without forcing: the caller's value passes again
        arm_town_seed(base, seed_log, None);
        assert_eq!(seed_fn(&game)(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12), 15);
        unsafe { inst.uninstall_all() };
        assert_eq!(seed_fn(&game)(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12), 15);
    }

    #[test]
    fn without_the_switch_the_seed_function_is_not_touched() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let plan = plan::plan(build, everything(), &ProcessImage { base });
        assert!(!plan.has(Feature::TownSeed));
        let rt = runtime(build, base);
        let (mut inst, _) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        assert_eq!(game.bytes(SEED_RVA as usize, SEED_FN.len()), SEED_FN);
        unsafe { inst.uninstall_all() };
    }

    #[test]
    fn a_changed_seed_function_is_not_hooked() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        game.fill(SEED_RVA as usize + 3, &[0x49]);
        let cfg = HookConfig { town_seed: true, ..Default::default() };
        let plan = plan::plan(build, cfg, &ProcessImage { base });
        assert!(plan.steps.is_empty());
        assert!(plan.report().iter().any(|l| l.contains("SKIPPED town_seed")), "{:?}", plan.report());
    }

    #[test]
    fn the_counters_alone_touch_only_the_apply_function() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let rt = runtime(build, base);
        let plan = plan::plan(build, HookConfig { hooks: true, ..Default::default() }, &ProcessImage { base });
        let (mut inst, _) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        assert_eq!(inst.hook_names(), ["apply_command"]);
        assert!(!inst.poolpatch());
        assert_eq!(game.bytes(ADD_RVA as usize, SUM5.len()), SUM5, "the add function is untouched");
        assert_eq!(game.bytes(POOL_RVA as usize, POOL_BEFORE.len()), POOL_BEFORE, "and so is the pool region");
        unsafe { inst.uninstall_all() };
        assert_eq!(game.bytes(APPLY_RVA as usize, 16), SUM);
    }

    #[test]
    fn a_failure_while_writing_the_jumps_takes_the_written_ones_out_again() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let rt = runtime(build, base);
        let plan = plan::plan(build, everything(), &ProcessImage { base });
        // something else patches the add function after it was prepared and before its jump is written
        let (add_bytes, stray) = (game.at(ADD_RVA as usize), [0xE9, 1, 2, 3, 4]);
        let (inst, lines) = unsafe {
            execute(&plan, base, &rt, &targets(), &mut |stage| {
                assert_eq!(stage, Stage::Prepared);
                crate::mem::write_code(add_bytes, &[(0, &stray)]).unwrap();
            })
        };
        assert!(inst.detours.is_empty(), "no jump is left: the game runs as shipped");
        assert!(lines.iter().any(|l| l.contains("command_list_add: jump NOT written")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("rolling back")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("apply_command: jump removed")), "{lines:?}");
        assert_eq!(game.bytes(APPLY_RVA as usize, 16), SUM, "the apply function that had its jump written is restored");
        assert_eq!(game.bytes(ADD_RVA as usize, 5), stray, "and the stranger's change is not touched");
        assert!(inst.poolpatch(), "the pool patch is independent and stays");
        assert_eq!(apply_fn(&game)(1, 2), 8);
    }

    #[test]
    fn a_prologue_that_changed_since_planning_skips_that_detour_only() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let rt = runtime(build, base);
        let plan = plan::plan(build, everything(), &ProcessImage { base });
        game.fill(APPLY_RVA as usize + 4, &[0x90]); // after the plan was made
        let (mut inst, lines) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        assert_eq!(inst.hook_names(), ["command_list_add"]);
        assert!(lines.iter().any(|l| l.contains("apply_command: NOT installed")), "{lines:?}");
        unsafe { inst.uninstall_all() };
        assert_eq!(game.bytes(ADD_RVA as usize, SUM5.len()), SUM5);
    }

    #[test]
    fn the_seed_pin_writes_mov_eax_ret_and_comes_out_again() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let rt = runtime(build, base);
        let cfg = HookConfig { luaseed: true, ..Default::default() };
        let plan = plan::plan(build, cfg, &ProcessImage { base });
        assert_eq!(plan.steps.len(), 1, "{:?}", plan.report());
        let (mut inst, lines) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        assert!(inst.lua_seed() && !inst.poolpatch() && inst.hook_names().is_empty(), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("lua seed pinned to 0x464c3353")), "{lines:?}");
        assert_eq!(game.bytes(LUASEED_RVA as usize, 14), SEED_AFTER);
        // the patched function really returns the constant
        let f: extern "system" fn() -> u32 = unsafe { core::mem::transmute(game.at(LUASEED_RVA as usize)) };
        assert_eq!(f(), 0x464C_3353);
        assert!(unsafe { inst.uninstall_all() }.iter().any(|l| l.contains("lua_makeseed: original bytes restored")));
        assert_eq!(game.bytes(LUASEED_RVA as usize, 14), SEED_BEFORE);
    }

    #[test]
    fn the_step_seed_makes_the_tick_getter_read_the_update_count_and_comes_out_again() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let rt = runtime(build, base);
        // a fake GameTime component: tickCount at +0x3c counted 900 frames, 500 of them standing still; updateCount 400
        let mut component = [0u8; 0x48];
        component[0x3c..0x40].copy_from_slice(&900u32.to_le_bytes());
        component[0x40..0x44].copy_from_slice(&400u32.to_le_bytes());
        let getter = || {
            let f: extern "system" fn(*const u8) -> u32 = unsafe { core::mem::transmute(game.at(TICK_RVA as usize)) };
            f(component.as_ptr())
        };
        assert_eq!(getter(), 900, "as shipped: the frame count");

        let cfg = HookConfig { step_seed: true, ..Default::default() };
        let plan = plan::plan(build, cfg, &ProcessImage { base });
        assert_eq!(plan.steps.len(), 1, "{:?}", plan.report());
        let (mut inst, lines) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        assert!(inst.step_seed() && !inst.lua_seed() && !inst.poolpatch() && inst.hook_names().is_empty(), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("simulation seeds read the step count")), "{lines:?}");
        assert_eq!(game.bytes(TICK_RVA as usize, TICK_AFTER.len()), TICK_AFTER);
        assert_eq!(getter(), 400, "patched: the step count, the same on every game");
        assert_eq!(game.protection(0), PAGE_EXECUTE_READ);
        assert_eq!(component[0x3c..0x40], 900u32.to_le_bytes(), "the field itself is not written");

        assert!(unsafe { inst.uninstall_all() }
            .iter()
            .any(|l| l.contains("tick_count_getter: original bytes restored")));
        assert!(inst.is_empty());
        assert_eq!(game.bytes(TICK_RVA as usize, TICK_BEFORE.len()), TICK_BEFORE);
        assert_eq!(getter(), 900);
    }

    #[test]
    fn a_tick_getter_that_does_not_match_is_left_alone_and_logged() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let rt = runtime(build, base);
        let cfg = HookConfig { step_seed: true, ..Default::default() };
        let plan = plan::plan(build, cfg, &ProcessImage { base });
        // the memory changed after planning: the executor refuses it again and writes nothing
        game.fill(TICK_RVA as usize + 2, &[0x38]);
        let (inst, lines) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        assert!(!inst.step_seed() && inst.is_empty());
        assert!(lines.iter().any(|l| l.starts_with("simulation seeds NOT moved to the step count")), "{lines:?}");
        assert_eq!(game.bytes(TICK_RVA as usize + 2, 1), [0x38], "nothing written");
    }

    #[test]
    fn a_seed_site_that_does_not_match_is_left_alone_and_logged() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let rt = runtime(build, base);
        let cfg = HookConfig { luaseed: true, ..Default::default() };
        // the planner already refuses other bytes
        game.fill(LUASEED_RVA as usize + 3, &[0xFF]);
        let plan = plan::plan(build, cfg, &ProcessImage { base });
        assert!(plan.steps.is_empty() && plan.skipped[0].name == "lua_makeseed", "{:?}", plan.report());
        // and the executor refuses them again if the memory changed after planning
        game.fill(LUASEED_RVA as usize, SEED_BEFORE);
        let plan = plan::plan(build, cfg, &ProcessImage { base });
        game.fill(LUASEED_RVA as usize + 3, &[0xFF]);
        let (inst, lines) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        assert!(!inst.lua_seed() && inst.is_empty());
        assert!(lines.iter().any(|l| l.starts_with("lua seed NOT pinned")), "{lines:?}");
        assert_eq!(game.bytes(LUASEED_RVA as usize + 3, 1), [0xFF], "nothing written");
    }

    #[test]
    fn a_pool_region_that_is_not_the_expected_one_is_not_patched() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let rt = runtime(build, base);
        let plan = plan::plan(build, HookConfig { poolpatch: true, ..Default::default() }, &ProcessImage { base });
        game.fill(POOL_RVA as usize + 2, &[0xFF]);
        let before = game.bytes(POOL_RVA as usize, POOL_BEFORE.len());
        let (inst, lines) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        assert!(inst.is_empty());
        assert!(lines.iter().any(|l| l.contains("script_pool: NOT applied")), "{lines:?}");
        assert_eq!(game.bytes(POOL_RVA as usize, POOL_BEFORE.len()), before);
    }

    #[test]
    fn an_empty_plan_changes_nothing() {
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let rt = runtime(build, base);
        let plan = plan::plan(build, HookConfig::default(), &ProcessImage { base });
        let (inst, lines) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        assert!(inst.is_empty() && lines.is_empty());
        assert_eq!(rt.original_addresses(), (0, 0));
    }

    #[test]
    fn the_real_build_row_finds_nothing_to_install_in_another_program() {
        // this test executable is not the game: whatever the 40420 row says, no byte matches, so nothing is planned
        let build = crate::build_table::fixture();
        let base = unsafe { GetModuleHandleW(core::ptr::null()) } as usize;
        let plan = plan_for_process(build, everything(), base, quiet);
        assert!(plan.steps.is_empty(), "{:?}", plan.steps);
        assert!(!plan.skipped.is_empty());
    }

    fn step_names(p: &Plan) -> Vec<&'static str> {
        p.steps.iter().map(Step::name).collect()
    }

    #[test]
    fn a_failed_detour_self_test_keeps_every_jump_out_but_not_the_pool_patch() {
        let (build, game) = (fake_build(), fake_game());
        let mut p = plan::plan(build, everything(), &ProcessImage { base: game.base as usize });
        assert_eq!(step_names(&p), ["apply_command", "command_list_add", "script_pool"]);
        assert!(p.engine.is_some());
        let report = selftest::Report { mechanics: Err("no executable memory".into()), caller: Err("not run".into()) };
        apply_selftest(&mut p, &report, quiet);
        assert_eq!(
            step_names(&p),
            ["script_pool"],
            "the pool patch needs neither detours nor call-site identification"
        );
        assert!(p.engine.is_none());
        let skipped: Vec<_> = p.skipped.iter().map(|s| (s.feature, s.name)).collect();
        assert_eq!(skipped, [(Feature::Counters, "apply_command"), (Feature::Hold, "command_list_add")]);
        assert!(p.skipped.iter().all(|s| s.reason.contains("self-test")));
    }

    #[test]
    fn a_failed_call_site_self_test_keeps_only_the_hold_hook_out() {
        let (build, game) = (fake_build(), fake_game());
        let mut p = plan::plan(build, everything(), &ProcessImage { base: game.base as usize });
        let report = selftest::Report { mechanics: Ok(()), caller: Err("found 0x1, expected 0x2".into()) };
        apply_selftest(&mut p, &report, quiet);
        assert_eq!(step_names(&p), ["apply_command", "script_pool"]);
        assert!(p.engine.is_none(), "without the hook the engine helpers are not wanted");
        assert_eq!(p.skipped.len(), 1);
        assert_eq!((p.skipped[0].feature, p.skipped[0].name), (Feature::Hold, "command_list_add"));
        assert!(p.skipped[0].reason.contains("caller identification"), "{}", p.skipped[0].reason);
    }

    #[test]
    fn a_passed_self_test_changes_nothing() {
        let (build, game) = (fake_build(), fake_game());
        let mut p = plan::plan(build, everything(), &ProcessImage { base: game.base as usize });
        let before = p.clone();
        apply_selftest(&mut p, &selftest::Report { mechanics: Ok(()), caller: Ok(()) }, quiet);
        assert_eq!(p, before);
    }

    #[test]
    fn the_installed_line_says_whether_there_are_jumps() {
        assert_eq!(
            installed_line(true),
            "hooks: installed (jumps in place; they pass everything through until the mod switches holding on)"
        );
        assert_eq!(installed_line(false), "hooks: installed (patch only)");
    }

    #[test]
    fn a_stale_identity_file_is_not_the_controllers_announcement() {
        let dir = crate::testutil::TempDir::new("fresh-stale");
        let path = dir.path().join("identity.txt");
        std::fs::write(&path, "id=0\n").unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        let since = SystemTime::now();
        assert!(!written_since(&path, since), "left over from an earlier run");
        let started = Instant::now();
        assert!(!wait_until_written(&path, since, Duration::from_millis(150), Duration::from_millis(10)));
        assert!(started.elapsed() >= Duration::from_millis(140), "it waited the whole time");
        assert!(!written_since(&dir.path().join("missing.txt"), since));
    }

    #[test]
    fn a_file_written_after_the_start_is_the_announcement_even_if_it_comes_late() {
        let dir = crate::testutil::TempDir::new("fresh-late");
        let path = dir.path().join("identity.txt");
        let since = SystemTime::now();
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(120));
                std::fs::write(path, "id=0\n").unwrap();
            })
        };
        let started = Instant::now();
        assert!(wait_until_written(&path, since, Duration::from_secs(10), Duration::from_millis(10)));
        assert!(started.elapsed() < Duration::from_secs(5));
        writer.join().unwrap();
    }

    /// The log lines of the attach test, in the order they were written.
    static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn capture(line: &str) {
        LOGGED.lock().unwrap_or_else(|p| p.into_inner()).push(line.to_string());
    }

    /// Asserts that `expected` (each a substring) appear in `lines` in this order.
    fn assert_in_order(lines: &[String], expected: &[&str]) {
        let mut from = 0;
        for want in expected {
            match lines[from..].iter().position(|l| l.contains(want)) {
                Some(i) => from += i + 1,
                None => panic!("`{want}` not found (in order) in {lines:#?}"),
            }
        }
    }

    #[test]
    fn attach_installs_reports_and_counts_end_to_end() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        // before attach() the runtime does not exist: the add hook hands back `out`, the apply hook returns 0
        assert!(RUNTIME.get().is_none());
        let mut out = [0u8; 16];
        let (n, p) = (core::ptr::null_mut(), core::ptr::null_mut());
        assert_eq!(unsafe { add_hook(n, out.as_mut_ptr(), n, p, n) }, out.as_mut_ptr());
        assert_eq!(unsafe { apply_hook(n, n) }, 0);

        // then the whole thing on a fake game: counters and the pool patch, with a session folder
        let dir = crate::testutil::TempDir::new("attach");
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let cfg = HookConfig { hooks: true, hold: false, poolpatch: true, ..Default::default() };
        let link = Some(Link { dir: dir.path().to_path_buf(), live: false });
        attach_at(base, build, cfg, link, capture);
        let rt = RUNTIME.get().expect("attach set the runtime");

        // the game's apply function now leads to the real hook, which counts the command and calls the original
        let mut payload = vec![0u8; 0x9c0];
        payload[0x9b8] = 52;
        let mut command = vec![0u8; 0x40];
        command[..8].copy_from_slice(&(payload.as_ptr() as usize).to_ne_bytes());
        let cmd = command.as_mut_ptr() as u64;
        assert_eq!(apply_fn(&game)(1, cmd), 1 + cmd + 5, "the original's result, through the trampoline");
        assert_eq!(apply_fn(&game)(1, 2), 8, "and a command that cannot be read is passed on as well");
        match rt.counters().snapshot() {
            crate::link::Event::Audit { counts, total, unreadable, .. } => {
                assert_eq!((counts, total, unreadable), (vec![(52, 1)], 2, 1));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(game.bytes(POOL_RVA as usize, POOL_AFTER.len()), POOL_AFTER, "the pool region is patched");
        assert_eq!(game.bytes(ADD_RVA as usize, SUM5.len()), SUM5, "the command list is not hooked: hold is off");

        // the service thread opens the link and tells the mod what is active
        let events = dir.path().join(crate::link::EVENTS_FILE);
        let started = Instant::now();
        let ready = loop {
            let lines = crate::link::CtlReader::new(events.clone()).poll().lines;
            if let Some(l) = lines.into_iter().find(|l| l.kind == "ready") {
                break l.payload;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "no ready event");
            std::thread::sleep(Duration::from_millis(20));
        };
        // the log lines docs/NATIVE_LINK.md section 8 tells a person to look for
        let logged = loop {
            let lines = LOGGED.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if lines.iter().any(|l| l.starts_with("event ready:")) {
                break lines;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "no ready line in the log");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_in_order(
            &logged,
            &[
                "hooks: switches hooks=on hold=off poolpatch=on luaseed=off step_seed=off town_seed=off fire_done=off",
                "hooks: self-test: detour, trampoline and stack argument work in this binary",
                "hooks: self-test: the hook identifies the call site of a detoured function",
                "hooks: apply_command: 16 bytes at rva 0x100 match the build table",
                "hooks: script_pool: 8 bytes at rva 0x300 match the build table",
                "hooks: script_pool: applied, 8 bytes at rva 0x300, page protection restored",
                "hooks: apply_command: jump written, original reachable at 0x",
                "hooks: installed (jumps in place; they pass everything through until the mod switches holding on)",
                "link: opened ",
                "event ready: build fake, hooks [apply_command], poolpatch applied, game clock no",
            ],
        );
        assert!(ready.contains("[1]=\"apply_command\"") && !ready.contains("command_list_add"), "{ready}");
        assert!(ready.contains("\"poolpatch\"]=true"), "{ready}");
        assert!(ready.contains("\"build\"]=\"fake\""), "{ready}");

        // put the game back (what a rollback would do) and check it is as shipped
        let mut inst = INSTALLED.lock().unwrap_or_else(|p| p.into_inner()).take().expect("kept");
        assert_eq!(inst.hook_names(), ["apply_command"]);
        unsafe { inst.uninstall_all() };
        assert_eq!(game.bytes(APPLY_RVA as usize, 16), SUM);
        assert_eq!(game.bytes(POOL_RVA as usize, POOL_BEFORE.len()), POOL_BEFORE);
        assert_eq!(apply_fn(&game)(1, 2), 8);
    }

    // ---- town growth

    const DEVELOP_RVA: u32 = 0x700;
    const CMD_RVA: u32 = 0x800;
    const GETTER_RVA: u32 = 0x900;
    const CANDS_RVA: u32 = 0xa00;
    // fake callers of the two redirected calls: the `call rel32` is at +4 (sort) and +13 (pick)
    const SORT_CALLER_RVA: u32 = 0xc00;
    const SORT_CALL_RVA: u32 = 0xc04;
    const SORT_CALLEE_RVA: u32 = 0xc40;
    const PICK_CALLER_RVA: u32 = 0xd00;
    const PICK_CALL_RVA: u32 = 0xd0d;
    const PICK_CALLEE_RVA: u32 = 0xd40;
    const EDGES_RVA: u32 = 0xb00;
    /// A fake `Develop(rcx, rdx, r8d, r9b, u32* state, a6, a7)`: reads the state through its fifth argument, which is
    /// `[rsp+28h]` at entry as in the game, and returns `*state + r8d`:
    /// `mov rax,[rsp+28h] ; mov eax,[rax] ; add eax,r8d ; nop5 ; ret`.
    const DEVELOP_FN: [u8; 16] =
        [0x48, 0x8B, 0x44, 0x24, 0x28, 0x8B, 0x00, 0x44, 0x01, 0xC0, 0x90, 0x90, 0x90, 0x90, 0x90, 0xC3];
    /// A fake of the game-time getter: `mov eax,[rcx] ; ret ; nop..` (14 bytes).
    const GETTER_FN: [u8; 14] = [0x8B, 0x01, 0xC3, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90];

    /// `mov rax, <target> ; jmp rax ; nop3` (15 bytes, no position-dependent operand): a fake game function whose
    /// behaviour is the Rust function `target` with the same arguments.
    fn stub(target: usize) -> [u8; 15] {
        let mut b = [0x90u8; 15];
        b[0] = 0x48;
        b[1] = 0xB8;
        b[2..10].copy_from_slice(&(target as u64).to_le_bytes());
        b[10] = 0xFF;
        b[11] = 0xE0;
        b
    }

    /// A call-site row of the fake build: the `call rel32` at `rva` leads to `callee`, whose body is the Rust function `to`.
    fn call_site(rva: u32, callee: u32, to: usize) -> crate::build_table::CallPatchSite {
        let rel = (i64::from(callee) - i64::from(rva) - 5) as i32;
        let mut call = vec![0xE8];
        call.extend_from_slice(&rel.to_le_bytes());
        crate::build_table::CallPatchSite {
            rva,
            expected: Box::leak(call.into_boxed_slice()),
            callee: CallSite { rva: callee, prologue: Box::leak(stub(to).to_vec().into_boxed_slice()) },
        }
    }

    /// A fake `TownUpdateSize` handler: a stub to `growth_probe`.
    fn handler_fn() -> [u8; 15] {
        stub(growth_probe as *const () as usize)
    }

    /// What the fake handler does: remember the game time the hook published, then run the (hooked) fake `Develop`
    /// of the fake game on a state it sets up, as the real handler does.
    static PROBE_DEVELOP: AtomicUsize = AtomicUsize::new(0);
    static PROBE_STATE: AtomicU32 = AtomicU32::new(0);
    static PROBE_INIT: AtomicU32 = AtomicU32::new(0);
    static PROBE_TIME: AtomicU64 = AtomicU64::new(0);

    extern "system" fn growth_probe(_this: u64, payload: u64) -> u64 {
        // a small number stands for the town itself; anything else is a pointer to a payload {town, 3 floats, flag}
        let town = if payload < 0x1_0000 { payload } else { u64::from(unsafe { *(payload as *const u32) }) };
        PROBE_TIME.store(GROWTH_TIME.load(SeqCst), SeqCst);
        PROBE_STATE.store(PROBE_INIT.load(SeqCst), SeqCst);
        develop(PROBE_DEVELOP.load(SeqCst), town, PROBE_STATE.as_ptr() as u64)
    }

    fn develop(at: usize, town: u64, state: u64) -> u64 {
        type DevelopFn = extern "system" fn(u64, u64, u64, u64, u64, u64, u64) -> u64;
        let f: DevelopFn = unsafe { core::mem::transmute(at) };
        f(1, 2, town, 1, state, 6, 7)
    }

    /// `Develop` of the fake game, called directly (not from the handler) on `PROBE_STATE` set to `init`.
    fn develop_direct(game: &Pages, town: u64, init: u32) -> u64 {
        PROBE_STATE.store(init, SeqCst);
        develop(game.at(DEVELOP_RVA as usize) as usize, town, PROBE_STATE.as_ptr() as u64)
    }

    static GROWTH_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn growth_log(line: &str) {
        GROWTH_LINES.lock().unwrap_or_else(|p| p.into_inner()).push(line.to_string());
    }

    fn growth_lines() -> Vec<String> {
        GROWTH_LINES.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The memory the fake handler's `this` points through: `this -> ctx (at +8) -> system (at +0x28) -> game time`.
    struct FakeSim {
        this: u64,
        time: &'static AtomicU32,
    }

    fn fake_sim() -> FakeSim {
        let time: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
        let ctx: &'static mut [u64; 8] = Box::leak(Box::new([0u64; 8]));
        ctx[5] = time as *const AtomicU32 as u64;
        let this: &'static mut [u64; 2] = Box::leak(Box::new([0u64; 2]));
        this[1] = ctx.as_ptr() as u64;
        FakeSim { this: this.as_ptr() as u64, time }
    }

    /// Runs the fake handler (through its hook, if installed) at game time `t` for `town` with the state `init`.
    fn command(game: &Pages, sim: &FakeSim, t: u32, town: u64, init: u32) -> u64 {
        sim.time.store(t, SeqCst);
        PROBE_INIT.store(init, SeqCst);
        type HandlerFn = extern "system" fn(u64, u64) -> u64;
        let f: HandlerFn = unsafe { core::mem::transmute(game.at(CMD_RVA as usize)) };
        f(sim.this, town)
    }

    fn growth_setup(settings: Settings) -> (Pages, &'static BuildAddrs, HookConfig) {
        (fake_game(), fake_build(), HookConfig { growth: settings, ..Default::default() })
    }

    fn install_growth(game: &Pages, build: &'static BuildAddrs, cfg: HookConfig) -> (Installed, Vec<String>, Plan) {
        install_growth_in(game, build, cfg, None)
    }

    fn install_growth_in(
        game: &Pages,
        build: &'static BuildAddrs,
        cfg: HookConfig,
        growth_dir: Option<PathBuf>,
    ) -> (Installed, Vec<String>, Plan) {
        let base = game.base as usize;
        let rt = runtime(build, base);
        let plan = plan::plan(build, cfg, &ProcessImage { base });
        let getter =
            if plan.has(Feature::GrowthCmd) { plan.game_time_getter.map_or(0, |r| base + r as usize) } else { 0 };
        GROWTH_LINES.lock().unwrap_or_else(|p| p.into_inner()).clear();
        arm_growth(base, growth_log, &cfg.growth, getter, growth_dir);
        PROBE_DEVELOP.store(game.at(DEVELOP_RVA as usize) as usize, SeqCst);
        let (inst, lines) = unsafe { execute(&plan, base, &rt, &targets(), &mut |_| {}) };
        (inst, lines, plan)
    }

    fn log_only() -> Settings {
        Settings { log: true, ..Default::default() }
    }

    fn derive_after_game(seconds: u64) -> Settings {
        Settings { log: true, mode: Mode::Derive, after_game_s: Some(seconds), ..Default::default() }
    }

    // ---- the deep growth log

    static SORT_SEEN: Mutex<Vec<[u64; 4]>> = Mutex::new(Vec::new());
    static PICK_SEEN: Mutex<Vec<[u64; 3]>> = Mutex::new(Vec::new());

    /// Fake `std::sort(begin, end, count, ctx)`: orders the 0x40-byte records by squared distance (x at +4, y at +8) from
    /// the origin, stably.
    extern "system" fn sort_callee_probe(begin: u64, end: u64, count: u64, ctx: u64) -> u64 {
        SORT_SEEN.lock().unwrap_or_else(|p| p.into_inner()).push([begin, end, count, ctx]);
        let n = ((end - begin) / 64) as usize;
        let bytes = unsafe { core::slice::from_raw_parts_mut(begin as *mut u8, n * 64) };
        let mut recs: Vec<Vec<u8>> = bytes.chunks(64).map(<[u8]>::to_vec).collect();
        let key = |r: &Vec<u8>| {
            let x = f32::from_le_bytes([r[4], r[5], r[6], r[7]]);
            let y = f32::from_le_bytes([r[8], r[9], r[10], r[11]]);
            y * y + x * x
        };
        recs.sort_by(|a, b| key(a).total_cmp(&key(b)));
        bytes.copy_from_slice(&recs.concat());
        0x1234
    }

    /// Fake `RandomInt(rng, lo, hi)`: `state = state * 3 + 1`, returns `state % hi`.
    extern "system" fn pick_callee_probe(rng: u64, lo: u64, hi: u64) -> u64 {
        PICK_SEEN.lock().unwrap_or_else(|p| p.into_inner()).push([rng, lo, hi]);
        let s = unsafe { &mut *(rng as *mut u32) };
        *s = s.wrapping_mul(3).wrapping_add(1);
        u64::from(*s) % (hi as u32 as u64)
    }

    /// What the fake candidate gather wrote into its out vector: (pointer, byte length) of the records, 0 = none.
    static CAND_DATA: AtomicUsize = AtomicUsize::new(0);
    static CAND_LEN: AtomicUsize = AtomicUsize::new(0);
    /// The arguments the fakes saw, to prove the hooks pass every slot (the floats too) as given.
    type CandArgs = (u64, u64, u32, u64, [u64; 5]);
    type EdgeArgs = (u64, u32, u64, u64, u64, u64);
    static CAND_SEEN: Mutex<Vec<CandArgs>> = Mutex::new(Vec::new());
    static EDGE_SEEN: Mutex<Vec<EdgeArgs>> = Mutex::new(Vec::new());
    static CAND_GAME: AtomicUsize = AtomicUsize::new(0);
    static CALLERS_ON: AtomicBool = AtomicBool::new(false);
    static SORT_CALLER: AtomicUsize = AtomicUsize::new(0);
    static PICK_CALLER: AtomicUsize = AtomicUsize::new(0);
    static SORT_BEGIN: AtomicUsize = AtomicUsize::new(0);
    static SORT_END: AtomicUsize = AtomicUsize::new(0);
    static SORT_CTX: AtomicUsize = AtomicUsize::new(0);
    static PICK_FRAME: AtomicUsize = AtomicUsize::new(0);
    static PICK_RNG: AtomicUsize = AtomicUsize::new(0);
    static PICK_RESULT: AtomicU64 = AtomicU64::new(0);
    static EDGE_GAME: AtomicUsize = AtomicUsize::new(0);
    static CAND_OUT: Mutex<[u64; 3]> = Mutex::new([0; 3]);

    type CandFn = extern "system" fn(u64, u64, f32, u64, u64, u64, u64, u64, u64) -> u64;
    type EdgeFn = extern "system" fn(u64, f32, u64, u64, u64, u64) -> u64;

    /// Fake `0x9677c0`: fills the out vector `[begin, end, cap]` with `CAND_DATA`, returns `out ^ 0x5a`.
    extern "system" fn cands_probe(
        out: u64,
        c: u64,
        radius: f32,
        r9: u64,
        a5: u64,
        a6: u64,
        a7: u64,
        a8: u64,
        a9: u64,
    ) -> u64 {
        CAND_SEEN.lock().unwrap_or_else(|p| p.into_inner()).push((out, c, radius.to_bits(), r9, [a5, a6, a7, a8, a9]));
        if out >= 0x1_0000 {
            let (b, n) = (CAND_DATA.load(SeqCst) as u64, CAND_LEN.load(SeqCst) as u64);
            unsafe {
                *(out as *mut u64) = b;
                *((out + 8) as *mut u64) = b + n;
                *((out + 16) as *mut u64) = b + n;
            }
        }
        out ^ 0x5a
    }

    /// Fake `0x95d100`: draws from the generator behind `rng` (`state * 3 + 1`), returns 0x77.
    extern "system" fn edges_probe(c: u64, radius: f32, r8: u64, r9: u64, a5: u64, rng: u64) -> u64 {
        EDGE_SEEN.lock().unwrap_or_else(|p| p.into_inner()).push((c, radius.to_bits(), r8, r9, a5, rng));
        if rng >= 0x1_0000 {
            unsafe { *(rng as *mut u32) = (*(rng as *const u32)).wrapping_mul(3).wrapping_add(1) };
        }
        0x77
    }

    /// Fake `Develop` for the deep tests: one gather, one edge selection, then its own draw (`state * 7 + 1`).
    extern "system" fn develop_probe(_a: u64, _b: u64, town: u64, _flag: u64, state: u64, _a6: u64, _a7: u64) -> u64 {
        let cands: CandFn = unsafe { core::mem::transmute(CAND_GAME.load(SeqCst)) };
        let edges: EdgeFn = unsafe { core::mem::transmute(EDGE_GAME.load(SeqCst)) };
        if CALLERS_ON.load(SeqCst) {
            // as inside the gather: the sort of the records, then (further on) the draw of an edge
            let sort: extern "system" fn(u64, u64, u64, u64) -> u64 =
                unsafe { core::mem::transmute(SORT_CALLER.load(SeqCst)) };
            let (b, e, c) =
                (SORT_BEGIN.load(SeqCst) as u64, SORT_END.load(SeqCst) as u64, SORT_CTX.load(SeqCst) as u64);
            sort(b, e, 3, c);
            let pick: extern "system" fn(u64, u64, u64) -> u64 =
                unsafe { core::mem::transmute(PICK_CALLER.load(SeqCst)) };
            PICK_RESULT.store(pick(PICK_FRAME.load(SeqCst) as u64, PICK_RNG.load(SeqCst) as u64, 3), SeqCst);
        }
        let mut out = CAND_OUT.lock().unwrap_or_else(|p| p.into_inner());
        cands(out.as_mut_ptr() as u64, 0xc0ffee, 2.5, 9, 10, 11, 12, 13, 14);
        drop(out);
        edges(0xbeef, 1.5, 3, 4, 5, state);
        let s = unsafe { &mut *(state as *mut u32) };
        *s = s.wrapping_mul(7).wrapping_add(1);
        u64::from(*s) + town
    }

    /// A record: id, 20 bytes at +4, flag, floats.
    fn record(id: u32, pos: u8, flag: u8, floats: u8) -> Vec<u8> {
        let mut r = vec![0u8; 0x40];
        r[..4].copy_from_slice(&id.to_le_bytes());
        r[4..0x18].fill(pos);
        r[0x18..0x24].fill(0xee);
        r[0x24..0x3c].fill(floats);
        r[0x3c] = flag;
        r
    }

    fn set_candidates(records: &[Vec<u8>]) {
        let data: &'static [u8] = Box::leak(records.concat().into_boxed_slice());
        CAND_DATA.store(data.as_ptr() as usize, SeqCst);
        CAND_LEN.store(data.len(), SeqCst);
    }

    fn deep_settings() -> Settings {
        Settings { log: true, deep: true, ..Default::default() }
    }

    /// The deep game: the fake game with its `Develop` replaced by a stub to `develop_probe`, and the build row to match.
    fn deep_game() -> (Pages, &'static BuildAddrs) {
        let game = fake_game();
        let stub_bytes = stub(develop_probe as *const () as usize);
        game.fill(DEVELOP_RVA as usize, &stub_bytes);
        let mut build = fake_build().clone();
        build.town_develop =
            Some(HookSite { rva: DEVELOP_RVA, prologue: Box::leak(stub_bytes.to_vec().into_boxed_slice()) });
        (game, Box::leak(Box::new(build)))
    }

    /// A payload `{town, 3 floats, flag}` that lives for the test.
    fn payload(town: u32, floats: [f32; 3], flag: u8) -> u64 {
        let mut b = vec![0u8; 0x18];
        b[..4].copy_from_slice(&town.to_le_bytes());
        for (i, f) in floats.iter().enumerate() {
            b[4 + 4 * i..8 + 4 * i].copy_from_slice(&f.to_le_bytes());
        }
        b[0x10] = flag;
        Box::leak(b.into_boxed_slice()).as_ptr() as u64
    }

    fn deep_lines(dir: &Path) -> Vec<String> {
        let text = std::fs::read_to_string(dir.join(growth::LOG_FILE)).unwrap_or_default();
        text.lines().filter(|l| l.starts_with("gdeep-")).map(str::to_string).collect()
    }

    fn field<'a>(line: &'a str, name: &str) -> &'a str {
        line.split(' ').find_map(|t| t.strip_prefix(&format!("{name}="))).unwrap_or_else(|| panic!("{name} in {line}"))
    }

    fn call_handler(game: &Pages, sim: &FakeSim, t: u32, payload: u64, init: u32) -> u64 {
        command(game, sim, t, payload, init)
    }

    #[test]
    fn the_deep_log_reports_payload_rng_candidates_and_edges_and_changes_nothing() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("deep");
        let (game, build) = deep_game();
        let sim = fake_sim();
        let cfg = HookConfig { growth: deep_settings(), ..Default::default() };
        let a = record(11, 1, 1, 2);
        let b = record(12, 3, 0, 4);
        set_candidates(&[a.clone(), b.clone()]);
        CAND_GAME.store(game.at(CANDS_RVA as usize) as usize, SeqCst);
        EDGE_GAME.store(game.at(EDGES_RVA as usize) as usize, SeqCst);
        let p7 = payload(7, [1.0, 2.5, -3.0], 0);

        // the game as shipped: result and arguments to compare with
        CAND_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clear();
        EDGE_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clear();
        PROBE_DEVELOP.store(game.at(DEVELOP_RVA as usize) as usize, SeqCst);
        let plain = call_handler(&game, &sim, 500, p7, 100);
        assert_eq!(plain, ((100 * 3 + 1) * 7 + 1) + 7);
        let plain_cands = CAND_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let plain_edges = EDGE_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!((plain_cands.len(), plain_edges.len()), (1, 1));

        let (mut inst, lines, plan) = install_growth_in(&game, build, cfg, Some(dir.clone()));
        assert_eq!(
            inst.hook_names(),
            ["town_develop", "town_update_size", "town_candidates", "town_edges", "town_sort", "town_pick"],
            "{lines:?} {:?}",
            plan.report()
        );
        assert!(lines.iter().any(|l| l.contains("town_candidates: jump written")), "{lines:?}");
        CAND_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clear();
        EDGE_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clear();

        assert_eq!(call_handler(&game, &sim, 500, p7, 100), plain, "the result of the hooked game is the same");
        let seen_c = CAND_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let seen_e = EDGE_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(seen_c.len(), 1);
        assert_eq!(seen_c, plain_cands, "every slot as without the hook");
        assert_eq!(seen_e, plain_edges);
        assert_eq!(
            (seen_c[0].2, seen_c[0].3, seen_c[0].4),
            (2.5f32.to_bits(), 9, [10, 11, 12, 13, 14]),
            "the float too"
        );
        assert_eq!(seen_e, vec![(0xbeef, 1.5f32.to_bits(), 3, 4, 5, PROBE_STATE.as_ptr() as u64)]);
        // a second command of the same town in the same tick, then another town
        set_candidates(&[b.clone(), a.clone()]);
        call_handler(&game, &sim, 500, p7, 5);
        call_handler(&game, &sim, 500, payload(8, [0.5, 0.5, 0.5], 1), 5);
        call_handler(&game, &sim, 501, p7, 5);

        let pid = std::process::id();
        let l = deep_lines(&dir);
        assert!(l[0].starts_with(&format!("gdeep-payload pid={pid} t=500 town=7 k=0 c=0 b1=0x3f800000 b2=0x40200000 b3=0xc0400000 f1=1 f2=2.5 f3=-3 flag=0")), "{}", l[0]);
        let d1 = growth::digest_records(&[a.clone(), b.clone()].concat());
        assert_eq!(
            l[1],
            format!(
                "gdeep-cand pid={pid} t=500 town=7 k=0 j=0 n=2 ho={:#018x} hs={:#018x} hio={:#018x} his={:#018x}",
                d1.ordered, d1.sum, d1.id_ordered, d1.id_sum
            )
        );
        assert_eq!(l[2], format!("gdeep-edge pid={pid} t=500 town=7 k=0 j=0 before=0x00000064 after=0x0000012d"));
        assert_eq!(l[3], format!("gdeep-dev pid={pid} t=500 town=7 k=0 d=1 seed=0x00000064 end=0x0000083c"));
        // the second command: k=1, the same records in the other order: same sum, other ordered hash
        assert!(l[4].starts_with(&format!("gdeep-payload pid={pid} t=500 town=7 k=1 c=1 ")), "{}", l[4]);
        let d2 = growth::digest_records(&[b.clone(), a.clone()].concat());
        assert_eq!((d1.sum, d1.id_sum), (d2.sum, d2.id_sum));
        assert_ne!(d1.ordered, d2.ordered);
        assert_eq!(field(&l[5], "hs"), format!("{:#018x}", d2.sum));
        assert_eq!(field(&l[5], "ho"), format!("{:#018x}", d2.ordered));
        assert!(
            l[8].starts_with(&format!("gdeep-payload pid={pid} t=500 town=8 k=0 c=2 ")) && l[8].ends_with("flag=1"),
            "{}",
            l[8]
        );
        // a new tick starts the counters again
        assert!(l[12].starts_with(&format!("gdeep-payload pid={pid} t=501 town=7 k=0 c=0 ")), "{}", l[12]);
        assert_eq!(l.len(), 16);

        let undone = unsafe { inst.uninstall_all() };
        assert_eq!(undone.len(), 6, "{undone:?}");
        assert_eq!(
            game.bytes(CANDS_RVA as usize, 15),
            stub(cands_probe as *const () as usize),
            "byte for byte as shipped"
        );
        assert_eq!(game.bytes(EDGES_RVA as usize, 15), stub(edges_probe as *const () as usize));
        assert_eq!(call_handler(&game, &sim, 500, p7, 100), plain);
        assert_eq!(deep_lines(&dir).len(), 16, "nothing is logged after the hooks are out");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record with the position floats (x at +4, y at +8) and an id.
    fn record_at(id: u32, x: f32, y: f32) -> Vec<u8> {
        let mut r = record(id, 0, 1, 2);
        r[4..8].copy_from_slice(&x.to_le_bytes());
        r[8..12].copy_from_slice(&y.to_le_bytes());
        r
    }

    fn leak_u32s(v: &[u32]) -> &'static mut [u32] {
        Box::leak(v.to_vec().into_boxed_slice())
    }

    #[test]
    fn the_sort_and_pick_redirects_log_the_input_order_and_the_candidates_and_change_nothing() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("deep-calls");
        let (game, build) = deep_game();
        let sim = fake_sim();
        set_candidates(&[record(1, 1, 1, 1)]);
        CAND_GAME.store(game.at(CANDS_RVA as usize) as usize, SeqCst);
        EDGE_GAME.store(game.at(EDGES_RVA as usize) as usize, SeqCst);
        SORT_CALLER.store(game.at(SORT_CALLER_RVA as usize) as usize, SeqCst);
        PICK_CALLER.store(game.at(PICK_CALLER_RVA as usize) as usize, SeqCst);
        // three records: distances 25, 25 and 2 from the origin; gathered in this order
        let (r1, r2, r3) = (record_at(1, 3.0, 4.0), record_at(2, 5.0, 0.0), record_at(3, 1.0, 1.0));
        let gathered: &'static mut [u8] = Box::leak([r1.clone(), r2.clone(), r3.clone()].concat().into_boxed_slice());
        SORT_BEGIN.store(gathered.as_ptr() as usize, SeqCst);
        SORT_END.store(gathered.as_ptr() as usize + gathered.len(), SeqCst);
        let centre: &'static mut [f32; 2] = Box::leak(Box::new([0.0, 0.0]));
        let ctx: &'static mut [u64; 8] = Box::leak(Box::new([0u64; 8]));
        ctx[4] = centre.as_ptr() as u64;
        SORT_CTX.store(ctx.as_ptr() as usize, SeqCst);
        // the pick's frame: the id vector's begin and end at frame-0x40 and frame-0x38
        let ids = leak_u32s(&[10, 20, 30]);
        let frame: &'static mut [u64; 16] = Box::leak(Box::new([0u64; 16]));
        frame[0] = ids.as_ptr() as u64;
        frame[1] = ids.as_ptr() as u64 + 12;
        let frame_ptr = frame.as_ptr() as u64 + 0x40;
        PICK_FRAME.store(frame_ptr as usize, SeqCst);
        let rng: &'static mut u32 = Box::leak(Box::new(0));
        PICK_RNG.store(rng as *mut u32 as usize, SeqCst);
        CALLERS_ON.store(true, SeqCst);
        let p7 = payload(7, [1.0, 2.0, 3.0], 0);
        PROBE_DEVELOP.store(game.at(DEVELOP_RVA as usize) as usize, SeqCst);

        // the game as shipped
        *rng = 100;
        SORT_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clear();
        PICK_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clear();
        let plain = command(&game, &sim, 500, p7, 100);
        let plain_pick = PICK_RESULT.load(SeqCst);
        let sorted_plain = gathered.to_vec();
        assert_eq!(plain_pick, 301 % 3);
        assert_ne!(sorted_plain, [r1.clone(), r2.clone(), r3.clone()].concat(), "the fake sort moved the records");
        let seen_sort = SORT_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let seen_pick = PICK_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clone();
        gathered.copy_from_slice(&[r1.clone(), r2.clone(), r3.clone()].concat());
        *rng = 100;
        SORT_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clear();
        PICK_SEEN.lock().unwrap_or_else(|p| p.into_inner()).clear();

        let cfg = HookConfig { growth: deep_settings(), ..Default::default() };
        let (mut inst, lines, plan) = install_growth_in(&game, build, cfg, Some(dir.clone()));
        assert_eq!(
            inst.hook_names(),
            ["town_develop", "town_update_size", "town_candidates", "town_edges", "town_sort", "town_pick"],
            "{lines:?} {:?}",
            plan.report()
        );
        assert!(lines.iter().any(|l| l.contains("town_sort: call at rva 0xc04 redirected")), "{lines:?}");
        assert_ne!(
            game.bytes(SORT_CALL_RVA as usize, 5),
            plan_expected(build, true),
            "the call now leads to the thunk"
        );

        assert_eq!(command(&game, &sim, 500, p7, 100), plain, "the result is the same");
        assert_eq!(PICK_RESULT.load(SeqCst), plain_pick, "the draw is the same");
        assert_eq!(gathered.to_vec(), sorted_plain, "the callee sorted the same input");
        assert_eq!(*SORT_SEEN.lock().unwrap_or_else(|p| p.into_inner()), seen_sort, "every argument as given");
        assert_eq!(*PICK_SEEN.lock().unwrap_or_else(|p| p.into_inner()), seen_pick);

        let pid = std::process::id();
        let l = deep_lines(&dir);
        let pre = growth::digest_positions(&[r1.clone(), r2.clone(), r3.clone()].concat());
        let sort_line = l.iter().find(|x| x.starts_with("gdeep-sort")).expect("sort line");
        assert_eq!(
            *sort_line,
            format!(
                "gdeep-sort pid={pid} t=500 town=7 k=0 j=0 n=3 ho={:#018x} hs={:#018x} hio={:#018x} his={:#018x} ties=1",
                pre.ordered, pre.sum, pre.id_ordered, pre.id_sum
            )
        );
        let ids_digest = growth::digest_ids(&[10u32, 20, 30].iter().flat_map(|i| i.to_le_bytes()).collect::<Vec<u8>>());
        let pick_line = l.iter().find(|x| x.starts_with("gdeep-pick")).expect("pick line");
        assert_eq!(
            *pick_line,
            format!(
                "gdeep-pick pid={pid} t=500 town=7 k=0 j=0 n=3 ho={:#018x} hs={:#018x} before=0x00000064 after=0x0000012d idx=1",
                ids_digest.ordered, ids_digest.sum
            )
        );
        // the sort line comes before the gather's own line (the sort is inside the gather)
        let at = |kind: &str| l.iter().position(|x| x.starts_with(kind)).unwrap();
        assert!(at("gdeep-sort") < at("gdeep-cand") && at("gdeep-pick") < at("gdeep-dev"));

        // the records gathered in another order: the sum stays, the ordered hash changes
        gathered.copy_from_slice(&[r3.clone(), r1.clone(), r2.clone()].concat());
        *rng = 100;
        command(&game, &sim, 501, p7, 100);
        let l = deep_lines(&dir);
        let second = l.iter().rev().find(|x| x.starts_with("gdeep-sort")).unwrap();
        let other = growth::digest_positions(&[r3.clone(), r1.clone(), r2.clone()].concat());
        assert_eq!((other.sum, other.ordered != pre.ordered), (pre.sum, true));
        assert!(second.contains(&format!("ho={:#018x} hs={:#018x}", other.ordered, other.sum)), "{second}");

        let undone = unsafe { inst.uninstall_all() };
        assert!(undone.iter().any(|u| u.contains("town_sort: call restored")), "{undone:?}");
        assert_eq!(game.bytes(SORT_CALL_RVA as usize, 5), plan_expected(build, true), "byte for byte as shipped");
        assert_eq!(game.bytes(PICK_CALL_RVA as usize, 5), plan_expected(build, false));
        let n_before = deep_lines(&dir).len();
        command(&game, &sim, 502, p7, 100);
        assert_eq!(deep_lines(&dir).len(), n_before);
        CALLERS_ON.store(false, SeqCst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn plan_expected(build: &BuildAddrs, sort: bool) -> Vec<u8> {
        let site = if sort { &build.town_sort_call } else { &build.town_pick_call };
        site.as_ref().unwrap().expected.to_vec()
    }

    #[test]
    fn a_changed_call_or_callee_leaves_the_call_alone() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (game, build) = deep_game();
        CALLERS_ON.store(false, SeqCst);
        game.fill(PICK_CALLEE_RVA as usize + 12, &[0xcc]);
        game.fill(SORT_CALL_RVA as usize + 1, &[0x77]);
        let sort_call_before = game.bytes(SORT_CALL_RVA as usize, 5);
        let cfg = HookConfig { growth: deep_settings(), ..Default::default() };
        let (mut inst, _, plan) = install_growth_in(&game, build, cfg, None);
        assert_eq!(inst.hook_names(), ["town_develop", "town_update_size", "town_candidates", "town_edges"]);
        let report = plan.report();
        assert!(report.iter().any(|l| l.contains("SKIPPED town_pick") && l.contains("callee")), "{report:?}");
        assert!(report.iter().any(|l| l.contains("SKIPPED town_sort")), "{report:?}");
        assert_eq!(game.bytes(SORT_CALL_RVA as usize, 5), sort_call_before);
        assert_eq!(game.bytes(PICK_CALL_RVA as usize, 5), plan_expected(build, false));
        unsafe { inst.uninstall_all() };
    }

    #[test]
    fn without_the_deep_switch_neither_wrap_is_installed_and_no_deep_line_is_written() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("deep-off");
        let (game, build) = deep_game();
        let sim = fake_sim();
        set_candidates(&[record(1, 1, 1, 1)]);
        CAND_GAME.store(game.at(CANDS_RVA as usize) as usize, SeqCst);
        EDGE_GAME.store(game.at(EDGES_RVA as usize) as usize, SeqCst);
        let cfg = HookConfig { growth: log_only(), ..Default::default() };
        let (mut inst, _, _) = install_growth_in(&game, build, cfg, Some(dir.clone()));
        assert_eq!(inst.hook_names(), ["town_develop", "town_update_size"]);
        assert_eq!(game.bytes(CANDS_RVA as usize, 15), stub(cands_probe as *const () as usize), "not touched");
        call_handler(&game, &sim, 10, payload(7, [1.0; 3], 0), 3);
        assert!(deep_lines(&dir).is_empty());
        assert!(std::fs::read_to_string(dir.join(growth::LOG_FILE)).unwrap().contains("growth n=1 "));
        unsafe { inst.uninstall_all() };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_prologue_leaves_that_function_alone_and_the_rest_keeps_logging() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("deep-mismatch");
        let (game, build) = deep_game();
        let sim = fake_sim();
        set_candidates(&[record(1, 1, 1, 1)]);
        CAND_GAME.store(game.at(CANDS_RVA as usize) as usize, SeqCst);
        EDGE_GAME.store(game.at(EDGES_RVA as usize) as usize, SeqCst);
        game.fill(CANDS_RVA as usize + 12, &[0xcc]);
        let cfg = HookConfig { growth: deep_settings(), ..Default::default() };
        let (mut inst, _, plan) = install_growth_in(&game, build, cfg, Some(dir.clone()));
        assert_eq!(
            inst.hook_names(),
            ["town_develop", "town_update_size", "town_edges", "town_sort", "town_pick"],
            "{:?}",
            plan.report()
        );
        assert!(plan.report().iter().any(|l| l.contains("SKIPPED town_candidates")));
        assert_eq!(game.bytes(CANDS_RVA as usize + 12, 1), [0xcc], "the changed function is left as it is");
        call_handler(&game, &sim, 10, payload(7, [1.0; 3], 0), 3);
        let kinds: Vec<_> = deep_lines(&dir).iter().map(|l| l.split(' ').next().unwrap().to_string()).collect();
        assert_eq!(
            kinds,
            ["gdeep-payload", "gdeep-edge", "gdeep-dev"],
            "{:?} {:?}",
            std::fs::read_to_string(dir.join(growth::LOG_FILE)),
            GROWTH_DIR.lock().map(|d| d.clone())
        );
        unsafe { inst.uninstall_all() };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn calls_outside_develop_are_numbered_orphans_and_bad_pointers_are_survived() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("deep-orphan");
        let (game, build) = deep_game();
        set_candidates(&[record(1, 1, 1, 1)]);
        let cfg = HookConfig { growth: deep_settings(), ..Default::default() };
        let (mut inst, _, _) = install_growth_in(&game, build, cfg, Some(dir.clone()));
        let cands: CandFn = unsafe { core::mem::transmute(game.at(CANDS_RVA as usize)) };
        let edges: EdgeFn = unsafe { core::mem::transmute(game.at(EDGES_RVA as usize)) };
        let mut out = [0u64; 3];
        assert_eq!(cands(out.as_mut_ptr() as u64, 1, 2.0, 3, 4, 5, 6, 7, 8), out.as_ptr() as u64 ^ 0x5a);
        let mut rng = 9u32;
        assert_eq!(edges(1, 1.0, 2, 3, 4, &mut rng as *mut u32 as u64), 0x77);
        assert_eq!(rng, 28, "the original ran");
        // an out vector that is not memory, a rng pointer that is not memory or not aligned: the game's call goes through
        assert_eq!(cands(0x10, 1, 2.0, 3, 4, 5, 6, 7, 8), 0x10 ^ 0x5a);
        assert_eq!(edges(1, 1.0, 2, 3, 4, 0x20), 0x77);
        assert_eq!(edges(1, 1.0, 2, 3, 4, 0x3), 0x77);
        // a vector with a bad length is reported, not read
        let mut bad = [0x1000u64, 0x1001, 0];
        deep_candidates(bad.as_mut_ptr() as u64);
        bad = [0x1000, 0x1000 + 0x40 * (growth::MAX_RECORDS as u64 + 1), 0];
        deep_candidates(bad.as_mut_ptr() as u64);
        assert!(deep_payload(0xdead_0000, Some(1)).is_none());
        assert!(deep_payload(0, None).is_none());
        let l = deep_lines(&dir);
        assert!(
            l[0].starts_with(&format!("gdeep-cand pid={} t=- town=0 k=0 j=0 n=1 ", std::process::id())),
            "{}",
            l[0]
        );
        assert!(
            l[1].starts_with("gdeep-edge ") && l[1].contains(" j=1 before=0x00000009 after=0x0000001c"),
            "{}",
            l[1]
        );
        assert!(l.iter().any(|x| x.ends_with("capped=1")), "{l:?}");
        unsafe { inst.uninstall_all() };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The deep log reads the engine's heap on the engine's threads: memory that goes away after it was found readable is a
    /// refused read (no line), not an access fault on a game thread.
    #[test]
    fn deep_reads_of_memory_that_went_away_are_refused_not_faults() {
        use windows_sys::Win32::System::Memory::{VirtualProtect, PAGE_NOACCESS, PAGE_READWRITE};
        let p = Pages::new(&[PAGE_READWRITE]);
        let mut payload = [0u8; 0x11];
        payload[..4].copy_from_slice(&7u32.to_le_bytes());
        p.fill(0, &payload);
        let at = p.at(0) as u64;
        assert_eq!(deep_read_u32(at), Some(7), "read once: the region is cached as readable");
        assert_eq!(deep_read_u64(at), Some(7));
        let mut old = 0u32;
        assert_ne!(unsafe { VirtualProtect(p.at(0) as *const _, 0x1000, PAGE_NOACCESS, &mut old) }, 0);
        let faults = crate::memguard::stats().faults;
        assert_eq!(deep_read_u32(at), None);
        assert_eq!(deep_read_u64(at), None);
        assert!(deep_payload(at, Some(1)).is_none());
        assert!(deep_vector(at, at + 0x40, 0x40, 4).is_err());
        assert!(crate::memguard::stats().faults > faults, "the stale cache entry faulted inside the guarded copy");
        assert_eq!(deep_read_u32(at + 2), None, "misaligned");
        assert_eq!(deep_read_u64(0), None);
    }

    /// Every function the game jumps to counts itself in and out (`hookwatch`), also when it returns early.
    #[test]
    fn the_field_hook_counts_itself_in_and_out() {
        let before = crate::hookwatch::FIELD_HOOK.read();
        FIELD_CALLEE.store(0, SeqCst);
        assert_eq!(unsafe { field_hook(0, 5, 0, 0) }, 5, "not armed: passes the out pointer back");
        let after = crate::hookwatch::FIELD_HOOK.read();
        assert_eq!((after.0 - before.0, after.1 - before.1), (1, 1));
    }

    #[test]
    #[ignore = "full set: the deep town trace past its 2000-command sampling threshold, 4 s; the plain trace test covers every-call and sampling (cargo test -- --include-ignored)"]
    fn traced_towns_are_logged_on_every_call_in_the_deep_log() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("deep-trace");
        let (game, build) = deep_game();
        let sim = fake_sim();
        set_candidates(&[record(1, 1, 1, 1)]);
        CAND_GAME.store(game.at(CANDS_RVA as usize) as usize, SeqCst);
        EDGE_GAME.store(game.at(EDGES_RVA as usize) as usize, SeqCst);
        let settings = Settings { trace: growth::Trace::parse("5").0, ..deep_settings() };
        let cfg = HookConfig { growth: settings, ..Default::default() };
        let (mut inst, _, _) = install_growth_in(&game, build, cfg, Some(dir.clone()));
        let (p5, p6) = (payload(5, [1.0; 3], 0), payload(6, [1.0; 3], 0));
        for i in 0..2300u32 {
            call_handler(&game, &sim, 1000 + i, p5, 3);
            call_handler(&game, &sim, 1000 + i, p6, 3);
        }
        let l = deep_lines(&dir);
        let count = |kind: &str, town: &str| l.iter().filter(|x| x.starts_with(kind) && x.contains(town)).count();
        assert_eq!(count("gdeep-payload", " town=5 "), 2300);
        assert_eq!(count("gdeep-dev", " town=5 "), 2300);
        let sampled = count("gdeep-payload", " town=6 ");
        assert!(
            (1000..1200).contains(&sampled),
            "the first 2000 commands (both towns) all, then a fraction: {sampled}"
        );
        // the lines of one command belong together: as many gathers and edges as payloads
        assert_eq!(count("gdeep-cand", " town=6 "), sampled);
        assert_eq!(count("gdeep-edge", " town=6 "), sampled);
        unsafe { inst.uninstall_all() };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_growth_log_reports_the_call_and_changes_nothing() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (game, build, cfg) = growth_setup(log_only());
        let sim = fake_sim();
        assert_eq!(develop_direct(&game, 7, 100), 107, "the fake: state + town");

        let (mut inst, lines, plan) = install_growth(&game, build, cfg);
        assert_eq!(plan.steps.len(), 2, "{:?}", plan.report());
        assert_eq!(inst.hook_names(), ["town_develop", "town_update_size"], "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("town_develop: jump written")), "{lines:?}");

        // through the fake handler: the game time is read through the pointer chain and logged
        assert_eq!(command(&game, &sim, 12_345, 7, 100), 107, "the original's result is unchanged");
        assert_eq!(PROBE_STATE.load(SeqCst), 100, "log only: the state is not touched");
        assert_eq!(PROBE_TIME.load(SeqCst), TIME_VALID | 12_345, "visible to Develop while the handler runs");
        assert_eq!(GROWTH_TIME.load(SeqCst), 0, "and gone when it returns");
        // Develop called by someone else (no game time known)
        assert_eq!(develop_direct(&game, 8, 100), 108);
        assert_eq!(PROBE_STATE.load(SeqCst), 100);

        let logged = growth_lines();
        assert_eq!(logged.len(), 2, "{logged:?}");
        let pid = std::process::id();
        let head = format!("growth n=1 pid={pid} town=7 seed=0x00000064 t=12345 r9b=1 a6=0x0000000000000006 ");
        assert!(logged[0].starts_with(&head), "{}", logged[0]);
        // the extra inputs of the call and its result: arg 6 as float + byte at +4, arg 7, rax (= state + town)
        assert!(logged[0].contains(" a6b=0 a7=0x7 ret=0x6b caller="), "{}", logged[0]);
        assert!(!logged[0].contains("used="), "{}", logged[0]);
        assert!(logged[1].starts_with(&format!("growth n=2 pid={pid} town=8 seed=0x00000064 r9b=1 ")), "{}", logged[1]);
        assert!(!logged[1].contains(" t="), "{}", logged[1]);

        unsafe { inst.uninstall_all() };
        assert_eq!(game.bytes(DEVELOP_RVA as usize, DEVELOP_FN.len()), DEVELOP_FN, "byte for byte as shipped");
        assert_eq!(game.bytes(CMD_RVA as usize, 15), handler_fn());
        assert_eq!(game.bytes(GETTER_RVA as usize, GETTER_FN.len()), GETTER_FN, "the getter is never touched");
        assert_eq!(develop_direct(&game, 1, 9), 10);
    }

    #[test]
    fn before_the_switch_derive_mode_only_logs_and_after_it_the_original_reads_the_derived_seed() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (game, build, cfg) = growth_setup(derive_after_game(60));
        let sim = fake_sim();
        let (mut inst, lines, _) = install_growth(&game, build, cfg);
        assert_eq!(inst.hook_names(), ["town_develop", "town_update_size"], "{lines:?}");

        // tick 299 (59.8 s): below the threshold, the original reads the game's own seed
        assert_eq!(command(&game, &sim, 299, 7, 100), 107);
        assert_eq!(PROBE_STATE.load(SeqCst), 100);
        assert!(!GROWTH_SWITCHED.load(SeqCst));

        // tick 302 (60.4 s): the switch; the original reads derive(town, tick rounded down to 5 ticks = 1000 ms)
        let q = growth::quantum_ticks(growth::DEFAULT_QUANTUM_MS);
        assert_eq!(q, 5);
        let d7 = growth::derive(7, 302, q);
        assert_eq!(d7, growth::derive(7, 300, q));
        assert_eq!(command(&game, &sim, 302, 7, 100), u64::from(d7) + 7, "the draw is from the derived seed");
        assert_eq!(PROBE_STATE.load(SeqCst), d7);
        assert!(GROWTH_SWITCHED.load(SeqCst));

        // two ticks later, same quantum: the same seed whatever the game's own seed (it differs: 5 vs 100)
        assert_eq!(command(&game, &sim, 304, 7, 5), u64::from(d7) + 7);
        // another town differs, the next quantum (tick 305 = 61 s) differs
        let d8 = growth::derive(8, 304, q);
        assert_ne!(d8, d7);
        assert_eq!(command(&game, &sim, 304, 8, 5), u64::from(d8) + 8);
        let d7_next = growth::derive(7, 305, q);
        assert_ne!(d7_next, d7);
        assert_eq!(command(&game, &sim, 305, 7, 5), u64::from(d7_next) + 7);

        // a call whose game time is unknown (not from TownUpdateSize) is never rewritten
        assert_eq!(develop_direct(&game, 7, 42), 49);

        let logged = growth_lines();
        let switches: Vec<_> = logged.iter().filter(|l| l.starts_with("growth-switch")).collect();
        assert_eq!(switches.len(), 1, "{logged:?}");
        assert!(
            switches[0].contains("game_time=302 ticks") && switches[0].contains("5 ticks = 1000 ms"),
            "{}",
            switches[0]
        );
        let calls: Vec<_> = logged.iter().filter(|l| l.starts_with("growth n=")).collect();
        assert_eq!(calls.len(), 6, "{logged:?}");
        assert!(!calls[0].contains("used="), "{}", calls[0]);
        assert!(calls[1].contains(&format!(" seed=0x00000064 t=302 used={d7:#010x} ")), "{}", calls[1]);
        assert!(calls[2].contains(&format!(" seed=0x00000005 t=304 used={d7:#010x} ")), "{}", calls[2]);
        assert!(!calls[5].contains("used="), "{}", calls[5]);
        // the switch line comes right before the first replaced call's line
        let at = logged.iter().position(|l| l.starts_with("growth-switch")).unwrap();
        assert!(logged[at + 1].starts_with("growth n=2 "), "{logged:?}");

        unsafe { inst.uninstall_all() };
        assert_eq!(game.bytes(DEVELOP_RVA as usize, DEVELOP_FN.len()), DEVELOP_FN);
    }

    #[test]
    fn a_wall_clock_threshold_not_yet_reached_keeps_derive_mode_logging_only() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let settings = Settings { log: true, mode: Mode::Derive, after_wall_s: 3600, ..Default::default() };
        let (game, build, cfg) = growth_setup(settings);
        let sim = fake_sim();
        let (mut inst, _, _) = install_growth(&game, build, cfg);
        assert_eq!(command(&game, &sim, 99_999_000, 7, 100), 107);
        assert_eq!(PROBE_STATE.load(SeqCst), 100);
        assert!(growth_lines().iter().all(|l| !l.contains("used=") && !l.starts_with("growth-switch")));
        unsafe { inst.uninstall_all() };
    }

    #[test]
    fn without_a_readable_game_time_derive_mode_never_rewrites() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (game, build, cfg) = growth_setup(Settings { log: true, mode: Mode::Derive, ..Default::default() });
        // the handler no longer matches the table: only Develop is hooked, there is no game time
        game.fill(CMD_RVA as usize + 13, &[0xCC]);
        let (mut inst, _, plan) = install_growth(&game, build, cfg);
        assert_eq!(inst.hook_names(), ["town_develop"]);
        assert!(plan.report().iter().any(|l| l.contains("SKIPPED town_update_size")), "{:?}", plan.report());
        assert_eq!(develop_direct(&game, 7, 100), 107);
        assert_eq!(PROBE_STATE.load(SeqCst), 100);
        assert!(growth_lines()[0]
            .starts_with(&format!("growth n=1 pid={} town=7 seed=0x00000064 r9b=1", std::process::id())));
        unsafe { inst.uninstall_all() };
    }

    #[test]
    fn a_changed_develop_prologue_installs_nothing() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (game, build, cfg) = growth_setup(log_only());
        game.fill(DEVELOP_RVA as usize + 6, &[0x40]);
        let (inst, _, plan) = install_growth(&game, build, cfg);
        assert!(plan.steps.iter().all(|s| s.feature() != Feature::Growth), "{:?}", plan.report());
        assert!(plan.report().iter().any(|l| l.contains("SKIPPED town_develop")), "{:?}", plan.report());
        assert!(!inst.hook_names().contains(&"town_develop"));
        assert_eq!(game.bytes(DEVELOP_RVA as usize + 6, 1), [0x40], "the changed function is left as it is");
    }

    #[test]
    fn without_the_growth_switches_nothing_of_growth_is_touched() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (build, game) = (fake_build(), fake_game());
        let base = game.base as usize;
        let plan = plan::plan(build, everything(), &ProcessImage { base });
        assert!(!plan.has(Feature::Growth) && !plan.has(Feature::GrowthCmd));
        assert_eq!(game.bytes(DEVELOP_RVA as usize, DEVELOP_FN.len()), DEVELOP_FN);
        assert_eq!(game.bytes(CMD_RVA as usize, 15), handler_fn());
    }

    #[test]
    fn only_the_first_calls_and_a_fraction_of_the_rest_are_logged() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (game, build, cfg) = growth_setup(log_only());
        let (mut inst, _, _) = install_growth(&game, build, cfg);
        for i in 0..2600u32 {
            develop_direct(&game, u64::from(i % 17), i.wrapping_mul(2_654_435_761));
        }
        let n = growth_lines().len();
        assert!((2000..2080).contains(&n), "2000 first calls, then about one in 50 of 600: {n}");
        unsafe { inst.uninstall_all() };
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fl3-growth-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn growth_lines_go_to_a_per_game_file_with_the_pid() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("file");
        let (game, build, cfg) = growth_setup(log_only());
        let sim = fake_sim();
        let (mut inst, _, _) = install_growth_in(&game, build, cfg, Some(dir.clone()));
        assert_eq!(command(&game, &sim, 777, 7, 100), 107);
        assert_eq!(develop_direct(&game, 8, 100), 108);
        let text = std::fs::read_to_string(dir.join(growth::LOG_FILE)).expect("growth.log");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        let pid = std::process::id();
        assert!(lines[0].starts_with(&format!("growth n=1 pid={pid} town=7 seed=0x00000064 t=777 ")), "{}", lines[0]);
        assert!(lines[1].contains(" town=8 "), "{}", lines[1]);
        // the same lines reached the shared log, so that old tooling keeps working
        assert_eq!(growth_lines().len(), 2);
        unsafe { inst.uninstall_all() };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_traced_town_is_logged_on_every_call_and_the_others_stay_sampled() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("trace");
        let settings = Settings { log: true, trace: growth::Trace::parse("5").0, ..Default::default() };
        let (game, build, cfg) = growth_setup(settings);
        let (mut inst, _, _) = install_growth_in(&game, build, cfg, Some(dir.clone()));
        for i in 0..2600u32 {
            develop_direct(&game, 5, i.wrapping_mul(2_654_435_761));
        }
        for i in 0..2600u32 {
            develop_direct(&game, 6, i.wrapping_mul(2_654_435_761));
        }
        let text = std::fs::read_to_string(dir.join(growth::LOG_FILE)).expect("growth.log");
        let traced = text.lines().filter(|l| l.contains(" town=5 ")).count();
        let other = text.lines().filter(|l| l.contains(" town=6 ")).count();
        assert_eq!(traced, 2600, "every call of the traced town");
        assert!(other < 2600 && other > 0, "the other town is sampled after the first 2000 calls: {other}");
        // the shared log is not flooded by the traced town: it keeps the sampling
        assert!(growth_lines().len() < 2600 + 2600, "{}", growth_lines().len());
        unsafe { inst.uninstall_all() };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cache of the fake `RiverField::At`: (field, cell, answer), as the game keeps one per field object.
    type FakeFieldEntry = (u64, (i32, i32), [u32; 5]);
    static FAKE_FIELD_CACHE: Mutex<Vec<FakeFieldEntry>> = Mutex::new(Vec::new());

    /// A fake `At(field, out, point, flag) -> out` that behaves like the game's: a cell asked before is answered from the cache
    /// (the answer of the first point asked there), a new cell is computed from the point and inserted (size at `field+0x20` + 1).
    extern "system" fn fake_field_at(field: u64, out: u64, point: u64, _flag: u64) -> u64 {
        let p = point as *const f32;
        let (x, y) = unsafe { (*p, *p.add(1)) };
        let cell = growth::field_cell(x, y);
        let mut cache = FAKE_FIELD_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        let answer = match cache.iter().find(|(f, c, _)| *f == field && *c == cell) {
            Some((_, _, a)) => *a,
            None => {
                let a = [x.to_bits(), y.to_bits(), 1.0f32.to_bits(), 0, 7.5f32.to_bits()];
                cache.push((field, cell, a));
                unsafe { *((field + 0x20) as *mut u64) += 1 };
                a
            }
        };
        let o = out as *mut u32;
        for (i, v) in answer.iter().enumerate() {
            unsafe { *o.add(i) = *v };
        }
        out
    }

    #[test]
    fn the_field_trace_tells_a_cached_answer_from_a_computed_one_and_changes_nothing() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let dir = temp_dir("field");
        *GROWTH_DIR.lock().unwrap_or_else(|p| p.into_inner()) = Some(dir.clone());
        GROWTH_DEEP.store(true, SeqCst);
        FIELD_ORPHANS.store(0, SeqCst);
        FIELD_OBJECTS.lock().unwrap_or_else(|p| p.into_inner()).clear();
        FAKE_FIELD_CACHE.lock().unwrap_or_else(|p| p.into_inner()).clear();
        FIELD_CALLEE.store(fake_field_at as *const () as usize, SeqCst);
        // two field objects (one per engine): two sources each (32 bytes), empty caches
        let sources = Box::leak(Box::new([0u8; 32]));
        let mk = || {
            let f: &'static mut [u64; 6] = Box::leak(Box::new([0u64; 6]));
            f[0] = sources.as_ptr() as u64;
            f[1] = f[0] + 32;
            f.as_mut_ptr() as u64
        };
        let (a, b) = (mk(), mk());
        let call = |field: u64, x: f32, y: f32| -> [u32; 5] {
            let point = [x, y];
            let mut out = [0u32; 5];
            let ret = unsafe { field_hook(field, out.as_mut_ptr() as u64, point.as_ptr() as u64, 0) };
            assert_eq!(ret, out.as_ptr() as u64, "the callee's result is passed back");
            out
        };
        let first = call(a, 10.0, 10.0);
        let cached = call(a, 20.0, 5.0); // same cell (0,0): the first point's answer
        assert_eq!(cached, first, "the fake answers from its cache like the game");
        let other_engine = call(b, 20.0, 5.0); // the other engine has not seen the cell: computed at this point
        assert_ne!(other_engine, first);
        FIELD_CALLEE.store(0, SeqCst);
        GROWTH_DEEP.store(false, SeqCst);
        *GROWTH_DIR.lock().unwrap_or_else(|p| p.into_inner()) = None;
        let text = std::fs::read_to_string(dir.join(growth::LOG_FILE)).expect("growth.log");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        let pid = std::process::id();
        assert!(lines[0]
            .starts_with(&format!("gdeep-field pid={pid} t=- town=0 k=0 j=0 cell=0,0 p=0x41200000,0x41200000 ")));
        assert!(lines[0].ends_with(" hit=0 n=1 src=2 buf=0"), "{}", lines[0]);
        assert!(lines[1].contains(" j=1 cell=0,0 p=0x41a00000,0x40a00000 a=0x41200000,0x41200000,"), "{}", lines[1]);
        assert!(lines[1].ends_with(" hit=1 n=1 src=2 buf=0"), "{}", lines[1]);
        assert!(lines[2].contains(" a=0x41a00000,0x40a00000,"), "{}", lines[2]);
        assert!(lines[2].ends_with(" hit=0 n=1 src=2 buf=1"), "{}", lines[2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_folder_only_the_shared_log_is_written() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let (game, build, cfg) = growth_setup(log_only());
        let (mut inst, _, _) = install_growth_in(&game, build, cfg, None);
        assert_eq!(develop_direct(&game, 8, 100), 108);
        assert_eq!(growth_lines().len(), 1);
        unsafe { inst.uninstall_all() };
    }
}
