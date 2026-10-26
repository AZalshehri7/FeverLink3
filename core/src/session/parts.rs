//! Messages longer than one frame: split into parts before they are sealed, put back together after they are opened, before the
//! session logic sees them (`docs/PROTOCOL.md`, "Messages in parts"). One mechanism for every message kind and both directions.
//!
//! Why here: the sealed channel is the lowest layer that sees whole messages (a kind and its payload), and every transport carries
//! its frames (TCP, the relay, Steam). Steam carries at most 512 KiB in one message (launcher `steamnet::MAX_MESSAGE`), and a
//! station's build is more (real session 2026-10-26: a 3-track station's action was 618 781 bytes and refused). A part is an
//! ordinary sealed frame, so every part is authenticated before a byte of it is kept: only a holder of the session key can make a
//! game hold a message in parts, and then at most [`MAX_MESSAGE`] bytes per connection, as many as really arrived.
//!
//! **Order.** A sender seals the parts of one message one after the other and sends them in a row on the connection, which is one
//! ordered stream; nothing else of that sender comes between them. The receiver hands the message on when its last part is in,
//! and everything the sender sent after it arrives after that part, so no later message overtakes it: the lockstep's input
//! barrier (a game that has read a peer's clock has every item that peer shipped before it) holds for a message in parts as for
//! any other. A whole frame that arrives while parts are open breaks that rule: the open message is dropped and logged.
//!
//! **Part payload** (binary, the one kind whose payload is not JSON): `u8 kind` of the whole message, `u32 total` bytes of the whole
//! payload, `u32 offset` of this part's bytes in it, both big-endian, then the bytes (at least one).

use super::kind::MessageKind;

/// The longest payload a message of the session may have, whole, in either direction; a longer one is not sent, and parts that
/// announce one are dropped. 8 MiB: 13 times the largest build seen in a real session (618 781 bytes, a 3-track station), twice
/// what the mod lets an action's body be in the worst escaping (3 MiB, `ACTION_BODY_MAX` in the bridge); the host's byte budget
/// per connection holds one message of it at once (`HostOptions::byte_burst`, 8 MiB), so a player sending one from a full budget
/// is never taken for a flood; at the about 1 MB/s one Steam connection delivers it takes some 8 s per hop, during which every
/// other game waits at its input barrier, which is as long as a session should stand for one item.
pub const MAX_MESSAGE: usize = 8 << 20;

/// The longest payload sealed as one frame; a longer one goes in parts. Sealed and framed it is 4 + 12 + 512 000 + 16 = 512 032
/// bytes, inside the 524 288 of one Steam message (the launcher checks this at compile time).
pub const MAX_FRAME_PAYLOAD: usize = 500 * 1024;

/// A part's header: the whole message's kind, its length and this part's offset.
pub const PART_HEADER: usize = 1 + 4 + 4;

/// Bytes of the message one part carries (the last one may carry fewer).
pub const PART_DATA: usize = MAX_FRAME_PAYLOAD - PART_HEADER;

/// The payloads of the parts `payload` travels in: `Ok(None)` when it fits one frame and goes as it is, `Err` when it is longer
/// than [`MAX_MESSAGE`].
pub(crate) fn split(kind: MessageKind, payload: &[u8]) -> Result<Option<Vec<Vec<u8>>>, String> {
    if payload.len() <= MAX_FRAME_PAYLOAD {
        return Ok(None);
    }
    if payload.len() > MAX_MESSAGE || kind == MessageKind::Part {
        return Err(format!("{} bytes, over the {MAX_MESSAGE} bytes a message may have", payload.len()));
    }
    let total = (payload.len() as u32).to_be_bytes();
    let parts = payload
        .chunks(PART_DATA)
        .enumerate()
        .map(|(i, data)| {
            let mut part = Vec::with_capacity(PART_HEADER + data.len());
            part.push(u8::from(kind));
            part.extend_from_slice(&total);
            part.extend_from_slice(&((i * PART_DATA) as u32).to_be_bytes());
            part.extend_from_slice(data);
            part
        })
        .collect();
    Ok(Some(parts))
}

/// The message in parts arriving on one connection: at most one is open at a time, since a sender never interleaves two.
#[derive(Debug, Default)]
pub(crate) struct Reassembly {
    open: Option<Open>,
}

#[derive(Debug)]
struct Open {
    kind: MessageKind,
    total: usize,
    /// What arrived so far: it grows with the parts, never ahead of them.
    data: Vec<u8>,
    /// When its last part arrived (the receiver's clock).
    last: f64,
}

impl Reassembly {
    /// Takes the payload of a part (opened, so authentic). `Ok(Some)`: the message is whole. `Ok(None)`: more parts follow. `Err`
    /// says what is wrong; the open message, if any, is dropped with it. The text holds numbers only, never the peer's bytes.
    pub fn accept(&mut self, part: &[u8], now: f64) -> Result<Option<(MessageKind, Vec<u8>)>, String> {
        let result = self.take(part, now);
        if result.is_err() {
            self.open = None;
        }
        result
    }

    fn take(&mut self, part: &[u8], now: f64) -> Result<Option<(MessageKind, Vec<u8>)>, String> {
        let Some((head, data)) = part.split_first_chunk::<PART_HEADER>().filter(|(_, d)| !d.is_empty()) else {
            return Err(format!("a part of {} bytes holds no data", part.len()));
        };
        let byte = head[0];
        let total = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let offset = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) as usize;
        let kind = MessageKind::from_u8(byte)
            .filter(|k| !k.is_unsealed() && !matches!(k, MessageKind::Part | MessageKind::Welcome))
            .ok_or_else(|| format!("a part of a message of kind {byte}, which never travels in parts"))?;
        if total > MAX_MESSAGE {
            return Err(format!(
                "a part of a message of {total} bytes, over the {MAX_MESSAGE} bytes a message may have"
            ));
        }
        let open = match self.open.as_mut() {
            Some(o) if o.kind == kind && o.total == total => o,
            Some(o) => {
                let had = (o.data.len(), o.total);
                return Err(format!("a part of another message while {} of {} bytes had arrived", had.0, had.1));
            }
            None => self.open.insert(Open { kind, total, data: Vec::new(), last: now }),
        };
        if offset != open.data.len() || offset + data.len() > total {
            return Err(format!(
                "a part of {} bytes at {offset} of {total}, where {} was next",
                data.len(),
                open.data.len()
            ));
        }
        open.data.extend_from_slice(data);
        open.last = now;
        if open.data.len() < total {
            return Ok(None);
        }
        Ok(self.open.take().map(|o| (o.kind, o.data)))
    }

    /// A whole frame arrived: a message still open is lost (its sender sends nothing between its parts). What was dropped, if any.
    pub fn interrupted(&mut self) -> Option<String> {
        self.open.take().map(|o| format!("{:?} of {} bytes after {} had arrived", o.kind, o.total, o.data.len()))
    }

    /// When the last part of the message still open arrived: the sender is busy sending it.
    pub fn receiving_since(&self) -> Option<f64> {
        self.open.as_ref().map(|o| o.last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn a_long_message_goes_in_parts_that_each_fit_a_frame_and_comes_back_whole() {
        assert_eq!(
            split(MessageKind::Action, &message(MAX_FRAME_PAYLOAD)),
            Ok(None),
            "one frame's worth goes as it is"
        );
        let payload = message(1_500_000);
        let parts = split(MessageKind::Action, &payload).unwrap().unwrap();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() <= MAX_FRAME_PAYLOAD));
        let mut r = Reassembly::default();
        assert_eq!(r.accept(&parts[0], 1.0), Ok(None));
        assert_eq!(r.accept(&parts[1], 2.0), Ok(None));
        assert_eq!(r.receiving_since(), Some(2.0));
        assert_eq!(r.accept(&parts[2], 3.0), Ok(Some((MessageKind::Action, payload))));
        assert_eq!(r.receiving_since(), None);
        assert_eq!(r.interrupted(), None);
    }

    #[test]
    fn a_message_over_the_cap_is_not_split_and_parts_that_announce_one_are_refused() {
        assert!(split(MessageKind::Action, &message(MAX_MESSAGE + 1)).is_err());
        let mut part = split(MessageKind::Action, &message(MAX_FRAME_PAYLOAD + 1)).unwrap().unwrap().remove(0);
        part[1..5].copy_from_slice(&((MAX_MESSAGE + 1) as u32).to_be_bytes());
        let mut r = Reassembly::default();
        assert!(r.accept(&part, 0.0).unwrap_err().contains("over the"));
        assert_eq!(r.receiving_since(), None, "nothing is kept");
    }

    #[test]
    fn malformed_parts_are_refused_and_drop_what_was_open() {
        let parts = split(MessageKind::Action, &message(1_200_000)).unwrap().unwrap();
        let mut r = Reassembly::default();
        // a part that does not start its message, an empty one, one of a kind that never travels in parts
        assert!(r.accept(&parts[1], 0.0).is_err());
        assert!(r.accept(&parts[0][..PART_HEADER], 0.0).is_err());
        for kind in [MessageKind::Welcome, MessageKind::Part, MessageKind::Join] {
            let mut p = parts[0].clone();
            p[0] = kind.into();
            assert!(r.accept(&p, 0.0).is_err(), "{kind:?}");
        }
        let mut unknown = parts[0].clone();
        unknown[0] = 200;
        assert!(r.accept(&unknown, 0.0).is_err());
        // a repeated part, a part of another message, a part past the end: each drops the open message
        let other = split(MessageKind::Chat, &message(1_200_000)).unwrap().unwrap();
        for bad in [&parts[0], &other[1]] {
            assert_eq!(r.accept(&parts[0], 0.0), Ok(None));
            assert!(r.accept(bad, 0.0).is_err());
            assert_eq!(r.receiving_since(), None);
        }
        let mut long = parts[2].clone();
        long.extend_from_slice(&[0; 8]);
        assert_eq!(r.accept(&parts[0], 0.0), Ok(None));
        assert_eq!(r.accept(&parts[1], 0.0), Ok(None));
        assert!(r.accept(&long, 0.0).is_err());
        // a whole frame in the middle of a message: the message is lost, and said
        assert_eq!(r.accept(&parts[0], 0.0), Ok(None));
        assert!(r.interrupted().unwrap().contains("Action of 1200000 bytes"));
        assert!(r.accept(&parts[1], 0.0).is_err(), "its rest is refused");
    }
}
