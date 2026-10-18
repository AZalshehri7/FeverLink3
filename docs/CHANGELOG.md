---
title: FeverLink3 changelog
---

# Changelog

All notable changes of each release. FeverLink3 is an unfinished test project: read the status in the README before using it.
Only changes that matter to a player or tester are listed here.

## 0.2.0-pre6

Fixes from the internet test of 0.2.0-pre5 with a friend. **Not yet played by a person:** tested in the automatic tests and one short lab run on one PC (two windows, scripted and real-tool builds, a build during a pause, a forced repair sync). Not tested over the internet.

### Fixed
- Builds are captured again before the game applies them (0.2.0-pre3 to pre5 never received the build tools' events: every build was applied at the click on one game, the games' objects drifted apart and repair syncs followed).
- Changing what a line loads or unloads (cargo filters, load mode, stops) works while the game runs, not only in a pause.
- The two games' checksums are really compared again (since 0.2.0-pre3 every check was skipped), and a run of skipped checks now shows a warning.
- No more slowing down and standing still on a fast connection: the waiting room between the games was one engine step batch too short.
- No more half-second stutter every few seconds on the host after a line was deleted (the company page read the deleted line).
- The speed buttons show the session's speed after a click again.

### Known gaps
- Not yet played by a person, and not tested over the internet.
- Unproven in the game: the clone freeze fix, refund money, lines with waypoints, roads over town parcels, the builder's engine check on big stations.
- The speed icon may still flicker briefly while the game waits for the other player.
- The gaps listed for the earlier versions below still apply where not fixed above.

## 0.2.0-pre5

Fixes from the owner's own test of 0.2.0-pre4. **Not tested in a game before release:** only the automatic tests ran on these fixes (the owner asked for the build without a lab check).

### Fixed
- Builds are captured again before the game applies them (0.2.0-pre3 and 0.2.0-pre4 could not listen to the build tools' events, so every build was applied at the click on one game and copied afterwards: roads of the guest did not appear on the host, a bulldoze on the host stayed on the guest, and a repair sync followed).
- Roads over a town's land parcels are no longer refused before they are sent ("heap pointers in the text cannot be resolved"): the copy now carries the parcels.

### Known gaps
- Not tested in a game before release, not yet played by a person, not tested over the internet.
- Everything listed as unproven for 0.2.0-pre4 still is (clone freeze fix, refund money, line edits with vehicles, waypoints, smoother waiting over the internet).

## 0.2.0-pre4

Fixes after 0.2.0-pre3. **Not yet played by a person:** tested only in two game windows on one PC (about 38 scripted builds, 10 real-tool builds, fast and interleaved builds from both windows, builds during a pause, two forced repair syncs followed by buying, a line edit and building, an airport with Configure and a runway). Not tested over the internet.

### Fixed
- After a repair sync the game starts by itself again (0.2.0-pre3 stopped at the loading screen until a player clicked "Start Game").
- Cloning vehicles should no longer freeze the game: FeverLink3 no longer tries to read things an engine object does not have; every such failed read made the game write a crash report, about 0.1 s each (thousands in the last internet test, most of them during the clone freezes). Not yet confirmed with a real clone.
- The game's own random choices (town and street development, vehicle track reservations) no longer depend on how long a game stood still waiting for the other player, a likely cause of the worlds drifting apart.
- Smoother play when waiting for the other player: the game slows down a little before it would have to stop, instead of running and then standing still.
- Bulldozing now hands the game its own refund list inside the game's command on every game. Whether the money comes back is not yet confirmed.
- Buying several vehicles in one go assigns each purchase to the right vehicle; putting a vehicle on a line during a pause waits until its purchase is done.
- Waypoints on a track that runs the other way on the other game land in the right place.

### Known gaps
- Not yet played by a person, and not tested over the internet.
- Unproven in the game: the clone freeze fix, refund money, editing a line that has vehicles, lines with waypoints, the smoother waiting over the internet.
- The gaps listed for the earlier versions below still apply where not fixed above.

## 0.2.0-pre3

Fixes from the first internet test with a friend and an airport test on one PC (0.2.0-pre2), plus work on stalls and the checksum. **Not yet played by a person:** tested only in two game windows on one PC (scripted builds, real-tool builds, fast and interleaved builds, builds during a pause, forced repair syncs followed by buying and building). The newest changes (line waypoints, the builder's own check before shipping, unusual tools during a pause) ran only in the automatic tests. Not tested over the internet.

### Fixed
- Line edits no longer start a repair sync: an edit that cannot be shared is refused on every game with a chat message, never applied on one game only. Lines with waypoints are shared too.
- A repair sync is no longer reported as failed while it worked, and buying vehicles and editing lines keep working after a repair sync (before, every window action timed out until a restart).
- Builds made right after a repair sync are no longer applied late on the other game.
- A build that the builder's own check finds unsafe is refused on every game instead of being built on one game only. The builder now also rebuilds its own build through the game's engine before sending it.
- A build whose ground the town changed before its turn (for example a town building replaced) is refused on every game alike, without a repair sync.
- Airports with Configure items, runways and stations over town buildings: the copy no longer refuses them on one game only.
- Bulldozing a station built beside a road works again.
- The game no longer stalls every few seconds for the checksum: it is now spread over many short steps (each a few milliseconds on one PC) instead of one long pause about once a minute.
- Fewer and shorter stand-stills while waiting for the other player: the company page and other periodic work no longer block a frame, and the waiting time adapts to the measured connection.
- Cloning several vehicles is sent as one action. Whether this removes the freeze of 0.2.0-pre2 is not proven yet.
- Bulldozing a fresh build carries the game's refund list to every game. Whether both games get the money back is not proven yet.
- Building with an unusual tool during a pause is applied in the pause on every game.

### Changed
- The mod's folder is now `mods\release\feverlink3` (was `feverlink3_1`). **Delete the old `feverlink3_1` folder** when you update, or the game loads two copies.
- The mod follows the official Transport Fever 3 modding documentation for every game API call it makes.
- A build appears on both games about 0.4 to 1.6 seconds after the click at 3x speed (median about 0.7 s, on one PC).

### Known gaps
- Not yet played by a person, and not tested over the internet.
- Unproven in the game: the clone freeze, refund amounts, editing a line that has vehicles, lines with waypoints, the builder's engine check on big stations (it may add up to about a second to the click), unusual tools during a pause.
- The gaps listed for the earlier versions below still apply where not fixed above.

## 0.2.0-pre2

A stability release from a full review of the code. **Not yet played by a person:** tested only in two game windows on one PC with scripted clicks (about 45 scripted builds and actions, 10 real-tool builds of every kind, fast builds, both players building a few seconds apart, and builds during a pause). Not tested over the internet.

### Fixed
- Several ways the native part of FeverLink3 could crash the game are closed: an error inside the game's own build functions no longer ends the game through our code; a station with several parts that failed to copy no longer frees the same memory twice; a held build is never handed back to the game empty; a damaged build from the other player is refused instead of crashing (two such crashes were found by testing thousands of damaged builds).
- A build that cannot be copied exactly on a game is no longer replaced by a different build there: that game builds nothing, the repair sync follows, and the builder's tool is freed.
- No build is applied late any more: every game waits for the other players' input before it passes a step. When the connection lags, the game may pause for a moment instead of drifting apart.
- A build is never applied at the click on one game and later on the others, even after an internal error.
- Vehicles, lines, loans and the other actions from the game's windows are applied at the same moment on every game (before, the player who clicked got them a little earlier, which could make the games number their objects differently).
- Building during a pause works again: every game applies the builds made in the pause in the same order, set by the host.
- Two builds due at the same moment (two players building at once) no longer overwrite each other.
- Ending a session returns the game to normal single-player play, and a new session never replays lines of an earlier one.
- The host keeps the session running while it sends the world to a joining player.
- The logs rotate instead of growing forever; the native event log no longer stops at 8 MB, which switched the build sync off in long sessions.

### Changed
- A build appears on both games about 0.4 to 1.3 seconds after the click at 3x speed (median about 0.7 s, on one PC).
- During a pause, each build made moves the world forward a step or a few (fractions of a second of game time), so that every game applies it at the same step.

### Known gaps
- Not yet played by a person, and not tested over the internet.
- The window actions (buying vehicles, lines) were tested through the scripted path, not through clicks in the game's windows.
- Two players building within the same second has not been tested (a few seconds apart works).
- The control files between the mod and the native part still use line numbers that two parts of the game's script write; a rare simultaneous write could still lose a build order (a proper fix is planned).
- The gaps listed for 0.2.0-pre1 below still apply.

## 0.2.0-pre1

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
- The host's game crashed when the guest built a track after the host had been building quickly (one build every 2-3 seconds) and then placed a station. Two causes, both fixed: (1) the mod's check that the native part of FeverLink3 is still alive switched the build sync off by mistake during fast building, and for the rest of the session, so the next station was applied at the click on one game only and the worlds split; the check now counts every answer of the native part, asks often enough while builds are being made, and switches the build sync on again by itself if it ever goes off. (2) A copied build now first checks that every existing road piece, track piece, stop and building it names is the same object, in the same place, on the receiving game. If not (the worlds have split), the build is refused there with a clear reason instead of crashing the game, and the usual repair sync fixes the worlds. Tested in two windows on one PC: 17 fast builds from both players (roads, tracks, stations, depots) with the sync on throughout and identical worlds, a track right after a station, and a forced split followed by builds that were refused and repaired without a crash.

- Builds are now copied from the game's own build and applied at the same moment on every game, the game you clicked in included (on by default; testers can switch it off with `FEVERLINK3_NATIVE_INJECT=0`). In two-window tests on one PC, roads (also through houses), bus stops (also a stop replacing a stop), road depots over houses, a train station (passenger and cargo), a module added to that station with Configure, rail track (also through houses and joining an existing track), the lane and crosswalk tools, a cargo road station over houses, a harbour, and bulldozing a house, a station and a harbour stayed identical on both games, with no repair sync, no crash, and the tool still usable after the build.

### Changed
- The logs are much more detailed during the test phase (every build, its result and the checks), so that a problem can be explained from the logs alone.
- The test logs now have one timing line per build (from the click to the moment every game applied it).
- The world check now also notices when the two games number their objects differently, and repairs it.
- Builds appear faster: the short freeze of about a third of a second at each build click is gone, and on the other game a station or depot no longer stalls the game for up to a second when it arrives. At 3x speed a build now appears on both games about 0.6 seconds after the click (before: 1.2 to 1.7 seconds).
- Builds appear sooner at normal speed: the waiting time before every game applies a build now adapts to what the games measure instead of a fixed safety margin. After the first few builds of a session, a build appears about 0.8 to 1.1 seconds after the click at 1x speed (before: about 1.8 seconds) and 0.3 to 0.7 seconds at 3x, tested with simulated network delays up to 0.1 seconds each way. Tested on one PC only, not yet over the internet.

### Known gaps
Known bugs in one line: a few corner cases are untested, a world closed to the main menu is not saved first, no internet test yet.
- A world closed to the main menu with "Return to Main Menu" is not saved by FeverLink3 first (the game offers no save there; use Save Game in the pause menu before leaving).
- If a game ever has to rebuild a depot or station another way instead of copying it (seen only in a test that forces this), the games differ for a moment and a repair sync follows.
- Two players building at the very same moment has not been tested.
- Separate companies and sharing terrain edits come after this first alpha.
- Stop styles or options picked in the stop tool may arrive with default settings on the other game (not verified).
- Terrain editing, ground paint, trees and rocks are not shared yet (bulldozing a tree is refused).
- A full session between two players over the internet has not been verified since these changes.
