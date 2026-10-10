//! What the hooks count, and the audit that reports it (`docs/NATIVE_LINK.md` sections 3 and 5).
//!
//! The simulation thread's hook may touch nothing but atomics: no lock, no file, no allocation. Everything here
//! that a hook calls is a relaxed atomic operation on a fixed array; the service thread reads the totals, turns
//! them into an [`Event::Audit`] when they changed and writes it.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};

use crate::link::{AddCounts, Event};

/// Most construction-tool call sites counted individually; later ones are counted as `tool` only.
pub const MAX_SITES: usize = 32;

/// Where a command added to the engine's list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddSource {
    /// A construction tool; the index in the build's `ui_add_sites`.
    Tool(usize),
    /// Any other caller.
    Other,
    /// The caller could not be identified.
    Unknown,
}

/// Commands the simulation applied, by kind byte, and commands the interface added, by source.
#[derive(Debug)]
pub struct Counters {
    kinds: [AtomicU64; 256],
    total: AtomicU64,
    unreadable: AtomicU64,
    add_tool: AtomicU64,
    add_other: AtomicU64,
    add_unknown: AtomicU64,
    sites: [AtomicU64; MAX_SITES],
}

impl Default for Counters {
    fn default() -> Self {
        Counters::new()
    }
}

impl Counters {
    pub fn new() -> Counters {
        Counters {
            kinds: std::array::from_fn(|_| AtomicU64::new(0)),
            total: AtomicU64::new(0),
            unreadable: AtomicU64::new(0),
            add_tool: AtomicU64::new(0),
            add_other: AtomicU64::new(0),
            add_unknown: AtomicU64::new(0),
            sites: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// The simulation applied a command of this kind.
    pub fn applied(&self, kind: u8) {
        self.kinds[kind as usize].fetch_add(1, Relaxed);
        self.total.fetch_add(1, Relaxed);
    }

    /// The simulation applied a command whose kind could not be read.
    pub fn applied_unreadable(&self) {
        self.unreadable.fetch_add(1, Relaxed);
        self.total.fetch_add(1, Relaxed);
    }

    /// The interface added a command to the engine's list.
    pub fn added(&self, source: AddSource) {
        match source {
            AddSource::Tool(site) => {
                self.add_tool.fetch_add(1, Relaxed);
                if let Some(c) = self.sites.get(site) {
                    c.fetch_add(1, Relaxed);
                }
            }
            AddSource::Other => {
                self.add_other.fetch_add(1, Relaxed);
            }
            AddSource::Unknown => {
                self.add_unknown.fetch_add(1, Relaxed);
            }
        }
    }

    pub fn total(&self) -> u64 {
        self.total.load(Relaxed)
    }

    /// The cumulative counts as an audit event. Kinds and sites with a zero count are left out.
    pub fn snapshot(&self) -> Event {
        let counts = (0..=255u8)
            .filter_map(|k| {
                let n = self.kinds[k as usize].load(Relaxed);
                (n > 0).then_some((k, n))
            })
            .collect();
        let sites = self
            .sites
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let n = c.load(Relaxed);
                (n > 0).then_some((i, n))
            })
            .collect();
        Event::Audit {
            counts,
            total: self.total.load(Relaxed),
            unreadable: self.unreadable.load(Relaxed),
            adds: AddCounts {
                tool: self.add_tool.load(Relaxed),
                other: self.add_other.load(Relaxed),
                unknown: self.add_unknown.load(Relaxed),
            },
            sites,
        }
    }
}

/// Decides when the cumulative counts are worth an event: every `interval_ms`, and only if they changed.
#[derive(Debug)]
pub struct Audit {
    interval_ms: u64,
    last_ms: u64,
    last: Option<Event>,
}

impl Audit {
    pub fn new(interval_ms: u64, start_ms: u64) -> Audit {
        Audit { interval_ms, last_ms: start_ms, last: None }
    }

    /// The audit event if the interval has passed and the counts differ from the last one reported.
    pub fn poll(&mut self, now: u64, counters: &Counters) -> Option<Event> {
        if now.saturating_sub(self.last_ms) < self.interval_ms {
            return None;
        }
        self.last_ms = now;
        let snap = counters.snapshot();
        let empty = matches!(&snap, Event::Audit { total: 0, adds, .. } if *adds == AddCounts::default());
        if self.last.as_ref() == Some(&snap) || (self.last.is_none() && empty) {
            return None;
        }
        self.last = Some(snap.clone());
        Some(snap)
    }
}

/// One line for `native.log` saying what an audit event says, for a person reading the log.
pub fn audit_log_line(event: &Event, kind_name: impl Fn(u8) -> Option<&'static str>) -> String {
    let Event::Audit { counts, total, unreadable, adds, sites } = event else {
        return String::new();
    };
    let mut line = format!("audit: {total} command(s) applied");
    for (k, n) in counts {
        match kind_name(*k) {
            Some(name) => line.push_str(&format!(", kind {k} ({name}) x{n}")),
            None => line.push_str(&format!(", kind {k} x{n}")),
        }
    }
    if *unreadable > 0 {
        line.push_str(&format!(", {unreadable} unreadable"));
    }
    line.push_str(&format!("; added by tools {}, others {}, unknown {}", adds.tool, adds.other, adds.unknown));
    for (i, n) in sites {
        line.push_str(&format!(", tool site {i} x{n}"));
    }
    line
}

// ---------------------------------------------------------------------------------------------------------------
// Released commands the simulation has yet to apply
// ---------------------------------------------------------------------------------------------------------------

/// Slots of the watch; a released command stays in one until the simulation applies it.
pub const WATCH_SLOTS: usize = 64;
/// A released command not seen being applied within this long is forgotten.
pub const WATCH_EXPIRY_MS: u64 = 60_000;

const RESULTS: usize = 64;
/// Marks a written result slot (the id may be 0).
const VALID: u64 = 1 << 63;

/// Remembers the payload pointer of every command the module released, so the simulation thread's hook can say
/// "this one just ran" and whether the engine accepted it. A command moves through the engine's queues as a
/// pointer to its payload, which stays the same from `Add` to `Apply`; that is the identity used. A payload
/// address could be reused after the command is gone, so a slot is single-use and expires.
#[derive(Debug)]
pub struct AppliedWatch {
    payloads: [AtomicUsize; WATCH_SLOTS],
    ids: [AtomicU64; WATCH_SLOTS],
    armed_ms: [AtomicU64; WATCH_SLOTS],
    armed: AtomicUsize,
    results: [AtomicU64; RESULTS],
    head: AtomicUsize,
    tail: AtomicUsize,
    lost: AtomicU64,
}

impl Default for AppliedWatch {
    fn default() -> Self {
        AppliedWatch::new()
    }
}

impl AppliedWatch {
    pub fn new() -> AppliedWatch {
        AppliedWatch {
            payloads: std::array::from_fn(|_| AtomicUsize::new(0)),
            ids: std::array::from_fn(|_| AtomicU64::new(0)),
            armed_ms: std::array::from_fn(|_| AtomicU64::new(0)),
            armed: AtomicUsize::new(0),
            results: std::array::from_fn(|_| AtomicU64::new(0)),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            lost: AtomicU64::new(0),
        }
    }

    /// Cheap test for the simulation hook: is anything being watched?
    pub fn is_idle(&self) -> bool {
        self.armed.load(Relaxed) == 0
    }

    /// Interface thread: starts watching a released command. `false` when every slot is taken.
    pub fn arm(&self, payload: usize, id: u64, now_ms: u64) -> bool {
        if payload == 0 {
            return false;
        }
        for i in 0..WATCH_SLOTS {
            if self.payloads[i].compare_exchange(0, usize::MAX, AcqRel, Relaxed).is_ok() {
                self.ids[i].store(id, Relaxed);
                self.armed_ms[i].store(now_ms, Relaxed);
                self.armed.fetch_add(1, Relaxed);
                self.payloads[i].store(payload, Release);
                return true;
            }
        }
        false
    }

    /// Simulation thread, before the engine applies a command: the id of the released command with this payload,
    /// if it is one. The slot is freed.
    pub fn claim(&self, payload: usize) -> Option<u64> {
        if payload == 0 || payload == usize::MAX {
            return None;
        }
        for i in 0..WATCH_SLOTS {
            if self.payloads[i].load(Acquire) == payload {
                // the id is read before the slot is freed: once it is free, `arm` may reuse it at any moment
                let id = self.ids[i].load(Relaxed);
                if self.payloads[i].compare_exchange(payload, 0, AcqRel, Relaxed).is_ok() {
                    self.armed.fetch_sub(1, Relaxed);
                    return Some(id);
                }
            }
        }
        None
    }

    /// Simulation thread, after the engine applied it: queue the verdict for the service thread. `false` when the
    /// queue is full (the verdict is dropped and counted in [`lost`](Self::lost)).
    pub fn finish(&self, id: u64, ok: bool) -> bool {
        loop {
            // head first: tail never falls behind a head read earlier, so the difference cannot go negative
            let h = self.head.load(Acquire);
            let t = self.tail.load(Acquire);
            if t.wrapping_sub(h) >= RESULTS {
                self.lost.fetch_add(1, Relaxed);
                return false;
            }
            if self.tail.compare_exchange(t, t.wrapping_add(1), AcqRel, Relaxed).is_ok() {
                self.results[t % RESULTS].store(VALID | (id << 1) | u64::from(ok), Release);
                return true;
            }
        }
    }

    /// Service thread: the next verdict, oldest first.
    pub fn next_result(&self) -> Option<(u64, bool)> {
        let h = self.head.load(Relaxed);
        let slot = &self.results[h % RESULTS];
        let v = slot.load(Acquire);
        if v & VALID == 0 {
            return None; // empty, or reserved by a producer that has not stored yet
        }
        slot.store(0, Relaxed);
        self.head.store(h.wrapping_add(1), Release);
        Some(((v & !VALID) >> 1, v & 1 == 1))
    }

    /// Service thread: forgets entries older than [`WATCH_EXPIRY_MS`]; returns their ids.
    pub fn expire(&self, now_ms: u64) -> Vec<u64> {
        let mut gone = Vec::new();
        for i in 0..WATCH_SLOTS {
            let p = self.payloads[i].load(Acquire);
            if p == 0 || p == usize::MAX || now_ms.saturating_sub(self.armed_ms[i].load(Relaxed)) < WATCH_EXPIRY_MS {
                continue;
            }
            let id = self.ids[i].load(Relaxed);
            if self.payloads[i].compare_exchange(p, 0, AcqRel, Relaxed).is_ok() {
                gone.push(id);
                self.armed.fetch_sub(1, Relaxed);
            }
        }
        gone
    }

    /// Verdicts dropped because the service thread fell behind.
    pub fn lost(&self) -> u64 {
        self.lost.load(Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pieces of a snapshot, for assertions.
    struct Snap {
        counts: Vec<(u8, u64)>,
        total: u64,
        unreadable: u64,
        adds: AddCounts,
        sites: Vec<(usize, u64)>,
    }

    fn audit_of(c: &Counters) -> Snap {
        match c.snapshot() {
            Event::Audit { counts, total, unreadable, adds, sites } => Snap { counts, total, unreadable, adds, sites },
            other => panic!("not an audit: {other:?}"),
        }
    }

    #[test]
    fn counts_applied_commands_by_kind() {
        let c = Counters::new();
        for k in [52, 17, 52, 52, 0, 255] {
            c.applied(k);
        }
        c.applied_unreadable();
        let a = audit_of(&c);
        assert_eq!(a.counts, [(0, 1), (17, 1), (52, 3), (255, 1)]);
        assert_eq!((a.total, a.unreadable), (7, 1));
        assert_eq!(a.adds, AddCounts::default());
        assert!(a.sites.is_empty());
        assert_eq!(c.total(), 7);
    }

    #[test]
    fn counts_adds_by_source_and_tool_site() {
        let c = Counters::new();
        for s in [
            AddSource::Tool(0),
            AddSource::Tool(3),
            AddSource::Tool(3),
            AddSource::Tool(MAX_SITES + 5),
            AddSource::Other,
            AddSource::Unknown,
            AddSource::Other,
        ] {
            c.added(s);
        }
        let a = audit_of(&c);
        assert_eq!(a.total, 0, "adds are not applied commands");
        assert_eq!(a.adds, AddCounts { tool: 4, other: 2, unknown: 1 });
        assert_eq!(a.sites, [(0, 1), (3, 2)], "a site beyond the table still counts as a tool add");
    }

    #[test]
    fn counters_are_exact_under_contention() {
        let c = Counters::new();
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for i in 0..10_000u32 {
                        c.applied((i % 3) as u8);
                        c.added(AddSource::Other);
                    }
                });
            }
        });
        let a = audit_of(&c);
        assert_eq!(a.total, 80_000);
        assert_eq!(a.adds.other, 80_000);
        assert_eq!(a.counts.iter().map(|(_, n)| n).sum::<u64>(), 80_000);
    }

    #[test]
    fn the_audit_reports_after_its_interval_and_only_on_change() {
        let c = Counters::new();
        let mut a = Audit::new(30_000, 0);
        assert_eq!(a.poll(29_999, &c), None);
        assert_eq!(a.poll(30_000, &c), None, "nothing counted yet: nothing to say");
        c.applied(52);
        assert_eq!(a.poll(40_000, &c), None, "the interval restarts after each look");
        let first = a.poll(60_000, &c).expect("changed");
        assert!(matches!(&first, Event::Audit { total: 1, .. }));
        assert_eq!(a.poll(90_000, &c), None, "unchanged");
        c.added(AddSource::Tool(1));
        assert!(matches!(a.poll(120_000, &c), Some(Event::Audit { total: 1, .. })), "an add alone is a change");
    }

    #[test]
    fn the_log_line_names_known_kinds() {
        let c = Counters::new();
        c.applied(52);
        c.applied(52);
        c.applied(17);
        c.applied_unreadable();
        c.added(AddSource::Tool(2));
        let line = audit_log_line(&c.snapshot(), |k| (k == 52).then_some("build_proposal"));
        assert_eq!(
            line,
            "audit: 4 command(s) applied, kind 17 x1, kind 52 (build_proposal) x2, 1 unreadable; \
             added by tools 1, others 0, unknown 0, tool site 2 x1"
        );
        assert_eq!(audit_log_line(&Event::Pong { n: 1 }, |_| None), "");
    }

    // ---- the watch

    #[test]
    fn a_released_command_is_recognised_once_when_it_is_applied() {
        let w = AppliedWatch::new();
        assert!(w.is_idle());
        assert!(w.arm(0xAAAA_0000, 7, 100));
        assert!(!w.is_idle());
        assert_eq!(w.claim(0xBBBB_0000), None, "another payload is not ours");
        assert_eq!(w.claim(0xAAAA_0000), Some(7));
        assert!(w.is_idle());
        assert_eq!(w.claim(0xAAAA_0000), None, "single use: a reused address is not mistaken for it");
        w.finish(7, true);
        w.finish(8, false);
        assert_eq!(w.next_result(), Some((7, true)));
        assert_eq!(w.next_result(), Some((8, false)));
        assert_eq!(w.next_result(), None);
    }

    #[test]
    fn null_payloads_are_never_watched() {
        let w = AppliedWatch::new();
        assert!(!w.arm(0, 1, 0));
        assert_eq!(w.claim(0), None);
        assert_eq!(w.claim(usize::MAX), None);
        assert!(w.is_idle());
    }

    #[test]
    fn the_watch_has_a_limit_and_expires_old_entries() {
        let w = AppliedWatch::new();
        for i in 0..WATCH_SLOTS {
            assert!(w.arm(0x1000 + i * 16, i as u64, 1_000));
        }
        assert!(!w.arm(0x9999_0000, 99, 1_000), "full");
        assert!(w.expire(1_000 + WATCH_EXPIRY_MS - 1).is_empty());
        let mut gone = w.expire(1_000 + WATCH_EXPIRY_MS);
        gone.sort_unstable();
        assert_eq!(gone, (0..WATCH_SLOTS as u64).collect::<Vec<_>>());
        assert!(w.is_idle());
        assert!(w.arm(0x9999_0000, 99, 70_000), "room again");
    }

    #[test]
    fn the_result_queue_is_bounded_and_ordered() {
        let w = AppliedWatch::new();
        for id in 0..(RESULTS as u64 + 5) {
            assert_eq!(w.finish(id, id % 2 == 0), id < RESULTS as u64);
        }
        assert_eq!(w.lost(), 5);
        let mut seen = Vec::new();
        while let Some((id, ok)) = w.next_result() {
            assert_eq!(ok, id % 2 == 0);
            seen.push(id);
        }
        assert_eq!(seen, (0..RESULTS as u64).collect::<Vec<_>>());
        w.finish(500, true);
        assert_eq!(w.next_result(), Some((500, true)), "and it keeps working after wrapping");
    }

    #[test]
    fn simulation_threads_and_the_service_thread_do_not_lose_verdicts() {
        let w = AppliedWatch::new();
        let per = 40u64;
        let total = std::sync::atomic::AtomicU64::new(0);
        std::thread::scope(|s| {
            for t in 0..4u64 {
                let w = &w;
                s.spawn(move || {
                    for i in 0..per {
                        let id = t * 1000 + i;
                        loop {
                            if w.arm(0x10_0000 + (id as usize) * 16, id, 0) {
                                break;
                            }
                            std::thread::yield_now();
                        }
                        assert_eq!(w.claim(0x10_0000 + (id as usize) * 16), Some(id));
                        while !w.finish(id, true) {
                            std::thread::yield_now();
                        }
                    }
                });
            }
            s.spawn(|| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                while total.load(Relaxed) < 4 * per {
                    // a producer that failed an assertion must not leave this loop spinning for ever
                    assert!(std::time::Instant::now() < deadline, "verdicts missing: {}", total.load(Relaxed));
                    match w.next_result() {
                        Some((_, ok)) => {
                            assert!(ok);
                            total.fetch_add(1, Relaxed);
                        }
                        None => std::thread::yield_now(),
                    }
                }
            });
        });
        assert_eq!(total.load(Relaxed), 4 * per);
    }
}
