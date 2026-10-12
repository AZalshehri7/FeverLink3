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

**Sealed frames** (everything except the three handshake kinds): `body = AES-256-GCM(key, nonce, payload) ‖ 16-byte tag`, with the 12 header bytes as additional authenticated data and `nonce = u32(sender) ‖ u64(counter)`. A receiver keeps a 64-frame replay window per sender and records a counter, and makes the sender's window, only after the tag verified, so a forged frame can neither burn a counter nor grow the receiver. A sender id belongs to one connection for the whole session (the host never gives one twice, a player who reconnects is a new sender) and its counter never restarts, so a nonce is never used twice under a key; a secret is used for one session only. A payload that does not fit a frame (more than `1 048 576 - 12 - 16` bytes) is not sent.

**Payloads** are UTF-8 JSON, camelCase, nulls omitted, byte arrays base64, doubles in their shortest exact form (a parser must give back the same double: `serde_json` needs `float_roundtrip` for that). Unknown fields are ignored; a change of meaning needs a new kind.

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
conn   → host    Join       { name, version, proof, resume? }                      unsealed, sender 65535
                 proof = HMAC-SHA256(key, nonce ‖ UTF8(name))
                 resume = { id, mac }, from a player coming back (see Reconnect)
host   → conn    Welcome    { you, hostName, version, players[], session, resume?, resumed? }  sealed, sender 0
   or            Reject     { reason, detail? }  then the host closes the connection  unsealed
```

Reject reasons: `bad-proof` (wrong key or password), `version` (detail names both versions), `full` (detail: `8 players`, and how many of them are away and holding their slot), `malformed`, `flood` (too many frames, see Limits), `timeout` (the handshake did not finish in time).

After Welcome the player seals with its assigned id. The host checks that the sender id in every frame equals the id bound to that connection; a mismatch is dropped. A player accepts sealed frames from sender 0 only. An unsealed Reject is honoured only before the Welcome; afterwards the host ends a session with the sealed Kicked (see Leave and removal), because anyone on the path can write an unsealed frame.

A player answers a Challenge only if its nonce is 16 bytes and the protocol number is its own. It honours a Reject only while it is still joining (a Reject is unsealed: anyone on the path can write one), and a second Welcome on a connection is dropped (it would replace the player's sealer and restart its counter).

Before a connection has joined, the host reads its first frame with a limit of 4096 bytes (`FrameCodec::HANDSHAKE_MAX_LENGTH`), closes it if no accepted Join arrived within 15 s of opening, and holds at most 32 such connections at once. The transport adds its own limits (64 connections, 16 per address, a 10 s write timeout, 1000 frames and 1 MiB per second per connection); see `docs/SECURITY.md`, section 6.

## Who may send what

| Kind | Value | Direction | Payload |
|---|---|---|---|
| Challenge | 1 | host → connection | `{nonce, protocol, version}` |
| Join | 2 | connection → host | `{name, version, proof, resume?: {id, mac}}` |
| Reject | 3 | host → connection | `{reason, detail?}` |
| Welcome | 10 | host → player | `{you, hostName, version, players: [{id, name, ready}], session, resume?: {token, epoch}, resumed?}` |
| Session | 11 | host → all | `{started, speed, pauseAt?, lead}` |
| PeerClock | 12 | host → all but origin | `{from, t, ahead}` |
| PlayerJoined | 13 | host → all but the joiner | `{player: {id, name, ready}}` |
| PlayerLeft | 14 | host → all | `{id, name, away?}` (`away` = the connection dropped and the slot is held) |
| ChecksumRequest | 15 | host → all | `{n, at, paused}` |
| Authority | 16 | host → players | `{n, values: {name: number}}` |
| ResyncSave | 17 | host → its own game | `{id, name}` |
| ResyncChunk | 18 | host → players | `{id, index, count, sha256, data}` |
| ResyncLoad | 19 | host → all | `{id, name, sha256}` |
| Permissions | 20 | host → all | `{build, bulldoze, spend, speed, other?}`, each `"everyone"`, `"hostOnly"` or `{"only": [ids]}` |
| Ping | 21 | host → player | `{n, rtt?: {id: ms}}`, answered by the player's session layer; never given to the game |
| Kicked | 22 | host → player | `{reason, detail?}` (the shape of Reject), sealed: the host ended this player's session |
| Hello | 30 | game → host | `{time, save?}` |
| Clock | 31 | game → host | `{t, speed, ahead}` |
| SpeedRequest | 32 | game → host | `{speed}` 0 = pause |
| ChecksumReport | 33 | game → host | `{n, time, parts: {name: hash}, authority?: {name: number}}` |
| SaveDone | 34 | host's game → host | `{id, ok, detail?}` |
| ReplayFailed | 35 | game → host | `{detail}` |
| ActionRefused | 36 | game → host | `{seq, detail}` |
| Pong | 37 | player → host | `{n}`, sent by the player's session layer, not by its game |
| Leave | 38 | player → host | `{}`: the player goes on purpose |
| Action | 50 | game → host → all but origin | `{stamp, seq, body, category?, from, allowed}` (`from` and `allowed` set by the host; `category` is `build`, `bulldoze`, `spend` or `other`, default `other`) |
| Chat | 51 | game → host → all but origin | `{text ≤ 500, from, name}` (set by the host) |
| BuildVeto | 52 | game → host → all but origin | `{seq, owner, why ≤ 300, from, name}` (`from`, `name` set by the host). "I cannot build action `owner`:`seq`", sent only by a game that dry-ran a held strict build and found it cannot be rebuilt; silence means yes. The originator cancels its held command, every game drops the queued replay. |
| BuildCommit | 53 | game → host → all but origin | `{seq, owner, stamp, from}` (`from` set by the host). The originator of a held strict build made while the game is PAUSED waits a real-time grace for vetoes (the peers dry-run the record and park it, applying nothing), then sends this; every game applies the parked build at `stamp` (the pause time, or a normal stamp when the game was unpaused meanwhile) and the originator releases its own command. A veto during the grace drops the build everywhere. |

A player may send kinds 30 to 38 and 50 to 52. Anything else from a player is dropped, counted and logged once. The host's own game is player 0 and is trusted like the host.

What the host does with the content of those kinds, beyond decoding them (a message that does not decode is ignored, as always):

- **Clock** with `t` outside `0..=2^50` or `speed` outside `0..=16` is dropped, counted and logged once per player: the host builds every stamp and pause from the clocks it holds.
- **Action** with a `body` over 256 KiB is dropped; **Chat** text is cut to 500 characters.
- **ChecksumReport** with more than 128 parts, or a part name or hash over 128 characters, or more than 128 `authority` entries, is ignored.
- Text a player chose (Hello, ReplayFailed, the version of a Join) is cut before it reaches a log line or a Reject.

## Session rules

- **Session state** is broadcast on every change and at least every 3 s. `lead` is what a game adds to its clock when stamping a local action, in game-time units (a multiple of 200). The host computes it from the slowest connected player's measured round trip and the speed: `200 × clamp(ceil((rtt/2 + 0.06 s) × 6 × speed) + 2, 3, 24)`, where 6 is the steps a second at normal speed (measured). It lengthens at once when a player turns out to be slow and shortens only after eight seconds of the wanted lead staying lower. On a good connection that is 3 steps, about half a second at normal speed.
- **Clock**: a game reports `{t, speed, ahead}` regularly. The host extrapolates `t + elapsed × speed × 1000` and relays it as PeerClock. Every game holds (speed 0) when it is more than `ahead` steps past the slowest peer.
- **Stamps** are multiples of 200. A future stamp chosen by the host is `roundUp(maxClock + lead)`.
- **Start**: `started = true`, `speed = 0`, `pauseAt` = the games' common time when they agree, and a checksum round is requested at once.
- **Pause** is a stamp: `pauseAt = futureStamp`; every game stops at exactly that step. **Speed** 1..4 clears `pauseAt`.
- **SpeedRequest** from a player allowed by the `speed` permission is obeyed while the session is started, no resync round is running and the value differs; from any other player it is dropped (see Permissions).
- **Checksum rounds**: every 10 s while running, the host sends `{n, at, paused}`. When `paused`, `at` is the latest time any game stopped at. Every game that said Hello must report; the round completes when they all have, compares the per-part hashes, and reports the differing parts with each game's value. Values from the host's report under `authority` are relayed to the players as Authority, which they apply to themselves (money correction). Rounds older than 20 are forgotten.
- **Actions** are relayed in the order the host received them, to every game but the origin, with `from` and `allowed` filled in by the host. The body is the mod's own serialisation and is opaque here.
- **Leaving**: a closed connection takes the player out of the game at once: its clock is dropped and PlayerLeft goes to everyone, with `away: true` because the host holds the slot for a return (see Reconnect). A player that says Leave first is gone for good (`away` left out).

## Resync round

A resync makes every other game load the host game's save (and the host's own game too when `HostOptions::host_reloads` is true, the library default; the in-game module also reloads it unless `FEVERLINK3_HOST_RELOAD=0`, see "Host that does not reload" below). It runs in the host's state machine (`resync_state()`: `Idle`, `Pausing`, `Saving`, `Transferring`, `Loading`, `Verifying`), is driven by incoming messages and by `tick()` with the host's clock, and has one round at a time. Each round has an id (1, 2, ...). The session must be started. Every state change is reported to the host's UI as `ResyncProgress`; the session stays paused after a failed round.

```
Pausing       host sets pauseAt (the usual future stamp; a pause already in force is kept) and broadcasts Session.
              Waits until every ready game's last Clock has t >= pauseAt and speed 0.            timeout 120 s
Saving        host → its own game   ResyncSave { id, name: "feverlink3-resync-<id>" }
              host's game → host    SaveDone { id, ok, detail? }                                   timeout 180 s
Transferring  the launcher reads the file and calls resync_file_ready(id, sha256, len); the host
              seals ResyncChunk to every player present at that moment, one call per chunk.      timeout 600 s
Loading       host → participants   ResyncLoad { id, name, sha256 }   (its own game included when host_reloads)
              Every participant's ready flag is cleared and the stored clocks are dropped; the round
              waits for a Hello from each. A host that does not reload keeps its ready flag and its
              clock and sends no Hello.                                                           timeout 600 s
Verifying     a paused checksum round: ChecksumRequest { n, at: pauseAt, paused: true }.         timeout 120 s
              Identical: the previous speed resumes (pauseAt cleared), round Idle.
              Different: ResyncFailed, the session stays paused, round Idle.
```

**Host that does not reload (`host_reloads = false`).** The host's game saves the world only to send it and keeps playing its live world, so the host never sees a loading screen; only the players load. The paused checksum in Verifying is then the proof that the host's live world and the players' loaded copies agree: a difference fails the round and leaves the session paused exactly as in the reloading case. The checksum covers money, loan, constructions, lines and time, not every part of the simulation, so a world with many vehicles is the place to watch for drift. `FEVERLINK3_HOST_RELOAD=0` selects this mode in the in-game module; it is not the default because a live world drifts from a freshly loaded one. With `host_reloads` the host is sent its `ResyncLoad` as soon as the save is announced (during `Transferring`), so it reloads while the players download (`HostOptions::host_loads_early`, `FEVERLINK3_HOST_LOAD_EARLY=0` for the old order: the host's load goes out together with the players' after the transfer; lab runs on 2026-10-12 could not tell the two orders apart, see the maintainer's private notes).

**Timeouts.** Pausing, Saving and Transferring end the round with `ResyncFailed` (reason names what was missing). Loading does not fail: the host logs the players that sent no Hello and continues with the ones that did. Verifying fails. Transferring and Verifying timeouts are not in the original design; they exist so a round cannot wait forever.

**Bounds.** A chunk carries at most 256 KiB (262 144 bytes) of data; a transfer has 1 to 2048 chunks, so a save is 1 byte to 512 MiB. The host cuts the file at exactly 256 KiB, `count = ceil(len / 256 KiB)`, and sends chunks in order from index 0 with the exact size (`chunk_len`); anything else is refused and logged, nothing is sent. `sha256` is 64 hex digits (the host stores it lower case). A base64 chunk frame is about 350 000 bytes, inside the 1 MiB frame limit. A host with no other player connected skips the chunks and goes straight to Loading.

**Receiving side** (`PlayerSession`). A ResyncChunk from the host is never passed to the game. The player validates: `sha256` is 64 hex digits; `1 <= count <= 2048`; `0 <= index < count`; `1 <= data.length <= 256 KiB`; for a chunk of a transfer already in progress, the same `count` and `sha256` as before. A duplicate index is ignored. One transfer is held at a time; a chunk with another `id` replaces it. An unfinished transfer is forgotten 10 minutes after its first chunk (`tick()` or the next chunk). When all chunks are present the player assembles them, computes SHA-256 and only on a match emits `PlayerOutput::SaveReceived { id, sha256, bytes }`; on a mismatch it forgets the transfer and counts the chunk as dropped. A ResyncLoad is delivered to the game only for an `id` and `sha256` whose save was received intact, otherwise it is dropped.

**During a round** SpeedRequest from players, and `set_speed` and `pause` from the host's UI, are ignored (logged). Actions are relayed as usual. Periodic checksum rounds do not run because the session is paused.

**Participants.** The players present when the file is announced are the round's participants: they get the chunks and the ResyncLoad and must say Hello again. A player who connects later waits for the next round (below).

## Hot join

`PlayerJoined` while the session is started starts a round with reason `new player` (`player returned` when the joiner is a player coming back, see Reconnect); the joiner is a participant, so it receives the save and says Hello after loading it. If a round is already running and has not yet announced its file (Pausing, Saving, Transferring before `resync_file_ready`), the joiner simply takes part in it. If the file has been announced (Transferring after the announcement, Loading, Verifying) the joiner is remembered and a new round `new player` starts when the running one ends, whether it succeeded or failed. A hot-join round ignores the cooldown. Before `start` a join starts nothing.

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

Each field is a `PlayerSet`: `"everyone"`, `"hostOnly"` or `{"only": [ids]}`; a missing field means everyone. `other` (renames, colours, line and vehicle settings: the actions of category `other`) is written only while it restricts something, so the message of a host that never touches it is unchanged. The host's own game (id 0) is allowed in every set. Players cannot send kind 20 (dropped). A player stores the last message so its mod can refuse locally and roll back.

**Speed** is enforced by the host: a SpeedRequest from a player not in `speed` is dropped, counted, and logged once per player. **Build, bulldoze and spend** are not dropped by the host; see below.

**Action category and verdict.** An Action gains `category`: `"build"`, `"bulldoze"`, `"spend"` or `"other"` (default `"other"`; any other value counts as `"other"`). On relay the host overwrites `category` with the normalised value and sets `allowed: bool` from the permissions (`other` is allowed unless the `other` set says otherwise). The action is relayed to everyone either way, so every mod sees the same stream and the originating mod, which knows the permissions, can refuse the action and roll back its own change. The host logs the first forbidden action per player and category.

## Players and slots

A session has at most `max_players` games, **the host's own included**: the default 8 is the host and seven players. Ids are handed out from 1 in the order players join and are **never given to anyone else**, not even after the player left: entity keys are `<sender>:<seq>` and frame nonces are `<sender>:<counter>`, so a reused id would repeat both. After 65 534 joins in one session the host answers `full` (`no ids left`). Display names are unique, case-insensitive; a clash becomes `name#id`, and the name of a player who is away still counts as taken.

A slot is held for every player that is connected or away. When a held slot is released (the window ran out, or the host's `kick`) the others get PlayerLeft without `away`, so no one keeps a ghost. A player who comes back to a session that has not started keeps the `ready` it had (its game was not reloaded); in a started session the hot-join round reloads it and it says Hello again. A joiner finding all `max_players` slots held is rejected with `full`; the detail counts the away ones, since they may return.

## Reconnect

A player whose connection dropped and that returns within the window gets its **id, name, slot and permissions** back (permissions are sets of ids, so keeping the id keeps them).

```
first Welcome   resume = { token: 16 random bytes, epoch: 0 }       sealed, so only holders of the session key see it
drop            the host takes the player out of the game, holds the slot, broadcasts PlayerLeft { away: true }
new connection  Join { name, version, proof, resume: { id, mac } }
                mac = HMAC-SHA256(token, "FeverLink3 resume" ‖ nonce ‖ u16 id ‖ UTF8(name))
host            token valid, slot held (or still bound to a connection the host has not noticed is dead) →
                Welcome { you: id, ..., resume: { token, epoch: epoch + 1 }, resumed: true }
                PlayerJoined to the others, Permissions to the returner, and in a started session a hot-join round
                (reason `player returned`); the returner is not ready until it says Hello after loading the save
                anything else → a plain join, a new id, `resumed` left out
```

- The **window** is `reconnect_window_seconds` (330 s: the launcher retries for 300 s, plus a margin). After it the slot is released and the player is gone; `0` holds nothing.
- The token never travels again: the claim is an HMAC over this connection's challenge nonce, so a captured Join is worth nothing on another connection and the host's `bad-proof` check fails first. A claim that does not hold (the slot expired, was released, or the id and token are not a pair this host gave) is not an error: it is a plain join with a new id, and a guess leaves the real slot alone. A host started again has a new secret, so a player of the old session fails the proof (`bad-proof`) and nothing carries over: **a session key never outlives its host**, which is also what keeps the `(sender, counter)` nonces unique across hosts.
- **Replays.** The welcome carries the nonce of the challenge it answers (`nonce`); a player drops a welcome that names another, so a welcome captured on an earlier connection and played again cannot hand it an id and an epoch whose counters were used before. A player also answers one challenge per connection, and drops any host frame whose counter is at or below the welcome's: the host seals one counter stream, so such a frame was sealed before this connection began (a fresh replay window cannot know that). A host that predates `nonce` sends none, and the check is then skipped.
- A return takes over a connection the host still believes in (a pulled cable or a sleeping PC is noticed late): the old one is closed, the others see PlayerLeft(away) and PlayerJoined.
- **Frame counters.** The sender id is reused, so its counter must not start again at 1: nonce reuse under one key breaks AES-GCM, and the host's replay window would refuse the frames. The counter of a connection starts at `epoch << 40`; the epoch is 0 for the first connection of an id and one more for each return (at most 2²⁴ − 1), so every nonce is used once and the first frame of the new connection is newer than anything the old one sent. The player's launcher keeps `{id, token}`; the epoch comes in the Welcome.
- The game never sees the token: the launcher removes `resume` from the welcome it writes to `to_game.log`. It does see `resumed: true`.
- A player that stops on purpose sends Leave first (below), so the slot is not held.

## Leave and removal

- **Leave** (38, player → host, sealed): the player goes on purpose. The host closes the connection, frees the slot at once and broadcasts PlayerLeft without `away`.
- **Kicked** (22, host → player, sealed): the host ended the session of this player. Reasons: `kicked` (the host's `kick`), `flood` (Limits), `ended` (the host stopped hosting or its world was closed: `HostSession::end_session` sends it to every joined player, and an unsealed Reject with the same reason to a connection that never joined, before it closes them all). The player's session layer reports it like a Reject (`PlayerOutput::Rejected`), so the launcher does not reconnect. Sealed, because an unsealed Reject is only honoured before the Welcome. The player's slot is **not** held: it may join again with the code like anyone else.
- **End of session** is the host's call too (`HostSession::end_session()`, made when the host session stops): without it a player only sees the connection drop and tries to reconnect to a host that is gone for the whole five minutes.
- **Kick** is the host's call (`HostSession::kick(id)`): a connected player is sent Kicked, disconnected and removed; for a player who is away it releases the held slot; the host's own id is refused.

## Ping and round-trip time

Every `ping_seconds` (2 s) the host sends each connected player `Ping { n, rtt? }`. The player's session layer answers at once with `Pong { n }`; neither reaches the game. The host measures `now − time the ping n was sent` on its own clock, keeps up to 8 unanswered pings per player (an older unanswered one is forgotten when a later one is answered) and smooths the samples: the first sets the value, each later one moves it a quarter of the way. `rtt` in a Ping is the smoothed value in milliseconds, one decimal, of every connected player the host has measured, so every player can show everyone's round-trip time. The host's own game is 0. A Pong with an `n` that was not sent, or was already answered, is ignored.

## Limits

- **Frames per second.** Every connection has a token bucket: `max_frames_per_second` (500) refilled per second, `frame_burst` (2000) held at most; every frame received on the connection takes one token, whatever it is (an unintroduced connection included). The frame that finds the bucket empty is dropped and counted and the connection is ended with reason `flood` and detail `more than N frames per second`: an unsealed Reject before the join, a sealed Kicked after it. The player loses its slot. A game that sends a clock line per rendered frame (240 a second) and a long road's worth of actions at once stays far inside; `0` turns the limit off. The host's own game is never limited. Over TCP the refusal is best effort: a peer that keeps writing while the host closes may see a reset instead.
- **Bytes per second.** The same bucket counts bytes beside frames: `max_bytes_per_second` (2 MiB) refilled per second, `byte_burst` (8 MiB) held at most; a frame takes its header and body (a frame bigger than the burst takes the whole burst). Over the limit: the same `flood` refusal, with detail `more than N bytes per second`. A player's traffic is small (actions, clocks, reports), and the limit is what stops 256 KiB actions at 500 a second from filling the host's disk; `0` turns it off. At most 32 connections wait for their Join at once (`HostSession::MAX_PENDING`); the next is closed on arrival.
- **Silence.** A joined connection from which nothing arrived for `idle_timeout_seconds` (30 s) is closed by the host; its slot is held like any dropped connection's. Pings keep a healthy one far inside the limit, and any frame counts, so a game that only sends clocks needs no pong. Turning the pings off turns this off, since a quiet lobby is not a dead connection. 
- **A host that was away.** A call into the host more than 5 s after the previous one means the host itself was stalled (its process suspended, its thread stuck in a write to a peer that stopped reading), and the frames of every connection are read in one go afterwards. The host then forgives the stall: nobody is timed out for the silence, the frame budgets are full again and not enforced for as long as the stall lasted (at most 60 s) while the backlog is read, the slots held for players who dropped keep their whole window, and the pings sent before it are forgotten, so no round-trip time measures it.
- **Reports.** A ChecksumReport counts only from a game that said Hello since its last load; one from a game that did not (a returner still running its old world) is ignored, logged once. A report that completes a round is not the only way a round completes: when the game a round was waiting for leaves, a round that has every other report is completed then.
- **Handshake.** A connection that has not completed the handshake `handshake_timeout_seconds` (10 s) after it was accepted gets Reject `timeout` and is closed.

## Not yet specified

Companies. It gets a section here when its state machine lands in Core.
