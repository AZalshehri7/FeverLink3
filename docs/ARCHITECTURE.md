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
 |   +-- native module (Rust)     |                   |   ...               |
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
| **Native module** (Rust) | Loaded by the game via a DLL it imports. Hooks the engine's command list to capture tool commands before they apply, hold them until their stamp, and release them. Applies determinism patches (single-worker script pool). Copies data a script cannot read (terrain cells). | Open sockets. Decide anything about the session. Run when no FeverLink3 session is active. |
| **Lua mod** (Lua) | Game-script side. Stamps local actions, ships them to the launcher, applies remote actions at their step, computes world checksums, saves and loads for resync, draws the in-game UI (players, chat, status). | Trust anything from the file link without checking the sender id the launcher attached. |
| **Launcher** (`FeverLink3.exe`, Rust) | Starts the game with the session environment. Hosts or joins a session. Owns the session clock, speed and pause. Relays commands in one order to everyone. Collects checksums and runs resync. Transfers savegames. | Interpret game commands. |
| **Core library** (`feverlink3_core`, Rust) | Everything in the launcher that has no Windows, UI or game dependency: framing, crypto, join codes, session state machine, message types. Fully unit-tested. | Reference the UI toolkit, the registry, Steam or the filesystem layout. |

The launcher and the game talk through files in a per-session folder, as the two prior projects do: it needs no process injection and survives either side restarting. Every line carries a monotonically increasing sequence number so a reader never re-dispatches history after a truncation.

## 3. Trust model

Threat: an outsider who learns the host's address, or a player who is not the host, must not be able to control the session.

- **The join code is the credential.** It carries a 16-byte random session secret. The session key is `HKDF-SHA256(ikm, salt = "FeverLink3 v1", info = "session")` with `ikm = secret`, or, when the host set a lobby password, `ikm = secret ‖ PBKDF2-HMAC-SHA256(password, salt = secret, 600 000 iterations)`. The stretching makes a leaked code without its password expensive to brute-force. (`core/src/crypto.rs`)
- **Sealed frames.** Every frame is `AES-256-GCM(key, nonce, plaintext, aad = header)`. Nonce = 4-byte sender id ‖ 8-byte counter, never reused: the sender id is assigned by the host at join and the counter only grows. Receivers keep a 64-frame replay window per sender.
- **Host decides who speaks.** The host rewrites the sender id on everything it relays. A player may send only the kinds in the *player allow-list* (hello, clock, action, action result, checksum, speed request, chat). Everything else from a player is dropped and logged once.
- **Players listen to the host only** for control (session state, checksum requests, authority values, resync). Other players reach them only through kinds the host relays.
- **Savegames are verified.** A transfer announces the SHA-256 of the file; the receiver checks every chunk's bounds and the final hash before writing. A savegame is code (`.sav.lua` runs on load), so joining a session means trusting the host as you trust a mod author. Say so in the UI.
- **Limits.** No forward secrecy: the key lives for the session. A hostile *player* holds the session key and can forge frames as another player at the transport level; the host's sender-id rewrite and allow-list are what contain them. This is the TpF2 Multiplayer model and it is the right trade-off for groups of friends.
- **Strangers and floods.** The host's port is open to the internet, so what an unauthenticated peer can cost is bounded (connections, a join deadline, a 4 KiB first frame, write timeouts) and so is what a joined player can make the host hold or relay (rates, sizes, clock values). The numbers and the review are in `docs/SECURITY.md`.

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

Entity references: roads as node positions (match within 1.5 m, else split edge or create node); constructions as file + transform + parameters; vehicles and lines as `<sender>:<seq>` keys bound on every game when the entity appears, or `s:<id>` for entities from the savegame. Whatever a game resolves from a key is checked (it exists, it is of the expected kind): an action that fails the check is refused, never applied to whatever shares the id. Depots and station groups also carry the building that owns them and their place in it, because their own id is not bound to a key (`docs/COVERAGE_OPS.md`).

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

The native module is written in Rust (`native/`): the hooking core is `unsafe` in any language, so the gain is in everything around it, which is pure, tested with `cargo test` on any machine, and compiled to a small stripped DLL. It recognises the game build by its PE timestamp and looks addresses up in one table per build (`build_table.rs`). Every address lives in that table, nowhere else, and every detour verifies the exact prologue bytes it expects before writing; the padding is written before the jump, and everything is written from `DllMain`, while the process is still starting and no game thread is inside the functions, so nothing can see half a jump. An unknown build means: hooks off, mod runs in degraded mode, clear message to the player. The tooling that produces a table row from a game executable is published in `tools/` so anyone can add a build. Beyond the build check, every piece is off unless an environment variable switches it on, is compared with the bytes actually in memory, is installed all-or-none from `DllMain` before the game runs the patched code, and falls back to pass-through (never to a rewrite of running code) when the mod misbehaves; see `docs/NATIVE_LINK.md`.

## 10. Testing

- `feverlink3_core`: `cargo test`, every public type. Property-style tests for framing and sealing round trips. The session state machines return their outgoing events from every call, so host and players are wired together in memory with a clock the test moves.
- Lua mod: two mocked games running the real Lua through lupa, host relay mocked; checks same actions at same steps, id translation, pause, late actions.
- Launcher: protocol state machine tests with in-memory transports; a `--selftest` that runs host + player over loopback.
- CI runs all of it on every push. Game-in-the-loop tests are manual and documented.

## 11. Distribution

No installer. A release is one zip: `FeverLink3.exe`, the proxy DLL, the mod folder and a README. The player extracts it anywhere and runs the exe.

- **Self-contained launcher.** A single Rust executable of a few megabytes, statically linked: no runtime to install on the player's PC, nothing to download first, and the same crypto crates as the core.
- **First run installs the game-side pieces**, nothing else: the proxy DLL into the game folder, the mod into the user mods folder, and the file that lets a Steam-started game find the launcher. An *Uninstall* button removes exactly those. No registry, no admin rights.
- **Reproducible releases.** Every release is a tag built by GitHub Actions; the zip's SHA-256 is in the release notes. Nothing is built on a developer's PC.
- **In-app updates.** The launcher checks GitHub Releases, downloads the zip, verifies an Ed25519 signature (public key embedded, private key kept offline by the maintainer) and replaces itself. Files the launcher downloads carry no mark-of-the-web, so Windows SmartScreen appears once per PC, at the very first run, and never for updates. The join code carries the host's version; a player on another version is offered the update before joining.
- **Code signing** is the only thing that removes the first-run warning. Apply to the SignPath Foundation (free for open-source projects) once 0.3 is playable; the CI builds and release history are their prerequisites. Paid certificates only if that fails.
- **No injection.** The proxy DLL is loaded by the game itself. It is small, unobfuscated, and documented in plain words in the README, because antivirus heuristics and players' trust both depend on that.

## 12. Live mode: the session runs inside the game

The terminal launcher is a development tool. A player installs, starts Transport Fever 3 from Steam and uses the
in-game window.

- `winhttp.dll` (the native module) starts `launcher::live::spawn()` on a thread when the process is the game and
  `FEVERLINK3_SESSION_DIR` is not set. The controller owns the same `run_host` / `run_player` sessions the launcher
  binary starts, so the protocol, the file link and the tests are the same code.
- The folder is fixed: `%LOCALAPPDATA%\FeverLink3\live`. The mod finds it without an environment variable. The
  controller clears it at every game start.
- The window (a button in the game bar, `GameBarInfoDisplayExtension`) appends commands to `ctl_cmd.log`
  (`host`, `join`, `leave`, `start`, `pause`, `speed`, `check`, `resync`) and reads the controller's answer from
  `ctl_status.txt`. The controller writes `identity.txt` (`role=idle|host|player`); the mod becomes active when the
  role is `host` or `player`, without reloading the game.
- The Lua UI state has no `io` global; the mod takes `package.loaded.io` (finding from MPFever, MIT).
- Verified in game build 40420 on 2026-10-09: the button and window appear, "Host a game" opens the listener inside
  the game process and the window shows the join code. Joining from a second game, saves and the native hooks are not
  yet verified.
- Panics: the workspace builds with `panic = "abort"`, so a panic in the controller or a session thread ends the game
  (after writing its message to `native.log`). Measured, `panic = "unwind"` makes the DLL 336 KB (38 %) bigger, more
  than the 150 KB allowed, so the profile stays and the panics that input can reach were removed instead; the thread
  roots already run through `launcher::guard::guarded`, which contains a panic the day the profile unwinds. See
  `docs/SECURITY.md`, section 5, for the table and the audit.
