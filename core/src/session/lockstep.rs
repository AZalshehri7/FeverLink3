use std::collections::{BTreeMap, VecDeque};

/// Game-time arithmetic shared by host and players.
#[derive(Debug)]
pub struct Lockstep;

impl Lockstep {
    /// Game-time units per simulation step.
    pub const STEP: i64 = 200;
    /// Game-time units per real second at speed 1.
    pub const UNITS_PER_SECOND: f64 = 1000.0;

    pub fn round_up_to_step(t: f64) -> i64 {
        // `as` saturates for huge or NaN input; the product must not wrap on top of that
        ((t / Self::STEP as f64).ceil() as i64).saturating_mul(Self::STEP)
    }

    /// Steps of game time that pass in one real second per speed unit. Measured in the real game (about 1200 units a second at
    /// normal speed), a little above the 1000 the engine is documented to run at, so a lead counted with it errs to the long side.
    pub const STEPS_PER_SECOND: f64 = 6.0;
    /// What an action costs on its way besides the network, in real seconds: the file hand-offs between the game and the
    /// session thread on both ends (polled every 20 ms) and a game frame.
    pub const HANDLING_SECONDS: f64 = 0.06;
    /// The shortest lead, in steps: one for the script event that carries the action into the simulation state (handled at
    /// its next step) and one of margin for the error in a peer's reported clock.
    pub const MIN_LEAD_STEPS: i64 = 3;
    pub const MAX_LEAD_STEPS: i64 = 24;

    /// How far past the fastest clock a stamp must lie so that every game has the action before it reaches the stamp, in
    /// game-time units: the one-way network time (half the slowest round trip) and the handling, counted in steps at this
    /// speed, plus the hand-off step and one step of margin. On a good connection that is 3 steps (about half a second at
    /// normal speed); a slow link or a higher speed lengthens it. It was a fixed 6 steps (1.2 s) at the first real test.
    pub fn lead_for(speed: i32, rtt_ms: f64) -> i64 {
        Self::lead_for_measured(speed, rtt_ms, 0.0)
    }

    /// The most real time the clock-gap term of the lead may add, in milliseconds. A lead covers jitter, not stalls: every step of
    /// lead is a step of delay on every build (a build's stamp is never before its game's promise, which is its clock plus the lead
    /// plus the receivers' hand-off allowance), so a game whose frames stall for seconds is not hidden behind a lead of seconds.
    pub const GAP_EXCESS_MAX_MS: f64 = 500.0;

    /// [`Self::lead_for`] with the measured cadence of the clocks: `gap_ms` is a high quantile of the time between two clock
    /// lines of a running game as the host receives them ([`LeadMeter::gap_ms`]). A running game's clock moves one step at a time,
    /// so a gap of one step's real time is what the two fixed steps already cover (a peer's barrier sits at its clock plus the
    /// lead and moves a step with each clock line); only the excess over it, up to [`Self::GAP_EXCESS_MAX_MS`], is added to the
    /// one-way time. In steps: `ceil((rtt/2 + handling + excess) * steps_per_second * speed) + 2`, bounded to 3..=24.
    pub fn lead_for_measured(speed: i32, rtt_ms: f64, gap_ms: f64) -> i64 {
        let rtt = if rtt_ms.is_finite() { rtt_ms.max(0.0) } else { 0.0 };
        let per_second = Self::STEPS_PER_SECOND * f64::from(speed.max(1));
        let gap = if gap_ms.is_finite() { gap_ms.max(0.0) } else { 0.0 };
        let excess = (gap - 1000.0 / per_second).clamp(0.0, Self::GAP_EXCESS_MAX_MS);
        let seconds = rtt / 2000.0 + Self::HANDLING_SECONDS + excess / 1000.0;
        let steps = (seconds * per_second).ceil() as i64 + 2;
        Self::STEP * steps.clamp(Self::MIN_LEAD_STEPS, Self::MAX_LEAD_STEPS)
    }

    /// The lead on a connection with no measured delay (the floor for a speed).
    pub fn lead_units(speed: i32) -> i64 {
        Self::lead_for(speed, 0.0)
    }
}

/// What the host's lead is made from ([`Lockstep::lead_for_measured`]): per game, the last few round trips and the recent gaps
/// between its clock lines while it runs. The host's lead is the one value every game stamps and promises with (the session's
/// `lead`), so every game follows the same rule; the lead only moves how often a game waits at the input barrier and how far
/// ahead a build is stamped, never whether an item arrives in time (the promise does that).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LeadMeter {
    rtt: BTreeMap<u16, VecDeque<f64>>,
    clocks: BTreeMap<u16, ClockCadence>,
}

/// The clock lines of one game: the last one's time and game time, and the gaps (host time, ms) between two lines of a running
/// game. A line whose game time did not move (the game stands at the barrier, the pacing or a pause and sends its keepalive), and
/// the first line after one, say nothing about the cadence of a running game and are left out.
#[derive(Debug, Clone, Default, PartialEq)]
struct ClockCadence {
    last: Option<(f64, i64, bool)>,
    gaps: VecDeque<(f64, f64)>,
}

impl LeadMeter {
    /// Round trips kept per player (a ping every 2 s: the last 16 s or so).
    pub const RTT_KEEP: usize = 8;
    /// Quantile of those round trips the lead covers (of 8: the second largest).
    pub const RTT_QUANTILE: f64 = 0.9;
    /// Clock gaps are counted over this many seconds of host time...
    pub const GAP_WINDOW_SECONDS: f64 = 10.0;
    /// ...at most this many per game...
    pub const GAP_KEEP: usize = 512;
    /// ...and only once a game has this many in the window (a few gaps are no quantile).
    pub const GAP_MIN: usize = 20;
    /// Quantile of the gaps the lead covers: jitter is covered, a rare stall is not (see [`Lockstep::GAP_EXCESS_MAX_MS`]).
    pub const GAP_QUANTILE: f64 = 0.95;

    pub fn rtt(&mut self, player: u16, ms: f64) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        let q = self.rtt.entry(player).or_default();
        q.push_back(ms);
        while q.len() > Self::RTT_KEEP {
            q.pop_front();
        }
    }

    /// A clock line of `player` showing game time `t` reached the host at host time `at` (seconds).
    pub fn clock(&mut self, player: u16, t: i64, at: f64) {
        let c = self.clocks.entry(player).or_default();
        let moved = c.last.is_some_and(|(_, lt, _)| t > lt);
        if let Some((last_at, _, last_moved)) = c.last {
            if moved && last_moved && at >= last_at {
                c.gaps.push_back((at, (at - last_at) * 1000.0));
                while c.gaps.len() > Self::GAP_KEEP {
                    c.gaps.pop_front();
                }
            }
        }
        c.last = Some((at, t, moved));
    }

    /// A player left: its measurements go (a gap across its absence says nothing about the link).
    pub fn forget(&mut self, player: u16) {
        self.rtt.remove(&player);
        self.clocks.remove(&player);
    }

    /// The round trip the lead covers: the largest, over the players `here` accepts, of each one's [`Self::RTT_QUANTILE`] of its
    /// last round trips. 0 while none was measured.
    pub fn rtt_ms(&self, here: impl Fn(u16) -> bool) -> f64 {
        self.rtt
            .iter()
            .filter(|(id, _)| here(**id))
            .filter_map(|(_, q)| quantile(q.iter().copied(), Self::RTT_QUANTILE))
            .fold(0.0, f64::max)
    }

    /// The clock gap the lead covers at host time `now`: the largest, over the games `here` accepts, of each one's
    /// [`Self::GAP_QUANTILE`] of its gaps in the last [`Self::GAP_WINDOW_SECONDS`]. 0 while no game has [`Self::GAP_MIN`] of them.
    pub fn gap_ms(&self, now: f64, here: impl Fn(u16) -> bool) -> f64 {
        let from = now - Self::GAP_WINDOW_SECONDS;
        self.clocks
            .iter()
            .filter(|(id, _)| here(**id))
            .filter_map(|(_, c)| {
                let recent: Vec<f64> = c.gaps.iter().filter(|(at, _)| *at >= from).map(|&(_, g)| g).collect();
                if recent.len() < Self::GAP_MIN {
                    return None;
                }
                quantile(recent.into_iter(), Self::GAP_QUANTILE)
            })
            .fold(0.0, f64::max)
    }
}

/// The `q` quantile (0..=1) of the values, the nearest rank; `None` without values.
fn quantile(values: impl Iterator<Item = f64>, q: f64) -> Option<f64> {
    let mut v: Vec<f64> = values.filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let i = ((v.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    Some(v[i.min(v.len() - 1)])
}

/// A player's last reported clock, extrapolated with its speed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerClock {
    pub t: i64,
    pub speed: i32,
    /// Host time (seconds) at which the report arrived.
    pub at: f64,
    /// The input barrier's promise that came with it (ClockMessage::safe, bounded), if any.
    pub safe: Option<i64>,
    /// The game stands still at a pause at this time (ClockMessage::stop), if it said so.
    pub stop: Option<i64>,
}

impl PlayerClock {
    /// Latest game time the host believes a game's clock can show (about 35 000 years of play).
    pub const MAX_TIME: i64 = 1 << 50;
    /// Fastest game speed the host believes a clock can report.
    pub const MAX_SPEED: i32 = 16;

    /// Whether a reported clock can be a real game's: the host builds every stamp and pause from these values,
    /// so a player must not be able to put an absurd one in.
    pub fn plausible(t: i64, speed: i32) -> bool {
        (0..=Self::MAX_TIME).contains(&t) && (0..=Self::MAX_SPEED).contains(&speed)
    }

    pub fn estimate(&self, now: f64) -> f64 {
        self.t as f64 + (now - self.at).max(0.0) * f64::from(self.speed) * Lockstep::UNITS_PER_SECOND
    }
}
