/// Game-time arithmetic shared by host and players.
#[derive(Debug)]
pub struct Lockstep;

impl Lockstep {
    /// Game-time units per simulation step.
    pub const STEP: i64 = 200;
    /// Game-time units per real second at speed 1.
    pub const UNITS_PER_SECOND: f64 = 1000.0;

    pub fn round_up_to_step(t: f64) -> i64 {
        (t / Self::STEP as f64).ceil() as i64 * Self::STEP
    }

    /// Steps a stamp must lie ahead of the fastest clock so that every game receives the message before reaching
    /// it: a fixed reserve plus two steps per speed unit for the engine's batch.
    pub fn lead_units(speed: i32) -> i64 {
        Self::STEP * (4 + 2 * i64::from(speed.max(1)))
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
    pub fn estimate(&self, now: f64) -> f64 {
        self.t as f64 + (now - self.at).max(0.0) * f64::from(self.speed) * Lockstep::UNITS_PER_SECOND
    }
}
