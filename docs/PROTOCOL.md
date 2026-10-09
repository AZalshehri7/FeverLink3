# Wire protocol

Protocol version 1. Implemented in the `feverlink3_core` crate, `core/src` (`protocol.rs`, `crypto.rs`, `lobby.rs`, `session/`); every rule here has a test in `core/tests`.

## Frame

```
u32  length      big-endian; counts everything after this field; 12 ≤ length ≤ 1 048 576
u8   version     1
u8   kind        MessageKind
u16  sender      0 = host, 1..65534 = a player, 65535 = not yet assigned
u64  counter     per sender, starts at 1, strictly increasing
[..] body
```

A length outside the bounds closes the connection before anything is allocated.

**Sealed frames** (everything except the three handshake kinds): `body = AES-256-GCM(key, nonce, payload) ‖ 16-byte tag`, with the 12 header bytes as additional authenticated data and `nonce = u32(sender) ‖ u64(counter)`. A receiver keeps a 64-frame replay window per sender and records a counter only after the tag verified, so a forged frame cannot burn a counter.

**Payloads** are UTF-8 JSON, camelCase, nulls omitted, byte arrays base64. Unknown fields are ignored; a change of meaning needs a new kind.

## Key

`secret` = 16 random bytes carried by the join code.
`ikm = secret`, or `secret ‖ PBKDF2-HMAC-SHA256(password, salt = secret, 600 000 iterations, 32 bytes)` when the host set a lobby password.
`key = HKDF-SHA256(ikm, salt = "FeverLink3 v1", info = "session", 32 bytes)`.

## Join code

Base32 (RFC 4648 alphabet, no padding; dashes, spaces and case ignored on input) of:

```
u8   version (1)
u32  created, unix seconds
16   secret
u8   endpoint count (0..8)
per endpoint: u8 family (4 | 6), address bytes, u16 port
4    first 4 bytes of SHA-256 of everything above
```

One IPv4 endpoint gives 53 characters. Anything that fails the check or the shape decodes to nothing, never to an error.

## Handshake

```
host   → conn    Challenge  { nonce: 16 bytes, protocol: 1, version: "0.1" }      unsealed, sender 65535
conn   → host    Join       { name, version, proof }                               unsealed, sender 65535
                 proof = HMAC-SHA256(key, nonce ‖ UTF8(name))
host   → conn    Welcome    { you, hostName, version, players[], session }         sealed, sender 0
   or            Reject     { reason, detail? }  then the host closes the connection  unsealed
```

Reject reasons: `bad-proof` (wrong key or password), `version` (detail names both versions), `full`, `malformed`.

After Welcome the player seals with its assigned id. The host checks that the sender id in every frame equals the id bound to that connection; a mismatch is dropped. A player accepts sealed frames from sender 0 only.

## Who may send what

| Kind | Value | Direction | Payload |
|---|---|---|---|
| Challenge | 1 | host → connection | `{nonce, protocol, version}` |
| Join | 2 | connection → host | `{name, version, proof}` |
| Reject | 3 | host → connection | `{reason, detail?}` |
| Welcome | 10 | host → player | `{you, hostName, version, players: [{id, name, ready}], session}` |
| Session | 11 | host → all | `{started, speed, pauseAt?, lead}` |
| PeerClock | 12 | host → all but origin | `{from, t, ahead}` |
| PlayerJoined | 13 | host → all but the joiner | `{player: {id, name, ready}}` |
| PlayerLeft | 14 | host → all | `{id, name}` |
| ChecksumRequest | 15 | host → all | `{n, at, paused}` |
| Authority | 16 | host → players | `{n, values: {name: number}}` |
| ResyncSave | 17 | host → its own game | `{id, name}` |
| ResyncChunk | 18 | host → players | `{id, index, count, sha256, data}` |
| ResyncLoad | 19 | host → all | `{id, name, sha256}` |
| Permissions | 20 | host → all | `{build, bulldoze, spend, speed}`, each `"everyone"`, `"hostOnly"` or `{"only": [ids]}` |
| Hello | 30 | game → host | `{time, save?}` |
| Clock | 31 | game → host | `{t, speed, ahead}` |
| SpeedRequest | 32 | game → host | `{speed}` 0 = pause |
| ChecksumReport | 33 | game → host | `{n, time, parts: {name: hash}, authority?: {name: number}}` |
| SaveDone | 34 | host's game → host | `{id, ok, detail?}` |
| ReplayFailed | 35 | game → host | `{detail}` |
| ActionRefused | 36 | game → host | `{seq, detail}` |
| Action | 50 | game → host → all but origin | `{stamp, seq, body, category?, from, allowed}` (`from` and `allowed` set by the host; `category` is `build`, `bulldoze`, `spend` or `other`, default `other`) |
| Chat | 51 | game → host → all but origin | `{text ≤ 500, from, name}` (set by the host) |

A player may send kinds 30 to 36, 50 and 51. Anything else from a player is dropped, counted and logged once. The host's own game is player 0 and is trusted like the host.

## Session rules

- **Session state** is broadcast on every change and at least every 3 s. `lead` is `200 × (4 + 2 × speed)` game-time units: what a game adds to its clock when stamping a local action.
- **Clock**: a game reports `{t, speed, ahead}` regularly. The host extrapolates `t + elapsed × speed × 1000` and relays it as PeerClock. Every game holds (speed 0) when it is more than `ahead` steps past the slowest peer.
- **Stamps** are multiples of 200. A future stamp chosen by the host is `roundUp(maxClock + lead)`.
- **Start**: `started = true`, `speed = 0`, `pauseAt` = the games' common time when they agree, and a checksum round is requested at once.
- **Pause** is a stamp: `pauseAt = futureStamp`; every game stops at exactly that step. **Speed** 1..4 clears `pauseAt`.
- **SpeedRequest** from a player allowed by the `speed` permission is obeyed while the session is started, no resync round is running and the value differs; from any other player it is dropped (see Permissions).
- **Checksum rounds**: every 10 s while running, the host sends `{n, at, paused}`. When `paused`, `at` is the latest time any game stopped at. Every game that said Hello must report; the round completes when they all have, compares the per-part hashes, and reports the differing parts with each game's value. Values from the host's report under `authority` are relayed to the players as Authority, which they apply to themselves (money correction). Rounds older than 20 are forgotten.
- **Actions** are relayed in the order the host received them, to every game but the origin, with `from` and `allowed` filled in by the host. The body is the mod's own serialisation and is opaque here.
- **Leaving**: a closed connection removes the player and its clock and broadcasts PlayerLeft.

## Resync round

A resync makes every game load the host game's save. It runs in the host's state machine (`resync_state()`: `Idle`, `Pausing`, `Saving`, `Transferring`, `Loading`, `Verifying`), is driven by incoming messages and by `tick()` with the host's clock, and has one round at a time. Each round has an id (1, 2, ...). The session must be started. Every state change is reported to the host's UI as `ResyncProgress`; the session stays paused after a failed round.

```
Pausing       host sets pauseAt (the usual future stamp; a pause already in force is kept) and broadcasts Session.
              Waits until every ready game's last Clock has t >= pauseAt and speed 0.            timeout 120 s
Saving        host → its own game   ResyncSave { id, name: "feverlink3-resync-<id>" }
              host's game → host    SaveDone { id, ok, detail? }                                   timeout 180 s
Transferring  the launcher reads the file and calls resync_file_ready(id, sha256, len); the host
              seals ResyncChunk to every player present at that moment, one call per chunk.      timeout 600 s
Loading       host → participants   ResyncLoad { id, name, sha256 }   (its own game included)
              Every participant's ready flag is cleared and the stored clocks are dropped; the round
              waits for a Hello from each.                                                        timeout 600 s
Verifying     a paused checksum round: ChecksumRequest { n, at: pauseAt, paused: true }.         timeout 120 s
              Identical: the previous speed resumes (pauseAt cleared), round Idle.
              Different: ResyncFailed, the session stays paused, round Idle.
```

**Timeouts.** Pausing, Saving and Transferring end the round with `ResyncFailed` (reason names what was missing). Loading does not fail: the host logs the players that sent no Hello and continues with the ones that did. Verifying fails. Transferring and Verifying timeouts are not in the original design; they exist so a round cannot wait forever.

**Bounds.** A chunk carries at most 256 KiB (262 144 bytes) of data; a transfer has 1 to 2048 chunks, so a save is 1 byte to 512 MiB. The host cuts the file at exactly 256 KiB, `count = ceil(len / 256 KiB)`, and sends chunks in order from index 0 with the exact size (`chunk_len`); anything else is refused and logged, nothing is sent. `sha256` is 64 hex digits (the host stores it lower case). A base64 chunk frame is about 350 000 bytes, inside the 1 MiB frame limit. A host with no other player connected skips the chunks and goes straight to Loading.

**Receiving side** (`PlayerSession`). A ResyncChunk from the host is never passed to the game. The player validates: `sha256` is 64 hex digits; `1 <= count <= 2048`; `0 <= index < count`; `1 <= data.length <= 256 KiB`; for a chunk of a transfer already in progress, the same `count` and `sha256` as before. A duplicate index is ignored. One transfer is held at a time; a chunk with another `id` replaces it. An unfinished transfer is forgotten 10 minutes after its first chunk (`tick()` or the next chunk). When all chunks are present the player assembles them, computes SHA-256 and only on a match emits `PlayerOutput::SaveReceived { id, sha256, bytes }`; on a mismatch it forgets the transfer and counts the chunk as dropped. A ResyncLoad is delivered to the game only for an `id` and `sha256` whose save was received intact, otherwise it is dropped.

**During a round** SpeedRequest from players, and `set_speed` and `pause` from the host's UI, are ignored (logged). Actions are relayed as usual. Periodic checksum rounds do not run because the session is paused.

**Participants.** The players present when the file is announced are the round's participants: they get the chunks and the ResyncLoad and must say Hello again. A player who connects later waits for the next round (below).

## Hot join

`PlayerJoined` while the session is started starts a round with reason `new player`; the joiner is a participant, so it receives the save and says Hello after loading it. If a round is already running and has not yet announced its file (Pausing, Saving, Transferring before `resync_file_ready`), the joiner simply takes part in it. If the file has been announced (Transferring after the announcement, Loading, Verifying) the joiner is remembered and a new round `new player` starts when the running one ends, whether it succeeded or failed. A hot-join round ignores the cooldown. Before `start` a join starts nothing.

## Resync policy

`ChecksumPolicy` is a pure struct the host calls on every `ChecksumOutcome` that completes outside a resync round, with the host's clock. A round that completes during a resync is judged by the Verifying rule only.

| Differing parts | Reload when |
|---|---|
| `money` | never; corrected through Authority |
| drift parts: `vehicles`, `onboard`, `persons`, `stocks`, `nodeConfigs`, `script:*` | at least one of them has differed in every checksum round for 300 s (a round with none of them resets the clock) |
| everything else | two consecutive rounds with exactly the same set of differing parts (a different set counts as a new first round, a clean round resets) |
| `ReplayFailed` from any game | at once |

Rounds are judged on their non-money parts, so `money` never hides another difference. A decision resets the counters. **Cooldown:** a decision made less than 120 s after the end of the previous round (finished or failed) is held back; the evidence is kept, so the next round after the cooldown decides at once. The cooldown applies to policy decisions only, not to the host's button or to hot join. A `ReplayFailed` inside the cooldown is not remembered.

A decision is emitted as `HostOutput::ResyncRequested { reason }` and, with the host option `auto_resync` (default true), the round starts at once. With `auto_resync = false` only the request is emitted and the launcher decides, for example by calling `start_resync(reason)`. Nothing is requested before the session starts or while a round is running.

## Permissions

Kind 20, host → all, sent whenever the host calls `set_permissions` and to every joiner right after its Welcome:

```json
{"build": "everyone", "bulldoze": "hostOnly", "spend": {"only": [1, 3]}, "speed": "everyone"}
```

Each field is a `PlayerSet`: `"everyone"`, `"hostOnly"` or `{"only": [ids]}`; a missing field means everyone. The host's own game (id 0) is allowed in every set. Players cannot send kind 20 (dropped). A player stores the last message so its mod can refuse locally and roll back.

**Speed** is enforced by the host: a SpeedRequest from a player not in `speed` is dropped, counted, and logged once per player. **Build, bulldoze and spend** are not dropped by the host; see below.

**Action category and verdict.** An Action gains `category`: `"build"`, `"bulldoze"`, `"spend"` or `"other"` (default `"other"`; any other value counts as `"other"`). On relay the host overwrites `category` with the normalised value and sets `allowed: bool` from the permissions (`other` is always allowed). The action is relayed to everyone either way, so every mod sees the same stream and the originating mod, which knows the permissions, can refuse the action and roll back its own change. The host logs the first forbidden action per player and category.

## Not yet specified

Companies. It gets a section here when its state machine lands in Core.
