//! FeverLink3 protocol core.
//!
//! Everything in the launcher that has no Windows, UI or game dependency: length-prefixed framing
//! ([`protocol`]), the sealed channel and session key ([`crypto`]), join codes ([`lobby`]), the messages a relay
//! server speaks ([`relay`]) and the session state machines with their message types ([`session`]). The design is
//! `docs/ARCHITECTURE.md`; the wire format is `docs/PROTOCOL.md` (the relay's: `docs/RELAY.md`); every rule there
//! has a test in `core/tests`.
//!
//! The state machines do no I/O. Frames and local-game messages go in through method calls and every call returns
//! the outgoing events ([`session::HostOutput`], [`session::PlayerOutput`]) for the caller to act on, so the whole
//! protocol is tested with an in-memory harness and the transport is the launcher's business.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod admission;
pub mod crypto;
pub mod lobby;
pub mod protocol;
pub mod relay;
pub mod session;
