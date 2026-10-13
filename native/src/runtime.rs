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

/// The unhooked `apply` function of the simulation: (simulation, command) -> whatever it returns. A C++ function,
/// so declared as one that may unwind.
pub type ApplyFn = unsafe extern "system-unwind" fn(sim: *mut u8, cmd: *mut u8) -> u64;

/// How often the audit event is written when the counts changed.
pub const AUDIT_INTERVAL_MS: u64 = 30_000;

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

    /// The rebuild self-check of the payload at `payload`: report into the rebuild file.
    fn rebuild_check(&self, event: &str, payload: usize, hold_id: Option<u64>) {
        let Some(sink) = self.rebuild.get() else { return };
        if !sink.has_dir() {
            return;
        }
        let Some(seq) = sink.begin() else { return };
        let report = (|| -> Result<String, String> {
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
            let text = portable::encode(&LiveMem, env, sink.settings, self.build.name, &head)?.text;
            let original = dump::Mem::read(&LiveMem, payload, dump::PAYLOAD_SIZE).ok_or("payload unreadable")?;
            let r = rebuild::roundtrip(&text, &original, self.base, sink.settings, &alloc, &LiveMem, &|b| {
                Some(b.as_ptr() as usize)
            });
            let freed = alloc.free_all();
            r.map(|r| format!("{}FREED {freed} blocks\n", r.text))
        })();
        let key = hold_id.map_or("none".to_string(), |k| k.to_string());
        let text = match report {
            Ok(t) => {
                format!("=== rebuild check #{seq} event={event} key={key} ===\n{t}=== end rebuild check #{seq} ===\n")
            }
            Err(why) => format!("=== rebuild check #{seq} event={event} key={key} REFUSED: {why} ===\n"),
        };
        if let Err(e) = sink.write(&text) {
            self.say(&format!("rebuild: cannot write {}: {e}", rebuild::REBUILD_FILE));
        }
    }

    /// The portable text of the payload at `payload` (or why it is refused) into the portable file.
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
    }

    // ---- the link

    /// Opens the control link in a session folder and tells the mod what is active (`ready`). Everything the
    /// control file holds at this moment is history from an earlier run and is not acted on.
    pub fn open_link(&self, dir: &Path) -> std::io::Result<()> {
        let reader = CtlReader::starting_at_end(dir.join(CTL_FILE))?;
        let mut events = EventWriter::open(dir)?;
        let (hooks, poolpatch) = lock(&self.installed).clone();
        let ready = Event::Ready {
            build: self.build.name.to_string(),
            version: self.version.to_string(),
            hooks: hooks.iter().map(|h| h.to_string()).collect(),
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
        *lock(&self.reader) = Some(reader);
        *lock(&self.writer) = Some(Writer { events, failed_logged: false });
        if let Some(d) = self.dump.get() {
            d.set_dir(dir);
        }
        if let Some(p) = self.portable.get() {
            p.set_dir(dir);
        }
        if let Some(p) = self.rebuild.get() {
            p.set_dir(dir);
        }
        Ok(())
    }

    /// Reads whatever the mod has added to the control file and applies it to the machine.
    fn ingest(&self, machine: &mut HoldMachine<Box<Held>>, now: u64) {
        let Some(mut guard) = try_lock_briefly(&self.reader) else { return };
        let Some(reader) = guard.as_mut() else { return };
        if !reader.has_news() {
            return;
        }
        let poll = reader.poll();
        drop(guard);
        for line in &poll.lines {
            match parse_ctl_line(line) {
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

    // ---- the service thread

    /// One round of the service thread: ingest, let time pass, write the events. Returns how many were written.
    pub fn pump(&self) -> usize {
        let now = self.now_ms();
        let mut events = {
            let mut machine = lock(&self.machine);
            self.ingest(&mut machine, now);
            machine.tick(now);
            self.sync_flags(&machine);
            machine.drain_events()
        };
        while let Some((id, ok)) = self.watch.next_result() {
            events.push(Event::Applied { id, ok });
        }
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
    /// Two rules shape this function, both learned from a crash of the real game (`docs/NATIVE_LINK.md`, section 10):
    ///
    /// - The build must unwind (`panic = "unwind"`, root `Cargo.toml`) and every function between this one and the
    ///   engine's `Add` ([`engine::release`]) is `system-unwind`, with `Add` called from its body, not from a closure.
    ///   A C++ exception of the game then passes through our frames as through any others. Under `panic = "abort"`
    ///   every Rust frame, `system-unwind` or not, is a terminate pad: the exception ends the process with
    ///   `panic in a function that cannot unwind`.
    /// - Our own bookkeeping runs inside [`crate::log::guarded`] and never around a call into the game. A bug in it
    ///   (under `unwind`) costs the step, not the game: the call goes on to the original.
    #[allow(improper_ctypes_definitions)] // only ever called from Rust, with Rust types; "system-unwind" is for the exception
    pub unsafe extern "system-unwind" fn on_add(&self, args: AddArgs, caller: Option<usize>) -> *mut u8 {
        let add = self.add_fn();
        // filled in as the planning goes, so that a panic half-way still leaves the part that was done (a command that
        // was taken out of the engine's hands must be given back, whatever happens)
        let mut plan = AddPlan { due: Vec::new(), spill: None, held_here: false, now: 0, dump: None };
        crate::log::guarded("add hook", || self.plan_add(&args, caller, add.is_some(), &mut plan));
        if let Some(job) = plan.dump.take() {
            // dev-only and read-only: the held command's payload, decoded (the command is already out of the engine's hands)
            crate::log::guarded("add hook (dump)", || {
                self.dump_payload("held", job.payload, Some(job.id), Some(job.site), Scope::Whole, Budget::Held, None)
            });
            crate::log::guarded("add hook (portable)", || {
                self.portable_payload("held", job.payload, Some(job.id), Some(job.site));
            });
            crate::log::guarded("add hook (rebuild)", || {
                self.rebuild_check("held", job.payload, Some(job.id));
            });
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
            }
        }
        if let Some(back) = plan.spill {
            unsafe { engine::release(&engine, back, add) };
            return args.out;
        }
        if plan.held_here {
            return args.out;
        }
        // SAFETY: the arguments of the live call that is being hooked.
        unsafe { add(args.list, args.out, args.cmd, args.done, args.progress) }
    }

    /// The part of [`Runtime::on_add`] that decides: counts the call and, when holding is on, asks the machine what
    /// is due and whether this command is held (taking it out of the engine's hands if so). Touches the engine only
    /// through its move constructor. The result is written into `plan` step by step.
    fn plan_add(&self, args: &AddArgs, caller: Option<usize>, have_add: bool, plan: &mut AddPlan) {
        let site = caller.and_then(|c| c.checked_sub(self.base)).and_then(|rva| u32::try_from(rva).ok());
        let tool = site.and_then(|rva| self.build.ui_site_index(rva));
        self.counters.added(match (caller, tool) {
            (None, _) => AddSource::Unknown,
            (Some(_), Some(i)) => AddSource::Tool(i),
            (Some(_), None) => AddSource::Other,
        });
        let Some(engine) = self.engine else { return };
        if !have_add || !self.live.load(Relaxed) {
            return;
        }
        let now = self.now_ms();
        plan.now = now;
        let Some(mut machine) = try_lock_briefly(&self.machine) else { return };
        // the mod writes `release` and then sends a command to get here: look at the file now, do not wait for the pump
        self.ingest(&mut machine, now);
        plan.due = machine.take_due(self.game_time(), now);

        if let (Some(rva), Some(_)) = (site, tool) {
            // SAFETY: the kind is read only from readable memory.
            if let Some(kind) = unsafe { engine::command_kind(args.cmd, engine.kind_offset) } {
                if machine.admit(kind, now) == Admit::Hold {
                    // SAFETY: the arguments of the live call, as the caller guarantees.
                    if let Some(held) = unsafe { engine::capture(&engine, args) } {
                        let done = held.done_ok();
                        let payload = held.payload();
                        // how the tool built the command (Context members, ignoreErrors, playerInitiated): a preview carries none of it
                        let flags = if self.build.kind_by_name("build_proposal") == Some(kind) {
                            // SAFETY: only readable memory is read.
                            unsafe { engine::build_flags(payload) }
                        } else {
                            None
                        };
                        match machine.hold_with(kind, rva, done, flags, held, now) {
                            Ok(id) => {
                                plan.held_here = true;
                                if (self.dump.get().is_some()
                                    || self.portable.get().is_some()
                                    || self.rebuild.get().is_some())
                                    && self.build.kind_by_name("build_proposal") == Some(kind)
                                {
                                    plan.dump = Some(DumpJob { id, site: rva, payload });
                                }
                            }
                            Err(back) => plan.spill = Some(back), // refused after capture: it goes straight on
                        }
                    }
                }
            }
        }
        self.sync_flags(&machine);
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
            crate::log::guarded("apply hook (rebuild)", || {
                // SAFETY: only readable memory is read.
                let payload = unsafe { engine::payload_of(cmd) }?;
                let kind_offset = self.build.kind_offset.unwrap_or(0) as usize;
                let kind = unsafe { engine::command_kind(cmd, kind_offset) }?;
                (self.build.kind_by_name("build_proposal") == Some(kind)).then_some(())?;
                (claimed.is_none() && sink.settings.scripts).then_some(())?;
                (portable::proposal_is_empty(&LiveMem, payload) == Some(false)).then_some(())?;
                self.rebuild_check("apply", payload, None);
                Some(())
            });
        }
        let result = unsafe { original(sim, cmd) };
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

    unsafe extern "system" fn e_move(dst: *mut u8, src: *mut u8) -> *mut u8 {
        unsafe {
            core::ptr::copy_nonoverlapping(src, dst, COMMAND_SIZE);
            core::ptr::write_bytes(src, 0, engine::RESULT_OFFSET);
        }
        dst
    }
    unsafe extern "system" fn e_destroy(cmd: *mut u8) {
        note(format!("destroy command {}", unsafe { *(cmd as *const usize) } != 0));
        unsafe { core::ptr::write_bytes(cmd, 0, COMMAND_SIZE) };
    }
    unsafe extern "system" fn e_handle(_h: *mut u8) {}
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

    fn events(dir: &Path) -> Vec<(String, String)> {
        let mut r = CtlReader::new(dir.join(crate::link::EVENTS_FILE));
        r.poll().lines.into_iter().map(|l| (l.kind, l.payload)).collect()
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
}
