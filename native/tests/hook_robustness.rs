//! Randomised robustness tests for the hook-side modules of the native crate: the control link, the hold machine,
//! the counters and the watch of released commands, and the hook plan.
//!
//! The control file is written by the Lua mod, inside the same process; the machine and the counters are driven by
//! the game's own threads. None of it may panic (a panic in the game process ends the game) and none of it may grow
//! without bound.
//!
//! Seeded; `FEVERLINK3_FUZZ_SCALE=50` runs everything 50 times longer, `FEVERLINK3_FUZZ_SEED=n` changes the seeds.

use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write as _;

use feverlink3_launcher::gamelink::{self, Line};
use feverlink3_launcher::luatext;
use feverlink3_native::build_table::{self, BuildAddrs, Rva};
use feverlink3_native::counters::{AddSource, AppliedWatch, Audit, Counters, MAX_SITES, WATCH_EXPIRY_MS, WATCH_SLOTS};
use feverlink3_native::hold::{Action, Admit, HoldConfig, HoldMachine};
use feverlink3_native::link::{
    parse_ctl_line, valid_kind_name, AddCounts, CtlMessage, CtlReader, Event, EventWriter, CTL_FILE, MAX_EVENTS_BYTES,
    MAX_HOLD_KINDS, MAX_LINE, MAX_READ,
};
use feverlink3_native::plan::{plan, Feature, HookConfig, Image, Step};
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

/// Fragments that make a Lua payload do something odd when glued together at random.
const PIECES: &[&str] = &[
    "{",
    "}",
    "[",
    "]",
    "=",
    ",",
    "\"",
    "\\",
    "-",
    "1",
    "0",
    "e",
    "E",
    ".",
    "1e300",
    "-1",
    "nil",
    "true",
    "false",
    "kinds",
    "id",
    "at",
    "n",
    "stamp",
    "build_proposal",
    "(",
    ")",
    "/",
    "0/0",
    "os.exit()",
    "function",
    "..",
    "--",
    "[[",
    "]]",
    "\\0",
    "\\255",
    "\\x",
    "\t",
    " ",
    "9007199254740993",
    "18446744073709551615",
];

fn random_text(r: &mut StdRng, max: usize) -> String {
    let mut s = String::new();
    for _ in 0..r.gen_range(0..=max) {
        match r.gen_range(0..20) {
            0..=9 => s.push_str(PIECES[r.gen_range(0..PIECES.len())]),
            10..=16 => s.push(r.gen_range(0x20u8..0x7f) as char),
            17 => s.push(['\n', '\r', '\0', '\u{7f}', '\u{2028}', '\u{85}'][r.gen_range(0..6)]),
            _ => s.push(['é', 'ß', '€', '\u{1F600}', '\u{FFFD}'][r.gen_range(0..5)]),
        }
    }
    s
}

/// A few edits (delete a stretch, insert a piece, flip a byte, repeat a stretch) on a text.
fn mutate(r: &mut StdRng, text: &str) -> String {
    let mut b = text.as_bytes().to_vec();
    for _ in 0..r.gen_range(0..6) {
        if b.is_empty() {
            break;
        }
        let at = r.gen_range(0..b.len());
        match r.gen_range(0..4) {
            0 => {
                let end = (at + r.gen_range(1..8)).min(b.len());
                b.drain(at..end);
            }
            1 => {
                let piece = PIECES[r.gen_range(0..PIECES.len())].as_bytes().to_vec();
                b.splice(at..at, piece);
            }
            2 => b[at] = r.gen_range(0x20u8..0x7f),
            _ => {
                let end = (at + r.gen_range(1..16)).min(b.len());
                let stretch = b[at..end].to_vec();
                b.splice(at..at, stretch);
            }
        }
    }
    String::from_utf8_lossy(&b).into_owned()
}

// ---- the control link

const CTL_TEMPLATES: &[(&str, &str)] = &[
    ("hold_on", r#"{["kinds"]={[1]="build_proposal",[2]="other"}}"#),
    ("hold_off", "{}"),
    ("release", r#"{["at"]=12000,["id"]=3}"#),
    ("cancel", r#"{["id"]=7}"#),
    ("terrain_apply", r#"{["id"]=2,["stamp"]=400}"#),
    ("ping", r#"{["n"]=-4}"#),
];

fn ctl_line(kind: &str, payload: &str) -> Line {
    Line { seq: 1, kind: kind.into(), from: "1".into(), payload: payload.into() }
}

#[test]
fn control_lines_of_any_shape_are_read_or_refused() {
    let mut r = rng(10);
    let (mut accepted, mut refused) = (0usize, 0usize);
    for _ in 0..iters(20_000) {
        let line = if r.gen_bool(0.8) {
            let (kind, payload) = CTL_TEMPLATES[r.gen_range(0..CTL_TEMPLATES.len())];
            ctl_line(kind, &mutate(&mut r, payload))
        } else {
            let kind =
                if r.gen_bool(0.5) { CTL_TEMPLATES[r.gen_range(0..6)].0.to_string() } else { random_text(&mut r, 6) };
            ctl_line(&kind, &random_text(&mut r, 120))
        };
        match parse_ctl_line(&line) {
            Ok(message) => {
                accepted += 1;
                match message {
                    CtlMessage::HoldOn { kinds } => {
                        assert!((1..=MAX_HOLD_KINDS).contains(&kinds.len()), "{line:?}");
                        assert!(kinds.iter().all(|k| valid_kind_name(k)), "{line:?}");
                    }
                    CtlMessage::Release { id, at } => assert!(id <= 1 << 53 && at <= 1 << 53, "{line:?}"),
                    CtlMessage::Cancel { id } => assert!(id <= 1 << 53, "{line:?}"),
                    CtlMessage::TerrainApply { id, stamp } => assert!(id <= 1 << 53 && stamp <= 1 << 53, "{line:?}"),
                    CtlMessage::HoldOff | CtlMessage::Ping { .. } => {}
                }
            }
            Err(e) => {
                refused += 1;
                assert!(!e.to_string().is_empty());
                assert!(e.to_string().len() < 200, "an error message must stay short: {e}");
            }
        }
    }
    assert!(accepted > 100 && refused > 100, "the mutations must reach both outcomes ({accepted} / {refused})");
}

#[test]
fn control_payloads_of_extreme_size_or_depth_are_refused_cheaply() {
    let deep = "{".repeat(200_000);
    let items: Vec<String> = (1..=20_000).map(|i| format!("[{i}]=\"k{i}\"")).collect();
    let wide = format!("{{[\"kinds\"]={{{}}}}}", items.join(","));
    let long_number = format!("{{[\"id\"]={}}}", "9".repeat(100_000));
    let long_string = format!("{{[\"kinds\"]={{[1]=\"{}\"}}}}", "a".repeat(1 << 20));
    for (kind, payload) in [
        ("release", deep.as_str()),
        ("hold_on", wide.as_str()),
        ("release", long_number.as_str()),
        ("hold_on", long_string.as_str()),
    ] {
        assert!(parse_ctl_line(&ctl_line(kind, payload)).is_err(), "{kind}: {} bytes", payload.len());
    }
}

/// Appends to a file the way the mod's writer does.
fn append(path: &std::path::Path, bytes: &[u8]) {
    OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(bytes).unwrap();
}

#[test]
fn the_control_reader_survives_any_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut r = rng(11);
    for round in 0..iters(150) {
        let path = dir.path().join(format!("ctl{round}.log"));
        let mut reader = CtlReader::new(path.clone());
        let mut seq = 0u64;
        let mut last = 0u64;
        for _ in 0..r.gen_range(1..12) {
            let mut chunk = Vec::new();
            for _ in 0..r.gen_range(0..8) {
                match r.gen_range(0..8) {
                    0..=2 => {
                        seq += r.gen_range(1..3);
                        chunk.extend(format!("{seq}\tping\t1\t{{[\"n\"]={seq}}}\n").bytes());
                    }
                    3 => chunk.extend(random_bytes(&mut r, 300)),
                    4 => chunk.extend(vec![b'x'; r.gen_range(MAX_LINE - 5..MAX_LINE + 50)]),
                    5 => chunk.push(b'\n'),
                    6 => chunk.extend(format!("{}\tping\t1\t{{}}\n", r.gen_range(0..seq + 2)).bytes()),
                    _ => chunk.extend(random_text(&mut r, 60).into_bytes()),
                }
            }
            append(&path, &chunk);
            if r.gen_bool(0.06) {
                let len = fs::metadata(&path).unwrap().len();
                OpenOptions::new().write(true).open(&path).unwrap().set_len(r.gen_range(0..=len)).unwrap();
            } else if r.gen_bool(0.03) {
                fs::remove_file(&path).unwrap();
            }
            let allowed_polls = fs::metadata(&path).map_or(0, |m| m.len() as usize) / MAX_READ + 3;
            let mut polls = 0;
            loop {
                polls += 1;
                assert!(polls <= allowed_polls + 1, "a poll must take at most {MAX_READ} bytes");
                let p = reader.poll();
                if p.gone {
                    last = 0;
                }
                for l in &p.lines {
                    assert!(l.seq > last, "seq {} after {last}", l.seq);
                    assert!(l.payload.len() <= MAX_LINE && l.kind.len() <= MAX_LINE);
                    last = l.seq;
                }
                assert_eq!(reader.last_seq(), last);
                if !reader.has_news() {
                    break;
                }
            }
        }
    }
}

#[test]
fn a_huge_line_is_thrown_away_one_bounded_read_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(CTL_FILE);
    let mut body = vec![b'a'; 1 << 20];
    body.extend_from_slice(b"\n1\tping\t1\t{}\n");
    append(&path, &body);
    let mut reader = CtlReader::new(path);
    let (mut polls, mut lines, mut discarded) = (0usize, 0usize, 0usize);
    while reader.has_news() {
        let p = reader.poll();
        polls += 1;
        lines += p.lines.len();
        discarded += p.discarded;
        assert!(polls < 100);
    }
    assert_eq!(polls, body.len().div_ceil(MAX_READ), "each poll takes at most MAX_READ bytes");
    assert_eq!((lines, discarded), (1, 1), "the huge line counts once and the next line is still read");
}

// ---- the event file

fn random_event(r: &mut StdRng) -> Event {
    let reasons = ["release", "hold_off", "silence", "timeout", "control_gone", "cancel"];
    match r.gen_range(0..9) {
        0 => Event::Ready {
            build: random_text(r, 20),
            version: random_text(r, 20),
            hooks: (0..r.gen_range(0..4)).map(|_| random_text(r, 12)).collect(),
            poolpatch: r.gen(),
            clock: r.gen(),
            kinds: (0..r.gen_range(0..5)).map(|_| (random_text(r, 12), r.gen())).collect(),
            sites: r.gen_range(0..1000),
        },
        1 => Event::Held { id: r.gen(), kind: r.gen(), site: r.gen(), ms: r.gen() },
        2 => Event::Released { id: r.gen(), reason: reasons[r.gen_range(0..6)], ms: r.gen() },
        3 => Event::Dropped { id: r.gen(), reason: reasons[r.gen_range(0..6)] },
        4 => Event::Applied { id: r.gen(), ok: r.gen() },
        5 => Event::Audit {
            counts: (0..r.gen_range(0..20)).map(|_| (r.gen(), r.gen())).collect(),
            total: r.gen(),
            unreadable: r.gen(),
            adds: AddCounts { tool: r.gen(), other: r.gen(), unknown: r.gen() },
            sites: (0..r.gen_range(0..10)).map(|_| (r.gen_range(0..64), r.gen())).collect(),
        },
        6 => Event::Holding {
            on: r.gen(),
            kinds: (0..r.gen_range(0..4)).map(|_| random_text(r, 12)).collect(),
            reason: "hold_on",
        },
        7 => Event::Pong { n: r.gen() },
        _ => Event::Error { place: random_text(r, 20), detail: random_text(r, 200) },
    }
}

/// What the mod's reader needs of an event line: one line, four fields, a payload in the Lua subset.
fn check_event_line(seq: u64, e: &Event) {
    let text = EventWriter::line(seq, e);
    assert!(text.ends_with('\n') && !text[..text.len() - 1].contains(['\n', '\r']), "{text:?}");
    let line = gamelink::parse_line(&text).unwrap_or_else(|| panic!("not a link line: {text:?}"));
    assert_eq!((line.seq, line.kind.as_str(), line.from.as_str()), (seq, e.kind(), "native"));
    let json = luatext::lua_to_json(&line.payload).unwrap_or_else(|| panic!("payload is not Lua literal: {text:?}"));
    let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
    if let Event::Error { place, detail } = e {
        assert_eq!(value["where"].as_str(), Some(place.as_str()));
        assert_eq!(value["detail"].as_str(), Some(detail.as_str()));
    }
}

#[test]
fn events_of_any_content_make_lines_the_mods_reader_accepts() {
    let mut r = rng(12);
    for i in 0..iters(10_000) {
        let e = random_event(&mut r);
        check_event_line(i as u64 + 1, &e);
    }
}

#[test]
fn the_event_file_stops_growing_and_numbering_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = EventWriter::open(dir.path()).unwrap();
    assert_eq!(w.next_seq(), 1);
    let small = Event::Error { place: "p".into(), detail: "d".repeat(900) };
    for _ in 0..500 {
        w.append(std::slice::from_ref(&small)).unwrap();
    }
    assert_eq!(EventWriter::open(dir.path()).unwrap().next_seq(), 501, "numbering continues after a reopen");
    let big = Event::Error { place: "p".into(), detail: "y".repeat(100_000) };
    let mut refused = None;
    for _ in 0..200 {
        if let Err(e) = w.append(std::slice::from_ref(&big)) {
            refused = Some(e);
            break;
        }
    }
    let refused = refused.expect("a mod that never reads the file must hit the limit");
    assert_eq!(refused.kind(), std::io::ErrorKind::StorageFull);
    let len = fs::metadata(w.path()).unwrap().len();
    assert!(len <= MAX_EVENTS_BYTES && len + 100_100 > MAX_EVENTS_BYTES, "{len}");
    assert!(w.append(&[]).is_ok(), "an empty batch is not an error");
}

// ---- the hold machine

const KINDS: [u8; 4] = [52, 7, 200, 1];

fn kind_table() -> Vec<(String, u8)> {
    vec![("build_proposal".into(), 52), ("other".into(), 7), ("third".into(), 200)]
}

fn random_ctl(r: &mut StdRng, max_id: u64) -> CtlMessage {
    let names = ["build_proposal", "other", "third", "unknown_kind"];
    let id = |r: &mut StdRng| if r.gen_bool(0.9) { r.gen_range(0..=max_id + 2) } else { r.gen_range(0..1u64 << 53) };
    match r.gen_range(0..6) {
        0 => {
            CtlMessage::HoldOn { kinds: (0..r.gen_range(1..5)).map(|_| names[r.gen_range(0..4)].to_string()).collect() }
        }
        1 => CtlMessage::HoldOff,
        2 => CtlMessage::Release { id: id(r), at: if r.gen_bool(0.5) { 0 } else { r.gen_range(0..3000) } },
        3 => CtlMessage::Cancel { id: id(r) },
        4 => CtlMessage::TerrainApply { id: id(r), stamp: r.gen_range(0..1u64 << 53) },
        _ => CtlMessage::Ping { n: r.gen() },
    }
}

#[test]
fn the_hold_machine_never_loses_or_duplicates_a_command() {
    let mut r = rng(13);
    for _ in 0..iters(300) {
        let cfg = HoldConfig {
            capacity: r.gen_range(0..80),
            max_hold_ms: r.gen_range(0..40_000),
            silence_ms: r.gen_range(0..20_000),
        };
        let mut m: HoldMachine<u64> = HoldMachine::new(cfg, kind_table());
        let (mut now, mut next_handle, mut max_id) = (0u64, 0u64, 0u64);
        let mut inside: BTreeSet<u64> = BTreeSet::new();
        for _ in 0..400 {
            now = match r.gen_range(0..40) {
                0 => r.gen(), // a wild clock
                1 | 2 => now.saturating_sub(r.gen_range(0..5000)),
                _ => now.saturating_add(r.gen_range(0..4000)),
            };
            match r.gen_range(0..9) {
                0 | 1 => m.handle(&random_ctl(&mut r, max_id), now),
                2..=4 => {
                    let kind = KINDS[r.gen_range(0..4)];
                    let admitted = m.admit(kind, now) == Admit::Hold;
                    if admitted || r.gen_bool(0.3) {
                        // the caller may capture without asking first: `hold` checks again
                        let handle = next_handle;
                        next_handle += 1;
                        match m.hold(kind, r.gen(), handle, now) {
                            Ok(id) => {
                                assert!(id > max_id, "ids grow");
                                max_id = id;
                                inside.insert(handle);
                            }
                            Err(back) => assert_eq!(back, handle, "a refused command is given back untouched"),
                        }
                    }
                }
                5 => m.tick(now),
                6 => {
                    let clock = if r.gen_bool(0.3) { None } else { Some(r.gen_range(0..3000)) };
                    let due = m.has_due(clock);
                    let actions = m.take_due(clock, now);
                    assert_eq!(due, !actions.is_empty());
                    for a in actions {
                        let (Action::Release { handle, .. } | Action::Drop { handle, .. }) = a;
                        assert!(inside.remove(&handle), "handle {handle} came back twice or never went in");
                    }
                }
                7 => {
                    if r.gen_bool(0.2) {
                        m.ctl_gone(now);
                    }
                }
                _ => {
                    let events = m.drain_events();
                    assert!(events.len() <= 520, "{} queued events", events.len());
                    for e in &events {
                        check_event_line(1, e);
                    }
                }
            }
            assert!(m.len() <= cfg.capacity, "the ring holds at most {} (has {})", cfg.capacity, m.len());
            assert_eq!(m.len(), inside.len());
            assert_eq!(m.held().len(), m.len());
        }
        // switching holding off sends everything back: nothing is stuck in the ring
        m.handle(&CtlMessage::HoldOff, now);
        for a in m.take_due(None, now) {
            let (Action::Release { handle, .. } | Action::Drop { handle, .. }) = a;
            assert!(inside.remove(&handle));
        }
        assert!(m.is_empty() && inside.is_empty());
    }
}

#[test]
fn a_writer_that_never_drains_cannot_make_the_machine_grow() {
    let cfg = HoldConfig { capacity: 64, max_hold_ms: 30_000, silence_ms: 10_000 };
    let mut m: HoldMachine<u32> = HoldMachine::new(cfg, kind_table());
    m.handle(&CtlMessage::HoldOn { kinds: vec!["build_proposal".into()] }, 0);
    for round in 0..5000u32 {
        let now = u64::from(round);
        m.handle(&CtlMessage::Ping { n: i64::from(round) }, now);
        m.handle(&CtlMessage::Release { id: u64::from(round) + 1_000_000, at: 0 }, now); // unknown id: an error
        if m.hold(52, 0, round, now).is_ok() {
            m.handle(&CtlMessage::Release { id: u64::from(round) + 1, at: 0 }, now);
        }
        m.take_due(None, now);
    }
    let events = m.drain_events();
    assert!(events.len() <= 513, "{} events kept", events.len());
    assert!(matches!(&events[0], Event::Error { place, detail } if place == "events" && detail.contains("lost")));
    assert!(m.drain_events().is_empty());
}

// ---- the counters and the watch

#[test]
fn the_counters_add_up_and_the_audit_speaks_only_of_changes() {
    let mut r = rng(14);
    let c = Counters::new();
    let mut kinds = [0u64; 256];
    let (mut unreadable, mut tool, mut other, mut unknown) = (0u64, 0u64, 0u64, 0u64);
    let mut sites = [0u64; MAX_SITES];
    for _ in 0..iters(20_000) {
        match r.gen_range(0..6) {
            0..=2 => {
                let k: u8 = r.gen();
                c.applied(k);
                kinds[k as usize] += 1;
            }
            3 => {
                c.applied_unreadable();
                unreadable += 1;
            }
            4 => {
                let site = if r.gen_bool(0.1) { usize::MAX } else { r.gen_range(0..MAX_SITES + 10) };
                c.added(AddSource::Tool(site));
                tool += 1;
                if site < MAX_SITES {
                    sites[site] += 1;
                }
            }
            _ => {
                if r.gen() {
                    c.added(AddSource::Other);
                    other += 1;
                } else {
                    c.added(AddSource::Unknown);
                    unknown += 1;
                }
            }
        }
    }
    let Event::Audit { counts, total, unreadable: u, adds, sites: s } = c.snapshot() else { panic!("not an audit") };
    let want: Vec<(u8, u64)> = (0..=255u8).filter(|k| kinds[*k as usize] > 0).map(|k| (k, kinds[k as usize])).collect();
    assert_eq!(counts, want);
    assert_eq!((total, u), (kinds.iter().sum::<u64>() + unreadable, unreadable));
    assert_eq!(c.total(), total);
    assert_eq!(adds, AddCounts { tool, other, unknown });
    let want_sites: Vec<(usize, u64)> = (0..MAX_SITES).filter(|i| sites[*i] > 0).map(|i| (i, sites[i])).collect();
    assert_eq!(s, want_sites);
    check_event_line(1, &c.snapshot());

    // the audit event: every interval, and only when the counts differ from the last one reported
    let fresh = Counters::new();
    let mut audit = Audit::new(100, 0);
    assert!(audit.poll(50, &fresh).is_none(), "before the interval");
    assert!(audit.poll(100, &fresh).is_none(), "nothing counted yet");
    fresh.applied(52);
    assert!(audit.poll(150, &fresh).is_none(), "the interval counts from the last poll");
    assert!(audit.poll(200, &fresh).is_some());
    assert!(audit.poll(400, &fresh).is_none(), "unchanged");
    fresh.added(AddSource::Other);
    assert!(audit.poll(600, &fresh).is_some());
    assert!(audit.poll(u64::MAX, &fresh).is_none());
    assert!(audit.poll(0, &fresh).is_none(), "a clock that goes backwards does not panic");
}

#[test]
fn the_watch_matches_a_simple_model() {
    let mut r = rng(15);
    let w = AppliedWatch::new();
    let mut slots: [Option<(usize, u64, u64)>; WATCH_SLOTS] = [None; WATCH_SLOTS];
    let mut results: VecDeque<(u64, bool)> = VecDeque::new();
    let mut lost = 0u64;
    let mut now = 0u64;
    for _ in 0..iters(30_000) {
        now = now.saturating_add(r.gen_range(0..WATCH_EXPIRY_MS / 20));
        let payload = match r.gen_range(0..40) {
            0 => 0,
            1 => usize::MAX,
            _ => r.gen_range(1..120usize),
        };
        let id: u64 = r.gen_range(0..1u64 << 62);
        match r.gen_range(0..8) {
            0..=2 => {
                let free = slots.iter().position(Option::is_none);
                let ok = payload != 0 && payload != usize::MAX && free.is_some();
                assert_eq!(w.arm(payload, id, now), ok, "arm {payload:#x}");
                if let (true, Some(i)) = (ok, free) {
                    slots[i] = Some((payload, id, now));
                }
            }
            3 | 4 => {
                let hit = slots.iter().position(|s| s.is_some_and(|(p, ..)| p == payload));
                let want = hit.filter(|_| payload != 0 && payload != usize::MAX).and_then(|i| slots[i]).map(|s| s.1);
                assert_eq!(w.claim(payload), want, "claim {payload:#x}");
                if let (Some(_), Some(i)) = (want, hit) {
                    slots[i] = None;
                }
            }
            5 => {
                let ok: bool = r.gen();
                let fits = results.len() < 64;
                assert_eq!(w.finish(id, ok), fits);
                if fits {
                    results.push_back((id, ok));
                } else {
                    lost += 1;
                }
            }
            6 => assert_eq!(w.next_result(), results.pop_front()),
            _ => {
                let mut gone = Vec::new();
                for s in &mut slots {
                    if let Some((_, sid, ms)) = *s {
                        if now.saturating_sub(ms) >= WATCH_EXPIRY_MS {
                            gone.push(sid);
                            *s = None;
                        }
                    }
                }
                assert_eq!(w.expire(now), gone);
            }
        }
        assert_eq!(w.is_idle(), slots.iter().all(Option::is_none));
        assert_eq!(w.lost(), lost);
    }
}

#[test]
fn a_payload_equal_to_the_slot_marker_is_never_watched() {
    // `usize::MAX` marks a slot being filled; if it were accepted as a payload the slot would stay taken forever
    let w = AppliedWatch::new();
    for _ in 0..WATCH_SLOTS * 2 {
        assert!(!w.arm(usize::MAX, 1, 0));
    }
    assert!(w.is_idle());
    assert!(w.arm(0x1000, 9, 0), "every slot is still free");
}

#[test]
fn the_watch_hands_every_verdict_over_once_across_threads() {
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    const N: u64 = 20_000;
    let w = Arc::new(AppliedWatch::new());
    let (tx, rx) = mpsc::channel::<(usize, u64)>();
    let interface = {
        let w = Arc::clone(&w);
        thread::spawn(move || {
            for id in 0..N {
                let payload = 0x1000 + id as usize * 16;
                while !w.arm(payload, id, 0) {
                    thread::yield_now();
                }
                tx.send((payload, id)).unwrap();
            }
        })
    };
    let simulation = {
        let w = Arc::clone(&w);
        thread::spawn(move || {
            for (payload, id) in rx {
                assert_eq!(w.claim(payload), Some(id));
                w.finish(id, id % 2 == 0);
            }
        })
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut got = Vec::new();
    while got.len() as u64 + w.lost() < N {
        match w.next_result() {
            Some(v) => got.push(v),
            None => {
                assert!(Instant::now() < deadline, "the verdicts stopped coming: {} of {N}", got.len());
                thread::yield_now();
            }
        }
    }
    interface.join().unwrap();
    simulation.join().unwrap();
    assert!(got.windows(2).all(|p| p[0].0 < p[1].0), "one producer, so verdicts come in order");
    assert!(got.iter().all(|(id, ok)| *ok == (id % 2 == 0)));
    assert!(w.is_idle() && w.next_result().is_none());
}

// ---- the plan

/// An image that answers for the planted ranges and invents the rest (or refuses everything).
struct Sparse {
    planted: Vec<(Rva, Vec<u8>)>,
    readable: bool,
}

impl Image for Sparse {
    fn read(&self, rva: Rva, len: usize) -> Option<Vec<u8>> {
        if !self.readable || u64::from(rva) + len as u64 > u64::from(u32::MAX) {
            return None;
        }
        let byte_at = |at: u64| {
            let planted = self.planted.iter().find(|(start, bytes)| {
                let start = u64::from(*start);
                at >= start && at < start + bytes.len() as u64
            });
            match planted {
                Some((start, bytes)) => bytes[(at - u64::from(*start)) as usize],
                None => (at.wrapping_mul(0x9E37_79B9) >> 7) as u8,
            }
        };
        Some((0..len as u64).map(|i| byte_at(u64::from(rva) + i)).collect())
    }
}

fn expected_bytes(b: &BuildAddrs) -> Vec<(Rva, &'static [u8])> {
    let mut v = Vec::new();
    for s in [&b.command_list_add, &b.apply_command, &b.town_seed].into_iter().flatten() {
        v.push((s.rva, s.prologue));
    }
    for s in [&b.command_move, &b.command_destroy, &b.handle_destroy].into_iter().flatten() {
        v.push((s.rva, s.prologue));
    }
    if let Some(s) = &b.script_pool {
        v.push((s.rva, s.expected));
    }
    v
}

#[test]
fn a_plan_only_ever_contains_what_the_image_confirms() {
    let mut r = rng(16);
    for build in build_table::BUILDS {
        let wanted = expected_bytes(build);
        let (mut with_steps, mut with_skips) = (0, 0);
        for _ in 0..iters(2_000) {
            let mut planted = Vec::new();
            for (rva, bytes) in &wanted {
                if r.gen_bool(0.8) {
                    let mut bytes = bytes.to_vec();
                    if r.gen_bool(0.2) && !bytes.is_empty() {
                        let i = r.gen_range(0..bytes.len());
                        bytes[i] ^= 1 << r.gen_range(0..8);
                    }
                    planted.push((*rva, bytes));
                }
            }
            let image = Sparse { planted, readable: r.gen_bool(0.95) };
            let cfg = HookConfig {
                hooks: r.gen(),
                hold: r.gen(),
                poolpatch: r.gen(),
                town_seed: r.gen(),
                force_seed: None,
                growth: Default::default(),
                luaseed: r.gen(),
            };
            let p = plan(build, cfg, &image);
            with_steps += usize::from(!p.steps.is_empty());
            with_skips += usize::from(!p.skipped.is_empty());
            for step in &p.steps {
                match step {
                    Step::Detour { rva, expected, .. } => {
                        assert_eq!(image.read(*rva, expected.len()).as_deref(), Some(*expected));
                    }
                    Step::CallPatch { rva, expected, .. } => {
                        assert_eq!(image.read(*rva, expected.len()).as_deref(), Some(*expected));
                    }
                    Step::Patch { rva, expected, replacement, .. } => {
                        assert_eq!(image.read(*rva, expected.len()).as_deref(), Some(*expected));
                        assert_eq!(expected.len(), replacement.len());
                    }
                }
            }
            assert!(!p.has(Feature::Counters) || cfg.hooks);
            assert!(!p.has(Feature::Hold) || (cfg.hooks && cfg.hold));
            assert!(!p.has(Feature::PoolPatch) || cfg.poolpatch);
            assert!(!p.has(Feature::Hold) || (p.engine.is_some() && p.kind_offset.is_some()));
            assert!(cfg.any() || p.steps.is_empty());
            assert!(p.steps.len() <= 4);
            assert!(!p.has(Feature::TownSeed) || cfg.town_seed);
            for line in p.report() {
                assert!(line.starts_with("hooks: "), "{line}");
            }
        }
        assert!(with_steps > 0 && with_skips > 0, "{}: both outcomes must be reached", build.name);
    }
}

#[test]
fn the_switches_read_any_text_a_lone_zero_turns_a_default_off_and_a_lone_one_an_opt_in_on() {
    let mut r = rng(17);
    for _ in 0..iters(5_000) {
        let value = random_text(&mut r, 6);
        let cfg =
            HookConfig::from_lookup(|k| (k != feverlink3_native::plan::FORCE_TOWN_SEED_VAR).then(|| value.clone()));
        // any text is a verdict (a number or an error) for the forced seed, never a panic
        let _ = feverlink3_native::plan::parse_seed(&value);
        let on = value.trim() == "1";
        // hooks, hold, the pool patch and the seed pin are on unless the text is a lone zero
        let default_on = value.trim() != "0";
        assert_eq!(cfg.town_seed, on, "{value:?}");
        assert_eq!(
            (cfg.hooks, cfg.hold, cfg.poolpatch, cfg.luaseed),
            (default_on, default_on, default_on, default_on),
            "{value:?}"
        );
        assert_eq!(cfg.any(), on || default_on);
        assert!(cfg.describe().starts_with("hooks="));
    }
}
