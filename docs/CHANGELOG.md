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
- Opening the pause menu no longer drops a player or switches the build sync off on that game.
- A build made by a script (for example the automated test) no longer makes the next real build fail on the other game.
- Bulldozing a train station no longer leaves the bulldozer busy for 30 seconds.
- No build that is refused or fails leaves its tool busy any more: the tool is free right away.
- Opening the pause menu right after a build, even for longer than 30 seconds, no longer loses or misplaces that build.
- A player whose world is closed to the main menu leaves the session within seconds (it took two minutes), and a player who only opens the pause menu stays in.
- When a guest's session ends while the guest's world is still open (the host stopped, the guest was removed or the connection was lost), the guest's world is saved first.
- A rail switch drawn with the track tool stays identical on both games with the new build copy (tested).

- Builds are now copied from the game's own build and applied at the same moment on every game, the game you clicked in included (on by default; testers can switch it off with `FEVERLINK3_NATIVE_INJECT=0`). In two-window tests on one PC, roads (also through houses), bus stops (also a stop replacing a stop), road depots over houses, a train station (passenger and cargo), a module added to that station with Configure, rail track (also through houses and joining an existing track), the lane and crosswalk tools, a cargo road station over houses, a harbour, and bulldozing a house, a station and a harbour stayed identical on both games, with no repair sync, no crash, and the tool still usable after the build.

### Changed
- The logs are much more detailed during the test phase (every build, its result and the checks), so that a problem can be explained from the logs alone.
- The test logs now have one timing line per build (from the click to the moment every game applied it).
- The world check now also notices when the two games number their objects differently, and repairs it.

### Known gaps
Known bugs in one line: a few corner cases are untested, a world closed to the main menu is not saved first, no internet test yet.
- A world closed to the main menu with "Return to Main Menu" is not saved by FeverLink3 first (the game offers no save there; use Save Game in the pause menu before leaving).
- If a game ever has to rebuild a depot or station another way instead of copying it (seen only in a test that forces this), the games differ for a moment and a repair sync follows.
- Two players building at the very same moment has not been tested.
- Stop styles or options picked in the stop tool may arrive with default settings on the other game (not verified).
- Terrain editing, ground paint, trees and rocks are not shared yet (bulldozing a tree is refused).
- A full session between two players over the internet has not been verified since these changes.
