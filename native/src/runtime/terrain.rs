//! The runtime's side of terrain height edits (`crate::terrain` has the layout, the text and the checks).
//! A child module of `runtime` so that it can use the runtime's private fields.

use super::Runtime;
use crate::dump::LiveMem;
use crate::engine::{self, AddArgs};
use crate::inject::{self as inj, Outcome};
use crate::link::Event;
use crate::native_copy as nc;
use crate::terrain::{self as tr, Apply, Header, Order};

impl Runtime {
    /// Terrain edits can be shared: the engine's Proposal functions (to empty a command) and this layout's build, as for `inject_next`.
    pub(super) fn terrain_ready(&self) -> bool {
        self.inject_ready()
    }

    pub(super) fn terrain_share(&self, on: bool) {
        if self.terrain.set_share(on) {
            self.say(&format!(
                "terrain: sharing height edits {}",
                if on {
                    "ON: a terrain tool's own command is emptied at its apply, its cells go to the mod, every game applies them at the stamp"
                } else {
                    "off: a terrain tool's command applies as it is"
                }
            ));
        }
    }

    /// `terrain_apply {key, x0, y0, w, h}`: the mod wrote `native_terrain_in_<key>.txt` before the line. A text that cannot be used is
    /// kept as the order's refusal: the carrier that comes for it is emptied, never applied with the script's own (flat) cells.
    pub(super) fn terrain_arm(&self, key: u64, head: Header, ttl: u64, now: u64) {
        let ttl_ms = if ttl == 0 { tr::DEFAULT_TTL_MS } else { ttl.min(tr::MAX_TTL_MS) };
        let dir = super::lock(&self.inject_dir_path).clone();
        let cells = match dir {
            None => Err("no session folder".to_string()),
            Some(d) => {
                let path = d.join(tr::in_file_name(key));
                let read = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()));
                let _ = std::fs::remove_file(&path);
                read.and_then(|t| tr::decode(&t)).and_then(|(h, c)| {
                    if h == head {
                        Ok(c)
                    } else {
                        Err(format!("the text's grid {h:?} is not the order's {head:?}"))
                    }
                })
            }
        };
        self.say(&format!(
            "terrain: order {key} armed for a carrier of {}x{} cells at {},{}{}",
            head.w,
            head.h,
            head.x0,
            head.y0,
            cells.as_ref().err().map(|e| format!(" (REFUSED, its carrier will be emptied: {e})")).unwrap_or_default()
        ));
        if let Some(old) = self.terrain.arm(Order { key, head, cells, armed_ms: now, ttl_ms }) {
            self.inject.refuse(
                old.key,
                0,
                Outcome::NoCommand,
                format!("terrain order {}: too many orders wait", old.key),
            );
        }
    }

    /// The orders nobody used (the service thread).
    pub(super) fn terrain_expire(&self, now: u64) {
        for o in self.terrain.expire(now) {
            self.inject.refuse(
                o.key,
                0,
                Outcome::NoCommand,
                format!("terrain order {}: no carrier came within {} ms", o.key, o.ttl_ms),
            );
        }
    }

    /// The command list's `Add`: a command with a height grid there is the interface's (a terrain tool, or an interface script).
    pub(super) fn terrain_note_add(&self, args: &AddArgs) {
        if !self.terrain.share() {
            return;
        }
        crate::log::guarded("terrain: add", || {
            let kind_offset = self.build.kind_offset.unwrap_or(0) as usize;
            // SAFETY: only readable memory is read.
            let kind = unsafe { engine::command_kind(args.cmd, kind_offset) };
            if kind.is_none() || self.build.kind_by_name("build_proposal") != kind {
                return;
            }
            let Some(payload) = (unsafe { engine::payload_of(args.cmd) }) else { return };
            if let Ok(Some(_)) = tr::find_grid(&LiveMem, payload) {
                self.terrain.note_tool(payload);
            }
        });
    }

    /// The simulation is about to apply `cmd`: a terrain tool's command is captured and emptied, a carrier gets its cells, an orphan
    /// is emptied. True when this module took the command (no other order may then use it). Calls the engine (an emptying): never
    /// inside `log::guarded`.
    pub(super) fn terrain_on_apply(&self, cmd: *mut u8) -> bool {
        if !self.terrain.share() && !self.terrain.pending() {
            return false;
        }
        let decided = crate::log::guarded("terrain: apply", || {
            let kind_offset = self.build.kind_offset.unwrap_or(0) as usize;
            // SAFETY: only readable memory is read.
            let kind = unsafe { engine::command_kind(cmd, kind_offset) };
            if kind.is_none() || self.build.kind_by_name("build_proposal") != kind {
                return None;
            }
            let payload = unsafe { engine::payload_of(cmd) }?;
            let grid = match tr::find_grid(&LiveMem, payload) {
                Ok(g) => g.map(|g| g.head),
                Err(why) => {
                    // a terrain member this module cannot read whole: only a tool's command or a carrier would matter, and both
                    // have exactly one grid; said once per command, applied as it is
                    self.say(&format!("terrain: a build command's terrain member is not carried: {why}"));
                    return None;
                }
            };
            grid?;
            // the mod writes its order and sends its carrier in the same breath: look at the file now (an older order may be
            // waiting too, so one that waits is no reason to skip the file)
            self.ingest_patch_order(self.now_ms());
            // SAFETY: only readable memory is read.
            let player = unsafe { engine::build_flags(payload) }.is_some_and(|f| f.player_initiated);
            Some((payload, grid, self.terrain.classify(payload, grid, player)))
        })
        .flatten();
        let Some((payload, grid, apply)) = decided else { return false };
        match apply {
            Apply::Pass => false,
            Apply::Tool => {
                let shipped =
                    crate::log::guarded("terrain: capture", || self.terrain_capture(payload)).unwrap_or(false);
                if !shipped {
                    self.inject.report(Event::TerrainRefused {
                        reason:
                            "the terrain edit could not be taken out of the tool's command: it is applied on no game"
                                .into(),
                    });
                }
                self.terrain_empty(payload, "the tool's own command (its cells go to every game at the stamp)");
                true
            }
            Apply::Carrier(order) => {
                let head = grid.expect("a carrier has a grid");
                let wrote = crate::log::guarded("terrain: write", || match &order.cells {
                    Ok(cells) => tr::write_cells(&LiveMem, &super::cmd_patch::LivePoke, payload, head, cells),
                    Err(why) => Err(why.clone()),
                })
                .unwrap_or_else(|| Err("a panic while writing the cells".into()));
                match wrote {
                    Ok(()) => {
                        self.say(&format!(
                            "terrain: order {}: {}x{} cells at {},{} written into its carrier and read back equal",
                            order.key, head.w, head.h, head.x0, head.y0
                        ));
                        self.inject.report(Event::Injected { key: order.key, nodes: 0, segments: 0, constructions: 0 });
                    }
                    Err(why) => {
                        let outcome = if self.terrain_empty(payload, "a carrier whose cells were refused") {
                            Outcome::Nothing
                        } else {
                            Outcome::Untouched
                        };
                        self.inject.refuse(order.key, 0, outcome, format!("terrain order {}: {why}", order.key));
                    }
                }
                true
            }
            Apply::Orphan => {
                self.terrain_empty(payload, "a player's script terrain edit without its order");
                self.inject.report(Event::TerrainRefused {
                    reason: "a script's terrain edit came without its order while edits are shared: emptied, applied on no game".into(),
                });
                true
            }
        }
    }

    /// The originator's copy of a tool's edit: `native_terrain_out_<id>.txt` and the `terrain` event. False when nothing was written.
    fn terrain_capture(&self, payload: usize) -> bool {
        let (head, cells) = match tr::capture(&LiveMem, payload) {
            Ok(Some(c)) => c,
            Ok(None) => return false,
            Err(why) => {
                self.say(&format!("terrain: a tool's edit is refused: {why}"));
                self.inject.report(Event::TerrainRefused { reason: why });
                return true; // said already
            }
        };
        let Some(dir) = super::lock(&self.inject_dir_path).clone() else { return false };
        let id = self.terrain.next_out_id();
        let text = tr::encode(head, &cells);
        let path = dir.join(tr::out_file_name(id));
        let tmp = dir.join(format!("{}.tmp", tr::out_file_name(id)));
        if let Err(e) = std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, &path)) {
            self.say(&format!("terrain: cannot write {}: {e}", path.display()));
            return false;
        }
        if id > 64 {
            let _ = std::fs::remove_file(dir.join(tr::out_file_name(id - 64)));
        }
        self.say(&format!(
            "terrain: edit {id} taken from a tool's command: {}x{} = {} cells at {},{} ({} bytes of text)",
            head.w,
            head.h,
            head.cells(),
            head.x0,
            head.y0,
            text.len()
        ));
        self.inject.report(Event::Terrain {
            id,
            x0: head.x0,
            y0: head.y0,
            w: head.w,
            h: head.h,
            bytes: text.len() as u64,
        });
        true
    }

    /// Empties the command's Proposal with the engine's own functions (`inject::neutralise`). False when it could not.
    fn terrain_empty(&self, payload: usize, what: &str) -> bool {
        let sane = crate::log::guarded("terrain: check", || inj::is_proposal(&LiveMem, payload))
            .unwrap_or_else(|| Err("a panic".into()));
        let fns = nc::ProposalFns::at(self.base, &self.build.engine);
        match (sane, fns) {
            (Ok(_), Ok(fns)) => {
                // SAFETY: the payload of the command the simulation is about to apply, accepted by `is_proposal`; verified engine functions.
                unsafe { inj::neutralise(&fns, payload) };
                self.say(&format!("terrain: {what}: emptied, it applies nothing here"));
                true
            }
            (Err(e), _) | (_, Err(e)) => {
                self.fault(
                    "terrain",
                    crate::failsafe::Action::Repair,
                    format!("{what} could not be emptied ({e}): this game applied it alone"),
                );
                false
            }
        }
    }
}
