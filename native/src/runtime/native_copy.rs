//! The runtime's side of `FEVERLINK3_NATIVE_COPY` (track A, step A1; `crate::native_copy` has the steps).
//! A child module of `runtime` so that it can use the runtime's private fields without growing `runtime.rs`.

use std::path::Path;

use super::{portable, rebuild, Runtime};
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
    /// known, before the engine function it announces is called. The steps call the engine, so they run outside
    /// `log::guarded` (the checks before them are guarded): never call this inside a guard.
    pub(super) fn native_copy_check(&self, event: &str, payload: usize, hold_id: Option<u64>) {
        let Some(sink) = self.native_copy.get() else { return };
        crate::log::calls_engine("native copy (the Proposal functions, operator new)");
        let write = |text: &str| {
            let _ = sink.write(&format!("{text}\n"));
        };
        let prepared = crate::log::guarded("native copy", || {
            if !sink.has_dir() {
                return None;
            }
            let seq = sink.begin()?;
            let key = hold_id.map_or("none".to_string(), |k| k.to_string());
            write(&format!("=== native copy #{seq} event={event} key={key} payload={payload:#x} ==="));
            let prepared = nc::ProposalFns::at(self.base, &self.build.engine)
                .and_then(|f| rebuild::EngineAlloc::at(self.base, &self.build.engine).map(|a| (f, a)));
            match prepared {
                Ok((fns, alloc)) if self.build.layout_name() == portable::LAYOUT_BUILD => Some((seq, fns, alloc)),
                Ok(_) => {
                    write(&format!(
                        "REFUSED: the functions are those of game build {}, this is {}",
                        portable::LAYOUT_BUILD,
                        self.build.layout_name()
                    ));
                    None
                }
                Err(why) => {
                    write(&format!("REFUSED: {why}"));
                    None
                }
            }
        });
        let Some((seq, fns, alloc)) = prepared.flatten() else { return };
        // the graph's block sizes come from the containers (see `Mem::heap_size` for why nothing asks the heap)
        let live = crate::dump::LiveMem;
        // the same steps first as a dry pass inside a guard (`native_copy::dry_pass`): a panic of ours is a refusal
        let dry =
            nc::dry_pass("native copy: dry pass", &live, self.base, sink.settings, self.build.layout_name(), |c| {
                let _ = nc::run(c, payload, &mut |_| {});
            });
        if let Err(why) = dry {
            write(&format!("REFUSED: {why}"));
            return;
        }
        let ctx = nc::Ctx {
            fns,
            alloc: &alloc,
            mem: &live,
            base: self.base,
            set: sink.settings,
            build_name: self.build.layout_name(),
        };
        nc::run(&ctx, payload, &mut |line| write(line));
        let left = alloc.free_all();
        write(&format!("=== end native copy #{seq} (leftover blocks freed: {left}) ==="));
    }
}
