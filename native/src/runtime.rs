//! The module's state while the game runs, and what the two hooks and the service thread do with it
//! (`docs/NATIVE_LINK.md` sections 4 and 5).
//!
//! Three kinds of callers meet here:
//!
//! - **The command-list hook** ([`Runtime::on_add`]) runs on the interface thread inside every call to
//!   `CommandList::Add`. Nearly always it only counts and calls the original. When the mod has switched holding
//!   on, it also takes the construction tools' commands of the held kinds out of the engine's hands, and it is
//!   the only place that hands held commands back, so the engine is only ever touched from the thread that
//!   queues commands. It never waits: if the state is busy it calls the original at once.
//! - **The apply hook** ([`Runtime::on_apply`]) runs on the simulation thread for every command the simulation
//!   applies. It touches nothing but atomics.
//! - **The service thread** ([`Runtime::pump`], every 20 ms) reads the control file, lets time pass in the state
//!   machine (silence, old entries), and writes the events. It never calls the engine.
//!
//! So a held command is released by the next call to `CommandList::Add` after the mod's `release` line is
//! visible. The mod sends a harmless command (a script event) right after writing the line to cause exactly
//! that; the hook also looks at the control file itself, so it does not have to wait for the service thread.
//!
//! The state machine, the counters and the link are tested on their own; the tests here wire them together with
//! a fake engine and real files.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};
use std::thread::Thread;
use std::time::Instant;

use crate::build_table::BuildAddrs;
use crate::counters::{audit_log_line, AddSource, AppliedWatch, Audit, Counters};
use crate::dump::{self, Budget, Dumper, Env, Header, LiveMem, Scope};
use crate::engine::{self, AddArgs, AddFn, EngineFns, Held};
use crate::hold::{Action, Admit, HoldConfig, HoldMachine};
use crate::link::{parse_ctl_line, CtlReader, Event, EventWriter};
use crate::portable;
use crate::rebuild;

mod assets;
mod cmd_patch;
mod inject;
mod native_copy;
mod terrain;

/// The unhooked `apply` function of the simulation: (simulation, command) -> whatever it returns. A C++ function,
/// so declared as one that may unwind.
pub type ApplyFn = unsafe extern "system-unwind" fn(sim: *mut u8, cmd: *mut u8) -> u64;

/// How often the audit event is written when the counts changed.
pub const AUDIT_INTERVAL_MS: u64 = 30_000;

/// Offset of the engine's result (`ProposalData`) inside a `build_proposal` payload (see `portable::PAYLOAD`, member `result`).
const PAYLOAD_RESULT_DATA: usize = 0x3d8;

/// What a [`Runtime`] is built from.
pub struct Init {
    pub build: &'static BuildAddrs,
    /// Address of the game's image: RVA + base is an address.
    pub base: usize,
    pub hold: HoldConfig,
    /// The engine helpers, when the hold hook is installed.
    pub engine: Option<EngineFns>,
    /// Where the module's own log lines go.
    pub log: fn(&str),
    /// Names of the hooks that are installed, for the `ready` event.
    pub hooks: Vec<&'static str>,
    pub poolpatch: bool,
    pub version: &'static str,
}

/// The link to the mod: the module reads one file and writes the other.
struct Writer {
    events: EventWriter,
    failed_logged: bool,
}

pub struct Runtime {
    build: &'static BuildAddrs,
    base: usize,
    engine: Option<EngineFns>,
    log: fn(&str),
    /// Which hooks and patches ended up installed, for the `ready` event.
    installed: Mutex<(Vec<&'static str>, bool)>,
    version: &'static str,
    start: Instant,
    /// Added to the real time, for the tests.
    skew_ms: AtomicU64,
    /// Holding is on, or something is still waiting to go back: the add hook has work beyond counting.
    live: AtomicBool,
    machine: Mutex<HoldMachine<Box<Held>>>,
    reader: Mutex<Option<CtlReader>>,
    /// Control-file polls read by the command hook (to arm a `patch_next` at once) whose other lines the machine has not seen yet.
    ctl_backlog: Mutex<Vec<crate::link::Poll>>,
    writer: Mutex<Option<Writer>>,
    audit: Mutex<Audit>,
    counters: Counters,
    watch: AppliedWatch,
    orig_add: AtomicUsize,
    orig_apply: AtomicUsize,
    /// The service thread, to wake when the hook has queued an event.
    waker: OnceLock<Thread>,
    /// The mod's `own_next`: its next script build is its own send, not one to hold (`plan_add`).
    own_send: crate::hold::OwnSend,
    /// Reads the game time, when this build has a way to. None for now: a scheduled release is due at once.
    clock: Option<fn() -> Option<u64>>,
    /// The dev-only dump of build commands (`FEVERLINK3_DUMP_COMMANDS`); unset in a normal run.
    dump: OnceLock<Dumper>,
    /// The dev-only portable text of build commands (`FEVERLINK3_PORTABLE_DUMP`); unset in a normal run.
    portable: OnceLock<portable::Sink>,
    /// The dev-only rebuild self-check (`FEVERLINK3_PORTABLE_REBUILD`); unset in a normal run.
    rebuild: OnceLock<portable::Sink>,
    /// The input-level text of every held build command, for the mod (`input.rs`).
    input: crate::input::InputOut,
    /// `FEVERLINK3_NATIVE_INJECT`: the originator's full portable text of every held command (`native_text_<id>.txt`), and the receiver's order.
    text: crate::input::InputOut,
    /// The originator's refusal of a held command's text before it is shipped (`native_refused_<id>.txt`, the reason): every receiver
    /// would refuse the same text, so the mod fails the held command and ships nothing (`inject::text_refusal`).
    refused_text: crate::input::InputOut,
    inject: crate::inject::State,
    /// `FEVERLINK3_ACTING_PLAYER` (on unless `=0`, `acting.rs`): at an injection the text's owners that are its own player become the carrier's player.
    acting_player: AtomicBool,
    /// Terrain height edits shared at the stamp (`terrain.rs`): the tools' commands noted at `Add`, the carriers' orders.
    terrain: crate::terrain::State,
    /// Tree, plant and rock ticks shared at the stamp (`assets.rs`): the tools' commands noted at `Add`, the carriers' orders.
    assets: crate::assets::State,
    inject_settings: OnceLock<portable::Settings>,
    inject_dir_path: Mutex<Option<std::path::PathBuf>>,
    /// The commands the command list's `Add` carried an order out on (the interface half's script commands), by the order's number,
    /// until the simulation applies them: their timing and the deferred held command that takes their result (`inject::Added`).
    /// A command the simulation's apply hook matched to its order needs no record: the apply hands it over within the same call.
    /// Before 2026-10-15 these were single slots keyed by the payload address: a second build in the same step overwrote the first's
    /// link (its held command never completed) and a reused address could match another command.
    added: Mutex<std::collections::BTreeMap<u64, inject::Added>>,
    /// The engine-copy experiment (`FEVERLINK3_NATIVE_COPY`, track A); unset in a normal run.
    native_copy: OnceLock<portable::Sink>,
    /// `FEVERLINK3_NATIVE_PATCH` (track A, A2c): the armed `patch_next` order and its events.
    patch: crate::cmd_patch::State,
    /// The unhooked `builder.proposalCreate` emitter (`preview_event`), 0 when it is not hooked.
    preview_fn: AtomicUsize,
    /// The first five arguments of the newest tool preview (the interface and script objects the emitter needs), kept while holding is
    /// on; dropped whenever holding switches on or off (a resync loads another world: its objects are new).
    preview_args: Mutex<Option<[u64; 5]>>,
    /// Holding as `sync_flags` last saw it.
    preview_holding: AtomicBool,
    /// The three captures `[ui, y, z]` of the builder-preview listener of the newest game interface (`build_table::PreviewOwnerSite`),
    /// replaced when the next world's interface is made (never dropped at a switch of holding: they belong to the interface, not to
    /// holding). The emitter's arguments are computed from them when a held command comes before any tool preview of the world.
    preview_owner: Mutex<Option<[u64; 3]>>,
    /// Those computed arguments compared with a real tool preview of the same interface: OWNER_UNCHECKED, OWNER_EQUAL, or
    /// OWNER_DIFFERENT (then they are not used until the next interface).
    preview_owner_check: AtomicU8,
    /// What went wrong in a way that may let this game differ from the others (`failsafe.rs`), until the service thread writes it.
    faults: crate::failsafe::Faults,
    /// Service-thread rounds in a row that panicked.
    pump_panics: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A lock that is almost never contended; a hook may not wait for it, but a few spins cost nothing.
fn try_lock_briefly<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    for _ in 0..200 {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => std::hint::spin_loop(),
        }
    }
    None
}

/// `Runtime::preview_owner_check`: not compared with a tool preview yet, being compared, equal, different.
const OWNER_UNCHECKED: u8 = 0;
const OWNER_CHECKING: u8 = 1;
const OWNER_EQUAL: u8 = 2;
const OWNER_DIFFERENT: u8 = 3;

/// The address range of the game's image at `base` (`SizeOfImage` of its PE header, guarded reads).
fn image_range(base: usize) -> Option<core::ops::Range<usize>> {
    let pe = crate::memguard::read_value::<u32>(base.checked_add(0x3c)?)? as usize;
    // PE signature (4) + COFF header (20) + `SizeOfImage` at 0x38 of the optional header
    let size = crate::memguard::read_value::<u32>(base.checked_add(pe + 4 + 20 + 0x38)?)? as usize;
    Some(base..base.checked_add(size)?)
}

/// The getter the third capture `z` of the listener calls (`z->vtable[slot]`), which must lie in the game's image `code`.
fn preview_getter(
    z: u64,
    site: &crate::build_table::PreviewOwnerSite,
    code: core::ops::Range<usize>,
) -> Result<usize, String> {
    let vtable = crate::memguard::word(z as usize).ok_or_else(|| format!("the capture {z:#x} cannot be read"))?;
    let f = crate::memguard::word((vtable as usize).wrapping_add(8 * site.z_getter_slot))
        .ok_or_else(|| format!("the vtable {vtable:#x} cannot be read"))? as usize;
    if !code.contains(&f) {
        return Err(format!("the getter {f:#x} is not in the game's image"));
    }
    Ok(f)
}

/// The emitter's first five arguments from the listener's captures `[ui, y, z]` and what `z`'s getter returned (`r`).
fn preview_args_from(
    [ui, y, _]: [u64; 3],
    r: u64,
    site: &crate::build_table::PreviewOwnerSite,
) -> Result<[u64; 5], String> {
    let at = |base: u64, off: usize| {
        crate::memguard::word((base as usize).wrapping_add(off))
            .ok_or_else(|| format!("{base:#x}+{off:#x} cannot be read"))
    };
    Ok([at(y, site.y_a)?, at(y, site.y_b)?, at(r, site.r_c)?, at(r, site.r_dispatcher)?, at(ui, site.ui_e)?])
}

impl Runtime {
    pub fn new(init: Init) -> Runtime {
        let table = init.build.kinds.iter().map(|(n, k)| (n.to_string(), *k)).collect();
        Runtime {
            build: init.build,
            base: init.base,
            engine: init.engine,
            log: init.log,
            installed: Mutex::new((init.hooks, init.poolpatch)),
            version: init.version,
            start: Instant::now(),
            skew_ms: AtomicU64::new(0),
            live: AtomicBool::new(false),
            machine: Mutex::new(HoldMachine::new(init.hold, table)),
            reader: Mutex::new(None),
            ctl_backlog: Mutex::new(Vec::new()),
            writer: Mutex::new(None),
            audit: Mutex::new(Audit::new(AUDIT_INTERVAL_MS, 0)),
            counters: Counters::new(),
            watch: AppliedWatch::new(),
            orig_add: AtomicUsize::new(0),
            orig_apply: AtomicUsize::new(0),
            waker: OnceLock::new(),
            own_send: crate::hold::OwnSend::default(),
            clock: None,
            dump: OnceLock::new(),
            portable: OnceLock::new(),
            rebuild: OnceLock::new(),
            native_copy: OnceLock::new(),
            patch: Default::default(),
            input: crate::input::InputOut::default(),
            text: crate::input::InputOut::with_prefix("native_text_"),
            refused_text: crate::input::InputOut::with_prefix("native_refused_"),
            inject: Default::default(),
            acting_player: AtomicBool::new(false),
            terrain: Default::default(),
            assets: Default::default(),
            inject_settings: OnceLock::new(),
            inject_dir_path: Mutex::new(None),
            added: Mutex::new(Default::default()),
            preview_fn: AtomicUsize::new(0),
            preview_args: Mutex::new(None),
            preview_holding: AtomicBool::new(false),
            preview_owner: Mutex::new(None),
            preview_owner_check: AtomicU8::new(OWNER_UNCHECKED),
            faults: crate::failsafe::Faults::new(),
            pump_panics: AtomicU64::new(0),
        }
    }

    /// Fail loud (`failsafe.rs`): the log line now, the `fault` event with the next round of the service thread. Calls nothing of
    /// the game, so it may be called inside a guard as well as outside.
    pub(crate) fn fault(&self, place: &'static str, action: crate::failsafe::Action, detail: impl Into<String>) {
        if let Some(f) = self.faults.trip(place, action, detail) {
            crate::log::guarded("fault report", || self.say(&f.line()));
            self.wake();
        }
    }

    /// One round of the service thread ([`Runtime::pump`]) with its panic contained. A panicking round may have lost events the mod
    /// waits for (a hold, a completion): a repair fault; three rounds in a row, a stop fault (the link no longer works).
    pub fn pump_round(&self) -> usize {
        match crate::log::guarded("service thread", || self.pump()) {
            Some(n) => {
                self.pump_panics.store(0, Relaxed);
                n
            }
            None => {
                let k = self.pump_panics.fetch_add(1, Relaxed) + 1;
                let action = if k >= 3 { crate::failsafe::Action::Stop } else { crate::failsafe::Action::Repair };
                self.fault(
                    "service thread",
                    action,
                    format!(
                        "a panic in a round of the service thread ({k} in a row): the events of that round may be lost"
                    ),
                );
                0
            }
        }
    }

    /// Switches the dev-only command dump on (`FEVERLINK3_DUMP_COMMANDS`), writing to `dir` when it is known.
    pub fn enable_dump(&self, settings: dump::Settings, dir: Option<&Path>) {
        let dumper = Dumper::new(settings);
        if let Some(dir) = dir {
            dumper.set_dir(dir);
        }
        if self.dump.set(dumper).is_ok() {
            self.say(&format!(
                "dump: FEVERLINK3_DUMP_COMMANDS is on: build commands are decoded into {} (read-only; script commands {}; at most {} held and {} apply/applied dumps)",
                dump::DUMP_FILE,
                if settings.scripts { "included" } else { "skipped" },
                settings.max_held,
                settings.max_applied
            ));
        }
    }

    /// Switches the dev-only portable text on (`FEVERLINK3_PORTABLE_DUMP`), writing to `dir` when it is known.
    pub fn enable_portable(&self, settings: portable::Settings, dir: Option<&Path>) {
        let sink = portable::Sink::new(settings);
        if let Some(dir) = dir {
            sink.set_dir(dir);
        }
        if self.portable.set(sink).is_ok() {
            self.say(&format!(
                "portable: FEVERLINK3_PORTABLE_DUMP is on: build commands are written as portable text into {} (read-only; script commands {}; layout table for game build {})",
                portable::PORTABLE_FILE,
                if settings.scripts { "included" } else { "skipped" },
                portable::LAYOUT_BUILD
            ));
        }
    }

    /// Switches the rebuild self-check on (`FEVERLINK3_PORTABLE_REBUILD`): a held command is written as portable text, a
    /// payload is built again from the text in memory from the game allocator, compared, and the memory given back.
    pub fn enable_rebuild(&self, settings: portable::Settings, dir: Option<&Path>) {
        let sink = portable::Sink::named(settings, rebuild::REBUILD_FILE);
        if let Some(dir) = dir {
            sink.set_dir(dir);
        }
        if self.rebuild.set(sink).is_ok() {
            self.say(&format!(
                "rebuild: FEVERLINK3_PORTABLE_REBUILD is on: held build commands are rebuilt from their portable text with the game allocator as a self-check, report in {} (nothing is handed to the engine; script commands {})",
                rebuild::REBUILD_FILE,
                if settings.scripts { "included" } else { "skipped" }
            ));
        }
    }

    /// The rebuild self-check of the payload at `payload`: report into the rebuild file. The rebuild allocates with the
    /// game's `operator new`, so it runs outside `log::guarded`; the text before it and the report after it are guarded.
    /// Never call this inside a guard.
    fn rebuild_check(&self, event: &str, payload: usize, hold_id: Option<u64>) {
        let Some(sink) = self.rebuild.get() else { return };
        crate::log::calls_engine("rebuild check (operator new and delete)");
        let prepared = crate::log::guarded("rebuild check", || {
            if !sink.has_dir() {
                return None;
            }
            let seq = sink.begin()?;
            let input = (|| -> Result<(rebuild::EngineAlloc, String, Vec<u8>), String> {
                let alloc = rebuild::EngineAlloc::at(self.base, &self.build.engine)?;
                let head = portable::Header {
                    seq,
                    event,
                    hold_id,
                    site_rva: None,
                    tool: None,
                    t_ms: self.now_ms(),
                    wall_ms: 0,
                    payload,
                };
                let env = Env { image_base: self.base };
                let text = portable::encode(&dump::LiveMem, env, sink.settings, self.build.layout_name(), &head)?.text;
                let original =
                    dump::Mem::read(&dump::LiveMem, payload, dump::PAYLOAD_SIZE).ok_or("payload unreadable")?;
                Ok((alloc, text, original))
            })();
            Some((seq, input))
        });
        let Some((seq, input)) = prepared.flatten() else { return };
        let report = input.and_then(|(alloc, text, original)| {
            // the same rebuild first as a dry pass inside a guard (`native_copy::dry_pass`): a panic of ours is a refusal
            let place = |b: &[u8]| Some(b.as_ptr() as usize);
            crate::native_copy::dry_pass(
                "rebuild check: dry pass",
                &dump::LiveMem,
                self.base,
                sink.settings,
                self.build.layout_name(),
                |c| {
                    let _ = rebuild::roundtrip(&text, &original, self.base, sink.settings, c.alloc, c.mem, &place);
                },
            )?;
            let r = rebuild::roundtrip(&text, &original, self.base, sink.settings, &alloc, &dump::LiveMem, &|b| {
                Some(b.as_ptr() as usize)
            });
            let freed = rebuild::Alloc::free_all(&alloc);
            r.map(|r| format!("{}FREED {freed} blocks\n", r.text))
        });
        crate::log::guarded("rebuild check (report)", || {
            let key = hold_id.map_or("none".to_string(), |k| k.to_string());
            let text = match report {
                Ok(t) => {
                    format!(
                        "=== rebuild check #{seq} event={event} key={key} ===\n{t}=== end rebuild check #{seq} ===\n"
                    )
                }
                Err(why) => format!("=== rebuild check #{seq} event={event} key={key} REFUSED: {why} ===\n"),
            };
            if let Err(e) = sink.write(&text) {
                self.say(&format!("rebuild: cannot write {}: {e}", rebuild::REBUILD_FILE));
            }
        });
    }

    /// The portable text of the payload at `payload` (or why it is refused) into the portable file.
    /// The input-level text of the build command at `payload`; `None` (and a line in the log) when the layout table refuses it.
    fn input_text(&self, payload: usize) -> Option<String> {
        let head = portable::Header {
            seq: 0,
            event: "input",
            hold_id: None,
            site_rva: None,
            tool: None,
            t_ms: self.now_ms(),
            wall_ms: 0,
            payload,
        };
        match portable::encode(
            &LiveMem,
            Env { image_base: self.base },
            portable::Settings::input_level(),
            self.build.layout_name(),
            &head,
        ) {
            Ok(p) => Some(p.text),
            Err(why) => {
                self.say(&format!("input: the layout table refuses this command: {why}"));
                None
            }
        }
    }

    fn portable_payload(&self, event: &str, payload: usize, hold_id: Option<u64>, site: Option<u32>) {
        let Some(sink) = self.portable.get() else { return };
        if !sink.has_dir() {
            return;
        }
        let Some(seq) = sink.begin() else { return };
        let tool =
            site.and_then(|rva| self.build.ui_site_index(rva)).map(|t| dump::TOOL_NAMES.get(t).copied().unwrap_or("?"));
        let head = portable::Header {
            seq,
            event,
            hold_id,
            site_rva: site,
            tool,
            t_ms: self.now_ms(),
            wall_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.as_millis()),
            payload,
        };
        let text = match portable::encode(
            &LiveMem,
            Env { image_base: self.base },
            sink.settings,
            self.build.layout_name(),
            &head,
        ) {
            Ok(p) => p.text,
            Err(why) => format!(
                "=== portable v{} REFUSED event={event} key={} seq={seq} payload={payload:#x}: {why} ===
",
                portable::VERSION,
                hold_id.map_or("none".to_string(), |k| k.to_string())
            ),
        };
        if let Err(e) = sink.write(&text) {
            self.say(&format!("portable: cannot write {}: {e}", portable::PORTABLE_FILE));
        }
    }

    /// One dump of the payload at `payload`; returns its sequence number, `None` when switched off, out of budget or
    /// without a folder. Reads only through [`LiveMem`], which checks every range.
    #[allow(clippy::too_many_arguments)]
    fn dump_payload(
        &self,
        event: &str,
        payload: usize,
        hold_id: Option<u64>,
        site: Option<u32>,
        scope: Scope,
        budget: Budget,
        group: Option<u32>,
    ) -> Option<u32> {
        let d = self.dump.get()?;
        if !d.has_dir() {
            return None;
        }
        let seq = d.begin(budget)?;
        let kind = self
            .build
            .kind_offset
            .and_then(|o| payload.checked_add(o as usize))
            .and_then(crate::memguard::byte)
            .unwrap_or(255);
        let head = Header {
            seq,
            event,
            hold_id,
            site_rva: site,
            tool: site.and_then(|rva| self.build.ui_site_index(rva)),
            kind,
            kind_name: self.build.kind_name(kind).unwrap_or("?"),
            t_ms: self.now_ms(),
            wall_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |t| t.as_millis()),
            thread: current_thread_id(),
            payload,
            group,
        };
        let text = dump::render(&LiveMem, Env { image_base: self.base }, d.settings, &head, scope);
        if let Err(e) = d.write(&text) {
            self.say(&format!("dump: cannot write {}: {e}", dump::DUMP_FILE));
        }
        let _ = d.write_index(&format!(
            "seq={seq} event={event} key={} site={} tool={} payload={payload:#x} bytes={} wall_ms={}",
            hold_id.map_or("none".to_string(), |k| k.to_string()),
            site.map_or("none".to_string(), |s| format!("{s:#x}")),
            head.tool.map_or("none".to_string(), |t| dump::TOOL_NAMES.get(t).copied().unwrap_or("?").to_string()),
            text.len(),
            head.wall_ms
        ));
        Some(seq)
    }

    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// Records what the installation really did; the `ready` event is written afterwards.
    pub fn set_installed(&self, hooks: Vec<&'static str>, poolpatch: bool) {
        *lock(&self.installed) = (hooks, poolpatch);
    }

    /// The unhooked functions the hooks call: (`Add`, `Apply`); 0 where not published.
    pub fn original_addresses(&self) -> (usize, usize) {
        (self.orig_add.load(Acquire), self.orig_apply.load(Acquire))
    }

    pub fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64 + self.skew_ms.load(Relaxed)
    }

    /// Lets time pass without waiting (tests).
    pub fn advance(&self, ms: u64) {
        self.skew_ms.fetch_add(ms, Relaxed);
    }

    /// Where the hooks find the unhooked functions; set before a hook's jump is written.
    pub fn set_originals(&self, add: Option<AddFn>, apply: Option<ApplyFn>) {
        if let Some(f) = add {
            self.orig_add.store(f as usize, Release);
        }
        if let Some(f) = apply {
            self.orig_apply.store(f as usize, Release);
        }
    }

    /// The same, by address (what the detour installer gives back).
    pub fn set_original_addresses(&self, add: usize, apply: usize) {
        if add != 0 {
            self.orig_add.store(add, Release);
        }
        if apply != 0 {
            self.orig_apply.store(apply, Release);
        }
    }

    /// The unhooked `builder.proposalCreate` emitter (`preview_event`); 0 leaves held commands without a preview of their own.
    pub fn set_preview_original(&self, f: usize) {
        self.preview_fn.store(f, Release);
    }

    /// Called by the hook on the emitter for every tool preview: its first five arguments are what a held command's preview needs.
    pub fn note_preview_args(&self, args: [u64; 5]) {
        if !self.preview_holding.load(Relaxed) {
            return;
        }
        if let Some(mut g) = try_lock_briefly(&self.preview_args) {
            *g = Some(args);
        }
    }

    /// Called by the hook where a new game interface makes its builder-preview listener: `captures` points at the lambda's three
    /// captures (guarded reads; nothing of the game is called).
    pub fn note_preview_owner(&self, captures: usize) {
        let read = |i: usize| crate::memguard::word(captures.wrapping_add(8 * i));
        let (Some(ui), Some(y), Some(z)) = (read(0), read(1), read(2)) else {
            self.say("preview: a game interface was made, but its builder-preview listener's captures could not be read: a held command before its first tool preview gets no preview of its own");
            return;
        };
        *lock(&self.preview_owner) = Some([ui, y, z]);
        self.preview_owner_check.store(OWNER_UNCHECKED, Relaxed);
        self.say(&format!(
            "preview: game interface {ui:#x} made (its builder-preview listener noted): a held command gets its own preview from it also before the first tool preview of this world"
        ));
    }

    /// The emitter's first five arguments as the newest game interface's listener computes them (`build_table::PreviewOwnerSite`).
    ///
    /// # Safety
    /// Calls the engine (the third capture's getter, as the listener does): only on the interface thread, outside `log::guarded`.
    unsafe fn derived_preview_args(&self) -> Result<[u64; 5], String> {
        let site = self.build.preview_owner.as_ref().ok_or("this build has no preview_owner site")?;
        if self.preview_owner_check.load(Relaxed) == OWNER_DIFFERENT {
            return Err("the arguments computed from the game interface differed from its real tool preview".into());
        }
        let caps = crate::log::guarded("preview owner", || *lock(&self.preview_owner))
            .flatten()
            .ok_or("no game interface was made since this module was loaded")?;
        let code = image_range(self.base).ok_or("the game's image size cannot be read")?;
        let getter = preview_getter(caps[2], site, code)?;
        type Getter = unsafe extern "system-unwind" fn(u64) -> u64;
        crate::log::calls_engine("the game interface's getter (the arguments of a held command's own preview)");
        // SAFETY: a function of the game's image read from the capture's vtable, called with the capture as `this`, as the listener does.
        let r = unsafe { core::mem::transmute::<usize, Getter>(getter)(caps[2]) };
        preview_args_from(caps, r, site)
    }

    /// Once per game interface, at a real tool preview: are the arguments computed from the interface's listener the ones the tool gave?
    /// Different, they are never used for this interface (a held command before a tool preview then has no preview of its own).
    ///
    /// # Safety
    /// As [`Runtime::derived_preview_args`]: inside the listener's call on the interface thread, outside `log::guarded`.
    pub unsafe fn check_preview_owner(&self, given: [u64; 5]) {
        if self.build.preview_owner.is_none()
            || self.preview_owner_check.compare_exchange(OWNER_UNCHECKED, OWNER_CHECKING, AcqRel, Relaxed).is_err()
        {
            return;
        }
        if crate::log::guarded("preview owner", || lock(&self.preview_owner).is_none()).unwrap_or(true) {
            self.preview_owner_check.store(OWNER_UNCHECKED, Relaxed);
            return;
        }
        let derived = unsafe { self.derived_preview_args() };
        let (state, line) = match derived {
            Ok(a) if a == given => {
                (OWNER_EQUAL, "preview: the arguments computed from the game interface equal its tool preview's".to_string())
            }
            Ok(a) => (
                OWNER_DIFFERENT,
                format!("preview: the arguments computed from the game interface DIFFER from its tool preview's ({a:x?} vs {given:x?}): not used for this interface"),
            ),
            Err(why) => (OWNER_DIFFERENT, format!("preview: the arguments could not be computed from the game interface ({why}): not used for this interface")),
        };
        self.preview_owner_check.store(state, Relaxed);
        crate::log::guarded("preview owner", || self.say(&line));
    }

    /// Gives the held build command `id` to the game scripts as the preview of the tool `held:<id>`, through the engine's own emitter of
    /// `builder.proposalCreate` with the arguments of the newest tool preview, its own Proposal (the payload's start) and the ProposalData
    /// slot of its payload (`+0x3d8`, empty before the apply). The scripts' `guiHandleEvent` runs inside this call, so the mod has the
    /// record of the command before the `held` event reaches it. Why: a tool that sends no preview (UI::ModuleBuilder, a station module
    /// added via Configure: run s4 2026-10-14) left the mod without a record before the build, so its command was released at once and
    /// applied 7 steps before the stamp on this game alone (entity ids then differ between the games). Called on the thread of the tool
    /// that added the command (the interface's), where the tools call the emitter themselves. The arguments are those of the newest
    /// tool preview since holding was switched on; before the first one of a world they are computed from the game interface's
    /// listener (`note_preview_owner`; friend test 2026-10-21: seven station configures right after a resync load had none, refused).
    ///
    /// # Safety
    /// `payload` is the payload of a live held `build_proposal` command.
    #[allow(improper_ctypes_definitions)]
    unsafe extern "system-unwind" fn give_preview(&self, id: crate::hold::HoldId, payload: usize) {
        type Emit = unsafe extern "system-unwind" fn(u64, u64, u64, u64, u64, *const u8, usize, usize, *mut u8) -> u64;
        let emit = self.preview_fn.load(Acquire);
        if emit == 0 {
            crate::log::line(&format!("hold: command {id}: no preview of its own (the emitter is not hooked)"));
            return;
        }
        let args = crate::log::guarded("preview of a held command", || *lock(&self.preview_args)).flatten();
        let (a, source) = match args {
            Some(a) => (a, "the newest tool preview"),
            // SAFETY: inside the Add hook on the interface thread, outside any guard
            None => match unsafe { self.derived_preview_args() } {
                Ok(a) => (a, "the game interface's listener: no tool preview since holding was switched on"),
                Err(why) => {
                    crate::log::line(&format!(
                        "hold: command {id}: no preview of its own (no tool preview seen since holding was switched on, and {why})"
                    ));
                    return;
                }
            },
        };
        // a std::string in its short form (MSVC x64: 16 bytes of text, size, capacity 15)
        let name = format!("held:{id}");
        let mut text = [0u64; 4];
        let bytes = name.as_bytes();
        if bytes.len() > 15 {
            return;
        }
        // SAFETY: 32 writable bytes; the text and its terminator fit the first 16.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), text.as_mut_ptr() as *mut u8, bytes.len());
        }
        text[2] = bytes.len() as u64;
        text[3] = 15;
        let mut handled = 0u8;
        crate::log::line(&format!(
            "hold: command {id}: giving it to the game scripts as the preview of the tool {name} (emitter arguments from {source})"
        ));
        // SAFETY: the emitter of this build (prologue checked when it was hooked), with the objects of the newest tool preview of this
        // session, the held command's Proposal and its ProposalData slot; the call into the scripts is synchronous.
        let emit: Emit = unsafe { core::mem::transmute::<usize, Emit>(emit) };
        crate::log::calls_engine("preview of a held command (the emitter, the game scripts)");
        unsafe {
            emit(
                a[0],
                a[1],
                a[2],
                a[3],
                a[4],
                text.as_ptr() as *const u8,
                payload,
                payload + PAYLOAD_RESULT_DATA,
                &mut handled,
            )
        };
        crate::log::line(&format!(
            "hold: command {id}: given to the game scripts as the preview of the tool {name} (handled {handled})"
        ));
    }

    /// The thread to wake when the add hook queued something for the mod.
    pub fn set_waker(&self, thread: Thread) {
        let _ = self.waker.set(thread);
    }

    fn say(&self, text: &str) {
        (self.log)(text);
    }

    fn game_time(&self) -> Option<u64> {
        self.clock.and_then(|read| read())
    }

    fn wake(&self) {
        if let Some(t) = self.waker.get() {
            t.unpark();
        }
    }

    fn sync_flags(&self, machine: &HoldMachine<Box<Held>>) {
        self.live.store(machine.enabled() || !machine.is_empty(), Relaxed);
        let on = machine.enabled();
        if self.preview_holding.swap(on, Relaxed) != on {
            *lock(&self.preview_args) = None;
        }
    }

    // ---- the link

    /// Opens the control link in a session folder and tells the mod what is active (`ready`). Everything the
    /// control file holds at this moment is history from an earlier run and is not acted on.
    pub fn open_link(&self, dir: &Path) -> std::io::Result<()> {
        let reader = CtlReader::writers_at_end(dir)?;
        let mut events = EventWriter::open(dir)?;
        let (hooks, poolpatch) = lock(&self.installed).clone();
        let mut hook_names: Vec<String> = hooks.iter().map(|h| h.to_string()).collect();
        if self.patch_ready() {
            hook_names.push(crate::cmd_patch::READY_NAME.to_string()); // the mod reads this string to know it may send `patch_next`
        }
        if self.inject_ready() {
            hook_names.push(crate::inject::READY_NAME.to_string()); // and this one, `inject_next`
            hook_names.push("defer".to_string()); // and `defer` + `inject_next {held}`: the deferred completion of a held command
            hook_names.push("fail".to_string()); // and `fail` + `complete_next`: a deferred command never waits for the age limit
            if self.acting_player.load(Relaxed) {
                hook_names.push(crate::acting::READY_NAME.to_string()); // an injected build is owned by its carrier's player
            }
        }
        if self.terrain_ready() {
            hook_names.push(crate::terrain::READY_NAME.to_string()); // `terrain_share` and `terrain_apply`: height edits at the stamp
        }
        if self.assets_ready() {
            hook_names.push(crate::assets::READY_NAME.to_string()); // `assets_apply`: tree, plant and rock ticks at the stamp
        }
        if crate::iplayer::STATE.armed() {
            hook_names.push(crate::iplayer::READY_NAME.to_string()); // `interface_player`: the interface acts as this game's company
        }
        if crate::autosave::STATE.armed() {
            hook_names.push(crate::autosave::READY_NAME.to_string()); // `autosave`: the games autosave together at the host's stamp
        }
        let ready = Event::Ready {
            build: self.build.name.to_string(),
            version: self.version.to_string(),
            hooks: hook_names,
            poolpatch,
            clock: self.clock.is_some(),
            kinds: self.build.kinds.iter().map(|(n, k)| (n.to_string(), *k)).collect(),
            sites: self.build.ui_add_sites.len(),
        };
        let first = events.end();
        events.append(std::slice::from_ref(&ready))?;
        self.say(&format!(
            "link: opened {} (control files: {}; events from {first})",
            dir.display(),
            reader.describe()
        ));
        if let Some(line) = ready.log_line() {
            self.say(&line);
        }
        // the wall clock and the C runtime's clock at one instant: the mod's `os.clock()` becomes wall-clock time with it
        let clock =
            Event::Timing { what: "clock", id: 0, detail: format!("wall={} clock={}", wall_ms(), crt_clock_ms()) };
        events.append(std::slice::from_ref(&clock))?;
        if let Some(line) = clock.log_line() {
            self.say(&line);
        }
        *lock(&self.reader) = Some(reader);
        *lock(&self.writer) = Some(Writer { events, failed_logged: false });
        self.input.set_dir(dir);
        self.text.set_dir(dir);
        self.refused_text.set_dir(dir);
        self.inject_dir(dir);
        if let Some(d) = self.dump.get() {
            d.set_dir(dir);
        }
        if let Some(p) = self.portable.get() {
            p.set_dir(dir);
        }
        if let Some(p) = self.rebuild.get() {
            p.set_dir(dir);
        }
        if let Some(p) = self.native_copy.get() {
            p.set_dir(dir);
        }
        Ok(())
    }

    /// Reads whatever the mod has added to the control file and applies it to the machine.
    fn ingest(&self, machine: &mut HoldMachine<Box<Held>>, now: u64) {
        let backlog = std::mem::take(&mut *lock(&self.ctl_backlog));
        for poll in backlog {
            self.apply_poll(machine, poll, now);
        }
        let Some(mut guard) = try_lock_briefly(&self.reader) else { return };
        let Some(reader) = guard.as_mut() else { return };
        if !reader.has_news() {
            return;
        }
        let mut poll = reader.poll();
        // the orders are armed before the reader is let go: see `arm_orders`
        self.arm_orders(&mut poll, now);
        drop(guard);
        self.apply_poll(machine, poll, now);
    }

    /// Arms the `patch_next` / `inject_next` orders of `poll` and leaves the other lines in it. Called while the reader's lock is held, so
    /// that an order read from the file is armed before anyone else can look at the file: once the reader has handed out a line, the
    /// file has no news any more, and the command hook (`ingest_patch_order`) would find neither news nor an armed order. That gap
    /// (service thread between `poll` and `apply_poll`) let the mod's replay command pass uninjected in real run rt1 R2 (2026-10-14:
    /// `inject_next key=2` written before the command, the module never saw the command with the order armed, then the order expired
    /// on town builds) while key 1 a minute earlier was injected.
    fn arm_orders(&self, poll: &mut crate::link::Poll, now: u64) {
        let mut rest = Vec::new();
        for line in std::mem::take(&mut poll.lines) {
            match parse_ctl_line(&line) {
                Ok(crate::link::CtlMessage::PatchNext { frozen, adds, at, ttl }) => {
                    self.arm_patch(frozen, adds, at, ttl, now)
                }
                Ok(crate::link::CtlMessage::InjectNext { key, ttl, held, tm, refuse, refund }) => {
                    self.inject.arm_timed(key, ttl, held, tm, refuse, refund, now)
                }
                Ok(crate::link::CtlMessage::CompleteNext { held, ttl, tm }) => self.inject.arm_link(held, ttl, tm, now),
                Ok(crate::link::CtlMessage::SendDone { key, held }) => self.inject.close(key, held),
                Ok(crate::link::CtlMessage::OwnNext) => self.own_send.arm(now),
                Ok(crate::link::CtlMessage::TerrainApply { key, x0, y0, w, h, ttl }) => {
                    self.terrain_arm(key, crate::terrain::Header { x0, y0, w, h }, ttl, now)
                }
                Ok(crate::link::CtlMessage::AssetsApply { key, ids, o2n, ttl }) => {
                    self.assets_arm(key, ids, o2n, ttl, now)
                }
                Ok(crate::link::CtlMessage::TerrainShare { on }) => self.terrain_share(on),
                _ => rest.push(line),
            }
        }
        poll.lines = rest;
    }

    fn apply_poll(&self, machine: &mut HoldMachine<Box<Held>>, poll: crate::link::Poll, now: u64) {
        for line in &poll.lines {
            match parse_ctl_line(line) {
                Ok(crate::link::CtlMessage::PatchNext { frozen, adds, at, ttl }) => {
                    self.arm_patch(frozen, adds, at, ttl, now)
                }
                Ok(crate::link::CtlMessage::InjectNext { key, ttl, held, tm, refuse, refund }) => {
                    self.inject.arm_timed(key, ttl, held, tm, refuse, refund, now)
                }
                Ok(crate::link::CtlMessage::CompleteNext { held, ttl, tm }) => self.inject.arm_link(held, ttl, tm, now),
                Ok(crate::link::CtlMessage::SendDone { key, held }) => self.inject.close(key, held),
                Ok(crate::link::CtlMessage::OwnNext) => self.own_send.arm(now),
                Ok(crate::link::CtlMessage::TerrainApply { key, x0, y0, w, h, ttl }) => {
                    self.terrain_arm(key, crate::terrain::Header { x0, y0, w, h }, ttl, now)
                }
                Ok(crate::link::CtlMessage::AssetsApply { key, ids, o2n, ttl }) => {
                    self.assets_arm(key, ids, o2n, ttl, now)
                }
                Ok(crate::link::CtlMessage::TerrainShare { on }) => self.terrain_share(on),
                Ok(crate::link::CtlMessage::HoldOff) => {
                    self.terrain_share(false); // the session's end (or a resync load): the tools apply as they are again
                    machine.handle(&crate::link::CtlMessage::HoldOff, now)
                }
                Ok(crate::link::CtlMessage::InterfacePlayer { entity }) => {
                    let ev = self.interface_player_order(&crate::iplayer::STATE, entity);
                    machine.push_event(ev);
                }
                Ok(crate::link::CtlMessage::Autosave { hold, now: run }) => {
                    let ev = self.autosave_order(&crate::autosave::STATE, hold, run);
                    machine.push_event(ev);
                    machine.handle(&crate::link::CtlMessage::Autosave { hold, now: run }, now);
                }
                Ok(crate::link::CtlMessage::Pace { permille }) => {
                    crate::pace::PACER.set(permille, crate::pace::clock_ms());
                    // the mod speaks: what is held keeps its liveness as with any other line
                    machine.handle(&crate::link::CtlMessage::Pace { permille }, now);
                }
                Ok(msg) => machine.handle(&msg, now),
                Err(e) => machine.error("ctl", format!("line {}: {e}", line.kind), now),
            }
        }
        if poll.discarded > 0 {
            machine.error("ctl", format!("{} control line(s) discarded", poll.discarded), now);
        }
        // a Lua state made a control writer (docs/NATIVE_LINK.md section 3): which state, from the from column of its first line, for the question how many
        // simulation states the game runs
        for w in &poll.new_writers {
            self.say(&format!("link: control writer {w} appeared"));
        }
        if let Some(e) = poll.error {
            machine.error("ctl", e, now);
        }
        if poll.gone {
            self.terrain_share(false);
            // the session folder was cleaned (the session is over): the interface acts as the savegame's player again
            if crate::iplayer::STATE.wanted().is_some() {
                let ev = self.interface_player_order(&crate::iplayer::STATE, None);
                machine.push_event(ev);
            }
            machine.ctl_gone(now);
        }
    }

    /// The mod's `interface_player` order (`iplayer.rs`), and the event that answers it. Refused when the piece is not armed (then the
    /// mod was never told it may send it: an old mod, or a line of a test).
    fn interface_player_order(&self, state: &crate::iplayer::State, entity: Option<u32>) -> Event {
        let what = entity.map_or("clear: back to the savegame's player".to_string(), |e| format!("act as entity {e}"));
        if !state.armed() {
            self.say(&format!("interface player: order '{what}' REFUSED: the piece is not installed on this game"));
            return state.event("the piece is not installed on this game");
        }
        let before = state.wanted();
        state.set(entity);
        if before != entity {
            self.say(&format!(
                "interface player: order '{what}' (savegame player {}): the front state takes it at the next Sync, the tools at the next world load",
                state.savegame().map_or("not read yet".to_string(), |p| p.to_string())
            ));
        }
        state.event("")
    }

    /// The mod's `autosave` line (`autosave.rs`), and the event that answers it (with this game's interval, for the host's stamps).
    /// Refused when the piece is not armed: the game then keeps its own autosave.
    fn autosave_order(&self, state: &crate::autosave::State, hold: bool, run: bool) -> Event {
        let clock = crate::pace::clock_ms();
        if !state.armed() {
            if run {
                self.say("sync autosave: 'now' REFUSED: the piece is not installed on this game (its own autosave runs as before)");
            }
            return state.event(clock, "the piece is not installed on this game");
        }
        let was = state.held(clock);
        state.order(hold, run, clock);
        if was != hold {
            self.say(if hold {
                "sync autosave: HOLD on: the game's own autosave timer is held off while the session runs"
            } else {
                "sync autosave: hold off: the game's own autosave runs as before"
            });
        }
        if run {
            self.say("sync autosave: 'now' (the host's stamp): the game's own autosave runs at the next frame of the game interface");
        }
        state.event(clock, "")
    }

    /// For the command hook, which needs a `patch_next` the mod wrote an instant ago and cannot wait for the service thread (nor for
    /// the machine's lock, nor for holding to be on): reads the control file, arms the order, and keeps the other lines for the
    /// next `ingest`. Takes no lock but the reader's and the backlog's.
    pub(super) fn ingest_patch_order(&self, now: u64) {
        // Waits for the reader instead of giving up after a few spins: the service thread holds it only while it reads the file (and
        // arms the orders it finds there), and giving up here meant the mod's own command could pass while its order sat in the file.
        let mut guard = lock(&self.reader);
        let Some(reader) = guard.as_mut() else { return };
        if !reader.has_news() {
            return;
        }
        let mut poll = reader.poll();
        self.arm_orders(&mut poll, now);
        drop(guard);
        lock(&self.ctl_backlog).push(poll);
        self.wake();
    }

    // ---- the service thread

    /// One round of the service thread: ingest, let time pass, write the events. Returns how many were written.
    pub fn pump(&self) -> usize {
        let now = self.now_ms();
        let mut events = {
            let mut machine = lock(&self.machine);
            self.ingest(&mut machine, now);
            // an order of a deferred command that no command will use: the command fails now (tool callback fired), not after 30 s
            for held in self.inject.take_failed(now) {
                self.say(&format!(
                    "hold: deferred command {held}: its order was refused before any command used it: failed now"
                ));
                machine.handle(&crate::link::CtlMessage::Fail { id: held }, now);
            }
            machine.tick(now);
            self.sync_flags(&machine);
            machine.drain_events()
        };
        while let Some((id, ok)) = self.watch.next_result() {
            events.push(Event::Applied { id, ok });
        }
        events.extend(self.patch.drain_events(now));
        self.terrain_expire(now);
        self.assets_expire(now);
        events.extend(self.inject.drain_events(now));
        for id in self.watch.expire(now) {
            events.push(Event::Error {
                place: "applied".into(),
                detail: format!("command {id} was released but never seen being applied"),
            });
        }
        // the interface player (iplayer.rs): a world start, a new front value or a refusal, said from this thread (never from a hook)
        let (lines, tell) = crate::iplayer::STATE.news();
        for line in lines {
            self.say(&line);
        }
        if tell {
            events.push(crate::iplayer::STATE.event(""));
        }
        // the synchronised autosave (autosave.rs): the interval read here, the hooks' counts said from this thread
        crate::autosave::STATE.read_minutes(&crate::hooks::GameMemory);
        let (lines, tell) = crate::autosave::STATE.news();
        for line in lines {
            self.say(&line);
        }
        if tell {
            events.push(crate::autosave::STATE.event(crate::pace::clock_ms(), ""));
        }
        // smooth pacing (pace.rs): its first stretched batch and a line a minute, from this thread (never from the hook on the main thread)
        let pace_now = crate::pace::clock_ms();
        // and the free-id order's line a minute (freeorder.rs), counted by its hook on the simulation's threads
        for line in [
            crate::pace::PACER.first_line(),
            crate::pace::PACER.minute_line(pace_now),
            crate::freeorder::STATS.minute_line(pace_now),
        ]
        .into_iter()
        .flatten()
        {
            self.say(&line);
        }
        if let Some(audit) = lock(&self.audit).poll(now, &self.counters) {
            self.say(&audit_log_line(&audit, |k| self.build.kind_name(k)));
            events.push(audit);
        }
        for e in &events {
            if let Some(line) = e.log_line() {
                self.say(&line);
            }
            // the session's start and end in the free-id order trace: past a game's `holding off` its world runs single-player
            if let Event::Holding { on, reason, .. } = e {
                crate::ids::mark(&format!("holding {} ({reason})", if *on { "on" } else { "off" }));
            }
        }
        // taken last, when nothing but the write is left (a panic before it loses no fault); `fault` logged each one already
        for f in self.faults.drain().into_iter().chain(crate::failsafe::GLOBAL.drain()) {
            events.push(Event::Fault { place: f.place.to_string(), detail: f.detail, action: f.action.name() });
        }
        if events.is_empty() {
            return 0;
        }
        let mut guard = lock(&self.writer);
        if let Some(w) = guard.as_mut() {
            if let Err(e) = w.events.append(&events) {
                if !w.failed_logged {
                    w.failed_logged = true;
                    self.say(&format!("link: cannot write {}: {e}", w.events.path().display()));
                }
            }
        }
        events.len()
    }

    // ---- the command-list hook

    /// The unhooked `Add`. `None` before it is published, which cannot happen once the jump is written.
    fn add_fn(&self) -> Option<AddFn> {
        match self.orig_add.load(Acquire) {
            0 => None,
            // SAFETY: the trampoline (or a test function) with CommandList::Add's signature.
            p => Some(unsafe { core::mem::transmute::<usize, AddFn>(p) }),
        }
    }

    /// What the hook on `CommandList::Add` does for one call. `caller` is the return address of the call
    /// (`detour::hook_caller`). Returns what `Add` returns: the original's result, or the emptied `out` handle
    /// when the command was held.
    ///
    /// # Safety
    /// `args` are the arguments of a live call to `CommandList::Add` on the calling thread.
    ///
    /// Three rules shape this function: the first learned from a crash of the real game (`docs/NATIVE_LINK.md`, section
    /// 10), the other two from a review of the code (2026-10-15; the second is shown by `runtime/tests/foreign.rs`):
    ///
    /// - The build must unwind (`panic = "unwind"`, root `Cargo.toml`) and every function between this one and the
    ///   engine is `system-unwind` or plain Rust, with the engine called from its body, not from a closure. A C++
    ///   exception of the game then passes through our frames as through any others, to the game's own handler.
    /// - Our own bookkeeping runs inside [`crate::log::guarded`], and no call into the game ever does (not the
    ///   capture's move constructor, not the injection's Proposal functions, not the game's `operator new`): Rust's
    ///   `catch_unwind` ends the process on a C++ exception. Each such piece is split into a guarded decision, the calls
    ///   in plain frames, and a guarded report. A bug in the bookkeeping costs the step, not the game.
    /// - A command taken out of the engine's hands is held, or handed back, with nothing that can fail in between:
    ///   everything the hold needs is read from the command before the capture ([`Runtime::plan_add`]).
    #[allow(improper_ctypes_definitions)] // only ever called from Rust, with Rust types; "system-unwind" is for the exception
    pub unsafe extern "system-unwind" fn on_add(&self, args: AddArgs, caller: Option<usize>) -> *mut u8 {
        let add = self.add_fn();
        // filled in as the planning goes, so that a panic half-way still leaves the part that was done (a command that
        // was taken out of the engine's hands must be given back, whatever happens)
        let mut plan = AddPlan {
            due: Vec::new(),
            spill: None,
            held_here: false,
            now: 0,
            dump: None,
            preview: None,
            tool: false,
            asset_tick: false,
            passed: None,
        };
        let hold = match crate::log::guarded("add hook", || self.plan_add(&args, caller, add.is_some(), &mut plan)) {
            Some(hold) => hold,
            None => {
                if plan.tool && self.live.load(Relaxed) {
                    plan.passed = Some("a panic while deciding whether to hold it (the PANIC line above)");
                }
                None
            }
        };
        if let Some(hold) = hold {
            // SAFETY: the arguments of the live call, as the caller guarantees.
            unsafe { self.capture_and_hold(&args, hold, &mut plan) };
        }
        if let Some(why) = plan.passed {
            // Fail loud: the other games apply this build at its stamp, this one at the click (or the mod never hears of it)
            self.fault(
                "add hook",
                crate::failsafe::Action::Repair,
                format!("a player's build command went to the engine at the click, not at its stamp: {why}"),
            );
        }
        if self.patch.enabled() {
            crate::log::guarded("add hook (patch trail)", || self.patch_note_add(&args, caller, plan.held_here));
        }
        if let Some(job) = plan.dump.take() {
            // calls the engine (the residue mask's copies): guards its own bookkeeping
            self.inject_publish_text(job.payload, job.id);
            // dev-only and read-only: the held command's payload, decoded (the command is already out of the engine's hands)
            crate::log::guarded("add hook (dump)", || {
                self.dump_payload("held", job.payload, Some(job.id), Some(job.site), Scope::Whole, Budget::Held, None)
            });
            crate::log::guarded("add hook (portable)", || {
                self.portable_payload("held", job.payload, Some(job.id), Some(job.site));
            });
            // dev-only, and both call the engine: they guard their own bookkeeping
            self.rebuild_check("held", job.payload, Some(job.id));
            self.native_copy_check("held", job.payload, Some(job.id));
        }
        if let Some((id, payload)) = plan.preview.take() {
            // SAFETY: the payload of the command held just now; it stays held while the scripts read it.
            unsafe { self.give_preview(id, payload) };
        }
        let Some(add) = add else { return args.out };
        let Some(engine) = self.engine else {
            // SAFETY: the arguments of the live call that is being hooked.
            return unsafe { add(args.list, args.out, args.cmd, args.done, args.progress) };
        };
        // the engine is touched only now, outside the lock, in the order the machine decided
        for action in plan.due {
            match action {
                Action::Release { id, handle, .. } => {
                    crate::log::guarded("add hook (watch)", || self.watch.arm(handle.payload(), id, plan.now));
                    // SAFETY: the original Add of this process, called on the thread that queues commands.
                    unsafe { engine::release(&engine, handle, add) };
                }
                Action::Drop { id, handle } => {
                    // SAFETY: as for `release`; the tool's callback is fired only when the capture found it in the build table.
                    let fired = unsafe { engine::discard(&engine, handle) };
                    if fired {
                        crate::log::line(&format!("hold: command {id}: the tool's completion callback was fired before the command was dropped"));
                    }
                }
                Action::Complete { id, handle, ok } => {
                    // SAFETY: as for `release`; the held command carries the result of this game's own stamp apply (adopt_result).
                    let fired = unsafe { engine::complete(&engine, handle) };
                    crate::log::line(&format!(
                        "hold: command {id}: completed with this game's stamp apply (accepted {ok}), tool callback fired: {fired}"
                    ));
                    self.inject.report(Event::Completed { id, ok, fired, ms: plan.now, reason: "stamp_apply" });
                }
                Action::Fail { id, handle, reason } => {
                    // SAFETY: as for `release`; the held command was never applied, so its result byte says "not applied".
                    let fired = unsafe { engine::fail(&engine, handle) };
                    crate::log::line(&format!(
                        "hold: command {id}: failed without a stamp apply ({reason}), tool callback fired with a failed result: {fired}"
                    ));
                    self.inject.report(Event::Completed { id, ok: false, fired, ms: plan.now, reason });
                }
            }
        }
        if let Some(back) = plan.spill {
            unsafe { engine::release(&engine, back, add) };
            return args.out;
        }
        if plan.held_here {
            return args.out;
        }
        self.terrain_note_add(&args);
        self.assets_note_add(&args, plan.tool && !plan.asset_tick);
        // both call the engine when an order is carried out: they guard their own bookkeeping
        if self.inject.enabled() {
            self.inject_on_add(&args, caller);
        }
        if self.patch.enabled() {
            self.patch_add(&args, caller);
        }
        // SAFETY: the arguments of the live call that is being hooked.
        unsafe { add(args.list, args.out, args.cmd, args.done, args.progress) }
    }

    /// The part of [`Runtime::on_add`] that decides: counts the call and, when holding is on, asks the machine what
    /// is due and whether this command is to be held. Never calls the engine (it runs inside `log::guarded`). The result
    /// is written into `plan` step by step; a command to hold comes back as a [`HoldStep`], with the machine still
    /// locked and everything the hold needs already read from the command, for [`Runtime::capture_and_hold`].
    fn plan_add<'a>(
        &'a self,
        args: &AddArgs,
        caller: Option<usize>,
        have_add: bool,
        plan: &mut AddPlan,
    ) -> Option<HoldStep<'a>> {
        let site = caller.and_then(|c| c.checked_sub(self.base)).and_then(|rva| u32::try_from(rva).ok());
        let tool = site.and_then(|rva| self.build.ui_site_index(rva));
        self.counters.added(match (caller, tool) {
            (None, _) => AddSource::Unknown,
            (Some(_), Some(i)) => AddSource::Tool(i),
            (Some(_), None) => AddSource::Other,
        });
        let engine = self.engine?;
        if !have_add || !self.live.load(Relaxed) {
            return None;
        }
        let now = self.now_ms();
        plan.now = now;
        plan.tool = tool.is_some();
        // A player's build that a script sent (no tool site): another mod's tool or window calling api.cmd.sendCommand with
        // makeWorldBuildProposalCmd from an interface Lua state. Real session 2026-10-26 (0.2.8, deadeye_tpf3_bus_bay's upgrade tool, a
        // react recipe calling `api.cmd.sendCommand(api.cmd.makeWorldBuildProposalCmd(proposal, ctx, false, true), cb)`): it passed here
        // as "another caller", the engine applied it at the click on the originator, the simulation half captured it after the apply and
        // the others applied it at the stamp, 3 steps later: a free-id fork and a resync, twice. Such a command is held like a tool's and
        // goes the held way (its own proposal is the record, every game applies it at the stamp, its callback gets the stamp apply's
        // result). The game's own scripts are not caught: their builds say playerInitiated false (towns, reforestation), and a send from
        // the simulation state never reaches this hook (b31 to b41). The terrain tool and the asset brushes have their own stamped way.
        // FeverLink3's own sends through here (the interface half's apply during a pause) are announced by `own_next` just before; the
        // next build of no tool site takes it, whatever its flags (a carrier, a second command with playerInitiated false), so it never
        // lingers on to let another script's build through.
        // SAFETY: the kind is read only from readable memory.
        let script_any = tool.is_none()
            && unsafe { engine::command_kind(args.cmd, engine.kind_offset) }
                == self.build.kind_by_name("build_proposal");
        let script_build = script_any && self.script_player_build(args);
        let mut machine = match try_lock_briefly(&self.machine) {
            Some(m) => m,
            // a tool's command may be one to hold: wait a little for the service thread rather than pass it through
            None if plan.tool || script_build => match lock_within(&self.machine, HOOK_WAIT) {
                Some(m) => m,
                None => {
                    if plan.tool || !(script_any && self.own_send.take(now)) {
                        plan.passed = Some("the hold state stayed busy");
                    }
                    return None;
                }
            },
            None => return None,
        };
        // the mod writes `release` and then sends a command to get here: look at the file now, do not wait for the pump
        self.ingest(&mut machine, now);
        plan.due = machine.take_due(self.game_time(), now);
        let own = script_any && self.own_send.take(now);
        let script = script_build && !own;

        // a tree, plant or rock placed by a tool on the hold list goes the brush's way, not held (runtime/assets.rs)
        plan.asset_tick = tool.is_some() && self.assets_tool_tick(args);
        // the site the hold reports: the tool's, or 0 for a script's send
        let hold_site = if tool.is_some() {
            site
        } else if script {
            Some(0)
        } else {
            None
        };
        if let (Some(rva), false) = (hold_site, plan.asset_tick) {
            // SAFETY: the kind is read only from readable memory.
            if let Some(kind) = unsafe { engine::command_kind(args.cmd, engine.kind_offset) } {
                let admit = machine.admit(kind, now);
                if admit == Admit::Full {
                    plan.passed = Some("the hold ring is full");
                }
                if admit == Admit::Off && tool.is_some() {
                    machine.tool_while_silent(kind, now); // a tool at work while the mod is silent: not a pause menu
                    plan.due.extend(machine.take_due(self.game_time(), now)); // given back before this command passes
                }
                // SAFETY: only readable memory is read; the capture takes the same payload pointer along.
                if let (Admit::Hold, Some(payload)) = (admit, unsafe { engine::payload_of(args.cmd) }) {
                    // Everything that can fail is done here, before the capture empties the caller's arguments: a panic in it
                    // costs the hold, and the command goes on to the engine untouched.
                    let build_proposal = self.build.kind_by_name("build_proposal") == Some(kind);
                    // how the tool built the command (Context members, ignoreErrors, playerInitiated): a preview carries none of it
                    // SAFETY: only readable memory is read.
                    let flags = if build_proposal { unsafe { engine::build_flags(payload) } } else { None };
                    // the input-level text of the same command (what a SimpleProposal is made of), for the mod
                    let t_input = std::time::Instant::now();
                    let input = flags.is_some().then(|| self.input_text(payload)).flatten();
                    let input_ms = t_input.elapsed().as_secs_f64() * 1000.0;
                    return Some(HoldStep { machine, engine, kind, rva, flags, input, input_ms, build_proposal });
                }
            }
        }
        self.sync_flags(&machine);
        drop(machine);
        if !plan.due.is_empty() {
            self.wake();
        }
        None
    }

    /// A `build_proposal` whose Context says playerInitiated and that is neither a terrain edit (its terrain member is set: terrain.rs) nor
    /// an asset tick (assets.rs): the class of a player's build a script sends ([`Runtime::plan_add`]). Only reads memory.
    fn script_player_build(&self, args: &AddArgs) -> bool {
        let Some(kind_offset) = self.build.kind_offset else { return false };
        // SAFETY: only readable memory is read.
        let kind = unsafe { engine::command_kind(args.cmd, kind_offset as usize) };
        if kind.is_none() || self.build.kind_by_name("build_proposal") != kind {
            return false;
        }
        // SAFETY: as above.
        let Some(payload) = (unsafe { engine::payload_of(args.cmd) }) else { return false };
        let Some(bytes) = dump::Mem::read(&LiveMem, payload, dump::PAYLOAD_SIZE) else { return false };
        let terrain = &bytes[crate::terrain::TERRAIN_OFF..crate::terrain::TERRAIN_OFF + crate::terrain::TERRAIN_LEN];
        bytes[crate::assets::FLAGS + 2] == 1
            && terrain.iter().all(|&b| b == 0)
            && !crate::assets::shape_of(&bytes).is_some_and(|(rm, add)| rm + add > 0)
    }

    /// Takes the command out of the engine's hands ([`engine::capture`], the engine's move constructor) and into the
    /// machine, which is still locked from [`Runtime::plan_add`]. Not inside `log::guarded`: the move is a call into
    /// the game, and an exception from it must reach the game's handler. Between the capture and `hold_with` nothing
    /// can fail (two field reads), so the command is held, or refused and put into `plan.spill` to go on to the
    /// engine; the bookkeeping after that is guarded again. An exception in the capture leaves the machine's lock by
    /// unwinding (a foreign exception does not even poison it, and every lock of it ignores poisoning).
    ///
    /// # Safety
    /// `args` are the arguments of a live call to `CommandList::Add` on the calling thread.
    unsafe fn capture_and_hold(&self, args: &AddArgs, step: HoldStep<'_>, plan: &mut AddPlan) {
        let HoldStep { mut machine, engine, kind, rva, flags, input, input_ms, build_proposal } = step;
        // SAFETY: as the caller guarantees.
        let held = unsafe { engine::capture(&engine, args) };
        let mut held_id = None;
        if let Some(held) = held {
            let (done, payload) = (held.done_ok(), held.payload());
            match machine.hold_with(kind, rva, done, flags, held, plan.now) {
                Ok(id) => {
                    plan.held_here = true;
                    held_id = Some((id, payload));
                }
                Err(back) => {
                    plan.spill = Some(back); // refused after capture: it goes straight on
                    plan.passed = Some("the hold ring refused it after the capture");
                }
            }
        } else {
            plan.passed = Some("the engine's command could not be taken out of the tool's call (capture refused)");
        }
        let reported = crate::log::guarded("add hook (held)", || {
            if let Some((id, payload)) = held_id {
                if input.is_some() {
                    self.inject.report(Event::Timing {
                        what: "input",
                        id,
                        detail: format!("held={id} wall={} input_ms={input_ms:.1}", wall_ms()),
                    });
                }
                if let Some(text) = &input {
                    if let Err(e) = self.input.write(id, text) {
                        self.say(&format!("input: cannot write {}: {e}", crate::input::file_name(id)));
                    }
                }
                if build_proposal {
                    plan.preview = Some((id, payload));
                }
                if build_proposal
                    && (self.inject.enabled()
                        || self.dump.get().is_some()
                        || self.portable.get().is_some()
                        || self.rebuild.get().is_some()
                        || self.native_copy.get().is_some())
                {
                    plan.dump = Some(DumpJob { id, site: rva, payload });
                }
            }
            self.sync_flags(&machine);
        });
        if reported.is_none() {
            if let Some((id, _)) = held_id {
                // Fail closed: what the mod needs of the held command (its texts, its preview) may be missing: it is failed (its tool
                // callback fired with a failed result at the next Add), so no game applies it; the mod may have shipped it already
                machine.handle(&crate::link::CtlMessage::Fail { id }, plan.now);
                plan.preview = None;
                plan.dump = None;
                self.sync_flags(&machine);
                drop(machine);
                self.fault(
                    "add hook (held)",
                    crate::failsafe::Action::Repair,
                    format!("a panic after command {id} was held: it is failed, nothing of it is applied on this game"),
                );
                self.wake();
                return;
            }
        }
        drop(machine);
        if plan.held_here || !plan.due.is_empty() {
            self.wake();
        }
    }

    // ---- the apply hook

    /// What the hook on the simulation's command application does for one command: count it, recognise it if the
    /// module released it, and call the original.
    ///
    /// # Safety
    /// `sim` and `cmd` are the arguments of a live call on the calling thread.
    ///
    /// `system-unwind` and calling the original from its own body, for the reason given at [`Runtime::on_add`]: the
    /// simulation's apply function can throw (a script error in a command's handler), and the exception must pass.
    #[allow(improper_ctypes_definitions)] // only ever called from Rust, with Rust types; "system-unwind" is for the exception
    pub unsafe extern "system-unwind" fn on_apply(&self, sim: *mut u8, cmd: *mut u8) -> u64 {
        let orig = self.orig_apply.load(Acquire);
        if orig == 0 {
            return 0; // unreachable once installed
        }
        // SAFETY: the trampoline of the apply function, published before the jump was written.
        let original = unsafe { core::mem::transmute::<usize, ApplyFn>(orig) };
        // the bookkeeping before the call: count the command, and recognise one the module released
        let claimed = crate::log::guarded("apply hook", || {
            let kind_offset = self.build.kind_offset.unwrap_or(0) as usize;
            // SAFETY: only readable memory is read.
            match unsafe { engine::command_kind(cmd, kind_offset) } {
                Some(k) => self.counters.applied(k),
                None => self.counters.applied_unreadable(),
            }
            if self.watch.is_idle() {
                return None;
            }
            unsafe { engine::payload_of(cmd) }.and_then(|p| self.watch.claim(p))
        })
        .flatten();
        // the orders of `inject_next` / `patch_next`: the script's command passes here, not the command list's `Add` (see apply_orders)
        let mut used = None;
        // a terrain or asset tool's command, a carrier of a shared edit or tick: this module's alone (calls the engine when it empties one
        // or builds a carrier's asset groups)
        let terrain = claimed.is_none() && (self.assets_on_apply(cmd) || self.terrain_on_apply(cmd));
        if !terrain && (self.inject.enabled() || self.patch.enabled()) {
            let mine = crate::log::guarded("apply hook (orders)", || {
                let kind_offset = self.build.kind_offset.unwrap_or(0) as usize;
                // SAFETY: only readable memory is read.
                let kind = unsafe { engine::command_kind(cmd, kind_offset) };
                self.note_apply_orders(kind, claimed.is_some())
            });
            if mine.is_none() {
                self.fault(
                    "apply hook (orders)",
                    crate::failsafe::Action::Repair,
                    "a panic while matching a command the simulation applies to its inject order: it is applied as the script sent it",
                );
            }
            if mine == Some(true) {
                // calls the engine when an order is carried out: guards its own bookkeeping
                used = self.apply_orders(cmd);
            }
        }
        // dev-only dump of a build command before the simulation applies it, and of its result part afterwards
        let dumped = self.dump.get().and_then(|_| {
            crate::log::guarded("apply hook (dump)", || {
                // SAFETY: only readable memory is read.
                let payload = unsafe { engine::payload_of(cmd) }?;
                let kind_offset = self.build.kind_offset.unwrap_or(0) as usize;
                let kind = unsafe { engine::command_kind(cmd, kind_offset) }?;
                (self.build.kind_by_name("build_proposal") == Some(kind)).then_some(())?;
                // script commands (no hold key) only at level 3
                (claimed.is_some() || self.dump.get().is_some_and(|d| d.settings.scripts)).then_some(())?;
                let seq = self.dump_payload("apply", payload, claimed, None, Scope::Whole, Budget::Applied, None)?;
                Some((payload, seq))
            })
            .flatten()
        });
        // dev-only portable text of a build command the simulation is about to apply (held ones carry their key)
        if let Some(sink) = self.portable.get() {
            crate::log::guarded("apply hook (portable)", || {
                // SAFETY: only readable memory is read.
                let payload = unsafe { engine::payload_of(cmd) }?;
                let kind_offset = self.build.kind_offset.unwrap_or(0) as usize;
                let kind = unsafe { engine::command_kind(cmd, kind_offset) }?;
                (self.build.kind_by_name("build_proposal") == Some(kind)).then_some(())?;
                (claimed.is_some() || sink.settings.scripts).then_some(())?;
                // a script command that changes nothing is not worth a dump
                (claimed.is_some() || portable::proposal_is_empty(&LiveMem, payload) == Some(false)).then_some(())?;
                self.portable_payload("apply", payload, claimed, None);
                Some(())
            });
        }
        if let Some(sink) = self.rebuild.get() {
            // script commands the module did not hold, with the level-3 switch
            let payload = crate::log::guarded("apply hook (rebuild)", || {
                // SAFETY: only readable memory is read.
                let payload = unsafe { engine::payload_of(cmd) }?;
                let kind_offset = self.build.kind_offset.unwrap_or(0) as usize;
                let kind = unsafe { engine::command_kind(cmd, kind_offset) }?;
                (self.build.kind_by_name("build_proposal") == Some(kind)).then_some(())?;
                (claimed.is_none() && sink.settings.scripts).then_some(())?;
                (portable::proposal_is_empty(&LiveMem, payload) == Some(false)).then_some(payload)
            });
            if let Some(payload) = payload.flatten() {
                // calls the game's allocator: guards its own bookkeeping
                self.rebuild_check("apply", payload, None);
            }
        }
        let result = unsafe { original(sim, cmd) };
        if let Some(used) = used {
            self.after_ordered_apply(cmd, used);
        }
        if let Some((payload, seq)) = dumped {
            crate::log::guarded("apply hook (dump after)", || {
                self.dump_payload("applied", payload, claimed, None, Scope::Result, Budget::Applied, Some(seq))
            });
        }
        if let Some(id) = claimed {
            crate::log::guarded("apply hook (result)", || {
                let ok = unsafe { engine::command_result(cmd) }.unwrap_or(false);
                self.watch.finish(id, ok);
            });
        }
        result
    }
}

/// The timing of an ordered script command until its stamp apply (see `inject::Used`).
pub(crate) struct StampTiming {
    pub key: u64,
    pub held: u64,
    pub tm: String,
    pub rebuild: String,
}

/// Milliseconds since 1970 (the wall clock every timing line uses, so that two games' lines can be compared).
pub(crate) fn wall_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The C runtime's `clock()` in milliseconds: what the game's Lua `os.clock()` reads (both use the process's one C runtime), so the mod can
/// turn its own clock into wall-clock time (`timing clock`).
pub(crate) fn crt_clock_ms() -> i64 {
    unsafe extern "C" {
        fn clock() -> i32;
    }
    // SAFETY: `clock` takes nothing and only reads the C runtime's start time.
    i64::from(unsafe { clock() })
}

/// What [`Runtime::plan_add`] decided: the engine is touched according to it, after the lock is gone.
struct AddPlan {
    /// Held commands to hand back or destroy, in order.
    due: Vec<Action<Box<Held>>>,
    /// A command taken and then refused by the machine: it goes straight on.
    spill: Option<Box<Held>>,
    /// The current command was held: the caller gets an empty handle and the original is not called.
    held_here: bool,
    now: u64,
    /// A held `build_proposal` command to dump (only with `FEVERLINK3_DUMP_COMMANDS`).
    dump: Option<DumpJob>,
    /// A held `build_proposal` command to give to the scripts as a preview (`give_preview`): its id and payload.
    preview: Option<(crate::hold::HoldId, usize)>,
    /// The command comes from a construction tool's call site (known before anything can fail).
    tool: bool,
    /// The tool's command is an asset tick ([`Runtime::assets_tool_tick`]): not held, it goes the brush's way.
    asset_tick: bool,
    /// Why a tool's command that holding should have taken goes to the engine at the click (a fault, `failsafe.rs`).
    passed: Option<&'static str>,
}

/// How long the command hook waits for the hold state for a tool's command (the service thread holds it while it reads the control
/// file and writes events: well under a millisecond as a rule). Longer, and the command passes and a fault is raised.
const HOOK_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// The lock, or `None` after `wait`.
fn lock_within<T>(m: &Mutex<T>, wait: std::time::Duration) -> Option<MutexGuard<'_, T>> {
    let until = Instant::now() + wait;
    loop {
        match m.try_lock() {
            Ok(g) => return Some(g),
            Err(TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() < until => std::thread::yield_now(),
            Err(TryLockError::WouldBlock) => return None,
        }
    }
}

/// A tool's command [`Runtime::plan_add`] decided to hold: the machine, still locked, and what the hold records about the
/// command, read before the capture.
struct HoldStep<'a> {
    machine: MutexGuard<'a, HoldMachine<Box<Held>>>,
    engine: EngineFns,
    kind: u8,
    rva: u32,
    flags: Option<engine::BuildFlags>,
    input: Option<String>,
    input_ms: f64,
    build_proposal: bool,
}

/// What the dump needs to know about a command that was just held.
struct DumpJob {
    id: u64,
    site: u32,
    payload: usize,
}

/// The id of the calling thread, for the dump header.
fn current_thread_id() -> u32 {
    // SAFETY: no arguments; returns the calling thread's id.
    unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{AddArgs, EngineFns, COMMAND_SIZE, HANDLE_SIZE, PROGRESS_SIZE};
    use crate::testutil::TempDir;
    use std::cell::RefCell;

    mod foreign;
    mod queue;

    /// Friend test 2026-10-21: a held command before any tool preview of the world gets the emitter's arguments computed from the game
    /// interface's listener captures, the way the listener computes them; a getter outside the game's image is never called.
    #[test]
    fn the_preview_arguments_come_from_the_game_interface_before_any_tool_preview() {
        let site = crate::build_table::fixture().preview_owner.as_ref().unwrap();
        let mut ui = vec![0u64; 0xc00 / 8];
        ui[0xb98 / 8] = 0xE;
        let mut y = vec![0u64; 0x100 / 8];
        (y[0x18 / 8], y[0xc8 / 8]) = (0xA, 0xB);
        let mut r = vec![0u64; 0x200 / 8];
        (r[0x18 / 8], r[0x1d8 / 8]) = (0xC, 0xD);
        static R: AtomicU64 = AtomicU64::new(0);
        R.store(r.as_ptr() as u64, SeqCst);
        unsafe extern "system-unwind" fn getter(_this: u64) -> u64 {
            R.load(SeqCst)
        }
        let vtable = [0u64, getter as *const () as u64];
        let z = [vtable.as_ptr() as u64];
        let caps = [ui.as_ptr() as u64, y.as_ptr() as u64, z.as_ptr() as u64];
        let at = getter as *const () as usize;
        let f = preview_getter(caps[2], site, at..at + 1).unwrap();
        assert_eq!(f, at);
        let got = unsafe { core::mem::transmute::<usize, unsafe extern "system-unwind" fn(u64) -> u64>(f)(caps[2]) };
        assert_eq!(preview_args_from(caps, got, site).unwrap(), [0xA, 0xB, 0xC, 0xD, 0xE]);
        assert!(preview_getter(caps[2], site, 0..1).unwrap_err().contains("not in the game's image"));
        assert!(preview_args_from([0, 0, 0], 0, site).is_err(), "a null capture is refused, not read");
    }

    // ---- a fake engine that records what the hooks do to it, per test thread

    thread_local! {
        static CALLS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        /// The fake engine throws from `Add` / `apply` while set: the stand-in for a C++ exception of the game.
        static THROW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// The payload of the fake engine's "exception".
    const THROWN: &str = "engine exception";

    fn throw_if_set() {
        if THROW.with(|t| t.get()) {
            // unwinds through the hook's frames like the game's own exception; no panic hook, no message
            std::panic::resume_unwind(Box::new(THROWN));
        }
    }

    fn note(s: impl Into<String>) {
        CALLS.with(|c| c.borrow_mut().push(s.into()));
    }

    fn calls() -> Vec<String> {
        CALLS.with(|c| std::mem::take(&mut *c.borrow_mut()))
    }

    unsafe extern "system-unwind" fn e_move(dst: *mut u8, src: *mut u8) -> *mut u8 {
        foreign::raise_if_armed();
        unsafe {
            core::ptr::copy_nonoverlapping(src, dst, COMMAND_SIZE);
            core::ptr::write_bytes(src, 0, engine::RESULT_OFFSET);
        }
        dst
    }
    unsafe extern "system-unwind" fn e_destroy(cmd: *mut u8) {
        note(format!("destroy command {}", unsafe { *(cmd as *const usize) } != 0));
        unsafe { core::ptr::write_bytes(cmd, 0, COMMAND_SIZE) };
    }
    unsafe extern "system-unwind" fn e_handle(_h: *mut u8) {}
    unsafe extern "system-unwind" fn e_add(
        _list: *mut u8,
        out: *mut u8,
        cmd: *mut u8,
        _done: *mut u8,
        _progress: *mut u8,
    ) -> *mut u8 {
        throw_if_set();
        let payload = unsafe { *(cmd as *const usize) };
        if payload == 0 {
            note("add empty command");
            return out;
        }
        let kind = unsafe { *((payload + 0x9b8) as *const u8) };
        note(format!("add kind {kind} tag {}", unsafe { *((payload + 8) as *const u8) }));
        out
    }
    unsafe extern "system-unwind" fn e_apply(_sim: *mut u8, cmd: *mut u8) -> u64 {
        throw_if_set();
        let payload = unsafe { *(cmd as *const usize) };
        if payload == 0 {
            return 1;
        }
        note(format!("apply tag {}", unsafe { *((payload + 8) as *const u8) }));
        // the real function stores the verdict in the command; payload tag 99 is refused
        unsafe { *cmd.add(engine::RESULT_OFFSET) = u8::from(*((payload + 8) as *const u8) != 99) };
        7
    }

    const SITE_A: u32 = 0x4d396f;
    const SITE_B: u32 = 0x51b56c;
    const BASE: usize = 0x1_4000_0000;

    fn build() -> &'static BuildAddrs {
        static BUILD: BuildAddrs = BuildAddrs {
            timestamp: 7,
            name: "fake",
            command_list_add: None,
            apply_command: None,
            town_seed: None,
            town_develop: None,
            town_update_size: None,
            script_convert: None,
            convert_update: None,
            preview_event: None,
            preview_owner: None,
            town_candidates: None,
            town_edges: None,
            town_sort_call: None,
            town_pick_call: None,
            town_field_call: None,
            batch_pace: None,
            free_order: None,
            game_time_getter: None,
            script_pool: None,
            lua_makeseed: None,
            tick_count_getter: None,
            command_move: None,
            command_destroy: None,
            handle_destroy: None,
            ui_add_sites: &[SITE_A, SITE_B],
            fire_done_calls: &[],
            kind_offset: Some(0x9b8),
            kinds: &[("build_proposal", 52)],
            engine: crate::build_table::EngineFns::NONE,
            layout: None,
        };
        &BUILD
    }

    fn quiet(_: &str) {}

    fn runtime(dir: Option<&Path>) -> Runtime {
        let rt = Runtime::new(Init {
            build: build(),
            base: BASE,
            hold: HoldConfig::default(),
            engine: Some(EngineFns {
                command_move: e_move,
                command_destroy: e_destroy,
                handle_destroy: e_handle,
                kind_offset: 0x9b8,
                fire_calls: &[],
                fire_done: false,
            }),
            log: quiet,
            hooks: vec!["apply_command", "command_list_add"],
            poolpatch: false,
            version: "test",
        });
        rt.set_originals(Some(e_add), Some(e_apply));
        if let Some(d) = dir {
            rt.open_link(d).expect("open link");
        }
        rt
    }

    /// One engine command: the 0x38-byte object and its payload (kind byte at 0x9b8, a tag byte at 8).
    struct Cmd {
        command: Box<[u8; 0x40]>,
        payload: Box<[u8; 0x9c0]>,
    }

    fn cmd(kind: u8, tag: u8) -> Cmd {
        let mut payload = Box::new([0u8; 0x9c0]);
        payload[0x9b8] = kind;
        payload[8] = tag;
        let mut command = Box::new([0u8; 0x40]);
        command[..8].copy_from_slice(&(payload.as_ptr() as usize).to_ne_bytes());
        Cmd { command, payload }
    }

    /// The arguments of an `Add` call, owned by the test.
    struct Call {
        c: Cmd,
        list: Box<[u8; 8]>,
        out: Box<[u8; HANDLE_SIZE]>,
        done: Box<[u8; engine::FUNCTION_SIZE]>,
        progress: Box<[u8; PROGRESS_SIZE]>,
    }

    impl Call {
        fn new(kind: u8, tag: u8) -> Call {
            Call {
                c: cmd(kind, tag),
                list: Box::new([9; 8]),
                out: Box::new([0xEE; HANDLE_SIZE]),
                done: Box::new([0; engine::FUNCTION_SIZE]), // an empty std::function
                progress: Box::new([5; PROGRESS_SIZE]),
            }
        }
        fn args(&mut self) -> AddArgs {
            AddArgs {
                list: self.list.as_mut_ptr(),
                out: self.out.as_mut_ptr(),
                cmd: self.c.command.as_mut_ptr(),
                done: self.done.as_mut_ptr(),
                progress: self.progress.as_mut_ptr(),
            }
        }
        fn payload(&self) -> usize {
            self.c.payload.as_ptr() as usize
        }
    }

    fn at(site: u32) -> Option<usize> {
        Some(BASE + site as usize)
    }

    /// The interface half's control file, named the way its writer names it (`native_ctl.gui.w<id>.log`).
    fn ctl_path(dir: &Path) -> std::path::PathBuf {
        crate::link::writer_log_path(dir, "native_ctl.gui", "1")
    }

    fn ctl(dir: &Path, text: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(ctl_path(dir))
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    /// The events of the link, without the diagnostic `timing` records (see `timing_records`).
    fn events(dir: &Path) -> Vec<(String, String)> {
        all_events(dir).into_iter().filter(|(k, _)| k != "timing").collect()
    }

    fn all_events(dir: &Path) -> Vec<(String, String)> {
        let mut r = CtlReader::new(dir.join(crate::link::EVENTS_FILE));
        r.poll().lines.into_iter().map(|l| (l.kind, l.payload)).collect()
    }

    #[test]
    fn the_link_opens_with_the_wall_clock_of_the_c_runtime_clock_for_the_timing_lines() {
        let dir = TempDir::new("rt-clock");
        let _rt = runtime(Some(dir.path()));
        let ev = all_events(dir.path());
        assert_eq!(ev[0].0, "ready");
        assert_eq!(ev[1].0, "timing", "{ev:?}");
        assert!(
            ev[1].1.contains("\"what\"]=\"clock\"") && ev[1].1.contains("wall=") && ev[1].1.contains("clock="),
            "{}",
            ev[1].1
        );
    }

    fn kinds(dir: &Path) -> Vec<String> {
        events(dir).into_iter().map(|(k, _)| k).collect()
    }

    const HOLD_ON: &str = "hold_on\t1\t{[\"kinds\"]={[1]=\"build_proposal\"}}\n";

    // ---- tests

    #[test]
    fn with_holding_off_the_hook_counts_and_calls_the_original() {
        let rt = runtime(None);
        let mut c = Call::new(52, 1);
        let out = unsafe { rt.on_add(c.args(), at(SITE_A)) };
        assert_eq!(out, c.args().out, "what the original returned");
        assert_eq!(calls(), ["add kind 52 tag 1"]);
        let mut o = Call::new(52, 2);
        unsafe { rt.on_add(o.args(), at(0x777)) };
        unsafe { rt.on_add(o.args(), None) };
        calls();
        match rt.counters().snapshot() {
            Event::Audit { adds, sites, .. } => {
                assert_eq!((adds.tool, adds.other, adds.unknown), (1, 1, 1));
                assert_eq!(sites, [(0, 1)]);
            }
            e => panic!("{e:?}"),
        }
        assert!(!rt.live.load(Relaxed), "the fast path: no lock, no file");
    }

    #[test]
    fn a_caller_before_the_image_is_not_a_tool_site() {
        let rt = runtime(None);
        let mut c = Call::new(52, 1);
        unsafe { rt.on_add(c.args(), Some(BASE - 16)) };
        unsafe { rt.on_add(c.args(), Some(usize::MAX)) };
        match rt.counters().snapshot() {
            Event::Audit { adds, .. } => assert_eq!((adds.tool, adds.other), (0, 2)),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn a_tool_command_of_a_held_kind_is_held_and_released_by_the_next_add_in_order() {
        let dir = TempDir::new("rt-hold");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        assert!(rt.live.load(Relaxed));

        let (mut a, mut b) = (Call::new(52, 10), Call::new(52, 11));
        let out_a = unsafe { rt.on_add(a.args(), at(SITE_A)) };
        let out_b = unsafe { rt.on_add(b.args(), at(SITE_B)) };
        assert_eq!((out_a, out_b), (a.args().out, b.args().out), "the tool gets its own empty handle back");
        assert_eq!(*a.out, [0; HANDLE_SIZE], "emptied");
        assert!(calls().is_empty(), "nothing reached the engine");
        rt.pump();
        assert_eq!(kinds(dir.path()), ["ready", "holding", "held", "held"]);

        // the mod releases both and sends a command of its own to get the module's attention
        ctl(dir.path(), "release\t1\t{[\"at\"]=0,[\"id\"]=2}\nrelease\t1\t{[\"at\"]=0,[\"id\"]=1}\n");
        let mut nop = Call::new(27, 50);
        unsafe { rt.on_add(nop.args(), at(0x999)) };
        assert_eq!(
            calls(),
            [
                "add kind 52 tag 10",
                "destroy command true",
                "add kind 52 tag 11",
                "destroy command true",
                "add kind 27 tag 50"
            ],
            "the held commands go back first, in id order here (same game time), then the one that arrived"
        );
        rt.pump();
        let ev = kinds(dir.path());
        assert_eq!(&ev[4..], ["released", "released"]);
    }

    #[test]
    fn only_tool_sites_and_only_held_kinds_are_held() {
        let dir = TempDir::new("rt-filter");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut script = Call::new(52, 1); // right kind, wrong caller (the mod's own sendCommand, say)
        unsafe { rt.on_add(script.args(), at(0x1234)) };
        let mut unknown = Call::new(52, 2);
        unsafe { rt.on_add(unknown.args(), None) };
        let mut other_kind = Call::new(42, 3); // a tool, wrong kind
        unsafe { rt.on_add(other_kind.args(), at(SITE_A)) };
        assert_eq!(calls(), ["add kind 52 tag 1", "add kind 52 tag 2", "add kind 42 tag 3"], "all went through");
        let mut unreadable = Call::new(52, 4);
        unreadable.c.command.fill(0); // no payload: the kind cannot be read
        unsafe { rt.on_add(unreadable.args(), at(SITE_A)) };
        assert_eq!(calls(), ["add empty command"], "an unreadable command passes");
        rt.pump();
        assert_eq!(kinds(dir.path()), ["ready", "holding"], "nothing was held");
    }

    /// Real session 2026-10-26: a mod's tool sent its street upgrade through api.cmd.sendCommand (no tool site) and the engine applied it
    /// at the click. A player's build from a script is held like a tool's (site 0); a game script's (playerInitiated false) and the mod's
    /// own send announced by `own_next` pass.
    #[test]
    fn a_players_build_a_script_sends_is_held_and_the_mods_own_send_passes() {
        let dir = TempDir::new("rt-script");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let player = |tag| {
            let mut c = Call::new(52, tag);
            c.c.payload[crate::assets::FLAGS + 2] = 1; // Context.playerInitiated
            c
        };
        let mut town = Call::new(52, 1); // playerInitiated false: a town's or the game's own script build
        unsafe { rt.on_add(town.args(), at(0x1234)) };
        let mut modded = player(2);
        let out = unsafe { rt.on_add(modded.args(), at(0x1234)) };
        assert_eq!(out, modded.args().out);
        assert_eq!(*modded.out, [0; HANDLE_SIZE], "held: the script's handle is emptied");
        assert_eq!(calls(), ["add kind 52 tag 1"], "only the town's build reached the engine");
        ctl(
            dir.path(),
            "own_next	1	{}
",
        );
        let (mut own, mut next) = (player(3), player(4));
        unsafe { rt.on_add(own.args(), at(0x1234)) };
        unsafe { rt.on_add(next.args(), None) };
        assert_eq!(
            calls(),
            ["add kind 52 tag 3"],
            "own_next lets exactly the mod's own send through; the one after is held again"
        );
        rt.pump();
        let held: Vec<String> = events(dir.path()).into_iter().filter(|(k, _)| k == "held").map(|(_, p)| p).collect();
        assert_eq!(held.len(), 2, "{held:?}");
        assert!(held.iter().all(|p| p.contains("[\"site\"]=0")), "a script's hold names no tool site: {held:?}");
    }

    #[test]
    fn hold_off_releases_what_is_held_at_the_next_add() {
        let dir = TempDir::new("rt-holdoff");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut a = Call::new(52, 10);
        unsafe { rt.on_add(a.args(), at(SITE_A)) };
        ctl(dir.path(), "hold_off\t1\t{}\n");
        rt.pump();
        assert!(rt.live.load(Relaxed), "something is still waiting to go back");
        let mut b = Call::new(52, 11);
        unsafe { rt.on_add(b.args(), at(SITE_A)) };
        assert_eq!(
            calls(),
            ["add kind 52 tag 10", "destroy command true", "add kind 52 tag 11"],
            "released, then this one passes: holding is off"
        );
        assert!(!rt.live.load(Relaxed), "back to the fast path");
    }

    #[test]
    fn silence_fails_what_the_mod_never_decided_on_without_the_mod() {
        let dir = TempDir::new("rt-silence");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut a = Call::new(52, 10);
        unsafe { rt.on_add(a.args(), at(SITE_A)) };
        rt.advance(9_000);
        rt.pump();
        assert!(calls().is_empty(), "9 s of silence: still holding");
        rt.advance(1_001);
        rt.pump();
        let mut b = Call::new(52, 11);
        unsafe { rt.on_add(b.args(), at(SITE_A)) };
        // the held command (no verdict of the mod) is failed, never added: it would run on this game alone
        assert_eq!(calls(), ["destroy command true", "add kind 52 tag 11"]);
        rt.pump();
        let k = kinds(dir.path());
        assert!(k.contains(&"error".to_string()) && k.iter().filter(|x| *x == "holding").count() == 2, "{k:?}");
        assert!(!k.contains(&"released".to_string()), "{k:?}");
    }

    #[test]
    fn cancel_destroys_the_command_without_queueing_it() {
        let dir = TempDir::new("rt-cancel");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut a = Call::new(52, 10);
        unsafe { rt.on_add(a.args(), at(SITE_A)) };
        ctl(dir.path(), "cancel\t1\t{[\"id\"]=1}\n");
        let mut nop = Call::new(27, 50);
        unsafe { rt.on_add(nop.args(), at(0x999)) };
        assert_eq!(calls(), ["destroy command true", "add kind 27 tag 50"], "never added");
        rt.pump();
        assert!(kinds(dir.path()).contains(&"dropped".to_string()));
    }

    #[test]
    fn the_control_file_disappearing_fails_what_the_mod_never_decided_on() {
        let dir = TempDir::new("rt-gone");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut a = Call::new(52, 10);
        unsafe { rt.on_add(a.args(), at(SITE_A)) };
        std::fs::remove_file(ctl_path(dir.path())).unwrap();
        let mut nop = Call::new(27, 50);
        unsafe { rt.on_add(nop.args(), at(0x999)) };
        assert_eq!(calls(), ["destroy command true", "add kind 27 tag 50"], "never added");
    }

    #[test]
    fn a_full_ring_lets_commands_through() {
        let dir = TempDir::new("rt-full");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut keep = Vec::new();
        for i in 0..64 {
            let mut c = Call::new(52, i as u8);
            unsafe { rt.on_add(c.args(), at(SITE_A)) };
            keep.push(c);
        }
        assert!(calls().is_empty());
        let mut extra = Call::new(52, 200);
        unsafe { rt.on_add(extra.args(), at(SITE_A)) };
        assert_eq!(calls(), ["add kind 52 tag 200"], "the 65th is not held");
        rt.pump();
        assert!(kinds(dir.path()).contains(&"error".to_string()));
        assert_eq!(
            faults(dir.path()),
            ["add hook repair"],
            "and it is loud: the build went in at the click on this game only"
        );
    }

    /// The `fault` events written so far, as "place action".
    fn faults(dir: &Path) -> Vec<String> {
        events(dir)
            .into_iter()
            .filter(|(k, _)| k == "fault")
            .map(|(_, p)| {
                let field = |name: &str| {
                    let at = p.find(&format!("[\"{name}\"]=\"")).map(|i| i + name.len() + 6).unwrap_or(0);
                    p[at..].split('"').next().unwrap_or("").to_string()
                };
                format!("{} {}", field("where"), field("action"))
            })
            .collect()
    }

    #[test]
    fn a_busy_state_makes_a_tools_command_wait_briefly_and_then_pass_loudly() {
        let dir = TempDir::new("rt-busy");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let guard = rt.machine.lock().unwrap(); // the service thread, say, stuck in the middle of something
        let mut other = Call::new(27, 9);
        let started = Instant::now();
        unsafe { rt.on_add(other.args(), at(0x999)) };
        assert!(started.elapsed() < HOOK_WAIT, "a command from no tool site never waits");
        let mut c = Call::new(52, 1);
        let started = Instant::now();
        unsafe { rt.on_add(c.args(), at(SITE_A)) };
        assert!(started.elapsed() >= HOOK_WAIT && started.elapsed().as_millis() < 500, "a short wait, then no more");
        drop(guard);
        assert_eq!(calls(), ["add kind 27 tag 9", "add kind 52 tag 1"], "it passed through, the command is not lost");
        rt.pump();
        assert_eq!(faults(dir.path()), ["add hook repair"], "one fault, for the tool's command only");
        assert!(events(dir.path()).iter().any(|(k, p)| k == "fault" && p.contains("the hold state stayed busy")));
    }

    #[test]
    fn a_tools_command_waits_out_a_short_busy_spell_and_is_held() {
        let dir = TempDir::new("rt-busy-short");
        let rt = std::sync::Arc::new(runtime(Some(dir.path())));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let (tx, rx) = std::sync::mpsc::channel();
        let other = rt.clone();
        let t = std::thread::spawn(move || {
            let _g = other.machine.lock().unwrap();
            tx.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
        });
        rx.recv().unwrap();
        let mut c = Call::new(52, 1);
        unsafe { rt.on_add(c.args(), at(SITE_A)) };
        t.join().unwrap();
        assert!(calls().is_empty(), "held, not passed on");
        rt.pump();
        assert!(faults(dir.path()).is_empty());
    }

    #[test]
    fn a_released_command_is_reported_applied_with_the_engines_verdict() {
        let dir = TempDir::new("rt-applied");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let (mut ok, mut refused) = (Call::new(52, 10), Call::new(52, 99));
        unsafe { rt.on_add(ok.args(), at(SITE_A)) };
        unsafe { rt.on_add(refused.args(), at(SITE_A)) };
        ctl(dir.path(), "release\t1\t{[\"id\"]=1}\nrelease\t1\t{[\"id\"]=2}\n");
        let mut nop = Call::new(27, 50);
        unsafe { rt.on_add(nop.args(), at(0x999)) };
        calls();

        // the simulation applies commands: the two released ones (the engine moved their payloads into its list,
        // so the commands it applies are new objects pointing at the same payloads), another one, and one with
        // an unreadable payload
        let mut sim = [0u8; 8];
        let mut run = |command: &mut [u8; 0x40]| unsafe { rt.on_apply(sim.as_mut_ptr(), command.as_mut_ptr()) };
        let queued = |payload: usize| {
            let mut c = Box::new([0u8; 0x40]);
            c[..8].copy_from_slice(&payload.to_ne_bytes());
            c
        };
        assert_eq!(run(&mut queued(ok.payload())), 7, "the original's result is passed on");
        assert_eq!(run(&mut queued(refused.payload())), 7);
        let stranger = Call::new(17, 3);
        run(&mut queued(stranger.payload()));
        run(&mut Box::new([0u8; 0x40]));
        assert_eq!(calls(), ["apply tag 10", "apply tag 99", "apply tag 3"]);
        rt.pump();
        let applied: Vec<_> = events(dir.path()).into_iter().filter(|(k, _)| k == "applied").map(|(_, p)| p).collect();
        assert_eq!(applied, ["{[\"id\"]=1,[\"ok\"]=true}", "{[\"id\"]=2,[\"ok\"]=false}"]);
        match rt.counters().snapshot() {
            Event::Audit { counts, total, unreadable, .. } => {
                assert_eq!(counts, [(17, 1), (52, 2)]);
                assert_eq!((total, unreadable), (4, 1));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn the_dev_dump_decodes_a_held_build_command_and_its_application_and_is_off_without_the_switch() {
        let dir = TempDir::new("rt-dump");
        let rt = runtime(Some(dir.path()));
        let settings = dump::Settings::from_lookup(|_| Some("1".into())).unwrap();
        rt.enable_dump(settings, Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        // a build whose toRemove vector holds two entity ids
        let ids: Vec<u32> = vec![62470, 62471];
        let mut call = Call::new(52, 10);
        let (begin, end) = (ids.as_ptr() as usize, ids.as_ptr() as usize + 8);
        call.c.payload[0x240..0x248].copy_from_slice(&begin.to_ne_bytes());
        call.c.payload[0x248..0x250].copy_from_slice(&end.to_ne_bytes());
        call.c.payload[0x250..0x258].copy_from_slice(&end.to_ne_bytes());
        let mut other = Call::new(17, 3);
        unsafe { rt.on_add(call.args(), at(SITE_A)) };
        unsafe { rt.on_add(other.args(), at(SITE_B)) };
        ctl(
            dir.path(),
            "release	1	{[\"id\"]=1}
",
        );
        let mut nop = Call::new(27, 50);
        unsafe { rt.on_add(nop.args(), at(0x999)) };
        let mut sim = [0u8; 8];
        let mut queued = Box::new([0u8; 0x40]);
        queued[..8].copy_from_slice(&call.payload().to_ne_bytes());
        unsafe { rt.on_apply(sim.as_mut_ptr(), queued.as_mut_ptr()) };
        // a script command (never held, no key) is not dumped at level 1
        let script = Call::new(52, 77);
        let mut queued2 = Box::new([0u8; 0x40]);
        queued2[..8].copy_from_slice(&script.payload().to_ne_bytes());
        unsafe { rt.on_apply(sim.as_mut_ptr(), queued2.as_mut_ptr()) };
        let index = std::fs::read_to_string(dir.path().join(dump::INDEX_FILE)).expect("an index file");
        assert_eq!(index.lines().count(), 3, "{index}");
        assert!(
            index.lines().next().unwrap().starts_with("seq=1 event=held key=1 site=0x4d396f tool=Bulldozer"),
            "{index}"
        );
        let text = std::fs::read_to_string(dir.path().join(dump::DUMP_FILE)).expect("a dump file");
        let headers: Vec<&str> = text.lines().filter(|l| l.starts_with("=== dump")).collect();
        assert_eq!(headers.len(), 3, "{headers:?}");
        assert!(
            headers[0]
                .starts_with("=== dump #1 event=held key=1 site=0x4d396f tool=0 Bulldozer kind=52 (build_proposal)"),
            "{}",
            headers[0]
        );
        assert!(headers[1].starts_with("=== dump #2 event=apply key=1 site=none tool=none kind=52"), "{}", headers[1]);
        assert!(
            headers[2].starts_with("=== dump #3 event=applied key=1") && headers[2].contains("group=2"),
            "{}",
            headers[2]
        );
        assert!(text.contains("ent?62470 ent?62471"), "the vector behind the pointer is decoded");
        assert!(text.contains("KNOWN +0x0240 toRemove: vector<Entity>"));

        // without the switch nothing is created
        let quiet = TempDir::new("rt-nodump");
        let rt = runtime(Some(quiet.path()));
        ctl(quiet.path(), HOLD_ON);
        rt.pump();
        let mut c = Call::new(52, 10);
        unsafe { rt.on_add(c.args(), at(SITE_A)) };
        assert!(!quiet.path().join(dump::DUMP_FILE).exists());
    }

    #[test]
    fn the_portable_dump_writes_held_and_released_commands_and_refuses_an_unknown_build() {
        let dir = TempDir::new("rt-portable");
        let rt = runtime(Some(dir.path()));
        let settings = portable::Settings::from_lookup(|_| Some("1".into())).unwrap();
        rt.enable_portable(settings, Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut call = Call::new(52, 10);
        unsafe { rt.on_add(call.args(), at(SITE_A)) };
        ctl(
            dir.path(),
            "release	1	{[\"id\"]=1}
",
        );
        let mut nop = Call::new(27, 50);
        unsafe { rt.on_add(nop.args(), at(0x999)) };
        let mut sim = [0u8; 8];
        let mut queued = Box::new([0u8; 0x40]);
        queued[..8].copy_from_slice(&call.payload().to_ne_bytes());
        unsafe { rt.on_apply(sim.as_mut_ptr(), queued.as_mut_ptr()) };
        // a script command is not written at level 1
        let script = Call::new(52, 77);
        let mut queued2 = Box::new([0u8; 0x40]);
        queued2[..8].copy_from_slice(&script.payload().to_ne_bytes());
        unsafe { rt.on_apply(sim.as_mut_ptr(), queued2.as_mut_ptr()) };
        let text = std::fs::read_to_string(dir.path().join(portable::PORTABLE_FILE)).expect("a portable file");
        let heads: Vec<&str> = text.lines().filter(|l| l.starts_with("=== portable")).collect();
        // the fake build is not game build 40420: both are refused, with the reason, and nothing is guessed
        assert_eq!(heads.len(), 2, "{text}");
        assert!(heads[0].contains("REFUSED event=held key=1") && heads[0].contains("game build 40420"), "{}", heads[0]);
        assert!(heads[1].contains("REFUSED event=apply key=1"), "{}", heads[1]);
    }

    #[test]
    fn at_level_three_script_commands_are_dumped_only_when_they_change_something() {
        let dir = TempDir::new("rt-portable3");
        let rt = runtime(Some(dir.path()));
        let settings = portable::Settings::from_lookup(|_| Some("3".into())).unwrap();
        rt.enable_portable(settings, Some(dir.path()));
        let ids: Vec<u32> = vec![62470, 62471];
        let mut busy = Call::new(52, 20);
        let (begin, end) = (ids.as_ptr() as usize, ids.as_ptr() as usize + 8);
        busy.c.payload[0x240..0x248].copy_from_slice(&begin.to_ne_bytes());
        busy.c.payload[0x248..0x250].copy_from_slice(&end.to_ne_bytes());
        busy.c.payload[0x250..0x258].copy_from_slice(&end.to_ne_bytes());
        let idle = Call::new(52, 0); // the fake payload keeps its tag in byte 8: 0 leaves the proposal untouched
        let mut sim = [0u8; 8];
        for c in [&idle, &busy, &idle] {
            let mut queued = Box::new([0u8; 0x40]);
            queued[..8].copy_from_slice(&c.payload().to_ne_bytes());
            unsafe { rt.on_apply(sim.as_mut_ptr(), queued.as_mut_ptr()) };
        }
        let text = std::fs::read_to_string(dir.path().join(portable::PORTABLE_FILE)).expect("a portable file");
        let heads: Vec<&str> = text.lines().filter(|l| l.starts_with("=== portable")).collect();
        assert_eq!(heads.len(), 1, "only the command with a removal: {text}");
        assert!(heads[0].contains("event=apply key=none"), "{}", heads[0]);
    }

    #[test]
    fn the_rebuild_self_check_refuses_when_the_game_allocator_is_not_where_it_should_be() {
        let dir = TempDir::new("rt-rebuild");
        let rt = runtime(Some(dir.path()));
        let settings = portable::Settings::from_var(|_| Some("1".into()), rebuild::REBUILD_VAR).unwrap();
        rt.enable_rebuild(settings, Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut call = Call::new(52, 10);
        unsafe { rt.on_add(call.args(), at(SITE_A)) };
        let text = std::fs::read_to_string(dir.path().join(rebuild::REBUILD_FILE)).expect("a report");
        // the fake image has no operator new at the real RVA: nothing is called, the reason is written
        assert!(text.contains("=== rebuild check #1 event=held key=1 REFUSED:"), "{text}");
    }

    #[test]
    fn the_audit_event_goes_out_after_its_interval() {
        let dir = TempDir::new("rt-audit");
        let rt = runtime(Some(dir.path()));
        let mut c = Call::new(17, 1);
        let mut sim = [0u8; 8];
        unsafe { rt.on_apply(sim.as_mut_ptr(), c.c.command.as_mut_ptr()) };
        rt.pump();
        assert_eq!(kinds(dir.path()), ["ready"]);
        rt.advance(AUDIT_INTERVAL_MS);
        rt.pump();
        assert_eq!(kinds(dir.path()), ["ready", "audit"]);
        rt.advance(AUDIT_INTERVAL_MS);
        rt.pump();
        assert_eq!(kinds(dir.path()).len(), 2, "unchanged counts are not repeated");
    }

    #[test]
    fn an_interface_player_order_is_answered_and_kept_only_where_the_piece_is_armed() {
        let dir = TempDir::new("rt-iplayer");
        let rt = runtime(Some(dir.path()));
        rt.pump();
        // the process's piece is not armed in the tests: the order is refused and nothing is kept
        ctl(dir.path(), "interface_player\t1\t{[\"entity\"]=26044}\n");
        rt.pump();
        let ev = events(dir.path());
        let answer = &ev.last().unwrap();
        assert_eq!(answer.0, "interface_player");
        assert!(answer.1.contains("[\"player\"]=-1") && answer.1.contains("not installed"), "{answer:?}");
        assert!(!ev[0].1.contains("interface_player"), "ready lists it only when armed: {}", ev[0].1);
        assert_eq!(crate::iplayer::STATE.wanted(), None);
        // an armed piece keeps the order and says it in the event
        let armed = crate::iplayer::State::new();
        armed.arm();
        let e = rt.interface_player_order(&armed, Some(26044));
        assert_eq!(e, Event::InterfacePlayer { player: 26044, savegame: -1, front: -1, world: 0, why: String::new() });
        assert_eq!((armed.wanted(), rt.interface_player_order(&armed, None)), (Some(26044), armed.event("")));
        assert_eq!(armed.wanted(), None);
    }

    #[test]
    fn ready_says_what_is_active_and_old_control_lines_are_history() {
        let dir = TempDir::new("rt-ready");
        ctl(dir.path(), HOLD_ON); // left over from an earlier run
        let rt = runtime(Some(dir.path()));
        rt.pump();
        let ev = events(dir.path());
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert_eq!(ev[0].0, "ready");
        for needle in [
            "\"build\"]=\"fake\"",
            "\"clock\"]=false",
            "[1]=\"apply_command\"",
            "\"build_proposal\"]=52",
            "\"sites\"]=2",
        ] {
            assert!(ev[0].1.contains(needle), "{needle} in {}", ev[0].1);
        }
        assert!(!rt.live.load(Relaxed), "the old hold_on was not acted on");
    }

    #[test]
    fn ping_is_answered_by_the_pump_alone() {
        let dir = TempDir::new("rt-ping");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), "ping\t1\t{[\"n\"]=12}\n");
        rt.pump();
        assert_eq!(events(dir.path())[1], ("pong".to_string(), "{[\"n\"]=12}".to_string()));
    }

    #[test]
    fn bad_control_lines_are_reported_not_fatal() {
        let dir = TempDir::new("rt-bad");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), "explode\t1\t{}\nrelease\t1\t{}\ngarbage\nping\t1\t{[\"n\"]=1}\n");
        rt.pump();
        let k = kinds(dir.path());
        assert_eq!(k.iter().filter(|x| *x == "error").count(), 3, "{k:?}"); // two bad lines, one discarded line
        assert!(k.contains(&"pong".to_string()), "and the good line after them still works");
    }

    // ---- the engine throws, our bookkeeping fails: the game goes on

    fn thrown(r: std::thread::Result<impl std::fmt::Debug>) -> bool {
        matches!(r, Err(p) if p.downcast_ref::<&str>() == Some(&THROWN))
    }

    #[test]
    fn an_exception_from_add_passes_through_the_hook_on_every_path() {
        let dir = TempDir::new("rt-throw-add");
        let rt = runtime(Some(dir.path()));
        // holding off: the plain pass-through
        THROW.with(|t| t.set(true));
        let mut c = Call::new(52, 1);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { rt.on_add(c.args(), at(SITE_A)) }));
        assert!(thrown(r));
        THROW.with(|t| t.set(false));
        // holding on: the release of a held command is what calls Add, from inside the hook
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut a = Call::new(52, 10);
        unsafe { rt.on_add(a.args(), at(SITE_A)) };
        ctl(dir.path(), "release\t1\t{[\"at\"]=0,[\"id\"]=1}\n");
        THROW.with(|t| t.set(true));
        let mut nop = Call::new(27, 50);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { rt.on_add(nop.args(), at(0x999)) }));
        assert!(thrown(r));
        THROW.with(|t| t.set(false));
        // and the module is still usable afterwards: nothing is stuck locked or half-counted
        calls();
        let mut again = Call::new(27, 51);
        unsafe { rt.on_add(again.args(), at(0x999)) };
        assert_eq!(calls(), ["add kind 27 tag 51"]);
        rt.pump();
    }

    #[test]
    fn an_exception_from_apply_passes_through_the_hook() {
        let dir = TempDir::new("rt-throw-apply");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut sim = [0u8; 8];
        let mut command = cmd(52, 4);
        THROW.with(|t| t.set(true));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            rt.on_apply(sim.as_mut_ptr(), command.command.as_mut_ptr())
        }));
        assert!(thrown(r));
        THROW.with(|t| t.set(false));
        assert_eq!(unsafe { rt.on_apply(sim.as_mut_ptr(), command.command.as_mut_ptr()) }, 7, "the next one is fine");
        calls();
    }

    fn broken_clock() -> Option<u64> {
        panic!("a bug in the bookkeeping")
    }

    #[test]
    fn a_bug_in_the_add_bookkeeping_still_calls_the_original() {
        let dir = TempDir::new("rt-bug-add");
        let mut rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        rt.clock = Some(broken_clock); // read inside the planning, under the machine's lock
        let mut c = Call::new(52, 1);
        let out = unsafe { rt.on_add(c.args(), at(SITE_A)) };
        assert_eq!(out, c.args().out);
        assert_eq!(calls(), ["add kind 52 tag 1"], "the game's call was made as if the module were not there");
        rt.clock = None;
        rt.pump();
        assert_eq!(faults(dir.path()), ["add hook repair"], "but not silently: the build went in at the click");
        // the lock the planning died holding is poisoned, not stuck: later calls still work
        let mut next = Call::new(27, 2);
        unsafe { rt.on_add(next.args(), at(SITE_A)) };
        assert_eq!(calls(), ["add kind 27 tag 2"]);
    }

    fn panicking_log(_: &str) {
        panic!("a bug in the bookkeeping while a command is read")
    }

    #[test]
    fn a_panic_while_a_command_to_hold_is_read_leaves_it_whole_for_the_original_add() {
        let dir = TempDir::new("rt-bug-hold");
        let mut rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        // the fake build's layout table refuses this command's input text and says so in the log: here, a panic. It used to come
        // after the capture had emptied the caller's command, and the original Add then got an empty command (a lost build, a
        // tool whose callback never came).
        rt.log = panicking_log;
        let mut c = Call::new(52, 1);
        let out = unsafe { rt.on_add(c.args(), at(SITE_A)) };
        assert_eq!(out, c.args().out);
        assert_eq!(calls(), ["add kind 52 tag 1"], "the engine got the command whole, as if the module were not there");
        assert_eq!(c.done[..], [0; engine::FUNCTION_SIZE][..]);
        assert_eq!(*c.progress, [5; PROGRESS_SIZE], "progress untouched");
        rt.log = quiet;
        rt.pump();
        assert!(!kinds(dir.path()).contains(&"held".to_string()), "nothing was held");
        // the next tool command is held as usual (the lock the planning died holding is poisoned, not stuck)
        let mut next = Call::new(52, 2);
        unsafe { rt.on_add(next.args(), at(SITE_A)) };
        assert!(calls().is_empty(), "held, not passed on");
        rt.pump();
        assert!(kinds(dir.path()).contains(&"held".to_string()));
    }

    #[test]
    fn a_panicking_service_round_is_a_repair_fault_and_three_in_a_row_a_stop() {
        let dir = TempDir::new("rt-pump-panic");
        let mut rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.log = panicking_log; // the round's log lines (the holding event's) panic
        assert_eq!(rt.pump_round(), 0);
        rt.log = quiet;
        rt.pump_round();
        assert_eq!(faults(dir.path()), ["service thread repair"]);
        rt.log = panicking_log;
        for _ in 0..3 {
            ctl(dir.path(), "bogus\t1\t{}\n"); // an error event: a log line, which panics
            rt.pump_round();
        }
        rt.log = quiet;
        rt.pump_round();
        assert_eq!(
            faults(dir.path()),
            ["service thread repair", "service thread repair", "service thread repair", "service thread stop"]
        );
        assert!(rt.faults.stopped());
    }

    #[test]
    fn a_runtime_without_the_engine_helpers_only_counts() {
        let rt = Runtime::new(Init {
            build: build(),
            base: BASE,
            hold: HoldConfig::default(),
            engine: None,
            log: quiet,
            hooks: vec!["apply_command"],
            poolpatch: false,
            version: "test",
        });
        rt.set_originals(Some(e_add), None);
        let mut c = Call::new(52, 1);
        unsafe { rt.on_add(c.args(), at(SITE_A)) };
        assert_eq!(calls(), ["add kind 52 tag 1"]);
    }

    const PATCH_NEXT: &str = "patch_next	1	{[\"adds\"]=2,[\"frozen\"]={[1]=1}}
";

    #[test]
    fn a_patch_next_written_just_before_the_command_is_seen_by_its_add_with_holding_off_and_no_pump() {
        let dir = TempDir::new("rt-patch-late");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_patch();
        ctl(dir.path(), PATCH_NEXT);
        assert!(!rt.live.load(Relaxed), "holding is off: the hold path does not look at the file");
        let mut c = Call::new(52, 1);
        unsafe { rt.on_add(c.args(), at(0x777)) };
        assert_eq!(calls(), ["add kind 52 tag 1"], "the original Add still runs");
        rt.pump();
        let ev = events(dir.path());
        let refused: Vec<_> = ev.iter().filter(|(k, _)| k == "patch_refused").collect();
        assert_eq!(refused.len(), 1, "{ev:?}");
        assert!(!refused[0].1.contains("expired"), "the order was taken by this very Add: {refused:?}");
        assert!(!rt.patch.pending());
    }

    #[test]
    fn lines_the_command_hook_read_for_the_patch_order_still_reach_the_machine() {
        let dir = TempDir::new("rt-patch-backlog");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_patch();
        ctl(
            dir.path(),
            &format!(
                "{PATCH_NEXT}hold_on	1	{{[\"kinds\"]={{[1]=\"build_proposal\"}}}}
"
            ),
        );
        let mut c = Call::new(52, 1);
        unsafe { rt.on_add(c.args(), at(0x777)) };
        assert!(!rt.patch.pending(), "the order was read in and taken by this Add");
        calls();
        rt.pump();
        assert!(rt.live.load(Relaxed), "hold_on was not lost");
        assert!(kinds(dir.path()).contains(&"holding".to_string()));
    }

    #[test]
    fn a_build_proposal_from_a_tool_site_is_reported_once_as_skipped_and_the_order_stays() {
        let dir = TempDir::new("rt-patch-skip");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_patch();
        rt.arm_patch(vec![1], 2, vec![], 0, rt.now_ms());
        for tag in 1..=3 {
            let mut c = Call::new(52, tag);
            unsafe { rt.on_add(c.args(), at(SITE_A)) };
        }
        calls();
        assert!(rt.patch.pending());
        rt.pump();
        let skipped = events(dir.path()).into_iter().filter(|(k, _)| k == "patch_skipped").count();
        assert_eq!(skipped, 1);
        rt.advance(crate::cmd_patch::DEFAULT_TTL_MS + 1);
        rt.pump();
        let ev = events(dir.path());
        let (_, why) = ev.iter().find(|(k, _)| k == "patch_refused").expect("expired");
        assert!(why.contains("3 Add(s), 3 build_proposal, 3 from construction tools"), "{why}");
        assert!(why.contains("kinds [52x3]") && why.contains("at arming: no build_proposal Add yet"), "{why}");
    }

    /// An `optional<Proposal>` as Convert leaves it: addedNodes of `nodes`, `cons` constructions, the flag at +0x358.
    struct Out {
        buf: Vec<u8>,
        _blocks: Vec<Vec<u8>>,
    }

    fn out(nodes: usize, cons: usize, has_value: bool) -> Out {
        let mut buf = vec![0u8; 0x360];
        let mut blocks = Vec::new();
        let mut vec_at = |off: usize, n: usize, stride: usize| {
            let mut b = vec![0u8; (n * stride).max(8)];
            if off == 0 {
                for i in 0..n {
                    b[i * stride + 4..i * stride + 8].copy_from_slice(&(i as f32 * 10.0).to_le_bytes());
                }
            }
            let p = b.as_ptr() as u64;
            buf[off..off + 8].copy_from_slice(&p.to_le_bytes());
            buf[off + 8..off + 16].copy_from_slice(&(p + (n * stride) as u64).to_le_bytes());
            buf[off + 16..off + 24].copy_from_slice(&(p + (n * stride) as u64).to_le_bytes());
            blocks.push(b);
        };
        if nodes > 0 {
            vec_at(0, nodes, 0x40);
        }
        if cons > 0 {
            vec_at(0x258, cons, 0xe48);
        }
        buf[0x358] = u8::from(has_value);
        Out { buf, _blocks: blocks }
    }

    fn patch_events(dir: &Path) -> Vec<(String, String)> {
        events(dir).into_iter().filter(|(k, _)| k.starts_with("patch")).collect()
    }

    #[test]
    fn a_conversion_that_produced_nothing_or_another_shape_is_logged_and_leaves_the_order_armed() {
        let dir = TempDir::new("rt-convert-skip");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_patch();
        rt.arm_patch(vec![1], 2, vec![], 0, rt.now_ms());
        let none = out(2, 1, false);
        rt.on_convert(none.buf.as_ptr() as usize, at(0xe2b1a1));
        let other = out(3, 1, true);
        rt.on_convert(other.buf.as_ptr() as usize, at(0xe2b1a1));
        let roads = out(2, 0, true);
        rt.on_convert(roads.buf.as_ptr() as usize, None);
        assert!(rt.patch.pending(), "no conversion described by the order: it keeps waiting");
        rt.pump();
        let ev = patch_events(dir.path());
        assert_eq!(ev.len(), 3, "{ev:?}");
        assert!(
            ev[0].1.contains("no proposal") && ev[1].1.contains("3 added nodes") && ev[2].1.contains("no construction"),
            "{ev:?}"
        );
    }

    #[test]
    fn a_matching_conversion_reaches_the_checks_and_a_refusal_consumes_the_order() {
        let dir = TempDir::new("rt-convert-match");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_patch();
        rt.arm_patch(vec![1], 2, vec![], 0, rt.now_ms());
        let m = out(2, 1, true);
        rt.on_convert(m.buf.as_ptr() as usize, at(0xe2b1a1));
        // the test process has no verified engine allocator: the order is refused (the write itself is covered by cmd_patch's tests)
        assert!(!rt.patch.pending());
        rt.pump();
        let ev = patch_events(dir.path());
        assert!(matches!(ev.as_slice(), [(k, why)] if k == "patch_refused" && why.contains("convert:")), "{ev:?}");
    }

    #[test]
    fn a_conversion_reads_an_order_the_service_thread_has_not_seen_yet() {
        let dir = TempDir::new("rt-convert-late");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_patch();
        ctl(dir.path(), PATCH_NEXT);
        let o = out(3, 1, true);
        rt.on_convert(o.buf.as_ptr() as usize, at(0xe2b1a1));
        assert!(rt.patch.pending(), "armed by the conversion itself, and 3 nodes are not the order's 2");
        assert_eq!(patch_events_after(&rt, dir.path()), 1);
    }

    fn patch_events_after(rt: &Runtime, dir: &Path) -> usize {
        rt.pump();
        patch_events(dir).len()
    }

    #[test]
    fn a_build_proposal_add_after_a_patched_conversion_just_clears_the_order() {
        let dir = TempDir::new("rt-convert-add");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_patch();
        rt.arm_patch(vec![1], 2, vec![], 0, rt.now_ms());
        rt.patch.converted_one();
        let mut c = Call::new(52, 1);
        unsafe { rt.on_add(c.args(), at(0x777)) };
        assert_eq!(calls(), ["add kind 52 tag 1"]);
        assert!(!rt.patch.pending());
        rt.pump();
        assert!(patch_events(dir.path()).is_empty(), "no refusal and no second write");
    }

    #[test]
    fn an_order_by_position_skips_a_conversion_without_the_node_and_dumps_every_new_shape_once() {
        let dir = TempDir::new("rt-convert-at");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_patch();
        rt.arm_patch(vec![], 0, vec![[10.0, 0.0, 0.0]], 0, rt.now_ms());
        let elsewhere = out(1, 1, true); // its only node is at x = 0
        rt.on_convert(elsewhere.buf.as_ptr() as usize, at(0xe2b1a1));
        rt.on_convert(elsewhere.buf.as_ptr() as usize, at(0xe2b1a1));
        let failed = out(4, 1, false);
        rt.on_convert(failed.buf.as_ptr() as usize, at(0xe2b1a1));
        let four = out(4, 1, true);
        rt.on_convert(four.buf.as_ptr() as usize, at(0xe2b1a1));
        rt.pump();
        let ev = events(dir.path());
        let dumps: Vec<_> = ev.iter().filter(|(k, _)| k == "convert_dump").collect();
        assert_eq!(dumps.len(), 3, "{ev:?}"); // 1 node (once), no value, 4 nodes
        assert!(dumps[1].1.contains("NO VALUE (Convert failed)"), "{dumps:?}");
        // the matching one reached the allocator check (not verified in the test process) and was refused: that consumes the order
        assert!(ev.iter().any(|(k, w)| k == "patch_refused" && w.contains("convert: the game's allocator")), "{ev:?}");
    }

    #[test]
    fn before_the_construction_update_the_order_is_checked_against_the_nodes_built_so_far() {
        let dir = TempDir::new("rt-convert-update");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_patch();
        assert!(!rt.before_convert_update(out(2, 0, true).buf.as_ptr() as usize), "nothing armed: untouched");
        rt.arm_patch(vec![1], 2, vec![], 0, rt.now_ms());
        let three = out(3, 0, true);
        assert!(rt.before_convert_update(three.buf.as_ptr() as usize));
        assert!(rt.patch.pending(), "another number of nodes: skipped, still armed");
        let two = out(2, 0, true);
        assert!(rt.before_convert_update(two.buf.as_ptr() as usize));
        // the test process has no verified engine allocator: refused after the checks passed (the write is covered by cmd_patch's tests)
        assert!(!rt.patch.pending());
        rt.after_convert_update(two.buf.as_ptr() as usize);
        rt.pump();
        let ev = events(dir.path());
        assert!(
            ev.iter().any(|(k, w)| k == "patch_skipped" && w.contains("convert update: the command has 3 added nodes")),
            "{ev:?}"
        );
        assert!(
            ev.iter().any(|(k, w)| k == "patch_refused" && w.contains("convert update: the game's allocator")),
            "{ev:?}"
        );
    }

    const INJECT_NEXT: &str = "inject_next	1	{[\"key\"]=5}
";

    #[test]
    fn an_inject_next_is_read_by_the_commands_own_add_and_a_command_that_cannot_be_emptied_is_reported_untouched() {
        let dir = TempDir::new("rt-inject");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_inject(portable::Settings::from_var(|_| Some("1".into()), crate::inject::INJECT_VAR).unwrap());
        ctl(dir.path(), INJECT_NEXT);
        // another kind of command does not take the order
        let mut other = Call::new(17, 1);
        unsafe { rt.on_add(other.args(), at(0x777)) };
        assert!(!rt.inject.pending(), "the file is only read for a build_proposal");
        // a tool's command is not the mod's
        let mut tool = Call::new(52, 2);
        unsafe { rt.on_add(tool.args(), at(SITE_A)) };
        // the mod's placeholder, identified by its place; the test process has no verified engine functions, so the refused order's
        // command cannot be emptied: it says so (`untouched`) and the mod asks for the repair
        let text = format!(
            "proposal.addedNodes.#\tn:1\nproposal.addedNodes[0].position\tv3:{:08x},00000000,00000000\n",
            100f32.to_bits()
        );
        std::fs::write(dir.path().join(crate::inject::inject_file_name(5)), text).unwrap();
        let (mut c, _node) = road_placeholder(100.0);
        let before = c.c.payload.to_vec();
        unsafe { rt.on_add(c.args(), at(0x777)) };
        assert!(calls().last().is_some_and(|l| l.starts_with("add kind 52")), "the original Add still runs");
        assert_eq!(c.c.payload.to_vec(), before, "not emptied");
        assert!(!rt.inject.pending());
        rt.pump();
        let ev = events(dir.path());
        assert!(
            ev.iter().any(|(k, w)| k == "inject_refused"
                && w.contains("key 5")
                && w.contains("[\"outcome\"]=\"untouched\"")
                && w.contains("the layout is that of game build")),
            "{ev:?}"
        );
    }

    #[test]
    fn a_command_that_cannot_be_identified_is_never_emptied_and_the_order_expires_into_a_refusal() {
        let dir = TempDir::new("rt-inject-unknown");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_inject(portable::Settings::from_var(|_| Some("1".into()), crate::inject::INJECT_VAR).unwrap());
        // engine stand-ins are there: the command COULD be emptied; it must not be, because nothing says it is the mod's
        inject::TEST_FNS.with(|f| {
            f.set(Some(crate::native_copy::ProposalFns { ctor: p_ctor, dtor: p_dtor, copy: p_copy, assign: p_copy }))
        });
        let mut sim = [0u8; 8];
        // 1. the side file is missing (unreadable): a foreign build_proposal (a town's road) goes by
        ctl(dir.path(), "inject_next\t1\t{[\"held\"]=3,[\"key\"]=5}\n");
        let (mut town, _n1) = road_placeholder(400.0);
        let before = town.c.payload.to_vec();
        calls();
        unsafe { rt.on_apply(sim.as_mut_ptr(), town.c.command.as_mut_ptr()) };
        let seen = calls();
        assert!(
            seen.len() == 1 && seen[0].starts_with("apply tag"),
            "applied as it is, no empty Proposal assigned: {seen:?}"
        );
        assert_eq!(town.c.payload.to_vec(), before, "untouched");
        assert!(rt.inject.pending(), "the order keeps waiting");
        assert!(lock(&rt.added).is_empty());
        // 2. a side file whose text names no place: no command can be identified by it either
        std::fs::write(dir.path().join(crate::inject::inject_file_name(6)), "toAdd.#\tn:0\n").unwrap();
        ctl(dir.path(), "inject_next\t1\t{[\"key\"]=6}\n");
        rt.pump(); // the module reads it: it waits behind key 5
        let (mut town2, _n2) = road_placeholder(400.0);
        let before2 = town2.c.payload.to_vec();
        unsafe { rt.on_apply(sim.as_mut_ptr(), town2.c.command.as_mut_ptr()) };
        let seen = calls();
        assert!(seen.len() == 1 && seen[0].starts_with("apply tag"), "{seen:?}");
        assert_eq!(town2.c.payload.to_vec(), before2, "untouched");
        assert!(rt.inject.pending());
        // the order expires unused: a refusal without a command (the mod asks for the repair)
        rt.advance(crate::inject::DEFAULT_TTL_MS + 1);
        rt.pump();
        rt.pump();
        let ev = events(dir.path());
        assert!(
            ev.iter().any(|(k, w)| k == "inject_refused"
                && w.contains("[\"key\"]=6")
                && w.contains("[\"outcome\"]=\"no_command\"")),
            "{ev:?}"
        );
        assert!(!ev.iter().any(|(_, w)| w.contains("[\"outcome\"]=\"nothing\"")), "nothing was emptied: {ev:?}");
        inject::TEST_FNS.with(|f| f.set(None));
    }

    // stand-ins for the engine's Proposal functions: an empty Proposal is zeros, the assignment copies the Proposal's bytes
    unsafe extern "system-unwind" fn p_ctor(p: *mut u8) -> *mut u8 {
        unsafe { core::ptr::write_bytes(p, 0, crate::native_copy::PROPOSAL_SIZE) };
        note("proposal ctor");
        p
    }
    unsafe extern "system-unwind" fn p_dtor(_p: *mut u8) {
        note("proposal dtor");
    }
    unsafe extern "system-unwind" fn p_copy(d: *mut u8, s: *const u8) -> *mut u8 {
        unsafe { core::ptr::copy_nonoverlapping(s, d, crate::native_copy::PROPOSAL_SIZE) };
        note("proposal assign");
        d
    }

    /// The mod's placeholder of a road: one added node at `x` (this game's rebuild of the record).
    fn road_placeholder(x: f32) -> (Call, Box<[u8; 0x40]>) {
        let mut c = Call::new(52, 0);
        let mut node = Box::new([0u8; 0x40]);
        node[4..8].copy_from_slice(&x.to_bits().to_le_bytes());
        let b = node.as_ptr() as u64;
        for (k, v) in [b, b + 0x40, b + 0x40].iter().enumerate() {
            c.c.payload[k * 8..k * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        (c, node)
    }

    #[test]
    fn a_refused_order_empties_the_mods_command_fails_its_held_command_and_reports_the_outcome() {
        let dir = TempDir::new("rt-inject-closed");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_inject(portable::Settings::from_var(|_| Some("1".into()), crate::inject::INJECT_VAR).unwrap());
        inject::TEST_FNS.with(|f| {
            f.set(Some(crate::native_copy::ProposalFns { ctor: p_ctor, dtor: p_dtor, copy: p_copy, assign: p_copy }))
        });
        // the originator's text of a road with a node at x = 100; the order names this game's deferred held command 3
        let text = format!(
            "proposal.addedNodes.#\tn:1\nproposal.addedNodes[0].position\tv3:{:08x},00000000,00000000\n",
            100f32.to_bits()
        );
        std::fs::write(dir.path().join(crate::inject::inject_file_name(5)), text).unwrap();
        ctl(dir.path(), "inject_next\t1\t{[\"held\"]=3,[\"key\"]=5}\n");
        let mut sim = [0u8; 8];
        // a town's road elsewhere goes by first: not the mod's command, untouched, the order keeps waiting
        let (mut town, _n1) = road_placeholder(400.0);
        let town_before = town.c.payload.to_vec();
        unsafe { rt.on_apply(sim.as_mut_ptr(), town.c.command.as_mut_ptr()) };
        assert_eq!(town.c.payload.to_vec(), town_before, "someone else's command is never touched");
        assert!(rt.inject.pending(), "put back");
        calls();
        // the mod's placeholder at the text's place: the identity holds, the order is refused (this test's layout is not the game's) and
        // the command is emptied before the simulation applies it
        let (mut ours, _n2) = road_placeholder(100.0);
        ours.c.payload[0x358] = 1;
        unsafe { rt.on_apply(sim.as_mut_ptr(), ours.c.command.as_mut_ptr()) };
        assert_eq!(
            calls(),
            ["proposal ctor", "proposal assign", "proposal dtor", "apply tag 0"],
            "emptied, then applied"
        );
        assert!(
            ours.c.payload[..crate::native_copy::PROPOSAL_SIZE].iter().all(|&b| b == 0),
            "nothing of the placeholder is applied"
        );
        assert_eq!((ours.c.payload[0x9b8], ours.c.payload[0x358]), (52, 1), "the kind and the Context stay");
        rt.pump();
        let ev = events(dir.path());
        assert!(
            ev.iter().any(|(k, w)| k == "inject_refused"
                && w.contains("[\"held\"]=3")
                && w.contains("[\"key\"]=5")
                && w.contains("[\"outcome\"]=\"nothing\"")
                && w.contains("the layout is that of game build")),
            "{ev:?}"
        );
        // the held command is failed at once (here it is not in the ring, so the machine says it does not know it)
        assert!(ev.iter().any(|(k, w)| k == "error" && w.contains("no held command 3")), "{ev:?}");
        inject::TEST_FNS.with(|f| f.set(None));
    }

    #[test]
    fn with_the_acting_player_an_order_whose_carrier_names_no_player_is_refused_and_one_with_a_player_goes_on() {
        let dir = TempDir::new("rt-inject-acting");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_inject(portable::Settings::from_var(|_| Some("1".into()), crate::inject::INJECT_VAR).unwrap());
        rt.enable_acting_player();
        inject::TEST_FNS.with(|f| {
            f.set(Some(crate::native_copy::ProposalFns { ctor: p_ctor, dtor: p_dtor, copy: p_copy, assign: p_copy }))
        });
        let text = format!(
            "proposal.addedNodes.#\tn:1\nproposal.addedNodes[0].position\tv3:{:08x},00000000,00000000\ncontext.player\tent:old#64976\n",
            100f32.to_bits()
        );
        let mut sim = [0u8; 8];
        for (key, player) in [(5u64, u32::MAX), (6, 52855)] {
            std::fs::write(dir.path().join(crate::inject::inject_file_name(key)), &text).unwrap();
            ctl(dir.path(), &format!("inject_next\t1\t{{[\"held\"]=3,[\"key\"]={key}}}\n"));
            let (mut ours, _n) = road_placeholder(100.0);
            ours.c.payload[crate::acting::CONTEXT_PLAYER..crate::acting::CONTEXT_PLAYER + 4]
                .copy_from_slice(&player.to_le_bytes());
            unsafe { rt.on_apply(sim.as_mut_ptr(), ours.c.command.as_mut_ptr()) };
            assert!(ours.c.payload[..crate::native_copy::PROPOSAL_SIZE].iter().all(|&b| b == 0), "refused: emptied");
        }
        rt.pump();
        let ev = events(dir.path());
        let refused = |key: u64, why: &str| {
            ev.iter().any(|(k, w)| k == "inject_refused" && w.contains(&format!("[\"key\"]={key}")) && w.contains(why))
        };
        assert!(refused(5, "acting player: the carrier command's context.player is -1"), "{ev:?}");
        // the remap went through (64976 -> 52855); the refusal is the next check's (this test's layout is not the game's)
        assert!(refused(6, "the layout is that of game build"), "{ev:?}");
        inject::TEST_FNS.with(|f| f.set(None));
    }

    #[test]
    fn the_command_that_carries_its_sends_conversion_is_the_orders_and_its_text_only_a_cross_check() {
        let dir = TempDir::new("rt-inject-positive");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_inject(portable::Settings::from_var(|_| Some("1".into()), crate::inject::INJECT_VAR).unwrap());
        inject::TEST_FNS.with(|f| {
            f.set(Some(crate::native_copy::ProposalFns { ctor: p_ctor, dtor: p_dtor, copy: p_copy, assign: p_copy }))
        });
        // the text names x = 100, the mod's own command builds at x = 300: a content check that misses (friend test 2026-10-20)
        let text = format!(
            "proposal.addedNodes.#\tn:1\nproposal.addedNodes[0].position\tv3:{:08x},00000000,00000000\n",
            100f32.to_bits()
        );
        std::fs::write(dir.path().join(crate::inject::inject_file_name(5)), text).unwrap();
        ctl(dir.path(), "inject_next\t1\t{[\"held\"]=3,[\"key\"]=5}\n");
        // the mod's send converts its command (the hook on scripting::Convert, this thread)
        let (mut conv, _n0) = road_placeholder(300.0);
        conv.c.payload[crate::native_copy::PROPOSAL_SIZE] = 1;
        rt.inject_on_convert(conv.c.payload.as_ptr() as usize);
        let mut sim = [0u8; 8];
        // a town's road applied meanwhile on this thread: not the conversion, untouched, the order waits
        let (mut town, _n1) = road_placeholder(400.0);
        let town_before = town.c.payload.to_vec();
        unsafe { rt.on_apply(sim.as_mut_ptr(), town.c.command.as_mut_ptr()) };
        assert_eq!(town.c.payload.to_vec(), town_before, "someone else's command is never touched");
        assert!(rt.inject.pending());
        calls();
        // the mod's command carries the conversion: it is the order's; its text does not fit: refused, and emptied before the apply
        let (mut ours, _n2) = road_placeholder(300.0);
        unsafe { rt.on_apply(sim.as_mut_ptr(), ours.c.command.as_mut_ptr()) };
        assert_eq!(
            calls(),
            ["proposal ctor", "proposal assign", "proposal dtor", "apply tag 0"],
            "emptied, then applied"
        );
        assert!(
            ours.c.payload[..crate::native_copy::PROPOSAL_SIZE].iter().all(|&b| b == 0),
            "the placeholder never applies"
        );
        // an order whose send ends without a command of it: refused at its send_done, not after its time to live
        ctl(dir.path(), "inject_next\t1\t{[\"key\"]=6}\nsend_done\t1\t{[\"key\"]=6}\n");
        rt.pump();
        let ev = events(dir.path());
        assert!(
            ev.iter().any(|(k, w)| k == "inject_refused"
                && w.contains("[\"key\"]=5")
                && w.contains("[\"outcome\"]=\"nothing\"")
                && w.contains(crate::inject::CROSS_CHECK)),
            "{ev:?}"
        );
        assert!(
            ev.iter().any(|(k, w)| k == "inject_refused"
                && w.contains("[\"key\"]=6")
                && w.contains("no_command")
                && w.contains("its send ended")),
            "{ev:?}"
        );
        assert!(!rt.inject.pending());
        inject::TEST_FNS.with(|f| f.set(None));
    }

    #[test]
    fn the_originators_own_proof_fails_closed_when_it_cannot_be_made_and_calls_nothing_of_the_engine_then() {
        let dir = TempDir::new("rt-own-proof");
        let rt = runtime(Some(dir.path()));
        let settings = portable::Settings::from_var(|_| Some("1".into()), crate::inject::INJECT_VAR).unwrap();
        rt.enable_native_inject(settings);
        inject::TEST_FNS.with(|f| {
            f.set(Some(crate::native_copy::ProposalFns { ctor: p_ctor, dtor: p_dtor, copy: p_copy, assign: p_copy }))
        });
        let text = format!(
            "proposal.addedNodes.#\tn:1\nproposal.addedNodes[0].position\tv3:{:08x},00000000,00000000\n",
            100f32.to_bits()
        );
        let (held, _node) = road_placeholder(100.0);
        let payload = held.c.payload.as_ptr() as usize;
        calls();
        // the functions are there (stand-ins), the game's allocator is not verified in the test process: refused, no engine call
        let why = rt.inject_own_proof(payload, &text, settings).unwrap_err();
        assert!(why.starts_with(crate::inject::OWN_PROOF) && why.contains("allocator was not verified"), "{why}");
        assert!(calls().is_empty(), "nothing of the engine was called");
        // a held command elsewhere than its text: refused by the checks before anything else
        let (other, _n2) = road_placeholder(400.0);
        let why = rt.inject_own_proof(other.c.payload.as_ptr() as usize, &text, settings).unwrap_err();
        assert!(why.contains("not the build its text describes"), "{why}");
        assert_eq!(crate::log::engine_calls_in_guard(), 0, "no engine call announced inside a guard");
        inject::TEST_FNS.with(|f| f.set(None));
    }

    thread_local! {
        static SAID: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }
    fn keep(l: &str) {
        SAID.with(|s| s.borrow_mut().push(l.to_string()));
    }

    #[test]
    fn an_orders_refund_entries_go_into_the_commands_context_through_the_engines_insert() {
        use crate::refund::tests::{calls as inserts, entry, fake_fns, Synthetic};
        let dir = TempDir::new("rt-refund");
        let mut rt = runtime(Some(dir.path()));
        rt.log = keep;
        // the mod's side file: this game's own entities, resolved from the originator's references
        std::fs::write(
            dir.path().join(crate::refund::file_name(5)),
            "50215\t-1\t3\t11\t0\t3143\n46947\t-1\t11\t11\t0\t2899\n",
        )
        .unwrap();
        let mut s = Synthetic::new(&[]); // the script command's Context container, as the engine made it (empty)
        let fns = fake_fns(&mut s);
        inject::TEST_REFUND_FNS.with(|f| f.set(Some(fns)));
        let n0 = inserts();
        // the checks refuse a count that is not the file's and a missing file, before any call
        let why = rt.refund_job(5, 3, dir.path(), s.addr()).err().unwrap();
        assert!(why.contains("the order says 3"), "{why}");
        let why = rt.refund_job(6, 1, dir.path(), s.addr()).err().unwrap();
        assert!(why.contains("cannot read native_refund_6.txt"), "{why}");
        let job = rt.refund_job(5, 2, dir.path(), s.addr()).expect("checked");
        assert_eq!(inserts(), n0, "the checks call nothing of the engine");
        rt.refund_engine(5, s.addr(), job).expect("set");
        assert_eq!(inserts() - n0, 1, "one call of the engine's insert");
        let (have, _) = crate::refund::read_container(&crate::dump::LiveMem, s.addr()).unwrap();
        assert_eq!(have, vec![entry(50215, 3, 3143), entry(46947, 11, 2899)]);
        let said = SAID.with(|l| l.borrow().clone());
        assert!(
            said.iter()
                .any(|l| l.contains("key 5: refund (native): 2 entries set in the command's Context (0 were there")
                    && l.contains("[50215{-1}rev3,11,0/3143 46947{-1}rev11,11,0/2899]")),
            "{said:?}"
        );
        assert_eq!(crate::log::engine_calls_in_guard(), 0, "the insert is not called inside a guard");
        // a container that is not an engine-made one: refused by the checks
        let zero = Box::new([0u8; 0x9c0]);
        let why = rt.refund_job(5, 2, dir.path(), zero.as_ptr() as usize).err().unwrap();
        assert!(why.contains("refund: the Context's refundableEntities has no load factor"), "{why}");
        inject::TEST_REFUND_FNS.with(|f| f.set(None));
    }

    #[test]
    fn the_originators_refund_window_goes_into_the_commands_context() {
        use crate::refund::{tests::Synthetic, WINDOW_OFFSET};
        let dir = TempDir::new("rt-refund-window");
        let mut rt = runtime(Some(dir.path()));
        rt.log = keep;
        // the script command's Context: the Lua Context has no field for the window, so the conversion leaves 0.0 (every donor text)
        let mut s = Synthetic::new(&[]);
        let dest = s.payload.as_mut_ptr() as usize;
        rt.refund_window(9, dest, 60.0).expect("written");
        assert_eq!(&s.payload[WINDOW_OFFSET..WINDOW_OFFSET + 4], &60.0f32.to_le_bytes());
        let said = SAID.with(|l| l.borrow().clone());
        assert!(
            said.iter().any(|l| l.contains("key 9: refund window 60 s of the originator's Context written")
                && l.contains("the script's conversion had 0)")),
            "{said:?}"
        );
    }

    #[test]
    fn with_the_switch_off_an_inject_next_is_refused_and_nothing_is_read() {
        let dir = TempDir::new("rt-inject-off");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), INJECT_NEXT);
        rt.pump();
        let ev = events(dir.path());
        assert!(ev.iter().any(|(k, w)| k == "inject_refused" && w.contains("is off")), "{ev:?}");
    }

    #[test]
    fn a_script_command_that_only_reaches_apply_takes_the_order_there_and_an_expiry_reports_the_applies_seen() {
        let dir = TempDir::new("rt-inject-apply");
        let rt = runtime(Some(dir.path()));
        rt.enable_native_inject(portable::Settings::from_var(|_| Some("1".into()), crate::inject::INJECT_VAR).unwrap());
        let mut sim = [0u8; 8];
        // the order is written, no Add of the command is ever seen; the simulation applies it
        ctl(dir.path(), INJECT_NEXT);
        let mut script = Call::new(52, 7);
        let before = script.c.payload.to_vec();
        unsafe { rt.on_apply(sim.as_mut_ptr(), script.c.command.as_mut_ptr()) };
        assert_eq!(calls().last().map(String::as_str), Some("apply tag 7"), "the original apply still runs");
        assert_eq!(script.c.payload.to_vec(), before, "not identified (no side file): untouched");
        assert!(rt.inject.pending(), "the order keeps waiting");
        // an order that no script command ever uses: the reason says which applies went by (it waits behind key 5)
        rt.inject.arm(9, 0, 0, rt.now_ms());
        let mut other = Call::new(17, 1);
        unsafe { rt.on_apply(sim.as_mut_ptr(), other.c.command.as_mut_ptr()) };
        assert!(!rt.note_apply_orders(Some(52), true), "a held one (the tool's) is noted, not carried out");
        rt.advance(crate::inject::DEFAULT_TTL_MS + 1);
        rt.pump();
        let ev = events(dir.path());
        assert!(
            ev.iter().any(|(k, w)| k == "inject_refused" && w.contains("key 9") && w.contains("1 held, 0 not held")),
            "{ev:?}"
        );
        assert!(
            ev.iter().any(|(k, w)| k == "inject_refused"
                && w.contains("key 5")
                && w.contains("expired")
                && w.contains("1 held, 0 not held")),
            "each order expires on its own, with the applies it saw: {ev:?}"
        );
    }

    #[test]
    fn no_call_into_the_game_is_announced_inside_a_guard_on_any_hook_path() {
        let before = crate::log::engine_calls_in_guard();
        let dir = TempDir::new("rt-no-engine-in-guard");
        let rt = runtime(Some(dir.path()));
        let set = portable::Settings::from_var(|_| Some("1".into()), crate::inject::INJECT_VAR).unwrap();
        rt.enable_native_inject(set);
        rt.enable_native_patch();
        rt.enable_rebuild(set, Some(dir.path()));
        rt.enable_native_copy(set, Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        // two tool commands held (the capture), then one released (Add) and one cancelled (destructors)
        let (mut a, mut b) = (Call::new(52, 10), Call::new(52, 11));
        unsafe { rt.on_add(a.args(), at(SITE_A)) };
        unsafe { rt.on_add(b.args(), at(SITE_B)) };
        ctl(dir.path(), "release\t1\t{[\"at\"]=0,[\"id\"]=1}\ncancel\t1\t{[\"id\"]=2}\n");
        let mut nop = Call::new(27, 50);
        unsafe { rt.on_add(nop.args(), at(0x999)) };
        assert!(calls().iter().any(|c| c == "add kind 52 tag 10"), "the release reached Add");
        // the orders on the command list's path and on the simulation's, and the two conversion hooks
        ctl(dir.path(), INJECT_NEXT);
        let mut script = Call::new(52, 3);
        unsafe { rt.on_add(script.args(), at(0x777)) };
        rt.inject.arm(6, 0, 0, rt.now_ms());
        let mut sim = [0u8; 8];
        unsafe { rt.on_apply(sim.as_mut_ptr(), script.c.command.as_mut_ptr()) };
        rt.arm_patch(vec![1], 2, vec![], 0, rt.now_ms());
        let o = out(2, 1, true);
        rt.before_convert_update(o.buf.as_ptr() as usize);
        rt.on_convert(o.buf.as_ptr() as usize, at(0xe2b1a1));
        calls();
        rt.pump();
        assert_eq!(crate::log::engine_calls_in_guard(), before, "an engine call inside log::guarded");
        // the check itself sees one
        crate::log::guarded("test", || crate::log::calls_engine("test"));
        assert_eq!(crate::log::engine_calls_in_guard(), before + 1);
    }
}
