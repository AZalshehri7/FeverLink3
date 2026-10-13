//! The runtime's side of `FEVERLINK3_NATIVE_PATCH` (track A, A2c; `crate::cmd_patch` has the checks and the writes).
//! A child module of `runtime` so that it can use the runtime's private fields without growing `runtime.rs`.

use super::Runtime;
use crate::cmd_patch as cp;
use crate::engine::{self, AddArgs};
use crate::link::Event;
use crate::rebuild::EngineAlloc;

/// Writes into the memory of a live command: only pages that are writable now.
struct LivePoke;

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
    pub(super) fn arm_patch(&self, frozen: Vec<u32>, adds: u32, ttl: u64, now: u64) {
        let gap = self.patch.ms_since_build_proposal(now);
        self.patch.arm(frozen, adds, ttl, now);
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
    pub(super) fn patch_add(&self, args: &AddArgs, caller: Option<usize>) {
        let Some(kind_offset) = self.build.kind_offset else { return };
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
            return; // another kind of command: the order keeps waiting
        }
        // a command of a construction tool is never the mod's (the tool's own commands are held or pass untouched)
        if tool.is_some() {
            if self.patch.pending() {
                if let Some(reason) = self.patch.skip_once("a build_proposal from a construction tool's call site") {
                    self.patch.report(Event::PatchSkipped { reason, kind, site });
                }
            }
            return;
        }
        // the mod writes `patch_next` and then sends the command: the order may still be in the file (the service thread has not
        // read it, holding may be off so the hold path did not look, the machine's lock may be taken)
        if !self.patch.pending() {
            self.ingest_patch_order(self.now_ms());
            if !self.patch.pending() {
                return;
            }
        }
        let now = self.now_ms();
        let Some(order) = self.patch.take(now) else { return };
        // SAFETY: only readable memory is read.
        let Some(payload) = (unsafe { engine::payload_of(args.cmd) }) else {
            self.patch.report(Event::PatchRefused { reason: "the command has no payload".into() });
            return;
        };
        let result = match EngineAlloc::at(self.base) {
            Err(why) => Err(format!("the game's allocator was not verified: {why}")),
            Ok(_) if self.build.name != crate::portable::LAYOUT_BUILD => Err(format!(
                "the layout is that of game build {}, this is {}",
                crate::portable::LAYOUT_BUILD,
                self.build.name
            )),
            Ok(alloc) => cp::apply(&crate::dump::LiveMem, &LivePoke, &alloc, payload, &order),
        };
        match result {
            Ok(done) => {
                self.patch.report(Event::Patched { frozen: order.frozen, adds: order.adds, allocated: done.allocated })
            }
            Err(reason) => self.patch.report(Event::PatchRefused { reason }),
        }
        self.wake();
    }
}
