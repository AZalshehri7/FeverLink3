//! Session key derivation and the sealed channel. See `docs/PROTOCOL.md`, "Key" and "Sealed frames".

use std::collections::HashMap;
use std::fmt;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::Aes256Gcm;
use sha2::Sha256;

use crate::protocol::{Frame, FrameCodec, FrameHeader};

/// The one symmetric key of a session, derived from the join code's secret and the optional lobby password.
///
/// `ikm = secret ‖ PBKDF2-HMAC-SHA256(password, salt = secret, 600 000 iterations, 32 bytes)` when a password is
/// set, else `ikm = secret`; `key = HKDF-SHA256(ikm, salt = "FeverLink3 v1", info = "session", 32)`.
///
/// The stretching makes a leaked code without its password expensive to brute-force. No forward secrecy: the key
/// lives for the session, as the architecture document states. A secret is used for one session only: a second
/// session under the same key would restart the host's frame counter and reuse nonces.
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

    /// As [`SessionKey::try_new_secret`], for tests and tools; panics if the random source fails. Code that runs in
    /// the game process uses the fallible form.
    pub fn new_secret() -> [u8; Self::SECRET_SIZE] {
        Self::try_new_secret().expect("operating system random source")
    }

    /// As [`SessionKey::new_secret`], but `None` when the random source fails instead of a panic: for code that runs
    /// inside the game, where a panic ends the process.
    pub fn try_new_secret() -> Option<[u8; Self::SECRET_SIZE]> {
        let mut secret = [0u8; Self::SECRET_SIZE];
        getrandom::getrandom(&mut secret).ok()?;
        Some(secret)
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
        // cannot fail: HKDF-SHA256 yields up to 255 × 32 bytes and 32 are asked for
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

/// Why [`Sealer::try_seal`] produced no frame. None of these happens with the messages of this protocol; they exist
/// so that the session code can drop a message instead of panicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SealError {
    /// The payload does not fit in one frame ([`Sealer::MAX_PAYLOAD`]).
    TooLong(usize),
    /// 2^64 - 1 frames were sealed under this sender id; one more would reuse a nonce.
    CounterExhausted,
    /// The cipher refused the message (it only does so beyond 2^36 bytes).
    Cipher,
}

impl fmt::Display for SealError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong(n) => {
                write!(f, "payload of {n} bytes does not fit in a frame (at most {})", Sealer::MAX_PAYLOAD)
            }
            Self::CounterExhausted => f.write_str("the frame counter is exhausted"),
            Self::Cipher => f.write_str("the cipher refused the message"),
        }
    }
}

impl std::error::Error for SealError {}

/// Seals outgoing frames for one sender: AES-256-GCM, 16-byte tag, nonce = `u32 sender ‖ u64 counter`. The counter
/// starts at 1 and only grows.
///
/// A nonce must never be used twice under one key, so: a new session has a new secret, hence a new key; a sender id
/// that is given out again (a player coming back inside the host's window) gets a new epoch, which starts its
/// counter above everything an earlier use of the id can reach ([`Sealer::with_epoch`]); a sealer that would run
/// out of counters refuses instead of wrapping.
pub struct Sealer {
    aes: Aes256Gcm,
    sender: u16,
    counter: u64,
}

impl Sealer {
    pub const NONCE_SIZE: usize = 12;
    pub const TAG_SIZE: usize = 16;
    /// The longest payload that still fits in a frame once header and tag are added.
    pub const MAX_PAYLOAD: usize = FrameCodec::MAX_FRAME_LENGTH - FrameHeader::SIZE - Self::TAG_SIZE;
    /// A sender's counter lives in `epoch << EPOCH_SHIFT ..`: the low 40 bits count frames, the high 24 are the epoch.
    pub const EPOCH_SHIFT: u32 = 40;
    /// The largest epoch; a sender id that has been used this many times is not reused.
    pub const MAX_EPOCH: u32 = (1 << 24) - 1;

    pub fn new(key: &SessionKey, sender: u16) -> Self {
        Self { aes: cipher(key), sender, counter: 0 }
    }

    /// A sealer that continues a sender's counter exactly: its first frame is numbered `last_counter + 1`. For tests
    /// and tools (a forger, a model of a peer); the state machines give a returning player's id a new epoch with
    /// [`Sealer::with_epoch`] instead.
    pub fn resume(key: &SessionKey, sender: u16, last_counter: u64) -> Self {
        Self { aes: cipher(key), sender, counter: last_counter }
    }

    /// A sealer for a sender id that may have been used before under this key (a player coming back). Its counter
    /// starts at `epoch << 40`, above every counter an earlier epoch can reach, so no nonce repeats and the receiver's
    /// replay window sees the new frames as newer. `None` when `epoch` is over [`Sealer::MAX_EPOCH`].
    pub fn with_epoch(key: &SessionKey, sender: u16, epoch: u32) -> Option<Self> {
        (epoch <= Self::MAX_EPOCH).then(|| Self {
            aes: cipher(key),
            sender,
            counter: u64::from(epoch) << Self::EPOCH_SHIFT,
        })
    }

    pub fn sender(&self) -> u16 {
        self.sender
    }

    pub fn last_counter(&self) -> u64 {
        self.counter
    }

    /// Seals `plaintext` as the next frame of this sender, or says why it cannot. Nothing leaves the sealer on an
    /// error, so a refused message costs no nonce that was ever visible on the wire.
    pub fn try_seal(&mut self, kind: u8, plaintext: &[u8]) -> Result<Frame, SealError> {
        if plaintext.len() > Self::MAX_PAYLOAD {
            return Err(SealError::TooLong(plaintext.len()));
        }
        self.counter = self.counter.checked_add(1).ok_or(SealError::CounterExhausted)?;
        let header = FrameHeader::new(FrameHeader::CURRENT_VERSION, kind, self.sender, self.counter);
        let nonce = nonce(self.sender, self.counter);
        let aad = header.to_bytes();
        let body =
            self.aes.encrypt((&nonce).into(), Payload { msg: plaintext, aad: &aad }).map_err(|_| SealError::Cipher)?;
        Ok(Frame::new(header, body))
    }

    /// As [`Sealer::try_seal`], for tests and tools. Panics where `try_seal` refuses: with a payload over
    /// [`Sealer::MAX_PAYLOAD`], or after 2^64 - 1 frames. The session state machines use `try_seal`.
    pub fn seal(&mut self, kind: u8, plaintext: &[u8]) -> Frame {
        match self.try_seal(kind, plaintext) {
            Ok(frame) => frame,
            Err(e) => panic!("cannot seal a frame: {e}"),
        }
    }
}

impl fmt::Debug for Sealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealer").field("sender", &self.sender).field("counter", &self.counter).finish()
    }
}

/// Opens incoming frames from any sender and refuses replays. A frame is authenticated before its counter is
/// recorded, and before its sender gets a window at all, so a forged frame can neither poison a window nor make the
/// opener grow: it holds one 16-byte window per sender that ever sent an authentic frame.
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
        // a sender seen for the first time has an empty window: look, do not insert, until the tag has verified
        let fresh = self
            .windows
            .get(&h.sender)
            .map_or_else(|| ReplayWindow::new().is_fresh(h.counter), |w| w.is_fresh(h.counter));
        if !fresh {
            return Err(OpenError::Replay);
        }
        let nonce = nonce(h.sender, h.counter);
        let aad = h.to_bytes();
        let plaintext = self
            .aes
            .decrypt((&nonce).into(), Payload { msg: &frame.body, aad: &aad })
            .map_err(|_| OpenError::Tamper)?;
        self.windows.entry(h.sender).or_default().accept(h.counter)?;
        Ok(plaintext)
    }
}

impl fmt::Debug for Opener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opener").field("windows", &self.windows).finish()
    }
}

fn cipher(key: &SessionKey) -> Aes256Gcm {
    // cannot fail: the key is exactly the 32 bytes AES-256 takes
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
