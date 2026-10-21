//! Resume after a short drop: what one end sent the other and has no acknowledgement for yet, kept so that a connection that
//! comes back can be handed exactly what it missed (`docs/PROTOCOL.md`, "Reconnect", "Catching up").
//!
//! Both ends keep one. The host keeps every message it sealed for a player (or would have, while the player is away) since the
//! last ping the player answered: a pong proves that everything the host sealed for it before that ping arrived (one ordered
//! connection). The player keeps every message its game sent since the last frame counter the host reports in its pings. When
//! the connection drops and the player comes back within the grace, the join names the last host frame it received, the welcome
//! names the last player frame the host received, and each end sends the other the rest, in the order it was first made, sealed
//! anew. Nothing is skipped and nothing arrives twice, so the player's game, which kept its world, applies every action at its
//! stamp like any other game; lockstep stays exact.

use std::collections::VecDeque;

use super::kind::MessageKind;

/// The messages a returner missed, in order: what [`Outbox::after`] gives.
pub(crate) type Missed = Vec<(MessageKind, Vec<u8>)>;

/// One message kept for a resume: the frame counter it was sealed with (`None`: never sealed, the peer was away).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Kept {
    pub counter: Option<u64>,
    pub kind: MessageKind,
    pub payload: Vec<u8>,
}

/// What one end sent the other and has no acknowledgement for. Bounded by [`Outbox::MAX_BYTES`].
#[derive(Debug, Default, Clone)]
pub(crate) struct Outbox {
    entries: VecDeque<Kept>,
    bytes: usize,
    /// The newest counter dropped for room (the host's side): a peer that did not receive it cannot be caught up.
    trimmed: u64,
    /// Why this outbox can no longer catch anyone up (set once, cleared by [`Outbox::clear`]).
    broken: Option<String>,
}

impl Outbox {
    /// At most this many payload bytes are kept: a minute of heavy building is far below it (a build is at most 480 KiB, a clock line
    /// about 100 bytes). Beyond it the oldest go, and a peer that needs them gets the old repair (a reload) instead.
    pub const MAX_BYTES: usize = 32 << 20;

    /// Keeps a message. `overflow_breaks`: dropping an entry for room makes the outbox useless (the player's side, which cannot know
    /// whether the host had it); otherwise a sealed entry dropped only raises the floor a returning peer must have reached.
    pub fn push(&mut self, counter: Option<u64>, kind: MessageKind, payload: &[u8], overflow_breaks: bool) {
        if self.broken.is_some() {
            return;
        }
        self.bytes += payload.len();
        self.entries.push_back(Kept { counter, kind, payload: payload.to_vec() });
        while self.bytes > Self::MAX_BYTES {
            let Some(old) = self.entries.pop_front() else { break };
            self.bytes -= old.payload.len();
            match old.counter {
                Some(c) if !overflow_breaks => self.trimmed = self.trimmed.max(c),
                _ => {
                    self.break_with(format!("more than {} MiB unacknowledged", Self::MAX_BYTES >> 20));
                    return;
                }
            }
        }
    }

    /// The other end has everything sealed up to `counter`.
    pub fn ack(&mut self, counter: u64) {
        while self.entries.front().is_some_and(|e| e.counter.is_some_and(|c| c <= counter)) {
            if let Some(old) = self.entries.pop_front() {
                self.bytes -= old.payload.len();
            }
        }
        self.trimmed = self.trimmed.max(counter);
    }

    /// The messages a peer whose last received frame was `last` has not seen, in order; `Err` names why it cannot be caught up.
    pub fn after(&self, last: u64) -> Result<Missed, String> {
        if let Some(why) = &self.broken {
            return Err(why.clone());
        }
        if last < self.trimmed {
            return Err(format!("frames after {last} up to {} are no longer kept", self.trimmed));
        }
        Ok(self
            .entries
            .iter()
            .filter(|e| e.counter.is_none_or(|c| c > last))
            .map(|e| (e.kind, e.payload.clone()))
            .collect())
    }

    /// Nothing can be caught up from here on (the world the peer had was replaced, the grace ran out).
    pub fn break_with(&mut self, why: String) {
        if self.broken.is_none() {
            self.broken = Some(why);
        }
        self.entries.clear();
        self.bytes = 0;
    }

    pub fn is_broken(&self) -> bool {
        self.broken.is_some()
    }

    /// Starts over, empty and usable (a new connection whose stream starts here).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Takes every entry out, leaving an empty, usable outbox.
    pub fn take(&mut self) -> Vec<Kept> {
        let out = self.entries.drain(..).collect();
        self.clear();
        out
    }

    /// Whether a host message kind belongs to the game's stream (what a resume replays). Not: the handshake, the session layer's own
    /// pings, a kick, and a save transfer or load (a world replaced is never caught up; see `HostSession::resume`).
    pub fn replayable(kind: MessageKind) -> bool {
        use MessageKind::*;
        !matches!(
            kind,
            Challenge | Join | Reject | Welcome | ResyncSave | ResyncChunk | ResyncLoad | Ping | Kicked | Pong | Leave
        )
    }

    /// Whether a kind the player's game sends is resent after a resume: all of them (clocks, actions, verdicts, reports, chat) but the
    /// session layer's own answers and a goodbye.
    pub fn replayable_from_player(kind: MessageKind) -> bool {
        kind.allowed_from_player()
            && !matches!(kind, MessageKind::Pong | MessageKind::Leave | MessageKind::ResyncUpload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_is_given_what_came_after_its_last_frame_and_what_was_kept_while_it_was_away() {
        let mut o = Outbox::default();
        o.push(Some(5), MessageKind::Action, b"a", false);
        o.push(Some(7), MessageKind::PeerClock, b"b", false);
        o.push(None, MessageKind::Action, b"c", false);
        let after = |o: &Outbox, last| o.after(last).map(|v| v.into_iter().map(|(_, p)| p).collect::<Vec<_>>());
        assert_eq!(Ok(vec![b"b".to_vec(), b"c".to_vec()]), after(&o, 5));
        o.ack(5);
        assert_eq!(Ok(vec![b"b".to_vec(), b"c".to_vec()]), after(&o, 5));
        assert!(after(&o, 4).is_err(), "a frame it never had is gone");
        o.break_with("replaced".into());
        assert_eq!(Err("replaced".to_owned()), after(&o, 7));
    }

    #[test]
    fn overflow_raises_the_floor_on_the_host_and_breaks_the_player() {
        let big = vec![0u8; Outbox::MAX_BYTES / 2 + 1];
        let mut host = Outbox::default();
        host.push(Some(1), MessageKind::Action, &big, false);
        host.push(Some(2), MessageKind::Action, &big, false);
        assert!(host.after(0).is_err() && host.after(1).is_ok());
        let mut player = Outbox::default();
        player.push(Some(1), MessageKind::Action, &big, true);
        player.push(Some(2), MessageKind::Action, &big, true);
        assert!(player.is_broken());
    }
}
