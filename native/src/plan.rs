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
//! 4. `FEVERLINK3_LUA_SEED=fixed`: pin the seed of Lua's string hash to a constant (`build_table::LUA_SEED`), so the
//!    iteration order of `pairs` over string keys is the same in every process. A patch, independent of the
//!    others. Any other value, or none, leaves the game's own per-process seed.
//!
//! Anything but the exact value `1` (after trimming blanks; `fixed` for the seed) is off.

use crate::build_table::{BuildAddrs, PatchSite, Rva};

pub const HOOKS_VAR: &str = "FEVERLINK3_HOOKS";
pub const HOLD_VAR: &str = "FEVERLINK3_HOLD";
pub const POOLPATCH_VAR: &str = "FEVERLINK3_POOLPATCH";
pub const LUA_SEED_VAR: &str = "FEVERLINK3_LUA_SEED";
/// `1` asks the mod's Lua probe (and the module's log) to record the string-hash seed's effect. Observation only.
pub const LUA_SEED_LOG_VAR: &str = "FEVERLINK3_LUA_SEED_LOG";

/// The switches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HookConfig {
    pub hooks: bool,
    pub hold: bool,
    pub poolpatch: bool,
    pub luaseed: bool,
}

impl HookConfig {
    /// Reads the switches through `get` (the environment in the module, a map in the tests).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> HookConfig {
        let on = |name: &str| get(name).is_some_and(|v| v.trim() == "1");
        HookConfig {
            hooks: on(HOOKS_VAR),
            hold: on(HOLD_VAR),
            poolpatch: on(POOLPATCH_VAR),
            luaseed: get(LUA_SEED_VAR).is_some_and(|v| v.trim() == "fixed"),
        }
    }

    /// True when the module may touch the game's code at all.
    pub fn any(&self) -> bool {
        self.hooks || self.poolpatch || self.luaseed
    }

    pub fn describe(&self) -> String {
        let f = |b: bool| if b { "on" } else { "off" };
        format!(
            "hooks={} hold={} poolpatch={} luaseed={}",
            f(self.hooks),
            f(self.hold),
            f(self.poolpatch),
            f(self.luaseed)
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
    /// The pinned Lua string-hash seed.
    LuaSeed,
}

impl Feature {
    pub fn name(self) -> &'static str {
        match self {
            Feature::Counters => "counters",
            Feature::Hold => "hold",
            Feature::PoolPatch => "poolpatch",
            Feature::LuaSeed => "luaseed",
        }
    }
}

/// One thing to do to the game's code, already checked against the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Detour { feature: Feature, name: &'static str, rva: Rva, expected: &'static [u8] },
    Patch { feature: Feature, name: &'static str, rva: Rva, expected: &'static [u8], replacement: &'static [u8] },
}

impl Step {
    pub fn name(&self) -> &'static str {
        match self {
            Step::Detour { name, .. } | Step::Patch { name, .. } => name,
        }
    }

    pub fn feature(&self) -> Feature {
        match self {
            Step::Detour { feature, .. } | Step::Patch { feature, .. } => *feature,
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
                Step::Patch { name, rva, expected, .. } => {
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
        HookConfig { hooks, hold, poolpatch, luaseed: false }
    }

    /// An image holding exactly the bytes the 40420 row expects, at the row's addresses.
    fn image_of_40420() -> BytesImage {
        let b = select(0x6ac5_0427).unwrap();
        let mut img = BytesImage::zeroed(0, 0x0400_0000);
        for s in [&b.command_list_add, &b.apply_command].into_iter().flatten() {
            img.write(s.rva, s.prologue);
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
            let c = HookConfig::from_lookup(|_| Some(off.to_string()));
            assert_eq!(c, HookConfig::default(), "{off:?} must not switch anything on");
        }
        assert!(HookConfig::from_lookup(|k| (k == HOOKS_VAR).then(|| " 1 ".to_string())).hooks, "blanks are trimmed");
        let only_pool = HookConfig::from_lookup(env(&[(POOLPATCH_VAR, "1")]));
        assert!(only_pool.any() && !only_pool.hooks, "the pool patch is its own switch");
        assert_eq!(all.describe(), "hooks=on hold=on poolpatch=on luaseed=off");
        assert_eq!(HookConfig::default().describe(), "hooks=off hold=off poolpatch=off luaseed=off");
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
