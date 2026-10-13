//! The runtime's side of `FEVERLINK3_NATIVE_INJECT` (`crate::inject` has the checks and the engine calls).
//! A child module of `runtime` so that it can use the runtime's private fields.

use super::Runtime;
use crate::dump::{Env, LiveHeapMem, LiveMem};
use crate::engine::{self, AddArgs};
use crate::inject as inj;
use crate::link::Event;
use crate::native_copy as nc;
use crate::portable;
use crate::rebuild::{Alloc, EngineAlloc};

impl Runtime {
    /// Switches the native injection on (`FEVERLINK3_NATIVE_INJECT=1`). `settings` are the portable text settings the originator's
    /// copy of the text is written with and the receiver's proof is made with.
    pub fn enable_native_inject(&self, settings: portable::Settings) {
        // the construction of the text is the receiver's own (its conversion), so its description's heap graph (tens of thousands of
        // blocks) is neither written nor needed
        let _ = self.inject_settings.set(portable::Settings { graph_skip: &["toAdd"], ..settings });
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
        match portable::encode(&live, Env { image_base: self.base }, settings, self.build.name, &head) {
            Ok(p) => match self.text.write(id, &p.text) {
                Ok(()) => self.say(&format!(
                    "native inject: hold {id}: portable text written in {} ms, {} bytes, {} lines (native_text_{id}.txt); {}",
                    t0.elapsed().as_millis(),
                    p.text.len(),
                    p.text.lines().count(),
                    p.text.lines().last().unwrap_or("")
                )),
                Err(e) => self.say(&format!("native inject: hold {id}: cannot write the text: {e}")),
            },
            Err(why) => self.say(&format!("native inject: hold {id}: the layout table refuses this command: {why}")),
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
        let t0 = std::time::Instant::now();
        match self.inject_run(order.key, dest) {
            Ok((nodes, segments, constructions)) => {
                self.say(&format!(
                    "native inject: key {}: INJECTED the engine Proposal built from the text ({nodes} nodes, {segments} segments, {constructions} construction(s)) in {} ms",
                    order.key,
                    t0.elapsed().as_millis()
                ));
                self.inject.report(Event::Injected { key: order.key, nodes, segments, constructions });
            }
            Err(why) => {
                self.say(&format!("native inject: key {}: REFUSED, the command is untouched: {why}", order.key));
                self.inject.refuse(format!("key {}: {why}", order.key));
            }
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
        let constructions = inj::construction_count(&text);
        inj::check_destination(&LiveMem, dest, constructions)?;
        // an index of the heap's blocks for the proof (refreshed before every text, see LiveHeapMem)
        let live = LiveHeapMem::new();
        let ctx =
            nc::Ctx { fns, alloc: &alloc, mem: &live, base: self.base, set: settings, build_name: self.build.name };
        let mut say = |l: &str| self.say(l);
        let donor = (constructions > 0).then_some(dest);
        let built = inj::build_verified(&ctx, &super::cmd_patch::LivePoke, &text, donor, &mut say);
        alloc.free_all();
        let built = built?;
        let stats = (built.nodes, built.segments, built.constructions);
        self.say("native inject: calling the engine copy assignment into the script command's Proposal");
        // SAFETY: `check_destination` passed on this payload moments ago, on the thread that queues the command.
        unsafe { inj::swap_into(&ctx, &built, dest) };
        inj::release(&ctx, built);
        Ok(stats)
    }
}
