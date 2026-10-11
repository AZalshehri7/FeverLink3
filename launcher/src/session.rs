//! The session thread: the one place where the network, the game's files and the clock meet.
//!
//! [`run_host`] and [`run_player`] each start a thread that owns the protocol state machine
//! ([`HostSession`] / [`PlayerSession`], which do no I/O and return their outgoing events from every call) and
//! loops over three sources of input:
//!
//! 1. [`NetEvent`]s from the sockets ([`crate::net`]),
//! 2. new lines of the game's `from_game.log` ([`crate::gamelink`]), translated to protocol messages by the table
//!    in [`LINE_KINDS`],
//! 3. a 250 ms tick ([`TICK`]) that drives the periodic session broadcast and checksum rounds,
//!
//! and acts on every output the state machine returns: frames to connections, lines to `to_game.log`, log text,
//! closing connections. The loop wakes at least every 20 ms, so a stop request or a new game line waits at most
//! that long.
//!
//! The player side reconnects: when the connection drops it tells the game `session started=false`, then tries
//! every [`PlayerConfig::reconnect_interval`] (5 s) for [`PlayerConfig::reconnect_window`] (5 minutes). Each attempt
//! asks for the id and slot the host gave in the first welcome (the [`ResumeTicket`]), so a player that is back
//! inside the host's window is the same player, with the same permissions; the host then runs a hot-join round and
//! the welcome tells the game the session state again. A player that stops on purpose says goodbye first, and the
//! host frees its slot at once instead of holding it.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use feverlink3_core::crypto::SessionKey;
use feverlink3_core::session::{
    codec, ChecksumOutcome, ChecksumPolicy, ConnectionState, HostOptions, HostOutput, HostSession, Lockstep,
    MessageKind, PeerClockMessage, Permissions, PlayerInfo, PlayerLeftMessage, PlayerOutput, PlayerSession,
    PlayerState, ResumeTicket, ResyncLimits, ResyncState, RosterEntry, SessionState, CAP_BULK_DGRAM, CAP_BULK_PAR,
};

use crate::gamelink::{self, Line};
use crate::guard;
use crate::luatext;
use crate::net::{
    BulkSendJob, BulkSendResult, ClientNet, ClientTransport, ConnectContext, HostNet, HostTransport, LinkInfo,
    NetEvent, ParContext, ParJob, CONNECT_TIMEOUT,
};
use crate::process::Role;
use crate::resync;
use crate::steamnet::bulk_io::{BulkMode, BulkSettings};
use crate::steamnet::par::wire::secret_from_key;

/// Period of the state machines' tick (session broadcast, checksum rounds).
pub const TICK: Duration = Duration::from_millis(250);
/// Longest the loop sleeps when nothing happens; bounds the latency of stop requests and game lines.
const POLL: Duration = Duration::from_millis(20);
/// How many log lines a handle keeps.
const LOG_KEEP: usize = 500;
/// Longest log line kept, in characters.
const LOG_LINE_MAX: usize = 2000;

// ---------------------------------------------------------------------------------------------------------------
// Kind mapping
// ---------------------------------------------------------------------------------------------------------------

/// One row of the table that maps file-link line kinds to protocol message kinds.
#[derive(Debug, Clone, Copy)]
pub struct LineKind {
    /// The `kind` column of the line.
    pub name: &'static str,
    pub kind: MessageKind,
    /// The launcher writes this line to `to_game.log`.
    pub to_game: bool,
    /// The launcher accepts this line from `from_game.log`.
    pub from_game: bool,
}

const fn row(name: &'static str, kind: MessageKind, to_game: bool, from_game: bool) -> LineKind {
    LineKind { name, kind, to_game, from_game }
}

/// The complete mapping, `docs/PROTOCOL.md` kinds to file-link kinds (documented in `launcher/README.md`).
/// Names are the snake_case of the message kind, except `peerclock`, which is what the mod already reads.
/// `resync_chunk` never goes to the game: the launcher streams savegame chunks to disk itself.
pub const LINE_KINDS: [LineKind; 21] = [
    row("welcome", MessageKind::Welcome, true, false),
    row("session", MessageKind::Session, true, false),
    row("peerclock", MessageKind::PeerClock, true, false),
    row("player_joined", MessageKind::PlayerJoined, true, false),
    row("player_left", MessageKind::PlayerLeft, true, false),
    row("checksum_request", MessageKind::ChecksumRequest, true, false),
    row("authority", MessageKind::Authority, true, false),
    row("resync_save", MessageKind::ResyncSave, true, false),
    row("resync_load", MessageKind::ResyncLoad, true, false),
    row("permissions", MessageKind::Permissions, true, false),
    row("hello", MessageKind::Hello, false, true),
    row("clock", MessageKind::Clock, false, true),
    row("speed_request", MessageKind::SpeedRequest, false, true),
    row("checksum_report", MessageKind::ChecksumReport, false, true),
    row("save_done", MessageKind::SaveDone, false, true),
    row("replay_failed", MessageKind::ReplayFailed, false, true),
    row("action_refused", MessageKind::ActionRefused, false, true),
    row("action", MessageKind::Action, true, true),
    row("chat", MessageKind::Chat, true, true),
    row("build_veto", MessageKind::BuildVeto, true, true),
    row("build_commit", MessageKind::BuildCommit, true, true),
];

/// The line kind a message is written as, `None` for kinds that do not go to the game.
pub fn line_kind_for(kind: MessageKind) -> Option<&'static str> {
    LINE_KINDS.iter().find(|r| r.kind == kind && r.to_game).map(|r| r.name)
}

/// The message kind of a line from the game, `None` for a kind the game may not send.
pub fn message_kind_for(line_kind: &str) -> Option<MessageKind> {
    LINE_KINDS.iter().find(|r| r.name == line_kind && r.from_game).map(|r| r.kind)
}

/// A message for the game as a file-link line: `(kind, from, payload)`. `from` is the origin player for actions
/// (the host filled it into the payload), else `0`, because the mod accepts everything but actions from `0` only.
/// `None` when the kind does not go to the game, the payload is not JSON, or it is an action whose body is not a
/// Lua literal: the mod `load`s the body, which would run a call or a loop another player put there.
pub fn to_game_line(kind: MessageKind, payload: &[u8]) -> Option<(&'static str, String, String)> {
    let name = line_kind_for(kind)?;
    let lua = luatext::json_to_lua(payload)?;
    let from = if kind == MessageKind::Action {
        let value = serde_json::from_slice::<serde_json::Value>(payload).ok()?;
        value.get("body").and_then(serde_json::Value::as_str).filter(|body| luatext::is_literal(body))?;
        value.get("from").and_then(serde_json::Value::as_u64).unwrap_or(0)
    } else {
        0
    };
    Some((name, from.to_string(), lua))
}

/// A line from the game as a protocol message. The `from` column is ignored: the state machine knows who the game
/// is. `Err` says why the line was refused.
pub fn from_game_line(line: &Line) -> Result<(MessageKind, Vec<u8>), String> {
    let kind = message_kind_for(&line.kind)
        .ok_or_else(|| format!("line {}: kind {:?} is not accepted", line.seq, line.kind))?;
    let json = luatext::lua_to_json(&line.payload)
        .ok_or_else(|| format!("line {}: payload of {} is not Lua literal syntax", line.seq, line.kind))?;
    Ok((kind, json))
}

// ---------------------------------------------------------------------------------------------------------------
// Handle and status
// ---------------------------------------------------------------------------------------------------------------

/// A checksum round that completed, the host's "last sync result".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncResult {
    pub n: i32,
    pub time: i64,
    pub reporters: Vec<u16>,
    /// Parts whose hashes differed between games; empty when the world is identical everywhere.
    pub differing: Vec<String>,
}

impl SyncResult {
    pub fn identical(&self) -> bool {
        self.differing.is_empty()
    }

    fn from_outcome(o: &ChecksumOutcome) -> SyncResult {
        SyncResult {
            n: o.n,
            time: o.time,
            reporters: o.reporters.clone(),
            differing: o.differing_parts().map(str::to_owned).collect(),
        }
    }
}

/// A game's last reported time. `speed` is known on the host only (players learn clocks as peer clocks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockInfo {
    pub id: u16,
    pub t: i64,
    pub speed: Option<i32>,
}

/// A snapshot of the session for a UI or a test.
#[derive(Debug, Clone)]
pub struct Status {
    pub role: Role,
    pub name: String,
    /// This game's sender id; the host is 0, a player has none until welcomed.
    pub my_id: Option<u16>,
    /// Player: joined (welcomed) over a live connection. Host: always true while running.
    pub connected: bool,
    /// Player: connection lost, retrying.
    pub reconnecting: bool,
    pub reconnect_attempts: u32,
    /// Player: the host's reject reason (`bad-proof`, `version`, `full`, `malformed`).
    pub rejected: Option<String>,
    /// Why a player gave up or the session ended abnormally.
    pub error: Option<String>,
    /// The thread has ended.
    pub finished: bool,
    pub players: Vec<PlayerInfo>,
    pub clocks: Vec<ClockInfo>,
    /// Started, speed, pause stamp and lead, as the host last decided them.
    pub session: Option<SessionState>,
    pub last_sync: Option<SyncResult>,
    /// Host: actions relayed so far.
    pub actions_relayed: u64,
    /// Frames and messages the state machine refused.
    pub dropped: u64,
    /// Host: the state of the resync round last reported (`None` before the first).
    pub resync: Option<String>,
    /// What the transport measured about each open connection: ping and route. The host has one per connection
    /// (by connection id), a player one to the host; empty for a transport that measures nothing (TCP).
    pub links: Vec<LinkInfo>,
    /// Everyone the session knows, by id, the host's own game (id 0) first: connected players with their round-trip
    /// time to the host, and players whose connection dropped and whose slot is held for a return (`Away`). A
    /// player's view is what the host last told it.
    pub roster: Vec<RosterEntry>,
    /// What players may do: the host's own setting, or what the host last announced to this player.
    pub permissions: Permissions,
    /// This game was told to load a save (a resync load) and has not said hello since: its world is being replaced and
    /// the mod is silent until the new one is up. Host and player alike (the host's own game loads only with
    /// `HostOptions::host_reloads`). The live controller does not take that silence for a closed world.
    pub reloading: bool,
    /// Player: chunks received and in all of the savegame the host is sending. Host: chunks sent and in all.
    pub transfer: Option<(u32, u32)>,
    /// A few words about the transfer for the window ("7 connections, 6.9 MB/s"), when it is a parallel one.
    pub transfer_note: Option<String>,
}

impl Status {
    pub(crate) fn new(role: Role, name: &str) -> Status {
        Status {
            role,
            name: name.to_owned(),
            my_id: (role == Role::Host).then_some(HostSession::HOST_ID),
            connected: role == Role::Host,
            reconnecting: false,
            reconnect_attempts: 0,
            rejected: None,
            error: None,
            finished: false,
            players: Vec::new(),
            clocks: Vec::new(),
            session: None,
            transfer: None,
            transfer_note: None,
            last_sync: None,
            actions_relayed: 0,
            dropped: 0,
            resync: None,
            links: Vec::new(),
            roster: Vec::new(),
            permissions: Permissions::default(),
            reloading: false,
        }
    }
}

/// What the owner of a host session can ask for (the host's own UI; players ignore these).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Start,
    Pause,
    Speed(i32),
    Checksum,
    /// Start a resync round now (the host's button).
    Resync,
    /// Replace the host's permissions; broadcast to every player.
    SetPermissions(Permissions),
    /// Remove a player from the session, or release the slot held for one that dropped. The player is told it was
    /// removed and does not reconnect. The host's own id is refused.
    Kick(u16),
}

#[derive(Debug)]
struct Shared {
    status: Mutex<Status>,
    logs: Mutex<Vec<String>>,
    /// How many lines were ever logged: the ring below holds only the last ones.
    log_total: AtomicU64,
    stop: AtomicBool,
    /// A host that is stopped tells its players (`HostSession::end_session`). [`SessionHandle::crash`] turns that off.
    goodbye: AtomicBool,
    /// How a host sends savegames to players that can take them another way ([`HostConfig::bulk`]).
    bulk: Mutex<BulkSettings>,
    /// The per-transfer secret of parallel transfers, derived from the session key.
    par_secret: Mutex<[u8; 32]>,
    echo: bool,
}

impl Shared {
    fn new(status: Status, echo: bool) -> Arc<Shared> {
        Arc::new(Shared {
            status: Mutex::new(status),
            logs: Mutex::new(Vec::new()),
            log_total: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            goodbye: AtomicBool::new(true),
            bulk: Mutex::new(BulkSettings { mode: BulkMode::Off, ..BulkSettings::default() }),
            par_secret: Mutex::new([0; 32]),
            echo,
        })
    }

    fn log(&self, text: impl AsRef<str>) {
        let text = text.as_ref();
        if self.echo {
            eprintln!("{text}");
        }
        let mut logs = self.logs.lock().unwrap_or_else(|p| p.into_inner());
        if logs.len() >= LOG_KEEP {
            logs.remove(0);
        }
        // a line can carry text a peer chose; the ring keeps 500 of them, so each is cut to a screenful
        logs.push(text.chars().take(LOG_LINE_MAX).collect());
        self.log_total.fetch_add(1, Ordering::AcqRel);
    }

    fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap_or_else(|p| p.into_inner()));
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

/// Control and observation of a running session thread. Dropping it stops the session.
#[derive(Debug)]
pub struct SessionHandle {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    thread: Mutex<Option<JoinHandle<()>>>,
    local_addr: Option<SocketAddr>,
}

impl SessionHandle {
    /// A snapshot of the session now.
    pub fn status(&self) -> Status {
        self.shared.status.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The most recent log lines (up to 500), oldest first.
    pub fn logs(&self) -> Vec<String> {
        self.shared.logs.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The log lines after the first `from` ever logged (the ring may have dropped some of them), and the number logged
    /// so far, to pass next time. Lets a caller write the log to a file without keeping its own copy.
    pub fn logs_since(&self, from: u64) -> (Vec<String>, u64) {
        let logs = self.shared.logs.lock().unwrap_or_else(|p| p.into_inner());
        let total = self.shared.log_total.load(Ordering::Acquire);
        let first_held = total.saturating_sub(logs.len() as u64);
        let skip = from.max(first_held).saturating_sub(first_held) as usize;
        (logs.iter().skip(skip).cloned().collect(), total)
    }

    /// Host: the address the listener is bound to.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// Asks the host session to do something. Ignored by players and after the thread ended.
    pub fn command(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// True once the thread has ended.
    pub fn is_finished(&self) -> bool {
        self.status().finished
    }

    /// Stops the session and waits for the thread. Idempotent. Connections are closed; the game is not touched.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
        let thread = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(t) = thread {
            let _ = t.join();
        }
    }

    /// Stops the session the way a dying process would: the connections are closed and nobody is told why, so the
    /// players see an outage and reconnect. [`SessionHandle::stop`] is the orderly end, where a host tells its players
    /// first. For tests of outages.
    pub fn crash(&self) {
        self.shared.goodbye.store(false, Ordering::Release);
        self.stop();
    }

    /// Polls `condition` on the status every 10 ms until it holds (`true`) or `timeout` passes (`false`).
    pub fn wait_for(&self, timeout: Duration, condition: impl Fn(&Status) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if condition(&self.status()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn clock_fn() -> Box<dyn Fn() -> f64 + Send> {
    let start = Instant::now();
    Box::new(move || start.elapsed().as_secs_f64())
}

/// `me` is this game's player id: the permissions line is the host's set-based message seen from that player.
fn write_to_game(link: &mut gamelink::Session, shared: &Shared, kind: MessageKind, payload: &[u8], me: u16) {
    let Some(line_kind) = line_kind_for(kind) else { return };
    if kind == MessageKind::ResyncLoad {
        // the game is about to replace its world: silent until its new script says hello
        shared.update(|s| s.reloading = true);
    }
    let view;
    let payload = if kind == MessageKind::Permissions {
        match resync::permissions_for_game(payload, me) {
            Some(v) => {
                view = v;
                &view[..]
            }
            None => {
                shared.log("permissions: payload not understood, not given to the game");
                return;
            }
        }
    } else {
        payload
    };
    match to_game_line(kind, payload) {
        Some((name, from, lua)) => {
            if let Err(e) = link.send(name, &from, &lua) {
                shared.log(format!("cannot write to_game.log: {e}"));
            }
        }
        None => shared.log(format!(
            "{line_kind}: payload is not JSON, or not a Lua literal where one is needed; not given to the game"
        )),
    }
}

/// New messages from the game, in file order. Lines that cannot be translated are logged and skipped.
fn read_from_game(link: &mut gamelink::Session, shared: &Shared) -> Vec<(MessageKind, Vec<u8>)> {
    let lines = match link.poll() {
        Ok(l) => l,
        Err(e) => {
            shared.log(format!("cannot read from_game.log: {e}"));
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for line in lines {
        match from_game_line(&line) {
            Ok(m) => {
                if m.0 == MessageKind::Hello {
                    // a script that starts says hello once: the world is up (again)
                    shared.update(|s| s.reloading = false);
                }
                out.push(m);
            }
            Err(why) => shared.log(why),
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------------------------------------------

/// Everything [`run_host`] needs.
#[derive(Debug)]
pub struct HostConfig {
    /// Where to listen; port 0 picks a free port ([`SessionHandle::local_addr`]).
    pub bind: SocketAddr,
    /// The host's display name.
    pub name: String,
    pub key: SessionKey,
    /// The file link with the host's own game. `identity.txt` is written by [`run_host`].
    pub link: gamelink::Session,
    pub options: HostOptions,
    /// The `save` folders the game writes its savegames to (see [`resync::save_dirs`]); resync reads the host's
    /// save from here.
    pub save_dirs: Vec<PathBuf>,
    /// How a resync save goes to players that can take it another way than in `ResyncChunk` frames:
    /// `FEVERLINK3_BULK=par` (the default, parallel Steam connections), `dgram` or `off`.
    pub bulk: BulkSettings,
    /// Print log lines to stderr as well as keeping them.
    pub echo_log: bool,
}

/// Binds the listener, writes the identity file and starts the host session thread.
pub fn run_host(config: HostConfig) -> io::Result<SessionHandle> {
    let bind = config.bind;
    run_host_over(config, move |tx| HostNet::bind(bind, tx))
}

/// As [`run_host`] over another transport. `listen` gets the sender the transport's [`NetEvent`]s must go to and
/// returns the transport; `config.bind` is not used. The session code is the same for every transport.
pub fn run_host_over<T: HostTransport>(
    config: HostConfig,
    listen: impl FnOnce(Sender<NetEvent>) -> io::Result<T>,
) -> io::Result<SessionHandle> {
    let HostConfig { name, key, mut link, options, save_dirs, echo_log, bulk, .. } = config;
    let (net_tx, net_rx) = mpsc::channel();
    let net = listen(net_tx)?;
    let par_secret = secret_from_key(key.key());
    let local_addr = net.local_addr();
    let mut host = HostSession::new(key, &name, clock_fn(), options);
    link.write_identity(HostSession::HOST_ID, host.host_name(), Role::Host.as_str())?;
    let shared = Shared::new(Status::new(Role::Host, host.host_name()), echo_log);
    *shared.bulk.lock().unwrap_or_else(|p| p.into_inner()) = bulk;
    *shared.par_secret.lock().unwrap_or_else(|p| p.into_inner()) = par_secret;
    // the first status already lists the host's own game
    refresh_host_status(&host, &net, &shared);
    shared.log(format!("listening on {}", net.describe()));
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let thread = {
        let shared = Arc::clone(&shared);
        thread::Builder::new().name("feverlink3-host".into()).spawn(move || {
            let outcome =
                guard::guarded(|| host_loop(&net, &net_rx, &cmd_rx, &mut host, &mut link, &save_dirs, &shared));
            if let Err(why) = outcome {
                shared.log(format!("the host session stopped: {why}"));
                shared.update(|s| s.error = Some(format!("internal error: {why}")));
            }
            drop(net);
            shared.update(|s| {
                s.finished = true;
                s.connected = false;
            });
        })?
    };
    Ok(SessionHandle { shared, commands: cmd_tx, thread: Mutex::new(Some(thread)), local_addr })
}

fn host_loop(
    net: &dyn HostTransport,
    net_rx: &Receiver<NetEvent>,
    cmd_rx: &Receiver<Command>,
    host: &mut HostSession,
    link: &mut gamelink::Session,
    save_dirs: &[PathBuf],
    shared: &Shared,
) {
    let mut last_tick = Instant::now().checked_sub(TICK).unwrap_or_else(Instant::now);
    while !shared.stopping() {
        let mut outputs = Vec::new();
        match net_rx.recv_timeout(POLL) {
            Ok(event) => outputs.extend(host_net_event(host, event, shared)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(event) = net_rx.try_recv() {
            outputs.extend(host_net_event(host, event, shared));
        }
        while let Ok(command) = cmd_rx.try_recv() {
            outputs.extend(match command {
                Command::Start => host.start(),
                Command::Pause => host.pause(),
                Command::Speed(s) => host.set_speed(s),
                Command::Checksum => host.request_checksum(),
                Command::Resync => host.start_resync("manual"),
                Command::SetPermissions(p) => host.set_permissions(p),
                Command::Kick(id) => host.kick(id),
            });
        }
        for (kind, payload) in read_from_game(link, shared) {
            outputs.extend(host.local_game_message(kind, &payload));
        }
        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            outputs.extend(host.tick());
        }
        host_dispatch(outputs, host, net, link, save_dirs, shared);
        refresh_host_status(host, net, shared);
    }
    // Stopped by its owner (or the transport is gone): say so to every player first. A player that only sees the
    // connection drop reconnects to nobody for five minutes; one that is told ends its session at once.
    if shared.goodbye.load(Ordering::Acquire) {
        let outputs = host.end_session();
        host_dispatch(outputs, host, net, link, save_dirs, shared);
    }
}

fn host_net_event(host: &mut HostSession, event: NetEvent, shared: &Shared) -> Vec<HostOutput> {
    match event {
        NetEvent::Opened { conn, peer } => {
            if let Some(peer) = peer {
                shared.log(format!("connection {conn} from {peer}"));
            }
            host.connection_opened(conn)
        }
        NetEvent::Frame { conn, frame } => host.frame_received(conn, &frame),
        NetEvent::Closed { conn, reason } => {
            shared.log(format!("connection {conn} closed: {reason}"));
            host.connection_closed(conn)
        }
        // only a player receives a save
        NetEvent::BulkSave { conn, .. } | NetEvent::BulkFile { conn, .. } => {
            shared.log(format!("connection {conn}: a bulk save arrived at the host; ignored"));
            Vec::new()
        }
        NetEvent::BulkProgress { .. } => Vec::new(),
        NetEvent::Note { text, .. } => {
            shared.log(text);
            Vec::new()
        }
    }
}

fn host_dispatch(
    outputs: Vec<HostOutput>,
    host: &mut HostSession,
    net: &dyn HostTransport,
    link: &mut gamelink::Session,
    save_dirs: &[PathBuf],
    shared: &Shared,
) {
    for output in outputs {
        match output {
            HostOutput::SendToConnection { connection, frame } => {
                // a failed write closes the connection; its Closed event removes the player
                if let Err(e) = net.send(connection, &frame) {
                    shared.log(format!("connection {connection}: write failed: {e}"));
                }
            }
            HostOutput::CloseConnection(connection) => net.close(connection),
            HostOutput::DeliverToLocalGame { kind, payload } => {
                write_to_game(link, shared, kind, &payload, HostSession::HOST_ID)
            }
            HostOutput::ResyncRequested { reason } => shared.log(format!("resync requested: {reason}")),
            HostOutput::ResyncProgress { id, state, detail } => {
                shared.log(format!("resync {id}: {state:?} {detail}"));
                shared.update(|s| s.resync = Some(format!("{state:?}")));
                if state == ResyncState::Transferring {
                    stream_save(id, host, net, link, save_dirs, shared);
                }
            }
            HostOutput::ResyncFailed { id, reason } => {
                shared.log(format!("resync {id} failed: {reason}"));
                shared.update(|s| s.resync = Some(format!("failed: {reason}")));
            }
            HostOutput::ChecksumCompleted(outcome) => {
                let result = SyncResult::from_outcome(&outcome);
                shared.log(if result.identical() {
                    format!("checksum {} identical across {} game(s)", result.n, result.reporters.len())
                } else {
                    format!(
                        "checksum {} differs in {}",
                        result.n,
                        ChecksumPolicy::describe_parts(result.differing.iter().map(String::as_str))
                    )
                });
                shared.update(|s| s.last_sync = Some(result));
            }
            HostOutput::Log(text) => shared.log(text),
            HostOutput::PlayerJoined(_) | HostOutput::PlayerLeft(_) | HostOutput::Received { .. } => {}
        }
    }
}

/// The Transferring step: the host game saved `feverlink3-resync-<id>`; hash it, announce it to the core, then
/// stream it from disk chunk by chunk (each chunk is sealed, sent and dropped, so a big save never sits in memory).
///
/// With `FEVERLINK3_BULK=dgram`, a player whose join announced `bulk-dgram-1` and whose transport can carry it (Steam)
/// gets the file as bulk datagrams instead, on a thread of its own per player, while the chunks go to the others. The
/// last chunk, which starts the load, is held back until every bulk transfer has ended; a transfer that failed (no
/// progress for 10 s, the receiver's SHA-256 did not match, the connection closed) is replaced by the chunks, for that
/// player only.
fn stream_save(
    id: i32,
    host: &mut HostSession,
    net: &dyn HostTransport,
    link: &mut gamelink::Session,
    save_dirs: &[PathBuf],
    shared: &Shared,
) {
    let name = resync::save_name(id);
    let Some(path) = resync::find_save(save_dirs, &name) else {
        shared.log(format!("resync {id}: {name}.sav not found in the savegame folders; the round will time out"));
        return;
    };
    let (sha, len) = match resync::hash_file(&path) {
        Ok(x) => x,
        Err(e) => {
            shared.log(format!("resync {id}: cannot read {}: {e}", path.display()));
            return;
        }
    };
    shared.log(format!("resync {id}: {len} bytes, SHA-256 {}", &sha[..12]));
    let outputs = host.resync_file_ready(id, &sha, len);
    host_dispatch(outputs, host, net, link, save_dirs, shared);
    let Some(count) = ResyncLimits::chunk_count(len) else { return };
    let abandon = AtomicBool::new(false);
    let settings = shared.bulk.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let secret = *shared.par_secret.lock().unwrap_or_else(|p| p.into_inner());
    thread::scope(|scope| {
        let mut runs: Vec<(u16, thread::ScopedJoinHandle<'_, BulkSendResult>, &'static str)> = Vec::new();
        if settings.mode == BulkMode::Par && settings.par.applies_to(len) {
            if let Some(digest) = sha_bytes(&sha) {
                let mut players = Vec::new();
                for player in host.resync_receivers() {
                    if !host.player_supports(player, CAP_BULK_PAR) {
                        continue;
                    }
                    let Some(sender) = host.player_connection(player).and_then(|c| net.par_sender(c)) else {
                        shared.log(format!(
                            "resync {id}: player #{player} announced parallel connections but the transport has none for it; sending chunks"
                        ));
                        continue;
                    };
                    let job = ParJob {
                        ctx: id as u32,
                        path: path.clone(),
                        sha256: digest,
                        secret,
                        cfg: settings.par.clone(),
                    };
                    let (abandon, shared) = (&abandon, shared);
                    let handle = scope.spawn(move || {
                        let cancelled = || abandon.load(Ordering::Acquire) || shared.stopping();
                        let progress = |p: &crate::net::ParProgress| {
                            let sent = (p.bytes / ResyncLimits::CHUNK_SIZE as u64) as u32;
                            shared.update(|s| {
                                s.transfer = Some((sent.min(count), count));
                                s.transfer_note = Some(par_note(p.lanes, p.rate));
                            });
                        };
                        sender(job, &cancelled, &progress)
                    });
                    players.push(player);
                    runs.push((player, handle, "parallel connections"));
                }
                if !players.is_empty() {
                    host.resync_set_bulk(id, &players);
                    shared.log(format!(
                        "resync {id}: sending the save ({len} bytes) to {} player(s) over parallel Steam connections (the others get chunks)",
                        players.len()
                    ));
                }
            }
        }
        if settings.mode == BulkMode::Dgram {
            if let Some(digest) = sha_bytes(&sha) {
                let mut players = Vec::new();
                for player in host.resync_receivers() {
                    if !host.player_supports(player, CAP_BULK_DGRAM) {
                        continue;
                    }
                    let Some(sender) = host.player_connection(player).and_then(|c| net.bulk_sender(c)) else {
                        continue;
                    };
                    let job = BulkSendJob { ctx: id as u32, path: path.clone(), sha256: digest };
                    let (abandon, shared) = (&abandon, shared);
                    let handle = scope.spawn(move || {
                        let cancelled = || abandon.load(Ordering::Acquire) || shared.stopping();
                        let progress = |bytes: u64| {
                            let sent = (bytes / ResyncLimits::CHUNK_SIZE as u64) as u32;
                            shared.update(|s| s.transfer = Some((sent.min(count), count)));
                        };
                        sender(job, &cancelled, &progress)
                    });
                    players.push(player);
                    runs.push((player, handle, "bulk datagrams"));
                }
                if !players.is_empty() {
                    host.resync_set_bulk(id, &players);
                    shared.log(format!(
                        "resync {id}: sending the save to {} player(s) as bulk datagrams (the others get chunks)",
                        players.len()
                    ));
                }
            }
        }
        for index in 0..count {
            shared.update(|s| s.transfer = Some((index, count)));
            // no other player connected, or the round moved on: nothing more to send
            if host.resync_state() != ResyncState::Transferring {
                break;
            }
            if index + 1 == count && !runs.is_empty() {
                finish_bulk(id, std::mem::take(&mut runs), &path, len, count, host, net, link, save_dirs, shared);
                if host.resync_state() != ResyncState::Transferring {
                    break;
                }
            }
            match resync::read_chunk(&path, len, index) {
                Ok(data) => {
                    let outputs = host.resync_chunk(id, index, &data);
                    host_dispatch(outputs, host, net, link, save_dirs, shared);
                }
                Err(e) => {
                    shared.log(format!("resync {id}: reading chunk {index}: {e}"));
                    break;
                }
            }
        }
        abandon.store(true, Ordering::Release);
    });
    shared.update(|s| {
        s.transfer = None;
        s.transfer_note = None;
    });
}

/// What a parallel transfer reports, as text for the window: "7 connections, 6.9 MB/s".
pub fn par_note(lanes: usize, rate: f64) -> String {
    format!("{lanes} connection{}, {:.1} MB/s", if lanes == 1 { "" } else { "s" }, rate / 1e6)
}

/// Waits for the bulk transfers of a round and, for each that failed, sends that player every chunk the usual way.
#[allow(clippy::too_many_arguments)]
fn finish_bulk(
    id: i32,
    runs: Vec<(u16, thread::ScopedJoinHandle<'_, BulkSendResult>, &'static str)>,
    path: &std::path::Path,
    len: u64,
    count: u32,
    host: &mut HostSession,
    net: &dyn HostTransport,
    link: &mut gamelink::Session,
    save_dirs: &[PathBuf],
    shared: &Shared,
) {
    for (player, handle, how) in runs {
        let who = host.players().get(&player).map_or_else(|| format!("#{player}"), |p| p.name.clone());
        match handle.join().unwrap_or_else(|_| BulkSendResult::Failed("the sending thread failed".into())) {
            BulkSendResult::Done { summary } => {
                shared.log(format!("resync {id}: {who} has the save ({how}): {summary}"));
            }
            BulkSendResult::Failed(why) => {
                shared.log(format!("resync {id}: bulk transfer to {who} failed ({why}); sending the chunks instead"));
                for index in 0..count {
                    if host.resync_state() != ResyncState::Transferring {
                        return;
                    }
                    match resync::read_chunk(path, len, index) {
                        Ok(data) => {
                            let outputs = host.resync_chunk_to(id, player, index, &data);
                            host_dispatch(outputs, host, net, link, save_dirs, shared);
                        }
                        Err(e) => {
                            shared.log(format!("resync {id}: reading chunk {index}: {e}"));
                            return;
                        }
                    }
                }
            }
        }
    }
}

fn sha_bytes(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

fn refresh_host_status(host: &HostSession, net: &dyn HostTransport, shared: &Shared) {
    shared.update(|s| {
        s.players = host.players().values().cloned().collect();
        s.clocks = host.clocks().iter().map(|(&id, c)| ClockInfo { id, t: c.t, speed: Some(c.speed) }).collect();
        s.session = Some(host.state().clone());
        s.actions_relayed = host.actions_relayed();
        s.dropped = host.dropped();
        s.links = net.links();
        s.roster = host.roster();
        s.permissions = host.permissions().clone();
    });
}

// ---------------------------------------------------------------------------------------------------------------
// Player
// ---------------------------------------------------------------------------------------------------------------

/// Everything [`run_player`] needs.
#[derive(Debug)]
pub struct PlayerConfig {
    /// The endpoints of the join code, tried in order.
    pub endpoints: Vec<SocketAddr>,
    pub name: String,
    pub key: SessionKey,
    /// Version text sent in the join message; must equal the host's.
    pub version: String,
    /// The file link with this player's game. `identity.txt` is written when the host's welcome gives the id.
    pub link: gamelink::Session,
    /// The `save` folders the game reads its savegames from; a save received in a resync is written here.
    pub save_dirs: Vec<PathBuf>,
    /// Per-endpoint connect timeout, 5 s.
    pub connect_timeout: Duration,
    /// How long to keep trying after the connection dropped, 5 minutes.
    pub reconnect_window: Duration,
    /// Pause between attempts, 5 s.
    pub reconnect_interval: Duration,
    pub echo_log: bool,
}

impl PlayerConfig {
    /// The defaults: 5 s connect timeout, reconnecting for 5 minutes at 5 s intervals.
    pub fn new(endpoints: Vec<SocketAddr>, name: &str, key: SessionKey, link: gamelink::Session) -> PlayerConfig {
        PlayerConfig {
            endpoints,
            name: name.to_owned(),
            key,
            version: HostOptions::default().version,
            link,
            save_dirs: Vec::new(),
            connect_timeout: CONNECT_TIMEOUT,
            reconnect_window: Duration::from_secs(300),
            reconnect_interval: Duration::from_secs(5),
            echo_log: false,
        }
    }
}

/// Connects (an error here is returned, not retried: a wrong code should fail fast) and starts the player
/// session thread.
pub fn run_player(config: PlayerConfig) -> io::Result<SessionHandle> {
    let endpoints = config.endpoints.clone();
    let connect = move |ctx: ConnectContext<'_>| {
        ClientNet::connect_with_timeout(&endpoints, ctx.conn, ctx.tx, ctx.timeout)
            .map(|net| Box::new(net) as Box<dyn ClientTransport>)
    };
    run_player_over(config, connect, &mut |_| {})
}

/// What connects a player to its host over some transport, now and at every reconnect. Given the id the events of
/// the connection must carry, where they go, how long to try, whether to give up, and where to say what it is doing.
pub type Connector = Box<dyn FnMut(ConnectContext<'_>) -> io::Result<Box<dyn ClientTransport>> + Send>;

/// As [`run_player`] over another transport: `connect` makes the connection (`config.endpoints` is not used,
/// `config.connect_timeout` is the time it may take). `progress` hears what the first attempt is doing; the
/// attempts of a reconnect report to the session log, and stop when the session is stopped.
pub fn run_player_over(
    config: PlayerConfig,
    connect: impl FnMut(ConnectContext<'_>) -> io::Result<Box<dyn ClientTransport>> + Send + 'static,
    progress: &mut dyn FnMut(&str),
) -> io::Result<SessionHandle> {
    let mut connect: Connector = Box::new(connect);
    let (net_tx, net_rx) = mpsc::channel();
    let first = connect(ConnectContext {
        conn: 1,
        tx: net_tx.clone(),
        timeout: config.connect_timeout,
        cancelled: &|| false,
        progress,
    })?;
    let mut player = PlayerSession::new(config.key.clone(), &config.name, &config.version);
    player.set_capabilities(capabilities_of(first.as_ref(), !config.save_dirs.is_empty()));
    if first.par_capable() && !config.save_dirs.is_empty() {
        enable_par(first.as_ref(), &config);
    }
    let shared = Shared::new(Status::new(Role::Player, &config.name), config.echo_log);
    shared.log(format!("connected to {}", first.describe()));
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let thread = {
        let shared = Arc::clone(&shared);
        thread::Builder::new().name("feverlink3-player".into()).spawn(move || {
            let mut state = PlayerLoop {
                config,
                player,
                net: Some(first),
                connect,
                generation: 1,
                net_tx,
                ticket: None,
                par_progress: None,
            };
            if let Err(why) = guard::guarded(|| state.run(&net_rx, &cmd_rx, &shared)) {
                shared.log(format!("the player session stopped: {why}"));
                shared.update(|s| s.error = Some(format!("internal error: {why}")));
            }
            state.net = None;
            shared.update(|s| {
                s.finished = true;
                s.connected = false;
                s.reconnecting = false;
            });
        })?
    };
    Ok(SessionHandle { shared, commands: cmd_tx, thread: Mutex::new(Some(thread)), local_addr: None })
}

/// What this player's end announces in its join message: it can take a savegame as bulk datagrams when the transport can
/// carry them (receiving is always on; the host decides whether to use it, see `FEVERLINK3_BULK`).
fn capabilities_of(net: &dyn ClientTransport, has_save_dir: bool) -> Vec<String> {
    let mut caps = Vec::new();
    if net.bulk_capable() {
        caps.push(CAP_BULK_DGRAM.to_owned());
    }
    if net.par_capable() && has_save_dir {
        caps.push(CAP_BULK_PAR.to_owned());
    }
    caps
}

/// Lets the host send this player a savegame over parallel connections: the finished file goes into the first
/// savegame folder. Without a savegame folder there is nowhere to put it, and the host keeps sending chunks.
fn enable_par(net: &dyn ClientTransport, config: &PlayerConfig) {
    if let Some(dir) = config.save_dirs.first() {
        net.enable_par(ParContext { secret: secret_from_key(config.key.key()), dir: dir.clone() });
    }
}

struct PlayerLoop {
    config: PlayerConfig,
    player: PlayerSession,
    net: Option<Box<dyn ClientTransport>>,
    connect: Connector,
    /// Id of the current connection attempt; events of older ones are stale.
    generation: u64,
    net_tx: Sender<NetEvent>,
    /// What the host's welcome granted: the next connection asks for the same id and slot with it.
    ticket: Option<ResumeTicket>,
    /// How far the parallel receive in progress is: (when last heard, bytes, total bytes, text).
    par_progress: Option<(Instant, u64, u64, String)>,
}

impl PlayerLoop {
    fn run(&mut self, net_rx: &Receiver<NetEvent>, cmd_rx: &Receiver<Command>, shared: &Shared) {
        let mut retry: Option<Retry> = None;
        let mut clocks: BTreeMap<u16, i64> = BTreeMap::new();
        while !shared.stopping() {
            let mut outputs = Vec::new();
            let mut events = Vec::new();
            match net_rx.recv_timeout(POLL) {
                Ok(e) => events.push(e),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            while let Ok(e) = net_rx.try_recv() {
                events.push(e);
            }
            for event in events {
                match event {
                    NetEvent::Opened { .. } => {}
                    NetEvent::Frame { conn, frame } if conn == self.generation => {
                        outputs.extend(self.player.frame_received(&frame));
                    }
                    NetEvent::BulkSave { conn, id, sha256, bytes, note } if conn == self.generation => {
                        shared.log(note);
                        outputs.extend(self.player.bulk_save_received(id, &sha256, bytes));
                    }
                    NetEvent::BulkFile { conn, id, sha256, len, path, note } if conn == self.generation => {
                        self.par_progress = None;
                        shared.log(note);
                        // the file is in the first savegame folder already; the others get a copy
                        let name = resync::save_name(id);
                        for dir in self.config.save_dirs.iter().skip(1) {
                            let copy = std::fs::create_dir_all(dir).and_then(|()| {
                                let tmp = dir.join(format!("{name}.sav.tmp"));
                                std::fs::copy(&path, &tmp)?;
                                std::fs::rename(&tmp, dir.join(format!("{name}.sav")))
                            });
                            if let Err(e) = copy {
                                shared.log(format!(
                                    "resync {id}: cannot copy the received save to {}: {e}",
                                    dir.display()
                                ));
                            }
                        }
                        outputs.extend(self.player.bulk_file_received(id, &sha256, len));
                    }
                    NetEvent::BulkProgress { conn, done, total, note, .. } if conn == self.generation => {
                        self.par_progress = Some((Instant::now(), done, total, note));
                    }
                    NetEvent::Note { conn, text } if conn == self.generation => {
                        self.par_progress = None;
                        shared.log(text);
                    }
                    NetEvent::Closed { conn, reason } if conn == self.generation => {
                        shared.log(format!("connection closed: {reason}"));
                        self.net = None;
                        if self.player.state() == PlayerState::Rejected {
                            // the reject frame and the close can arrive together: record the reject first
                            self.dispatch(std::mem::take(&mut outputs), &mut clocks, shared);
                            return;
                        }
                        // the game must stop running the session while it has no host
                        let lost = SessionState {
                            started: false,
                            speed: 0,
                            pause_at: None,
                            lead: Lockstep::lead_units(1),
                            resync: None,
                        };
                        write_to_game(&mut self.config.link, shared, MessageKind::Session, &codec::encode(&lost), 0);
                        match retry.as_mut() {
                            // another connection of the same outage dropped before the host welcomed us: the outage's
                            // five minutes run on, or a host that accepts and resets would keep us trying for ever
                            Some(r) => r.next_attempt = Instant::now() + self.config.reconnect_interval,
                            None => {
                                shared.log(format!(
                                    "reconnecting every {} s for {} s",
                                    self.config.reconnect_interval.as_secs_f32(),
                                    self.config.reconnect_window.as_secs_f32()
                                ));
                                retry = Some(Retry::start(&self.config));
                            }
                        }
                        clocks.clear();
                        shared.update(|s| {
                            s.connected = false;
                            s.reconnecting = true;
                            s.clocks.clear();
                        });
                    }
                    _ => {}
                }
            }
            while cmd_rx.try_recv().is_ok() {}

            for (kind, payload) in read_from_game(&mut self.config.link, shared) {
                outputs.extend(self.player.local_game_message(kind, &payload));
            }
            if self.dispatch(outputs, &mut clocks, shared) {
                return;
            }
            if self.net.is_some() && self.player.state() == PlayerState::Joined {
                // welcomed: the outage is over, whatever it took
                retry = None;
            } else if let Some(r) = retry.as_mut() {
                if Instant::now() >= r.deadline {
                    let why = format!("could not reconnect within {} s", self.config.reconnect_window.as_secs());
                    shared.log(&why);
                    shared.update(|s| s.error = Some(why.clone()));
                    return;
                }
                if self.net.is_none() && Instant::now() >= r.next_attempt {
                    // a fresh state machine for the fresh connection: new challenge, and the ticket for the old slot
                    self.try_reconnect(r, shared);
                }
            }
            let joined = self.player.state() == PlayerState::Joined && self.net.is_some();
            shared.update(|s| {
                s.connected = joined;
                s.dropped = self.player.dropped();
                if let Some(session) = self.player.session() {
                    s.session = Some(session.clone());
                }
                // a parallel receive reports bytes; the window counts in chunks of the resync
                if self.par_progress.as_ref().is_some_and(|p| p.0.elapsed() > Duration::from_secs(15)) {
                    self.par_progress = None;
                }
                match &self.par_progress {
                    Some((_, done, total, note)) if *total > 0 => {
                        let chunks = ResyncLimits::chunk_count(*total).unwrap_or(1);
                        let got = (*done / ResyncLimits::CHUNK_SIZE as u64) as u32;
                        s.transfer = Some((got.min(chunks), chunks));
                        s.transfer_note = Some(note.clone());
                    }
                    _ => {
                        s.transfer = self.player.transfer_progress();
                        s.transfer_note = None;
                    }
                }
                s.clocks = clocks.iter().map(|(&id, &t)| ClockInfo { id, t, speed: None }).collect();
                s.links = self.net.as_ref().and_then(|n| n.link()).into_iter().collect();
                // while a new connection has no welcome yet, the last roster stays: no empty list in between
                if self.player.state() == PlayerState::Joined {
                    let roster = self.player.roster();
                    s.players = roster
                        .iter()
                        .filter(|e| e.state == ConnectionState::Connected)
                        .map(|e| PlayerInfo { id: e.id, name: e.name.clone(), ready: e.ready })
                        .collect();
                    s.roster = roster;
                    s.permissions = self.player.permissions().clone();
                }
            });
        }
        self.say_goodbye(net_rx);
    }

    /// The session was stopped by its owner: tell the host this player leaves on purpose, so it frees the slot at
    /// once instead of holding it for a return. Waits briefly for the host to close the connection, which shows the
    /// message arrived; the socket is not torn down with it still in flight.
    fn say_goodbye(&mut self, net_rx: &Receiver<NetEvent>) {
        if self.player.state() != PlayerState::Joined {
            return;
        }
        let Some(net) = &self.net else { return };
        for output in self.player.leave() {
            if let PlayerOutput::SendToHost(frame) = output {
                if net.send(&frame).is_err() {
                    return;
                }
            }
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match net_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(NetEvent::Closed { conn, .. }) if conn == self.generation => return,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// One reconnect attempt. True when a connection was made.
    fn try_reconnect(&mut self, retry: &mut Retry, shared: &Shared) -> bool {
        retry.attempts += 1;
        retry.next_attempt = Instant::now() + self.config.reconnect_interval;
        shared.update(|s| s.reconnect_attempts = retry.attempts);
        self.generation += 1;
        let attempt = retry.attempts;
        let mut last_said = String::new();
        let mut say = |text: &str| {
            if last_said != text {
                last_said = text.to_owned();
                shared.log(format!("reconnect attempt {attempt}: {text}"));
            }
        };
        let ctx = ConnectContext {
            conn: self.generation,
            tx: self.net_tx.clone(),
            timeout: self.config.connect_timeout,
            // a stop request must not wait for a route that takes half a minute to fail
            cancelled: &|| shared.stopping(),
            progress: &mut say,
        };
        match (self.connect)(ctx) {
            Ok(net) => {
                shared.log(format!("reconnected to {} (attempt {})", net.describe(), retry.attempts));
                let (key, name, version) = (self.config.key.clone(), &self.config.name, &self.config.version);
                self.player = match self.ticket.clone() {
                    Some(ticket) => PlayerSession::resuming(key, name, version, ticket),
                    None => PlayerSession::new(key, name, version),
                };
                self.player.set_capabilities(capabilities_of(net.as_ref(), !self.config.save_dirs.is_empty()));
                if net.par_capable() && !self.config.save_dirs.is_empty() {
                    enable_par(net.as_ref(), &self.config);
                }
                self.net = Some(net);
                // still reconnecting until the host's welcome says so
                shared.update(|s| s.my_id = None);
                true
            }
            Err(e) => {
                shared.log(format!("reconnect attempt {} failed: {e}", retry.attempts));
                false
            }
        }
    }

    /// Acts on the state machine's outputs. True when the session is over (rejected).
    fn dispatch(&mut self, outputs: Vec<PlayerOutput>, clocks: &mut BTreeMap<u16, i64>, shared: &Shared) -> bool {
        for output in outputs {
            match output {
                PlayerOutput::SendToHost(frame) => {
                    if let Some(net) = &self.net {
                        if let Err(e) = net.send(&frame) {
                            shared.log(format!("write failed: {e}"));
                        }
                    }
                }
                PlayerOutput::DeliverToLocalGame { kind, payload } => {
                    match kind {
                        MessageKind::PeerClock => {
                            if let Some(m) = codec::try_decode::<PeerClockMessage>(&payload) {
                                clocks.insert(m.from, m.t);
                            }
                        }
                        MessageKind::PlayerLeft => {
                            if let Some(m) = codec::try_decode::<PlayerLeftMessage>(&payload) {
                                clocks.remove(&m.id);
                            }
                        }
                        _ => {}
                    }
                    let me = self.player.id().unwrap_or(0);
                    write_to_game(&mut self.config.link, shared, kind, &payload, me);
                }
                PlayerOutput::Welcomed(welcome) => {
                    self.ticket = self.player.ticket().cloned();
                    let name = self.player.assigned_name().unwrap_or(&self.config.name).to_owned();
                    if let Err(e) = self.config.link.write_identity(welcome.you, &name, Role::Player.as_str()) {
                        shared.log(format!("cannot write identity.txt: {e}"));
                    }
                    shared.update(|s| {
                        s.reconnecting = false;
                        s.my_id = Some(welcome.you);
                        s.name = name;
                        s.players = welcome.players.clone();
                        s.session = Some(welcome.session.clone());
                    });
                }
                PlayerOutput::Rejected(reject) => {
                    let why = match &reject.detail {
                        Some(d) => format!("{} ({d})", reject.reason),
                        None => reject.reason.clone(),
                    };
                    shared.update(|s| s.rejected = Some(why));
                }
                PlayerOutput::SaveReceived { id, sha256, bytes } => {
                    let name = resync::save_name(id);
                    match resync::write_save(&self.config.save_dirs, &name, &bytes) {
                        Ok(n) => shared.log(format!(
                            "resync {id}: save received ({} bytes, SHA-256 {}), written to {n} folder(s) as {name}.sav",
                            bytes.len(),
                            &sha256[..12.min(sha256.len())]
                        )),
                        Err(e) => shared.log(format!("resync {id}: cannot write the received save: {e}")),
                    }
                }
                PlayerOutput::Log(text) => shared.log(text),
            }
        }
        // the host closes the connection after a reject; do not wait for that, nobody will retry
        if self.player.state() == PlayerState::Rejected {
            self.net = None;
            return true;
        }
        false
    }
}

/// Bookkeeping of one outage.
struct Retry {
    next_attempt: Instant,
    deadline: Instant,
    attempts: u32,
}

impl Retry {
    fn start(config: &PlayerConfig) -> Retry {
        let now = Instant::now();
        Retry { next_attempt: now + config.reconnect_interval, deadline: now + config.reconnect_window, attempts: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_consistent() {
        for r in &LINE_KINDS {
            if r.to_game {
                assert_eq!(line_kind_for(r.kind), Some(r.name));
            }
            if r.from_game {
                assert_eq!(message_kind_for(r.name), Some(r.kind));
            }
        }
        // pings, pongs, leaving and being removed are the session layer's own talk: the game never sees them
        let session_layer = [MessageKind::Ping, MessageKind::Pong, MessageKind::Leave, MessageKind::Kicked];
        for k in MessageKind::FROM_PLAYER {
            assert_eq!(!session_layer.contains(&k), LINE_KINDS.iter().any(|r| r.kind == k && r.from_game), "{k:?}");
        }
        for k in session_layer {
            assert!(LINE_KINDS.iter().all(|r| r.kind != k), "{k:?} has no line kind");
            assert!(to_game_line(k, b"{}").is_none(), "{k:?} is never written to the game");
        }
        // nothing a player could not send, and nothing unsealed
        for r in LINE_KINDS.iter().filter(|r| r.from_game) {
            assert!(r.kind.allowed_from_player(), "{:?}", r.kind);
        }
        assert_eq!(line_kind_for(MessageKind::ResyncChunk), None);
        assert_eq!(message_kind_for("session"), None, "a game cannot speak for the host");
        assert_eq!(message_kind_for("nope"), None);
        let names: std::collections::BTreeSet<_> = LINE_KINDS.iter().map(|r| r.name).collect();
        assert_eq!(names.len(), LINE_KINDS.len(), "line kinds are unique");
    }

    #[test]
    fn the_host_holds_a_slot_longer_than_a_player_keeps_trying() {
        let dir = tempfile::tempdir().unwrap();
        let link = gamelink::Session::create_under(dir.path(), "p").unwrap();
        let key = SessionKey::derive(&SessionKey::new_secret(), None);
        let config = PlayerConfig::new(Vec::new(), "p", key, link);
        let held = HostOptions::default().reconnect_window_seconds;
        assert!(
            held > config.reconnect_window.as_secs_f64(),
            "a player must not find its slot gone while it still tries"
        );
    }

    #[test]
    fn lines_translate_both_ways() {
        let (name, from, lua) =
            to_game_line(MessageKind::Action, br#"{"stamp":400,"seq":2,"body":"{}","from":3}"#).unwrap();
        assert_eq!((name, from.as_str()), ("action", "3"));
        assert_eq!(lua, r#"{["body"]="{}",["from"]=3,["seq"]=2,["stamp"]=400}"#);
        let (name, from, _) = to_game_line(MessageKind::Chat, br#"{"text":"hi","from":3,"name":"B"}"#).unwrap();
        assert_eq!((name, from.as_str()), ("chat", "0"), "the mod takes chat from the host only");
        assert!(to_game_line(MessageKind::Hello, b"{}").is_none());
        assert!(to_game_line(MessageKind::Session, b"nope").is_none());

        // an action body is Lua that the mod loads: only a literal gets to it
        let action = |body: &str| {
            to_game_line(
                MessageKind::Action,
                &serde_json::to_vec(&serde_json::json!({ "stamp": 1, "seq": 1, "body": body })).unwrap(),
            )
        };
        assert!(action(r#"{["fn"]="build",["at"]={[1]=1.5,[2]=(0/0)}}"#).is_some());
        for hostile in
            ["(function() while true do end end)()", r#"("x"):rep(1e9)"#, "x", "", "{} or f()", "{[1]=print()}"]
        {
            assert!(action(hostile).is_none(), "{hostile:?}");
        }
        assert!(
            to_game_line(MessageKind::Action, br#"{"stamp":1,"seq":1}"#).is_none(),
            "no body, nothing for the mod to load"
        );
        assert!(to_game_line(MessageKind::Action, br#"{"stamp":1,"seq":1,"body":5}"#).is_none());

        let line =
            Line { seq: 1, kind: "hello".into(), from: "5".into(), payload: r#"{["time"]=1200,["name"]="x"}"#.into() };
        let (kind, json) = from_game_line(&line).unwrap();
        assert_eq!(kind, MessageKind::Hello);
        let hello: feverlink3_core::session::HelloMessage = codec::try_decode(&json).unwrap();
        assert_eq!(hello.time, 1200);
        let line = Line { seq: 2, kind: "session".into(), from: "0".into(), payload: "{}".into() };
        assert!(from_game_line(&line).is_err());
        let line = Line { seq: 3, kind: "clock".into(), from: "0".into(), payload: "os.exit()".into() };
        assert!(from_game_line(&line).is_err());
    }
}
