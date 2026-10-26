//! The host's savegame transfer, without stopping the host's loop.
//!
//! Until 2026-10-15 the Transferring step ran inside the loop's dispatch: it hashed the file, sent every chunk (each
//! send waiting for room in the transport) and then waited for the parallel or datagram transfers to end. For a 30 MB
//! save that is tens of seconds in which the host read no network event (a closed connection included), no line of its
//! own game (the hello of its early reload), and sent no ping, session line or relayed message. Now the loop only
//! starts and steps the transfer ([`HostIo::pump`], once per turn, a few chunks at a time):
//!
//! - the file is hashed on a thread of its own, and the parallel and datagram sends run on threads of their own; each
//!   reports back over a channel, and a failed one is replaced by the chunks for that player only, as before;
//! - every frame for a connection that receives the save goes through that connection's [`Outbox`], a writer thread
//!   that sends in order. The host seals each frame when it is made and a receiver refuses a counter more than 64
//!   behind the newest it has seen, so frames to one connection must leave in the order they were sealed; the outbox
//!   keeps that order while the loop never waits for the transport. The loop seals the next chunk only when every
//!   connection that gets chunks has less than [`OUTBOX_ROOM`] queued, so a slow receiver slows the chunks, not the
//!   loop, and a big save is never in memory;
//! - a receiver that disconnects is dropped by the core (`forget_participant`); its outbox and its parallel send are
//!   abandoned and the others go on. A round that moves on (a timeout, a failure) ends the transfer.
//!
//! What the games are sent meanwhile is the core's business: messages for a participant's game are held until its load
//! (`HostSession::holds_for_load`), the order the blocking transfer produced by accident.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, Scope};
use std::time::Instant;

use feverlink3_core::protocol::Frame;
use feverlink3_core::session::{
    sha256_hex, ConnectionId, HostSession, MessageKind, ResyncChunkMessage, ResyncLimits, ResyncState, CAP_BULK_DGRAM,
    CAP_BULK_PAR,
};

use super::{host_dispatch, par_note, sha_bytes, Shared};
use crate::gamelink;
use crate::net::{BulkSendJob, BulkSendResult, HostTransport, ParJob};
use crate::resync;
use crate::steamnet::bulk_io::BulkMode;

/// Bytes a receiving connection may have queued before the next chunk is sealed: about three chunk frames.
pub(super) const OUTBOX_ROOM: usize = 1024 * 1024;
/// Chunks sealed per turn of the loop at most, so one turn stays short whatever the transport takes.
const CHUNKS_PER_TURN: u32 = 8;

enum Out {
    Frame(Frame),
    Close,
}

/// The ordered writer of one connection while it receives a save.
struct Outbox {
    tx: Sender<Out>,
    /// Items queued and not yet handed to the transport.
    items: Arc<AtomicUsize>,
    /// Their bytes.
    bytes: Arc<AtomicUsize>,
    /// The host closed it: nothing more is queued.
    closed: bool,
}

/// One parallel or datagram send to one player.
struct Run {
    player: u16,
    how: &'static str,
    result: Receiver<BulkSendResult>,
    cancel: Arc<AtomicBool>,
}

enum Stage {
    /// The file is being found and hashed: (path, SHA-256 hex, length), or why not.
    Hashing(Receiver<Result<(PathBuf, String, u64), String>>),
    Sending(Sending),
}

struct Sending {
    path: PathBuf,
    len: u64,
    count: u32,
    /// The next chunk for every player that gets chunks.
    next: u32,
    runs: Vec<Run>,
    /// Players whose parallel or datagram send failed, and the next chunk each of them gets on its own.
    fallbacks: BTreeMap<u16, u32>,
    /// Players whose save does not come in the shared chunks (a run, or its fallback).
    bulk: BTreeSet<u16>,
    /// The receivers when the file was announced, to say who left.
    receivers: BTreeSet<u16>,
}

struct SaveTransfer {
    id: i32,
    started: Instant,
    stage: Stage,
}

/// A bug report's bundle on its way to the player who asked for it: the same chunks and the same paced outbox as a save,
/// from memory (a bundle is at most 24 MB).
struct ReportSend {
    id: i32,
    to: u16,
    bytes: Vec<u8>,
    sha256: String,
    count: u32,
    next: u32,
    started: Instant,
}

/// What the host's loop owns besides the state machine: the transport, the outboxes of the connections that receive a
/// save, and the transfer in progress. Every frame of the host goes out through [`HostIo::send`].
pub(super) struct HostIo<'scope, 'env> {
    scope: &'scope Scope<'scope, 'env>,
    pub(super) net: &'env (dyn HostTransport + Sync),
    pub(super) shared: &'env Shared,
    save_dirs: &'env [PathBuf],
    outboxes: BTreeMap<ConnectionId, Outbox>,
    transfer: Option<SaveTransfer>,
    /// The round the transfer in progress (or being stepped) belongs to.
    round: Option<i32>,
    /// A bug report's bundle being sent.
    report: Option<ReportSend>,
}

impl<'scope, 'env> HostIo<'scope, 'env> {
    pub(super) fn new(
        scope: &'scope Scope<'scope, 'env>,
        net: &'env (dyn HostTransport + Sync),
        shared: &'env Shared,
        save_dirs: &'env [PathBuf],
    ) -> Self {
        HostIo { scope, net, shared, save_dirs, outboxes: BTreeMap::new(), transfer: None, round: None, report: None }
    }

    /// A transfer is running: the loop should come back soon to seal the next chunks.
    pub(super) fn busy(&self) -> bool {
        self.round.is_some() || self.report.is_some()
    }

    /// Starts sending a bug report's bundle to player `to` (the one who asked). One at a time: a newer one replaces it.
    pub(super) fn send_report(&mut self, id: i32, to: u16, bytes: Vec<u8>) {
        let Some(count) = ResyncLimits::chunk_count(bytes.len() as u64) else {
            self.shared.log(format!("report {id}: the bundle ({} bytes) cannot be sent", bytes.len()));
            return;
        };
        self.shared
            .log(format!("report {id}: sending the bundle ({} bytes, {count} chunk(s)) to player {to}", bytes.len()));
        let sha256 = sha256_hex(&bytes);
        self.report = Some(ReportSend { id, to, bytes, sha256, count, next: 0, started: Instant::now() });
    }

    /// Seals the next chunks of the bundle as far as the receiver's outbox has room.
    fn pump_report(&mut self, host: &mut HostSession, link: &mut gamelink::Session) {
        let Some(mut r) = self.report.take() else { return };
        let Some(conn) = host.player_connection(r.to) else {
            self.shared.log(format!("report {}: player {} is gone; the bundle was not sent", r.id, r.to));
            return;
        };
        self.open_outbox(conn);
        let mut budget = CHUNKS_PER_TURN;
        while r.next < r.count && budget > 0 && self.has_room(conn) {
            let start = r.next as usize * ResyncLimits::CHUNK_SIZE;
            let size = ResyncLimits::chunk_len(r.bytes.len() as u64, r.next).unwrap_or(0);
            let msg = ResyncChunkMessage {
                id: r.id,
                index: r.next as i32,
                count: r.count as i32,
                sha256: r.sha256.clone(),
                data: r.bytes.get(start..start + size).unwrap_or_default().to_vec(),
            };
            let outputs = host.send_report_chunk(r.to, &msg);
            host_dispatch(outputs, link, self);
            r.next += 1;
            budget -= 1;
        }
        if r.next < r.count {
            self.report = Some(r);
        } else {
            let secs = r.started.elapsed().as_secs_f64();
            self.shared.log(format!("report {}: the bundle went out to player {} in {secs:.1} s", r.id, r.to));
        }
    }

    /// Writes a frame: through the connection's outbox when it has one (in order behind what is queued), directly
    /// otherwise. A failed write closes the connection; its Closed event removes the player.
    pub(super) fn send(&mut self, conn: ConnectionId, frame: Frame) {
        // a message in parts (a big build) takes seconds at Steam's ~1 MB/s per connection: its own writer sends it, in order, and
        // the session thread goes on (the outbox goes once it is empty, `pump`)
        if frame.header.kind == u8::from(MessageKind::Part) {
            self.open_outbox(conn);
        }
        match self.outboxes.get(&conn) {
            Some(outbox) if outbox.closed => {}
            Some(outbox) => {
                outbox.items.fetch_add(1, Ordering::AcqRel);
                outbox.bytes.fetch_add(frame.body.len(), Ordering::AcqRel);
                if outbox.tx.send(Out::Frame(frame)).is_err() {
                    self.outboxes.remove(&conn);
                }
            }
            None => {
                if let Err(e) = self.net.send(conn, &frame) {
                    self.shared.log(format!("connection {conn}: write failed: {e}"));
                }
            }
        }
    }

    /// Closes a connection after what is queued for it.
    pub(super) fn close(&mut self, conn: ConnectionId) {
        match self.outboxes.get_mut(&conn) {
            Some(outbox) if outbox.closed => {}
            Some(outbox) => {
                outbox.closed = true;
                outbox.items.fetch_add(1, Ordering::AcqRel);
                if outbox.tx.send(Out::Close).is_err() {
                    self.net.close(conn);
                }
            }
            None => self.net.close(conn),
        }
    }

    /// The transport says the connection is gone: what is queued for it is dropped with its writer.
    pub(super) fn connection_closed(&mut self, conn: ConnectionId) {
        self.outboxes.remove(&conn);
    }

    /// The save of round `id` came from a player's game (`HostOutput::SourceSave`): written where the host's game would have saved
    /// it, so that the transfer that follows finds it like the host's own save ([`HostIo::begin_transfer`]).
    pub(super) fn write_source_save(&mut self, id: i32, sha256: &str, bytes: &[u8]) {
        let name = resync::save_name(id);
        match resync::write_save(self.save_dirs, &name, bytes) {
            Ok(n) => self.shared.log(format!(
                "resync {id}: the source player's save ({} bytes, SHA-256 {}) written to {n} folder(s) as {name}.sav",
                bytes.len(),
                &sha256[..12.min(sha256.len())]
            )),
            Err(e) => self
                .shared
                .log(format!("resync {id}: cannot write the source player's save: {e}; the round will time out")),
        }
    }

    /// The round entered Transferring: find and hash the save on a thread, then send it ([`HostIo::pump`]).
    pub(super) fn begin_transfer(&mut self, id: i32) {
        if self.round == Some(id) {
            return; // the core reports Transferring again when the file is announced
        }
        self.abandon("a new round started");
        let (tx, rx) = mpsc::channel();
        let dirs = self.save_dirs.to_vec();
        let spawned = thread::Builder::new().name("feverlink3-save-hash".into()).spawn_scoped(self.scope, move || {
            let name = resync::save_name(id);
            let outcome = match resync::find_save(&dirs, &name) {
                None => Err(format!("{name}.sav not found in the savegame folders; the round will time out")),
                Some(path) => match resync::hash_file(&path) {
                    Ok((sha, len)) => Ok((path, sha, len)),
                    Err(e) => Err(format!("cannot read {}: {e}", path.display())),
                },
            };
            let _ = tx.send(outcome);
        });
        if let Err(e) = spawned {
            self.shared.log(format!("resync {id}: cannot start the thread that reads the save: {e}"));
            return;
        }
        self.round = Some(id);
        self.transfer = Some(SaveTransfer { id, started: Instant::now(), stage: Stage::Hashing(rx) });
    }

    /// Steps the transfer once: takes the results of the threads, seals the chunks there is room for, and ends the
    /// transfer when the round has moved on. Also lets the outboxes of finished transfers go once they are empty.
    pub(super) fn pump(&mut self, host: &mut HostSession, link: &mut gamelink::Session) {
        if let Some(mut t) = self.transfer.take() {
            let live = host.resync_id() == Some(t.id) && host.resync_state() == ResyncState::Transferring;
            let keep = live && self.step(&mut t, host, link);
            // the dispatch inside the step may have moved the round on, or begun another
            let current = self.round == Some(t.id)
                && host.resync_id() == Some(t.id)
                && host.resync_state() == ResyncState::Transferring;
            if keep && current {
                self.transfer = Some(t);
            } else {
                self.end(t, host);
            }
        }
        self.pump_report(host, link);
        let mut receiving: BTreeSet<ConnectionId> = match &self.transfer {
            Some(SaveTransfer { stage: Stage::Sending(_), .. }) => {
                host.resync_receivers().iter().filter_map(|&p| host.player_connection(p)).collect()
            }
            _ => BTreeSet::new(),
        };
        receiving.extend(self.report.as_ref().and_then(|r| host.player_connection(r.to)));
        self.outboxes.retain(|conn, o| receiving.contains(conn) || o.items.load(Ordering::Acquire) > 0);
    }

    /// The loop ends: the threads are told to stop, and the writers send what is queued (no more chunks) and end.
    pub(super) fn finish(&mut self) {
        self.abandon("the session stopped");
        self.outboxes.clear();
    }

    fn abandon(&mut self, why: &str) {
        if let Some(t) = self.transfer.take() {
            self.shared.log(format!("resync {}: transfer abandoned: {why}", t.id));
            if let Stage::Sending(s) = &t.stage {
                s.runs.iter().for_each(|r| r.cancel.store(true, Ordering::Release));
            }
            self.clear_status();
        }
        self.round = None;
    }

    fn end(&mut self, t: SaveTransfer, host: &HostSession) {
        if self.round == Some(t.id) {
            self.round = None;
        }
        if let Stage::Sending(s) = &t.stage {
            s.runs.iter().for_each(|r| r.cancel.store(true, Ordering::Release));
            let state = if host.resync_id() == Some(t.id) { host.resync_state() } else { ResyncState::Idle };
            if s.next >= s.count && state != ResyncState::Idle {
                let secs = t.started.elapsed().as_secs_f64();
                self.shared.log(format!("resync {}: save sent ({} bytes) in {secs:.1} s", t.id, s.len));
            } else {
                self.shared.log(format!(
                    "resync {}: transfer ended at chunk {} of {}: the round is {state:?}",
                    t.id, s.next, s.count
                ));
            }
        }
        self.clear_status();
    }

    fn clear_status(&self) {
        self.shared.update(|s| {
            s.transfer = None;
            s.transfer_note = None;
        });
    }

    /// One step of a live transfer. False: it is over (the file could not be read).
    fn step(&mut self, t: &mut SaveTransfer, host: &mut HostSession, link: &mut gamelink::Session) -> bool {
        let id = t.id;
        if let Stage::Hashing(rx) = &t.stage {
            let (path, sha, len) = match rx.try_recv() {
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => {
                    self.shared.log(format!("resync {id}: the thread that reads the save failed"));
                    return false;
                }
                Ok(Err(why)) => {
                    self.shared.log(format!("resync {id}: {why}"));
                    return false;
                }
                Ok(Ok(found)) => found,
            };
            self.shared.log(format!("resync {id}: {len} bytes, SHA-256 {}", &sha[..12.min(sha.len())]));
            let outputs = host.resync_file_ready(id, &sha, len);
            host_dispatch(outputs, link, self);
            let Some(count) = ResyncLimits::chunk_count(len) else { return false };
            if host.resync_state() != ResyncState::Transferring {
                return true; // nobody else to send to: the round went straight on to loading
            }
            let receivers: BTreeSet<u16> = host.resync_receivers().into_iter().collect();
            let mut sending = Sending {
                path,
                len,
                count,
                next: 0,
                runs: Vec::new(),
                fallbacks: BTreeMap::new(),
                bulk: BTreeSet::new(),
                receivers: receivers.clone(),
            };
            self.start_runs(id, &sha, &mut sending, host);
            for &player in &receivers {
                if let Some(conn) = host.player_connection(player) {
                    self.open_outbox(conn);
                }
            }
            t.stage = Stage::Sending(sending);
        }
        let Stage::Sending(s) = &mut t.stage else { return true };
        self.collect_runs(id, s, host);
        let mut budget = CHUNKS_PER_TURN;
        if !self.send_fallbacks(id, s, host, link, &mut budget) {
            return false;
        }
        while s.next < s.count && budget > 0 && host.resync_state() == ResyncState::Transferring {
            // the last chunk starts the load: it waits until every player has the save another way
            if s.next + 1 == s.count && (!s.runs.is_empty() || !s.fallbacks.is_empty()) {
                break;
            }
            let room = host
                .resync_receivers()
                .into_iter()
                .filter(|p| !s.bulk.contains(p))
                .filter_map(|p| host.player_connection(p))
                .all(|conn| self.has_room(conn));
            if !room {
                break;
            }
            match resync::read_chunk(&s.path, s.len, s.next) {
                Ok(data) => {
                    let outputs = host.resync_chunk(id, s.next, &data);
                    s.next += 1;
                    budget -= 1;
                    host_dispatch(outputs, link, self);
                }
                Err(e) => {
                    self.shared.log(format!("resync {id}: reading chunk {}: {e}", s.next));
                    return false;
                }
            }
            let (next, count) = (s.next, s.count);
            self.shared.update(|st| st.transfer = Some((next, count)));
        }
        true
    }

    /// Starts the parallel or datagram sends for the players that can take them (`FEVERLINK3_BULK`).
    fn start_runs(&mut self, id: i32, sha: &str, s: &mut Sending, host: &mut HostSession) {
        let settings = self.shared.bulk.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let secret = *self.shared.par_secret.lock().unwrap_or_else(|p| p.into_inner());
        let Some(digest) = sha_bytes(sha) else { return };
        let (shared, scope, count, len) = (self.shared, self.scope, s.count, s.len);
        let mut players = Vec::new();
        if settings.mode == BulkMode::Par && settings.par.applies_to(len) {
            for player in host.resync_receivers() {
                if !host.player_supports(player, CAP_BULK_PAR) {
                    continue;
                }
                let Some(sender) = host.player_connection(player).and_then(|c| self.net.par_sender(c)) else {
                    shared.log(format!(
                        "resync {id}: player #{player} announced parallel connections but the transport has none for it; sending chunks"
                    ));
                    continue;
                };
                let job =
                    ParJob { ctx: id as u32, path: s.path.clone(), sha256: digest, secret, cfg: settings.par.clone() };
                let cancel = Arc::new(AtomicBool::new(false));
                let (tx, result) = mpsc::channel();
                let stop = Arc::clone(&cancel);
                let spawned =
                    thread::Builder::new().name("feverlink3-save-par".into()).spawn_scoped(scope, move || {
                        let cancelled = || stop.load(Ordering::Acquire) || shared.stopping();
                        let progress = |p: &crate::net::ParProgress| {
                            let sent = (p.bytes / ResyncLimits::CHUNK_SIZE as u64) as u32;
                            shared.update(|st| {
                                st.transfer = Some((sent.min(count), count));
                                st.transfer_note = Some(par_note(p.lanes, p.rate));
                            });
                        };
                        let _ = tx.send(sender(job, &cancelled, &progress));
                    });
                if spawned.is_ok() {
                    players.push(player);
                    s.runs.push(Run { player, how: "parallel connections", result, cancel });
                }
            }
            if !players.is_empty() {
                shared.log(format!(
                    "resync {id}: sending the save ({len} bytes) to {} player(s) over parallel Steam connections (the others get chunks)",
                    players.len()
                ));
            }
        }
        if settings.mode == BulkMode::Dgram {
            for player in host.resync_receivers() {
                if !host.player_supports(player, CAP_BULK_DGRAM) {
                    continue;
                }
                let Some(sender) = host.player_connection(player).and_then(|c| self.net.bulk_sender(c)) else {
                    continue;
                };
                let job = BulkSendJob { ctx: id as u32, path: s.path.clone(), sha256: digest };
                let cancel = Arc::new(AtomicBool::new(false));
                let (tx, result) = mpsc::channel();
                let stop = Arc::clone(&cancel);
                let spawned =
                    thread::Builder::new().name("feverlink3-save-dgram".into()).spawn_scoped(scope, move || {
                        let cancelled = || stop.load(Ordering::Acquire) || shared.stopping();
                        let progress = |bytes: u64| {
                            let sent = (bytes / ResyncLimits::CHUNK_SIZE as u64) as u32;
                            shared.update(|st| st.transfer = Some((sent.min(count), count)));
                        };
                        let _ = tx.send(sender(job, &cancelled, &progress));
                    });
                if spawned.is_ok() {
                    players.push(player);
                    s.runs.push(Run { player, how: "bulk datagrams", result, cancel });
                }
            }
            if !players.is_empty() {
                shared.log(format!(
                    "resync {id}: sending the save to {} player(s) as bulk datagrams (the others get chunks)",
                    players.len()
                ));
            }
        }
        if !players.is_empty() {
            host.resync_set_bulk(id, &players);
            s.bulk.extend(players);
        }
    }

    /// Takes the results of the finished sends; a failed one becomes chunks for that player. A player that left is
    /// let go: its send is cancelled and nobody waits for it.
    fn collect_runs(&mut self, id: i32, s: &mut Sending, host: &HostSession) {
        let receivers: BTreeSet<u16> = host.resync_receivers().into_iter().collect();
        for gone in s.receivers.difference(&receivers) {
            self.shared.log(format!("resync {id}: player #{gone} left during the transfer; the others go on"));
        }
        s.receivers = receivers;
        let mut still = Vec::new();
        for run in std::mem::take(&mut s.runs) {
            let who = host.players().get(&run.player).map_or_else(|| format!("#{}", run.player), |p| p.name.clone());
            if !s.receivers.contains(&run.player) {
                run.cancel.store(true, Ordering::Release);
                continue;
            }
            let outcome = match run.result.try_recv() {
                Err(TryRecvError::Empty) => {
                    still.push(run);
                    continue;
                }
                Err(TryRecvError::Disconnected) => BulkSendResult::Failed("the sending thread failed".into()),
                Ok(outcome) => outcome,
            };
            match outcome {
                BulkSendResult::Done { summary } => {
                    self.shared.log(format!("resync {id}: {who} has the save ({}): {summary}", run.how));
                }
                BulkSendResult::Failed(why) => {
                    self.shared
                        .log(format!("resync {id}: bulk transfer to {who} failed ({why}); sending the chunks instead"));
                    s.fallbacks.insert(run.player, 0);
                }
            }
        }
        s.runs = still;
        s.fallbacks.retain(|p, _| s.receivers.contains(p));
    }

    /// Every chunk, on its own, to each player whose other way failed, as far as its outbox has room. False: the file
    /// could not be read.
    fn send_fallbacks(
        &mut self,
        id: i32,
        s: &mut Sending,
        host: &mut HostSession,
        link: &mut gamelink::Session,
        budget: &mut u32,
    ) -> bool {
        let players: Vec<u16> = s.fallbacks.keys().copied().collect();
        for player in players {
            while *budget > 0 && host.resync_state() == ResyncState::Transferring {
                let Some(&index) = s.fallbacks.get(&player) else { break };
                if index >= s.count {
                    s.fallbacks.remove(&player);
                    break;
                }
                let Some(conn) = host.player_connection(player) else {
                    s.fallbacks.remove(&player);
                    break;
                };
                if !self.has_room(conn) {
                    break;
                }
                match resync::read_chunk(&s.path, s.len, index) {
                    Ok(data) => {
                        let outputs = host.resync_chunk_to(id, player, index, &data);
                        s.fallbacks.insert(player, index + 1);
                        *budget -= 1;
                        host_dispatch(outputs, link, self);
                    }
                    Err(e) => {
                        self.shared.log(format!("resync {id}: reading chunk {index}: {e}"));
                        return false;
                    }
                }
            }
        }
        true
    }

    fn has_room(&self, conn: ConnectionId) -> bool {
        self.outboxes.get(&conn).is_none_or(|o| o.bytes.load(Ordering::Acquire) < OUTBOX_ROOM)
    }

    /// From now on every frame for `conn` goes through a writer thread of its own, in order.
    fn open_outbox(&mut self, conn: ConnectionId) {
        if self.outboxes.contains_key(&conn) {
            return;
        }
        let (tx, rx) = mpsc::channel::<Out>();
        let items = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let (net, shared) = (self.net, self.shared);
        let (left, size) = (Arc::clone(&items), Arc::clone(&bytes));
        let spawned =
            thread::Builder::new().name(format!("feverlink3-out-{conn}")).spawn_scoped(self.scope, move || {
                write_in_order(net, shared, conn, &rx, &left, &size);
            });
        match spawned {
            Ok(_) => {
                self.outboxes.insert(conn, Outbox { tx, items, bytes, closed: false });
            }
            Err(e) => self.shared.log(format!("connection {conn}: no writer thread ({e}); frames go out directly")),
        }
    }
}

/// The writer of one outbox: sends what comes in order until the outbox is dropped. After a failed write the rest is
/// dropped (the transport closes the connection and says so); once the session stops, chunks are no longer sent, but
/// what follows them (the host's goodbye) is.
fn write_in_order(
    net: &(dyn HostTransport + Sync),
    shared: &Shared,
    conn: ConnectionId,
    rx: &Receiver<Out>,
    items: &AtomicUsize,
    bytes: &AtomicUsize,
) {
    let mut failed = false;
    for out in rx {
        match out {
            Out::Frame(frame) => {
                let chunk = frame.header.kind == u8::from(MessageKind::ResyncChunk);
                if !failed && !(chunk && shared.stopping()) {
                    if let Err(e) = net.send(conn, &frame) {
                        failed = true;
                        shared.log(format!("connection {conn}: write failed: {e}"));
                    }
                }
                bytes.fetch_sub(frame.body.len(), Ordering::AcqRel);
            }
            Out::Close => net.close(conn),
        }
        items.fetch_sub(1, Ordering::AcqRel);
    }
}
