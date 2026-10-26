//! The runtime's side of shared tree, plant and rock ticks (`crate::assets` has the layout, the text, the checks and the build).
//! A child module of `runtime` so that it can use the runtime's private fields.

use super::Runtime;
use crate::assets::{self as as_, Apply, Order};
use crate::dump::{u64_at, LiveMem, Mem, PAYLOAD_SIZE};
use crate::engine::{self, AddArgs};
use crate::inject::Outcome;
use crate::link::Event;

type F1 = unsafe extern "system-unwind" fn(*mut u8) -> *mut u8;
type F2 = unsafe extern "system-unwind" fn(*mut u8, *const u8) -> *mut u8;
type F3 = unsafe extern "system-unwind" fn(*mut u8, *mut u8, *const u8) -> *mut u8;
type FResize = unsafe extern "system-unwind" fn(*mut u8, usize);
type FAssign = unsafe extern "system-unwind" fn(*mut u8, *const u8, usize);

/// The engine's element functions of the asset remover, at the build table's places, after the bytes the build search recorded at each
/// (it proved the whole function there) are compared with memory. The two builders' bytes are compared too: their code is the proof of
/// the offsets the module writes, so a game whose builders are not where and what the search found has no asset sharing.
#[derive(Clone, Copy)]
pub(super) struct AssetFns {
    ctor: F1,
    copy: F2,
    dtor: F1,
    insert: F3,
    resize: FResize,
    push: F2,
    assign: FAssign,
}

impl AssetFns {
    pub(super) fn at(base: usize, engine: &crate::build_table::EngineFns) -> Result<AssetFns, String> {
        let check = |rva: Option<u32>, name: &str| -> Result<usize, String> {
            let rva = crate::build_table::engine_rva(rva, name)?;
            let want = engine.prologue(name)?;
            let got = LiveMem
                .read(base + rva as usize, want.len())
                .ok_or_else(|| format!("{name} at {rva:#x} is not readable"))?;
            (got == want)
                .then_some(base + rva as usize)
                .ok_or_else(|| format!("{name} at {rva:#x} does not have the expected bytes"))
        };
        check(engine.asset_remover, "asset_remover")?;
        check(engine.asset_builder, "asset_builder")?;
        let ctor = check(engine.asset_con_ctor, "asset_con_ctor")?;
        let copy = check(engine.asset_con_copy, "asset_con_copy")?;
        let dtor = check(engine.asset_con_dtor, "asset_con_dtor")?;
        let insert = check(engine.asset_con_insert, "asset_con_insert")?;
        let resize = check(engine.asset_subcon_resize, "asset_subcon_resize")?;
        let push = check(engine.asset_elem_push, "asset_elem_push")?;
        let assign = check(engine.asset_models_assign, "asset_models_assign")?;
        // SAFETY: each address was just compared with the recorded bytes of the engine function of that signature (asset_sites.py: the
        // remover calls each with exactly these arguments).
        unsafe {
            Ok(AssetFns {
                ctor: core::mem::transmute::<usize, F1>(ctor),
                copy: core::mem::transmute::<usize, F2>(copy),
                dtor: core::mem::transmute::<usize, F1>(dtor),
                insert: core::mem::transmute::<usize, F3>(insert),
                resize: core::mem::transmute::<usize, FResize>(resize),
                push: core::mem::transmute::<usize, F2>(push),
                assign: core::mem::transmute::<usize, FAssign>(assign),
            })
        }
    }
}

impl as_::Engine for AssetFns {
    unsafe fn ctor(&self, at: *mut u8) {
        // SAFETY: as the trait's caller guarantees.
        unsafe { (self.ctor)(at) };
    }
    unsafe fn dtor(&self, at: *mut u8) {
        // SAFETY: as the trait's caller guarantees.
        unsafe { (self.dtor)(at) };
    }
    unsafe fn copy(&self, dst: *mut u8, src: *const u8) {
        // SAFETY: as the trait's caller guarantees.
        unsafe { (self.copy)(dst, src) };
    }
    unsafe fn insert(&self, vec: *mut u8, at: *mut u8, val: *const u8) {
        // SAFETY: as the trait's caller guarantees.
        unsafe { (self.insert)(vec, at, val) };
    }
    unsafe fn resize(&self, vec: *mut u8, n: usize) {
        // SAFETY: as the trait's caller guarantees.
        unsafe { (self.resize)(vec, n) };
    }
    unsafe fn push(&self, vec: *mut u8, val: *mut u8) {
        // SAFETY: as the trait's caller guarantees.
        unsafe { (self.push)(vec, val) };
    }
    unsafe fn assign(&self, vec: *mut u8, first: *const u8, n: usize) {
        // SAFETY: as the trait's caller guarantees.
        unsafe { (self.assign)(vec, first, n) };
    }
}

impl Runtime {
    /// Ticks can be shared: what terrain sharing needs (the Proposal functions to empty a command) and the asset functions.
    pub(super) fn assets_ready(&self) -> bool {
        self.terrain_ready() && AssetFns::at(self.base, &self.build.engine).is_ok()
    }

    /// Follows `terrain_share`: on only where the module can carry ticks (a game that cannot keeps the vegetation tools off, the mod's gate).
    pub(super) fn assets_share(&self, on: bool) {
        let on = on && self.assets_ready();
        if self.assets.set_share(on) {
            self.say(&format!(
                "assets: sharing tree, plant and rock ticks {}",
                if on {
                    "ON: an asset tool's own command is emptied at its apply, its groups go to the mod, every game builds them at the stamp"
                } else {
                    "off: an asset tool's command applies as it is"
                }
            ));
        }
    }

    /// `assets_apply {key, rm, o2n}`: the mod wrote `native_assets_in_<key>.txt` before the line. A text that cannot be used is kept as the
    /// order's refusal: the carrier that comes for it is emptied.
    pub(super) fn assets_arm(&self, key: u64, ids: Vec<u32>, o2n: u64, ttl: u64, now: u64) {
        let rm = ids.len() as u64;
        let ttl_ms = if ttl == 0 { as_::DEFAULT_TTL_MS } else { ttl.min(as_::MAX_TTL_MS) };
        let dir = super::lock(&self.inject_dir_path).clone();
        let (rec, sum) = match dir {
            None => (Err("no session folder".to_string()), 0),
            Some(d) => {
                let path = d.join(as_::in_file_name(key));
                let read = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()));
                let _ = std::fs::remove_file(&path);
                let sum = read.as_deref().map(as_::text_sum).unwrap_or(0);
                let rec = read.and_then(|t| as_::decode(&t)).and_then(|r| {
                    if r.remove.len() as u64 == rm && r.old2new.len() as u64 == o2n {
                        Ok(r)
                    } else {
                        Err(format!(
                            "the text removes {} with {} old2new entries, the order {rm} with {o2n}",
                            r.remove.len(),
                            r.old2new.len()
                        ))
                    }
                });
                (rec, sum)
            }
        };
        self.say(&format!(
            "assets: order {key} armed for a carrier that removes {rm} ({o2n} old2new){}",
            match &rec {
                Ok(r) => format!(": {} group(s) of {} model(s) to build", r.groups.len(), r.models()),
                Err(e) => format!(" (REFUSED, its carrier will be emptied: {e})"),
            }
        ));
        if let Some(old) = self.assets.arm(Order { key, ids, o2n, rec, sum, armed_ms: now, ttl_ms }) {
            self.inject.refuse(old.key, 0, Outcome::Nothing, format!("assets order {}: too many orders wait", old.key));
        }
    }

    /// The orders nobody used (the service thread). Not closed at `send_done` as an inject order is: during a pause the interface half
    /// sends the carrier, which the simulation applies only later (terrain orders wait the same way).
    pub(super) fn assets_expire(&self, now: u64) {
        for o in self.assets.expire(now) {
            self.inject.refuse(
                o.key,
                0,
                Outcome::Nothing,
                format!("assets order {}: no carrier came within {} ms: the tick is not applied here", o.key, o.ttl_ms),
            );
        }
    }

    /// The command list's `Add`: an asset tick from a site off the hold list (the hold list's tools, the bulldozer among them, are never
    /// asset tools) is the interface's own tick.
    pub(super) fn assets_note_add(&self, args: &AddArgs, hold_site: bool) {
        if !self.assets.share() || hold_site {
            return;
        }
        crate::log::guarded("assets: add", || {
            let kind_offset = self.build.kind_offset.unwrap_or(0) as usize;
            // SAFETY: only readable memory is read.
            let kind = unsafe { engine::command_kind(args.cmd, kind_offset) };
            if kind.is_none() || self.build.kind_by_name("build_proposal") != kind {
                return;
            }
            let Some(payload) = (unsafe { engine::payload_of(args.cmd) }) else { return };
            let Some(bytes) = LiveMem.read(payload, PAYLOAD_SIZE) else { return };
            // Only the interface's own commands reach `Add`: a game script's send from the simulation state never does (b31 to b41, see
            // `note_apply_orders`), so the game's reforestation (reforestation.script.tl, the same builder, playerInitiated false) and other
            // mods' simulation-state commands are never noted here and apply as they are on every game. The flag is required as well (the
            // brush's command had 1, trees1), so that no command a script marks as not the player's is ever taken for a tick.
            if bytes[as_::FLAGS + 2] == 1 && as_::shape_of(&bytes).is_some_and(|(rm, add)| rm + add > 0) {
                self.assets.note_tool(payload);
            }
        });
    }

    /// A command from a tool on the hold list that is an asset tick all the same: the construction builder placing one tree, plant or
    /// rock (owner's two-window test 2026-10-26: held 1..6 at site 0x51c3ac, one nameless auto-removable group, refused by the mod as a
    /// construction without a file). The rule is the tick's shape, not the tool: a player's `build_proposal` whose Proposal [`as_::capture`]
    /// reads whole as asset groups adding at least one. Such a command is not held: it goes on as a brush's tick does (noted here, captured
    /// and emptied at its apply, built at the stamp on every game). Anything else, or a group in another shape, is held as before.
    /// Only reads memory (called inside the add hook's guarded planning).
    pub(super) fn assets_tool_tick(&self, args: &AddArgs) -> bool {
        if !self.assets.share() {
            return false;
        }
        let kind_offset = self.build.kind_offset.unwrap_or(0) as usize;
        // SAFETY: only readable memory is read.
        let kind = unsafe { engine::command_kind(args.cmd, kind_offset) };
        if kind.is_none() || self.build.kind_by_name("build_proposal") != kind {
            return false;
        }
        let Some(payload) = (unsafe { engine::payload_of(args.cmd) }) else { return false };
        let Some(bytes) = LiveMem.read(payload, PAYLOAD_SIZE) else { return false };
        if bytes[as_::FLAGS + 2] != 1 {
            return false;
        }
        match as_::capture(&LiveMem, payload) {
            Ok(Some(rec)) if !rec.groups.is_empty() => {
                self.say(&format!(
                    "assets: a tool's command on the hold list is an asset tick ({} group(s) of {} model(s) added, {} removed): not held, it goes the brush's way",
                    rec.groups.len(),
                    rec.models(),
                    rec.remove.len()
                ));
                true
            }
            _ => false,
        }
    }

    /// The simulation is about to apply `cmd`: an asset tool's tick is captured and emptied, a carrier gets its groups. True when this
    /// module took the command. Calls the engine (an emptying, the element functions): never inside `log::guarded`.
    pub(super) fn assets_on_apply(&self, cmd: *mut u8) -> bool {
        if !self.assets.share() && !self.assets.pending() {
            return false;
        }
        let decided = crate::log::guarded("assets: apply", || {
            let kind_offset = self.build.kind_offset.unwrap_or(0) as usize;
            // SAFETY: only readable memory is read.
            let kind = unsafe { engine::command_kind(cmd, kind_offset) };
            if kind.is_none() || self.build.kind_by_name("build_proposal") != kind {
                return None;
            }
            let payload = unsafe { engine::payload_of(cmd) }?;
            let bytes = LiveMem.read(payload, PAYLOAD_SIZE)?;
            let shape = as_::shape_of(&bytes);
            let player = bytes[as_::FLAGS + 2] == 1;
            if shape.is_some_and(|(_, add)| add == 0) && player {
                // the mod writes its order and sends its carrier in the same breath: the order may still be in the file
                self.ingest_patch_order(self.now_ms());
            }
            let o2n = u64_at(&bytes, as_::OLD2NEW + 0x10) as usize;
            let ids = as_::removed_ids(&LiveMem, &bytes);
            Some((payload, self.assets.classify(payload, shape, &ids, o2n, player)))
        })
        .flatten();
        let Some((payload, apply)) = decided else { return false };
        match apply {
            Apply::Pass => false,
            Apply::Tool => {
                // the lab (FEVERLINK3_DUMP_COMMANDS=2): the tick as the tool made it, before it is emptied, its groups' subconstructions
                // and models followed (dump.rs)
                crate::log::guarded("assets: dump", || {
                    self.dump_payload(
                        "asset_tick",
                        payload,
                        None,
                        None,
                        crate::dump::Scope::Whole,
                        crate::dump::Budget::Applied,
                        None,
                    )
                });
                let shipped = crate::log::guarded("assets: capture", || self.assets_capture(payload)).unwrap_or(false);
                if !shipped {
                    self.inject.report(Event::AssetsRefused {
                        reason: "the tick could not be taken out of the tool's command: it is applied on no game"
                            .into(),
                    });
                }
                self.terrain_empty(payload, "an asset tool's own command (its groups go to every game at the stamp)");
                true
            }
            Apply::Carrier(order) => {
                // a carrier that removes nothing is an empty Proposal, as a ground paint carrier is: when another order waits at the same
                // time, which command is whose is not certain, so this one applies nothing (fail closed; never the wrong build). One that
                // removes is known by its exact ids.
                let other = (order.ids.is_empty() && (self.inject.pending() || self.terrain.pending())).then(|| {
                    "another order (inject or terrain) waits for an empty command too: which is whose is not certain".to_string()
                });
                let wrote = match (&order.rec, other) {
                    (_, Some(why)) => Err(why),
                    (Err(why), None) => Err(why.clone()),
                    (Ok(rec), None) => self.assets_build(payload, rec, order.sum),
                };
                match wrote {
                    Ok(()) => {
                        // the lab: the carrier as the module filled it, to set beside the tick's own dump
                        crate::log::guarded("assets: dump", || {
                            let (s, b) = (crate::dump::Scope::Whole, crate::dump::Budget::Applied);
                            self.dump_payload("asset_carrier", payload, None, None, s, b, None)
                        });
                        let r = order.rec.as_ref().expect("built");
                        self.say(&format!(
                            "assets: order {}: {} group(s) of {} model(s) built into its carrier (removing {}) and read back equal",
                            order.key,
                            r.groups.len(),
                            r.models(),
                            r.remove.len()
                        ));
                        self.inject.report(Event::Injected {
                            key: order.key,
                            nodes: 0,
                            segments: 0,
                            constructions: r.groups.len() as u64,
                        });
                    }
                    Err(why) => {
                        let outcome = if self.terrain_empty(payload, "a carrier whose tick was refused") {
                            Outcome::Nothing
                        } else {
                            Outcome::Untouched
                        };
                        self.inject.refuse(order.key, 0, outcome, format!("assets order {}: {why}", order.key));
                    }
                }
                true
            }
        }
    }

    /// The carrier's groups, built by the engine's functions after every check. Not inside `log::guarded` (the engine's allocator may
    /// throw; that exception must reach the game).
    fn assets_build(&self, payload: usize, rec: &as_::Record, sum: u64) -> Result<(), String> {
        crate::log::guarded("assets: carrier check", || as_::check_carrier(&LiveMem, payload, rec))
            .unwrap_or_else(|| Err("a panic while checking the carrier".into()))?;
        let fns = AssetFns::at(self.base, &self.build.engine)?;
        crate::log::calls_engine(
            "assets: the groups of a carrier (ConstructionEntity, resize, emplace_back, assign, copy)",
        );
        // SAFETY: the carrier the simulation is about to apply, checked just above; verified engine functions.
        unsafe { as_::build_into(&fns, &LiveMem, &super::cmd_patch::LivePoke, payload, rec) }?;
        // the lab's check on the originator: its own groups as the tool made them against this rebuild, word for word outside pointers
        if let Some(kept) = self.assets.take_images(sum) {
            crate::log::guarded("assets: image", || {
                let base = LiveMem.read(payload + as_::TO_ADD, 8).map(|b| u64_at(&b, 0) as usize).unwrap_or(0);
                let mut lines = Vec::new();
                for (i, img) in kept.iter().enumerate() {
                    let diff = match as_::image(&LiveMem, base + i * as_::ENTITY) {
                        Some(now) => as_::image_diff(img, &now, 24),
                        None => vec!["unreadable".into()],
                    };
                    lines.push(if diff.is_empty() {
                        format!("group {i} equal")
                    } else {
                        format!("group {i} differs at {}", diff.join(" "))
                    });
                }
                self.say(&format!(
                    "assets: the originator's own groups against the rebuild (words outside heap pointers): {}",
                    lines.join("; ")
                ));
            });
        }
        Ok(())
    }

    /// The originator's copy of a tick: `native_assets_out_<id>.txt` and the `assets` event. False when nothing was written.
    fn assets_capture(&self, payload: usize) -> bool {
        let rec = match as_::capture(&LiveMem, payload) {
            Ok(Some(r)) => r,
            Ok(None) => return false,
            Err(why) => {
                self.say(&format!("assets: a tool's tick is refused: {why}"));
                self.inject.report(Event::AssetsRefused { reason: why });
                return true; // said already
            }
        };
        let Some(dir) = super::lock(&self.inject_dir_path).clone() else { return false };
        let id = self.assets.next_out_id();
        let text = as_::encode(&rec);
        if text.len() > as_::MAX_TEXT {
            let why = format!("a tick of {} bytes of text is over the bound ({}): refused", text.len(), as_::MAX_TEXT);
            self.say(&format!("assets: a tool's tick is refused: {why}"));
            self.inject.report(Event::AssetsRefused { reason: why });
            return true; // said already
        }
        let path = dir.join(as_::out_file_name(id));
        let tmp = dir.join(format!("{}.tmp", as_::out_file_name(id)));
        if let Err(e) = std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, &path)) {
            self.say(&format!("assets: cannot write {}: {e}", path.display()));
            return false;
        }
        if id > 64 {
            let _ = std::fs::remove_file(dir.join(as_::out_file_name(id - 64)));
        }
        let base = LiveMem.read(payload + as_::TO_ADD, 8).map(|b| u64_at(&b, 0) as usize).unwrap_or(0);
        let images: Vec<_> =
            (0..rec.groups.len()).filter_map(|i| as_::image(&LiveMem, base + i * as_::ENTITY)).collect();
        if images.len() == rec.groups.len() {
            self.assets.keep_images(as_::text_sum(&text), images);
        }
        self.say(&format!(
            "assets: tick {id} taken from a tool's command: removes {} group(s), adds {} group(s) of {} model(s), old2new {}, flags {:?} ({} bytes of text)",
            rec.remove.len(),
            rec.groups.len(),
            rec.models(),
            rec.old2new.len(),
            rec.flags,
            text.len()
        ));
        self.inject.report(Event::Assets {
            id,
            remove: rec.remove.len() as u64,
            groups: rec.groups.len() as u64,
            models: rec.models() as u64,
            bytes: text.len() as u64,
        });
        true
    }
}
