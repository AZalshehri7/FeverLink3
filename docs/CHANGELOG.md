---
title: FeverLink3 changelog
---

# Changelog

All notable changes of each release. FeverLink3 is an unfinished test project: read the status in the README before using it.
Only changes that matter to a player or tester are listed here.

## Unreleased (towards 0.1.1)

Tested so far with two games on one PC (a scripted test that builds, bulldozes and checks both worlds), not yet with two players over the internet.

### Fixed
- Roads, junctions, bridges, tunnels, traffic lights and crosswalks built by one player now show up identically on the other player's game.
- Rail track, switches, signals and electrification now reach the other game.
- Bus stops now reach the other game, including a second and a third stop on the same road.
- Road depots, rail depots, airfields and the underground station entrance now reach the other game.
- Lines, vehicles bought at a depot built during the session, renames, loans, selling a vehicle and game speed work between the games.
- A build that the other game cannot make is no longer applied on one game only; the game that cannot build it says so in the chat.
- A tool no longer stops working after a refused build.
- Several crashes of the game after building a junction, a stop or a station are fixed.
- A game that loads a save from an earlier session no longer stalls the world check.
- Very large money amounts (billions) no longer break the mod.
- Demolishing a building no longer makes the games drift apart (a rare town-growth difference after a demolition).

- Developer test option (off by default, `FEVERLINK3_NATIVE_INJECT=1` on both games): a road depot placed with the real tool, on a free spot or over town buildings, now appears identically on the other game. The game's own build is copied natively (a few tens of KB, not the whole multi-megabyte object). Five real two-window runs in a row stayed identical: no crash, no repair sync. Other tool kinds (stops, stations, rail) are not proven with it yet.

### Changed
- The logs are much more detailed during the test phase (every build, its result and the checks), so that a problem can be explained from the logs alone.

### Known gaps
- Terrain editing, ground paint, trees and rocks are not shared yet.
- Modular stations and harbours, the lane tool, tram stops and cargo stations have not been tested in the scripted test.
- A road built through a house, and a station that removes town buildings, still need a real two-player test.
- A full session between two players over the internet has not been verified since these changes.
