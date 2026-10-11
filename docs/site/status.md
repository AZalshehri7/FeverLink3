---
title: FeverLink3 status
---

# Status

Last updated 2026-10-10. The project started on 2026-10-09. [Back to the start page](../index.html) · [Roadmap](../ROADMAP.html)

FeverLink3 is an early test build and not yet a full multiplayer game: two real copies of the game can host and join and follow each other's speed, pause and chat commands, and a thin slice of building (roads, bulldozing, loans, lines) has been seen working between two games; stations, rail, vehicles and terrain have not been tried. This page keeps three things apart: what has been seen working in the real game, what has only been tested without the game, and what has not been done.

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
| 2026-10-10 | System test in two real games (host and guest on one PC; worlds compared at one game time with an id-independent fingerprint, and a resync or a differing checksum round during a step counts as a failure): the game's own speed and pause buttons in the bottom bar control both games from either side; loans taken in the real Account window sync both ways without doubling; renaming a town, the company and a station group syncs both ways; creating a line syncs both ways; a road built with the real road tool shows on the other game about one second later, in both directions; bulldozing a road piece syncs (the refund shows up on the other game after the host's money correction, about 20 s). |
| 2026-10-10 | The session buttons in two real games: the guest's **Leave the session** returns it to the start screen and the host's list drops it; the host's **Remove** next to a player removes it and the guest sees why; a returning guest is synced again (hot join); when the host returns to the main menu the host's session ends by itself and the guest sees "The host ended the session". |
| 2026-10-10 | Town growth: two real games resynced to one world and left running with no action drifted apart within about 90 s (other building variants and levels in the same places, a house on one side only, one more street piece), also with pollution and noise neutralised as MPFever does. A test that held growth still made the worlds identical, but that approach was removed on 2026-10-11 because it removes a game mechanic; the fix has to keep growth working. |

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
- Not yet exercised in the system test: stations, depots, other constructions, rail tracks, buying vehicles and assigning them to lines, terrain editing (its height grid cannot be read from a script, so it is not replicated), and the effect of the host's permissions on a player's actions. The same capture and replay path carried a road, a construction (warehouse, port), a bulldozing and a line, so these are expected to work, but that is not shown.
- Town growth is native and not the same on two games (see `ARCHITECTURE.md` section 7): towns drift apart within about 90 s and a build through a town can then fail on the other game. Towns are never switched off or changed to hide this; the cause is being analysed so that growth can be made identical.
- Money after a bulldozing: the refund the native tool books is not part of the replayed build, so the two games differ by that amount until the host's money correction (about 20 s).
- The native hooks that hold construction commands back so every game builds at the same moment: the code exists but is switched off by default and has never run in the real game.
- Separate companies and a dedicated server. The installer's double-click menu has been tested on a fake PC only, not yet on a clean PC.

## What would change this page

Each line above moves to "seen working" only after it has been seen in the real game, with the date. The proof milestones and the plan are in the [roadmap](../ROADMAP.html).
