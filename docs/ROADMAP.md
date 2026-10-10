# Roadmap

Versions are not dated. Each phase ends when its tests pass and two people have played it.

## 0.1 — Protocol core *(in progress)*
- [x] Length-prefixed framing with size limit
- [x] Sealed channel: AES-256-GCM, HKDF session key, replay window
- [x] Join codes: base32, version, addresses, secret, checksum
- [x] Session state machine (host and player sides) with in-memory transport tests
- [x] Message kinds and JSON payload schemas (`docs/PROTOCOL.md`)
- [x] Core ported from C# to Rust (`core/`, crate `feverlink3_core`), same tests, one Cargo workspace with the native module
- [x] Resync round state machine (pause, save, transfer, load, verify, resume) with tests
- [x] CI on every push

## 0.2 — Launcher
- [x] Host / join by code over TCP, player allow-list, sender-id rewrite
- [ ] Game launch with session environment, file link with sequence numbers
- [ ] Savegame transfer with SHA-256, streamed from disk
- [ ] Session clock, speed, pause, checksum rounds, resync round
- [x] `--selftest` over loopback
- [x] Packaging: self-contained single-file exe, portable zip, reproducible release job with SHA-256 (release job and `tools/release` done; first tagged release waits for the real signing key)
- [x] In-app updater with Ed25519-signed releases; version carried in the join code (signature, hash, version and file-swap logic done in `launcher/src/update.rs`; the network fetch is not, nor the join-code version)
- [ ] First-run install and one-button uninstall of the game-side pieces
- [ ] Project site on GitHub Pages (`docs/`): download, how to play, known issues, privacy

## 0.3 — Game side
- [x] Native module skeleton in Rust: winhttp proxy, PE parsing, signature scanner, detour maths, build table, `cargo test`
- [x] Address tool (`tools/addr`): finds hook sites in a game executable and prints a build-table row; first run against build 40420 in `tools/addr/out/6ac50427.txt`
- [x] Native module logic, off unless switched on: counters on the simulation's command application, hold/release hook on the command list with the control file (`native_ctl.log` / `native_events.log`), single-worker script pool patch; installed all-or-none from `DllMain` behind `FEVERLINK3_HOOKS` / `FEVERLINK3_HOLD` / `FEVERLINK3_POOLPATCH`; tested on fake game functions (`docs/NATIVE_LINK.md`)
- [ ] Native module verified in the game: counters, hold/release with each construction tool, pool patch (`docs/NATIVE_LINK.md` section 8)
- [ ] Lua mod strict mode: `hold_on`, ping, match `held`, stop at the stamp, `release` + trigger command, `cancel`, `hold_off` before a resync (`docs/NATIVE_LINK.md` section 7)
- [ ] Native module size: `no_std` + `alloc` with a HeapAlloc allocator once the hook set is fixed
- [ ] Lua mod: stamping, barrier, replay, checksums, deterministic random (core, file link and window commands done; native-build replay and in-game UI open)
- [x] Lua lockstep test with lupa
- [ ] First playable: two players, shared company, roads and vehicles
- [ ] First public release: GitHub Releases zip, Nexus Mods listing, site updated

## 0.4 — Everything a player does
- [ ] Constructions against streets, upgrades, signals, decorations, terrain
- [ ] Lines, vehicle orders, renames, colours
- [x] In-game player list and chat (mod side; needs a game run)
- [ ] 3 to 8 players (host limit of 8 games, ids, fan-out, flood limit, ping and round-trip times, a resync round and a kick with eight games: core and launcher done, tested over loopback; waits for a game run)

## 0.5 — Playing with friends
- [ ] Lua mod on the in-game Mod Hub and, if TF3 supports it, the Steam Workshop (auto-updates for every player); first confirm its automated approval allows the mod's file link
- [ ] Steam invites and friends-list join
- [ ] Host permissions: enforcement in the launcher and mod (model and protocol done in core; the live controller takes `permissions` and `kick` commands, `docs/LIVE_CONTROL.md`; the window and the game side are open)
- [ ] Reconnection (same id, name, slot and permissions back within 5.5 minutes, then the hot-join round: core and launcher done, tested over loopback; waits for a game run)

## 0.6 — Companies
- [ ] Separate companies on one map, own money and vehicles
- [ ] Station access permissions, shared infrastructure rules

## 0.7 — Mixed modes
- [ ] Several players per company, joining and leaving companies
- [ ] Shared infrastructure rules and fees

## 0.8 — Reach
- [ ] UDP hole punching, STUN, relay fallback
- [ ] Optional public game list
- [ ] Code signing through SignPath (apply once 0.3 is playable)

## 1.0 — Dedicated server
- [ ] Headless host role: a game copy running the mod permanently, no window
- [ ] Persistence by autosave; joining is the hot-join round from the server's save
- [ ] Players join and leave a world that runs for days
- [ ] Linux / Proton package for cheap hosting
