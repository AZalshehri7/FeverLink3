//! Frame header and length-prefixed wire format. See `docs/PROTOCOL.md`, "Frame".

use std::fmt;
use std::io::{self, Read, Write};

/// The 12 header bytes of every frame. They travel in clear and are authenticated as the GCM additional data,
/// so a receiver can route on `kind` and `sender` before decrypting, yet neither can be altered without detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameHeader {
    /// Protocol version; [`FrameHeader::CURRENT_VERSION`] today.
    pub version: u8,
    /// Message kind (see `docs/PROTOCOL.md`).
    pub kind: u8,
    /// Sender id assigned by the host at join; 0 is the host itself.
    pub sender: u16,
    /// Per-sender counter, strictly increasing. Nonce material and replay protection.
    pub counter: u64,
}

impl FrameHeader {
    pub const CURRENT_VERSION: u8 = 1;
    pub const SIZE: usize = 1 + 1 + 2 + 8;

    pub const fn new(version: u8, kind: u8, sender: u16, counter: u64) -> Self {
        Self { version, kind, sender, counter }
    }

    /// Writes the 12 bytes, big-endian. Panics when `dst` is shorter: that is a programming error, not input.
    pub fn write(&self, dst: &mut [u8]) {
        assert!(dst.len() >= Self::SIZE, "header needs 12 bytes");
        dst[0] = self.version;
        dst[1] = self.kind;
        dst[2..4].copy_from_slice(&self.sender.to_be_bytes());
        dst[4..12].copy_from_slice(&self.counter.to_be_bytes());
    }

    /// Reads a header from the first 12 bytes of `src`. Panics when `src` is shorter.
    pub fn read(src: &[u8]) -> Self {
        assert!(src.len() >= Self::SIZE, "header needs 12 bytes");
        Self {
            version: src[0],
            kind: src[1],
            sender: u16::from_be_bytes([src[2], src[3]]),
            counter: u64::from_be_bytes(src[4..12].try_into().expect("8 bytes")),
        }
    }

    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        self.write(&mut b);
        b
    }
}

/// A frame as read from the wire: header plus body. The body is ciphertext and tag for a sealed frame, plaintext
/// for the unsealed handshake kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: FrameHeader,
    pub body: Vec<u8>,
}

impl Frame {
    pub fn new(header: FrameHeader, body: Vec<u8>) -> Self {
        Self { header, body }
    }
}

/// The bytes on the wire are not a frame of this protocol. The connection must be closed.
#[derive(Debug)]
pub enum ProtocolError {
    /// The length field is smaller than a header.
    LengthBelowHeader(u32),
    /// The length field, or a frame to encode, exceeds [`FrameCodec::MAX_FRAME_LENGTH`].
    LengthOverLimit(usize),
    /// The stream ended inside a frame.
    Truncated,
    /// The stream failed.
    Io(io::Error),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LengthBelowHeader(n) => write!(f, "frame length {n} is shorter than a header"),
            Self::LengthOverLimit(n) => {
                write!(f, "frame of {n} bytes exceeds the {} byte limit", FrameCodec::MAX_FRAME_LENGTH)
            }
            Self::Truncated => f.write_str("stream ended inside a frame"),
            Self::Io(e) => write!(f, "stream error: {e}"),
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ProtocolError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Self::Truncated
        } else {
            Self::Io(e)
        }
    }
}

/// Wire format: `u32 length` (big-endian, counts everything after itself) then header then body. The length is
/// bounded by [`FrameCodec::MAX_FRAME_LENGTH`]: a peer cannot make the reader allocate without limit.
#[derive(Debug)]
pub struct FrameCodec;

impl FrameCodec {
    /// Largest frame after the length field: header plus body. 1 MiB holds a 256 KiB savegame chunk with room to
    /// spare and is far above any control message.
    pub const MAX_FRAME_LENGTH: usize = 1 << 20;
    pub const LENGTH_FIELD_SIZE: usize = 4;

    fn check_length(length: u32) -> Result<usize, ProtocolError> {
        let n = length as usize;
        if n < FrameHeader::SIZE {
            return Err(ProtocolError::LengthBelowHeader(length));
        }
        if n > Self::MAX_FRAME_LENGTH {
            return Err(ProtocolError::LengthOverLimit(n));
        }
        Ok(n)
    }

    pub fn encode(header: &FrameHeader, body: &[u8]) -> Result<Vec<u8>, ProtocolError> {
        let length = FrameHeader::SIZE + body.len();
        if length > Self::MAX_FRAME_LENGTH {
            return Err(ProtocolError::LengthOverLimit(length));
        }
        let mut buf = Vec::with_capacity(Self::LENGTH_FIELD_SIZE + length);
        buf.extend_from_slice(&(length as u32).to_be_bytes());
        buf.extend_from_slice(&header.to_bytes());
        buf.extend_from_slice(body);
        Ok(buf)
    }

    pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, ProtocolError> {
        Self::encode(&frame.header, &frame.body)
    }

    /// Decodes one complete length-prefixed frame from the front of `buffer`. `Ok(None)` when the buffer does not
    /// yet hold a whole frame; otherwise the frame and the number of bytes it consumed.
    pub fn try_decode(buffer: &[u8]) -> Result<Option<(Frame, usize)>, ProtocolError> {
        if buffer.len() < Self::LENGTH_FIELD_SIZE {
            return Ok(None);
        }
        let length = Self::check_length(u32::from_be_bytes(buffer[..4].try_into().expect("4 bytes")))?;
        let total = Self::LENGTH_FIELD_SIZE + length;
        if buffer.len() < total {
            return Ok(None);
        }
        let header = FrameHeader::read(&buffer[Self::LENGTH_FIELD_SIZE..]);
        let body = buffer[Self::LENGTH_FIELD_SIZE + FrameHeader::SIZE..total].to_vec();
        Ok(Some((Frame::new(header, body), total)))
    }

    pub fn write<W: Write + ?Sized>(stream: &mut W, frame: &Frame) -> Result<(), ProtocolError> {
        stream.write_all(&Self::encode_frame(frame)?)?;
        Ok(())
    }

    /// Reads one frame. `Ok(None)` on a clean end of stream (no byte of a next frame read); an error on a bad
    /// length or a stream that ends inside a frame.
    pub fn read<R: Read + ?Sized>(stream: &mut R) -> Result<Option<Frame>, ProtocolError> {
        let mut len_buf = [0u8; Self::LENGTH_FIELD_SIZE];
        let got = read_up_to(stream, &mut len_buf)?;
        if got == 0 {
            return Ok(None);
        }
        if got < Self::LENGTH_FIELD_SIZE {
            return Err(ProtocolError::Truncated);
        }
        let length = Self::check_length(u32::from_be_bytes(len_buf))?;
        let mut rest = vec![0u8; length];
        stream.read_exact(&mut rest)?;
        let header = FrameHeader::read(&rest);
        rest.drain(..FrameHeader::SIZE);
        Ok(Some(Frame::new(header, rest)))
    }
}

fn read_up_to<R: Read + ?Sized>(stream: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match stream.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}
