//! Compact trace of the free-id order (`FEVERLINK3_TRACE`, on unless `=0`, like the mod's trace.log): in the order the
//! engine does it, the entity ids each ECS engine hands out and the ids that join its free-id queue, so that two games'
//! logs show whether freed ids are reused in another order.
//!
//! Why (TODO "trace the freed-id order", 2026-10-19): MPFever 0.3.14 sorts by entity id six batches whose order is engine
//! history, the first of them the freed ids before they join the FIFO free-id queue (`ecs::Engine::EndModification`,
//! build 40420 site `0x2bba0e1`, DESIGN_DECISIONS 2026-10-18). `AddEntity` takes the FRONT of that queue, so a batch of
//! freed ids appended in another order on one game hands the same ids to other entities from then on, even when both
//! games removed the same entities. The verbose `ids` trace logs the removal REQUESTS (`RemoveEntity`), not the order the
//! ids join the queue; this trace reads that order from the queue itself.
//!
//! How, without a new hook site: the queue only grows at its back (the engine appends the freed ids when a modification
//! ends) and only shrinks at its front through `AddEntity`, which is hooked. So at every hooked call the ids past the
//! size seen after the previous call are exactly the ids appended since, in queue order. A queue that shrank without an
//! `AddEntity` (a load) is reported as `reset`.
//!
//! One line per burst of an engine's work (calls less than [`GAP_MS`] apart), only when something happened:
//!
//! ```text
//! idorder e=<engine> g=<burst> n=<ops so far> at=<HH:MM:SS.mmm> ents=<id slots> free=<queue size> r=<removal requests> h=<hash> ops=+12,-7,-9
//! ```
//!
//! `+id` = handed out by `AddEntity`, `-id` = joined the free-id queue (not a request: the order the queue holds them).
//! `n` counts the `+`/`-` ops of this engine since it was first seen, `h` is a rolling FNV-1a over them, so equal `n`
//! with unequal `h` on two games means the order differed before that point. The native module has no simulation step
//! of its own (the game time is read only inside the growth command), so a burst is not a step: a game catching up runs
//! several steps in one burst. Compare the flattened `ops` streams (`tools/gametest/freed_id_diff.py`), not the lines.
//!
//! This file is the pure part (unit-tested); the hooks that feed it live in [`crate::ids`].

/// The switch the mod's trace.log uses too: on unless `0`.
pub const TRACE_VAR: &str = "FEVERLINK3_TRACE";

/// Calls of one engine further apart than this start a new line.
pub const GAP_MS: u64 = 10;

/// A line never holds more ops than this (a load creates tens of thousands of entities in one burst).
pub const MAX_OPS_PER_LINE: usize = 4096;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// True unless the variable says `0` (the mod's rule for `FEVERLINK3_TRACE`).
pub fn trace_on(value: Option<&str>) -> bool {
    value.map(|v| v.trim() != "0").unwrap_or(true)
}

/// One op of the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `AddEntity` handed this id out.
    Alloc(i32),
    /// This id joined the back of the free-id queue.
    Freed(i32),
}

/// The rolling hash after `op`.
pub fn hash_op(h: u64, op: Op) -> u64 {
    let (kind, id) = match op {
        Op::Alloc(id) => (1u64, id),
        Op::Freed(id) => (2u64, id),
    };
    (h ^ ((kind << 32) | u64::from(id as u32))).wrapping_mul(FNV_PRIME)
}

/// FNV-1a over a whole queue (the `fifo=` of the first-seen and reset lines).
pub fn queue_hash(ids: &[i32]) -> u64 {
    ids.iter().fold(FNV_OFFSET, |h, &id| (h ^ u64::from(id as u32)).wrapping_mul(FNV_PRIME))
}

/// What one engine has done since it was first seen.
#[derive(Debug)]
pub struct EngineOrder {
    /// The engine's address, low 24 bits (as the verbose trace prints it).
    pub tag: usize,
    burst: u64,
    ops: u64,
    hash: u64,
    /// The queue size after the last hooked call: anything beyond it was appended since.
    expected_free: u64,
    last_ms: u64,
    pending: Vec<Op>,
    removed: u32,
    started: String,
    ents: u64,
    free: u64,
}

impl EngineOrder {
    /// A newly seen engine whose queue holds `free` ids.
    pub fn new(tag: usize, free: u64, ents: u64) -> EngineOrder {
        EngineOrder {
            tag,
            burst: 0,
            ops: 0,
            hash: FNV_OFFSET,
            expected_free: free,
            last_ms: 0,
            pending: Vec::new(),
            removed: 0,
            started: String::new(),
            ents,
            free,
        }
    }

    /// The first-seen line: the queue as it stood when this trace met the engine.
    pub fn first_line(&self, at: &str, queue: &[i32]) -> String {
        format!(
            "idorder e={:x} first seen at={at} ents={} free={} fifo={:016x} head={}",
            self.tag,
            self.ents,
            self.free,
            queue_hash(queue),
            join(queue.iter().take(8).map(|id| id.to_string()))
        )
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty() || self.removed > 0
    }

    /// Starts a hooked call at `now_ms` with the queue now `free_now` long. `tail(k)` reads the queue's last `k` ids
    /// in order. Lines that are due go to `out`.
    /// `at()` is the wall time, asked for only when a line starts.
    pub fn enter(
        &mut self,
        now_ms: u64,
        at: impl FnOnce() -> String,
        free_now: u64,
        tail: impl FnOnce(usize) -> Vec<i32>,
        out: &mut Vec<String>,
    ) {
        if self.has_pending() && now_ms.saturating_sub(self.last_ms) > GAP_MS {
            self.flush(out);
        }
        let shrank = free_now < self.expected_free;
        let at = if !self.has_pending() || shrank { at() } else { String::new() };
        if !self.has_pending() {
            self.started = at.clone();
        }
        self.last_ms = now_ms;
        if free_now > self.expected_free {
            let k = (free_now - self.expected_free) as usize;
            for id in tail(k) {
                self.push(Op::Freed(id), out);
            }
        } else if shrank {
            // shrank without an AddEntity: the queue was rebuilt (a load), the stream starts over from here
            self.flush(out);
            self.started = at.clone();
            out.push(format!(
                "idorder e={:x} reset at={at} free={free_now} (was {}) n={} h={:016x}",
                self.tag, self.expected_free, self.ops, self.hash
            ));
        }
        self.expected_free = free_now;
        self.free = free_now;
    }

    /// `AddEntity` returned `id`; the queue is now `free_after` long and the engine has `ents` id slots.
    pub fn alloc(&mut self, id: i32, free_after: u64, ents: u64, out: &mut Vec<String>) {
        self.push(Op::Alloc(id), out);
        self.expected_free = free_after;
        self.free = free_after;
        self.ents = ents;
    }

    /// A `RemoveEntity` request (the id joins the queue later, when the modification ends).
    pub fn removal(&mut self) {
        self.removed += 1;
    }

    fn push(&mut self, op: Op, out: &mut Vec<String>) {
        self.hash = hash_op(self.hash, op);
        self.ops += 1;
        self.pending.push(op);
        if self.pending.len() >= MAX_OPS_PER_LINE {
            self.flush(out);
        }
    }

    /// Writes the pending line, if any, when the engine has been idle past [`GAP_MS`] at `now_ms`.
    pub fn flush_idle(&mut self, now_ms: u64, out: &mut Vec<String>) {
        if self.has_pending() && now_ms.saturating_sub(self.last_ms) > GAP_MS {
            self.flush(out);
        }
    }

    /// Writes the pending line, if any.
    pub fn flush(&mut self, out: &mut Vec<String>) {
        if !self.has_pending() {
            return;
        }
        self.burst += 1;
        let ops = join(self.pending.iter().map(|op| match op {
            Op::Alloc(id) => format!("+{id}"),
            Op::Freed(id) => format!("-{id}"),
        }));
        out.push(format!(
            "idorder e={:x} g={} n={} at={} ents={} free={} r={} h={:016x} ops={ops}",
            self.tag, self.burst, self.ops, self.started, self.ents, self.free, self.removed, self.hash
        ));
        self.pending.clear();
        self.removed = 0;
    }
}

impl EngineOrder {
    /// `<engine>:<n>:<hash>`, this engine's place in its stream for a mark line.
    pub fn place(&self) -> String {
        format!("{:x}:{}:{:016x}", self.tag, self.ops, self.hash)
    }
}

/// A mark in the stream: the session's holding switched (`what`, e.g. `holding off (hold_off)`) when each engine had done
/// the ops its `place` names. Ops past a game's `holding off` mark ran after its session ended, as single-player: two games
/// need not agree there (M-quick 2026-10-19: the only "fork" of that run began in the first batch after both sessions ended,
/// while the guest saved its world, and was read as a lockstep fork). Pending lines of every engine are written first, so
/// the mark falls between whole lines.
pub fn mark_line(at: &str, what: &str, engines: &mut [&mut EngineOrder], out: &mut Vec<String>) {
    for e in engines.iter_mut() {
        e.flush(out);
    }
    let places = join(engines.iter().map(|e| e.place()));
    out.push(format!("idorder mark at={at} {what} engines={places}"));
}

fn join(it: impl Iterator<Item = String>) -> String {
    it.collect::<Vec<_>>().join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake free-id queue: the engine appends at the back and AddEntity pops the front.
    struct Fifo(std::collections::VecDeque<i32>, i32);

    impl Fifo {
        fn tail(&self, k: usize) -> Vec<i32> {
            self.0.iter().skip(self.0.len() - k).copied().collect()
        }
        fn add(&mut self) -> i32 {
            self.0.pop_front().unwrap_or_else(|| {
                self.1 += 1;
                self.1
            })
        }
    }

    /// One AddEntity through the trace, as the hook does it.
    fn add(e: &mut EngineOrder, q: &mut Fifo, now: u64, out: &mut Vec<String>) -> i32 {
        let snapshot = q.0.clone();
        e.enter(now, || "t".to_string(), q.0.len() as u64, |k| Fifo(snapshot, 0).tail(k), out);
        let id = q.add();
        e.alloc(id, q.0.len() as u64, q.1 as u64, out);
        id
    }

    #[test]
    fn the_switch_is_on_unless_it_says_zero() {
        assert!(trace_on(None));
        assert!(trace_on(Some("1")));
        assert!(trace_on(Some("")));
        assert!(!trace_on(Some("0")));
        assert!(!trace_on(Some(" 0 ")));
    }

    #[test]
    fn freed_ids_are_logged_in_the_order_they_joined_the_queue_and_before_the_next_add() {
        let mut q = Fifo(vec![3, 4].into(), 100);
        let mut e = EngineOrder::new(0xabc, 2, 100);
        let mut out = Vec::new();
        assert_eq!(add(&mut e, &mut q, 0, &mut out), 3);
        e.removal();
        e.removal();
        // the modification ends: the engine appends 9 then 7 (its own history order)
        q.0.extend([9, 7]);
        assert_eq!(add(&mut e, &mut q, 1, &mut out), 4);
        assert_eq!(add(&mut e, &mut q, 2, &mut out), 9);
        assert!(out.is_empty(), "one burst, nothing written yet");
        e.flush(&mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("idorder e=abc g=1 n=5 at=t ents=100 free=1 r=2 h="), "{}", out[0]);
        assert!(out[0].ends_with(" ops=+3,-9,-7,+4,+9"), "{}", out[0]);
    }

    #[test]
    fn a_gap_starts_a_new_line_and_an_idle_engine_is_flushed_by_another() {
        let mut q = Fifo(Default::default(), 0);
        let mut e = EngineOrder::new(1, 0, 0);
        let mut out = Vec::new();
        add(&mut e, &mut q, 0, &mut out);
        add(&mut e, &mut q, 5, &mut out);
        add(&mut e, &mut q, 5 + GAP_MS + 1, &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains(" g=1 n=2 ") && out[0].ends_with("ops=+1,+2"), "{}", out[0]);
        e.flush_idle(5 + GAP_MS + 2, &mut out);
        assert_eq!(out.len(), 1, "not idle yet");
        e.flush_idle(100, &mut out);
        assert!(out[1].contains(" g=2 n=3 ") && out[1].ends_with("ops=+3"), "{}", out[1]);
        e.flush(&mut out);
        assert_eq!(out.len(), 2, "nothing pending: no empty line");
    }

    #[test]
    fn the_same_ids_in_another_free_order_give_another_hash_and_the_same_order_the_same() {
        let run = |freed: [i32; 3]| {
            let mut q = Fifo(Default::default(), 10);
            let mut e = EngineOrder::new(1, 0, 10);
            let mut out = Vec::new();
            add(&mut e, &mut q, 0, &mut out);
            q.0.extend(freed);
            for _ in 0..3 {
                add(&mut e, &mut q, 1, &mut out);
            }
            e.flush(&mut out);
            out.pop().unwrap()
        };
        let h = |l: String| l.split(" h=").nth(1).unwrap()[..16].to_string();
        assert_eq!(h(run([5, 6, 7])), h(run([5, 6, 7])));
        assert_ne!(h(run([5, 6, 7])), h(run([7, 6, 5])), "a reordered free batch shows in the hash");
        assert!(run([7, 6, 5]).ends_with("ops=+11,-7,-6,-5,+7,+6,+5"));
    }

    #[test]
    fn a_queue_that_shrinks_without_an_add_is_a_reset() {
        let mut e = EngineOrder::new(1, 5, 0);
        let mut out = Vec::new();
        e.enter(0, || "t".to_string(), 5, |_| unreachable!(), &mut out);
        e.alloc(1, 4, 9, &mut out);
        e.enter(1, || "u".to_string(), 2, |_| unreachable!(), &mut out);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].ends_with("ops=+1"));
        assert!(out[1].starts_with("idorder e=1 reset at=u free=2 (was 4) n=1 h="), "{}", out[1]);
        // and it goes on from the new size
        e.enter(2, || "v".to_string(), 3, |k| vec![42; k], &mut out);
        e.flush(&mut out);
        assert!(out[2].ends_with("ops=-42"), "{}", out[2]);
    }

    #[test]
    fn a_huge_burst_is_split_into_bounded_lines() {
        let mut q = Fifo(Default::default(), 0);
        let mut e = EngineOrder::new(1, 0, 0);
        let mut out = Vec::new();
        for _ in 0..MAX_OPS_PER_LINE + 3 {
            add(&mut e, &mut q, 0, &mut out);
        }
        e.flush(&mut out);
        assert_eq!(out.len(), 2);
        assert!(out[1].contains(&format!(" n={} ", MAX_OPS_PER_LINE + 3)) && out[1].ends_with("ops=+4097,+4098,+4099"));
    }

    #[test]
    fn a_mark_writes_the_pending_lines_first_and_names_every_engines_place() {
        let (mut qa, mut qb) = (Fifo(Default::default(), 0), Fifo(Default::default(), 100));
        let (mut a, mut b) = (EngineOrder::new(0xa, 0, 0), EngineOrder::new(0xb, 0, 100));
        let mut out = Vec::new();
        add(&mut a, &mut qa, 0, &mut out);
        add(&mut a, &mut qa, 1, &mut out);
        add(&mut b, &mut qb, 1, &mut out);
        assert!(out.is_empty());
        mark_line("12:00:00.000", "holding off (hold_off)", &mut [&mut a, &mut b], &mut out);
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(out[0].starts_with("idorder e=a g=1 n=2 ") && out[1].starts_with("idorder e=b g=1 n=1 "), "{out:?}");
        assert_eq!(
            out[2],
            format!("idorder mark at=12:00:00.000 holding off (hold_off) engines={},{}", a.place(), b.place())
        );
        assert!(a.place().starts_with("a:2:") && b.place().starts_with("b:1:"));
        // the stream goes on unchanged after a mark: the next line counts on
        add(&mut a, &mut qa, 100, &mut out);
        a.flush(&mut out);
        assert!(out[3].starts_with("idorder e=a g=2 n=3 "), "{}", out[3]);
    }

    #[test]
    fn the_first_seen_line_names_the_queue() {
        let e = EngineOrder::new(0x4da740, 3, 50);
        let l = e.first_line("00:00:01.000", &[3, 4, 1942]);
        assert_eq!(
            l,
            format!(
                "idorder e=4da740 first seen at=00:00:01.000 ents=50 free=3 fifo={:016x} head=3,4,1942",
                queue_hash(&[3, 4, 1942])
            )
        );
    }
}
