use std::fmt;
use std::time::Instant;

use super::host::HostSession;
use super::kind::MessageKind;
use super::messages::{
    codec, ChallengeMessage, JoinMessage, Permissions, RejectMessage, ResyncChunkMessage, ResyncLoadMessage,
    SessionState, WelcomeMessage,
};
use super::names::Names;
use super::resync::IncomingSave;
use crate::crypto::{Opener, Sealer, SessionKey};
use crate::protocol::{Frame, FrameHeader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Connecting,
    Joined,
    Rejected,
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
    Rejected(RejectMessage),
    Log(String),
}

/// The player side of a session: answers the host's challenge, then seals everything its game sends and opens
/// everything the host sends. No I/O: frames in through [`PlayerSession::frame_received`], the local game through
/// [`PlayerSession::local_game_message`]; every call returns the [`PlayerOutput`]s to act on. Every sealed frame
/// a player receives comes from the host (sender 0); other players' actions arrive relayed, with their origin
/// inside the payload.
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
    now: Box<dyn Fn() -> f64 + Send>,
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
            now,
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
        // a Reject is unsealed, so anyone on the path can write one: it only counts while we are still asking to
        // join, where dropping the connection would do the same harm. Once welcomed, it must not end a session.
        if kind == Some(MessageKind::Reject)
            && sender == HostSession::UNASSIGNED
            && self.state == PlayerState::Connecting
        {
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
            let proof = HostSession::proof(&self.key, &ch.nonce, &self.name).to_vec();
            let join = JoinMessage { name: self.name.clone(), version: self.version.clone(), proof };
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

        if kind == MessageKind::Welcome {
            // one Welcome per connection: a second would replace the sealer and restart its counter, which reuses
            // nonces under the same key
            if self.state == PlayerState::Joined {
                self.drop(kind_byte, "second welcome".to_owned());
                return;
            }
            let Some(w) = codec::try_decode::<WelcomeMessage>(&plain) else {
                self.drop(kind_byte, "malformed welcome".to_owned());
                return;
            };
            self.id = Some(w.you);
            self.assigned_name =
                Some(w.players.iter().find(|p| p.id == w.you).map_or_else(|| self.name.clone(), |p| p.name.clone()));
            self.host_name = Some(w.host_name.clone());
            self.session = Some(w.session.clone());
            self.sealer = Some(Sealer::new(&self.key, w.you));
            self.state = PlayerState::Joined;
            self.log(format!(
                "joined {}'s session as {} (#{})",
                w.host_name,
                self.assigned_name.as_deref().unwrap_or(""),
                w.you
            ));
            self.out.push(PlayerOutput::Welcomed(w));
            self.out.push(PlayerOutput::DeliverToLocalGame { kind, payload: plain });
            return;
        }
        if self.state != PlayerState::Joined {
            self.drop(kind_byte, "before welcome".to_owned());
            return;
        }
        if kind == MessageKind::Session {
            if let Some(s) = codec::try_decode::<SessionState>(&plain) {
                self.session = Some(s);
            }
        }
        if kind == MessageKind::Permissions {
            match codec::try_decode::<Permissions>(&plain) {
                Some(p) => self.permissions = p,
                None => {
                    self.drop(kind_byte, "malformed permissions".to_owned());
                    return;
                }
            }
        }
        if kind == MessageKind::ResyncChunk {
            self.receive_chunk(&plain);
            return;
        }
        if kind == MessageKind::ResyncLoad {
            match codec::try_decode::<ResyncLoadMessage>(&plain) {
                Some(m) if self.incoming.has(m.id, &m.sha256) => {}
                Some(m) => {
                    self.drop(kind_byte, format!("load of save {} that was not received intact", m.id));
                    return;
                }
                None => {
                    self.drop(kind_byte, "malformed load".to_owned());
                    return;
                }
            }
        }
        self.out.push(PlayerOutput::DeliverToLocalGame { kind, payload: plain });
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

    /// A message from this player's game, sealed and sent to the host. Dropped (and counted) before the welcome:
    /// the game should not be running before the host has accepted us.
    pub fn local_game_message(&mut self, kind: MessageKind, payload: &[u8]) -> Vec<PlayerOutput> {
        match self
            .sealer
            .as_mut()
            .filter(|_| self.state == PlayerState::Joined)
            .map(|s| s.try_seal(kind.into(), payload))
        {
            Some(Ok(frame)) => self.out.push(PlayerOutput::SendToHost(frame)),
            Some(Err(e)) => self.drop(kind.into(), format!("not sent: {e}")),
            None => self.drop(kind.into(), "not joined".to_owned()),
        }
        self.drain()
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
