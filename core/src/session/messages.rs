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

/// Connection to host: display name and `HMAC-SHA256(sessionKey, nonce ‖ UTF8(name))`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinMessage {
    pub name: String,
    pub version: String,
    #[serde(with = "base64_bytes")]
    pub proof: Vec<u8>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerClockMessage {
    pub from: u16,
    pub t: i64,
    pub ahead: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecksumRequestMessage {
    pub n: i32,
    pub at: i64,
    pub paused: bool,
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

// ---- player -> host

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
}

impl ActionMessage {
    pub const CATEGORIES: [&'static str; 4] = ["build", "bulldoze", "spend", "other"];

    pub fn new(stamp: i64, seq: i64, body: &str) -> Self {
        Self { stamp, seq, body: body.to_owned(), from: 0, category: Self::default_category(), allowed: true }
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
}

impl Permissions {
    /// Whether `player` may perform an action of `category` (`other` is never restricted).
    pub fn allows_action(&self, category: &str, player: u16) -> bool {
        match category {
            "build" => self.build.allows(player),
            "bulldoze" => self.bulldoze.allows(player),
            "spend" => self.spend.allows(player),
            _ => true,
        }
    }
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
