//! `feverlink3 selftest`: a host and two players over loopback, each with a fake game that talks through the real
//! file link. No game is started. The same function runs as the integration test `tests/selftest.rs`.
//!
//! What it proves, end to end through sockets, threads, sealing and the file link:
//!
//! 1. both players are welcomed with distinct ids and the host sees all three players;
//! 2. the host's session state reaches the files of all three games;
//! 3. an action written by player 1's game arrives in the host's and player 2's `to_game.log` with `from` set to
//!    player 1, even when the action claims another sender, and does not come back to player 1;
//! 4. a player with the wrong password is rejected (and a raw socket with a wrong proof gets `bad-proof`, then
//!    EOF);
//! 5. a 2 MiB garbage line on a raw socket closes that connection and nothing else.

use std::fs;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::protocol::{Frame, FrameCodec, FrameHeader};
use feverlink3_core::session::{
    codec, ChallengeMessage, HostOptions, HostSession, JoinMessage, MessageKind, RejectMessage,
};

use crate::gamelink::{self, Line, Tail};
use crate::session::{run_host, run_player, Command, HostConfig, PlayerConfig, SessionHandle};

/// How long any single wait may take before the self-test fails.
const WAIT: Duration = Duration::from_secs(5);

/// What passed, in order.
#[derive(Debug, Default)]
pub struct Report {
    pub checks: Vec<String>,
    pub elapsed: Duration,
}

fn ensure(report: &mut Report, ok: bool, what: &str) -> Result<(), String> {
    if ok {
        report.checks.push(what.to_owned());
        Ok(())
    } else {
        Err(format!("FAILED: {what}"))
    }
}

/// A stand-in for the mod: appends lines to `from_game.log` and reads `to_game.log` the way the mod does.
struct FakeGame {
    dir: PathBuf,
    id: String,
    next_seq: u64,
    tail: Tail,
    seen: Vec<Line>,
}

impl FakeGame {
    fn new(dir: &Path, id: &str) -> FakeGame {
        FakeGame {
            dir: dir.to_path_buf(),
            id: id.to_owned(),
            next_seq: 1,
            tail: Tail::new(dir.join(gamelink::TO_GAME)),
            seen: Vec::new(),
        }
    }

    /// Appends one line in the format the mod writes.
    fn write(&mut self, kind: &str, payload: &str) -> io::Result<()> {
        let line = Line { seq: self.next_seq, kind: kind.into(), from: self.id.clone(), payload: payload.into() };
        self.next_seq += 1;
        let mut f = fs::OpenOptions::new().create(true).append(true).open(self.dir.join(gamelink::FROM_GAME))?;
        f.write_all(gamelink::format_line(&line).as_bytes())
    }

    fn pump(&mut self) {
        if let Ok(lines) = self.tail.poll() {
            self.seen.extend(lines);
        }
    }

    /// Waits for a line of `kind` whose payload contains `needle`.
    fn wait_for(&mut self, kind: &str, needle: &str, timeout: Duration) -> Option<Line> {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            if let Some(l) = self.seen.iter().find(|l| l.kind == kind && l.payload.contains(needle)) {
                return Some(l.clone());
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn has(&mut self, kind: &str, needle: &str) -> bool {
        self.pump();
        self.seen.iter().any(|l| l.kind == kind && l.payload.contains(needle))
    }
}

/// Removes the scratch folder when the test ends, however it ends.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Reads frames from a raw socket until it is closed. `Ok(true)` when the peer closed (EOF or reset),
/// `Ok(false)` when it was still open after the timeout.
fn closed_within(stream: &mut TcpStream, timeout: Duration) -> bool {
    let _ = stream.set_read_timeout(Some(timeout));
    loop {
        match FrameCodec::read(stream) {
            Ok(Some(_)) => {}
            Ok(None) => return true,
            Err(feverlink3_core::protocol::ProtocolError::Io(e))
                if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                return false
            }
            Err(_) => return true,
        }
    }
}

fn raw_challenge(addr: SocketAddr) -> Result<(TcpStream, ChallengeMessage), String> {
    let mut stream = TcpStream::connect_timeout(&addr, WAIT).map_err(|e| format!("raw connect: {e}"))?;
    stream.set_read_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
    let frame = FrameCodec::read(&mut stream)
        .map_err(|e| format!("raw read of the challenge: {e}"))?
        .ok_or("the host closed before sending a challenge")?;
    let challenge = codec::try_decode::<ChallengeMessage>(&frame.body).ok_or("the challenge does not decode")?;
    Ok((stream, challenge))
}

/// Runs the whole self-test. `Err` names the first failed check.
pub fn run() -> Result<Report, String> {
    let started = Instant::now();
    let mut report = Report::default();
    let scratch = Scratch(std::env::temp_dir().join(format!("feverlink3-selftest-{:08x}", rand::random::<u32>())));

    // both keys are derived at once: the password stretching is the slowest thing here
    let secret = SessionKey::new_secret();
    let (key, wrong_key) = thread::scope(|s| {
        let a = s.spawn(|| SessionKey::derive(&secret, Some("correct horse")));
        let b = s.spawn(|| SessionKey::derive(&secret, Some("wrong horse")));
        (a.join().expect("derive"), b.join().expect("derive"))
    });

    let io = |e: io::Error| e.to_string();
    let host_link = gamelink::Session::create_under(&scratch.0, "host").map_err(io)?;
    let host_dir = host_link.dir().to_path_buf();
    let host = run_host(HostConfig {
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        name: "Host".into(),
        key: key.clone(),
        link: host_link,
        options: HostOptions::default(),
        echo_log: false,
    })
    .map_err(io)?;
    let addr = host.local_addr().ok_or("the host has no address")?;
    let mut host_game = FakeGame::new(&host_dir, "0");

    let mut players: Vec<(SessionHandle, FakeGame)> = Vec::new();
    for name in ["Alice", "Bob"] {
        let link = gamelink::Session::create_under(&scratch.0, name).map_err(io)?;
        let dir = link.dir().to_path_buf();
        let handle = run_player(PlayerConfig::new(vec![addr], name, key.clone(), link)).map_err(io)?;
        players.push((handle, FakeGame::new(&dir, "")));
    }

    // 1. welcomed
    for (handle, _) in &players {
        let ok = handle.wait_for(WAIT, |s| s.connected && s.my_id.is_some());
        ensure(&mut report, ok, "player is welcomed")?;
    }
    let ids: Vec<u16> = players.iter().map(|(h, _)| h.status().my_id.unwrap_or(0)).collect();
    ensure(&mut report, ids[0] != ids[1] && ids.iter().all(|&i| i != 0), "players got distinct ids, not the host's")?;
    ensure(&mut report, host.wait_for(WAIT, |s| s.players.len() == 3), "host sees itself and both players")?;
    for ((_, game), id) in players.iter_mut().zip(&ids) {
        game.id = id.to_string();
        let ok = game.wait_for("welcome", "\"Host\"", WAIT).is_some();
        ensure(&mut report, ok, "the welcome reached the player's game file")?;
    }
    for (_, game) in &players {
        let identity = fs::read_to_string(game.dir.join(gamelink::IDENTITY)).map_err(io)?;
        ensure(&mut report, identity.contains("role=player"), "identity.txt written for the player")?;
    }

    // 2. session state reaches every game
    host_game.write("hello", "{[\"time\"]=0}").map_err(io)?;
    for (_, game) in &mut players {
        game.write("hello", "{[\"time\"]=0}").map_err(io)?;
    }
    host.command(Command::Start);
    ensure(
        &mut report,
        host_game.wait_for("session", "[\"started\"]=true", WAIT).is_some(),
        "session state reached the host's game",
    )?;
    for (_, game) in &mut players {
        let ok = game.wait_for("session", "[\"started\"]=true", WAIT).is_some();
        ensure(&mut report, ok, "session state reached a player's game")?;
    }
    ensure(&mut report, host.status().session.is_some_and(|s| s.started), "host status shows the session started")?;

    // 3. an action goes to everyone else with the sender filled in
    let claim_other =
        format!("{{[\"body\"]=\"{{[\\\"fn\\\"]=\\\"x\\\"}}\",[\"from\"]={},[\"seq\"]=1,[\"stamp\"]=2400}}", ids[1]);
    players[0].1.write("action", &claim_other).map_err(io)?;
    let want = format!("[\"from\"]={}", ids[0]);
    let at_host = host_game.wait_for("action", &want, WAIT);
    ensure(
        &mut report,
        at_host.as_ref().is_some_and(|l| l.from == ids[0].to_string()),
        "the host's game got the action, from = player 1",
    )?;
    let at_bob = players[1].1.wait_for("action", &want, WAIT);
    ensure(
        &mut report,
        at_bob.as_ref().is_some_and(|l| l.from == ids[0].to_string()),
        "player 2's game got the action, from = player 1",
    )?;
    ensure(
        &mut report,
        at_bob.is_some_and(|l| l.payload.contains("[\"stamp\"]=2400") && l.payload.contains("\\\"fn\\\"")),
        "the action body and stamp arrive intact",
    )?;
    thread::sleep(Duration::from_millis(150));
    ensure(&mut report, !players[0].1.has("action", ""), "the action does not come back to its originator")?;
    ensure(&mut report, host.status().actions_relayed == 1, "host counted one relayed action")?;

    // 4. wrong password
    let wrong_link = gamelink::Session::create_under(&scratch.0, "Mallory").map_err(io)?;
    let mallory = run_player(PlayerConfig::new(vec![addr], "Mallory", wrong_key.clone(), wrong_link)).map_err(io)?;
    ensure(&mut report, mallory.wait_for(WAIT, |s| s.finished), "a wrong-password player's session ends")?;
    let status = mallory.status();
    ensure(
        &mut report,
        status.rejected.as_deref() == Some("bad-proof") && status.my_id.is_none(),
        "wrong password: rejected with bad-proof",
    )?;
    {
        let (mut raw, challenge) = raw_challenge(addr)?;
        let proof = HostSession::proof(&wrong_key, &challenge.nonce, "Raw").to_vec();
        let join = JoinMessage { name: "Raw".into(), version: HostOptions::default().version, proof };
        let header =
            FrameHeader::new(FrameHeader::CURRENT_VERSION, MessageKind::Join.into(), HostSession::UNASSIGNED, 0);
        FrameCodec::write(&mut raw, &Frame::new(header, codec::encode(&join))).map_err(|e| e.to_string())?;
        let reply = FrameCodec::read(&mut raw).map_err(|e| e.to_string())?.ok_or("closed without a reject")?;
        let reject = codec::try_decode::<RejectMessage>(&reply.body);
        ensure(
            &mut report,
            reply.header.kind == u8::from(MessageKind::Reject) && reject.is_some_and(|r| r.reason == "bad-proof"),
            "raw socket with a wrong proof gets a bad-proof reject",
        )?;
        ensure(&mut report, closed_within(&mut raw, WAIT), "and the host then closes that connection")?;
    }
    ensure(&mut report, host.status().players.len() == 3, "the rejected players never entered the player list")?;

    // 5. garbage does not touch anyone else
    {
        let (mut raw, _challenge) = raw_challenge(addr)?;
        let mut garbage = vec![b'x'; 2 << 20];
        garbage.push(b'\n');
        // the host may close and reset before everything is written; that is the point
        let _ = raw.write_all(&garbage);
        ensure(&mut report, closed_within(&mut raw, WAIT), "a 2 MiB garbage line closes that connection")?;
    }
    ensure(&mut report, host.status().players.len() == 3, "the other players are still listed")?;
    ensure(&mut report, players.iter().all(|(h, _)| h.status().connected), "the other players are still connected")?;
    players[1].1.write("action", "{[\"body\"]=\"after\",[\"seq\"]=2,[\"stamp\"]=2800}").map_err(io)?;
    let from_bob = "\"after\"";
    ensure(
        &mut report,
        players[0].1.wait_for("action", from_bob, WAIT).is_some(),
        "an action still relays after the garbage",
    )?;
    ensure(&mut report, host_game.wait_for("action", from_bob, WAIT).is_some(), "and reaches the host's game too")?;

    for (handle, _) in &players {
        handle.stop();
    }
    mallory.stop();
    host.stop();
    report.elapsed = started.elapsed();
    Ok(report)
}
