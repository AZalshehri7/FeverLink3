//! Canonical free-id order (`FEVERLINK3_FREE_ORDER`, on unless `=0`; must be the same on every game of a session, like the pool
//! patch and the step seed).
//!
//! The engine's `ecs::Engine` keeps the ids of removed entities in a FIFO `std::deque<int>` (engine `+0xd8`, size `+0xf8`):
//! `AddEntity` (`0x2bb8940`) takes the FRONT, and `EndModification` (`0x2bb9ea0`) appends at the BACK the ids `RemoveEntity`
//! (`0x2bbc700`) collected in the modification, in the order of the removal requests (the modification's `vector<int>` at `+0x0`,
//! `push_back` at `0x2bbc90d`). That order is the order in which some system happened to visit the entities (node lists, hash maps):
//! engine history. Two games that remove the same entities in another order hand the same ids to other entities from then on.
//!
//! The mechanism: the call `deque<int>::_Insert_range(queue, &ret, where = size, first, last)` at `0x2bba103` in `EndModification`
//! (the callee `0x2bb62a0` has no other caller in the executable: rel32 scan of `.text`, no pointer to it in the image) is redirected
//! to a thunk of the module, which sorts `[first, last)` ascending by entity id in place and then calls the real insert with every
//! argument as given. What the sort cannot change, by construction of the site (build 40420, disassembly):
//!
//! * the systems' modification callbacks (vtable slot `+0x50`, the loop at `0x2bba06c`) have all run before the call;
//! * right after it the modification is released (`0x2bba10c` clears the engine's pointer, `0x2bb86e0` frees its two vectors and the
//!   0x58-byte object): nothing reads the removed list again;
//! * the insert itself copies the ids in range order (back path: `push_back` from `first` up; front path for an empty queue:
//!   `push_front` from `last-1` down, then a rotate), so the queue's order is exactly the order of the range.
//!
//! So the ids handed out from then on depend only on WHICH ids each modification freed, never on the order the engine visited them.
//! Nothing else changes: the same entities are removed, the same number of ids is reused, no game mechanic is touched.
//!
//! This file is the pure part (unit-tested on a fake queue); the hook lives in `hooks.rs` (`free_order_hook`).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The switch: on unless `0`.
pub const FREE_ORDER_VAR: &str = "FEVERLINK3_FREE_ORDER";
/// The piece's name in `native.log` and the mod's `ready` event.
pub const HOOK_NAME: &str = "free_order";
/// A batch longer than this is not what the site hands over (a modification frees at most a few thousand ids, a world load does not
/// go through here): left as it is, and a stop fault.
pub const MAX_BATCH: usize = 1 << 22;

/// The canonical order of one batch of freed ids: ascending by entity id. Returns whether the order changed.
pub fn canonical(ids: &mut [i32]) -> bool {
    if ids.windows(2).all(|w| w[0] <= w[1]) {
        return false;
    }
    ids.sort_unstable();
    true
}

/// The number of ids in `[first, last)` (two `int*` of the engine), or why the range is not a batch of ids.
pub fn batch_len(first: u64, last: u64) -> Result<usize, String> {
    if last < first {
        return Err(format!("the range ends before it starts ({first:#x}..{last:#x})"));
    }
    if !first.is_multiple_of(4) || !(last - first).is_multiple_of(4) {
        return Err(format!("the range is not a run of 4-byte ids ({first:#x}..{last:#x})"));
    }
    let n = ((last - first) / 4) as usize;
    if n > MAX_BATCH {
        return Err(format!("{n} ids in one batch (more than {MAX_BATCH})"));
    }
    if n > 0 && first == 0 {
        return Err("the range starts at address 0".into());
    }
    Ok(n)
}

/// What the hook did, for the service thread's line a minute.
#[derive(Debug, Default)]
pub struct Stats {
    batches: AtomicU64,
    reordered: AtomicU64,
    ids: AtomicU64,
    largest: AtomicU64,
    minute_at: AtomicU64,
}

impl Stats {
    pub const fn new() -> Stats {
        Stats {
            batches: AtomicU64::new(0),
            reordered: AtomicU64::new(0),
            ids: AtomicU64::new(0),
            largest: AtomicU64::new(0),
            minute_at: AtomicU64::new(0),
        }
    }

    /// One batch of `n` ids went into the queue; `reordered`: the engine's order was not already ascending.
    pub fn note(&self, n: usize, reordered: bool) {
        self.batches.fetch_add(1, Relaxed);
        self.ids.fetch_add(n as u64, Relaxed);
        self.largest.fetch_max(n as u64, Relaxed);
        if reordered {
            self.reordered.fetch_add(1, Relaxed);
        }
    }

    /// Once a minute (`now` in `pace::clock_ms`), when a batch went in: how many, how many the sort changed; the counts start over.
    pub fn minute_line(&self, now: u64) -> Option<String> {
        let at = self.minute_at.load(Relaxed);
        if at == 0 {
            self.minute_at.store(now.max(1), Relaxed);
            return None;
        }
        if now.saturating_sub(at) < 60_000 {
            return None;
        }
        self.minute_at.store(now.max(1), Relaxed);
        let batches = self.batches.swap(0, Relaxed);
        let reordered = self.reordered.swap(0, Relaxed);
        let ids = self.ids.swap(0, Relaxed);
        let largest = self.largest.swap(0, Relaxed);
        (batches > 0).then(|| {
            format!(
                "{HOOK_NAME}: last 60 s: {batches} freed-id batch(es) into the queue, {reordered} put in id order, {ids} id(s), \
                 largest {largest}"
            )
        })
    }
}

/// The hook's counts.
pub static STATS: Stats = Stats::new();

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A fake `ecs::Engine` free-id queue: `add` takes the front or a new id, `end_modification` appends a batch at the back as the
    /// engine does, through the module's sort when `sorted`.
    struct Engine {
        free: VecDeque<i32>,
        next: i32,
        sorted: bool,
    }

    impl Engine {
        fn new(free: &[i32], next: i32, sorted: bool) -> Engine {
            Engine { free: free.iter().copied().collect(), next, sorted }
        }
        fn add(&mut self) -> i32 {
            self.free.pop_front().unwrap_or_else(|| {
                self.next += 1;
                self.next
            })
        }
        fn end_modification(&mut self, removed: &[i32]) {
            let mut batch = removed.to_vec();
            if self.sorted {
                canonical(&mut batch);
            }
            self.free.extend(batch);
        }
    }

    /// Two games remove the same entities, each in its own history order, then create entities: the ids they hand out.
    fn run(sorted: bool, orders: [&[&[i32]]; 2]) -> [Vec<i32>; 2] {
        orders.map(|batches| {
            let mut e = Engine::new(&[3, 4], 100, sorted);
            let mut out = vec![e.add()];
            for b in batches {
                e.end_modification(b);
                out.extend((0..3).map(|_| e.add()));
            }
            out
        })
    }

    #[test]
    fn the_same_ids_freed_in_another_order_are_handed_out_in_the_same_order() {
        let game1: &[&[i32]] = &[&[50859, 49942, 48057], &[7, 9, 8]];
        let game2: &[&[i32]] = &[&[49942, 48057, 50859], &[9, 8, 7]];
        let [a, b] = run(false, [game1, game2]);
        assert_ne!(a, b, "as shipped: the history order forks the ids");
        let [a, b] = run(true, [game1, game2]);
        assert_eq!(a, b, "sorted: the same ids for the same entities");
        assert_eq!(a, [3, 4, 48057, 49942, 50859, 7, 8]);
    }

    #[test]
    fn another_set_of_freed_ids_still_forks_and_the_sort_does_not_hide_it() {
        // the M lab of 2026-10-19: one game freed 50859, the other 49942 and 48057 in the same step
        let [a, b] = run(true, [&[&[50859]], &[&[49942, 48057]]]);
        assert_ne!(a, b, "a game that removed another entity is a real fork, not an order");
    }

    #[test]
    fn the_sort_only_reorders_a_batch_and_keeps_every_id() {
        let mut v = vec![9, -1, 9, 3];
        assert!(canonical(&mut v));
        assert_eq!(v, [-1, 3, 9, 9]);
        assert!(!canonical(&mut v), "already in order: untouched");
        let mut empty: [i32; 0] = [];
        assert!(!canonical(&mut empty));
        let mut one = [5];
        assert!(!canonical(&mut one));
    }

    #[test]
    fn only_a_run_of_aligned_ids_is_a_batch() {
        assert_eq!(batch_len(0x1000, 0x1000), Ok(0));
        assert_eq!(batch_len(0x1000, 0x100c), Ok(3));
        assert_eq!(batch_len(0, 0), Ok(0), "an empty vector has null pointers");
        assert!(batch_len(0x1000, 0xff0).is_err());
        assert!(batch_len(0x1002, 0x100a).is_err());
        assert!(batch_len(0x1000, 0x1006).is_err());
        assert!(batch_len(0, 8).is_err());
        assert!(batch_len(0x1000, 0x1000 + 4 * (MAX_BATCH as u64 + 1)).is_err());
    }

    #[test]
    fn the_minute_line_counts_batches_and_starts_over() {
        let s = Stats::new();
        assert_eq!(s.minute_line(1_000), None, "the first call starts the minute");
        s.note(3, true);
        s.note(1, false);
        assert_eq!(s.minute_line(30_000), None, "not a minute yet");
        assert_eq!(
            s.minute_line(61_000).unwrap(),
            "free_order: last 60 s: 2 freed-id batch(es) into the queue, 1 put in id order, 4 id(s), largest 3"
        );
        assert_eq!(s.minute_line(122_000), None, "a quiet minute says nothing");
    }
}
