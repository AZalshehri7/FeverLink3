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

    /// Steps a stamp must lie ahead of the fastest clock so that every game receives the message before reaching
    /// it: a fixed reserve of two steps plus two steps per speed unit for the engine's batch (0.8 s at normal speed; it was
    /// four steps, 1.2 s, until the first real test showed that a road should show on the other game in under a second).
    pub fn lead_units(speed: i32) -> i64 {
        Self::STEP * (2 + 2 * i64::from(speed.max(1)))
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
