//! The text of the read-only town-seed log (`FEVERLINK3_LOG_TOWN_SEED=1`, `docs/NATIVE_LINK.md` section 2).
//!
//! The hook itself is in `hooks.rs`; this is the part that needs no Windows: how a call is turned into a log line.
//! One line per call of the function that seeds the town developer's task:
//!
//! ```text
//! town-seed n=<counter> seed=<hex> caller=<rva> a3=<hex> a4=<hex>
//! ```
//!
//! Two games are compared by cutting every line after the third field (`cut -d' ' -f1-4`), so nothing that can
//! differ between two runs of the same game (pointers, ASLR) is in those fields: `caller` is an RVA, not an address.
//! `a3` and `a4` are the 32-bit integer arguments the caller passes in `r8d` and `r9d` (index values, meaning not
//! established); they come after the diffable fields on purpose.

use core::sync::atomic::{AtomicU64, Ordering};

/// Prefix of every line, so the log can be filtered with `grep '^town-seed'`.
pub const PREFIX: &str = "town-seed";

/// Calls counted so far; the first call is number 1.
#[derive(Debug, Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub const fn new() -> Counter {
        Counter(AtomicU64::new(0))
    }

    /// Starts again from zero (when the hook is armed).
    pub fn reset(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

    /// The number of this call: 1, 2, 3, ...
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// The caller as a log field: an RVA when the return address lies inside the image (`[base, base + size)`), the
/// raw address marked `abs:` when it does not, `?` when the hook could not tell.
pub fn caller_field(caller: Option<usize>, base: usize, image_size: usize) -> String {
    match caller {
        None => "?".to_string(),
        Some(c) if c >= base && c - base < image_size => format!("{:#x}", c - base),
        Some(c) => format!("abs:{c:#x}"),
    }
}

/// The log line of one call. `seed`, `a3` and `a4` are the low 32 bits of their argument slots.
pub fn line(n: u64, seed: u32, caller: &str, a3: u32, a4: u32) -> String {
    format!("{PREFIX} n={n} seed={seed:#010x} caller={caller} a3={a3:#x} a4={a4:#x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counter_starts_at_one_and_counts_up() {
        let c = Counter::new();
        assert_eq!([c.next(), c.next(), c.next()], [1, 2, 3]);
    }

    #[test]
    fn a_line_has_the_fields_in_a_fixed_order() {
        assert_eq!(
            line(7, 0xdead_beef, "0x3b9d53", 3, 0x10),
            "town-seed n=7 seed=0xdeadbeef caller=0x3b9d53 a3=0x3 a4=0x10"
        );
        assert_eq!(line(1, 5, "?", 0, 0), "town-seed n=1 seed=0x00000005 caller=? a3=0x0 a4=0x0");
    }

    #[test]
    fn the_diffable_part_ignores_the_trailing_fields() {
        let a = line(2, 9, "0x3b9d53", 1, 2);
        let b = line(2, 9, "0x3b9d53", 8, 9);
        let cut = |s: &str| s.split(' ').take(4).collect::<Vec<_>>().join(" ");
        assert_eq!(cut(&a), cut(&b));
        assert_ne!(a, b);
    }

    #[test]
    fn the_caller_is_an_rva_inside_the_image_and_marked_outside() {
        let base = 0x1_4000_0000;
        assert_eq!(caller_field(Some(base + 0x3b9d53), base, 0x4_000_000), "0x3b9d53");
        assert_eq!(caller_field(Some(base), base, 0x1000), "0x0");
        assert_eq!(caller_field(Some(base + 0x1000), base, 0x1000), format!("abs:{:#x}", base + 0x1000));
        assert_eq!(caller_field(Some(base - 1), base, 0x1000), format!("abs:{:#x}", base - 1));
        assert_eq!(caller_field(None, base, 0x1000), "?");
    }
}
