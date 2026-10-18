//! The host side of the resync round: pause, save, transfer, load, verify. A state machine driven by incoming
//! messages and by [`HostSession::tick`], using the host's injected clock. See `docs/PROTOCOL.md`, "Resync round".

use std::collections::BTreeSet;

use super::{clip, HostOutput, HostSession};
use crate::session::checksum::ChecksumOutcome;
use crate::session::kind::MessageKind;
use crate::session::lockstep::{Lockstep, PlayerClock};
use crate::session::messages::{
    codec, ActionRefusedMessage, ChecksumRequestMessage, ResyncChunkMessage, ResyncLoadMessage, ResyncSaveMessage,
    SaveDoneMessage, SessionState,
};
use crate::session::resync::{normalize_sha256, ResyncLimits, ResyncState};
use crate::session::verdict::{Verdict, VerdictOutcome};

/// The saved file as the launcher announced it.
#[derive(Debug)]
pub(super) struct FileInfo {
    sha256: String,
    len: u64,
    count: u32,
    next: u32,
}

/// How often a paused verify round that cannot be compared (different sim times) is asked again.
const MAX_VERIFY_SKEWS: u8 = 3;
/// How often a paused verify round in which a game reported a part not ready (and nothing differs) is asked again.
const MAX_VERIFY_NOT_READY: u8 = 3;
/// A line in the session log with the count of skewed rounds after this many finished rounds.
const SKEW_LOG_EVERY: u32 = 20;

#[derive(Debug)]
pub(super) struct ResyncRound {
    id: i32,
    state: ResyncState,
    reason: String,
    /// Host time the current state was entered.
    entered: f64,
    resume_speed: i32,
    /// The pause the round stops the games at; after the load, the time the loaded world stands at (`on_load_hello`).
    pause_at: i64,
    /// The time the participants' games said hello at after the load (the save's time), once one has.
    loaded_at: Option<i64>,
    file: Option<FileInfo>,
    /// Players present when the transfer started: they get the chunks and the load, and must say Hello again.
    participants: Option<BTreeSet<u16>>,
    /// Participants whose save travels another way (bulk datagrams): they are not sent `ResyncChunk`s, and the
    /// launcher falls back to [`HostSession::resync_chunk_to`] for one whose transfer fails.
    bulk: BTreeSet<u16>,
    verify_n: Option<i32>,
    /// Paused verify rounds that came back not comparable (the games reported different sim times): asked again.
    verify_skews: u8,
    /// Paused verify rounds in which a game reported a part not ready: asked again.
    verify_not_ready: u8,
    /// The host's game was sent the load while the players are still receiving the save (it is paused, so it can
    /// reload in that time instead of after it).
    host_loading: bool,
    /// What the games of the participants were sent while their save was on its way, held back until the load
    /// (`HostSession::holds_for_load`): (receiver, kind, payload), the host's own game as [`HostSession::HOST_ID`].
    held: Vec<(u16, MessageKind, Vec<u8>)>,
}

/// At most this many messages are held for the games of a round while the save travels; more are dropped (counted
/// and logged). A paused round sees a few actions and clocks, not thousands.
const MAX_HELD: usize = 20_000;

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

    /// The players (not the host's game) the transfer in progress goes to. Valid once the file was announced.
    pub fn resync_receivers(&self) -> Vec<u16> {
        self.resync
            .as_ref()
            .and_then(|r| r.participants.as_ref())
            .map(|p| p.iter().copied().filter(|&id| id != Self::HOST_ID).collect())
            .unwrap_or_default()
    }

    /// Whether the player announced `cap` in its join message.
    pub fn player_supports(&self, id: u16, cap: &str) -> bool {
        self.identities.get(&id).is_some_and(|i| i.caps.iter().any(|c| c == cap))
    }

    /// The save of round `id` travels to these players another way: `resync_chunk` no longer sends them chunks. Call
    /// after `resync_file_ready` and before the first chunk. Players that are not receivers are ignored.
    pub fn resync_set_bulk(&mut self, id: i32, players: &[u16]) {
        let Some(r) = self.resync.as_mut().filter(|r| r.id == id && r.state == ResyncState::Transferring) else {
            return;
        };
        let receivers = r.participants.clone().unwrap_or_default();
        r.bulk = players.iter().copied().filter(|p| *p != Self::HOST_ID && receivers.contains(p)).collect();
    }

    /// Seals chunk `index` of the file to one player only, whatever the round's chunk counter says: the fallback when
    /// a bulk transfer to that player failed. Does not move the round on. The chunk must have exactly the size
    /// [`ResyncLimits::chunk_len`] gives.
    pub fn resync_chunk_to(&mut self, id: i32, player: u16, index: u32, data: &[u8]) -> Vec<HostOutput> {
        let ok = self.resync.as_ref().and_then(|r| {
            let file = r.file.as_ref()?;
            let valid = r.id == id
                && r.state == ResyncState::Transferring
                && r.participants.as_ref().is_some_and(|p| p.contains(&player))
                && ResyncLimits::chunk_len(file.len, index) == Some(data.len());
            valid.then(|| (file.sha256.clone(), file.count))
        });
        if let Some((sha256, count)) = ok {
            let msg = ResyncChunkMessage { id, index: index as i32, count: count as i32, sha256, data: data.to_vec() };
            let only: BTreeSet<u16> = [player].into();
            self.send_to_players(MessageKind::ResyncChunk, &codec::encode(&msg), &only);
        } else {
            self.log(format!("chunk {index} of save {id} for player {player} refused"));
        }
        self.drain()
    }

    /// A player left the game. The round stops counting on it: a player that drops and returns under the same id is
    /// still the same id to the round, would be sent the rest of a transfer it cannot assemble and a load it never
    /// received, and would hold the round in Loading until its timeout because it never says hello to that load.
    /// It takes part in the next round instead (`pending_new_player`).
    pub(super) fn forget_participant(&mut self, id: u16) {
        if let Some(r) = self.resync.as_mut() {
            r.bulk.remove(&id);
            if let Some(participants) = r.participants.as_mut() {
                participants.remove(&id);
            }
            r.held.retain(|(to, ..)| *to != id);
        }
    }

    /// Whether a message of `kind` for the game of `to` must wait for that game's load. While the save of a round
    /// travels, the participants' games are paused at the save's point and will replace their world by the save; a
    /// message sent to them now (an action, a commit, a clock, a session line) would reach the world that is about to be
    /// thrown away. So it is held and sent right after the `ResyncLoad`, and the reloaded game reads it after its load
    /// line. Before the launcher sent the save on its own thread the host's loop was stuck in the transfer and every such
    /// message went out after the load by accident; this keeps that order on purpose. Not held: what the session layer
    /// handles itself (pings, the transfer, the load, a kick), and anything for the host's own game once it was sent
    /// its load early or when it does not reload.
    pub(super) fn holds_for_load(&self, to: u16, kind: MessageKind) -> bool {
        use MessageKind::*;
        if matches!(kind, Challenge | Join | Reject | Welcome | ResyncChunk | ResyncLoad | Ping | Kicked) {
            return false;
        }
        let Some(r) = self.resync.as_ref().filter(|r| r.state == ResyncState::Transferring) else {
            return false;
        };
        if !r.participants.as_ref().is_some_and(|p| p.contains(&to)) {
            return false;
        }
        to != Self::HOST_ID || (self.options.host_reloads && !r.host_loading)
    }

    /// Keeps a message for the game of `to` until its load ([`HostSession::holds_for_load`]).
    pub(super) fn hold_for_load(&mut self, to: u16, kind: MessageKind, payload: &[u8]) {
        let full = self.resync.as_ref().is_none_or(|r| r.held.len() >= MAX_HELD);
        if full {
            self.dropped += 1;
            if self.first_time("held/full".to_owned()) {
                self.log(format!(
                    "more than {MAX_HELD} messages held for the games during a save transfer; {kind:?} dropped"
                ));
            }
            return;
        }
        if let Some(r) = self.resync.as_mut() {
            r.held.push((to, kind, payload.to_vec()));
        }
    }

    /// Sends what was held for the games while the save travelled, in the order it was made. Called once the round left
    /// `Transferring` (after the loads went out, or because the round ended without them).
    fn release_held(&mut self, held: Vec<(u16, MessageKind, Vec<u8>)>) {
        if held.is_empty() {
            return;
        }
        self.log(format!("{} message(s) held during the save transfer sent after the load", held.len()));
        for (to, kind, payload) in held {
            if to == Self::HOST_ID {
                self.out.push(HostOutput::DeliverToLocalGame { kind, payload });
            } else if let Some(connection) = self.connection_of(to) {
                self.send(connection, kind, &payload);
            }
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
        // every game gets the host's world: what the verdicts of the old worlds said no longer matters
        self.verdicts.clear();
        let id = self.resync_counter;
        // Tell everyone before the games pause: a repair copies the host's world to everybody, so a build that never reached the host
        // (a kind that is not replicated yet) is lost on the other game.
        match reason {
            "session start" | "new player" | "player returned" => {}
            "manual" => self.announce("A sync starts: everyone gets the host's world."),
            other => self.announce(&format!(
                "A repair sync starts ({other}): builds of this kind that did not reach the other game will be lost."
            )),
        }
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
            loaded_at: None,
            file: None,
            participants: None,
            bulk: BTreeSet::new(),
            verify_n: None,
            verify_skews: 0,
            verify_not_ready: 0,
            host_loading: false,
            held: Vec::new(),
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

    /// From the save to the load: the games' worlds are about to be replaced by the save, so what they report (where they stand still)
    /// says nothing about the world they will run.
    pub(super) fn resync_replaces_world(&self) -> bool {
        self.resync
            .as_ref()
            .is_some_and(|r| matches!(r.state, ResyncState::Saving | ResyncState::Transferring | ResyncState::Loading))
    }

    /// A participant's game said hello after the round's load, its world standing at `time`: the save's time, the same on every game
    /// (they all loaded one file). Every step before it belongs to the world the games left, so the session's pause moves there, and
    /// so do the round's verify stamp and the floor of the pause order (no stamp of the pause before it). Real friend test 2026-10-16:
    /// the pause stayed at 10341800 after both games had loaded the save at 10342400; a build made then was stamped at the stale pause
    /// and reached the other game 3 steps after its stamp (LATE, a fork, another resync). The game's clock from before the load goes
    /// too: it describes the world it left (the clock that follows the hello is the loaded world's).
    pub(super) fn on_load_hello(&mut self, from: u16, time: i64) {
        let Some(r) = self.resync.as_mut() else { return };
        let loading = matches!(r.state, ResyncState::Transferring | ResyncState::Loading);
        if !loading || !r.participants.as_ref().is_some_and(|p| p.contains(&from)) {
            return;
        }
        if !PlayerClock::plausible(time, 0) {
            self.dropped += 1;
            let who = self.name(from);
            self.log(format!("hello of {who} after the load does not move the pause (time {time})"));
            return;
        }
        let id = r.id;
        let earlier = r.loaded_at;
        let loaded = earlier.map_or(time, |t| t.max(time));
        let round = r.pause_at;
        r.loaded_at = Some(loaded);
        r.pause_at = r.pause_at.max(loaded);
        let pause = r.pause_at;
        self.clocks.remove(&from);
        if let Some(t) = earlier.filter(|&t| t != time) {
            // one save, one time: a game that says otherwise did not load what the others did (the verify round will tell)
            let who = self.name(from);
            self.log(format!(
                "resync {id}: {who} loaded at {time}, another game at {t}; the pause is the furthest, {pause}"
            ));
        }
        // the pause order's floor is a stamp of the world before the load
        self.pause_last = None;
        if self.state.pause_at != Some(pause) {
            let was = self.state.pause_at.map_or_else(|| "none".to_owned(), |p| p.to_string());
            self.log(format!(
                "resync {id}: the games loaded the save at {loaded}; the pause moves from {was} (round {round}) to {pause}"
            ));
            self.state.pause_at = Some(pause);
            self.broadcast_session();
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
            self.load_host_early();
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
        let mut participants = r.participants.clone().unwrap_or_default();
        for bulk in &r.bulk {
            participants.remove(bulk);
        }
        // nobody is sent this chunk (every receiver gets the save by bulk datagrams): do not even seal it
        if participants.iter().any(|&p| p != Self::HOST_ID) {
            self.send_to_players(MessageKind::ResyncChunk, &codec::encode(&msg), &participants);
        }
        if done {
            self.enter_loading();
            self.resync_step();
        }
    }

    /// With a reloading host the host's game loads its own save now, while the players download it: the host is paused
    /// and its file is already complete, so the reload costs no extra time after the transfer.
    fn load_host_early(&mut self) {
        if !self.options.host_reloads || !self.options.host_loads_early {
            return;
        }
        let Some(r) = self.resync.as_ref() else { return };
        let (id, sha256) = (r.id, r.file.as_ref().map(|f| f.sha256.clone()).unwrap_or_default());
        if let Some(p) = self.players.get_mut(&Self::HOST_ID) {
            p.ready = false;
        }
        self.clocks.remove(&Self::HOST_ID);
        if let Some(r) = self.resync.as_mut() {
            r.host_loading = true;
        }
        let load = ResyncLoadMessage { id, name: format!("feverlink3-resync-{id}"), sha256 };
        let host: BTreeSet<u16> = [Self::HOST_ID].into_iter().collect();
        self.send_to_players(MessageKind::ResyncLoad, &codec::encode(&load), &host);
    }

    fn enter_loading(&mut self) {
        let host_reloads = self.options.host_reloads;
        let host_loading = self.resync.as_ref().is_some_and(|r| r.host_loading);
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
            if p.id == Self::HOST_ID && host_loading {
                continue; // loaded early: its hello may already be in
            }
            p.ready = p.id == Self::HOST_ID && !host_reloads;
        }
        if host_loading {
            self.clocks.retain(|&id, _| id == Self::HOST_ID);
        } else if host_reloads {
            self.clocks.clear();
        } else {
            self.clocks.retain(|&id, _| id == Self::HOST_ID);
        }
        self.enter(ResyncState::Loading, format!("loading; waiting for {} game(s)", participants.len()));
        let load = ResyncLoadMessage { id, name: format!("feverlink3-resync-{id}"), sha256 };
        let mut to_load = participants;
        if host_loading {
            to_load.remove(&Self::HOST_ID);
        }
        self.send_to_players(MessageKind::ResyncLoad, &codec::encode(&load), &to_load);
        let held = self.resync.as_mut().map(|r| std::mem::take(&mut r.held)).unwrap_or_default();
        self.release_held(held);
    }

    fn enter_verifying(&mut self) {
        let Some(at) = self.resync.as_ref().map(|r| r.pause_at) else { return };
        // a regular round asked before the resync is of the world the games just replaced: never waited for after it
        self.forget_running_round();
        self.checksum_n += 1;
        let n = self.checksum_n;
        self.checksums.open(n);
        if let Some(r) = self.resync.as_mut() {
            r.verify_n = Some(n);
        }
        self.enter(ResyncState::Verifying, "comparing the paused checksums".to_owned());
        let request = ChecksumRequestMessage { n, at, paused: true, full: true, spread: 0 };
        self.broadcast(MessageKind::ChecksumRequest, &codec::encode(&request), None);
        self.last_checksum = self.now();
    }

    // ---------------------------------------------------------------- results

    /// Counts a finished round and, every `SKEW_LOG_EVERY` rounds, logs how many were skipped as skewed (the games hashed
    /// at different sim times), so a real game can measure how well the games hash at the same time.
    fn count_round(&mut self, skewed: bool) {
        self.rounds_done += 1;
        if skewed {
            self.rounds_skewed += 1;
        }
        if self.rounds_done.is_multiple_of(SKEW_LOG_EVERY) {
            let (done, skewed) = (self.rounds_done, self.rounds_skewed);
            self.log(format!(
                "checksum rounds: {skewed} of {done} skipped as skewed (games hashed at different sim times)"
            ));
        }
    }

    pub(super) fn on_checksum_outcome(&mut self, outcome: ChecksumOutcome) {
        let verify = self.resync.as_ref().and_then(|r| r.verify_n);
        self.count_round(!outcome.comparable());
        self.out.push(HostOutput::ChecksumCompleted(outcome.clone()));
        if self.resync.is_some() {
            if verify == Some(outcome.n) {
                if outcome.identical() {
                    self.finish();
                } else if outcome.comparable() && outcome.differences.is_empty() && !outcome.ready() {
                    // a game could not hash a part yet (right after its load): unknown, not a difference. Asked again a few
                    // times; a part two games did hash and that differs fails the repair at once (the branch below).
                    let tries = self.resync.as_mut().map_or(0, |r| {
                        r.verify_not_ready += 1;
                        r.verify_not_ready
                    });
                    let what = outcome.describe_not_ready();
                    if tries <= MAX_VERIFY_NOT_READY {
                        self.log(format!(
                            "verify round {}: not ready yet ({what}); asked again ({tries} of {MAX_VERIFY_NOT_READY})",
                            outcome.n
                        ));
                        self.enter_verifying();
                    } else {
                        self.fail(format!(
                            "the games could not be verified after the reload: not ready after {MAX_VERIFY_NOT_READY} retries ({what})"
                        ));
                    }
                } else if !outcome.comparable() {
                    // the games sampled at different sim times: ask again (a few times) before giving up, so a skewed round
                    // does not leave the host paused with a failed resync
                    let again = self.resync.as_mut().is_some_and(|r| {
                        r.verify_skews += 1;
                        r.verify_skews <= MAX_VERIFY_SKEWS
                    });
                    if again {
                        self.enter_verifying();
                    } else {
                        self.fail(
                            "games reported different times while paused, so the checksums cannot be compared"
                                .to_owned(),
                        );
                    }
                } else {
                    let parts: Vec<&str> = outcome.differing_parts().collect();
                    let unknown = if outcome.ready() {
                        String::new()
                    } else {
                        format!(" (not measured: {})", outcome.describe_not_ready())
                    };
                    self.fail(format!("games still differ after the reload: {}{unknown}", parts.join(", ")));
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

    /// A game's verdict of an action at its stamp. A refusal decided at the stamp from the game's own world is deterministic: identical
    /// worlds refuse alike, so a refusal alone asks for nothing (lab run 2026-10-18: a town replaced a building during a build's lead,
    /// both games refused the build at its stamp, and each asked for a resync of two identical worlds). Only verdicts that DIFFER
    /// between games (one applied it, another refused it) are a fork, and that asks for the repair at once.
    pub(super) fn on_action_verdict(&mut self, from: u16, m: ActionRefusedMessage) {
        let who = self.name(from);
        let key = format!("{}:{}", m.from, m.seq);
        if !m.applied {
            let class = m.class.as_deref().map(|c| format!(" ({})", clip(c, 40))).unwrap_or_default();
            self.log(format!("action {key} refused on {who} at its stamp{class}: {}", clip(&m.detail, 200)));
        }
        if self.resync.is_some() || !self.state.started || !self.players.get(&from).is_some_and(|p| p.ready) {
            return;
        }
        let expected = self.expected_reporters();
        let verdict = Verdict { applied: m.applied, detail: m.detail };
        match self.verdicts.record((m.from, m.seq), from, verdict, &expected) {
            VerdictOutcome::Waiting | VerdictOutcome::Agreed { applied: true } => {}
            VerdictOutcome::Agreed { applied: false } => {
                self.log(format!(
                    "action {key} was refused alike on every game (the same world refuses alike): no repair needed"
                ));
            }
            VerdictOutcome::Differ { applied, refused } => {
                let names = |ids: Vec<u16>| ids.into_iter().map(|g| self.name(g)).collect::<Vec<_>>().join(", ");
                let first = refused.first().map(|(_, d)| clip(d, 160)).unwrap_or_default();
                let applied = names(applied);
                let refused = names(refused.into_iter().map(|(g, _)| g).collect());
                self.log(format!(
                    "action {key}: applied on {applied}, refused on {refused} ({first}): the worlds forked at this action"
                ));
                let now = self.now();
                if let Some(reason) = self.policy.on_verdicts_differ(now, &key) {
                    self.request_resync(reason);
                }
            }
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
        if !matches!(r.reason.as_str(), "session start" | "new player" | "player returned") {
            self.announce("The sync is done: everyone has the host's world again.");
        }
        self.out.push(HostOutput::ResyncProgress {
            id: r.id,
            state: ResyncState::Idle,
            detail: format!("resumed at x{}", r.resume_speed),
        });
        self.start_pending();
    }

    /// Ends the round without resuming: the session stays paused.
    fn fail(&mut self, reason: String) {
        let Some(mut r) = self.resync.take() else { return };
        // no load follows: what was held for the games goes out now, they keep the world they have
        let held = std::mem::take(&mut r.held);
        self.release_held(held);
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
