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

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
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
/// than four fields, or a `seq` that is not a number.
pub fn parse_line(text: &str) -> Option<Line> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    let mut parts = text.splitn(4, '\t');
    let seq = parts.next()?.trim().parse().ok()?;
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
#[derive(Debug)]
pub struct Session {
    dir: PathBuf,
    next_seq: u64,
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
    /// highest `seq` already written; `from_game.log` is tailed from its start.
    pub fn open(dir: &Path) -> io::Result<Session> {
        fs::create_dir_all(dir)?;
        let to_game = dir.join(TO_GAME);
        let last = highest_seq(&to_game)?;
        Ok(Session { dir: dir.to_path_buf(), next_seq: last + 1, tail: Tail::new(dir.join(FROM_GAME)) })
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
        let mut file = OpenOptions::new().create(true).append(true).open(self.dir.join(TO_GAME))?;
        file.write_all(format_line(&line).as_bytes())?;
        file.flush()?;
        self.next_seq += 1;
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

/// The highest `seq` in a log file, 0 when the file is missing or holds no valid line.
pub fn highest_seq(path: &Path) -> io::Result<u64> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut max = 0;
    for line in BufReader::new(file).lines() {
        if let Some(l) = parse_line(&line?) {
            max = max.max(l.seq);
        }
    }
    Ok(max)
}

// ---------------------------------------------------------------------------------------------------------------
// Tail
// ---------------------------------------------------------------------------------------------------------------

/// Reads new lines from an append-only log, tolerating a missing file, a partial last line, truncation and a
/// rewrite. Lines whose `seq` is not above the last dispatched one are dropped, never dispatched twice.
#[derive(Debug)]
pub struct Tail {
    path: PathBuf,
    /// Bytes of the file already read (an incomplete last line is read, and kept in `pending`).
    offset: u64,
    /// Highest `seq` handed out so far.
    last_seq: u64,
    /// Bytes read past `offset` that do not yet end with a newline.
    pending: Vec<u8>,
}

impl Tail {
    /// A tail of `path` from its start, with nothing dispatched yet.
    pub fn new(path: PathBuf) -> Tail {
        Tail { path, offset: 0, last_seq: 0, pending: Vec::new() }
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
                self.offset = 0;
                self.pending.clear();
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        if len < self.offset {
            self.offset = 0;
            self.pending.clear();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut fresh = Vec::new();
        file.read_to_end(&mut fresh)?;
        if fresh.is_empty() {
            return Ok(Vec::new());
        }

        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(&fresh);
        let mut out = Vec::new();
        let mut consumed = 0;
        while let Some(nl) = buf[consumed..].iter().position(|&b| b == b'\n') {
            let end = consumed + nl + 1;
            let text = String::from_utf8_lossy(&buf[consumed..end]);
            if let Some(line) = parse_line(&text) {
                if line.seq > self.last_seq {
                    self.last_seq = line.seq;
                    out.push(line);
                }
            }
            consumed = end;
        }
        // `offset` counts every byte read from the file; an incomplete last line waits in `pending`
        self.offset += fresh.len() as u64;
        self.pending = buf[consumed..].to_vec();
        Ok(out)
    }

    /// Runs the tail on its own thread, calling `on_line` for every new line, polling every `interval`, until
    /// [`TailHandle::stop`] is called or the handle is dropped. Read errors are passed to `on_error` and the
    /// loop continues (the file may be mid-rewrite).
    pub fn spawn(
        mut self,
        interval: Duration,
        mut on_line: impl FnMut(Line) + Send + 'static,
        mut on_error: impl FnMut(io::Error) + Send + 'static,
    ) -> TailHandle {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("feverlink3-tail".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    match self.poll() {
                        Ok(lines) => lines.into_iter().for_each(&mut on_line),
                        Err(e) => on_error(e),
                    }
                    thread::sleep(interval);
                }
                self
            })
            .expect("spawn tail thread");
        TailHandle { stop, thread: Some(thread) }
    }
}

/// Controls a tail thread started with [`Tail::spawn`].
#[derive(Debug)]
pub struct TailHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Tail>>,
}

impl TailHandle {
    /// Stops the thread and returns the tail, whose `last_seq` tells what was dispatched.
    pub fn stop(mut self) -> Tail {
        self.stop.store(true, Ordering::Relaxed);
        let thread = self.thread.take().expect("tail thread joined once");
        thread.join().expect("tail thread panicked")
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
        let handle = Tail::new(path.clone()).spawn(
            Duration::from_millis(10),
            move |l| sink.lock().unwrap().push(l),
            |e| panic!("tail error: {e}"),
        );
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(format_line(&line(2, "second")).as_bytes()).unwrap();
        drop(f);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let tail = handle.stop();
        assert_eq!(*got.lock().unwrap(), vec![line(1, "first"), line(2, "second")]);
        assert_eq!(tail.last_seq(), 2);
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
