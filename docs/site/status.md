---
title: FeverLink3 status
---

# Status

Last updated 2026-10-10. The project started on 2026-10-09. [Back to the start page](../index.html) · [Roadmap](../ROADMAP.html)

FeverLink3 is an early test build and not yet a full multiplayer game: two real copies of the game can host and join and follow each other's speed, pause and chat commands, but building together has not been seen yet. This page keeps three things apart: what has been seen working in the real game, what has only been tested without the game, and what has not been done.

## Seen working in the real game

Game build 40420, the maintainer's PC.

| Date | What |
|---|---|
| 2026-10-09 | The mod loads, and its window opens from a button in the bottom bar. |
| 2026-10-09 | With an earlier build of the window, whose Host button used a direct connection (since taken out of the window), hosting showed a join code, and a second client (a test client, not a second copy of the game) joined that session by its code. |
| 2026-10-09 | The mod drives the game's own save and load, and after a reload it finds its session files again. |
| 2026-10-09 | The Steam peer-to-peer listener starts inside the game. |
| 2026-10-10 | Inside one running game, a Steam peer-to-peer connection to the game's own Steam ID opens and 6 pings are answered. |
| 2026-10-10 | The Steam peer-to-peer connection between two different Steam accounts: the maintainer's friend reported `connected` with a round trip of 199 ms through a Steam relay (reported by the friend, not seen by the maintainer). |
| 2026-10-10 | Two real copies of the game on one PC, over the developer connection on the loopback address (two games of one Steam account cannot reach each other through Steam): the host hosts and presses **Copy code**, the guest pastes the code with Ctrl+V and joins, the host presses **Start the session**, the host's world is sent in chunks and the guest's game loads it while the host's game keeps playing, and the paused checksum after the transfer matches. [Recording](../media/split-screen-demo.gif). |
| 2026-10-10 | In the same two games: **2x**, **Pause** and **1x** pressed in the window of either game change the speed of both, with the same date on both, and a chat message typed in one game appears in the other. |

## Tested without the game

Written and covered by automated tests (unit tests, loopback sessions, two simulated games running the real mod code), but not yet seen between two real games:

- the sealed connection (encryption, join codes, who may send what),
- stamping actions to a future step and applying them in the same order everywhere, including the schemas for everyday operations (vehicles, lines, names, bookings, events),
- world checksums and the repair by loading the host's savegame (resync), including sending a save in chunks with a SHA-256 check,
- permissions for building, bulldozing and spending,
- the player-side reconnect (the same player back within about five minutes) and the hot join of a running session, the host removing a player, and eight games in one session,
- install and uninstall of the game-side files, release packaging, and update verification.

## Not done or not verified

- Hosting and joining a whole session over Steam between two accounts (the connection test worked; carrying a session over it is the next test with a friend).
- A busy world: the sync in which the host's game keeps playing was checked on a small world with no lines or vehicles, and the checksum covers money, loan, constructions, lines and time only. Whether a world with many vehicles stays identical is not known yet; `FEVERLINK3_HOST_RELOAD=1` makes the host reload too, as a fallback.
- Building together: replicating construction and vehicle commands between two real games.
- The native hooks that hold construction commands back so every game builds at the same moment: the code exists but is switched off by default and has never run in the real game.
- Separate companies and a dedicated server. The installer's double-click menu has been tested on a fake PC only, not yet on a clean PC.

## What would change this page

Each line above moves to "seen working" only after it has been seen in the real game, with the date. The proof milestones and the plan are in the [roadmap](../ROADMAP.html).
