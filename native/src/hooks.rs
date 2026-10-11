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

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use windows_sys::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GetModuleHandleW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
};

use crate::build_table::{BuildAddrs, Rva};
use crate::detour::{hook_caller, Detour};
use crate::engine::{AddArgs, EngineFns};
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
    match RUNTIME.get() {
        // SAFETY: the arguments of the live call this function was jumped to from.
        Some(rt) => unsafe { rt.on_apply(sim, cmd) },
        None => 0,
    }
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
        // SAFETY: stored by `arm_town_seed` from a `fn(&str)`.
        let log: fn(&str) = unsafe { core::mem::transmute(log) };
        let caller = townseed::caller_field(caller, TOWN_SEED_BASE.load(Ordering::Acquire), TOWN_SEED_IMAGE_BOUND);
        log(&townseed::line(TOWN_SEED_COUNTER.next(), seed as u32, forced, &caller, a3 as u32, a4 as u32));
    }
    // The ninth slot is an ordinary parameter of this function: the call below writes the outgoing copy of it to the
    // original's `[rsp+0x48]`, so replacing the value here is what the original reads. The other slots pass as given.
    let seed = forced.map_or(seed, u64::from);
    type Original = unsafe extern "system-unwind" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
    // SAFETY: the trampoline `Detour::prepare` built for the function this hook replaces, which takes these slots.
    let original: Original = unsafe { core::mem::transmute(original) };
    unsafe { original(out, a2, a3, a4, a5, a6, a7, a8, seed, a10, a11, a12) }
}

/// Where the jumps lead.
#[derive(Debug, Clone, Copy)]
pub struct Targets {
    pub apply: usize,
    pub add: usize,
    pub seed: usize,
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
}

impl Installed {
    pub fn hook_names(&self) -> Vec<&'static str> {
        self.detours.iter().map(|(n, _)| *n).collect()
    }

    pub fn poolpatch(&self) -> bool {
        self.patch.as_ref().is_some_and(AppliedPatch::is_applied)
    }

    pub fn is_empty(&self) -> bool {
        self.detours.is_empty() && self.patch.is_none()
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
        while let Some((name, mut d)) = self.detours.pop() {
            match unsafe { d.uninstall() } {
                Ok(()) => lines.push(format!("hooks: {name}: jump removed, original bytes restored")),
                Err(e) => lines.push(format!("hooks: {name}: jump NOT removed: {e}")),
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
        if let Step::Patch { name, rva, expected, replacement, .. } = step {
            let at = (base + *rva as usize) as *mut u8;
            match unsafe { AppliedPatch::apply(at, expected, replacement) } {
                Ok(p) => {
                    lines.push(format!(
                        "hooks: {name}: applied, {} bytes at rva {rva:#x}, page protection {}",
                        expected.len(),
                        if p.protection_restored() { "restored" } else { "NOT restored" }
                    ));
                    inst.patch = Some(p);
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
        let mut undo = Installed { detours: written, patch: None };
        lines.push("hooks: rolling back the jumps already written".to_string());
        lines.extend(unsafe { undo.uninstall_all() });
    } else {
        inst.detours = written;
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
        let remove = matches!(s, Step::Detour { feature, .. } if drop_all || (drop_hold && *feature == Feature::Hold));
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
    let cfg = HookConfig::from_lookup(get);
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
        log("hooks: nothing to install");
        return;
    }

    let engine = match (plan.has(Feature::Hold), plan.engine, plan.kind_offset) {
        // SAFETY: the plan verified the prologues of the three helpers against the image.
        (true, Some(e), Some(k)) => {
            Some(unsafe { EngineFns::at(base, e.command_move, e.command_destroy, e.handle_destroy, k) })
        }
        _ => None,
    };
    // the town-seed log is not one of the pieces the mod's `ready` event lists
    let planned: Vec<&'static str> = plan
        .steps
        .iter()
        .filter(|s| matches!(s, Step::Detour { .. }) && s.feature() != Feature::TownSeed)
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
    let targets = Targets {
        apply: apply_hook as *const () as usize,
        add: add_hook as *const () as usize,
        seed: town_seed_hook as *const () as usize,
    };
    if plan.has(Feature::TownSeed) {
        arm_town_seed(base, log, cfg.force_seed);
    }
    // SAFETY: called from DllMain, before the game runs the functions being patched.
    let (installed, lines) = unsafe { execute(&plan, base, rt, &targets, &mut |_| {}) };
    for l in lines {
        log(&l);
    }
    let mod_hooks: Vec<&'static str> = installed.hook_names().into_iter().filter(|n| *n != "town_seed").collect();
    let seed_logged = installed.hook_names().contains(&"town_seed");
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
        log("hooks: town_seed: logging each call of the town task's seeding function as 'town-seed n=.. seed=.. caller=..'");
        if let Some(v) = cfg.force_seed {
            log(&format!("hooks: town_seed: EXPERIMENT: replacing the seed argument with {v:#010x} on every call"));
        } else {
            log("hooks: town_seed: read-only");
        }
    }
    if !mod_pieces {
        // only the seed log: nothing for the control link to do
        log("hooks: installed (town-seed log only; nothing else in the game is changed, no control link)");
        return;
    }
    log(&installed_line(hooked));
    if let Some(link) = link {
        spawn_service(rt, link, since, log);
    } else {
        log("hooks: no session folder: counters only in memory, no control link");
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

/// The service thread: opens the control link once the session folder is ready, then pumps every 20 ms.
fn spawn_service(rt: &'static Runtime, link: Link, since: SystemTime, log: fn(&str)) {
    let spawned = std::thread::Builder::new().name("feverlink3-native".into()).spawn(move || {
        if link.live {
            // the in-process controller clears the folder when it starts and then writes identity.txt: wait for
            // a file newer than this process, or the first events would be deleted under us
            let identity = link.dir.join("identity.txt");
            if !wait_until_written(&identity, since, Duration::from_secs(120), Duration::from_millis(100)) {
                log("link: the session controller did not announce itself within 120 s; opening the folder anyway");
            }
        }
        if let Err(e) = rt.open_link(&link.dir) {
            log(&format!("link: cannot open {}: {e}", link.dir.display()));
            return;
        }
        loop {
            rt.pump();
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
    const POOL_BEFORE: &[u8] = &[0x83, 0xF8, 0x08, 0x0F, 0x8D, 0x25, 0x01, 0x00];
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
        Box::leak(Box::new(BuildAddrs {
            timestamp: 9,
            name: "fake",
            command_list_add: Some(add.clone()),
            apply_command: Some(APPLY.clone()),
            town_seed: Some(HookSite { rva: SEED_RVA, prologue: &SEED_FN[..14] }),
            script_pool: Some(pool.clone()),
            command_move: Some(helper.clone()),
            command_destroy: Some(helper.clone()),
            handle_destroy: Some(helper.clone()),
            ui_add_sites: &[0x500],
            kind_offset: Some(0x9b8),
            kinds: &[("build_proposal", 52)],
        }))
    }

    /// A fake game image: one executable page with the fake functions at their RVAs; `base` is the page.
    fn fake_game() -> Pages {
        let g = Pages::new(&[PAGE_EXECUTE_READ]);
        g.fill(APPLY_RVA as usize, &SUM);
        g.fill(ADD_RVA as usize, SUM5);
        g.fill(POOL_RVA as usize, POOL_BEFORE);
        g.fill(SEED_RVA as usize, &SEED_FN);
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
        assert!(logged[0].starts_with("town-seed n=1 seed=0x00000009 caller="), "{}", logged[0]);
        assert!(logged[0].ends_with(" a3=0x3 a4=0x4"), "{}", logged[0]);
        assert!(logged[1].starts_with("town-seed n=2 seed=0x00000001 caller="), "{}", logged[1]);
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
        assert!(logged[0].starts_with("town-seed n=1 seed=0x00000009 forced=0x00000100 caller="), "{}", logged[0]);
        assert!(logged[1].starts_with("town-seed n=2 seed=0x00000001 forced=0x00000100 caller="), "{}", logged[1]);

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
        let build = crate::build_table::select(0x6ac5_0427).unwrap();
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
                "hooks: switches hooks=on hold=off poolpatch=on",
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
}
