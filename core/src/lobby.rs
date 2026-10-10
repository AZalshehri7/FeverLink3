//! Join codes. See `docs/PROTOCOL.md`, "Join code".

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::crypto::SessionKey;

/// What a host gives their friends. It carries where to connect and the session secret, so the code is the
/// credential: anyone who has it can join (unless the host also set a password).
///
/// Binary layout, then base32 (RFC 4648 alphabet, no padding, case-insensitive, dashes and spaces ignored):
///
/// ```text
/// u8   version (1 | 2)
/// u32  created, unix seconds
/// 16   secret
/// u8   endpoint count (0..8)
/// per endpoint: u8 family (4 | 6), address bytes, u16 port
/// -- version 2 only; a code with nothing to put here is written as version 1 --
/// u8   extension count (1..8)
/// per extension: u8 tag, u16 length, `length` value bytes
///   tag 1, relays: u8 count (1..4), per relay: u8 family (4 | 6 | 1), address bytes, u16 port
///                  (family 4: 4 bytes, 6: 16 bytes, 1: u8 length and that many bytes of DNS name)
///   tag 2, Steam ID: u64 big endian, not 0
///   any other tag: skipped by the decoder, so a field added later does not break the codes of this build
/// --
/// 4    check = first 4 bytes of SHA-256 of everything before
/// ```
///
/// A code with one IPv4 endpoint is 33 bytes, 53 characters. A relay (`docs/RELAY.md`) is where a host that cannot
/// be reached directly waits for its players; the room they ask for there is not in the code, both sides derive it
/// from the secret ([`SessionKey::relay_room_id`]). Codes of version 1 keep decoding, and a code without
/// extensions is still written as version 1, so builds that predate relays can read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinCode {
    /// The version found on the wire (1 or 2); [`JoinCode::encode`] writes 2 only when there is an extension.
    pub version: u8,
    /// Unix seconds, 32 bits as on the wire.
    pub created: u32,
    pub endpoints: Vec<SocketAddr>,
    /// Relay servers, in the order a joiner should try them after the direct endpoints.
    pub relays: Vec<RelayAddr>,
    /// The host's Steam ID (64 bits): a joiner connects to it through Steam's peer-to-peer networking, which needs
    /// no open port on either side. `None` for a code that only has direct endpoints.
    pub steam_id: Option<u64>,
    pub secret: [u8; SessionKey::SECRET_SIZE],
}

/// Where a relay server listens: an IP address or a DNS name, and a port. A name keeps a code valid when the
/// operator moves the relay to another address; resolving it is the caller's business (this crate does no I/O).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelayAddr {
    host: RelayHost,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RelayHost {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    /// Lower case, 1 to [`RelayAddr::MAX_NAME_LEN`] bytes of letters, digits, `-` and `.`.
    Name(String),
}

impl RelayAddr {
    pub const MAX_NAME_LEN: usize = 100;
    const FAMILY_NAME: u8 = 1;

    /// `host` is an IPv4 or IPv6 literal (brackets optional) or a DNS name; `None` for anything else or port 0.
    pub fn new(host: &str, port: u16) -> Option<Self> {
        if port == 0 {
            return None;
        }
        let host = host.trim();
        let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
        if let Ok(ip) = bare.parse::<Ipv6Addr>() {
            return Some(Self { host: RelayHost::V6(ip), port });
        }
        if let Ok(ip) = host.parse::<Ipv4Addr>() {
            return Some(Self { host: RelayHost::V4(ip), port });
        }
        Self::valid_name(host).map(|name| Self { host: RelayHost::Name(name), port })
    }

    /// `host:port`, `[v6]:port` or, with a `default_port`, just a host. `None` when it does not fit those shapes.
    pub fn parse(text: &str, default_port: Option<u16>) -> Option<Self> {
        let text = text.trim();
        if let Some(rest) = text.strip_prefix('[') {
            let (host, tail) = rest.split_once(']')?;
            return match tail.strip_prefix(':') {
                Some(port) => Self::new(host, port.parse().ok()?),
                None if tail.is_empty() => Self::new(host, default_port?),
                None => None,
            };
        }
        match text.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => Self::new(host, port.parse().ok()?),
            Some(_) => None,
            None => Self::new(text, default_port?),
        }
    }

    fn valid_name(name: &str) -> Option<String> {
        let name = name.strip_suffix('.').unwrap_or(name);
        if name.is_empty() || name.len() > Self::MAX_NAME_LEN {
            return None;
        }
        let name = name.to_ascii_lowercase();
        let label_ok = |l: &str| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        };
        if !name.split('.').all(label_ok) {
            return None;
        }
        // an all-digit last label is a malformed IPv4 address, never a name
        let last = name.rsplit('.').next().unwrap_or("");
        if last.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(name)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The host part without brackets, as `ToSocketAddrs` takes it together with [`RelayAddr::port`].
    pub fn host(&self) -> String {
        match &self.host {
            RelayHost::V4(a) => a.to_string(),
            RelayHost::V6(a) => a.to_string(),
            RelayHost::Name(n) => n.clone(),
        }
    }

    /// The address itself when the host is an IP literal, `None` for a name.
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        match &self.host {
            RelayHost::V4(a) => Some(SocketAddr::new(IpAddr::V4(*a), self.port)),
            RelayHost::V6(a) => Some(SocketAddr::new(IpAddr::V6(*a), self.port)),
            RelayHost::Name(_) => None,
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        match &self.host {
            RelayHost::V4(a) => {
                out.push(4);
                out.extend_from_slice(&a.octets());
            }
            RelayHost::V6(a) => {
                out.push(6);
                out.extend_from_slice(&a.octets());
            }
            RelayHost::Name(n) => {
                out.push(Self::FAMILY_NAME);
                out.push(n.len() as u8); // at most MAX_NAME_LEN, checked on construction
                out.extend_from_slice(n.as_bytes());
            }
        }
        out.extend_from_slice(&self.port.to_be_bytes());
    }

    fn read(cursor: &mut Cursor<'_>) -> Option<Self> {
        let host = match cursor.u8()? {
            4 => RelayHost::V4(Ipv4Addr::from(<[u8; 4]>::try_from(cursor.take(4)?).ok()?)),
            6 => RelayHost::V6(Ipv6Addr::from(<[u8; 16]>::try_from(cursor.take(16)?).ok()?)),
            Self::FAMILY_NAME => {
                let len = usize::from(cursor.u8()?);
                RelayHost::Name(Self::valid_name(std::str::from_utf8(cursor.take(len)?).ok()?)?)
            }
            _ => return None,
        };
        let port = cursor.u16()?;
        if port == 0 {
            return None;
        }
        Some(Self { host, port })
    }
}

impl From<SocketAddr> for RelayAddr {
    fn from(addr: SocketAddr) -> Self {
        let host = match addr.ip() {
            IpAddr::V4(a) => RelayHost::V4(a),
            IpAddr::V6(a) => RelayHost::V6(a),
        };
        Self { host, port: addr.port() }
    }
}

impl fmt::Display for RelayAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            RelayHost::V6(a) => write!(f, "[{a}]:{}", self.port),
            _ => write!(f, "{}:{}", self.host(), self.port),
        }
    }
}

/// Reads the bytes of a decoded code front to back; every read says `None` instead of running past the end.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// Why a join code could not be created. Decoding never errors: it yields `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinCodeError {
    TooManyEndpoints(usize),
    TooManyRelays(usize),
}

impl fmt::Display for JoinCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyEndpoints(n) => write!(f, "{n} endpoints; at most {} allowed", JoinCode::MAX_ENDPOINTS),
            Self::TooManyRelays(n) => write!(f, "{n} relays; at most {} allowed", JoinCode::MAX_RELAYS),
        }
    }
}

impl std::error::Error for JoinCodeError {}

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

impl JoinCode {
    /// Layout without extensions; what [`JoinCode::create`] makes.
    pub const VERSION_BASIC: u8 = 1;
    /// Layout with an extension list (today: the relays).
    pub const VERSION_EXTENDED: u8 = 2;
    /// The newest layout this build understands.
    pub const CURRENT_VERSION: u8 = Self::VERSION_EXTENDED;
    pub const MAX_ENDPOINTS: usize = 8;
    pub const MAX_RELAYS: usize = 4;
    pub const MAX_EXTENSIONS: usize = 8;
    /// Longest text [`JoinCode::try_decode`] looks at. The longest valid code (eight IPv6 endpoints, four relays
    /// with names of 100 characters, a Steam ID) is 976 characters; dashes and spaces for readability add a
    /// quarter. The text comes from a paste box, so it is bounded before anything is allocated for it.
    pub const MAX_TEXT_LENGTH: usize = 2048;
    /// Extension tag of the relay list.
    const EXT_RELAYS: u8 = 1;
    /// Extension tag of the host's Steam ID.
    const EXT_STEAM: u8 = 2;
    const CHECK_SIZE: usize = 4;
    const MIN_BYTES: usize = 1 + 4 + SessionKey::SECRET_SIZE + 1 + Self::CHECK_SIZE;

    /// `created` in unix seconds; `None` is now. The code has no relays; see [`JoinCode::with_relays`].
    pub fn create(
        endpoints: &[SocketAddr],
        secret: [u8; SessionKey::SECRET_SIZE],
        created: Option<u32>,
    ) -> Result<Self, JoinCodeError> {
        if endpoints.len() > Self::MAX_ENDPOINTS {
            return Err(JoinCodeError::TooManyEndpoints(endpoints.len()));
        }
        let created = created.unwrap_or_else(|| {
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0).min(u64::from(u32::MAX))
                as u32
        });
        Ok(Self {
            version: Self::VERSION_BASIC,
            created,
            endpoints: endpoints.to_vec(),
            relays: Vec::new(),
            steam_id: None,
            secret,
        })
    }

    /// The same code with this relay list (replacing any). An empty list gives a version 1 code again.
    pub fn with_relays(mut self, relays: &[RelayAddr]) -> Result<Self, JoinCodeError> {
        if relays.len() > Self::MAX_RELAYS {
            return Err(JoinCodeError::TooManyRelays(relays.len()));
        }
        self.relays = relays.to_vec();
        self.version = self.wire_version();
        Ok(self)
    }

    /// The same code with the host's Steam ID, so a joiner can connect through Steam. 0 is not a Steam ID and gives
    /// no field.
    pub fn with_steam_id(mut self, steam_id: u64) -> Self {
        self.steam_id = (steam_id != 0).then_some(steam_id);
        self.version = self.wire_version();
        self
    }

    /// The optional fields as `(tag, value)`, in tag order. A field added later is one more entry here and one
    /// more arm in [`JoinCode::try_decode`].
    fn extensions(&self) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        // as with the endpoints, a hand-built value with too many relays keeps the first ones
        let relays = self.relays.get(..Self::MAX_RELAYS).unwrap_or(&self.relays);
        if !relays.is_empty() {
            let mut value = vec![relays.len() as u8];
            for relay in relays {
                relay.write(&mut value);
            }
            out.push((Self::EXT_RELAYS, value));
        }
        if let Some(id) = self.steam_id {
            out.push((Self::EXT_STEAM, id.to_be_bytes().to_vec()));
        }
        out
    }

    /// The version [`JoinCode::encode`] writes: 1 unless there is something only version 2 can carry.
    fn wire_version(&self) -> u8 {
        if self.extensions().is_empty() {
            Self::VERSION_BASIC
        } else {
            Self::VERSION_EXTENDED
        }
    }

    /// The room a joiner asks the relays for, derived from the secret ([`SessionKey::relay_room_id`]).
    pub fn relay_room(&self) -> [u8; SessionKey::ROOM_ID_SIZE] {
        SessionKey::relay_room_id(&self.secret)
    }

    pub fn encode(&self) -> String {
        // the fields are public, so a hand-built value can hold more than a code can carry: keep the first ones
        // rather than write a code that nobody can read back
        let endpoints = self.endpoints.get(..Self::MAX_ENDPOINTS).unwrap_or(&self.endpoints);
        let extensions = self.extensions();
        let mut body = Vec::with_capacity(Self::MIN_BYTES + endpoints.len() * 19 + self.relays.len() * 24);
        body.push(if extensions.is_empty() { Self::VERSION_BASIC } else { Self::VERSION_EXTENDED });
        body.extend_from_slice(&self.created.to_be_bytes());
        body.extend_from_slice(&self.secret);
        body.push(endpoints.len() as u8);
        for ep in endpoints {
            match ep.ip() {
                IpAddr::V4(a) => {
                    body.push(4);
                    body.extend_from_slice(&a.octets());
                }
                IpAddr::V6(a) => {
                    body.push(6);
                    body.extend_from_slice(&a.octets());
                }
            }
            body.extend_from_slice(&ep.port().to_be_bytes());
        }
        if !extensions.is_empty() {
            body.push(extensions.len() as u8);
            for (tag, value) in &extensions {
                body.push(*tag);
                body.extend_from_slice(&(value.len() as u16).to_be_bytes()); // at most a few hundred bytes
                body.extend_from_slice(value);
            }
        }
        let check = Sha256::digest(&body);
        body.extend_from_slice(&check[..Self::CHECK_SIZE]);
        base32_encode(&body)
    }

    /// Parses a code typed or pasted by a player. `None` for anything that is not a valid code of a version this
    /// build understands; never panics on bad input.
    pub fn try_decode(text: &str) -> Option<Self> {
        if text.trim().is_empty() || text.len() > Self::MAX_TEXT_LENGTH {
            return None;
        }
        let bytes = base32_decode(text)?;
        if bytes.len() < Self::MIN_BYTES {
            return None;
        }
        let body_len = bytes.len() - Self::CHECK_SIZE;
        let expect = Sha256::digest(&bytes[..body_len]);
        if expect[..Self::CHECK_SIZE] != bytes[body_len..] {
            return None;
        }
        let mut r = Cursor { bytes: &bytes[..body_len], at: 0 };
        let version = r.u8()?;
        if version != Self::VERSION_BASIC && version != Self::VERSION_EXTENDED {
            return None;
        }
        let created = u32::from_be_bytes(r.take(4)?.try_into().ok()?);
        let secret: [u8; SessionKey::SECRET_SIZE] = r.take(SessionKey::SECRET_SIZE)?.try_into().ok()?;
        let count = usize::from(r.u8()?);
        if count > Self::MAX_ENDPOINTS {
            return None;
        }
        let mut endpoints = Vec::with_capacity(count);
        for _ in 0..count {
            let ip = match r.u8()? {
                4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(r.take(4)?).ok()?)),
                6 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(r.take(16)?).ok()?)),
                _ => return None,
            };
            endpoints.push(SocketAddr::new(ip, r.u16()?));
        }
        let mut relays = Vec::new();
        let mut steam_id = None;
        if version == Self::VERSION_EXTENDED {
            let count = usize::from(r.u8()?);
            if count == 0 || count > Self::MAX_EXTENSIONS {
                return None; // the encoder writes version 2 only when it has an extension
            }
            let mut have_relays = false;
            for _ in 0..count {
                let tag = r.u8()?;
                let len = usize::from(r.u16()?);
                let mut value = Cursor { bytes: r.take(len)?, at: 0 };
                if tag == Self::EXT_RELAYS {
                    let n = usize::from(value.u8()?);
                    if have_relays || n == 0 || n > Self::MAX_RELAYS {
                        return None;
                    }
                    have_relays = true;
                    for _ in 0..n {
                        relays.push(RelayAddr::read(&mut value)?);
                    }
                    if !value.done() {
                        return None;
                    }
                }
                if tag == Self::EXT_STEAM {
                    let id = u64::from_be_bytes(value.take(8)?.try_into().ok()?);
                    if steam_id.is_some() || id == 0 || !value.done() {
                        return None;
                    }
                    steam_id = Some(id);
                }
                // any other tag belongs to a later build: its bytes are skipped
            }
        }
        if !r.done() {
            return None;
        }
        Some(Self { version, created, endpoints, relays, steam_id, secret })
    }
}

/// RFC 4648 base32, upper case, no padding.
pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &b in data {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Lenient RFC 4648 base32 decoding: case, dashes, spaces and `=` padding are ignored. `None` on any other
/// character or when the trailing bits are not zero padding.
pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    // the capacity is a guess from untrusted text: never let it be more than a join code needs
    let mut out = Vec::with_capacity((text.len().saturating_mul(5) / 8).min(JoinCode::MAX_TEXT_LENGTH));
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for ch in text.chars() {
        if ch == '-' || ch == ' ' || ch == '=' {
            continue;
        }
        let upper = ch.to_ascii_uppercase();
        let v = ALPHABET.iter().position(|&a| char::from(a) == upper)? as u32;
        buffer = (buffer << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xFF) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    // the leftover bits of the last character must be padding zeros
    if bits >= 5 || (buffer & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}
