# Wire protocol

Protocol version 1. Implemented in `src/FeverLink3.Core` (`Protocol/`, `Crypto/`, `Session/`); every rule here has a test in `tests/FeverLink3.Core.Tests`.

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
| Hello | 30 | game → host | `{time, save?}` |
| Clock | 31 | game → host | `{t, speed, ahead}` |
| SpeedRequest | 32 | game → host | `{speed}` 0 = pause |
| ChecksumReport | 33 | game → host | `{n, time, parts: {name: hash}, authority?: {name: number}}` |
| SaveDone | 34 | host's game → host | `{id, ok, detail?}` |
| ReplayFailed | 35 | game → host | `{detail}` |
| ActionRefused | 36 | game → host | `{seq, detail}` |
| Action | 50 | game → host → all but origin | `{stamp, seq, body, from}` (`from` set by the host) |
| Chat | 51 | game → host → all but origin | `{text ≤ 500, from, name}` (set by the host) |

A player may send kinds 30 to 36, 50 and 51. Anything else from a player is dropped, counted and logged once. The host's own game is player 0 and is trusted like the host.

## Session rules

- **Session state** is broadcast on every change and at least every 3 s. `lead` is `200 × (4 + 2 × speed)` game-time units: what a game adds to its clock when stamping a local action.
- **Clock**: a game reports `{t, speed, ahead}` regularly. The host extrapolates `t + elapsed × speed × 1000` and relays it as PeerClock. Every game holds (speed 0) when it is more than `ahead` steps past the slowest peer.
- **Stamps** are multiples of 200. A future stamp chosen by the host is `roundUp(maxClock + lead)`.
- **Start**: `started = true`, `speed = 0`, `pauseAt` = the games' common time when they agree, and a checksum round is requested at once.
- **Pause** is a stamp: `pauseAt = futureStamp`; every game stops at exactly that step. **Speed** 1..4 clears `pauseAt`.
- **SpeedRequest** from any player is obeyed while the session is started and the value differs (host permissions come later and will gate it here).
- **Checksum rounds**: every 10 s while running, the host sends `{n, at, paused}`. When `paused`, `at` is the latest time any game stopped at. Every game that said Hello must report; the round completes when they all have, compares the per-part hashes, and reports the differing parts with each game's value. Values from the host's report under `authority` are relayed to the players as Authority, which they apply to themselves (money correction). Rounds older than 20 are forgotten.
- **Actions** are relayed in the order the host received them, to every game but the origin, with `from` filled in by the host. The body is the mod's own serialisation and is opaque here.
- **Leaving**: a closed connection removes the player and its clock and broadcasts PlayerLeft.

## Not yet specified

Resync round orchestration (who waits for what, timeouts), hot join, permissions, companies. Each gets a section here when its state machine lands in Core.
