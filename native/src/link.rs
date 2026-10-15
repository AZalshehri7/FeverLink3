//! The control link between the Lua mod and the native module (`docs/NATIVE_LINK.md` section 3).
//!
//! Two append-only text files in the session folder, in exactly the shape of the launcher link
//! (`docs/ARCHITECTURE.md` section 2): one record per line, `seq<TAB>kind<TAB>from<TAB>payload`, the payload one
//! line of Lua literal syntax, `seq` growing by one per line, a reader keeping the last `seq` it dispatched. The
//! mod therefore reads and writes them with the helpers it already has (`C.newWriter`, `C.newReader`, `C.parse`).
//!
//! - `native_ctl.log`: the mod writes, the module reads ([`CtlReader`], [`CtlMessage`]).
//! - `native_events.log`: the module writes, the mod reads ([`EventWriter`], [`Event`]).
//!
//! Everything here is pure or plain file access. The reader is bounded (a line longer than [`MAX_LINE`] is thrown
//! away, at most [`MAX_READ`] bytes are taken per poll) because the content comes from outside the process.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use feverlink3_launcher::gamelink::{self, Line};
use feverlink3_launcher::luatext;
use serde_json::Value;

use crate::engine::BuildFlags;

/// The mod writes, the module reads.
pub const CTL_FILE: &str = "native_ctl.log";
/// The module writes, the mod reads.
pub const EVENTS_FILE: &str = "native_events.log";
/// The `from` column of every event line.
pub const EVENTS_FROM: &str = "native";
/// A control line longer than this is thrown away.
pub const MAX_LINE: usize = 8 * 1024;
/// Most bytes read from the control file in one poll.
pub const MAX_READ: usize = 64 * 1024;
/// Most kind names one `hold_on` may carry.
pub const MAX_HOLD_KINDS: usize = 16;
/// The events file stops growing here: a mod that never reads it must not fill the disk.
pub const MAX_EVENTS_BYTES: u64 = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------------------------
// Mod to module
// ---------------------------------------------------------------------------------------------------------------

/// One message of `native_ctl.log`.
#[derive(Debug, Clone, PartialEq)]
pub enum CtlMessage {
    /// Start holding commands of these kinds (names of the build's kind table).
    HoldOn { kinds: Vec<String> },
    /// Stop holding and release everything now, in order.
    HoldOff,
    /// Release held command `id` when the simulation reaches game time `at`; 0 means now.
    Release { id: u64, at: u64 },
    /// Drop held command `id`.
    Cancel { id: u64 },
    /// Keep held command `id` until this game's own apply of the same build at the stamp (the `inject_next` that names it as `held`), then
    /// fire the tool's callback with that apply's result instead of applying the held command. Never released after this line: a
    /// deferred command that is not completed is failed (holding off, timeout): its callback is fired with a failed result and it is
    /// destroyed, so the build cannot be applied twice and the tool is not left busy.
    Defer { id: u64 },
    /// Held command `id` gets no stamp apply of its own on this game (its replay was refused or failed): fire its tool callback with
    /// the command's own "not applied" result and destroy it now.
    Fail { id: u64 },
    /// The NEXT script `build_proposal` the simulation applies is this game's stamp apply of deferred command `held`, built from the
    /// record (not injected): the held command takes that command's result, as with `inject_next {held}`.
    CompleteNext { held: u64, ttl: u64, tm: String },
    /// Apply the staged terrain edit `id` at `stamp` (stage 0.4: parsed, not acted on).
    TerrainApply { id: u64, stamp: u64 },
    /// Liveness; answered with `pong`.
    Ping { n: i64 },
    /// `FEVERLINK3_NATIVE_PATCH`: patch the NEXT script `build_proposal` command that reaches the command list (not a held one):
    /// its `frozenNodes` get these indices into `proposal.addedNodes`, which must hold exactly `adds` nodes. Expires after `ttl` ms
    /// (0: the module's default).
    /// `at` (optional, 1 to 64 `{x, y, z}` positions) names the nodes by position instead: each must match exactly one node of the
    /// converted proposal within 0.05 m, and then `frozen` and `adds` are not needed (and not used).
    PatchNext { frozen: Vec<u32>, adds: u32, at: Vec<[f64; 3]>, ttl: u64 },
    /// `FEVERLINK3_NATIVE_INJECT`: the NEXT script `build_proposal` command (a placeholder without construction) gets the engine `Proposal`
    /// the module builds natively from the portable text in `native_inject_<key>.txt` (`inject.rs`). Expires after `ttl` ms (0: default).
    /// `held` (0: none) is this game's deferred held command of the same build (`defer`): it completes with this command's result.
    /// `tm` (optional) is the mod's timing text of the build; the module adds the rebuild and apply times and writes it back (`timing`).
    /// `refuse` (optional, a test switch: `FEVERLINK3_DEBUG_REFUSE_INJECT`): the module refuses the order with this reason once a command
    /// passed the identity checks, exactly like any other refusal (the command is emptied, `held` is failed).
    InjectNext { key: u64, ttl: u64, held: u64, tm: String, refuse: String },
}

/// Why a control line could not be turned into a [`CtlMessage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtlError {
    /// A kind this module does not know.
    UnknownKind(String),
    /// The payload is not in the accepted Lua literal subset.
    Malformed,
    /// The payload parsed but a field is missing or out of range; says which.
    BadPayload(&'static str),
}

impl std::fmt::Display for CtlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CtlError::UnknownKind(k) => write!(f, "unknown control kind {k:?}"),
            CtlError::Malformed => f.write_str("payload is not Lua literal syntax"),
            CtlError::BadPayload(why) => write!(f, "bad payload: {why}"),
        }
    }
}

/// Most indices one `patch_next` may carry.
pub const MAX_PATCH_FROZEN: usize = 64;

/// A kind name of the control link: lower-case letters, digits and `_`, 1 to 32 characters.
pub fn valid_kind_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

fn parse_payload(text: &str) -> Result<Value, CtlError> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Default::default())); // a hand-written `hold_off` line may leave it out
    }
    let json = luatext::lua_to_json(text).ok_or(CtlError::Malformed)?;
    serde_json::from_slice(&json).map_err(|_| CtlError::Malformed)
}

/// A whole number from 0 to 2^53 (the range a Lua double holds exactly). The mod writes integers as `%d` and
/// anything else as `%.17g`, so a float with no fraction is accepted too.
fn whole(v: &Value) -> Option<u64> {
    const LIMIT: f64 = 9_007_199_254_740_992.0;
    if let Some(n) = v.as_u64() {
        return (n as f64 <= LIMIT).then_some(n);
    }
    let f = v.as_f64()?;
    ((0.0..=LIMIT).contains(&f) && f.fract() == 0.0).then_some(f as u64)
}

/// The optional `tm` member of an order: the mod's timing text of a build (plain printable text, at most 1024 bytes).
fn timing_text(p: &Value) -> Result<String, CtlError> {
    match p.get("tm") {
        None => Ok(String::new()),
        Some(v) => {
            let t = v.as_str().ok_or(CtlError::BadPayload("tm must be a string"))?;
            if t.len() > 1024 || t.chars().any(|c| c.is_control()) {
                return Err(CtlError::BadPayload("tm must be printable text of at most 1024 bytes"));
            }
            Ok(t.to_string())
        }
    }
}

/// An optional short text member (plain printable text, at most 200 bytes; absent: empty).
fn short_text(p: &Value, key: &str) -> Result<String, CtlError> {
    match p.get(key) {
        None => Ok(String::new()),
        Some(v) => {
            let t = v.as_str().ok_or(CtlError::BadPayload("a text member must be a string"))?;
            if t.len() > 200 || t.chars().any(|c| c.is_control()) {
                return Err(CtlError::BadPayload("a text member must be printable text of at most 200 bytes"));
            }
            Ok(t.to_string())
        }
    }
}

fn required(p: &Value, key: &str, why: &'static str) -> Result<u64, CtlError> {
    p.get(key).and_then(whole).ok_or(CtlError::BadPayload(why))
}

fn optional(p: &Value, key: &str, why: &'static str) -> Result<u64, CtlError> {
    match p.get(key) {
        None => Ok(0),
        Some(v) => whole(v).ok_or(CtlError::BadPayload(why)),
    }
}

/// Turns one record of `native_ctl.log` into a message.
pub fn parse_ctl_line(line: &Line) -> Result<CtlMessage, CtlError> {
    let p = parse_payload(&line.payload)?;
    match line.kind.as_str() {
        "hold_on" => {
            let list = p.get("kinds").and_then(Value::as_array).ok_or(CtlError::BadPayload("kinds must be a list"))?;
            if list.is_empty() || list.len() > MAX_HOLD_KINDS {
                return Err(CtlError::BadPayload("kinds must name 1 to 16 kinds"));
            }
            let mut kinds = Vec::with_capacity(list.len());
            for item in list {
                match item.as_str().filter(|s| valid_kind_name(s)) {
                    Some(name) => kinds.push(name.to_string()),
                    None => return Err(CtlError::BadPayload("a kind name is not a lower-case identifier")),
                }
            }
            Ok(CtlMessage::HoldOn { kinds })
        }
        "hold_off" => Ok(CtlMessage::HoldOff),
        "release" => Ok(CtlMessage::Release {
            id: required(&p, "id", "release needs a numeric id")?,
            at: optional(&p, "at", "at must be a game time")?,
        }),
        "cancel" => Ok(CtlMessage::Cancel { id: required(&p, "id", "cancel needs a numeric id")? }),
        "defer" => Ok(CtlMessage::Defer { id: required(&p, "id", "defer needs a numeric id")? }),
        "fail" => Ok(CtlMessage::Fail { id: required(&p, "id", "fail needs a numeric id")? }),
        "complete_next" => Ok(CtlMessage::CompleteNext {
            held: required(&p, "held", "complete_next needs a held command id")?,
            ttl: optional(&p, "ttl", "ttl must be a number of milliseconds")?,
            tm: timing_text(&p)?,
        }),
        "terrain_apply" => Ok(CtlMessage::TerrainApply {
            id: required(&p, "id", "terrain_apply needs a numeric id")?,
            stamp: optional(&p, "stamp", "stamp must be a game time")?,
        }),
        "patch_next" => {
            let mut at = Vec::new();
            if let Some(v) = p.get("at") {
                let list = v.as_array().ok_or(CtlError::BadPayload("at must be a list of {x, y, z}"))?;
                if list.is_empty() || list.len() > MAX_PATCH_FROZEN {
                    return Err(CtlError::BadPayload("at must hold 1 to 64 positions"));
                }
                for item in list {
                    let xyz = item.as_array().filter(|a| a.len() == 3);
                    let n: Option<Vec<f64>> = xyz.map(|a| a.iter().filter_map(Value::as_f64).collect());
                    match n {
                        Some(n) if n.len() == 3 && n.iter().all(|f| f.is_finite()) => at.push([n[0], n[1], n[2]]),
                        _ => return Err(CtlError::BadPayload("a position in at is not {x, y, z} of finite numbers")),
                    }
                }
            }
            let mut frozen = Vec::new();
            if at.is_empty() || p.get("frozen").is_some() {
                let list =
                    p.get("frozen").and_then(Value::as_array).ok_or(CtlError::BadPayload("frozen must be a list"))?;
                if list.is_empty() || list.len() > MAX_PATCH_FROZEN {
                    return Err(CtlError::BadPayload("frozen must hold 1 to 64 indices"));
                }
                for item in list {
                    let n = whole(item).and_then(|n| u32::try_from(n).ok());
                    frozen.push(n.ok_or(CtlError::BadPayload("a frozen index is not a whole number"))?);
                }
            }
            let adds = if at.is_empty() || p.get("adds").is_some() {
                u32::try_from(required(&p, "adds", "patch_next needs the number of added nodes")?)
                    .map_err(|_| CtlError::BadPayload("adds is too large"))?
            } else {
                0
            };
            Ok(CtlMessage::PatchNext {
                frozen,
                adds,
                at,
                ttl: optional(&p, "ttl", "ttl must be a number of milliseconds")?,
            })
        }
        "inject_next" => Ok(CtlMessage::InjectNext {
            key: required(&p, "key", "inject_next needs a numeric key")?,
            ttl: optional(&p, "ttl", "ttl must be a number of milliseconds")?,
            held: optional(&p, "held", "held must be a held command id")?,
            tm: timing_text(&p)?,
            refuse: short_text(&p, "refuse")?,
        }),
        "ping" => {
            let n = match p.get("n") {
                None => 0,
                Some(v) => v
                    .as_i64()
                    .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0 && f.abs() <= 9.0e15).map(|f| f as i64))
                    .ok_or(CtlError::BadPayload("n must be a whole number"))?,
            };
            Ok(CtlMessage::Ping { n })
        }
        other => Err(CtlError::UnknownKind(other.chars().take(40).collect())),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Reading the control file
// ---------------------------------------------------------------------------------------------------------------

/// What one poll of the control file found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Poll {
    /// New, complete, valid records in file order.
    pub lines: Vec<Line>,
    /// The file existed before and is gone now. Reported once.
    pub gone: bool,
    /// Records thrown away: longer than [`MAX_LINE`], not a link record, or a `seq` not above the last one.
    pub discarded: usize,
    /// A read error (the file is retried on the next poll).
    pub error: Option<String>,
}

/// Reads new records from `native_ctl.log`: tolerates a missing file, a partial last line, truncation and a
/// rewrite; a `seq` that is not above the last dispatched one is dropped, so history never plays twice.
#[derive(Debug)]
pub struct CtlReader {
    path: PathBuf,
    /// Bytes of the file consumed; an incomplete last line waits in `pending`.
    offset: u64,
    last_seq: u64,
    pending: Vec<u8>,
    /// In the middle of a line longer than [`MAX_LINE`]: everything up to its newline is dropped.
    skipping: bool,
    existed: bool,
}

impl CtlReader {
    /// A reader that takes the file from its first byte.
    pub fn new(path: PathBuf) -> CtlReader {
        CtlReader { path, offset: 0, last_seq: 0, pending: Vec::new(), skipping: false, existed: false }
    }

    /// A reader that treats whatever the file holds now as history: it starts at the end and remembers the
    /// highest `seq` in it. Used when the link opens, so lines of an earlier run are never acted on.
    pub fn starting_at_end(path: PathBuf) -> io::Result<CtlReader> {
        let mut r = CtlReader::new(path);
        match fs::metadata(&r.path) {
            Ok(m) => {
                r.offset = m.len();
                r.existed = true;
                r.last_seq = tail_highest_seq(&r.path)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(r)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// True when the file is longer than what was consumed: the cheap check (a metadata call, no open) that lets a
    /// caller on a hot path decide whether a [`poll`](Self::poll) is worth it.
    pub fn has_news(&self) -> bool {
        match fs::metadata(&self.path) {
            Ok(m) => m.len() != self.offset,
            Err(_) => self.existed,
        }
    }

    pub fn poll(&mut self) -> Poll {
        let mut out = Poll::default();
        let len = match fs::metadata(&self.path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if self.existed {
                    // gone for good as far as this session goes: a fresh file starts a fresh numbering
                    self.existed = false;
                    self.offset = 0;
                    self.last_seq = 0;
                    self.pending.clear();
                    self.skipping = false;
                    out.gone = true;
                }
                return out;
            }
            Err(e) => {
                out.error = Some(format!("{}: {e}", self.path.display()));
                return out;
            }
        };
        self.existed = true;
        if len < self.offset {
            // truncated or rewritten: read from the top again, the seq filter keeps old lines out
            self.offset = 0;
            self.pending.clear();
            self.skipping = false;
        }
        if len == self.offset {
            return out;
        }
        let mut chunk = Vec::new();
        let read = File::open(&self.path).and_then(|mut f| {
            f.seek(SeekFrom::Start(self.offset))?;
            f.take(MAX_READ as u64).read_to_end(&mut chunk)
        });
        if let Err(e) = read {
            out.error = Some(format!("{}: {e}", self.path.display()));
            return out;
        }
        self.offset += chunk.len() as u64;
        self.feed(&chunk, &mut out);
        out
    }

    fn feed(&mut self, bytes: &[u8], out: &mut Poll) {
        for &b in bytes {
            if b == b'\n' {
                if std::mem::take(&mut self.skipping) {
                    self.pending.clear();
                } else {
                    let raw = std::mem::take(&mut self.pending);
                    self.accept(&raw, out);
                }
            } else if self.skipping {
                // the rest of an oversize line
            } else if self.pending.len() >= MAX_LINE {
                self.pending.clear();
                self.skipping = true;
                out.discarded += 1;
            } else {
                self.pending.push(b);
            }
        }
    }

    fn accept(&mut self, raw: &[u8], out: &mut Poll) {
        let text = String::from_utf8_lossy(raw);
        if text.trim().is_empty() {
            return;
        }
        match gamelink::parse_line(&text) {
            Some(line) if line.seq > self.last_seq => {
                self.last_seq = line.seq;
                out.lines.push(line);
            }
            _ => out.discarded += 1,
        }
    }
}

/// The highest `seq` among the complete records in the last [`MAX_READ`] bytes of a file; 0 when there is none.
fn tail_highest_seq(path: &Path) -> io::Result<u64> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let len = f.metadata()?.len();
    let from = len.saturating_sub(MAX_READ as u64);
    f.seek(SeekFrom::Start(from))?;
    let mut tail = Vec::new();
    f.take(MAX_READ as u64).read_to_end(&mut tail)?;
    let text = String::from_utf8_lossy(&tail);
    let mut lines = text.split('\n');
    if from > 0 {
        lines.next(); // the first piece may start mid-line
    }
    Ok(lines.filter_map(gamelink::parse_line).map(|l| l.seq).max().unwrap_or(0))
}

// ---------------------------------------------------------------------------------------------------------------
// Module to mod
// ---------------------------------------------------------------------------------------------------------------

/// A value in Lua literal syntax, written the way the mod's own serialiser writes it (`C.serialize`): sorted keys,
/// integers as `%d`, strings with `\n \r \t \" \\` and `\ddd` for the other control characters. Events carry no
/// floats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lua {
    Int(i64),
    Bool(bool),
    Str(String),
    /// `{[1]=a,[2]=b}`
    List(Vec<Lua>),
    /// `{["key"]=v,...}`
    Map(BTreeMap<String, Lua>),
    /// `{[3]=v,...}`
    IntMap(BTreeMap<i64, Lua>),
}

impl Lua {
    /// An unsigned count as an integer (saturating at `i64::MAX`, which no counter reaches).
    pub fn uint(n: u64) -> Lua {
        Lua::Int(i64::try_from(n).unwrap_or(i64::MAX))
    }

    pub fn str(s: impl Into<String>) -> Lua {
        Lua::Str(s.into())
    }

    pub fn map<const N: usize>(entries: [(&str, Lua); N]) -> Lua {
        Lua::Map(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn write(&self, out: &mut String) {
        match self {
            Lua::Int(n) => out.push_str(&n.to_string()),
            Lua::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Lua::Str(s) => write_string(s, out),
            Lua::List(items) => {
                out.push('{');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&format!("[{}]=", i + 1));
                    item.write(out);
                }
                out.push('}');
            }
            Lua::Map(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('[');
                    write_string(k, out);
                    out.push_str("]=");
                    v.write(out);
                }
                out.push('}');
            }
            Lua::IntMap(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&format!("[{k}]="));
                    v.write(out);
                }
                out.push('}');
            }
        }
    }

    pub fn text(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_ascii_control() => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Counts of the commands the interface added to the engine's list, by where they came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AddCounts {
    /// From one of the construction-tool sites.
    pub tool: u64,
    /// From any other caller.
    pub other: u64,
    /// The caller could not be identified.
    pub unknown: u64,
}

/// One message of `native_events.log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Written once at start: what is active.
    Ready {
        build: String,
        version: String,
        hooks: Vec<String>,
        poolpatch: bool,
        /// Whether the module can read the game time (it cannot yet: a `release` at a later game time is due at once).
        clock: bool,
        /// Kind names the control link understands, with their kind bytes.
        kinds: Vec<(String, u8)>,
        /// How many construction-tool call sites the hold hook knows.
        sites: usize,
    },
    /// A command was captured.
    Held {
        id: u64,
        kind: u8,
        site: u32,
        ms: u64,
        /// Cancelling this command fires the tool's own completion callback first (`FEVERLINK3_STRICT_FIRE_DONE=1` and a callback that
        /// was read out of the executable).
        done: bool,
        /// The Context members and flags of a build command, read from the command itself.
        flags: Option<BuildFlags>,
    },
    /// A held command was handed back to the engine (`reason` says why now).
    Released {
        id: u64,
        reason: &'static str,
        ms: u64,
    },
    /// A held command was destroyed instead of released.
    Dropped {
        id: u64,
        reason: &'static str,
    },
    /// A per-build timing record (`what`: `clock` once at start, `capture` of a held command's text, `input`, `apply` at the stamp
    /// apply of an ordered command): plain `name=value` text in `detail`, wall-clock times in milliseconds since 1970.
    Timing {
        what: &'static str,
        id: u64,
        detail: String,
    },
    /// The simulation applied a command the module had released.
    Applied {
        id: u64,
        ok: bool,
    },
    /// A deferred held command took the result of this game's own stamp apply of the same build and its tool callback was fired
    /// (`fired`); the held command itself was never applied.
    /// `reason` is `stamp_apply` for that; any other value (`fail`, `cancel`, `timeout`, `hold_off`, ...) says the command left without a
    /// stamp apply and its callback was fired with a failed result (`ok` false).
    Completed {
        id: u64,
        ok: bool,
        fired: bool,
        ms: u64,
        reason: &'static str,
    },
    /// `patch_next` was carried out on a script command: its frozenNodes now hold `frozen` (`adds` added nodes were verified).
    Patched {
        frozen: Vec<u32>,
        adds: u32,
        /// A new block was allocated for the proposal's list (else the engine's own capacity was reused).
        allocated: bool,
    },
    /// `patch_next` was not carried out; the command was left untouched.
    PatchRefused {
        reason: String,
    },
    /// A `build_proposal` went by while an order was armed and was not used (diagnostic; the order stays armed).
    PatchSkipped {
        reason: String,
        kind: Option<u8>,
        /// The call site's offset in the game image.
        site: Option<u32>,
    },
    /// What a conversion produced while a `patch_next` order was armed (nodes, segments, frozenNodes lists), once per distinct shape.
    ConvertDump {
        text: String,
    },
    /// `inject_next` was carried out: the command now holds the engine Proposal built from the text of `key`.
    Injected {
        key: u64,
        nodes: u64,
        segments: u64,
        constructions: u64,
    },
    /// `inject_next` was not carried out. `key`: the order (0: none); `held`: the deferred held command of the same build (0: none), which
    /// the module fails; `outcome`: what became of the command (`nothing`: emptied, nothing of the build applied here; `untouched`: it
    /// could not be emptied; `no_command`: no command took the order). See `inject::Outcome`.
    InjectRefused {
        key: u64,
        held: u64,
        outcome: &'static str,
        reason: String,
    },
    /// Cumulative counts of the commands the simulation applied, by kind byte.
    Audit {
        counts: Vec<(u8, u64)>,
        total: u64,
        unreadable: u64,
        adds: AddCounts,
        sites: Vec<(usize, u64)>,
    },
    /// Holding was switched on or off, by the mod or by the module (`reason`).
    Holding {
        on: bool,
        kinds: Vec<String>,
        reason: &'static str,
    },
    Pong {
        n: i64,
    },
    Error {
        place: String,
        detail: String,
    },
}

impl Event {
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Ready { .. } => "ready",
            Event::Held { .. } => "held",
            Event::Released { .. } => "released",
            Event::Dropped { .. } => "dropped",
            Event::Applied { .. } => "applied",
            Event::Completed { .. } => "completed",
            Event::Timing { .. } => "timing",
            Event::Patched { .. } => "patched",
            Event::PatchRefused { .. } => "patch_refused",
            Event::PatchSkipped { .. } => "patch_skipped",
            Event::Injected { .. } => "injected",
            Event::InjectRefused { .. } => "inject_refused",
            Event::ConvertDump { .. } => "convert_dump",
            Event::Audit { .. } => "audit",
            Event::Holding { .. } => "holding",
            Event::Pong { .. } => "pong",
            Event::Error { .. } => "error",
        }
    }

    /// A line for the module's own log, for a person reading it; `None` for events only the mod needs.
    pub fn log_line(&self) -> Option<String> {
        Some(match self {
            Event::Ready { build, hooks, poolpatch, clock, .. } => format!(
                "event ready: build {build}, hooks [{}], poolpatch {}, game clock {}",
                hooks.join(", "),
                if *poolpatch { "applied" } else { "off" },
                if *clock { "yes" } else { "no" }
            ),
            Event::Held { id, kind, site, .. } => {
                format!("event held: command {id}, kind {kind}, from tool site {site:#x}")
            }
            Event::Released { id, reason, .. } => format!("event released: command {id} ({reason})"),
            Event::Dropped { id, reason } => format!("event dropped: command {id} ({reason})"),
            Event::Applied { id, ok } => format!("event applied: command {id}, accepted by the engine: {ok}"),
            Event::Completed { id, ok, fired, reason: "stamp_apply", .. } => format!(
                "event completed: command {id} took the result of this game's stamp apply (accepted: {ok}), tool callback fired: {fired}"
            ),
            Event::Completed { id, fired, reason, .. } => format!(
                "event completed: command {id} FAILED without a stamp apply ({reason}), tool callback fired with a failed result: {fired}"
            ),
            Event::Holding { on, kinds, reason } => {
                format!("event holding {} ({reason}) [{}]", if *on { "on" } else { "off" }, kinds.join(", "))
            }
            Event::Error { place, detail } => format!("event error [{place}]: {detail}"),
            Event::Patched { frozen, adds, allocated } => {
                format!("event patched: frozenNodes {frozen:?} of {adds} added nodes (new block: {allocated})")
            }
            Event::PatchRefused { reason } => format!("event patch_refused: {reason}"),
            Event::ConvertDump { text } => format!("event convert_dump: {text}"),
            Event::Injected { key, nodes, segments, constructions } => format!(
                "event injected: key {key}: {nodes} nodes, {segments} segments, {constructions} construction(s)"
            ),
            Event::InjectRefused { key, held, outcome, reason } => {
                format!("event inject_refused: key {key} held {held} outcome {outcome}: {reason}")
            }
            Event::PatchSkipped { reason, kind, site } => {
                format!("event patch_skipped: {reason} (kind {kind:?}, call site rva {site:x?})")
            }
            Event::Timing { what, id, detail } => format!("timing {what} {id}: {detail}"),
            Event::Audit { .. } | Event::Pong { .. } => return None,
        })
    }

    pub fn payload(&self) -> String {
        let v = match self {
            Event::Ready { build, version, hooks, poolpatch, clock, kinds, sites } => Lua::map([
                ("build", Lua::str(build)),
                ("clock", Lua::Bool(*clock)),
                ("hooks", Lua::List(hooks.iter().map(Lua::str).collect())),
                ("kinds", Lua::Map(kinds.iter().map(|(n, k)| (n.clone(), Lua::Int(i64::from(*k)))).collect())),
                ("poolpatch", Lua::Bool(*poolpatch)),
                ("sites", Lua::uint(*sites as u64)),
                ("version", Lua::str(version)),
            ]),
            Event::Held { id, kind, site, ms, done, flags } => {
                let mut fields = vec![
                    ("id", Lua::uint(*id)),
                    ("kind", Lua::Int(i64::from(*kind))),
                    ("ms", Lua::uint(*ms)),
                    ("site", Lua::Int(i64::from(*site))),
                ];
                if *done {
                    fields.insert(0, ("done", Lua::Bool(true)));
                }
                if let Some(f) = flags {
                    fields.insert(
                        0,
                        (
                            "ctx",
                            Lua::map([
                                ("align", Lua::Bool(f.check_terrain_alignment)),
                                ("buildings", Lua::Bool(f.gather_buildings)),
                                ("cleanup", Lua::Bool(f.cleanup_street_graph)),
                                ("fields", Lua::Bool(f.gather_fields)),
                                ("ie", Lua::Bool(f.ignore_errors)),
                                ("pi", Lua::Bool(f.player_initiated)),
                                ("redo", Lua::Bool(f.extend_proposal_redo_pillars)),
                            ]),
                        ),
                    );
                }
                Lua::Map(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
            }
            Event::Released { id, reason, ms } => {
                Lua::map([("id", Lua::uint(*id)), ("ms", Lua::uint(*ms)), ("reason", Lua::str(*reason))])
            }
            Event::Dropped { id, reason } => Lua::map([("id", Lua::uint(*id)), ("reason", Lua::str(*reason))]),
            Event::Applied { id, ok } => Lua::map([("id", Lua::uint(*id)), ("ok", Lua::Bool(*ok))]),
            Event::Timing { what, id, detail } => {
                Lua::map([("detail", Lua::str(detail)), ("id", Lua::uint(*id)), ("what", Lua::str(*what))])
            }
            Event::Completed { id, ok, fired, ms, reason } => Lua::map([
                ("fired", Lua::Bool(*fired)),
                ("id", Lua::uint(*id)),
                ("ms", Lua::uint(*ms)),
                ("ok", Lua::Bool(*ok)),
                ("reason", Lua::str(*reason)),
            ]),
            Event::Audit { counts, total, unreadable, adds, sites } => Lua::map([
                (
                    "adds",
                    Lua::map([
                        ("other", Lua::uint(adds.other)),
                        ("tool", Lua::uint(adds.tool)),
                        ("unknown", Lua::uint(adds.unknown)),
                    ]),
                ),
                ("counts", Lua::IntMap(counts.iter().map(|(k, n)| (i64::from(*k), Lua::uint(*n))).collect())),
                ("sites", Lua::IntMap(sites.iter().map(|(i, n)| (*i as i64, Lua::uint(*n))).collect())),
                ("total", Lua::uint(*total)),
                ("unreadable", Lua::uint(*unreadable)),
            ]),
            Event::Holding { on, kinds, reason } => Lua::map([
                ("kinds", Lua::List(kinds.iter().map(Lua::str).collect())),
                ("on", Lua::Bool(*on)),
                ("reason", Lua::str(*reason)),
            ]),
            Event::Pong { n } => Lua::map([("n", Lua::Int(*n))]),
            Event::Patched { frozen, adds, allocated } => Lua::map([
                ("adds", Lua::uint(u64::from(*adds))),
                ("allocated", Lua::Bool(*allocated)),
                ("frozen", Lua::List(frozen.iter().map(|n| Lua::uint(u64::from(*n))).collect())),
            ]),
            Event::PatchRefused { reason } => Lua::map([("reason", Lua::str(reason))]),
            Event::ConvertDump { text } => Lua::map([("text", Lua::str(text))]),
            Event::Injected { key, nodes, segments, constructions } => Lua::map([
                ("constructions", Lua::uint(*constructions)),
                ("key", Lua::uint(*key)),
                ("nodes", Lua::uint(*nodes)),
                ("segments", Lua::uint(*segments)),
            ]),
            Event::InjectRefused { key, held, outcome, reason } => Lua::map([
                ("held", Lua::uint(*held)),
                ("key", Lua::uint(*key)),
                ("outcome", Lua::str(*outcome)),
                ("reason", Lua::str(reason)),
            ]),
            Event::PatchSkipped { reason, kind, site } => Lua::map([
                ("kind", Lua::Int(kind.map_or(-1, i64::from))),
                ("reason", Lua::str(reason)),
                ("site", Lua::Int(site.map_or(-1, i64::from))),
            ]),
            Event::Error { place, detail } => Lua::map([("detail", Lua::str(detail)), ("where", Lua::str(place))]),
        };
        v.text()
    }
}

/// Appends events to `native_events.log`, continuing the numbering of whatever the file already holds.
#[derive(Debug)]
pub struct EventWriter {
    path: PathBuf,
    next_seq: u64,
    size: u64,
}

impl EventWriter {
    /// Opens the writer in a session folder. Nothing is created until the first [`append`](Self::append).
    pub fn open(dir: &Path) -> io::Result<EventWriter> {
        let path = dir.join(EVENTS_FILE);
        let next_seq = tail_highest_seq(&path)? + 1;
        let size = match fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        Ok(EventWriter { path, next_seq, size })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The `seq` the next event gets.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// The text of one event line, including the newline.
    pub fn line(seq: u64, event: &Event) -> String {
        gamelink::format_line(&Line {
            seq,
            kind: event.kind().to_string(),
            from: EVENTS_FROM.to_string(),
            payload: event.payload(),
        })
    }

    /// Writes the events with one `write`, so a reader never sees half a batch's lines interleaved with another's.
    /// The numbers are used up even when the write fails: a gap is harmless, a repeated `seq` would be dropped.
    pub fn append(&mut self, events: &[Event]) -> io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut text = String::new();
        for e in events {
            text.push_str(&EventWriter::line(self.next_seq, e));
            self.next_seq += 1;
        }
        if self.size + text.len() as u64 > MAX_EVENTS_BYTES {
            return Err(io::Error::new(io::ErrorKind::StorageFull, "native_events.log reached its size limit"));
        }
        OpenOptions::new().create(true).append(true).open(&self.path)?.write_all(text.as_bytes())?;
        self.size += text.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn line(seq: u64, kind: &str, payload: &str) -> Line {
        Line { seq, kind: kind.into(), from: "1".into(), payload: payload.into() }
    }

    fn parse(kind: &str, payload: &str) -> Result<CtlMessage, CtlError> {
        parse_ctl_line(&line(1, kind, payload))
    }

    // ---- control messages

    #[test]
    fn patch_next_parses_and_rejects_bad_payloads() {
        assert_eq!(
            parse("patch_next", r#"{["adds"]=3,["frozen"]={[1]=1}}"#),
            Ok(CtlMessage::PatchNext { frozen: vec![1], adds: 3, at: vec![], ttl: 0 })
        );
        assert_eq!(
            parse("patch_next", r#"{["adds"]=2,["frozen"]={[1]=0,[2]=1},["ttl"]=500}"#),
            Ok(CtlMessage::PatchNext { frozen: vec![0, 1], adds: 2, at: vec![], ttl: 500 })
        );
        assert_eq!(
            parse("patch_next", r#"{["adds"]=2,["at"]={[1]={[1]=1.5,[2]=-2,[3]=0.25}},["frozen"]={[1]=1}}"#),
            Ok(CtlMessage::PatchNext { frozen: vec![1], adds: 2, at: vec![[1.5, -2.0, 0.25]], ttl: 0 })
        );
        assert_eq!(
            parse("patch_next", r#"{["at"]={[1]={[1]=1,[2]=2,[3]=3},[2]={[1]=4,[2]=5,[3]=6}},["ttl"]=7}"#),
            Ok(CtlMessage::PatchNext { frozen: vec![], adds: 0, at: vec![[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]], ttl: 7 })
        );
        for bad in [
            r#"{["at"]={}}"#,
            r#"{["at"]={[1]={[1]=1,[2]=2}}}"#,
            r#"{["at"]={[1]=5}}"#,
            r#"{["adds"]=3}"#,
            r#"{["frozen"]={[1]=1}}"#,
            r#"{["adds"]=3,["frozen"]={}}"#,
            r#"{["adds"]=3,["frozen"]={[1]=-1}}"#,
        ] {
            assert!(matches!(parse("patch_next", bad), Err(CtlError::BadPayload(_))), "{bad}");
        }
        assert_eq!(
            parse("inject_next", r#"{["key"]=12,["ttl"]=500}"#),
            Ok(CtlMessage::InjectNext { key: 12, ttl: 500, held: 0, tm: String::new(), refuse: String::new() })
        );
        assert_eq!(
            parse("inject_next", r#"{["key"]=3}"#),
            Ok(CtlMessage::InjectNext { key: 3, ttl: 0, held: 0, tm: String::new(), refuse: String::new() })
        );
        assert_eq!(
            parse("inject_next", r#"{["key"]=3,["held"]=9}"#),
            Ok(CtlMessage::InjectNext { key: 3, ttl: 0, held: 9, tm: String::new(), refuse: String::new() })
        );
        assert_eq!(
            parse("inject_next", r#"{["held"]=9,["key"]=3,["refuse"]="debug switch"}"#),
            Ok(CtlMessage::InjectNext { key: 3, ttl: 0, held: 9, tm: String::new(), refuse: "debug switch".into() })
        );
        assert!(
            matches!(parse("inject_next", r#"{["key"]=3,["refuse"]=1}"#), Err(CtlError::BadPayload(_))),
            "refuse is text"
        );
        assert_eq!(parse("defer", r#"{["id"]=4}"#), Ok(CtlMessage::Defer { id: 4 }));
        assert_eq!(parse("fail", r#"{["id"]=4}"#), Ok(CtlMessage::Fail { id: 4 }));
        assert_eq!(
            parse("complete_next", r#"{["held"]=7,["tm"]="key=1:2 click=5@g9"}"#),
            Ok(CtlMessage::CompleteNext { held: 7, ttl: 0, tm: "key=1:2 click=5@g9".into() })
        );
        assert!(matches!(parse("complete_next", "{}"), Err(CtlError::BadPayload(_))), "held is required");
        assert!(matches!(parse("inject_next", r#"{["key"]=3,["tm"]=5}"#), Err(CtlError::BadPayload(_))), "tm is text");
        assert!(matches!(parse("inject_next", "{}"), Err(CtlError::BadPayload(_))));
        let i = Event::InjectRefused { key: 4, held: 2, outcome: "nothing", reason: "r".into() };
        assert_eq!(
            (i.kind(), i.payload().as_str()),
            ("inject_refused", r#"{["held"]=2,["key"]=4,["outcome"]="nothing",["reason"]="r"}"#)
        );
        assert_eq!(Event::Injected { key: 1, nodes: 2, segments: 3, constructions: 0 }.kind(), "injected");
        let d = Event::ConvertDump { text: "t".into() };
        assert_eq!((d.kind(), d.payload().as_str()), ("convert_dump", r#"{["text"]="t"}"#));
        let e = Event::PatchRefused { reason: "x".into() };
        assert_eq!((e.kind(), e.payload().as_str()), ("patch_refused", r#"{["reason"]="x"}"#));
        assert_eq!(Event::Patched { frozen: vec![1], adds: 3, allocated: true }.kind(), "patched");
    }

    #[test]
    fn every_control_kind_parses_from_the_mods_serialiser_output() {
        // these payloads are what C.serialize writes: sorted keys, [k]=v, integers as %d
        assert_eq!(
            parse("hold_on", r#"{["kinds"]={[1]="build_proposal"}}"#),
            Ok(CtlMessage::HoldOn { kinds: vec!["build_proposal".into()] })
        );
        assert_eq!(parse("hold_off", "{}"), Ok(CtlMessage::HoldOff));
        assert_eq!(parse("release", r#"{["at"]=12000,["id"]=3}"#), Ok(CtlMessage::Release { id: 3, at: 12000 }));
        assert_eq!(parse("cancel", r#"{["id"]=7}"#), Ok(CtlMessage::Cancel { id: 7 }));
        assert_eq!(
            parse("terrain_apply", r#"{["id"]=2,["stamp"]=400}"#),
            Ok(CtlMessage::TerrainApply { id: 2, stamp: 400 })
        );
        assert_eq!(parse("ping", r#"{["n"]=-4}"#), Ok(CtlMessage::Ping { n: -4 }));
    }

    #[test]
    fn lines_written_by_the_mods_own_writer_parse() {
        // The bytes feverlink3_common.lua produces: C.newWriter(path, 1), then C.writerSend for each kind with
        // {kinds={"build_proposal"}}, {n=3}, {id=3, at=12400}, {id=7}, {} and {id=2, stamp=400}.
        let file = "1\thold_on\t1\t{[\"kinds\"]={[1]=\"build_proposal\"}}\n2\tping\t1\t{[\"n\"]=3}\n\
                    3\trelease\t1\t{[\"at\"]=12400,[\"id\"]=3}\n4\tcancel\t1\t{[\"id\"]=7}\n5\thold_off\t1\t{}\n\
                    6\tterrain_apply\t1\t{[\"id\"]=2,[\"stamp\"]=400}\n";
        let dir = TempDir::new("mod-writer");
        let path = dir.path().join(CTL_FILE);
        fs::write(&path, file).unwrap();
        let messages: Vec<_> = CtlReader::new(path).poll().lines.iter().map(|l| parse_ctl_line(l).unwrap()).collect();
        assert_eq!(
            messages,
            [
                CtlMessage::HoldOn { kinds: vec!["build_proposal".into()] },
                CtlMessage::Ping { n: 3 },
                CtlMessage::Release { id: 3, at: 12400 },
                CtlMessage::Cancel { id: 7 },
                CtlMessage::HoldOff,
                CtlMessage::TerrainApply { id: 2, stamp: 400 },
            ]
        );
    }

    #[test]
    fn hand_written_lines_are_accepted_too() {
        assert_eq!(
            parse("hold_on", r#"{kinds={"build_proposal","other"}}"#).unwrap(),
            CtlMessage::HoldOn { kinds: vec!["build_proposal".into(), "other".into()] }
        );
        assert_eq!(parse("hold_off", ""), Ok(CtlMessage::HoldOff));
        assert_eq!(parse("release", "{id=1}"), Ok(CtlMessage::Release { id: 1, at: 0 }), "at defaults to now");
        assert_eq!(parse("ping", "{}"), Ok(CtlMessage::Ping { n: 0 }));
        assert_eq!(parse("release", r#"{["id"]=1.0,["at"]=2e3}"#), Ok(CtlMessage::Release { id: 1, at: 2000 }));
    }

    #[test]
    fn bad_payloads_are_errors_not_panics() {
        for (kind, payload) in [
            ("hold_on", "{}"),
            ("hold_on", r#"{["kinds"]={}}"#),
            ("hold_on", r#"{["kinds"]="build_proposal"}"#),
            ("hold_on", r#"{["kinds"]={[1]=5}}"#),
            ("hold_on", r#"{["kinds"]={[1]="Build Proposal"}}"#),
            ("hold_on", r#"{["kinds"]={[1]=""}}"#),
            ("release", "{}"),
            ("release", r#"{["id"]=-1}"#),
            ("release", r#"{["id"]=1.5}"#),
            ("release", r#"{["id"]="1"}"#),
            ("release", r#"{["id"]=1,["at"]=-5}"#),
            ("release", r#"{["id"]=1e300}"#),
            ("cancel", r#"{["id"]=true}"#),
            ("terrain_apply", "{}"),
            ("ping", r#"{["n"]="x"}"#),
        ] {
            assert!(matches!(parse(kind, payload), Err(CtlError::BadPayload(_))), "{kind} {payload}");
        }
        for payload in ["os.exit()", "{", "{[1]=}", "{} extra", "(0/0)"] {
            assert_eq!(parse("hold_off", payload), Err(CtlError::Malformed), "{payload}");
        }
        assert_eq!(parse("explode", "{}"), Err(CtlError::UnknownKind("explode".into())));
        let long = "x".repeat(500);
        assert!(matches!(parse(&long, "{}"), Err(CtlError::UnknownKind(k)) if k.len() == 40));
    }

    #[test]
    fn too_many_kinds_are_refused() {
        let names: Vec<String> = (1..=17).map(|i| format!("[{i}]=\"k{i}\"")).collect();
        let payload = format!("{{[\"kinds\"]={{{}}}}}", names.join(","));
        assert!(matches!(parse("hold_on", &payload), Err(CtlError::BadPayload(_))));
        let ok = format!("{{[\"kinds\"]={{{}}}}}", names[..16].join(","));
        assert!(matches!(parse("hold_on", &ok), Ok(CtlMessage::HoldOn { kinds }) if kinds.len() == 16));
    }

    #[test]
    fn kind_names() {
        assert!(valid_kind_name("build_proposal"));
        assert!(valid_kind_name("k2"));
        assert!(!valid_kind_name(""));
        assert!(!valid_kind_name("Build"));
        assert!(!valid_kind_name("a b"));
        assert!(!valid_kind_name(&"a".repeat(33)));
    }

    // ---- reading the file

    fn append(path: &Path, text: &str) {
        OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
    }

    fn rec(seq: u64, kind: &str, payload: &str) -> String {
        format!("{seq}\t{kind}\t1\t{payload}\n")
    }

    #[test]
    fn reader_returns_new_complete_lines_once() {
        let dir = TempDir::new("reader-basic");
        let path = dir.path().join(CTL_FILE);
        let mut r = CtlReader::new(path.clone());
        assert_eq!(r.poll(), Poll::default(), "a missing file is not an event");
        assert!(!r.has_news());
        append(&path, &rec(1, "ping", "{}"));
        assert!(r.has_news());
        let p = r.poll();
        assert_eq!(p.lines.len(), 1);
        assert_eq!(p.lines[0].kind, "ping");
        assert!(!r.has_news());
        assert_eq!(r.poll(), Poll::default(), "nothing is dispatched twice");
        append(&path, &(rec(2, "hold_off", "{}") + &rec(3, "ping", r#"{["n"]=1}"#)));
        let kinds: Vec<_> = r.poll().lines.into_iter().map(|l| l.kind).collect();
        assert_eq!(kinds, ["hold_off", "ping"]);
        assert_eq!(r.last_seq(), 3);
    }

    #[test]
    fn a_partial_last_line_waits_for_its_newline() {
        let dir = TempDir::new("reader-partial");
        let path = dir.path().join(CTL_FILE);
        let mut r = CtlReader::new(path.clone());
        append(&path, "1\tping\t1\t{[\"n\"]");
        assert!(r.poll().lines.is_empty());
        append(&path, "=5}\n");
        let p = r.poll();
        assert_eq!(p.lines.len(), 1);
        assert_eq!(p.lines[0].payload, r#"{["n"]=5}"#);
    }

    #[test]
    fn crlf_lines_and_blank_lines_are_fine() {
        let dir = TempDir::new("reader-crlf");
        let path = dir.path().join(CTL_FILE);
        let mut r = CtlReader::new(path.clone());
        append(&path, "1\tping\t1\t{}\r\n\r\n2\tping\t1\t{}\r\n");
        let p = r.poll();
        assert_eq!(p.lines.len(), 2);
        assert_eq!(p.discarded, 0);
        assert_eq!(p.lines[0].payload, "{}");
    }

    #[test]
    fn a_patch_next_from_a_second_writer_with_the_next_sequence_is_accepted() {
        // the interface half writes seq 1..3 (hold_on, release, ping), the simulation half continues at 4 right before its command
        let dir = TempDir::new("link-two-writers");
        let path = dir.path().join(CTL_FILE);
        let mut r = CtlReader::new(path.clone());
        append(
            &path,
            &(rec(1, "hold_on", r#"{["kinds"]={[1]="build_proposal"}}"#) + &rec(2, "release", r#"{["id"]=1}"#)),
        );
        assert_eq!(r.poll().lines.len(), 2);
        append(&path, &rec(3, "patch_next", r#"{["adds"]=3,["frozen"]={[1]=1}}"#));
        let p = r.poll();
        assert_eq!(p.lines.len(), 1, "{p:?}");
        assert_eq!(
            parse_ctl_line(&p.lines[0]),
            Ok(CtlMessage::PatchNext { frozen: vec![1], adds: 3, at: vec![], ttl: 0 })
        );
        // a line whose sequence number is not above the last one is dropped, whoever wrote it
        append(&path, &rec(3, "patch_next", r#"{["adds"]=3,["frozen"]={[1]=1}}"#));
        let p = r.poll();
        assert_eq!((p.lines.len(), p.discarded), (0, 1));
    }

    #[test]
    fn old_and_repeated_sequence_numbers_are_dropped() {
        let dir = TempDir::new("reader-seq");
        let path = dir.path().join(CTL_FILE);
        let mut r = CtlReader::new(path.clone());
        append(&path, &(rec(5, "ping", "{}") + &rec(5, "ping", "{}") + &rec(4, "ping", "{}") + &rec(6, "ping", "{}")));
        let p = r.poll();
        assert_eq!(p.lines.iter().map(|l| l.seq).collect::<Vec<_>>(), [5, 6]);
        assert_eq!(p.discarded, 2);
    }

    #[test]
    fn truncation_restarts_the_read_without_replaying_history() {
        let dir = TempDir::new("reader-truncate");
        let path = dir.path().join(CTL_FILE);
        let mut r = CtlReader::new(path.clone());
        append(&path, &(rec(1, "ping", "{}") + &rec(2, "ping", "{}") + &rec(3, "ping", "{}")));
        assert_eq!(r.poll().lines.len(), 3);
        fs::write(&path, rec(1, "ping", "{}") + &rec(2, "ping", "{}")).unwrap(); // shorter: rewritten history
        let p = r.poll();
        assert!(p.lines.is_empty());
        assert_eq!(p.discarded, 2);
        append(&path, &rec(4, "hold_off", "{}"));
        assert_eq!(r.poll().lines.len(), 1, "new numbers after a rewrite are dispatched");
    }

    #[test]
    fn a_deleted_file_is_reported_once_and_a_new_one_starts_afresh() {
        let dir = TempDir::new("reader-gone");
        let path = dir.path().join(CTL_FILE);
        let mut r = CtlReader::new(path.clone());
        append(&path, &rec(7, "ping", "{}"));
        assert_eq!(r.poll().lines.len(), 1);
        fs::remove_file(&path).unwrap();
        assert!(r.has_news(), "the disappearance is news");
        let p = r.poll();
        assert!(p.gone);
        assert!(!r.poll().gone, "reported once");
        append(&path, &rec(1, "ping", "{}"));
        assert_eq!(r.poll().lines.len(), 1, "a re-created file may restart its numbering");
    }

    #[test]
    fn a_file_that_never_existed_is_not_gone() {
        let dir = TempDir::new("reader-never");
        let mut r = CtlReader::new(dir.path().join(CTL_FILE));
        assert!(!r.poll().gone);
    }

    #[test]
    fn oversize_lines_are_thrown_away_without_losing_the_next_ones() {
        let dir = TempDir::new("reader-oversize");
        let path = dir.path().join(CTL_FILE);
        let mut r = CtlReader::new(path.clone());
        let huge = "x".repeat(MAX_LINE * 3);
        append(&path, &(rec(1, "ping", "{}") + &format!("2\tping\t1\t{huge}\n") + &rec(3, "ping", "{}")));
        let p = r.poll();
        assert_eq!(p.lines.iter().map(|l| l.seq).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(p.discarded, 1);
        assert!(r.pending.len() < MAX_LINE, "the buffer stays bounded");
    }

    #[test]
    fn an_oversize_line_split_across_polls_is_still_one_discard() {
        let dir = TempDir::new("reader-oversize-split");
        let path = dir.path().join(CTL_FILE);
        let mut r = CtlReader::new(path.clone());
        append(&path, &format!("1\tping\t1\t{}", "y".repeat(MAX_LINE + 10)));
        assert_eq!(r.poll().discarded, 1);
        append(&path, &format!("{}\n", "z".repeat(1000)));
        let p = r.poll();
        assert_eq!(p.discarded, 0, "the tail of the same line is not counted again");
        assert!(p.lines.is_empty());
        append(&path, &rec(2, "ping", "{}"));
        assert_eq!(r.poll().lines.len(), 1);
    }

    #[test]
    fn a_read_is_bounded_and_resumes() {
        let dir = TempDir::new("reader-bounded");
        let path = dir.path().join(CTL_FILE);
        let mut r = CtlReader::new(path.clone());
        let one = rec(1, "ping", "{}");
        let n = MAX_READ / one.len() * 2 + 10;
        let mut text = String::new();
        for i in 1..=n as u64 {
            text.push_str(&rec(i, "ping", "{}"));
        }
        fs::write(&path, &text).unwrap();
        let first = r.poll().lines.len();
        assert!(first > 0 && first < n, "one poll takes at most MAX_READ bytes ({first} of {n})");
        let mut total = first;
        while r.has_news() {
            total += r.poll().lines.len();
        }
        assert_eq!(total, n);
    }

    #[test]
    fn garbage_lines_are_counted_not_dispatched() {
        let dir = TempDir::new("reader-garbage");
        let path = dir.path().join(CTL_FILE);
        let mut r = CtlReader::new(path.clone());
        append(&path, "not a record\n\u{1}\u{2}\n1\tping\t1\t{}\nnoseq\tping\t1\t{}\n");
        let p = r.poll();
        assert_eq!(p.lines.len(), 1);
        assert_eq!(p.discarded, 3);
        fs::write(&path, [0xff, 0xfe, b'\n']).unwrap(); // invalid UTF-8 never panics
        r.poll();
    }

    #[test]
    fn starting_at_the_end_ignores_history_but_keeps_its_numbering() {
        let dir = TempDir::new("reader-history");
        let path = dir.path().join(CTL_FILE);
        append(&path, &(rec(1, "hold_on", r#"{["kinds"]={[1]="build_proposal"}}"#) + &rec(2, "ping", "{}")));
        let mut r = CtlReader::starting_at_end(path.clone()).unwrap();
        assert_eq!(r.last_seq(), 2);
        assert!(r.poll().lines.is_empty(), "an earlier run's lines are not acted on");
        append(&path, &rec(3, "hold_off", "{}"));
        assert_eq!(r.poll().lines.len(), 1);
        let none = CtlReader::starting_at_end(dir.path().join("missing.log")).unwrap();
        assert_eq!(none.last_seq(), 0);
    }

    // ---- events

    #[test]
    fn lua_literals_match_the_mods_serialiser() {
        let v = Lua::map([
            ("s", Lua::str("a\tb\nc\"d\\e\u{1}f é")),
            ("n", Lua::Int(-3)),
            ("b", Lua::Bool(false)),
            ("l", Lua::List(vec![Lua::Int(1), Lua::str("x")])),
            ("i", Lua::IntMap([(52, Lua::Int(9)), (3, Lua::Int(1))].into_iter().collect())),
            ("e", Lua::Map(BTreeMap::new())),
        ]);
        assert_eq!(
            v.text(),
            "{[\"b\"]=false,[\"e\"]={},[\"i\"]={[3]=1,[52]=9},[\"l\"]={[1]=1,[2]=\"x\"},[\"n\"]=-3,\
             [\"s\"]=\"a\\tb\\nc\\\"d\\\\e\\001f é\"}"
        );
        assert!(!v.text().contains(['\n', '\t', '\r']), "an event payload stays on one line");
    }

    #[test]
    fn events_read_back_through_the_launchers_parser() {
        // the launcher's luatext is the mod's `C.parse` in Rust: whatever it reads, the mod reads
        let ev = Event::Ready {
            build: "40420".into(),
            version: "0.1.0".into(),
            hooks: vec!["apply_command".into(), "command_list_add".into()],
            poolpatch: false,
            clock: false,
            kinds: vec![("build_proposal".into(), 52)],
            sites: 12,
        };
        let json: Value = serde_json::from_slice(&luatext::lua_to_json(&ev.payload()).unwrap()).unwrap();
        assert_eq!(json["build"], "40420");
        assert_eq!(json["hooks"], serde_json::json!(["apply_command", "command_list_add"]));
        assert_eq!(json["kinds"]["build_proposal"], 52);
        assert_eq!(json["clock"], false);
        assert_eq!(json["sites"], 12);
        assert_eq!(ev.kind(), "ready");
    }

    #[test]
    fn a_held_build_carries_its_flags_to_the_mod() {
        let flags = BuildFlags {
            gather_buildings: true,
            gather_fields: true,
            check_terrain_alignment: false,
            cleanup_street_graph: true,
            extend_proposal_redo_pillars: true,
            ignore_errors: false,
            player_initiated: true,
        };
        let ev = Event::Held { id: 4, kind: 52, site: 1, ms: 2, done: true, flags: Some(flags) };
        let json: Value = serde_json::from_slice(&luatext::lua_to_json(&ev.payload()).unwrap()).unwrap();
        assert_eq!(
            json["ctx"],
            serde_json::json!({"align": false, "buildings": true, "cleanup": true, "fields": true, "ie": false, "pi": true, "redo": true})
        );
        let none = Event::Held { id: 4, kind: 52, site: 1, ms: 2, done: false, flags: None };
        let json: Value = serde_json::from_slice(&luatext::lua_to_json(&none.payload()).unwrap()).unwrap();
        assert!(json.get("ctx").is_none(), "a command without readable flags says nothing about them");
    }

    #[test]
    fn every_event_has_a_kind_and_a_payload_the_mod_can_parse() {
        let events = vec![
            Event::Held { id: 1, kind: 52, site: 0x4d396f, ms: 1500, done: false, flags: None },
            Event::Released { id: 1, reason: "release", ms: 2500 },
            Event::Dropped { id: 2, reason: "cancel" },
            Event::Applied { id: 1, ok: true },
            Event::Audit {
                counts: vec![(17, 4), (52, 2)],
                total: 6,
                unreadable: 0,
                adds: AddCounts { tool: 2, other: 5, unknown: 0 },
                sites: vec![(0, 2)],
            },
            Event::Holding { on: true, kinds: vec!["build_proposal".into()], reason: "hold_on" },
            Event::Pong { n: 9 },
            Event::Error { place: "release".into(), detail: "unknown id 9 \"x\"\n".into() },
        ];
        for ev in events {
            let payload = ev.payload();
            assert!(luatext::lua_to_json(&payload).is_some(), "{} payload {payload}", ev.kind());
            assert!(!payload.contains('\n'));
            let text = EventWriter::line(4, &ev);
            let back = gamelink::parse_line(&text).expect("a link record");
            assert_eq!((back.seq, back.kind.as_str(), back.from.as_str()), (4, ev.kind(), "native"));
            assert_eq!(back.payload, payload);
        }
        let audit: Value = serde_json::from_slice(
            &luatext::lua_to_json(
                &Event::Audit {
                    counts: vec![(17, 4), (52, 2)],
                    total: 6,
                    unreadable: 1,
                    adds: AddCounts { tool: 2, other: 5, unknown: 3 },
                    sites: vec![(0, 2)],
                }
                .payload(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(audit["counts"]["52"], 2);
        assert_eq!(audit["adds"]["unknown"], 3);
        assert_eq!(audit["unreadable"], 1);
    }

    #[test]
    fn events_have_a_line_for_people_and_the_docs_quote_them() {
        // docs/NATIVE_LINK.md section 8 quotes these lines; the ones only the mod needs have none
        let ready = Event::Ready {
            build: "40420".into(),
            version: "0.1.0".into(),
            hooks: vec!["apply_command".into(), "command_list_add".into()],
            poolpatch: false,
            clock: false,
            kinds: vec![],
            sites: 12,
        };
        assert_eq!(
            ready.log_line().unwrap(),
            "event ready: build 40420, hooks [apply_command, command_list_add], poolpatch off, game clock no"
        );
        let line = |e: Event| e.log_line().unwrap();
        assert_eq!(
            line(Event::Held { id: 1, kind: 52, site: 0x4d396f, ms: 5, done: false, flags: None }),
            "event held: command 1, kind 52, from tool site 0x4d396f"
        );
        assert_eq!(line(Event::Released { id: 1, reason: "release", ms: 5 }), "event released: command 1 (release)");
        assert_eq!(line(Event::Dropped { id: 1, reason: "cancel" }), "event dropped: command 1 (cancel)");
        assert_eq!(line(Event::Applied { id: 1, ok: true }), "event applied: command 1, accepted by the engine: true");
        assert_eq!(
            line(Event::Holding { on: true, kinds: vec!["build_proposal".into()], reason: "hold_on" }),
            "event holding on (hold_on) [build_proposal]"
        );
        assert_eq!(
            line(Event::Holding { on: false, kinds: vec![], reason: "silence" }),
            "event holding off (silence) []"
        );
        assert_eq!(
            line(Event::Error { place: "hold".into(), detail: "silence: holding is off".into() }),
            "event error [hold]: silence: holding is off"
        );
        assert_eq!(Event::Pong { n: 1 }.log_line(), None);
        assert_eq!(
            Event::Audit { counts: vec![], total: 0, unreadable: 0, adds: AddCounts::default(), sites: vec![] }
                .log_line(),
            None,
            "the audit has its own line (counters::audit_log_line)"
        );
    }

    #[test]
    fn golden_event_lines() {
        assert_eq!(
            EventWriter::line(1, &Event::Held { id: 3, kind: 52, site: 5_060_975, ms: 120, done: false, flags: None }),
            "1\theld\tnative\t{[\"id\"]=3,[\"kind\"]=52,[\"ms\"]=120,[\"site\"]=5060975}\n"
        );
        assert_eq!(
            EventWriter::line(2, &Event::Released { id: 3, reason: "silence", ms: 99 }),
            "2\treleased\tnative\t{[\"id\"]=3,[\"ms\"]=99,[\"reason\"]=\"silence\"}\n"
        );
        assert_eq!(EventWriter::line(9, &Event::Pong { n: 4 }), "9\tpong\tnative\t{[\"n\"]=4}\n");
    }

    /// The lines the module writes, as the Lua test `tests/lua/test_native_link.py` also holds them: each side
    /// is pinned to the same text, so neither can drift from the other.
    #[test]
    fn golden_lines_shared_with_the_lua_test() {
        let ready = Event::Ready {
            build: "40420".into(),
            version: "0.1.0".into(),
            hooks: vec!["apply_command".into(), "command_list_add".into()],
            poolpatch: true,
            clock: false,
            kinds: vec![("build_proposal".into(), 52)],
            sites: 12,
        };
        assert_eq!(
            EventWriter::line(1, &ready),
            "1\tready\tnative\t{[\"build\"]=\"40420\",[\"clock\"]=false,[\"hooks\"]={[1]=\"apply_command\",[2]=\"command_list_add\"},\
             [\"kinds\"]={[\"build_proposal\"]=52},[\"poolpatch\"]=true,[\"sites\"]=12,[\"version\"]=\"0.1.0\"}\n"
        );
        let holding = Event::Holding { on: true, kinds: vec!["build_proposal".into()], reason: "hold_on" };
        assert_eq!(
            EventWriter::line(2, &holding),
            "2\tholding\tnative\t{[\"kinds\"]={[1]=\"build_proposal\"},[\"on\"]=true,[\"reason\"]=\"hold_on\"}\n"
        );
        let audit = Event::Audit {
            counts: vec![(17, 4), (52, 2)],
            total: 6,
            unreadable: 0,
            adds: AddCounts { tool: 2, other: 5, unknown: 0 },
            sites: vec![(0, 2)],
        };
        assert_eq!(
            EventWriter::line(3, &audit),
            "3\taudit\tnative\t{[\"adds\"]={[\"other\"]=5,[\"tool\"]=2,[\"unknown\"]=0},[\"counts\"]={[17]=4,[52]=2},\
             [\"sites\"]={[0]=2},[\"total\"]=6,[\"unreadable\"]=0}\n"
        );
        let error = Event::Error { place: "release".into(), detail: "no held command 9".into() };
        assert_eq!(
            EventWriter::line(4, &error),
            "4\terror\tnative\t{[\"detail\"]=\"no held command 9\",[\"where\"]=\"release\"}\n"
        );
    }

    #[test]
    fn writer_numbers_events_and_continues_after_a_restart() {
        let dir = TempDir::new("writer");
        let mut w = EventWriter::open(dir.path()).unwrap();
        assert_eq!(w.next_seq(), 1);
        assert!(!w.path().exists(), "nothing is created before the first event");
        w.append(&[]).unwrap();
        assert!(!w.path().exists());
        w.append(&[Event::Pong { n: 1 }, Event::Pong { n: 2 }]).unwrap();
        w.append(&[Event::Pong { n: 3 }]).unwrap();
        let text = fs::read_to_string(w.path()).unwrap();
        let seqs: Vec<u64> = text.lines().filter_map(gamelink::parse_line).map(|l| l.seq).collect();
        assert_eq!(seqs, [1, 2, 3]);
        let again = EventWriter::open(dir.path()).unwrap();
        assert_eq!(again.next_seq(), 4, "a new writer continues the numbering");
    }

    #[test]
    fn a_mod_side_reader_sees_what_the_writer_wrote() {
        // the same CtlReader logic the launcher's Tail has, run over the events file
        let dir = TempDir::new("writer-reader");
        let mut w = EventWriter::open(dir.path()).unwrap();
        w.append(&[Event::Pong { n: 1 }]).unwrap();
        let mut r = CtlReader::new(dir.path().join(EVENTS_FILE));
        assert_eq!(r.poll().lines.len(), 1);
        w.append(&[Event::Pong { n: 2 }]).unwrap();
        let p = r.poll();
        assert_eq!((p.lines.len(), p.lines[0].seq), (1, 2));
    }

    #[test]
    fn the_events_file_has_a_size_limit() {
        let dir = TempDir::new("writer-limit");
        let mut w = EventWriter::open(dir.path()).unwrap();
        w.size = MAX_EVENTS_BYTES - 10;
        let err = w.append(&[Event::Pong { n: 1 }]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
        assert!(!w.path().exists());
    }
}
