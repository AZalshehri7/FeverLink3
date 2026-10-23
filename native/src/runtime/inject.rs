//! The runtime's side of `FEVERLINK3_NATIVE_INJECT` (`crate::inject` has the checks and the engine calls).
//! A child module of `runtime` so that it can use the runtime's private fields.

use super::Runtime;
use crate::dump::{Env, LiveMem, Mem};
use crate::engine::{self, AddArgs};
use crate::inject as inj;
use crate::link::Event;
use crate::native_copy as nc;
use crate::portable;
use crate::rebuild::{Alloc, EngineAlloc};

use super::{wall_ms, StampTiming};

/// What an unresolved pointer of a text points at, for the log: the word's two 32-bit halves (a small low half under a high half that
/// is a heap prefix is a 4-byte value beside residue, not a pointer), the first 32 bytes there, and what its first word is (an offset
/// into the game's image is likely a vtable: an engine object of a polymorphic class).
fn describe_target(addr: usize, base: usize) -> String {
    let (lo, hi) = (addr as u64 & 0xffff_ffff, addr as u64 >> 32);
    let Some(b) = LiveMem.read(addr, 32) else {
        return format!("{addr:#x} (low32 {lo:#x}, high32 {hi:#x}) unreadable");
    };
    let w0 = crate::dump::u64_at(&b, 0) as usize;
    let first = if w0 >= base && w0 - base < crate::dump::IMAGE_BOUND {
        format!("first word I+{:x} (an image pointer: a vtable?)", w0 - base)
    } else if crate::dump::pointer_like(w0 as u64) && LiveMem.read(w0, 1).is_some() {
        "first word a readable address".to_string()
    } else {
        "first word a literal".to_string()
    };
    format!("{addr:#x} (low32 {lo:#x}, high32 {hi:#x}) bytes {}; {first}", crate::plan::hex(&b))
}

impl Runtime {
    /// Switches the native injection on (`FEVERLINK3_NATIVE_INJECT=1`). `settings` are the portable text settings the originator's
    /// copy of the text is written with and the receiver's proof is made with.
    pub fn enable_native_inject(&self, settings: portable::Settings) {
        // the construction of the text is the receiver's own (its conversion), so its description's heap graph (tens of thousands of
        // blocks) is neither written nor needed
        let _ = self.inject_settings.set(portable::Settings { graph_skip: &["toAdd"], typed: true, ..settings });
        self.inject.enable();
        match crate::refund::RefundFns::at(self.base, &self.build.engine) {
            Ok(_) => self.say(&format!("native inject: refund (native): the engine's insert of the Context's refundableEntities at {:#x} is verified: an order with refund entries puts them into its command's Context", self.build.engine.refund_insert.unwrap_or(0))),
            Err(e) => self.say(&format!("native inject: refund (native): {e}: an order with refund entries is refused")),
        }
        self.say("native inject: FEVERLINK3_NATIVE_INJECT is on: held commands are also written as native_text_<id>.txt (full portable text), and each inject_next order (queued in the mod's order) makes its own script build_proposal (the one that carries the conversion of the mod's own send; without one seen, the text's place or constructions) take the engine Proposal built from native_inject_<key>.txt (all checks first; a refusal empties the command: nothing of the build is applied on this game)");
    }

    pub(super) fn inject_dir(&self, dir: &std::path::Path) {
        *super::lock(&self.inject_dir_path) = Some(dir.to_path_buf());
    }

    /// The switch is on and the prerequisites hold (this layout's build, the engine's Proposal functions, the allocator's prologues):
    /// the `ready` event then lists `inject_next`.
    pub(super) fn inject_ready(&self) -> bool {
        self.inject.enabled()
            && self.build.layout_name() == portable::LAYOUT_BUILD
            && nc::ProposalFns::at(self.base, &self.build.engine).is_ok()
            && EngineAlloc::at(self.base, &self.build.engine).is_ok()
    }

    /// The originator's side: the held command's full portable text, for the mod to ship (`native_text_<id>.txt`). The
    /// residue mask calls the engine's copy constructor, so this runs outside `log::guarded` and guards the rest itself:
    /// never call it inside a guard.
    pub(super) fn inject_publish_text(&self, payload: usize, id: u64) {
        if !self.inject.enabled() {
            return;
        }
        let Some(settings) = self.inject_settings.get().copied() else { return };
        let t0 = std::time::Instant::now();
        let wall0 = wall_ms();
        // R2: which unnamed bytes of the held command are values and which residue, by a differential engine copy (both copies destroyed)
        crate::log::calls_engine("native inject: residue mask (the engine's copy constructor)");
        let mask = nc::ProposalFns::at(self.base, &self.build.engine).and_then(|fns| {
            nc::residue_mask(&fns, &LiveMem, self.base, settings, self.build.layout_name(), payload, &mut |l| {
                self.say(l)
            })
        });
        let Some(mut job) =
            crate::log::guarded("native inject: text", || self.inject_encode_text(payload, id, settings, t0, mask))
        else {
            return self.refuse_text_after_panic(id, "native inject: text");
        };
        // Owner decision 2026-10-18 (#6): a text that passes the text-only checks is also proved by the engine on this game, before
        // anything leaves it: the same rebuild every receiver makes at the stamp, destroyed again (the engine calls are not guarded)
        if let (Ok(p), None) = (&job.encoded, &job.refusal) {
            let t_proof = std::time::Instant::now();
            let proof = self.inject_own_proof(payload, &p.text, settings);
            job.proof_ms = t_proof.elapsed().as_secs_f64() * 1000.0;
            match proof {
                Ok(stats) => job.proved = Some(stats),
                Err(why) => job.refusal = Some(why),
            }
        }
        if crate::log::guarded("native inject: text (ship)", || self.inject_write_text(id, t0, wall0, job)).is_none() {
            self.refuse_text_after_panic(id, "native inject: text (ship)");
        }
    }

    /// Fail closed when our own code panicked on a held command's text: a refusal file (the mod fails the held command and ships
    /// nothing; it reads the refusal before the text) instead of no file at all, which the mod took as "no native text, build it from
    /// the record" (fail open).
    fn refuse_text_after_panic(&self, id: u64, site: &str) {
        let why = format!("our own code panicked on this command's text in {site} (the PANIC line in native.log)");
        let written = self.refused_text.write(id, &format!("{why}\n"));
        crate::log::guarded("native inject: text refusal", || {
            self.say(&format!(
                "native inject: hold {id}: REFUSED BEFORE SHIPPING: {why}; native_refused_{id}.txt {}",
                if written.is_ok() {
                    "written: the mod fails the held command and ships nothing"
                } else {
                    "NOT written"
                }
            ))
        });
        if written.is_err() {
            self.fault(
                "native inject: text",
                crate::failsafe::Action::Repair,
                format!("hold {id}: {why}; the refusal file could not be written"),
            );
        }
    }

    /// The originator's own engine proof of its text (`inject::own_proof`) on the held command at `payload`: the checks and the dry
    /// pass guarded, the engine part not. `Err` (the refusal) whenever the proof cannot be made: an unverified function or allocator is
    /// a refusal too (fail closed: the receivers could not inject it either). Never call it inside a guard.
    pub(super) fn inject_own_proof(
        &self,
        payload: usize,
        text: &str,
        settings: portable::Settings,
    ) -> Result<(u64, u64, u64), String> {
        crate::log::calls_engine("native inject: the originator's own proof");
        let ready = crate::log::guarded("native inject: own proof (checks)", || -> Result<_, String> {
            let donor = inj::own_checks(&LiveMem, payload, text)?;
            let fns = self.proposal_fns().map_err(|why| format!("{}{why}", inj::OWN_PROOF))?;
            let alloc = EngineAlloc::at(self.base, &self.build.engine)
                .map_err(|why| format!("{}the game's allocator was not verified: {why}", inj::OWN_PROOF))?;
            Ok((donor, fns, alloc))
        });
        let (donor, fns, alloc) = match ready {
            Some(r) => r?,
            None => return Err(format!("{}our own check panicked (the PANIC line in native.log)", inj::OWN_PROOF)),
        };
        inj::dry_own_proof(&LiveMem, self.base, settings, self.build.layout_name(), text, donor)
            .map_err(|why| format!("{}{why}", inj::OWN_PROOF))?;
        let live = LiveMem;
        let ctx = nc::Ctx {
            fns,
            alloc: &alloc,
            mem: &live,
            base: self.base,
            set: settings,
            build_name: self.build.layout_name(),
        };
        let mut say = |l: &str| self.say(l);
        inj::own_proof(&ctx, &super::cmd_patch::LivePoke, text, donor, &mut say)
    }

    /// The text of [`Runtime::inject_publish_text`] with the mask and its text-only checks (no engine call).
    fn inject_encode_text(
        &self,
        payload: usize,
        id: u64,
        settings: portable::Settings,
        t0: std::time::Instant,
        mask: Result<nc::MaskInfo, String>,
    ) -> TextJob {
        let live = LiveMem;
        let head = portable::Header {
            seq: 0,
            event: "inject",
            hold_id: Some(id),
            site_rva: None,
            tool: None,
            t_ms: self.now_ms(),
            wall_ms: 0,
            payload,
        };
        let mask = mask
            .map_err(|e| {
                self.say(&format!("native inject: hold {id}: no residue mask, unnamed words are not carried: {e}"))
            })
            .ok();
        if let Some(m) = &mask {
            self.say(&format!(
                "native inject: hold {id}: residue mask: {} carried / {} residue bytes in {} unnamed regions ({} ms); toAdd[0] carried ranges: {}",
                m.carried,
                m.residue,
                m.regions,
                m.ms,
                m.carried_ranges("toAdd[0].")
            ));
        }
        let mask_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let opts = portable::EncodeOpts { collect: None, mask: mask.as_ref().map(|m| &m.mask) };
        let t_enc = std::time::Instant::now();
        let encoded = portable::encode_with(
            &live,
            Env { image_base: self.base },
            settings,
            self.build.layout_name(),
            &head,
            opts,
        );
        let encode_ms = t_enc.elapsed().as_secs_f64() * 1000.0;
        if let Ok(p) = &encoded {
            // every pointer the text could not follow: where it is, where it points (an address is for this log only) and what the
            // memory there looks like, so that one run shows everything that stays to be understood
            for (member, addr) in p.unresolved.iter().take(40) {
                self.say(&format!(
                    "native inject: hold {id}: unresolved pointer in {member} -> {}",
                    describe_target(*addr, self.base)
                ));
            }
            self.say(&format!(
                "native inject: hold {id}: structural dump read {} heap bytes of its bound {} (scaled from the proposal's own size)",
                p.stats.heap_read, p.stats.heap_budget
            ));
        }
        // The originator decides (open bug 7): the part of every receiver's proof that rests on the text alone is run here, on the same
        // text with the same rules. A text that every game would refuse at the stamp is not shipped: the reason goes to
        // native_refused_<id>.txt instead of native_text_<id>.txt, and the mod fails the held command (nothing is built on any game).
        let refusal: Option<String> = match &encoded {
            Ok(p) => inj::text_refusal(&p.text, self.base),
            Err(why) => Some(format!("the layout table refuses this command: {why}")),
        };
        TextJob { encoded, refusal, mask_ms, encode_ms, proof_ms: 0.0, proved: None }
    }

    /// The rest of [`Runtime::inject_publish_text`] once the text is checked and proved: the file (or the refusal), the log lines and the
    /// timing record.
    fn inject_write_text(&self, id: u64, t0: std::time::Instant, wall0: u64, job: TextJob) {
        let TextJob { encoded, refusal, mask_ms, encode_ms, proof_ms, proved } = job;
        if let Some((nodes, segments, constructions)) = proved {
            self.say(&format!(
                "native inject: hold {id}: own proof PASSED in {proof_ms:.1} ms: the engine Proposal rebuilt from this text the way every receiver rebuilds it ({nodes} nodes, {segments} segments, {constructions} construction(s)) equals the text in every named member; destroyed, nothing of it applied"
            ));
        }
        let t_write = std::time::Instant::now();
        let written: Option<std::io::Result<()>> = match (&encoded, &refusal) {
            (Ok(p), None) => Some(self.text.write(id, &p.text)),
            (_, Some(why)) => {
                let (short, report) = why.split_once(inj::REPORT_MARK).unwrap_or((why.as_str(), ""));
                let _ = self.refused_text.write(id, &format!("{short}\n{report}"));
                let whose = if short.starts_with(inj::OWN_PROOF) {
                    format!("the engine's own rebuild of this text on this game was refused ({proof_ms:.1} ms), as every receiver's would be")
                } else {
                    "every game would refuse this text".to_string()
                };
                self.say(&format!(
                    "native inject: hold {id}: REFUSED BEFORE SHIPPING, {whose}: {short}; native_refused_{id}.txt written, no native_text_{id}.txt: the mod fails the held command and ships nothing"
                ));
                None
            }
            (Err(_), None) => None,
        };
        let write_ms = t_write.elapsed().as_secs_f64() * 1000.0;
        let bytes = encoded.as_ref().map_or(0, |p| p.text.len());
        self.inject.report(Event::Timing {
            what: "capture",
            id,
            detail: format!(
                "held={id} start={wall0} end={} ms={:.1} bytes={bytes} mask_ms={mask_ms:.1} encode_ms={encode_ms:.1} proof_ms={proof_ms:.1} proof={} write_ms={write_ms:.1}",
                wall_ms(),
                t0.elapsed().as_secs_f64() * 1000.0,
                match (&proved, &refusal) {
                    (Some(_), _) => "passed",
                    (None, Some(r)) if r.starts_with(inj::OWN_PROOF) => "refused",
                    _ => "not_run",
                }
            ),
        });
        match encoded.map(|p| (written, p)) {
            Ok((Some(w), p)) => match w {
                Ok(()) if !p.unresolved.is_empty() => {
                    self.say(&format!(
                        "native inject: hold {id}: portable text written in {} ms, {} bytes, {} lines (native_text_{id}.txt), {} pointer(s) not followed; {}",
                        t0.elapsed().as_millis(),
                        p.text.len(),
                        p.text.lines().count(),
                        p.unresolved.len(),
                        p.text.lines().last().unwrap_or("")
                    ))
                }
                Ok(()) => self.say(&format!(
                    "native inject: hold {id}: portable text written in {} ms, {} bytes, {} lines (native_text_{id}.txt); {}",
                    t0.elapsed().as_millis(),
                    p.text.len(),
                    p.text.lines().count(),
                    p.text.lines().last().unwrap_or("")
                )),
                Err(e) => self.say(&format!("native inject: hold {id}: cannot write the text: {e}")),
            },
            Ok((None, _)) => {}
            Err(why) => self.say(&format!("native inject: hold {id}: the layout table refuses this command: {why}")),
        }
    }

    /// The simulation's application of a command (the `apply_command` hook, on the simulation thread): a script's `sendCommand` from the
    /// simulation state does not pass the command list's `Add` hook (b31 to b41: the engine executed the command, our `Add` hook never
    /// saw a non-tool kind 52), so the orders are also carried out here, on a `build_proposal` the module did not hold, BEFORE the original
    /// applies it. Dev-only switches, and like the dev dumps that already run here they read a file and call the engine on this thread;
    /// everything is checked first and a refusal empties the command. A construction tool's command that was not held can be mistaken for
    /// the script's here; the order's own checks (`inject::same_build`: the constructions, or the place) guard against that.
    ///
    /// This part only notes the apply (guarded by the caller): true when the command is a `build_proposal` the module did not hold, for
    /// which the caller then runs [`Runtime::apply_orders`] outside the guard.
    pub(super) fn note_apply_orders(&self, kind: Option<u8>, held: bool) -> bool {
        let bp = kind.is_some() && self.build.kind_by_name("build_proposal") == kind;
        if !bp {
            return false;
        }
        let now = self.now_ms();
        self.inject.note_apply(held, now);
        self.patch.note_apply(held, now);
        !held
    }

    /// The orders for a script `build_proposal` the simulation is about to apply (see [`Runtime::note_apply_orders`]): what the command
    /// list's `Add` already did for it ([`Runtime::take_added`]), else the waiting order it is matched to. The result goes to
    /// [`Runtime::after_ordered_apply`] once the simulation has applied the command. Calls the engine when an order is carried out:
    /// never call it inside `log::guarded`.
    pub(super) fn apply_orders(&self, cmd: *mut u8) -> Option<Used> {
        crate::log::calls_engine("apply orders");
        let args = AddArgs {
            list: core::ptr::null_mut(),
            out: core::ptr::null_mut(),
            cmd,
            done: core::ptr::null_mut(),
            progress: core::ptr::null_mut(),
        };
        if self.inject.enabled() {
            // a command the interface half added took its order at the Add: it never takes a second one here
            if let Some(used) = crate::log::guarded("native inject: added", || self.take_added(cmd)).flatten() {
                return Some(used);
            }
            return self.inject_add(&args, None);
        }
        if self.patch.enabled() {
            self.patch_add(&args, None);
        }
        None
    }

    /// `scripting::Convert` produced the `optional<Proposal>` at `out` (engaged when the byte after the Proposal is not 0) on the calling
    /// thread. While orders wait, what it builds is kept with the thread ([`inj::State::note_conversion`]): the mod writes its order, then
    /// its own send converts its command on the thread of its script, then the engine queues or applies that command, so the command that
    /// carries this conversion on this thread is the mod's (positive identity, owner decision B 2026-10-21). Reads only; contained.
    pub fn inject_on_convert(&self, out: usize) {
        if !self.inject.enabled() {
            return;
        }
        crate::log::guarded("native inject: convert", || {
            let now = self.now_ms();
            if !self.inject.pending() {
                // the mod writes its order and converts its command in the same breath: look at the file now
                self.ingest_patch_order(now);
                if !self.inject.pending() {
                    return;
                }
            }
            if LiveMem.read(out + nc::PROPOSAL_SIZE, 1).is_none_or(|b| b[0] == 0) {
                return; // the conversion produced no proposal (it failed): the factory throws, nothing is sent
            }
            let thread = super::current_thread_id();
            match inj::proposal_identity(&LiveMem, out) {
                Ok(id) if !id.is_empty() => {
                    self.say(&format!(
                        "native inject: conversion on thread {thread} while {} order(s) wait: {} point(s), {} removed, {} construction(s): the command that carries it is the mod's",
                        self.inject.waiting_count(),
                        id.place.points.len(),
                        id.place.removed.len(),
                        id.construction_count()
                    ));
                    self.inject.note_conversion(thread, id);
                }
                Ok(_) => self.say(&format!(
                    "native inject: conversion on thread {thread} builds nothing: it identifies no command"
                )),
                Err(why) => self.say(&format!("native inject: conversion on thread {thread} cannot be read: {why}")),
            }
        });
    }

    /// The command list's `Add` of a command that was not held (the interface half's script commands): an order matched to it is
    /// carried out now, and what it needs after the simulation's apply is kept with the command's identity (see [`Added`]).
    pub(super) fn inject_on_add(&self, args: &AddArgs, caller: Option<usize>) {
        crate::log::calls_engine("native inject: an order may be carried out at Add");
        let Some(used) = self.inject_add(args, caller) else { return };
        crate::log::guarded("native inject: keep for the apply", || {
            // SAFETY: only readable memory is read.
            let Some(payload) = (unsafe { engine::payload_of(args.cmd) }) else { return };
            let identity = inj::identity(&LiveMem, payload);
            let mut added = super::lock(&self.added);
            self.prune_added(&mut added);
            match identity {
                Ok(identity) => {
                    added.insert(used.seq, Added { payload, identity, used });
                }
                Err(why) => {
                    drop(added);
                    self.say(&format!(
                        "native inject: key {}: the command cannot be recognised at its apply ({why}): no timing, and held command {} fails",
                        used.timing.key,
                        used.link.unwrap_or(0)
                    ));
                    if let Some(held) = used.link {
                        self.inject.fail_held(held);
                    }
                }
            }
        });
    }

    /// The command `cmd` the simulation is about to apply is one the command list's `Add` carried an order out on: the same payload AND
    /// the same identity as after the order ([`inj::identity`]; a payload address the engine freed and gave to another command is not
    /// the same command). Takes its record.
    fn take_added(&self, cmd: *mut u8) -> Option<Used> {
        let mut added = super::lock(&self.added);
        if added.is_empty() {
            return None;
        }
        self.prune_added(&mut added);
        // SAFETY: only readable memory is read.
        let payload = unsafe { engine::payload_of(cmd) }?;
        let seq = {
            let mut same = added.iter().filter(|(_, a)| a.payload == payload);
            let first = same.next()?;
            let identity = inj::identity(&LiveMem, payload).ok()?;
            std::iter::once(first).chain(same).find(|(_, a)| a.identity == identity).map(|(s, _)| *s)?
        };
        added.remove(&seq).map(|a| a.used)
    }

    /// Drops the records no apply can need any more, by no wall clock: a pause can last minutes (the host waits for every game's stop
    /// report and a round trip) and keeps a paused build deferred all that time, so a record waits as long as its deferred held command
    /// is kept. The hold machine owns that lifetime (its age stands still while the mod is silent, and its own end of a held command
    /// fires the tool's callback): a record whose held command is failed, cancelled or gone is dropped (nothing to complete, nothing
    /// failed here).
    /// A record without a held command (timing only) is dropped when more than [`MAX_ADDED`] wait, the oldest first. Before (until
    /// 2026-10-15), a record older than 60 s of wall time failed its held command: a long pause failed a paused build's tool.
    fn prune_added(&self, added: &mut std::collections::BTreeMap<u64, Added>) {
        let gone: Vec<u64> = {
            let machine = super::lock(&self.machine);
            added
                .iter()
                .filter(|(_, a)| a.used.link.is_some_and(|held| !machine.is_kept(held)))
                .map(|(s, _)| *s)
                .collect()
        };
        for seq in gone {
            let Some(a) = added.remove(&seq) else { continue };
            self.say(&format!(
                "native inject: key {}: held command {} is no longer kept (failed, cancelled or gone): the record of its command (added, not yet applied) is dropped",
                a.used.timing.key,
                a.used.link.unwrap_or(0)
            ));
        }
        while added.values().filter(|a| a.used.link.is_none()).count() > MAX_ADDED {
            let Some(seq) = added.iter().find(|(_, a)| a.used.link.is_none()).map(|(s, _)| *s) else { break };
            if let Some(a) = added.remove(&seq) {
                self.say(&format!(
                    "native inject: key {}: more than {MAX_ADDED} added commands wait for their apply: the oldest record (timing only) is dropped",
                    a.used.timing.key
                ));
            }
        }
    }

    /// Matches the command to a waiting order and carries it out: if this is a script `build_proposal` and one of the waiting orders
    /// identifies it, that order is used (once). Runs before the original `Add` or apply, on the thread that queues or applies it.
    ///
    /// Carrying the order out calls the engine (its Proposal functions and `operator new`, [`Runtime::inject_engine`]), so this runs
    /// outside `log::guarded`: the decision before ([`Runtime::inject_take`]) and the report after ([`Runtime::inject_finish`]) are
    /// guarded, the engine part is not. Never call it inside a guard.
    pub(super) fn inject_add(&self, args: &AddArgs, caller: Option<usize>) -> Option<Used> {
        crate::log::calls_engine("native inject: an order may be carried out");
        let job = match crate::log::guarded("native inject: order", || self.inject_take(args, caller)) {
            Some(job) => job,
            None => {
                self.fault(
                    "native inject: order",
                    crate::failsafe::Action::Repair,
                    "a panic while matching a command to its inject order: the command passed untouched (it may be the mod's placeholder)",
                );
                None
            }
        };
        let InjectJob { order, dest, t0, rb0, prepared } = job?;
        let forced = order.forced.clone();
        let run = match prepared {
            // the same parsing and build first as a dry pass inside a guard: a panic of ours on the peer's text is a refusal
            Some(Ok(_)) if !forced.is_empty() => {
                Err(format!("forced refusal ({forced}): the test switch FEVERLINK3_DEBUG_REFUSE_INJECT"))
            }
            Some(Ok(p)) => {
                match inj::dry_build(&LiveMem, self.base, p.settings, self.build.layout_name(), &p.text, p.donor) {
                    Err(why) => Err(why),
                    Ok(()) => {
                        crate::log::calls_engine("native inject (the Proposal functions, operator new)");
                        self.inject_engine(order.key, dest, p)
                    }
                }
            }
            Some(Err(why)) => Err(why),
            None => Err(String::new()), // complete_next: the record's own command is this game's stamp apply; nothing to inject
        };
        // Fail closed: an order refused on a command that took it (the mod's, as far as the identity checks can tell) empties that command,
        // so this game applies nothing of the build rather than the placeholder (this game's own rebuild of the record, not the build).
        // The order was matched to this command by its identity before (inject_take), so every error here is a refusal.
        let refused = !order.link_only && run.is_err();
        let outcome = refused.then(|| self.inject_neutralise(order.key, dest));
        let key = order.key;
        let used = crate::log::guarded("native inject: result", || self.inject_finish(order, (t0, rb0), run, outcome));
        if used.is_none() {
            self.fault(
                "native inject: result",
                crate::failsafe::Action::Repair,
                format!("key {key}: a panic in the report of a carried-out order: the mod is not told its outcome"),
            );
        }
        used
    }

    /// The engine's Proposal functions; a test sets stand-ins (`TEST_FNS`).
    fn proposal_fns(&self) -> Result<nc::ProposalFns, String> {
        #[cfg(test)]
        if let Some(f) = TEST_FNS.with(|t| t.get()) {
            return Ok(f);
        }
        nc::ProposalFns::at(self.base, &self.build.engine)
    }

    /// Empties the command a refused order was used for (`inject::neutralise`): what became of it. Calls the engine: never inside a guard
    /// (the checks before are guarded).
    fn inject_neutralise(&self, key: u64, dest: usize) -> inj::Outcome {
        crate::log::calls_engine("native inject: the refused order's command is emptied");
        let ready = crate::log::guarded("native inject: neutralise check", || {
            let fns = self.proposal_fns()?;
            inj::is_proposal(&LiveMem, dest)?;
            Ok::<_, String>(fns)
        });
        match ready {
            Some(Ok(fns)) => {
                // SAFETY: `is_proposal` accepted this payload just now, on the thread that applies (or queues) the command; the functions
                // are the verified ones.
                unsafe { inj::neutralise(&fns, dest) };
                inj::Outcome::Nothing
            }
            Some(Err(why)) => {
                crate::log::guarded("native inject: neutralise report", || {
                    self.say(&format!("native inject: key {key}: the command could NOT be emptied: {why}"))
                });
                inj::Outcome::Untouched
            }
            None => inj::Outcome::Untouched,
        }
    }

    /// The decision of [`Runtime::inject_add`]: which waiting order this command is, and everything the injection needs that does not
    /// call the engine (the side file, the destination's checks). `None` when it is no waiting order's command.
    ///
    /// Positive identity first (owner decision B, 2026-10-21): an order whose send converted its command on this thread
    /// ([`Runtime::inject_on_convert`]) is used by the command that carries that conversion ([`inj::ConvMatch::Ours`]), whatever its
    /// text says; the text is then only a cross-check, and a cross-check that fails refuses the order and empties the command (nothing of
    /// the build is applied here), it never lets the command pass. A command on that thread that does not carry the conversion (a town's
    /// own build applied meanwhile: lab A-new-run 2026-10-18, 12 of 48 orders saw one first) is never that order's.
    /// Only without a conversion seen on this thread (the conversion hook not installed) does the content decide, as before:
    /// `inject_next` by its text ([`Runtime::inject_prepare`], `inject::same_build`), `complete_next` by its held command
    /// ([`Runtime::same_as_held`]); there, a command the order fails passes untouched and the order keeps waiting.
    fn inject_take(&self, args: &AddArgs, caller: Option<usize>) -> Option<InjectJob> {
        let site = caller.and_then(|c| c.checked_sub(self.base)).and_then(|rva| u32::try_from(rva).ok());
        if site.and_then(|rva| self.build.ui_site_index(rva)).is_some() {
            return None; // a construction tool's command is never the mod's
        }
        let kind_offset = self.build.kind_offset?;
        // SAFETY: only readable memory is read.
        let kind = unsafe { engine::command_kind(args.cmd, kind_offset as usize) }?;
        if self.build.kind_by_name("build_proposal") != Some(kind) {
            return None; // another kind of command: the orders keep waiting
        }
        let now = self.now_ms();
        // the mod writes an order and sends its command in the same breath, possibly several in one step: whatever the file holds now
        // is armed before the command is matched (even with orders waiting, this command's may be the newest line)
        self.ingest_patch_order(now);
        if !self.inject.pending() {
            return None;
        }
        // SAFETY: only readable memory is read.
        let dest = unsafe { engine::payload_of(args.cmd) }?; // no payload: no order can tell it is its command
        let thread = super::current_thread_id();
        let have = inj::identity(&LiveMem, dest).ok();
        for order in self.inject.waiting(now) {
            let (positive, how) = match self.inject.conversion_match(order.seq, thread, have.as_ref()) {
                inj::ConvMatch::Ours(how) => (true, how),
                inj::ConvMatch::Other(why) => {
                    self.say(&format!(
                        "native inject: key {} (held {}): a build_proposal on thread {thread} that does not carry the conversion of the mod's own send for this order: not its command, passed untouched{}: {why}",
                        order.key,
                        order.held,
                        have.as_ref().map_or(" (unreadable)".to_string(), |h| format!(
                            " ({} point(s), {} removed, {} construction(s))",
                            h.place.points.len(),
                            h.place.removed.len(),
                            h.construction_count()
                        ))
                    ));
                    continue;
                }
                inj::ConvMatch::Unseen => (false, String::new()),
            };
            let prepared = if order.link_only {
                match self.same_as_held(order.held, dest) {
                    Ok(()) => None,
                    Err(why) if positive => {
                        // the command of its own send all the same: this game's stamp apply of the build; the held command takes its result
                        self.say(&format!(
                            "native inject: complete_next held {}: cross-check: {why}; it carries the conversion of the mod's own send for this order: used",
                            order.held
                        ));
                        None
                    }
                    Err(why) => {
                        self.say(&format!(
                            "native inject: complete_next held {}: {why}; the order waits for its own command (no conversion of its send seen on thread {thread})",
                            order.held
                        ));
                        continue;
                    }
                }
            } else {
                match self.inject_prepare(order.key, order.refund, dest, positive) {
                    Err(why) if !positive && inj::keeps_waiting(&why) => {
                        self.say(&format!(
                            "native inject: key {}: {why}; the order waits for its own command (no conversion of its send seen on thread {thread})",
                            order.key
                        ));
                        continue;
                    }
                    p => Some(p),
                }
            };
            // expired between the two looks: it was refused, the command is not its any more
            let Some(order) = self.inject.take(order.seq) else { continue };
            self.say(&format!(
                "native inject: order {} (key {}, held {}) is this command's: {} on thread {thread} ({} order(s) still wait)",
                order.seq,
                order.key,
                order.held,
                if positive {
                    format!("it carries the conversion of the mod's own send (positive identity: {how})")
                } else {
                    "matched by its content (no conversion of its send seen)".to_string()
                },
                self.inject.waiting_count()
            ));
            return Some(InjectJob { order, dest, t0: std::time::Instant::now(), rb0: wall_ms(), prepared });
        }
        None
    }

    /// A `complete_next` names no text: the script's command is its build when it builds what this game's own held command `held` of
    /// the build builds ([`inj::same_command`]). Reads the held command under the machine's lock (it cannot be destroyed meanwhile).
    fn same_as_held(&self, held: u64, dest: usize) -> Result<(), String> {
        let machine = super::lock(&self.machine);
        let Some(h) = machine.handle_of(held) else {
            return Err(format!("{}held command {held} is not held here", inj::UNKNOWN));
        };
        inj::same_command(&LiveMem, h.payload(), dest)
    }

    /// The report of [`Runtime::inject_add`]: the log lines, the event, and what the apply of the command needs (its timing record,
    /// the deferred held command that takes its result).
    fn inject_finish(
        &self,
        order: inj::Order,
        (t0, rb0): (std::time::Instant, u64),
        run: Result<(u64, u64, u64), String>,
        outcome: Option<inj::Outcome>,
    ) -> Used {
        // a deferred held command of the same build takes this command's result once the simulation has applied it, when it was injected
        // (or `complete_next`); a refused order fails the held command instead, as the emptied command carries no build
        let mut link = (order.held != 0).then_some(order.held);
        let mut how = "injected";
        match run {
            Err(why) if order.link_only && why.is_empty() => {
                self.say(&format!(
                    "native inject: complete_next held {}: this script build_proposal is this game's stamp apply (record path)",
                    order.held
                ));
                how = "record (complete_next)";
            }
            Ok((nodes, segments, constructions)) => {
                self.say(&format!(
                    "native inject: key {}: INJECTED the engine Proposal built from the text ({nodes} nodes, {segments} segments, {constructions} construction(s)) in {} ms",
                    order.key,
                    t0.elapsed().as_millis()
                ));
                self.inject.report(Event::Injected { key: order.key, nodes, segments, constructions });
            }
            Err(why) => {
                // the short reason goes to the log and the event; the full report (every unresolved pointer, every differing line) to a file
                let (why, report) = match why.split_once(inj::REPORT_MARK) {
                    Some((w, r)) => (w.to_string(), Some(r.to_string())),
                    None => (why, None),
                };
                if let (Some(report), Some(dir)) = (report, super::lock(&self.inject_dir_path).clone()) {
                    let name = inj::report_file_name(order.key);
                    match std::fs::write(dir.join(&name), &report) {
                        Ok(()) => self.say(&format!("native inject: key {}: full report in {name}", order.key)),
                        Err(e) => self.say(&format!("native inject: key {}: cannot write {name}: {e}", order.key)),
                    }
                    for l in report.lines().take(60) {
                        self.say(&format!("native inject: report: {l}"));
                    }
                }
                let outcome = outcome.unwrap_or(inj::Outcome::Untouched);
                if outcome == inj::Outcome::Nothing {
                    self.say(&format!(
                        "native inject: key {}: REFUSED, the command was emptied: nothing of the build is applied on this game: {why}",
                        order.key
                    ));
                    how = "refused (nothing applied)";
                } else {
                    self.say(&format!(
                        "native inject: key {}: REFUSED and the command could NOT be emptied: it applies the mod's placeholder (the mod asks for the repair): {why}",
                        order.key
                    ));
                    how = "refused (NOT emptied)";
                }
                self.inject.refuse(order.key, order.held, outcome, format!("key {}: {why}", order.key));
                // the held command of the same build never takes the result of a command that did not carry the build: it fails now (its
                // tool callback fired with a failed result at the next Add), so the tool is not left busy and nothing is applied twice
                if order.held != 0 {
                    self.say(&format!(
                        "native inject: key {}: held command {} fails (its tool callback is fired with a failed result)",
                        order.key, order.held
                    ));
                    self.inject.fail_held(order.held);
                }
                link = None;
            }
        }
        if let Some(held) = link {
            self.say(&format!(
                "native inject: key {}: held command {held} completes with this command's result after the apply",
                order.key
            ));
        }
        self.wake();
        Used {
            seq: order.seq,
            timing: StampTiming {
                key: order.key,
                held: order.held,
                tm: order.tm,
                rebuild: format!(
                    "rebuild={rb0}..{} rebuild_ms={:.1} how={how}",
                    wall_ms(),
                    t0.elapsed().as_secs_f64() * 1000.0
                ),
            },
            link,
        }
    }

    /// After the simulation applied `cmd`, the command an order was used for ([`Runtime::apply_orders`] handed `used` over within the
    /// same apply: no lookup by address): its timing record, then its deferred held command takes its result.
    pub(super) fn after_ordered_apply(&self, cmd: *mut u8, used: Used) {
        // the timing first: the deferred completion swaps the applied command's payload with the held one's (plain memory, no engine call:
        // `engine::adopt_result`)
        crate::log::guarded("apply hook (timing)", || self.timing_after_apply(cmd, &used.timing));
        if let Some(held) = used.link {
            crate::log::guarded("apply hook (deferred completion)", || self.complete_deferred(cmd, held));
        }
    }

    /// One `timing apply` record with the mod's timing text, the rebuild's times and the apply's wall-clock time (the mod writes it into
    /// its trace as the build's timing line).
    fn timing_after_apply(&self, cmd: *mut u8, t: &StampTiming) {
        // SAFETY: only readable memory is read.
        let ok = unsafe { engine::command_result(cmd) }.unwrap_or(false);
        let detail = format!("{} {} apply={} accepted={ok}", t.tm, t.rebuild, wall_ms());
        let id = if t.held != 0 { t.held } else { t.key };
        self.inject.report(Event::Timing { what: "apply", id, detail });
        self.wake();
    }

    /// After the simulation applied `cmd` (simulation thread), the script command deferred held command `held` waits for: the held
    /// command takes its result and is due for completion (its tool callback is fired at the next `Add`, on the interface thread).
    fn complete_deferred(&self, cmd: *mut u8, held: u64) {
        let mut adopted = None;
        let done = {
            let mut machine = super::lock(&self.machine);
            // SAFETY: `cmd` is the command the apply function was just called with; the held command is only touched under this lock.
            let ok = unsafe { engine::command_result(cmd) }.unwrap_or(false);
            machine.complete(held, ok, |h| adopted = Some(unsafe { engine::adopt_result(h, cmd) }))
        };
        match adopted {
            Some(a) if done => self.say(&format!(
                "deferred completion: held command {held} took the result of this game's stamp apply (accepted {}, {} bytes in the command's vector); its tool callback is fired at the next Add",
                a.ok, a.vec_bytes
            )),
            _ => self.say(&format!(
                "deferred completion: held command {held} is no longer deferred (dropped before the stamp apply): nothing to complete"
            )),
        }
        self.wake();
    }

    /// What the injection needs before the engine is called: the side file, the destination's checks (is this the mod's command), then
    /// the functions and the allocator (their bytes checked). Calls nothing of the engine. A refusal empties the command, so it may only
    /// come after the command was positively identified as the mod's placeholder for this order. `positive`: it carries the conversion of
    /// the mod's own send ([`Runtime::inject_take`]), so every error is a refusal ([`inj::CROSS_CHECK`] for a text that does not fit).
    /// Without it, every error before the content identity (no side file, an unreadable one, a text that names no place) is
    /// [`inj::UNKNOWN`] and a mismatch is [`inj::NOT_OURS`]; both leave the command untouched and the order waiting (refused at its
    /// `send_done`, or expired). Emptying someone else's command on one game only would itself fork the worlds.
    ///
    /// `refund`: how many refund entries the order's side file holds (`native_refund_<key>.txt`); read and checked here, with the
    /// command's Context container and the engine's insert function, so that the engine part only calls the insert ([`refund_job`]).
    fn inject_prepare(&self, key: u64, refund: u64, dest: usize, positive: bool) -> Result<Prepared, String> {
        let read = || -> Result<(portable::Settings, std::path::PathBuf, String), String> {
            let settings = self.inject_settings.get().copied().ok_or("no portable settings")?;
            let dir = super::lock(&self.inject_dir_path).clone().ok_or("no link folder")?;
            let path = dir.join(inj::inject_file_name(key));
            let len = std::fs::metadata(&path)
                .map_err(|e| format!("no side file {}: {e}", inj::inject_file_name(key)))?
                .len();
            if len > inj::MAX_TEXT_BYTES {
                return Err(format!("the side file has {len} bytes, more than {}", inj::MAX_TEXT_BYTES));
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", inj::inject_file_name(key)))?;
            Ok((settings, dir, text))
        };
        // positive identity: the command is the mod's own (it carries its send's conversion), so every error is a refusal that empties it
        let (settings, dir, text) =
            read().map_err(|why| if positive { why } else { format!("{}{why}", inj::UNKNOWN) })?;
        let counts = inj::donor_counts(&text);
        let (edge_objects, constructions) = (counts[0], counts[1]);
        // without the positive identity, a command that does not fit the text is not the one the mod sent (a town's build_proposal, b75):
        // the order waits for the next; with it, the text is a cross-check of the mod's own command, and a mismatch is a refusal
        inj::check_destination(&LiveMem, dest, constructions, edge_objects)
            .and_then(|()| inj::same_build(&LiveMem, dest, &text))
            .map_err(|why| {
                if positive {
                    format!("{}{why}", inj::CROSS_CHECK)
                } else if why.starts_with(inj::UNKNOWN) {
                    why
                } else {
                    format!("{}{why}", inj::NOT_OURS)
                }
            })?;
        // from here on the command is the mod's placeholder for this order: a refusal empties it
        if self.build.layout_name() != portable::LAYOUT_BUILD {
            return Err(format!(
                "the layout is that of game build {}, this is {}",
                portable::LAYOUT_BUILD,
                self.build.layout_name()
            ));
        }
        let fns = self.proposal_fns()?;
        let alloc = EngineAlloc::at(self.base, &self.build.engine)
            .map_err(|why| format!("the game's allocator was not verified: {why}"))?;
        let donor = counts.iter().any(|&n| n > 0).then_some(dest);
        let refund = if refund > 0 { Some(self.refund_job(key, refund, &dir, dest)?) } else { None };
        let window = crate::refund::window_of(&text).map_err(|e| format!("refund: {e}"))?;
        Ok(Prepared { fns, alloc, settings, dir, text, donor, refund, window })
    }

    /// The refund part of an order (owner decision 2026-10-18: the bulldoze refund natively, `refund.rs`): the side file's entries,
    /// the engine's insert function (its bytes checked) and the plan for the command's own Context container (an engine-made one; which
    /// entries it lacks). Calls nothing of the engine. Every error is a refusal of the order: the command is the mod's (identified
    /// before), and a build is applied as the originator's text and Context say or not at all.
    pub(super) fn refund_job(&self, key: u64, n: u64, dir: &std::path::Path, dest: usize) -> Result<RefundJob, String> {
        let name = crate::refund::file_name(key);
        let text = std::fs::read_to_string(dir.join(&name)).map_err(|e| format!("refund: cannot read {name}: {e}"))?;
        let entries = crate::refund::parse_file(&text, n).map_err(|e| format!("refund: {name}: {e}"))?;
        let fns = self.refund_fns().map_err(|e| format!("refund: {e}"))?;
        let plan = crate::refund::plan(&LiveMem, dest, &entries).map_err(|e| format!("refund: {e}"))?;
        Ok(RefundJob { fns, entries, plan })
    }

    /// The engine's insert function of the refund container; a test sets a stand-in (`TEST_REFUND_FNS`).
    fn refund_fns(&self) -> Result<crate::refund::RefundFns, String> {
        #[cfg(test)]
        if let Some(f) = TEST_REFUND_FNS.with(|t| t.get()) {
            return Ok(f);
        }
        crate::refund::RefundFns::at(self.base, &self.build.engine)
    }

    /// The engine part of the refund: the engine's insert into the command's Context container, then the read-back (guarded). The
    /// money it brings back is measured by the mod (`refund (native): ... account before ..., after ...` in mod.log).
    pub(super) fn refund_engine(&self, key: u64, dest: usize, job: RefundJob) -> Result<(), String> {
        let RefundJob { fns, entries, plan } = job;
        crate::log::calls_engine("native inject: refund (the Context's refundableEntities insert)");
        self.say(&format!(
            "native inject: key {key}: refund (native): calling the engine's insert of {} of {} entries into the command's Context refundableEntities ({} there before)",
            plan.missing,
            entries.len(),
            plan.before
        ));
        // SAFETY: `refund::plan` accepted this payload's container in `inject_prepare`, on this thread, moments ago; nothing since has
        // touched the Context (the Proposal swap writes only the first 0x358 bytes); the function is the verified one.
        unsafe { crate::refund::insert(&fns, dest, &plan) };
        let checked = crate::log::guarded("native inject: refund check", || {
            let done = crate::refund::check(&LiveMem, dest, &entries, &plan)?;
            self.say(&format!(
                "native inject: key {key}: refund (native): {} entries set in the command's Context ({} were there from the script's conversion, {} of ours there with other values; {} in the container now) [{}]",
                done.set,
                done.before,
                done.differ,
                done.after,
                crate::refund::text(&entries)
            ));
            Ok::<(), String>(())
        });
        drop(plan); // the source range: Rust memory, never linked into the container
        checked.unwrap_or_else(|| Err("refund: our own check panicked (the PANIC line in native.log)".into()))
    }

    /// The engine part of the injection: the verified Proposal is built (engine constructor, copies, `operator new`), swapped into the
    /// command (copy assignment) and destroyed. Not inside `log::guarded`, so that an exception of the engine reaches the game's own
    /// handler; the reports in between, which call nothing of the engine, are guarded.
    fn inject_engine(&self, key: u64, dest: usize, p: Prepared) -> Result<(u64, u64, u64), String> {
        let Prepared { fns, alloc, settings, dir, text, donor, refund, window } = p;
        let live = LiveMem;
        let ctx = nc::Ctx {
            fns,
            alloc: &alloc,
            mem: &live,
            base: self.base,
            set: settings,
            build_name: self.build.layout_name(),
        };
        let mut say = |l: &str| self.say(l);
        let built = inj::build_verified(&ctx, &super::cmd_patch::LivePoke, &text, donor, &mut say);
        alloc.free_all();
        let built = built?;
        let stats = (built.nodes, built.segments, built.constructions);
        crate::log::guarded("native inject: reports", || {
            if !built.r4_report.is_empty() {
                let name = inj::report_file_name(key);
                match std::fs::write(dir.join(&name), &built.r4_report) {
                    Ok(()) => self.say(&format!("native inject: key {key}: informational R4 report in {name}")),
                    Err(e) => self.say(&format!("native inject: key {key}: cannot write {name}: {e}")),
                }
            }
            if donor.is_some() {
                self.inject_donor_report(key, dest, &text, &built, &dir);
            }
        });
        self.say("native inject: calling the engine copy assignment into the script command's Proposal");
        // SAFETY: `check_destination` passed on this payload moments ago, on the thread that queues the command.
        unsafe { inj::swap_into(&ctx, &built, dest) };
        inj::release(&ctx, built);
        // the refund list goes into the command's own Context after the Proposal (a refused proof writes nothing into it); a failure
        // here refuses the order and the command is emptied (`inject_add`), on every game alike (the same side file, the same checks)
        if let Some(job) = refund {
            self.refund_engine(key, dest, job)?;
        }
        // the Context's refund window as the originator's tool set it (`refund::WINDOW_OFFSET`): without it the engine refunds nothing,
        // whatever the list holds (lab O, 2026-10-19); a plain float of the Context, written on every game from the same text
        if let Some(w) = window {
            self.refund_window(key, dest, w)?;
        }
        Ok(stats)
    }

    /// Writes the originator's refund window into the command's Context (`refund::set_window`) and says what the script's conversion
    /// had there. `Err` (the order is refused) when the write failed.
    pub(super) fn refund_window(&self, key: u64, dest: usize, w: f32) -> Result<(), String> {
        let had =
            LiveMem.read(dest + crate::refund::WINDOW_OFFSET, 4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        crate::refund::set_window(&super::cmd_patch::LivePoke, dest, w).map_err(|e| format!("refund: {e}"))?;
        self.say(&format!(
            "native inject: key {key}: refund window {w} s of the originator's Context written into the command's Context (the script's conversion had {})",
            had.map_or("an unreadable value".to_string(), |h| h.to_string())
        ));
        Ok(())
    }

    /// Hybrid injections only (the construction is the receiver's own conversion): writes what that conversion looked like next to the
    /// originator's text, so that what the two disagree on (unnamed regions, ids) can be read: `native_inject_donor_<key>.txt` (the
    /// script command's Proposal as portable text, before the swap), `native_inject_final_<key>.txt` (the engine-built Proposal that is
    /// swapped in) and `native_inject_diff_<key>.txt` (every line where the originator's text and the donor differ).
    fn inject_donor_report(&self, key: u64, dest: usize, text: &str, built: &inj::Built, dir: &std::path::Path) {
        let Some(settings) = self.inject_settings.get().copied() else { return };
        let live = LiveMem;
        let encode = |payload: usize, event: &'static str| {
            let head = portable::Header {
                seq: 0,
                event,
                hold_id: None,
                site_rva: None,
                tool: None,
                t_ms: self.now_ms(),
                wall_ms: 0,
                payload,
            };
            portable::encode(&live, Env { image_base: self.base }, settings, self.build.layout_name(), &head)
                .map(|p| p.text)
        };
        let write = |name: String, body: &str| match std::fs::write(dir.join(&name), body) {
            Ok(()) => self.say(&format!("native inject: key {key}: wrote {name} ({} bytes)", body.len())),
            Err(e) => self.say(&format!("native inject: key {key}: cannot write {name}: {e}")),
        };
        match encode(dest, "donor") {
            Ok(donor) => {
                let diff = crate::native_copy::diff_all(text, &donor);
                let named = diff.iter().filter(|l| l.starts_with("NAMED")).count();
                self.say(&format!(
                    "native inject: key {key}: the script command's own conversion differs from the originator's text in {} line(s), {named} named, {} unnamed",
                    diff.len(),
                    diff.len() - named
                ));
                write(
                    format!("native_inject_diff_{key}.txt"),
                    &diff.join(
                        "
",
                    ),
                );
                write(format!("native_inject_donor_{key}.txt"), &donor);
            }
            Err(e) => self.say(&format!("native inject: key {key}: the donor cannot be written as text: {e}")),
        }
        if let Ok(fin) = encode(built.proposal.addr(), "final") {
            write(format!("native_inject_final_{key}.txt"), &fin);
        }
    }
}

/// An order [`Runtime::inject_take`] decided to carry out, for the engine part and the report.
struct InjectJob {
    order: inj::Order,
    dest: usize,
    t0: std::time::Instant,
    rb0: u64,
    /// What the engine part needs, or why it is refused; `None` for `complete_next` (nothing to inject).
    prepared: Option<Result<Prepared, String>>,
}

/// What the apply of a command an order was used for needs ([`Runtime::after_ordered_apply`]): the order's number, its timing record
/// and the deferred held command that takes the command's result (none when the order was refused or names none).
pub(crate) struct Used {
    pub seq: u64,
    pub timing: StampTiming,
    pub link: Option<u64>,
}

/// The most records of added commands without a held command (timing only) kept until their apply (see `Runtime::prune_added`).
const MAX_ADDED: usize = 64;

/// A command the command list's `Add` carried an order out on, until the simulation applies it (`Runtime::added`, keyed by the order's
/// number). The payload alone does not identify it: the engine may free it and give the address to another command; the identity
/// after the order (`inject::identity`) must be the same too.
pub(crate) struct Added {
    pub payload: usize,
    pub identity: inj::Identity,
    pub used: Used,
}

/// A held command's text between its encoding and its file ([`Runtime::inject_publish_text`]): the refusal (text-only checks, then
/// the originator's own engine proof) and the times.
struct TextJob {
    encoded: Result<portable::Portable, String>,
    refusal: Option<String>,
    mask_ms: f64,
    encode_ms: f64,
    proof_ms: f64,
    /// `(nodes, segments, constructions)` of the Proposal the own proof built and destroyed, when it passed.
    proved: Option<(u64, u64, u64)>,
}

/// What [`Runtime::inject_prepare`] found: the engine part runs with it.
struct Prepared {
    fns: nc::ProposalFns,
    alloc: EngineAlloc,
    settings: portable::Settings,
    dir: std::path::PathBuf,
    text: String,
    /// The script command's payload when the text adds constructions or edge objects (the receiver's own conversion of them).
    donor: Option<usize>,
    /// The order's refund entries and the plan for the command's Context container (`None`: the order names none).
    refund: Option<RefundJob>,
    /// The originator's Context refund window from the text (`None`: the text names none; the command keeps its own).
    window: Option<f32>,
}

/// The refund part of an order, checked before the engine is called ([`Runtime::refund_job`]).
pub(super) struct RefundJob {
    fns: crate::refund::RefundFns,
    entries: Vec<crate::engine::RefundEntry>,
    plan: crate::refund::Plan,
}

#[cfg(test)]
thread_local! {
    /// Stand-ins for the engine's Proposal functions in a test (the test process has no game image).
    pub(super) static TEST_FNS: std::cell::Cell<Option<nc::ProposalFns>> = const { std::cell::Cell::new(None) };
    /// A stand-in for the engine's insert of the refund container in a test.
    pub(super) static TEST_REFUND_FNS: std::cell::Cell<Option<crate::refund::RefundFns>> = const { std::cell::Cell::new(None) };
}
