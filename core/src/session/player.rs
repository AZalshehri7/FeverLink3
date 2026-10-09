use std::fmt;

use super::host::HostSession;
use super::kind::MessageKind;
use super::messages::{codec, ChallengeMessage, JoinMessage, RejectMessage, SessionState, WelcomeMessage};
use super::names::Names;
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
    DeliverToLocalGame { kind: MessageKind, payload: Vec<u8> },
    Welcomed(WelcomeMessage),
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
    pub fn new(key: SessionKey, name: &str, version: &str) -> Self {
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
        self.out.push(PlayerOutput::DeliverToLocalGame { kind, payload: plain });
    }

    /// A message from this player's game, sealed and sent to the host. Dropped (and counted) before the welcome:
    /// the game should not be running before the host has accepted us.
    pub fn local_game_message(&mut self, kind: MessageKind, payload: &[u8]) -> Vec<PlayerOutput> {
        match self.sealer.as_mut().filter(|_| self.state == PlayerState::Joined) {
            Some(sealer) => {
                let frame = sealer.seal(kind.into(), payload);
                self.out.push(PlayerOutput::SendToHost(frame));
            }
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
