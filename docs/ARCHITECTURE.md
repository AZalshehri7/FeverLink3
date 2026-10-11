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
 |   +-- Lua mod                  |   sealed frames over Steam  |
 |        stamp, replay, checksum |<---------+                  |
 |              ^  files          |          |                  |
 |              v                 |          v                  v
 |  FeverLink3.exe (host)          |<---- FeverLink3.exe (player) ...
 |   lobby, relay, resync         |
 +--------------------------------+
```

| Component | Responsibility | Must not |
|---|---|---|
| **Native module** (Rust) | Loaded by the game via a DLL it imports. Hooks the engine's command list to capture tool commands before they apply, hold them until their stamp, and release them. Applies determinism patches (single-worker script pool). Terrain cells are not copied yet (the hook refuses them). | Open sockets. Decide anything about the session. Run when no FeverLink3 session is active. |
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

Topology: a host-relayed star (every player connects to the host). Transport: the players' transport is Steam's peer-to-peer networking, which carries these frames between the games (the listener and a one-process connection test exist; carrying a whole session over it is not finished, `docs/ROADMAP.md`). Plain TCP is the development and test transport that the tests and the `host` / `join` commands use. The frame format does not depend on the transport.

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
- **Stamp.** A local action is stamped `now + lead`, where lead covers the one-way network time to the slowest peer, the file hand-offs and a game frame, one step for the script event that carries the action into the simulation state, and one step of margin, counted in steps at the current speed (`Lockstep::lead_for`: 3 steps on a good connection, about half a second at normal speed, longer for a slow link or a higher speed). The host measures the round trips with its pings, supplies the current lead with the session state and shortens it only after it stayed lower for eight seconds; a game may raise it, never lower it.
- **Barrier.** No game runs more than *window* steps ahead of the slowest peer. A game that reaches the barrier sets speed 0 until the peer catches up. Pause is a stamp too: every game stops at exactly that step.
- **Ordering.** Actions at the same stamp apply in (stamp, sender id, sequence) order on every game.
- **Late actions** (stamp already passed when received) are applied at once and logged `LATE`; a late action at the host triggers a checksum at the next common stamp.

## 6. Replication

Three modes, from TpF2 Multiplayer, chosen per action kind:

| Mode | Where the originator's game applies it | Used for |
|---|---|---|
| **Strict** | Held by the native module until the stamp. At the stamp the originator's game releases its own held command (the engine applies it, the tool's callback runs and the tool stays alive) and every other game replays the build. Nobody's world runs ahead. | Construction tools, bulldozer, vehicle purchase, line edits, terrain. Built so far: the construction tools and the bulldozer, in the mod (not yet run in the game, see below) |
| **Replay on peers** | Natively at the click; peers apply a few steps later. | Actions the native module cannot hold without crashing the tool |
| **Poll** | Natively; the mod detects the change by scanning and ships it. | Renames, colours, settings with no command hook |

Rules: never cancel a command that nothing will replay (solo play stays stock); never cancel a command that cannot be fully decoded (losing the player's action is worse than a visible divergence); the wire never carries engine entity ids.

**Strict mode for the native tools, as the mod does it** (`docs/NATIVE_LINK.md` section 7; follows TpF2 Multiplayer, MIT). The mod's interface half watches `native_events.log`. When the module reports the `command_list_add` hook, the mod answers its ping, writes `hold_on` for `build_proposal` and pings every 2 s. A construction tool's confirm click then produces two things: the engine's tool event `builder.proposalApply`, which the mod encodes into a build record (the same record the Lua path ships), and the module's `held` event for the tool's command. The mod pairs them (oldest capture, within 2 s), checks that this game could rebuild the record, ships the action with a stamp and leaves the command **held** (the first design cancelled it and replayed the build on the originator too; in the first real two-game run the tool was dead after one build, because a cancelled command never gives the tool its "applied" callback). The item goes to the originator's own simulation queue marked with a local `heldId` (never on the wire). At the stamp the simulation half does everything it does on a peer (decode, company check, ledger, snapshot of the entity lists, binders and watches that key what the build creates) except sending the command, and reports a `#release` result; the interface half then writes the replay mark, `release` and a trigger, so the engine applies the tool's own command a moment after the stamp, the tool gets its callback, and `onPreBuildProposal` finds the replay mark and is not captured as a new build. A refusal at the stamp (the rule that refuses it on every game) cancels the command instead. If the module gives a held command back before the stamp (its silence rule, its 30 s age, `hold_off`), the simulation half is told (`heldout`/`heldgone`) and swallows the early build's capture, so it is not shipped twice. A game paused at the stamp releases at once and finishes its bookkeeping when the module reports `applied`. **Not run in the game yet**; the originator applies its copy slightly after the stamp (the interface frame that sees the result, the trigger and the next tick), which the first real run has to measure ('released at game time' / 'applied by the engine' lines of mod.log). Whatever does not fit is released, and the build goes the old way (replay on peers: the engine applies it at the click, the simulation half captures it from `onPreBuildProposal`, the peers rebuild it at the stamp). Strict mode is therefore optional and switches itself off: no module, a module without the hook, a reload (`hold_off` first), the end of the session, silence, a fault. Nothing in solo play changes.

Entity references: roads as node positions (match within 1.5 m, else split edge or create node); constructions as file + transform + parameters; vehicles and lines as `<sender>:<seq>` keys bound on every game when the entity appears, or `s:<id>` for entities from the savegame. Whatever a game resolves from a key is checked (it exists, it is of the expected kind): an action that fails the check is refused, never applied to whatever shares the id. Depots and station groups also carry the building that owns them and their place in it, because their own id is not bound to a key (`docs/COVERAGE_OPS.md`). Where ids come from: both games load the same save, so ids of what the save holds are equal; every id allocated afterwards is not (the engine reuses freed ids, so even order is no guide: a guest bought vehicles 106787, 106694, 106821 where the host got 101856, 106758, 106759), which is why nothing created in a session travels by id. Two details keep a key from going wrong. A window command that names an entity another window command made a moment ago (a vehicle bought and put on a line within a second) carries the placeholder `ui:<record>` from the UI hook, and the bridge turns it into the key of that record when it ships the command, since the bridge's table of keys reaches the hook only about half a second later; without it the entity travelled as `s:<id>`, an id the other game does not have. An action that names an entity a not-yet-finished action created (the engine shows a new entity a step after the command) waits for it a few steps instead of being refused. And a game that could not replicate something (a native build, an action whose entities were not all found) keeps a short note of it and ships the recent notes with its next actions (`body.diverged`); a refusal on another game that is an unresolved reference then quotes them, so the log names the first divergence and not only the reference that came out wrong.

## 7. Determinism

The engine is deterministic given identical commands, with three known exceptions (the first two both prior projects hit, the third is TF3's own):

1. **Game scripts run on a thread pool** whose iteration order depends on timing on CPUs with 8 or more threads. Fix at the engine level: the native module gives the script update its own single-worker pool, as the engine itself does on small CPUs. Rendering and simulation keep every core.
2. **`math.random` in game scripts** is seeded from wall time. The mod replaces it with a deterministic generator reseeded from game time each step.

3. **Town growth** is not deterministic across two games (measured in the real game, 2026-10-10: two games resynced to one world and left running for 90 s end with other building variants and levels in the same places, a house on one side only, and a street segment more on one side). It does not come from the game scripts (the XP part of the growth script seeds its random numbers from game time), and neutralising the pollution and noise ratings did not stop it. A build through a town (a road, a station) is rebuilt on the other games from the buildings it removes, so towns that differ make builds fail and the money differ. TpF2's engine grows towns identically; MPFever lets its resync repair the difference. FeverLink3 keeps the full game: town growth is never switched off and no rating is changed. What is known about the cause (2026-10-11, details and evidence in `research/TOWN_GROWTH_FINDINGS.md`): town growth is native code (the game's `TownDeveloper`), not a script. Its random number generator (a `std::mt19937`) is seeded from the ninth stack argument of the engine function at RVA `0x8d4270` (build 40420), when the first world of a process is loaded; a resync load does not call that function again, so after a resync the generator carries on from whatever state it has. In one measured run the seeds of the two games were different and constructions drifted about 10 s after each of three resyncs. In a second run the seed was forced to the same value in both games (an experiment switch, `NATIVE_LINK.md`): constructions then stayed identical for about 13 checksum rounds, and the first difference came at round 14, in balance, companies and lines while the player was building (stations and stops were not replicated at that stage). That is a hint, not a result: why the original seeds differ is **not** established (they may come from the save), and a forced seed has not been shown to hold over time. The next step is to reset the generator's state to a common value at every sync point; that analysis is in progress (`research/TOWN_RNG_STATE.md` in another branch). Whatever is found, FeverLink3 will not disable or change a game mechanic for this: no growth freeze and no change to the ratings.

Known residual drift (vehicle positions, passengers) is tolerated: it is compared, logged, and only repaired when it lasts for minutes.

## 8. Checksums and resync

- Every 10 s of real time the host names a future stamp; every game hashes its world at that stamp and reports per-part hashes (money, loans, company, constructions, lines, time, town buildings, stations, edges, node configs, vehicles, names and a few more; industries, cargo and terrain edits are not hashed yet).
- **Town buildings** (read-only; the idea is MPFever's `townBuildings` lane): `townBuildings` is `<count>/<hash>` over every `TOWN_BUILDING`; `townBuildings:<town id>` is the same per town (the first 48 towns), so the log names the town whose growth differs and shows the building counts on each side. A building enters the hash by model file, position (0.1 m), level, depth, parcel count and build time, never by entity id. Not a drift part: it reloads after two consecutive differing checks, like constructions.
- **Lanes the script computes today** (`L.checksumParts`, one small function each, registered by one `lane(...)` line; a lane whose engine accessor is missing is simply absent): `time`, `balance`, `loan`, `constructions`, `lines`, `companies`, plus the wider ones from MPFever (MIT, `mpfever_refs.lua` `contentHash`):

  | Lane | What is hashed (sorted, no entity ids, positions rounded to 0.1) | Policy |
  |---|---|---|
  | `stations` | per station group: stations, terminals, positions of the terminals' stop nodes | reload after 2 rounds |
  | `edges` | per road/rail edge: endpoints in canonical order, street type, type index, road type, template, style, decoration count, models of the edge objects (signals, stops) | reload after 2 rounds |
  | `nodeConfigs` | per node: position, lane connections, crosswalks, traffic-light preference and type, double slip | drift |
  | `vehicles` | per line (name and stop count): vehicle count and sorted coarse states (state, stopped by the player, sell on arrival); no positions, no ids | drift |
  | `names` | sorted names of station groups, lines, towns, players | reload after 2 rounds |

  Cost per round is one component read per entity (station groups and their terminal nodes, road edges and nodes and edge objects, vehicles, named entities) plus a sort and one hash; roads dominate, so a large map is tens of milliseconds, in the simulation script. Vehicle ids are left out on purpose: ids are not guaranteed equal between games and a permanent false difference would reload every 5 minutes. The launcher log names the lanes ("checksum 12 differs in stations, vehicles (drift)").
- **Money** is corrected in place from the host's values. **Drift parts** reload only after 5 minutes. **Everything else** reloads after two consecutive differing checks, or at once when a replay fails.
- **Resync**: pause at a stamp → wait for all clocks to stop → host saves under a unique name → SHA-256 → stream to players → every other game loads it (the host's own game keeps playing its live world; `host_reloads` turns the host's reload back on) → paused checksum must match → resume at the previous speed. A joiner goes through the same round.

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
- **Code signing** is the only thing that removes the first-run warning. The SignPath Foundation's free programme needs an OSI-approved licence and the project is under the PolyForm Noncommercial License, so it probably does not qualify; the paid routes (a Certum certificate, SignPath's paid service) and the trade-off are in `docs/SIGNING.md`.
- **No injection.** The proxy DLL is loaded by the game itself. It is small, unobfuscated, and documented in plain words (`docs/SECURITY_FOR_USERS.md`), because antivirus heuristics and players' trust both depend on that.

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
- A session ends with the world it lives in: the controller watches `from_game.log` (the mod writes to it every half
  second while its script runs) and ends the host or player session when the game that had spoken goes silent for 8
  seconds, except during a resync reload. A host that stops tells its players (`Kicked`, reason `ended`) so they end
  theirs at once. `docs/LIVE_CONTROL.md`, "When the world is closed".
- The Lua UI state has no `io` global; the mod takes `package.loaded.io` (finding from MPFever, MIT).
- Verified in game build 40420 on 2026-10-09: the button and window appear, "Host a game" opens the listener inside
  the game process and the window shows the join code. Joining from a second game, saves and the native hooks are not
  yet verified.
- Panics: the workspace builds with `panic = "unwind"` (the native hooks must let the game's C++ exceptions through,
  `docs/NATIVE_LINK.md` section 10), so a panic in the controller or a session thread is contained by
  `launcher::guard::guarded` (a log line, a stopped session) and one in a hook by `native::log::guarded` (the game's
  call goes on). The panic hook writes message, place and a backtrace to `native.log` first. See `docs/SECURITY.md`,
  section 5, for the audit.
