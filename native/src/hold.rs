//! The hold/release state machine (`docs/NATIVE_LINK.md` section 4).
//!
//! Pure: no engine, no files, no clock of its own. The caller passes the real time in milliseconds (`now`) and,
//! when it has one, the game time (`clock`) into every call, and gets back what to do: which captured commands to
//! hand back to the engine and which to destroy. What a captured command *is* (`H`) is the caller's business; the
//! engine glue (`engine.rs`) uses a boxed copy of the call's arguments, the tests use a number.
//!
//! What is held, and in which order it leaves:
//!
//! - A command is held only when holding is on, its kind is in the held set and the ring has room
//!   ([`HoldMachine::admit`]); anything else passes through untouched, so solo play is stock behaviour.
//! - Entries leave in `(game time, id)` order. Without a game clock (this build cannot read the game time yet) a
//!   scheduled entry is due at once, which is the right reading for a mod that has already stopped the game at
//!   the stamp.
//! - Nothing can be held forever. Holding switches itself off after [`HoldConfig::silence_ms`] without a line
//!   from the mod (implicit `hold_off`), and an entry older than [`HoldConfig::max_hold_ms`] is released on its
//!   own. When holding goes off, every entry is released in order.

use std::collections::VecDeque;

use crate::engine::BuildFlags;
use crate::link::{CtlMessage, Event};

pub type HoldId = u64;

/// The limits of the ring and of the mod's silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldConfig {
    /// Most commands held at once; the next one passes through (and an error is reported).
    pub capacity: usize,
    /// A held command is released by the module after this long, whatever the mod says.
    pub max_hold_ms: u64,
    /// Holding switches itself off after this long without a line from the mod.
    pub silence_ms: u64,
}

impl Default for HoldConfig {
    fn default() -> Self {
        HoldConfig { capacity: 64, max_hold_ms: 30_000, silence_ms: 10_000 }
    }
}

/// Why an entry is handed back to the engine now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseReason {
    /// The mod asked (`release`).
    Release,
    /// The mod switched holding off.
    HoldOff,
    /// Holding was switched off after too long without a line from the mod.
    Silence,
    /// This entry was held longer than `max_hold_ms`.
    Timeout,
    /// The control file disappeared.
    ControlGone,
}

impl ReleaseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ReleaseReason::Release => "release",
            ReleaseReason::HoldOff => "hold_off",
            ReleaseReason::Silence => "silence",
            ReleaseReason::Timeout => "timeout",
            ReleaseReason::ControlGone => "control_gone",
        }
    }
}

/// Whether a command arriving now is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    Hold,
    /// Holding is off.
    Off,
    /// Holding is on but not for this kind.
    KindNotHeld,
    /// The ring is full: the command passes through.
    Full,
}

/// What to do with a captured command.
#[derive(Debug, PartialEq, Eq)]
pub enum Action<H> {
    /// Hand it back to the engine (its normal path), in the order the actions come.
    Release { id: HoldId, handle: H, reason: ReleaseReason },
    /// Destroy it: the mod cancelled it.
    Drop { id: HoldId, handle: H },
    /// A deferred command whose build this game applied at the stamp: fire its tool callback with that result (the handle already
    /// holds it), then destroy it. The command itself is never applied.
    Complete { id: HoldId, handle: H, ok: bool },
}

impl<H> Action<H> {
    pub fn id(&self) -> HoldId {
        match self {
            Action::Release { id, .. } | Action::Drop { id, .. } | Action::Complete { id, .. } => *id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Held; the mod has not said when to release it.
    Waiting,
    /// The mod gave a game time (0 = now).
    Scheduled { at: u64 },
    /// Going back to the engine at the next [`HoldMachine::take_due`].
    Due(ReleaseReason),
    /// To be destroyed at the next [`HoldMachine::take_due`].
    Cancelled,
    /// Kept until this game's own stamp apply of the same build ([`HoldMachine::complete`]); never released.
    Deferred,
    /// Took the result of that apply: its callback is fired at the next [`HoldMachine::take_due`].
    Completed { ok: bool },
}

#[derive(Debug)]
struct Entry<H> {
    id: HoldId,
    kind: u8,
    held_ms: u64,
    phase: Phase,
    handle: H,
}

/// At most `MAX_PER_WINDOW` error events per `WINDOW_MS`; the rest are counted and mentioned in the next one.
#[derive(Debug, Default)]
struct ErrorLimiter {
    window_start: u64,
    sent: u32,
    suppressed: u32,
}

impl ErrorLimiter {
    const WINDOW_MS: u64 = 10_000;
    const MAX_PER_WINDOW: u32 = 6;

    /// `Some(n)` when an error may be sent now (n = how many were held back since the last one).
    fn allow(&mut self, now: u64) -> Option<u32> {
        if now.saturating_sub(self.window_start) >= Self::WINDOW_MS {
            self.window_start = now;
            self.sent = 0;
        }
        if self.sent >= Self::MAX_PER_WINDOW {
            self.suppressed += 1;
            return None;
        }
        self.sent += 1;
        Some(std::mem::take(&mut self.suppressed))
    }
}

/// Most events kept for the writer; older ones are dropped (and said so) if the writer falls behind.
const MAX_QUEUED_EVENTS: usize = 512;

#[derive(Debug)]
pub struct HoldMachine<H> {
    cfg: HoldConfig,
    /// The build's kind table: name to kind byte.
    table: Vec<(String, u8)>,
    enabled: bool,
    /// Kind bytes being held, sorted.
    kinds: Vec<u8>,
    /// In id order.
    entries: VecDeque<Entry<H>>,
    next_id: HoldId,
    last_ctl_ms: u64,
    events: Vec<Event>,
    lost_events: u64,
    errors: ErrorLimiter,
}

impl<H> HoldMachine<H> {
    pub fn new(cfg: HoldConfig, table: Vec<(String, u8)>) -> HoldMachine<H> {
        HoldMachine {
            cfg,
            table,
            enabled: false,
            kinds: Vec::new(),
            entries: VecDeque::new(),
            next_id: 1,
            last_ctl_ms: 0,
            events: Vec::new(),
            lost_events: 0,
            errors: ErrorLimiter::default(),
        }
    }

    pub fn config(&self) -> HoldConfig {
        self.cfg
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Commands held (or waiting to leave) right now.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The names of the kinds being held, in table order.
    pub fn held_kind_names(&self) -> Vec<String> {
        self.table.iter().filter(|(_, k)| self.kinds.contains(k)).map(|(n, _)| n.clone()).collect()
    }

    fn push_event(&mut self, e: Event) {
        if self.events.len() >= MAX_QUEUED_EVENTS {
            self.events.remove(0);
            self.lost_events += 1;
        }
        self.events.push(e);
    }

    /// The events produced since the last call, oldest first.
    pub fn drain_events(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if self.lost_events > 0 {
            let detail = format!("{} event(s) lost: the writer fell behind", std::mem::take(&mut self.lost_events));
            out.push(Event::Error { place: "events".into(), detail });
        }
        out.append(&mut self.events);
        out
    }

    /// Reports a problem to the mod, rate limited.
    pub fn error(&mut self, place: &str, detail: impl Into<String>, now: u64) {
        if let Some(held_back) = self.errors.allow(now) {
            let mut detail = detail.into();
            if held_back > 0 {
                detail.push_str(&format!(" (+{held_back} more errors held back)"));
            }
            self.push_event(Event::Error { place: place.to_string(), detail });
        }
    }

    fn kind_of_name(&self, name: &str) -> Option<u8> {
        self.table.iter().find(|(n, _)| n == name).map(|(_, k)| *k)
    }

    /// Applies a line of the control file. Every valid line proves the mod is alive.
    pub fn handle(&mut self, msg: &CtlMessage, now: u64) {
        self.last_ctl_ms = now;
        match msg {
            CtlMessage::HoldOn { kinds } => {
                let mut wanted: Vec<u8> = Vec::new();
                let mut unknown = Vec::new();
                for name in kinds {
                    match self.kind_of_name(name) {
                        Some(k) if !wanted.contains(&k) => wanted.push(k),
                        Some(_) => {}
                        None => unknown.push(name.as_str()),
                    }
                }
                if !unknown.is_empty() {
                    let detail = format!("unknown kind(s): {}", unknown.join(", "));
                    self.error("hold_on", detail, now);
                }
                if wanted.is_empty() {
                    return; // nothing the module knows how to hold: holding stays as it was
                }
                wanted.sort_unstable();
                if !self.enabled || self.kinds != wanted {
                    self.enabled = true;
                    self.kinds = wanted;
                    let kinds = self.held_kind_names();
                    self.push_event(Event::Holding { on: true, kinds, reason: "hold_on" });
                }
            }
            CtlMessage::HoldOff => {
                let was = self.enabled;
                self.enabled = false;
                self.release_all(ReleaseReason::HoldOff);
                if was {
                    self.push_event(Event::Holding { on: false, kinds: Vec::new(), reason: "hold_off" });
                }
            }
            CtlMessage::Release { id, at } => match self.entries.iter_mut().find(|e| e.id == *id) {
                Some(e) => {
                    if matches!(e.phase, Phase::Waiting | Phase::Scheduled { .. }) {
                        e.phase = Phase::Scheduled { at: *at };
                    } // already leaving: a second release is harmless
                }
                None => self.error("release", format!("no held command {id}"), now),
            },
            CtlMessage::Cancel { id } => match self.entries.iter_mut().find(|e| e.id == *id) {
                Some(e) => {
                    if !matches!(e.phase, Phase::Completed { .. }) {
                        e.phase = Phase::Cancelled; // a completed command's callback is fired with its real result instead
                    }
                }
                None => self.error("cancel", format!("no held command {id}"), now),
            },
            CtlMessage::Defer { id } => match self.entries.iter_mut().find(|e| e.id == *id) {
                Some(e) => {
                    if matches!(e.phase, Phase::Waiting | Phase::Scheduled { .. }) {
                        e.phase = Phase::Deferred;
                    }
                }
                None => self.error("defer", format!("no held command {id}"), now),
            },
            CtlMessage::TerrainApply { id, .. } => {
                self.error("terrain_apply", format!("edit {id}: terrain edits are not copied by this build"), now)
            }
            CtlMessage::Ping { n } => self.push_event(Event::Pong { n: *n }),
            CtlMessage::PatchNext { .. } | CtlMessage::InjectNext { .. } => {} // taken by the runtime before the machine (cmd_patch.rs, inject)
        }
    }

    fn release_all(&mut self, reason: ReleaseReason) {
        for e in &mut self.entries {
            if matches!(e.phase, Phase::Waiting | Phase::Scheduled { .. }) {
                e.phase = Phase::Due(reason);
            } else if e.phase == Phase::Deferred {
                // its build is applied at the stamp by this game's own replay: releasing it would build it twice
                e.phase = Phase::Cancelled;
            }
        }
    }

    /// Switches holding off on the module's own initiative and sends everything back.
    fn stop(&mut self, why: &'static str, reason: ReleaseReason, now: u64) {
        let was = self.enabled;
        self.enabled = false;
        let waiting = self.entries.len();
        self.release_all(reason);
        if was || waiting > 0 {
            self.error("hold", format!("{why}: holding is off, {waiting} held command(s) go back to the engine"), now);
        }
        if was {
            self.push_event(Event::Holding { on: false, kinds: Vec::new(), reason: why });
        }
    }

    /// The control file is gone: nobody can ask for a release any more.
    pub fn ctl_gone(&mut self, now: u64) {
        if self.enabled || !self.entries.is_empty() {
            self.stop("control_gone", ReleaseReason::ControlGone, now);
        }
    }

    /// Time passes: silence switches holding off, an old entry is released.
    pub fn tick(&mut self, now: u64) {
        if self.enabled && now.saturating_sub(self.last_ctl_ms) >= self.cfg.silence_ms {
            self.stop("silence", ReleaseReason::Silence, now);
        }
        let max = self.cfg.max_hold_ms;
        let mut timed_out = Vec::new();
        for e in &mut self.entries {
            if matches!(e.phase, Phase::Waiting | Phase::Scheduled { .. }) && now.saturating_sub(e.held_ms) >= max {
                e.phase = Phase::Due(ReleaseReason::Timeout);
                timed_out.push(e.id);
            } else if e.phase == Phase::Deferred && now.saturating_sub(e.held_ms) >= max {
                e.phase = Phase::Cancelled; // never completed: dropped, not applied a second time
                timed_out.push(e.id);
            }
        }
        for id in timed_out {
            self.error("hold", format!("command {id} held for {max} ms: released"), now);
        }
    }

    /// Whether a command of this kind would be held now. Reports a full ring.
    pub fn admit(&mut self, kind: u8, now: u64) -> Admit {
        if !self.enabled {
            Admit::Off
        } else if !self.kinds.contains(&kind) {
            Admit::KindNotHeld
        } else if self.entries.len() >= self.cfg.capacity {
            let capacity = self.cfg.capacity;
            self.error("hold", format!("{capacity} commands already held: this one passes through"), now);
            Admit::Full
        } else {
            Admit::Hold
        }
    }

    /// Takes a captured command into the ring. `Err` gives it back when it may no longer be held (checked again
    /// here, so a caller that captured after [`admit`](Self::admit) cannot overfill the ring or hold while off).
    pub fn hold(&mut self, kind: u8, site: u32, handle: H, now: u64) -> Result<HoldId, H> {
        self.hold_with(kind, site, false, None, handle, now)
    }

    /// As [`hold`](Self::hold); `done` tells the mod that cancelling this command fires the tool's completion callback first.
    pub fn hold_with(
        &mut self,
        kind: u8,
        site: u32,
        done: bool,
        flags: Option<BuildFlags>,
        handle: H,
        now: u64,
    ) -> Result<HoldId, H> {
        if !self.enabled || !self.kinds.contains(&kind) || self.entries.len() >= self.cfg.capacity {
            return Err(handle);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push_back(Entry { id, kind, held_ms: now, phase: Phase::Waiting, handle });
        self.push_event(Event::Held { id, kind, site, ms: now, done, flags });
        Ok(id)
    }

    fn is_due(phase: Phase, clock: Option<u64>) -> bool {
        match phase {
            Phase::Waiting | Phase::Deferred => false,
            Phase::Scheduled { at } => at == 0 || clock.is_none_or(|c| c >= at),
            Phase::Due(_) | Phase::Cancelled | Phase::Completed { .. } => true,
        }
    }

    /// Whether [`take_due`](Self::take_due) would return something.
    pub fn has_due(&self, clock: Option<u64>) -> bool {
        self.entries.iter().any(|e| Self::is_due(e.phase, clock))
    }

    /// Removes and returns what is to leave the ring now, in `(game time, id)` order. `clock` is the game time
    /// when the caller can read it; without it a scheduled entry is due at once.
    pub fn take_due(&mut self, clock: Option<u64>, now: u64) -> Vec<Action<H>> {
        let mut due: Vec<(u64, Entry<H>)> = Vec::new();
        let mut keep = VecDeque::with_capacity(self.entries.len());
        for e in self.entries.drain(..) {
            if Self::is_due(e.phase, clock) {
                let key = match e.phase {
                    Phase::Scheduled { at } => at,
                    _ => 0,
                };
                due.push((key, e));
            } else {
                keep.push_back(e);
            }
        }
        self.entries = keep;
        due.sort_by_key(|(key, e)| (*key, e.id));
        let mut out = Vec::with_capacity(due.len());
        for (_, e) in due {
            match e.phase {
                Phase::Cancelled => {
                    self.push_event(Event::Dropped { id: e.id, reason: "cancel" });
                    out.push(Action::Drop { id: e.id, handle: e.handle });
                }
                Phase::Completed { ok } => out.push(Action::Complete { id: e.id, handle: e.handle, ok }),
                phase => {
                    let reason = match phase {
                        Phase::Due(r) => r,
                        _ => ReleaseReason::Release,
                    };
                    self.push_event(Event::Released { id: e.id, reason: reason.as_str(), ms: now });
                    out.push(Action::Release { id: e.id, handle: e.handle, reason });
                }
            }
        }
        out
    }

    /// This game applied the build of deferred command `id` at the stamp (another command, on the simulation thread): `adopt` gives the
    /// held command that apply's result, and its callback is fired at the next [`take_due`](Self::take_due). False (and `adopt` not
    /// called) when `id` is not deferred (dropped meanwhile, or never deferred).
    pub fn complete(&mut self, id: HoldId, ok: bool, adopt: impl FnOnce(&mut H)) -> bool {
        match self.entries.iter_mut().find(|e| e.id == id && e.phase == Phase::Deferred) {
            Some(e) => {
                adopt(&mut e.handle);
                e.phase = Phase::Completed { ok };
                true
            }
            None => false,
        }
    }

    /// For the tests and diagnostics: the held entries as `(id, kind)`.
    pub fn held(&self) -> Vec<(HoldId, u8)> {
        self.entries.iter().map(|e| (e.id, e.kind)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUILD: u8 = 52;
    const OTHER: u8 = 9;

    fn machine() -> HoldMachine<u64> {
        HoldMachine::new(
            HoldConfig { capacity: 4, max_hold_ms: 30_000, silence_ms: 10_000 },
            vec![("build_proposal".into(), BUILD), ("other".into(), OTHER)],
        )
    }

    fn hold_on(m: &mut HoldMachine<u64>, names: &[&str], now: u64) {
        m.handle(&CtlMessage::HoldOn { kinds: names.iter().map(|s| s.to_string()).collect() }, now);
    }

    fn hold(m: &mut HoldMachine<u64>, kind: u8, now: u64) -> HoldId {
        let id = m.next_id;
        m.hold(kind, 77, id, now).expect("held")
    }

    fn ids<H>(actions: &[Action<H>]) -> Vec<HoldId> {
        actions.iter().map(Action::id).collect()
    }

    fn kinds_of(events: &[Event]) -> Vec<&'static str> {
        events.iter().map(Event::kind).collect()
    }

    #[test]
    fn off_by_default_and_everything_passes() {
        let mut m = machine();
        assert!(!m.enabled());
        assert_eq!(m.admit(BUILD, 0), Admit::Off);
        assert_eq!(m.hold(BUILD, 1, 5, 0), Err(5), "a late capture is given back");
        assert!(m.take_due(None, 0).is_empty());
        assert!(m.drain_events().is_empty());
        assert!(m.is_empty());
    }

    #[test]
    fn hold_on_selects_kinds_by_name() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 100);
        assert!(m.enabled());
        assert_eq!(m.admit(BUILD, 100), Admit::Hold);
        assert_eq!(m.admit(OTHER, 100), Admit::KindNotHeld);
        assert_eq!(m.admit(7, 100), Admit::KindNotHeld, "an unrecognised kind is never held");
        let ev = m.drain_events();
        assert_eq!(ev, [Event::Holding { on: true, kinds: vec!["build_proposal".into()], reason: "hold_on" }]);
        hold_on(&mut m, &["build_proposal"], 200);
        assert!(m.drain_events().is_empty(), "repeating the same hold_on says nothing");
        hold_on(&mut m, &["build_proposal", "other"], 300);
        assert_eq!(m.admit(OTHER, 300), Admit::Hold);
        assert_eq!(kinds_of(&m.drain_events()), ["holding"], "a changed set is announced");
    }

    #[test]
    fn unknown_names_are_reported_and_known_ones_still_apply() {
        let mut m = machine();
        hold_on(&mut m, &["nonsense", "build_proposal"], 0);
        assert!(m.enabled());
        let ev = m.drain_events();
        assert_eq!(kinds_of(&ev), ["error", "holding"]);
        assert!(matches!(&ev[0], Event::Error { place, detail } if place == "hold_on" && detail.contains("nonsense")));
        let mut m2 = machine();
        hold_on(&mut m2, &["nonsense"], 0);
        assert!(!m2.enabled(), "nothing resolvable: holding does not start");
        assert_eq!(kinds_of(&m2.drain_events()), ["error"]);
    }

    #[test]
    fn a_held_command_gets_an_id_and_an_event() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        m.drain_events();
        let a = hold(&mut m, BUILD, 10);
        let b = hold(&mut m, BUILD, 20);
        assert_eq!((a, b), (1, 2));
        assert_eq!(m.held(), [(1, BUILD), (2, BUILD)]);
        assert_eq!(
            m.drain_events(),
            [
                Event::Held { id: 1, kind: BUILD, site: 77, ms: 10, done: false, flags: None },
                Event::Held { id: 2, kind: BUILD, site: 77, ms: 20, done: false, flags: None }
            ]
        );
    }

    #[test]
    fn release_with_a_time_zero_is_due_at_the_next_take() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let id = hold(&mut m, BUILD, 5);
        assert!(!m.has_due(None));
        assert!(m.take_due(None, 6).is_empty(), "nothing leaves before the mod says so");
        m.handle(&CtlMessage::Release { id, at: 0 }, 7);
        assert!(m.has_due(None));
        let out = m.take_due(None, 8);
        assert_eq!(out, [Action::Release { id, handle: id, reason: ReleaseReason::Release }]);
        assert!(m.is_empty());
        let ev = m.drain_events();
        assert_eq!(ev.last(), Some(&Event::Released { id, reason: "release", ms: 8 }));
        assert!(m.take_due(None, 9).is_empty(), "an entry leaves once");
    }

    #[test]
    fn a_scheduled_release_waits_for_the_game_clock() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let id = hold(&mut m, BUILD, 5);
        m.handle(&CtlMessage::Release { id, at: 12_000 }, 6);
        assert!(!m.has_due(Some(11_800)));
        assert!(m.take_due(Some(11_800), 7).is_empty());
        assert!(m.has_due(Some(12_000)));
        assert_eq!(ids(&m.take_due(Some(12_000), 8)), [id]);
    }

    #[test]
    fn without_a_clock_a_scheduled_release_is_due_at_once() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let id = hold(&mut m, BUILD, 5);
        m.handle(&CtlMessage::Release { id, at: 99_999_999 }, 6);
        assert_eq!(ids(&m.take_due(None, 7)), [id], "no clock: the mod has already stopped the game at the stamp");
    }

    #[test]
    fn entries_leave_in_game_time_then_id_order() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let (a, b, c, d) =
            (hold(&mut m, BUILD, 1), hold(&mut m, BUILD, 2), hold(&mut m, BUILD, 3), hold(&mut m, BUILD, 4));
        m.handle(&CtlMessage::Release { id: d, at: 400 }, 5);
        m.handle(&CtlMessage::Release { id: b, at: 400 }, 5);
        m.handle(&CtlMessage::Release { id: a, at: 600 }, 5);
        m.handle(&CtlMessage::Release { id: c, at: 200 }, 5);
        assert_eq!(ids(&m.take_due(Some(10_000), 6)), [c, b, d, a]);
    }

    #[test]
    fn a_later_release_for_a_scheduled_entry_replaces_its_time() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let id = hold(&mut m, BUILD, 1);
        m.handle(&CtlMessage::Release { id, at: 5_000 }, 2);
        m.handle(&CtlMessage::Release { id, at: 800 }, 3);
        assert_eq!(ids(&m.take_due(Some(800), 4)), [id]);
    }

    #[test]
    fn hold_off_releases_everything_in_order_and_stops_holding() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let (a, b, c) = (hold(&mut m, BUILD, 1), hold(&mut m, BUILD, 2), hold(&mut m, BUILD, 3));
        m.handle(&CtlMessage::Release { id: c, at: 100 }, 4); // scheduled, not due yet by the clock
        m.drain_events();
        m.handle(&CtlMessage::HoldOff, 5);
        assert!(!m.enabled());
        assert_eq!(m.admit(BUILD, 5), Admit::Off);
        let out = m.take_due(Some(0), 6);
        assert_eq!(ids(&out), [a, b, c], "all of them, in the order they were held, whatever the clock says");
        assert!(out.iter().all(|a| matches!(a, Action::Release { reason: ReleaseReason::HoldOff, .. })));
        assert_eq!(kinds_of(&m.drain_events()), ["holding", "released", "released", "released"]);
    }

    #[test]
    fn hold_off_while_off_says_nothing() {
        let mut m = machine();
        m.handle(&CtlMessage::HoldOff, 0);
        assert!(m.drain_events().is_empty());
    }

    #[test]
    fn cancel_drops_the_command_without_releasing_it() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let a = hold(&mut m, BUILD, 1);
        let b = hold(&mut m, BUILD, 2);
        m.drain_events();
        m.handle(&CtlMessage::Cancel { id: a }, 3);
        let out = m.take_due(None, 4);
        assert_eq!(out, [Action::Drop { id: a, handle: a }]);
        assert_eq!(m.held(), [(b, BUILD)], "the other one stays");
        assert_eq!(m.drain_events(), [Event::Dropped { id: a, reason: "cancel" }]);
    }

    #[test]
    fn cancel_wins_over_a_release_that_has_not_been_taken_yet() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let a = hold(&mut m, BUILD, 1);
        m.handle(&CtlMessage::Release { id: a, at: 0 }, 2);
        m.handle(&CtlMessage::Cancel { id: a }, 3);
        assert_eq!(m.take_due(None, 4), [Action::Drop { id: a, handle: a }]);
        // and the other way round: a release after a cancel does not undo it
        let b = hold(&mut m, BUILD, 5);
        m.handle(&CtlMessage::Cancel { id: b }, 6);
        m.handle(&CtlMessage::Release { id: b, at: 0 }, 7);
        assert_eq!(m.take_due(None, 8), [Action::Drop { id: b, handle: b }]);
    }

    #[test]
    fn a_deferred_command_is_never_released_and_completes_with_the_stamp_applys_result() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let a = hold(&mut m, BUILD, 1);
        let b = hold(&mut m, BUILD, 2);
        m.handle(&CtlMessage::Defer { id: a }, 3);
        m.handle(&CtlMessage::Release { id: a, at: 0 }, 4);
        assert!(m.take_due(None, 5).is_empty(), "a release does not undo a defer");
        let mut adopted = 0;
        assert!(m.complete(a, true, |h| adopted = *h));
        assert_eq!(adopted, a);
        assert!(!m.complete(a, true, |_| panic!("once")), "completed already");
        assert!(!m.complete(b, true, |_| panic!("not deferred")));
        assert_eq!(m.take_due(None, 6), [Action::Complete { id: a, handle: a, ok: true }]);
        // holding off or a timeout drops a deferred command instead of releasing it
        m.handle(&CtlMessage::Defer { id: b }, 7);
        m.handle(&CtlMessage::HoldOff, 8);
        assert_eq!(m.take_due(None, 9), [Action::Drop { id: b, handle: b }]);
        hold_on(&mut m, &["build_proposal"], 10);
        let c = hold(&mut m, BUILD, 10);
        m.handle(&CtlMessage::Defer { id: c }, 11);
        m.handle(&CtlMessage::Ping { n: 1 }, 30_000);
        m.tick(30_010);
        assert_eq!(m.take_due(None, 30_010), [Action::Drop { id: c, handle: c }]);
    }

    #[test]
    fn unknown_ids_are_reported() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        m.drain_events();
        m.handle(&CtlMessage::Release { id: 9, at: 0 }, 1);
        m.handle(&CtlMessage::Cancel { id: 9 }, 1);
        let ev = m.drain_events();
        assert_eq!(kinds_of(&ev), ["error", "error"]);
    }

    #[test]
    fn ping_is_answered_and_counts_as_a_sign_of_life() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        m.drain_events();
        m.handle(&CtlMessage::Ping { n: 5 }, 9_000);
        assert_eq!(m.drain_events(), [Event::Pong { n: 5 }]);
        m.tick(18_000); // 9 s after the ping: not silent yet
        assert!(m.enabled());
        m.tick(19_000);
        assert!(!m.enabled(), "10 s after the ping");
    }

    #[test]
    fn silence_switches_holding_off_and_releases_everything() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 1_000);
        let (a, b) = (hold(&mut m, BUILD, 2_000), hold(&mut m, BUILD, 3_000));
        m.drain_events();
        m.tick(10_999);
        assert!(m.enabled(), "one millisecond short of the limit");
        m.tick(11_000);
        assert!(!m.enabled());
        let ev = m.drain_events();
        assert_eq!(kinds_of(&ev), ["error", "holding"]);
        assert!(matches!(&ev[1], Event::Holding { on: false, reason: "silence", .. }));
        let out = m.take_due(None, 11_001);
        assert_eq!(ids(&out), [a, b]);
        assert!(out.iter().all(|x| matches!(x, Action::Release { reason: ReleaseReason::Silence, .. })));
        assert_eq!(m.admit(BUILD, 11_002), Admit::Off, "new commands pass again");
    }

    #[test]
    fn silence_with_holding_already_off_does_nothing() {
        let mut m = machine();
        m.tick(1_000_000);
        assert!(m.drain_events().is_empty());
    }

    #[test]
    fn an_old_entry_is_released_on_its_own_even_while_the_mod_talks() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let a = hold(&mut m, BUILD, 1_000);
        m.handle(&CtlMessage::Ping { n: 1 }, 29_000);
        let b = hold(&mut m, BUILD, 29_500);
        m.drain_events();
        m.tick(30_999);
        assert!(m.take_due(None, 30_999).is_empty());
        m.tick(31_000);
        let out = m.take_due(None, 31_000);
        assert_eq!(out, [Action::Release { id: a, handle: a, reason: ReleaseReason::Timeout }]);
        assert!(m.enabled(), "a timeout does not switch holding off");
        assert_eq!(m.held(), [(b, BUILD)]);
    }

    #[test]
    fn a_full_ring_lets_the_next_command_through_with_an_error() {
        let mut m = machine(); // capacity 4
        hold_on(&mut m, &["build_proposal"], 0);
        for t in 1..=4 {
            hold(&mut m, BUILD, t);
        }
        m.drain_events();
        assert_eq!(m.admit(BUILD, 10), Admit::Full);
        assert_eq!(m.hold(BUILD, 1, 99, 10), Err(99), "and a capture that raced is given back");
        assert_eq!(m.len(), 4);
        assert_eq!(kinds_of(&m.drain_events()), ["error"]);
    }

    #[test]
    fn the_control_file_disappearing_releases_everything() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let a = hold(&mut m, BUILD, 1);
        m.drain_events();
        m.ctl_gone(5);
        assert!(!m.enabled());
        let out = m.take_due(None, 6);
        assert_eq!(out, [Action::Release { id: a, handle: a, reason: ReleaseReason::ControlGone }]);
        let mut idle = machine();
        idle.ctl_gone(5);
        assert!(idle.drain_events().is_empty(), "nothing held and not holding: nothing to say");
    }

    #[test]
    fn terrain_apply_is_refused_politely() {
        let mut m = machine();
        m.handle(&CtlMessage::TerrainApply { id: 1, stamp: 400 }, 0);
        assert!(matches!(&m.drain_events()[..], [Event::Error { place, .. }] if place == "terrain_apply"));
    }

    #[test]
    fn errors_are_rate_limited_and_the_next_one_says_how_many_were_held_back() {
        let mut m = machine();
        for i in 0..20 {
            m.error("test", format!("problem {i}"), 1_000);
        }
        let ev = m.drain_events();
        assert_eq!(ev.len(), ErrorLimiter::MAX_PER_WINDOW as usize);
        m.error("test", "later", 1_000 + ErrorLimiter::WINDOW_MS);
        let ev = m.drain_events();
        assert!(matches!(&ev[..], [Event::Error { detail, .. }] if detail.contains("14 more errors held back")));
    }

    #[test]
    fn the_event_queue_is_bounded() {
        let mut m = machine();
        for n in 0..(MAX_QUEUED_EVENTS as i64 + 10) {
            m.handle(&CtlMessage::Ping { n }, 0);
        }
        let ev = m.drain_events();
        assert_eq!(ev.len(), MAX_QUEUED_EVENTS + 1);
        assert!(matches!(&ev[0], Event::Error { place, .. } if place == "events"));
        assert_eq!(ev[1], Event::Pong { n: 10 }, "the oldest were dropped");
    }

    #[test]
    fn hold_off_then_hold_on_again_starts_clean() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let a = hold(&mut m, BUILD, 1);
        m.handle(&CtlMessage::HoldOff, 2);
        assert_eq!(ids(&m.take_due(None, 3)), [a]);
        hold_on(&mut m, &["build_proposal"], 4);
        let b = hold(&mut m, BUILD, 5);
        assert_eq!(b, a + 1, "ids keep growing");
    }

    /// A small deterministic generator: the property test needs reproducible randomness, not quality.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self, below: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) % below
        }
    }

    #[test]
    fn random_use_never_loses_or_repeats_a_command_and_never_overfills() {
        let mut total = 0;
        for seed in 1..=40u64 {
            let mut rng = Rng(seed);
            let mut m = machine();
            let mut now = 0u64;
            let mut held_ids = Vec::new();
            let mut left = Vec::new();
            for _ in 0..400 {
                now += rng.next(2_500);
                match rng.next(9) {
                    0 => hold_on(&mut m, &["build_proposal"], now),
                    1 => m.handle(&CtlMessage::HoldOff, now),
                    2 | 3 => {
                        if m.admit(BUILD, now) == Admit::Hold {
                            let id = m.next_id;
                            held_ids.push(m.hold(BUILD, 1, id, now).expect("room"));
                        }
                    }
                    4 => m.handle(
                        &CtlMessage::Release { id: 1 + rng.next(held_ids.len() as u64 + 2), at: rng.next(3) * 200 },
                        now,
                    ),
                    5 => m.handle(&CtlMessage::Cancel { id: 1 + rng.next(held_ids.len() as u64 + 2) }, now),
                    6 => m.handle(&CtlMessage::Ping { n: 1 }, now),
                    7 => m.tick(now),
                    _ => {
                        let clock = if rng.next(2) == 0 { None } else { Some(rng.next(1_000)) };
                        let out = m.take_due(clock, now);
                        let order: Vec<_> = out.iter().map(Action::id).collect();
                        left.extend(order);
                    }
                }
                assert!(m.len() <= 4, "seed {seed}: ring overfilled");
            }
            m.handle(&CtlMessage::HoldOff, now);
            left.extend(ids(&m.take_due(None, now)));
            assert!(m.is_empty(), "seed {seed}");
            left.sort_unstable();
            assert_eq!(left, held_ids, "seed {seed}: every held command left exactly once");
            total += held_ids.len();
        }
        assert!(total > 300, "the random runs held too little to mean anything ({total})");
    }
}
