use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt;

use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::checksum::{ChecksumOutcome, ChecksumRounds};
use super::kind::MessageKind;
use super::limits::RateLimiter;
use super::linkstats::{HostLinkStats, OffsetEstimate};
use super::lockstep::{LeadMeter, Lockstep, PlayerClock};
use super::messages::{
    codec, ActionMessage, ActionRefusedMessage, AuthorityMessage, BuildCommitMessage, BuildVetoMessage,
    ChallengeMessage, ChatMessage, ChecksumReportMessage, ChecksumRequestMessage, ClockMessage, HelloMessage, Hops,
    JoinMessage, PeerClockMessage, Permissions, PingMessage, PlayerInfo, PlayerJoinedMessage, PlayerLeftMessage,
    PongMessage, RejectMessage, ReplayFailedMessage, ResumeClaim, ResumeGrant, SaveDoneMessage, SessionState,
    SpeedRequestMessage, WelcomeMessage,
};
use super::names::Names;
use super::policy::{ChecksumPolicy, ChecksumPolicyConfig};
use super::resync::ResyncState;
use super::roster::{ConnectionState, RosterEntry};
use super::verdict::VerdictBook;
use crate::crypto::{Opener, Sealer, SessionKey};
use crate::protocol::{Frame, FrameHeader};

mod resync_impl;
use resync_impl::ResyncRound;

/// A transport connection, numbered by the caller.
pub type ConnectionId = u64;

#[derive(Debug, Clone, PartialEq)]
pub struct HostOptions {
    pub version: String,
    /// Most games in the session, the host's own included: 8 means the host and seven players. A slot held for a
    /// player who dropped counts.
    pub max_players: usize,
    pub session_broadcast_seconds: f64,
    pub checksum_seconds: f64,
    /// Start a resync round by itself when the checksum policy asks for one (default true). When false the host
    /// only emits [`HostOutput::ResyncRequested`] and the launcher decides.
    pub auto_resync: bool,
    /// Whether the host's own game loads the saved world in a resync round, like every other game (default true:
    /// all games then start from a state that went through the same save and load). When false the host's game keeps
    /// running its live world, only the other games load, and the paused checksum after the round says whether the
    /// two still match (a difference fails the round and leaves the session paused, as it always does).
    pub host_reloads: bool,
    /// With a reloading host: send the host's game its load while the players still download (default true) instead of
    /// together with them after the transfer. A lab experiment switch (FEVERLINK3_HOST_LOAD_EARLY=0).
    pub host_loads_early: bool,
    /// Every Nth checksum round is a FULL one; the rounds in between are light (no edges, constructions, node settings, town
    /// buildings or ids, see `ChecksumPolicy::HEAVY_PARTS`). Default 1: every round is full. A running full round is spread over
    /// the steps (chunks of entities, `ChecksumPolicy::spread_steps`), which made the light rounds unnecessary (lab 2026-10-18: 2896
    /// entities in 15 chunks of 200 every 3 steps, step max 6.0 ms against a 30 ms budget; no round over 54 ms). A value above 1
    /// (FEVERLINK3_FULL_CHECKSUM_EVERY) gives back the old light/full split for the lab. Paused rounds and the verification
    /// round of a resync are always full and hashed whole.
    pub full_checksum_every: u32,
    /// A regular (running) checksum round that has not been reported by every ready game after this long is dropped: a game
    /// left mid-round or reloaded, and its report will never come. Until then the next regular round waits for it, so two
    /// spread rounds never run at once.
    pub checksum_round_timeout_seconds: f64,
    /// Frames per second one connection may send, on average. The frame that takes it over the limit gets the
    /// connection refused with reason `flood`. 0 turns the limit off. The host's own game is never limited.
    pub max_frames_per_second: u32,
    /// Frames a connection may send at once before the rate above applies.
    pub frame_burst: u32,
    /// Bytes per second one connection may send, on average (a frame costs its header and body), with the same
    /// consequence as the frame rate. A player's traffic is small (actions, clocks, reports); the limit is what a
    /// stranger holding the code costs in host CPU, memory and, for relayed actions, disk. 0 turns it off.
    pub max_bytes_per_second: u32,
    /// Bytes a connection may send at once before the rate above applies.
    pub byte_burst: u32,
    /// How long a player's id, slot and permissions are held after its connection dropped. A player that comes back
    /// within it, with the token from its welcome, gets them back. The launcher retries for 300 s, so this adds a
    /// margin. 0 holds nothing.
    pub reconnect_window_seconds: f64,
    /// Seconds between pings: the round-trip time of every player, and proof that its launcher is alive.
    pub ping_seconds: f64,
    /// A joined connection that sent nothing for this long is closed (its slot is held like any dropped one). 0 turns
    /// it off, and so does turning the pings off: a quiet lobby is not a dead connection. Pings keep a healthy
    /// connection well inside it.
    pub idle_timeout_seconds: f64,
    /// A connection that has not completed the handshake after this long is closed. 0 turns it off.
    pub handshake_timeout_seconds: f64,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            version: "0.1".to_owned(),
            max_players: 8,
            session_broadcast_seconds: 3.0,
            checksum_seconds: 10.0,
            auto_resync: true,
            host_reloads: true,
            host_loads_early: true,
            full_checksum_every: 1,
            checksum_round_timeout_seconds: 60.0,
            max_frames_per_second: 500,
            frame_burst: 2000,
            max_bytes_per_second: 2 << 20,
            byte_burst: 8 << 20,
            reconnect_window_seconds: 330.0,
            ping_seconds: 2.0,
            idle_timeout_seconds: 30.0,
            handshake_timeout_seconds: 10.0,
        }
    }
}

/// Regular checksum rounds delayed because the previous one was still running (logged at most once a minute).
#[derive(Debug, Default, Clone, Copy)]
struct RoundWaits {
    /// Start of the counting period (None: nothing counted yet).
    since: Option<f64>,
    count: u32,
    longest: f64,
    /// When the round now waiting first had to wait.
    waiting_since: Option<f64>,
}

/// How often at most the host logs how many regular checksum rounds waited for the previous one.
const ROUND_WAIT_LOG_SECONDS: f64 = 60.0;

/// How long the lead wanted by the measured delays must stay below the one in force before the lead is shortened.
const LEAD_SHRINK_AFTER_SECONDS: f64 = 8.0;

/// What the host asks its caller to do, in this order.
#[derive(Debug, Clone, PartialEq)]
pub enum HostOutput {
    /// A frame to write to a connection.
    SendToConnection {
        connection: ConnectionId,
        frame: Frame,
    },
    /// The caller must close this connection, after the frames already sent to it: the host rejected it, removed
    /// its player, cut it off for silence or flooding, or a returning player took its place. No `Closed` event
    /// needs to be reported back; the host has already forgotten the connection.
    CloseConnection(ConnectionId),
    /// A message for the host's own game.
    DeliverToLocalGame {
        kind: MessageKind,
        payload: Vec<u8>,
    },
    /// Every message accepted from a player or the local game, for logging and for kinds the host only observes
    /// (save done, replay failed, action refused).
    Received {
        from: u16,
        kind: MessageKind,
        payload: Vec<u8>,
    },
    PlayerJoined(PlayerInfo),
    PlayerLeft(PlayerInfo),
    ChecksumCompleted(ChecksumOutcome),
    /// The checksum policy (or a replay failure) wants a resync. Always emitted; the round starts by itself only
    /// with [`HostOptions::auto_resync`].
    ResyncRequested {
        reason: String,
    },
    /// The resync round changed state (`Idle` = finished and resumed). `detail` is for the host's UI.
    ResyncProgress {
        id: i32,
        state: ResyncState,
        detail: String,
    },
    /// The round gave up. The session stays paused; resume with `set_speed`.
    ResyncFailed {
        id: i32,
        reason: String,
    },
    Log(String),
}

#[derive(Debug)]
struct Connection {
    id: ConnectionId,
    nonce: [u8; 16],
    player: Option<u16>,
    /// Host time the connection was accepted: a handshake that drags on is cut.
    opened: f64,
    /// Host time of the last frame from it, whatever it was.
    last_heard: f64,
    limiter: RateLimiter,
    /// The byte budget: a frame costs its header and body, so a few large frames cannot do what many small ones are
    /// stopped from doing.
    bytes: RateLimiter,
}

/// What the host keeps of a joined player beyond its connection: the secret that lets it come back as itself, and how
/// often its id was used (the epoch of its frame counters).
struct Identity {
    name: String,
    token: [u8; 16],
    epoch: u32,
    /// Host time the connection was lost; `None` while the player is connected.
    away_since: Option<f64>,
    /// The game had said hello when the connection dropped. A player coming back to a session that has not started
    /// keeps it: its game was not reloaded.
    ready: bool,
    /// The capabilities its last join announced.
    caps: Vec<String>,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity").field("name", &self.name).field("epoch", &self.epoch).finish_non_exhaustive()
    }
}

/// An action or a held build's commit made during a pause, waiting for its stamp (HostSession::flush_pause).
#[derive(Debug)]
enum PauseItem {
    Action(ActionMessage),
    Commit(BuildCommitMessage),
}

/// The host side of a session, as a state machine with no I/O: connections are integers the caller owns, frames
/// go in through [`HostSession::frame_received`] and every call returns the [`HostOutput`]s to act on. The host's
/// own game is player 0, reached through [`HostSession::local_game_message`] and
/// [`HostOutput::DeliverToLocalGame`]. Time comes from the `now` function given to the constructor, so tests can
/// drive it.
///
/// Thread-safety: none. The caller serialises all calls (one lock around the host), which also gives every game
/// the same message order.
pub struct HostSession {
    key: SessionKey,
    options: HostOptions,
    now: Box<dyn Fn() -> f64 + Send>,
    sealer: Sealer,
    opener: Opener,
    connections: BTreeMap<ConnectionId, Connection>,
    /// The games that are connected now: the host's own (id 0) and every player with a live connection.
    players: BTreeMap<u16, PlayerInfo>,
    /// Every player but the host that holds a slot, connected or away, by id.
    identities: BTreeMap<u16, Identity>,
    clocks: BTreeMap<u16, PlayerClock>,
    /// Smoothed round-trip time per connected player, milliseconds.
    rtt_ms: BTreeMap<u16, f64>,
    /// Pings sent and not yet answered, per player, oldest first: (n, host time sent, host wall clock sent).
    pings: BTreeMap<u16, VecDeque<(u32, f64, Option<i64>)>>,
    /// The host's wall clock in ms since 1970 ([HostSession::set_wall_clock]); without it no line carries host stamps and no
    /// offset is measured. Logs only.
    wall: Option<Box<dyn Fn() -> i64 + Send>>,
    /// Each player's wall clock offset to the host's, from the pongs (PongMessage::w).
    wall_off: BTreeMap<u16, OffsetEstimate>,
    /// The per-minute link.host lines: round trips and the gaps between the clocks the host handles.
    link_stats: HostLinkStats,
    /// The recent round trips and clock cadences the lead is made from ([`LeadMeter`], [`HostSession::wanted_lead`]).
    lead_meter: LeadMeter,
    ping_n: u32,
    last_ping: f64,
    /// Host time of the last call that paces the host (a frame, a tick).
    last_call: f64,
    /// Until this host time the host is catching up after being away: what its connections sent meanwhile is not
    /// counted against them.
    stall_until: f64,
    checksums: ChecksumRounds,
    /// Every game's verdict of a build at its stamp, compared: only games whose verdicts differ have forked (verdict.rs).
    verdicts: VerdictBook,
    /// Checksum rounds completed, and how many of them were skipped because the games hashed at different sim times.
    rounds_done: u32,
    rounds_skewed: u32,
    permissions: Permissions,
    policy: ChecksumPolicy,
    resync: Option<ResyncRound>,
    resync_counter: i32,
    last_running_speed: i32,
    /// A player joined while a round was already past the point of taking it in: the reason of the round to start
    /// when that one ends.
    pending_new_player: Option<&'static str>,
    dropped_once: HashSet<String>,
    next_player_id: u16,
    checksum_n: i32,
    last_session_broadcast: f64,
    /// Since when the lead wanted by the measured delays has been below the one in force (a lead only shrinks after it has
    /// stayed lower for a while, so a single quick ping cannot make it too short).
    lead_lower_since: Option<f64>,
    last_checksum: f64,
    /// The running checksum round still waiting for reports (its number and when it was asked): the next regular round waits
    /// for it (`regular_round_running`).
    running_round: Option<(i32, f64)>,
    /// Regular rounds that had to wait for the previous one since the last log line about it: since when counted, how many,
    /// the longest wait, and since when the current wait lasts.
    round_waits: RoundWaits,
    host_name: String,
    state: SessionState,
    actions_relayed: u64,
    /// The last number of the pause order (`ActionMessage::pause_seq`): every action or commit made during a pause gets the next one
    /// and goes to every game, its sender included. Never reset in a session, so a number is never given twice.
    pause_seq: u64,
    /// Actions and commits of the pause waiting for every game to say where it stands (flush_pause), in arrival order.
    pause_held: Vec<PauseItem>,
    /// The last stamp the host gave an action of the current pause.
    pause_last: Option<i64>,
    /// A checksum round asked for at a pause before every game said where it stands: stamped at the furthest once they have.
    pending_paused_checksum: bool,
    dropped: u64,
    out: Vec<HostOutput>,
}

impl fmt::Debug for HostSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostSession")
            .field("host_name", &self.host_name)
            .field("state", &self.state)
            .field("players", &self.players)
            .field("connections", &self.connections)
            .finish_non_exhaustive()
    }
}

impl HostSession {
    pub const HOST_ID: u16 = 0;
    pub const UNASSIGNED: u16 = 0xFFFF;
    pub const PROTOCOL_VERSION: i32 = FrameHeader::CURRENT_VERSION as i32;
    /// Bytes of the random nonce in a Challenge.
    pub const NONCE_SIZE: usize = 16;
    /// Connections waiting to introduce themselves at once. The next one is closed on arrival, so a flood of
    /// connections cannot grow the host's tables.
    pub const MAX_PENDING: usize = 32;
    /// Longest `body` of an Action a player may send (bytes): what fits one Steam message (512 KiB, `steam_core::MAX_MESSAGE`) with
    /// room for the frame header, the sealed channel's tag and the action's other fields. A build carries the engine's own command as
    /// text: a modular station is about 150 KB, a module added to one via Configure 298 KB (run c10b 2026-10-14: dropped at the old
    /// 256 KiB, the other game never built it and a resync followed). The limit still keeps one player from making the host relay
    /// and write megabytes for everyone, repeatedly.
    pub const MAX_ACTION_BODY: usize = 480 * 1024;
    /// Messages and drop reasons logged once are remembered up to this many; then the memory is cleared.
    const MAX_REMEMBERED: usize = 1024;

    /// `now` returns seconds on any monotonic clock.
    pub fn new(key: SessionKey, host_name: &str, now: Box<dyn Fn() -> f64 + Send>, options: HostOptions) -> Self {
        let host_name = Names::sanitize(host_name);
        let mut players = BTreeMap::new();
        players.insert(Self::HOST_ID, PlayerInfo { id: Self::HOST_ID, name: host_name.clone(), ready: false });
        Self {
            sealer: Sealer::new(&key, Self::HOST_ID),
            opener: Opener::new(&key),
            key,
            options,
            now,
            connections: BTreeMap::new(),
            players,
            identities: BTreeMap::new(),
            clocks: BTreeMap::new(),
            rtt_ms: BTreeMap::new(),
            pings: BTreeMap::new(),
            wall: None,
            wall_off: BTreeMap::new(),
            link_stats: HostLinkStats::default(),
            lead_meter: LeadMeter::default(),
            ping_n: 0,
            last_ping: f64::NEG_INFINITY,
            last_call: f64::NEG_INFINITY,
            stall_until: f64::NEG_INFINITY,
            checksums: ChecksumRounds::new(),
            verdicts: VerdictBook::new(),
            rounds_done: 0,
            rounds_skewed: 0,
            permissions: Permissions::default(),
            policy: ChecksumPolicy::new(ChecksumPolicyConfig::default()),
            resync: None,
            resync_counter: 0,
            last_running_speed: 1,
            pending_new_player: None,
            dropped_once: HashSet::new(),
            next_player_id: 1,
            checksum_n: 0,
            last_session_broadcast: f64::NEG_INFINITY,
            lead_lower_since: None,
            last_checksum: f64::NEG_INFINITY,
            running_round: None,
            round_waits: RoundWaits::default(),
            host_name,
            state: SessionState {
                started: false,
                speed: 0,
                pause_at: None,
                lead: Lockstep::lead_units(1),
                resync: None,
            },
            actions_relayed: 0,
            pause_seq: 0,
            pause_held: Vec::new(),
            pause_last: None,
            pending_paused_checksum: false,
            dropped: 0,
            out: Vec::new(),
        }
    }

    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Every player, the host's game (id 0) included, by id.
    pub fn players(&self) -> &BTreeMap<u16, PlayerInfo> {
        &self.players
    }

    pub fn clocks(&self) -> &BTreeMap<u16, PlayerClock> {
        &self.clocks
    }

    pub fn permissions(&self) -> &Permissions {
        &self.permissions
    }

    pub fn actions_relayed(&self) -> u64 {
        self.actions_relayed
    }

    /// Frames refused so far (wrong sender, failed authentication, disallowed kind, before the handshake).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    fn now(&self) -> f64 {
        (self.now)()
    }

    fn drain(&mut self) -> Vec<HostOutput> {
        std::mem::take(&mut self.out)
    }

    fn log(&mut self, text: String) {
        self.out.push(HostOutput::Log(text));
    }

    // ---------------------------------------------------------------- connections

    pub fn connection_opened(&mut self, connection: ConnectionId) -> Vec<HostOutput> {
        if self.connections.values().filter(|c| c.player.is_none()).count() >= Self::MAX_PENDING {
            self.refuse(connection, "too many connections waiting to join");
            return self.drain();
        }
        let now = self.now();
        let mut nonce = [0u8; Self::NONCE_SIZE];
        if getrandom::getrandom(&mut nonce).is_err() {
            // game-side code does not panic: with no random numbers there is no challenge to give
            self.log(format!("connection {connection} refused: the host has no source of random numbers"));
            let why = RejectMessage::new("malformed", Some("the host has no source of random numbers"));
            self.send_unsealed(connection, MessageKind::Reject, codec::encode(&why));
            self.out.push(HostOutput::CloseConnection(connection));
            return self.drain();
        }
        let limiter = RateLimiter::new(self.options.max_frames_per_second, self.options.frame_burst, now);
        let bytes = RateLimiter::new(self.options.max_bytes_per_second, self.options.byte_burst, now);
        self.connections.insert(
            connection,
            Connection { id: connection, nonce, player: None, opened: now, last_heard: now, limiter, bytes },
        );
        let challenge = ChallengeMessage {
            nonce: nonce.to_vec(),
            protocol: Self::PROTOCOL_VERSION,
            version: self.options.version.clone(),
        };
        self.send_unsealed(connection, MessageKind::Challenge, codec::encode(&challenge));
        self.drain()
    }

    /// Closes a connection that was never entered in the tables.
    fn refuse(&mut self, connection: ConnectionId, why: &str) {
        self.dropped += 1;
        if self.first_time(format!("refuse/{why}")) {
            self.log(format!("connection {connection} refused: {why}"));
        }
        self.out.push(HostOutput::CloseConnection(connection));
    }

    /// The transport lost a connection. A joined player's slot is held for [`HostOptions::reconnect_window_seconds`]
    /// so it can come back as itself; the others are told it is away.
    pub fn connection_closed(&mut self, connection: ConnectionId) -> Vec<HostOutput> {
        if let Some(Connection { player: Some(id), .. }) = self.connections.remove(&connection) {
            self.remove_player(id, true);
        }
        self.drain()
    }

    pub fn frame_received(&mut self, connection: ConnectionId, frame: &Frame) -> Vec<HostOutput> {
        let now = self.now();
        self.note_time(now);
        let forgiven = now < self.stall_until;
        let Some(c) = self.connections.get_mut(&connection) else {
            return self.drain();
        };
        c.last_heard = now;
        let player = c.player;
        let cost = (FrameHeader::SIZE + frame.body.len()).try_into().unwrap_or(u32::MAX);
        let over_frames = !forgiven && !c.limiter.allow(now);
        let over_bytes = !forgiven && !c.bytes.allow_cost(now, cost);
        if over_frames || over_bytes {
            self.dropped += 1;
            let who = player.map_or_else(|| format!("connection {connection}"), |id| self.name(id));
            let detail = if over_frames {
                format!("more than {} frames per second", self.options.max_frames_per_second)
            } else {
                format!("more than {} bytes per second", self.options.max_bytes_per_second)
            };
            self.log(format!("{who} sent {detail}; the connection is closed"));
            self.expel(connection, "flood", &detail);
            return self.drain();
        }
        let kind_byte = frame.header.kind;
        let kind = MessageKind::from_u8(kind_byte);
        match player {
            None => {
                if kind != Some(MessageKind::Join) {
                    self.drop(connection, kind_byte, "not introduced".to_owned());
                } else {
                    self.handle_join(connection, frame);
                }
            }
            Some(player) => {
                if frame.header.sender != player {
                    // the id the frame claims is left out: it is the sender's to choose, and it would make every
                    // forged frame a new line of the log
                    let why = format!("sender id is not the one of the connection (player {player})");
                    self.drop(connection, kind_byte, why);
                    return self.drain();
                }
                match self.opener.try_open(frame) {
                    Err(err) => self.drop(connection, kind_byte, err.to_string()),
                    Ok(plain) => match kind {
                        Some(k) if k.allowed_from_player() => self.handle_message(player, k, &plain),
                        Some(_) => self.drop(connection, kind_byte, "host only".to_owned()),
                        None => self.drop(connection, kind_byte, "unknown kind".to_owned()),
                    },
                }
            }
        }
        self.drain()
    }

    fn handle_join(&mut self, connection: ConnectionId, frame: &Frame) {
        let Some(join) = codec::try_decode::<JoinMessage>(&frame.body) else {
            self.reject(connection, "malformed", None);
            return;
        };
        if join.version != self.options.version {
            let detail = format!("host runs {}, you run {}", self.options.version, clip(&join.version, 32));
            self.reject(connection, "version", Some(&detail));
            return;
        }
        let Some(nonce) = self.connections.get(&connection).map(|c| c.nonce) else {
            return;
        };
        if !Self::verify_proof(&self.key, &nonce, &join.name, &join.proof) {
            self.reject(connection, "bad-proof", None);
            return;
        }
        if let Some(claim) = &join.resume {
            if self.resume(connection, &nonce, &join, claim) {
                return;
            }
            // the usual outcome after the host was restarted; also what a guessed id gets
            self.log(format!("connection {connection}: no slot #{} to give back; joining as a new player", claim.id));
        }
        self.admit(connection, &join);
    }

    /// A player joining for the first time (or whose claim to an old slot did not hold).
    fn admit(&mut self, connection: ConnectionId, join: &JoinMessage) {
        // every player but the host holds an identity, connected or away: the slot of one that may come back counts
        if 1 + self.identities.len() >= self.options.max_players {
            let away = self.identities.values().filter(|i| i.away_since.is_some()).count();
            let mut detail = format!("{} players", self.options.max_players);
            if away > 0 {
                detail.push_str(&format!(", {away} of them away and holding their slot"));
            }
            self.reject(connection, "full", Some(&detail));
            return;
        }
        if self.next_player_id == Self::UNASSIGNED {
            self.reject(connection, "full", Some("no ids left"));
            return;
        }
        // game-side code does not panic: with no random source there is no token to give, so no join
        let mut token = [0u8; 16];
        if getrandom::getrandom(&mut token).is_err() {
            self.reject(connection, "malformed", Some("the host has no source of random numbers"));
            return;
        }

        // ids only grow and are never given to anyone else: entity keys are `<sender>:<seq>` and nonces are
        // `<sender>:<counter>`, so a reused id would repeat both
        let id = self.next_player_id;
        self.next_player_id += 1;
        let taken: Vec<&str> = self
            .players
            .values()
            .map(|p| p.name.as_str())
            .chain(self.identities.values().map(|i| i.name.as_str()))
            .collect();
        let name = Names::unique(&Names::sanitize(&join.name), id, taken);
        let caps = join.caps.iter().take(16).map(|c| clip(c, 32)).collect();
        self.identities
            .insert(id, Identity { name: name.clone(), token, epoch: 0, away_since: None, ready: false, caps });
        let info = PlayerInfo { id, name: name.clone(), ready: false };
        self.players.insert(id, info.clone());
        if let Some(c) = self.connections.get_mut(&connection) {
            c.player = Some(id);
        }
        self.log(format!("{name} joined (connection {connection})"));
        self.greet(connection, info, ResumeGrant { token: token.to_vec(), epoch: 0 }, false);
    }

    /// A player coming back. True when `claim` proves it holds the token of a slot this host still keeps, and the
    /// player now has it again. See `docs/PROTOCOL.md`, "Reconnect".
    fn resume(&mut self, connection: ConnectionId, nonce: &[u8; 16], join: &JoinMessage, claim: &ResumeClaim) -> bool {
        let id = claim.id;
        let Some(identity) = self.identities.get(&id) else { return false };
        if identity.epoch >= Sealer::MAX_EPOCH
            || !Self::verify_resume(&identity.token, nonce, id, &join.name, &claim.mac)
        {
            return false;
        }
        // the old connection may not have been noticed as lost yet (a cable pulled, a sleeping PC): the new one wins
        if let Some(old) = self.connection_of(id) {
            let name = self.name(id);
            self.log(format!("{name} is back on connection {connection}; the old connection {old} is closed"));
            self.connections.remove(&old);
            self.out.push(HostOutput::CloseConnection(old));
            self.remove_player(id, true);
        }
        let Some(identity) = self.identities.get_mut(&id) else { return false };
        identity.epoch += 1;
        identity.away_since = None;
        identity.caps = join.caps.iter().take(16).map(|c| clip(c, 32)).collect();
        let (name, token, epoch) = (identity.name.clone(), identity.token, identity.epoch);
        // before the start nothing is reloaded for the returner, so it is as ready as it was; in a started session the
        // hot-join round reloads it and it says hello again
        let ready = identity.ready && !self.state.started;
        let info = PlayerInfo { id, name: name.clone(), ready };
        self.players.insert(id, info.clone());
        if let Some(c) = self.connections.get_mut(&connection) {
            c.player = Some(id);
        }
        self.log(format!("{name} is back (connection {connection}), slot #{id}"));
        self.greet(connection, info, ResumeGrant { token: token.to_vec(), epoch }, true);
        true
    }

    /// The welcome, the others told, the permissions, and for a started session the hot-join round.
    fn greet(&mut self, connection: ConnectionId, info: PlayerInfo, grant: ResumeGrant, resumed: bool) {
        let id = info.id;
        let nonce = self.connections.get(&connection).map(|c| c.nonce.to_vec()).unwrap_or_default();
        let welcome = WelcomeMessage {
            you: id,
            host_name: self.host_name.clone(),
            version: self.options.version.clone(),
            players: self.players.values().cloned().collect(),
            session: self.state.clone(),
            nonce,
            resume: Some(grant),
            resumed,
        };
        self.send(connection, MessageKind::Welcome, &codec::encode(&welcome));
        let joined = PlayerJoinedMessage { player: info.clone() };
        self.broadcast(MessageKind::PlayerJoined, &codec::encode(&joined), Some(id));
        self.send(connection, MessageKind::Permissions, &codec::encode(&self.permissions));
        self.out.push(HostOutput::PlayerJoined(info));
        self.on_new_player(if resumed { "player returned" } else { "new player" });
    }

    fn reject(&mut self, connection: ConnectionId, reason: &str, detail: Option<&str>) {
        self.log(format!("connection {connection} rejected: {reason} {}", detail.unwrap_or("")));
        self.send_unsealed(connection, MessageKind::Reject, codec::encode(&RejectMessage::new(reason, detail)));
        self.connections.remove(&connection);
        self.out.push(HostOutput::CloseConnection(connection));
    }

    /// What a joiner must send back for a challenge: `HMAC-SHA256(key, nonce ‖ UTF8(name))`.
    pub fn proof(key: &SessionKey, nonce: &[u8], name: &str) -> [u8; 32] {
        Self::proof_mac(key, nonce, name).finalize().into_bytes().into()
    }

    /// Constant-time check of a proof.
    pub fn verify_proof(key: &SessionKey, nonce: &[u8], name: &str, proof: &[u8]) -> bool {
        Self::proof_mac(key, nonce, name).verify_slice(proof).is_ok()
    }

    fn proof_mac(key: &SessionKey, nonce: &[u8], name: &str) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(key.key()).expect("HMAC takes any key length");
        mac.update(nonce);
        mac.update(name.as_bytes());
        mac
    }

    /// What a returning player sends to claim its old slot: `HMAC-SHA256(token, "FeverLink3 resume" ‖ nonce ‖ id ‖
    /// name)`. The token never travels again, and the nonce makes a claim good for one connection only.
    pub fn resume_proof(token: &[u8], nonce: &[u8], id: u16, name: &str) -> [u8; 32] {
        Self::resume_mac(token, nonce, id, name).finalize().into_bytes().into()
    }

    fn verify_resume(token: &[u8], nonce: &[u8], id: u16, name: &str, mac: &[u8]) -> bool {
        Self::resume_mac(token, nonce, id, name).verify_slice(mac).is_ok()
    }

    fn resume_mac(token: &[u8], nonce: &[u8], id: u16, name: &str) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(token).expect("HMAC takes any key length");
        mac.update(b"FeverLink3 resume");
        mac.update(nonce);
        mac.update(&id.to_be_bytes());
        mac.update(name.as_bytes());
        mac
    }

    fn drop(&mut self, connection: ConnectionId, kind: u8, why: String) {
        self.dropped += 1;
        let kind = kind_name(kind);
        if self.first_time(format!("{connection}/{kind}/{why}")) {
            self.log(format!("connection {connection}: {kind} dropped ({why})"));
        }
    }

    /// True the first time `key` is asked about: how a line that a misbehaving peer could cause for every frame
    /// gets logged once. The memory is bounded; when it is full it is cleared, so a flood of different keys repeats
    /// a few lines but never grows the host.
    fn first_time(&mut self, key: String) -> bool {
        if self.dropped_once.len() >= Self::MAX_REMEMBERED {
            self.dropped_once.clear();
        }
        self.dropped_once.insert(key)
    }

    // ---------------------------------------------------------------- the host's own game

    /// A message from the host's own game. It is trusted like the host itself, so every kind is accepted.
    pub fn local_game_message(&mut self, kind: MessageKind, payload: &[u8]) -> Vec<HostOutput> {
        self.handle_message(Self::HOST_ID, kind, payload);
        self.drain()
    }

    // ---------------------------------------------------------------- messages

    fn handle_message(&mut self, from: u16, kind: MessageKind, payload: &[u8]) {
        match kind {
            MessageKind::Hello => {
                if let Some(p) = self.players.get_mut(&from) {
                    p.ready = true;
                }
                // the payload is the player's: a few bytes of it are enough for a log line
                let shown = String::from_utf8_lossy(payload.get(..160).unwrap_or(payload));
                self.log(format!("{} ready {}", self.name(from), clip(&shown, 120)));
                // after a resync load: the session's pause moves to where the loaded world stands (before the round moves on, so the
                // verify round and the session line this player is sent below already carry it)
                if let Some(m) = codec::try_decode::<HelloMessage>(payload) {
                    self.on_load_hello(from, m.time);
                }
                self.resync_step();
                if from != Self::HOST_ID {
                    if let Some(connection) = self.connection_of(from) {
                        self.send(connection, MessageKind::Session, &codec::encode(&self.state));
                    }
                }
            }

            MessageKind::Clock => {
                if let Some(m) = codec::try_decode::<ClockMessage>(payload) {
                    if !PlayerClock::plausible(m.t, m.speed) {
                        // a clock that is not a game time would stamp every game's next action into the far future
                        self.dropped += 1;
                        if self.first_time(format!("clock/{from}")) {
                            self.log(format!("clock of {} dropped (t {}, speed {})", self.name(from), m.t, m.speed));
                        }
                    } else {
                        // relayed in the order it came, after every action its game shipped before it: the input barrier
                        // (the mod's L.barrier) counts on that
                        let safe = m.bounded_safe();
                        // a stop is the time the clock shows, nothing else; a game that makes no pause promise (an older mod)
                        // counts as standing still where its clock shows speed 0
                        let stop = if m.pause { m.stop.filter(|&s| s == m.t) } else { (m.speed == 0).then_some(m.t) };
                        let now = self.now();
                        self.clocks.insert(from, PlayerClock { t: m.t, speed: m.speed, at: now, safe, stop });
                        self.link_stats.clock(from, now);
                        self.lead_meter.clock(from, m.t, now);
                        self.resync_step();
                        self.flush_pause();
                        // a player cannot push every other game's stamps into the far future with an absurd margin
                        let need = m.need.map(|n| n.clamp(0, ClockMessage::MAX_NEED));
                        let peer = PeerClockMessage {
                            from,
                            t: m.t,
                            ahead: m.ahead,
                            need,
                            safe,
                            pause: m.pause && safe.is_some(),
                            hop: m.hop.as_ref().map(|h| self.relayed_hops(h, from)),
                        };
                        self.broadcast(MessageKind::PeerClock, &codec::encode(&peer), Some(from));
                    }
                }
            }

            MessageKind::Action => {
                let decoded = codec::try_decode::<ActionMessage>(payload);
                if decoded.as_ref().is_some_and(|m| m.body.len() > Self::MAX_ACTION_BODY) {
                    self.dropped += 1;
                    if self.first_time(format!("bigaction/{from}")) {
                        self.log(format!(
                            "action of {} dropped (body over {} bytes)",
                            self.name(from),
                            Self::MAX_ACTION_BODY
                        ));
                    }
                } else if let Some(mut m) = decoded {
                    m.from = from;
                    m.category = m.normalized_category().to_owned();
                    m.allowed = self.permissions.allows_action(&m.category, from);
                    if !m.allowed && self.first_time(format!("denied/{from}/{}", m.category)) {
                        let who = self.name(from);
                        self.log(format!("{who} may not {}; action relayed as refused", m.category));
                    }
                    m.pause_seq = None;
                    m.pause_upto = None;
                    m.hop = m.hop.as_ref().map(|h| self.relayed_hops(h, from));
                    self.link_stats.action(from);
                    if m.pause {
                        // made during a pause: stamped by the host and relayed to every game, the sender too (flush_pause)
                        self.pause_held.push(PauseItem::Action(m));
                        self.flush_pause();
                    } else {
                        self.broadcast(MessageKind::Action, &codec::encode(&m), Some(from));
                        self.actions_relayed += 1;
                    }
                }
            }

            MessageKind::BuildVeto => {
                if let Some(mut m) = codec::try_decode::<BuildVetoMessage>(payload) {
                    m.from = from;
                    m.name = Some(self.name(from));
                    m.why = m.why.chars().take(300).collect();
                    self.broadcast(MessageKind::BuildVeto, &codec::encode(&m), Some(from));
                }
            }

            MessageKind::BuildCommit => {
                if let Some(mut m) = codec::try_decode::<BuildCommitMessage>(payload) {
                    m.from = from;
                    m.pause_seq = None;
                    m.pause_upto = None;
                    if m.pause {
                        self.pause_held.push(PauseItem::Commit(m));
                        self.flush_pause();
                    } else {
                        self.broadcast(MessageKind::BuildCommit, &codec::encode(&m), Some(from));
                    }
                }
            }

            MessageKind::Chat => {
                if let Some(m) = codec::try_decode::<ChatMessage>(payload) {
                    if !m.text.trim().is_empty() {
                        let text: String = m.text.chars().take(500).collect();
                        let chat = ChatMessage { text, from, name: Some(self.name(from)) };
                        self.broadcast(MessageKind::Chat, &codec::encode(&chat), Some(from));
                    }
                }
            }

            MessageKind::SpeedRequest => {
                if let Some(m) = codec::try_decode::<SpeedRequestMessage>(payload) {
                    if !self.permissions.speed.allows(from) {
                        self.dropped += 1;
                        if self.first_time(format!("speed/{from}")) {
                            let who = self.name(from);
                            self.log(format!("speed request from {who} dropped (not allowed)"));
                        }
                    } else if self.resync.is_some() {
                        // once per player and round: a game's speed control asks every frame while its world loads (friend test
                        // 2026-10-16: 1210 of these lines in one resync)
                        if self.first_time(format!("speed-resync/{from}/{}", self.resync_counter)) {
                            let who = self.name(from);
                            self.log(format!("speed request from {who} ignored (resync in progress; further ones in this round are not logged)"));
                        }
                    } else if self.state.started {
                        let same = (m.speed > 0 && self.state.pause_at.is_none() && m.speed == self.state.speed)
                            || (m.speed <= 0 && self.state.pause_at.is_some());
                        if !same {
                            if m.speed <= 0 {
                                self.pause_inner();
                            } else {
                                self.set_speed_inner(m.speed);
                            }
                            let what = if m.speed <= 0 { "pause".to_owned() } else { format!("x{}", m.speed) };
                            self.log(format!("speed {what} asked by {}", self.name(from)));
                        }
                    }
                }
            }

            MessageKind::ChecksumReport => {
                // a report that is not the size of a world's hashes is treated like one that does not decode
                if let Some(m) = codec::try_decode::<ChecksumReportMessage>(payload).filter(ChecksumRounds::acceptable)
                {
                    if from == Self::HOST_ID {
                        if let Some(authority) = m.authority.as_ref().filter(|a| !a.is_empty()) {
                            let msg = AuthorityMessage { n: m.n, values: authority.clone() };
                            self.broadcast(MessageKind::Authority, &codec::encode(&msg), Some(Self::HOST_ID));
                        }
                    }
                    if self.players.get(&from).is_some_and(|p| p.ready) {
                        let expected = self.expected_reporters();
                        if let Some(outcome) = self.checksums.report(from, m, &expected) {
                            self.on_checksum_outcome(outcome);
                        }
                    } else if self.dropped_once.insert(format!("unready-report/{from}")) {
                        // a game that has not said hello since its last load has no world to compare yet; counted, it could
                        // fail the verification of a round it was never part of
                        let who = self.name(from);
                        self.log(format!("checksum report from {who} ignored: no hello since its last load"));
                    }
                }
            }

            MessageKind::SaveDone => {
                if let Some(m) = codec::try_decode::<SaveDoneMessage>(payload) {
                    self.on_save_done(from, m);
                }
            }

            MessageKind::ReplayFailed => {
                if let Some(m) = codec::try_decode::<ReplayFailedMessage>(payload) {
                    self.on_replay_failed(from, &m.detail);
                }
            }

            MessageKind::ActionRefused => {
                if let Some(m) = codec::try_decode::<ActionRefusedMessage>(payload) {
                    self.on_action_verdict(from, m);
                }
            }

            MessageKind::Pong if from != Self::HOST_ID => {
                if let Some(m) = codec::try_decode::<PongMessage>(payload) {
                    self.on_pong(from, m.n, m.w);
                }
            }

            MessageKind::Leave if from != Self::HOST_ID => {
                let who = self.name(from);
                self.log(format!("{who} leaves the session"));
                if let Some(connection) = self.connection_of(from) {
                    self.connections.remove(&connection);
                    self.out.push(HostOutput::CloseConnection(connection));
                }
                self.remove_player(from, false);
            }

            _ => {}
        }
        self.out.push(HostOutput::Received { from, kind, payload: payload.to_vec() });
    }

    // ---------------------------------------------------------------- session control (the launcher's host UI)

    pub fn start(&mut self) -> Vec<HostOutput> {
        let mut times: Vec<i64> = self.clocks.values().map(|c| c.t).collect();
        times.sort_unstable();
        times.dedup();
        let common = if times.len() == 1 { Some(times[0]) } else { None };
        self.state = SessionState { started: true, speed: 0, pause_at: common, ..self.state.clone() };
        let ready = self.players.values().filter(|p| p.ready).count();
        let at = common.map_or("different times".to_owned(), |t| t.to_string());
        self.log(format!("session started with {ready} ready game(s) at {at}"));
        self.broadcast_session();
        if times.len() > 1 {
            // The games report different times, so they are not in the same world: bring everyone into the
            // host's world now instead of waiting for the host to ask.
            self.begin_resync("session start");
        } else {
            self.request_checksum_inner();
        }
        self.drain()
    }

    pub fn set_speed(&mut self, speed: i32) -> Vec<HostOutput> {
        if self.resync.is_some() {
            self.log("speed change ignored: resync in progress".to_owned());
        } else {
            self.set_speed_inner(speed);
        }
        self.drain()
    }

    pub fn pause(&mut self) -> Vec<HostOutput> {
        if self.resync.is_none() {
            self.pause_inner();
        }
        self.drain()
    }

    pub fn request_checksum(&mut self) -> Vec<HostOutput> {
        self.request_checksum_inner();
        self.drain()
    }

    /// Replaces who may build, bulldoze, spend and change speed, and tells every game (kind 20). Only the host's
    /// launcher can call this; a player cannot send the kind.
    pub fn set_permissions(&mut self, permissions: Permissions) -> Vec<HostOutput> {
        if permissions != self.permissions {
            self.permissions = permissions;
            self.log("permissions changed".to_owned());
        }
        self.broadcast(MessageKind::Permissions, &codec::encode(&self.permissions), None);
        self.drain()
    }

    /// Call regularly (every 250 ms or so): periodic session broadcast, checksum rounds and the resync round's
    /// waits and timeouts.
    pub fn tick(&mut self) -> Vec<HostOutput> {
        let t = self.now();
        self.note_time(t);
        self.resync_step();
        self.watch_connections(t);
        self.ping_players(t);
        let wall = self.wall_ms();
        let offsets: BTreeMap<u16, i64> =
            self.wall_off.iter().filter_map(|(&id, o)| o.value().map(|v| (id, v))).collect();
        for line in
            self.link_stats.minute(t, wall, |id| if id == Self::HOST_ID { Some(0) } else { offsets.get(&id).copied() })
        {
            self.log(line);
        }
        self.adapt_lead(t);
        if t - self.last_session_broadcast >= self.options.session_broadcast_seconds {
            self.broadcast_session();
        }
        let running = self.regular_round_running(t);
        if self.state.started
            && self.state.pause_at.is_none()
            && t - self.last_checksum >= self.options.checksum_seconds
        {
            if running {
                // the previous round is still being read (spread over the steps) or reported: the next one waits for it, so two
                // spread rounds never run at once and a slow game delays the interval instead of piling rounds up
                self.round_waits.waiting_since.get_or_insert(t);
            } else {
                if let Some(since) = self.round_waits.waiting_since.take() {
                    let w = &mut self.round_waits;
                    w.since.get_or_insert(since);
                    w.count += 1;
                    w.longest = w.longest.max(t - since);
                }
                self.request_checksum_inner();
            }
        }
        self.log_round_waits(t);
        self.drain()
    }

    /// Whether the last running checksum round still waits for reports. A round open longer than
    /// `checksum_round_timeout_seconds` is dropped here (a game left mid-round or reloaded and will never report it).
    fn regular_round_running(&mut self, t: f64) -> bool {
        let Some((n, asked)) = self.running_round else { return false };
        let expected = self.expected_reporters();
        let missing = self.checksums.missing(n, &expected);
        if missing.is_empty() {
            // complete, or waiting for no ready game (none has said hello): nothing to wait for
            self.running_round = None;
            return false;
        }
        if t - asked < self.options.checksum_round_timeout_seconds {
            return true;
        }
        let missing: Vec<String> = missing.into_iter().map(|id| self.name(id)).collect();
        self.checksums.drop_round(n);
        self.running_round = None;
        self.log(format!(
            "checksum round {n} dropped: not reported by {} after {:.0} s",
            missing.join(", "),
            t - asked
        ));
        false
    }

    /// At most once a minute: how many regular rounds had to wait for the previous one, and the longest wait.
    fn log_round_waits(&mut self, t: f64) {
        let w = self.round_waits;
        let Some(since) = w.since else { return };
        if t - since < ROUND_WAIT_LOG_SECONDS {
            return;
        }
        self.log(format!(
            "checksum: {} round(s) waited for the previous one in the last {:.0} s (longest wait {:.1} s)",
            w.count,
            t - since,
            w.longest
        ));
        self.round_waits = RoundWaits { waiting_since: w.waiting_since, ..RoundWaits::default() };
    }

    /// What the resync round is doing, for the players' windows (None when no round runs). Tells everyone when it changes.
    pub(super) fn set_resync_phase(&mut self, phase: Option<&str>) {
        let phase = phase.map(str::to_owned);
        if self.state.resync != phase {
            self.state.resync = phase;
            self.broadcast_session();
        }
    }

    /// The slowest connected player's round trip, in milliseconds (0 while none was measured).
    fn slowest_rtt_ms(&self) -> f64 {
        self.rtt_ms.iter().filter(|(id, _)| self.players.contains_key(id)).map(|(_, &ms)| ms).fold(0.0, f64::max)
    }

    /// The round trip and the clock gap the lead covers now (ms): the larger of the slowest smoothed round trip and the slowest
    /// player's high quantile of its last round trips (jitter), and the slowest game's high quantile of its recent clock gaps
    /// (the host's own game included: its clocks reach every other game).
    fn lead_inputs(&self) -> (f64, f64) {
        let here = |id: u16| self.players.contains_key(&id);
        let rtt = self.slowest_rtt_ms().max(self.lead_meter.rtt_ms(here));
        (rtt, self.lead_meter.gap_ms(self.now(), here))
    }

    /// The lead the measured delays ask for at this speed ([`Lockstep::lead_for_measured`]).
    pub(super) fn wanted_lead(&self, speed: i32) -> i64 {
        let (rtt, gap) = self.lead_inputs();
        Lockstep::lead_for_measured(speed, rtt, gap)
    }

    /// Keeps `state.lead` in step with the measured delays: longer at once, shorter only after eight seconds of being lower.
    fn adapt_lead(&mut self, t: f64) {
        if !self.state.started {
            return;
        }
        let speed = self.state.speed.max(1);
        let want = self.wanted_lead(speed);
        let old = self.state.lead;
        if want > self.state.lead {
            self.state.lead = want;
            self.lead_lower_since = None;
            self.broadcast_session();
        } else if want < self.state.lead {
            let since = *self.lead_lower_since.get_or_insert(t);
            if t - since >= LEAD_SHRINK_AFTER_SECONDS {
                self.state.lead = want;
                self.lead_lower_since = None;
                self.broadcast_session();
            }
        } else {
            self.lead_lower_since = None;
        }
        if self.state.lead != old {
            // bug 4/6: what the lead was made from, each time it changes (the lab run checks the steps against link.ping and link.host)
            let (rtt, gap) = self.lead_inputs();
            self.log(format!(
                "link.lead lead_steps={} was_steps={} speed={speed} rtt_ms={rtt:.0} gap_ms={gap:.0} step_ms={:.0}",
                self.state.lead / Lockstep::STEP,
                old / Lockstep::STEP,
                1000.0 / (Lockstep::STEPS_PER_SECOND * f64::from(speed))
            ));
        }
    }

    fn set_speed_inner(&mut self, speed: i32) {
        if !self.state.started {
            return;
        }
        let speed = speed.clamp(1, 4);
        self.last_running_speed = speed;
        let lead = self.wanted_lead(speed);
        self.lead_lower_since = None;
        self.state = SessionState { speed, pause_at: None, lead, ..self.state.clone() };
        self.broadcast_session();
        // actions of the pause still held (a game had not said where it stands): a normal stamp now, after the resume line
        self.flush_pause();
    }

    fn pause_inner(&mut self) {
        if !self.state.started {
            return;
        }
        self.state.pause_at = Some(self.future_stamp());
        self.pause_last = None;
        self.broadcast_session();
    }

    fn request_checksum_inner(&mut self) {
        let paused = matches!(self.state.pause_at, Some(p) if self.max_clock() >= p);
        // a round at a pause is stamped where the furthest game stands still (the others run on to it): known once every game said
        // where it stands (the pause order, flush_pause)
        let stopped = match self.state.pause_at {
            Some(p) if paused => self.pause_stops(p),
            _ => None,
        };
        if paused && stopped.is_none() {
            self.pending_paused_checksum = true;
            return;
        }
        self.checksum_n += 1;
        let n = self.checksum_n;
        // Every Nth round (the first one too) and every paused round is a full check; the rounds between leave out the heavy lanes.
        // N is 1 by default: every round is full (a running one spread over the steps).
        let every = i64::from(self.options.full_checksum_every.max(1));
        let full = paused || (i64::from(n) - 1) % every == 0;
        if full {
            self.checksums.open(n);
        } else {
            self.checksums.open_light(n);
        }
        let at = match stopped {
            Some(furthest) => furthest,
            None => self.future_stamp(),
        };
        let spread = ChecksumPolicy::spread_steps(full, paused, self.state.speed);
        let request = ChecksumRequestMessage { n, at, paused, full, spread };
        self.broadcast(MessageKind::ChecksumRequest, &codec::encode(&request), None);
        self.last_checksum = self.now();
        if !paused {
            // a paused round is hashed whole at once and never holds up the timer (which does not run at a pause)
            self.running_round = Some((n, self.last_checksum));
        }
    }

    /// Forgets the running round being waited for (a resync's reload makes its reports moot), and drops it.
    pub(super) fn forget_running_round(&mut self) {
        if let Some((n, _)) = self.running_round.take() {
            self.checksums.drop_round(n);
        }
    }

    /// The fastest game's estimated clock now; 0 with no clocks.
    pub fn max_clock(&self) -> i64 {
        if self.clocks.is_empty() {
            return 0;
        }
        let now = self.now();
        self.clocks.values().map(|c| c.estimate(now)).fold(f64::NEG_INFINITY, f64::max) as i64
    }

    /// A stamp every game reaches after receiving a message sent now. Never before a promise a game has made (ClockMessage::safe):
    /// a game may run up to the smallest promise of the others, so no game has passed the largest one, and a pause or a checksum
    /// stamped there is never late on any game, whatever lead each of them used.
    pub fn future_stamp(&self) -> i64 {
        let by_lead = Lockstep::round_up_to_step(
            self.max_clock().saturating_add(self.wanted_lead(self.state.speed.max(1))) as f64,
        );
        let promised = self.clocks.values().filter_map(|c| c.safe).max().unwrap_or(by_lead);
        by_lead.max(Lockstep::round_up_to_step(promised as f64))
    }

    fn broadcast_session(&mut self) {
        self.broadcast(MessageKind::Session, &codec::encode(&self.state), None);
        self.last_session_broadcast = self.now();
    }

    // ---------------------------------------------------------------- who is here

    /// Every player the host knows, by id: its own game, the connected players, and those whose connection dropped
    /// and whose slot is held for a return.
    pub fn roster(&self) -> Vec<RosterEntry> {
        let mut out = Vec::with_capacity(1 + self.identities.len());
        if let Some(host) = self.players.get(&Self::HOST_ID) {
            out.push(RosterEntry {
                id: host.id,
                name: host.name.clone(),
                ready: host.ready,
                state: ConnectionState::Connected,
                rtt_ms: Some(0.0),
            });
        }
        for (&id, identity) in &self.identities {
            let here = self.players.get(&id);
            out.push(RosterEntry {
                id,
                name: here.map_or_else(|| identity.name.clone(), |p| p.name.clone()),
                ready: here.is_some_and(|p| p.ready),
                state: if here.is_some() { ConnectionState::Connected } else { ConnectionState::Away },
                rtt_ms: if here.is_some() { self.rtt_ms.get(&id).copied() } else { None },
            });
        }
        out
    }

    /// A connected player's round-trip time to the host in milliseconds, smoothed; `None` before its first answer.
    /// The host's own game is 0.
    pub fn rtt_ms(&self, player: u16) -> Option<f64> {
        if player == Self::HOST_ID {
            Some(0.0)
        } else {
            self.rtt_ms.get(&player).copied()
        }
    }

    /// Removes a player on the host's decision: a connected player is told (sealed `Kicked`, reason `kicked`),
    /// disconnected and loses its slot; the slot of a player who is away is released. The host's own game cannot be
    /// removed. The player may join again with the join code like anyone else.
    pub fn kick(&mut self, player: u16) -> Vec<HostOutput> {
        if player == Self::HOST_ID {
            self.log("the host cannot be removed".to_owned());
        } else if let Some(connection) = self.connection_of(player) {
            let who = self.name(player);
            self.log(format!("{who} was removed by the host"));
            self.expel(connection, "kicked", "removed by the host");
        } else if self.identities.contains_key(&player) {
            self.release_slot(player, |name| format!("the slot of {name} was released by the host"));
        } else {
            self.log(format!("there is no player #{player} to remove"));
        }
        self.drain()
    }

    /// Ends the session on the host's decision (its owner stopped hosting, or the host's world was closed): every
    /// joined player is told (sealed `Kicked`, reason `ended`) and its connection closed, and a connection that never
    /// joined gets the unsealed `Reject` with the same reason. Without this a player only saw the connection drop and
    /// tried to reconnect to nobody for five minutes. No slot is held for a return: the owner drops the session after it
    /// has sent the outputs.
    pub fn end_session(&mut self) -> Vec<HostOutput> {
        let message = codec::encode(&RejectMessage::new("ended", Some("the host ended the session")));
        for (connection, c) in std::mem::take(&mut self.connections) {
            if c.player.is_some() {
                self.send(connection, MessageKind::Kicked, &message);
            } else {
                self.send_unsealed(connection, MessageKind::Reject, message.clone());
            }
            self.out.push(HostOutput::CloseConnection(connection));
        }
        self.log("the host ended the session".to_owned());
        self.drain()
    }

    /// Takes a player out of the game. `hold` keeps its id, slot and token for [`HostOptions::reconnect_window_seconds`]
    /// (the connection dropped); without it the slot is freed (the player left, or the host removed it).
    fn remove_player(&mut self, id: u16, hold: bool) {
        let Some(info) = self.players.remove(&id) else { return };
        let now = self.now();
        self.clocks.remove(&id);
        // the actions of a pause may have waited for this game to say where it stands
        self.flush_pause();
        self.rtt_ms.remove(&id);
        self.pings.remove(&id);
        self.wall_off.remove(&id);
        self.link_stats.forget(id);
        self.lead_meter.forget(id);
        let hold = hold && self.options.reconnect_window_seconds > 0.0;
        match self.identities.get_mut(&id) {
            Some(identity) if hold => {
                identity.away_since = Some(now);
                identity.ready = info.ready;
            }
            _ => {
                self.identities.remove(&id);
            }
        }
        let tail = if hold { "; the slot is held for a return" } else { "" };
        self.log(format!("{} left{tail}", info.name));
        let left = PlayerLeftMessage { id, name: info.name.clone(), away: hold };
        self.broadcast(MessageKind::PlayerLeft, &codec::encode(&left), Some(id));
        self.out.push(HostOutput::PlayerLeft(info));
        // a resync round counts on the games it sent the save to; this one is gone, and if it returns it is a joiner
        self.forget_participant(id);
        // a checksum round that waited for this game's report alone is complete now
        let expected = self.expected_reporters();
        for outcome in self.checksums.finished(&expected) {
            self.on_checksum_outcome(outcome);
        }
        self.resync_step();
    }

    /// Frees the slot held for a player who is away, and tells the others it is gone for good (they had it as away).
    /// `why` makes the log line from the player's name.
    fn release_slot(&mut self, id: u16, why: impl FnOnce(&str) -> String) {
        let Some(identity) = self.identities.remove(&id) else { return };
        self.log(why(&identity.name));
        let left = PlayerLeftMessage { id, name: identity.name, away: false };
        self.broadcast(MessageKind::PlayerLeft, &codec::encode(&left), Some(id));
    }

    /// The games a checksum round waits for: those that said hello since their last load.
    fn expected_reporters(&self) -> Vec<u16> {
        self.players.values().filter(|p| p.ready).map(|p| p.id).collect()
    }

    /// Ends a connection by the host's decision and says why. A joined player gets a sealed `Kicked` and loses its
    /// slot; a connection that never joined gets the unsealed `Reject`.
    fn expel(&mut self, connection: ConnectionId, reason: &str, detail: &str) {
        let Some(c) = self.connections.remove(&connection) else { return };
        let message = codec::encode(&RejectMessage::new(reason, Some(detail)));
        match c.player {
            Some(id) => {
                self.send(connection, MessageKind::Kicked, &message);
                self.out.push(HostOutput::CloseConnection(connection));
                self.remove_player(id, false);
            }
            None => {
                self.send_unsealed(connection, MessageKind::Reject, message);
                self.out.push(HostOutput::CloseConnection(connection));
            }
        }
    }

    /// Longest gap between two calls into the host that still counts as the host paying attention. Anything longer
    /// means the host itself was away (its process suspended, its thread stuck in a write to a peer that stopped
    /// reading): nobody could be heard meanwhile, and what the connections sent is only now being read.
    const STALL_SECONDS: f64 = 5.0;
    /// How long at most the frames of a stalled host's backlog are forgiven for.
    const STALL_GRACE_MAX: f64 = 60.0;

    /// Notes that the host runs now. After a gap of more than [`Self::STALL_SECONDS`] it forgives what the stall did:
    /// every connection counts as just heard from, the frame budgets are full again and not enforced while the backlog
    /// is read, the slots held for players who dropped keep their whole window, and the pings sent before the stall
    /// are forgotten (their answers would measure the stall, not the player).
    fn note_time(&mut self, now: f64) {
        let gap = now - self.last_call;
        self.last_call = now;
        if !gap.is_finite() || gap <= Self::STALL_SECONDS {
            return;
        }
        self.stall_until = now + gap.min(Self::STALL_GRACE_MAX);
        for c in self.connections.values_mut() {
            c.last_heard = now;
            if c.player.is_none() {
                c.opened = now;
            }
            c.limiter.refill(now);
            c.bytes.refill(now);
        }
        for identity in self.identities.values_mut() {
            if let Some(since) = identity.away_since.as_mut() {
                *since += gap;
            }
        }
        self.pings.clear();
    }

    /// Closes connections that went quiet or never finished the handshake, and frees the slots whose players did
    /// not come back in time.
    fn watch_connections(&mut self, t: f64) {
        let handshake = self.options.handshake_timeout_seconds;
        let idle = if self.options.ping_seconds > 0.0 { self.options.idle_timeout_seconds } else { 0.0 };
        let overdue: Vec<(ConnectionId, Option<u16>)> = self
            .connections
            .values()
            .filter(|c| match c.player {
                Some(_) => idle > 0.0 && t - c.last_heard >= idle,
                None => handshake > 0.0 && t - c.opened >= handshake,
            })
            .map(|c| (c.id, c.player))
            .collect();
        for (connection, player) in overdue {
            match player {
                Some(id) => {
                    let who = self.name(id);
                    self.log(format!("{who} timed out: nothing heard for {idle:.0} s"));
                    self.connections.remove(&connection);
                    self.out.push(HostOutput::CloseConnection(connection));
                    self.remove_player(id, true);
                }
                None => self.reject(connection, "timeout", Some("the handshake did not finish in time")),
            }
        }
        let window = self.options.reconnect_window_seconds;
        let gone: Vec<u16> = self
            .identities
            .iter()
            .filter(|(_, i)| i.away_since.is_some_and(|since| t - since >= window))
            .map(|(&id, _)| id)
            .collect();
        for id in gone {
            self.release_slot(id, |name| format!("{name} did not come back within {window:.0} s; the slot is free"));
        }
    }

    /// Every `ping_seconds`: a ping to each connected player, carrying the round-trip times known so far.
    fn ping_players(&mut self, t: f64) {
        let every = self.options.ping_seconds;
        if every <= 0.0 || t - self.last_ping < every {
            return;
        }
        self.last_ping = t;
        self.ping_n = self.ping_n.wrapping_add(1);
        let n = self.ping_n;
        let rtt: BTreeMap<u16, f64> = self.rtt_ms.iter().map(|(&id, &ms)| (id, (ms * 10.0).round() / 10.0)).collect();
        let off: BTreeMap<u16, i64> = self.wall_off.iter().filter_map(|(&id, o)| o.value().map(|v| (id, v))).collect();
        let payload = codec::encode(&PingMessage { n, rtt, off });
        let wall = self.wall_ms();
        let targets: Vec<(ConnectionId, u16)> =
            self.connections.values().filter_map(|c| c.player.map(|id| (c.id, id))).collect();
        for (connection, player) in targets {
            self.send(connection, MessageKind::Ping, &payload);
            let queue = self.pings.entry(player).or_default();
            queue.push_back((n, t, wall));
            while queue.len() > 8 {
                queue.pop_front();
            }
        }
    }

    /// `w`: the player's wall clock when it answered (newer players), for its clock offset.
    fn on_pong(&mut self, from: u16, n: u32, w: Option<i64>) {
        let now = self.now();
        let back = self.wall_ms();
        let Some(queue) = self.pings.get_mut(&from) else { return };
        let Some(at) = queue.iter().position(|&(k, _, _)| k == n) else { return };
        let (_, sent, sent_wall) = queue[at];
        // older pings that were never answered are not going to be
        queue.drain(..=at);
        let sample = ((now - sent) * 1000.0).max(0.0);
        let smoothed = self.rtt_ms.get(&from).map_or(sample, |old| old * 0.75 + sample * 0.25);
        self.rtt_ms.insert(from, smoothed);
        self.link_stats.rtt(from, sample);
        self.lead_meter.rtt(from, sample);
        let off = match (sent_wall, w.filter(|v| Hops::plausible_ms(*v)), back) {
            (Some(s), Some(p), Some(b)) => Some(self.wall_off.entry(from).or_default().sample(s, p, b)),
            _ => None,
        };
        // one line per answered ping (every ping_seconds): the round trip's own cadence next to the clocks' (bug 4, a 5 s cadence)
        let off = off.map_or_else(|| "?".to_owned(), |o| o.to_string());
        self.log(format!(
            "link.ping wall={} peer={from} rtt_ms={sample:.1} srtt_ms={smoothed:.1} off_ms={off}",
            back.unwrap_or(0)
        ));
    }

    /// Gives the host a wall clock (ms since 1970), for the latency logs: the stamps it adds to relayed clocks and actions
    /// ([`Hops`]) and the players' clock offsets. Without one neither is made. Changes nothing the session decides.
    pub fn set_wall_clock(&mut self, wall: Box<dyn Fn() -> i64 + Send>) {
        self.wall = Some(wall);
    }

    fn wall_ms(&self) -> Option<i64> {
        self.wall.as_ref().map(|w| w())
    }

    /// A player's wall clock minus the host's, in ms, as measured from its pongs; 0 for the host's own game, `None` before
    /// the first measurement (or with a player or host that does not stamp).
    pub fn wall_offset_ms(&self, player: u16) -> Option<i64> {
        if player == Self::HOST_ID {
            return self.wall.as_ref().map(|_| 0);
        }
        self.wall_off.get(&player).and_then(OffsetEstimate::value)
    }

    /// The stamps of a line from `from` as the host relays them (see [`Hops::relayed`]).
    fn relayed_hops(&self, h: &Hops, from: u16) -> Hops {
        let rtt = if from == Self::HOST_ID { Some(0.0) } else { self.rtt_ms.get(&from).copied() };
        h.relayed(self.wall_ms(), self.wall_offset_ms(from), rtt)
    }

    // ---------------------------------------------------------------- sending

    /// To every player connection and the local game, except `except`. One sealer, so the host's counter stream is
    /// shared by all connections; a receiver only ever sees it increase.
    /// A chat line from the session itself to every game (the host's own included), named "FeverLink3".
    pub(super) fn announce(&mut self, text: &str) {
        let chat = ChatMessage { text: text.to_owned(), from: Self::HOST_ID, name: Some("FeverLink3".to_owned()) };
        self.broadcast(MessageKind::Chat, &codec::encode(&chat), None);
    }

    /// The pause order. An action (or a held build's commit) made while its game stood at the pause is flagged `pause`. Applied at
    /// once from each game's interface state, in arrival order, it was not one order (the sender had its own first); applied at the
    /// pause stamp, it was late, because no game stands AT the pause: speed 0 takes effect a few steps late, by a different amount on
    /// each game (real run pause3, 2026-10-15: pauseAt 534400, both games stood at 535000, every paused build forked). So the host
    /// STAMPS it: one step past the furthest place any game stands (each says where, ClockMessage::stop, once it stands still), then
    /// relays it to EVERY game, its sender included, numbered (`pause_seq`). Every game applies it at that stamp like any action: a
    /// game runs on to it (one step or a few, the pause's world moves that little) and the simulation applies it there, exactly,
    /// whatever the overshoot. No game can be past the stamp: none moves while paused, except to a stamp the host gave, and the next
    /// stamp waits until every game stands still again at or past the last one. Held while a game has not said where it stands (it
    /// may still be coasting). After the resume (or with the session running) a held one gets a normal stamp, past every promise.
    fn flush_pause(&mut self) {
        if self.pause_held.is_empty() && !self.pending_paused_checksum {
            return;
        }
        // from the save to the load the games' stops describe the world that is being replaced: a stamp counted from them can lie
        // before the save's time (the host's game, loading early, is not counted, so the furthest stop is a guest's, behind the host's
        // save) and would be applied late on every reloaded game. Held until every game stands still in the loaded world.
        if self.resync_replaces_world() {
            return;
        }
        let stamp = match self.state.pause_at {
            // after the resume: past the last stamp of the pause plus one step. Every game runs on to that step (the pause order's
            // run-on, the mod's pauseRun), and a peer whose last clock is still its pause promise lets it stand there (the mod's
            // L.barrier), so a stamp there would already be passed; a clock-plus-lead stamp counted from games that had not said where
            // they stand yet could lie behind it. The lead's floor of three steps hid this; a mock with a one-step lead showed it (a
            // breach after the resume, 2026-10-18).
            None => {
                let s = self.future_stamp();
                self.pause_last.map_or(s, |l| s.max(l.saturating_add(2 * Lockstep::STEP)))
            }
            Some(p) => match self.pause_stops(p) {
                Some(furthest) => Lockstep::round_up_to_step(furthest as f64) + Lockstep::STEP,
                None => return,
            },
        };
        let paused = self.state.pause_at.is_some();
        let held = std::mem::take(&mut self.pause_held);
        // the batch marker: every item of this flush names the number of its last one (a game runs on to the flush's stamp only once
        // it has read them all; mock with jitter, 2026-10-18: the run-on of the first of two passed the stamp of the second)
        let upto = Some(self.pause_seq + held.len() as u64);
        for item in held {
            self.pause_seq += 1;
            match item {
                PauseItem::Action(mut m) => {
                    m.stamp = if paused { stamp } else { stamp.max(m.stamp) };
                    m.pause_seq = Some(self.pause_seq);
                    m.pause_upto = upto;
                    self.broadcast(MessageKind::Action, &codec::encode(&m), None);
                    self.actions_relayed += 1;
                }
                PauseItem::Commit(mut m) => {
                    m.stamp = if paused { stamp } else { stamp.max(m.stamp) };
                    m.pause_seq = Some(self.pause_seq);
                    m.pause_upto = upto;
                    self.broadcast(MessageKind::BuildCommit, &codec::encode(&m), None);
                }
            }
            if paused {
                self.pause_last = Some(stamp);
            }
        }
        if paused && std::mem::take(&mut self.pending_paused_checksum) {
            self.request_checksum_inner();
        }
    }

    /// The furthest place a game stands at the pause `p`, once every ready game said where it stands still (at or past the pause and
    /// the last stamp the host gave in it); `None` while one has not.
    fn pause_stops(&self, p: i64) -> Option<i64> {
        let floor = self.pause_last.unwrap_or(p).max(p);
        let mut furthest = p;
        for player in self.players.values().filter(|x| x.ready) {
            match self.clocks.get(&player.id).and_then(|c| c.stop) {
                Some(s) if s >= floor => furthest = furthest.max(s),
                _ => return None,
            }
        }
        Some(furthest)
    }

    fn broadcast(&mut self, kind: MessageKind, payload: &[u8], except: Option<u16>) {
        let targets: Vec<ConnectionId> = self
            .connections
            .values()
            .filter(|c| matches!(c.player, Some(id) if Some(id) != except))
            .map(|c| c.id)
            .collect();
        for connection in targets {
            self.send(connection, kind, payload);
        }
        if except != Some(Self::HOST_ID) {
            if self.holds_for_load(Self::HOST_ID, kind) {
                self.hold_for_load(Self::HOST_ID, kind, payload);
            } else {
                self.out.push(HostOutput::DeliverToLocalGame { kind, payload: payload.to_vec() });
            }
        }
    }

    fn send(&mut self, connection: ConnectionId, kind: MessageKind, payload: &[u8]) {
        // sealed only when it is sent: a frame sealed now and sent later would arrive behind newer counters (replay window)
        if let Some(to) = self.connections.get(&connection).and_then(|c| c.player) {
            if self.holds_for_load(to, kind) {
                self.hold_for_load(to, kind, payload);
                return;
            }
        }
        match self.sealer.try_seal(kind.into(), payload) {
            Ok(frame) => self.out.push(HostOutput::SendToConnection { connection, frame }),
            // a relayed message that grew past the frame limit when the host added its fields, say
            Err(e) => {
                self.dropped += 1;
                if self.first_time(format!("seal/{kind:?}")) {
                    self.log(format!("{kind:?} not sent to connection {connection}: {e}"));
                }
            }
        }
    }

    fn send_unsealed(&mut self, connection: ConnectionId, kind: MessageKind, payload: Vec<u8>) {
        let header = FrameHeader::new(FrameHeader::CURRENT_VERSION, kind.into(), Self::UNASSIGNED, 0);
        self.out.push(HostOutput::SendToConnection { connection, frame: Frame::new(header, payload) });
    }

    /// The connection of a connected player (not the host's own game).
    pub fn player_connection(&self, player: u16) -> Option<ConnectionId> {
        self.connection_of(player)
    }

    fn connection_of(&self, player: u16) -> Option<ConnectionId> {
        self.connections.values().find(|c| c.player == Some(player)).map(|c| c.id)
    }

    fn name(&self, id: u16) -> String {
        self.players.get(&id).map_or_else(|| format!("#{id}"), |p| p.name.clone())
    }
}

fn kind_name(kind: u8) -> String {
    MessageKind::from_u8(kind).map_or_else(|| kind.to_string(), |k| format!("{k:?}"))
}

/// At most `max` characters of `text`, with `...` where something was cut: for text a peer chose that goes into a
/// log line or a reject.
fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if out.len() < text.len() {
        out.push_str("...");
    }
    out
}
