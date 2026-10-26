//! The control link between the Lua mod and the native module (`docs/NATIVE_LINK.md` section 3).
//!
//! Append-only text files in the session folder, in exactly the shape of the launcher link (`docs/ARCHITECTURE.md`
//! section 2): one record per line, `kind<TAB>from<TAB>payload`, the payload one line of Lua literal syntax. A line
//! carries no number: the order of the lines in a file is the order, and a reader reads by position (the generation
//! and the byte offset it consumed), taking complete lines only. The mod reads and writes them with the helpers it
//! already has (`C.newWriter`, `C.newReader`, `C.parse`).
//!
//! - The control files (the mod writes, the module reads: [`CtlReader`], [`CtlMessage`]) are ONE FILE PER WRITER. The
//!   mod's interface half and its simulation half are Lua states of their own, and the game runs the simulation half
//!   in more than one Lua state (real runs b3real, b3real2, 2026-10-15: two writers of one file). Two Lua states on
//!   two threads cannot share a file: the C runtime's append is a seek to the end and a write, two steps, so two
//!   appends at the same moment overwrite or split each other's lines (two lupa states appending 200-byte lines lost
//!   7 % of them; 5000-byte lines were torn as well). So every writer names a file of its own,
//!   `<family>.w<id>.log`, with an id that is unique without coordination (the address of the writer's Lua table, the
//!   clocks, a count: the game's Lua has no rename and no exclusive open, real run 2026-10-18 02:33). The families are
//!   `native_ctl.gui` and `native_ctl.sim` ([`CTL_FAMILIES`]). The reader finds the files by listing the folder (when
//!   on every poll, and in `has_news`), reads each by its own position, and hands out, per
//!   poll, the interface writers' new lines and then the simulation writers'. Each writer's lines keep their order;
//!   lines of two writers have no order between them beyond that (each order is complete in itself).
//! - `native_events.log`: the module writes, the mod reads ([`EventWriter`], [`Event`]). One writer.
//!
//! Every file is a generation log (`feverlink3_launcher::gamelink`, "Generations"): it never grows without bound and
//! is never truncated; the readers follow its `rotate` lines.
//!
//! Everything here is pure or plain file access. The reader is bounded (a line longer than [`MAX_LINE`] is thrown
//! away, at most [`MAX_READ`] bytes are taken per poll and file) because the content comes from outside the process.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use feverlink3_launcher::gamelink::{self, Line, Pos};
use feverlink3_launcher::luatext;
use serde_json::Value;

use crate::engine::BuildFlags;

/// The control-file families, one per half of the mod: (writer name, file stem). A writer of family `stem` writes
/// `<stem>.w<id>.log`, its id hex digits.
pub const CTL_FAMILIES: [(&str, &str); 2] = [("gui", "native_ctl.gui"), ("sim", "native_ctl.sim")];
/// The module writes, the mod reads.
pub const EVENTS_FILE: &str = "native_events.log";
/// The events file starts a new generation at this size: small, because a reloaded mod reads the newest generation
/// as history.
pub const EVENTS_ROTATE_BYTES: u64 = 1 << 20;
/// The `from` column of every event line.
pub const EVENTS_FROM: &str = "native";
/// A control line longer than this is thrown away.
pub const MAX_LINE: usize = 8 * 1024;
/// Most bytes read from one control file in one poll.
pub const MAX_READ: usize = 64 * 1024;
/// Most kind names one `hold_on` may carry.
pub const MAX_HOLD_KINDS: usize = 16;

// ---------------------------------------------------------------------------------------------------------------
// Mod to module
// ---------------------------------------------------------------------------------------------------------------

/// One message of a control file (`native_ctl.<half>.w<id>.log`).
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
    /// The script `build_proposal` that builds what deferred command `held` builds (its place or constructions) is this game's stamp
    /// apply of it, built from the record (not injected): the held command takes that command's result, as with `inject_next {held}`.
    CompleteNext { held: u64, ttl: u64, tm: String },
    /// `terrain.rs`: the NEXT script `build_proposal` whose terrain height grid has exactly this header (`x0, y0, w, h`: the mod's carrier,
    /// `GridVec2f.new(x0, y0, w, h)`) gets the cells of `native_terrain_in_<key>.txt` written into it before the simulation applies it.
    /// Answered `injected {key}` or `inject_refused {key}` (the carrier then applies nothing). Expires after `ttl` ms (0: default).
    TerrainApply { key: u64, x0: i32, y0: i32, w: i32, h: i32, ttl: u64 },
    /// `terrain.rs`: share terrain height edits (`on`): a terrain tool's own command is emptied at its apply and its cells are written
    /// for the mod (`terrain` event); off: the tool's command applies as it is. The mod says it once holding is on and at the session's end.
    TerrainShare { on: bool },
    /// `assets.rs`: the NEXT script `build_proposal` that only removes exactly the entities `ids`, in this order (this game's own ids of the
    /// asset groups the tick replaces, the mod resolved them), with `o2n` entries in its `old2new`, and adds nothing (the mod's carrier of
    /// a shared tree, plant or rock brush tick or asset eraser tick) gets the asset groups of `native_assets_in_<key>.txt` built into its
    /// `toAdd` before the simulation applies it. Answered `injected {key}` or `inject_refused {key}` (the carrier then applies nothing).
    /// Expires after `ttl` ms (0: default). Sharing follows `terrain_share`.
    AssetsApply { key: u64, ids: Vec<u32>, o2n: u64, ttl: u64 },
    /// Liveness; answered with `pong`. `paused` (optional, absent: false): the session's game time stands still right now (the session
    /// stands at its pause, or its speed is 0): what is held does not age meanwhile (`hold.rs`). Every ping says it, and the mod pings
    /// at once when it changes, so a lost line is corrected by the next ping.
    Ping { n: i64, paused: bool },
    /// `FEVERLINK3_NATIVE_PATCH`: patch the NEXT script `build_proposal` command that reaches the command list (not a held one):
    /// its `frozenNodes` get these indices into `proposal.addedNodes`, which must hold exactly `adds` nodes. Expires after `ttl` ms
    /// (0: the module's default).
    /// `at` (optional, 1 to 64 `{x, y, z}` positions) names the nodes by position instead: each must match exactly one node of the
    /// converted proposal within 0.05 m, and then `frozen` and `adds` are not needed (and not used).
    PatchNext { frozen: Vec<u32>, adds: u32, at: Vec<[f64; 3]>, ttl: u64 },
    /// `FEVERLINK3_NATIVE_INJECT`: the script `build_proposal` command of this build (the mod's placeholder, identified by the text's place
    /// or constructions) gets the engine `Proposal` the module builds natively from the portable text in `native_inject_<key>.txt`
    /// (`inject.rs`). Orders wait in a queue in the order they were written. Expires after `ttl` ms (0: default).
    /// `held` (0: none) is this game's deferred held command of the same build (`defer`): it completes with this command's result.
    /// `tm` (optional) is the mod's timing text of the build; the module adds the rebuild and apply times and writes it back (`timing`).
    /// `refuse` (optional, a test switch: `FEVERLINK3_DEBUG_REFUSE_INJECT`): the module refuses the order with this reason once a command
    /// passed the identity checks, exactly like any other refusal (the command is emptied, `held` is failed).
    /// `refund` (optional, 0: none) is how many refund entries the mod wrote in `native_refund_<key>.txt` (this game's own entities):
    /// the module puts them into the command's Context `refundableEntities` with the engine's insert (`refund.rs`).
    InjectNext { key: u64, ttl: u64, held: u64, tm: String, refuse: String, refund: u64 },
    /// The mod's send of the command an `inject_next` (`key`, not 0) or a `complete_next` (`key` 0, its `held`) was written for has
    /// returned (owner decision B, 2026-10-21: the order is the mod's own send's, not a guess by content). The order is armed only
    /// between its line and this one; an order still waiting now was used by no command during its send and is refused at once.
    SendDone { key: u64, held: u64 },
    /// The NEXT player `build_proposal` a script sends through the command list is the mod's own send (its interface half's apply of a
    /// stamped build during a pause): not held. Every other one is held like a tool's command (`Runtime::plan_add`, `hold::OwnSend`).
    OwnNext,
    /// Smooth pacing (`pace.rs`): scale the engine's batch interval by `permille` / 1000 (500 to 2000, held to that range). The mod
    /// sends it when the factor changes and every second while it is not 1; one not refreshed for `pace::STALE_MS` goes back to 1.
    Pace { permille: u32 },
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

/// A whole number that fits an `i32` (grid coordinates may be negative: the map's origin is its centre).
fn signed(p: &Value, key: &str, why: &'static str) -> Result<i32, CtlError> {
    let v = p.get(key).ok_or(CtlError::BadPayload(why))?;
    let n = v.as_i64().or_else(|| v.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 1e10).map(|f| f as i64));
    n.and_then(|n| i32::try_from(n).ok()).ok_or(CtlError::BadPayload(why))
}

fn positive(n: i32, why: &'static str) -> Result<i32, CtlError> {
    if n >= 0 {
        Ok(n)
    } else {
        Err(CtlError::BadPayload(why))
    }
}

fn optional(p: &Value, key: &str, why: &'static str) -> Result<u64, CtlError> {
    match p.get(key) {
        None => Ok(0),
        Some(v) => whole(v).ok_or(CtlError::BadPayload(why)),
    }
}

/// Turns one record of a control file into a message.
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
            key: required(&p, "key", "terrain_apply needs a numeric key")?,
            x0: signed(&p, "x0", "terrain_apply needs a whole x0")?,
            y0: signed(&p, "y0", "terrain_apply needs a whole y0")?,
            w: signed(&p, "w", "terrain_apply needs a whole w").and_then(|w| positive(w, "w must not be negative"))?,
            h: signed(&p, "h", "terrain_apply needs a whole h").and_then(|h| positive(h, "h must not be negative"))?,
            ttl: optional(&p, "ttl", "ttl must be a number of milliseconds")?,
        }),
        "assets_apply" => {
            let mut ids = Vec::new();
            // a tick that removes nothing (a brush on open ground) sends `ids = {}`: an empty Lua table reads as an empty object, not a
            // list (quick lab trees-speed2 2026-10-26: "assets_apply: bad payload: ids must be a list" on both games, the brush's tick
            // applied on no game)
            if let Some(v) = p.get("ids").filter(|v| !v.as_object().is_some_and(|o| o.is_empty())) {
                let list = v.as_array().ok_or(CtlError::BadPayload("ids must be a list"))?;
                if list.len() > crate::assets::MAX_REMOVE {
                    return Err(CtlError::BadPayload("ids must hold at most 256 entities"));
                }
                for item in list {
                    let n = whole(item).and_then(|n| u32::try_from(n).ok());
                    ids.push(n.ok_or(CtlError::BadPayload("an id is not a whole number"))?);
                }
            }
            Ok(CtlMessage::AssetsApply {
                key: required(&p, "key", "assets_apply needs a numeric key")?,
                ids,
                o2n: optional(&p, "o2n", "o2n must be a count")?,
                ttl: optional(&p, "ttl", "ttl must be a number of milliseconds")?,
            })
        }
        "terrain_share" => Ok(CtlMessage::TerrainShare {
            on: p
                .get("on")
                .and_then(Value::as_bool)
                .ok_or(CtlError::BadPayload("terrain_share needs on = true or false"))?,
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
            refund: optional(&p, "refund", "refund must be a number of entries")?,
        }),
        "own_next" => Ok(CtlMessage::OwnNext),
        "send_done" => {
            let (key, held) = (
                optional(&p, "key", "key must be an inject order's key")?,
                optional(&p, "held", "held must be a held command id")?,
            );
            if key == 0 && held == 0 {
                return Err(CtlError::BadPayload(
                    "send_done needs the key of an inject_next or the held of a complete_next",
                ));
            }
            Ok(CtlMessage::SendDone { key, held })
        }
        "ping" => {
            let n = match p.get("n") {
                None => 0,
                Some(v) => v
                    .as_i64()
                    .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0 && f.abs() <= 9.0e15).map(|f| f as i64))
                    .ok_or(CtlError::BadPayload("n must be a whole number"))?,
            };
            let paused = match p.get("paused") {
                None => false,
                Some(v) => v.as_bool().ok_or(CtlError::BadPayload("paused must be true or false"))?,
            };
            Ok(CtlMessage::Ping { n, paused })
        }
        "pace" => {
            let p = required(&p, "permille", "pace needs a factor in thousandths")?;
            let permille = crate::pace::accept_permille(p)
                .ok_or(CtlError::BadPayload("permille must be a factor from 500 to 2000"))?;
            Ok(CtlMessage::Pace { permille })
        }
        other => Err(CtlError::UnknownKind(other.chars().take(40).collect())),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Reading the control files
// ---------------------------------------------------------------------------------------------------------------

/// What one poll of the control files found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Poll {
    /// New, complete, valid records: each writer's in its order, the writers one after the other (interface ones first).
    pub lines: Vec<Line>,
    /// A control file existed before and is gone now (the session folder was cleaned). Reported once.
    pub gone: bool,
    /// Records thrown away: longer than [`MAX_LINE`], or not a link record.
    pub discarded: usize,
    /// A read error (the file is retried on the next poll), or a generation that was deleted before it was read.
    pub error: Option<String>,
    /// Writers that appeared since the link opened, at their first line: `sim.w<id> (from <its from column>)`, for the log.
    pub new_writers: Vec<String>,
}

/// Generation switches one poll of one file follows at most.
const MAX_SWITCHES: usize = 4;

/// Reads new records from the control files by position: one source per writer, each with its own position.
/// Tolerates a missing file, a partial last line (it waits for its newline), truncation and a rewrite (read again
/// from the first byte), and follows each file's generations. Nothing is dropped for what a line says.
#[derive(Debug)]
pub struct CtlReader {
    families: Vec<Family>,
    /// The folder the writers' files are found in by listing it (`None` for a reader of one fixed file).
    scan: Option<Scan>,
}

#[derive(Debug)]
struct Scan {
    dir: PathBuf,
}

/// One family of writer files (or one plain file).
#[derive(Debug)]
struct Family {
    /// `gui`, `sim` (a reader of one file: the file's name).
    name: String,
    /// The file stem of the family (`native_ctl.sim`); `None` for a reader of one fixed file.
    stem: Option<String>,
    sources: Vec<Source>,
}

/// The writer id in a file name of family `stem`: `<stem>.w<id>.log`, `<stem>.w<id>.<gen>.log` or `<stem>.w<id>.gen`, the id
/// hex digits only.
pub fn writer_id(file_name: &str, stem: &str) -> Option<String> {
    let rest = file_name.strip_prefix(stem)?.strip_prefix(".w")?;
    let (id, tail) = rest.split_once('.')?;
    let tail_ok = tail == "log"
        || tail == "gen"
        || tail.strip_suffix(".log").is_some_and(|g| !g.is_empty() && g.bytes().all(|b| b.is_ascii_digit()));
    (tail_ok && !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit())).then(|| id.to_string())
}

/// The control file (its generation 1) of writer `id` of a family.
pub fn writer_log_path(dir: &Path, stem: &str, id: &str) -> PathBuf {
    dir.join(format!("{stem}.w{id}.log"))
}

impl CtlReader {
    /// A reader of one file from its first byte (tests, tools, the events file).
    pub fn new(path: PathBuf) -> CtlReader {
        let name = file_label(&path);
        CtlReader {
            families: vec![Family { name: name.clone(), stem: None, sources: vec![Source::new(&name, path)] }],
            scan: None,
        }
    }

    /// A reader of one file that treats whatever it holds now as history: it starts at its end.
    pub fn starting_at_end(path: PathBuf) -> io::Result<CtlReader> {
        let name = file_label(&path);
        let source = Source::at_end(&name, path)?;
        Ok(CtlReader { families: vec![Family { name, stem: None, sources: vec![source] }], scan: None })
    }

    /// The reader of the link: every writer's control file of the session folder ([`CTL_FAMILIES`]), found by listing the
    /// folder. The files there now are read from their ends, so lines of an earlier run are never acted on; a file that
    /// appears later is read from its start.
    pub fn writers_at_end(dir: &Path) -> io::Result<CtlReader> {
        let families = CTL_FAMILIES
            .iter()
            .map(|(name, stem)| Family { name: name.to_string(), stem: Some(stem.to_string()), sources: Vec::new() })
            .collect();
        let mut r = CtlReader { families, scan: Some(Scan { dir: dir.to_path_buf() }) };
        r.discover(true)?;
        Ok(r)
    }

    /// Lists the folder and takes on every writer file not known yet (`history`: read from its end).
    fn discover(&mut self, history: bool) -> io::Result<()> {
        let Some(scan) = self.scan.as_ref() else { return Ok(()) };
        let mut names: Vec<String> = match fs::read_dir(&scan.dir) {
            Ok(entries) => entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        names.sort();
        for f in &mut self.families {
            let Some(stem) = f.stem.clone() else { continue };
            for n in &names {
                let Some(id) = writer_id(n, &stem) else { continue };
                let name = format!("{}.w{id}", f.name);
                if f.sources.iter().any(|s| s.name == name) {
                    continue;
                }
                let path = writer_log_path(&scan.dir, &stem, &id);
                let mut s = if history { Source::at_end(&name, path)? } else { Source::new(&name, path) };
                s.announce = !history;
                f.sources.push(s);
            }
        }
        Ok(())
    }

    /// Whether the folder holds a writer file not known yet: one listing of the folder. (Its modification time is no
    /// guide: on NTFS it did not change when a file was created right after the link opened.)
    fn folder_news(&self) -> bool {
        let Some(scan) = self.scan.as_ref() else { return false };
        let Ok(entries) = fs::read_dir(&scan.dir) else { return false };
        entries.flatten().any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            self.families.iter().any(|f| {
                f.stem.as_deref().and_then(|stem| writer_id(&n, stem)).is_some_and(|id| {
                    let name = format!("{}.w{id}", f.name);
                    !f.sources.iter().any(|s| s.name == name)
                })
            })
        })
    }
    /// Where each writer's file is read, for the log: `gui.w1a2b at 1:345, sim.w3c4d at 1:0` (`no control writer
    /// yet` when there is none).
    pub fn describe(&self) -> String {
        let parts: Vec<String> =
            self.families.iter().flat_map(|f| &f.sources).map(|s| format!("{} at {}", s.name, s.position())).collect();
        if parts.is_empty() {
            "no control writer yet".to_string()
        } else {
            parts.join(", ")
        }
    }

    /// The writers known (files, or the one file).
    pub fn writers(&self) -> usize {
        self.families.iter().map(|f| f.sources.len()).sum()
    }

    /// True when a file is longer than what was consumed (or gone), or the folder may hold a new writer's file: the cheap
    /// check (a metadata call per writer and one for the folder, no open) that lets a caller on a hot path decide whether
    /// a [`poll`](Self::poll) is worth it.
    pub fn has_news(&self) -> bool {
        self.folder_news() || self.families.iter().any(|f| f.sources.iter().any(Source::has_news))
    }

    pub fn poll(&mut self) -> Poll {
        let mut out = Poll::default();
        if self.folder_news() {
            if let Err(e) = self.discover(false) {
                out.error = Some(format!("listing the control files: {e}"));
            }
        }
        for f in &mut self.families {
            for s in &mut f.sources {
                s.poll(&mut out);
            }
        }
        out
    }
}
fn file_label(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// One writer's file.
#[derive(Debug)]
struct Source {
    /// The writer (`gui.s1`, `sim.s2`; the file's name for a reader of one file).
    name: String,
    /// The log's name (its generation 1).
    base: PathBuf,
    gen: u64,
    /// Bytes of the generation file read; an incomplete last line waits in `pending`.
    offset: u64,
    pending: Vec<u8>,
    /// In the middle of a line that is not taken (longer than [`MAX_LINE`], or history when the reader opened):
    /// everything up to its newline is dropped.
    skipping: bool,
    /// The current generation file was seen.
    existed: bool,
    /// A writer that appeared after the link opened: its first line is announced in `Poll::new_writers`.
    announce: bool,
}

impl Source {
    fn new(name: &str, base: PathBuf) -> Source {
        Source {
            name: name.to_string(),
            base,
            gen: 1,
            offset: 0,
            pending: Vec::new(),
            skipping: false,
            existed: false,
            announce: false,
        }
    }

    /// Whatever the newest generation holds now is history: reading starts at its end, and an unfinished last line
    /// is history too (dropped up to its newline). A generation that already ends with its `rotate` line is history
    /// as a whole: reading starts in the next one.
    fn at_end(name: &str, base: PathBuf) -> io::Result<Source> {
        let mut s = Source::new(name, base);
        s.gen = gamelink::newest_generation(&s.base);
        let path = s.path();
        match File::open(&path) {
            Ok(mut f) => {
                let len = f.metadata()?.len();
                s.offset = len;
                s.existed = true;
                let from = len.saturating_sub(MAX_READ as u64);
                f.seek(SeekFrom::Start(from))?;
                let mut tail = Vec::new();
                f.take(MAX_READ as u64).read_to_end(&mut tail)?;
                if tail.last().is_some_and(|b| *b != b'\n') {
                    s.skipping = true;
                } else if let Some(next) = last_rotate(&tail, from > 0) {
                    s.next_generation(next);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(s)
    }

    fn path(&self) -> PathBuf {
        gamelink::generation_path(&self.base, self.gen)
    }

    /// Where the lines read end.
    fn position(&self) -> Pos {
        Pos { gen: self.gen, offset: self.offset.saturating_sub(self.pending.len() as u64) }
    }

    fn next_generation(&mut self, next: gamelink::Rotate) {
        self.gen = next.gen;
        self.offset = next.skip;
        self.pending.clear();
        self.skipping = false;
        self.existed = false;
    }

    fn has_news(&self) -> bool {
        match fs::metadata(self.path()) {
            Ok(m) => m.len() != self.offset,
            Err(_) => self.existed,
        }
    }

    fn poll(&mut self, out: &mut Poll) {
        for _ in 0..=MAX_SWITCHES {
            if !self.poll_generation(out) {
                break;
            }
        }
    }

    /// One read of the current generation. True when the source went on to another generation.
    fn poll_generation(&mut self, out: &mut Poll) -> bool {
        let path = self.path();
        let len = match fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let newest = gamelink::newest_generation(&self.base);
                if newest > self.gen {
                    // deleted under a reader that fell behind by every kept generation: on with the newest (cannot
                    // happen to a reader polled every 20 ms; said, never silent)
                    out.error = Some(format!(
                        "{}: control file {} was deleted before it was read to its end; going on with generation {newest}",
                        self.name,
                        path.display()
                    ));
                    self.next_generation(gamelink::Rotate { gen: newest, skip: 0 });
                    return true;
                }
                if self.existed {
                    // gone for good as far as this session goes: a fresh file is read from its first byte
                    *self = Source::new(&self.name, self.base.clone());
                    out.gone = true;
                }
                return false;
            }
            Err(e) => {
                out.error = Some(format!("{}: {e}", path.display()));
                return false;
            }
        };
        self.existed = true;
        if len < self.offset {
            // shorter than what was read: another file now (only a cleaned folder replaces one); read it from the top
            self.offset = 0;
            self.pending.clear();
            self.skipping = false;
        }
        if len == self.offset {
            return false;
        }
        let mut chunk = Vec::new();
        let read = File::open(&path).and_then(|mut f| {
            f.seek(SeekFrom::Start(self.offset))?;
            f.take(MAX_READ as u64).read_to_end(&mut chunk)
        });
        if let Err(e) = read {
            out.error = Some(format!("{}: {e}", path.display()));
            return false;
        }
        self.offset += chunk.len() as u64;
        match self.feed(&chunk, out) {
            Some(next) => {
                self.next_generation(next);
                true
            }
            None => false,
        }
    }

    /// The records of `bytes`; stops at a `rotate` line and returns where it leads (the writer wrote nothing after it
    /// in this file).
    fn feed(&mut self, bytes: &[u8], out: &mut Poll) -> Option<gamelink::Rotate> {
        for &b in bytes {
            if b == b'\n' {
                if std::mem::take(&mut self.skipping) {
                    self.pending.clear();
                } else {
                    let raw = std::mem::take(&mut self.pending);
                    let before = out.lines.len();
                    let next = accept(&raw, out);
                    if self.announce && out.lines.len() > before {
                        self.announce = false;
                        out.new_writers.push(format!("{} (from {})", self.name, out.lines[before].from));
                    }
                    if next.is_some() {
                        return next;
                    }
                }
            } else if self.skipping {
                // the rest of a line that is not taken
            } else if self.pending.len() >= MAX_LINE {
                self.pending.clear();
                self.skipping = true;
                out.discarded += 1;
            } else {
                self.pending.push(b);
            }
        }
        None
    }
}

/// One complete line: a record goes out, a `rotate` line says where the file goes on, anything else is counted.
fn accept(raw: &[u8], out: &mut Poll) -> Option<gamelink::Rotate> {
    let text = String::from_utf8_lossy(raw);
    if text.trim().is_empty() {
        return None;
    }
    match gamelink::parse_line(&text) {
        Some(line) => {
            if let Some(next) = gamelink::rotate_target(&line) {
                return Some(next);
            }
            out.lines.push(line);
        }
        None => out.discarded += 1,
    }
    None
}

/// Where the last complete line of `tail` leads when it is a `rotate` line (`cut`: the first piece may start
/// mid-line).
fn last_rotate(tail: &[u8], cut: bool) -> Option<gamelink::Rotate> {
    let text = String::from_utf8_lossy(tail);
    let mut pieces: Vec<&str> = text.split('\n').collect();
    pieces.pop(); // after the last newline: nothing
    if cut && pieces.len() <= 1 {
        return None;
    }
    pieces
        .iter()
        .rev()
        .find(|p| !p.trim().is_empty())
        .and_then(|p| gamelink::parse_line(p))
        .and_then(|l| gamelink::rotate_target(&l))
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
        /// The RVA of the tool's call site; 0 for a player's build a script sent (no tool site).
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
    /// `terrain.rs`, the originator: a terrain tool's height edit was taken out of its own command at the apply (the command applied
    /// nothing) and its cells written as `native_terrain_out_<id>.txt`, for the mod to ship; every game applies them at the stamp.
    /// `x0, y0, w, h`: its height grid (`w = h = 0`: none, a paint-only edit); `paint`: it paints the ground.
    Terrain {
        id: u64,
        x0: i32,
        y0: i32,
        w: i32,
        h: i32,
        paint: bool,
        bytes: u64,
    },
    /// `terrain.rs`, the originator: a terrain tool's edit was emptied at its apply but cannot be shipped (`reason`): it is applied nowhere.
    TerrainRefused {
        reason: String,
    },
    /// `assets.rs`, the originator: a tree, plant or rock brush tick or an asset eraser tick was taken out of its own command at the apply
    /// (the command applied nothing) and written as `native_assets_out_<id>.txt`: it replaces `remove` asset groups and adds `groups`
    /// groups of `models` models in all. The mod ships it; every game applies it at the stamp.
    Assets {
        id: u64,
        remove: u64,
        groups: u64,
        models: u64,
        bytes: u64,
    },
    /// `assets.rs`, the originator: an asset tool's tick was emptied at its apply but cannot be shipped (`reason`): it is applied nowhere.
    AssetsRefused {
        reason: String,
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
    /// A hook could not do its part and this game may now differ from the others (`failsafe.rs`): `action` `repair` (the mod asks the
    /// host for the repair) or `stop` (a determinism piece is missing for the rest of this run: the player is told to leave and restart).
    Fault {
        place: String,
        detail: String,
        action: &'static str,
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
            Event::Terrain { .. } => "terrain",
            Event::TerrainRefused { .. } => "terrain_refused",
            Event::Assets { .. } => "assets",
            Event::AssetsRefused { .. } => "assets_refused",
            Event::Injected { .. } => "injected",
            Event::InjectRefused { .. } => "inject_refused",
            Event::ConvertDump { .. } => "convert_dump",
            Event::Audit { .. } => "audit",
            Event::Holding { .. } => "holding",
            Event::Pong { .. } => "pong",
            Event::Error { .. } => "error",
            Event::Fault { .. } => "fault",
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
            Event::Held { id, kind, site, flags, .. } => {
                let refund = match flags.as_ref().map(|f| f.refundable.as_ref()) {
                    Some(Some(ids)) => format!(", {} refundable entities", ids.len()),
                    Some(None) => ", refundable entities unreadable".to_string(),
                    None => String::new(),
                };
                // site 0: no tool's call site, a script's sendCommand (Runtime::plan_add)
                let from = if *site == 0 {
                    "a script's sendCommand: held by the native safety net".to_string()
                } else {
                    format!("tool site {site:#x}")
                };
                format!("event held: command {id}, kind {kind}, from {from}{refund}")
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
            Event::Fault { place, detail, action } => format!("event fault [{place}] ({action}): {detail}"),
            Event::Patched { frozen, adds, allocated } => {
                format!("event patched: frozenNodes {frozen:?} of {adds} added nodes (new block: {allocated})")
            }
            Event::PatchRefused { reason } => format!("event patch_refused: {reason}"),
            Event::ConvertDump { text } => format!("event convert_dump: {text}"),
            Event::Terrain { id, x0, y0, w, h, paint, bytes } => {
                format!("event terrain: edit {id}: heights {w}x{h} at {x0},{y0}, paint {paint} ({bytes} bytes of text)")
            }
            Event::TerrainRefused { reason } => format!("event terrain_refused: {reason}"),
            Event::Assets { id, remove, groups, models, bytes } => format!(
                "event assets: tick {id}: removes {remove} asset group(s), adds {groups} group(s) of {models} model(s) ({bytes} bytes of text)"
            ),
            Event::AssetsRefused { reason } => format!("event assets_refused: {reason}"),
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
                    let mut ctx = vec![
                        ("align", Lua::Bool(f.check_terrain_alignment)),
                        ("buildings", Lua::Bool(f.gather_buildings)),
                        ("cleanup", Lua::Bool(f.cleanup_street_graph)),
                        ("fields", Lua::Bool(f.gather_fields)),
                        ("ie", Lua::Bool(f.ignore_errors)),
                        ("pi", Lua::Bool(f.player_initiated)),
                        ("redo", Lua::Bool(f.extend_proposal_redo_pillars)),
                    ];
                    // the Context's refundableEntities, each { e = local entity id, k = key integer, r = revision {3 integers},
                    // n = value integer } (the mod names the entity by reference before it travels); absent when the container could
                    // not be read
                    if let Some(entries) = &f.refundable {
                        let entry = |r: &crate::engine::RefundEntry| {
                            Lua::map([
                                ("e", Lua::uint(u64::from(r.entity))),
                                ("k", Lua::Int(i64::from(r.key))),
                                ("n", Lua::Int(i64::from(r.value))),
                                ("r", Lua::List(r.revision.iter().map(|&x| Lua::Int(i64::from(x))).collect())),
                            ])
                        };
                        ctx.push(("refund", Lua::List(entries.iter().map(entry).collect())));
                    }
                    fields.insert(0, ("ctx", Lua::Map(ctx.into_iter().map(|(k, v)| (k.to_string(), v)).collect())));
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
            Event::Terrain { id, x0, y0, w, h, paint, bytes } => Lua::map([
                ("bytes", Lua::uint(*bytes)),
                ("h", Lua::Int(i64::from(*h))),
                ("id", Lua::uint(*id)),
                ("paint", Lua::Bool(*paint)),
                ("w", Lua::Int(i64::from(*w))),
                ("x0", Lua::Int(i64::from(*x0))),
                ("y0", Lua::Int(i64::from(*y0))),
            ]),
            Event::TerrainRefused { reason } => Lua::map([("reason", Lua::str(reason))]),
            Event::Assets { id, remove, groups, models, bytes } => Lua::map([
                ("bytes", Lua::uint(*bytes)),
                ("groups", Lua::uint(*groups)),
                ("id", Lua::uint(*id)),
                ("models", Lua::uint(*models)),
                ("remove", Lua::uint(*remove)),
            ]),
            Event::AssetsRefused { reason } => Lua::map([("reason", Lua::str(reason))]),
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
            Event::Fault { place, detail, action } => {
                Lua::map([("action", Lua::str(*action)), ("detail", Lua::str(detail)), ("where", Lua::str(place))])
            }
        };
        v.text()
    }
}

/// Appends events to `native_events.log` (a generation log, one writer: this module).
///
/// Every new generation starts with the latest `ready` and the `timing clock` line after it: a reloaded mod, which
/// reads only the newest generation as history, finds the module's state there instead of reading the whole log
/// again, and a mod that reads on from the generation before starts after them (the `rotate` line's `skip`).
#[derive(Debug)]
pub struct EventWriter {
    out: gamelink::GenWriter,
    /// The latest `ready` line and the `timing clock` line written after it.
    header: String,
}

impl EventWriter {
    /// Opens the writer in a session folder. Nothing is created until the first [`append`](Self::append).
    pub fn open(dir: &Path) -> io::Result<EventWriter> {
        let base = dir.join(EVENTS_FILE);
        let out =
            gamelink::GenWriter::open(base, EVENTS_FROM)?.with_limits(EVENTS_ROTATE_BYTES, gamelink::KEEP_GENERATIONS);
        Ok(EventWriter { out, header: String::new() })
    }

    /// The same writer with another generation size (tests).
    pub fn with_rotation(mut self, rotate_bytes: u64) -> EventWriter {
        self.out = self.out.with_limits(rotate_bytes, gamelink::KEEP_GENERATIONS);
        self
    }

    /// The file the next event goes to.
    pub fn path(&self) -> PathBuf {
        self.out.path()
    }

    /// Where the log ends: the next event starts here.
    pub fn end(&self) -> Pos {
        self.out.end()
    }

    /// The text of one event line, including the newline.
    pub fn line(event: &Event) -> String {
        gamelink::format_line(&Line {
            kind: event.kind().to_string(),
            from: EVENTS_FROM.to_string(),
            payload: event.payload(),
        })
    }

    /// Writes the events with one `write`, so a reader never sees half a batch's lines interleaved with another's.
    pub fn append(&mut self, events: &[Event]) -> io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut text = String::new();
        let mut header = None::<String>;
        for e in events {
            let line = EventWriter::line(e);
            match e {
                Event::Ready { .. } => header = Some(line.clone()),
                // a stop fault is the module's state for the rest of the run, like `ready`: a reloaded mod finds it in the header
                Event::Timing { what: "clock", .. } | Event::Fault { action: "stop", .. } => {
                    let h = header.get_or_insert_with(|| self.header.clone());
                    if !h.is_empty() {
                        h.push_str(&line);
                    }
                }
                _ => {}
            }
            text.push_str(&line);
        }
        let written = self.out.append(&text);
        if let Some(h) = header {
            self.header = h;
            self.out.set_header(self.header.clone());
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs::OpenOptions;
    use std::io::Write;

    fn line(kind: &str, payload: &str) -> Line {
        Line { kind: kind.into(), from: "1".into(), payload: payload.into() }
    }

    fn parse(kind: &str, payload: &str) -> Result<CtlMessage, CtlError> {
        parse_ctl_line(&line(kind, payload))
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
            Ok(CtlMessage::InjectNext {
                key: 12,
                ttl: 500,
                held: 0,
                tm: String::new(),
                refuse: String::new(),
                refund: 0
            })
        );
        assert_eq!(
            parse("inject_next", r#"{["key"]=3}"#),
            Ok(CtlMessage::InjectNext { key: 3, ttl: 0, held: 0, tm: String::new(), refuse: String::new(), refund: 0 })
        );
        assert_eq!(
            parse("inject_next", r#"{["key"]=3,["held"]=9}"#),
            Ok(CtlMessage::InjectNext { key: 3, ttl: 0, held: 9, tm: String::new(), refuse: String::new(), refund: 0 })
        );
        assert_eq!(
            parse("inject_next", r#"{["held"]=9,["key"]=3,["refuse"]="debug switch"}"#),
            Ok(CtlMessage::InjectNext {
                key: 3,
                ttl: 0,
                held: 9,
                tm: String::new(),
                refuse: "debug switch".into(),
                refund: 0
            })
        );
        assert!(
            matches!(parse("inject_next", r#"{["key"]=3,["refuse"]=1}"#), Err(CtlError::BadPayload(_))),
            "refuse is text"
        );
        // a bulldozing's order names how many refund entries its side file holds
        assert_eq!(
            parse("inject_next", r#"{["key"]=3,["refund"]=2}"#),
            Ok(CtlMessage::InjectNext { key: 3, ttl: 0, held: 0, tm: String::new(), refuse: String::new(), refund: 2 })
        );
        assert!(
            matches!(parse("inject_next", r#"{["key"]=3,["refund"]=-1}"#), Err(CtlError::BadPayload(_))),
            "refund is a count"
        );
        assert_eq!(parse("send_done", r#"{["key"]=3}"#), Ok(CtlMessage::SendDone { key: 3, held: 0 }));
        assert_eq!(parse("send_done", r#"{["held"]=7}"#), Ok(CtlMessage::SendDone { key: 0, held: 7 }));
        assert!(matches!(parse("send_done", "{}"), Err(CtlError::BadPayload(_))), "it names its order");
        assert_eq!(parse("own_next", "{}"), Ok(CtlMessage::OwnNext));
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
            parse("terrain_apply", r#"{["h"]=15,["key"]=2,["w"]=15,["x0"]=-311,["y0"]=137}"#),
            Ok(CtlMessage::TerrainApply { key: 2, x0: -311, y0: 137, w: 15, h: 15, ttl: 0 })
        );
        assert_eq!(parse("terrain_share", r#"{["on"]=true}"#), Ok(CtlMessage::TerrainShare { on: true }));
        assert_eq!(
            parse("assets_apply", r#"{["ids"]={[1]=9001,[2]=9002},["key"]=3,["o2n"]=1}"#),
            Ok(CtlMessage::AssetsApply { key: 3, ids: vec![9001, 9002], o2n: 1, ttl: 0 })
        );
        assert_eq!(
            parse("assets_apply", r#"{["ids"]={},["key"]=4,["o2n"]=0}"#),
            Ok(CtlMessage::AssetsApply { key: 4, ids: vec![], o2n: 0, ttl: 0 }),
            "a brush tick removes nothing: the mod's empty table is an empty list"
        );
        assert!(matches!(parse("assets_apply", r#"{["ids"]={["a"]=1},["key"]=4}"#), Err(CtlError::BadPayload(_))));
        assert_eq!(parse("ping", r#"{["n"]=-4}"#), Ok(CtlMessage::Ping { n: -4, paused: false }));
        assert_eq!(parse("ping", r#"{["n"]=5,["paused"]=true}"#), Ok(CtlMessage::Ping { n: 5, paused: true }));
        assert!(matches!(parse("ping", r#"{["n"]=5,["paused"]=1}"#), Err(CtlError::BadPayload(_))));
        assert_eq!(parse("pace", r#"{["permille"]=1500}"#), Ok(CtlMessage::Pace { permille: 1500 }));
        assert_eq!(parse("pace", r#"{["permille"]=1000}"#), Ok(CtlMessage::Pace { permille: 1000 }));
        assert_eq!(
            parse("pace", r#"{["permille"]=2400}"#),
            Ok(CtlMessage::Pace { permille: 2000 }),
            "capped at half speed"
        );
        assert!(matches!(parse("pace", r#"{["permille"]=0}"#), Err(CtlError::BadPayload(_))));
        assert!(matches!(parse("pace", r#"{["permille"]=1.5}"#), Err(CtlError::BadPayload(_))));
        assert!(matches!(parse("pace", "{}"), Err(CtlError::BadPayload(_))));
    }

    #[test]
    fn lines_written_by_the_mods_own_writer_parse() {
        // The bytes feverlink3_common.lua produces: C.newWriter(path, 1), then C.writerSend for each kind with
        // {kinds={"build_proposal"}}, {n=3}, {id=3, at=12400}, {id=7}, {} and {on=false}.
        let file = "hold_on\t1\t{[\"kinds\"]={[1]=\"build_proposal\"}}\nping\t1\t{[\"n\"]=3}\n\
                    release\t1\t{[\"at\"]=12400,[\"id\"]=3}\ncancel\t1\t{[\"id\"]=7}\nhold_off\t1\t{}\n\
                    terrain_share\t1\t{[\"on\"]=false}\n";
        let dir = TempDir::new("mod-writer");
        let path = dir.path().join("native_ctl.gui.s1.log");
        fs::write(&path, file).unwrap();
        let messages: Vec<_> = CtlReader::new(path).poll().lines.iter().map(|l| parse_ctl_line(l).unwrap()).collect();
        assert_eq!(
            messages,
            [
                CtlMessage::HoldOn { kinds: vec!["build_proposal".into()] },
                CtlMessage::Ping { n: 3, paused: false },
                CtlMessage::Release { id: 3, at: 12400 },
                CtlMessage::Cancel { id: 7 },
                CtlMessage::HoldOff,
                CtlMessage::TerrainShare { on: false },
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
        assert_eq!(parse("ping", "{}"), Ok(CtlMessage::Ping { n: 0, paused: false }));
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
            ("terrain_apply", r#"{["key"]=1,["x0"]=0,["y0"]=0,["w"]=-1,["h"]=3}"#),
            ("terrain_share", "{}"),
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

    fn rec(kind: &str, payload: &str) -> String {
        format!("{kind}\t1\t{payload}\n")
    }

    fn pings(p: &Poll) -> Vec<String> {
        p.lines.iter().map(|l| l.payload.clone()).collect()
    }

    /// The file a writer of a family names for itself (`<stem>.w<id>.log`); it appears with the first line.
    fn writer(dir: &Path, stem: &str, id: &str) -> PathBuf {
        writer_log_path(dir, stem, id)
    }

    const GUI: &str = "native_ctl.gui";
    const SIM: &str = "native_ctl.sim";

    #[test]
    fn reader_returns_new_complete_lines_once() {
        let dir = TempDir::new("reader-basic");
        let path = dir.path().join("ctl.log");
        let mut r = CtlReader::new(path.clone());
        assert_eq!(r.poll(), Poll::default(), "a missing file is not an event");
        assert!(!r.has_news());
        append(&path, &rec("ping", "{}"));
        assert!(r.has_news());
        let p = r.poll();
        assert_eq!(p.lines.len(), 1);
        assert_eq!(p.lines[0].kind, "ping");
        assert!(!r.has_news());
        assert_eq!(r.poll(), Poll::default(), "nothing is dispatched twice");
        append(&path, &(rec("hold_off", "{}") + &rec("ping", r#"{["n"]=1}"#)));
        let kinds: Vec<_> = r.poll().lines.into_iter().map(|l| l.kind).collect();
        assert_eq!(kinds, ["hold_off", "ping"]);
    }

    #[test]
    fn a_partial_last_line_waits_for_its_newline() {
        let dir = TempDir::new("reader-partial");
        let path = dir.path().join("ctl.log");
        let mut r = CtlReader::new(path.clone());
        append(&path, "ping\t1\t{[\"n\"]");
        assert!(r.poll().lines.is_empty());
        assert_eq!(r.describe(), "ctl.log at 1:0", "the unfinished line is not consumed");
        append(&path, "=5}\n");
        let p = r.poll();
        assert_eq!(p.lines.len(), 1);
        assert_eq!(p.lines[0].payload, r#"{["n"]=5}"#);
    }

    #[test]
    fn crlf_lines_and_blank_lines_are_fine() {
        let dir = TempDir::new("reader-crlf");
        let path = dir.path().join("ctl.log");
        let mut r = CtlReader::new(path.clone());
        append(&path, "ping\t1\t{}\r\n\r\nping\t1\t{}\r\n");
        let p = r.poll();
        assert_eq!(p.lines.len(), 2);
        assert_eq!(p.discarded, 0);
        assert_eq!(p.lines[0].payload, "{}");
    }

    #[test]
    fn the_same_line_twice_is_two_orders() {
        // nothing is dropped for what a line says (it used to be dropped for its number)
        let dir = TempDir::new("reader-repeat");
        let path = dir.path().join("ctl.log");
        let mut r = CtlReader::new(path.clone());
        let order = rec("patch_next", r#"{["adds"]=3,["frozen"]={[1]=1}}"#);
        append(&path, &(order.clone() + &order));
        let p = r.poll();
        assert_eq!((p.lines.len(), p.discarded), (2, 0));
    }

    #[test]
    fn truncation_resets_the_reader_to_the_first_byte() {
        let dir = TempDir::new("reader-truncate");
        let path = dir.path().join("ctl.log");
        let mut r = CtlReader::new(path.clone());
        append(&path, &(rec("ping", "{[\"n\"]=1}") + &rec("ping", "{[\"n\"]=2}") + &rec("ping", "{[\"n\"]=3}")));
        assert_eq!(r.poll().lines.len(), 3);
        // shorter: another file now, read from its start
        fs::write(&path, rec("ping", "{[\"n\"]=10}")).unwrap();
        assert_eq!(pings(&r.poll()), [r#"{["n"]=10}"#]);
        append(&path, &rec("hold_off", "{}"));
        assert_eq!(r.poll().lines.len(), 1, "and read on from there");
    }

    #[test]
    fn a_deleted_file_is_reported_once_and_a_new_one_starts_afresh() {
        let dir = TempDir::new("reader-gone");
        let path = dir.path().join("ctl.log");
        let mut r = CtlReader::new(path.clone());
        append(&path, &rec("ping", "{}"));
        assert_eq!(r.poll().lines.len(), 1);
        fs::remove_file(&path).unwrap();
        assert!(r.has_news(), "the disappearance is news");
        let p = r.poll();
        assert!(p.gone);
        assert!(!r.poll().gone, "reported once");
        append(&path, &rec("ping", "{}"));
        assert_eq!(r.poll().lines.len(), 1, "a re-created file is read from its first byte");
    }

    #[test]
    fn a_file_that_never_existed_is_not_gone() {
        let dir = TempDir::new("reader-never");
        let mut r = CtlReader::new(dir.path().join("ctl.log"));
        assert!(!r.poll().gone);
    }

    #[test]
    fn oversize_lines_are_thrown_away_without_losing_the_next_ones() {
        let dir = TempDir::new("reader-oversize");
        let path = dir.path().join("ctl.log");
        let mut r = CtlReader::new(path.clone());
        let huge = "x".repeat(MAX_LINE * 3);
        append(&path, &(rec("ping", "{1}") + &format!("ping\t1\t{huge}\n") + &rec("ping", "{3}")));
        let p = r.poll();
        assert_eq!(pings(&p), ["{1}", "{3}"]);
        assert_eq!(p.discarded, 1);
        assert!(r.families[0].sources[0].pending.len() < MAX_LINE, "the buffer stays bounded");
    }

    #[test]
    fn an_oversize_line_split_across_polls_is_still_one_discard() {
        let dir = TempDir::new("reader-oversize-split");
        let path = dir.path().join("ctl.log");
        let mut r = CtlReader::new(path.clone());
        append(&path, &format!("ping\t1\t{}", "y".repeat(MAX_LINE + 10)));
        assert_eq!(r.poll().discarded, 1);
        append(&path, &format!("{}\n", "z".repeat(1000)));
        let p = r.poll();
        assert_eq!(p.discarded, 0, "the tail of the same line is not counted again");
        assert!(p.lines.is_empty());
        append(&path, &rec("ping", "{}"));
        assert_eq!(r.poll().lines.len(), 1);
    }

    #[test]
    fn a_read_is_bounded_and_resumes() {
        let dir = TempDir::new("reader-bounded");
        let path = dir.path().join("ctl.log");
        let mut r = CtlReader::new(path.clone());
        let one = rec("ping", "{}");
        let n = MAX_READ / one.len() * 2 + 10;
        fs::write(&path, one.repeat(n)).unwrap();
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
        let path = dir.path().join("ctl.log");
        let mut r = CtlReader::new(path.clone());
        append(&path, "not a record\n\u{1}\u{2}\nping\t1\t{}\nnofrom\n");
        let p = r.poll();
        assert_eq!(p.lines.len(), 1);
        assert_eq!(p.discarded, 3);
        fs::write(&path, [0xff, 0xfe, b'\n']).unwrap(); // invalid UTF-8 never panics
        r.poll();
    }

    #[test]
    fn starting_at_the_end_ignores_history_and_an_unfinished_history_line() {
        let dir = TempDir::new("reader-history");
        let path = dir.path().join("ctl.log");
        append(
            &path,
            &(rec("hold_on", r#"{["kinds"]={[1]="build_proposal"}}"#) + &rec("ping", "{}") + "ping\t1\t{[\"n\"]"),
        );
        let mut r = CtlReader::starting_at_end(path.clone()).unwrap();
        assert!(r.poll().lines.is_empty(), "an earlier run's lines are not acted on");
        append(&path, "=4}\n");
        assert!(r.poll().lines.is_empty(), "the end of a line the earlier run was writing is history too");
        append(&path, &rec("hold_off", "{}"));
        assert_eq!(r.poll().lines.iter().map(|l| l.kind.as_str()).collect::<Vec<_>>(), ["hold_off"]);
        let mut none = CtlReader::starting_at_end(dir.path().join("missing.log")).unwrap();
        append(&dir.path().join("missing.log"), &rec("ping", "{}"));
        assert_eq!(none.poll().lines.len(), 1, "a file made after the start is news from its first byte");
    }

    // ---- one file per writer

    #[test]
    fn two_writers_interleaving_lose_no_line() {
        // two simulation states and the interface half, each with its own file, write in turns: every line arrives once,
        // each writer's in its order (with one shared file, two states appending at the same moment overwrite each other)
        let dir = TempDir::new("two-writers");
        let mut r = CtlReader::writers_at_end(dir.path()).unwrap();
        assert!(!r.has_news());
        assert_eq!(r.describe(), "no control writer yet");
        let gui = writer(dir.path(), GUI, "a1");
        let sim_a = writer(dir.path(), SIM, "b2");
        let sim_b = writer(dir.path(), SIM, "c3");
        let mut got = Vec::new();
        for i in 0..30 {
            append(&sim_a, &rec("inject_next", &format!("a{i}")));
            append(&sim_b, &rec("inject_next", &format!("b{i}")));
            if i % 3 == 0 {
                append(&gui, &rec("ping", &format!("g{i}")));
            }
            if i % 4 == 0 {
                let p = r.poll();
                assert_eq!(p.discarded, 0);
                got.extend(p.lines);
            }
        }
        got.extend(r.poll().lines);
        assert!(!r.has_news());
        for (who, want) in [("a", 30), ("b", 30), ("g", 10)] {
            let mine: Vec<&str> = got.iter().map(|l| l.payload.as_str()).filter(|p| p.starts_with(who)).collect();
            let expected: Vec<String> =
                (0..30).filter(|i| who != "g" || i % 3 == 0).map(|i| format!("{who}{i}")).collect();
            assert_eq!(mine.len(), want, "{who}");
            assert_eq!(mine, expected, "{who}: every line once, in its writer's order");
        }
        assert!(r.describe().starts_with("gui.wa1 at 1:"), "{}", r.describe());
        assert_eq!(r.writers(), 3);
    }

    #[test]
    fn a_writer_that_appears_after_the_link_opened_is_read_from_its_first_byte_and_named() {
        let dir = TempDir::new("late-writer");
        let mut r = CtlReader::writers_at_end(dir.path()).unwrap();
        let sim = writer(dir.path(), SIM, "d4");
        append(&sim, &rec("inject_next", "{first}"));
        assert!(r.has_news(), "a new file in the folder is news");
        let p = r.poll();
        assert_eq!(pings(&p), ["{first}"]);
        assert_eq!(p.new_writers, ["sim.wd4 (from 1)"]);
        assert!(r.poll().new_writers.is_empty(), "named once");
        // an empty file: known, nothing to read; named at its first line
        fs::write(writer(dir.path(), SIM, "e5"), "").unwrap();
        let p = r.poll();
        assert!(p.lines.is_empty() && p.new_writers.is_empty());
        append(&writer(dir.path(), SIM, "e5"), &rec("fail", "{}"));
        assert_eq!(r.poll().new_writers, ["sim.we5 (from 1)"]);
        // files of no family, and names that are not a writer's, are not read
        append(&dir.path().join("native_ctl.sim.wxyz.log"), &rec("ping", "{}"));
        append(&dir.path().join("native_ctl.other.w12.log"), &rec("ping", "{}"));
        append(&dir.path().join("native_inject_5.txt"), "text");
        assert!(r.poll().lines.is_empty());
        assert_eq!(r.writers(), 2);
    }

    #[test]
    fn writer_file_names() {
        assert_eq!(writer_id("native_ctl.sim.w0a1b.log", SIM), Some("0a1b".into()));
        assert_eq!(writer_id("native_ctl.sim.w0a1b.3.log", SIM), Some("0a1b".into()), "a later generation");
        assert_eq!(writer_id("native_ctl.sim.w0a1b.gen", SIM), Some("0a1b".into()), "the pointer");
        for bad in [
            "native_ctl.sim.w.log",
            "native_ctl.sim.wzz.log",
            "native_ctl.sim.log",
            "native_ctl.sim.w1.x.log",
            "native_ctl.gui.w1.log",
        ] {
            assert_eq!(writer_id(bad, SIM), None, "{bad}");
        }
    }

    #[test]
    fn opening_the_link_treats_each_writers_lines_as_history() {
        let dir = TempDir::new("two-writers-history");
        append(&writer(dir.path(), GUI, "1"), &rec("ping", "{}"));
        let sim = writer(dir.path(), SIM, "2");
        append(&sim, &rec("fail", "{}"));
        let mut r = CtlReader::writers_at_end(dir.path()).unwrap();
        assert!(r.poll().lines.is_empty());
        append(&sim, &rec("inject_next", "{}"));
        let p = r.poll();
        assert_eq!(p.lines.iter().map(|l| l.kind.as_str()).collect::<Vec<_>>(), ["inject_next"]);
        assert!(p.new_writers.is_empty(), "the writers there at the start are not new");
    }

    #[test]
    fn a_reader_follows_rotations_without_losing_or_splitting_a_line() {
        let dir = TempDir::new("rotate-follow");
        let base = writer(dir.path(), SIM, "3");
        let mut w = gamelink::GenWriter::open(base.clone(), "1").unwrap().with_limits(150, 3);
        let mut r = CtlReader::writers_at_end(dir.path()).unwrap();
        let mut got = Vec::new();
        let mut discarded = 0;
        for i in 1..=120u64 {
            w.append(&rec("ping", &format!("{{[\"n\"]={i}}}"))).unwrap();
            if i % 5 == 0 || i % 11 == 0 {
                let p = r.poll();
                discarded += p.discarded;
                assert!(p.error.is_none(), "{p:?}");
                got.extend(pings(&p));
            }
        }
        while r.has_news() {
            let p = r.poll();
            discarded += p.discarded;
            got.extend(pings(&p));
        }
        assert_eq!(got, (1..=120).map(|i| format!("{{[\"n\"]={i}}}")).collect::<Vec<_>>());
        assert_eq!(discarded, 0, "a rotate line is no discarded record");
        assert!(w.generation() > 3, "it rotated ({})", w.generation());
        assert!(!gamelink::generation_path(&base, w.generation() - 3).exists(), "older generations are deleted");
        // a reader opened now starts at the end of the newest generation, also when that one has just been ended
        let mut late = CtlReader::writers_at_end(dir.path()).unwrap();
        assert!(late.poll().lines.is_empty());
        w.append(&rec("ping", "{121}")).unwrap();
        assert_eq!(pings(&late.poll()), ["{121}"]);
    }

    #[test]
    fn a_link_opened_right_after_a_rotate_line_reads_the_next_generation_from_its_start() {
        let dir = TempDir::new("rotate-open");
        let base = writer(dir.path(), GUI, "4");
        append(&base, &(rec("ping", "{}") + &gamelink::rotate_line("1", 2, 0)));
        fs::write(gamelink::pointer_path(&base), "2\n").unwrap();
        let mut r = CtlReader::writers_at_end(dir.path()).unwrap();
        assert!(r.poll().lines.is_empty());
        append(&gamelink::generation_path(&base, 2), &rec("hold_off", "{}"));
        assert_eq!(r.poll().lines.iter().map(|l| l.kind.as_str()).collect::<Vec<_>>(), ["hold_off"]);
    }

    // ---- events

    #[test]
    fn lua_literals_match_the_mods_serialiser() {
        let v = Lua::map([
            ("s", Lua::str("a\tb\nc\"d\\e\u{1}f Ã©")),
            ("n", Lua::Int(-3)),
            ("b", Lua::Bool(false)),
            ("l", Lua::List(vec![Lua::Int(1), Lua::str("x")])),
            ("i", Lua::IntMap([(52, Lua::Int(9)), (3, Lua::Int(1))].into_iter().collect())),
            ("e", Lua::Map(BTreeMap::new())),
        ]);
        assert_eq!(
            v.text(),
            "{[\"b\"]=false,[\"e\"]={},[\"i\"]={[3]=1,[52]=9},[\"l\"]={[1]=1,[2]=\"x\"},[\"n\"]=-3,\
             [\"s\"]=\"a\\tb\\nc\\\"d\\\\e\\001f Ã©\"}"
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
            refundable: None,
        };
        let ev = Event::Held { id: 4, kind: 52, site: 1, ms: 2, done: true, flags: Some(flags.clone()) };
        let json: Value = serde_json::from_slice(&luatext::lua_to_json(&ev.payload()).unwrap()).unwrap();
        assert_eq!(
            json["ctx"],
            serde_json::json!({"align": false, "buildings": true, "cleanup": true, "fields": true, "ie": false, "pi": true, "redo": true})
        );
        assert!(ev.log_line().unwrap().ends_with("refundable entities unreadable"), "{:?}", ev.log_line());
        let refund = Event::Held {
            id: 4,
            kind: 52,
            site: 1,
            ms: 2,
            done: true,
            flags: Some(BuildFlags {
                refundable: Some(vec![
                    crate::engine::RefundEntry { entity: 98719, key: -1, revision: [1, 11, 0], value: 0x153 },
                    crate::engine::RefundEntry { entity: 7, key: -1, revision: [1, 9, 0], value: 0x156 },
                ]),
                ..flags
            }),
        };
        let json: Value = serde_json::from_slice(&luatext::lua_to_json(&refund.payload()).unwrap()).unwrap();
        assert_eq!(
            json["ctx"]["refund"],
            serde_json::json!([{"e": 98719, "k": -1, "r": [1, 11, 0], "n": 0x153}, {"e": 7, "k": -1, "r": [1, 9, 0], "n": 0x156}]),
            "every entry goes to the mod: local id, key integer, revision, value integer"
        );
        assert!(refund.log_line().unwrap().ends_with(", 2 refundable entities"), "{:?}", refund.log_line());
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
            let text = EventWriter::line(&ev);
            let back = gamelink::parse_line(&text).expect("a link record");
            assert_eq!((back.kind.as_str(), back.from.as_str()), (ev.kind(), "native"));
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
            EventWriter::line(&Event::Held { id: 3, kind: 52, site: 5_060_975, ms: 120, done: false, flags: None }),
            "held\tnative\t{[\"id\"]=3,[\"kind\"]=52,[\"ms\"]=120,[\"site\"]=5060975}\n"
        );
        assert_eq!(
            EventWriter::line(&Event::Released { id: 3, reason: "silence", ms: 99 }),
            "released\tnative\t{[\"id\"]=3,[\"ms\"]=99,[\"reason\"]=\"silence\"}\n"
        );
        assert_eq!(EventWriter::line(&Event::Pong { n: 4 }), "pong\tnative\t{[\"n\"]=4}\n");
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
            EventWriter::line(&ready),
            "ready\tnative\t{[\"build\"]=\"40420\",[\"clock\"]=false,[\"hooks\"]={[1]=\"apply_command\",[2]=\"command_list_add\"},\
             [\"kinds\"]={[\"build_proposal\"]=52},[\"poolpatch\"]=true,[\"sites\"]=12,[\"version\"]=\"0.1.0\"}\n"
        );
        let holding = Event::Holding { on: true, kinds: vec!["build_proposal".into()], reason: "hold_on" };
        assert_eq!(
            EventWriter::line(&holding),
            "holding\tnative\t{[\"kinds\"]={[1]=\"build_proposal\"},[\"on\"]=true,[\"reason\"]=\"hold_on\"}\n"
        );
        let audit = Event::Audit {
            counts: vec![(17, 4), (52, 2)],
            total: 6,
            unreadable: 0,
            adds: AddCounts { tool: 2, other: 5, unknown: 0 },
            sites: vec![(0, 2)],
        };
        assert_eq!(
            EventWriter::line(&audit),
            "audit\tnative\t{[\"adds\"]={[\"other\"]=5,[\"tool\"]=2,[\"unknown\"]=0},[\"counts\"]={[17]=4,[52]=2},\
             [\"sites\"]={[0]=2},[\"total\"]=6,[\"unreadable\"]=0}\n"
        );
        let error = Event::Error { place: "release".into(), detail: "no held command 9".into() };
        assert_eq!(
            EventWriter::line(&error),
            "error\tnative\t{[\"detail\"]=\"no held command 9\",[\"where\"]=\"release\"}\n"
        );
    }

    #[test]
    fn writer_appends_events_and_continues_the_file_after_a_restart() {
        let dir = TempDir::new("writer");
        let mut w = EventWriter::open(dir.path()).unwrap();
        assert_eq!(w.end(), Pos::START);
        assert!(!w.path().exists(), "nothing is created before the first event");
        w.append(&[]).unwrap();
        assert!(!w.path().exists());
        w.append(&[Event::Pong { n: 1 }, Event::Pong { n: 2 }]).unwrap();
        w.append(&[Event::Pong { n: 3 }]).unwrap();
        let text = fs::read_to_string(w.path()).unwrap();
        let ns: Vec<String> = text.lines().filter_map(gamelink::parse_line).map(|l| l.payload).collect();
        assert_eq!(ns, [r#"{["n"]=1}"#, r#"{["n"]=2}"#, r#"{["n"]=3}"#]);
        let again = EventWriter::open(dir.path()).unwrap();
        assert_eq!(again.end(), Pos { gen: 1, offset: text.len() as u64 }, "a new writer goes on at the end");
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
        assert_eq!(pings(&p), [r#"{["n"]=2}"#]);
    }

    #[test]
    fn the_events_file_rotates_and_a_new_generation_starts_with_the_modules_state() {
        let dir = TempDir::new("writer-rotate");
        let mut w = EventWriter::open(dir.path()).unwrap().with_rotation(600);
        let ready = Event::Ready {
            build: "40420".into(),
            version: "0.1.0".into(),
            hooks: vec!["command_list_add".into()],
            poolpatch: false,
            clock: false,
            kinds: vec![],
            sites: 1,
        };
        w.append(std::slice::from_ref(&ready)).unwrap();
        w.append(&[Event::Timing { what: "clock", id: 0, detail: "wall=1 clock=2".into() }]).unwrap();
        // a reader that follows from the first byte, polled now and then while the writer goes through generations
        let mut follower = CtlReader::new(dir.path().join(EVENTS_FILE));
        let mut got = Vec::new();
        for n in 1..=200 {
            w.append(&[Event::Pong { n }]).unwrap();
            if n % 7 == 0 {
                got.extend(follower.poll().lines);
            }
        }
        got.extend(follower.poll().lines);
        let pongs: Vec<String> = got.iter().filter(|l| l.kind == "pong").map(|l| l.payload.clone()).collect();
        assert_eq!(
            pongs,
            (1..=200).map(|n| format!("{{[\"n\"]={n}}}")).collect::<Vec<_>>(),
            "every event exactly once, in order, across the generations"
        );
        assert_eq!(
            got.iter().filter(|l| l.kind == "ready").count(),
            1,
            "a reader that comes through a rotation starts after the copy at the top of the next file"
        );
        assert_eq!(got.iter().filter(|l| l.kind == "timing").count(), 1);
        let newest = gamelink::newest_generation(&dir.path().join(EVENTS_FILE));
        assert!(newest > gamelink::KEEP_GENERATIONS, "it rotated ({newest})");
        let files = fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "log"))
            .count();
        assert_eq!(files as u64, gamelink::KEEP_GENERATIONS, "older generations are deleted: the log is bounded");
        // a reloaded mod reads the newest generation only, and finds the module's state at its top
        let mut fresh = CtlReader::new(gamelink::generation_path(&dir.path().join(EVENTS_FILE), newest));
        let lines = fresh.poll().lines;
        assert_eq!(lines[0].kind, "ready");
        assert_eq!(lines[1].kind, "timing");
        assert!(lines[2..].iter().all(|l| l.kind == "pong"));
    }
}
