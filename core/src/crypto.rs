//! Session key derivation and the sealed channel. See `docs/PROTOCOL.md`, "Key" and "Sealed frames".

use std::collections::HashMap;
use std::fmt;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::Aes256Gcm;
use sha2::Sha256;

use crate::protocol::{Frame, FrameHeader};

/// The one symmetric key of a session, derived from the join code's secret and the optional lobby password.
///
/// `ikm = secret ‖ PBKDF2-HMAC-SHA256(password, salt = secret, 600 000 iterations, 32 bytes)` when a password is
/// set, else `ikm = secret`; `key = HKDF-SHA256(ikm, salt = "FeverLink3 v1", info = "session", 32)`.
///
/// The stretching makes a leaked code without its password expensive to brute-force. No forward secrecy: the key
/// lives for the session, as the architecture document states.
#[derive(Clone)]
pub struct SessionKey {
    key: [u8; Self::KEY_SIZE],
}

impl SessionKey {
    pub const SECRET_SIZE: usize = 16;
    pub const KEY_SIZE: usize = 32;
    pub const PASSWORD_ITERATIONS: u32 = 600_000;

    pub const ROOM_ID_SIZE: usize = 16;

    const SALT: &'static [u8] = b"FeverLink3 v1";
    const INFO: &'static [u8] = b"session";
    const ROOM_INFO: &'static [u8] = b"relay room";

    /// 16 random bytes for a new session. Panics only if the operating system's random source fails.
    pub fn new_secret() -> [u8; Self::SECRET_SIZE] {
        let mut secret = [0u8; Self::SECRET_SIZE];
        getrandom::getrandom(&mut secret).expect("operating system random source");
        secret
    }

    /// `None` and `Some("")` both mean no password.
    pub fn derive(secret: &[u8; Self::SECRET_SIZE], password: Option<&str>) -> Self {
        let mut ikm = Vec::with_capacity(Self::SECRET_SIZE + Self::KEY_SIZE);
        ikm.extend_from_slice(secret);
        if let Some(password) = password.filter(|p| !p.is_empty()) {
            let mut stretched = [0u8; Self::KEY_SIZE];
            pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), secret, Self::PASSWORD_ITERATIONS, &mut stretched);
            ikm.extend_from_slice(&stretched);
        }
        let mut key = [0u8; Self::KEY_SIZE];
        hkdf::Hkdf::<Sha256>::new(Some(Self::SALT), &ikm)
            .expand(Self::INFO, &mut key)
            .expect("32 bytes is a valid HKDF output length");
        Self { key }
    }

    /// 32 key bytes. Treat as secret; never log.
    pub fn key(&self) -> &[u8; Self::KEY_SIZE] {
        &self.key
    }

    /// The name of a session on a relay server (`docs/RELAY.md`): `HKDF-SHA256(secret, salt = "FeverLink3 v1",
    /// info = "relay room", 16 bytes)`. Everyone who holds the join code derives the same 128 bits, so the code
    /// does not have to carry them, and the relay never learns the secret: HKDF with another info label is a
    /// one-way function of it, independent of the session key. It needs the secret only, not the lobby password,
    /// because the joiner knows the code before it knows whether a password is wanted.
    pub fn relay_room_id(secret: &[u8; Self::SECRET_SIZE]) -> [u8; Self::ROOM_ID_SIZE] {
        let mut id = [0u8; Self::ROOM_ID_SIZE];
        hkdf::Hkdf::<Sha256>::new(Some(Self::SALT), secret)
            .expand(Self::ROOM_INFO, &mut id)
            .expect("16 bytes is a valid HKDF output length");
        id
    }
}

impl fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionKey(..)")
    }
}

/// Why a frame was refused by [`Opener::try_open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpenError {
    /// Header version is not the one this build speaks.
    BadVersion,
    /// Body shorter than a GCM tag.
    TooShort,
    /// Counter already accepted from this sender, or older than the replay window.
    Replay,
    /// Authentication failed: wrong key, altered header or body.
    Tamper,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BadVersion => "BadVersion",
            Self::TooShort => "TooShort",
            Self::Replay => "Replay",
            Self::Tamper => "Tamper",
        })
    }
}

impl std::error::Error for OpenError {}

/// Seals outgoing frames for one sender: AES-256-GCM, 16-byte tag, nonce = `u32 sender ‖ u64 counter`. The counter
/// starts at 1 and only grows, so a nonce is never reused under a key as long as every sender id is unique in the
/// session, which the host guarantees by assigning them.
pub struct Sealer {
    aes: Aes256Gcm,
    sender: u16,
    counter: u64,
}

impl Sealer {
    pub const NONCE_SIZE: usize = 12;
    pub const TAG_SIZE: usize = 16;

    pub fn new(key: &SessionKey, sender: u16) -> Self {
        Self { aes: cipher(key), sender, counter: 0 }
    }

    pub fn sender(&self) -> u16 {
        self.sender
    }

    pub fn last_counter(&self) -> u64 {
        self.counter
    }

    pub fn seal(&mut self, kind: u8, plaintext: &[u8]) -> Frame {
        self.counter += 1;
        let header = FrameHeader::new(FrameHeader::CURRENT_VERSION, kind, self.sender, self.counter);
        let nonce = nonce(self.sender, self.counter);
        let aad = header.to_bytes();
        let body = self
            .aes
            .encrypt((&nonce).into(), Payload { msg: plaintext, aad: &aad })
            .expect("AES-GCM encryption cannot fail for an in-memory message");
        Frame::new(header, body)
    }
}

impl fmt::Debug for Sealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealer").field("sender", &self.sender).field("counter", &self.counter).finish()
    }
}

/// Opens incoming frames from any sender and refuses replays. A frame is authenticated before its counter is
/// recorded, so a forged frame cannot poison the window.
pub struct Opener {
    aes: Aes256Gcm,
    windows: HashMap<u16, ReplayWindow>,
}

impl Opener {
    pub fn new(key: &SessionKey) -> Self {
        Self { aes: cipher(key), windows: HashMap::new() }
    }

    pub fn try_open(&mut self, frame: &Frame) -> Result<Vec<u8>, OpenError> {
        let h = &frame.header;
        if h.version != FrameHeader::CURRENT_VERSION {
            return Err(OpenError::BadVersion);
        }
        if frame.body.len() < Sealer::TAG_SIZE {
            return Err(OpenError::TooShort);
        }
        let window = self.windows.entry(h.sender).or_default();
        if !window.is_fresh(h.counter) {
            return Err(OpenError::Replay);
        }
        let nonce = nonce(h.sender, h.counter);
        let aad = h.to_bytes();
        let plaintext = self
            .aes
            .decrypt((&nonce).into(), Payload { msg: &frame.body, aad: &aad })
            .map_err(|_| OpenError::Tamper)?;
        window.accept(h.counter).expect("freshness was checked above");
        Ok(plaintext)
    }
}

impl fmt::Debug for Opener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opener").field("windows", &self.windows).finish()
    }
}

fn cipher(key: &SessionKey) -> Aes256Gcm {
    Aes256Gcm::new_from_slice(key.key()).expect("32-byte key")
}

fn nonce(sender: u16, counter: u64) -> [u8; Sealer::NONCE_SIZE] {
    let mut n = [0u8; Sealer::NONCE_SIZE];
    n[..4].copy_from_slice(&u32::from(sender).to_be_bytes());
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}

/// Sliding window of the last 64 counters accepted from one sender (RFC 6479 style bitmap).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayWindow {
    /// Highest counter accepted; 0 = none yet.
    highest: u64,
    /// Bit k set = counter `highest - k` accepted.
    bitmap: u64,
}

impl ReplayWindow {
    pub const SIZE: u64 = 64;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn highest(&self) -> u64 {
        self.highest
    }

    pub fn is_fresh(&self, counter: u64) -> bool {
        if counter == 0 {
            return false; // counters start at 1
        }
        if counter > self.highest {
            return true;
        }
        let back = self.highest - counter;
        if back >= Self::SIZE {
            return false; // older than the window: refused
        }
        (self.bitmap & (1u64 << back)) == 0
    }

    /// Records a counter. `Err(OpenError::Replay)` when it is not fresh.
    pub fn accept(&mut self, counter: u64) -> Result<(), OpenError> {
        if !self.is_fresh(counter) {
            return Err(OpenError::Replay);
        }
        if counter > self.highest {
            let shift = counter - self.highest;
            self.bitmap = if shift >= Self::SIZE { 0 } else { self.bitmap << shift };
            self.highest = counter;
            self.bitmap |= 1;
        } else {
            self.bitmap |= 1u64 << (self.highest - counter);
        }
        Ok(())
    }
}
