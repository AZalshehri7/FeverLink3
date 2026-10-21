# Roadmap

Status as of 2026-10-21. The project started on 2026-10-09. What changed in each version is in the [changelog](CHANGELOG.html).

How to read the markers: `[x]` done, `[~]` done but not yet seen in the real game in the way that matters (for example only on one PC), `[ ]` not done.

## Versions

### 0.1.0 (2026-10-14): first working multiplayer session (done)

- [x] Host and join from the in-game window; the host's world is sent and loaded on the other game; speed, pause and chat follow each other (seen in two real games on one PC, 2026-10-10).
- [x] Builds copied natively from the game's own build command and applied at the same step on every game: roads, junctions, bridges, tunnels, rail, signals, stops, stations and modules, depots, lanes, crosswalks and bulldozing (seen in two real games on one PC, 2026-10-14).
- [x] Lines, vehicles, renames and loans between the games (seen in two real games on one PC).
- [x] World checks and the repair by loading the host's save.

### 0.1.5 (2026-10-19): feature-complete test version (done)

- [x] Sessions over Steam between two PCs: played in internet tests with a friend, the last one on 2026-10-20. These tests found problems (object ids out of order, held builds, a crash) that 0.2.0 fixes.
- [~] Steam invites from Steam's own dialog and the friends list: tested in the automatic tests, not yet between two PCs.
- [~] Signed updates (`feverlink3 update`): tested in the automatic tests.
- [~] Mod check at join, stall catch-up (a frozen game no longer stops the others), coming back after a short drop: tested in the automatic tests and in lab runs on one PC.

### 0.2.0: first public beta (next)

- [~] The fixes from the friend test of 2026-10-20: object id order, held builds without a record refused, the crash, terrain tools off during a session. Tested in the automatic tests and in lab runs on one PC.
- [~] New in-game interface: bar icon with colours, pop-up, window with tabs, plain messages, the list of events.
- [~] Report a problem: one zip with every player's logs, collected by the host. Not yet tried in a real game.
- [~] Faster session start: pause under 2 s, whole sync about 17 s (two games on one PC).
- [x] README rewritten.
- [ ] A session on two PCs over Steam with these changes, played by the owner, before the release.

## Later

- [ ] Sharing terrain editing, ground paint, trees and rocks.
- [ ] Separate companies for each player on one map, with station access and shared infrastructure rules.
- [ ] Host permissions in the window (the model and the protocol are done).
- [ ] Distribution beyond GitHub: Nexus Mods, and the game's Mod Hub if it accepts a mod with a native part (`docs/WORKSHOP.md`). Transport Fever 3 has no Steam Workshop.
- [ ] Code signing of the files (`docs/SIGNING.md`).
- [ ] Dedicated server: a game copy that hosts permanently, joining from its autosave.

## What gates 0.2.0

1. A session on two PCs over Steam with the 0.2.0 changes, and the owner has played it.
2. The claims in the README, `docs/PLAYING.md` and `docs/SECURITY_FOR_USERS.md` re-checked against the code that is released.
3. The release is signed with the real key (`docs/RELEASING.md`).

## Credits

FeverLink3 is a fresh implementation that builds on two earlier MIT-licensed projects: [TpF2 Multiplayer](https://silver2127.github.io/tpf2-multiplayer/) by silver2127 (Transport Fever 2) and [MPFever](https://github.com/dkdknight/MPFever) by dkdknight / arthur (Transport Fever 3). Their licences are in `THIRD_PARTY_NOTICES.md`.
