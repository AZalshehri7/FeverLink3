//! Bulk transfer over Steam's unreliable datagrams, with our own reliability and congestion control.
//!
//! Why: on a direct route with 214 ms round trip and a 300 Mbit/s line, Steam's reliable stream moved about 1 MB/s
//! whatever send rate was asked for. The stream is the limiter on that path (its recovery from loss and delay at a
//! high round trip), not the line. The connection (NAT traversal, relay fallback, encryption) stays Steam's; the bulk
//! data goes as unreliable messages of 1100 bytes, one per UDP packet, and this module does what TCP would do:
//! numbering, selective acks, retransmission of only what was lost, and a delay-based send rate that keeps the
//! queue at the bottleneck short so voice calls and games on the same line are not hurt.
//!
//! - [`wire`]: the datagram format.
//! - [`cc`]: the rate controller.
//! - [`sender`] and [`receiver`]: the two ends as plain state machines driven by a clock; they know nothing of Steam,
//!   which is what lets `tests` run them against a simulated network.
//! - [`runner`] (in `bulk_io.rs`): threads that connect them to a Steam connection.
//!
//! See `docs/STEAM_TRANSPORT.md`, "Bulk datagrams".

pub mod cc;
pub mod receiver;
pub mod sender;
pub mod wire;

pub use cc::{Cc, CcConfig};
pub use receiver::{Limits, Receiver, ReceiverState};
pub use sender::{MemSource, Sender, SenderConfig, SenderState, SenderStats, Source};
pub use wire::{parse, Packet, DATAGRAM_SIZE, MAGIC, PAYLOAD};

#[cfg(test)]
mod tests;
