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
conn   → host    Join       { name, version, proof, resume?, caps?, mods? }        unsealed, sender 65535
                 proof = HMAC-SHA256(key, nonce ‖ UTF8(name))
                 resume = { id, mac, last? }, from a player coming back (see Reconnect, Catching up)
host   → conn    Welcome    { you, hostName, version, players[], session, resume?, resumed?, modsChecked?, catchUp? }  sealed, sender 0
   or            Reject     { reason, detail?, mods? }  then the host closes the connection  unsealed
```

Reject reasons: `bad-proof` (wrong key or password), `version` (detail names both versions, or says the player's FeverLink3 sends no mod list), `mods` (see "Mod list"), `full` (detail: `8 players`, and how many of them are away and holding their slot), `malformed`, `flood` (too many frames, see Limits), `timeout` (the handshake did not finish in time).

After Welcome the player seals with its assigned id. The host checks that the sender id in every frame equals the id bound to that connection; a mismatch is dropped. A player accepts sealed frames from sender 0 only. An unsealed Reject is honoured only before the Welcome; afterwards the host ends a session with the sealed Kicked (see Leave and removal), because anyone on the path can write an unsealed frame.

A player answers a Challenge only if its nonce is 16 bytes and the protocol number is its own. It honours a Reject only while it is still joining (a Reject is unsealed: anyone on the path can write one), and a second Welcome on a connection is dropped (it would replace the player's sealer and restart its counter).

Before a connection has joined, the host reads its first frame with a limit of 4096 bytes (`FrameCodec::HANDSHAKE_MAX_LENGTH`), closes it if no accepted Join arrived within 15 s of opening, and holds at most 32 such connections at once. The transport adds its own limits (64 connections, 16 per address, a 10 s write timeout, 1000 frames and 1 MiB per second per connection); see `docs/SECURITY.md`, section 6.

### Mod list

Two games that load the same save but run other mods, another game build or another FeverLink3 build run apart from the first minute (`core/src/session/mods.rs`). Two checks, both before any world data is sent:

- **At join, the builds.** Join `mods = {game, feverlink3, native, unsupported?, searched?, tags, more?, unread?}` (`unsupported`: the game build the player's native module does not know, which refuses the join even when every build matches, and the host's window names the player; `searched`: `build:hash`, the game build and a 16-hex-digit hash of every address the native module's search found, which must equal the host's own; absent when the module proved nothing): the game build (`getBuildVersion()`, tf3-docs reference/api/main.html), the FeverLink3 version and the SHA-256 of the native module file in clear, and per installed mod (sorted by id) 4 bytes of `SHA-256("FeverLink3 mod", id, revision)` and 4 bytes of the same over id, revision and files hash (zero: files unknown), base64, at most 256 mods (`more` when the player has more). It keeps the join far inside the 4096-byte first frame. A different build refuses the join: `Reject {reason: "mods", detail, mods}` with the host's builds; an empty build (not readable) is not compared. The player's controller takes the list and the build from its window's `mods` command, which the window sends by itself at load (docs/LIVE_CONTROL.md), so a join through a Steam invite carries them too.
- **Between the save and the transfer, the mods the host's WORLD uses.** The host's game reads them from the save it just made: `app.findAllSavegames`, `app.getSavegameInfo(id):get()` (`Async<SaveGameData>`), `info.metadata.gameModDescs[i].modId.name` (tf3-docs reference/api/app.html; `SaveGameData`, `GameMetadata`, `Async` on reference/api/api/type; `Mod.GameModDesc` on reference/api/api/type/mod.html), and sends the ids in `SaveDone.mods`. Revision, cosmetic flag and files hash of each come from the host's installed list: the window reads `app.getUserProfile():getModRep()`, `getInstalledMods()`, `getModDesc(id).revision` / `.cosmetic` (reference/api/api/type, `UserProfile` and `ModRep`; `Mod.ModDesc` on type/mod.html), and the session controller hashes each mod's files, found by the `modId` of the `mod.json` in the user's `staging_area/` and `mods/` and the game's `mods/` (`mods/release/`, tf3-docs mod-structure). The hash covers every file but `_metadata/`, sorted by path: path, size and, for files up to 4 MiB and while 256 MiB in all have not been read, the content's SHA-256. mod.io downloads live in a folder no page documents: those mods are compared by revision only. A player with a proven difference in a world mod (missing, another revision, the same revision with other files when both hashes are known) is removed before the transfer with a sealed `Kicked {reason: "mods", detail, mods}`, `mods` being the world's list; the player turns it into readable lines ("host has X revision 2, you have revision 1", "missing Y") for its window. Installed mods the world does not use never matter, on either side.
- **Warnings, not refusals.** A difference in a mod whose `mod.json` says `cosmetic` (the only documented sign that a mod does not change the simulation), a files hash unknown on one side, a player with more mods than the join names, and a list one side could not read (`unread`, or a `SaveDone` without `mods`) are not proven mismatches: the host logs them (`MOD WARNING`) and puts them in `Session.modWarnings` (the newest 8), which every window shows.
- **Old peers.** A host with a list refuses a join without one with `version` ("compares mod lists and yours does not"). A host compares nothing when it has no list (the developer launcher). A player that sent a list and gets a Welcome without `modsChecked` talks to an older host: it sends Leave at once and reports `version`, so neither side runs on.
- **Not covered.** A session started without a round (every game already at the same time) sends no world, so only the builds were compared.

## Who may send what

| Kind | Value | Direction | Payload |
|---|---|---|---|
| Challenge | 1 | host → connection | `{nonce, protocol, version}` |
| Join | 2 | connection → host | `{name, version, proof, resume?: {id, mac, last?}, caps?, mods?}` |
| Reject | 3 | host → connection | `{reason, detail?, mods?}` |
| Welcome | 10 | host → player | `{you, hostName, version, players: [{id, name, ready}], session, resume?: {token, epoch}, resumed?, modsChecked?, catchUp?}` (`catchUp`: see Catching up) |
| Session | 11 | host → all | `{started, speed, pauseAt?, lead, resync?, modWarnings?, stalled?, loading?}` (`stalled`: the ids of the stalled games, see the stall timeout; `loading`: while a round is `loading`, the ids of the games that have not said hello after the load, which every window names) |
| PeerClock | 12 | host → all but origin | `{from, t, ahead, need?, safe?, pause?, stalled?, hop?}` (`stalled`: made by the host for a stalled game, `safe` is the host's promise for it) |
| PlayerJoined | 13 | host → all but the joiner | `{player: {id, name, ready}}` |
| PlayerLeft | 14 | host → all | `{id, name, away?}` (`away` = the connection dropped and the slot is held) |
| ChecksumRequest | 15 | host → all | `{n, at, paused, full?, spread?, chunk?, detail?}` (`full: false` = a light round; absent = full; `spread` = a full round read a chunk every `spread` steps, absent = hashed whole at `at`; `chunk` = entities a chunk reads, absent = the game's 200; `detail` = keep the itemised keys in the game-script state for the dump at a repair) |
| Authority | 16 | host → players | `{n, values: {name: number}}` |
| ResyncSave | 17 | host → its own game (or the repair's source player, see "Repair source") | `{id, name}` |
| ResyncChunk | 18 | host → players | `{id, index, count, sha256, data}` |
| ResyncLoad | 19 | host → all | `{id, name, sha256}` |
| Permissions | 20 | host → all | `{build, bulldoze, spend, speed, other?}`, each `"everyone"`, `"hostOnly"` or `{"only": [ids]}` |
| Ping | 21 | host → player | `{n, rtt?: {id: ms}, off?: {id: ms}, ack?}`, answered by the player's session layer; never given to the game (`ack`: the counter of the last frame the host received from this player, see Catching up) |
| Kicked | 22 | host → player | `{reason, detail?}` (the shape of Reject), sealed: the host ended this player's session |
| ReportAsk | 23 | host → players | `{id, report, by, byName, maxBytes, salt}`: build your part of bug report `report` and send it as transfer `id` (see "Bug reports"); read by the launcher, never given to the game |
| ReportBundle | 24 | host → the player who asked | `{id, index, count, sha256, data}` (the shape of ResyncChunk): the finished bug report; launcher only |
| ReportBusy | 25 | host → the player who asked | `{id, byName}`: the request was refused because report `id`, asked by `byName`, is still being made; launcher only |
| Hello | 30 | game → host | `{time, save?}` |
| Clock | 31 | game → host | `{t, speed, ahead, need?, safe?, pause?, stop?, hop?}` (`stop`: the game stands still at the pause at `t`; see the pause order) |
| SpeedRequest | 32 | game → host | `{speed}` 0 = pause |
| ChecksumReport | 33 | game → host | `{n, time, parts: {name: hash}, authority?: {name: number}, notReady?: [name]}` |
| SaveDone | 34 | the saving game → host | `{id, ok, detail?, mods?}` (`mods`: ids of the mods the saved world uses, see "Mod list"); accepted only from the round's source |
| ReplayFailed | 35 | game → host | `{detail}` |
| ActionRefused | 36 | game → host | `{seq, from, detail, applied?, class?, partial?}`: the game's verdict of action `from:seq` at its stamp (see "Action verdicts"; `partial`: the engine threw after it had the command, this game's world may be partly built, see "Repair source") |
| Pong | 37 | player → host | `{n, w?}`, sent by the player's session layer, not by its game |
| Leave | 38 | player → host | `{}`: the player goes on purpose |
| ResyncUpload | 39 | player's launcher → host | `{id, index, count, sha256, data}` (the shape of ResyncChunk): the source player's save, see "Repair source"; never resent after a resume |
| ReportRequest | 40 | player's launcher → host | `{note?}`: make a bug report (the window's button); see "Bug reports" |
| ReportPart | 41 | player's launcher → host | `{id, index, count, sha256, data}` (the shape of ResyncChunk): this game's part of a bug report; never resent after a resume |
| Action | 50 | game → host → all but origin (a `pause` action: all) | `{stamp, seq, body, category?, from, allowed, pause?, applied?, pauseSeq?, pauseUpto?, hop?}` (`from`, `allowed`, `pauseSeq` and `pauseUpto` set by the host; `pauseUpto` is the `pauseSeq` of the last item of the same flush of the pause order, and a game runs on to the flush's stamp only once it has read that one; `category` is `build`, `bulldoze`, `spend` or `other`, default `other`; see the pause order) |
| Chat | 51 | game → host → all but origin | `{text ≤ 500, from, name, level?}` (`from`, `name` set by the host; `level` = info/warn/error makes it a system notice the window shows apart from the chat, kept on relay) |
| BuildVeto | 52 | game → host → all but origin | `{seq, owner, why ≤ 300, from, name}` (`from`, `name` set by the host). "I cannot build action `owner`:`seq`", sent only by a game that dry-ran a held strict build and found it cannot be rebuilt; silence means yes. The originator cancels its held command, every game drops the queued replay. `host?` (only the host's own, cleared on relay): the host refuses the action on every game, its origin included (see the stall timeout). |
| BuildCommit | 53 | game → host → all but origin | `{seq, owner, stamp, from}` (`from` set by the host). The originator of a held strict build made while the game is PAUSED waits a real-time grace for vetoes (the peers dry-run the record and park it, applying nothing), then sends this; every game applies the parked build at `stamp` (the pause time, or a normal stamp when the game was unpaused meanwhile) and the originator releases its own command. A veto during the grace drops the build everywhere. |

A player may send kinds 30 to 41 and 50 to 53. Anything else from a player is dropped, counted and logged once. The host's own game is player 0 and is trusted like the host.

What the host does with the content of those kinds, beyond decoding them (a message that does not decode is ignored, as always):

- **Clock** with `t` outside `0..=2^50` or `speed` outside `0..=16` is dropped, counted and logged once per player: the host builds every stamp and pause from the clocks it holds.
- **Action** with a `body` over 480 KiB is dropped (it must fit one Steam message of 512 KiB); **Chat** text is cut to 500 characters.
- **ChecksumReport** with more than 128 parts, or a part name or hash over 128 characters, or more than 128 `authority` or `notReady` entries (or a `notReady` name over 128 characters), is ignored.
- Text a player chose (Hello, ReplayFailed, the version of a Join) is cut before it reaches a log line or a Reject.

**Latency stamps (logs only).** `hop` on Clock, PeerClock and Action is `{g?, l?, h?, so?, sr?, w?, ro?, rr?}`: wall-clock ms since 1970 when the sending mod wrote the line (`g`), its launcher read it (`l`), the host handled it (`h`), the receiving launcher wrote it to `to_game.log` (`w`), with the sender's and the receiver's clock offset to the host (`so`, `ro`: their wall clock minus the host's) and round trip (`sr`, `rr`). The host sets `h`, `so` and `sr` itself and drops `w`, `ro`, `rr` and implausible stamps from a player. The offsets come from the pongs: `w` is the player's wall clock at the answer, the host takes the offset of the tightest of the last eight round trips (NTP's midpoint rule) and reports every player's in the ping's `off`. Nothing decides anything from these fields; an older peer leaves them out and ignores them.

## Session rules

- **Session state** is broadcast on every change and at least every 3 s. `lead` is what a game adds to its clock when stamping a local action, in game-time units (a multiple of 200). The host computes it from the slowest connected player's measured round trip and the speed: `200 × clamp(ceil((rtt/2 + 0.06 s) × 6 × speed) + 2, 3, 24)`, where 6 is the steps a second at normal speed (measured). It lengthens at once when a player turns out to be slow and shortens only after eight seconds of the wanted lead staying lower. On a good connection that is 3 steps, about half a second at normal speed.
- **Clock**: a game reports `{t, speed, ahead, need?}` regularly. The host extrapolates `t + elapsed × speed × 1000` and relays it as PeerClock. Every game holds (speed 0) when it is more than `ahead` steps past the slowest peer. `need` (optional, steps, clamped by the host to 0..24) is the stamp margin the game measured it needs to receive a build before its stamp; a game stamping a build adds the largest `need` of its peers to the fastest clock plus the lead (mod: `L.marginSample`, `L.marginExtra`) only when a peer sends no promise; otherwise a build every game applies at its stamp is stamped at the game's own promise floor (`L.floorStamp`), which the barrier below keeps ahead of every game, and `need` is a measurement only. `safe` (optional) is the input barrier's promise: every item the game ships after this clock is stamped at or after `safe` (`pause` = true: the game is stopped at a pause and `safe` is that pause). Since a game's clocks and actions reach every other game in the order it sent them (one connection to the host, relayed in the order handled), a game that has read a peer's clock has every item that peer shipped before it, so it simulates up to the smallest promise of its peers less `speed + 2` steps of hand-off allowance (a pause stop: exactly), and no item reaches it after its stamp (mod: `L.promise`, `L.barrier`). The host relays `safe` cut down to at most `t + 12800` and never stamps a pause or a checksum before the largest promise; a running checksum round is stamped on a batch boundary (a multiple of `speed` steps, its spread marks `speed` steps apart) so no game shortens a batch to land on it.
- **Stamps** are multiples of 200. A future stamp chosen by the host is `max(roundUp(maxClock + lead), largest safe)`.
- **Start**: `started = true`, `speed = 0`, `pauseAt` = the games' common time when they agree, and a checksum round is requested at once.
- **Pause** is a stamp: `pauseAt = futureStamp`; every game stops at exactly that step. **Speed** 1..4 clears `pauseAt`.
- **SpeedRequest** from a player allowed by the `speed` permission is obeyed while the session is started, no resync round is running and the value differs; from any other player it is dropped (see Permissions).
- **Checksum rounds**: every 5 s while running (10 s up to 2026-10-18; `FEVERLINK3_FAST_CHECKSUM=0` gives it back), the host sends `{n, at, paused}`. When `paused`, `at` is the latest time any game stopped at. Every game that said Hello must report; the round completes when they all have, compares the per-part hashes, and reports the differing parts with each game's value. A part a game names in `notReady` (it could not hash it: right after a load its company registry is not read back yet, or the lane failed) is unknown in that round: it is not compared on any game and is never a difference, while a part both hashed, or one only one world has, is compared as always. A paused verify round with nothing different but a part not ready is asked again up to 3 times, then the repair fails as "could not be verified" (not as "still differ"). Values from the host's report under `authority` are relayed to the players as Authority, which they apply to themselves (money correction). Rounds older than 20 are forgotten.
- **Actions** are relayed in the order the host received them, to every game but the origin, with `from` and `allowed` filled in by the host. The body is the mod's own serialisation and is opaque here.
- **The stall timeout** (`core/src/session/host/stall.rs`, `FEVERLINK3_STALL_TIMEOUT_S`, default 3 s, 0 = off). A ready game whose Clock lines have not come for that long while the session runs and another game's still come (an autosave, a frozen or minimised window) is STALLED: Session lists it in `stalled`, and the host keeps a promise for it that follows the largest promise of the games that run, sent to every other game as a PeerClock of that game flagged `stalled` (its `t` is the game's last clock). A game keeps to that `safe` like any promise but does not pace itself to the stalled game's `t`. Every action of the stalled game passes the host, which holds the promise: one marked `pause` (a stalled game marks every action so, mod: `X.hostStamps`) gets the pause order's running stamp, past every promise and every kept one, and goes to every game, the origin included; one stamped at or after the kept promise is relayed as it is; one stamped before it is not relayed and is refused on every game with a BuildVeto flagged `host` (queued copies dropped, a held build cancelled, a waiting window command failed): never applied late. The stalled game, once it runs again, catches up (mod `L.catchUp`: engine batches of up to 4 steps, a batch interval down to half), and its window says so. The kept promise stops following the others once the game's clock is within the lead of the furthest one, and the stall ends at the first Clock of the game whose own `safe` has reached it: from there its own clocks are relayed again, never below what the others were promised. A hello (a load) ends a stall too; a game that falls silent again while catching up has its promise follow the others again. The host's log says `stall: ...` at each change. No step is skipped or changed: the stalled game runs every step later, with the same actions at the same stamps.
- **The pause order.** An Action (or BuildCommit) its game made while standing at the session's pause carries `pause: true`. No game stands AT the pause: speed 0 takes effect a few steps late, by a different amount on each game (real run pause3, 2026-10-15: pauseAt 534400, both games stood at 535000). So the host holds it until every ready game has said where it stands still (`stop` in its Clock, at or past the last stamp the host gave in this pause), then STAMPS it one step past the furthest of those places, gives it the next `pauseSeq` (a session counter, never reset; a player's own values are overwritten) and relays it to EVERY game, the origin included. Every game applies it at that stamp like any action (a stopped game runs on to it, a step or a few; the simulation applies it exactly there, whatever the overshoot), the origin only once it came back. A checksum round asked for at a pause is stamped where the furthest game stands, once every game said so. Held items left at the resume get a normal stamp (past every clock and promise) after the resume line; one that reaches the host after the resume gets one too. An Action with `applied: true` as well (a native build no hold caught: its origin's engine applied it already, where that game stood still) is numbered the same way but keeps its own stamp; every other game runs on to that step or stands there and applies it there.
- **A build without a record in the pause order (mod only, 2026-10-18).** A held tool command that has no record before the engine applies it (no capture: no preview, a site the strict table does not know) cannot travel before its apply. In a pause its game keeps it held and sends only its place: an ordinary pause Action whose body is `{fn = "pauseSlot", held}` (category `build`). The host numbers and stamps it like any other. At that number every game holds (no run-on past the slot's stamp plus a step, the later numbers wait, a resume waits too); the origin releases its command once every game stands still at or past that point and it stands at or past every other one, its engine applies it there, and the record goes out as a plain Action stamped at that time with `pauseSlot` (the slot's key) in its body. Every other game, standing at or before it, applies it there and then handles the waiting numbers, each stamped at least one step after the build. A `BuildVeto` from the origin naming the slot gives it up (no record could be made): nothing of it is applied, the numbers follow.
- **Leaving**: a closed connection takes the player out of the game at once: its clock is dropped and PlayerLeft goes to everyone, with `away: true` because the host holds the slot for a return (see Reconnect). The others run on without it; a return within the grace is caught up (see Catching up). A player that says Leave first is gone for good (`away` left out).

## Resync round

A resync makes every other game load the host game's save (and the host's own game too when `HostOptions::host_reloads` is true, the library default; the in-game module also reloads it unless `FEVERLINK3_HOST_RELOAD=0`, see "Host that does not reload" below). It runs in the host's state machine (`resync_state()`: `Idle`, `Pausing`, `Saving`, `Transferring`, `Loading`, `Verifying`), is driven by incoming messages and by `tick()` with the host's clock, and has one round at a time. Each round has an id (1, 2, ...). The session must be started. Every state change is reported to the host's UI as `ResyncProgress`; the session stays paused after a failed round.

```
Pausing       host sets pauseAt (the usual future stamp; a pause already in force is kept; a session that never
              ran: the step the host's game stands at) and broadcasts Session.
              Waits until every ready game in the session's world has a last Clock with t >= pauseAt
              and speed 0. Outsiders are not waited for (see below).                             timeout 120 s
Saving        host → its own game   ResyncSave { id, name: "feverlink3-resync-<id>" }
              host's game → host    SaveDone { id, ok, detail? }                                   timeout 180 s
Transferring  the launcher reads the file and calls resync_file_ready(id, sha256, len); the host
              seals ResyncChunk to every player present at that moment, one call per chunk.      timeout 600 s
Loading       host → participants   ResyncLoad { id, name, sha256 }   (its own game included when host_reloads)
              Every participant's ready flag is cleared and the stored clocks are dropped; the round
              waits for a Hello from each. A host that does not reload keeps its ready flag and its
              clock and sends no Hello.                                                           timeout 600 s
              A participant's Hello { time } moves pauseAt to that time (the save's, the same on
              every game; the furthest when they differ, logged) and drops that game's clock from
              before the load; Session is broadcast again.
Verifying     a paused checksum round: ChecksumRequest { n, at: pauseAt, paused: true }.         timeout 120 s
              Identical: the previous speed resumes (pauseAt cleared), round Idle.
              Different: ResyncFailed, the session stays paused, round Idle.
```

**Outsiders.** A ready game whose world is not the session's yet: every game but the host's when the session starts with games at different times, and a player that joins (or comes back without its world) a started session. The round's load replaces its world; its Hello in Loading ends the mark. Until then its clock holds no pause and counts for no stamp (`max_clock`, `future_stamp`), it is relayed to the others as a PeerClock flagged `stalled` with no `safe` and no `need` (no game paces itself to it or keeps to its promise), and an Action or BuildCommit of it is refused on every game with a host `BuildVeto`. The host logs `resync <id>: pause <t> waits only for the games in the session's world; not for <names>`. UI run 2026-10-21: the start round's pause was the host's clock plus the lead and waited 37 s for a guest whose world stood 35 800 game-ms behind to run up to it at x1.

**Host that does not reload (`host_reloads = false`).** The host's game saves the world only to send it and keeps playing its live world, so the host never sees a loading screen; only the players load. The paused checksum in Verifying is then the proof that the host's live world and the players' loaded copies agree: a difference fails the round and leaves the session paused exactly as in the reloading case. The checksum covers money, loan, constructions, lines and time, not every part of the simulation, so a world with many vehicles is the place to watch for drift. `FEVERLINK3_HOST_RELOAD=0` selects this mode in the in-game module; it is not the default because a live world drifts from a freshly loaded one. With `host_reloads` the host is sent its `ResyncLoad` as soon as the save is announced (during `Transferring`), so it reloads while the players download (`HostOptions::host_loads_early`, `FEVERLINK3_HOST_LOAD_EARLY=0` for the old order: the host's load goes out together with the players' after the transfer; lab runs on 2026-10-12 could not tell the two orders apart, see the maintainer's private notes).

**Timeouts.** Pausing, Saving and Transferring end the round with `ResyncFailed` (reason names what was missing). Loading does not fail: the host logs the players that sent no Hello and continues with the ones that did. Verifying fails. Transferring and Verifying timeouts are not in the original design; they exist so a round cannot wait forever.

**Bounds.** A chunk carries at most 256 KiB (262 144 bytes) of data; a transfer has 1 to 2048 chunks, so a save is 1 byte to 512 MiB. The host cuts the file at exactly 256 KiB, `count = ceil(len / 256 KiB)`, and sends chunks in order from index 0 with the exact size (`chunk_len`); anything else is refused and logged, nothing is sent. `sha256` is 64 hex digits (the host stores it lower case). A base64 chunk frame is about 350 000 bytes, inside the 1 MiB frame limit. A host with no other player connected skips the chunks and goes straight to Loading.

**Receiving side** (`PlayerSession`). A ResyncChunk from the host is never passed to the game. The player validates: `sha256` is 64 hex digits; `1 <= count <= 2048`; `0 <= index < count`; `1 <= data.length <= 256 KiB`; for a chunk of a transfer already in progress, the same `count` and `sha256` as before. A duplicate index is ignored. One transfer is held at a time; a chunk with another `id` replaces it. An unfinished transfer is forgotten 10 minutes after its first chunk (`tick()` or the next chunk). When all chunks are present the player assembles them, computes SHA-256 and only on a match emits `PlayerOutput::SaveReceived { id, sha256, bytes }`; on a mismatch it forgets the transfer and counts the chunk as dropped. A ResyncLoad is delivered to the game only for an `id` and `sha256` whose save was received intact, otherwise it is dropped.

**During a round** SpeedRequest from players, and `set_speed` and `pause` from the host's UI, are ignored (logged). Actions are relayed as usual. Periodic checksum rounds do not run because the session is paused.

**The pause after the load.** The save is taken where the host's game stands still, which can be a few steps past `pauseAt` (speed 0 takes effect late), and every game loads it there. A stamp at the old `pauseAt` is then before the time every game stands at, and is applied late (friend test 2026-10-16: pauseAt 10341800, loaded at 10342400, a build stamped 10341800 was applied 3 steps late on the other game). So the Hellos of the load move `pauseAt` (above), the pause order's floor of the old world is dropped, and from Saving to the end of Loading the pause order stamps nothing: the stops the games reported describe the world being replaced (a host loading early no longer counts, so the furthest stop would be a guest's, behind the save). Held actions of the pause are stamped once every game stands still in the loaded world. The mod does the same on its own side: a game reloaded by a resync treats any `pauseAt` before its load time as that time (`L.loadedAt`), for the Session lines from before the load that it reads after it.

**While the save travels** (Transferring, after `resync_file_ready`) the host keeps relaying, but a participant's game is about to replace its world by the save: every message for it except the session layer's own (Ping, ResyncChunk, ResyncLoad, Kicked) is held (Action, BuildCommit, BuildVeto, PeerClock, Session, Chat, roster lines...) and sealed and sent right after its ResyncLoad, in the order it was made, so the reloaded game reads it after its load line. The same holds for the host's own game when it reloads after the transfer; a host sent its load early (or one that does not reload) gets everything at once. A round that fails in Transferring sends what was held then; a participant that leaves is forgotten with what was held for it. The launcher streams the save without stopping the host's loop (`launcher/src/session/transfer.rs`).

**Repair source.** A round copies the host's world, unless the host's own game reported an action `partial` (the engine threw while it applied a command the mod handed it: the apply is not transactional, the world may be partly built). Such games are marked until a round finishes. At Saving the host then asks the first ready player that is not marked and announced `resync-source-1` (its launcher knows the savegame folders) for its save instead: ResyncSave goes to that player, its SaveDone is the one accepted, and its launcher uploads the file as ResyncUpload frames (not counted against the frame limits) while the round stays in Saving (timeout 180 + 600 s). Complete and matching its SHA-256, the host's launcher writes it as `feverlink3-resync-<id>.sav` in the host's savegame folders (`HostOutput::SourceSave`) and the round goes on from Transferring exactly as if the host had saved it: the host's game loads it too, every player is sent it. With no such player (every game marked, no capable player, or a host that does not reload) the host's world is copied as before and a WARNING line and a chat line say it may still be broken. The source leaving before its save arrived fails the round.

**Participants.** The players present when the file is announced are the round's participants: they get the chunks and the ResyncLoad and must say Hello again. A player who connects later waits for the next round (below).

## Hot join

`PlayerJoined` while the session is started starts a round with reason `new player` (`player returned` when the joiner is a player coming back that cannot be caught up, see Reconnect); the joiner is a participant, so it receives the save and says Hello after loading it. If a round is already running and has not yet announced its file (Pausing, Saving, Transferring before `resync_file_ready`), the joiner simply takes part in it. If the file has been announced (Transferring after the announcement, Loading, Verifying) the joiner is remembered and a new round `new player` starts when the running one ends, whether it succeeded or failed. A hot-join round ignores the cooldown. Before `start` a join starts nothing.

## Resync policy

`ChecksumPolicy` is a pure struct the host calls on every `ChecksumOutcome` that completes outside a resync round, with the host's clock. A round that completes during a resync is judged by the Verifying rule only.

| Differing parts | Reload when |
|---|---|
| `money` | never; corrected through Authority |
| drift parts: `vehicles`, `onboard`, `persons`, `stocks`, `nodeConfigs`, `script:*` | at least one of them has differed in every checksum round for 300 s (a round with none of them resets the clock) |
| everything else | two consecutive rounds in which some ordinary part differs, whichever parts (a fork spreads from lane to lane and the per-town `townBuildings:<town>` parts change with each town; a clean round resets) |
| `ReplayFailed` from any game | at once |
| `ActionRefused` verdicts of one action that differ between games (one applied it, another refused it) | at once |

Rounds are judged on their non-money parts, so `money` never hides another difference. A decision resets the counters. **Cooldown:** a decision made less than 120 s after the end of the previous round (finished or failed) is never dropped: it is remembered (one at a time, the first reason) and the repair starts at the first host tick after the cooldown (`ChecksumPolicy::due`, reason `<reason> (waited for the cooldown)`); a full round in which every part was measured and no non-money part differs cancels it (the worlds match). A resync round that starts for another reason (the host's button, hot join) is the repair and forgets it. One repair runs at a time and at most one starts per cooldown, which keeps resync loops out. The cooldown applies to policy decisions only, not to the host's button or to hot join. Before 2026-10-23 a decision inside the cooldown was dropped: two real sessions stayed forked for 95 s and 8 min.

### Action verdicts

Every game sends its verdict of every build at its stamp: `action_applied` (applied; for a build injected from the originator's text, once the native module answered `injected`) or `action_refused` (refused, with the reason and its `class`: `inject_refused`, `decode`, `engine`, `company`, `module`). Both are kind 36; the launcher sets `applied: true` for the `action_applied` line. A refusal decided at the stamp from the game's own world is deterministic: identical worlds refuse an action identically, so a refusal alone asks for no repair (lab run 2026-10-18: a town replaced a building during a build's lead and every game refused the build at its stamp). The host keeps each action's verdicts (`VerdictBook`, at most 256 open actions) until every ready game has answered; as soon as two games answer differently the worlds have forked at that action and the policy asks for a resync at once (`verdicts of <from:seq> differ`, cooldown permitting). An applied verdict's `detail` is empty, except for a shared terrain height edit, where it is the game's proof of the heights after the apply (`terrain heights <cells>:<hash>`, a hash of `api.engine.terrain.getBaseHeightAt` over the edited cells); two applied verdicts with different details are a fork too (2026-10-25). A resync forgets every open action.
**Every round is full.** Since full rounds are spread over the steps (below), every regular round is a full one (`full_checksum_every` default 1; lab 2026-10-18: 2896 entities in 15 chunks of 200 every 3 steps, step max 6.0 ms against the 30 ms budget, no round over 54 ms). `ids` and every heavy lane are therefore compared in every round, and the policy's streak, drift and not-ready rules judge every part every round.

**No overlap.** The host asks for the next regular round only when the previous running round has been reported by every ready game. When the 5 s timer (`checksum_seconds`) fires while a round is still being read or reported, the next round waits and starts at the first tick after the last report: the interval is 10 s, or the round's own length when that is longer. The host logs at most once a minute `checksum: <k> round(s) waited for the previous one in the last <s> s (longest wait <w> s)`. A round that is not complete after `checksum_round_timeout_seconds` (60 s: a game left mid-round or reloaded) is dropped (`checksum round <n> dropped: not reported by <names> after <s> s`); its late reports are ignored. A resync's verification round drops a regular round still open from before the reload. Paused rounds and the verification round are asked as before and do not hold up the timer.

**The free-id queue part `freeq` (2026-10-21).** In the in-game session controller the native module adds `freeq` = `<epoch>|<n>:<h>|...` to each report the game writes: `<epoch>` is the low 32 bits of the hash of the ECS engine's free-id queue when the order trace first met the engine (equal on two games that loaded the same world), each `<n>:<h>` the rolling hash of the engine's id stream (ids handed out, ids joining the free-id queue, in order) after its `n`-th op, one every 1024 ops, the newest three. It is not a hash of the round's step (the native module has no step clock): the host compares it at the newest `n` every report names (`checksum::FREEQ_PART`), and leaves it out of the round when a game has no such part (an older game, the trace off, `FEVERLINK3_FREEQ_CHECK=0`), another epoch or no common `n`. Equal `n` with another hash means the next entity ids differ on the games before any build uses them; it is an ordinary part (two rounds in a row: repair).

**Light rounds (lab switch and older peers).** `FEVERLINK3_FULL_CHECKSUM_EVERY=<n>` with n > 1 gives back the old split: the heavy lanes (`ChecksumPolicy::HEAVY_PARTS`: `edges`, `constructions`, `nodeConfigs`, `townBuildings` with its `townBuildings:<town>` parts, `ids`) only in every n-th round (the first round, every paused round and the verification round of a resync are always full). The other rounds carry `full: false`; a game then leaves the heavy lanes out, and the host drops them from the reports of a light round anyway (an older game that sends everything is not a difference). The policy counts a heavy part only over full rounds: a light round neither grows nor resets a streak or a drift clock that consists of heavy parts only; any other part is judged every round as before.

**Spread full rounds.** A full round that is not paused carries `spread` (`ChecksumPolicy::spread_steps`: the game speed, 1..4 steps, one engine batch). At `at` every game hashes the light lanes and the `ids` lane and takes the entity lists of the heavy lanes; then it reads `L.CHECKSUM_CHUNK` (200) entities of those lists at each step `at + k * spread` (k = 1, 2, ...; at most `L.CHECKSUM_MAX_CHUNKS` chunks, a bigger world reads more per chunk). The chunks are cut by count, never by time, and each chunk step is a landing mark, so every game reads every entity at the same step: equal worlds give equal parts, and a world that differs (before the stamp or during the round) still differs. A heavy lane is the order-free sum of its entities' key hashes. The report carries `time = at`; a game that read a chunk after its step (a batch that missed its mark) reports `at` plus the lateness, so the host skips that round as `time_skew` instead of comparing different worlds. Actions are applied at their stamps as always while a round is read; a chunk read after them sees what they built, on every game at the same step. Light rounds, paused rounds and the resync's verification round are hashed whole (`spread` absent).

**Chunk size and detail (2026-10-19).** A report of a spread round carries `cost: {entityNs, stepMaxUs, chunk}` (the dearer of the costliest chunk's and the round's average time per entity on that game). The host sizes the next rounds' `chunk` from the dearest cost of each present game's last three rounds so that the slowest game's chunk costs about 12 ms of the 30 ms step budget (`ChecksumPolicy::chunk_for`: 50..4000 entities, at most twice the last one, a change logged as `checksum chunk: N entities a step from now on (...)`). Every game gets the same number, so the chunks still fall at the same steps and the comparison stays exact. After a compared round that differed, the next rounds carry `detail` until one is identical: such a round keeps every chunk's itemised keys in the game-script state (whichever simulation Lua state read it), so the `detail_s<sync>_<time>_<player>.txt` dump every game writes at the repair is complete and of the same chunks on every game (header `complete: detail round N`); without a detail round the dump is this state's own keys, said `partial`.


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
new connection  Join { name, version, proof, resume: { id, mac, last? } }
                mac = HMAC-SHA256(token, "FeverLink3 resume" ‖ nonce ‖ u16 id ‖ UTF8(name) [‖ "last" ‖ u64 last])
host            token valid, slot held (or still bound to a connection the host has not noticed is dead) →
                Welcome { you: id, ..., resume: { token, epoch: epoch + 1 }, resumed: true, catchUp? }
                caught up (see Catching up): the messages it missed, PlayerJoined to the others, Permissions to the
                returner; it stays ready and catches up as a stalled game, no round
                otherwise: PlayerJoined to the others, Permissions to the returner, and in a started session a hot-join
                round (reason `player returned`); the returner is not ready until it says Hello after loading the save
                anything else → a plain join, a new id, `resumed` left out
```

- The **window** is `reconnect_window_seconds` (330 s: the launcher retries for 300 s, plus a margin). After it the slot is released and the player is gone; `0` holds nothing.
- The token never travels again: the claim is an HMAC over this connection's challenge nonce, so a captured Join is worth nothing on another connection and the host's `bad-proof` check fails first. A claim that does not hold (the slot expired, was released, or the id and token are not a pair this host gave) is not an error: it is a plain join with a new id, and a guess leaves the real slot alone. A host started again has a new secret, so a player of the old session fails the proof (`bad-proof`) and nothing carries over: **a session key never outlives its host**, which is also what keeps the `(sender, counter)` nonces unique across hosts.
- **Replays.** The welcome carries the nonce of the challenge it answers (`nonce`); a player drops a welcome that names another, so a welcome captured on an earlier connection and played again cannot hand it an id and an epoch whose counters were used before. A player also answers one challenge per connection, and drops any host frame whose counter is at or below the welcome's: the host seals one counter stream, so such a frame was sealed before this connection began (a fresh replay window cannot know that). A host that predates `nonce` sends none, and the check is then skipped.
- A return takes over a connection the host still believes in (a pulled cable or a sleeping PC is noticed late): the old one is closed, the others see PlayerLeft(away) and PlayerJoined.
- **Frame counters.** The sender id is reused, so its counter must not start again at 1: nonce reuse under one key breaks AES-GCM, and the host's replay window would refuse the frames. The counter of a connection starts at `epoch << 40`; the epoch is 0 for the first connection of an id and one more for each return (at most 2²⁴ − 1), so every nonce is used once and the first frame of the new connection is newer than anything the old one sent. The player's launcher keeps `{id, token}`; the epoch comes in the Welcome.
- The game never sees the token: the launcher removes `resume` from the welcome it writes to `to_game.log`. It does see `resumed: true`.
- A player that stops on purpose sends Leave first (below), so the slot is not held.

### Catching up

A short drop (a Wi-Fi blip, a Steam relay hiccup) does not reload the returner: its game kept its world, so it is given exactly what it missed and runs on from there (`core/src/session/resume.rs`, `HostSession::resume`, `begin_catch_up`). One mechanism, the same for every message kind:

- **What each end keeps.** The host keeps every message of the game's stream it seals for a player (everything but the handshake, Ping, Kicked and a save's chunks and load) with the frame counter it went out under, and, while the player is away, every message it would have sent it. A Pong answers a Ping over one ordered connection, so the player has everything sealed for it before that ping, and the host drops it. The player's session layer keeps every message its game sends (all but Pong and Leave) with its counter until a Ping's `ack` (the last counter the host received from it) covers it, and, while it waits for a welcome, keeps what its game sends unsealed. At most 32 MiB each way.
- **The claim.** A returning launcher hands the old session's last received host frame counter and its kept messages to the new session (`PlayerSession::carry_to`). The join's `resume.last` is that counter, covered by the mac, so nobody on the path can make the host skip frames. A launcher that kept nothing, or had to drop something unacknowledged, sends no `last`.
- **The host catches up** when `last` is given, `HostOptions::resume_grace_seconds` (60 s; 0 = off) has not passed since the drop, no resync round is running or ran while the player was away (the others' world was replaced), the returner had said hello in the session's world, and the host still keeps every frame after `last`. The Welcome then carries `catchUp` = the last counter it received from this player, and the missed messages follow it at once, in their first order, sealed anew. The player resends every kept message after `catchUp`, in order. Nothing is skipped and nothing arrives twice.
- **Lockstep stays exact.** The returner's game stood still while the link was down (the launcher told it `started = false`, and no new promise reached it). Every missed action is stamped at or after the promises it read before the drop, so it reaches the game before its stamp. In a started session the returner is a stalled game from the first moment (see the stall timeout): the host keeps a promise for it that follows the games that run (they run on), its game catches up with the mod's catch-up, every action it ships is host-stamped or refused on every game if its stamp lies before that promise (an action resent from before the drop included: never applied late), and the stall ends when its own promise reaches the host's. No step is skipped or changed.
- **The check.** The first compared checksum round the returner reports decides: the same world (`resume: <name> caught up and the worlds match`), or a repair at once (`resume: <name> caught up but the worlds differ ... repair`), without waiting for the policy's second round.
- **Otherwise** (away longer than the grace, a resync meanwhile, no `last`, frames no longer kept) the welcome carries no `catchUp`, the player drops what it kept, and a started session reloads it with the hot-join round as before (`resume: <name> cannot be caught up (<why>): its game gets the host's world`). The host logs `resume: <name> is caught up: N message(s) ...` when it does; the player's launcher `resume: caught up by the host; N message(s) of the game ... are sent again`.
- **The window** shows "Reconnecting (N s)" while the launcher retries (`reconnectingS` in the controller's status).
- **Not proven** over a real network or Steam relay; the tests cut a loopback TCP link (`launcher/tests/reconnect.rs`) and the in-memory harness (`core/tests/resume_tests.rs`).

## Leave and removal

- **Leave** (38, player → host, sealed): the player goes on purpose. The host closes the connection, frees the slot at once and broadcasts PlayerLeft without `away`.
- **Kicked** (22, host → player, sealed): the host ended the session of this player. Reasons: `kicked` (the host's `kick`), `flood` (Limits), `ended` (the host stopped hosting or its world was closed: `HostSession::end_session` sends it to every joined player, and an unsealed Reject with the same reason to a connection that never joined, before it closes them all). The player's session layer reports it like a Reject (`PlayerOutput::Rejected`), so the launcher does not reconnect. Sealed, because an unsealed Reject is only honoured before the Welcome. The player's slot is **not** held: it may join again with the code like anyone else.
- **End of session** is the host's call too (`HostSession::end_session()`, made when the host session stops): without it a player only sees the connection drop and tries to reconnect to a host that is gone for the whole five minutes.
- **Kick** is the host's call (`HostSession::kick(id)`): a connected player is sent Kicked, disconnected and removed; for a player who is away it releases the held slot; the host's own id is refused.

## Ping and round-trip time

Every `ping_seconds` (2 s) the host sends each connected player `Ping { n, rtt? }`. The player's session layer answers at once with `Pong { n }`; neither reaches the game. The host measures `now − time the ping n was sent` on its own clock, keeps up to 8 unanswered pings per player (an older unanswered one is forgotten when a later one is answered) and smooths the samples: the first sets the value, each later one moves it a quarter of the way. `rtt` in a Ping is the smoothed value in milliseconds, one decimal, of every connected player the host has measured, so every player can show everyone's round-trip time. The host's own game is 0. A Pong with an `n` that was not sent, or was already answered, is ignored.

## Steam invites

Implemented in `core/src/admission.rs` (token, messages, the host's `Gate`) and `launcher/src/steam_admit.rs` (the
exchange over Steam); the Steam side is `docs/STEAM_TRANSPORT.md`, section 14. A pasted join code works as before.

**Connect token.** What Steam carries: `+feverlink3_join <payload>`, payload
`1:<host steamID64, 17 digits>:<session, 16 hex>[:<ticket, 16 hex>]`. `1` is the token format; a reader that finds
another number says "update FeverLink3". `session` and `ticket` are 64 random bits each, new for every hosted session.
The token without the ticket is public: it is the host's rich presence `connect` value (every friend sees it). The
overlay invite dialog carries the token with the ticket. **Neither ever holds the secret, the password or the key.**
At most 255 bytes (Steam's limit for a rich presence value); a token is about 75.

**Admission exchange.** The friend's game connects to the host's Steam ID on virtual port 3; Steam authenticates both
accounts and encrypts the connection. One Steam message each: `FL3A` then UTF-8 JSON, at most 2048 bytes.

```
friend → host   request   { session: hex16, ticket?: hex16, name, version }
host → friend   waiting   {}                            the host's player is asked
host → friend   admitted  { key: hex of the 32-byte session key }   then the host closes
             or refused   { reason }                    then the host closes
```

The host decides on the account Steam reports for the connection, never on the name in the request:

- `session` is not this session's: `refused stale`.
- `ticket` equals this session's ticket (the friend came through the host's overlay invite), or the account was
  admitted earlier in this session: `admitted` at once.
- Otherwise the request waits for the host's player (`waiting`; the window lists it): `admit` sends `admitted`,
  `refuse` sends `refused refused` and refuses further requests of that account for 60 s; nobody answers within 120 s:
  `refused timeout`; more than 8 waiting: `refused busy`; the host stops: `refused ended`. Unreadable: `refused
  malformed`. A connection that sends nothing for 15 s is closed; at most 16 admission connections at once.

The admitted friend joins the session on virtual port 0 with the key it received, through the normal handshake. The
key, not the secret, crosses: the friend never learns the secret or the password. Steam's friend list "Invite to game"
carries the public token (rich presence), so such a friend is asked for like a "Join game" click.

## Bug reports

The window's "Report a problem" makes ONE zip that holds the logs of every game in the session, so a player can attach it to a GitHub
issue (`core/src/session/report.rs`, `launcher/src/report.rs`, `launcher/src/live/report.rs`). Nothing is sent to the project by
itself; the parts travel only between the session's games, like the world does.

- **Asking.** Any game asks: the host's own window calls `HostSession::request_report`, a player's launcher sends `ReportRequest
  {note?}` (cut to 300 characters). One report at a time: a request while one runs is refused at once with `ReportBusy {id, byName}` to its
  sender only (the host's own window gets `HostOutput::Report(Busy)`). Its window says "A bug report is already being made; it
  will reach <byName>." and shows the button again; it does not wait for a bundle.
- **Collecting.** The host numbers the report (`id`, the transfer id, from 1), picks a name of six random hex digits (`report`) and a
  random `salt`, and sends `ReportAsk` to every connected player; its own launcher gets the same ask as `HostOutput::Report(BuildPart)`.
  `maxBytes` is the bundle's limit (24 MB less 256 KiB) shared equally by the games. Every game but the one that asked gets a notice:
  a player "<name> made a bug report; your logs were sent to the host.", the host's own game "<name> made a bug report; your logs are
  in it.".
- **Parts.** Each game builds its part on its own PC: its logs redacted (below), cut from the front to `maxBytes`, zipped. A player's
  launcher sends it as `ReportPart` chunks of transfer `id`, exactly like a repair source's save (`ResyncUpload`): 256 KiB chunks
  with the whole part's SHA-256, a few per turn of its loop, checked and assembled at the host by the same `IncomingSave`. The host
  lets them past its flood limit only while it waits for that game's part, and refuses a part announced bigger than `maxBytes`
  before it holds anything. The host's own part comes from its launcher (`HostSession::report_part`).
- **Timeout.** A game that has sent nothing within 60 s of the ask is missing; one whose part is on its way has 180 s. A game that
  leaves is missing at once. The report completes anyway: `HostOutput::Report(Collected)` lists every game's part or the reason it is
  missing.
- **The bundle.** The host's launcher writes `FeverLink3-report-<YYYYMMDD-HHMMSS>-<report>.zip` (UTC): `report.txt` at the top
  (report id, time, who asked, the note, the players, the missing parts and why, what was cut), then one folder per game, `player-1`
  the host and `player-2`... in id order. When the host asked, it goes to the host's Desktop; when a player asked, the host keeps a
  copy in `Documents\FeverLink3\reports` and sends it to that player as `ReportBundle` chunks through the connection's paced
  outbox (the save transfer's, `launcher/src/session/transfer.rs`). The player who asked saves it to its Desktop (else
  `Documents\FeverLink3\reports`), opens its folder and a prefilled GitHub issue. A player that does not get the bundle within 5
  minutes (or whose session ends) makes a report of its own logs alone and says so in it. Outside a session the button makes a
  report of this game alone, the same way.
- **Redaction** (on the PC that wrote the logs, before anything is zipped or sent; `launcher/src/report/redact.rs`): join codes are
  removed; a Steam ID (17 digits, `76561...`) becomes `steam-<8 hex>`, a hash with the report's salt, the same in every part of one
  report; each part names its own game's tag (`selftag.txt`), and the host's bundle replaces it with that game's `player-N` (the salt
  stays out of the bundle); Steam account numbers (`userdata\<n>`, the game's `userId`) become `%STEAMACCOUNT%`; IPv4 and IPv6
  addresses become `<ipv4>` / `<ipv6>` (loopback stays); the Windows user name becomes `%USER%`.
- **Old peers.** An older player ignores `ReportAsk` (unknown kind): it is listed as missing after 60 s. An older host drops
  `ReportRequest`: the player's window falls back to its own logs after 5 minutes. An older player ignores `ReportBusy` and
  falls back the same way. `ReportBusy` is for the launcher like `ReportAsk`: never held for a repair's load.

## Limits

- **Frames per second.** Every connection has a token bucket: `max_frames_per_second` (500) refilled per second, `frame_burst` (2000) held at most; every frame received on the connection takes one token, whatever it is (an unintroduced connection included). The frame that finds the bucket empty is dropped and counted and the connection is ended with reason `flood` and detail `more than N frames per second`: an unsealed Reject before the join, a sealed Kicked after it. The player loses its slot. A game that sends a clock line per rendered frame (240 a second) and a long road's worth of actions at once stays far inside; `0` turns the limit off. The host's own game is never limited. Over TCP the refusal is best effort: a peer that keeps writing while the host closes may see a reset instead.
- **Bytes per second.** The same bucket counts bytes beside frames: `max_bytes_per_second` (2 MiB) refilled per second, `byte_burst` (8 MiB) held at most; a frame takes its header and body (a frame bigger than the burst takes the whole burst). Over the limit: the same `flood` refusal, with detail `more than N bytes per second`. A player's traffic is small (actions, clocks, reports), and the limit is what stops 480 KiB actions at 500 a second from filling the host's disk; `0` turns it off. At most 32 connections wait for their Join at once (`HostSession::MAX_PENDING`); the next is closed on arrival.
- **Silence.** A joined connection from which nothing arrived for `idle_timeout_seconds` (30 s) is closed by the host; its slot is held like any dropped connection's. Pings keep a healthy one far inside the limit, and any frame counts, so a game that only sends clocks needs no pong. Turning the pings off turns this off, since a quiet lobby is not a dead connection. 
- **A host that was away.** A call into the host more than 5 s after the previous one means the host itself was stalled (its process suspended, its thread stuck in a write to a peer that stopped reading), and the frames of every connection are read in one go afterwards. The host then forgives the stall: nobody is timed out for the silence, the frame budgets are full again and not enforced for as long as the stall lasted (at most 60 s) while the backlog is read, the slots held for players who dropped keep their whole window, and the pings sent before it are forgotten, so no round-trip time measures it.
- **Reports.** A ChecksumReport counts only from a game that said Hello since its last load; one from a game that did not (a returner still running its old world) is ignored, logged once. A report that completes a round is not the only way a round completes: when the game a round was waiting for leaves, a round that has every other report is completed then.
- **Handshake.** A connection that has not completed the handshake `handshake_timeout_seconds` (10 s) after it was accepted gets Reject `timeout` and is closed.

## Not yet specified

Companies. It gets a section here when its state machine lands in Core.
