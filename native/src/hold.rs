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
//! - A silence is not a `hold_off`: the next line from the mod switches holding on again with the same kinds
//!   (`holding {on = true, reason = "resume"}`). The game's pause menu stops every game script, so the mod falls
//!   silent while the world is still there (real runs inj7 and s3, 2026-10-14: holding went off at the menu and was
//!   never on again afterwards; builds then went the old way and the worlds drifted).
//! - A silence keeps what is held: the entries wait, and the time the mod is silent does not count towards their age
//!   (a pause menu longer than the age limit right after a build click must not drop or release the build that waits
//!   for its stamp). A held kind sent by a tool while the mod is silent proves the game is not paused but the mod is
//!   gone: everything is then given back as before.
//! - A session pause is not a silence (the mod keeps pinging) but its game time stands still too: while the mod's pings say
//!   `paused`, nothing held ages (a build deferred at a pause waits minutes for the host's pause stamp). Every other way out stays.
//! - A deferred command never leaves without its tool callback: completed with the result of this game's stamp apply,
//!   or, on every other way out (`fail`, `cancel`, holding off, the age limit, the control file gone), failed: the callback
//!   is fired with the command's own "not applied" result, so a tool is never left busy.
//! - The completion with the stamp apply's result waits until the mod says that the interface's world has that apply
//!   (`release` of the deferred id, sent once the interface's game time is past the stamp): the tool's callback reads the
//!   entities of its result in the interface's world, which copies the simulation's changes a little later (h1/h2,
//!   2026-10-14: with a 3 ms capture and a 2 ms rebuild, the next Add came before that copy, the road tool's callback asked
//!   for a component of a removed town building in the interface's world and the game stopped on `it != components.end()`).

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
    /// A command that leaves without a stamp apply of its own (refused, failed, too old, holding off): fire its tool callback with
    /// the command's own "not applied" result, then destroy it, so that the tool is free at once. `reason` is for the log.
    Fail { id: HoldId, handle: H, reason: &'static str },
}

impl<H> Action<H> {
    pub fn id(&self) -> HoldId {
        match self {
            Action::Release { id, .. }
            | Action::Drop { id, .. }
            | Action::Complete { id, .. }
            | Action::Fail { id, .. } => *id,
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
    /// Took the result of that apply: its callback is fired at the next [`HoldMachine::take_due`] once the entry is `ready`.
    Completed { ok: bool },
    /// Leaves without a stamp apply: its callback is fired with a failed result at the next [`HoldMachine::take_due`].
    Failed(&'static str),
}

#[derive(Debug)]
struct Entry<H> {
    id: HoldId,
    kind: u8,
    held_ms: u64,
    /// The machine's still time ([`HoldMachine::still_total`]) when this entry was held: the age of an entry leaves out the time the
    /// mod was silent and the time the session's game time stood still.
    silent_at_hold: u64,
    phase: Phase,
    /// The interface's world has this game's stamp apply (the mod's `release` of a deferred entry, or no mod left to say it): a
    /// completed entry's callback may be fired.
    ready: bool,
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
    /// The kinds that were held when a silence switched holding off: the mod's next line holds them again.
    suspended: Option<Vec<u8>>,
    /// Since when the mod is silent (its last line), while a silence lasts.
    silent_since: Option<u64>,
    /// Since when the session's game time stands still (the mod's ping says `paused`), while the session pause lasts.
    paused_since: Option<u64>,
    /// The still time of all earlier silences and session pauses, in milliseconds.
    silent_total: u64,
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
            suspended: None,
            silent_since: None,
            paused_since: None,
            silent_total: 0,
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
        if let Some(since) = self.silent_since.take() {
            self.silent_total = self.silent_total.saturating_add(now.saturating_sub(since));
        }
        if let Some(kinds) = self.suspended.take() {
            // the mod speaks again after a silence: holding comes back as it was, unless this line says otherwise
            if !matches!(msg, CtlMessage::HoldOn { .. } | CtlMessage::HoldOff) && !self.enabled && !kinds.is_empty() {
                self.enabled = true;
                self.kinds = kinds;
                let kinds = self.held_kind_names();
                self.push_event(Event::Holding { on: true, kinds, reason: "resume" });
            }
        }
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
                self.session_paused(false, now); // the session is over: what is held leaves now, failed if deferred
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
                    } else if matches!(e.phase, Phase::Deferred | Phase::Completed { .. }) {
                        e.ready = true; // a deferred command is never released: the interface's world has the stamp apply
                    } // already leaving: a second release is harmless
                }
                None => self.error("release", format!("no held command {id}"), now),
            },
            CtlMessage::Cancel { id } => match self.entries.iter_mut().find(|e| e.id == *id) {
                Some(e) => match e.phase {
                    Phase::Completed { .. } | Phase::Failed(_) => {} // its callback is fired with its result already
                    Phase::Deferred => e.phase = Phase::Failed("cancel"), // never leave a deferred tool without its callback
                    _ => e.phase = Phase::Cancelled,
                },
                None => self.error("cancel", format!("no held command {id}"), now),
            },
            CtlMessage::Fail { id } => match self.entries.iter_mut().find(|e| e.id == *id) {
                Some(e) => {
                    if matches!(e.phase, Phase::Waiting | Phase::Scheduled { .. } | Phase::Deferred) {
                        e.phase = Phase::Failed("fail");
                    }
                }
                None => self.error("fail", format!("no held command {id}"), now),
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
            CtlMessage::Ping { n, paused } => {
                self.session_paused(*paused, now);
                self.push_event(Event::Pong { n: *n });
            }
            CtlMessage::PatchNext { .. }
            | CtlMessage::InjectNext { .. }
            | CtlMessage::CompleteNext { .. }
            | CtlMessage::Pace { .. } => {} // taken by the runtime before the machine (cmd_patch.rs, inject, pace.rs)
        }
    }

    fn release_all(&mut self, reason: ReleaseReason) {
        for e in &mut self.entries {
            e.ready = true; // nobody is left to say when the interface's world is ready: a completed entry fires as before
            if matches!(e.phase, Phase::Waiting | Phase::Scheduled { .. }) {
                e.phase = Phase::Due(reason);
            } else if e.phase == Phase::Deferred {
                // its build is applied at the stamp by this game's own replay: releasing it would build it twice; its tool callback is
                // fired with a failed result so that the tool is not left busy
                e.phase = Phase::Failed(reason.as_str());
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
        self.suspended = None;
        self.session_paused(false, now);
        if let Some(since) = self.silent_since.take() {
            self.silent_total = self.silent_total.saturating_add(now.saturating_sub(since));
        }
        if self.enabled || !self.entries.is_empty() {
            self.stop("control_gone", ReleaseReason::ControlGone, now);
        }
    }

    /// The session's game time stands still (`paused`, from the mod's ping) or runs again. While it stands, nothing held ages: a
    /// session pause can last minutes (the host waits for every game's stop report and a round trip before it stamps the builds of
    /// the pause), and a build deferred at the pause waits for that stamp; the mod still pings meanwhile, so this is not a silence.
    /// Every other way out stays: `hold_off` (the session ends), the control file gone, `fail`, `cancel`.
    fn session_paused(&mut self, paused: bool, now: u64) {
        match (paused, self.paused_since) {
            (true, None) => self.paused_since = Some(now),
            (false, Some(since)) => {
                self.paused_since = None;
                self.silent_total = self.silent_total.saturating_add(now.saturating_sub(since));
            }
            _ => {}
        }
    }

    /// The machine's still time so far: every silence of the mod and every session pause, the open ones up to `now`.
    fn still_total(&self, now: u64) -> u64 {
        let open = |since: Option<u64>| since.map_or(0, |s| now.saturating_sub(s));
        self.silent_total.saturating_add(open(self.silent_since)).saturating_add(open(self.paused_since))
    }

    /// How long an entry has been held, without the time the mod was silent (the pause menu stops the mod's script and the game) and
    /// without the time the session's game time stood still (a session pause).
    fn age(&self, e: &Entry<H>, now: u64) -> u64 {
        let still = self.still_total(now).saturating_sub(e.silent_at_hold);
        now.saturating_sub(e.held_ms).saturating_sub(still)
    }

    /// Time passes: silence suspends holding (what is held waits, its age stands still), an old entry is released.
    pub fn tick(&mut self, now: u64) {
        if self.enabled && now.saturating_sub(self.last_ctl_ms) >= self.cfg.silence_ms {
            // The game's pause menu stops every game script and the simulation: the mod is silent while its world is still there.
            // What is held waits for the mod's next line (a pause menu longer than the age limit right after a build click dropped
            // the deferred build before, 2026-10-14); new commands pass meanwhile (see `tool_while_silent`).
            self.suspended = Some(self.kinds.clone());
            // a session pause open at the mod's last line ends there: from then on the silence counts (never both at once)
            self.session_paused(false, self.last_ctl_ms);
            self.silent_since = Some(self.last_ctl_ms);
            self.enabled = false;
            let waiting = self.entries.len();
            self.error(
                "hold",
                format!("silence: holding is suspended until the mod speaks again; {waiting} held command(s) wait, their age stands still"),
                now,
            );
            self.push_event(Event::Holding { on: false, kinds: Vec::new(), reason: "silence" });
        }
        if self.silent_since.is_some() {
            return; // nothing ages while the mod is silent
        }
        let max = self.cfg.max_hold_ms;
        let (mut timed_out, mut unconfirmed) = (Vec::new(), Vec::new());
        let ages: Vec<u64> = self.entries.iter().map(|e| self.age(e, now)).collect();
        for (e, age) in self.entries.iter_mut().zip(ages) {
            if matches!(e.phase, Phase::Waiting | Phase::Scheduled { .. }) && age >= max {
                e.phase = Phase::Due(ReleaseReason::Timeout);
                timed_out.push(e.id);
            } else if e.phase == Phase::Deferred && age >= max {
                e.phase = Phase::Failed("timeout"); // never completed: not applied a second time, its tool callback fired as failed
                timed_out.push(e.id);
            } else if matches!(e.phase, Phase::Completed { .. }) && !e.ready && age >= max {
                e.ready = true; // the mod never said the interface's world was ready: the callback is fired now, not never
                unconfirmed.push(e.id);
            }
        }
        for id in timed_out {
            self.error("hold", format!("command {id} held for {max} ms: released"), now);
        }
        for id in unconfirmed {
            self.error(
                "hold",
                format!("command {id} completed but not confirmed by the mod for {max} ms: its callback is fired now"),
                now,
            );
        }
    }

    /// A tool sent a command of a kind that was held before a silence, while the mod is still silent: the game is not paused (a tool
    /// works), the mod is gone. Everything held goes back the old way (released; a deferred command fails), as a silence did before.
    pub fn tool_while_silent(&mut self, kind: u8, now: u64) {
        let Some(kinds) = &self.suspended else { return };
        if self.enabled || !kinds.contains(&kind) || self.entries.is_empty() {
            return;
        }
        let waiting = self.entries.len();
        self.release_all(ReleaseReason::Silence);
        self.error(
            "hold",
            format!(
                "a tool works while the mod is silent (not a pause): {waiting} held command(s) go back to the engine"
            ),
            now,
        );
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
        let silent_at_hold = self.still_total(now);
        self.entries.push_back(Entry {
            id,
            kind,
            held_ms: now,
            silent_at_hold,
            phase: Phase::Waiting,
            ready: false,
            handle,
        });
        self.push_event(Event::Held { id, kind, site, ms: now, done, flags });
        Ok(id)
    }

    fn is_due(e: &Entry<H>, clock: Option<u64>) -> bool {
        match e.phase {
            Phase::Waiting | Phase::Deferred => false,
            Phase::Scheduled { at } => at == 0 || clock.is_none_or(|c| c >= at),
            Phase::Completed { .. } => e.ready,
            Phase::Due(_) | Phase::Cancelled | Phase::Failed(_) => true,
        }
    }

    /// Whether [`take_due`](Self::take_due) would return something.
    pub fn has_due(&self, clock: Option<u64>) -> bool {
        self.entries.iter().any(|e| Self::is_due(e, clock))
    }

    /// Removes and returns what is to leave the ring now, in `(game time, id)` order. `clock` is the game time
    /// when the caller can read it; without it a scheduled entry is due at once.
    pub fn take_due(&mut self, clock: Option<u64>, now: u64) -> Vec<Action<H>> {
        let mut due: Vec<(u64, Entry<H>)> = Vec::new();
        let mut keep = VecDeque::with_capacity(self.entries.len());
        for e in self.entries.drain(..) {
            if Self::is_due(&e, clock) {
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
                Phase::Failed(reason) => out.push(Action::Fail { id: e.id, handle: e.handle, reason }),
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
    /// held command that apply's result, and its callback is fired at the [`take_due`](Self::take_due) after the mod's `release` of it
    /// (the interface's world has the apply). False (and `adopt` not
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

    /// Held command `id`, whatever its phase (an order of its build identifies the script's command by it: `inject::same_command`).
    pub fn handle_of(&self, id: HoldId) -> Option<&H> {
        self.entries.iter().find(|e| e.id == id).map(|e| &e.handle)
    }

    /// Held command `id` is still held and not leaving (waiting, scheduled or deferred; not failed, cancelled, completed or gone): a
    /// stamp apply of its build may still be due for it.
    pub fn is_kept(&self, id: HoldId) -> bool {
        self.entries
            .iter()
            .any(|e| e.id == id && matches!(e.phase, Phase::Waiting | Phase::Scheduled { .. } | Phase::Deferred))
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
        // holding off or a timeout fails a deferred command (its callback fired as failed) instead of releasing it
        m.handle(&CtlMessage::Defer { id: b }, 7);
        m.handle(&CtlMessage::HoldOff, 8);
        assert_eq!(m.take_due(None, 9), [Action::Fail { id: b, handle: b, reason: "hold_off" }]);
        hold_on(&mut m, &["build_proposal"], 10);
        let c = hold(&mut m, BUILD, 10);
        m.handle(&CtlMessage::Defer { id: c }, 11);
        m.handle(&CtlMessage::Ping { n: 1, paused: false }, 30_000);
        m.tick(30_010);
        assert_eq!(m.take_due(None, 30_010), [Action::Fail { id: c, handle: c, reason: "timeout" }]);
    }

    #[test]
    fn a_completed_command_fires_only_once_the_mod_says_the_interface_has_the_apply() {
        // h1/h2 (2026-10-14): the callback fired at the first Add after the simulation's apply, before the interface's world had it; the
        // road tool read a removed town building there and the game stopped
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let a = hold(&mut m, BUILD, 1);
        m.handle(&CtlMessage::Defer { id: a }, 2);
        assert!(m.complete(a, true, |_| {}));
        assert!(!m.has_due(None) && m.take_due(None, 3).is_empty(), "completed, but the interface has not said so");
        m.handle(&CtlMessage::Release { id: a, at: 0 }, 4);
        assert_eq!(m.take_due(None, 5), [Action::Complete { id: a, handle: a, ok: true }]);
        // the mod gone (holding off): a completed entry fires as before
        let b = hold(&mut m, BUILD, 6);
        m.handle(&CtlMessage::Defer { id: b }, 7);
        assert!(m.complete(b, true, |_| {}));
        m.handle(&CtlMessage::HoldOff, 8);
        assert_eq!(m.take_due(None, 9), [Action::Complete { id: b, handle: b, ok: true }]);
        // the mod never confirms: the age limit fires it, it is never left busy
        hold_on(&mut m, &["build_proposal"], 10);
        let c = hold(&mut m, BUILD, 10);
        m.handle(&CtlMessage::Defer { id: c }, 11);
        assert!(m.complete(c, false, |_| {}));
        m.handle(&CtlMessage::Ping { n: 1, paused: false }, 30_000);
        m.tick(30_000);
        assert!(m.take_due(None, 30_000).is_empty());
        m.handle(&CtlMessage::Ping { n: 2, paused: false }, 30_010);
        m.tick(30_010);
        assert_eq!(m.take_due(None, 30_010), [Action::Complete { id: c, handle: c, ok: false }]);
    }

    #[test]
    fn fail_and_cancel_free_a_deferred_tool_at_once() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let (a, b, c) = (hold(&mut m, BUILD, 1), hold(&mut m, BUILD, 2), hold(&mut m, BUILD, 3));
        m.handle(&CtlMessage::Defer { id: a }, 4);
        m.handle(&CtlMessage::Defer { id: b }, 4);
        m.handle(&CtlMessage::Fail { id: a }, 5);
        m.handle(&CtlMessage::Cancel { id: b }, 5);
        m.handle(&CtlMessage::Cancel { id: c }, 5); // not deferred: an ordinary cancel
        assert_eq!(
            m.take_due(None, 6),
            [
                Action::Fail { id: a, handle: a, reason: "fail" },
                Action::Fail { id: b, handle: b, reason: "cancel" },
                Action::Drop { id: c, handle: c }
            ]
        );
        assert!(!m.complete(a, true, |_| panic!("gone")), "a failed command cannot complete later");
    }

    #[test]
    fn a_long_pause_keeps_a_deferred_command_and_its_age_stands_still() {
        // a 45 s pause menu right after the click: the mod falls silent, nothing is dropped, and the age limit counts only the time the
        // mod spoke
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let a = hold(&mut m, BUILD, 1_000);
        m.handle(&CtlMessage::Defer { id: a }, 1_100);
        m.tick(11_100); // 10 s silent: suspended
        assert!(!m.enabled());
        m.tick(56_000);
        assert!(m.take_due(None, 56_000).is_empty(), "nothing leaves during the pause");
        m.handle(&CtlMessage::Ping { n: 1, paused: false }, 56_100); // the menu is closed
        assert!(m.enabled(), "resumed");
        m.tick(56_200);
        assert!(m.take_due(None, 56_200).is_empty(), "about 0.1 s of age counted before the pause, not 55 s");
        assert!(m.complete(a, true, |_| {}));
        m.handle(&CtlMessage::Release { id: a, at: 0 }, 56_250); // the interface's world has the apply
        assert_eq!(m.take_due(None, 56_300), [Action::Complete { id: a, handle: a, ok: true }]);
        // and the age limit still holds for time the mod spoke: 30 s after the resume
        let b = hold(&mut m, BUILD, 57_000);
        m.handle(&CtlMessage::Defer { id: b }, 57_000);
        m.handle(&CtlMessage::Ping { n: 2, paused: false }, 86_000);
        m.tick(86_999);
        assert!(m.take_due(None, 86_999).is_empty());
        m.handle(&CtlMessage::Ping { n: 3, paused: false }, 87_000);
        m.tick(87_000);
        assert_eq!(m.take_due(None, 87_000), [Action::Fail { id: b, handle: b, reason: "timeout" }]);
    }

    #[test]
    fn a_deferred_command_waits_through_five_minutes_of_a_session_pause_with_pings_and_still_times_out_while_running() {
        // a build deferred at a session pause: the host waits for every game's stop report and a round trip before it stamps it; the mod
        // keeps pinging (no silence), with `paused`
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let a = hold(&mut m, BUILD, 1_000);
        m.handle(&CtlMessage::Defer { id: a }, 1_100);
        m.handle(&CtlMessage::Ping { n: 1, paused: true }, 2_000);
        let mut n = 1;
        for t in (4_000..=302_000).step_by(2_000) {
            n += 1;
            m.handle(&CtlMessage::Ping { n, paused: true }, t);
            m.tick(t);
            assert!(m.enabled(), "pings: no silence");
            assert!(m.take_due(None, t).is_empty(), "nothing leaves during the session pause (t = {t})");
        }
        // the pause ends; the stamp comes a moment later: 1 s of age before the pause, 0.5 s after
        m.handle(&CtlMessage::Ping { n: n + 1, paused: false }, 303_000);
        m.tick(303_500);
        assert!(m.take_due(None, 303_500).is_empty());
        assert!(m.complete(a, true, |_| {}), "still deferred: it takes its stamp apply's result");
        m.handle(&CtlMessage::Release { id: a, at: 0 }, 303_600);
        assert_eq!(m.take_due(None, 303_600), [Action::Complete { id: a, handle: a, ok: true }]);
        // held 31 s while the session runs (pings without `paused`): failed by the age limit as before
        let b = hold(&mut m, BUILD, 310_000);
        m.handle(&CtlMessage::Defer { id: b }, 310_000);
        for t in (312_000..=338_000).step_by(2_000) {
            m.handle(&CtlMessage::Ping { n: 0, paused: false }, t);
        }
        m.tick(339_000);
        assert!(m.take_due(None, 339_000).is_empty(), "29 s: not yet");
        m.handle(&CtlMessage::Ping { n: 0, paused: false }, 341_000);
        m.tick(341_000);
        assert_eq!(m.take_due(None, 341_000), [Action::Fail { id: b, handle: b, reason: "timeout" }]);
        // a session that ends during a pause (hold_off) still fails what is deferred, whatever its age
        let c = hold_on_and_defer(&mut m, 350_000);
        m.handle(&CtlMessage::Ping { n: 0, paused: true }, 350_100);
        m.handle(&CtlMessage::HoldOff, 600_000);
        assert_eq!(m.take_due(None, 600_000), [Action::Fail { id: c, handle: c, reason: "hold_off" }]);
    }

    fn hold_on_and_defer(m: &mut HoldMachine<u64>, now: u64) -> HoldId {
        hold_on(m, &["build_proposal"], now);
        let id = hold(m, BUILD, now);
        m.handle(&CtlMessage::Defer { id }, now);
        id
    }

    #[test]
    fn a_tool_working_while_the_mod_is_silent_gives_everything_back() {
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        let (a, b) = (hold(&mut m, BUILD, 1), hold(&mut m, BUILD, 2));
        m.handle(&CtlMessage::Defer { id: b }, 3);
        m.tick(10_003);
        m.tool_while_silent(OTHER, 10_004);
        assert!(m.take_due(None, 10_004).is_empty(), "another kind proves nothing");
        m.tool_while_silent(BUILD, 10_005);
        assert_eq!(
            m.take_due(None, 10_006),
            [
                Action::Release { id: a, handle: a, reason: ReleaseReason::Silence },
                Action::Fail { id: b, handle: b, reason: "silence" }
            ]
        );
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
        m.handle(&CtlMessage::Ping { n: 5, paused: false }, 9_000);
        assert_eq!(m.drain_events(), [Event::Pong { n: 5 }]);
        m.tick(18_000); // 9 s after the ping: not silent yet
        assert!(m.enabled());
        m.tick(19_000);
        assert!(!m.enabled(), "10 s after the ping");
    }

    #[test]
    fn silence_suspends_holding_and_keeps_everything() {
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
        assert!(m.take_due(None, 11_001).is_empty(), "what is held waits for the mod (the pause menu)");
        assert_eq!(m.held(), [(a, BUILD), (b, BUILD)]);
        assert_eq!(m.admit(BUILD, 11_002), Admit::Off, "new commands pass meanwhile");
    }

    #[test]
    fn the_mods_next_line_after_a_silence_switches_holding_on_again() {
        // the pause menu stops the mod's script: holding goes off, and comes back with the first ping afterwards
        let mut m = machine();
        hold_on(&mut m, &["build_proposal"], 0);
        m.tick(10_000);
        assert!(!m.enabled());
        m.drain_events();
        m.handle(&CtlMessage::Ping { n: 7, paused: false }, 60_000);
        assert!(m.enabled(), "resumed");
        let ev = m.drain_events();
        assert!(matches!(&ev[0], Event::Holding { on: true, reason: "resume", .. }), "{ev:?}");
        assert_eq!(ev[1], Event::Pong { n: 7 });
        assert_eq!(m.held_kind_names(), ["build_proposal"]);
        assert_eq!(m.admit(BUILD, 60_001), Admit::Hold);
        // a hold_off is final: no resume after it
        m.handle(&CtlMessage::HoldOff, 60_002);
        m.drain_events();
        m.handle(&CtlMessage::Ping { n: 8, paused: false }, 60_003);
        assert!(!m.enabled());
        // nor after the control file went away
        hold_on(&mut m, &["build_proposal"], 70_000);
        m.tick(80_000);
        m.ctl_gone(80_001);
        m.handle(&CtlMessage::Ping { n: 9, paused: false }, 80_002);
        assert!(!m.enabled());
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
        m.handle(&CtlMessage::Ping { n: 1, paused: false }, 29_000);
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
            m.handle(&CtlMessage::Ping { n, paused: false }, 0);
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
                    6 => m.handle(&CtlMessage::Ping { n: 1, paused: false }, now),
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
