# FeverLink3 architecture

This document is the design the code must follow. When code and document disagree, one of them is a bug; fix it and say which.

## 1. Model

**Deterministic lockstep with host authority.** Every game runs the same simulation from the same savegame. Only player commands travel. A command is applied at the same simulation step on every game. The host's game is the reference: checksums are compared against it, and a lasting difference is repaired from its savegame.

Why not alternatives:

- *State synchronisation* would need to ship the world. A Transport Fever world is hundreds of megabytes of entities updated every step. Impossible at the engine's size and with no engine support.
- *Screen sharing* (Steam Remote Play, Parsec) gives one camera and one cursor. Not multiplayer.
- *Turn-based save passing* is not simultaneous play.

Lockstep is the only model that fits, and it is the one both prior projects (TpF2 Multiplayer, MPFever) converged on independently.

## 2. Components

```
 Player PC                                            Player PC
 +--------------------------------+                   +---------------------+
 |  Transport Fever 3             |                   |  Transport Fever 3  |
 |   +-- native module (C++)      |                   |   ...               |
 |   |    capture, hold, release  |                   +---------------------+
 |   +-- Lua mod                  |     sealed TCP              |
 |        stamp, replay, checksum |<---------+                  |
 |              ^  files          |          |                  |
 |              v                 |          v                  v
 |  FeverLink3.exe (host)          |<---- FeverLink3.exe (player) ...
 |   lobby, relay, resync         |
 +--------------------------------+
```

| Component | Responsibility | Must not |
|---|---|---|
| **Native module** | Loaded by the game via a DLL it imports. Hooks the engine's command list to capture tool commands before they apply, hold them until their stamp, and release them. Applies determinism patches (single-worker script pool). Copies data a script cannot read (terrain cells). | Open sockets. Decide anything about the session. Run when no FeverLink3 session is active. |
| **Lua mod** | Game-script side. Stamps local actions, ships them to the launcher, applies remote actions at their step, computes world checksums, saves and loads for resync, draws the in-game UI (players, chat, status). | Trust anything from the file link without checking the sender id the launcher attached. |
| **Launcher** (`FeverLink3.exe`) | Starts the game with the session environment. Hosts or joins a session. Owns the session clock, speed and pause. Relays commands in one order to everyone. Collects checksums and runs resync. Transfers savegames. | Interpret game commands. |
| **Core library** | Everything in the launcher that has no Windows, UI or game dependency: framing, crypto, join codes, session state machine, message types. Fully unit-tested. | Reference WinForms, the registry, Steam or the filesystem layout. |

The launcher and the game talk through files in a per-session folder, as the two prior projects do: it needs no process injection and survives either side restarting. Every line carries a monotonically increasing sequence number so a reader never re-dispatches history after a truncation.

## 3. Trust model

Threat: an outsider who learns the host's address, or a player who is not the host, must not be able to control the session.

- **The join code is the credential.** It carries a 16-byte random session secret. The session key is `HKDF-SHA256(ikm, salt = "FeverLink3 v1", info = "session")` with `ikm = secret`, or, when the host set a lobby password, `ikm = secret ‖ PBKDF2-HMAC-SHA256(password, salt = secret, 600 000 iterations)`. The stretching makes a leaked code without its password expensive to brute-force. (`src/FeverLink3.Core/Crypto/SessionKey.cs`)
- **Sealed frames.** Every frame is `AES-256-GCM(key, nonce, plaintext, aad = header)`. Nonce = 4-byte sender id ‖ 8-byte counter, never reused: the sender id is assigned by the host at join and the counter only grows. Receivers keep a 64-frame replay window per sender.
- **Host decides who speaks.** The host rewrites the sender id on everything it relays. A player may send only the kinds in the *player allow-list* (hello, clock, action, action result, checksum, speed request, chat). Everything else from a player is dropped and logged once.
- **Players listen to the host only** for control (session state, checksum requests, authority values, resync). Other players reach them only through kinds the host relays.
- **Savegames are verified.** A transfer announces the SHA-256 of the file; the receiver checks every chunk's bounds and the final hash before writing. A savegame is code (`.sav.lua` runs on load), so joining a session means trusting the host as you trust a mod author. Say so in the UI.
- **Limits.** No forward secrecy: the key lives for the session. A hostile *player* holds the session key and can forge frames as another player at the transport level; the host's sender-id rewrite and allow-list are what contain them. This is the TpF2 Multiplayer model and it is the right trade-off for groups of friends.

## 4. Wire protocol

Transport: TCP, host-relayed star (every player connects to the host). UDP hole punching and a relay server are later work; the frame format must not change for them.

Frame on the wire:

```
u32  length        (big-endian, of everything after this field; max 1 MiB)
u8   version       (1)
u8   kind
u16  sender id     (0 = host, assigned at join)
u64  counter       (per sender, nonce material)
[..] ciphertext + 16-byte GCM tag   (plaintext = payload)
```

Header bytes are the GCM additional data, so kind and sender cannot be altered without detection. Payload is UTF-8 JSON for control messages and raw bytes for savegame chunks. Message kinds and their payload schemas are listed in `docs/PROTOCOL.md` (to be written with the launcher).

The *only* unsealed frames are `join` (carries a proof the sender knows the key: HMAC of a server nonce) and the host's `reject`.

## 5. Lockstep

- **Clock.** Game time from the engine, in game-time units; one simulation step is 200 units. Every game sends its clock to the host regularly; the host relays it as a peer clock.
- **Stamp.** A local action is stamped `now + lead`, where lead covers the round trip to the slowest peer plus a reserve for the engine's batch. The host supplies the current lead with the session state; a game may raise it, never lower it.
- **Barrier.** No game runs more than *window* steps ahead of the slowest peer. A game that reaches the barrier sets speed 0 until the peer catches up. Pause is a stamp too: every game stops at exactly that step.
- **Ordering.** Actions at the same stamp apply in (stamp, sender id, sequence) order on every game.
- **Late actions** (stamp already passed when received) are applied at once and logged `LATE`; a late action at the host triggers a checksum at the next common stamp.

## 6. Replication

Three modes, from TpF2 Multiplayer, chosen per action kind:

| Mode | Where the originator's game applies it | Used for |
|---|---|---|
| **Strict** | Held by the native module, applied at the stamp like everywhere else. Nobody's world runs ahead. | Construction tools, bulldozer, vehicle purchase, line edits, terrain |
| **Replay on peers** | Natively at the click; peers apply a few steps later. | Actions the native module cannot hold without crashing the tool |
| **Poll** | Natively; the mod detects the change by scanning and ships it. | Renames, colours, settings with no command hook |

Rules: never cancel a command that nothing will replay (solo play stays stock); never cancel a command that cannot be fully decoded (losing the player's action is worse than a visible divergence); the wire never carries engine entity ids.

Entity references: roads as node positions (match within 1.5 m, else split edge or create node); constructions as file + transform + parameters; vehicles and lines as `<sender>:<seq>` keys bound on every game when the entity appears, or `s:<id>` for entities from the savegame.

## 7. Determinism

The engine is deterministic given identical commands, with two known exceptions both prior projects hit:

1. **Game scripts run on a thread pool** whose iteration order depends on timing on CPUs with 8 or more threads. Fix at the engine level: the native module gives the script update its own single-worker pool, as the engine itself does on small CPUs. Rendering and simulation keep every core.
2. **`math.random` in game scripts** is seeded from wall time. The mod replaces it with a deterministic generator reseeded from game time each step.

Known residual drift (vehicle positions, passengers) is tolerated: it is compared, logged, and only repaired when it lasts for minutes.

## 8. Checksums and resync

- Every 10 s of real time the host names a future stamp; every game hashes its world at that stamp and reports per-part hashes (money, loans, company, towns, industries, cargo, vehicles, roads, constructions, lines, names, node configs, terrain edits).
- **Money** is corrected in place from the host's values. **Drift parts** reload only after 5 minutes. **Everything else** reloads after two consecutive differing checks, or at once when a replay fails.
- **Resync**: pause at a stamp → wait for all clocks to stop → host saves under a unique name → SHA-256 → stream to players → everyone, host included, loads it → paused checksum must match → resume at the previous speed. A joiner goes through the same round.

## 9. Game builds

The native module recognises the game build by its PE timestamp and looks addresses up in one table per build. Every address lives in that table, nowhere else, and every patch verifies the bytes it expects before writing. An unknown build means: hooks off, mod runs in degraded mode, clear message to the player. The tooling that produces a table row from a game executable is published in `tools/` so anyone can add a build.

## 10. Testing

- `FeverLink3.Core`: xunit, every public type. Property-style tests for framing and sealing round trips.
- Lua mod: two mocked games running the real Lua through lupa, host relay mocked; checks same actions at same steps, id translation, pause, late actions.
- Launcher: protocol state machine tests with in-memory transports; a `--selftest` that runs host + player over loopback.
- CI runs all of it on every push. Game-in-the-loop tests are manual and documented.
