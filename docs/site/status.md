---
title: FeverLink3 status
---

# Status

Last updated 2026-10-10. The project started on 2026-10-09. [Back to the start page](../index.html) · [Roadmap](../ROADMAP.html)

FeverLink3 is an early test build and not yet a playable multiplayer game. This page keeps three things apart: what has been seen working in the real game, what has only been tested without the game, and what has not been done.

## Seen working in the real game

Game build 40420, the maintainer's PC.

| Date | What |
|---|---|
| 2026-10-09 | The mod loads, and its window opens from a button in the bottom bar. |
| 2026-10-09 | "Host a game" in the window starts a session inside the game and shows a join code. |
| 2026-10-09 | A second client (a test client, not a second copy of the game) joins that session by its code. |
| 2026-10-09 | The mod drives the game's own save and load, and after a reload it finds its session files again. |
| 2026-10-09 | The Steam peer-to-peer listener starts inside the game. |
| 2026-10-10 | Inside one running game, a Steam peer-to-peer connection to the game's own Steam ID opens and 6 pings are answered. |

## Tested without the game

Written and covered by automated tests (unit tests, loopback sessions, two simulated games running the real mod code), but not yet seen between two real games:

- the sealed connection (encryption, join codes, who may send what),
- stamping actions to a future step and applying them in the same order everywhere,
- world checksums and the repair by loading the host's savegame (resync), including sending a save in chunks with a SHA-256 check,
- permissions for building, bulldozing and spending,
- the player-side reconnect and the hot join of a running session,
- install and uninstall of the game-side files, release packaging, and update verification.

## Not done or not verified

- Building together: replicating construction and vehicle commands between two real games.
- Sending a savegame between two real games and loading it on the other side.
- Joining from a second real copy of the game.
- The Steam peer-to-peer connection between two different Steam accounts (the first test is still to come, so the connection is described as being tested), and the relay server fallback (being built).
- The native hooks that hold construction commands back so every game builds at the same moment: the code exists, nothing is switched on in this version.
- Separate companies, a guided installer, a dedicated server.

## What would change this page

Each line above moves to "seen working" only after it has been seen in the real game, with the date. The proof milestones P1 to P11 and the plan are in the [roadmap](../ROADMAP.html).
