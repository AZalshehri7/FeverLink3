//! The host's side of a bug report (`docs/PROTOCOL.md`, "Bug reports"): one report at a time, asked by any game; every
//! connected game is asked for its part, the parts come back as chunks the way a repair's save does, and after the last
//! part or the timeout the launcher gets the collected report ([`HostReportEvent::Collected`]).

use super::{HostOutput, HostSession};
use crate::session::kind::MessageKind;
use crate::session::messages::{codec, ChatMessage, ResyncChunkMessage};
use crate::session::notices::{self, Level};
use crate::session::report::{
    random_hex, HostReportEvent, ReportAskMessage, ReportBusyMessage, ReportLimits, ReportRequestMessage, ReportRound,
};

impl HostSession {
    /// The host's own window asks for a bug report. `note`: what the player typed ("what were you doing?").
    pub fn request_report(&mut self, note: &str) -> Vec<HostOutput> {
        self.start_report(Self::HOST_ID, note);
        self.drain()
    }

    /// The host game's own part of report transfer `id`, built by its launcher.
    pub fn report_part(&mut self, id: i32, bytes: Vec<u8>) -> Vec<HostOutput> {
        match self.report.as_mut().filter(|r| r.ask().id == id) {
            Some(r) => {
                if r.local_part(Self::HOST_ID, bytes) {
                    self.log(format!("report {id}: the host's part is ready"));
                }
            }
            None => self.log(format!("report {id}: the host's part came after the report ended; dropped")),
        }
        self.report_step();
        self.drain()
    }

    /// One chunk of a finished bundle for the player who asked for it (`ReportBundle`). The launcher paces the chunks; nothing
    /// is sent when the player is gone.
    pub fn send_report_chunk(&mut self, to: u16, m: &ResyncChunkMessage) -> Vec<HostOutput> {
        match self.connection_of(to) {
            Some(connection) => {
                self.send(connection, MessageKind::ReportBundle, &codec::encode(m));
            }
            None => {
                if self.first_time(format!("report-bundle/{to}/{}", m.id)) {
                    let who = self.name(to);
                    self.log(format!("report {}: {who} is gone; the bundle is not sent", m.id));
                }
            }
        }
        self.drain()
    }

    /// A report is being collected: its transfer id.
    pub fn report_running(&self) -> Option<i32> {
        self.report.as_ref().map(|r| r.ask().id)
    }

    /// The parts of the running report this player may still send: its chunks pass the flood limit (like a repair's save).
    pub(super) fn report_upload_from(&self, player: u16) -> bool {
        player != Self::HOST_ID && self.report.as_ref().is_some_and(|r| r.expects_chunks(player))
    }

    pub(super) fn on_report_request(&mut self, from: u16, payload: &[u8]) {
        let note = codec::try_decode::<ReportRequestMessage>(payload).map(|m| m.note).unwrap_or_default();
        self.start_report(from, &note);
    }

    /// Asks every connected game for its part. A second request while one runs is refused at once: its sender gets
    /// `ReportBusy` (the host's own window `HostReportEvent::Busy`) naming who asked for the running one.
    fn start_report(&mut self, by: u16, note: &str) {
        let by_name = self.name(by);
        if let Some(r) = &self.report {
            let busy = ReportBusyMessage { id: r.ask().id, by_name: r.ask().by_name.clone() };
            self.log(format!(
                "report: {by_name} asked for a bug report while report {} (asked by {}) runs; refused",
                busy.id, busy.by_name
            ));
            if by == Self::HOST_ID {
                self.out.push(HostOutput::Report(HostReportEvent::Busy(busy)));
            } else if let Some(connection) = self.connection_of(by) {
                self.send(connection, MessageKind::ReportBusy, &codec::encode(&busy));
            }
            return;
        }
        self.report_counter += 1;
        let id = self.report_counter;
        let players: Vec<(u16, String)> = self.players.values().map(|p| (p.id, p.name.clone())).collect();
        // without randomness the report still goes on: a fixed salt only makes the Steam ID tags guessable within it
        let report = random_hex(3).unwrap_or_else(|| format!("{id:06x}"));
        let salt = random_hex(16).unwrap_or_else(|| report.clone());
        let ask = ReportAskMessage {
            id,
            report: report.clone(),
            by,
            by_name: by_name.clone(),
            max_bytes: ReportLimits::part_budget(players.len()),
            salt,
        };
        let names: Vec<&str> = players.iter().map(|(_, n)| n.as_str()).collect();
        self.log(format!(
            "report {id} ({report}): {by_name} asked for a bug report; asking {} game(s) for their logs: {}",
            players.len(),
            names.join(", ")
        ));
        let now = self.now();
        self.report = Some(ReportRound::new(ask.clone(), note, &players, now));
        let payload = codec::encode(&ask);
        for &(player, _) in players.iter().filter(|(p, _)| *p != Self::HOST_ID) {
            if let Some(connection) = self.connection_of(player) {
                self.send(connection, MessageKind::ReportAsk, &payload);
            }
            if player != by {
                self.notice_to(player, Level::Info, &notices::report_made(&by_name));
            }
        }
        if by != Self::HOST_ID {
            self.notice_to(Self::HOST_ID, Level::Info, &notices::report_made_host(&by_name));
        }
        self.out.push(HostOutput::Report(HostReportEvent::BuildPart(ask)));
    }

    /// A chunk of a player's part (`ReportPart`).
    pub(super) fn on_report_part(&mut self, from: u16, payload: &[u8]) {
        let who = self.name(from);
        let Some(m) = codec::try_decode::<ResyncChunkMessage>(payload) else {
            self.dropped += 1;
            self.log(format!("a report chunk from {who} does not decode; dropped"));
            return;
        };
        let now = self.now();
        let (id, index) = (m.id, m.index);
        let Some(r) = self.report.as_mut() else {
            self.dropped += 1;
            if self.first_time(format!("report-part/{from}/{id}")) {
                self.log(format!("report chunk {index} from {who} ignored: no report runs"));
            }
            return;
        };
        match r.chunk(from, now, m) {
            Ok(true) => self.log(format!("report {id}: {who}'s part arrived")),
            Ok(false) => {}
            Err(why) => {
                self.dropped += 1;
                if self.first_time(format!("report-part/{from}/{id}/err")) {
                    self.log(format!("report {id}: a chunk from {who} refused: {why}"));
                }
            }
        }
        self.report_step();
    }

    /// A game left: its part is missing unless it was complete.
    pub(super) fn report_player_left(&mut self, id: u16) {
        if let Some(r) = self.report.as_mut() {
            r.left(id);
        }
        self.report_step();
    }

    /// Ends the running report once every part is in, missing, or out of time; the launcher writes the bundle.
    pub(super) fn report_step(&mut self) {
        let now = self.now();
        if !self.report.as_ref().is_some_and(|r| r.done(now)) {
            return;
        }
        let Some(r) = self.report.take() else { return };
        let report = r.finish(now);
        let missing: Vec<String> =
            report.parts.iter().filter_map(|p| p.missing.as_ref().map(|why| format!("{} ({why})", p.name))).collect();
        self.log(format!(
            "report {} ({}): collected in {:.1} s; missing: {}",
            report.ask.id,
            report.ask.report,
            report.seconds,
            if missing.is_empty() { "none".to_owned() } else { missing.join(", ") }
        ));
        self.out.push(HostOutput::Report(HostReportEvent::Collected(report)));
    }

    /// A notice for one game only (a chat line with a level, as [`HostSession::announce`] sends to every game).
    fn notice_to(&mut self, player: u16, level: Level, text: &str) {
        let chat = ChatMessage {
            text: text.to_owned(),
            from: Self::HOST_ID,
            name: Some("FeverLink3".to_owned()),
            level: Some(level.as_str().to_owned()),
        };
        let payload = codec::encode(&chat);
        if player == Self::HOST_ID {
            self.out.push(HostOutput::DeliverToLocalGame { kind: MessageKind::Chat, payload });
        } else if let Some(connection) = self.connection_of(player) {
            self.send(connection, MessageKind::Chat, &payload);
        }
    }
}
