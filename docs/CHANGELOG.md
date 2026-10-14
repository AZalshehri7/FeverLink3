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

- Builds are now copied from the game's own build and applied at the same moment on every game, the game you clicked in included (on by default; testers can switch it off with `FEVERLINK3_NATIVE_INJECT=0`). In two-window tests on one PC, roads (also through houses), bus stops (also a stop replacing a stop), road depots over houses, a train station and a module added to that station with Configure stayed identical on both games, with no repair sync, no crash, and the tool still usable after the build.

### Changed
- The logs are much more detailed during the test phase (every build, its result and the checks), so that a problem can be explained from the logs alone.

### Known gaps
Known bugs in one line: a build next to something built earlier can fail on the other game (repaired by a sync), the pause menu can drop a player, rail and harbours untested with the new build copy, no internet test yet.
- A build that touches something built earlier in the same session (for example a depot next to a new bus stop) can fail on the other game; the games then repair themselves with a sync and that build is lost. Seen once, after a long automated test.
- Opening the pause menu during a session switches the build sync off on that game and can drop that player from the session.
- Rail track (also splitting a track), harbours and their bulldozing have not been tested with the new build copy.
- Two players building at the very same moment has not been tested.
- Stop styles or options picked in the stop tool may arrive with default settings on the other game (not verified).
- Terrain editing, ground paint, trees and rocks are not shared yet.
- Modular stations and harbours, the lane tool, tram stops and cargo stations have not been tested in the scripted test.
- A road built through a house, and a station that removes town buildings, still need a real two-player test.
- A full session between two players over the internet has not been verified since these changes.
