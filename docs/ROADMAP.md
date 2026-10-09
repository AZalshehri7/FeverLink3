# Roadmap

Status as of 2026-10-10. The project started on 2026-10-09, so everything marked done below happened in those two days. A stage ends when its tests pass and two people have played it; no stage has ended its playable part yet.

## Where things stand

FeverLink3 is an early test build. It is not yet a playable multiplayer game. The line between "written and tested without the game", "run in the real game" and "proven between two real games" is kept sharp on purpose.

**Verified in the real game (Transport Fever 3 build 40420, 2026-10-09):**

- The mod loads, and its window opens from a button in the bottom bar.
- "Host a game" in the window starts a session inside the game process and shows a join code.
- A second client (a test client, not a second copy of the game) can join that session by its code.
- The mod can drive the game's own `app.saveGame` / `app.loadGame`, and after a reload it resumes its file link to the session.
- The Steam peer-to-peer probe listens inside the game.

**Proven on 2026-10-10:** inside one running game, the game opened a Steam peer-to-peer connection to its own Steam ID and 6 pings were answered.

**Not verified yet:** building together (command replication between two real games), savegame transfer between two real games, the native command hooks (not enabled in the code yet), joining from a second real copy of the game, a Steam connection between two different accounts, the relay fallback.

### Proof milestones

Each one is a thing that has to be seen working before the matching claim may appear in a README, a listing or a release note.

| # | Milestone | State |
|---|---|---|
| P1 | The mod loads in the real game and the window opens | verified 2026-10-09 |
| P2 | Host from the window, a test client joins by the code | verified 2026-10-09 |
| P3 | Save and load driven by the mod, file link resumes after the reload | verified 2026-10-09 |
| P4 | The Steam peer-to-peer listener starts inside the game | verified 2026-10-09 |
| P5 | Steam peer-to-peer to the game's own Steam ID, one process, 6 pings answered | proven 2026-10-10 |
| P6 | Steam peer-to-peer between two different Steam accounts on two PCs | being tested, first run still to come |
| P7 | Relay fallback connects two games when Steam peer-to-peer does not | being built |
| P8 | Two real games in one session, the host's world reaches the joiner and loads | not yet |
| P9 | A road built in one game appears in the other (command replication, Lua path) | not yet |
| P10 | Construction held and released at the same step by the native hooks | not yet |
| P11 | An hour of play with identical checksums and no resync, then two people have played it | not yet |

### How to read the markers

- `[x]` done and tested.
- `[~]` written and tested without the game, not yet verified in the game (or not between two real games).
- `[ ]` not done.

### Timeline

- **2026-10-09:** project skeleton and design; protocol core and its tests; session state machines; native module skeleton in Rust; Lua mod with lockstep core and a two-game lupa test; address tool and its first run on build 40420; launcher platform layer; release pipeline and update verification; resync round, checksum policy, hot join and permissions in the core; launcher TCP host/join; first run of the mod in the real game; live mode (the session runs inside the game, driven by the in-game window).
- **2026-10-10:** Steam peer-to-peer probe, first one-process connection to the game's own Steam ID; plug-in point in the mod for command schemas; CI runs every Lua test; documentation and listing material (this document set).

## Workstreams

Work after 2026-10-10 runs in parallel workstreams. Each one owns a part of the stages below and writes its own document in `docs/` as it merges (for example `RELAY.md`, `LIVE_CONTROL.md`, `COMPANIES.md`, `COVERAGE_*.md`, `NATIVE_LINK.md`, `SECURITY.md`).

| Workstream | Covers | Stages |
|---|---|---|
| Relay and connection | Steam peer-to-peer as the way players reach each other (primary); the relay server fallback; the first cross-account test; session traffic over either path. Replaces the hole-punching item that used to sit in 0.8. | 0.2, 0.3 |
| Session robustness and reconnect | A player who drops comes back without losing the world; hot join; host leaving; timeouts; what the window shows when something fails. | 0.2, 0.5 |
| Construction and operations replication | Roads, tracks, stations, signals, terrain, bulldozing; vehicles, lines, orders, renames; coverage tables of which engine commands are replicated and how. | 0.3, 0.4 |
| Companies | Separate companies on one map, own money and vehicles, station access, shared infrastructure rules. | 0.6, 0.7 |
| Native hooks | The detours in the game executable that hold construction commands and release them at a stamp; the one-worker script pool; the control link to the mod. | 0.3 |
| Launcher wizard | A guided install, uninstall and update instead of terminal commands. | 0.2 |
| Hardening | Review of everything that parses input or runs in the game process; a guard against panics aborting the game; limits; the release signing key; code signing. | all |
| Listing and documentation | The Nexus Mods page text and screenshots, the user security explainer, the project site, the Mod Hub / Steam Workshop plan, the code signing plan. | 0.2, 0.3 |

## 0.1 — Protocol core (complete 2026-10-09)

- [x] Length-prefixed framing with size limit
- [x] Sealed channel: AES-256-GCM, HKDF session key, replay window
- [x] Join codes: base32, version, addresses, secret, checksum
- [x] Session state machine (host and player sides) with in-memory transport tests
- [x] Message kinds and JSON payload schemas (`docs/PROTOCOL.md`)
- [x] Core ported from C# to Rust (`core/`, crate `feverlink3_core`), same tests, one Cargo workspace with the native module
- [x] Resync round state machine (pause, save, transfer, load, verify, resume) with tests
- [x] CI on every push

## 0.2 — Launcher, connection and the first test release

First tagged build: `v0.2.0-test1`, a GitHub pre-release (the in-app updater skips pre-releases by default).

- [x] Host / join by code, player allow-list, sender-id rewrite. The transport used so far is plain TCP, kept as a development and test transport; the way players connect is the Steam item below.
- [~] Game launch with session environment, file link with sequence numbers (command line `--launch`; unit-tested, not the path the in-game window uses)
- [~] Savegame transfer with SHA-256, streamed from disk (tested over loopback; not between two real games)
- [~] Session clock, speed, pause, checksum rounds, resync round (core state machines and loopback tests)
- [x] `--selftest` over loopback
- [x] Command-line install, uninstall and doctor: `feverlink3 install`, `uninstall`, `doctor`, each with `--dry-run` where it changes things
- [ ] Guided installer (wizard) that replaces the terminal commands *(launcher wizard)*
- [x] Live mode: the session controller runs inside the game, driven by the in-game window (verified in the real game 2026-10-09, see P1 to P4)
- [~] Players connect through Steam peer-to-peer *(relay and connection)*: the listener starts and the game connected to its own Steam ID (P5); the connection between two different accounts (P6) is the next test; session traffic does not run over it yet
- [ ] Relay server fallback *(relay and connection)*
- [~] Reconnect after a dropped connection *(session robustness)*: the player side retries for five minutes and the host treats the return as a hot join (loopback tests); not run between two real games
- [x] Packaging: self-contained single-file exe, portable zip, reproducible release job with SHA-256 (job and `tools/release` done; the first tagged release is not cut yet)
- [~] In-app updater with Ed25519-signed releases: signature, hash, version and file-swap logic are done and tested in `launcher/src/update.rs`; the network fetch is not written, and the public key in the repository is a placeholder whose private half is public, so no release may be signed with it *(hardening)*
- [~] Project site (`docs/index.md`, `docs/site/`): written 2026-10-10; it goes live with the first tagged release, which supplies the download link
- [~] Nexus Mods page text, screenshot list and security explainer (`docs/nexus/`, `docs/SECURITY_FOR_USERS.md`): written 2026-10-10; the screenshots are still to be captured and the claims re-checked against the final code before publishing

## 0.3 — Game side (first playable)

- [x] Native module skeleton in Rust: winhttp proxy, PE parsing, signature scanner, detour maths, build table, `cargo test`
- [x] Address tool (`tools/addr`): finds hook sites in a game executable and prints a build-table row; first run against build 40420 in `tools/addr/out/6ac50427.txt`
- [ ] Native module: command-list hook, hold/release, single-worker script pool *(native hooks)*. The detour code and the build table row exist and are tested; nothing is installed in the game yet (P10)
- [ ] Native module size: `no_std` + `alloc` with a HeapAlloc allocator once the hook set is fixed
- [~] Lua mod: stamping, barrier, replay, checksums, deterministic random. All of it runs in the two-game lupa test; in the real game the mod loads, the window opens, and save/load work (P1 to P3); replaying commands between two real games is not verified (P9)
- [x] Lua lockstep test with lupa
- [ ] First playable: two players, shared company, roads and vehicles (P8, P9, P11)
- [ ] First public release: GitHub Releases zip, Nexus Mods listing, site updated. The pre-release `v0.2.0-test1` is the first step; the playable release needs P8 and P9

## 0.4 — Everything a player does *(construction and operations replication)*

- [ ] Constructions against streets, upgrades, signals, decorations, terrain
- [ ] Lines, vehicle orders, renames, colours
- [~] In-game player list and chat: the model and the window exist and are tested with mocks; not run with two real games
- [ ] 3 to 8 players

## 0.5 — Playing with friends

- [ ] Distribution beyond GitHub and Nexus Mods. **Transport Fever 3 has no Steam Workshop** (the developers' support page says it is not included); its in-game Mod Hub runs on mod.io, and the published mod rules allow only the official scripting API and no executable files, so a mod that ships a native DLL is not expected to pass. The sources, the likely blockers and the cleanest alternatives are in `docs/WORKSHOP.md`
- [ ] Steam invitations and friends-list join *(relay and connection)*
- [ ] Host permissions: enforcement in the launcher and mod (model and protocol done in core; the window has no control for it yet)
- [ ] Hot join of a running game and reconnection that keeps the player's place *(session robustness)*; the hot-join round exists in the core and is tested over loopback

## 0.6 — Companies *(companies)*

- [ ] Separate companies on one map, own money and vehicles
- [ ] Station access permissions, shared infrastructure rules

## 0.7 — Mixed modes

- [ ] Several players per company, joining and leaving companies
- [ ] Shared infrastructure rules and fees

## 0.8 — Reach

- [ ] Optional public game list
- [ ] Code signing: apply to the SignPath Foundation once a release exists in the form that is to be signed (their rule), not later than the first playable release; plan, checklist and the alternatives in `docs/SIGNING.md` *(hardening)*

The hole-punching, STUN and relay items that used to be listed here are replaced by Steam peer-to-peer plus the relay fallback, which moved to 0.2 because players should never have to configure anything to connect.

## 1.0 — Dedicated server

- [ ] Headless host role: a game copy running the mod permanently, no window
- [ ] Persistence by autosave; joining is the hot-join round from the server's save
- [ ] Players join and leave a world that runs for days
- [ ] Linux / Proton package for cheap hosting

## What gates the first release

`v0.2.0-test1` is published only when the owner decides, and after:

1. The claims in the listing and in `docs/SECURITY_FOR_USERS.md` have been re-checked against the code that is tagged (the list is in `docs/nexus/README.md`).
2. A real release signing key exists and the placeholder in `launcher/src/update.rs` is replaced (`docs/RELEASING.md`).
3. The wording about the connection reflects the result of P6: "being tested" until two different accounts have connected, and never "works over the internet" before that.
