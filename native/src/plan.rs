//! Which hooks to install, and why not (`docs/NATIVE_LINK.md` section 2).
//!
//! Everything the module may do to the game's code is off unless an environment variable switches it on, and each
//! piece is checked against the bytes actually in memory before anything is written. This module is the pure part
//! of that decision: it takes the switches, the build's row of the build table and a view of the image, and says
//! which steps to carry out and which pieces are skipped, with the reason. The unsafe part (`hooks.rs`) only
//! executes the steps.
//!
//! The switches, in the order to try them in the game:
//!
//! 1. `FEVERLINK3_HOOKS=1`: the read-only counters (a hook on the simulation's command application that counts
//!    and passes everything on).
//! 2. `FEVERLINK3_HOOKS=1` and `FEVERLINK3_HOLD=1`: also the hold/release hook on the command list. It holds
//!    nothing until the mod (or a person) writes `hold_on` to the control file.
//! 3. `FEVERLINK3_POOLPATCH=1`: the single-worker script pool. A patch, not a hook: independent of the others.
//!
//! 4. `FEVERLINK3_LOG_TOWN_SEED=1`: a read-only hook that logs the town developer's random seed on every call
//!    of the function that seeds its task. Independent of the others; it never changes a result.
//!
//!    `FEVERLINK3_FORCE_TOWN_SEED=<number>` (decimal or `0x` hex, 32 bit): the same hook also replaces the seed
//!    argument with that value before calling the original. An experiment switch, not run in the game. A value
//!    that does not parse is ignored (and logged); it then changes nothing.
//!
//! 5. `FEVERLINK3_LOG_GROWTH=1`: a read-only hook on `TownDeveloper::Develop` (and one on the `TownUpdateSize`
//!    handler to read the game time) that logs each growth call. `FEVERLINK3_GROWTH_SEED_MODE=derive` (with
//!    `..._AFTER`, `..._AFTER_GAMETIME`, `..._QUANTUM_MS`) additionally replaces the random seed `Develop` reads.
//!    See `growth.rs`. An experiment switch, not run in the game.
//!

//! 6. `FEVERLINK3_LUA_SEED=fixed`: pin the seed of Lua's string hash to a constant (`build_table::LUA_SEED`), so the
//!    iteration order of `pairs` over string keys is the same in every process. A patch, independent of the others.
//!    Any other value, or none, leaves the game's own per-process seed.
//!
//! Anything but the exact value `1` (after trimming blanks; `fixed` for the seed) is off.

use crate::build_table::{BuildAddrs, PatchSite, Rva};
use crate::growth::Settings;

pub const HOOKS_VAR: &str = "FEVERLINK3_HOOKS";
pub const HOLD_VAR: &str = "FEVERLINK3_HOLD";
pub const POOLPATCH_VAR: &str = "FEVERLINK3_POOLPATCH";
pub const TOWN_SEED_VAR: &str = "FEVERLINK3_LOG_TOWN_SEED";
pub const FORCE_TOWN_SEED_VAR: &str = "FEVERLINK3_FORCE_TOWN_SEED";

/// Parses the forced seed: decimal or `0x` hex, must fit 32 bits; blanks around it are ignored.
pub fn parse_seed(text: &str) -> Result<u32, String> {
    let t = text.trim();
    let parsed = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => t.parse::<u64>(),
    };
    match parsed {
        Ok(v) => u32::try_from(v).map_err(|_| format!("{t:?} does not fit 32 bits")),
        Err(_) => Err(format!("{t:?} is not a decimal or 0x-hex number")),
    }
}

/// What `FEVERLINK3_FORCE_TOWN_SEED` says: `Ok(None)` when unset or blank, `Err(reason)` when it does not parse.
pub fn forced_seed(get: impl Fn(&str) -> Option<String>) -> Result<Option<u32>, String> {
    match get(FORCE_TOWN_SEED_VAR) {
        Some(v) if !v.trim().is_empty() => parse_seed(&v).map(Some),
        _ => Ok(None),
    }
}
pub const LUA_SEED_VAR: &str = "FEVERLINK3_LUA_SEED";
/// `1` asks the mod's Lua probe (and the module's log) to record the string-hash seed's effect. Observation only.
pub const LUA_SEED_LOG_VAR: &str = "FEVERLINK3_LUA_SEED_LOG";

/// The switches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HookConfig {
    pub hooks: bool,
    pub hold: bool,
    pub poolpatch: bool,
    /// Log the town developer's random seed (read-only; independent of the other switches).
    pub town_seed: bool,
    /// Replace the seed argument with this value (implies the hook; an invalid value is ignored).
    pub force_seed: Option<u32>,
    /// The town-growth log and seed experiment (`growth.rs`).
    pub growth: Settings,
    pub luaseed: bool,
}

impl HookConfig {
    /// Reads the switches through `get` (the environment in the module, a map in the tests).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> HookConfig {
        let on = |name: &str| get(name).is_some_and(|v| v.trim() == "1");
        let force_seed = forced_seed(&get).ok().flatten();
        HookConfig {
            hooks: on(HOOKS_VAR),
            hold: on(HOLD_VAR),
            poolpatch: on(POOLPATCH_VAR),
            town_seed: on(TOWN_SEED_VAR) || force_seed.is_some(),
            force_seed,
            growth: Some(Settings::from_lookup(&get).0).filter(Settings::active).unwrap_or_default(),
            luaseed: get(LUA_SEED_VAR).is_some_and(|v| v.trim() == "fixed"),
        }
    }

    /// True when the module may touch the game's code at all.
    pub fn any(&self) -> bool {
        self.hooks || self.poolpatch || self.town_seed || self.growth.active() || self.luaseed
    }

    pub fn describe(&self) -> String {
        let f = |b: bool| if b { "on" } else { "off" };
        format!(
            "hooks={} hold={} poolpatch={} luaseed={} town_seed={}{}{}",
            f(self.hooks),
            f(self.hold),
            f(self.poolpatch),
            f(self.luaseed),
            f(self.town_seed),
            self.force_seed.map(|v| format!(" force_seed={v:#010x}")).unwrap_or_default(),
            self.growth.describe()
        )
    }
}

/// A view of the game's image by relative virtual address.
pub trait Image {
    /// `len` bytes at `rva`, or `None` when they are not all readable.
    fn read(&self, rva: Rva, len: usize) -> Option<Vec<u8>>;
}

/// An image held in memory: `bytes[i]` is the byte at `base + i`. For the tests.
#[derive(Debug, Clone)]
pub struct BytesImage {
    pub base: Rva,
    pub bytes: Vec<u8>,
}

impl BytesImage {
    pub fn zeroed(base: Rva, len: usize) -> BytesImage {
        BytesImage { base, bytes: vec![0; len] }
    }

    pub fn write(&mut self, rva: Rva, data: &[u8]) {
        let at = (rva - self.base) as usize;
        self.bytes[at..at + data.len()].copy_from_slice(data);
    }
}

impl Image for BytesImage {
    fn read(&self, rva: Rva, len: usize) -> Option<Vec<u8>> {
        let at = rva.checked_sub(self.base)? as usize;
        self.bytes.get(at..at.checked_add(len)?).map(<[u8]>::to_vec)
    }
}

/// What a piece of the module does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// The read-only counters.
    Counters,
    /// The hold/release hook.
    Hold,
    /// The single-worker script pool.
    PoolPatch,
    /// The read-only log of the town developer's seed.
    TownSeed,
    /// The log (and seed experiment) on `TownDeveloper::Develop`.
    Growth,
    /// The read-only hook on the `TownUpdateSize` handler that reads the game time for the growth hook.
    GrowthCmd,
    /// The read-only wrap of the octree candidate gather (deep growth log).
    GrowthCands,
    /// The read-only wrap of the country-edge selection (deep growth log).
    GrowthEdges,
    /// The call-site redirect before the candidate sort (deep growth log).
    GrowthSort,
    /// The call-site redirect before the country-edge pick (deep growth log).
    GrowthPick,
    /// The pinned Lua string-hash seed.
    LuaSeed,
}

impl Feature {
    pub fn name(self) -> &'static str {
        match self {
            Feature::Counters => "counters",
            Feature::Hold => "hold",
            Feature::PoolPatch => "poolpatch",
            Feature::TownSeed => "town_seed",
            Feature::Growth => "town_develop",
            Feature::GrowthCmd => "town_update_size",
            Feature::GrowthCands => "town_candidates",
            Feature::GrowthEdges => "town_edges",
            Feature::GrowthSort => "town_sort",
            Feature::GrowthPick => "town_pick",
            Feature::LuaSeed => "luaseed",
        }
    }
}

/// One thing to do to the game's code, already checked against the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Detour {
        feature: Feature,
        name: &'static str,
        rva: Rva,
        expected: &'static [u8],
    },
    Patch {
        feature: Feature,
        name: &'static str,
        rva: Rva,
        expected: &'static [u8],
        replacement: &'static [u8],
    },
    /// The displacement of a `call rel32` at `rva` is redirected to a thunk of the module (`expected` is the whole
    /// 5-byte instruction); the hook calls `callee` itself.
    CallPatch {
        feature: Feature,
        name: &'static str,
        rva: Rva,
        expected: &'static [u8],
        callee: Rva,
    },
}

impl Step {
    pub fn name(&self) -> &'static str {
        match self {
            Step::Detour { name, .. } | Step::Patch { name, .. } | Step::CallPatch { name, .. } => name,
        }
    }

    pub fn feature(&self) -> Feature {
        match self {
            Step::Detour { feature, .. } | Step::Patch { feature, .. } | Step::CallPatch { feature, .. } => *feature,
        }
    }
}

/// A piece that was asked for and will not be installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub feature: Feature,
    pub name: &'static str,
    pub reason: String,
}

/// The engine helpers of the hold path, checked against the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineAddrs {
    pub command_move: Rva,
    pub command_destroy: Rva,
    pub handle_destroy: Rva,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// In the order to carry them out.
    pub steps: Vec<Step>,
    pub skipped: Vec<Skip>,
    /// Remarks that are not a skipped piece (a switch that does nothing on its own).
    pub notes: Vec<String>,
    /// The command-kind byte offset, when the counters or the hold hook are planned.
    pub kind_offset: Option<u32>,
    /// The engine helpers, when the hold hook is planned.
    pub engine: Option<EngineAddrs>,
    /// The `GameTime` getter, when the growth hook can read the game time (its prologue matched).
    pub game_time_getter: Option<Rva>,
}

impl Plan {
    pub fn has(&self, feature: Feature) -> bool {
        self.steps.iter().any(|s| s.feature() == feature)
    }

    /// The lines for `native.log` that say what was decided.
    pub fn report(&self) -> Vec<String> {
        let mut out = Vec::new();
        for s in &self.steps {
            out.push(match s {
                Step::Detour { name, rva, expected, .. } => {
                    format!("hooks: {name}: {} bytes at rva {rva:#x} match the build table", expected.len())
                }
                Step::Patch { name, rva, expected, .. } | Step::CallPatch { name, rva, expected, .. } => {
                    format!("hooks: {name}: {} bytes at rva {rva:#x} match the build table", expected.len())
                }
            });
        }
        for s in &self.skipped {
            out.push(format!("hooks: SKIPPED {} ({}): {}", s.name, s.feature.name(), s.reason));
        }
        for n in &self.notes {
            out.push(format!("hooks: note: {n}"));
        }
        out
    }
}

/// `01 02 ff ..`, at most 24 bytes shown.
pub fn hex(bytes: &[u8]) -> String {
    let mut s: Vec<String> = bytes.iter().take(24).map(|b| format!("{b:02x}")).collect();
    if bytes.len() > 24 {
        s.push("..".into());
    }
    s.join(" ")
}

/// Compares the image with the bytes the table expects; the error says what was found.
fn verify(image: &dyn Image, rva: Rva, expected: &[u8]) -> Result<(), String> {
    match image.read(rva, expected.len()) {
        None => Err(format!("{} bytes at rva {rva:#x} are not readable", expected.len())),
        Some(found) if found == expected => Ok(()),
        Some(found) => {
            Err(format!("bytes at rva {rva:#x} are {} but the table expects {}", hex(&found), hex(expected)))
        }
    }
}

/// Decides what to install on this build. Nothing is written; every piece is checked against `image` first.
pub fn plan(build: &BuildAddrs, cfg: HookConfig, image: &dyn Image) -> Plan {
    let mut p = Plan::default();
    if cfg.hold && !cfg.hooks {
        p.notes.push(format!("{HOLD_VAR}=1 does nothing without {HOOKS_VAR}=1"));
    }
    if cfg.hooks {
        plan_counters(build, image, &mut p);
        if cfg.hold {
            plan_hold(build, image, &mut p);
        }
    }
    if cfg.town_seed {
        plan_town_seed(build, image, &mut p);
    }
    if cfg.growth.active() {
        plan_growth(build, image, &mut p);
        if cfg.growth.deep {
            plan_growth_deep(build, image, &mut p);
            plan_growth_calls(build, image, &mut p);
        }
    }
    if cfg.poolpatch {
        plan_patch(Feature::PoolPatch, "script_pool", &build.script_pool, image, &mut p);
    }
    if cfg.luaseed {
        plan_patch(Feature::LuaSeed, "lua_makeseed", &build.lua_makeseed, image, &mut p);
    }
    p
}

/// A patch site: a step when the build has it and the image holds the expected bytes, a skip with the reason otherwise.
fn plan_patch(feature: Feature, name: &'static str, site: &Option<PatchSite>, image: &dyn Image, p: &mut Plan) {
    match site {
        None => {
            p.skipped.push(Skip { feature, name, reason: format!("the build table has no {name} site for this build") })
        }
        Some(site) => match verify(image, site.rva, site.expected) {
            Ok(()) => p.steps.push(Step::Patch {
                feature,
                name,
                rva: site.rva,
                expected: site.expected,
                replacement: site.replacement,
            }),
            Err(reason) => p.skipped.push(Skip { feature, name, reason }),
        },
    }
}

fn plan_counters(build: &BuildAddrs, image: &dyn Image, p: &mut Plan) {
    let skip = |p: &mut Plan, reason: String| {
        p.skipped.push(Skip { feature: Feature::Counters, name: "apply_command", reason });
    };
    let Some(site) = &build.apply_command else {
        return skip(p, "the build table has no apply_command site for this build".into());
    };
    let Some(kind_offset) = build.kind_offset else {
        return skip(p, "the build table has no command-kind offset for this build".into());
    };
    match verify(image, site.rva, site.prologue) {
        Ok(()) => {
            p.steps.push(Step::Detour {
                feature: Feature::Counters,
                name: "apply_command",
                rva: site.rva,
                expected: site.prologue,
            });
            p.kind_offset = Some(kind_offset);
        }
        Err(reason) => skip(p, reason),
    }
}

fn plan_town_seed(build: &BuildAddrs, image: &dyn Image, p: &mut Plan) {
    let skip = |p: &mut Plan, reason: String| {
        p.skipped.push(Skip { feature: Feature::TownSeed, name: "town_seed", reason });
    };
    let Some(site) = &build.town_seed else {
        return skip(p, "the build table has no town_seed site for this build".into());
    };
    match verify(image, site.rva, site.prologue) {
        Ok(()) => p.steps.push(Step::Detour {
            feature: Feature::TownSeed,
            name: "town_seed",
            rva: site.rva,
            expected: site.prologue,
        }),
        Err(reason) => skip(p, reason),
    }
}

/// `Develop` is the piece; the `TownUpdateSize` handler hook and the `GameTime` getter are added only when both match,
/// because they exist to give the log (and the seed experiment) the game time. Without them the log still works.
fn plan_growth(build: &BuildAddrs, image: &dyn Image, p: &mut Plan) {
    let skip = |p: &mut Plan, feature: Feature, name: &'static str, reason: String| {
        p.skipped.push(Skip { feature, name, reason });
    };
    let Some(site) = &build.town_develop else {
        return skip(
            p,
            Feature::Growth,
            "town_develop",
            "the build table has no town_develop site for this build".into(),
        );
    };
    if let Err(reason) = verify(image, site.rva, site.prologue) {
        return skip(p, Feature::Growth, "town_develop", reason);
    }
    p.steps.push(Step::Detour {
        feature: Feature::Growth,
        name: "town_develop",
        rva: site.rva,
        expected: site.prologue,
    });
    let (Some(cmd), Some(getter)) = (&build.town_update_size, &build.game_time_getter) else {
        return skip(
            p,
            Feature::GrowthCmd,
            "town_update_size",
            "the build table lacks the TownUpdateSize handler or the GameTime getter: no game time".into(),
        );
    };
    let checked = verify(image, cmd.rva, cmd.prologue).and_then(|()| verify(image, getter.rva, getter.prologue));
    match checked {
        Ok(()) => {
            p.steps.push(Step::Detour {
                feature: Feature::GrowthCmd,
                name: "town_update_size",
                rva: cmd.rva,
                expected: cmd.prologue,
            });
            p.game_time_getter = Some(getter.rva);
        }
        Err(reason) => skip(p, Feature::GrowthCmd, "town_update_size", format!("{reason}: no game time")),
    }
}

/// The two read-only wraps of the deep log. Each is checked on its own; a missing one costs only its own lines.
fn plan_growth_deep(build: &BuildAddrs, image: &dyn Image, p: &mut Plan) {
    for (feature, name, site) in [
        (Feature::GrowthCands, "town_candidates", &build.town_candidates),
        (Feature::GrowthEdges, "town_edges", &build.town_edges),
    ] {
        match site {
            None => {
                p.skipped.push(Skip { feature, name, reason: "the build table has no such site for this build".into() })
            }
            Some(site) => match verify(image, site.rva, site.prologue) {
                Ok(()) => p.steps.push(Step::Detour { feature, name, rva: site.rva, expected: site.prologue }),
                Err(reason) => p.skipped.push(Skip { feature, name, reason }),
            },
        }
    }
}

/// The two call-site redirects of the deep log: the call bytes and the callee's prologue must both match.
fn plan_growth_calls(build: &BuildAddrs, image: &dyn Image, p: &mut Plan) {
    for (feature, name, site) in [
        (Feature::GrowthSort, "town_sort", &build.town_sort_call),
        (Feature::GrowthPick, "town_pick", &build.town_pick_call),
    ] {
        let Some(site) = site else {
            p.skipped.push(Skip { feature, name, reason: "the build table has no such site for this build".into() });
            continue;
        };
        let checked = verify(image, site.rva, site.expected)
            .and_then(|()| verify(image, site.callee.rva, site.callee.prologue).map_err(|e| format!("callee: {e}")));
        match checked {
            Ok(()) => p.steps.push(Step::CallPatch {
                feature,
                name,
                rva: site.rva,
                expected: site.expected,
                callee: site.callee.rva,
            }),
            Err(reason) => p.skipped.push(Skip { feature, name, reason }),
        }
    }
}

fn plan_hold(build: &BuildAddrs, image: &dyn Image, p: &mut Plan) {
    let skip = |p: &mut Plan, reason: String| {
        p.skipped.push(Skip { feature: Feature::Hold, name: "command_list_add", reason });
    };
    let (Some(add), Some(mv), Some(destroy), Some(handle)) =
        (&build.command_list_add, &build.command_move, &build.command_destroy, &build.handle_destroy)
    else {
        return skip(p, "the build table lacks command_list_add or one of the engine helpers for this build".into());
    };
    let Some(kind_offset) = build.kind_offset else {
        return skip(p, "the build table has no command-kind offset for this build".into());
    };
    if build.ui_add_sites.is_empty() {
        return skip(p, "the build table has no list of construction-tool call sites for this build".into());
    }
    if build.kinds.is_empty() {
        return skip(p, "the build table names no command kind the link may hold".into());
    }
    for (name, site) in [("command_move", mv), ("command_destroy", destroy), ("handle_destroy", handle)] {
        if let Err(reason) = verify(image, site.rva, site.prologue) {
            return skip(p, format!("engine helper {name}: {reason}"));
        }
    }
    match verify(image, add.rva, add.prologue) {
        Ok(()) => {
            p.steps.push(Step::Detour {
                feature: Feature::Hold,
                name: "command_list_add",
                rva: add.rva,
                expected: add.prologue,
            });
            p.kind_offset = Some(kind_offset);
            p.engine =
                Some(EngineAddrs { command_move: mv.rva, command_destroy: destroy.rva, handle_destroy: handle.rva });
        }
        Err(reason) => skip(p, reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_table::{select, CallSite, HookSite, PatchSite};

    fn cfg(hooks: bool, hold: bool, poolpatch: bool) -> HookConfig {
        HookConfig { hooks, hold, poolpatch, ..Default::default() }
    }

    /// An image holding exactly the bytes the 40420 row expects, at the row's addresses.
    fn image_of_40420() -> BytesImage {
        let b = select(0x6ac5_0427).unwrap();
        let mut img = BytesImage::zeroed(0, 0x0400_0000);
        for s in [
            &b.command_list_add,
            &b.apply_command,
            &b.town_seed,
            &b.town_develop,
            &b.town_update_size,
            &b.town_candidates,
            &b.town_edges,
        ]
        .into_iter()
        .flatten()
        {
            img.write(s.rva, s.prologue);
        }
        let g = b.game_time_getter.as_ref().unwrap();
        img.write(g.rva, g.prologue);
        for c in [&b.town_sort_call, &b.town_pick_call].into_iter().flatten() {
            img.write(c.rva, c.expected);
            img.write(c.callee.rva, c.callee.prologue);
        }
        for s in [&b.command_move, &b.command_destroy, &b.handle_destroy].into_iter().flatten() {
            img.write(s.rva, s.prologue);
        }
        let p = b.script_pool.as_ref().unwrap();
        img.write(p.rva, p.expected);
        let q = b.lua_makeseed.as_ref().unwrap();
        img.write(q.rva, q.expected);
        img
    }

    fn names(p: &Plan) -> Vec<&'static str> {
        p.steps.iter().map(Step::name).collect()
    }

    // ---- the switches

    #[test]
    fn switches_are_off_unless_exactly_one() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(HookConfig::from_lookup(env(&[])), HookConfig::default());
        assert!(!HookConfig::default().any());
        let all = HookConfig::from_lookup(env(&[(HOOKS_VAR, "1"), (HOLD_VAR, "1"), (POOLPATCH_VAR, "1")]));
        assert_eq!(all, cfg(true, true, true));
        for off in ["", "0", "true", "yes", "on", "2", "11", "-1", "one"] {
            let c = HookConfig::from_lookup(|k| (k != FORCE_TOWN_SEED_VAR).then(|| off.to_string()));
            assert_eq!(c, HookConfig::default(), "{off:?} must not switch anything on");
        }
        assert!(HookConfig::from_lookup(|k| (k == HOOKS_VAR).then(|| " 1 ".to_string())).hooks, "blanks are trimmed");
        let only_pool = HookConfig::from_lookup(env(&[(POOLPATCH_VAR, "1")]));
        assert!(only_pool.any() && !only_pool.hooks, "the pool patch is its own switch");
        assert_eq!(all.describe(), "hooks=on hold=on poolpatch=on luaseed=off town_seed=off");
        assert_eq!(HookConfig::default().describe(), "hooks=off hold=off poolpatch=off luaseed=off town_seed=off");
        let seed = HookConfig::from_lookup(env(&[(TOWN_SEED_VAR, "1")]));
        assert!(seed.any() && seed.town_seed && !seed.hooks && !seed.hold && !seed.poolpatch, "its own switch");
        assert_eq!(seed.describe(), "hooks=off hold=off poolpatch=off luaseed=off town_seed=on");
        assert!(!HookConfig::from_lookup(env(&[(TOWN_SEED_VAR, "true")])).any(), "only the exact value 1");
    }

    #[test]
    fn the_forced_seed_parses_decimal_and_hex_in_32_bits_and_ignores_the_rest() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(parse_seed("12345"), Ok(12345));
        assert_eq!(parse_seed(" 0x2cd15ea0 "), Ok(0x2cd1_5ea0));
        assert_eq!(parse_seed("0XFFFFFFFF"), Ok(u32::MAX));
        assert_eq!(parse_seed("4294967295"), Ok(u32::MAX));
        for bad in ["4294967296", "0x100000000", "-1", "abc", "0x", "0xzz", "1.5"] {
            assert!(parse_seed(bad).is_err(), "{bad}");
        }
        let c = HookConfig::from_lookup(env(&[(FORCE_TOWN_SEED_VAR, "0x10")]));
        assert_eq!((c.force_seed, c.town_seed, c.any()), (Some(16), true, true), "forcing installs the hook");
        assert_eq!(c.describe(), "hooks=off hold=off poolpatch=off luaseed=off town_seed=on force_seed=0x00000010");
        let bad = HookConfig::from_lookup(env(&[(FORCE_TOWN_SEED_VAR, "nope")]));
        assert_eq!(bad, HookConfig::default(), "an invalid value switches nothing on");
        assert!(forced_seed(env(&[(FORCE_TOWN_SEED_VAR, "nope")])).is_err());
        assert_eq!(forced_seed(env(&[])), Ok(None));
        assert_eq!(forced_seed(env(&[(FORCE_TOWN_SEED_VAR, "  ")])), Ok(None));
        let both = HookConfig::from_lookup(env(&[(TOWN_SEED_VAR, "1"), (FORCE_TOWN_SEED_VAR, "7")]));
        assert_eq!((both.town_seed, both.force_seed), (true, Some(7)));
    }

    #[test]
    fn the_town_seed_is_planned_only_on_request_and_only_when_the_bytes_match() {
        let (b, mut img) = (select(0x6ac5_0427).unwrap(), image_of_40420());
        let on = HookConfig { town_seed: true, ..Default::default() };
        let p = plan(b, on, &img);
        assert_eq!(names(&p), ["town_seed"], "{:?}", p.report());
        assert!(matches!(&p.steps[0], Step::Detour { rva: 0x8d4270, feature: Feature::TownSeed, .. }));
        assert!(plan(b, HookConfig { hooks: true, hold: true, poolpatch: true, ..Default::default() }, &img)
            .steps
            .iter()
            .all(|s| s.feature() != Feature::TownSeed));
        img.write(0x8d4270, &[0x90]);
        let p = plan(b, on, &img);
        assert!(p.steps.is_empty());
        assert_eq!(p.skipped.len(), 1);
        assert_eq!(p.skipped[0].name, "town_seed");
        assert!(p.skipped[0].reason.contains("0x8d4270"), "{}", p.skipped[0].reason);
    }

    #[test]
    fn a_build_without_a_town_seed_site_skips_it_with_a_reason() {
        let p = plan(&BARE, HookConfig { town_seed: true, ..Default::default() }, &BytesImage::zeroed(0, 16));
        assert!(p.steps.is_empty());
        assert_eq!(
            p.report(),
            ["hooks: SKIPPED town_seed (town_seed): the build table has no town_seed site for this build"]
        );
    }

    // ---- planning on the real 40420 row

    #[test]
    fn nothing_is_planned_by_default() {
        let b = select(0x6ac5_0427).unwrap();
        let p = plan(b, HookConfig::default(), &image_of_40420());
        assert_eq!(p, Plan::default());
        assert!(p.report().is_empty());
    }

    #[test]
    fn the_counters_alone_are_one_detour_on_the_apply_function() {
        let b = select(0x6ac5_0427).unwrap();
        let p = plan(b, cfg(true, false, false), &image_of_40420());
        assert_eq!(names(&p), ["apply_command"]);
        assert!(p.has(Feature::Counters) && !p.has(Feature::Hold) && !p.has(Feature::PoolPatch));
        assert_eq!(p.kind_offset, Some(0x9b8));
        assert_eq!(p.engine, None, "no engine helper is needed to count");
        assert!(p.skipped.is_empty() && p.notes.is_empty());
    }

    #[test]
    fn hold_adds_the_command_list_hook_after_the_counters() {
        let b = select(0x6ac5_0427).unwrap();
        let p = plan(b, cfg(true, true, false), &image_of_40420());
        assert_eq!(names(&p), ["apply_command", "command_list_add"]);
        assert_eq!(
            p.engine,
            Some(EngineAddrs { command_move: 0x9d0430, command_destroy: 0x9d0670, handle_destroy: 0x3040c90 })
        );
    }

    #[test]
    fn the_report_lines_are_the_ones_the_docs_quote() {
        let b = select(0x6ac5_0427).unwrap();
        let p = plan(b, cfg(true, true, true), &image_of_40420());
        assert_eq!(
            p.report(),
            [
                "hooks: apply_command: 14 bytes at rva 0x9e33b0 match the build table",
                "hooks: command_list_add: 18 bytes at rva 0x9d4040 match the build table",
                "hooks: script_pool: 22 bytes at rva 0xab00fc match the build table",
            ]
        );
        let none = plan(b, cfg(true, true, true), &BytesImage::zeroed(0, 0x0400_0000));
        assert!(none.report().iter().all(|l| l.starts_with("hooks: SKIPPED ")), "{:?}", none.report());
        assert!(none.report()[0].starts_with("hooks: SKIPPED apply_command (counters): bytes at rva 0x9e33b0 are 00"));
    }

    #[test]
    fn hold_without_hooks_does_nothing_and_says_so() {
        let b = select(0x6ac5_0427).unwrap();
        let p = plan(b, cfg(false, true, false), &image_of_40420());
        assert!(p.steps.is_empty());
        assert_eq!(p.notes.len(), 1);
        assert!(p.report()[0].contains("does nothing without FEVERLINK3_HOOKS=1"));
    }

    #[test]
    fn the_pool_patch_is_independent_of_the_hooks() {
        let b = select(0x6ac5_0427).unwrap();
        let p = plan(b, cfg(false, false, true), &image_of_40420());
        assert_eq!(names(&p), ["script_pool"]);
        match &p.steps[0] {
            Step::Patch { rva, expected, replacement, .. } => {
                assert_eq!(*rva, 0xab00fc);
                assert_eq!(expected.len(), replacement.len());
            }
            other => panic!("not a patch: {other:?}"),
        }
        let all = plan(b, cfg(true, true, true), &image_of_40420());
        assert_eq!(names(&all), ["apply_command", "command_list_add", "script_pool"]);
    }

    #[test]
    fn a_wrong_byte_skips_only_that_piece_and_says_which() {
        let b = select(0x6ac5_0427).unwrap();
        let mut img = image_of_40420();
        let apply = b.apply_command.as_ref().unwrap();
        img.write(apply.rva + 3, &[0x90]);
        let p = plan(b, cfg(true, true, true), &img);
        assert_eq!(names(&p), ["command_list_add", "script_pool"], "the others are still installed");
        assert_eq!(p.skipped.len(), 1);
        let s = &p.skipped[0];
        assert_eq!((s.feature, s.name), (Feature::Counters, "apply_command"));
        assert!(s.reason.contains("0x9e33b0") && s.reason.contains("90"), "{}", s.reason);
        assert!(p.report().iter().any(|l| l.starts_with("hooks: SKIPPED apply_command (counters)")));
    }

    #[test]
    fn a_changed_engine_helper_keeps_the_hold_hook_off_but_not_the_counters() {
        let b = select(0x6ac5_0427).unwrap();
        for helper in [&b.command_move, &b.command_destroy, &b.handle_destroy] {
            let mut img = image_of_40420();
            let site = helper.as_ref().unwrap();
            img.write(site.rva, &[0xCC]);
            let p = plan(b, cfg(true, true, false), &img);
            assert_eq!(names(&p), ["apply_command"]);
            assert_eq!(p.skipped.len(), 1);
            assert_eq!(p.skipped[0].feature, Feature::Hold);
            assert!(p.skipped[0].reason.contains("engine helper"), "{}", p.skipped[0].reason);
            assert_eq!(p.engine, None);
        }
    }

    #[test]
    fn growth_is_planned_with_its_game_time_pieces_and_degrades_without_them() {
        let b = select(0x6ac5_0427).unwrap();
        let on = HookConfig { growth: Settings { log: true, ..Default::default() }, ..Default::default() };
        assert!(on.any());
        let p = plan(b, on, &image_of_40420());
        assert_eq!(names(&p), ["town_develop", "town_update_size"], "{:?}", p.report());
        assert_eq!(p.game_time_getter, Some(0x2a96c0));
        // the handler changed: Develop is still logged, without game time
        let mut img = image_of_40420();
        img.write(0x9e12b0 + 3, &[0x90]);
        let p = plan(b, on, &img);
        assert_eq!(names(&p), ["town_develop"]);
        assert_eq!(p.game_time_getter, None);
        assert_eq!(p.skipped.len(), 1);
        assert!(p.skipped[0].reason.contains("no game time"), "{}", p.skipped[0].reason);
        // Develop changed: nothing of growth
        let mut img = image_of_40420();
        img.write(0x8de0b0 + 1, &[0x90]);
        let p = plan(b, on, &img);
        assert!(p.steps.is_empty());
        assert_eq!(p.skipped[0].name, "town_develop");
        // the derive mode alone is enough to ask for the hook
        let derive = HookConfig::from_lookup(env_pairs(&[("FEVERLINK3_GROWTH_SEED_MODE", "derive")]));
        assert!(derive.any() && derive.growth.mode == crate::growth::Mode::Derive);
        assert!(derive.describe().contains("growth=on seed_mode=derive"), "{}", derive.describe());
        // not asked: not planned
        assert!(plan(b, HookConfig::default(), &image_of_40420()).steps.is_empty());
    }

    #[test]
    fn the_deep_log_adds_two_wraps_each_checked_on_its_own() {
        let b = select(0x6ac5_0427).unwrap();
        let deep = HookConfig::from_lookup(env_pairs(&[("FEVERLINK3_LOG_GROWTH_DEEP", "1")]));
        assert!(deep.any() && deep.growth.deep);
        let p = plan(b, deep, &image_of_40420());
        assert_eq!(
            names(&p),
            ["town_develop", "town_update_size", "town_candidates", "town_edges", "town_sort", "town_pick"],
            "{:?}",
            p.report()
        );
        let mut img = image_of_40420();
        img.write(0x9677c0 + 6, &[0x90]);
        let p = plan(b, deep, &img);
        assert_eq!(names(&p), ["town_develop", "town_update_size", "town_edges", "town_sort", "town_pick"]);
        assert!(p.skipped.iter().any(|s| s.name == "town_candidates"));
        // a changed call or callee skips only that redirect
        let mut img = image_of_40420();
        img.write(0x967c16 + 1, &[0x00]);
        assert!(!names(&plan(b, deep, &img)).contains(&"town_sort"));
        let mut img = image_of_40420();
        img.write(0x3055870 + 2, &[0x00]);
        let p = plan(b, deep, &img);
        assert!(!names(&p).contains(&"town_pick") && names(&p).contains(&"town_sort"));
        assert!(p.skipped.iter().any(|s| s.name == "town_pick" && s.reason.starts_with("callee: ")), "{:?}", p.skipped);
        // without the deep switch neither is planned
        let log = HookConfig::from_lookup(env_pairs(&[("FEVERLINK3_LOG_GROWTH", "1")]));
        assert_eq!(names(&plan(b, log, &image_of_40420())), ["town_develop", "town_update_size"]);
    }

    fn env_pairs(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn a_changed_pool_region_is_not_patched() {
        let b = select(0x6ac5_0427).unwrap();
        let mut img = image_of_40420();
        img.write(0xab00fc + 21, &[0x03]);
        let p = plan(b, cfg(false, false, true), &img);
        assert!(p.steps.is_empty());
        assert_eq!(p.skipped[0].name, "script_pool");
    }

    #[test]
    fn another_executable_installs_nothing() {
        // every address of the 40420 row pointing into bytes of some other program
        let b = select(0x6ac5_0427).unwrap();
        let mut img = BytesImage::zeroed(0, 0x0400_0000);
        for (i, byte) in img.bytes.iter_mut().enumerate() {
            *byte = (i.wrapping_mul(2654435761) >> 7) as u8;
        }
        let p = plan(b, cfg(true, true, true), &img);
        assert!(p.steps.is_empty(), "{:?}", p.steps);
        assert_eq!(p.skipped.len(), 3);
        assert!(p.engine.is_none() && p.kind_offset.is_none());
    }

    #[test]
    fn an_image_that_is_too_small_is_not_readable_rather_than_a_panic() {
        let b = select(0x6ac5_0427).unwrap();
        let tiny = BytesImage::zeroed(0, 0x1000);
        let p = plan(b, cfg(true, true, true), &tiny);
        assert!(p.steps.is_empty());
        assert!(p.skipped.iter().all(|s| s.reason.contains("not readable")), "{:?}", p.skipped);
    }

    // ---- planning on made-up rows

    static BARE: BuildAddrs = BuildAddrs {
        timestamp: 1,
        name: "bare",
        command_list_add: None,
        apply_command: None,
        town_seed: None,
        town_develop: None,
        town_update_size: None,
        town_candidates: None,
        town_edges: None,
        town_sort_call: None,
        town_pick_call: None,
        game_time_getter: None,
        script_pool: None,
        lua_makeseed: None,
        command_move: None,
        command_destroy: None,
        handle_destroy: None,
        ui_add_sites: &[],
        kind_offset: None,
        kinds: &[],
    };

    #[test]
    fn a_build_with_nothing_in_the_table_skips_everything_with_a_reason() {
        let p = plan(&BARE, cfg(true, true, true), &BytesImage::zeroed(0, 0x100));
        assert!(p.steps.is_empty());
        let by: Vec<_> = p.skipped.iter().map(|s| (s.feature, s.name)).collect();
        assert_eq!(
            by,
            [
                (Feature::Counters, "apply_command"),
                (Feature::Hold, "command_list_add"),
                (Feature::PoolPatch, "script_pool")
            ]
        );
        assert!(p.skipped.iter().all(|s| s.reason.contains("build table")));
    }

    #[test]
    fn hold_needs_a_tool_list_and_a_kind() {
        static ADD: HookSite = HookSite { rva: 0x10, prologue: &[0x90; 14] };
        static HELPER: CallSite = CallSite { rva: 0x40, prologue: &[0x90; 8] };
        let mut b = BARE.clone();
        b.command_list_add = Some(ADD.clone());
        b.command_move = Some(HELPER.clone());
        b.command_destroy = Some(HELPER.clone());
        b.handle_destroy = Some(HELPER.clone());
        b.kind_offset = Some(8);
        let mut img = BytesImage::zeroed(0, 0x100);
        img.write(0x10, &[0x90; 14]);
        img.write(0x40, &[0x90; 8]);
        let hold_skip = |p: &Plan| p.skipped.iter().find(|s| s.feature == Feature::Hold).map(|s| s.reason.clone());
        let leaked: &'static BuildAddrs = Box::leak(Box::new(b.clone()));
        let p = plan(leaked, cfg(true, true, false), &img);
        assert!(hold_skip(&p).unwrap().contains("tool call sites"));
        b.ui_add_sites = &[0x20];
        let leaked: &'static BuildAddrs = Box::leak(Box::new(b.clone()));
        let p = plan(leaked, cfg(true, true, false), &img);
        assert!(hold_skip(&p).unwrap().contains("command kind"));
        b.kinds = &[("build_proposal", 52)];
        let leaked: &'static BuildAddrs = Box::leak(Box::new(b));
        let p = plan(leaked, cfg(true, true, false), &img);
        assert_eq!(names(&p), ["command_list_add"]);
        assert_eq!(hold_skip(&p), None);
    }

    #[test]
    fn pool_patch_needs_the_table_row() {
        static PATCH: PatchSite = PatchSite { rva: 8, expected: &[1, 2, 3], replacement: &[9, 9, 9] };
        let mut b = BARE.clone();
        b.script_pool = Some(PATCH.clone());
        let leaked: &'static BuildAddrs = Box::leak(Box::new(b));
        let mut img = BytesImage::zeroed(0, 0x20);
        assert!(plan(leaked, cfg(false, false, true), &img).steps.is_empty());
        img.write(8, &[1, 2, 3]);
        assert_eq!(names(&plan(leaked, cfg(false, false, true), &img)), ["script_pool"]);
    }

    #[test]
    fn the_seed_switch_is_exactly_fixed() {
        let env = |v: Option<&'static str>| {
            HookConfig::from_lookup(move |n| if n == LUA_SEED_VAR { v.map(String::from) } else { None })
        };
        assert!(env(Some("fixed")).luaseed && env(Some(" fixed ")).luaseed);
        for off in [None, Some(""), Some("0"), Some("1"), Some("FIXED"), Some("on")] {
            assert!(!env(off).luaseed, "{off:?}");
        }
        assert!(env(Some("fixed")).any(), "the seed pin alone makes the module touch the game's code");
        assert!(!HookConfig::default().any());
    }

    #[test]
    fn the_seed_pin_is_planned_only_when_asked_and_verified() {
        let img = image_of_40420();
        let b = select(0x6ac5_0427).unwrap();
        let on = HookConfig { luaseed: true, ..Default::default() };
        let p = plan(b, on, &img);
        assert_eq!(names(&p), ["lua_makeseed"]);
        let Step::Patch { replacement, expected, feature, .. } = &p.steps[0] else { panic!("not a patch") };
        assert_eq!(*feature, Feature::LuaSeed);
        assert_eq!(expected.len(), 26);
        assert_eq!(&replacement[..6], &[0xB8, 0x53, 0x33, 0x4C, 0x46, 0xC3]);
        assert!(plan(b, HookConfig::default(), &img).steps.is_empty(), "off by default");

        // other bytes at the site: nothing is planned, the reason is given (fail open)
        let mut other = img.clone();
        other.write(b.lua_makeseed.as_ref().unwrap().rva + 7, &[0x00]);
        let p = plan(b, on, &other);
        assert!(p.steps.is_empty());
        assert_eq!((p.skipped[0].name, p.skipped[0].feature), ("lua_makeseed", Feature::LuaSeed));
        assert!(p.skipped[0].reason.contains("the table expects"), "{}", p.skipped[0].reason);

        // a build without the site
        let mut bare = BARE.clone();
        bare.lua_makeseed = None;
        let leaked: &'static BuildAddrs = Box::leak(Box::new(bare));
        let p = plan(leaked, on, &img);
        assert!(p.steps.is_empty() && p.skipped[0].reason.contains("no lua_makeseed site"));
    }

    #[test]
    fn hex_is_short() {
        assert_eq!(hex(&[1, 0xab]), "01 ab");
        assert_eq!(hex(&[0; 30]).split(' ').count(), 25);
        assert!(hex(&[0; 30]).ends_with(".."));
    }
}
