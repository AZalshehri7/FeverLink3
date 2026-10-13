//! The runtime's side of `FEVERLINK3_NATIVE_COPY` (track A, step A1; `crate::native_copy` has the steps).
//! A child module of `runtime` so that it can use the runtime's private fields without growing `runtime.rs`.

use std::path::Path;

use super::{portable, rebuild, LiveMem, Runtime};
use crate::native_copy as nc;
use crate::rebuild::Alloc as _;

impl Runtime {
    /// Switches the engine-copy experiment on (`FEVERLINK3_NATIVE_COPY`), writing `native_copy.log` into `dir` when it is known.
    pub fn enable_native_copy(&self, settings: portable::Settings, dir: Option<&Path>) {
        let sink = portable::Sink::named(settings, nc::NATIVE_COPY_FILE);
        if let Some(dir) = dir {
            sink.set_dir(dir);
        }
        if self.native_copy.set(sink).is_ok() {
            self.say(&format!(
                "native copy: FEVERLINK3_NATIVE_COPY is on: for each held build command the engine's Proposal constructor, destructor and copy constructor are run on scratch memory, report in {} (the held command is only read; nothing is handed to the engine's command list)",
                nc::NATIVE_COPY_FILE
            ));
        }
    }

    /// The three steps of [`crate::native_copy::run`] for the held command at `payload`; every line is written as it is
    /// known, before the engine function it announces is called.
    pub(super) fn native_copy_check(&self, event: &str, payload: usize, hold_id: Option<u64>) {
        let Some(sink) = self.native_copy.get() else { return };
        if !sink.has_dir() {
            return;
        }
        let Some(seq) = sink.begin() else { return };
        let key = hold_id.map_or("none".to_string(), |k| k.to_string());
        let write = |text: &str| {
            let _ = sink.write(&format!("{text}\n"));
        };
        write(&format!("=== native copy #{seq} event={event} key={key} payload={payload:#x} ==="));
        let prepared = nc::ProposalFns::at(self.base).and_then(|f| rebuild::EngineAlloc::at(self.base).map(|a| (f, a)));
        let (fns, alloc) = match prepared {
            Ok(x) if self.build.name == portable::LAYOUT_BUILD => x,
            Ok(_) => {
                write(&format!(
                    "REFUSED: the functions are those of game build {}, this is {}",
                    portable::LAYOUT_BUILD,
                    self.build.name
                ));
                return;
            }
            Err(why) => {
                write(&format!("REFUSED: {why}"));
                return;
            }
        };
        let ctx = nc::Ctx {
            fns,
            alloc: &alloc,
            mem: &LiveMem,
            base: self.base,
            set: sink.settings,
            build_name: self.build.name,
        };
        nc::run(&ctx, payload, &mut |line| write(line));
        let left = alloc.free_all();
        write(&format!("=== end native copy #{seq} (leftover blocks freed: {left}) ==="));
    }
}
