# Roadmap

Status as of 2026-10-10. The project started on 2026-10-09, so everything marked done below happened in those two days. A stage ends when its tests pass and two people have played it; no stage has ended its playable part yet.

## Where things stand

FeverLink3 is an early test build. It is not yet a playable multiplayer game. The first test build, `0.2.0-test1`, has one job: to find out whether two players can reach each other through Steam. Hosting and joining over that connection are the next test build. The line between "written and tested without the game", "run in the real game" and "proven between two real games" is kept sharp on purpose.

**Verified in the real game (Transport Fever 3 build 40420, 2026-10-09):**

- The mod loads, and its window opens from a button in the bottom bar.
- With an earlier build of the window, whose Host button used a direct connection (since taken out of the window, kept as a developer transport), hosting started a session inside the game process and showed a join code, and a second client (a test client, not a second copy of the game) joined that session by its code.
- The mod can drive the game's own `app.saveGame` / `app.loadGame`, and after a reload it resumes its file link to the session.
- The Steam peer-to-peer probe listens inside the game.

**Proven on 2026-10-10:** inside one running game, the game opened a Steam peer-to-peer connection to its own Steam ID and 6 pings were answered.

**Not verified yet:** a Steam connection between two different accounts (the test of `0.2.0-test1`), hosting and joining over Steam (not finished), building together (command replication between two real games), savegame transfer between two real games, the native command hooks (in the code but switched off, never run in the game), and joining from a second real copy of the game.

### Proof milestones

Each one is a thing that has to be seen working before the matching claim may appear in a README, a listing or a release note.

| # | Milestone | State |
|---|---|---|
| P1 | The mod loads in the real game and the window opens | verified 2026-10-09 |
| P2 | Host from the window, a test client joins by the code (earlier build, direct connection, since taken out of the window) | verified 2026-10-09 |
| P3 | Save and load driven by the mod, file link resumes after the reload | verified 2026-10-09 |
| P4 | The Steam peer-to-peer listener starts inside the game | verified 2026-10-09 |
| P5 | Steam peer-to-peer to the game's own Steam ID, one process, 6 pings answered | proven 2026-10-10 |
| P6 | Steam peer-to-peer between two different Steam accounts on two PCs | being tested: this is what `0.2.0-test1` is for, the first run is still to come |
| P7 | Hosting and joining over Steam: a session carried over the Steam connection | not yet, the next test build |
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
- **2026-10-10:** Steam peer-to-peer probe, first one-process connection to the game's own Steam ID; the window became Steam-first and the connection research was written down (`docs/research/P2P_CONNECTIVITY.md`); the join code can carry the host's Steam ID; plug-in point in the mod for command schemas and the schemas for everyday operations (vehicles, lines, names, bookings, events) with their tests (`docs/COVERAGE_OPS.md`); CI runs every Lua test; documentation and listing material (this document set).

## Workstreams

Work after 2026-10-10 runs in parallel workstreams. Each one owns a part of the stages below and writes its own document in `docs/` as it merges (for example `LIVE_CONTROL.md`, `COMPANIES.md`, `COVERAGE_*.md`, `NATIVE_LINK.md`, `SECURITY.md`).

| Workstream | Covers | Stages |
|---|---|---|
| Connection (Steam peer-to-peer) | Steam peer-to-peer as the way players reach each other: the first cross-account test (P6), then hosting and joining carried over it (P7), and what the window shows when it fails. Other transports only if Steam proves not to be enough (research notes: `docs/research/P2P_CONNECTIVITY.md`). Replaces the hole-punching item that used to sit in 0.8. | 0.2, 0.3 |
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

First tagged build: `v0.2.0-test1`, a GitHub pre-release (the in-app updater skips pre-releases by default). Its job is the Steam connection test between two accounts (P6); the zip holds a testing installer, the native module and the mod.

- [x] Host / join by code, player allow-list, sender-id rewrite. The transport used so far is plain TCP, kept as a development and test transport (the window's Host button no longer starts it); the way players connect is the Steam item below.
- [~] Game launch with session environment, file link with sequence numbers (command line `--launch`; unit-tested, not the path the in-game window uses)
- [~] Savegame transfer with SHA-256, streamed from disk (tested over loopback; not between two real games)
- [~] Session clock, speed, pause, checksum rounds, resync round (core state machines and loopback tests)
- [x] `--selftest` over loopback
- [x] Command-line install, uninstall and doctor: `feverlink3 install`, `uninstall`, `doctor`, each with `--dry-run` where it changes things
- [ ] Guided installer (wizard) that replaces the terminal commands *(launcher wizard)*
- [x] Live mode: the session controller runs inside the game, driven by the in-game window (verified in the real game 2026-10-09, see P1 to P4)
- [~] Players connect through Steam peer-to-peer *(connection)*: the listener starts and the game connected to its own Steam ID (P5); the connection between two different accounts (P6) is the test of `0.2.0-test1`; session traffic does not run over it yet (P7)
- [~] The join code carries the host's Steam ID *(connection)*: the code format and its tests are in the core; the in-game controller does not write it yet
- [~] Reconnect after a dropped connection *(session robustness)*: the player side retries for five minutes and the host treats the return as a hot join (loopback tests); not run between two real games
- [x] Packaging: self-contained single-file exe, portable zip, reproducible release job with SHA-256 (job and `tools/release` done; the first tagged release is not cut yet)
- [~] In-app updater with Ed25519-signed releases: signature, hash, version and file-swap logic are done and tested in `launcher/src/update.rs`; the network fetch is not written, and the public key in the repository is a placeholder whose private half is public, so no release may be signed with it *(hardening)*
- [~] Project site (`docs/index.md`, `docs/site/`): written 2026-10-10; it goes live with the first tagged release, which supplies the download link
- [~] Nexus Mods page text, screenshot list and security explainer (`docs/nexus/`, `docs/SECURITY_FOR_USERS.md`): written 2026-10-10; the screenshots are still to be captured and the claims re-checked against the final code before publishing

## 0.3 — Game side (first playable)

- [x] Native module skeleton in Rust: winhttp proxy, PE parsing, signature scanner, detour maths, build table, `cargo test`
- [x] Address tool (`tools/addr`): finds hook sites in a game executable and prints a build-table row; first run against build 40420 in `tools/addr/out/6ac50427.txt`
- [~] Native module logic *(native hooks)*, off unless switched on: counters on the simulation's command application, hold/release hook on the command list with the control file (`native_ctl.log` / `native_events.log`), single-worker script pool patch; installed all-or-none from `DllMain` behind `FEVERLINK3_HOOKS` / `FEVERLINK3_HOLD` / `FEVERLINK3_POOLPATCH`; tested on fake game functions, never run in the real game (`docs/NATIVE_LINK.md`)
- [ ] Native module verified in the game: counters, hold/release with each construction tool, pool patch (`docs/NATIVE_LINK.md` section 8; P10)
- [ ] Lua mod strict mode: `hold_on`, ping, match `held`, stop at the stamp, `release` + trigger command, `cancel`, `hold_off` before a resync (`docs/NATIVE_LINK.md` section 7)
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

- [ ] Distribution: the mod in the game's own Mod Hub (mod.io backend) and on Nexus Mods like any other mod, with the native module delivered separately for now; the installer stays a testing tool. **Transport Fever 3 has no Steam Workshop** (the developers' support page says it is not included). Whether the Mod Hub accepts a mod that needs a separate native component is being checked: its published rules allow only the official scripting API and no executable files. The sources, the likely blockers and the steps are in `docs/WORKSHOP.md`
- [ ] Steam invitations and friends-list join *(connection)*
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
- [ ] Code signing *(hardening)*: the free SignPath Foundation programme needs an OSI-approved licence, so the project (PolyForm Noncommercial) probably does not qualify; the paid options, their cost and eligibility, and the decision for the owner are in `docs/SIGNING.md`

The hole-punching and STUN items that used to be listed here are replaced by Steam peer-to-peer, which moved to 0.2 because players should never have to configure anything to connect.

## 1.0 — Dedicated server

- [ ] Headless host role: a game copy running the mod permanently, no window
- [ ] Persistence by autosave; joining is the hot-join round from the server's save
- [ ] Players join and leave a world that runs for days
- [ ] Linux / Proton package for cheap hosting

## Credits

FeverLink3 is a fresh implementation that builds on two earlier MIT-licensed projects: [TpF2 Multiplayer](https://silver2127.github.io/tpf2-multiplayer/) by silver2127 (Transport Fever 2) and [MPFever](https://github.com/dkdknight/MPFever) by dkdknight / arthur (Transport Fever 3). Their licences are in `THIRD_PARTY_NOTICES.md`.

## What gates the first release

`v0.2.0-test1` is published only when the owner decides, and after:

1. The claims in the listing and in `docs/SECURITY_FOR_USERS.md` have been re-checked against the code that is tagged (the list is in `docs/nexus/README.md`).
2. A real release signing key exists and the placeholder in `launcher/src/update.rs` is replaced (`docs/RELEASING.md`).
3. The wording about the connection reflects the result of P6: "being tested" until two different accounts have connected, and never "works over the internet" before that.
