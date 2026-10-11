//! Parallel Steam connections: the way a big file (a resync save of 70 to 80 MB) crosses Steam.
//!
//! **Why.** Measured between two real PCs: one Steam connection delivers about 1 MB/s whatever its send rate is set to
//! (2 MiB/s per connection was even worse on a relay path), but several connections to the same peer add up almost
//! linearly (2 connections 2.0 MB/s, 4 connections 3.7 MB/s, ping unchanged). So the file is sent over N connections
//! at once, with N chosen from the size of the file and then adapted to what the path really delivers.
//!
//! **What it is.** The session's own connection (virtual port 0) is untouched: it keeps carrying the control
//! messages and the lockstep. The sender opens N more Steam connections to the receiver's virtual port [`PAR_PORT`]
//! (one listen socket per process, [`service::ParService`]). Each connection starts with a `Hello` that names the
//! transfer (id, size, chunk size, SHA-256 of the whole file) and proves knowledge of a per-transfer secret; then
//! binary chunks follow, no base64. Chunks go to whichever connection has room (work stealing, at most about 1 MiB
//! in flight on one connection); the receiver writes them straight into the file at their offset (no copy of the
//! file is held in memory), acknowledges them, checks the SHA-256 of the whole file at the end and reports `Done`.
//!
//! - [`wire`]: the messages.
//! - [`growth`]: the pure controller that decides when another connection is opened or the newest one closed.
//! - [`send`]: the sending engine (threads, lanes, requeueing).
//! - [`service`]: the receiving side (the listener, the lanes, the writer).
//!
//! See `docs/STEAM_TRANSPORT.md`, "Parallel connections".

pub mod growth;
pub mod send;
pub mod service;
pub mod wire;

#[cfg(test)]
mod tests;

use std::time::Duration;

pub use send::{run_send, Control, FileSource, PayloadSource, Progress, SendReport, SendSpec, SyntheticPayload};
pub use service::{Allow, ParEvent, ParService};

/// The virtual port the receiving side listens on (0 is the session, 1 the probe).
pub const PAR_PORT: i32 = 2;
/// What one Steam connection delivers (measured), and the send rate every bulk connection is given: 1 MiB/s.
pub const PER_CONN_RATE: i32 = 1 << 20;
/// The transfer time the rule aims at, in seconds: with it a 75 MB save gets 7 connections and an 80 MB one 8 (about
/// 10.5 s at the measured rate, 15 to 20 s with the start-up and the slower first seconds).
pub const TARGET_SECONDS: f64 = 10.5;
/// The most connections one transfer opens.
pub const N_MAX: u32 = 16;
/// Files smaller than this go the old way, in `ResyncChunk` frames on the session's connection.
pub const MIN_SIZE: u64 = 6_000_000;
/// Payload of one chunk. 128 KiB fills Steam's pacing well and keeps a lost connection's loss small.
pub const CHUNK: usize = 128 * 1024;
/// The most unacknowledged bytes on one connection.
pub const INFLIGHT: usize = 1 << 20;
/// Connections streaming from the start (or all of them when fewer are wanted).
pub const INITIAL: u32 = 4;
/// A transfer with no progress for this long is abandoned (the caller falls back to the chunks).
pub const STALL: Duration = Duration::from_secs(10);

/// Environment: `FEVERLINK3_BULK_CONNS=<n>` forces the number of connections (growth is then not checked for
/// scaling), `FEVERLINK3_BULK_MAX=<n>` changes the cap.
pub const CONNS_ENV: &str = "FEVERLINK3_BULK_CONNS";
pub const MAX_ENV: &str = "FEVERLINK3_BULK_MAX";

/// The tunable numbers of a transfer.
#[derive(Debug, Clone, PartialEq)]
pub struct ParConfig {
    pub per_conn_rate: i32,
    pub target_seconds: f64,
    pub max_conns: u32,
    /// `Some(n)`: use n connections whatever the size says.
    pub forced: Option<u32>,
    pub min_size: u64,
    pub chunk: usize,
    pub inflight: usize,
    pub initial: u32,
    pub stall: Duration,
    /// Each new connection must add at least this fraction of the per-connection rate (measured over 2 s).
    pub gain_fraction: f64,
    /// At most one new connection per this many seconds: the time one needs to show that it adds speed.
    pub open_every: f64,
    /// How long a new connection may take to be ready.
    pub connect_timeout: Duration,
}

impl Default for ParConfig {
    fn default() -> ParConfig {
        ParConfig {
            per_conn_rate: PER_CONN_RATE,
            target_seconds: TARGET_SECONDS,
            max_conns: N_MAX,
            forced: None,
            min_size: MIN_SIZE,
            chunk: CHUNK,
            inflight: INFLIGHT,
            initial: INITIAL,
            stall: STALL,
            gain_fraction: 0.6,
            open_every: 2.0,
            connect_timeout: Duration::from_secs(25),
        }
    }
}

impl ParConfig {
    /// The defaults with the environment's overrides.
    pub fn from_env() -> ParConfig {
        Self::from_values(std::env::var(CONNS_ENV).ok().as_deref(), std::env::var(MAX_ENV).ok().as_deref())
    }

    /// The defaults with the values of the two variables (`None`, empty or not a number: no override).
    pub fn from_values(conns: Option<&str>, max: Option<&str>) -> ParConfig {
        let number = |v: Option<&str>| v.and_then(|v| v.trim().parse::<u32>().ok()).filter(|n| *n >= 1);
        let mut cfg = ParConfig::default();
        if let Some(m) = number(max) {
            cfg.max_conns = m.min(64);
        }
        if let Some(n) = number(conns) {
            cfg.forced = Some(n.min(cfg.max_conns));
        }
        cfg
    }

    /// How many connections a file of `size` bytes gets: `ceil(size / (rate * target))` between 1 and the cap, or the
    /// forced number.
    pub fn conns_for(&self, size: u64) -> u32 {
        if let Some(n) = self.forced {
            return n.clamp(1, self.max_conns.max(1));
        }
        let per_conn = f64::from(self.per_conn_rate.max(1)) * self.target_seconds.max(0.1);
        let n = (size as f64 / per_conn).ceil();
        (n as u32).clamp(1, self.max_conns.max(1))
    }

    /// Whether a file of `size` bytes is sent this way (smaller ones use the chunk frames).
    pub fn applies_to(&self, size: u64) -> bool {
        size >= self.min_size
    }
}
