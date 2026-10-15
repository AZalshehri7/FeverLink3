//! The file link with one game instance.
//!
//! The launcher and the game talk through files in a per-session folder (`docs/ARCHITECTURE.md` section 2): no
//! process injection, and either side may restart. The folder is `%TEMP%\feverlink3\<name>-<time>-<rand>\` and
//! holds:
//!
//! - `to_game.log`: written by the launcher, read by the mod. Append-only.
//! - `from_game.log`: written by the mod, read by the launcher. Append-only.
//! - `identity.txt`: who this game is (`name=`, `role=`, `id=` lines), written before the game starts.
//! - `native.log`: the native module's log (written by the module, not here).
//!
//! Every line of the two logs is `seq<TAB>kind<TAB>from<TAB>payload\n`, with `seq` strictly increasing within
//! one file and the payload on one line, verbatim (the mod writes Lua literal syntax that is already one line, and
//! its reader does not unescape; see [`format_line`]). A reader keeps the
//! last `seq` it dispatched and ignores anything at or below it, so a truncated or rewritten file never replays
//! history. A writer that restarts has to continue from the highest `seq` already in the file, which is why
//! [`Session::open`] reads it back.
//!
//! Generations (every link log, here and in the native module and the mod: `docs/ARCHITECTURE.md` section 2). A log
//! never grows without bound and is never truncated: it is a series of generation files, `to_game.log` (generation 1),
//! `to_game.2.log`, `to_game.3.log`, ..., and `to_game.gen` names the newest. A writer that has put [`ROTATE_BYTES`]
//! into its generation ends it with a `rotate` line (`seq` 0, so no reader dispatches it) naming the next one, writes
//! the pointer, deletes the generation [`KEEP_GENERATIONS`] back and goes on in the next file. A reader that reads the
//! `rotate` line has read the whole generation (its writer adds nothing after it) and goes on at the start of the next:
//! no line is lost or split by a rotation, and following one costs a reader no file access at all until it happens.
//! The `seq` numbering goes on across generations.

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

/// Largest `seq` a line may carry. The mod writes the number as a Lua double, which holds integers exactly up to
/// 2^53; a line beyond that is garbage, and accepting it would end the numbering for every line after it.
pub const MAX_SEQ: u64 = 1 << 53;

/// One line of either log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub seq: u64,
    pub kind: String,
    pub from: String,
    pub payload: String,
}

/// Formats a line, including the trailing newline. The payload travels verbatim: the mod's reader does not
/// unescape anything and its serialiser already keeps a payload on one line (control characters inside strings
/// are written as `\n`, `\t`, `\ddd`), so an escaping layer here would corrupt every string. `kind` and `from`
/// must not contain tabs or line breaks; any found are replaced by spaces, and so are line breaks in the payload
/// (a guard against a caller bug, they cannot occur in serialiser output) rather than corrupting the file.
pub fn format_line(line: &Line) -> String {
    let payload: String = line.payload.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    format!("{}\t{}\t{}\t{}\n", line.seq, clean_field(&line.kind), clean_field(&line.from), payload)
}

fn clean_field(s: &str) -> String {
    s.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c }).collect()
}

/// Parses one line (without or with its trailing newline). `None` for anything malformed: a line with fewer
/// than four fields, or a `seq` that is not a number up to [`MAX_SEQ`].
pub fn parse_line(text: &str) -> Option<Line> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    let mut parts = text.splitn(4, '\t');
    let seq = parts.next()?.trim().parse().ok().filter(|seq| *seq <= MAX_SEQ)?;
    let kind = parts.next()?.to_string();
    let from = parts.next()?.to_string();
    let payload = parts.next()?.to_string();
    Some(Line { seq, kind, from, payload })
}

// ---------------------------------------------------------------------------------------------------------------
// Generations
// ---------------------------------------------------------------------------------------------------------------

/// A writer starts a new generation once its current one holds this much. The mod uses the same value
/// (`C.ROTATE_BYTES`); a reader does not need to know it.
pub const ROTATE_BYTES: u64 = 4 << 20;

/// Generation files a writer keeps (the newest included); older ones are deleted when a new one starts.
pub const KEEP_GENERATIONS: u64 = 4;

/// The kind of the line that ends a generation. Its `seq` is 0 and its payload `{["gen"]=<next>}`.
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

/// The text of the line that ends a generation and names the next.
pub fn rotate_line(from: &str, next: u64) -> String {
    format_line(&Line { seq: 0, kind: ROTATE_KIND.into(), from: from.into(), payload: format!("{{[\"gen\"]={next}}}") })
}

/// The generation a `rotate` line names, `None` for every other line.
pub fn rotate_target(line: &Line) -> Option<u64> {
    if line.seq != 0 || line.kind != ROTATE_KIND {
        return None;
    }
    let digits: String = line
        .payload
        .split('=')
        .nth(1)?
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok().filter(|g| *g >= 2)
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
    /// Lines written again at the top of every new generation (they keep their `seq`, so a reader that came
    /// through the earlier generations drops them; one that starts in the new one reads them as history).
    header: String,
}

impl GenWriter {
    /// Continues the newest generation of `base` (the pointer), at its current size. `from` is the `from` column of
    /// the `rotate` lines.
    pub fn open(base: PathBuf, from: &str) -> io::Result<GenWriter> {
        let gen = newest_generation(&base);
        let size = match fs::metadata(generation_path(&base, gen)) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        Ok(GenWriter {
            base,
            from: from.to_string(),
            gen,
            size,
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

    /// Ends the current generation and starts the next. Only the `rotate` line has to be written: without the pointer a
    /// reader that follows still finds the next file (a fresh one starts one generation early and follows); an old
    /// file that cannot be deleted (a reader holds it open) stays until the folder is cleaned.
    fn rotate(&mut self) -> io::Result<()> {
        let next = self.gen + 1;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path())?
            .write_all(rotate_line(&self.from, next).as_bytes())?;
        let _ = fs::write(pointer_path(&self.base), format!("{next}\n"));
        if next > self.keep {
            let _ = fs::remove_file(generation_path(&self.base, next - self.keep));
        }
        self.gen = next;
        self.size = 0;
        if !self.header.is_empty() {
            let header = std::mem::take(&mut self.header);
            let written = OpenOptions::new().create(true).append(true).open(self.path())?.write_all(header.as_bytes());
            self.size = if written.is_ok() { header.len() as u64 } else { 0 };
            self.header = header;
        }
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
#[derive(Debug)]
pub struct Session {
    dir: PathBuf,
    next_seq: u64,
    to_game: GenWriter,
    tail: Tail,
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

    /// Opens the link in an existing folder. `to_game.log` is read back so numbering continues after the
    /// highest `seq` already written; `from_game.log` is tailed from its start (its oldest generation).
    pub fn open(dir: &Path) -> io::Result<Session> {
        fs::create_dir_all(dir)?;
        let to_game = dir.join(TO_GAME);
        let last = highest_seq(&to_game)?;
        Ok(Session {
            dir: dir.to_path_buf(),
            next_seq: last.saturating_add(1),
            to_game: GenWriter::open(to_game, "0")?,
            tail: Tail::new(dir.join(FROM_GAME)),
        })
    }

    /// The session folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The `seq` the next [`send`](Self::send) will use.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Writes `identity.txt`: who this game instance is. The mod reads it when it loads.
    pub fn write_identity(&self, player_id: u16, name: &str, role: &str) -> io::Result<()> {
        let text = format!("id={}\nname={}\nrole={}\n", player_id, clean_field(name), clean_field(role));
        fs::write(self.dir.join(IDENTITY), text)
    }

    /// Appends one line to `to_game.log` and returns its `seq`.
    pub fn send(&mut self, kind: &str, from: &str, payload: &str) -> io::Result<u64> {
        let seq = self.next_seq;
        let line = Line { seq, kind: kind.to_string(), from: from.to_string(), payload: payload.to_string() };
        self.to_game.append(&format_line(&line))?;
        self.next_seq = seq.saturating_add(1);
        Ok(seq)
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

/// The highest `seq` in a log, 0 when it is missing or holds no valid line: its newest generation is read (the one
/// before it too when the newest has no line yet), the way a [`Tail`] reads, so invalid text or an enormous line is
/// no problem.
pub fn highest_seq(path: &Path) -> io::Result<u64> {
    let newest = newest_generation(path);
    let mut tail = Tail::at_generation(path.to_path_buf(), newest.saturating_sub(1).max(1));
    loop {
        let before = (tail.gen, tail.offset);
        tail.poll()?;
        if (tail.gen, tail.offset) == before {
            return Ok(tail.last_seq);
        }
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

/// Reads new lines from an append-only log, tolerating a missing file, a partial last line, truncation and a
/// rewrite, and following the log's generations. Lines whose `seq` is not above the last dispatched one are
/// dropped, never dispatched twice.
///
/// A rewrite is recognised by content, not only by size: the tail remembers the first bytes of the file and the
/// last bytes it read, and a file that no longer holds them at the same places was replaced, even when it is not
/// shorter than what was read (truncated and grown again between two polls). Reading then restarts at the top, and
/// the `seq` filter keeps history out, so no new line is lost and none is read from the middle.
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
    /// Highest `seq` handed out so far.
    last_seq: u64,
    /// Bytes read past `offset` that do not yet end with a newline.
    pending: Vec<u8>,
    /// The line being read is over [`MAX_LINE_BYTES`]: what is left of it is dropped, up to its newline.
    skipping: bool,
    /// The first bytes of the file as read (up to [`SIGNATURE_BYTES`]).
    head: Vec<u8>,
    /// The last bytes read, which end at `offset` (up to [`SIGNATURE_BYTES`]).
    mark: Vec<u8>,
}

impl Tail {
    /// A tail of `path` from its start (its oldest generation still there), with nothing dispatched yet.
    pub fn new(path: PathBuf) -> Tail {
        let newest = newest_generation(&path);
        let mut gen = newest;
        while gen > 1 && generation_path(&path, gen - 1).exists() {
            gen -= 1;
        }
        Tail::at_generation(path, gen)
    }

    /// A tail from the start of generation `gen`.
    pub fn at_generation(path: PathBuf, gen: u64) -> Tail {
        Tail {
            path,
            gen: gen.max(1),
            offset: 0,
            last_seq: 0,
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

    /// Highest `seq` returned so far.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Reads whatever was appended since the last call and returns the complete, new lines in file order, through
    /// the generations the log went on to. A file that is not the one read before (shorter, or other bytes where
    /// the remembered ones were) was rewritten: reading restarts at its beginning, and the `seq` filter keeps old
    /// lines from coming back.
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
                    // the writer's last line in this file: everything before it was read, the rest is in `next`
                    self.rewind();
                    self.gen = next;
                    return Ok(true);
                }
                if parsed.seq > self.last_seq {
                    self.last_seq = parsed.seq;
                    out.push(parsed);
                }
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
        Ok(holds(file, 0, &self.head)? && holds(file, self.offset - self.mark.len() as u64, &self.mark)?)
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
    /// Stops the thread and returns the tail, whose `last_seq` tells what was dispatched. `None` if the thread
    /// ended in a panic of its own (a callback's, say).
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

    fn line(seq: u64, payload: &str) -> Line {
        Line { seq, kind: "action".into(), from: "1".into(), payload: payload.into() }
    }

    #[test]
    fn line_round_trip_keeps_the_payload_verbatim() {
        let l = Line { seq: 7, kind: "chat".into(), from: "3".into(), payload: r#"{["t"]="a\"b\\n\009"}"#.into() };
        let text = format_line(&l);
        assert_eq!(text.matches('\n').count(), 1, "payload stays on one line");
        assert!(text.contains(r#"{["t"]="a\"b\\n\009"}"#), "no escaping layer");
        assert_eq!(parse_line(&text), Some(l));
        let raw = Line { seq: 8, kind: "x".into(), from: "0".into(), payload: "a\nb\rc".into() };
        assert_eq!(parse_line(&format_line(&raw)).unwrap().payload, "a b c", "stray line breaks cannot split a line");
        assert_eq!(
            parse_line("12\thello\thost\t\n"),
            Some(Line { seq: 12, kind: "hello".into(), from: "host".into(), payload: String::new() })
        );
        assert_eq!(parse_line("3\tclock\t0\tpayload\twith\ttabs\r\n").unwrap().payload, "payload\twith\ttabs");
    }

    #[test]
    fn malformed_lines_are_rejected() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("x\tkind\tfrom\tpayload"), None);
        assert_eq!(parse_line("1\tkind\tfrom"), None);
    }

    #[test]
    fn fields_with_tabs_are_cleaned_not_corrupting() {
        let l = Line { seq: 1, kind: "a\tb".into(), from: "c\nd".into(), payload: "p".into() };
        assert_eq!(parse_line(&format_line(&l)).unwrap().kind, "a b");
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
    fn session_writes_identity_and_numbered_lines_and_continues_after_reopen() {
        let root = TempDir::new().unwrap();
        let mut s = Session::create_under(root.path(), "host").unwrap();
        assert!(s.dir().starts_with(root.path()));
        s.write_identity(0, "Aziz", "host").unwrap();
        assert_eq!(fs::read_to_string(s.dir().join(IDENTITY)).unwrap(), "id=0\nname=Aziz\nrole=host\n");

        assert_eq!(s.send("session", "host", "{\"started\":false}").unwrap(), 1);
        assert_eq!(s.send("action", "2", "line two").unwrap(), 2);
        let text = fs::read_to_string(s.dir().join(TO_GAME)).unwrap();
        assert_eq!(text, "1\tsession\thost\t{\"started\":false}\n2\taction\t2\tline two\n");

        let dir = s.dir().to_path_buf();
        drop(s);
        let mut again = Session::open(&dir).unwrap();
        assert_eq!(again.next_seq(), 3);
        assert_eq!(again.send("x", "host", "").unwrap(), 3);
        assert_eq!(highest_seq(&dir.join(TO_GAME)).unwrap(), 3);
        assert_eq!(highest_seq(&dir.join("missing.log")).unwrap(), 0);
    }

    #[test]
    fn tail_returns_new_lines_once_and_waits_for_partial_lines() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut tail = Tail::new(path.clone());
        assert_eq!(tail.poll().unwrap(), vec![], "missing file is not an error");

        let mut f = File::create(&path).unwrap();
        f.write_all(format_line(&line(1, "one")).as_bytes()).unwrap();
        f.write_all(b"2\taction\t1\tpart").unwrap();
        f.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line(1, "one")]);
        assert_eq!(tail.poll().unwrap(), vec![], "nothing new, partial line held back");

        f.write_all(b"ial\n").unwrap();
        f.write_all(format_line(&line(3, "three")).as_bytes()).unwrap();
        f.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line(2, "partial"), line(3, "three")]);
        assert_eq!(tail.last_seq(), 3);
        assert_eq!(tail.poll().unwrap(), vec![]);
    }

    #[test]
    fn tail_never_redispatches_after_truncation_or_rewrite() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut tail = Tail::new(path.clone());
        let mut text = String::new();
        for i in 1..=5 {
            text.push_str(&format_line(&line(i, "x")));
        }
        fs::write(&path, &text).unwrap();
        assert_eq!(tail.poll().unwrap().len(), 5);

        // truncated to nothing, then the writer continues with new numbers
        fs::write(&path, "").unwrap();
        assert_eq!(tail.poll().unwrap(), vec![]);
        fs::write(&path, format_line(&line(6, "six"))).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line(6, "six")]);

        // rewritten with old history first: only the new line comes out
        let mut rewritten = String::new();
        for i in 1..=6 {
            rewritten.push_str(&format_line(&line(i, "x")));
        }
        rewritten.push_str(&format_line(&line(7, "seven")));
        fs::write(&path, &rewritten).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line(7, "seven")]);

        // rewritten shorter with only old numbers: nothing at all
        fs::write(&path, format_line(&line(2, "old"))).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![]);

        // file deleted and recreated
        fs::remove_file(&path).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![]);
        fs::write(&path, format_line(&line(8, "eight"))).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line(8, "eight")]);
        assert_eq!(tail.last_seq(), 8);
    }

    #[test]
    fn tail_sees_a_truncate_and_regrow_within_one_poll() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut tail = Tail::new(path.clone());
        let mut text = String::new();
        for i in 1..=3 {
            text.push_str(&format_line(&line(i, "before")));
        }
        fs::write(&path, &text).unwrap();
        assert_eq!(tail.poll().unwrap().len(), 3);
        // between two polls the file is truncated and grows past the old length again with other lines: the old
        // reader went on at its offset, in the middle of a line, and lost the lines before it
        let mut regrown = String::new();
        for i in 4..=9 {
            regrown.push_str(&format_line(&line(i, "after the truncation")));
        }
        assert!(regrown.len() as u64 > text.len() as u64);
        fs::write(&path, &regrown).unwrap();
        let got = tail.poll().unwrap();
        assert_eq!(got.iter().map(|l| l.seq).collect::<Vec<_>>(), [4, 5, 6, 7, 8, 9]);
        assert!(got.iter().all(|l| l.payload == "after the truncation"), "no line read from its middle");
        // the same once more with the same first lines (a writer that starts over the same way): the bytes before the
        // offset differ
        let mut same_start = String::new();
        for i in 4..=6 {
            same_start.push_str(&format_line(&line(i, "after the truncation")));
        }
        assert!(same_start.len() > SIGNATURE_BYTES, "the first bytes are the same");
        for i in 10..=16 {
            same_start.push_str(&format_line(&line(i, "and again")));
        }
        fs::write(&path, &same_start).unwrap();
        assert_eq!(tail.poll().unwrap().iter().map(|l| l.seq).collect::<Vec<_>>(), (10..=16).collect::<Vec<_>>());
        // and a plain append is read on from the offset as before
        OpenOptions::new().append(true).open(&path).unwrap().write_all(format_line(&line(17, "x")).as_bytes()).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line(17, "x")]);
    }

    #[test]
    fn to_game_rotates_and_a_tail_reads_every_line_once_across_generations() {
        let root = TempDir::new().unwrap();
        let mut s = Session::create_under(root.path(), "host").unwrap().with_rotation(300, 3);
        let base = s.dir().join(TO_GAME);
        let mut tail = Tail::new(base.clone());
        let mut got = Vec::new();
        for i in 1..=100u64 {
            assert_eq!(s.send("action", "2", &format!("payload {i}")).unwrap(), i);
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
        assert_eq!(got.iter().map(|l| l.seq).collect::<Vec<_>>(), (1..=100).collect::<Vec<_>>());
        assert!(got.iter().enumerate().all(|(i, l)| l.payload == format!("payload {}", i + 1)), "no line split");
        let newest = newest_generation(&base);
        assert!(newest > 3, "it rotated ({newest})");
        assert!(!base.exists() && generation_path(&base, newest - 2).exists(), "three generations are kept");
        // a session reopened on the folder continues the numbering of the newest generation
        let dir = s.dir().to_path_buf();
        drop(s);
        assert_eq!(highest_seq(&base).unwrap(), 100);
        assert_eq!(Session::open(&dir).unwrap().next_seq(), 101);
        // a tail started now begins at the oldest generation still there, and reads on through the newest
        let mut late = Tail::new(base.clone());
        assert_eq!(late.generation(), newest - 2);
        let mut seqs = Vec::new();
        loop {
            let more = late.poll().unwrap();
            if more.is_empty() {
                break;
            }
            seqs.extend(more.into_iter().map(|l| l.seq));
        }
        assert_eq!(seqs.last(), Some(&100));
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
        // the heartbeat's look follows the newest file
        assert_eq!(newest_mark(&base).map(|m| m.0), Some(newest));
    }

    #[test]
    fn rotate_lines_name_the_next_generation() {
        let l = parse_line(&rotate_line("native", 7)).unwrap();
        assert_eq!((l.seq, l.kind.as_str(), rotate_target(&l)), (0, ROTATE_KIND, Some(7)));
        assert_eq!(rotate_target(&line(0, "x")), None);
        assert_eq!(
            rotate_target(&Line {
                seq: 3,
                kind: ROTATE_KIND.into(),
                from: "0".into(),
                payload: "{[\"gen\"]=2}".into()
            }),
            None
        );
        let base = Path::new("dir").join("native_ctl.sim.log");
        assert_eq!(generation_path(&base, 1), base);
        assert_eq!(generation_path(&base, 12), Path::new("dir").join("native_ctl.sim.12.log"));
        assert_eq!(pointer_path(&base), Path::new("dir").join("native_ctl.sim.gen"));
    }

    #[test]
    fn tail_skips_garbage_and_out_of_order_lines() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut tail = Tail::new(path.clone());
        let mut text = String::new();
        text.push_str("garbage line\n");
        text.push_str(&format_line(&line(2, "two")));
        text.push_str(&format_line(&line(1, "one, late")));
        text.push_str(&format_line(&line(2, "two again")));
        text.push_str(&format_line(&line(3, "three")));
        fs::write(&path, &text).unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line(2, "two"), line(3, "three")]);
    }

    #[test]
    fn tail_thread_delivers_lines_and_stops() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        fs::write(&path, format_line(&line(1, "first"))).unwrap();
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&got);
        let handle = Tail::new(path.clone())
            .spawn(Duration::from_millis(10), move |l| sink.lock().unwrap().push(l), |e| panic!("tail error: {e}"))
            .unwrap();
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(format_line(&line(2, "second")).as_bytes()).unwrap();
        drop(f);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let tail = handle.stop().expect("the thread ended cleanly");
        assert_eq!(*got.lock().unwrap(), vec![line(1, "first"), line(2, "second")]);
        assert_eq!(tail.last_seq(), 2);
    }

    #[test]
    fn a_line_over_the_limit_is_dropped_whole_however_it_arrives() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut tail = Tail::new(path.clone());
        let mut f = File::create(&path).unwrap();
        f.write_all(format_line(&line(1, "before")).as_bytes()).unwrap();
        // the start of a line that never ends, in pieces bigger than the limit
        f.write_all(b"2\taction\t1\t").unwrap();
        f.write_all(&vec![b'x'; MAX_LINE_BYTES + 10]).unwrap();
        f.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line(1, "before")]);
        assert!(tail.pending.is_empty(), "the reader does not keep what it is skipping");
        f.write_all(&vec![b'y'; MAX_LINE_BYTES]).unwrap();
        f.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec![]);
        assert!(tail.pending.is_empty());
        // its newline ends the skipping; the next line is read normally
        f.write_all(b"tail of the long line\n").unwrap();
        f.write_all(format_line(&line(3, "after")).as_bytes()).unwrap();
        f.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line(3, "after")]);

        // a whole line over the limit that arrives in one poll is dropped too
        let mut huge = b"4\taction\t1\t".to_vec();
        huge.extend(vec![b'z'; MAX_LINE_BYTES]);
        huge.push(b'\n');
        f.write_all(&huge).unwrap();
        f.write_all(format_line(&line(5, "five")).as_bytes()).unwrap();
        f.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec![line(5, "five")]);
    }

    #[test]
    fn a_poll_reads_a_bounded_amount_and_catches_up_on_the_next() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut text = String::new();
        let mut seq = 0;
        while text.len() < (MAX_READ_PER_POLL as usize) * 2 + 1000 {
            seq += 1;
            text.push_str(&format_line(&line(seq, &"p".repeat(1000))));
        }
        fs::write(&path, &text).unwrap();
        let mut tail = Tail::new(path);
        let first = tail.poll().unwrap();
        assert!(!first.is_empty() && (first.len() as u64) < seq, "the first poll stopped at its budget");
        let mut total = first.len() as u64;
        loop {
            let more = tail.poll().unwrap();
            if more.is_empty() {
                break;
            }
            total += more.len() as u64;
        }
        assert_eq!((total, tail.last_seq()), (seq, seq), "and every line arrived exactly once");
    }

    #[test]
    fn garbage_and_huge_numbers_do_not_end_the_numbering() {
        let root = TempDir::new().unwrap();
        let path = root.path().join(FROM_GAME);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\xff\xfe not text \x00\x01\n");
        bytes.extend_from_slice(format!("{}\tx\t0\tbeyond what a Lua number holds\n", u64::MAX).as_bytes());
        bytes.extend_from_slice(format!("{}\tx\t0\tjust over\n", MAX_SEQ + 1).as_bytes());
        bytes.extend_from_slice(format_line(&line(7, "seven")).as_bytes());
        fs::write(&path, &bytes).unwrap();
        assert_eq!(Tail::new(path.clone()).poll().unwrap(), vec![line(7, "seven")]);
        assert_eq!(highest_seq(&path).unwrap(), 7, "also for a file whose text is not UTF-8");
        assert_eq!(parse_line(&format!("{MAX_SEQ}\tk\t0\tp")).map(|l| l.seq), Some(MAX_SEQ));
        // a log that already holds the largest number goes on after it
        fs::write(root.path().join(TO_GAME), format_line(&line(MAX_SEQ, "last"))).unwrap();
        let mut s = Session::open(root.path()).unwrap();
        assert_eq!(s.next_seq(), MAX_SEQ + 1);
        assert_eq!(s.send("x", "0", "p").unwrap(), MAX_SEQ + 1);
    }

    #[test]
    fn session_poll_reads_from_game() {
        let root = TempDir::new().unwrap();
        let mut s = Session::create_under(root.path(), "player").unwrap();
        assert_eq!(s.poll().unwrap(), vec![]);
        fs::write(s.dir().join(FROM_GAME), format_line(&line(1, "hello"))).unwrap();
        assert_eq!(s.poll().unwrap(), vec![line(1, "hello")]);
        assert_eq!(s.into_tail().last_seq(), 1);
    }
}
