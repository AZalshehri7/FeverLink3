//! Payloads are UTF-8 JSON, camelCase, nulls omitted. Byte arrays are base64. Unknown fields are ignored so a newer
//! peer can add fields without breaking an older one; a changed meaning needs a new [`super::MessageKind`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// JSON encoding of every payload.
pub mod codec {
    use serde::de::DeserializeOwned;
    use serde::Serialize;

    pub fn encode<T: Serialize>(message: &T) -> Vec<u8> {
        serde_json::to_vec(message).expect("every message type serialises to JSON")
    }

    /// `None` for anything that is not a well-formed message of the expected shape; never panics on input from
    /// the network.
    pub fn try_decode<T: DeserializeOwned>(bytes: &[u8]) -> Option<T> {
        serde_json::from_slice(bytes).ok()
    }
}

/// Serde helper: a byte array as a base64 string. Use with `#[serde(with = "base64_bytes")]`.
pub mod base64_bytes {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

// ---- unsealed

/// Host to a new connection: prove you hold the session key by answering with an HMAC of this nonce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChallengeMessage {
    #[serde(with = "base64_bytes")]
    pub nonce: Vec<u8>,
    pub protocol: i32,
    pub version: String,
}

/// Connection to host: display name and `HMAC-SHA256(sessionKey, nonce ‖ UTF8(name))`. A player whose connection
/// dropped adds `resume` to ask for its old id and slot back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinMessage {
    pub name: String,
    pub version: String,
    #[serde(with = "base64_bytes")]
    pub proof: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumeClaim>,
    /// What this player's end can do besides the basics (see [`CAP_BULK_DGRAM`]). A host ignores words it does not
    /// know; a host that predates the field ignores the field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<String>,
}

/// Capability: the player can receive a savegame as bulk datagrams (own reliability and congestion control over Steam's
/// unreliable messages) instead of as `ResyncChunk` frames. Only a Steam transport has it.
pub const CAP_BULK_DGRAM: &str = "bulk-dgram-1";

/// Capability: the player can receive a savegame over several parallel Steam connections (the host opens them to the
/// player's second virtual port), written straight to the save file. Only a Steam transport has it.
pub const CAP_BULK_PAR: &str = "bulk-par-1";

impl JoinMessage {
    pub fn new(name: &str, version: &str, proof: Vec<u8>) -> Self {
        Self { name: name.to_owned(), version: version.to_owned(), proof, resume: None, caps: Vec::new() }
    }
}

/// "I am player `id`": `HMAC-SHA256(token, "FeverLink3 resume" ‖ nonce ‖ u16 id ‖ UTF8(name))` with the token from
/// the player's earlier welcome and the nonce of this connection's challenge. The token itself never travels again,
/// and a claim cannot be replayed on another connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeClaim {
    pub id: u16,
    #[serde(with = "base64_bytes")]
    pub mac: Vec<u8>,
}

/// Host to a connection it will close. Reasons: `bad-proof`, `version`, `full`, `malformed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectMessage {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl RejectMessage {
    pub fn new(reason: &str, detail: Option<&str>) -> Self {
        Self { reason: reason.to_owned(), detail: detail.map(str::to_owned) }
    }
}

// ---- session model

/// What every game must agree on. Broadcast by the host on every change and every few seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    /// The session runs (games may leave the loaded time).
    pub started: bool,
    /// 0 while paused or not started, else 1..4.
    pub speed: i32,
    /// Game time every game stops at exactly; `None` when running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_at: Option<i64>,
    /// Game-time units a local action is stamped ahead of now.
    pub lead: i64,
    /// While a resync round runs, what it is doing (`pausing`, `saving`, `sending`, `loading`, `verifying`): the players'
    /// windows say so, because their games just stand still while the host's world is prepared and sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resync: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerInfo {
    pub id: u16,
    pub name: String,
    pub ready: bool,
}

// ---- host -> player

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WelcomeMessage {
    pub you: u16,
    pub host_name: String,
    pub version: String,
    pub players: Vec<PlayerInfo>,
    pub session: SessionState,
    /// The nonce of the challenge this welcome answers. It ties the welcome to one connection, so a welcome captured
    /// on an earlier connection and played again is recognised. Left out by a host that predates it.
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "base64_bytes")]
    pub nonce: Vec<u8>,
    /// What this player needs to get its id and slot back after a lost connection. Read by the player's session
    /// layer only; it is not passed on to the game.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumeGrant>,
    /// The player came back with a valid claim and holds the id it had before.
    #[serde(default, skip_serializing_if = "is_false")]
    pub resumed: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// The secret that lets a player come back as itself. Sealed inside the welcome, so only holders of the session key
/// see it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeGrant {
    #[serde(with = "base64_bytes")]
    pub token: Vec<u8>,
    /// Which use of this id this is: 0 for the first connection, one more for each return. The player's frame
    /// counters start at `epoch << 40`, so a nonce is never used twice under the session key and the host's replay
    /// window sees the new connection's frames as newer than the old one's.
    pub epoch: u32,
}

impl std::fmt::Debug for ResumeGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumeGrant").field("token", &"..").field("epoch", &self.epoch).finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerJoinedMessage {
    pub player: PlayerInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerLeftMessage {
    pub id: u16,
    pub name: String,
    /// The connection was lost and the host holds the slot for a return; the player is not gone for good.
    #[serde(default, skip_serializing_if = "is_false")]
    pub away: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerClockMessage {
    pub from: u16,
    pub t: i64,
    pub ahead: i32,
    /// The stamp margin this game measured it needs (ClockMessage::need), relayed as the host received it (clamped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub need: Option<i32>,
    /// The input barrier's promise (ClockMessage::safe), relayed as the host received it; `None` when absent or implausible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe: Option<i64>,
    /// `safe` is the pause the game is stopped at (ClockMessage::pause).
    #[serde(default, skip_serializing_if = "is_false")]
    pub pause: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecksumRequestMessage {
    pub n: i32,
    pub at: i64,
    pub paused: bool,
    /// False asks for a LIGHT round: every game leaves out the heavy lanes (`ChecksumPolicy::HEAVY_PARTS`: edges, constructions,
    /// node settings, town buildings), which cost about a second of game thread in a big world. Absent means a full round, so an
    /// older host's requests and an older game that ignores the flag both stay correct.
    #[serde(default = "full_by_default", skip_serializing_if = "is_true")]
    pub full: bool,
}

fn full_by_default() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorityMessage {
    pub n: i32,
    pub values: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResyncSaveMessage {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResyncChunkMessage {
    pub id: i32,
    pub index: i32,
    pub count: i32,
    pub sha256: String,
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResyncLoadMessage {
    pub id: i32,
    pub name: String,
    pub sha256: String,
}

/// Host to one player (kind 21): answer with a [`PongMessage`] carrying the same `n`. `rtt` is what the host last
/// measured for every player, in milliseconds, so each player can show everyone's round-trip time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PingMessage {
    pub n: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rtt: BTreeMap<u16, f64>,
}

// ---- player -> host

/// Answer to a [`PingMessage`] (kind 37), sent by the player's session layer, not by its game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PongMessage {
    pub n: u32,
}

/// The game has loaded a savegame and runs the mod. `time` is its game time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloMessage {
    pub time: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<String>,
}

impl HelloMessage {
    pub fn new(time: i64) -> Self {
        Self { time, save: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClockMessage {
    pub t: i64,
    pub speed: i32,
    pub ahead: i32,
    /// Steps of stamp margin, beyond the lead, that this game measured it needs to receive a build before its stamp (the mod's
    /// L.marginNeed: from the slack between each received build's arrival and its stamp, and any late apply). `None`: nothing
    /// measured yet at this speed; the stamping game then uses its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub need: Option<i32>,
    /// The input barrier's promise (the mod's L.promise): every item this game ships after this message is stamped at or after
    /// `safe`, so every other game may simulate the steps before it (less a hand-off allowance) without waiting. `None`: an older
    /// mod, paced as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe: Option<i64>,
    /// The game is stopped at a pause and `safe` is that pause: the others may reach it exactly and not pass it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pause: bool,
}

impl ClockMessage {
    /// The most margin a game may ask for, in steps (the lead's own ceiling): a larger value would only delay every build.
    pub const MAX_NEED: i32 = 24;
    /// The furthest a promise may lie past its clock, in game time: the longest lead and the largest extra with room to spare.
    pub const MAX_SAFE_AHEAD: i64 = 64 * 200;

    /// The promise as the host relays it: one further past the clock than [`Self::MAX_SAFE_AHEAD`] is cut down to that (a
    /// shorter promise only makes the others wait more; a longer one would let them run into steps its sender may still stamp
    /// something for). A smaller one, even below the clock, is relayed as it is, for the same reason.
    pub fn bounded_safe(&self) -> Option<i64> {
        self.safe.map(|s| s.min(self.t.saturating_add(Self::MAX_SAFE_AHEAD)))
    }
}

/// 0 asks for a pause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedRequestMessage {
    pub speed: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecksumReportMessage {
    pub n: i32,
    pub time: i64,
    pub parts: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<BTreeMap<String, f64>>,
}

impl ChecksumReportMessage {
    pub fn new(n: i32, time: i64, parts: BTreeMap<String, String>) -> Self {
        Self { n, time, parts, authority: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveDoneMessage {
    pub id: i32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayFailedMessage {
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRefusedMessage {
    pub seq: i64,
    pub detail: String,
}

// ---- relayed

/// A player action, already stamped by its originator. `body` is opaque to the protocol: the mod's own
/// serialisation. `from`, and `allowed` (the host's verdict from [`Permissions`]), are filled in by the host on
/// relay. A forbidden action is still relayed, tagged `allowed: false`, so every mod can refuse it and the
/// originator can roll it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionMessage {
    pub stamp: i64,
    pub seq: i64,
    pub body: String,
    #[serde(default)]
    pub from: u16,
    /// `build`, `bulldoze`, `spend` or `other`. Anything else, or nothing, counts as `other`.
    #[serde(default = "ActionMessage::default_category")]
    pub category: String,
    /// Set by the host; a player's own value is overwritten.
    #[serde(default = "ActionMessage::default_allowed")]
    pub allowed: bool,
    /// Set by the originator: the action was made while its game stood at the session's pause and is applied during the pause.
    /// The host gives it the next [`pause_seq`](Self::pause_seq) and relays it to EVERY game, the sender included: every game
    /// applies paused actions in that one order (the arrival order on its own connection would differ between games).
    #[serde(default, skip_serializing_if = "is_false")]
    pub pause: bool,
    /// Set by the host on a `pause` action (a player's own value is overwritten): its place in the session's pause order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_seq: Option<u64>,
}

impl ActionMessage {
    pub const CATEGORIES: [&'static str; 4] = ["build", "bulldoze", "spend", "other"];

    pub fn new(stamp: i64, seq: i64, body: &str) -> Self {
        Self {
            stamp,
            seq,
            body: body.to_owned(),
            from: 0,
            category: Self::default_category(),
            allowed: true,
            pause: false,
            pause_seq: None,
        }
    }

    /// The same action made during a pause (see [`pause`](Self::pause)).
    pub fn paused(mut self) -> Self {
        self.pause = true;
        self
    }

    pub fn with_category(mut self, category: &str) -> Self {
        self.category = category.to_owned();
        self
    }

    fn default_category() -> String {
        "other".to_owned()
    }

    fn default_allowed() -> bool {
        true
    }

    /// The category normalised to one of [`Self::CATEGORIES`].
    pub fn normalized_category(&self) -> &'static str {
        Self::CATEGORIES.iter().copied().find(|c| *c == self.category).unwrap_or("other")
    }
}

// ---- permissions

/// Who may do something. The host's own game is always allowed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlayerSet {
    #[default]
    Everyone,
    HostOnly,
    /// These player ids (and the host).
    Only(Vec<u16>),
}

impl PlayerSet {
    pub fn is_everyone(&self) -> bool {
        matches!(self, Self::Everyone)
    }

    pub fn allows(&self, player: u16) -> bool {
        player == 0
            || match self {
                Self::Everyone => true,
                Self::HostOnly => false,
                Self::Only(ids) => ids.contains(&player),
            }
    }
}

/// Host to all (kind 20): what players may do. Defaults to everything for everyone.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Permissions {
    #[serde(default)]
    pub build: PlayerSet,
    #[serde(default)]
    pub bulldoze: PlayerSet,
    #[serde(default)]
    pub spend: PlayerSet,
    #[serde(default)]
    pub speed: PlayerSet,
    /// Actions of category `other` (renames, colours, line and vehicle settings). Left out of the message while it
    /// is `everyone`.
    #[serde(default, skip_serializing_if = "PlayerSet::is_everyone")]
    pub other: PlayerSet,
}

impl Permissions {
    /// Whether `player` may perform an action of `category`; any category but the first three counts as `other`.
    pub fn allows_action(&self, category: &str, player: u16) -> bool {
        match category {
            "build" => self.build.allows(player),
            "bulldoze" => self.bulldoze.allows(player),
            "spend" => self.spend.allows(player),
            _ => self.other.allows(player),
        }
    }
}

/// "I cannot build this": the veto of a held strict build (`owner`:`seq` names the action). `why` is the first cause the game found
/// (an unresolved node or segment, a critical error of the engine). `from` and `name` are set by the host on relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildVetoMessage {
    pub seq: i64,
    pub owner: u16,
    pub why: String,
    #[serde(default)]
    pub from: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// "Apply it": the commit of a held strict build made in a paused game (`owner`:`seq` names the action; `stamp` is the game time to
/// apply it at). `from` is set by the host on relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildCommitMessage {
    pub seq: i64,
    pub owner: u16,
    pub stamp: i64,
    #[serde(default)]
    pub from: u16,
    /// As [`ActionMessage::pause`]: a commit sent while the session is paused is applied in the pause order, by every game.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pause: bool,
    /// As [`ActionMessage::pause_seq`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_seq: Option<u64>,
}

/// `from` and `name` are set by the host on relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub text: String,
    #[serde(default)]
    pub from: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn new(text: &str) -> Self {
        Self { text: text.to_owned(), from: 0, name: None }
    }
}
