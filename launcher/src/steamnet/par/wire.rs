//! The messages of a parallel transfer. Each is one Steam message (reliable, in order per connection). Little-endian.
//!
//! | kind | from | fields after magic and kind |
//! |---|---|---|
//! | 1 hello | sender | xfer u32, ctx u32, total u64, chunk u32, count u32, sha256 [32], lane u16, token [16] |
//! | 2 accept | receiver | xfer u32 |
//! | 3 refuse | receiver | xfer u32, reason u8 |
//! | 4 chunk | sender | xfer u32, index u32, count u32, len u32, payload |
//! | 5 ack | receiver | xfer u32, n u16, n x index u32 |
//! | 6 done | receiver | xfer u32, ok u8 |
//! | 7 abort | both | xfer u32, reason u8 |
//!
//! The token proves that the sender knows the per-transfer secret (derived from the session key): HMAC-SHA-256 over
//! the transfer's fields, cut to 16 bytes. Steam already authenticates the peer's Steam ID; the receiver also checks
//! that.

use sha2::{Digest, Sha256};

pub const MAGIC: u8 = 0xFD;
pub const HELLO: u8 = 1;
pub const ACCEPT: u8 = 2;
pub const REFUSE: u8 = 3;
pub const CHUNK: u8 = 4;
pub const ACK: u8 = 5;
pub const DONE: u8 = 6;
pub const ABORT: u8 = 7;

/// Why a transfer or connection was refused or aborted.
pub mod reason {
    pub const BAD_TOKEN: u8 = 1;
    pub const BUSY: u8 = 2;
    pub const TOO_BIG: u8 = 3;
    pub const MISMATCH: u8 = 4;
    pub const NOT_EXPECTED: u8 = 5;
    pub const BAD_FIELDS: u8 = 6;
    pub const CANCELLED: u8 = 7;
    pub const TIMEOUT: u8 = 8;
    pub const WRITE: u8 = 9;
    pub const TOO_MANY: u8 = 10;
}

pub fn reason_text(code: u8) -> &'static str {
    match code {
        reason::BAD_TOKEN => "wrong secret",
        reason::BUSY => "another transfer is running",
        reason::TOO_BIG => "too big",
        reason::MISMATCH => "does not match the transfer",
        reason::NOT_EXPECTED => "not expected",
        reason::BAD_FIELDS => "bad fields",
        reason::CANCELLED => "cancelled",
        reason::TIMEOUT => "timed out",
        reason::WRITE => "cannot write the file",
        reason::TOO_MANY => "too many connections",
        _ => "unknown reason",
    }
}

/// The header of a chunk message: magic, kind, xfer, index, count, len.
pub const CHUNK_HEADER: usize = 2 + 4 + 4 + 4 + 4;
/// The most indices one ack carries.
pub const ACK_MAX: usize = 1024;
/// Size of a hello message.
pub const HELLO_SIZE: usize = 2 + 4 + 4 + 8 + 4 + 4 + 32 + 2 + 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub xfer: u32,
    pub ctx: u32,
    pub total: u64,
    pub chunk: u32,
    pub count: u32,
    pub sha256: [u8; 32],
    pub lane: u16,
    pub token: [u8; 16],
}

impl Hello {
    /// A hello with the token for `secret`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        secret: &[u8; 32],
        xfer: u32,
        ctx: u32,
        total: u64,
        chunk: u32,
        count: u32,
        sha256: [u8; 32],
        lane: u16,
    ) -> Hello {
        let mut h = Hello { xfer, ctx, total, chunk, count, sha256, lane, token: [0; 16] };
        h.token = h.expected_token(secret);
        h
    }

    /// The token this hello must carry for `secret` (the lane number is not part of it).
    pub fn expected_token(&self, secret: &[u8; 32]) -> [u8; 16] {
        let mut msg = Vec::with_capacity(64);
        msg.extend_from_slice(b"feverlink3 bulk-par-1");
        msg.extend_from_slice(&self.xfer.to_le_bytes());
        msg.extend_from_slice(&self.ctx.to_le_bytes());
        msg.extend_from_slice(&self.total.to_le_bytes());
        msg.extend_from_slice(&self.chunk.to_le_bytes());
        msg.extend_from_slice(&self.count.to_le_bytes());
        msg.extend_from_slice(&self.sha256);
        let full = hmac_sha256(secret, &msg);
        let mut out = [0u8; 16];
        out.copy_from_slice(&full[..16]);
        out
    }

    pub fn token_ok(&self, secret: &[u8; 32]) -> bool {
        let want = self.expected_token(secret);
        want.iter().zip(&self.token).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }
}

/// HMAC-SHA-256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// The per-transfer secret of a session: derived from the session key, so both ends have it and no stranger does.
pub fn secret_from_key(key: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"feverlink3 bulk-par-1 secret");
    h.update(key);
    h.finalize().into()
}

/// The bytes of chunk `index` of a file of `total` bytes in chunks of `chunk`; `None` outside the file.
pub fn chunk_len(total: u64, chunk: u32, index: u32) -> Option<usize> {
    if chunk == 0 {
        return None;
    }
    let start = u64::from(index).checked_mul(u64::from(chunk))?;
    if start >= total {
        return None;
    }
    Some((total - start).min(u64::from(chunk)) as usize)
}

/// How many chunks a file of `total` bytes has.
pub fn chunk_count(total: u64, chunk: u32) -> Option<u32> {
    if chunk == 0 || total == 0 {
        return None;
    }
    u32::try_from(total.div_ceil(u64::from(chunk))).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg<'a> {
    Hello(Hello),
    Accept { xfer: u32 },
    Refuse { xfer: u32, reason: u8 },
    Chunk { xfer: u32, index: u32, count: u32, payload: &'a [u8] },
    Ack { xfer: u32, indices: Vec<u32> },
    Done { xfer: u32, ok: bool },
    Abort { xfer: u32, reason: u8 },
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

/// Parses one message; `None` when it is not exactly a well-formed message.
pub fn parse(d: &[u8]) -> Option<Msg<'_>> {
    if *d.first()? != MAGIC {
        return None;
    }
    let kind = *d.get(1)?;
    let xfer = u32_at(d, 2)?;
    match kind {
        HELLO => {
            if d.len() != HELLO_SIZE {
                return None;
            }
            Some(Msg::Hello(Hello {
                xfer,
                ctx: u32_at(d, 6)?,
                total: u64::from_le_bytes(d.get(10..18)?.try_into().ok()?),
                chunk: u32_at(d, 18)?,
                count: u32_at(d, 22)?,
                sha256: d.get(26..58)?.try_into().ok()?,
                lane: u16::from_le_bytes(d.get(58..60)?.try_into().ok()?),
                token: d.get(60..76)?.try_into().ok()?,
            }))
        }
        ACCEPT if d.len() == 6 => Some(Msg::Accept { xfer }),
        REFUSE if d.len() == 7 => Some(Msg::Refuse { xfer, reason: d[6] }),
        CHUNK => {
            let (index, count, len) = (u32_at(d, 6)?, u32_at(d, 10)?, u32_at(d, 14)? as usize);
            let payload = d.get(CHUNK_HEADER..)?;
            (payload.len() == len && len > 0).then_some(Msg::Chunk { xfer, index, count, payload })
        }
        ACK => {
            let n = u16::from_le_bytes(d.get(6..8)?.try_into().ok()?) as usize;
            if n > ACK_MAX || d.len() != 8 + 4 * n {
                return None;
            }
            let indices = (0..n).map(|i| u32_at(d, 8 + 4 * i)).collect::<Option<Vec<_>>>()?;
            Some(Msg::Ack { xfer, indices })
        }
        DONE if d.len() == 7 => Some(Msg::Done { xfer, ok: d[6] != 0 }),
        ABORT if d.len() == 7 => Some(Msg::Abort { xfer, reason: d[6] }),
        _ => None,
    }
}

pub fn encode_hello(h: &Hello) -> Vec<u8> {
    let mut b = Vec::with_capacity(HELLO_SIZE);
    b.extend_from_slice(&[MAGIC, HELLO]);
    b.extend_from_slice(&h.xfer.to_le_bytes());
    b.extend_from_slice(&h.ctx.to_le_bytes());
    b.extend_from_slice(&h.total.to_le_bytes());
    b.extend_from_slice(&h.chunk.to_le_bytes());
    b.extend_from_slice(&h.count.to_le_bytes());
    b.extend_from_slice(&h.sha256);
    b.extend_from_slice(&h.lane.to_le_bytes());
    b.extend_from_slice(&h.token);
    b
}

fn small(kind: u8, xfer: u32, extra: &[u8]) -> Vec<u8> {
    let mut b = vec![MAGIC, kind];
    b.extend_from_slice(&xfer.to_le_bytes());
    b.extend_from_slice(extra);
    b
}

pub fn encode_accept(xfer: u32) -> Vec<u8> {
    small(ACCEPT, xfer, &[])
}

pub fn encode_refuse(xfer: u32, reason: u8) -> Vec<u8> {
    small(REFUSE, xfer, &[reason])
}

pub fn encode_done(xfer: u32, ok: bool) -> Vec<u8> {
    small(DONE, xfer, &[u8::from(ok)])
}

pub fn encode_abort(xfer: u32, reason: u8) -> Vec<u8> {
    small(ABORT, xfer, &[reason])
}

pub fn encode_ack(xfer: u32, indices: &[u32]) -> Vec<u8> {
    let n = indices.len().min(ACK_MAX);
    let mut b = small(ACK, xfer, &(n as u16).to_le_bytes());
    for i in &indices[..n] {
        b.extend_from_slice(&i.to_le_bytes());
    }
    b
}

/// A chunk message with room for `len` payload bytes (zeroed): the caller fills `buf[CHUNK_HEADER..]`.
pub fn chunk_buffer(xfer: u32, index: u32, count: u32, len: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(CHUNK_HEADER + len);
    b.extend_from_slice(&[MAGIC, CHUNK]);
    b.extend_from_slice(&xfer.to_le_bytes());
    b.extend_from_slice(&index.to_le_bytes());
    b.extend_from_slice(&count.to_le_bytes());
    b.extend_from_slice(&(len as u32).to_le_bytes());
    b.resize(CHUNK_HEADER + len, 0);
    b
}

/// A chunk message: header and payload in one buffer.
pub fn encode_chunk(xfer: u32, index: u32, count: u32, payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(CHUNK_HEADER + payload.len());
    b.extend_from_slice(&[MAGIC, CHUNK]);
    b.extend_from_slice(&xfer.to_le_bytes());
    b.extend_from_slice(&index.to_le_bytes());
    b.extend_from_slice(&count.to_le_bytes());
    b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    b.extend_from_slice(payload);
    b
}
