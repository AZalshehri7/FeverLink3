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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};
use std::thread::Thread;
use std::time::Instant;

use crate::build_table::BuildAddrs;
use crate::counters::{audit_log_line, AddSource, AppliedWatch, Audit, Counters};
use crate::dump::{self, Budget, Dumper, Env, Header, LiveMem, Scope};
use crate::engine::{self, AddArgs, AddFn, EngineFns, Held};
use crate::hold::{Action, Admit, HoldConfig, HoldMachine};
use crate::link::{parse_ctl_line, CtlReader, Event, EventWriter, CTL_FILE};
use crate::portable;
use crate::rebuild;

mod cmd_patch;
mod inject;
mod native_copy;

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
    inject: crate::inject::State,
    inject_settings: OnceLock<portable::Settings>,
    inject_dir_path: Mutex<Option<std::path::PathBuf>>,
    /// The script command an `inject_next` with `held` was used for (its payload) and that deferred held command: when the simulation
    /// has applied the script command, the held command takes its result (`engine::adopt_result`).
    defer_link: Mutex<Option<(usize, u64)>>,
    /// The script command an order was used for and its timing so far (the mod's `tm`, the rebuild's wall-clock start and end and how it
    /// went): after the simulation applied that command, one `timing apply` line is written (`timing_after_apply`).
    stamp_timing: Mutex<Option<StampTiming>>,
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
            clock: None,
            dump: OnceLock::new(),
            portable: OnceLock::new(),
            rebuild: OnceLock::new(),
            native_copy: OnceLock::new(),
            patch: Default::default(),
            input: crate::input::InputOut::default(),
            text: crate::input::InputOut::with_prefix("native_text_"),
            inject: Default::default(),
            inject_settings: OnceLock::new(),
            inject_dir_path: Mutex::new(None),
            defer_link: Mutex::new(None),
            stamp_timing: Mutex::new(None),
            preview_fn: AtomicUsize::new(0),
            preview_args: Mutex::new(None),
            preview_holding: AtomicBool::new(false),
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
                let alloc = rebuild::EngineAlloc::at(self.base)?;
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
                let text = portable::encode(&dump::LiveMem, env, sink.settings, self.build.name, &head)?.text;
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
                self.build.name,
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
            self.build.name,
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
        let text =
            match portable::encode(&LiveMem, Env { image_base: self.base }, sink.settings, self.build.name, &head) {
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
            // SAFETY: the byte is read only after its range is checked.
            .and_then(|at| unsafe { crate::mem::readable(at as *const u8, 1).then(|| *(at as *const u8)) })
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

    /// Gives the held build command `id` to the game scripts as the preview of the tool `held:<id>`, through the engine's own emitter of
    /// `builder.proposalCreate` with the arguments of the newest tool preview, its own Proposal (the payload's start) and the ProposalData
    /// slot of its payload (`+0x3d8`, empty before the apply). The scripts' `guiHandleEvent` runs inside this call, so the mod has the
    /// record of the command before the `held` event reaches it. Why: a tool that sends no preview (UI::ModuleBuilder, a station module
    /// added via Configure: run s4 2026-10-14) left the mod without a record before the build, so its command was released at once and
    /// applied 7 steps before the stamp on this game alone (entity ids then differ between the games). Called on the thread of the tool
    /// that added the command (the interface's), where the tools call the emitter themselves. Nothing happens without a preview seen
    /// since holding was switched on.
    ///
    /// # Safety
    /// `payload` is the payload of a live held `build_proposal` command.
    #[allow(improper_ctypes_definitions)]
    unsafe extern "system-unwind" fn give_preview(&self, id: crate::hold::HoldId, payload: usize) {
        type Emit = unsafe extern "system-unwind" fn(u64, u64, u64, u64, u64, *const u8, usize, usize, *mut u8) -> u64;
        let emit = self.preview_fn.load(Acquire);
        let args = crate::log::guarded("preview of a held command", || *lock(&self.preview_args)).flatten();
        let (Some(a), true) = (args, emit != 0) else {
            crate::log::line(&format!(
                "hold: command {id}: no preview of its own (no tool preview seen since holding was switched on)"
            ));
            return;
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
            "hold: command {id}: giving it to the game scripts as the preview of the tool {name}"
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
        let reader = CtlReader::starting_at_end(dir.join(CTL_FILE))?;
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
        let first_seq = events.next_seq();
        events.append(std::slice::from_ref(&ready))?;
        self.say(&format!(
            "link: opened {} (control file from seq {}, events from seq {})",
            dir.display(),
            reader.last_seq(),
            first_seq
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
                Ok(crate::link::CtlMessage::InjectNext { key, ttl, held, tm, refuse }) => {
                    self.inject.arm_timed(key, ttl, held, tm, refuse, now)
                }
                Ok(crate::link::CtlMessage::CompleteNext { held, ttl, tm }) => self.inject.arm_link(held, ttl, tm, now),
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
                Ok(crate::link::CtlMessage::InjectNext { key, ttl, held, tm, refuse }) => {
                    self.inject.arm_timed(key, ttl, held, tm, refuse, now)
                }
                Ok(crate::link::CtlMessage::CompleteNext { held, ttl, tm }) => self.inject.arm_link(held, ttl, tm, now),
                Ok(msg) => machine.handle(&msg, now),
                Err(e) => machine.error("ctl", format!("line {} ({}): {e}", line.seq, line.kind), now),
            }
        }
        if poll.discarded > 0 {
            machine.error("ctl", format!("{} control line(s) discarded", poll.discarded), now);
        }
        if let Some(e) = poll.error {
            machine.error("ctl", e, now);
        }
        if poll.gone {
            machine.ctl_gone(now);
        }
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
        events.extend(self.inject.drain_events(now));
        for id in self.watch.expire(now) {
            events.push(Event::Error {
                place: "applied".into(),
                detail: format!("command {id} was released but never seen being applied"),
            });
        }
        if let Some(audit) = lock(&self.audit).poll(now, &self.counters) {
            self.say(&audit_log_line(&audit, |k| self.build.kind_name(k)));
            events.push(audit);
        }
        if events.is_empty() {
            return 0;
        }
        for e in &events {
            if let Some(line) = e.log_line() {
                self.say(&line);
            }
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
        let mut plan = AddPlan { due: Vec::new(), spill: None, held_here: false, now: 0, dump: None, preview: None };
        let hold = crate::log::guarded("add hook", || self.plan_add(&args, caller, add.is_some(), &mut plan)).flatten();
        if let Some(hold) = hold {
            // SAFETY: the arguments of the live call, as the caller guarantees.
            unsafe { self.capture_and_hold(&args, hold, &mut plan) };
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
        // both call the engine when an order is carried out: they guard their own bookkeeping
        if self.inject.enabled() {
            self.inject_add(&args, caller);
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
        let mut machine = try_lock_briefly(&self.machine)?;
        // the mod writes `release` and then sends a command to get here: look at the file now, do not wait for the pump
        self.ingest(&mut machine, now);
        plan.due = machine.take_due(self.game_time(), now);

        if let (Some(rva), Some(_)) = (site, tool) {
            // SAFETY: the kind is read only from readable memory.
            if let Some(kind) = unsafe { engine::command_kind(args.cmd, engine.kind_offset) } {
                let admit = machine.admit(kind, now);
                if admit == Admit::Off {
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
                Err(back) => plan.spill = Some(back), // refused after capture: it goes straight on
            }
        }
        crate::log::guarded("add hook (held)", || {
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
        if self.inject.enabled() || self.patch.enabled() {
            let mine = crate::log::guarded("apply hook (orders)", || {
                let kind_offset = self.build.kind_offset.unwrap_or(0) as usize;
                // SAFETY: only readable memory is read.
                let kind = unsafe { engine::command_kind(cmd, kind_offset) };
                self.note_apply_orders(kind, claimed.is_some())
            });
            if mine == Some(true) {
                // calls the engine when an order is carried out: guards its own bookkeeping
                self.apply_orders(cmd);
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
        if self.inject.enabled() {
            // the timing first: the deferred completion swaps the applied command's payload with the held one's (plain memory, no
            // engine call: `engine::adopt_result`)
            crate::log::guarded("apply hook (timing)", || self.timing_after_apply(cmd));
            crate::log::guarded("apply hook (deferred completion)", || self.complete_deferred(cmd));
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

/// The timing of an ordered script command until its stamp apply (see `Runtime::stamp_timing`).
pub(crate) struct StampTiming {
    pub payload: usize,
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
            town_candidates: None,
            town_edges: None,
            town_sort_call: None,
            town_pick_call: None,
            game_time_getter: None,
            script_pool: None,
            lua_makeseed: None,
            command_move: None,
            command_destroy: None,
            handle_destroy: None,
            ui_add_sites: &[SITE_A, SITE_B],
            fire_done_calls: &[],
            kind_offset: Some(0x9b8),
            kinds: &[("build_proposal", 52)],
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

    fn ctl(dir: &Path, text: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(CTL_FILE))
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

    const HOLD_ON: &str = "1\thold_on\t1\t{[\"kinds\"]={[1]=\"build_proposal\"}}\n";

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
        ctl(dir.path(), "2\trelease\t1\t{[\"at\"]=0,[\"id\"]=2}\n3\trelease\t1\t{[\"at\"]=0,[\"id\"]=1}\n");
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

    #[test]
    fn hold_off_releases_what_is_held_at_the_next_add() {
        let dir = TempDir::new("rt-holdoff");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut a = Call::new(52, 10);
        unsafe { rt.on_add(a.args(), at(SITE_A)) };
        ctl(dir.path(), "2\thold_off\t1\t{}\n");
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
    fn silence_gives_everything_back_without_the_mod() {
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
        assert_eq!(calls(), ["add kind 52 tag 10", "destroy command true", "add kind 52 tag 11"]);
        rt.pump();
        let k = kinds(dir.path());
        assert!(k.contains(&"error".to_string()) && k.iter().filter(|x| *x == "holding").count() == 2, "{k:?}");
        let reasons: Vec<_> = events(dir.path()).into_iter().filter(|(k, _)| k == "released").collect();
        assert!(reasons[0].1.contains("silence"), "{reasons:?}");
    }

    #[test]
    fn cancel_destroys_the_command_without_queueing_it() {
        let dir = TempDir::new("rt-cancel");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut a = Call::new(52, 10);
        unsafe { rt.on_add(a.args(), at(SITE_A)) };
        ctl(dir.path(), "2\tcancel\t1\t{[\"id\"]=1}\n");
        let mut nop = Call::new(27, 50);
        unsafe { rt.on_add(nop.args(), at(0x999)) };
        assert_eq!(calls(), ["destroy command true", "add kind 27 tag 50"], "never added");
        rt.pump();
        assert!(kinds(dir.path()).contains(&"dropped".to_string()));
    }

    #[test]
    fn the_control_file_disappearing_releases_everything() {
        let dir = TempDir::new("rt-gone");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let mut a = Call::new(52, 10);
        unsafe { rt.on_add(a.args(), at(SITE_A)) };
        std::fs::remove_file(dir.path().join(CTL_FILE)).unwrap();
        let mut nop = Call::new(27, 50);
        unsafe { rt.on_add(nop.args(), at(0x999)) };
        assert_eq!(calls(), ["add kind 52 tag 10", "destroy command true", "add kind 27 tag 50"]);
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
    }

    #[test]
    fn a_busy_state_never_makes_the_hook_wait() {
        let dir = TempDir::new("rt-busy");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), HOLD_ON);
        rt.pump();
        let guard = rt.machine.lock().unwrap(); // the service thread, say, in the middle of something
        let mut c = Call::new(52, 1);
        let started = Instant::now();
        unsafe { rt.on_add(c.args(), at(SITE_A)) };
        assert!(started.elapsed().as_millis() < 500, "no waiting");
        drop(guard);
        assert_eq!(calls(), ["add kind 52 tag 1"], "it simply passed through, the command is not lost");
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
        ctl(dir.path(), "2\trelease\t1\t{[\"id\"]=1}\n3\trelease\t1\t{[\"id\"]=2}\n");
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
            "2	release	1	{[\"id\"]=1}
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
            "2	release	1	{[\"id\"]=1}
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
        ctl(dir.path(), "1\tping\t1\t{[\"n\"]=12}\n");
        rt.pump();
        assert_eq!(events(dir.path())[1], ("pong".to_string(), "{[\"n\"]=12}".to_string()));
    }

    #[test]
    fn bad_control_lines_are_reported_not_fatal() {
        let dir = TempDir::new("rt-bad");
        let rt = runtime(Some(dir.path()));
        ctl(dir.path(), "1\texplode\t1\t{}\n2\trelease\t1\t{}\ngarbage\n3\tping\t1\t{[\"n\"]=1}\n");
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
        ctl(dir.path(), "2\trelease\t1\t{[\"at\"]=0,[\"id\"]=1}\n");
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
        // the lock the planning died holding is poisoned, not stuck: later calls still work
        rt.clock = None;
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

    const PATCH_NEXT: &str = "1	patch_next	1	{[\"adds\"]=2,[\"frozen\"]={[1]=1}}
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
                "{PATCH_NEXT}2	hold_on	1	{{[\"kinds\"]={{[1]=\"build_proposal\"}}}}
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

    const INJECT_NEXT: &str = "1	inject_next	1	{[\"key\"]=5}
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
        ctl(dir.path(), "1\tinject_next\t1\t{[\"held\"]=3,[\"key\"]=5}\n");
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
        assert!(lock(&rt.defer_link).is_none());
        // 2. a side file whose text names no place: no command can be identified by it either
        std::fs::write(dir.path().join(crate::inject::inject_file_name(6)), "toAdd.#\tn:0\n").unwrap();
        ctl(dir.path(), "2\tinject_next\t1\t{[\"key\"]=6}\n");
        rt.pump(); // the module reads it (it replaces key 5, which is refused without a command: no_command)
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
        ctl(dir.path(), "1\tinject_next\t1\t{[\"held\"]=3,[\"key\"]=5}\n");
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
        assert!(lock(&rt.defer_link).is_none(), "the held command never takes the result of the emptied command");
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
        // an order that no script command ever uses: the reason says which applies went by (it replaces key 5, which is refused)
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
            ev.iter().any(|(k, w)| k == "inject_refused" && w.contains("key 5") && w.contains("replaced")),
            "{ev:?}"
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
        ctl(dir.path(), "2\trelease\t1\t{[\"at\"]=0,[\"id\"]=1}\n3\tcancel\t1\t{[\"id\"]=2}\n");
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
