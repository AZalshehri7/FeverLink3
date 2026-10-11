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
//! Anything but the exact value `1` (after trimming blanks) is off.

use crate::build_table::{BuildAddrs, Rva};

pub const HOOKS_VAR: &str = "FEVERLINK3_HOOKS";
pub const HOLD_VAR: &str = "FEVERLINK3_HOLD";
pub const POOLPATCH_VAR: &str = "FEVERLINK3_POOLPATCH";
pub const TOWN_SEED_VAR: &str = "FEVERLINK3_LOG_TOWN_SEED";

/// The switches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HookConfig {
    pub hooks: bool,
    pub hold: bool,
    pub poolpatch: bool,
    /// Log the town developer's random seed (read-only; independent of the other switches).
    pub town_seed: bool,
}

impl HookConfig {
    /// Reads the switches through `get` (the environment in the module, a map in the tests).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> HookConfig {
        let on = |name: &str| get(name).is_some_and(|v| v.trim() == "1");
        HookConfig {
            hooks: on(HOOKS_VAR),
            hold: on(HOLD_VAR),
            poolpatch: on(POOLPATCH_VAR),
            town_seed: on(TOWN_SEED_VAR),
        }
    }

    /// True when the module may touch the game's code at all.
    pub fn any(&self) -> bool {
        self.hooks || self.poolpatch || self.town_seed
    }

    pub fn describe(&self) -> String {
        let f = |b: bool| if b { "on" } else { "off" };
        format!(
            "hooks={} hold={} poolpatch={} town_seed={}",
            f(self.hooks),
            f(self.hold),
            f(self.poolpatch),
            f(self.town_seed)
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
}

impl Feature {
    pub fn name(self) -> &'static str {
        match self {
            Feature::Counters => "counters",
            Feature::Hold => "hold",
            Feature::PoolPatch => "poolpatch",
            Feature::TownSeed => "town_seed",
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
    if cfg.town_seed {
        plan_town_seed(build, image, &mut p);
    }
    if cfg.poolpatch {
        match &build.script_pool {
            None => p.skipped.push(Skip {
                feature: Feature::PoolPatch,
                name: "script_pool",
                reason: "the build table has no script_pool site for this build".into(),
            }),
            Some(site) => match verify(image, site.rva, site.expected) {
                Ok(()) => p.steps.push(Step::Patch {
                    feature: Feature::PoolPatch,
                    name: "script_pool",
                    rva: site.rva,
                    expected: site.expected,
                    replacement: site.replacement,
                }),
                Err(reason) => p.skipped.push(Skip { feature: Feature::PoolPatch, name: "script_pool", reason }),
            },
        }
    }
    p
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
        HookConfig { hooks, hold, poolpatch, town_seed: false }
    }

    /// An image holding exactly the bytes the 40420 row expects, at the row's addresses.
    fn image_of_40420() -> BytesImage {
        let b = select(0x6ac5_0427).unwrap();
        let mut img = BytesImage::zeroed(0, 0x0400_0000);
        for s in [&b.command_list_add, &b.apply_command, &b.town_seed].into_iter().flatten() {
            img.write(s.rva, s.prologue);
        }
        for s in [&b.command_move, &b.command_destroy, &b.handle_destroy].into_iter().flatten() {
            img.write(s.rva, s.prologue);
        }
        let p = b.script_pool.as_ref().unwrap();
        img.write(p.rva, p.expected);
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
        assert_eq!(all.describe(), "hooks=on hold=on poolpatch=on town_seed=off");
        assert_eq!(HookConfig::default().describe(), "hooks=off hold=off poolpatch=off town_seed=off");
        let seed = HookConfig::from_lookup(env(&[(TOWN_SEED_VAR, "1")]));
        assert!(seed.any() && seed.town_seed && !seed.hooks && !seed.hold && !seed.poolpatch, "its own switch");
        assert_eq!(seed.describe(), "hooks=off hold=off poolpatch=off town_seed=on");
        assert!(!HookConfig::from_lookup(env(&[(TOWN_SEED_VAR, "true")])).any(), "only the exact value 1");
    }

    #[test]
    fn the_town_seed_is_planned_only_on_request_and_only_when_the_bytes_match() {
        let (b, mut img) = (select(0x6ac5_0427).unwrap(), image_of_40420());
        let on = HookConfig { town_seed: true, ..Default::default() };
        let p = plan(b, on, &img);
        assert_eq!(names(&p), ["town_seed"], "{:?}", p.report());
        assert!(matches!(&p.steps[0], Step::Detour { rva: 0x8d4270, feature: Feature::TownSeed, .. }));
        assert!(plan(b, HookConfig { hooks: true, hold: true, poolpatch: true, town_seed: false }, &img)
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
        script_pool: None,
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
    fn hex_is_short() {
        assert_eq!(hex(&[1, 0xab]), "01 ab");
        assert_eq!(hex(&[0; 30]).split(' ').count(), 25);
        assert!(hex(&[0; 30]).ends_with(".."));
    }
}
