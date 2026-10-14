//! The runtime's side of `FEVERLINK3_NATIVE_INJECT` (`crate::inject` has the checks and the engine calls).
//! A child module of `runtime` so that it can use the runtime's private fields.

use super::Runtime;
use crate::dump::{Env, LiveHeapMem, LiveMem, Mem};
use crate::engine::{self, AddArgs};
use crate::inject as inj;
use crate::link::Event;
use crate::native_copy as nc;
use crate::portable;
use crate::rebuild::{Alloc, EngineAlloc};

use super::{wall_ms, StampTiming};

impl Runtime {
    /// Switches the native injection on (`FEVERLINK3_NATIVE_INJECT=1`). `settings` are the portable text settings the originator's
    /// copy of the text is written with and the receiver's proof is made with.
    pub fn enable_native_inject(&self, settings: portable::Settings) {
        // the construction of the text is the receiver's own (its conversion), so its description's heap graph (tens of thousands of
        // blocks) is neither written nor needed
        let _ = self.inject_settings.set(portable::Settings { graph_skip: &["toAdd"], typed: true, ..settings });
        self.inject.enable();
        self.say("native inject: FEVERLINK3_NATIVE_INJECT is on: held commands are also written as native_text_<id>.txt (full portable text), and an inject_next order makes the next script build_proposal take the engine Proposal built from native_inject_<key>.txt (all checks first; a refusal leaves the command untouched)");
    }

    pub(super) fn inject_dir(&self, dir: &std::path::Path) {
        *super::lock(&self.inject_dir_path) = Some(dir.to_path_buf());
    }

    /// The switch is on and the prerequisites hold (this layout's build, the engine's Proposal functions, the allocator's prologues):
    /// the `ready` event then lists `inject_next`.
    pub(super) fn inject_ready(&self) -> bool {
        self.inject.enabled()
            && self.build.name == portable::LAYOUT_BUILD
            && nc::ProposalFns::at(self.base).is_ok()
            && EngineAlloc::at(self.base).is_ok()
    }

    /// The originator's side: the held command's full portable text, for the mod to ship (`native_text_<id>.txt`).
    pub(super) fn inject_publish_text(&self, payload: usize, id: u64) {
        if !self.inject.enabled() {
            return;
        }
        let Some(settings) = self.inject_settings.get().copied() else { return };
        let t0 = std::time::Instant::now();
        let wall0 = wall_ms();
        let live = LiveHeapMem::new();
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
        // R2: which unnamed bytes of the held command are values and which residue, by a differential engine copy (both copies destroyed)
        let mask = nc::ProposalFns::at(self.base)
            .and_then(|fns| {
                nc::residue_mask(&fns, &live, self.base, settings, self.build.name, payload, &mut |l| self.say(l))
            })
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
        let encoded =
            portable::encode_with(&live, Env { image_base: self.base }, settings, self.build.name, &head, opts);
        let encode_ms = t_enc.elapsed().as_secs_f64() * 1000.0;
        let t_write = std::time::Instant::now();
        let written: Option<std::io::Result<()>> = encoded.as_ref().ok().map(|p| self.text.write(id, &p.text));
        let write_ms = t_write.elapsed().as_secs_f64() * 1000.0;
        let walk_ms = live.last_walk_ms();
        let bytes = encoded.as_ref().map_or(0, |p| p.text.len());
        self.inject.report(Event::Timing {
            what: "capture",
            id,
            detail: format!(
                "held={id} start={wall0} end={} ms={:.1} bytes={bytes} mask_ms={mask_ms:.1} encode_ms={encode_ms:.1} heap_walk_ms={walk_ms:.1} heap_blocks={} write_ms={write_ms:.1}",
                wall_ms(),
                t0.elapsed().as_secs_f64() * 1000.0,
                live.blocks()
            ),
        });
        match encoded.map(|p| (written, p)) {
            Ok((Some(w), p)) => match w {
                Ok(()) if !p.unresolved.is_empty() => {
                    // every pointer the text could not follow: where it is, where it points (an address is for this log only) and what
                    // the memory there looks like, so that one run shows everything that stays to be understood
                    for (member, addr) in p.unresolved.iter().take(40) {
                        let bytes = LiveMem.read(*addr, 32).map_or("unreadable".to_string(), |b| crate::plan::hex(&b));
                        self.say(&format!(
                            "native inject: hold {id}: unresolved pointer in {member} -> {addr:#x} [{}] bytes {bytes}",
                            live.heap_near(*addr)
                        ));
                    }
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
    /// everything is checked first and a refusal leaves the command untouched. A construction tool's command that was not held can be
    /// mistaken for the script's here; the order's own checks (the number of constructions or nodes) guard against that.
    pub(super) fn apply_orders(&self, cmd: *mut u8, kind: Option<u8>, held: bool) {
        let bp = kind.is_some() && self.build.kind_by_name("build_proposal") == kind;
        if !bp {
            return;
        }
        let now = self.now_ms();
        self.inject.note_apply(held, now);
        self.patch.note_apply(held, now);
        if held {
            return;
        }
        let args = AddArgs {
            list: core::ptr::null_mut(),
            out: core::ptr::null_mut(),
            cmd,
            done: core::ptr::null_mut(),
            progress: core::ptr::null_mut(),
        };
        if self.inject.enabled() {
            self.inject_add(&args, None);
        } else if self.patch.enabled() {
            self.patch_add(&args, None);
        }
    }

    /// The command list's `Add` of a command that was not held: if an order is armed and this is a script `build_proposal`, carries it
    /// out (once). Runs before the original `Add`, on the thread that queues commands.
    pub(super) fn inject_add(&self, args: &AddArgs, caller: Option<usize>) {
        let site = caller.and_then(|c| c.checked_sub(self.base)).and_then(|rva| u32::try_from(rva).ok());
        if site.and_then(|rva| self.build.ui_site_index(rva)).is_some() {
            return; // a construction tool's command is never the mod's
        }
        let Some(kind_offset) = self.build.kind_offset else { return };
        // SAFETY: only readable memory is read.
        let Some(kind) = (unsafe { engine::command_kind(args.cmd, kind_offset as usize) }) else { return };
        if self.build.kind_by_name("build_proposal") != Some(kind) {
            return; // another kind of command: the order keeps waiting
        }
        let now = self.now_ms();
        if !self.inject.pending() {
            // the mod writes `inject_next` and sends the command in the same breath: look at the file now
            self.ingest_patch_order(now);
            if !self.inject.pending() {
                return;
            }
        }
        let Some(order) = self.inject.take(now) else {
            self.wake();
            return;
        };
        // SAFETY: only readable memory is read.
        let Some(dest) = (unsafe { engine::payload_of(args.cmd) }) else {
            self.inject.refuse(format!("key {}: the command has no payload", order.key));
            self.wake();
            return;
        };
        // a deferred held command of the same build takes this command's result once the simulation has applied it, injected or not
        // (a refusal leaves the command to the record path, which is still this game's stamp apply of the build)
        let link = (order.held != 0).then_some((dest, order.held));
        let t0 = std::time::Instant::now();
        let rb0 = wall_ms();
        let run = if order.link_only {
            Err(String::new()) // complete_next: the record's own command is this game's stamp apply; nothing to inject
        } else {
            self.inject_run(order.key, dest)
        };
        match run {
            Err(why) if order.link_only && why.is_empty() => {
                self.say(&format!(
                    "native inject: complete_next held {}: this script build_proposal is this game's stamp apply (record path)",
                    order.held
                ));
            }
            Ok((nodes, segments, constructions)) => {
                self.say(&format!(
                    "native inject: key {}: INJECTED the engine Proposal built from the text ({nodes} nodes, {segments} segments, {constructions} construction(s)) in {} ms",
                    order.key,
                    t0.elapsed().as_millis()
                ));
                self.inject.report(Event::Injected { key: order.key, nodes, segments, constructions });
                *super::lock(&self.inject_last_ok) = Some(order.key);
            }
            Err(why) if why.starts_with(inj::NOT_OURS) => {
                self.say(&format!(
                    "native inject: key {}: {why}; the order waits for the next build_proposal",
                    order.key
                ));
                self.inject.put_back(order);
                self.wake();
                return;
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
                self.say(&format!("native inject: key {}: REFUSED, the command is untouched: {why}", order.key));
                self.inject.refuse(format!("key {}: {why}", order.key));
            }
        }
        let how = if order.link_only {
            "record (complete_next)"
        } else if super::lock(&self.inject_last_ok).take() == Some(order.key) {
            "injected"
        } else {
            "refused (record path)"
        };
        *super::lock(&self.stamp_timing) = Some(StampTiming {
            payload: dest,
            key: order.key,
            held: order.held,
            tm: order.tm.clone(),
            rebuild: format!(
                "rebuild={rb0}..{} rebuild_ms={:.1} how={how}",
                wall_ms(),
                t0.elapsed().as_secs_f64() * 1000.0
            ),
        });
        if let Some((payload, held)) = link {
            self.say(&format!(
                "native inject: key {}: held command {held} completes with this command's result after the apply",
                order.key
            ));
            *super::lock(&self.defer_link) = Some((payload, held));
        }
        self.wake();
    }

    /// After the simulation applied `cmd`: when it is the command an order was used for, one `timing apply` record with the mod's timing
    /// text, the rebuild's times and the apply's wall-clock time (the mod writes it into its trace as the build's timing line).
    pub(super) fn timing_after_apply(&self, cmd: *mut u8) {
        // SAFETY: only readable memory is read.
        let Some(payload) = (unsafe { engine::payload_of(cmd) }) else { return };
        let t = {
            let mut g = super::lock(&self.stamp_timing);
            match &*g {
                Some(t) if t.payload == payload => g.take(),
                _ => None,
            }
        };
        let Some(t) = t else { return };
        let ok = unsafe { engine::command_result(cmd) }.unwrap_or(false);
        let detail = format!("{} {} apply={} accepted={ok}", t.tm, t.rebuild, wall_ms());
        let id = if t.held != 0 { t.held } else { t.key };
        self.inject.report(Event::Timing { what: "apply", id, detail });
        self.wake();
    }

    /// After the simulation applied `cmd` (simulation thread): when it is the script command a deferred held command waits for, the held
    /// command takes its result and is due for completion (its tool callback is fired at the next `Add`, on the interface thread).
    pub(super) fn complete_deferred(&self, cmd: *mut u8) {
        // SAFETY: only readable memory is read.
        let Some(payload) = (unsafe { engine::payload_of(cmd) }) else { return };
        let link = {
            let mut g = super::lock(&self.defer_link);
            match *g {
                Some((p, _)) if p == payload => g.take(),
                _ => None,
            }
        };
        let Some((_, held)) = link else { return };
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

    /// Everything between the order and the swap; every early return leaves the command untouched.
    fn inject_run(&self, key: u64, dest: usize) -> Result<(u64, u64, u64), String> {
        if self.build.name != portable::LAYOUT_BUILD {
            return Err(format!(
                "the layout is that of game build {}, this is {}",
                portable::LAYOUT_BUILD,
                self.build.name
            ));
        }
        let fns = nc::ProposalFns::at(self.base)?;
        let alloc =
            EngineAlloc::at(self.base).map_err(|why| format!("the game's allocator was not verified: {why}"))?;
        let settings = self.inject_settings.get().copied().ok_or("no portable settings")?;
        let dir = super::lock(&self.inject_dir_path).clone().ok_or("no link folder")?;
        // the cheap refusals first
        let path = dir.join(inj::inject_file_name(key));
        let len =
            std::fs::metadata(&path).map_err(|e| format!("no side file {}: {e}", inj::inject_file_name(key)))?.len();
        if len > inj::MAX_TEXT_BYTES {
            return Err(format!("the side file has {len} bytes, more than {}", inj::MAX_TEXT_BYTES));
        }
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", inj::inject_file_name(key)))?;
        let counts = inj::donor_counts(&text);
        let (edge_objects, constructions) = (counts[0], counts[1]);
        // a command that does not fit the text is not the one the mod sent (a town's build_proposal, b75): the order waits for the next
        inj::check_destination(&LiveMem, dest, constructions, edge_objects)
            .and_then(|()| inj::same_constructions(&LiveMem, dest, &text))
            .map_err(|why| format!("{}{why}", inj::NOT_OURS))?;
        // an index of the heap's blocks for the proof (refreshed before every text, see LiveHeapMem)
        let live = LiveHeapMem::new();
        let ctx =
            nc::Ctx { fns, alloc: &alloc, mem: &live, base: self.base, set: settings, build_name: self.build.name };
        let mut say = |l: &str| self.say(l);
        let donor = counts.iter().any(|&n| n > 0).then_some(dest);
        let built = inj::build_verified(&ctx, &super::cmd_patch::LivePoke, &text, donor, &mut say);
        alloc.free_all();
        let built = built?;
        let stats = (built.nodes, built.segments, built.constructions);
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
        self.say("native inject: calling the engine copy assignment into the script command's Proposal");
        // SAFETY: `check_destination` passed on this payload moments ago, on the thread that queues the command.
        unsafe { inj::swap_into(&ctx, &built, dest) };
        inj::release(&ctx, built);
        Ok(stats)
    }

    /// Hybrid injections only (the construction is the receiver's own conversion): writes what that conversion looked like next to the
    /// originator's text, so that what the two disagree on (unnamed regions, ids) can be read: `native_inject_donor_<key>.txt` (the
    /// script command's Proposal as portable text, before the swap), `native_inject_final_<key>.txt` (the engine-built Proposal that is
    /// swapped in) and `native_inject_diff_<key>.txt` (every line where the originator's text and the donor differ).
    fn inject_donor_report(&self, key: u64, dest: usize, text: &str, built: &inj::Built, dir: &std::path::Path) {
        let Some(settings) = self.inject_settings.get().copied() else { return };
        let live = LiveHeapMem::new();
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
            portable::encode(&live, Env { image_base: self.base }, settings, self.build.name, &head).map(|p| p.text)
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
