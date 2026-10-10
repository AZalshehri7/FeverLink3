//! Resync round vocabulary and the receiving side of a savegame transfer. The host side of the round lives in
//! `host/resync_impl.rs`; the wire rules are `docs/PROTOCOL.md`, "Resync round".

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

use super::messages::ResyncChunkMessage;

/// Where a resync round stands. See `docs/PROTOCOL.md`, "Resync round".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResyncState {
    Idle,
    Pausing,
    Saving,
    Transferring,
    Loading,
    Verifying,
}

/// Size limits and timeouts of a round.
#[derive(Debug)]
pub struct ResyncLimits;

impl ResyncLimits {
    /// Largest payload of one ResyncChunk.
    pub const CHUNK_SIZE: usize = 256 * 1024;
    /// Most chunks of one transfer (512 MiB).
    pub const MAX_CHUNKS: u32 = 2048;
    pub const PAUSE_TIMEOUT: f64 = 120.0;
    pub const SAVE_TIMEOUT: f64 = 180.0;
    pub const TRANSFER_TIMEOUT: f64 = 600.0;
    pub const LOAD_TIMEOUT: f64 = 600.0;
    pub const VERIFY_TIMEOUT: f64 = 120.0;
    /// A player forgets a transfer that has not finished this long after its first chunk.
    pub const RECEIVE_TIMEOUT: f64 = 600.0;

    /// Chunks needed for a file of `len` bytes; `None` for an empty file or one over [`Self::MAX_CHUNKS`].
    pub fn chunk_count(len: u64) -> Option<u32> {
        if len == 0 {
            return None;
        }
        let count = len.div_ceil(Self::CHUNK_SIZE as u64);
        u32::try_from(count).ok().filter(|c| *c <= Self::MAX_CHUNKS)
    }

    /// Exact size chunk `index` of a file of `len` bytes must have.
    pub fn chunk_len(len: u64, index: u32) -> Option<usize> {
        let count = Self::chunk_count(len)?;
        if index >= count {
            return None;
        }
        let start = u64::from(index) * Self::CHUNK_SIZE as u64;
        Some((len - start).min(Self::CHUNK_SIZE as u64) as usize)
    }
}

/// Lower-case hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// `Some(lower-case)` when `text` is 64 hex digits.
pub fn normalize_sha256(text: &str) -> Option<String> {
    (text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())).then(|| text.to_ascii_lowercase())
}

/// Why a chunk was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkError {
    BadSha,
    BadCount(i32),
    BadIndex(i32),
    BadSize(usize),
    /// Count or hash differs from the chunks already held for this id.
    Inconsistent,
    /// The assembled bytes hash to something else; the transfer is forgotten.
    HashMismatch,
}

impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadSha => f.write_str("sha256 is not 64 hex digits"),
            Self::BadCount(c) => write!(f, "chunk count {c} outside 1..={}", ResyncLimits::MAX_CHUNKS),
            Self::BadIndex(i) => write!(f, "chunk index {i} outside the count"),
            Self::BadSize(n) => write!(f, "chunk of {n} bytes outside 1..={}", ResyncLimits::CHUNK_SIZE),
            Self::Inconsistent => f.write_str("count or sha256 differs from earlier chunks"),
            Self::HashMismatch => f.write_str("assembled save does not match its sha256"),
        }
    }
}

impl std::error::Error for ChunkError {}

#[derive(Debug)]
struct Transfer {
    id: i32,
    count: usize,
    sha256: String,
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
    started: f64,
}

/// A save fully received and verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedSave {
    pub id: i32,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

/// The receiving side of a transfer: one in progress at a time, a new id replaces the old one.
#[derive(Debug, Default)]
pub struct IncomingSave {
    current: Option<Transfer>,
    done: BTreeSet<(i32, String)>,
}

impl IncomingSave {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes one chunk. `Ok(Some(save))` when it completed a transfer whose hash matches.
    pub fn accept(&mut self, now: f64, m: ResyncChunkMessage) -> Result<Option<ReceivedSave>, ChunkError> {
        let sha = normalize_sha256(&m.sha256).ok_or(ChunkError::BadSha)?;
        let count = usize::try_from(m.count)
            .ok()
            .filter(|c| (1..=ResyncLimits::MAX_CHUNKS as usize).contains(c))
            .ok_or(ChunkError::BadCount(m.count))?;
        let index = usize::try_from(m.index).ok().filter(|i| *i < count).ok_or(ChunkError::BadIndex(m.index))?;
        if m.data.is_empty() || m.data.len() > ResyncLimits::CHUNK_SIZE {
            return Err(ChunkError::BadSize(m.data.len()));
        }
        let t = match &mut self.current {
            Some(t) if t.id == m.id => {
                if t.count != count || t.sha256 != sha {
                    return Err(ChunkError::Inconsistent);
                }
                t
            }
            slot => slot.insert(Transfer {
                id: m.id,
                count,
                sha256: sha,
                chunks: vec![None; count],
                received: 0,
                started: now,
            }),
        };
        if let Some(slot) = t.chunks.get_mut(index).filter(|slot| slot.is_none()) {
            *slot = Some(m.data);
            t.received += 1;
        }
        if t.received < t.count {
            return Ok(None);
        }
        let Some(t) = self.current.take() else {
            return Ok(None);
        };
        // one allocation of the right size: the save can be hundreds of megabytes
        let mut bytes = Vec::with_capacity(t.chunks.iter().flatten().map(Vec::len).sum());
        for chunk in t.chunks.into_iter().flatten() {
            bytes.extend_from_slice(&chunk);
        }
        if sha256_hex(&bytes) != t.sha256 {
            return Err(ChunkError::HashMismatch);
        }
        self.done.insert((t.id, t.sha256.clone()));
        Ok(Some(ReceivedSave { id: t.id, sha256: t.sha256, bytes }))
    }

    /// Drops an unfinished transfer older than [`ResyncLimits::RECEIVE_TIMEOUT`]; returns its id.
    pub fn expire(&mut self, now: f64) -> Option<i32> {
        if self.current.as_ref().is_some_and(|t| now - t.started >= ResyncLimits::RECEIVE_TIMEOUT) {
            return self.current.take().map(|t| t.id);
        }
        None
    }

    /// Whether a save with this id and hash was received completely and verified.
    pub fn has(&self, id: i32, sha256: &str) -> bool {
        normalize_sha256(sha256).is_some_and(|s| self.done.contains(&(id, s)))
    }

    /// Chunks received and chunks in all, of the transfer in progress.
    pub fn progress(&self) -> Option<(usize, usize)> {
        self.current.as_ref().map(|t| (t.received, t.count))
    }

    /// The id of the transfer in progress.
    pub fn in_progress(&self) -> Option<i32> {
        self.current.as_ref().map(|t| t.id)
    }
}
