---
title: FeverLink3 status
---

# Status

Last updated 2026-10-22, version 0.2.1 (beta). [Back to the start page](../index.html) · [Roadmap](../ROADMAP.html) · [Changelog](../CHANGELOG.html)

This page keeps three things apart: what has been seen working in the real game, what has only been tested automatically, and the known gaps.

## Seen working in the real game

Two tests with a friend over the internet through Steam (2026-10-20 and 2026-10-21, about 3.5 hours together), and the maintainer's own tests:

- Two players on two PCs connect over Steam with a join code; the host's world is sent to the friend.
- Building together: roads, rail, stations, stops, depots, lines and vehicles. A road stands on both games 1.4 s after the click (game speed x1, real time, two games on one PC, a grown save; 2026-10-22).
- The full game: towns grow, traffic flows, ratings change, as in single player.
- World checks run all the time; when the worlds drifted apart, an automatic repair sync gave everyone the host's world back (about 15 s for an 80 MB world over Steam).
- Shared speed, pause and chat.
- The new in-game interface: the bar icon with its colours, the pop-up and the window.
- **Report a problem** over Steam: one zip with every player's logs.
- Automatic updates, on the maintainer's PC: 0.2.1 downloaded, checked and installed itself, finished after one restart.

## Tested automatically only

These are covered by the automatic tests (about 1,600 on every change) or by two game windows on one PC, but not yet seen between two PCs:

- Joining from a Steam invite by itself (fixed in 0.2.0; needs a two-PC test). The join code works.
- The warning about other mods that change the world on their own.
- Putting the previous version back when an update does not start.
- The switch that turns automatic updates off.

## Known gaps

- Terrain editing, ground paint and trees are not shared yet; the terrain tools are switched off during a session.
- Some extra repair syncs: the game sometimes orders its internal object ids differently on two PCs. FeverLink3 finds this within seconds and repairs it automatically; the cause is being traced.
- Mods that change the world on their own (for example an automatic line namer) cause extra repair syncs. Turn their automatic changes off while playing together.
- Two players building at the same spot within about a second: one of the two builds can be refused (the worlds stay equal).
- All players share one company; separate companies are planned.
- Releases are not code-signed until 1.0.0. Updates come only from the official GitHub releases over HTTPS, with every file's checksum checked.

Each item moves to "seen working" only after it has been seen in the real game.
