//! The per-connection frame limit: a token bucket the host consults for every frame a connection sends. See
//! `docs/PROTOCOL.md`, "Limits".

/// Lets `per_second` frames through on average and up to `burst` at once. Pure: the caller passes the time, so a
/// test moves its own clock.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    per_second: f64,
    burst: f64,
    tokens: f64,
    last: f64,
}

impl RateLimiter {
    /// A bucket that starts full. `per_second == 0` never refuses; a `burst` under 1 counts as 1.
    pub fn new(per_second: u32, burst: u32, now: f64) -> Self {
        let burst = f64::from(burst.max(1));
        Self { per_second: f64::from(per_second), burst, tokens: burst, last: now }
    }

    /// Fills the bucket again: the host was away, and the frames it is reading now were sent while it was.
    pub fn refill(&mut self, now: f64) {
        self.tokens = self.burst;
        self.last = self.last.max(now);
    }

    /// Takes one frame's worth of the budget. False when the connection is over its limit.
    pub fn allow(&mut self, now: f64) -> bool {
        self.allow_cost(now, 1)
    }

    /// Takes `cost` units of the budget (bytes, for a bucket counted in bytes). A cost above the burst is charged as
    /// the whole burst, so one oversize item is refused only when the bucket is not full.
    pub fn allow_cost(&mut self, now: f64, cost: u32) -> bool {
        if self.per_second <= 0.0 {
            return true;
        }
        let elapsed = (now - self.last).max(0.0);
        self.last = self.last.max(now);
        self.tokens = (self.tokens + elapsed * self.per_second).min(self.burst);
        let cost = f64::from(cost.max(1)).min(self.burst);
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }
}
