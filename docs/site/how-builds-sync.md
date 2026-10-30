---
title: How builds stay identical
---

# How a build reaches the other game

[Back to the start page](../index.html) · [Status](status.html) · [Changelog](../CHANGELOG.html)

This is the short version. Nothing here is a promise: [what is verified and what is not](status.html) is on the status page.

1. **You build something** with the game's own tool (a road, a depot, a station). The tool works as usual and shows its preview.
2. **The mod catches the build** before the game applies it, and gives it a time a moment in the future.
3. **Every game builds it at that same time**, the one you clicked in included, so the worlds stay the same without sending the world around.
4. **Checks run all the time.** The games compare their worlds. If they ever differ, the difference is repaired from the host's game.

Many builds cannot be rebuilt exactly from a description, because the game decides details itself (a depot's entrance over a street, a stop's exact place). So the mod copies the game's own build from the game you clicked in, and every game, yours included, applies that same copy at the same moment. In two tests with a friend over Steam (2026-10-20 and 2026-10-21) this worked for roads, rail, stations, stops, depots, lines and vehicles.

Known limits right now (version 0.2.10):
- Trees, plants, rocks and the vegetation eraser are shared since 0.2.8 (terrain heights and ground paint since 0.2.7). This is new: seen in an automated test with two game windows on one PC, not yet between two PCs; tell us if the trees look different for someone.
- Advanced Track Patterns junctions: the game log says RESULT DIFFERS because the mod's pattern stands as plain track; the worlds stay equal.
- Two players building at the same spot within about a second: one of the two builds can be refused on both games (the worlds stay equal).
- Some extra repair syncs still happen, found within seconds and repaired automatically. Mods that change the world on their own (for example an automatic line namer) cause them too: turn their automatic changes off while playing together.

No game rule is turned off to make this work: towns grow, traffic runs and ratings change on every game as usual.
