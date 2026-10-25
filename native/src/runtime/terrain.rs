//! The runtime's side of terrain edits, heights and ground paint (`crate::terrain` has the layout, the text and the checks).
//! A child module of `runtime` so that it can use the runtime's private fields.

use super::Runtime;
use crate::dump::{LiveMem, Mem};
use crate::engine::{self, AddArgs};
use crate::inject::{self as inj, Outcome};
use crate::link::Event;
use crate::native_copy as nc;
use crate::rebuild::EngineAlloc;
use crate::terrain::{self as tr, Apply, Header, Order, Seen};

/// One line for the log: what an edit changes.
fn edit_text(e: &tr::Edit) -> String {
    let mut parts = Vec::new();
    if let Some((h, _)) = &e.height {
        parts.push(format!("heights {}x{} = {} cells at {},{}", h.w, h.h, h.cells(), h.x0, h.y0));
    }
    if let Some(p) = &e.paint {
        parts.push(format!(
            "ground paint {}x{} = {} cells at {},{}",
            p.head.w,
            p.head.h,
            p.head.cells(),
            p.head.x0,
            p.head.y0
        ));
    }
    parts.join(" and ")
}

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
        let edit = match dir {
            None => Err("no session folder".to_string()),
            Some(d) => {
                let path = d.join(tr::in_file_name(key));
                let read = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()));
                let _ = std::fs::remove_file(&path);
                read.and_then(|t| tr::decode(&t)).and_then(|e| {
                    if e.carrier_head() == head {
                        Ok(e)
                    } else {
                        Err(format!("the text's height grid {:?} is not the order's {head:?}", e.carrier_head()))
                    }
                })
            }
        };
        self.say(&format!(
            "terrain: order {key} armed for a carrier {}{}",
            if head.w == 0 {
                "without a height grid (paint only)".to_string()
            } else {
                format!("of {}x{} cells at {},{}", head.w, head.h, head.x0, head.y0)
            },
            edit.as_ref().err().map(|e| format!(" (REFUSED, its carrier will be emptied: {e})")).unwrap_or_default()
        ));
        if let Some(old) = self.terrain.arm(Order { key, head, edit, armed_ms: now, ttl_ms }) {
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

    /// The command list's `Add`: a command with a terrain edit there is the interface's (a terrain or paint tool, or an interface script).
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
            let block = LiveMem.read(payload + tr::TERRAIN_OFF, tr::TERRAIN_LEN);
            if block.is_some_and(|b| b.iter().any(|&x| x != 0)) {
                self.terrain.note_tool(payload);
            }
            // what a tool's command carries, for the lab (bounded): its terrain member, or every non-zero word when that is zero (the
            // tree, plant and rock brushes: constructions)
            if self.terrain.probes.load(std::sync::atomic::Ordering::Relaxed) < 40 {
                let text = tr::probe_text(&LiveMem, payload, 4096).or_else(|| {
                    let b = LiveMem.read(payload, 0x3e0)?;
                    let words: Vec<String> = (0..0x3e0)
                        .step_by(8)
                        .filter(|&o| crate::dump::u64_at(&b, o) != 0)
                        .map(|o| format!("+{o:#05x}={:016x}", crate::dump::u64_at(&b, o)))
                        .collect();
                    Some(format!("terrain member zero; non-zero words: {}", words.join(" ")))
                });
                if let Some(text) = text {
                    let n = self.terrain.probes.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    // SAFETY: only readable memory is read.
                    let player = unsafe { engine::build_flags(payload) }.map(|f| f.player_initiated);
                    self.say(&format!("terrain probe {n} (a command at Add, playerInitiated {player:?}): {text}"));
                }
            }
        });
    }

    /// The simulation is about to apply `cmd`: a terrain tool's command is captured and emptied, a carrier gets its edit, an orphan is
    /// emptied. True when this module took the command (no other order may then use it). Calls the engine (an emptying, the game's
    /// allocator): never inside `log::guarded`.
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
            let block = LiveMem.read(payload + tr::TERRAIN_OFF, tr::TERRAIN_LEN)?;
            let zero = block.iter().all(|&b| b == 0);
            let height = match tr::find_grid(&LiveMem, payload) {
                Ok(g) => g.map(|g| g.head),
                Err(_) => None,
            };
            // SAFETY: only readable memory is read.
            let player = unsafe { engine::build_flags(payload) }.is_some_and(|f| f.player_initiated);
            let empty = zero && crate::portable::proposal_is_empty(&LiveMem, payload) == Some(true);
            if zero && !(empty && player && self.terrain.pending()) {
                return None; // no terrain edit, and not a paint carrier: nothing of this module's
            }
            // the mod writes its order and sends its carrier in the same breath: look at the file now (an older order may be
            // waiting too, so one that waits is no reason to skip the file)
            self.ingest_patch_order(self.now_ms());
            let paint = !zero && block[tr::PAINT_OFF..].iter().any(|&b| b != 0) || (!zero && height.is_none());
            let seen = Seen { height, paint, empty, player };
            Some((payload, seen, self.terrain.classify(payload, seen)))
        })
        .flatten();
        let Some((payload, seen, apply)) = decided else { return false };
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
                self.terrain_empty(payload, "the tool's own command (its edit goes to every game at the stamp)");
                true
            }
            Apply::Carrier(order) => {
                let wrote = match &order.edit {
                    Ok(edit) => self.terrain_write(payload, seen, edit),
                    Err(why) => Err(why.clone()),
                };
                match wrote {
                    Ok(()) => {
                        self.say(&format!(
                            "terrain: order {}: {} written into its carrier and read back equal",
                            order.key,
                            order.edit.as_ref().map(edit_text).unwrap_or_default()
                        ));
                        self.inject.report(Event::Injected { key: order.key, nodes: 0, segments: 0, constructions: 0 });
                    }
                    Err(why) => {
                        let outcome = if self.terrain_empty(payload, "a carrier whose edit was refused") {
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

    /// Writes an order's edit into its carrier: the height cells in place, the paint in blocks of the game's allocator. Not inside
    /// `log::guarded` (the allocator is the game's `operator new`, whose exception must reach the game).
    fn terrain_write(&self, payload: usize, seen: Seen, edit: &tr::Edit) -> Result<(), String> {
        if let Some((head, cells)) = &edit.height {
            if seen.height != Some(*head) {
                return Err(format!("the carrier's height grid {:?} is not the edit's {head:?}", seen.height));
            }
            crate::log::guarded("terrain: write", || {
                tr::write_cells(&LiveMem, &super::cmd_patch::LivePoke, payload, *head, cells)
            })
            .unwrap_or_else(|| Err("a panic while writing the cells".into()))?;
        }
        if let Some(paint) = &edit.paint {
            let alloc = EngineAlloc::at(self.base, &self.build.engine)?;
            crate::log::calls_engine("terrain: the paint grids of a carrier (operator new)");
            tr::write_paint(&LiveMem, &super::cmd_patch::LivePoke, &alloc, payload, paint)?;
        }
        Ok(())
    }

    /// The originator's copy of a tool's edit: `native_terrain_out_<id>.txt` and the `terrain` event. False when nothing was written.
    fn terrain_capture(&self, payload: usize) -> bool {
        let edit = match tr::capture(&LiveMem, payload) {
            Ok(Some(e)) => e,
            Ok(None) => return false,
            Err(why) => {
                self.say(&format!("terrain: a tool's edit is refused: {why}"));
                self.inject.report(Event::TerrainRefused { reason: why });
                return true; // said already
            }
        };
        let Some(dir) = super::lock(&self.inject_dir_path).clone() else { return false };
        let id = self.terrain.next_out_id();
        let text = tr::encode(&edit);
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
            "terrain: edit {id} taken from a tool's command: {} ({} bytes of text)",
            edit_text(&edit),
            text.len()
        ));
        let head = edit.carrier_head();
        self.inject.report(Event::Terrain {
            id,
            x0: head.x0,
            y0: head.y0,
            w: head.w,
            h: head.h,
            paint: edit.paint.is_some(),
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
