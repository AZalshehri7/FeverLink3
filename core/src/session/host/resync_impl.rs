//! The host side of the resync round: pause, save, transfer, load, verify. A state machine driven by incoming
//! messages and by [`HostSession::tick`], using the host's injected clock. See `docs/PROTOCOL.md`, "Resync round".

use std::collections::BTreeSet;

use super::{clip, HostOutput, HostSession};
use crate::session::checksum::ChecksumOutcome;
use crate::session::kind::MessageKind;
use crate::session::lockstep::Lockstep;
use crate::session::messages::{
    codec, ChecksumRequestMessage, ResyncChunkMessage, ResyncLoadMessage, ResyncSaveMessage, SaveDoneMessage,
    SessionState,
};
use crate::session::resync::{normalize_sha256, ResyncLimits, ResyncState};

/// The saved file as the launcher announced it.
#[derive(Debug)]
pub(super) struct FileInfo {
    sha256: String,
    len: u64,
    count: u32,
    next: u32,
}

#[derive(Debug)]
pub(super) struct ResyncRound {
    id: i32,
    state: ResyncState,
    reason: String,
    /// Host time the current state was entered.
    entered: f64,
    resume_speed: i32,
    pause_at: i64,
    file: Option<FileInfo>,
    /// Players present when the transfer started: they get the chunks and the load, and must say Hello again.
    participants: Option<BTreeSet<u16>>,
    verify_n: Option<i32>,
}

impl HostSession {
    pub fn resync_state(&self) -> ResyncState {
        self.resync.as_ref().map_or(ResyncState::Idle, |r| r.state)
    }

    /// The id of the round in progress.
    pub fn resync_id(&self) -> Option<i32> {
        self.resync.as_ref().map(|r| r.id)
    }

    /// Starts a resync round by hand (the host's button). Ignored when the session has not started or a round is
    /// already running.
    pub fn start_resync(&mut self, reason: &str) -> Vec<HostOutput> {
        self.begin_resync(reason);
        self.drain()
    }

    /// The launcher read the host game's save from disk: its SHA-256 (hex) and length. Valid in `Transferring`
    /// only. The launcher then streams the file with [`HostSession::resync_chunk`], or nothing when no other
    /// player is connected.
    pub fn resync_file_ready(&mut self, id: i32, sha256: &str, len: u64) -> Vec<HostOutput> {
        self.file_ready(id, sha256, len);
        self.drain()
    }

    /// Seals chunk `index` of the file (in order, from 0) as a ResyncChunk to every participating player. The
    /// chunk must have exactly the size [`ResyncLimits::chunk_len`] gives; at most 256 KiB. After the last chunk
    /// the round moves on to loading.
    pub fn resync_chunk(&mut self, id: i32, index: u32, data: &[u8]) -> Vec<HostOutput> {
        self.send_chunk(id, index, data);
        self.drain()
    }

    /// A player left the game. The round stops counting on it: a player that drops and returns under the same id is
    /// still the same id to the round, would be sent the rest of a transfer it cannot assemble and a load it never
    /// received, and would hold the round in Loading until its timeout because it never says hello to that load.
    /// It takes part in the next round instead (`pending_new_player`).
    pub(super) fn forget_participant(&mut self, id: u16) {
        if let Some(participants) = self.resync.as_mut().and_then(|r| r.participants.as_mut()) {
            participants.remove(&id);
        }
    }

    // ---------------------------------------------------------------- starting

    pub(super) fn begin_resync(&mut self, reason: &str) {
        if !self.state.started {
            self.log(format!("resync ({reason}) ignored: session not started"));
            return;
        }
        if self.resync.is_some() {
            self.log(format!("resync ({reason}) ignored: a round is running"));
            return;
        }
        self.resync_counter += 1;
        let id = self.resync_counter;
        let resume_speed = if self.state.speed > 0 { self.state.speed } else { self.last_running_speed };
        if self.state.speed == 0 && self.state.pause_at.is_none() {
            // A session that has never run holds at speed 0 with no pause stamp (the games were at different times
            // when it started). No game would ever reach a stamp in the future, so the games may run up to it, at
            // the speed the session resumes at. The world that everyone gets is the host's, so the stamp is
            // counted from the host's own clock, not from the furthest game's.
            let base = match self.clocks.get(&Self::HOST_ID) {
                Some(c) => c.estimate(self.now()) as i64,
                None => self.max_clock(),
            };
            let lead = self.wanted_lead(resume_speed);
            let at = Lockstep::round_up_to_step(base.saturating_add(lead) as f64);
            self.state = SessionState { speed: resume_speed, pause_at: Some(at), lead, ..self.state.clone() };
            self.broadcast_session();
        } else if self.state.pause_at.is_none() {
            self.pause_inner();
        }
        let pause_at = self.state.pause_at.unwrap_or(0);
        self.log(format!("resync {id} started: {reason}"));
        self.resync = Some(ResyncRound {
            id,
            state: ResyncState::Pausing,
            reason: reason.to_owned(),
            entered: self.now(),
            resume_speed,
            pause_at,
            file: None,
            participants: None,
            verify_n: None,
        });
        self.set_resync_phase(Some("pausing"));
        self.progress(format!("pausing at {pause_at}: {reason}"));
        self.resync_step();
    }

    /// A player joined (`new player`) or came back (`player returned`): a started session gets a round for it.
    pub(super) fn on_new_player(&mut self, reason: &'static str) {
        if !self.state.started {
            return;
        }
        match &self.resync {
            None => self.begin_resync(reason),
            Some(r) if r.participants.is_some() => {
                self.pending_new_player = Some(reason);
                self.log(format!("{reason} waits for the next resync round"));
            }
            Some(_) => {}
        }
    }

    // ---------------------------------------------------------------- the state machine

    /// Moves the round on as far as the current facts allow and applies the timeouts. Idempotent.
    pub(super) fn resync_step(&mut self) {
        loop {
            let Some((state, entered, pause_at)) = self.resync.as_ref().map(|r| (r.state, r.entered, r.pause_at))
            else {
                return;
            };
            let waited = self.now() - entered;
            match state {
                ResyncState::Pausing => {
                    if self.pause_reached(pause_at) {
                        self.enter_saving();
                    } else if waited >= ResyncLimits::PAUSE_TIMEOUT {
                        let waiting = self.pause_laggards(pause_at);
                        self.fail(format!("games did not stop within 120 s (waiting for {waiting:?})"));
                        return;
                    } else {
                        return;
                    }
                }
                ResyncState::Saving => {
                    if waited >= ResyncLimits::SAVE_TIMEOUT {
                        self.fail("the host's game did not finish saving within 180 s".to_owned());
                    }
                    return;
                }
                ResyncState::Transferring => {
                    if waited >= ResyncLimits::TRANSFER_TIMEOUT {
                        self.fail("the savegame transfer did not finish within 600 s".to_owned());
                    }
                    return;
                }
                ResyncState::Loading => {
                    let missing = self.missing_hellos();
                    if missing.is_empty() {
                        self.enter_verifying();
                    } else if waited >= ResyncLimits::LOAD_TIMEOUT {
                        let names: Vec<String> = missing.iter().map(|&id| self.name(id)).collect();
                        self.log(format!("no hello within 600 s from {}; continuing without them", names.join(", ")));
                        self.enter_verifying();
                    } else {
                        return;
                    }
                }
                ResyncState::Verifying => {
                    if waited >= ResyncLimits::VERIFY_TIMEOUT {
                        self.fail("the verification checksum did not complete within 120 s".to_owned());
                    }
                    return;
                }
                ResyncState::Idle => return,
            }
        }
    }

    /// Every ready game has stopped (speed 0) at or after `pause_at`. Vacuously true with no ready game.
    fn pause_reached(&self, pause_at: i64) -> bool {
        self.pause_laggards(pause_at).is_empty()
    }

    fn pause_laggards(&self, pause_at: i64) -> Vec<u16> {
        self.players
            .values()
            .filter(|p| p.ready)
            .filter(|p| !matches!(self.clocks.get(&p.id), Some(c) if c.t >= pause_at && c.speed == 0))
            .map(|p| p.id)
            .collect()
    }

    fn missing_hellos(&self) -> Vec<u16> {
        let Some(participants) = self.resync.as_ref().and_then(|r| r.participants.as_ref()) else {
            return Vec::new();
        };
        participants.iter().copied().filter(|id| self.players.get(id).is_some_and(|p| !p.ready)).collect()
    }

    fn enter(&mut self, state: ResyncState, detail: String) {
        let now = self.now();
        if let Some(r) = self.resync.as_mut() {
            r.state = state;
            r.entered = now;
        }
        self.set_resync_phase(match state {
            ResyncState::Pausing => Some("pausing"),
            ResyncState::Saving => Some("saving"),
            ResyncState::Transferring => Some("sending"),
            ResyncState::Loading => Some("loading"),
            ResyncState::Verifying => Some("verifying"),
            ResyncState::Idle => None,
        });
        self.progress(detail);
    }

    fn progress(&mut self, detail: String) {
        let (id, state) = self.resync.as_ref().map_or((0, ResyncState::Idle), |r| (r.id, r.state));
        self.out.push(HostOutput::ResyncProgress { id, state, detail });
    }

    fn enter_saving(&mut self) {
        let Some(id) = self.resync_id() else { return };
        let name = format!("feverlink3-resync-{id}");
        self.enter(ResyncState::Saving, format!("saving {name}"));
        let payload = codec::encode(&ResyncSaveMessage { id, name });
        self.out.push(HostOutput::DeliverToLocalGame { kind: MessageKind::ResyncSave, payload });
    }

    pub(super) fn on_save_done(&mut self, from: u16, m: SaveDoneMessage) {
        let Some((state, id)) = self.resync.as_ref().map(|r| (r.state, r.id)) else { return };
        if from != Self::HOST_ID {
            if self.first_time(format!("savedone/{from}")) {
                let who = self.name(from);
                self.log(format!("SaveDone from {who} ignored (only the host's game saves)"));
            }
            return;
        }
        if state != ResyncState::Saving || id != m.id {
            self.log(format!("SaveDone {} ignored", m.id));
            return;
        }
        if m.ok {
            self.enter(ResyncState::Transferring, "waiting for the launcher to read the save".to_owned());
        } else {
            let detail = m.detail.unwrap_or_default();
            self.fail(format!("the host's game could not save: {detail}"));
        }
    }

    fn file_ready(&mut self, id: i32, sha256: &str, len: u64) {
        let ok =
            matches!(&self.resync, Some(r) if r.state == ResyncState::Transferring && r.id == id && r.file.is_none());
        if !ok {
            self.log(format!("save file {id} announced out of turn; ignored"));
            return;
        }
        let Some(sha256) = normalize_sha256(sha256) else {
            self.fail("the launcher announced a malformed sha256".to_owned());
            return;
        };
        let Some(count) = ResyncLimits::chunk_count(len) else {
            self.fail(format!("a save of {len} bytes cannot be sent (1 byte to 512 MiB)"));
            return;
        };
        let participants: BTreeSet<u16> = self.players.keys().copied().collect();
        let remote = participants.iter().any(|&p| p != Self::HOST_ID);
        if let Some(r) = self.resync.as_mut() {
            r.file = Some(FileInfo { sha256, len, count, next: 0 });
            r.participants = Some(participants);
        }
        if remote {
            self.progress(format!("sending {len} bytes in {count} chunk(s)"));
        } else {
            self.enter_loading();
            self.resync_step();
        }
    }

    fn send_chunk(&mut self, id: i32, index: u32, data: &[u8]) {
        let Some(r) = self.resync.as_mut().filter(|r| r.state == ResyncState::Transferring && r.id == id) else {
            self.log(format!("chunk {index} of save {id} ignored: no transfer in progress"));
            return;
        };
        let Some(file) = r.file.as_mut() else {
            self.log("chunk before the file was announced; ignored".to_owned());
            return;
        };
        if index != file.next || ResyncLimits::chunk_len(file.len, index) != Some(data.len()) {
            let (next, size) = (file.next, ResyncLimits::chunk_len(file.len, index));
            self.log(format!("chunk {index} ({} bytes) refused; expected index {next}, size {size:?}", data.len()));
            return;
        }
        let msg = ResyncChunkMessage {
            id,
            index: index as i32,
            count: file.count as i32,
            sha256: file.sha256.clone(),
            data: data.to_vec(),
        };
        file.next += 1;
        let done = file.next == file.count;
        let participants = r.participants.clone().unwrap_or_default();
        self.send_to_players(MessageKind::ResyncChunk, &codec::encode(&msg), &participants);
        if done {
            self.enter_loading();
            self.resync_step();
        }
    }

    fn enter_loading(&mut self) {
        let host_reloads = self.options.host_reloads;
        if !host_reloads {
            // the host's game keeps its live world: it is not sent the load, owes no new hello and keeps its clock
            if let Some(participants) = self.resync.as_mut().and_then(|r| r.participants.as_mut()) {
                participants.remove(&Self::HOST_ID);
            }
        }
        let Some(r) = &self.resync else { return };
        let (id, participants) = (r.id, r.participants.clone().unwrap_or_default());
        let sha256 = r.file.as_ref().map(|f| f.sha256.clone()).unwrap_or_default();
        for p in self.players.values_mut() {
            p.ready = p.id == Self::HOST_ID && !host_reloads;
        }
        if host_reloads {
            self.clocks.clear();
        } else {
            self.clocks.retain(|&id, _| id == Self::HOST_ID);
        }
        self.enter(ResyncState::Loading, format!("loading; waiting for {} game(s)", participants.len()));
        let load = ResyncLoadMessage { id, name: format!("feverlink3-resync-{id}"), sha256 };
        self.send_to_players(MessageKind::ResyncLoad, &codec::encode(&load), &participants);
    }

    fn enter_verifying(&mut self) {
        let Some(at) = self.resync.as_ref().map(|r| r.pause_at) else { return };
        self.checksum_n += 1;
        let n = self.checksum_n;
        self.checksums.open(n);
        if let Some(r) = self.resync.as_mut() {
            r.verify_n = Some(n);
        }
        self.enter(ResyncState::Verifying, "comparing the paused checksums".to_owned());
        let request = ChecksumRequestMessage { n, at, paused: true };
        self.broadcast(MessageKind::ChecksumRequest, &codec::encode(&request), None);
        self.last_checksum = self.now();
    }

    // ---------------------------------------------------------------- results

    pub(super) fn on_checksum_outcome(&mut self, outcome: ChecksumOutcome) {
        let verify = self.resync.as_ref().and_then(|r| r.verify_n);
        self.out.push(HostOutput::ChecksumCompleted(outcome.clone()));
        if self.resync.is_some() {
            if verify == Some(outcome.n) {
                if outcome.identical() {
                    self.finish();
                } else {
                    let parts: Vec<&str> = outcome.differing_parts().collect();
                    self.fail(format!("games still differ after the reload: {}", parts.join(", ")));
                }
            }
            return;
        }
        if !self.state.started {
            return;
        }
        let now = self.now();
        if let Some(reason) = self.policy.on_outcome(now, &outcome) {
            self.request_resync(reason);
        }
    }

    pub(super) fn on_replay_failed(&mut self, from: u16, detail: &str) {
        let who = self.name(from);
        self.log(format!("replay failed on {who}: {}", clip(detail, 200)));
        if self.resync.is_some() || !self.state.started {
            return;
        }
        let now = self.now();
        if let Some(reason) = self.policy.on_replay_failed(now) {
            self.request_resync(reason);
        }
    }

    fn request_resync(&mut self, reason: String) {
        self.out.push(HostOutput::ResyncRequested { reason: reason.clone() });
        if self.options.auto_resync {
            self.begin_resync(&reason);
        }
    }

    fn finish(&mut self) {
        let Some(r) = self.resync.take() else { return };
        self.set_resync_phase(None);
        self.set_speed_inner(r.resume_speed);
        self.policy.resync_finished(self.now());
        self.out.push(HostOutput::ResyncProgress {
            id: r.id,
            state: ResyncState::Idle,
            detail: format!("resumed at x{}", r.resume_speed),
        });
        self.start_pending();
    }

    /// Ends the round without resuming: the session stays paused.
    fn fail(&mut self, reason: String) {
        let Some(r) = self.resync.take() else { return };
        self.set_resync_phase(None);
        self.log(format!("resync {} ({}) failed: {reason}", r.id, r.reason));
        self.policy.resync_finished(self.now());
        self.out.push(HostOutput::ResyncFailed { id: r.id, reason });
        self.start_pending();
    }

    fn start_pending(&mut self) {
        if let Some(reason) = self.pending_new_player.take() {
            self.begin_resync(reason);
        }
    }

    /// Seals one frame per listed player that is still connected.
    fn send_to_players(&mut self, kind: MessageKind, payload: &[u8], players: &BTreeSet<u16>) {
        for &p in players {
            if p == Self::HOST_ID {
                if kind == MessageKind::ResyncLoad {
                    self.out.push(HostOutput::DeliverToLocalGame { kind, payload: payload.to_vec() });
                }
            } else if let Some(connection) = self.connection_of(p) {
                self.send(connection, kind, payload);
            }
        }
    }
}
