//! Bug reports: one player asks, every game of the session builds its own part (its logs, redacted on its own PC), the
//! players' launchers send theirs to the host the way a repair's save travels (`ResyncChunkMessage` chunks, checked against
//! their SHA-256 by [`IncomingSave`]), and the host's launcher writes one bundle. A game that does not answer in time is
//! listed as missing; the report completes anyway. The wire rules are `docs/PROTOCOL.md`, "Bug reports".
//!
//! This module is the host's bookkeeping of one report ([`ReportRound`]) and the two payloads only the report uses. No file
//! is read here: what a part holds, the redaction and the bundle are the launcher's (`launcher/src/report`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::messages::ResyncChunkMessage;
use super::resync::{IncomingSave, ResyncLimits};

/// Sizes and timeouts of a report.
#[derive(Debug)]
pub struct ReportLimits;

impl ReportLimits {
    /// A game must answer within this many seconds of the ask: its part (the host's own) or the first chunk of it.
    pub const ANSWER_SECONDS: f64 = 60.0;
    /// A part whose chunks are on their way must be complete within this many seconds of the ask (a big part on a slow link).
    pub const UPLOAD_SECONDS: f64 = 180.0;
    /// The bundle's limit: GitHub takes attachments up to 25 MB, the rest is room for the zip's own overhead.
    pub const MAX_BUNDLE_BYTES: u64 = 24_000_000;
    /// What the bundle keeps for its own `report.txt` and the zip's directory, besides the parts.
    pub const RESERVE_BYTES: u64 = 256 * 1024;
    /// Longest note a player may add ("what were you doing?"), in characters.
    pub const NOTE_CHARS: usize = 300;

    /// The bytes each of `players` parts may take, so that every part fits one bundle under [`Self::MAX_BUNDLE_BYTES`].
    pub fn part_budget(players: usize) -> u64 {
        (Self::MAX_BUNDLE_BYTES - Self::RESERVE_BYTES) / players.max(1) as u64
    }
}

/// Player to host (`ReportRequest`): "make a bug report". The note is what the player typed, cut to
/// [`ReportLimits::NOTE_CHARS`] by the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportRequestMessage {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// Host to every player (`ReportAsk`), and to the host's own launcher: build your part of report `report` and send it as
/// transfer `id`. The note stays with the host: only the bundle's `report.txt` has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportAskMessage {
    /// The transfer id the part's chunks carry (`ResyncChunkMessage::id`), counted by the host from 1.
    pub id: i32,
    /// The report's name for people: a few random hex digits, in the bundle's file name and the issue.
    pub report: String,
    /// Who asked, and the name the others' notice shows.
    pub by: u16,
    pub by_name: String,
    /// The most bytes the part may have (the bundle's limit shared by every game).
    pub max_bytes: u64,
    /// Random per report: every game turns a Steam ID into the same `steam-<hash>` tag with it, so the host can name each
    /// player's own tag `player-N` in the bundle without any Steam ID leaving a PC.
    pub salt: String,
}

/// Host to the player who asked (`ReportBusy`), and to the host's own launcher: report `id`, asked by `by_name`, is still being
/// made, so this request was refused. Its window says so at once instead of waiting for a bundle that will not come.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportBusyMessage {
    /// The transfer id of the report that runs (`ReportAskMessage::id`).
    pub id: i32,
    /// Who asked for the running report: its bundle reaches that player.
    pub by_name: String,
}

/// What a host's report asks of its launcher (`HostOutput::Report`).
#[derive(Debug, Clone, PartialEq)]
pub enum HostReportEvent {
    /// Build the host game's own part and hand it to `HostSession::report_part`.
    BuildPart(ReportAskMessage),
    /// Every part is in or missing: write the bundle; when `ask.by` is a player, send it to that player
    /// (`HostSession::send_report_chunk`).
    Collected(CollectedReport),
    /// The host's own window asked while another report runs: refused, the window says whose it is.
    Busy(ReportBusyMessage),
}

/// What a player's report traffic asks of its launcher (`PlayerOutput::Report`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerReportEvent {
    /// The host asks for this game's part: build it and send it as `ReportPart` chunks of transfer `ask.id`.
    Asked(ReportAskMessage),
    /// The bundle of the report this player asked for arrived whole and matches its SHA-256. `report` is the report's name
    /// from the ask (empty when the ask was not seen).
    Bundle { id: i32, report: String, bytes: Vec<u8> },
    /// The host refused this player's request: another report runs (the window says whose it is).
    Busy(ReportBusyMessage),
}

/// What became of one game's part.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Slot {
    Waiting,
    Uploading,
    Done(Vec<u8>),
    Missing(String),
}

#[derive(Debug)]
struct Part {
    name: String,
    slot: Slot,
    incoming: IncomingSave,
}

/// One game's part of a finished report: its bytes, or why there are none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedPart {
    pub player: u16,
    pub name: String,
    pub bytes: Option<Vec<u8>>,
    pub missing: Option<String>,
}

/// A report whose parts are all in or timed out: the host's launcher writes the bundle from it.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectedReport {
    pub ask: ReportAskMessage,
    pub note: String,
    /// Every game the host asked, the host's own first (id order).
    pub parts: Vec<CollectedPart>,
    /// Seconds from the ask to the end.
    pub seconds: f64,
}

/// The host's bookkeeping of one report: which game has sent its part, which is still sending, which is missing and why.
/// Driven by the host with its clock; no I/O.
#[derive(Debug)]
pub struct ReportRound {
    ask: ReportAskMessage,
    note: String,
    started: f64,
    parts: BTreeMap<u16, Part>,
}

impl ReportRound {
    /// A report asked at `now` of the games `players` (id, name), the host's own included.
    pub fn new(ask: ReportAskMessage, note: &str, players: &[(u16, String)], now: f64) -> ReportRound {
        let parts = players
            .iter()
            .map(|(id, name)| (*id, Part { name: name.clone(), slot: Slot::Waiting, incoming: IncomingSave::new() }))
            .collect();
        let note = note.chars().take(ReportLimits::NOTE_CHARS).collect();
        ReportRound { ask, note, started: now, parts }
    }

    pub fn ask(&self) -> &ReportAskMessage {
        &self.ask
    }

    /// The round waits for chunks of this game's part: the host lets them pass its flood limit, like a repair's save.
    pub fn expects_chunks(&self, player: u16) -> bool {
        self.parts.get(&player).is_some_and(|p| matches!(p.slot, Slot::Waiting | Slot::Uploading))
    }

    /// A whole part from the host's own launcher. False when none is expected from it (or it is over the size).
    pub fn local_part(&mut self, player: u16, bytes: Vec<u8>) -> bool {
        let max = self.ask.max_bytes;
        let Some(p) = self.parts.get_mut(&player).filter(|p| matches!(p.slot, Slot::Waiting | Slot::Uploading)) else {
            return false;
        };
        p.slot = if bytes.is_empty() || bytes.len() as u64 > max {
            Slot::Missing(format!("its part had {} bytes (1 to {max} allowed)", bytes.len()))
        } else {
            Slot::Done(bytes)
        };
        true
    }

    /// One chunk of a player's part. `Ok(true)` when it completed the part; `Err` says why it was refused (a malformed or
    /// oversized part makes that game's part missing, the others go on).
    pub fn chunk(&mut self, player: u16, now: f64, m: ResyncChunkMessage) -> Result<bool, String> {
        if m.id != self.ask.id {
            return Err(format!("chunk of report transfer {} while {} runs", m.id, self.ask.id));
        }
        // a part never needs more chunks than its budget: refused before anything is held
        let most = ResyncLimits::chunk_count(self.ask.max_bytes).unwrap_or(1) as i32;
        let Some(p) = self.parts.get_mut(&player).filter(|p| matches!(p.slot, Slot::Waiting | Slot::Uploading)) else {
            return Err("no part of this game is expected".to_owned());
        };
        if m.count > most {
            p.slot = Slot::Missing(format!("its part was too big ({} chunks, at most {most})", m.count));
            return Err("part over its budget".to_owned());
        }
        match p.incoming.accept(now, m) {
            Ok(None) => {
                p.slot = Slot::Uploading;
                Ok(false)
            }
            Ok(Some(save)) => {
                p.slot = Slot::Done(save.bytes);
                Ok(true)
            }
            Err(e) => {
                p.slot = Slot::Missing(format!("its part could not be used: {e}"));
                Err(e.to_string())
            }
        }
    }

    /// A game left the session: its part is missing unless it was complete.
    pub fn left(&mut self, player: u16) {
        if let Some(p) = self.parts.get_mut(&player).filter(|p| matches!(p.slot, Slot::Waiting | Slot::Uploading)) {
            p.slot = Slot::Missing("it left the session before its part arrived".to_owned());
        }
    }

    /// Every part is in, missing, or out of time at `now`.
    pub fn done(&self, now: f64) -> bool {
        let age = now - self.started;
        self.parts.values().all(|p| match p.slot {
            Slot::Done(_) | Slot::Missing(_) => true,
            Slot::Waiting => age >= ReportLimits::ANSWER_SECONDS,
            Slot::Uploading => age >= ReportLimits::UPLOAD_SECONDS,
        })
    }

    /// The report as it stands at `now`: what did not arrive is listed as missing, with the reason.
    pub fn finish(self, now: f64) -> CollectedReport {
        let parts = self
            .parts
            .into_iter()
            .map(|(player, p)| {
                let (bytes, missing) = match p.slot {
                    Slot::Done(b) => (Some(b), None),
                    Slot::Missing(why) => (None, Some(why)),
                    Slot::Waiting => {
                        (None, Some(format!("it did not answer within {} s", ReportLimits::ANSWER_SECONDS)))
                    }
                    Slot::Uploading => (
                        None,
                        Some(format!("its part did not arrive completely within {} s", ReportLimits::UPLOAD_SECONDS)),
                    ),
                };
                CollectedPart { player, name: p.name, bytes, missing }
            })
            .collect();
        CollectedReport { ask: self.ask, note: self.note, parts, seconds: (now - self.started).max(0.0) }
    }
}

/// Lower-case hex of `n` random bytes; `None` when the system has no randomness to give.
pub(super) fn random_hex(n: usize) -> Option<String> {
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).ok()?;
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::resync::sha256_hex;

    fn ask(max: u64) -> ReportAskMessage {
        ReportAskMessage {
            id: 3,
            report: "a1b2c3".into(),
            by: 1,
            by_name: "Pat".into(),
            max_bytes: max,
            salt: "00".into(),
        }
    }

    fn players() -> Vec<(u16, String)> {
        vec![(0, "Host".into()), (1, "Pat".into()), (2, "Kim".into())]
    }

    /// `bytes` as the chunks a player's launcher sends for transfer `id`.
    fn chunks(id: i32, bytes: &[u8]) -> Vec<ResyncChunkMessage> {
        let count = ResyncLimits::chunk_count(bytes.len() as u64).unwrap();
        let sha256 = sha256_hex(bytes);
        bytes
            .chunks(ResyncLimits::CHUNK_SIZE)
            .enumerate()
            .map(|(i, d)| ResyncChunkMessage {
                id,
                index: i as i32,
                count: count as i32,
                sha256: sha256.clone(),
                data: d.to_vec(),
            })
            .collect()
    }

    #[test]
    fn the_budget_shares_the_bundle_limit_between_the_games() {
        assert_eq!(ReportLimits::part_budget(1), ReportLimits::MAX_BUNDLE_BYTES - ReportLimits::RESERVE_BYTES);
        assert!(ReportLimits::part_budget(8) * 8 + ReportLimits::RESERVE_BYTES <= ReportLimits::MAX_BUNDLE_BYTES);
        assert_eq!(ReportLimits::part_budget(0), ReportLimits::part_budget(1));
    }

    #[test]
    fn a_report_completes_when_every_part_is_in() {
        let mut r = ReportRound::new(ask(4 << 20), "the road vanished", &players(), 0.0);
        assert!(r.local_part(0, b"host part".to_vec()));
        assert!(!r.local_part(0, b"again".to_vec()), "one part per game");
        let big = vec![7u8; ResyncLimits::CHUNK_SIZE + 10];
        let mut sent = chunks(3, &big).into_iter();
        assert_eq!(Ok(false), r.chunk(1, 1.0, sent.next().unwrap()));
        assert!(r.expects_chunks(1));
        assert_eq!(Ok(true), r.chunk(1, 2.0, sent.next().unwrap()));
        assert!(!r.expects_chunks(1));
        assert!(!r.done(3.0), "Kim's part is still awaited");
        for c in chunks(3, b"kim") {
            assert_eq!(Ok(true), r.chunk(2, 3.0, c));
        }
        assert!(r.done(3.0));
        let report = r.finish(4.0);
        assert_eq!("the road vanished", report.note);
        let got: Vec<(u16, Option<usize>)> =
            report.parts.iter().map(|p| (p.player, p.bytes.as_ref().map(Vec::len))).collect();
        assert_eq!(vec![(0, Some(9)), (1, Some(big.len())), (2, Some(3))], got);
        assert!(report.parts.iter().all(|p| p.missing.is_none()));
        assert_eq!(4.0, report.seconds);
    }

    #[test]
    fn a_game_that_does_not_answer_in_time_is_listed_as_missing_and_the_report_still_completes() {
        let mut r = ReportRound::new(ask(4 << 20), "", &players(), 10.0);
        r.local_part(0, b"host".to_vec());
        // Pat started sending, Kim says nothing
        let big = vec![1u8; ResyncLimits::CHUNK_SIZE * 2];
        let first = chunks(3, &big).remove(0);
        assert_eq!(Ok(false), r.chunk(1, 20.0, first));
        assert!(!r.done(10.0 + ReportLimits::ANSWER_SECONDS - 0.1));
        // past the answer time: Kim is out, Pat's upload may go on
        assert!(!r.done(10.0 + ReportLimits::ANSWER_SECONDS));
        assert!(r.done(10.0 + ReportLimits::UPLOAD_SECONDS));
        let report = r.finish(10.0 + ReportLimits::UPLOAD_SECONDS);
        let missing: Vec<(u16, bool)> = report.parts.iter().map(|p| (p.player, p.missing.is_some())).collect();
        assert_eq!(vec![(0, false), (1, true), (2, true)], missing);
        assert!(report.parts[2].missing.as_deref().unwrap().contains("did not answer within 60 s"));
        assert!(report.parts[1].missing.as_deref().unwrap().contains("did not arrive completely"));
    }

    #[test]
    fn a_game_that_leaves_or_sends_garbage_is_missing_with_the_reason_and_the_others_go_on() {
        let mut r = ReportRound::new(ask(4 << 20), "", &players(), 0.0);
        r.local_part(0, b"host".to_vec());
        r.left(1);
        let mut bad = chunks(3, b"kim's part").remove(0);
        bad.sha256 = "0".repeat(64);
        assert!(r.chunk(2, 1.0, bad).is_err(), "the hash must match");
        assert!(r.done(1.0), "nothing is awaited any more");
        let report = r.finish(1.0);
        assert!(report.parts[1].missing.as_deref().unwrap().contains("left the session"));
        assert!(report.parts[2].missing.as_deref().unwrap().contains("could not be used"));
        // a game the round never asked, and a chunk of another transfer, are refused
        let mut r = ReportRound::new(ask(4 << 20), "", &players(), 0.0);
        assert!(r.chunk(9, 0.0, chunks(3, b"x").remove(0)).is_err());
        assert!(r.chunk(1, 0.0, chunks(4, b"x").remove(0)).is_err());
        assert!(!r.local_part(9, b"x".to_vec()));
    }

    #[test]
    fn a_part_over_its_budget_is_refused_before_it_is_held() {
        let mut r = ReportRound::new(ask(ResyncLimits::CHUNK_SIZE as u64), "", &players(), 0.0);
        let big = vec![0u8; ResyncLimits::CHUNK_SIZE * 3];
        assert!(r.chunk(1, 0.0, chunks(3, &big).remove(0)).is_err());
        assert!(!r.expects_chunks(1));
        assert!(r.local_part(0, vec![0u8; ResyncLimits::CHUNK_SIZE + 1]));
        let report = r.finish(0.0);
        assert!(report.parts[0].missing.as_deref().unwrap().contains("allowed"), "{:?}", report.parts[0]);
        assert!(report.parts[1].missing.as_deref().unwrap().contains("too big"));
    }

    #[test]
    fn the_note_is_cut_and_the_messages_keep_their_shape() {
        let long = "x".repeat(ReportLimits::NOTE_CHARS + 50);
        let r = ReportRound::new(ask(10), &long, &players(), 0.0);
        assert_eq!(ReportLimits::NOTE_CHARS, r.finish(0.0).note.chars().count());
        let text = String::from_utf8(crate::session::codec::encode(&ask(10))).unwrap();
        assert_eq!(r#"{"id":3,"report":"a1b2c3","by":1,"byName":"Pat","maxBytes":10,"salt":"00"}"#, text);
        let empty = crate::session::codec::encode(&ReportRequestMessage::default());
        assert_eq!(b"{}".to_vec(), empty);
        assert_eq!(
            Some(ReportRequestMessage { note: "hi".into() }),
            crate::session::codec::try_decode(br#"{"note":"hi","extra":1}"#)
        );
        assert_eq!(Some(12), random_hex(6).map(|s| s.len()));
    }
}
