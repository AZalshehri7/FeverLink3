//! The file link with one game instance.
//!
//! The launcher and the game talk through files in a per-session folder (`docs/ARCHITECTURE.md` section 2): no
//! process injection, and either side may restart. The folder is `%TEMP%\feverlink3\<name>-<time>-<rand>\` and
//! holds:
//!
//! - `to_game.log`: written by the launcher, read by the mod. Append-only.
//! - `from_game.log`: written by the mod, read by the launcher. Append-only.
//! - `identity.txt`: who this game is and which session it is in (`session=`, `start=`, `id=`, `name=`, `role=` lines).
//! - `native.log`: the native module's log (written by the module, not here).
//!
//! Every line of the two logs is `kind<TAB>from<TAB>payload\n`, the payload on one line, verbatim (the mod writes Lua
//! literal syntax that is already one line, and its reader does not unescape; see [`format_line`]). A line carries no
//! number: the order of the lines in the file IS the order, and every reader reads by position, the generation and
//! the byte offset it has consumed ([`Pos`]). A reader takes complete lines only (up to the last newline), so a line
//! still being written waits for its end. A file shorter than what was consumed, or one that no longer holds the bytes
//! read at the same places, was replaced: reading starts again at its first byte (the files are append-only; only a
//! cleaned folder replaces them). Where a session begins and where a reloaded mod resumes are positions too
//! (`start=<gen>:<offset>` in `identity.txt`, the mod's resync cursor).
//!
//! (The lines used to carry a sequence number that a reader filtered on; a writer that restarted its numbering, the
//! mod's after a reload in the friend test of 2026-10-16, then had every later line dropped as old. Positions cannot
//! go wrong that way.)
//!
//! One writer per file. A writer here appends each batch of lines with one `write` on a file opened for appending,
//! which Windows carries out as one append, so a line lands whole. The mod's writers do not have that guarantee (the C
//! runtime's append is a seek to the end and a write, two steps): a file the mod can write from two Lua states at
//! once has one file per writer instead (the native control files, `docs/NATIVE_LINK.md` section 3).
//!
//! Generations (every link log, here and in the native module and the mod: `docs/ARCHITECTURE.md` section 2). A log
//! never grows without bound and is never truncated: it is a series of generation files, `to_game.log` (generation 1),
//! `to_game.2.log`, `to_game.3.log`, ..., and `to_game.gen` names the newest. A writer that has put [`ROTATE_BYTES`]
//! into its generation ends it with a `rotate` line naming the next one, writes the pointer, deletes the generation
//! [`KEEP_GENERATIONS`] back and goes on in the next file. A reader that reads the `rotate` line has read the whole
//! generation (its writer adds nothing after it) and goes on in the next: no line is lost or split by a rotation, and
//! following one costs a reader no file access at all until it happens. A writer with a header (lines a reader that
//! starts in a generation needs, the native module's `ready`) writes it at the top of the next generation BEFORE the
//! `rotate` line, and the `rotate` line says how many bytes it holds (`skip`): a reader that came through the earlier
//! generation starts after it (it read those lines already), one that starts in the new generation reads them.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Name of the file the launcher writes.
pub const TO_GAME: &str = "to_game.log";
/// Name of the file the game writes.
pub const FROM_GAME: &str = "from_game.log";
/// Name of the identity file.
pub const IDENTITY: &str = "identity.txt";

/// Longest line a reader accepts, in bytes, newline included. A payload is at most 1 MiB of JSON, which is at most
/// four times that as Lua text; a longer line is somebody's mistake, and one without a newline must not make the
/// reader keep everything it has read.
pub const MAX_LINE_BYTES: usize = 4 << 20;

/// Most bytes one [`Tail::poll`] reads. A file that grew by more is read over several polls.
const MAX_READ_PER_POLL: u64 = 8 << 20;

/// One line of either log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: String,
    pub from: String,
    pub payload: String,
}

/// A place in a generation log: generation `gen`, `offset` bytes into its file. Positions are ordered by generation,
/// then offset. Written as `<gen>:<offset>` (`identity.txt`, the mod's resync cursor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pos {
    pub gen: u64,
    pub offset: u64,
}

impl Pos {
    /// The first byte of a log.
    pub const START: Pos = Pos { gen: 1, offset: 0 };

    /// Reads `<gen>:<offset>`; `None` for anything else.
    pub fn parse(text: &str) -> Option<Pos> {
        let (gen, offset) = text.trim().split_once(':')?;
        let gen = gen.parse::<u64>().ok().filter(|g| *g >= 1)?;
        Some(Pos { gen, offset: offset.parse().ok()? })
    }
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.gen, self.offset)
    }
}

/// Formats a line, including the trailing newline. The payload travels verbatim: the mod's reader does not
/// unescape anything and its serialiser already keeps a payload on one line (control characters inside strings
/// are written as `\n`, `\t`, `\ddd`), so an escaping layer here would corrupt every string. `kind` and `from`
/// must not contain tabs or line breaks; any found are replaced by spaces, and so are line breaks in the payload
/// (a guard against a caller bug, they cannot occur in serialiser output) rather than corrupting the file.
pub fn format_line(line: &Line) -> String {
    let payload: String = line.payload.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    format!("{}\t{}\t{}\n", clean_field(&line.kind), clean_field(&line.from), payload)
}

fn clean_field(s: &str) -> String {
    s.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c }).collect()
}

/// Parses one line (without or with its trailing newline). `None` for a line with fewer than three fields; the
/// payload is everything after the second tab.
pub fn parse_line(text: &str) -> Option<Line> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    let mut parts = text.splitn(3, '\t');
    let kind = parts.next()?.to_string();
    let from = parts.next()?.to_string();
    let payload = parts.next()?.to_string();
    Some(Line { kind, from, payload })
}

// ---------------------------------------------------------------------------------------------------------------
// Generations
// ---------------------------------------------------------------------------------------------------------------

/// A writer starts a new generation once its current one holds this much. The mod uses the same value
/// (`C.ROTATE_BYTES`); a reader does not need to know it.
pub const ROTATE_BYTES: u64 = 4 << 20;

/// Generation files a writer keeps (the newest included); older ones are deleted when a new one starts.
pub const KEEP_GENERATIONS: u64 = 4;

/// The kind of the line that ends a generation. Its payload is `{["gen"]=<next>,["skip"]=<bytes>}`.
pub const ROTATE_KIND: &str = "rotate";

/// The file of generation `gen` of the log `base`: `base` itself for generation 1, `<stem>.<gen>.<ext>` after it.
pub fn generation_path(base: &Path, gen: u64) -> PathBuf {
    if gen <= 1 {
        return base.to_path_buf();
    }
    let stem = base.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    match base.extension() {
        Some(ext) => base.with_file_name(format!("{stem}.{gen}.{}", ext.to_string_lossy())),
        None => base.with_file_name(format!("{stem}.{gen}")),
    }
}

/// The pointer file of the log `base` (`<stem>.gen`): the number of its newest generation.
pub fn pointer_path(base: &Path) -> PathBuf {
    base.with_extension("gen")
}

/// The newest generation the pointer names; 1 when there is no pointer (or it cannot be read).
pub fn newest_generation(base: &Path) -> u64 {
    fs::read_to_string(pointer_path(base))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .filter(|g| *g >= 1)
        .unwrap_or(1)
}

/// Where a log ends now: its newest generation and that file's length (0 when it does not exist yet).
pub fn end_of(base: &Path) -> io::Result<Pos> {
    let gen = newest_generation(base);
    match fs::metadata(generation_path(base, gen)) {
        Ok(m) => Ok(Pos { gen, offset: m.len() }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Pos { gen, offset: 0 }),
        Err(e) => Err(e),
    }
}

/// The text of the line that ends a generation: the next one is `next`, and its first `skip` bytes are the
/// writer's header (a reader that comes through this line starts after them).
pub fn rotate_line(from: &str, next: u64, skip: u64) -> String {
    format_line(&Line {
        kind: ROTATE_KIND.into(),
        from: from.into(),
        payload: format!("{{[\"gen\"]={next},[\"skip\"]={skip}}}"),
    })
}

/// Where a `rotate` line sends a reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotate {
    /// The next generation (at least 2).
    pub gen: u64,
    /// Bytes at the top of it a reader that comes through the rotation has read already (the header).
    pub skip: u64,
}

/// The whole number after `["<name>"]=` in a Lua table text.
fn lua_whole(payload: &str, name: &str) -> Option<u64> {
    let at = payload.find(&format!("[\"{name}\"]="))? + name.len() + 5;
    let digits: String =
        payload[at..].chars().skip_while(|c| c.is_whitespace()).take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Where a `rotate` line leads, `None` for every other line.
pub fn rotate_target(line: &Line) -> Option<Rotate> {
    if line.kind != ROTATE_KIND {
        return None;
    }
    let gen = lua_whole(&line.payload, "gen").filter(|g| *g >= 2)?;
    Some(Rotate { gen, skip: lua_whole(&line.payload, "skip").unwrap_or(0) })
}

/// The one writer of a generation log (see the module documentation). It keeps no file open: each append opens the
/// current generation, so other processes may read, and the folder may be cleaned, at any time.
#[derive(Debug)]
pub struct GenWriter {
    base: PathBuf,
    from: String,
    gen: u64,
    /// Bytes in the current generation file.
    size: u64,
    rotate_bytes: u64,
    keep: u64,
    /// Lines written again at the top of every new generation (for a reader that starts there; one that comes
    /// through the rotation skips them, see the module documentation).
    header: String,
}

impl GenWriter {
    /// Continues the newest generation of `base` (the pointer), at its current size. `from` is the `from` column of
    /// the `rotate` lines.
    pub fn open(base: PathBuf, from: &str) -> io::Result<GenWriter> {
        let end = end_of(&base)?;
        Ok(GenWriter {
            base,
            from: from.to_string(),
            gen: end.gen,
            size: end.offset,
            rotate_bytes: ROTATE_BYTES,
            keep: KEEP_GENERATIONS,
            header: String::new(),
        })
    }

    /// The same writer with another generation size and number of generations kept (at least 2).
    pub fn with_limits(mut self, rotate_bytes: u64, keep: u64) -> GenWriter {
        self.rotate_bytes = rotate_bytes.max(1);
        self.keep = keep.max(2);
        self
    }

    /// The log's name (generation 1).
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The file the next line goes to (unless it starts a new generation).
    pub fn path(&self) -> PathBuf {
        generation_path(&self.base, self.gen)
    }

    pub fn generation(&self) -> u64 {
        self.gen
    }

    /// Where the log ends after what this writer wrote.
    pub fn end(&self) -> Pos {
        Pos { gen: self.gen, offset: self.size }
    }

    /// Lines written again at the top of every later generation.
    pub fn set_header(&mut self, text: String) {
        self.header = text;
    }

    /// Appends complete lines with one `write`. A full generation is ended first.
    pub fn append(&mut self, text: &str) -> io::Result<()> {
        if self.size > 0 && self.size >= self.rotate_bytes {
            self.rotate()?;
        }
        OpenOptions::new().create(true).append(true).open(self.path())?.write_all(text.as_bytes())?;
        self.size += text.len() as u64;
        Ok(())
    }

    /// Ends the current generation and starts the next. The header goes into the next file first, so a reader that
    /// follows the `rotate` line finds it there and starts after it. Only the `rotate` line has to be written: without
    /// the pointer a reader that follows still finds the next file (a fresh one starts one generation early and
    /// follows); an old file that cannot be deleted (a reader holds it open) stays until the folder is cleaned.
    fn rotate(&mut self) -> io::Result<()> {
        let next = self.gen + 1;
        let next_path = generation_path(&self.base, next);
        let mut skip = 0;
        if !self.header.is_empty() {
            // a rotation that failed after the header was written finds it there and does not write it twice
            let have = fs::metadata(&next_path).map(|m| m.len()).unwrap_or(0);
            if have == 0 {
                OpenOptions::new().create(true).append(true).open(&next_path)?.write_all(self.header.as_bytes())?;
            }
            skip = fs::metadata(&next_path).map(|m| m.len()).unwrap_or(0);
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path())?
            .write_all(rotate_line(&self.from, next, skip).as_bytes())?;
        let _ = fs::write(pointer_path(&self.base), format!("{next}\n"));
        if next > self.keep {
            let _ = fs::remove_file(generation_path(&self.base, next - self.keep));
        }
        self.gen = next;
        self.size = skip;
        Ok(())
    }
}

/// Makes a session or player name safe as a folder-name prefix: ASCII letters, digits, `-` and `_` only,
/// at most 24 characters, never empty.
pub fn folder_safe(name: &str) -> String {
    let s: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(24).collect();
    if s.is_empty() {
        "session".to_string()
    } else {
        s
    }
}

/// `%TEMP%\feverlink3`: where session folders live.
pub fn sessions_root() -> PathBuf {
    std::env::temp_dir().join("feverlink3")
}

/// A fresh session folder name: `<name>-<unix seconds>-<8 hex digits>`.
pub fn new_session_folder_name(name: &str) -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{}-{}-{:08x}", folder_safe(name), secs, rand::random::<u32>())
}

// ---------------------------------------------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------------------------------------------

/// The launcher's side of the link with one game: writes `to_game.log`, tails `from_game.log`.
///
/// One `Session` is one session's identity on the link. The live folder outlives sessions (quit to the menu, load another
/// save, host again: the same `to_game.log` and `from_game.log`), so a session never reads or names the lines of an earlier
/// one: [`Session::open`] starts tailing `from_game.log` after what it holds, and [`Session::write_identity`] tells the mod the
/// session's id and where `to_game.log` ended before it (`start=<gen>:<offset>`), which the mod starts reading at.
#[derive(Debug)]
pub struct Session {
    dir: PathBuf,
    to_game: GenWriter,
    tail: Tail,
    /// This session's id (16 hex digits, random), written to `identity.txt`.
    id: String,
    /// Where `to_game.log` ended when the session was opened: every line of this session lies after it.
    start: Pos,
}

impl Session {
    /// Creates a new session folder under [`sessions_root`] and opens the link in it.
    pub fn create(name: &str) -> io::Result<Session> {
        Self::create_under(&sessions_root(), name)
    }

    /// Creates a new session folder under `root`.
    pub fn create_under(root: &Path, name: &str) -> io::Result<Session> {
        let dir = root.join(new_session_folder_name(name));
        fs::create_dir_all(&dir)?;
        Self::open(&dir)
    }

    /// Opens the link in an existing folder for a new session. `to_game.log` is appended to where it ends (that place is
    /// the session's `start`); `from_game.log` is tailed from where it ends now: what it holds was written by the game in
    /// an earlier session (the mod writes only while `identity.txt` names a session, which this one does not yet), and a
    /// host that read it would relay an earlier session's actions.
    pub fn open(dir: &Path) -> io::Result<Session> {
        fs::create_dir_all(dir)?;
        let to_game = GenWriter::open(dir.join(TO_GAME), "0")?;
        let start = to_game.end();
        let mut tail = Tail::new(dir.join(FROM_GAME));
        // read through (and drop) what the file holds; an unfinished last line stays pending and is read when it ends
        loop {
            let before = tail.position();
            tail.poll()?;
            if tail.position() == before {
                break;
            }
        }
        Ok(Session { dir: dir.to_path_buf(), to_game, tail, id: format!("{:016x}", rand::random::<u64>()), start })
    }

    /// This session's id, as `identity.txt` names it.
    pub fn session_id(&self) -> &str {
        &self.id
    }

    /// Where `to_game.log` ended before this session: the mod reads the lines after it.
    pub fn start(&self) -> Pos {
        self.start
    }

    /// The session folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where `to_game.log` ends now.
    pub fn end(&self) -> Pos {
        self.to_game.end()
    }

    /// Writes `identity.txt`: who this game instance is and which session it is in. The mod reads it about once a second.
    /// `session=` and `start=` come first and `role=` last: the mod takes the file only with a complete `role=` line, so a
    /// read that races this write sees all of it or no role at all.
    pub fn write_identity(&self, player_id: u16, name: &str, role: &str) -> io::Result<()> {
        let text = format!(
            "session={}\nstart={}\nid={}\nname={}\nrole={}\n",
            self.id,
            self.start,
            player_id,
            clean_field(name),
            clean_field(role)
        );
        fs::write(self.dir.join(IDENTITY), text)
    }

    /// Appends one line to `to_game.log`.
    pub fn send(&mut self, kind: &str, from: &str, payload: &str) -> io::Result<()> {
        let line = Line { kind: kind.to_string(), from: from.to_string(), payload: payload.to_string() };
        self.to_game.append(&format_line(&line))
    }

    /// The same session with other generation limits for `to_game.log` (tests).
    pub fn with_rotation(mut self, rotate_bytes: u64, keep: u64) -> Session {
        self.to_game = self.to_game.with_limits(rotate_bytes, keep);
        self
    }

    /// New, not yet seen lines of `from_game.log`. See [`Tail::poll`].
    pub fn poll(&mut self) -> io::Result<Vec<Line>> {
        self.tail.poll()
    }

    /// The tail of `from_game.log`, for the blocking variant ([`Tail::spawn`]).
    pub fn into_tail(self) -> Tail {
        self.tail
    }
}

/// The newest generation file of a log and its length and modification time: what a heartbeat looks at to see
/// whether a writer is still writing (`None` when there is no such file).
pub fn newest_mark(base: &Path) -> Option<(u64, u64, Option<SystemTime>)> {
    let gen = newest_generation(base);
    fs::metadata(generation_path(base, gen)).ok().map(|m| (gen, m.len(), m.modified().ok()))
}

// ---------------------------------------------------------------------------------------------------------------
// Tail
// ---------------------------------------------------------------------------------------------------------------

/// Bytes a [`Tail`] remembers of the start of a file and of what it read last, to recognise the same file again.
const SIGNATURE_BYTES: usize = 64;

/// Generation switches one [`Tail::poll`] follows at most (each reads at most [`MAX_READ_PER_POLL`]).
const MAX_SWITCHES_PER_POLL: usize = 4;

/// Reads new lines from an append-only log by position, tolerating a missing file, a partial last line, truncation
/// and a rewrite, and following the log's generations. Every complete line after the position is handed out once, in
/// file order; nothing is dropped for what it says.
///
/// A rewrite is recognised by content, not only by size: the tail remembers the first bytes of the file and the
/// last bytes it read, and a file that no longer holds them at the same places was replaced, even when it is not
/// shorter than what was read (truncated and grown again between two polls). Reading then starts again at its first
/// byte: the new file's lines are new lines (nobody rewrites a link log; only a cleaned folder replaces one).
///
/// The file is written by the game and may hold anything: a poll reads at most 8 MiB, a line longer than
/// [`MAX_LINE_BYTES`] is dropped as a whole (up to its newline, however many polls that takes), and invalid UTF-8
/// is read as the replacement character. Memory stays at most the size of one line plus one poll.
#[derive(Debug)]
pub struct Tail {
    /// The log's name (generation 1).
    path: PathBuf,
    /// The generation being read.
    gen: u64,
    /// Bytes of the file already read (an incomplete last line is read, and kept in `pending`).
    offset: u64,
    /// Bytes read past the last complete line that do not yet end with a newline.
    pending: Vec<u8>,
    /// The line being read is over [`MAX_LINE_BYTES`]: what is left of it is dropped, up to its newline.
    skipping: bool,
    /// The first bytes of the file as read (up to [`SIGNATURE_BYTES`]); empty when reading did not start at byte 0.
    head: Vec<u8>,
    /// The last bytes read, which end at `offset` (up to [`SIGNATURE_BYTES`]).
    mark: Vec<u8>,
}

impl Tail {
    /// A tail of `path` from its start (its oldest generation still there).
    pub fn new(path: PathBuf) -> Tail {
        let newest = newest_generation(&path);
        let mut gen = newest;
        while gen > 1 && generation_path(&path, gen - 1).exists() {
            gen -= 1;
        }
        Tail::at(path, Pos { gen, offset: 0 })
    }

    /// A tail from the start of generation `gen`.
    pub fn at_generation(path: PathBuf, gen: u64) -> Tail {
        Tail::at(path, Pos { gen, offset: 0 })
    }

    /// A tail that reads the lines after `pos` (a place a line ends, as [`position`](Self::position) gives it).
    pub fn at(path: PathBuf, pos: Pos) -> Tail {
        Tail {
            path,
            gen: pos.gen.max(1),
            offset: pos.offset,
            pending: Vec::new(),
            skipping: false,
            head: Vec::new(),
            mark: Vec::new(),
        }
    }

    /// Back to the start of the file, forgetting what was read of an unfinished line.
    fn rewind(&mut self) {
        self.offset = 0;
        self.pending = Vec::new();
        self.skipping = false;
        self.head.clear();
        self.mark.clear();
    }

    /// The log being tailed (its generation 1).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The generation being read.
    pub fn generation(&self) -> u64 {
        self.gen
    }

    /// Where the lines handed out so far end: the next line starts here.
    pub fn position(&self) -> Pos {
        Pos { gen: self.gen, offset: self.offset.saturating_sub(self.pending.len() as u64) }
    }

    /// Reads whatever was appended since the last call and returns the complete lines in file order, through the
    /// generations the log went on to. A file that is not the one read before (shorter, or other bytes where the
    /// remembered ones were) was replaced: reading starts again at its beginning.
    pub fn poll(&mut self) -> io::Result<Vec<Line>> {
        let mut out = Vec::new();
        for _ in 0..=MAX_SWITCHES_PER_POLL {
            if !self.poll_generation(&mut out)? {
                break;
            }
        }
        Ok(out)
    }

    /// One read of the current generation. True when the tail went on to another generation (worth reading now).
    fn poll_generation(&mut self, out: &mut Vec<Line>) -> io::Result<bool> {
        let mut file = match File::open(generation_path(&self.path, self.gen)) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // deleted under a tail that fell far behind: on with the newest generation (what was in between
                // is gone); otherwise the file may be recreated later with fresh content: start over then
                let newest = newest_generation(&self.path);
                self.rewind();
                if newest > self.gen {
                    self.gen = newest;
                    return Ok(true);
                }
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        if len == self.offset {
            return Ok(false); // nothing new (a file replaced by one of the same length is noticed when it grows)
        }
        if len < self.offset || !self.same_file(&mut file)? {
            self.rewind();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut fresh = Vec::new();
        file.take(MAX_READ_PER_POLL).read_to_end(&mut fresh)?;
        if fresh.is_empty() {
            return Ok(false);
        }
        // `offset` counts every byte read from the file; an incomplete last line waits in `pending`
        if self.offset == 0 {
            self.head = fresh[..fresh.len().min(SIGNATURE_BYTES)].to_vec();
        }
        self.mark.extend_from_slice(&fresh[fresh.len().saturating_sub(SIGNATURE_BYTES)..]);
        let excess = self.mark.len().saturating_sub(SIGNATURE_BYTES);
        self.mark.drain(..excess);
        self.offset = self.offset.saturating_add(fresh.len() as u64);

        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(&fresh);
        drop(fresh);
        let mut rest: &[u8] = &buf;
        while let Some((line, after)) =
            rest.iter().position(|&b| b == b'\n').and_then(|newline| rest.split_at_checked(newline + 1))
        {
            rest = after;
            // the end of a line that was too long, or one that is: neither is looked at
            if std::mem::take(&mut self.skipping) || line.len() > MAX_LINE_BYTES {
                continue;
            }
            if let Some(parsed) = parse_line(&String::from_utf8_lossy(line)) {
                if let Some(next) = rotate_target(&parsed) {
                    // the writer's last line in this file: everything before it was read, the rest is in `next`,
                    // after the header this tail has read already
                    self.rewind();
                    self.gen = next.gen;
                    self.offset = next.skip;
                    return Ok(true);
                }
                out.push(parsed);
            }
        }
        if self.skipping || rest.len() > MAX_LINE_BYTES {
            self.skipping = true;
        } else {
            self.pending = rest.to_vec();
        }
        Ok(false)
    }

    /// Whether the open file still holds, at the same places, the bytes this tail read from it: its first bytes and
    /// the last ones before `offset`. Two small reads per poll.
    fn same_file(&self, file: &mut File) -> io::Result<bool> {
        if self.offset == 0 {
            return Ok(true);
        }
        let holds = |file: &mut File, at: u64, want: &[u8]| -> io::Result<bool> {
            let mut probe = vec![0u8; want.len()];
            file.seek(SeekFrom::Start(at))?;
            match file.read_exact(&mut probe) {
                Ok(()) => Ok(probe == want),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
                Err(e) => Err(e),
            }
        };
        let mark_at = self.offset.saturating_sub(self.mark.len() as u64);
        Ok(holds(file, 0, &self.head)? && holds(file, mark_at, &self.mark)?)
    }

    /// Runs the tail on its own thread, calling `on_line` for every new line, polling every `interval`, until
    /// [`TailHandle::stop`] is called or the handle is dropped. Read errors are passed to `on_error` and the
    /// loop continues (the file may be mid-rewrite). An error only if the thread cannot be started.
    pub fn spawn(
        mut self,
        interval: Duration,
        mut on_line: impl FnMut(Line) + Send + 'static,
        mut on_error: impl FnMut(io::Error) + Send + 'static,
    ) -> io::Result<TailHandle> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::Builder::new().name("feverlink3-tail".into()).spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                match self.poll() {
                    Ok(lines) => lines.into_iter().for_each(&mut on_line),
                    Err(e) => on_error(e),
                }
                thread::sleep(interval);
            }
            self
        })?;
        Ok(TailHandle { stop, thread: Some(thread) })
    }
}

/// Controls a tail thread started with [`Tail::spawn`].
#[derive(Debug)]
pub struct TailHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Tail>>,
}

impl TailHandle {
    /// Stops the thread and returns the tail, whose `position` tells what was read. `None` if the thread ended in a
    /// panic of its own (a callback's, say).
    pub fn stop(mut self) -> Option<Tail> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().and_then(|t| t.join().ok())
    }
}

impl Drop for TailHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    fn line(payload: &str) -> Line {
        Line { kind: "action".into(), from: "1".into(), payload: payload.into() }
    }

    fn payloads(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.payload.clone()).collect()
    }

    #[test]
    fn line_round_trip_keeps_the_payload_verbatim() {
        let l = Line { kind: "chat".into(), from: "3".into(), payload: r#"{["t"]="a\"b\\n\009"}"#.into() };
        let text = format_line(&l);
        assert_eq!(text.matches('\n').count(), 1, "payload stays on one line");
        assert!(text.contains(r#"{["t"]="a\"b\\n\009"}"#), "no escaping layer");
        assert_eq!(parse_line(&text), Some(l));
        let raw = Line { kind: "x".into(), from: "0".into(), payload: "a\nb\rc".into() };
        assert_eq!(parse_line(&format_line(&raw)).unwrap().payload, "a b c", "stray line breaks cannot split a line");
        assert_eq!(
            parse_line("hello\thost\t\n"),
            Some(Line { kind: "hello".into(), from: "host".into(), payload: String::new() })
        );
        assert_eq!(parse_line("clock\t0\tpayload\twith\ttabs\r\n").unwrap().payload, "payload\twith\ttabs");
    }

    #[test]
    fn malformed_lines_are_rejected() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("kind\tfrom"), None);
        assert_eq!(parse_line("garbage line"), None);
    }

    #[test]
    fn fields_with_tabs_are_cleaned_not_corrupting() {
        let l = Line { kind: "a\tb".into(), from: "c\nd".into(), payload: "p".into() };
        assert_eq!(parse_line(&format_line(&l)).unwrap().kind, "a b");
    }

    #[test]
    fn positions_read_and_write_and_order() {
        assert_eq!(Pos::parse("3:120"), Some(Pos { gen: 3, offset: 120 }));
        assert_eq!(Pos { gen: 3, offset: 120 }.to_string(), "3:120");
        assert_eq!(Pos::parse(" 1:0\n"), Some(Pos::START));
        for bad in ["", "7", "0:5", "a:1", "1:-1", "1:2:3"] {
            assert_eq!(Pos::parse(bad), None, "{bad}");
        }
        assert!(Pos { gen: 1, offset: 900 } < Pos { gen: 2, offset: 0 });
        assert!(Pos { gen: 2, offset: 10 } < Pos { gen: 2, offset: 11 });
    }

    #[test]
    fn folder_names() {
        assert_eq!(folder_safe("Host (Aziz) !"), "HostAziz");
        assert_eq!(folder_safe("***"), "session");
        let n = new_session_folder_name("host");
        let parts: Vec<&str> = n.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "host");
        assert!(parts[1].parse::<u64>().is_ok());
        assert_eq!(parts[2].len(), 8);
        assert_eq!(sessions_root().file_name().unwrap(), "feverlink3");
    }

    #[test]
    fn session_writes_identity_and_lines_and_continues_after_reopen() {
        let root = TempDir::new().unwrap();
        let mut s = Session::create_under(root.path(), "host").unwrap();
        assert!(s.dir().starts_with(root.path()));
        s.write_identity(0, "Aziz", "host").unwrap();
        assert_eq!(s.session_id().len(), 16);
        assert_eq!(
            fs::read_to_string(s.dir().join(IDENTITY)).unwrap(),
            format!("session={}\nstart=1:0\nid=0\nname=Aziz\nrole=host\n", s.session_id())
        );

        s.send("session", "host", "{\"started\":false}").unwrap();
        s.send("action", "2", "line two").unwrap();
        let text = fs::read_to_string(s.dir().join(TO_GAME)).unwrap();
        assert_eq!(text, "session\thost\t{\"started\":false}\naction\t2\tline two\n");
        assert_eq!(s.end(), Pos { gen: 1, offset: text.len() as u64 });

        let dir = s.dir().to_path_buf();
        drop(s);
        let mut again = Session::open(&dir).unwrap();
        assert_eq!(
            again.start(),
            Pos { gen: 1, offset: text.len() as u64 },
            "the next session starts where the log ends"
        );
        again.send("x", "host", "").unwrap();
        assert!(fs::read_to_string(dir.join(TO_GAME)).unwrap().ends_with("line two\nx\thost\t\n"));
        assert_eq!(end_of(&dir.join("missing.log")).unwrap(), Pos::START);
    }

    #[test]
    fn tail_returns_new_lines_once_and_waits_for_partial_lines() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut tail = Tail::new(path.clone());
        assert_eq!(tail.poll().unwrap(), vec![], "missing file is not an error");

        let mut f = File::create(&path).unwrap();
        f.write_all(format_line(&line("one")).as_bytes()).unwrap();
        f.write_all(b"action\t1\tpart").unwrap();
        f.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line("one")]);
        assert_eq!(
            tail.position(),
            Pos { gen: 1, offset: format_line(&line("one")).len() as u64 },
            "the partial line is not consumed"
        );
        assert_eq!(tail.poll().unwrap(), vec![], "nothing new, partial line held back");

        f.write_all(b"ial\n").unwrap();
        f.write_all(format_line(&line("three")).as_bytes()).unwrap();
        f.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line("partial"), line("three")]);
        assert_eq!(tail.position().offset, fs::metadata(&path).unwrap().len());
        assert_eq!(tail.poll().unwrap(), vec![]);
    }

    #[test]
    fn the_same_line_twice_is_two_lines() {
        // nothing is dropped for what a line says: two equal lines are two lines
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let text = format_line(&line("same")) + &format_line(&line("same"));
        fs::write(&path, text).unwrap();
        assert_eq!(Tail::new(path).poll().unwrap(), vec![line("same"), line("same")]);
    }

    #[test]
    fn a_tail_at_a_position_reads_what_follows_and_a_shorter_file_resets_it() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(TO_GAME);
        let first = format_line(&line("before"));
        fs::write(&path, first.clone() + &format_line(&line("after"))).unwrap();
        let mut tail = Tail::at(path.clone(), Pos { gen: 1, offset: first.len() as u64 });
        assert_eq!(tail.poll().unwrap(), vec![line("after")]);
        // a position beyond the file (the folder was cleaned): the file is another one, read from its first byte
        let mut beyond = Tail::at(path.clone(), Pos { gen: 1, offset: 10_000 });
        assert_eq!(payloads(&beyond.poll().unwrap()), ["before", "after"]);
    }

    #[test]
    fn truncation_resets_the_tail_to_the_first_byte() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut tail = Tail::new(path.clone());
        let mut text = String::new();
        for i in 1..=5 {
            text.push_str(&format_line(&line(&format!("old {i}"))));
        }
        fs::write(&path, &text).unwrap();
        assert_eq!(tail.poll().unwrap().len(), 5);

        // truncated to nothing, then a writer starts over: every line of the new file is read
        fs::write(&path, "").unwrap();
        assert_eq!(tail.poll().unwrap(), vec![]);
        fs::write(&path, format_line(&line("six"))).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line("six")]);

        // rewritten shorter: the new content is read from its start
        fs::write(&path, format_line(&line("x"))).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line("x")]);

        // file deleted and recreated
        fs::remove_file(&path).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![]);
        fs::write(&path, format_line(&line("eight"))).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line("eight")]);
    }

    #[test]
    fn tail_sees_a_truncate_and_regrow_within_one_poll() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut tail = Tail::new(path.clone());
        let mut text = String::new();
        for i in 1..=3 {
            text.push_str(&format_line(&line(&format!("before {i}"))));
        }
        fs::write(&path, &text).unwrap();
        assert_eq!(tail.poll().unwrap().len(), 3);
        // between two polls the file is truncated and grows past the old length again with other lines: a reader that
        // went on at its offset would start in the middle of a line and lose the lines before it
        let mut regrown = String::new();
        for i in 4..=9 {
            regrown.push_str(&format_line(&line(&format!("after the truncation {i}"))));
        }
        assert!(regrown.len() as u64 > text.len() as u64);
        fs::write(&path, &regrown).unwrap();
        let got = tail.poll().unwrap();
        assert_eq!(got.len(), 6);
        assert!(got.iter().all(|l| l.payload.starts_with("after the truncation")), "no line read from its middle");
        // and a plain append is read on from the offset as before
        OpenOptions::new().append(true).open(&path).unwrap().write_all(format_line(&line("x")).as_bytes()).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line("x")]);
    }

    #[test]
    fn to_game_rotates_and_a_tail_reads_every_line_once_across_generations() {
        let root = TempDir::new().unwrap();
        let mut s = Session::create_under(root.path(), "host").unwrap().with_rotation(300, 3);
        let base = s.dir().join(TO_GAME);
        let mut tail = Tail::new(base.clone());
        let mut got = Vec::new();
        for i in 1..=100u64 {
            s.send("action", "2", &format!("payload {i}")).unwrap();
            if i % 9 == 0 {
                got.extend(tail.poll().unwrap());
            }
        }
        loop {
            let more = tail.poll().unwrap();
            if more.is_empty() {
                break;
            }
            got.extend(more);
        }
        assert_eq!(
            payloads(&got),
            (1..=100).map(|i| format!("payload {i}")).collect::<Vec<_>>(),
            "every line once, in order"
        );
        let newest = newest_generation(&base);
        assert!(newest > 3, "it rotated ({newest})");
        assert!(!base.exists() && generation_path(&base, newest - 2).exists(), "three generations are kept");
        // a session reopened on the folder starts where the newest generation ends
        let dir = s.dir().to_path_buf();
        drop(s);
        let end = end_of(&base).unwrap();
        assert_eq!(end.gen, newest);
        assert_eq!(Session::open(&dir).unwrap().start(), end);
        // a tail started now begins at the oldest generation still there, and reads on through the newest
        let mut late = Tail::new(base.clone());
        assert_eq!(late.generation(), newest - 2);
        let mut seen = Vec::new();
        loop {
            let more = late.poll().unwrap();
            if more.is_empty() {
                break;
            }
            seen.extend(more);
        }
        assert_eq!(seen.last().map(|l| l.payload.as_str()), Some("payload 100"));
        assert_eq!(late.position(), end);
        // the heartbeat's look follows the newest file
        assert_eq!(newest_mark(&base).map(|m| m.0), Some(newest));
    }

    #[test]
    fn a_header_is_read_once_by_a_tail_that_follows_and_by_one_that_starts_after_the_rotation() {
        let root = TempDir::new().unwrap();
        let base = root.path().join("events.log");
        let mut w = GenWriter::open(base.clone(), "native").unwrap().with_limits(100, 4);
        let header = format_line(&Line { kind: "ready".into(), from: "native".into(), payload: "{}".into() });
        w.append(&header).unwrap();
        w.set_header(header.clone());
        let mut follower = Tail::new(base.clone());
        let mut got = follower.poll().unwrap();
        for i in 1..=12 {
            w.append(&format_line(&line(&format!("event {i}")))).unwrap();
            got.extend(follower.poll().unwrap());
        }
        assert!(w.generation() > 2, "it rotated");
        let kinds = got.iter().filter(|l| l.kind == "ready").count();
        assert_eq!(kinds, 1, "the follower read the header once: {got:?}");
        assert_eq!(got.iter().filter(|l| l.kind == "action").count(), 12, "and every event once");
        // a reader that starts in the newest generation finds the header at its top
        let fresh = Tail::at_generation(base.clone(), w.generation()).poll().unwrap();
        assert_eq!(fresh.first().map(|l| l.kind.as_str()), Some("ready"));
    }

    #[test]
    fn rotate_lines_name_the_next_generation() {
        let l = parse_line(&rotate_line("native", 7, 33)).unwrap();
        assert_eq!((l.kind.as_str(), rotate_target(&l)), (ROTATE_KIND, Some(Rotate { gen: 7, skip: 33 })));
        assert_eq!(rotate_target(&line("x")), None);
        assert_eq!(
            rotate_target(&Line { kind: ROTATE_KIND.into(), from: "0".into(), payload: "{[\"gen\"]=2}".into() }),
            Some(Rotate { gen: 2, skip: 0 })
        );
        assert_eq!(
            rotate_target(&Line { kind: ROTATE_KIND.into(), from: "0".into(), payload: "{[\"gen\"]=1}".into() }),
            None
        );
        let base = Path::new("dir").join("native_ctl.sim.log");
        assert_eq!(generation_path(&base, 1), base);
        assert_eq!(generation_path(&base, 12), Path::new("dir").join("native_ctl.sim.12.log"));
        assert_eq!(pointer_path(&base), Path::new("dir").join("native_ctl.sim.gen"));
    }

    #[test]
    fn tail_skips_garbage_and_keeps_the_order_of_the_file() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut tail = Tail::new(path.clone());
        let mut text = String::new();
        text.push_str("garbage line\n");
        text.push_str(&format_line(&line("two")));
        text.push_str(&format_line(&line("one, written after two")));
        text.push_str(&format_line(&line("three")));
        fs::write(&path, &text).unwrap();
        assert_eq!(payloads(&tail.poll().unwrap()), ["two", "one, written after two", "three"]);
    }

    #[test]
    fn tail_thread_delivers_lines_and_stops() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        fs::write(&path, format_line(&line("first"))).unwrap();
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&got);
        let handle = Tail::new(path.clone())
            .spawn(Duration::from_millis(10), move |l| sink.lock().unwrap().push(l), |e| panic!("tail error: {e}"))
            .unwrap();
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(format_line(&line("second")).as_bytes()).unwrap();
        drop(f);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let tail = handle.stop().expect("the thread ended cleanly");
        assert_eq!(*got.lock().unwrap(), vec![line("first"), line("second")]);
        assert_eq!(tail.position().offset, fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn a_line_over_the_limit_is_dropped_whole_however_it_arrives() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut tail = Tail::new(path.clone());
        let mut f = File::create(&path).unwrap();
        f.write_all(format_line(&line("before")).as_bytes()).unwrap();
        // the start of a line that never ends, in pieces bigger than the limit
        f.write_all(b"action\t1\t").unwrap();
        f.write_all(&vec![b'x'; MAX_LINE_BYTES + 10]).unwrap();
        f.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line("before")]);
        assert!(tail.pending.is_empty(), "the reader does not keep what it is skipping");
        f.write_all(&vec![b'y'; MAX_LINE_BYTES]).unwrap();
        f.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec![]);
        assert!(tail.pending.is_empty());
        // its newline ends the skipping; the next line is read normally
        f.write_all(b"tail of the long line\n").unwrap();
        f.write_all(format_line(&line("after")).as_bytes()).unwrap();
        f.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line("after")]);

        // a whole line over the limit that arrives in one poll is dropped too
        let mut huge = b"action\t1\t".to_vec();
        huge.extend(vec![b'z'; MAX_LINE_BYTES]);
        huge.push(b'\n');
        f.write_all(&huge).unwrap();
        f.write_all(format_line(&line("five")).as_bytes()).unwrap();
        f.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line("five")]);
    }

    #[test]
    fn a_poll_reads_a_bounded_amount_and_catches_up_on_the_next() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut text = String::new();
        let mut n = 0u64;
        while text.len() < (MAX_READ_PER_POLL as usize) * 2 + 1000 {
            n += 1;
            text.push_str(&format_line(&line(&"p".repeat(1000))));
        }
        fs::write(&path, &text).unwrap();
        let mut tail = Tail::new(path);
        let first = tail.poll().unwrap();
        assert!(!first.is_empty() && (first.len() as u64) < n, "the first poll stopped at its budget");
        let mut total = first.len() as u64;
        loop {
            let more = tail.poll().unwrap();
            if more.is_empty() {
                break;
            }
            total += more.len() as u64;
        }
        assert_eq!(total, n, "and every line arrived exactly once");
    }

    #[test]
    fn garbage_does_not_stop_the_reading() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\xff\xfe not text \x00\x01\n");
        bytes.extend_from_slice(format_line(&line("seven")).as_bytes());
        fs::write(&path, &bytes).unwrap();
        assert_eq!(Tail::new(path.clone()).poll().unwrap(), vec![line("seven")]);
    }

    #[test]
    fn session_poll_reads_from_game() {
        let root = TempDir::new().unwrap();
        let mut s = Session::create_under(root.path(), "player").unwrap();
        assert_eq!(s.poll().unwrap(), vec![]);
        fs::write(s.dir().join(FROM_GAME), format_line(&line("hello"))).unwrap();
        assert_eq!(s.poll().unwrap(), vec![line("hello")]);
        assert_eq!(s.into_tail().position().offset, format_line(&line("hello")).len() as u64);
    }

    #[test]
    fn a_session_opened_in_a_used_folder_names_itself_and_never_sees_the_earlier_sessions_lines() {
        // quit to the menu, load another save, host again: the live folder keeps both link files
        let root = TempDir::new().unwrap();
        let mut first = Session::open(root.path()).unwrap();
        first.write_identity(0, "Aziz", "host").unwrap();
        first.send("session", "0", "{started=true}").unwrap();
        first.send("action", "1", "{seq=1}").unwrap();
        let end = first.end();
        fs::write(root.path().join(FROM_GAME), format_line(&line("hello")) + &format_line(&line("old action")))
            .unwrap();
        assert_eq!(first.poll().unwrap().len(), 2);
        let first_id = first.session_id().to_owned();
        drop(first);

        let mut second = Session::open(root.path()).unwrap();
        assert_ne!(second.session_id(), first_id, "every session has an id of its own");
        assert_eq!(second.start(), end, "the mod starts reading where the earlier session's lines end");
        second.write_identity(0, "Aziz", "host").unwrap();
        let identity = fs::read_to_string(root.path().join(IDENTITY)).unwrap();
        assert!(identity.starts_with(&format!("session={}\nstart={end}\n", second.session_id())), "{identity}");
        assert!(
            identity.ends_with("role=host\n"),
            "the role comes last: a racing read sees a complete file or no role"
        );
        assert_eq!(second.poll().unwrap(), vec![], "the earlier session's lines from the game are not read again");
        let mut f = OpenOptions::new().append(true).open(root.path().join(FROM_GAME)).unwrap();
        f.write_all(format_line(&line("hello again")).as_bytes()).unwrap();
        assert_eq!(second.poll().unwrap(), vec![line("hello again")]);
    }
}
