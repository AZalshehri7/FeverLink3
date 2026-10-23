//! Payloads are UTF-8 JSON, camelCase, nulls omitted. Byte arrays are base64. Unknown fields are ignored so a newer
//! peer can add fields without breaking an older one; a changed meaning needs a new [`super::MessageKind`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::mods::{ModFingerprint, ModTags};

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
    /// The player's game, mods and builds in compact form (see [`super::mods`]). A host that compares mod lists refuses
    /// a join without it (`version`: an older FeverLink3); an older host ignores the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mods: Option<ModTags>,
}

/// Capability: the player can receive a savegame as bulk datagrams (own reliability and congestion control over Steam's
/// unreliable messages) instead of as `ResyncChunk` frames. Only a Steam transport has it.
pub const CAP_BULK_DGRAM: &str = "bulk-dgram-1";

/// Capability: the player can receive a savegame over several parallel Steam connections (the host opens them to the
/// player's second virtual port), written straight to the save file. Only a Steam transport has it.
pub const CAP_BULK_PAR: &str = "bulk-par-1";

/// Capability: the player's launcher can upload its game's save to the host (`ResyncUpload`), so a repair can take its world from
/// this game when the host's own world may be partly built. Announced by a launcher that knows the game's savegame folders.
pub const CAP_RESYNC_SOURCE: &str = "resync-source-1";

impl JoinMessage {
    pub fn new(name: &str, version: &str, proof: Vec<u8>) -> Self {
        Self { name: name.to_owned(), version: version.to_owned(), proof, resume: None, caps: Vec::new(), mods: None }
    }
}

/// "I am player `id`": `HMAC-SHA256(token, "FeverLink3 resume" ‖ nonce ‖ u16 id ‖ UTF8(name) [‖ "last" ‖ u64 last])` with the
/// token from the player's earlier welcome and the nonce of this connection's challenge. The token itself never travels again,
/// and a claim cannot be replayed on another connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeClaim {
    pub id: u16,
    #[serde(with = "base64_bytes")]
    pub mac: Vec<u8>,
    /// The counter of the last host frame this player received: its game kept its world and asks to be caught up from there
    /// (`docs/PROTOCOL.md`, "Catching up"). Covered by the mac. Absent: the player cannot be caught up (it kept nothing it sent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<u64>,
}

/// Host to a connection it will close. Reasons: `bad-proof`, `version`, `mods`, `full`, `malformed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectMessage {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// With reason `mods`: the host's full list, from which the player makes the readable diff. Sent only after the
    /// join's proof was verified (only holders of the code see it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mods: Option<ModFingerprint>,
}

impl RejectMessage {
    pub fn new(reason: &str, detail: Option<&str>) -> Self {
        Self { reason: reason.to_owned(), detail: detail.map(str::to_owned), mods: None }
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
    /// What could not be compared or only differs in cosmetic mods (`docs/PROTOCOL.md`, "Mod list"): every window shows it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mod_warnings: Vec<String>,
    /// The games whose clocks have not come for the stall timeout (`HostOptions::stall_timeout_seconds`) while the session runs: the
    /// others run on without waiting for them (the host keeps their promises, see `PeerClockMessage::stalled`), and each of them, once
    /// it runs again, catches up and has every action it ships stamped by the host (as in the pause order) until it is out of this list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stalled: Vec<u16>,
    /// While a resync round is `loading`: the games that have not yet said hello after the load (their world is still loading).
    /// Every window names them, because the others only stand still meanwhile (friend test 2026-10-21: ~52 s with only the step shown).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loading: Vec<u16>,
}

impl SessionState {
    /// Mod warnings a session state carries at most (the newest).
    pub const MOD_WARNINGS_KEPT: usize = 8;
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
    /// The host compared this player's mod list and it matched. A player that sent one and gets a welcome without
    /// this flag is talking to an older host and leaves (`version`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub mods_checked: bool,
    /// The host catches this returner up instead of reloading it: the frames it missed follow this welcome, and the number is the
    /// counter of the last frame the host received from it, so it sends the rest of what its game sent (`docs/PROTOCOL.md`,
    /// "Catching up"). Read by the session layer only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up: Option<u64>,
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

/// Wall-clock stamps (milliseconds since 1970, each on the clock of the machine that took it) of one clock or action line on
/// its way from the game that made it to a game that reads it, for the latency logs (owner list 2026-10-17, items 4 and 6).
/// Measurement only: nothing decides anything from it, and an older peer simply leaves it out. The sending game's mod starts
/// it (`g`), its launcher adds `l`, the host `h` with the sender's clock offset and round trip, the receiving launcher `w`
/// with its own; the receiving mod reads the line (its own stamp, not carried further). Offsets are the machine's wall clock
/// minus the host's (`HostSession::wall_offset_ms`), so a stamp `x` taken on that machine is `x - offset` on the host's clock.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hops {
    /// The sending game's mod wrote the line to `from_game.log` (sender's clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub g: Option<i64>,
    /// The sender's launcher read it from `from_game.log` (sender's clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l: Option<i64>,
    /// The host handled it (host's clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<i64>,
    /// The sender's wall clock minus the host's, ms (0 for the host's own game; absent before it was measured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub so: Option<i64>,
    /// The sender's round trip to the host, whole ms (0 for the host's own game).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sr: Option<i64>,
    /// The receiving launcher wrote it to `to_game.log` (receiver's clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<i64>,
    /// The receiver's wall clock minus the host's, ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ro: Option<i64>,
    /// The receiver's round trip to the host, whole ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rr: Option<i64>,
}

impl Hops {
    /// A value a game could not have meant as a wall clock (ms since 1970 between 2001 and 2286) is left out: these stamps
    /// are only ever logged, and a player's own numbers must not make another game's log lie.
    pub fn plausible_ms(v: i64) -> bool {
        (1_000_000_000_000..10_000_000_000_000).contains(&v)
    }

    /// The stamps as the host passes them on: what the sender said (implausible ones dropped), the host's own stamp and what
    /// the host knows of the sender. Fields the host or a receiver fills are never taken from a player.
    pub fn relayed(&self, host_ms: Option<i64>, sender_offset: Option<i64>, sender_rtt: Option<f64>) -> Hops {
        Hops {
            g: self.g.filter(|v| Self::plausible_ms(*v)),
            l: self.l.filter(|v| Self::plausible_ms(*v)),
            h: host_ms,
            so: sender_offset,
            sr: sender_rtt.filter(|r| r.is_finite()).map(|r| r.round() as i64),
            w: None,
            ro: None,
            rr: None,
        }
    }
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
    /// The game `from` is stalled (`SessionState::stalled`): `t` is its last clock and `safe` is the host's promise for it, not the
    /// game's own: every action of that game the host relays after this line is stamped at or after `safe` (the host stamps them, or
    /// refuses one on every game). A receiving game keeps to `safe` as to any promise but does not pace itself to `t`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stalled: bool,
    /// The clock line's stamps so far ([`Hops`]), when its game sent any. Logs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hop: Option<Hops>,
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
    /// A full round spread over the simulation: every game reads the heavy lanes a fixed number of entities at a time, one chunk
    /// every `spread` steps after `at` (`ChecksumPolicy::spread_steps`), so no single step stalls the game. 0 (absent) = the whole
    /// round at `at`, as before: light and paused rounds, and an older host.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub spread: u32,
    /// Entities every game reads per chunk of a spread round, sized by the host from the games' measured cost
    /// (`ChecksumPolicy::chunk_for`). The same number goes to every game, so every game cuts the entity lists at the same places.
    /// 0 (absent) = the game's own default (200 entities), as before.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub chunk: u32,
    /// The round keeps its itemised keys in the game-script state, complete and the same on every game, for the dump at a resync.
    /// The host asks for it after a round that differed, until one is identical again.
    #[serde(default, skip_serializing_if = "is_false")]
    pub detail: bool,
}

impl ChecksumRequestMessage {
    /// A round without a host chunk size or detail keys.
    pub fn new(n: i32, at: i64, paused: bool, full: bool, spread: u32) -> Self {
        Self { n, at, paused, full, spread, chunk: 0, detail: false }
    }
}

fn is_zero(v: &u32) -> bool {
    *v == 0
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
    /// Each player's wall clock minus the host's, in milliseconds, as the host measured it from the pongs (`PongMessage::w`), so a
    /// player can put the stamps of a line ([`Hops`]) on one clock. Logs only; absent from an older host.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub off: BTreeMap<u16, i64>,
    /// The counter of the last frame the host received from this player: what the player keeps for a resume up to it can go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<u64>,
}

// ---- player -> host

/// Answer to a [`PingMessage`] (kind 37), sent by the player's session layer, not by its game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PongMessage {
    pub n: u32,
    /// The player's wall clock when it answered (ms since 1970): with the ping's send and the pong's arrival the host estimates
    /// the player's clock offset (NTP's rule, the arrival in the middle of the round trip). Absent from an older player.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<i64>,
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
    /// The game stands still at a pause, at this game time (its speed 0 took effect and its time did not move for a while). The host
    /// stamps the actions of the pause past the largest of these (the pause order), so every game can still apply them at their stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<i64>,
    /// Wall-clock stamps of this line ([`Hops`]): the mod's write and its launcher's read. Logs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hop: Option<Hops>,
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
    /// Parts this game could not hash for this round (its registry not read from the loaded state yet, a lane that failed):
    /// unknown, not different. They are not in `parts`; the host does not compare them in this round.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_ready: Vec<String>,
    /// What a spread round cost this game (absent for a round hashed whole): the host sizes the next rounds' chunks from it.
    /// A cost that does not read (a float, a string) is dropped, never the report: the report is what is compared.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient_cost")]
    pub cost: Option<ChecksumCost>,
}

fn lenient_cost<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ChecksumCost>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

/// The cost of one spread checksum round on one game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecksumCost {
    /// Nanoseconds an entity: the dearer of the costliest chunk's and the whole round's average.
    #[serde(default)]
    pub entity_ns: u32,
    /// The costliest chunk, microseconds.
    #[serde(default)]
    pub step_max_us: u32,
    /// The entities a chunk read in that round.
    #[serde(default)]
    pub chunk: u32,
}

impl ChecksumReportMessage {
    pub fn new(n: i32, time: i64, parts: BTreeMap<String, String>) -> Self {
        Self { n, time, parts, authority: None, not_ready: Vec::new(), cost: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveDoneMessage {
    pub id: i32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The ids of the mods the saved world uses (`app.getSavegameInfo`, feverlink3_hud.lua `H.worldMods`), as the game
    /// writes them: a list of strings (an empty Lua table arrives as `{}`). Absent: they could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mods: Option<serde_json::Value>,
}

impl SaveDoneMessage {
    /// The world's mod ids, `None` when the game could not read them (or wrote something else).
    pub fn world_mods(&self) -> Option<Vec<String>> {
        match self.mods.as_ref()? {
            serde_json::Value::Array(a) => a.iter().map(|v| v.as_str().map(str::to_owned)).collect(),
            serde_json::Value::Object(o) if o.is_empty() => Some(Vec::new()),
            _ => None,
        }
    }
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
    /// The action's originator (the action is `from:seq`). Not the sender: the host knows who sent it.
    #[serde(default)]
    pub from: u16,
    /// The sending game's verdict of the action at its stamp: `true` applied (the line kind `action_applied`), `false` refused
    /// (`action_refused`, and every message of an older mod). The host compares the verdicts of every game (`VerdictBook`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub applied: bool,
    /// The refusal's class (`inject_refused`, `decode`, `engine`, `company`, `module`), for the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// The engine threw while it applied the command (after the mod handed it over): the apply is not transactional, so the sending
    /// game's world may be PARTLY built. The host never takes a repair's world from such a game (`HostSession::repair_source`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

impl ActionRefusedMessage {
    /// A refusal of action `from:seq` on the sending game.
    pub fn refused(from: u16, seq: i64, detail: impl Into<String>) -> Self {
        Self { seq, detail: detail.into(), from, applied: false, class: None, partial: false }
    }

    /// The action `from:seq` applied at its stamp on the sending game.
    pub fn applied(from: u16, seq: i64) -> Self {
        Self { seq, detail: String::new(), from, applied: true, class: None, partial: false }
    }
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
    /// Set by the originator on a `pause` action: its own engine applied it already, at `stamp`, where its game stood still (a native
    /// build no hold caught). The host numbers it in the pause order like any other but keeps its stamp: every other game runs on to
    /// that step or stands there and applies it at that step (real session 2026-10-23: stamped at the pause stamp instead, 553 steps
    /// before where every game stood, it was late on the host and forced a resync).
    #[serde(default, skip_serializing_if = "is_false")]
    pub applied: bool,
    /// Set by the host on a `pause` action (a player's own value is overwritten): its place in the session's pause order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_seq: Option<u64>,
    /// Set by the host with `pause_seq`: the `pause_seq` of the last item of the same flush of the pause order (every item of one
    /// flush has one stamp). A game runs on to a stamp of the pause only once it has read the whole flush, so the run-on of an
    /// earlier item never passes the stamp of a later one still on its way. `None` (an older host): the run-on as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_upto: Option<u64>,
    /// Wall-clock stamps of this action on its way ([`Hops`]): the host replaces what the sender claims about the host's part.
    /// Logs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hop: Option<Hops>,
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
            applied: false,
            pause_seq: None,
            pause_upto: None,
            hop: None,
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
    /// The host itself refuses the action on every game, its sender's included: a stalled game's action stamped before the promise the
    /// host made for that game while it stood still (`PeerClockMessage::stalled`). Never taken from a player (the host clears it on relay).
    #[serde(default, skip_serializing_if = "is_false")]
    pub host: bool,
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
    /// As [`ActionMessage::pause_upto`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_upto: Option<u64>,
}

/// `from` and `name` are set by the host on relay. With a `level` (`info`, `warn`, `error`: [`notices::Level`](super::notices::Level))
/// the line is a system message, the session's own or a game's about its player's action: the mod shows it among the window's
/// notices, never in the chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub text: String,
    #[serde(default)]
    pub from: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
}

impl ChatMessage {
    pub fn new(text: &str) -> Self {
        Self { text: text.to_owned(), from: 0, name: None, level: None }
    }
}
