//! Session model: message kinds and payloads, lockstep arithmetic, checksum rounds, and the host and player state
//! machines. See `docs/PROTOCOL.md`, "Handshake" onwards.

mod checksum;
mod host;
mod kind;
mod limits;
pub mod linkstats;
mod lockstep;
pub mod messages;
pub mod mods;
mod names;
pub mod notices;
mod player;
mod policy;
pub mod report;
mod resume;
mod resync;
mod roster;
mod verdict;

pub use checksum::{ChecksumDifference, ChecksumOutcome, ChecksumRounds, FREEQ_PART};
pub use host::{ConnectionId, HostOptions, HostOutput, HostSession};
pub use kind::MessageKind;
pub use limits::RateLimiter;
pub use lockstep::{JitterBuffer, LeadMeter, Lockstep, PlayerClock};
pub use messages::codec;
pub use messages::*;
pub use mods::{ModDifference, ModEntry, ModFingerprint, ModTags};
pub use names::Names;
pub use player::{PlayerOutput, PlayerSession, PlayerState, ResumeTicket};
pub use policy::{ChecksumPolicy, ChecksumPolicyConfig};
pub use report::{
    CollectedPart, CollectedReport, HostReportEvent, PlayerReportEvent, ReportAskMessage, ReportBusyMessage,
    ReportLimits, ReportRequestMessage, ReportRound,
};
pub use resync::{normalize_sha256, sha256_hex, ChunkError, IncomingSave, ReceivedSave, ResyncLimits, ResyncState};
pub use roster::{ConnectionState, RosterEntry};
pub use verdict::{Verdict, VerdictBook, VerdictOutcome, VERDICTS_KEPT};
