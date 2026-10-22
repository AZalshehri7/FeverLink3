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
    /// speed, plus the hand-off step and one engine batch (`speed` steps: one at normal speed). On a good connection that is 3
    /// steps at normal speed (about half a second), 7 at x4; a slow link or a higher speed lengthens it. It was a fixed 6 steps
    /// (1.2 s) at the first real test.
    pub fn lead_for(speed: i32, rtt_ms: f64) -> i64 {
        Self::lead_for_measured(speed, rtt_ms, 0.0)
    }

    /// The most real time the clock-gap term of the lead may add, in milliseconds. A lead covers jitter, not stalls: every step of
    /// lead is a step of delay on every build (a build's stamp is never before its game's promise, which is its clock plus the lead
    /// plus the receivers' hand-off allowance), so a game whose frames stall for seconds is not hidden behind a lead of seconds.
    pub const GAP_EXCESS_MAX_MS: f64 = 500.0;

    /// The real time of one engine batch at any speed, in seconds: the engine runs `speed` steps at once, a batch about every sixth
    /// of a second (measured in the friend test of 2026-10-18: 190-218 ms between batches at x2 and x4, the clock moving 4 steps a
    /// line at x4).
    pub const BATCH_SECONDS: f64 = 1.0 / Self::STEPS_PER_SECOND;

    /// [`Self::lead_for`] with the measured cadence of the clocks: `gap_ms` is a high quantile of the time between two moves of a
    /// running game's clock as the host receives them ([`LeadMeter::gap_ms`]). The lead is also the room every game has before its
    /// input barrier (a peer's barrier sits at its clock plus the lead), and a running game's clock moves a whole engine batch at a
    /// time: `speed` steps every [`Self::BATCH_SECONDS`]. So the lead holds one batch of steps on top of the hand-off step, and only
    /// the excess of the gap over a batch's real time, up to [`Self::GAP_EXCESS_MAX_MS`], is added to the one-way time. In steps:
    /// `ceil((rtt/2 + handling + excess) * steps_per_second * speed) + 1 + speed`, bounded to 3..=24.
    ///
    /// Friend test of 2026-10-18 (x4, 30 ms round trip): the lead was counted as if the clock moved one step per line (+ 2 steps), 4
    /// steps at x4, which is exactly one batch: every game had at most one batch of room, both games stood at their barriers about
    /// 20 times a minute and the smooth pacing slowed both to 0.6x nearly all the time (pace_mean 1.58, paced_s 60).
    pub fn lead_for_measured(speed: i32, rtt_ms: f64, gap_ms: f64) -> i64 {
        Self::lead_for_cover(speed, Self::cover_ms(rtt_ms, gap_ms))
    }

    /// The real time (ms) a lead must cover for a round trip and a clock gap: the one-way time and the gap's excess over one batch's
    /// real time (at most [`Self::GAP_EXCESS_MAX_MS`]). Not a number or negative counts as 0.
    pub fn cover_ms(rtt_ms: f64, gap_ms: f64) -> f64 {
        Self::cover_ms_at(rtt_ms, gap_ms, Self::BATCH_SECONDS * 1000.0)
    }

    /// [`Self::cover_ms`] with the engine's batch interval as measured (`batch_ms`, [`Self::batch_ms_or_nominal`]) instead of
    /// [`Self::BATCH_SECONDS`]: only the part of a clock gap beyond one real batch is jitter the lead must cover.
    pub fn cover_ms_at(rtt_ms: f64, gap_ms: f64, batch_ms: f64) -> f64 {
        let rtt = if rtt_ms.is_finite() { rtt_ms.max(0.0) } else { 0.0 };
        let gap = if gap_ms.is_finite() { gap_ms.max(0.0) } else { 0.0 };
        let excess = (gap - Self::batch_ms_or_nominal(Some(batch_ms))).clamp(0.0, Self::GAP_EXCESS_MAX_MS);
        rtt / 2.0 + excess
    }

    /// The bounds a measured batch interval is taken within (ms). Below: a burst of clock lines that a delay on the link bunched
    /// together, not an engine that fast. Above: batches stretched by the smooth pacing (up to 2x) or frames slower than the
    /// engine; a lead counted from them would shrink with every stretch and stretch the batches further, so the bound stops that loop.
    pub const BATCH_MS_MIN: f64 = 100.0;
    pub const BATCH_MS_MAX: f64 = 300.0;

    /// The engine's batch interval to count a lead with (ms): the measured one ([`LeadMeter::batch_ms`]) within
    /// [`Self::BATCH_MS_MIN`]..=[`Self::BATCH_MS_MAX`], or [`Self::BATCH_SECONDS`] before anything is measured (or for a value that is
    /// not a number).
    pub fn batch_ms_or_nominal(measured: Option<f64>) -> f64 {
        match measured {
            Some(ms) if ms.is_finite() && ms > 0.0 => ms.clamp(Self::BATCH_MS_MIN, Self::BATCH_MS_MAX),
            _ => Self::BATCH_SECONDS * 1000.0,
        }
    }

    /// Steps of game time a running game simulates in one real second at `speed` with batches `batch_ms` apart: `speed` steps a
    /// batch (L.pace runs full batches: the checksum marks lie on batch boundaries, [`Self::round_up_to_batch`]). With the nominal
    /// batch it is [`Self::STEPS_PER_SECOND`] per speed unit; measured in the real game of 2026-10-22 about 200 ms a batch, 5 a speed unit.
    pub fn steps_per_second(speed: i32, batch_ms: f64) -> f64 {
        // as a ratio to the nominal batch, so that the nominal one gives STEPS_PER_SECOND exactly (no rounding moves a ceil)
        let nominal = Self::BATCH_SECONDS * 1000.0;
        Self::STEPS_PER_SECOND * f64::from(speed.clamp(1, 4)) * (nominal / Self::batch_ms_or_nominal(Some(batch_ms)))
    }

    /// `t` rounded up to a batch boundary at `speed`: a multiple of `speed` steps of game time. Every game runs full batches of
    /// `speed` steps from such a boundary to the next, so a checksum stamp there (and its spread marks, `speed` steps apart) never
    /// shortens a batch; the stamp is the same game time on every game (the host gives it), so the round is read at the same step
    /// everywhere as before. Real game of 2026-10-22 at x4: the marks lay at other phases than the batches, L.pace shortened batches
    /// to land on them (4,1,1,4,3,3,4 steps) and the game ran at about x2.7.
    pub fn round_up_to_batch(t: i64, speed: i32) -> i64 {
        let batch = Self::STEP * i64::from(speed.clamp(1, 4));
        let up = t.saturating_add(batch - 1).div_euclid(batch);
        up.saturating_mul(batch)
    }

    /// The lead that covers `cover_ms` of real time at this speed: `ceil((cover + handling) * steps_per_second * speed) + 1 + speed`
    /// steps, bounded to 3..=24. The hand-off step and one engine batch (the 907af4e fix of 2026-10-18) are its floor.
    pub fn lead_for_cover(speed: i32, cover_ms: f64) -> i64 {
        Self::lead_for_cover_at(speed, cover_ms, Self::BATCH_SECONDS * 1000.0)
    }

    /// [`Self::lead_for_cover`] counted with the measured batch interval (`batch_ms`, [`Self::batch_ms_or_nominal`]): the real time is
    /// turned into steps at [`Self::steps_per_second`] instead of the fixed [`Self::STEPS_PER_SECOND`]. The floor (the hand-off step and
    /// one batch of `speed` steps) is in steps and does not change. A lead only decides how often a game waits at its input barrier,
    /// never whether an item arrives in time (the promise does that), so a measure that is off costs waits, not sync.
    pub fn lead_for_cover_at(speed: i32, cover_ms: f64, batch_ms: f64) -> i64 {
        let cover = if cover_ms.is_finite() { cover_ms.clamp(0.0, 60_000.0) } else { 0.0 };
        let per_second = if speed > 4 {
            // beyond the engine's x4: the old count (no batch of more than 4 steps is known)
            Self::STEPS_PER_SECOND * f64::from(speed)
        } else {
            Self::steps_per_second(speed.max(1), batch_ms)
        };
        let seconds = cover / 1000.0 + Self::HANDLING_SECONDS;
        let batch = i64::from(speed.clamp(1, 4));
        let steps = (seconds * per_second).ceil() as i64 + 1 + batch;
        Self::STEP * steps.clamp(Self::MIN_LEAD_STEPS, Self::MAX_LEAD_STEPS)
    }

    /// The lead on a connection with no measured delay (the floor for a speed).
    pub fn lead_units(speed: i32) -> i64 {
        Self::lead_for(speed, 0.0)
    }
}

/// The host's lead as a jitter buffer (TPF3-MP's playout idea, Juliansgith, MIT; our own measure): it covers the LATEST arrival seen
/// recently, not a quantile of them, plus a small safety margin, grows at once when a later one comes and then shrinks slowly. A
/// quantile left the top 5-10 % of the arrivals uncovered, and those spikes are what stood a game at its barrier (friend test of
/// 2026-10-18, x4 over 30 ms: the guest stood 6-7 s a minute; its clock gaps were 140 ms at the median and up to 825 ms).
///   * the arrival: [`LeadMeter`] gives the largest round trip of each player's last eight and the largest clock gap of each running
///     game in ten seconds ([`Lockstep::cover_ms`] of them: the one-way time and the gap's excess over a batch, at most 500 ms);
///   * the buffer is that plus [`Self::SAFETY_MS`]; a larger value replaces it at once, a smaller one only as the buffer shrinks by
///     [`Self::SHRINK_MS_PER_SECOND`] of real time (playing the backlog off at 105 %, as TPF3-MP does), never below it;
///   * the lead is [`Lockstep::lead_for_cover`] of the buffer: never below one engine batch plus the hand-off step.
///
/// It replaced "the 90 % / 95 % quantile, shorter only after eight seconds lower", which dropped a whole step-down at once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JitterBuffer {
    ms: f64,
    at: Option<f64>,
}

impl JitterBuffer {
    /// Kept on top of the latest arrival (ms): a frame of the receiving game and the file hand-off's polling (20 ms).
    pub const SAFETY_MS: f64 = 20.0;
    /// How fast the buffer shrinks towards a smaller arrival (ms per second of real time: 5 %).
    pub const SHRINK_MS_PER_SECOND: f64 = 50.0;

    /// The buffer after the arrival `cover_ms` ([`Lockstep::cover_ms`]) was measured at host time `now` (seconds).
    pub fn update(&mut self, cover_ms: f64, now: f64) -> f64 {
        let target = if cover_ms.is_finite() { cover_ms.max(0.0) } else { 0.0 } + Self::SAFETY_MS;
        let dt = self.at.map_or(0.0, |a| if now.is_finite() { (now - a).max(0.0) } else { 0.0 });
        if now.is_finite() {
            self.at = Some(now);
        }
        self.ms = if target >= self.ms { target } else { (self.ms - Self::SHRINK_MS_PER_SECOND * dt).max(target) };
        self.ms
    }

    /// The buffer now (ms); 0 before the first update.
    pub fn ms(&self) -> f64 {
        self.ms
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

/// The clock lines of one game: the host time and game time of its clock's last move, whether its engine has run since then, and
/// the gaps (host time, ms) between two moves of a running game. Between two moves a game also sends lines whose time did not move:
/// a keepalive of a running engine between two batches (the 30-frame and real-time cadences, X.clockDue) says nothing and is
/// skipped; a line of a game whose engine stands (speed 0: at the barrier, the pacing, a pause) ends the run, and the move after it
/// starts a new one. It took the line before a move to have moved too, which a keepalive between two batches never had: in the
/// friend test of 2026-10-18 nearly every gap was left out (link.lead gap_ms=0), and the lead fell to its floor.
#[derive(Debug, Clone, Default, PartialEq)]
struct ClockCadence {
    last: Option<(f64, i64, bool)>,
    /// (host time, gap ms, game time the move covered)
    gaps: VecDeque<(f64, f64, i64)>,
}

impl LeadMeter {
    /// Round trips kept per player (a ping every 2 s: the last 16 s or so).
    pub const RTT_KEEP: usize = 8;
    /// Quantile of those round trips the lead covers: the largest (the latest arrival, [`JitterBuffer`]).
    pub const RTT_QUANTILE: f64 = 1.0;
    /// Clock gaps are counted over this many seconds of host time...
    pub const GAP_WINDOW_SECONDS: f64 = 10.0;
    /// ...at most this many per game...
    pub const GAP_KEEP: usize = 512;
    /// ...and only once a game has this many in the window (a few gaps are no quantile).
    pub const GAP_MIN: usize = 20;
    /// Quantile of the gaps the lead covers: the largest (the latest arrival, [`JitterBuffer`]); a stall is still not hidden behind the
    /// lead (the excess is at most [`Lockstep::GAP_EXCESS_MAX_MS`]) and a game that stops for seconds is a stall (host/stall.rs).
    pub const GAP_QUANTILE: f64 = 1.0;

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

    /// A clock line of `player` showing game time `t` and engine speed `speed` reached the host at host time `at` (seconds).
    pub fn clock(&mut self, player: u16, t: i64, speed: i32, at: f64) {
        let c = self.clocks.entry(player).or_default();
        let running = speed > 0;
        c.last = match c.last {
            Some((last_at, lt, ran)) if t > lt => {
                if ran && at >= last_at {
                    c.gaps.push_back((at, (at - last_at) * 1000.0, t - lt));
                    while c.gaps.len() > Self::GAP_KEEP {
                        c.gaps.pop_front();
                    }
                }
                Some((at, t, running))
            }
            // the same time: a keepalive between two batches keeps the run, a standing engine ends it
            Some((last_at, lt, ran)) if t == lt => Some((last_at, lt, ran && running)),
            // the first line, or a clock that went back (a reload): no gap until the next move
            _ => Some((at, t, false)),
        };
    }

    /// A game's clock lines start over (it was stalled, host/stall.rs): the silence is no gap of a running game, so the next move is
    /// counted from the line after it, not from the line before the silence.
    pub fn restart_cadence(&mut self, player: u16) {
        if let Some(c) = self.clocks.get_mut(&player) {
            c.last = None;
        }
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
                let recent: Vec<f64> = c.gaps.iter().filter(|(at, _, _)| *at >= from).map(|&(_, g, _)| g).collect();
                if recent.len() < Self::GAP_MIN {
                    return None;
                }
                quantile(recent.into_iter(), Self::GAP_QUANTILE)
            })
            .fold(0.0, f64::max)
    }
}

impl LeadMeter {
    /// Quantile of a game's full-batch gaps taken as its engine's batch interval: a low one, the batches the smooth pacing did not
    /// stretch (it stretches them up to 2x near a stop) and the frames did not delay.
    pub const BATCH_QUANTILE: f64 = 0.25;

    /// The engine's batch interval (ms) at `speed`, measured from the clock lines in the last [`Self::GAP_WINDOW_SECONDS`]: per game
    /// that `here` accepts, the [`Self::BATCH_QUANTILE`] of the gaps before a move of exactly one full batch (`speed` steps: a batch
    /// shortened at a stop or two batches seen in one frame say nothing about the interval), at least [`Self::GAP_MIN`] of them; the
    /// shortest over the games (the fastest engine runs the most steps in a given real time, so the lead in steps is the longer one).
    /// `None` until a game has enough. Real game of 2026-10-22: about 200 ms at x1 and x4 (the lead assumed 167 ms).
    pub fn batch_ms(&self, now: f64, speed: i32, here: impl Fn(u16) -> bool) -> Option<f64> {
        let from = now - Self::GAP_WINDOW_SECONDS;
        let full = Lockstep::STEP * i64::from(speed.clamp(1, 4));
        self.clocks
            .iter()
            .filter(|(id, _)| here(**id))
            .filter_map(|(_, c)| {
                let recent: Vec<f64> =
                    c.gaps.iter().filter(|(at, _, dt)| *at >= from && *dt == full).map(|&(_, g, _)| g).collect();
                if recent.len() < Self::GAP_MIN {
                    return None;
                }
                quantile(recent.into_iter(), Self::BATCH_QUANTILE)
            })
            .reduce(f64::min)
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
