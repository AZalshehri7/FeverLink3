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
        let rtt = if rtt_ms.is_finite() { rtt_ms.max(0.0) } else { 0.0 };
        let seconds = rtt / 2000.0 + Self::HANDLING_SECONDS;
        let steps = (seconds * Self::STEPS_PER_SECOND * f64::from(speed.max(1))).ceil() as i64 + 2;
        Self::STEP * steps.clamp(Self::MIN_LEAD_STEPS, Self::MAX_LEAD_STEPS)
    }

    /// The lead on a connection with no measured delay (the floor for a speed).
    pub fn lead_units(speed: i32) -> i64 {
        Self::lead_for(speed, 0.0)
    }
}

/// A player's last reported clock, extrapolated with its speed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerClock {
    pub t: i64,
    pub speed: i32,
    /// Host time (seconds) at which the report arrived.
    pub at: f64,
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
