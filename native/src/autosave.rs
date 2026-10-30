//! `FEVERLINK3_SYNC_AUTOSAVE` (on by default, `=0` off): during a session the games autosave together, at the host's stamp, instead
//! of each on its own timer.
//!
//! Why (friend session 2026-10-30, two PCs): each game's own autosave stopped its simulation ~3.3 s ("Saving game complete, took
//! 3254 ms", thread "Simulation Thread"). The guest's interval was 2 minutes, the host's 10, so the other game waited at the input
//! barrier ~4 s every ~2 minutes. The documented API has no autosave control. Owner's design: while a session runs the games' own
//! timers are held off, the host stamps an `autosave` action at its own interval and every game runs the game's own autosave there,
//! so the stops overlap (one stop for everyone) and every player keeps the game's own autosave files and their rotation.
//!
//! What the executable says (static research of build 40425, `docs/research/AUTOSAVE_20261030.md`; NOT yet seen in the game):
//!
//! - `UI::CGameUI::AutoSave(this)` (0x65a150, named by its assert and its lambda's RTTI) performs one autosave: it asserts
//!   `!IsCurrentlySaving()` (`this+0x1170`, a fatal assert), sets that flag and queues the save, which the simulation thread runs
//!   (the GameSave command, 0x9e1b00 -> 0x32f860, "Saving to file", "Saving game complete").
//! - Its only caller is the "autosave due" decision inside `CGameUI`'s per-frame update (0x65ecd0, vtable slot 52): the timer
//!   `this+0xe38` (microseconds of frame time) against `autosaveIntervalMinutes * 60 000 000` (`[GetGlobalSettings()+0x18]`), and after
//!   the call the timer restarts at 0 (0x65f882).
//!
//! So the game's own timer is held off by its own call returning without saving while the mod's hold stands (the timer restarts as
//! after a save, whatever the interval is), and the mod's `autosave {now}` runs the original AutoSave from the start of that per-frame
//! update, the thread and the `this` the game uses for it, unless a save is under way. The interval is read for the mod (the host
//! stamps at its own). The hold expires when the mod stops refreshing it: the game then autosaves as before.
//!
//! Everything here is pure or atomics: the hooks (`hooks.rs`) hand in a [`Memory`] of the game; the tests hand in a map.

use crate::build_table::{HookSite, Rva};
use crate::iplayer::Witness;
use crate::plan::Image;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::Mutex;

/// The switch: on unless exactly `0`.
pub const SYNC_VAR: &str = "FEVERLINK3_SYNC_AUTOSAVE";
/// What the `ready` event lists when both detours are in place (the mod sends `autosave` only then).
pub const READY_NAME: &str = "sync_autosave";
/// The two detours' names in the plan and in `native.log`.
pub const AUTOSAVE_HOOK: &str = "autosave";
pub const STEP_HOOK: &str = "gameui_step";
/// The mod refreshes its hold every 10 s; one not refreshed for this long is over (the game's own timer runs again).
pub const HOLD_STALE_MS: u64 = 30_000;
/// A `now` that no per-frame update took within this long (no world shown) is dropped, never run late.
pub const NOW_TTL_MS: u64 = 10_000;
/// `CGameUI+0x1170`: the saving flag (`IsCurrentlySaving`, 0x660bd0).
pub const SAVING_OFFSET: usize = 0x1170;
/// `GlobalSettings+0x18`: `autosaveIntervalMinutes` (sol usertype `AppConfig`, member offset 0x18 at 0x16cd40d).
pub const INTERVAL_OFFSET: usize = 0x18;

/// The sites of one game build (static analysis, `docs/research/AUTOSAVE_20261030.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub timestamp: u32,
    pub name: &'static str,
    pub autosave: HookSite,
    pub step: HookSite,
    /// The global `g_globalSettings` pointer that `GetGlobalSettings` (0x2c7110) returns.
    pub settings: Rva,
    pub witnesses: &'static [Witness],
}

pub static ROWS: &[Row] = &[Row {
    timestamp: 0x6ad9_ce10,
    name: "40425",
    // mov [rsp+10h],rbx ; mov [rsp+18h],rsi ; mov [rsp+20h],rdi ; push rbp
    autosave: HookSite {
        rva: 0x65a150,
        prologue: &[0x48, 0x89, 0x5C, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x48, 0x89, 0x7C, 0x24, 0x20, 0x55],
    },
    // mov rax,rsp ; push rbx ; push rsi ; push rdi ; push r12 ; push r13 ; push r14 ; push r15 ; sub rsp,8C0h
    step: HookSite {
        rva: 0x65ecd0,
        prologue: &[
            0x48, 0x8B, 0xC4, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x81, 0xEC, 0xC0,
            0x08, 0x00, 0x00,
        ],
    },
    settings: 0x4061c98,
    witnesses: &[
        Witness {
            rva: 0x2c7110,
            // sub rsp,28h ; mov rax,[rip+3D9AB7Dh] (-> 0x4061c98) ; test rax,rax
            bytes: &[0x48, 0x83, 0xEC, 0x28, 0x48, 0x8B, 0x05, 0x7D, 0xAB, 0xD9, 0x03, 0x48, 0x85, 0xC0],
            what: "GetGlobalSettings returns the global at 0x4061c98",
        },
        Witness {
            rva: 0x65f858,
            // call GetGlobalSettings ; movsxd rcx,[rax+18h] ; imul rax,rcx,3938700h ; cmp byte [rsi+1170h],0 ; jne ;
            // cmp rax,[rsi+0E38h] ; jge ; mov rcx,rsi ; call AutoSave ; mov [rsi+0E38h],r15
            bytes: &[
                0xE8, 0xB3, 0x78, 0xC6, 0xFF, 0x48, 0x63, 0x48, 0x18, 0x48, 0x69, 0xC1, 0x00, 0x87, 0x93, 0x03, 0x80,
                0xBE, 0x70, 0x11, 0x00, 0x00, 0x00, 0x75, 0x18, 0x48, 0x3B, 0x86, 0x38, 0x0E, 0x00, 0x00, 0x7D, 0x0F,
                0x48, 0x8B, 0xCE, 0xE8, 0xCE, 0xA8, 0xFF, 0xFF, 0x4C, 0x89, 0xBE, 0x38, 0x0E, 0x00, 0x00,
            ],
            what: "the per-frame update calls AutoSave when the timer passes autosaveIntervalMinutes (GlobalSettings+0x18)",
        },
        Witness {
            rva: 0x65a336,
            // cmp byte [rsi+1170h],0 ; jne (the fatal assert) ; mov byte [rsi+1170h],1
            bytes: &[
                0x80, 0xBE, 0x70, 0x11, 0x00, 0x00, 0x00, 0x0F, 0x85, 0x60, 0x01, 0x00, 0x00, 0xC6, 0x86, 0x70, 0x11,
                0x00, 0x00, 0x01,
            ],
            what: "AutoSave asserts and sets the saving flag CGameUI+0x1170",
        },
    ],
}];

pub fn row_for(timestamp: u32) -> Option<&'static Row> {
    ROWS.iter().find(|r| r.timestamp == timestamp)
}

pub fn switch_on(get: impl Fn(&str) -> Option<String>) -> bool {
    get(SYNC_VAR).is_none_or(|v| v.trim() != "0")
}

/// The row of this build with both prologues and every witness in the image. Any difference: the piece stays off.
pub fn check(timestamp: u32, build_name: &str, image: &dyn Image) -> Result<&'static Row, String> {
    let row = row_for(timestamp)
        .ok_or_else(|| format!("no verified row for game build {build_name} (timestamp {timestamp:#x})"))?;
    let same = |rva: Rva, want: &[u8], what: &str| match image.read(rva, want.len()) {
        Some(found) if found == want => Ok(()),
        Some(found) => Err(format!(
            "{what}: bytes at rva {rva:#x} are {} where the row expects {}",
            crate::plan::hex(&found),
            crate::plan::hex(want)
        )),
        None => Err(format!("{what}: {} bytes at rva {rva:#x} are not readable", want.len())),
    };
    same(row.autosave.rva, row.autosave.prologue, "CGameUI::AutoSave")?;
    same(row.step.rva, row.step.prologue, "CGameUI's per-frame update")?;
    for w in row.witnesses {
        same(w.rva, w.bytes, w.what)?;
    }
    Ok(row)
}

/// The game's memory as the hooks see it. Every read may be refused (`None`).
pub trait Memory {
    fn word(&self, addr: usize) -> Option<usize>;
    fn int(&self, addr: usize) -> Option<i32>;
    fn byte(&self, addr: usize) -> Option<u8>;
}

/// What the service thread said last.
#[derive(Debug, Default)]
struct Told {
    held_off: u64,
    runs: u64,
    refusals: u64,
    minutes: Option<Option<i32>>,
}

#[derive(Debug)]
pub struct State {
    armed: AtomicBool,
    /// The address of `g_globalSettings` in this process (0: not armed).
    settings: AtomicUsize,
    /// The hold stands until this clock (`pace::clock_ms`); 0: none.
    hold_until: AtomicU64,
    /// When the mod asked for an autosave now (clock + 1; 0: nothing asked).
    asked: AtomicU64,
    /// The module's own call of AutoSave is running (main thread): the AutoSave detour lets it through.
    own: AtomicBool,
    held_off: AtomicU64,
    runs: AtomicU64,
    refusals: AtomicU64,
    /// The last refusal (a static text).
    refusal: Mutex<&'static str>,
    /// The interval the service thread read last (-1: unreadable).
    minutes: AtomicI64,
    told: Mutex<Told>,
}

impl State {
    pub const fn new() -> State {
        State {
            armed: AtomicBool::new(false),
            settings: AtomicUsize::new(0),
            hold_until: AtomicU64::new(0),
            asked: AtomicU64::new(0),
            own: AtomicBool::new(false),
            held_off: AtomicU64::new(0),
            runs: AtomicU64::new(0),
            refusals: AtomicU64::new(0),
            refusal: Mutex::new(""),
            minutes: AtomicI64::new(-1),
            told: Mutex::new(Told { held_off: 0, runs: 0, refusals: 0, minutes: None }),
        }
    }

    /// Both detours are in place; `settings` is the address of `g_globalSettings`.
    pub fn arm(&self, settings: usize) {
        self.settings.store(settings, SeqCst);
        self.armed.store(true, SeqCst);
    }

    pub fn armed(&self) -> bool {
        self.armed.load(SeqCst)
    }

    pub fn held(&self, now: u64) -> bool {
        now < self.hold_until.load(SeqCst)
    }

    /// The mod's `autosave` line (service thread): the hold on (refreshed) or off, and an autosave now.
    pub fn order(&self, hold: bool, now_asked: bool, now: u64) {
        self.hold_until.store(if hold { now + HOLD_STALE_MS } else { 0 }, SeqCst);
        if now_asked {
            self.asked.store(now + 1, SeqCst);
        }
    }

    fn refuse(&self, why: &'static str) {
        *self.refusal.lock().unwrap_or_else(|p| p.into_inner()) = why;
        self.refusals.fetch_add(1, SeqCst);
    }

    /// The AutoSave detour (main thread): whether the original runs. The game's own call is dropped while the hold stands.
    pub fn let_through(&self, now: u64) -> bool {
        if !self.armed() || self.own.load(SeqCst) || !self.held(now) {
            return true;
        }
        self.held_off.fetch_add(1, SeqCst);
        false
    }

    /// The per-frame update's detour (main thread, `gameui` its `this`): whether the module runs AutoSave now. Taken once: a request
    /// that cannot run (a save under way, the flag unreadable, too old) is dropped and said, never run twice or late.
    pub fn take_due(&self, mem: &dyn Memory, gameui: usize, now: u64) -> bool {
        if !self.armed() {
            return false;
        }
        let asked = self.asked.load(SeqCst);
        if asked == 0 || self.asked.compare_exchange(asked, 0, SeqCst, SeqCst).is_err() {
            return false;
        }
        if now.saturating_sub(asked - 1) > NOW_TTL_MS {
            self.refuse("no game interface took the request within 10 s (no world shown)");
            return false;
        }
        match gameui.checked_add(SAVING_OFFSET).and_then(|a| mem.byte(a)) {
            Some(0) => true,
            Some(_) => {
                self.refuse("a save of this game was under way (CGameUI+0x1170 set)");
                false
            }
            None => {
                self.refuse("the game interface's saving flag (CGameUI+0x1170) was not readable");
                false
            }
        }
    }

    /// Around the module's own call of the original AutoSave (main thread).
    pub fn own_call(&self, inside: bool) {
        self.own.store(inside, SeqCst);
        if !inside {
            self.runs.fetch_add(1, SeqCst);
        }
    }

    /// `autosaveIntervalMinutes` now (service thread); `None` when not armed or unreadable (the settings not loaded yet).
    pub fn read_minutes(&self, mem: &dyn Memory) -> Option<i32> {
        if !self.armed() {
            return None;
        }
        let at = self.settings.load(SeqCst);
        let m = mem.word(at).filter(|&p| p != 0).and_then(|p| mem.int(p.checked_add(INTERVAL_OFFSET)?));
        self.minutes.store(m.map_or(-1, i64::from), SeqCst);
        m
    }

    /// The service thread's lines since it last asked, and whether the mod should hear the state (a new interval, a run, a refusal).
    pub fn news(&self) -> (Vec<String>, bool) {
        if !self.armed() {
            return (Vec::new(), false);
        }
        let mut t = self.told.lock().unwrap_or_else(|p| p.into_inner());
        let mut lines = Vec::new();
        let mut event = false;
        let m = self.minutes.load(SeqCst);
        let m = (m >= 0).then_some(m as i32);
        if t.minutes != Some(m) {
            t.minutes = Some(m);
            event = true;
            lines.push(match m {
                Some(0) => "sync autosave: this game's autosave is off (autosaveIntervalMinutes 0)".to_string(),
                Some(m) => format!("sync autosave: this game's autosave interval is {m} min (autosaveIntervalMinutes)"),
                None => "sync autosave: the autosave interval is not readable (yet)".to_string(),
            });
        }
        let held_off = self.held_off.load(SeqCst);
        if held_off != t.held_off {
            t.held_off = held_off;
            lines.push(format!(
                "sync autosave: the game's own autosave was due and HELD OFF (session hold; {held_off} so far)"
            ));
        }
        let runs = self.runs.load(SeqCst);
        if runs != t.runs {
            t.runs = runs;
            event = true;
            lines.push(format!("sync autosave: the game's own autosave RAN at the session's stamp ({runs} so far)"));
        }
        let refusals = self.refusals.load(SeqCst);
        if refusals != t.refusals {
            t.refusals = refusals;
            event = true;
            lines.push(format!(
                "sync autosave: autosave now REFUSED: {} ({refusals} so far)",
                *self.refusal.lock().unwrap_or_else(|p| p.into_inner())
            ));
        }
        (lines, event)
    }

    /// The `autosave` event: the interval (-1: unknown), whether the hold stands, the runs so far, the last refusal.
    pub fn event(&self, now: u64, why: &str) -> crate::link::Event {
        crate::link::Event::Autosave {
            minutes: self.minutes.load(SeqCst),
            hold: self.held(now),
            runs: self.runs.load(SeqCst),
            why: why.to_string(),
        }
    }
}

impl Default for State {
    fn default() -> State {
        State::new()
    }
}

pub static STATE: State = State::new();

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        words: HashMap<usize, usize>,
        ints: HashMap<usize, i32>,
        bytes: HashMap<usize, u8>,
    }

    impl Memory for Fake {
        fn word(&self, addr: usize) -> Option<usize> {
            self.words.get(&addr).copied()
        }
        fn int(&self, addr: usize) -> Option<i32> {
            self.ints.get(&addr).copied()
        }
        fn byte(&self, addr: usize) -> Option<u8> {
            self.bytes.get(&addr).copied()
        }
    }

    const UI: usize = 0x10_0000;
    const GLOBAL: usize = 0x20_0000;
    const SETTINGS: usize = 0x30_0000;

    #[test]
    fn the_hold_drops_only_the_games_own_call_and_a_request_runs_once_unless_a_save_is_under_way() {
        let s = State::new();
        let mut g = Fake::default();
        g.bytes.insert(UI + SAVING_OFFSET, 0);
        g.words.insert(GLOBAL, SETTINGS);
        g.ints.insert(SETTINGS + INTERVAL_OFFSET, 10);
        // not armed: the game's own autosave and nothing else
        s.order(true, true, 0);
        assert!(s.let_through(1) && !s.take_due(&g, UI, 1) && s.read_minutes(&g).is_none());
        s.arm(GLOBAL);
        assert_eq!(s.read_minutes(&g), Some(10));
        s.order(true, false, 1000);
        assert!(!s.let_through(2000), "held: the game's own call returns without saving");
        assert!(s.let_through(1000 + HOLD_STALE_MS), "a hold the mod stopped refreshing is over");
        // the module's own call passes the hold, once
        s.order(true, true, 5000);
        assert!(s.take_due(&g, UI, 5016));
        s.own_call(true);
        assert!(s.let_through(5016));
        s.own_call(false);
        assert!(!s.take_due(&g, UI, 5033), "taken once");
        // a save under way, or a request no frame took in time: dropped and said
        g.bytes.insert(UI + SAVING_OFFSET, 1);
        s.order(true, true, 6000);
        assert!(!s.take_due(&g, UI, 6016));
        s.order(true, true, 7000);
        g.bytes.insert(UI + SAVING_OFFSET, 0);
        assert!(!s.take_due(&g, UI, 7001 + NOW_TTL_MS));
        let (lines, event) = s.news();
        assert!(event);
        assert!(lines.iter().any(|l| l.contains("interval is 10 min")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("HELD OFF (session hold; 1 so far)")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("RAN at the session's stamp (1 so far)")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("REFUSED: no game interface took")), "{lines:?}");
        assert_eq!(s.news(), (vec![], false), "said once");
        s.order(false, false, 9000);
        assert_eq!(
            s.event(9000, ""),
            crate::link::Event::Autosave { minutes: 10, hold: false, runs: 1, why: String::new() }
        );
    }

    #[test]
    fn the_row_needs_both_prologues_and_every_witness() {
        assert!(switch_on(|_| None) && !switch_on(|_| Some("0".into())));
        let row = row_for(0x6ad9_ce10).expect("40425");
        assert!(row.autosave.prologue.len() >= crate::detour::JMP_ABS_LEN);
        assert!(row.step.prologue.len() >= crate::detour::JMP_ABS_LEN);
        let mut img = crate::plan::BytesImage::zeroed(0x10_0000, 0x260_0000);
        img.write(row.autosave.rva, row.autosave.prologue);
        img.write(row.step.rva, row.step.prologue);
        for w in row.witnesses.iter().take(2) {
            img.write(w.rva, w.bytes);
        }
        let err = check(row.timestamp, "40425", &img).unwrap_err();
        assert!(err.starts_with("AutoSave asserts and sets the saving flag"), "a missing witness refuses: {err}");
        img.write(row.witnesses[2].rva, row.witnesses[2].bytes);
        assert_eq!(check(row.timestamp, "40425", &img), Ok(row));
        assert!(check(0x1234, "x", &img).unwrap_err().starts_with("no verified row for game build x"));
    }
}
