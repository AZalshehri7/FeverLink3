//! The relay protocol: how a host and its players reach each other through a relay server when neither side can
//! accept a connection. See `docs/RELAY.md`.
//!
//! A relay forwards the frames of [`crate::protocol`] without opening them: they are sealed with a key it never
//! has. Each peer holds one TCP connection to the relay and speaks the small messages defined here on it:
//!
//! - the **host** registers a room ([`RelayMessage::Register`]) and gets one connection to the relay for all its
//!   players; the relay tells it about each joiner as a *virtual connection* with a number
//!   ([`RelayMessage::Open`], [`RelayMessage::HostData`], [`RelayMessage::Close`]), so the host's session code sees
//!   ordinary connections;
//! - a **joiner** names the room ([`RelayMessage::Join`]) and then exchanges plain [`RelayMessage::Data`] with the
//!   host, one complete [`crate::protocol::FrameCodec`] frame per message;
//! - both sides keep the link alive with [`RelayMessage::Ping`] / [`RelayMessage::Pong`] and learn why the relay
//!   refused or ended a connection from [`RelayMessage::Error`].
//!
//! Like [`crate::protocol`] this module does no I/O of its own beyond the `Read` it is handed: [`RelayReader`]
//! keeps a partial message across read timeouts, which is what lets a peer wake up now and then to send a ping.

use std::fmt;
use std::io::{self, Read, Write};

use crate::crypto::SessionKey;
use crate::protocol::{Frame, FrameCodec, FrameHeader};

/// The name of a session on a relay: [`SessionKey::relay_room_id`] of the join code's secret.
pub type RoomId = [u8; SessionKey::ROOM_ID_SIZE];

/// Version carried in [`RelayMessage::Register`] and [`RelayMessage::Join`].
pub const RELAY_PROTOCOL_VERSION: u8 = 1;

/// The port a relay listens on, and the one assumed for a relay address written without a port.
pub const DEFAULT_RELAY_PORT: u16 = 28100;

/// Message type bytes.
mod ty {
    pub const REGISTER: u8 = 1;
    pub const JOIN: u8 = 2;
    pub const REGISTERED: u8 = 3;
    pub const JOINED: u8 = 4;
    pub const ERROR: u8 = 5;
    pub const PING: u8 = 6;
    pub const PONG: u8 = 7;
    pub const DATA: u8 = 16;
    pub const HOST_DATA: u8 = 17;
    pub const OPEN: u8 = 18;
    pub const CLOSE: u8 = 19;
}

/// What the relay asks of the peer it just admitted: ping when you have sent nothing for `ping_secs`, and expect to
/// be dropped when the relay hears nothing from you for `idle_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub ping_secs: u16,
    pub idle_secs: u16,
}

/// Why the relay refused or ended a connection. An open set: a newer relay may send codes this build does not
/// know, which print as `error-<n>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ErrorCode(pub u8);

impl ErrorCode {
    /// Malformed or unexpected message, a message type the peer's role may not send.
    pub const BAD_REQUEST: Self = Self(1);
    pub const BAD_VERSION: Self = Self(2);
    /// Register: a host already holds this room.
    pub const ROOM_TAKEN: Self = Self(3);
    /// Join: no host holds this room (yet, or any more).
    pub const NO_SUCH_ROOM: Self = Self(4);
    pub const ROOM_FULL: Self = Self(5);
    pub const TOO_MANY_ROOMS: Self = Self(6);
    pub const TOO_MANY_CONNECTIONS: Self = Self(7);
    /// One address holds too many connections or rooms.
    pub const IP_LIMIT: Self = Self(8);
    pub const FRAME_TOO_LARGE: Self = Self(9);
    /// The room moved more bytes per second than the relay allows for longer than it will wait.
    pub const RATE_LIMITED: Self = Self(10);
    /// No valid first message, nothing heard for too long, or a joiner that never spoke.
    pub const TIMEOUT: Self = Self(11);
    /// Sent to a joiner when the host's link to the relay ended.
    pub const HOST_GONE: Self = Self(12);
    /// The relay is stopping.
    pub const SHUTDOWN: Self = Self(13);

    pub fn name(self) -> String {
        match self.0 {
            1 => "bad-request".into(),
            2 => "bad-version".into(),
            3 => "room-taken".into(),
            4 => "no-such-room".into(),
            5 => "room-full".into(),
            6 => "too-many-rooms".into(),
            7 => "too-many-connections".into(),
            8 => "ip-limit".into(),
            9 => "frame-too-large".into(),
            10 => "rate-limited".into(),
            11 => "timeout".into(),
            12 => "host-gone".into(),
            13 => "shutdown".into(),
            n => format!("error-{n}"),
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// Why a virtual connection ended, in [`RelayMessage::Close`]. Informational: logged, never acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CloseReason(pub u8);

impl CloseReason {
    /// The joiner closed its connection (or, host to relay, the host dropped the joiner).
    pub const LEFT: Self = Self(0);
    /// The joiner broke the protocol or its connection failed.
    pub const ERROR: Self = Self(1);
    /// Idle or silent for too long.
    pub const TIMEOUT: Self = Self(2);
    /// The relay ended it: a limit, a flood, or shutdown.
    pub const KICKED: Self = Self(3);

    pub fn name(self) -> String {
        match self.0 {
            0 => "left".into(),
            1 => "error".into(),
            2 => "timeout".into(),
            3 => "kicked".into(),
            n => format!("reason-{n}"),
        }
    }
}

impl fmt::Display for CloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// One message on a relay connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayMessage {
    /// Host to relay, first message: hold this room for me.
    Register {
        version: u8,
        room: RoomId,
    },
    /// Joiner to relay, first message: connect me to the host of this room.
    Join {
        version: u8,
        room: RoomId,
    },
    /// Relay to host: the room is yours.
    Registered(Timing),
    /// Relay to joiner: you are connected to the host (it has been told with `Open`).
    Joined(Timing),
    /// Relay to either, then the relay closes the connection.
    Error {
        code: ErrorCode,
        text: String,
    },
    Ping(u64),
    /// Answers a `Ping` with the same token.
    Pong(u64),
    /// Joiner to relay and relay to joiner: exactly one [`FrameCodec`] frame, length prefix included.
    Data(Vec<u8>),
    /// Host to relay and relay to host: the same for the joiner with this virtual connection number.
    HostData {
        vconn: u32,
        frame: Vec<u8>,
    },
    /// Relay to host: a joiner arrived. The host is not told who it is or where from.
    Open {
        vconn: u32,
    },
    /// Either way: this virtual connection is over. Host to relay it makes the relay close the joiner.
    Close {
        vconn: u32,
        reason: CloseReason,
    },
}

impl RelayMessage {
    pub fn error(code: ErrorCode, text: &str) -> Self {
        Self::Error { code, text: text.to_owned() }
    }

    /// For logs: the message without the bytes of a data frame.
    pub fn summary(&self) -> String {
        match self {
            Self::Register { .. } => "Register".into(),
            Self::Join { .. } => "Join".into(),
            Self::Registered(_) => "Registered".into(),
            Self::Joined(_) => "Joined".into(),
            Self::Error { code, text } => format!("Error {code} {text}"),
            Self::Ping(n) => format!("Ping {n}"),
            Self::Pong(n) => format!("Pong {n}"),
            Self::Data(d) => format!("Data {} bytes", d.len()),
            Self::HostData { vconn, frame } => format!("HostData {vconn} {} bytes", frame.len()),
            Self::Open { vconn } => format!("Open {vconn}"),
            Self::Close { vconn, reason } => format!("Close {vconn} {reason}"),
        }
    }
}

/// The bytes on the wire are not a relay message. The connection must be closed.
#[derive(Debug)]
pub enum RelayError {
    /// The length field is zero: a message has at least a type byte.
    LengthZero,
    /// The length field exceeds [`RelayCodec::MAX_LENGTH`]; nothing was allocated for it.
    LengthOverLimit(usize),
    /// The stream ended inside a message.
    Truncated,
    UnknownType(u8),
    /// A body of the wrong size or content for its type.
    Malformed(&'static str),
    /// A data message whose frame is not exactly one well-formed [`FrameCodec`] frame.
    BadFrame(&'static str),
    /// A data message whose frame is longer than [`FrameCodec::MAX_FRAME_LENGTH`].
    FrameTooLarge(usize),
    Io(io::Error),
}

impl RelayError {
    /// A read that timed out or would block: nothing is wrong, the partial message is kept, call again.
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Io(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
    }

    /// What a relay tells a peer that sent this.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::LengthOverLimit(_) | Self::FrameTooLarge(_) => ErrorCode::FRAME_TOO_LARGE,
            _ => ErrorCode::BAD_REQUEST,
        }
    }
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LengthZero => f.write_str("relay message of length 0"),
            Self::LengthOverLimit(n) => {
                write!(f, "relay message of {n} bytes exceeds the {} byte limit", RelayCodec::MAX_LENGTH)
            }
            Self::Truncated => f.write_str("stream ended inside a relay message"),
            Self::UnknownType(t) => write!(f, "unknown relay message type {t}"),
            Self::Malformed(what) => write!(f, "malformed relay message: {what}"),
            Self::BadFrame(what) => write!(f, "data message does not hold one frame: {what}"),
            Self::FrameTooLarge(n) => {
                write!(f, "frame of {n} bytes exceeds the {} byte limit", FrameCodec::MAX_FRAME_LENGTH)
            }
            Self::Io(e) => write!(f, "stream error: {e}"),
        }
    }
}

impl std::error::Error for RelayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for RelayError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Wire format of a relay message: `u32 length` (big-endian, counts everything after itself), `u8 type`, body.
/// The length is bounded by [`RelayCodec::MAX_LENGTH`]: a peer cannot make the reader allocate without limit.
#[derive(Debug)]
pub struct RelayCodec;

impl RelayCodec {
    /// Largest message after the length field. A [`FrameCodec`] frame of the largest size, with its own length
    /// prefix, a type byte and a virtual connection number fit with room to spare.
    pub const MAX_LENGTH: usize = FrameCodec::MAX_FRAME_LENGTH + 16;
    pub const LENGTH_FIELD_SIZE: usize = 4;
    /// Longest text the encoder puts in an `Error`; longer text is cut at a character boundary.
    pub const MAX_ERROR_TEXT: usize = 200;
    /// Longest text the decoder accepts in an `Error`.
    const MAX_ERROR_TEXT_IN: usize = 1024;

    fn start(kind: u8, body_len: usize) -> Result<Vec<u8>, RelayError> {
        let length = 1 + body_len;
        if length > Self::MAX_LENGTH {
            return Err(RelayError::LengthOverLimit(length));
        }
        let mut out = Vec::with_capacity(Self::LENGTH_FIELD_SIZE + length);
        out.extend_from_slice(&(length as u32).to_be_bytes());
        out.push(kind);
        Ok(out)
    }

    pub fn encode(message: &RelayMessage) -> Result<Vec<u8>, RelayError> {
        match message {
            RelayMessage::Register { version, room } => Self::room_message(ty::REGISTER, *version, room),
            RelayMessage::Join { version, room } => Self::room_message(ty::JOIN, *version, room),
            RelayMessage::Registered(t) => Self::timing_message(ty::REGISTERED, *t),
            RelayMessage::Joined(t) => Self::timing_message(ty::JOINED, *t),
            RelayMessage::Error { code, text } => {
                let mut end = text.len().min(Self::MAX_ERROR_TEXT);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                let mut out = Self::start(ty::ERROR, 1 + end)?;
                out.push(code.0);
                out.extend_from_slice(&text.as_bytes()[..end]);
                Ok(out)
            }
            RelayMessage::Ping(token) => Self::token_message(ty::PING, *token),
            RelayMessage::Pong(token) => Self::token_message(ty::PONG, *token),
            RelayMessage::Data(frame) => {
                check_frame(frame)?;
                let mut out = Self::start(ty::DATA, frame.len())?;
                out.extend_from_slice(frame);
                Ok(out)
            }
            RelayMessage::HostData { vconn, frame } => {
                check_frame(frame)?;
                let mut out = Self::start(ty::HOST_DATA, 4 + frame.len())?;
                out.extend_from_slice(&vconn.to_be_bytes());
                out.extend_from_slice(frame);
                Ok(out)
            }
            RelayMessage::Open { vconn } => {
                let mut out = Self::start(ty::OPEN, 4)?;
                out.extend_from_slice(&vconn.to_be_bytes());
                Ok(out)
            }
            RelayMessage::Close { vconn, reason } => {
                let mut out = Self::start(ty::CLOSE, 5)?;
                out.extend_from_slice(&vconn.to_be_bytes());
                out.push(reason.0);
                Ok(out)
            }
        }
    }

    fn room_message(kind: u8, version: u8, room: &RoomId) -> Result<Vec<u8>, RelayError> {
        let mut out = Self::start(kind, 1 + room.len())?;
        out.push(version);
        out.extend_from_slice(room);
        Ok(out)
    }

    fn timing_message(kind: u8, t: Timing) -> Result<Vec<u8>, RelayError> {
        let mut out = Self::start(kind, 4)?;
        out.extend_from_slice(&t.ping_secs.to_be_bytes());
        out.extend_from_slice(&t.idle_secs.to_be_bytes());
        Ok(out)
    }

    fn token_message(kind: u8, token: u64) -> Result<Vec<u8>, RelayError> {
        let mut out = Self::start(kind, 8)?;
        out.extend_from_slice(&token.to_be_bytes());
        Ok(out)
    }

    /// A `Data` message for a frame, built in one allocation (the hot path of a joiner).
    pub fn encode_data(frame: &Frame) -> Result<Vec<u8>, RelayError> {
        let inner = FrameCodec::encode_frame(frame).map_err(|_| RelayError::FrameTooLarge(frame.body.len()))?;
        let mut out = Self::start(ty::DATA, inner.len())?;
        out.extend_from_slice(&inner);
        Ok(out)
    }

    /// A `HostData` message for a frame to the joiner `vconn` (the hot path of a host).
    pub fn encode_host_data(vconn: u32, frame: &Frame) -> Result<Vec<u8>, RelayError> {
        let inner = FrameCodec::encode_frame(frame).map_err(|_| RelayError::FrameTooLarge(frame.body.len()))?;
        let mut out = Self::start(ty::HOST_DATA, 4 + inner.len())?;
        out.extend_from_slice(&vconn.to_be_bytes());
        out.extend_from_slice(&inner);
        Ok(out)
    }

    /// Writes one message.
    pub fn write<W: Write + ?Sized>(stream: &mut W, message: &RelayMessage) -> Result<(), RelayError> {
        stream.write_all(&Self::encode(message)?)?;
        Ok(())
    }

    /// The frame inside a `Data` or `HostData` message.
    pub fn decode_frame(bytes: &[u8]) -> Result<Frame, RelayError> {
        check_frame(bytes)?;
        match FrameCodec::try_decode(bytes) {
            Ok(Some((frame, _))) => Ok(frame),
            _ => Err(RelayError::BadFrame("not a frame")),
        }
    }

    /// Decodes one complete message from the front of `buffer`. `Ok(None)` when the buffer does not yet hold a
    /// whole message; otherwise the message and the number of bytes it consumed.
    pub fn try_decode(buffer: &[u8]) -> Result<Option<(RelayMessage, usize)>, RelayError> {
        let Some(head) = buffer.get(..Self::LENGTH_FIELD_SIZE) else { return Ok(None) };
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if length == 0 {
            return Err(RelayError::LengthZero);
        }
        if length > Self::MAX_LENGTH {
            return Err(RelayError::LengthOverLimit(length));
        }
        let total = Self::LENGTH_FIELD_SIZE + length;
        let Some(message) = buffer.get(Self::LENGTH_FIELD_SIZE..total) else { return Ok(None) };
        let (kind, body) = (message[0], &message[1..]);
        Ok(Some((Self::decode_body(kind, body)?, total)))
    }

    fn decode_body(kind: u8, body: &[u8]) -> Result<RelayMessage, RelayError> {
        let exact =
            |n: usize, what: &'static str| if body.len() == n { Ok(()) } else { Err(RelayError::Malformed(what)) };
        Ok(match kind {
            ty::REGISTER | ty::JOIN => {
                exact(1 + SessionKey::ROOM_ID_SIZE, "register or join needs a version and a 16 byte room")?;
                let room: RoomId = body[1..].try_into().map_err(|_| RelayError::Malformed("room"))?;
                if kind == ty::REGISTER {
                    RelayMessage::Register { version: body[0], room }
                } else {
                    RelayMessage::Join { version: body[0], room }
                }
            }
            ty::REGISTERED | ty::JOINED => {
                exact(4, "registered or joined needs two u16")?;
                let t = Timing {
                    ping_secs: u16::from_be_bytes([body[0], body[1]]),
                    idle_secs: u16::from_be_bytes([body[2], body[3]]),
                };
                if kind == ty::REGISTERED {
                    RelayMessage::Registered(t)
                } else {
                    RelayMessage::Joined(t)
                }
            }
            ty::ERROR => {
                let (code, text) = body.split_first().ok_or(RelayError::Malformed("error needs a code"))?;
                if text.len() > Self::MAX_ERROR_TEXT_IN {
                    return Err(RelayError::Malformed("error text too long"));
                }
                RelayMessage::Error { code: ErrorCode(*code), text: String::from_utf8_lossy(text).into_owned() }
            }
            ty::PING | ty::PONG => {
                exact(8, "ping or pong needs a u64")?;
                let token = u64::from_be_bytes(body.try_into().map_err(|_| RelayError::Malformed("token"))?);
                if kind == ty::PING {
                    RelayMessage::Ping(token)
                } else {
                    RelayMessage::Pong(token)
                }
            }
            ty::DATA => {
                check_frame(body)?;
                RelayMessage::Data(body.to_vec())
            }
            ty::HOST_DATA => {
                let (vconn, frame) =
                    body.split_at_checked(4).ok_or(RelayError::Malformed("host data needs a vconn"))?;
                check_frame(frame)?;
                RelayMessage::HostData {
                    vconn: u32::from_be_bytes(vconn.try_into().map_err(|_| RelayError::Malformed("vconn"))?),
                    frame: frame.to_vec(),
                }
            }
            ty::OPEN => {
                exact(4, "open needs a vconn")?;
                RelayMessage::Open {
                    vconn: u32::from_be_bytes(body.try_into().map_err(|_| RelayError::Malformed("vconn"))?),
                }
            }
            ty::CLOSE => {
                exact(5, "close needs a vconn and a reason")?;
                RelayMessage::Close {
                    vconn: u32::from_be_bytes([body[0], body[1], body[2], body[3]]),
                    reason: CloseReason(body[4]),
                }
            }
            other => return Err(RelayError::UnknownType(other)),
        })
    }
}

/// A data message must hold exactly one frame as [`FrameCodec`] writes it: a length prefix that matches the rest,
/// at least a header, at most the frame limit. Nothing else about the frame is looked at; it is sealed.
fn check_frame(bytes: &[u8]) -> Result<(), RelayError> {
    let Some(prefix) = bytes.get(..FrameCodec::LENGTH_FIELD_SIZE) else {
        return Err(RelayError::BadFrame("shorter than a length prefix"));
    };
    let declared = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
    if declared > FrameCodec::MAX_FRAME_LENGTH {
        return Err(RelayError::FrameTooLarge(declared));
    }
    if declared < FrameHeader::SIZE {
        return Err(RelayError::BadFrame("shorter than a frame header"));
    }
    if declared != bytes.len() - FrameCodec::LENGTH_FIELD_SIZE {
        return Err(RelayError::BadFrame("length prefix does not match the data"));
    }
    Ok(())
}

/// What [`RelayReader::step`] came to.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadStep {
    Message(RelayMessage),
    /// A read brought bytes but not yet a whole message.
    More,
    /// The stream ended between two messages.
    End,
}

/// Reads relay messages from a stream, keeping what it has read but not yet used. A read that times out (a socket
/// with a read timeout) returns [`RelayError::Io`] with a `WouldBlock` or `TimedOut` kind and loses nothing: call
/// again after doing whatever the timeout was for, such as sending a ping.
///
/// A timeout does not come while bytes keep arriving, so a peer that sends one byte every so often never gets one.
/// A server that must not wait for such a peer drives the reader with [`RelayReader::step`], which returns after
/// every read, and keeps its own clock.
#[derive(Debug, Default)]
pub struct RelayReader {
    buf: Vec<u8>,
    start: usize,
}

impl RelayReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes read from the stream but not yet returned as messages.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }

    /// One step of reading: decodes a message that is already buffered, else does at most one read from the stream
    /// and tries again. Returns after every read, so a caller can look at its clocks while a peer trickles a
    /// message in a byte at a time; [`RelayReader::read_message`] is the loop for callers that have no such clocks.
    pub fn step<R: Read + ?Sized>(&mut self, stream: &mut R) -> Result<ReadStep, RelayError> {
        if let Some(message) = self.take_buffered()? {
            return Ok(ReadStep::Message(message));
        }
        if self.fill(stream)? == 0 {
            return if self.buffered() == 0 { Ok(ReadStep::End) } else { Err(RelayError::Truncated) };
        }
        Ok(match self.take_buffered()? {
            Some(message) => ReadStep::Message(message),
            None => ReadStep::More,
        })
    }

    /// The next message. `Ok(None)` on a clean end of stream (nothing of a next message read); an error on a
    /// message that cannot be decoded or a stream that ends inside one. Keeps reading until one of those happens,
    /// however slowly the bytes come: use [`RelayReader::step`] where the time matters.
    pub fn read_message<R: Read + ?Sized>(&mut self, stream: &mut R) -> Result<Option<RelayMessage>, RelayError> {
        loop {
            match self.step(stream)? {
                ReadStep::Message(message) => return Ok(Some(message)),
                ReadStep::More => {}
                ReadStep::End => return Ok(None),
            }
        }
    }

    /// The first buffered message, if the buffer holds a whole one.
    fn take_buffered(&mut self) -> Result<Option<RelayMessage>, RelayError> {
        let Some((message, used)) = RelayCodec::try_decode(&self.buf[self.start..])? else { return Ok(None) };
        self.start += used;
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
            if self.buf.capacity() > 64 * 1024 {
                self.buf = Vec::new(); // a large frame went by: do not hold its memory while idle
            }
        }
        Ok(Some(message))
    }

    /// One read into the buffer; the number of bytes it brought, 0 at the end of the stream. A read that fails
    /// (a timeout, say) leaves the buffer as it was.
    fn fill<R: Read + ?Sized>(&mut self, stream: &mut R) -> Result<usize, RelayError> {
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        // read what the message in progress still needs (at least a little), never more than a quarter MiB
        let want = match self.buf.get(..RelayCodec::LENGTH_FIELD_SIZE) {
            Some(h) => {
                let total = RelayCodec::LENGTH_FIELD_SIZE + u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
                total.saturating_sub(self.buf.len()).clamp(512, 256 * 1024)
            }
            None => 4096,
        };
        let old = self.buf.len();
        self.buf.resize(old + want, 0);
        let read = loop {
            match stream.read(&mut self.buf[old..]) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => break other,
            }
        };
        match read {
            Ok(n) => {
                self.buf.truncate(old + n);
                Ok(n)
            }
            Err(e) => {
                self.buf.truncate(old);
                Err(RelayError::Io(e))
            }
        }
    }
}
