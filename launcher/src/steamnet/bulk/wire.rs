//! The datagram wire format of the bulk transfer: what one Steam unreliable message holds. See `docs/STEAM_TRANSPORT.md`,
//! "Bulk datagrams".
//!
//! Every datagram starts with [`MAGIC`] and a kind byte and ends with a CRC-32 of everything before it, so a damaged or
//! foreign message is dropped instead of believed. All numbers are little-endian. A session frame starts with its
//! 4-byte big-endian length (always `00` for a frame the protocol allows), and the speed test's text messages start
//! with `tput`, so [`MAGIC`] tells a datagram from both.
//!
//! | kind | from | fields (after magic, kind) |
//! |---|---|---|
//! | 1 offer | sender | xfer u32, ts u32, ctx u32, total u64, payload u16, sha256 \[32\] |
//! | 2 data | sender | xfer u32, seq u32, ts u32, payload |
//! | 3 ack | receiver | xfer u32, cum u32, ts_echo u32, hold_us u32, n u8, n x (start u32, len u32) |
//! | 4 done | receiver | xfer u32, ok u8 |
//! | 5 abort | both | xfer u32, reason u8 |
//! | 6 fin | sender | xfer u32 |

/// First byte of every bulk datagram.
pub const MAGIC: u8 = 0xFB;
/// The size of a full data datagram, bytes. Steam splits a message above its packet size (about 1200 bytes of payload)
/// into fragments and loses all of it when one fragment is lost; 1100 stays below, with room for Steam's own header.
pub const DATAGRAM_SIZE: usize = 1100;
/// Magic, kind, xfer, seq, ts and the CRC.
pub const DATA_OVERHEAD: usize = 2 + 4 + 4 + 4 + 4;
/// Payload bytes of a full data datagram.
pub const PAYLOAD: usize = DATAGRAM_SIZE - DATA_OVERHEAD;
/// Smallest payload per datagram an offer may ask for.
pub const MIN_PAYLOAD: usize = 64;
/// Most selective-ack ranges in one ack.
pub const MAX_RANGES: usize = 64;
/// Largest datagram of any kind this module makes or accepts.
pub const MAX_DATAGRAM: usize = DATAGRAM_SIZE;

const K_OFFER: u8 = 1;
const K_DATA: u8 = 2;
const K_ACK: u8 = 3;
const K_DONE: u8 = 4;
const K_ABORT: u8 = 5;
const K_FIN: u8 = 6;

/// Why a transfer was abandoned (`abort` reason byte).
pub mod reason {
    pub const REFUSED: u8 = 1;
    pub const BUSY: u8 = 2;
    pub const TOO_BIG: u8 = 3;
    pub const CANCELLED: u8 = 4;
    pub const TIMEOUT: u8 = 5;
    pub const INTERNAL: u8 = 6;

    pub fn text(code: u8) -> &'static str {
        match code {
            REFUSED => "refused by the receiver",
            BUSY => "the receiver is busy",
            TOO_BIG => "too big for the receiver",
            CANCELLED => "cancelled",
            TIMEOUT => "timed out",
            _ => "unknown reason",
        }
    }
}

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// CRC-32 (IEEE, the one of zip and PNG).
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for b in bytes {
        c = CRC_TABLE[((c ^ u32::from(*b)) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

/// The first datagram: what is coming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub xfer: u32,
    /// Sender clock, microseconds, echoed by the first ack (the first round-trip sample).
    pub ts: u32,
    /// Caller's context (the resync round id; 0 for the speed test).
    pub ctx: u32,
    pub total: u64,
    /// Payload bytes of every data datagram but the last.
    pub payload: u16,
    pub sha256: [u8; 32],
}

impl Offer {
    /// Data datagrams the transfer takes; `None` for an empty or absurd offer.
    pub fn packets(&self) -> Option<u32> {
        if self.total == 0 || self.payload == 0 {
            return None;
        }
        u32::try_from(self.total.div_ceil(u64::from(self.payload))).ok()
    }
}

/// A selective ack: everything below `cum` arrived, and so did the listed ranges above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ack {
    pub xfer: u32,
    pub cum: u32,
    /// The sender clock of the newest datagram that arrived, as it was sent.
    pub ts_echo: u32,
    /// How long the receiver held that datagram before sending this ack, microseconds.
    pub hold_us: u32,
    /// (first seq, count), ascending, separated by gaps, all above `cum`.
    pub ranges: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet<'a> {
    Offer(Offer),
    Data { xfer: u32, seq: u32, ts: u32, payload: &'a [u8] },
    Ack(Ack),
    Done { xfer: u32, ok: bool },
    Abort { xfer: u32, reason: u8 },
    Fin { xfer: u32 },
}

impl Packet<'_> {
    pub fn xfer(&self) -> u32 {
        match self {
            Packet::Offer(o) => o.xfer,
            Packet::Ack(a) => a.xfer,
            Packet::Data { xfer, .. }
            | Packet::Done { xfer, .. }
            | Packet::Abort { xfer, .. }
            | Packet::Fin { xfer } => *xfer,
        }
    }
}

fn seal(mut body: Vec<u8>) -> Vec<u8> {
    let crc = crc32(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    body
}

fn head(kind: u8, xfer: u32, extra: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(6 + extra + 4);
    b.push(MAGIC);
    b.push(kind);
    b.extend_from_slice(&xfer.to_le_bytes());
    b
}

pub fn encode_offer(o: &Offer) -> Vec<u8> {
    let mut b = head(K_OFFER, o.xfer, 50);
    b.extend_from_slice(&o.ts.to_le_bytes());
    b.extend_from_slice(&o.ctx.to_le_bytes());
    b.extend_from_slice(&o.total.to_le_bytes());
    b.extend_from_slice(&o.payload.to_le_bytes());
    b.extend_from_slice(&o.sha256);
    seal(b)
}

pub fn encode_data(xfer: u32, seq: u32, ts: u32, payload: &[u8]) -> Vec<u8> {
    let mut b = head(K_DATA, xfer, 8 + payload.len());
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&ts.to_le_bytes());
    b.extend_from_slice(payload);
    seal(b)
}

pub fn encode_ack(a: &Ack) -> Vec<u8> {
    let n = a.ranges.len().min(MAX_RANGES);
    let mut b = head(K_ACK, a.xfer, 13 + n * 8);
    b.extend_from_slice(&a.cum.to_le_bytes());
    b.extend_from_slice(&a.ts_echo.to_le_bytes());
    b.extend_from_slice(&a.hold_us.to_le_bytes());
    b.push(n as u8);
    for (start, len) in a.ranges.iter().take(n) {
        b.extend_from_slice(&start.to_le_bytes());
        b.extend_from_slice(&len.to_le_bytes());
    }
    seal(b)
}

pub fn encode_done(xfer: u32, ok: bool) -> Vec<u8> {
    let mut b = head(K_DONE, xfer, 1);
    b.push(u8::from(ok));
    seal(b)
}

pub fn encode_abort(xfer: u32, reason: u8) -> Vec<u8> {
    let mut b = head(K_ABORT, xfer, 1);
    b.push(reason);
    seal(b)
}

pub fn encode_fin(xfer: u32) -> Vec<u8> {
    seal(head(K_FIN, xfer, 0))
}

/// Whether a received Steam message is a bulk datagram (by its first byte).
pub fn is_bulk(data: &[u8]) -> bool {
    data.first() == Some(&MAGIC)
}

/// The kind and transfer id of a datagram without checking it: for routing only.
pub fn peek(data: &[u8]) -> Option<(u8, u32)> {
    if data.len() < 6 || data[0] != MAGIC {
        return None;
    }
    Some((data[1], u32::from_le_bytes(data[2..6].try_into().ok()?)))
}

/// Whether a datagram is an offer (by its kind byte).
pub fn peek_is_offer(data: &[u8]) -> bool {
    peek(data).is_some_and(|(kind, _)| kind == K_OFFER)
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Reads a datagram. `None` for anything that is not a well-formed bulk datagram: wrong magic or kind, bad CRC, a
/// length that does not fit its kind, ranges that are empty, out of order or not above `cum`.
pub fn parse(data: &[u8]) -> Option<Packet<'_>> {
    if data.len() < 10 || data.len() > MAX_DATAGRAM || data[0] != MAGIC {
        return None;
    }
    let (body, crc) = data.split_at(data.len() - 4);
    if crc32(body) != u32::from_le_bytes(crc.try_into().ok()?) {
        return None;
    }
    let kind = body[1];
    let xfer = u32_at(body, 2)?;
    let rest = &body[6..];
    match kind {
        K_OFFER => {
            if rest.len() != 4 + 4 + 8 + 2 + 32 {
                return None;
            }
            Some(Packet::Offer(Offer {
                xfer,
                ts: u32_at(rest, 0)?,
                ctx: u32_at(rest, 4)?,
                total: u64::from_le_bytes(rest[8..16].try_into().ok()?),
                payload: u16::from_le_bytes(rest[16..18].try_into().ok()?),
                sha256: rest[18..50].try_into().ok()?,
            }))
        }
        K_DATA => {
            if rest.len() < 8 {
                return None;
            }
            Some(Packet::Data { xfer, seq: u32_at(rest, 0)?, ts: u32_at(rest, 4)?, payload: &rest[8..] })
        }
        K_ACK => {
            if rest.len() < 13 {
                return None;
            }
            let cum = u32_at(rest, 0)?;
            let n = usize::from(rest[12]);
            if n > MAX_RANGES || rest.len() != 13 + n * 8 {
                return None;
            }
            let mut ranges = Vec::with_capacity(n);
            let mut floor = u64::from(cum) + 1; // the packet `cum` itself is missing, so ranges start above it
            for i in 0..n {
                let start = u32_at(rest, 13 + i * 8)?;
                let len = u32_at(rest, 17 + i * 8)?;
                let end = u64::from(start) + u64::from(len);
                if len == 0 || u64::from(start) < floor || end > u64::from(u32::MAX) {
                    return None;
                }
                // a gap of at least one packet separates ranges
                floor = end + 1;
                ranges.push((start, len));
            }
            Some(Packet::Ack(Ack { xfer, cum, ts_echo: u32_at(rest, 4)?, hold_us: u32_at(rest, 8)?, ranges }))
        }
        K_DONE => (rest.len() == 1 && rest[0] <= 1).then(|| Packet::Done { xfer, ok: rest[0] == 1 }),
        K_ABORT => (rest.len() == 1).then(|| Packet::Abort { xfer, reason: rest[0] }),
        K_FIN => rest.is_empty().then_some(Packet::Fin { xfer }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_known_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn a_full_data_datagram_is_1100_bytes() {
        let payload = vec![7u8; PAYLOAD];
        let d = encode_data(1, 2, 3, &payload);
        assert_eq!(d.len(), DATAGRAM_SIZE);
        assert!(is_bulk(&d));
        assert_eq!(parse(&d), Some(Packet::Data { xfer: 1, seq: 2, ts: 3, payload: &payload }));
    }

    #[test]
    fn every_kind_round_trips() {
        let offer = Offer { xfer: 9, ts: 5, ctx: 77, total: 75_000_000, payload: PAYLOAD as u16, sha256: [3; 32] };
        assert_eq!(parse(&encode_offer(&offer)), Some(Packet::Offer(offer.clone())));
        assert_eq!(offer.packets(), Some(75_000_000u64.div_ceil(PAYLOAD as u64) as u32));
        let ack = Ack { xfer: 9, cum: 10, ts_echo: 1, hold_us: 2, ranges: vec![(12, 3), (20, 1), (22, 100)] };
        assert_eq!(parse(&encode_ack(&ack)), Some(Packet::Ack(ack)));
        assert_eq!(parse(&encode_done(4, true)), Some(Packet::Done { xfer: 4, ok: true }));
        assert_eq!(parse(&encode_done(4, false)), Some(Packet::Done { xfer: 4, ok: false }));
        assert_eq!(parse(&encode_abort(4, reason::BUSY)), Some(Packet::Abort { xfer: 4, reason: 2 }));
        assert_eq!(parse(&encode_fin(4)), Some(Packet::Fin { xfer: 4 }));
        assert_eq!(peek(&encode_fin(4)), Some((K_FIN, 4)));
        assert!(!peek_is_offer(&encode_fin(4)));
        assert!(peek_is_offer(&encode_offer(&Offer {
            xfer: 1,
            ts: 0,
            ctx: 0,
            total: 1,
            payload: 64,
            sha256: [0; 32]
        })));
    }

    #[test]
    fn damaged_or_foreign_datagrams_are_refused() {
        let d = encode_data(1, 2, 3, &[1, 2, 3, 4]);
        for i in 0..d.len() {
            let mut bad = d.clone();
            bad[i] ^= 0x40;
            assert_eq!(parse(&bad), None, "flip at {i}");
        }
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[MAGIC]), None);
        assert_eq!(parse(b"tput up 1 2 3\n"), None);
        assert_eq!(parse(&d[..d.len() - 1]), None);
        // a frame of the session starts with 0
        assert!(!is_bulk(&[0, 0, 1, 0]));
        assert_eq!(parse(&vec![MAGIC; 2000]), None);
    }

    #[test]
    fn malformed_acks_are_refused() {
        let make = |cum: u32, ranges: Vec<(u32, u32)>| {
            let mut b = head(K_ACK, 1, 0);
            b.extend_from_slice(&cum.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            b.push(ranges.len() as u8);
            for (s, l) in ranges {
                b.extend_from_slice(&s.to_le_bytes());
                b.extend_from_slice(&l.to_le_bytes());
            }
            seal(b)
        };
        assert!(parse(&make(10, vec![(12, 2)])).is_some());
        assert!(parse(&make(10, vec![(10, 2)])).is_none(), "starts at the missing packet");
        assert!(parse(&make(10, vec![(5, 2)])).is_none(), "below cum");
        assert!(parse(&make(10, vec![(12, 0)])).is_none(), "empty");
        assert!(parse(&make(10, vec![(12, 2), (14, 1)])).is_none(), "no gap between ranges");
        assert!(parse(&make(10, vec![(20, 2), (12, 2)])).is_none(), "out of order");
        assert!(parse(&make(10, vec![(u32::MAX - 1, 5)])).is_none(), "overflow");
        let many: Vec<(u32, u32)> = (0..65).map(|i| (12 + i * 3, 1)).collect();
        assert!(parse(&make(10, many)).is_none(), "more than the limit");
    }
}
