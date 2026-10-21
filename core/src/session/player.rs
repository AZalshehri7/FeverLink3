use std::collections::BTreeMap;
use std::fmt;
use std::time::Instant;

use super::host::HostSession;
use super::kind::MessageKind;
use super::messages::{
    codec, ChallengeMessage, JoinMessage, Permissions, PingMessage, PlayerJoinedMessage, PlayerLeftMessage,
    PongMessage, RejectMessage, ResumeClaim, ResyncChunkMessage, ResyncLoadMessage, SessionState, WelcomeMessage,
};
use super::mods::ModFingerprint;
use super::names::Names;
use super::report::{PlayerReportEvent, ReportAskMessage, ReportBusyMessage, ReportLimits, ReportRequestMessage};
use super::resume::Outbox;
use super::resync::IncomingSave;
use super::roster::{ConnectionState, RosterEntry};
use crate::crypto::{Opener, Sealer, SessionKey};
use crate::protocol::{Frame, FrameHeader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Connecting,
    Joined,
    Rejected,
}

/// What a player keeps to get its id and slot back after a lost connection: the host gives it in the welcome
/// ([`PlayerSession::ticket`]) and the next connection's session takes it ([`PlayerSession::resuming`]). The token
/// is a secret between this player and the host; it is never passed to the game.
#[derive(Clone, PartialEq, Eq)]
pub struct ResumeTicket {
    pub id: u16,
    pub token: Vec<u8>,
}

impl fmt::Debug for ResumeTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResumeTicket").field("id", &self.id).field("token", &"..").finish()
    }
}

/// What the player side asks its caller to do, in this order.
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerOutput {
    SendToHost(Frame),
    DeliverToLocalGame {
        kind: MessageKind,
        payload: Vec<u8>,
    },
    Welcomed(WelcomeMessage),
    /// A savegame sent by the host arrived complete and its SHA-256 matches. The launcher writes it to disk; the
    /// ResyncLoad that follows names it.
    SaveReceived {
        id: i32,
        sha256: String,
        bytes: Vec<u8>,
    },
    /// The session is over and will not be retried: the host refused the join (before the welcome), or ended the
    /// session afterwards (`kicked`, `flood`).
    Rejected(RejectMessage),
    /// A bug report needs the launcher: build this game's part, or the bundle this player asked for arrived.
    Report(PlayerReportEvent),
    Log(String),
}

/// The player side of a session: answers the host's challenge, then seals everything its game sends and opens
/// everything the host sends. No I/O: frames in through [`PlayerSession::frame_received`], the local game through
/// [`PlayerSession::local_game_message`]; every call returns the [`PlayerOutput`]s to act on. Every sealed frame
/// a player receives comes from the host (sender 0); other players' actions arrive relayed, with their origin
/// inside the payload.
///
/// A session is one connection. After a lost connection the caller starts a new one with
/// [`PlayerSession::resuming`] and the ticket of the old one to get the same id back.
pub struct PlayerSession {
    key: SessionKey,
    name: String,
    version: String,
    opener: Opener,
    sealer: Option<Sealer>,
    state: PlayerState,
    id: Option<u16>,
    assigned_name: Option<String>,
    host_name: Option<String>,
    session: Option<SessionState>,
    permissions: Permissions,
    incoming: IncomingSave,
    /// The bundle of a bug report this player asked for, on its way (the same checks as a save).
    report_in: IncomingSave,
    /// The last report the host asked this game's part of: (transfer id, report name), to name the bundle.
    report_ask: Option<(i32, String)>,
    /// What the join message announces (see [`super::CAP_BULK_DGRAM`]).
    caps: Vec<String>,
    /// This game's mod list and builds, sent in compact form with the join (see [`super::mods`]).
    mods: Option<ModFingerprint>,
    /// The slot this session asks for in its join, if it is a return.
    resume: Option<ResumeTicket>,
    /// What the welcome granted for the next return.
    ticket: Option<ResumeTicket>,
    /// The nonce of the challenge this connection answered: the welcome must carry it.
    challenge: Option<Vec<u8>>,
    /// The counter of the welcome. Everything the host sealed before it was sealed before this connection began, so
    /// a frame with a counter at or below it is a replay from an earlier connection, which a fresh replay window
    /// cannot know.
    host_floor: u64,
    /// The counter of the last host frame received, over this connection or the ones before it ([`PlayerSession::carry_to`]): a
    /// return asks to be caught up from there ([`ResumeClaim::last`]).
    host_last: Option<u64>,
    /// What this player's game sent and the host has not acknowledged yet (its pings' `ack`), resent after a resume that catches
    /// up (`resume.rs`); while a returning session waits for its welcome, what the game sends is kept here unsealed.
    outbox: Outbox,
    roster: BTreeMap<u16, RosterEntry>,
    now: Box<dyn Fn() -> f64 + Send>,
    /// The wall clock (ms since 1970) a pong carries for the host's offset estimate ([PlayerSession::set_wall_clock]). Logs only.
    wall: Option<Box<dyn Fn() -> i64 + Send>>,
    /// Each player's wall clock minus the host's, ms, as the host's pings report them.
    offsets: BTreeMap<u16, i64>,
    dropped: u64,
    out: Vec<PlayerOutput>,
}

impl fmt::Debug for PlayerSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlayerSession")
            .field("name", &self.name)
            .field("state", &self.state)
            .field("id", &self.id)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl PlayerSession {
    /// Uses the process's monotonic clock for transfer timeouts.
    pub fn new(key: SessionKey, name: &str, version: &str) -> Self {
        let start = Instant::now();
        Self::with_clock(key, name, version, Box::new(move || start.elapsed().as_secs_f64()))
    }

    /// `now` returns seconds on any monotonic clock; tests inject theirs.
    pub fn with_clock(key: SessionKey, name: &str, version: &str, now: Box<dyn Fn() -> f64 + Send>) -> Self {
        Self::build(key, name, version, now, None)
    }

    /// A session for a new connection after the old one was lost: the join asks the host for the id and slot named by
    /// `ticket`. If the host no longer has them (it was restarted, or the slot expired or was released) the welcome is
    /// that of a new player, with another id; check [`WelcomeMessage::resumed`].
    pub fn resuming(key: SessionKey, name: &str, version: &str, ticket: ResumeTicket) -> Self {
        let start = Instant::now();
        let now = Box::new(move || start.elapsed().as_secs_f64());
        Self::build(key, name, version, now, Some(ticket))
    }

    /// As [`PlayerSession::resuming`] with an injected clock.
    pub fn resuming_with_clock(
        key: SessionKey,
        name: &str,
        version: &str,
        ticket: ResumeTicket,
        now: Box<dyn Fn() -> f64 + Send>,
    ) -> Self {
        Self::build(key, name, version, now, Some(ticket))
    }

    fn build(
        key: SessionKey,
        name: &str,
        version: &str,
        now: Box<dyn Fn() -> f64 + Send>,
        resume: Option<ResumeTicket>,
    ) -> Self {
        Self {
            opener: Opener::new(&key),
            key,
            name: Names::sanitize(name),
            version: version.to_owned(),
            sealer: None,
            state: PlayerState::Connecting,
            id: None,
            assigned_name: None,
            host_name: None,
            session: None,
            permissions: Permissions::default(),
            incoming: IncomingSave::new(),
            report_in: IncomingSave::new(),
            report_ask: None,
            caps: Vec::new(),
            mods: None,
            resume,
            ticket: None,
            challenge: None,
            host_floor: 0,
            host_last: None,
            outbox: Outbox::default(),
            roster: BTreeMap::new(),
            now,
            wall: None,
            offsets: BTreeMap::new(),
            dropped: 0,
            out: Vec::new(),
        }
    }

    pub fn state(&self) -> PlayerState {
        self.state
    }

    /// The sender id the host assigned; `None` before the welcome.
    pub fn id(&self) -> Option<u16> {
        self.id
    }

    /// The display name the host settled on (it may have added `#id`).
    pub fn assigned_name(&self) -> Option<&str> {
        self.assigned_name.as_deref()
    }

    pub fn host_name(&self) -> Option<&str> {
        self.host_name.as_deref()
    }

    /// The last session state the host sent.
    pub fn session(&self) -> Option<&SessionState> {
        self.session.as_ref()
    }

    /// What the host last said players may do.
    pub fn permissions(&self) -> &Permissions {
        &self.permissions
    }

    /// What to give [`PlayerSession::resuming`] for the next connection; `None` before the welcome (and with a host
    /// that does not grant one).
    pub fn ticket(&self) -> Option<&ResumeTicket> {
        self.ticket.as_ref()
    }

    /// Everyone in the session as far as this player knows, by id: who was in the welcome and has joined or left
    /// since, with the round-trip times the host reports in its pings. A player whose connection dropped but whose
    /// slot the host holds shows as away.
    pub fn roster(&self) -> Vec<RosterEntry> {
        self.roster.values().cloned().collect()
    }

    /// This player's own round-trip time to the host in milliseconds, as the host measured it; `None` until the first
    /// ping has been answered and reported back.
    pub fn rtt_ms(&self) -> Option<f64> {
        self.id.and_then(|id| self.roster.get(&id)).and_then(|e| e.rtt_ms)
    }

    /// This player's wall clock minus the host's, in ms, as the host measured it and reported in its pings; None before that.
    /// Logs only (the stamps of a line, [super::Hops]).
    pub fn wall_offset_ms(&self) -> Option<i64> {
        self.id.and_then(|id| self.offsets.get(&id).copied())
    }

    /// Gives the session a wall clock (ms since 1970): its pongs then carry it, so the host can measure this player's clock
    /// offset for the latency logs. Changes nothing the session decides.
    pub fn set_wall_clock(&mut self, wall: Box<dyn Fn() -> i64 + Send>) {
        self.wall = Some(wall);
    }

    /// Call now and then: forgets a savegame transfer that has not finished within 10 minutes.
    pub fn tick(&mut self) -> Vec<PlayerOutput> {
        self.expire_transfer();
        self.drain()
    }

    fn expire_transfer(&mut self) {
        let now = (self.now)();
        if let Some(id) = self.incoming.expire(now) {
            self.log(format!("unfinished save transfer {id} forgotten after 10 minutes"));
        }
    }

    /// Chunks received and chunks in all of the savegame being sent to this game, while one is.
    pub fn transfer_progress(&self) -> Option<(u32, u32)> {
        self.incoming.progress().map(|(done, all)| (done as u32, all as u32))
    }

    /// Frames and local messages refused so far.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    fn drain(&mut self) -> Vec<PlayerOutput> {
        std::mem::take(&mut self.out)
    }

    pub fn frame_received(&mut self, frame: &Frame) -> Vec<PlayerOutput> {
        self.handle_frame(frame);
        self.drain()
    }

    fn handle_frame(&mut self, frame: &Frame) {
        let kind_byte = frame.header.kind;
        let kind = MessageKind::from_u8(kind_byte);
        let sender = frame.header.sender;
        if kind == Some(MessageKind::Reject) && sender == HostSession::UNASSIGNED {
            // an unsealed reject is the answer to a join; once welcomed, only sealed frames can end the session
            if self.state != PlayerState::Connecting {
                self.drop(kind_byte, "unsealed reject after the welcome".to_owned());
                return;
            }
            let r = codec::try_decode::<RejectMessage>(&frame.body)
                .unwrap_or_else(|| RejectMessage::new("malformed", None));
            self.state = PlayerState::Rejected;
            self.log(format!("rejected by the host: {} {}", r.reason, r.detail.as_deref().unwrap_or("")));
            self.out.push(PlayerOutput::Rejected(r));
            return;
        }
        if self.state == PlayerState::Connecting
            && kind == Some(MessageKind::Challenge)
            && sender == HostSession::UNASSIGNED
        {
            let Some(ch) = codec::try_decode::<ChallengeMessage>(&frame.body) else {
                self.drop(kind_byte, "malformed challenge".to_owned());
                return;
            };
            if ch.protocol != HostSession::PROTOCOL_VERSION {
                self.drop(kind_byte, format!("protocol {}", ch.protocol));
                return;
            }
            if ch.nonce.len() != HostSession::NONCE_SIZE {
                self.drop(kind_byte, "challenge nonce of the wrong size".to_owned());
                return;
            }
            if self.challenge.is_some() {
                // a host sends one challenge per connection; a second one is somebody else's
                self.drop(kind_byte, "second challenge".to_owned());
                return;
            }
            self.challenge = Some(ch.nonce.clone());
            let proof = HostSession::proof(&self.key, &ch.nonce, &self.name).to_vec();
            let mut join = JoinMessage::new(&self.name, &self.version, proof);
            join.caps = self.caps.clone();
            join.mods = self.mods.as_ref().map(ModFingerprint::tags);
            // asks to be caught up only when it can resend everything its game sent that the host may not have
            let last = self.host_last.filter(|_| !self.outbox.is_broken());
            join.resume = self.resume.as_ref().map(|t| ResumeClaim {
                id: t.id,
                mac: HostSession::resume_proof(&t.token, &ch.nonce, t.id, &self.name, last).to_vec(),
                last,
            });
            let header =
                FrameHeader::new(FrameHeader::CURRENT_VERSION, MessageKind::Join.into(), HostSession::UNASSIGNED, 0);
            self.out.push(PlayerOutput::SendToHost(Frame::new(header, codec::encode(&join))));
            return;
        }
        if sender != HostSession::HOST_ID {
            self.drop(kind_byte, format!("sender {sender} is not the host"));
            return;
        }
        let plain = match self.opener.try_open(frame) {
            Ok(plain) => plain,
            Err(err) => {
                self.drop(kind_byte, err.to_string());
                return;
            }
        };
        let Some(kind) = kind else {
            self.drop(kind_byte, "unknown kind".to_owned());
            return;
        };

        if self.state == PlayerState::Joined && frame.header.counter <= self.host_floor {
            self.drop(kind_byte, "sealed before this connection began".to_owned());
            return;
        }
        if kind == MessageKind::Welcome {
            // one Welcome per connection: a second would replace the sealer and restart its counter, which reuses
            // nonces under the same key
            if self.state == PlayerState::Joined {
                self.drop(kind_byte, "second welcome".to_owned());
                return;
            }
            self.welcomed(&plain, frame.header.counter);
            return;
        }
        if self.state != PlayerState::Joined {
            self.drop(kind_byte, "before welcome".to_owned());
            return;
        }
        self.host_last = Some(frame.header.counter);
        match kind {
            MessageKind::Session => {
                if let Some(s) = codec::try_decode::<SessionState>(&plain) {
                    self.session = Some(s);
                }
            }
            MessageKind::Permissions => match codec::try_decode::<Permissions>(&plain) {
                Some(p) => self.permissions = p,
                None => {
                    self.drop(kind_byte, "malformed permissions".to_owned());
                    return;
                }
            },
            MessageKind::ResyncChunk => {
                self.receive_chunk(&plain);
                return;
            }
            MessageKind::ResyncLoad => match codec::try_decode::<ResyncLoadMessage>(&plain) {
                Some(m) if self.incoming.has(m.id, &m.sha256) => {}
                Some(m) => {
                    self.drop(kind_byte, format!("load of save {} that was not received intact", m.id));
                    return;
                }
                None => {
                    self.drop(kind_byte, "malformed load".to_owned());
                    return;
                }
            },
            MessageKind::Ping => {
                // answered here, never passed on: the game has nothing to do with it
                match codec::try_decode::<PingMessage>(&plain) {
                    Some(m) => self.answer_ping(&m),
                    None => self.drop(kind_byte, "malformed ping".to_owned()),
                }
                return;
            }
            MessageKind::ReportAsk => {
                // for the launcher, never the game
                match codec::try_decode::<ReportAskMessage>(&plain) {
                    Some(m) => {
                        self.log(format!("report {}: the host asks for this game's logs ({} asked)", m.id, m.by_name));
                        self.report_ask = Some((m.id, m.report.clone()));
                        self.out.push(PlayerOutput::Report(PlayerReportEvent::Asked(m)));
                    }
                    None => self.drop(kind_byte, "malformed report ask".to_owned()),
                }
                return;
            }
            MessageKind::ReportBundle => {
                self.receive_bundle(&plain);
                return;
            }
            MessageKind::ReportBusy => {
                // for the launcher, never the game: the window goes back to the button
                match codec::try_decode::<ReportBusyMessage>(&plain) {
                    Some(m) => {
                        self.log(format!(
                            "report: refused, report {} (asked by {}) is still being made",
                            m.id, m.by_name
                        ));
                        self.out.push(PlayerOutput::Report(PlayerReportEvent::Busy(m)));
                    }
                    None => self.drop(kind_byte, "malformed report busy".to_owned()),
                }
                return;
            }
            MessageKind::Kicked => {
                let r =
                    codec::try_decode::<RejectMessage>(&plain).unwrap_or_else(|| RejectMessage::new("kicked", None));
                self.state = PlayerState::Rejected;
                self.log(format!("the host ended our session: {} {}", r.reason, r.detail.as_deref().unwrap_or("")));
                self.out.push(PlayerOutput::Rejected(r));
                return;
            }
            MessageKind::PlayerJoined => {
                if let Some(m) = codec::try_decode::<PlayerJoinedMessage>(&plain) {
                    let p = m.player;
                    let entry = RosterEntry {
                        id: p.id,
                        name: p.name,
                        ready: p.ready,
                        state: ConnectionState::Connected,
                        rtt_ms: None,
                    };
                    self.roster.insert(p.id, entry);
                }
            }
            MessageKind::PlayerLeft => {
                if let Some(m) = codec::try_decode::<PlayerLeftMessage>(&plain) {
                    if m.away {
                        if let Some(e) = self.roster.get_mut(&m.id) {
                            e.state = ConnectionState::Away;
                            e.rtt_ms = None;
                            e.ready = false;
                        }
                    } else {
                        self.roster.remove(&m.id);
                    }
                }
            }
            _ => {}
        }
        self.out.push(PlayerOutput::DeliverToLocalGame { kind, payload: plain });
    }

    fn welcomed(&mut self, plain: &[u8], counter: u64) {
        let kind_byte = MessageKind::Welcome.into();
        let Some(w) = codec::try_decode::<WelcomeMessage>(plain) else {
            self.drop(kind_byte, "malformed welcome".to_owned());
            return;
        };
        // a welcome from the session's key but for another connection is a replay: taking its id and epoch would
        // repeat counters this id has sealed with before
        if !w.nonce.is_empty() && self.challenge.as_deref() != Some(w.nonce.as_slice()) {
            self.drop(kind_byte, "welcome for another connection".to_owned());
            return;
        }
        let epoch = w.resume.as_ref().map_or(0, |g| g.epoch);
        let Some(sealer) = Sealer::with_epoch(&self.key, w.you, epoch) else {
            self.drop(kind_byte, format!("welcome with epoch {epoch}"));
            return;
        };
        self.id = Some(w.you);
        self.host_floor = counter;
        // what the host sends from here on is kept for the next return; what it sent before (the welcome included) is in this world
        self.host_last = Some(counter);
        self.assigned_name =
            Some(w.players.iter().find(|p| p.id == w.you).map_or_else(|| self.name.clone(), |p| p.name.clone()));
        self.host_name = Some(w.host_name.clone());
        self.session = Some(w.session.clone());
        self.ticket = w.resume.as_ref().map(|g| ResumeTicket { id: w.you, token: g.token.clone() });
        self.roster = w
            .players
            .iter()
            .map(|p| {
                let entry = RosterEntry {
                    id: p.id,
                    name: p.name.clone(),
                    ready: p.ready,
                    state: ConnectionState::Connected,
                    rtt_ms: (p.id == HostSession::HOST_ID).then_some(0.0),
                };
                (p.id, entry)
            })
            .collect();
        self.sealer = Some(sealer);
        self.state = PlayerState::Joined;
        if self.mods.is_some() && !w.mods_checked {
            // an older host: it did not compare the mod lists and is about to send its world; leave before that
            let detail =
                "the host runs an older FeverLink3 that does not compare mod lists; both must run the same version";
            let leave = self.leave();
            self.out.extend(leave);
            self.state = PlayerState::Rejected;
            self.log(format!("leaving {}'s session: {detail}", w.host_name));
            self.out.push(PlayerOutput::Rejected(RejectMessage::new("version", Some(detail))));
            return;
        }
        let back = if w.resumed { " (back in its old slot)" } else { "" };
        self.log(format!(
            "joined {}'s session as {} (#{}){back}",
            w.host_name,
            self.assigned_name.as_deref().unwrap_or(""),
            w.you
        ));
        let for_game = without_secret(plain);
        let catch_up = w.catch_up.filter(|_| w.resumed);
        self.out.push(PlayerOutput::Welcomed(w));
        self.out.push(PlayerOutput::DeliverToLocalGame { kind: MessageKind::Welcome, payload: for_game });
        self.resend_after(catch_up);
    }

    /// After the welcome of a return. Caught up (`heard`: the last frame the host has from this player): everything the game sent
    /// after it goes again, in order, sealed anew. Not caught up (a plain welcome, or one that reloads this game): nothing is resent,
    /// the host's world replaces this one.
    fn resend_after(&mut self, heard: Option<u64>) {
        let kept = self.outbox.take();
        let Some(heard) = heard else {
            if !kept.is_empty() {
                self.log(format!(
                    "{} message(s) of the game from before the welcome not sent: the host does not catch this game up",
                    kept.len()
                ));
            }
            return;
        };
        let again: Vec<_> = kept.into_iter().filter(|k| k.counter.is_none_or(|c| c > heard)).collect();
        self.log(format!(
            "resume: caught up by the host; {} message(s) of the game the host did not have are sent again",
            again.len()
        ));
        for k in again {
            self.seal_and_send(k.kind, &k.payload);
        }
    }

    /// Seals a message of the game, keeps it until the host acknowledges it, and sends it.
    fn seal_and_send(&mut self, kind: MessageKind, payload: &[u8]) {
        let Some(sealer) = self.sealer.as_mut() else { return };
        match sealer.try_seal(kind.into(), payload) {
            Ok(frame) => {
                if Outbox::replayable_from_player(kind) {
                    self.outbox.push(Some(frame.header.counter), kind, payload, true);
                }
                self.out.push(PlayerOutput::SendToHost(frame));
            }
            Err(e) => self.drop(kind.into(), format!("not sent: {e}")),
        }
    }

    /// For the next connection after this one was lost: hands it the last host frame received and what the game sent that the
    /// host has not acknowledged, so the host can catch this game up instead of reloading it (`docs/PROTOCOL.md`, "Catching up").
    /// Call before the next session handles its first frame; it must be one made with [`PlayerSession::resuming`].
    pub fn carry_to(&mut self, next: &mut PlayerSession) {
        next.host_last = self.host_last;
        next.outbox = std::mem::take(&mut self.outbox);
    }

    /// Notes the round-trip times the host reports and answers with the same `n`.
    fn answer_ping(&mut self, m: &PingMessage) {
        for (id, &ms) in &m.rtt {
            if let Some(e) = self.roster.get_mut(id).filter(|e| e.state == ConnectionState::Connected) {
                e.rtt_ms = Some(ms);
            }
        }
        for (&id, &off) in &m.off {
            self.offsets.insert(id, off);
        }
        if let Some(ack) = m.ack {
            self.outbox.ack(ack);
        }
        let w = self.wall.as_ref().map(|w| w());
        if let Some(sealer) = self.sealer.as_mut() {
            let frame = sealer.seal(MessageKind::Pong.into(), &codec::encode(&PongMessage { n: m.n, w }));
            self.out.push(PlayerOutput::SendToHost(frame));
        }
    }

    /// What this player's end can do, announced in the join message: set before the connection's first frame is handled.
    pub fn set_capabilities(&mut self, caps: Vec<String>) {
        self.caps = caps;
    }

    /// This game's mod list for the join: set before the connection's first frame is handled. `None` sends none (a host
    /// that compares mod lists then refuses the join).
    pub fn set_mods(&mut self, mods: Option<ModFingerprint>) {
        self.mods = mods;
    }

    /// The mod list given with [`PlayerSession::set_mods`].
    pub fn mods(&self) -> Option<&ModFingerprint> {
        self.mods.as_ref()
    }

    /// A savegame that arrived whole over another route than `ResyncChunk` frames (bulk datagrams): the same checks, the
    /// same output as when the last chunk arrives. Ignored (with a log line) before the welcome.
    pub fn bulk_save_received(&mut self, id: i32, sha256: &str, bytes: Vec<u8>) -> Vec<PlayerOutput> {
        if self.state != PlayerState::Joined {
            self.log(format!("bulk save {id} ignored: not joined"));
            return self.drain();
        }
        match self.incoming.accept_whole(id, sha256, bytes) {
            Ok(save) => {
                self.log(format!("save {} received as bulk datagrams ({} bytes)", save.id, save.bytes.len()));
                self.out.push(PlayerOutput::SaveReceived { id: save.id, sha256: save.sha256, bytes: save.bytes });
            }
            Err(err) => self.log(format!("bulk save {id} refused: {err}")),
        }
        self.drain()
    }

    /// A savegame that arrived as a file over parallel connections, already checked against its SHA-256 and written to
    /// disk by the launcher: recorded like a received save, so the `ResyncLoad` that names it is accepted. The file is
    /// not held here, so there is no output to act on. Ignored (with a log line) before the welcome.
    pub fn bulk_file_received(&mut self, id: i32, sha256: &str, len: u64) -> Vec<PlayerOutput> {
        if self.state != PlayerState::Joined {
            self.log(format!("bulk file {id} ignored: not joined"));
            return self.drain();
        }
        match self.incoming.accept_external(id, sha256, len) {
            Ok(()) => self.log(format!("save {id} received over parallel connections ({len} bytes)")),
            Err(err) => self.log(format!("bulk file {id} refused: {err}")),
        }
        self.drain()
    }

    fn receive_chunk(&mut self, plain: &[u8]) {
        let kind = MessageKind::ResyncChunk.into();
        let Some(m) = codec::try_decode::<ResyncChunkMessage>(plain) else {
            self.drop(kind, "malformed chunk".to_owned());
            return;
        };
        self.expire_transfer();
        let now = (self.now)();
        match self.incoming.accept(now, m) {
            Ok(Some(save)) => {
                self.log(format!("save {} received ({} bytes)", save.id, save.bytes.len()));
                self.out.push(PlayerOutput::SaveReceived { id: save.id, sha256: save.sha256, bytes: save.bytes });
            }
            Ok(None) => {}
            Err(err) => self.drop(kind, err.to_string()),
        }
    }

    /// A chunk of the bug report bundle this player asked for.
    fn receive_bundle(&mut self, plain: &[u8]) {
        let kind = MessageKind::ReportBundle.into();
        let Some(m) = codec::try_decode::<ResyncChunkMessage>(plain) else {
            self.drop(kind, "malformed report chunk".to_owned());
            return;
        };
        // a bundle is never over the report's limit: refused before anything is held
        let most = super::ResyncLimits::chunk_count(ReportLimits::MAX_BUNDLE_BYTES).unwrap_or(1) as i32;
        if m.count > most {
            self.drop(kind, format!("report bundle of {} chunks (at most {most})", m.count));
            return;
        }
        let now = (self.now)();
        self.report_in.expire(now);
        match self.report_in.accept(now, m) {
            Ok(Some(b)) => {
                let report = match &self.report_ask {
                    Some((id, name)) if *id == b.id => name.clone(),
                    _ => String::new(),
                };
                self.log(format!("report {}: the bundle arrived ({} bytes)", b.id, b.bytes.len()));
                self.out.push(PlayerOutput::Report(PlayerReportEvent::Bundle { id: b.id, report, bytes: b.bytes }));
            }
            Ok(None) => {}
            Err(err) => self.drop(kind, err.to_string()),
        }
    }

    /// The window asks for a bug report: the host asks every game for its part and sends this player the bundle.
    pub fn request_report(&mut self, note: &str) -> Vec<PlayerOutput> {
        let note: String = note.chars().take(ReportLimits::NOTE_CHARS).collect();
        self.local_game_message(MessageKind::ReportRequest, &codec::encode(&ReportRequestMessage { note }))
    }

    /// A message from this player's game, sealed and sent to the host. Dropped (and counted) before the welcome:
    /// the game should not be running before the host has accepted us.
    /// A returning session (made with [`PlayerSession::resuming`]) keeps what the game sends before the welcome, for the host to get
    /// once it has caught this game up; with a plain welcome it is dropped then.
    pub fn local_game_message(&mut self, kind: MessageKind, payload: &[u8]) -> Vec<PlayerOutput> {
        if self.state == PlayerState::Joined && self.sealer.is_some() {
            self.seal_and_send(kind, payload);
        } else if self.state == PlayerState::Connecting && self.resume.is_some() && Outbox::replayable_from_player(kind)
        {
            self.outbox.push(None, kind, payload, true);
        } else {
            self.drop(kind.into(), "not joined".to_owned());
        }
        self.drain()
    }

    /// Tells the host this player is going on purpose, so that it frees the slot at once instead of holding it for a
    /// return. Send the frame it returns, then close the connection. Nothing before the welcome.
    pub fn leave(&mut self) -> Vec<PlayerOutput> {
        self.local_game_message(MessageKind::Leave, b"{}")
    }

    fn drop(&mut self, kind: u8, why: String) {
        self.dropped += 1;
        let kind = MessageKind::from_u8(kind).map_or_else(|| kind.to_string(), |k| format!("{k:?}"));
        self.log(format!("{kind} dropped ({why})"));
    }

    fn log(&mut self, text: String) {
        self.out.push(PlayerOutput::Log(text));
    }
}

/// A welcome payload without its `resume` field: the game gets everything but the secret. If the payload cannot be
/// handled the game gets nothing, never the secret.
fn without_secret(plain: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<serde_json::Value>(plain) {
        Ok(serde_json::Value::Object(mut map)) => {
            map.remove("resume");
            map.remove("catchUp");
            serde_json::to_vec(&serde_json::Value::Object(map)).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}
