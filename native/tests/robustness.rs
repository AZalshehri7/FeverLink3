//! Randomised robustness tests for the native crate's pure parsers: the PE header reader, the pattern scanner and
//! the detour byte maths. They read the game's own executable and tables written by hand, not network input, but
//! they run in the game process, where a panic ends the game.
//!
//! Seeded; `FEVERLINK3_FUZZ_SCALE=50` runs everything 50 times longer, `FEVERLINK3_FUZZ_SEED=n` changes the seeds.

use feverlink3_native::build_table;
use feverlink3_native::detour::{check_prologue, patch_bytes, trampoline_bytes, JMP_ABS_LEN};
use feverlink3_native::pe;
use feverlink3_native::scan::Pattern;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

fn iters(base: usize) -> usize {
    base * std::env::var("FEVERLINK3_FUZZ_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

fn rng(salt: u64) -> StdRng {
    let seed: u64 = std::env::var("FEVERLINK3_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0xFEED_F00D);
    StdRng::seed_from_u64(seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

fn random_bytes(r: &mut StdRng, max: usize) -> Vec<u8> {
    let len = r.gen_range(0..=max);
    (0..len).map(|_| r.gen()).collect()
}

/// A minimal PE32+ header block with the given sections `(name, rva, size)`.
fn image(timestamp: u32, sections: &[(String, u32, u32)]) -> Vec<u8> {
    let mut v = vec![0u8; 0x40];
    v[0] = b'M';
    v[1] = b'Z';
    let pe_at = 0x80u32;
    v[0x3c..0x40].copy_from_slice(&pe_at.to_le_bytes());
    v.resize(pe_at as usize, 0);
    v.extend_from_slice(b"PE\0\0");
    v.extend_from_slice(&0x8664u16.to_le_bytes());
    v.extend_from_slice(&(sections.len() as u16).to_le_bytes());
    v.extend_from_slice(&timestamp.to_le_bytes());
    v.extend_from_slice(&[0u8; 8]);
    let opt_size = 240u16;
    v.extend_from_slice(&opt_size.to_le_bytes());
    v.extend_from_slice(&0x22u16.to_le_bytes());
    let opt_start = v.len();
    v.resize(opt_start + opt_size as usize, 0);
    v[opt_start..opt_start + 2].copy_from_slice(&0x20bu16.to_le_bytes());
    v[opt_start + 60..opt_start + 64].copy_from_slice(&0x400u32.to_le_bytes());
    for (name, rva, size) in sections {
        let mut sec = [0u8; 40];
        sec[..name.len()].copy_from_slice(name.as_bytes());
        sec[8..12].copy_from_slice(&size.to_le_bytes());
        sec[12..16].copy_from_slice(&rva.to_le_bytes());
        v.extend_from_slice(&sec);
    }
    v
}

#[test]
fn a_header_that_was_built_is_read_back_exactly() {
    let mut r = rng(1);
    for i in 0..iters(500) {
        let sections: Vec<(String, u32, u32)> =
            (0..r.gen_range(0..12)).map(|k| (format!(".s{k}{}", r.gen_range(0..100)), r.gen(), r.gen())).collect();
        let timestamp: u32 = r.gen();
        let bytes = image(timestamp, &sections);
        let headers = pe::parse(&bytes).unwrap_or_else(|| panic!("iteration {i}: a built image does not parse"));
        assert_eq!(headers.timestamp, timestamp);
        assert_eq!(headers.size_of_headers, 0x400);
        let read: Vec<(String, u32, u32)> = headers.sections.iter().map(|s| (s.name.clone(), s.rva, s.size)).collect();
        assert_eq!(read, sections, "iteration {i}");
        // cut anywhere: the header is whole or refused, never half a table
        let cut = r.gen_range(0..bytes.len());
        if let Some(h) = pe::parse(&bytes[..cut]) {
            assert_eq!(h.sections.len(), sections.len(), "iteration {i}: cut at {cut}");
        }
    }
}

#[test]
fn bytes_that_are_not_a_header_are_refused_or_read_within_bounds() {
    let mut r = rng(2);
    let base = image(0x1234_5678, &[(".text".into(), 0x1000, 0x2000), (".rdata".into(), 0x3000, 0x100)]);
    for i in 0..iters(8000) {
        let bytes = match r.gen_range(0..4) {
            0 => random_bytes(&mut r, 600),
            1 => {
                // a valid header with a few bytes damaged, the way a corrupt or unfamiliar executable would be
                let mut b = base.clone();
                for _ in 0..r.gen_range(1..6) {
                    let at = r.gen_range(0..b.len());
                    b[at] = r.gen();
                }
                b
            }
            2 => {
                // the length-like fields at their worst: e_lfanew, section count, optional header size
                let mut b = base.clone();
                match r.gen_range(0..3) {
                    0 => b[0x3c..0x40].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF - r.gen_range(0..8)]),
                    1 => b[0x86..0x88].copy_from_slice(&u16::MAX.to_le_bytes()),
                    _ => b[0x94..0x96].copy_from_slice(&u16::MAX.to_le_bytes()),
                }
                b
            }
            _ => base[..r.gen_range(0..base.len())].to_vec(),
        };
        if let Some(h) = pe::parse(&bytes) {
            // whatever was read lies inside what was given
            assert!(h.sections.len() <= 0xFFFF);
            assert!(bytes.len() >= 0x80 + 4 + 20, "iteration {i}");
        }
    }
}

#[test]
fn patterns_parse_and_match_like_the_obvious_loop() {
    let mut r = rng(3);
    for i in 0..iters(3000) {
        let hay = random_bytes(&mut r, 400);
        let tokens: Vec<Option<u8>> =
            (0..r.gen_range(1..8)).map(|_| if r.gen_bool(0.3) { None } else { Some(r.gen_range(0..4)) }).collect();
        let text: Vec<String> = tokens.iter().map(|t| t.map_or("??".to_owned(), |b| format!("{b:02X}"))).collect();
        let text = text.join(" ");
        // a haystack with small bytes so that matches happen
        let hay: Vec<u8> = hay.iter().map(|b| b % 4).collect();
        let pattern = Pattern::parse(&text);
        if tokens.iter().all(Option::is_none) {
            assert!(pattern.is_none(), "{text}");
            continue;
        }
        let pattern = pattern.unwrap_or_else(|| panic!("iteration {i}: {text:?} does not parse"));
        assert_eq!(pattern.len(), tokens.len());
        let naive: Vec<usize> = (0..hay.len().saturating_sub(tokens.len() - 1))
            .filter(|&at| {
                tokens.iter().enumerate().all(|(k, t)| hay.get(at + k).is_some_and(|b| t.is_none_or(|x| x == *b)))
            })
            .filter(|&at| at + tokens.len() <= hay.len())
            .collect();
        assert_eq!(pattern.find_all(&hay), naive, "iteration {i}: {text}");
        match pattern.find_unique(&hay) {
            Ok(at) => assert_eq!(naive, [at]),
            Err(n) => assert_eq!(n, naive.len()),
        }
        // near a hint: the same answer as the global search whenever the local one is not ambiguous, and no
        // arithmetic trouble for any hint or radius
        let hint = [r.gen_range(0..500), usize::MAX, usize::MAX - 3, 0][r.gen_range(0..4)];
        let radius = [r.gen_range(0..300), usize::MAX, 0, 1][r.gen_range(0..4)];
        let near = pattern.find_near(&hay, hint, radius);
        if let Ok(at) = near {
            assert!(naive.contains(&at), "iteration {i}");
        }
        // a start past the end, or so far that adding the pattern's length would wrap
        for at in [hay.len(), hay.len() + 1, usize::MAX, usize::MAX - tokens.len() + 1, usize::MAX - 1] {
            assert!(!pattern.matches_at(&hay, at) || at + tokens.len() <= hay.len(), "iteration {i}: at {at}");
        }
    }
}

#[test]
fn pattern_text_of_any_kind_is_read_or_refused() {
    let mut r = rng(4);
    for _ in 0..iters(6000) {
        let text: String = (0..r.gen_range(0..40))
            .map(|_| {
                ["48", "8B", "??", "?", "4", "ZZ", " ", "  ", "\t", "é", "0x1", "FFF", "-1", "+1", "g0", "\u{0}"]
                    [r.gen_range(0..16)]
            })
            .collect::<Vec<_>>()
            .join([" ", "", "  "][r.gen_range(0..3)]);
        if let Some(p) = Pattern::parse(&text) {
            assert!(!p.is_empty() && p.len() == text.split_whitespace().count(), "{text:?}");
        }
    }
}

#[test]
fn detour_arithmetic_holds_for_any_prologue() {
    let mut r = rng(5);
    for i in 0..iters(3000) {
        let expected = random_bytes(&mut r, 40);
        let found: Vec<u8> = if r.gen_bool(0.5) {
            expected.iter().copied().chain(random_bytes(&mut r, 10)).collect()
        } else {
            random_bytes(&mut r, 40)
        };
        let checked = check_prologue(&found, &expected);
        if expected.len() < JMP_ABS_LEN {
            assert!(checked.is_err(), "iteration {i}");
        } else {
            assert_eq!(checked.is_ok(), found.starts_with(&expected), "iteration {i}");
        }
        let hook: u64 = r.gen();
        match patch_bytes(hook, expected.len()) {
            Ok(patch) => {
                assert_eq!(patch.len(), expected.len());
                assert_eq!(&patch[..JMP_ABS_LEN], &feverlink3_native::detour::jmp_abs(hook));
                assert!(patch[JMP_ABS_LEN..].iter().all(|b| *b == 0xCC));
            }
            Err(_) => assert!(expected.len() < JMP_ABS_LEN),
        }
        // a function address is below 2^47; the trampoline resumes right after the stolen bytes
        let target: u64 = r.gen_range(0..1 << 47);
        let tramp = trampoline_bytes(&expected, target);
        assert_eq!(&tramp[..expected.len()], &expected[..]);
        assert_eq!(tramp[expected.len()..], feverlink3_native::detour::jmp_abs(target + expected.len() as u64));
    }
}

#[test]
fn the_build_table_answers_for_every_timestamp() {
    let mut r = rng(6);
    for _ in 0..iters(20_000) {
        let stamp: u32 = r.gen();
        if let Some(build) = build_table::select(stamp) {
            assert_eq!(build.timestamp, stamp);
        }
    }
    for build in build_table::BUILDS {
        assert_eq!(build_table::select(build.timestamp).map(|b| b.name), Some(build.name));
    }
}
