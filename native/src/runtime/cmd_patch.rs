//! The runtime's side of `FEVERLINK3_NATIVE_PATCH` (track A, A2c; `crate::cmd_patch` has the checks and the writes).
//! A child module of `runtime` so that it can use the runtime's private fields without growing `runtime.rs`.

use super::Runtime;
use crate::cmd_patch as cp;
use crate::engine::{self, AddArgs};
use crate::link::Event;
use crate::rebuild::EngineAlloc;

/// Writes into the memory of a live command: only pages that are writable now.
pub(super) struct LivePoke;

impl cp::Poke for LivePoke {
    fn poke(&self, addr: usize, data: &[u8]) -> bool {
        const WRITABLE: u32 = 0x04 | 0x08 | 0x40 | 0x80; // PAGE_READWRITE, WRITECOPY, EXECUTE_READWRITE, EXECUTE_WRITECOPY
                                                         // SAFETY: only asks the OS about the pages; the copy follows a check that both ends of the range are writable.
        let ok = unsafe {
            [addr, addr + data.len().saturating_sub(1)]
                .iter()
                .all(|&a| crate::mem::protection_of(a as *const u8).is_some_and(|p| p & WRITABLE != 0))
        };
        if ok {
            // SAFETY: the range was just found writable; the callers pass addresses inside the command's own memory.
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
        }
        ok
    }
}

impl Runtime {
    /// Switches the command patch on (`FEVERLINK3_NATIVE_PATCH=1`).
    pub fn enable_native_patch(&self) {
        self.patch.enable();
        self.say("native patch: FEVERLINK3_NATIVE_PATCH is on: a patch_next order from the mod writes frozenNodes into the next script build_proposal command (all checks first; a refusal leaves the command untouched)");
    }

    /// The switch is on and the prerequisites hold (this layout's build, the allocator's prologues): the `ready` event then lists
    /// `patch_next` among the hooks, which is how the mod learns it may send the order.
    pub(super) fn patch_ready(&self) -> bool {
        cp::announces(self.patch.enabled(), self.build.name, || EngineAlloc::at(self.base).is_ok())
    }

    /// A `patch_next` order of the control file.
    pub(super) fn arm_patch(&self, frozen: Vec<u32>, adds: u32, at: Vec<[f64; 3]>, ttl: u64, now: u64) {
        let gap = self.patch.ms_since_build_proposal(now);
        self.patch.arm(frozen, adds, at, ttl, now);
        self.say(&format!(
            "native patch: patch_next first seen; ms since the previous build_proposal Add: {}",
            gap.map_or("none yet".to_string(), |g| g.to_string())
        ));
    }

    /// Remembers one Add (any kind) for the diagnostics of an order that finds no command.
    pub(super) fn patch_note_add(&self, args: &AddArgs, caller: Option<usize>, held: bool) {
        let kind = self.build.kind_offset.and_then(|o| {
            // SAFETY: only readable memory is read.
            unsafe { engine::command_kind(args.cmd, o as usize) }
        });
        let rva = caller.and_then(|c| c.checked_sub(self.base)).and_then(|r| u32::try_from(r).ok());
        let is_bp = kind.is_some() && self.build.kind_by_name("build_proposal") == kind;
        self.patch.note_add(cp::AddRec { kind, rva, ms: self.now_ms(), held }, is_bp);
    }

    /// The command list's `Add` of a command that was not held: if an order is armed and this is a script `build_proposal`, carries it
    /// out (once). Runs before the original `Add`, on the thread that queues commands.
    ///
    /// The write allocates with the game's `operator new`, so this runs outside `log::guarded` and guards the decision and the report
    /// itself: never call it inside a guard.
    pub(super) fn patch_add(&self, args: &AddArgs, caller: Option<usize>) {
        crate::log::calls_engine("native patch: an order may be carried out (operator new)");
        let job = crate::log::guarded("native patch: order", || self.patch_take(args, caller)).flatten();
        let Some((order, payload)) = job else { return };
        // the same write first as a dry pass inside a guard (`native_copy::dry_pass`): a panic of ours is a refusal
        let dry = self.patch_dry("native patch: dry pass", |c| {
            let _ = cp::apply(c.mem, &crate::native_copy::DryPoke, c.alloc, payload, &order);
        });
        let result = dry.and_then(|()| match self.patch_alloc() {
            Err(why) => Err(why),
            Ok(alloc) => cp::apply(&crate::dump::LiveMem, &LivePoke, &alloc, payload, &order),
        });
        crate::log::guarded("native patch: result", || {
            match result {
                Ok(done) => self.patch.report(Event::Patched {
                    frozen: done.frozen,
                    adds: u32::try_from(done.nodes).unwrap_or(u32::MAX),
                    allocated: done.allocated,
                }),
                Err(reason) => self.patch.report(Event::PatchRefused { reason }),
            }
            self.wake();
        });
    }

    /// The decision of [`Runtime::patch_add`]: the order and the command's payload when the order is to be carried out now.
    fn patch_take(&self, args: &AddArgs, caller: Option<usize>) -> Option<(cp::Order, usize)> {
        let kind_offset = self.build.kind_offset?;
        // SAFETY: only readable memory is read.
        let kind = unsafe { engine::command_kind(args.cmd, kind_offset as usize) };
        let is_bp = kind.is_some() && self.build.kind_by_name("build_proposal") == kind;
        let site = caller.and_then(|c| c.checked_sub(self.base)).and_then(|rva| u32::try_from(rva).ok());
        let tool = site.and_then(|rva| self.build.ui_site_index(rva));
        if let Some(armed) = self.patch.armed_ms().filter(|_| self.patch.pending()) {
            let rec = cp::AddRec { kind, rva: site, ms: self.now_ms(), held: false };
            self.patch.saw_add(rec, is_bp, tool.is_some(), armed);
        }
        if !is_bp {
            return None; // another kind of command: the order keeps waiting
        }
        // a command of a construction tool is never the mod's (the tool's own commands are held or pass untouched)
        if tool.is_some() {
            if self.patch.pending() {
                if let Some(reason) = self.patch.skip_once("a build_proposal from a construction tool's call site") {
                    self.patch.report(Event::PatchSkipped { reason, kind, site });
                }
            }
            return None;
        }
        // the mod writes `patch_next` and then sends the command: the order may still be in the file (the service thread has not
        // read it, holding may be off so the hold path did not look, the machine's lock may be taken)
        if !self.patch.pending() {
            self.ingest_patch_order(self.now_ms());
            if !self.patch.pending() {
                return None;
            }
        }
        let now = self.now_ms();
        let converted = self.patch.converted();
        let order = self.patch.take(now)?;
        if converted > 0 {
            // the conversion hook already wrote the order into the proposal(s) this build converted; the command made from one of
            // them is here, so the order is done (nothing is written a second time)
            self.say(&format!(
                "native patch: a build_proposal Add arrived (call site rva {:x?}) after {converted} patched conversion(s): the order is cleared",
                site
            ));
            return None;
        }
        // SAFETY: only readable memory is read.
        let Some(payload) = (unsafe { engine::payload_of(args.cmd) }) else {
            self.patch.report(Event::PatchRefused { reason: "the command has no payload".into() });
            return None;
        };
        Some((order, payload))
    }

    /// Before the construction update inside `Convert` runs: `proposal` is the Proposal being built (it holds the SimpleProposal's
    /// nodes and segments, not yet the construction's entrance). While an order is armed and describes it (the index form's `adds` is
    /// the count of nodes the SimpleProposal brought), `proposal.frozenNodes` is written, so that the update sees the nodes the tool's
    /// own proposal froze. Returns whether anything was armed (the caller then logs what the update left).
    ///
    /// The write allocates with the game's `operator new`: this guards its decision and its report itself and must not be called
    /// inside `log::guarded`.
    pub fn before_convert_update(&self, proposal: usize) -> bool {
        crate::log::calls_engine("native patch: convert update (operator new)");
        let job = crate::log::guarded("native patch: convert update", || {
            let now = self.now_ms();
            if !self.patch.pending() {
                self.ingest_patch_order(now);
                if !self.patch.pending() {
                    return None;
                }
            }
            let order = self.patch.peek(now)?;
            let nodes = cp::added_nodes(&crate::dump::LiveMem, proposal).map_or("?".to_string(), |n| n.to_string());
            let fits = cp::describes_frozen_only(&crate::dump::LiveMem, proposal, &order);
            Some((now, order, nodes, fits))
        });
        let Some((now, order, nodes, fits)) = job.flatten() else { return false };
        let outcome = fits.and_then(|()| {
            self.patch_dry("native patch: convert update: dry pass", |c| {
                let _ = cp::apply_proposal_frozen_only(c.mem, &crate::native_copy::DryPoke, c.alloc, proposal, &order);
            })
            .map_err(cp::Fail::Refuse)?;
            match self.patch_alloc() {
                Err(why) => Err(cp::Fail::Refuse(why)),
                Ok(alloc) => cp::apply_proposal_frozen_only(&crate::dump::LiveMem, &LivePoke, &alloc, proposal, &order),
            }
        });
        crate::log::guarded("native patch: convert update (result)", || {
            self.convert_update_report(now, &nodes, outcome)
        });
        true
    }

    fn convert_update_report(&self, now: u64, nodes: &str, outcome: Result<cp::Done, cp::Fail>) {
        match outcome {
            Ok(done) => {
                self.patch.converted_one();
                self.say(&format!("native patch: convert update: {nodes} nodes before the construction update: PATCHED proposal.frozenNodes {:?} (new block: {})", done.frozen, done.allocated));
                self.patch.convert_event(Event::Patched {
                    frozen: done.frozen,
                    adds: u32::try_from(done.nodes).unwrap_or(u32::MAX),
                    allocated: done.allocated,
                });
            }
            Err(cp::Fail::NotThis(why)) => {
                self.say(&format!(
                    "native patch: convert update: {nodes} nodes before the construction update: skipped: {why}"
                ));
                self.patch.convert_event(Event::PatchSkipped {
                    reason: format!("convert update: {why}"),
                    kind: None,
                    site: None,
                });
            }
            Err(cp::Fail::Refuse(why)) => {
                self.patch.take(now);
                self.say(&format!(
                    "native patch: convert update: {nodes} nodes before the construction update: REFUSED: {why}"
                ));
                self.patch.report(Event::PatchRefused { reason: format!("convert update: {why}") });
            }
        }
        self.wake();
    }

    /// After the construction update: what the proposal holds now (a dump per distinct shape), to see whether the update honoured the
    /// frozen nodes (no extra nodes of its own) and what it left in the frozen lists.
    pub fn after_convert_update(&self, proposal: usize) {
        let (shape, dump) = cp::dump_proposal(&crate::dump::LiveMem, proposal, None);
        if self.patch.new_shape(&format!("after update: {shape}")) {
            self.say(&format!("native patch: convert update: after the update: {dump}"));
            self.patch.convert_event(Event::ConvertDump { text: format!("after the construction update: {dump}") });
        }
    }

    /// The dry pass of a patch write (`native_copy::dry_pass`): `f` with stand-ins for the allocator and the live writes, inside a guard.
    fn patch_dry(&self, site: &'static str, f: impl FnOnce(&crate::native_copy::Ctx)) -> Result<(), String> {
        let set = crate::portable::Settings::input_level();
        crate::native_copy::dry_pass(site, &crate::dump::LiveMem, self.base, set, self.build.name, f)
    }

    /// The allocator and layout checks both ways of patching need.
    fn patch_alloc(&self) -> Result<EngineAlloc, String> {
        let alloc =
            EngineAlloc::at(self.base).map_err(|why| format!("the game's allocator was not verified: {why}"))?;
        if self.build.name != crate::portable::LAYOUT_BUILD {
            return Err(format!(
                "the layout is that of game build {}, this is {}",
                crate::portable::LAYOUT_BUILD,
                self.build.name
            ));
        }
        Ok(alloc)
    }

    /// What the hook on `scripting::Convert` does after the original returned: `out` is the `std::optional<Proposal>` it filled
    /// (the Proposal at +0, "has a value" at +0x358). While a `patch_next` order is armed, a proposal that has a value and that the
    /// order describes (same number of added nodes, one construction at least) gets its frozenNodes written, before anything checks
    /// it. The order stays armed (the mod's dry verdict and the build convert separately, and the command's `Add` clears it);
    /// a conversion the order does not describe leaves both alone. Every call while an order is armed is in `native.log`.
    ///
    /// The write allocates with the game's `operator new`: this guards its decision and its report itself and must not be called
    /// inside `log::guarded`.
    pub fn on_convert(&self, out: usize, caller: Option<usize>) {
        if !self.patch.enabled() {
            return; // the hook is there for the native inject's identity alone (`inject_on_convert`)
        }
        crate::log::calls_engine("native patch: convert (operator new)");
        let job = crate::log::guarded("native patch: convert", || {
            let now = self.now_ms();
            if !self.patch.pending() {
                // the mod writes `patch_next` and calls the engine in the same breath: look at the file now
                self.ingest_patch_order(now);
                if !self.patch.pending() {
                    return None;
                }
            }
            let order = self.patch.peek(now)?; // expired: the service thread reports it
            let site = caller.and_then(|c| c.checked_sub(self.base)).and_then(|rva| u32::try_from(rva).ok());
            let at = site.map_or("?".to_string(), |r| format!("{r:x}"));
            let mem = crate::dump::LiveMem;
            let flag = crate::dump::Mem::read(&mem, out + cp::PROPOSAL_SIZE, 1).map(|b| b[0]);
            let nodes = cp::added_nodes(&mem, out).map_or("?".to_string(), |n| n.to_string());
            // what this conversion produced, once per distinct shape (the engine's own frozenNodes lists show whether it fills them)
            let (shape, dump) = cp::dump_proposal(&mem, out, flag);
            if self.patch.new_shape(&shape) {
                self.say(&format!("native patch: convert: call site rva {at}: {dump}"));
                self.patch.convert_event(Event::ConvertDump { text: format!("call site rva {at}: {dump}") });
            }
            let fits: Result<(), cp::Fail> = match flag {
                None => Err(cp::Fail::NotThis("the result is not readable".into())),
                Some(0) => Err(cp::Fail::NotThis("Convert produced no proposal (the conversion failed)".into())),
                // the order must describe this proposal before the allocator matters: another conversion is none of its business
                Some(_) => cp::describes(&mem, out, &order),
            };
            Some((now, order, site, at, nodes, fits))
        });
        let Some((now, order, site, at, nodes, fits)) = job.flatten() else { return };
        let outcome = fits.and_then(|()| {
            self.patch_dry("native patch: convert: dry pass", |c| {
                let _ = cp::apply_proposal(c.mem, &crate::native_copy::DryPoke, c.alloc, out, &order);
            })
            .map_err(cp::Fail::Refuse)?;
            match self.patch_alloc() {
                Err(why) => Err(cp::Fail::Refuse(why)),
                Ok(alloc) => cp::apply_proposal(&crate::dump::LiveMem, &LivePoke, &alloc, out, &order),
            }
        });
        crate::log::guarded("native patch: convert (result)", || {
            self.convert_report(now, &order, site, &at, &nodes, outcome)
        });
    }

    fn convert_report(
        &self,
        now: u64,
        order: &cp::Order,
        site: Option<u32>,
        at: &str,
        nodes: &str,
        outcome: Result<cp::Done, cp::Fail>,
    ) {
        match outcome {
            Ok(done) => {
                self.patch.converted_one();
                self.say(&format!(
                    "native patch: convert: call site rva {at}, {nodes} added nodes: PATCHED frozenNodes {:?} (new block: {}){}; the order stays armed until a build_proposal Add or its ttl",
                    done.frozen,
                    done.allocated,
                    if order.at.is_empty() { String::new() } else { format!(", resolved from {} position(s)", order.at.len()) }
                ));
                self.patch.convert_event(Event::Patched {
                    frozen: done.frozen,
                    adds: u32::try_from(done.nodes).unwrap_or(u32::MAX),
                    allocated: done.allocated,
                });
            }
            Err(cp::Fail::NotThis(why)) => {
                self.say(&format!(
                    "native patch: convert: call site rva {at}, {nodes} added nodes (order: {}): skipped: {why}",
                    if order.at.is_empty() {
                        format!("adds {}", order.adds)
                    } else {
                        format!("{} position(s)", order.at.len())
                    }
                ));
                self.patch.convert_event(Event::PatchSkipped { reason: format!("convert: {why}"), kind: None, site });
            }
            Err(cp::Fail::Refuse(why)) => {
                self.patch.take(now);
                self.say(&format!("native patch: convert: call site rva {at}, {nodes} added nodes: REFUSED: {why}"));
                self.patch.report(Event::PatchRefused { reason: format!("convert: {why}") });
            }
        }
        self.wake();
    }
}
