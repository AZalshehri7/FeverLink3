# Roadmap

Versions are not dated. Each phase ends when its tests pass and two people have played it.

## 0.1 — Protocol core *(in progress)*
- [x] Length-prefixed framing with size limit
- [x] Sealed channel: AES-256-GCM, HKDF session key, replay window
- [x] Join codes: base32, version, addresses, secret, checksum
- [x] Session state machine (host and player sides) with in-memory transport tests
- [x] Message kinds and JSON payload schemas (`docs/PROTOCOL.md`)
- [ ] Resync round state machine (pause, save, transfer, load, verify, resume) with tests
- [x] CI on every push

## 0.2 — Launcher
- [ ] Host / join by code over TCP, player allow-list, sender-id rewrite
- [ ] Game launch with session environment, file link with sequence numbers
- [ ] Savegame transfer with SHA-256, streamed from disk
- [ ] Session clock, speed, pause, checksum rounds, resync round
- [ ] `--selftest` over loopback
- [ ] Packaging: self-contained single-file exe, portable zip, reproducible release job with SHA-256
- [ ] In-app updater with Ed25519-signed releases; version carried in the join code
- [ ] First-run install and one-button uninstall of the game-side pieces
- [ ] Project site on GitHub Pages (`docs/`): download, how to play, known issues, privacy

## 0.3 — Game side
- [x] Native module skeleton in Rust: winhttp proxy, PE parsing, signature scanner, detour maths, build table, `cargo test`
- [ ] Address tool: finds hook sites in a game executable and prints a build-table row
- [ ] Native module: command-list hook, hold/release, single-worker script pool
- [ ] Native module size: `no_std` + `alloc` with a HeapAlloc allocator once the hook set is fixed
- [ ] Lua mod: stamping, barrier, replay, checksums, deterministic random
- [ ] Lua lockstep test with lupa
- [ ] First playable: two players, shared company, roads and vehicles
- [ ] First public release: GitHub Releases zip, Nexus Mods listing, site updated

## 0.4 — Everything a player does
- [ ] Constructions against streets, upgrades, signals, decorations, terrain
- [ ] Lines, vehicle orders, renames, colours
- [ ] In-game player list and chat
- [ ] 3 to 8 players

## 0.5 — Playing with friends
- [ ] Steam invites and friends-list join
- [ ] Host permissions: who may build, bulldoze, spend, change speed
- [ ] Reconnection and hot join

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
