---
title: FeverLink3 status
---

# Status

Last updated 2026-10-23, version 0.2.4 (beta). [Back to the start page](../index.html) · [Roadmap](../ROADMAP.html) · [Changelog](../CHANGELOG.html)

This page keeps three things apart: what has been seen working in the real game, what has only been tested automatically, and the known gaps.

## Seen working in the real game

Played over the internet with a friend through Steam, in sessions of up to 3.5 hours, and on the maintainer's PC:

- Two players on two PCs connect over Steam with a join code; the host's world is sent to the friend.
- Building together: roads, rail, stations, stops, depots, lines and vehicles. A road stands on both games 1.4 s after the click at game speed x1 and 0.8 s at x4 (real time, two games on one PC, a grown save).
- The full game: towns grow, traffic flows, ratings change, as in single player.
- World checks run all the time; when the worlds drifted apart, an automatic repair sync gave everyone the host's world back (about 15 s for an 80 MB world over Steam).
- How often a repair was needed: in the 3.5-hour session over Steam with a friend, an automatic repair was needed about once every 40 minutes (5 times, from object ids in a different order). Other mods that change the world on their own can add more: an automatic line namer caused 5 more in the same session (plus 1 sync for the join).
- Shared speed, pause and chat.
- The new in-game interface: the bar icon with its colours, the pop-up and the window.
- **Report a problem** over Steam: one zip with every player's logs.
- Automatic updates, on the maintainer's PC: 0.2.1 and later 0.2.2 downloaded, checked and installed the next version by themselves (0.2.2 to 0.2.3 on 2026-10-23), finished after one restart.
- The warning about other mods that change the world on their own (seen with Auto Color Lines and Auto Assign Terminals on both PCs).

## Tested automatically only

These are covered by the automatic tests (about 1,600 on every change) or by two game windows on one PC, but not yet seen between two PCs:

- Joining from a Steam invite by itself (fixed in 0.2.1; needs a two-PC test). The join code works.
- A road that joins or splits an existing road no longer reports a false "could not be built" (two game windows on one PC, 5 roads at x1 and x4).
- Every game keeps its world details when a sync starts (two game windows on one PC).
- Game build 40423 (two game windows on one PC: hooks on, builds equal, no automatic sync).
- Other mods that change the world on their own (Auto Line Namer, Auto Color Lines): the host's copy makes the change and both games apply it at the same step (two game windows on one PC, new lines made by hand). A setting changed in such a mod's own window is not yet seen.
- Configuring a station right after a sync (0.2.3; not yet seen in the game).
- Putting the previous version back when an update does not start.
- The switch that turns automatic updates off.

## Known gaps

- **After a Transport Fever 3 update, FeverLink3 needs an update too.** 0.2.4 knows game builds 40420 and 40423; on a newer build the FeverLink3 window says so and hosting is refused until FeverLink3 is updated.
- Terrain editing, ground paint and trees are not shared yet; the terrain tools are switched off during a session.
- Some extra repair syncs: the game sometimes orders its internal object ids differently on two PCs. FeverLink3 finds this within seconds and repairs it automatically; the cause is being traced.
- Mods that change the world on their own with a kind of change FeverLink3 cannot share yet still cause extra repair syncs; FeverLink3 names such a mod in a warning.
- Two players building at the same spot within about a second: one of the two builds can be refused (the worlds stay equal).
- All players share one company; separate companies are planned.
- Releases are not code-signed until 1.0.0. Updates come only from the official GitHub releases over HTTPS, with every file's checksum checked.

Each item moves to "seen working" only after it has been seen in the real game.
