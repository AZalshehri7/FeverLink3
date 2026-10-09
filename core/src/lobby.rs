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
/// u8   version (1)
/// u32  created, unix seconds
/// 16   secret
/// u8   endpoint count (0..8)
/// per endpoint: u8 family (4 | 6), address bytes, u16 port
/// 4    check = first 4 bytes of SHA-256 of everything before
/// ```
///
/// A code with one IPv4 endpoint is 33 bytes, 53 characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinCode {
    pub version: u8,
    /// Unix seconds, 32 bits as on the wire.
    pub created: u32,
    pub endpoints: Vec<SocketAddr>,
    pub secret: [u8; SessionKey::SECRET_SIZE],
}

/// Why a join code could not be created. Decoding never errors: it yields `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinCodeError {
    TooManyEndpoints(usize),
}

impl fmt::Display for JoinCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyEndpoints(n) => write!(f, "{n} endpoints; at most {} allowed", JoinCode::MAX_ENDPOINTS),
        }
    }
}

impl std::error::Error for JoinCodeError {}

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

impl JoinCode {
    pub const CURRENT_VERSION: u8 = 1;
    pub const MAX_ENDPOINTS: usize = 8;
    /// Longest text [`JoinCode::try_decode`] looks at. The longest valid code (eight IPv6 endpoints) is 286
    /// characters; dashes and spaces for readability add a quarter. The text comes from a paste box, so it is
    /// bounded before anything is allocated for it.
    pub const MAX_TEXT_LENGTH: usize = 1024;
    const CHECK_SIZE: usize = 4;
    const MIN_BYTES: usize = 1 + 4 + SessionKey::SECRET_SIZE + 1 + Self::CHECK_SIZE;

    /// `created` in unix seconds; `None` is now.
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
        Ok(Self { version: Self::CURRENT_VERSION, created, endpoints: endpoints.to_vec(), secret })
    }

    pub fn encode(&self) -> String {
        // the fields are public, so a hand-built value can hold more than a code can carry: keep the first ones
        // rather than write a code that nobody can read back
        let endpoints = self.endpoints.get(..Self::MAX_ENDPOINTS).unwrap_or(&self.endpoints);
        let mut body = Vec::with_capacity(Self::MIN_BYTES + endpoints.len() * 19);
        body.push(self.version);
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
        let r = &bytes[..body_len];
        let mut i = 0;
        let version = r[i];
        i += 1;
        if version != Self::CURRENT_VERSION {
            return None;
        }
        let created = u32::from_be_bytes(r[i..i + 4].try_into().ok()?);
        i += 4;
        let secret: [u8; SessionKey::SECRET_SIZE] = r[i..i + SessionKey::SECRET_SIZE].try_into().ok()?;
        i += SessionKey::SECRET_SIZE;
        let count = usize::from(r[i]);
        i += 1;
        if count > Self::MAX_ENDPOINTS {
            return None;
        }
        let mut endpoints = Vec::with_capacity(count);
        for _ in 0..count {
            let family = *r.get(i)?;
            i += 1;
            let addr_len = match family {
                4 => 4,
                6 => 16,
                _ => return None,
            };
            if i + addr_len + 2 > r.len() {
                return None;
            }
            let ip = match family {
                4 => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(&r[i..i + 4]).ok()?)),
                _ => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&r[i..i + 16]).ok()?)),
            };
            i += addr_len;
            let port = u16::from_be_bytes([r[i], r[i + 1]]);
            i += 2;
            endpoints.push(SocketAddr::new(ip, port));
        }
        if i != r.len() {
            return None;
        }
        Some(Self { version, created, endpoints, secret })
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
