//! Session model: message kinds and payloads, lockstep arithmetic, checksum rounds, and the host and player state
//! machines. See `docs/PROTOCOL.md`, "Handshake" onwards.

mod checksum;
mod host;
mod kind;
mod lockstep;
pub mod messages;
mod names;
mod player;
mod policy;
mod resync;

pub use checksum::{ChecksumDifference, ChecksumOutcome, ChecksumRounds};
pub use host::{ConnectionId, HostOptions, HostOutput, HostSession};
pub use kind::MessageKind;
pub use lockstep::{Lockstep, PlayerClock};
pub use messages::codec;
pub use messages::*;
pub use names::Names;
pub use player::{PlayerOutput, PlayerSession, PlayerState};
pub use policy::{ChecksumPolicy, ChecksumPolicyConfig};
pub use resync::{normalize_sha256, sha256_hex, ChunkError, IncomingSave, ReceivedSave, ResyncLimits, ResyncState};
