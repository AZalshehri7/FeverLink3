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
//! Every line of the two logs is `seq<TAB>kind<TAB>from<TAB>payload\n`, with `seq` strictly increasing within
//! one file and the payload on one line, verbatim (the mod writes Lua literal syntax that is already one line, and
//! its reader does not unescape; see [`format_line`]). A reader keeps the
//! last `seq` it dispatched and ignores anything at or below it, so a truncated or rewritten file never replays
//! history. A writer that restarts has to continue from the highest `seq` already in the file, which is why
//! [`Session::open`] reads it back.

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
/// session's id and the last `to_game.log` sequence before it (`start=`), which the mod skips to.
#[derive(Debug)]
pub struct Session {
    dir: PathBuf,
    next_seq: u64,
    tail: Tail,
    /// This session's id (16 hex digits, random), written to `identity.txt`.
    id: String,
    /// The highest `seq` of `to_game.log` when the session was opened: every line of this session is above it.
    start: u64,
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

    /// Opens the link in an existing folder for a new session. `to_game.log` is read back so numbering continues after
    /// the highest `seq` already written (that number is the session's `start`); `from_game.log` is tailed from where it
    /// ends now: what it holds was written by the game in an earlier session (the mod writes only while `identity.txt`
    /// names a session, which this one does not yet), and a host that read it would relay an earlier session's actions.
    pub fn open(dir: &Path) -> io::Result<Session> {
        fs::create_dir_all(dir)?;
        let to_game = dir.join(TO_GAME);
        let last = highest_seq(&to_game)?;
        let mut tail = Tail::new(dir.join(FROM_GAME));
        loop {
            let before = tail.offset;
            tail.poll()?;
            if tail.offset == before {
                break;
            }
        }
        Ok(Session {
            dir: dir.to_path_buf(),
            next_seq: last.saturating_add(1),
            tail,
            id: format!("{:016x}", rand::random::<u64>()),
            start: last,
        })
    }

    /// This session's id, as `identity.txt` names it.
    pub fn session_id(&self) -> &str {
        &self.id
    }

    /// The highest `seq` of `to_game.log` before this session: the mod dispatches only lines above it.
    pub fn start_seq(&self) -> u64 {
        self.start
    }

    /// The session folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The `seq` the next [`send`](Self::send) will use.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
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

    /// Appends one line to `to_game.log` and returns its `seq`.
    pub fn send(&mut self, kind: &str, from: &str, payload: &str) -> io::Result<u64> {
        let seq = self.next_seq;
        let line = Line { seq, kind: kind.to_string(), from: from.to_string(), payload: payload.to_string() };
        let mut file = OpenOptions::new().create(true).append(true).open(self.dir.join(TO_GAME))?;
        file.write_all(format_line(&line).as_bytes())?;
        file.flush()?;
        self.next_seq = seq.saturating_add(1);
        Ok(seq)
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

/// The highest `seq` in a log file, 0 when the file is missing or holds no valid line. The file is read the way a
/// [`Tail`] reads it, so invalid text or an enormous line is no problem.
pub fn highest_seq(path: &Path) -> io::Result<u64> {
    let mut tail = Tail::new(path.to_path_buf());
    loop {
        let before = tail.offset;
        tail.poll()?;
        if tail.offset == before {
            return Ok(tail.last_seq);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Tail
// ---------------------------------------------------------------------------------------------------------------

/// Reads new lines from an append-only log, tolerating a missing file, a partial last line, truncation and a
/// rewrite. Lines whose `seq` is not above the last dispatched one are dropped, never dispatched twice.
///
/// The file is written by the game and may hold anything: a poll reads at most 8 MiB, a line longer than
/// [`MAX_LINE_BYTES`] is dropped as a whole (up to its newline, however many polls that takes), and invalid UTF-8
/// is read as the replacement character. Memory stays at most the size of one line plus one poll.
#[derive(Debug)]
pub struct Tail {
    path: PathBuf,
    /// Bytes of the file already read (an incomplete last line is read, and kept in `pending`).
    offset: u64,
    /// Highest `seq` handed out so far.
    last_seq: u64,
    /// Bytes read past `offset` that do not yet end with a newline.
    pending: Vec<u8>,
    /// The line being read is over [`MAX_LINE_BYTES`]: what is left of it is dropped, up to its newline.
    skipping: bool,
}

impl Tail {
    /// A tail of `path` from its start, with nothing dispatched yet.
    pub fn new(path: PathBuf) -> Tail {
        Tail { path, offset: 0, last_seq: 0, pending: Vec::new(), skipping: false }
    }

    /// Back to the start of the file, forgetting what was read of an unfinished line.
    fn rewind(&mut self) {
        self.offset = 0;
        self.pending = Vec::new();
        self.skipping = false;
    }

    /// The file being tailed.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Highest `seq` returned so far.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Reads whatever was appended since the last call and returns the complete, new lines in file order.
    /// A file shorter than what was consumed means it was truncated or rewritten: reading restarts at its
    /// beginning, and the `seq` filter keeps old lines from coming back.
    pub fn poll(&mut self) -> io::Result<Vec<Line>> {
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // the file may be recreated later with fresh content: start over then
                self.rewind();
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        if len < self.offset {
            self.rewind();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut fresh = Vec::new();
        file.take(MAX_READ_PER_POLL).read_to_end(&mut fresh)?;
        if fresh.is_empty() {
            return Ok(Vec::new());
        }
        // `offset` counts every byte read from the file; an incomplete last line waits in `pending`
        self.offset = self.offset.saturating_add(fresh.len() as u64);

        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(&fresh);
        drop(fresh);
        let mut out = Vec::new();
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
        Ok(out)
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
        assert_eq!(s.session_id().len(), 16);
        assert_eq!(
            fs::read_to_string(s.dir().join(IDENTITY)).unwrap(),
            format!("session={}\nstart=0\nid=0\nname=Aziz\nrole=host\n", s.session_id())
        );

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

    #[test]
    fn a_session_opened_in_a_used_folder_names_itself_and_never_sees_the_earlier_sessions_lines() {
        // quit to the menu, load another save, host again: the live folder keeps both link files
        let root = TempDir::new().unwrap();
        let mut first = Session::open(root.path()).unwrap();
        first.write_identity(0, "Aziz", "host").unwrap();
        first.send("session", "0", "{started=true}").unwrap();
        first.send("action", "1", "{seq=1}").unwrap();
        fs::write(root.path().join(FROM_GAME), format_line(&line(1, "hello")) + &format_line(&line(2, "old action")))
            .unwrap();
        assert_eq!(first.poll().unwrap().len(), 2);
        let first_id = first.session_id().to_owned();
        drop(first);

        let mut second = Session::open(root.path()).unwrap();
        assert_ne!(second.session_id(), first_id, "every session has an id of its own");
        assert_eq!(second.start_seq(), 2, "the mod skips to the last line of the earlier session");
        second.write_identity(0, "Aziz", "host").unwrap();
        let identity = fs::read_to_string(root.path().join(IDENTITY)).unwrap();
        assert!(identity.starts_with(&format!("session={}\nstart=2\n", second.session_id())), "{identity}");
        assert!(
            identity.ends_with("role=host\n"),
            "the role comes last: a racing read sees a complete file or no role"
        );
        assert_eq!(second.poll().unwrap(), vec![], "the earlier session's lines from the game are not read again");
        let mut f = OpenOptions::new().append(true).open(root.path().join(FROM_GAME)).unwrap();
        f.write_all(format_line(&line(3, "hello again")).as_bytes()).unwrap();
        assert_eq!(second.poll().unwrap(), vec![line(3, "hello again")]);
        assert_eq!(second.send("session", "0", "{}").unwrap(), 3);
    }
}
