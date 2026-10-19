//! The stall timeout: one game that stops answering no longer holds every other game at its input barrier.
//!
//! A running game sends a clock line with its promise (`ClockMessage::safe`) every frame its clock moves and every 0.25 s anyway. Every
//! other game may simulate up to that promise and no further (the mod's `L.barrier`). A game whose frames stop (an autosave, a frozen or
//! minimised window, a clone freeze: 7-10 s in the owner's notes of 2026-10-17) sends nothing, its promise stays where it was, and every
//! other game stood at it until the frozen game came back.
//!
//! After `HostOptions::stall_timeout_seconds` without a clock line from a running game while another game's lines still come, the host
//! declares it STALLED (`SessionState::stalled`) and keeps a promise for it: the proxy, which follows the furthest promise of the games
//! that run (so they have the room they would have with a peer at their own pace) and goes to every other game in a `PeerClockMessage`
//! flagged `stalled`. The host can promise for the stalled game because every action of that game passes through the host first:
//!   * an action it marks for the host's stamp (`ActionMessage::pause`: the mod marks every action of a stalled game so, X.hostStamps)
//!     gets the pause order's running stamp, `future_stamp`, which is past every promise and every proxy (flush_pause), and goes to every
//!     game, its sender included, which applies it there;
//!   * an action or commit stamped by its game at or after the proxy is relayed as it is;
//!   * one stamped before the proxy (shipped before its game knew it was stalled, or held up on the link) cannot be applied in time
//!     everywhere and is never applied late: the host refuses it on every game, its sender's included, with a `BuildVetoMessage` flagged
//!     `host` (the veto path: queued copies are dropped, a held build is cancelled, a waiting window command fails).
//!
//! Nothing in any world changes: every game still simulates every step and applies every action at the stamp every game has (the
//! full-game rule; no step is skipped). The stalled game simply runs its steps later, faster while it is behind (the mod's catch-up:
//! engine batches up to x4 and a batch interval down to half), and its window says it is catching up.
//!
//! The stall ends when the game's own promise has reached the proxy: from that line on its own promises are relayed again and are no
//! smaller than what the others were promised. So that it can, the proxy stops following the others once the game has caught up to
//! within the session's lead of the furthest clock (it would otherwise move on as fast as the game's own promise), and moves on again
//! if the game falls silent once more. The others then have the room of one lead for the moment it takes.
//!
//! The policy (time out a frozen member instead of letting it pace the room, let it catch up later) is TPF3-MP's (Juliansgith, MIT; its
//! server stops pacing on a member that has not advanced for 20 s); the promise kept by the relay and the refusal on every game are ours.

use super::*;

/// What the host keeps of a stalled game.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Stall {
    /// Host time it was declared.
    pub since: f64,
    /// The promise the host keeps for the game (game time): never lowered.
    pub proxy: i64,
    /// The game's last clock, its `ahead` and margin need, as the proxy line passes them on.
    pub t: i64,
    pub ahead: i32,
    pub need: Option<i32>,
    /// Host time its clock lines came again (`None` while it is silent).
    pub back: Option<f64>,
    /// Its clock has come within the lead of the furthest one: the proxy waits for its own promise.
    pub caught: bool,
}

impl HostSession {
    /// The promise the host can keep for a stalled game now: the furthest promise of the games that run.
    pub(super) fn proxy_target(&self) -> Option<i64> {
        self.clocks.iter().filter(|(id, _)| !self.stalls.contains_key(id)).filter_map(|(_, c)| c.safe).max()
    }

    /// The furthest clock of the games that run.
    fn furthest_running_clock(&self) -> Option<i64> {
        self.clocks.iter().filter(|(id, _)| !self.stalls.contains_key(id)).map(|(_, c)| c.t).max()
    }

    /// The proxy line of stalled game `id` to every other game.
    pub(super) fn send_proxy(&mut self, id: u16) {
        let Some(s) = self.stalls.get(&id) else { return };
        let peer = PeerClockMessage {
            from: id,
            t: s.t,
            ahead: s.ahead,
            need: s.need,
            safe: Some(s.proxy),
            pause: false,
            stalled: true,
            hop: None,
        };
        self.broadcast(MessageKind::PeerClock, &codec::encode(&peer), Some(id));
    }

    /// Every tick: a running game whose clock lines stopped while another game's still come is declared stalled.
    pub(super) fn watch_stalls(&mut self, t: f64) {
        let timeout = self.options.stall_timeout_seconds;
        if timeout.is_nan()
            || timeout <= 0.0
            || !self.state.started
            || self.state.pause_at.is_some()
            || self.resync.is_some()
        {
            return;
        }
        let ready: Vec<u16> = self.players.values().filter(|p| p.ready).map(|p| p.id).collect();
        let silent = |c: &PlayerClock| t - c.at >= timeout;
        // nothing to run on for when no game is heard from (the host itself was away: its own STALL_SECONDS)
        let anyone_heard =
            ready.iter().any(|id| !self.stalls.contains_key(id) && self.clocks.get(id).is_some_and(|c| !silent(c)));
        if !anyone_heard {
            return;
        }
        let newly: Vec<u16> = ready
            .iter()
            .copied()
            .filter(|id| !self.stalls.contains_key(id) && self.clocks.get(id).is_some_and(silent))
            .collect();
        for id in newly {
            let Some(c) = self.clocks.get(&id).copied() else { continue };
            let proxy = c.safe.unwrap_or(c.t).max(self.proxy_target().unwrap_or(i64::MIN));
            self.stalls.insert(id, Stall { since: t, proxy, t: c.t, ahead: 0, need: None, back: None, caught: false });
            // the silence is no clock gap of a running game: the lead must not grow by it when the clock moves again
            self.lead_meter.restart_cadence(id);
            self.state.stalled.push(id);
            self.state.stalled.sort_unstable();
            let who = self.name(id);
            self.log(format!(
                "stall: {who} sent no clock for {:.1} s (its clock t={}, its promise {}): the others run on, the host keeps the promise {proxy} for it until it has caught up",
                t - c.at,
                c.t,
                c.safe.map_or("none".to_owned(), |v| v.to_string())
            ));
            self.broadcast_session();
            self.send_proxy(id);
        }
        // a game that had caught up and falls silent again: its proxy follows the others again
        let ids: Vec<u16> = self.stalls.keys().copied().collect();
        for id in ids {
            let quiet = self.clocks.get(&id).is_some_and(silent);
            if let Some(s) = self.stalls.get_mut(&id) {
                if quiet && (s.back.is_some() || s.caught) {
                    s.back = None;
                    s.caught = false;
                }
            }
        }
        self.follow_stalls();
    }

    /// After a running game's clock line: each stalled game's proxy follows the furthest promise (unless that game has caught up).
    pub(super) fn follow_stalls(&mut self) {
        if self.stalls.is_empty() {
            return;
        }
        let Some(target) = self.proxy_target() else { return };
        let ids: Vec<u16> = self.stalls.keys().copied().collect();
        for id in ids {
            let moved = match self.stalls.get_mut(&id) {
                Some(s) if !s.caught && target > s.proxy => {
                    s.proxy = target;
                    true
                }
                _ => false,
            };
            if moved {
                self.send_proxy(id);
            }
        }
    }

    /// A clock line of game `from`. False when it is not stalled (the line is relayed as usual). When it is, the line is not relayed:
    /// the proxy line carries its clock; the stall ends here once its own promise has reached the proxy (true is still returned: the
    /// caller relays nothing, this function relays the game's own line itself after the session says it is back).
    pub(super) fn stalled_clock(
        &mut self,
        from: u16,
        t: i64,
        ahead: i32,
        need: Option<i32>,
        safe: Option<i64>,
    ) -> bool {
        let now = self.now();
        let lead = self.state.lead;
        let furthest = self.furthest_running_clock();
        let Some(s) = self.stalls.get_mut(&from) else { return false };
        let first = s.back.is_none() && t != s.t;
        if first {
            s.back = Some(now);
        }
        let behind = furthest.map_or(0, |f| f - t);
        if !s.caught && s.back.is_some() && behind <= lead {
            s.caught = true;
        }
        let (proxy, since, caught) = (s.proxy, s.since, s.caught);
        let moved = s.t != t;
        s.t = t;
        s.ahead = ahead;
        s.need = need;
        let who = self.name(from);
        if first {
            self.log(format!(
                "stall: {who} runs again at t={t}, {} step(s) behind the furthest game, after {:.1} s: catching up (actions it ships are stamped by the host)",
                behind / Lockstep::STEP,
                now - since
            ));
        }
        if safe.is_some_and(|p| p >= proxy) {
            self.stalls.remove(&from);
            self.state.stalled.retain(|&p| p != from);
            self.log(format!(
                "stall: {who} caught up at t={t} after {:.1} s: its own promise {} has reached the host's {proxy}; its clocks are relayed again",
                now - since,
                safe.unwrap_or(0)
            ));
            self.broadcast_session();
            let peer = PeerClockMessage { from, t, ahead, need, safe, pause: false, stalled: false, hop: None };
            self.broadcast(MessageKind::PeerClock, &codec::encode(&peer), Some(from));
            return true;
        }
        if moved || caught {
            self.send_proxy(from);
        }
        true
    }

    /// A stall ends without the promise check (the game loaded a world: a resync or a join; its hello comes before its first clock).
    pub(super) fn end_stall(&mut self, id: u16, why: &str) {
        if self.stalls.remove(&id).is_some() {
            self.state.stalled.retain(|&p| p != id);
            let who = self.name(id);
            self.log(format!("stall: {who} is no longer stalled: {why}"));
            self.broadcast_session();
        }
    }

    /// Whether an action (or a commit) of game `from` stamped `stamp` must be refused: it is stalled and the stamp lies before the
    /// promise the host keeps for it, so another game may have simulated that step already. It is then refused on every game, its sender's
    /// included (a `BuildVetoMessage` flagged `host`), and not relayed. Never applied late.
    pub(super) fn refuse_stalled(&mut self, from: u16, seq: i64, stamp: i64, what: &str) -> bool {
        let Some(proxy) = self.stalls.get(&from).map(|s| s.proxy) else { return false };
        if stamp >= proxy {
            return false;
        }
        let who = self.name(from);
        self.log(format!(
            "stall: {what} {from}:{seq} of {who} stamped {stamp}, before the promise {proxy} the host keeps for it while it is stalled: refused on every game"
        ));
        let veto = BuildVetoMessage {
            seq,
            owner: from,
            why: format!(
                "stamped {stamp} while its game was stalled, before the step {proxy} the other games may have reached: refused on every game, never applied late"
            ),
            from: Self::HOST_ID,
            name: Some("FeverLink3".to_owned()),
            host: true,
        };
        self.broadcast(MessageKind::BuildVeto, &codec::encode(&veto), None);
        true
    }
}
