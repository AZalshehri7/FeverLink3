//! Randomised robustness tests for the launcher's parsers and the file link: random and mutated input to the Lua
//! text reader and writer, the log lines and the tail that reads them from a file the game writes, the UPnP and
//! Steam parsers, the VDF reader and the live controller's command file. Nothing may panic, nothing may allocate
//! absurd amounts, and what is valid must round-trip.
//!
//! Seeded, so a failure repeats; `FEVERLINK3_FUZZ_SCALE=50` runs every test 50 times longer and
//! `FEVERLINK3_FUZZ_SEED=n` changes the seeds.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::Ipv4Addr;
use std::time::Duration;

use feverlink3_core::session::MessageKind;
use feverlink3_launcher::gamelink::{self, Line, Tail};
use feverlink3_launcher::live::{Controller, CMD_FILE};
use feverlink3_launcher::session::{from_game_line, to_game_line, LINE_KINDS};
use feverlink3_launcher::steam::parse_vdf;
use feverlink3_launcher::steamnet;
use feverlink3_launcher::{luatext, upnp};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value};

// ------------------------------------------------------------------------------------------ memory measurement

thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

/// Counts the bytes the current thread holds, so a test can ask how much a call needed at its largest.
struct Tracking;

fn grow(n: usize) {
    let _ = LIVE.try_with(|live| {
        live.set(live.get() + n);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(live.get())));
    });
}

fn shrink(n: usize) {
    let _ = LIVE.try_with(|live| live.set(live.get().saturating_sub(n)));
}

// SAFETY: every call is forwarded to the system allocator unchanged; the counters are thread-local `Cell`s that
// need no allocation of their own.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        shrink(layout.size());
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            shrink(layout.size());
            grow(new_size);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Tracking = Tracking;

/// Runs `f` and returns its result with the most bytes it held at once, beyond what was held when it began.
fn peak_of<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let base = LIVE.with(Cell::get);
    PEAK.with(|p| p.set(base));
    let result = f();
    (result, PEAK.with(Cell::get).saturating_sub(base))
}

const MIB: usize = 1 << 20;

// ------------------------------------------------------------------------------------------------- generators

fn iters(base: usize) -> usize {
    base * std::env::var("FEVERLINK3_FUZZ_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

fn rng(salt: u64) -> StdRng {
    let seed: u64 = std::env::var("FEVERLINK3_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0xFEED_F00D);
    StdRng::seed_from_u64(seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

fn random_bytes(r: &mut StdRng, max: usize) -> Vec<u8> {
    let len = if r.gen_bool(0.3) { r.gen_range(0..16.min(max + 1)) } else { r.gen_range(0..=max) };
    (0..len).map(|_| r.gen()).collect()
}

/// Text with the awkward characters in it: quotes, backslashes, controls, NUL, non-ASCII, astral planes.
fn random_text(r: &mut StdRng, max_chars: usize) -> String {
    const SPECIAL: [char; 14] =
        ['"', '\\', '\n', '\r', '\t', '\0', '\u{7f}', '\u{85}', '\u{2028}', 'é', 'ß', '日', '😀', '\u{feff}'];
    let len = r.gen_range(0..=max_chars);
    (0..len)
        .map(|_| match r.gen_range(0..4) {
            0 => SPECIAL[r.gen_range(0..SPECIAL.len())],
            1 => char::from_u32(r.gen_range(0..0x30000)).unwrap_or('?'),
            _ => char::from(r.gen_range(b' '..=b'~')),
        })
        .collect()
}

fn mutate(r: &mut StdRng, bytes: &mut Vec<u8>) {
    for _ in 0..r.gen_range(1..=4) {
        match r.gen_range(0..8) {
            0 if !bytes.is_empty() => {
                let i = r.gen_range(0..bytes.len());
                bytes[i] ^= 1 << r.gen_range(0..8);
            }
            1 if !bytes.is_empty() => {
                let i = r.gen_range(0..bytes.len());
                bytes[i] = r.gen();
            }
            2 => {
                let at = r.gen_range(0..=bytes.len());
                let extra = random_bytes(r, 16);
                bytes.splice(at..at, extra);
            }
            3 if !bytes.is_empty() => {
                let a = r.gen_range(0..bytes.len());
                let b = r.gen_range(a..=bytes.len());
                bytes.drain(a..b);
            }
            4 if !bytes.is_empty() => {
                let a = r.gen_range(0..bytes.len());
                let b = r.gen_range(a..=bytes.len());
                let copy = bytes[a..b].to_vec();
                let at = r.gen_range(0..=bytes.len());
                bytes.splice(at..at, copy);
            }
            5 => {
                let keep = r.gen_range(0..=bytes.len());
                bytes.truncate(keep);
            }
            6 if !bytes.is_empty() => {
                let i = r.gen_range(0..bytes.len());
                bytes[i] =
                    [b'"', b'{', b'}', b'[', b']', b',', b'=', b'\\', b'0', b'-', b'(', b')'][r.gen_range(0..12)];
            }
            _ => bytes.extend(random_bytes(r, 8)),
        }
    }
}

const KEYS: &[&str] = &[
    "t", "speed", "ahead", "name", "version", "stamp", "seq", "body", "category", "from", "id", "players", "session",
    "started", "lead", "text", "parts", "money", "values", "ok", "detail", "only",
];

/// A JSON value of random shape that survives the trip to Lua and back unchanged: no null inside an array, no empty
/// array (both are written as `nil` or `{}`, which come back as a missing key and an empty object), numbers that
/// fit an i64 or a finite double.
fn json_value(r: &mut StdRng, depth: usize) -> Value {
    match r.gen_range(0..11) {
        0 => json!(r.gen::<bool>()),
        1 => json!(r.gen_range(-5i64..5)),
        2 => json!([i64::MIN, i64::MAX, 0, -1, 1 << 53, 255, 1 << 31][r.gen_range(0..7)]),
        3 => json!([0.5, -0.25, 1e308, 1e-308, 123456789.125, 1.1847772900119483e215][r.gen_range(0..6)]),
        4 => json!(r.gen::<f64>() * 1e6 - 5e5),
        5 | 6 => json!(random_text(r, 25)),
        7 if depth < 4 => Value::Array((0..r.gen_range(1..4)).map(|_| json_value(r, depth + 1)).collect()),
        _ if depth < 4 => {
            let mut map = serde_json::Map::new();
            for _ in 0..r.gen_range(0..6) {
                let key =
                    if r.gen_bool(0.85) { KEYS[r.gen_range(0..KEYS.len())].to_owned() } else { random_text(r, 6) };
                map.insert(key, json_value(r, depth + 1));
            }
            Value::Object(map)
        }
        _ => json!(r.gen_range(0..100)),
    }
}

// ------------------------------------------------------------------------------------------------------ luatext

#[test]
fn json_survives_the_trip_to_lua_and_back() {
    let mut r = rng(1);
    for i in 0..iters(1500) {
        let value = json_value(&mut r, 0);
        let bytes = serde_json::to_vec(&value).unwrap();
        let (lua, peak) = peak_of(|| luatext::json_to_lua(&bytes));
        let lua = lua.unwrap_or_else(|| panic!("iteration {i}: {value} does not translate"));
        assert!(peak <= 16 * bytes.len() + 64 * 1024, "iteration {i}: {peak} bytes for {} of JSON", bytes.len());
        assert!(!lua.contains(['\n', '\r']), "iteration {i}: one line");
        assert!(luatext::is_literal(&lua), "iteration {i}: {lua}");
        let back = luatext::lua_to_json(&lua).unwrap_or_else(|| panic!("iteration {i}: {lua} does not read back"));
        let back: Value = serde_json::from_slice(&back).unwrap();
        // a table whose keys are exactly 1..n is an array and nothing else is: an object with keys "1", "2" stays
        // an object because its keys are strings, but an empty object and an empty array are both `{}`
        assert_eq!(back, value, "iteration {i}: {lua}");
        // stable: the text of the value read back is the text it came from
        assert_eq!(luatext::json_to_lua(&serde_json::to_vec(&back).unwrap()).as_deref(), Some(lua.as_str()));
    }
}

/// Lua-looking text: pieces of the grammar, the mod's own escapes, and things that are not literals.
fn lua_like_text(r: &mut StdRng) -> String {
    const PIECES: [&str; 40] = [
        "{", "}", "[", "]", "=", ",", ";", "\"", "'", "\\", "\\n", "\\0", "\\255", "\\256", "\\\"", "nil", "true",
        "false", "1", "-1", "1.5", "1e+21", "1e999", "--", "(0/0)", "(1/0)", "(-1/0)", "(", ")", " ", "\t", "\n", "a",
        "name", "function", "end", "os", ".", ":", "[[",
    ];
    (0..r.gen_range(0..40)).map(|_| PIECES[r.gen_range(0..PIECES.len())]).collect()
}

#[test]
fn lua_text_of_any_shape_is_read_or_refused_and_never_costs_much() {
    let mut r = rng(2);
    for i in 0..iters(6000) {
        let text = match r.gen_range(0..4) {
            0 => lua_like_text(&mut r),
            1 => String::from_utf8_lossy(&random_bytes(&mut r, 200)).into_owned(),
            2 => random_text(&mut r, 100),
            _ => {
                let mut valid =
                    luatext::json_to_lua(&serde_json::to_vec(&json_value(&mut r, 0)).unwrap()).unwrap().into_bytes();
                mutate(&mut r, &mut valid);
                String::from_utf8_lossy(&valid).into_owned()
            }
        };
        let (json, peak) = peak_of(|| luatext::lua_to_json(&text));
        let (lenient, _) = peak_of(|| luatext::is_literal(&text));
        assert!(peak <= 64 * text.len() + 64 * 1024, "iteration {i}: {peak} bytes for {} bytes", text.len());
        if let Some(json) = json {
            assert!(lenient, "iteration {i}: the strict reader accepts what the validator refuses: {text:?}");
            // what is read can be written, and the text it is written as reads back to the same value
            let again = luatext::json_to_lua(&json).unwrap_or_else(|| panic!("iteration {i}: {text:?}"));
            assert_eq!(luatext::lua_to_json(&again).as_deref(), Some(json.as_slice()), "iteration {i}: {text:?}");
        }
    }
}

#[test]
fn huge_and_deep_text_is_refused_cheaply() {
    let (_, peak) = peak_of(|| {
        for open in ["{", "{[1]=", "{a=", "[", "(", "\"\\"] {
            let text = open.repeat(2_000_000);
            assert!(luatext::lua_to_json(&text).is_none() && !luatext::is_literal(&text));
        }
    });
    assert!(peak < 64 * MIB, "{peak}");
    // 4 MiB of elements: stopped by the value budget long before the memory is spent
    let many = format!("{{{}}}", "1,".repeat(2 * MIB));
    let (json, peak) = peak_of(|| luatext::lua_to_json(&many));
    assert!(json.is_none() && peak < 64 * MIB, "{peak}");
    // and by the size limit for one long string
    let long = format!("\"{}\"", "x".repeat(luatext::MAX_INPUT));
    let (json, peak) = peak_of(|| luatext::lua_to_json(&long));
    assert!(json.is_none() && peak < MIB, "{peak}");
}

// ----------------------------------------------------------------------------------------------- log lines

fn clean_field(r: &mut StdRng) -> String {
    random_text(r, 12).chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect()
}

#[test]
fn log_lines_round_trip_and_garbage_is_not_a_line() {
    let mut r = rng(3);
    for i in 0..iters(3000) {
        let line = Line {
            seq: r.gen_range(0..=gamelink::MAX_SEQ),
            kind: clean_field(&mut r),
            from: clean_field(&mut r),
            payload: random_text(&mut r, 60).replace(['\n', '\r'], " "),
        };
        assert_eq!(gamelink::parse_line(&gamelink::format_line(&line)), Some(line.clone()), "iteration {i}: {line:?}");
        // the same fields with the awkward characters left in come back cleaned, never corrupted
        let rough = Line {
            seq: line.seq,
            kind: random_text(&mut r, 8),
            from: random_text(&mut r, 8),
            payload: random_text(&mut r, 30),
        };
        let text = gamelink::format_line(&rough);
        assert_eq!(text.matches('\n').count(), 1, "iteration {i}: one physical line");
        let parsed = gamelink::parse_line(&text).unwrap_or_else(|| panic!("iteration {i}: {rough:?}"));
        assert_eq!(parsed.seq, rough.seq);
        assert!(!parsed.kind.contains(['\t', '\n', '\r']) && !parsed.from.contains(['\t', '\n', '\r']));

        // anything at all: a line or nothing, and a line is within the rules
        let garbage = match r.gen_range(0..3) {
            0 => random_text(&mut r, 80),
            1 => String::from_utf8_lossy(&random_bytes(&mut r, 100)).into_owned(),
            _ => format!(
                "{}\t{}\t{}\t{}",
                r.gen::<u64>(),
                random_text(&mut r, 5),
                random_text(&mut r, 5),
                random_text(&mut r, 5)
            ),
        };
        if let Some(l) = gamelink::parse_line(&garbage) {
            assert!(l.seq <= gamelink::MAX_SEQ && !l.kind.contains('\t') && !l.from.contains('\t'), "{garbage:?}");
        }
    }
}

/// What a tail must hand out for the complete lines of `content`: the lines whose number is above every number before.
fn model_of(content: &[u8]) -> Vec<Line> {
    let mut out = Vec::new();
    let mut last = 0;
    let mut rest = content;
    while let Some(nl) = rest.iter().position(|&b| b == b'\n') {
        let (line, after) = rest.split_at(nl + 1);
        rest = after;
        if let Some(l) = gamelink::parse_line(&String::from_utf8_lossy(line)) {
            if l.seq > last {
                last = l.seq;
                out.push(l);
            }
        }
    }
    out
}

#[test]
fn a_tail_reads_a_file_the_game_writes_in_any_pieces_and_any_state() {
    let mut r = rng(4);
    let dir = tempfile::tempdir().unwrap();
    for i in 0..iters(150) {
        let path = dir.path().join(format!("from_game_{i}.log"));
        // the content: valid lines, lines with old or huge numbers, garbage, blank lines, a last line that ends
        let mut content = Vec::new();
        let mut seq = 0u64;
        for _ in 0..r.gen_range(1..40) {
            match r.gen_range(0..8) {
                0 => content.extend(random_bytes(&mut r, 60)),
                1 => content.push(b'\n'),
                2 => {
                    seq = seq.saturating_sub(r.gen_range(0..3));
                    content.extend(
                        gamelink::format_line(&Line { seq, kind: "x".into(), from: "0".into(), payload: "old".into() })
                            .bytes(),
                    );
                }
                3 => content.extend(format!("{}\tx\t0\thuge\n", u64::MAX - r.gen_range(0..4)).bytes()),
                _ => {
                    seq += r.gen_range(1..4);
                    content.extend(
                        gamelink::format_line(&Line {
                            seq,
                            kind: "action".into(),
                            from: "1".into(),
                            payload: random_text(&mut r, 30).replace(['\n', '\r'], " "),
                        })
                        .bytes(),
                    );
                }
            }
        }
        content.push(b'\n');
        let expected = model_of(&content);

        // written in pieces of random size, the tail polled in between, sometimes the file rewritten from the start
        let mut tail = Tail::new(path.clone());
        let mut got: Vec<Line> = Vec::new();
        let mut written = 0;
        let mut file = fs::File::create(&path).unwrap();
        while written < content.len() {
            let piece = r.gen_range(1..=40).min(content.len() - written);
            file.write_all(&content[written..written + piece]).unwrap();
            file.flush().unwrap();
            written += piece;
            if r.gen_bool(0.5) {
                let (lines, peak) = peak_of(|| tail.poll().unwrap());
                assert!(peak < MIB, "iteration {i}: {peak}");
                got.extend(lines);
            }
        }
        got.extend(tail.poll().unwrap());
        assert_eq!(got, expected, "iteration {i}");
        assert_eq!(tail.last_seq(), expected.last().map_or(0, |l| l.seq));
        assert_eq!(gamelink::highest_seq(&path).unwrap(), tail.last_seq(), "iteration {i}");
        // the same file as one block
        assert_eq!(Tail::new(path.clone()).poll().unwrap(), expected);
    }
}

#[test]
fn a_tail_given_endless_noise_without_a_newline_holds_a_bounded_amount() {
    let mut r = rng(5);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("noise.log");
    let mut tail = Tail::new(path.clone());
    let mut file = OpenOptions::new().create(true).append(true).open(&path).unwrap();
    let mut total = 0usize;
    for i in 0..iters(300) {
        let mut noise = random_bytes(&mut r, 100_000);
        noise.retain(|b| *b != b'\n');
        total += noise.len();
        file.write_all(&noise).unwrap();
        file.flush().unwrap();
        let (lines, peak) = peak_of(|| tail.poll().unwrap());
        assert!(lines.is_empty());
        // at most one line (the limit) plus what one poll brought, and a copy of it
        assert!(peak <= 2 * (gamelink::MAX_LINE_BYTES + 100_000) + MIB, "iteration {i}: {peak} after {total} bytes");
    }
    // the noise ends, a real line follows: it is read
    file.write_all(b"\n").unwrap();
    file.write_all(
        gamelink::format_line(&Line { seq: 1, kind: "hello".into(), from: "0".into(), payload: "{}".into() })
            .as_bytes(),
    )
    .unwrap();
    file.flush().unwrap();
    let lines = tail.poll().unwrap();
    assert_eq!(lines.iter().map(|l| l.kind.as_str()).collect::<Vec<_>>(), ["hello"]);
}

#[test]
fn what_the_game_could_say_to_the_session_is_checked_not_trusted() {
    let mut r = rng(6);
    for i in 0..iters(3000) {
        let row = &LINE_KINDS[r.gen_range(0..LINE_KINDS.len())];
        let payload = match r.gen_range(0..3) {
            0 => lua_like_text(&mut r),
            1 => luatext::json_to_lua(&serde_json::to_vec(&json_value(&mut r, 0)).unwrap()).unwrap(),
            _ => random_text(&mut r, 80),
        };
        let line = Line {
            seq: i as u64 + 1,
            kind: row.name.to_owned(),
            from: random_text(&mut r, 3),
            payload: payload.clone(),
        };
        match from_game_line(&line) {
            Ok((kind, json)) => {
                assert!(row.from_game, "{} is not a kind a game sends", row.name);
                assert_eq!(kind, row.kind);
                assert!(serde_json::from_slice::<Value>(&json).is_ok());
            }
            Err(why) => assert!(!why.is_empty()),
        }
        // and the other way: any JSON a peer could have sent, towards the game
        let wire = serde_json::to_vec(&json_value(&mut r, 0)).unwrap();
        if let Some((name, from, lua)) = to_game_line(row.kind, &wire) {
            assert!(row.to_game && name == row.name);
            assert!(from.parse::<u64>().is_ok() && !lua.contains('\n'));
        }
        let mut raw = wire.clone();
        mutate(&mut r, &mut raw);
        let _ = to_game_line(row.kind, &raw);
    }
}

#[test]
fn an_action_body_reaches_the_game_only_as_a_literal() {
    let mut r = rng(7);
    for i in 0..iters(3000) {
        let body = if r.gen_bool(0.5) {
            luatext::json_to_lua(&serde_json::to_vec(&json_value(&mut r, 0)).unwrap()).unwrap()
        } else {
            lua_like_text(&mut r)
        };
        let payload = serde_json::to_vec(&json!({ "stamp": 1, "seq": 1, "from": 2, "body": body })).unwrap();
        let delivered = to_game_line(MessageKind::Action, &payload).is_some();
        assert_eq!(delivered, luatext::is_literal(&body), "iteration {i}: {body:?}");
    }
}

// ---------------------------------------------------------------------------------------------------- UPnP

fn xml_like(r: &mut StdRng) -> String {
    const PIECES: [&str; 28] = [
        "<root>",
        "</root>",
        "<service>",
        "</service>",
        "<serviceType>",
        "</serviceType>",
        "<controlURL>",
        "</controlURL>",
        "urn:schemas-upnp-org:service:WANIPConnection:1",
        "urn:schemas-upnp-org:service:WANPPPConnection:1",
        "/ctl/IPConn",
        "http://192.168.0.1:5000/x",
        "http://203.0.113.9/",
        "<NewExternalIPAddress>",
        "</NewExternalIPAddress>",
        "203.0.113.9",
        "999.1.1.1",
        "\r\n",
        "LOCATION: ",
        "Location:http://10.0.0.1/d.xml",
        "é",
        "日本",
        "😀",
        "<",
        ">",
        "/",
        ":",
        "http://",
    ];
    (0..r.gen_range(0..30)).map(|_| PIECES[r.gen_range(0..PIECES.len())]).collect()
}

#[test]
fn router_answers_of_any_shape_are_read_or_refused() {
    let mut r = rng(8);
    for i in 0..iters(6000) {
        let text = match r.gen_range(0..3) {
            0 => xml_like(&mut r),
            1 => random_text(&mut r, 120),
            _ => {
                let mut b = xml_like(&mut r).into_bytes();
                mutate(&mut r, &mut b);
                String::from_utf8_lossy(&b).into_owned()
            }
        };
        let base =
            ["http://192.168.0.1:5000/rootDesc.xml", "http://203.0.113.9/x", "http://router/", "", text.as_str()]
                [r.gen_range(0..5)];
        if let Some(location) = upnp::parse_location(&text) {
            assert!(upnp::is_lan_url(&location), "iteration {i}: {location}");
        }
        if let Some((control, service)) = upnp::parse_control_url(&text, base) {
            assert!(upnp::is_lan_url(&control), "iteration {i}: {control}");
            assert!(service.starts_with("urn:schemas-upnp-org:service:WAN"));
        }
        let _ = upnp::parse_external_ip(&text);
        if let Some((authority, path)) = upnp::split_url(&text) {
            assert!(!authority.contains(char::is_whitespace) && !path.contains(char::is_control), "iteration {i}");
        }
        let _ = upnp::is_lan_url(&text);
    }
    // addresses: the rule agrees with the standard library's notion of private for every address of a sample
    for _ in 0..iters(20_000) {
        let ip = Ipv4Addr::from(r.gen::<u32>());
        let o = ip.octets();
        let expected =
            ip.is_private() || ip.is_loopback() || ip.is_link_local() || (o[0] == 100 && (64..128).contains(&o[1]));
        assert_eq!(upnp::is_private(ip), expected, "{ip}");
        assert_eq!(upnp::is_lan_url(&format!("http://{ip}:80/x")), expected && !ip.is_loopback(), "{ip}");
    }
}

// -------------------------------------------------------------------------------------------------- Steam

#[test]
fn steam_structures_of_any_size_and_content_are_read_or_refused() {
    let mut r = rng(9);
    for i in 0..iters(6000) {
        let bytes = random_bytes(&mut r, 900);
        let _ = steamnet::parse_status_event(&bytes);
        let _ = steamnet::steam_id_of(&bytes);
        let _ = steamnet::relay_pop(&bytes);
        if let Some(reply) = steamnet::reply_for(&bytes) {
            assert!(
                bytes.starts_with(b"ping ") && reply.starts_with(b"pong ") && reply.len() == bytes.len(),
                "iteration {i}"
            );
        } else {
            assert!(!bytes.starts_with(b"ping "));
        }
        let _ = steamnet::pong_id(&bytes);
        // a structure at the size Steam gives, with random content
        let mut full = vec![0u8; 708];
        r.fill(&mut full[..]);
        if let Some(e) = steamnet::parse_status_event(&full) {
            assert!((-1..=5).contains(&e.state) && (-1..=5).contains(&e.old_state));
        }
        full.truncate(r.gen_range(0..708));
        assert!(steamnet::parse_status_event(&full).is_none());
        // identities and Steam IDs
        let id: u64 = r.gen();
        assert_eq!(steamnet::steam_id_of(&steamnet::identity_bytes(id)), Some(id));
        let digits = format!("{}", r.gen_range(1_000_000_000_000_000u64..u64::MAX));
        assert_eq!(
            steamnet::parse_steam_id(&format!(" {digits}\t")),
            digits.parse().ok().filter(|_| digits.len() <= 20)
        );
        let text = random_text(&mut r, 24);
        if let Some(parsed) = steamnet::parse_steam_id(&text) {
            assert!(text.trim().bytes().all(|b| b.is_ascii_digit()) && text.trim().parse::<u64>() == Ok(parsed));
        }
        let ping = format!("pong {}", r.gen::<u64>());
        assert!(steamnet::pong_id(ping.as_bytes()).is_some());
    }
}

#[test]
fn a_vdf_file_of_any_content_is_read_without_trouble() {
    let mut r = rng(10);
    let sample =
        "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Steam\"\n\t\t\"apps\" { \"1\" \"2\" }\n\t}\n}\n";
    for i in 0..iters(3000) {
        let text = match r.gen_range(0..3) {
            0 => random_text(&mut r, 200),
            1 => {
                let mut b = sample.as_bytes().to_vec();
                mutate(&mut r, &mut b);
                String::from_utf8_lossy(&b).into_owned()
            }
            _ => (0..r.gen_range(0..60))
                .map(|_| ["{", "}", "\"", "\\", "a", " ", "\n", "//", "\"k\" ", "{ "][r.gen_range(0..10)])
                .collect(),
        };
        let (_, peak) = peak_of(|| parse_vdf(&text));
        assert!(peak <= 200 * text.len() + 64 * 1024, "iteration {i}: {peak} for {} bytes", text.len());
        let _ = feverlink3_launcher::steam::library_paths(&text, std::path::Path::new("C:\\Steam"));
    }
    // a megabyte of opening braces: no stack overflow, bounded memory
    let deep = "{".repeat(MIB);
    let (_, peak) = peak_of(|| parse_vdf(&deep));
    assert!(peak < 128 * MIB, "{peak}");
}

// ---------------------------------------------------------------------------------------- the live window

#[test]
fn the_controller_survives_whatever_is_in_the_command_file() {
    let mut r = rng(11);
    let dir = tempfile::tempdir().unwrap();
    let mut controller = Controller::new(dir.path().to_path_buf(), "Pat".into(), Vec::new()).unwrap();
    let kinds =
        ["join", "leave", "start", "pause", "speed", "check", "resync", "steamtest", "host_not", "", "JOIN", "x\u{0}y"];
    let mut seq = 0u64;
    let mut file = OpenOptions::new().create(true).append(true).open(dir.path().join(CMD_FILE)).unwrap();
    for i in 0..iters(400) {
        seq += 1;
        let code = match r.gen_range(0..4) {
            0 => random_text(&mut r, 100),
            1 => "A".repeat(r.gen_range(0..5000)),
            2 => "MFRGGZDFMZTWQ2LKNNWG23TPOBYXE43UOV3HO6DZPI======".to_owned(),
            _ => String::new(),
        };
        let payload = match r.gen_range(0..5) {
            0 => format!(
                "{{[\"code\"]={:?},[\"name\"]={:?},[\"password\"]={:?}}}",
                code,
                random_text(&mut r, 40),
                random_text(&mut r, 10)
            ),
            1 => format!("{{[\"n\"]={},[\"id\"]=\"{}\"}}", r.gen::<i64>(), r.gen::<u64>()),
            2 => lua_like_text(&mut r),
            3 => format!("{{[\"port\"]={},[\"code\"]={}}}", r.gen::<i64>(), r.gen::<f64>()),
            _ => String::from_utf8_lossy(&random_bytes(&mut r, 80)).into_owned(),
        };
        let line = Line { seq, kind: kinds[r.gen_range(0..kinds.len())].to_owned(), from: "0".into(), payload };
        file.write_all(gamelink::format_line(&line).as_bytes()).unwrap();
        if r.gen_bool(0.1) {
            file.write_all(&random_bytes(&mut r, 200)).unwrap();
            file.write_all(b"\n").unwrap();
        }
        file.flush().unwrap();
        controller.step();
        assert!(
            ["idle", "failed", "joining"].contains(&controller.state_name()),
            "iteration {i}: {}",
            controller.state_name()
        );
        if controller.state_name() == "joining" {
            // a code that happens to be valid points nowhere reachable: give the attempt time to fail, then go on
            for _ in 0..200 {
                controller.step();
                if controller.state_name() != "joining" {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}
