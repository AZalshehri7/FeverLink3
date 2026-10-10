---
title: FeverLink3 status
---

# Status

Last updated 2026-10-10. The project started on 2026-10-09. [Back to the start page](../index.html) · [Roadmap](../ROADMAP.html)

FeverLink3 is an early test build and not yet a playable multiplayer game. The first test build has one job: to find out whether two players can reach each other through Steam. This page keeps three things apart: what has been seen working in the real game, what has only been tested without the game, and what has not been done.

## Seen working in the real game

Game build 40420, the maintainer's PC.

| Date | What |
|---|---|
| 2026-10-09 | The mod loads, and its window opens from a button in the bottom bar. |
| 2026-10-09 | With an earlier build of the window, whose Host button used a direct connection (since taken out of the window), hosting showed a join code, and a second client (a test client, not a second copy of the game) joined that session by its code. |
| 2026-10-09 | The mod drives the game's own save and load, and after a reload it finds its session files again. |
| 2026-10-09 | The Steam peer-to-peer listener starts inside the game. |
| 2026-10-10 | Inside one running game, a Steam peer-to-peer connection to the game's own Steam ID opens and 6 pings are answered. |

## Tested without the game

Written and covered by automated tests (unit tests, loopback sessions, two simulated games running the real mod code), but not yet seen between two real games:

- the sealed connection (encryption, join codes, who may send what),
- stamping actions to a future step and applying them in the same order everywhere, including the schemas for everyday operations (vehicles, lines, names, bookings, events),
- world checksums and the repair by loading the host's savegame (resync), including sending a save in chunks with a SHA-256 check,
- permissions for building, bulldozing and spending,
- the player-side reconnect (the same player back within about five minutes) and the hot join of a running session, the host removing a player, and eight games in one session,
- install and uninstall of the game-side files, release packaging, and update verification.

## Not done or not verified

- The Steam peer-to-peer connection between two different Steam accounts: this is what the current test build is for, so the connection is described as being tested.
- Hosting and joining a game over Steam (the window's Host button says so), and sending the host's world to a second real game and loading it there.
- Joining from a second real copy of the game.
- Building together: replicating construction and vehicle commands between two real games.
- The native hooks that hold construction commands back so every game builds at the same moment: the code exists but is switched off by default and has never run in the real game.
- Separate companies and a dedicated server. The installer's double-click menu has been tested on a fake PC only, not yet on a clean PC.

## What would change this page

Each line above moves to "seen working" only after it has been seen in the real game, with the date. The proof milestones and the plan are in the [roadmap](../ROADMAP.html).
