---
title: FeverLink3 changelog
---

# Changelog

The changes of each version that matter to a player or tester. Earlier test builds were folded into these versions.

## 0.2.1 (2026-10-22)

### Fixed
- The automatic update check could make the game show 'Internal error' at start (it started a helper program that held a game file open); downloads now run inside the game, a while after it has started. 0.2.0 was withdrawn.
- Report a problem opens the report's folder and the issue page through Windows' shell instead of starting Explorer from the game.

## 0.2.0 (2026-10-22: first public beta)

Fixes from two internet tests with a friend (2026-10-20 and 2026-10-21), a new in-game interface, a way to report a problem, and a faster start of a session. The interface, the faster start and Report a problem were played over Steam with a friend on 2026-10-21; the fixes from that evening (listed last under Fixed) are tested in the automatic tests and in two game windows on one PC.

### Added
- **New in-game interface:** an icon in the game's bottom bar whose colour shows the session state (white, green, yellow, red), a pop-up above it with what matters now, and a window with tabs. Sync steps show their progress.
- **Plain messages:** one set of short, plain player messages ("Your ..." for you, "<name>'s ..." for the others). Session notices no longer fill the chat, and the events of the session are listed in the window.
- **Automatic updates:** at a game start FeverLink3 checks GitHub for a newer version, checks every file's SHA-256 against the release, gets it ready, and asks for one restart; a version that does not start is taken back. Never during a session; can be turned off in Troubleshooting. When two players have different FeverLink3 versions, the older one is told an update is available. Releases are unsigned until 1.0.0.
- **Report a problem:** one button (in the red pop-up, in the Players tab and under Troubleshooting) makes one zip with every player's logs, collected by the host over the session. The logs are cleaned of personal details on each player's own PC first, and nothing is sent anywhere by itself.

### Fixed
- Object ids stay in the same order on every game: a build the game holds without a FeverLink3 record is refused on every game, never let through on one game only.
- The terrain tools are switched off during a session (a terrain edit ran on its maker's game alone).
- A crash from the friend test: a bus stop whose build moves town buildings is handled as FeverLink3's own build again.
- An engine error halfway through applying a build now asks for a repair sync, and a repair never copies a world that may be partly built.
- The world check also compares the order of free object ids, so such a split is found at once.
- From the 2026-10-21 test: a road upgrade that moves a town building is copied exactly again (it was refused on every game); a build refused because its copy differs now says so instead of "the world changed".
- From the 2026-10-21 test: after a repair sync, a reference to something built before it can no longer point at something new (line assignments were refused).
- From the 2026-10-21 test: a Steam invite joins by itself when a world is loaded; with no world loaded the window shows the invite with a Join button. Joining with the code cancels a waiting invite (it kept the icon yellow).
- From the 2026-10-21 test: the bar icon is drawn at the size of the game's own icons; a sync names the players still loading the world.
- Faster start of a session: a game whose world is about to be replaced no longer holds a pause. The pause at a session start is now under 2 s and the whole sync about 17 s (before: a pause of about 37 s; measured with two games on one PC).

### Changed
- README rewritten in plain words.
- A mod that changes the world on its own during a session (for example an automatic line namer) is named in a warning: its changes are not shared and cause extra syncs. Turn its automatic changes off while playing together.
- The id-order trace (test logs) names the game step and thread of every change.

### Known gaps
- The 2026-10-21 fixes are not yet played over Steam; the Steam invite fix needs a two-PC test.
- The engine sometimes frees object ids in a different order on two games (5 times in a 3.5-hour session): the world check finds it within seconds and a repair sync follows. The cause is being traced.
- Terrain editing, ground paint, trees and rocks are not shared (the terrain tools are off during a session).
- Automatic updates are tested with fake releases only; the first real update is the 0.2.0 test described in docs/RELEASING.md ("Testing the auto update").
- Two players building at the same spot within about a second: one of the two builds can be refused on both games (the worlds stay equal).

## 0.1.5 (2026-10-19)

The feature-complete test version. Its last build was played over the internet with a friend on 2026-10-20; that test found the problems fixed in 0.2.0. Before that it was tested in the automatic tests and in lab runs with two game windows on one PC.

### Added
- **Steam invites:** invite friends with Steam's own invite dialog or from the Steam friends list (Invite to Game / Join Game). The host lets each friend in; the session key is never shown to other friends.
- **Signed updates:** releases are signed in a protected GitHub environment, and `feverlink3 update` installs only a signed release, with a rollback if the new version does not start.
- **Mod check:** a player whose game build, FeverLink3 version or mods (the ones the host's world uses) differ is told what differs before the world is sent.
- **Stall catch-up:** if one player's game stops answering for 3 s (an autosave, a slow PC), the others play on and that game catches up afterwards with the same actions. A player who loses the connection for less than a minute comes back without a reload.
- The bulldozer's refund for something just built works in multiplayer (the same money on every game).
- Building during a pause: every game applies the builds made in the pause in the same order.
- Line edits (cargo filters, load mode, stops, waypoints) work while the game runs; an edit that cannot be shared is refused on every game with a message.
- When FeverLink3 cannot carry a build to the other players, the tool says so in red before you click.
- Every build writes one result line to the logs, which makes problem reports much more useful.

### Fixed
- Several ways the native part could crash the game are closed (errors inside the game's build functions, a double free, an empty held build, a damaged build from another player).
- No build is applied late or on one game only: every game waits for the others' input before it passes a step, and window actions (vehicles, lines, loans) are applied at the same moment on every game.
- Builds are captured before the game applies them for every build tool; roads over a town's land parcels, airports with Configure items, runways and stations over town buildings are carried.
- After a repair sync the game starts by itself, and buying vehicles and editing lines keep working.
- The game's own random choices (town development, track reservations) no longer depend on how long a game stood still waiting.
- The world check runs about every 5 s, spread over many small steps, instead of a long stall about once a minute, and the checks are really compared.
- Smoother pacing: the game that is behind catches up a little faster instead of the other one standing still; no stutter after a line is deleted.
- Ending a session returns the game to normal single-player play; the logs rotate instead of growing forever.

### Changed
- The mod's folder is now `mods\release\feverlink3` (was `feverlink3_1`). **Delete the old `feverlink3_1` folder** when you update, or the game loads two copies.
- The mod follows the official Transport Fever 3 modding documentation for every game API call it makes.
- A build appears on both games about 0.4 to 1.6 s after the click at 3x speed (median about 0.7 s, on one PC).

### Known gaps
- Steam invites not tested between two PCs.
- Unproven in real play: the fix for the freeze when cloning vehicles, lines with waypoints, the builder's engine check on big stations.
- In 2 of 8 lab starts one of the two games did not finish loading the world (only with an extra diagnostic log switched on). If it happens, restart that game.
- The speed icon may flicker briefly while the game waits for the other player.

## 0.1.0 (2026-10-14)

The first working multiplayer session: builds are copied natively from the game's own build command and applied at the same moment on every game. Tested with two games on one PC (a scripted sweep and builds made by clicking with the real tools), not yet with two players over the internet.

### Added
- Roads, junctions, bridges, tunnels, traffic lights, crosswalks and lane changes built by one player show up identically on the other games.
- Rail track, switches, signals and electrification; bus stops; stations and their modules (Configure); road and rail depots, airfields, harbours and the underground station entrance.
- Bulldozing, also of houses, stations and harbours.
- Lines, vehicles bought at a depot, renames, loans, selling a vehicle and game speed work between the games.
- The world check notices when the games differ (also when they number their objects differently) and repairs it with the host's save.
- Detailed logs for testing: every build, its result and its timing from the click to the moment every game applied it.

### Fixed
- A build that the other game cannot make is refused there with a reason in the chat, never applied on one game only, and the tool stays usable.
- The host's game crashed during fast building: the build sync was switched off by mistake, and a build naming objects the other game did not have crashed it. Both fixed (17 fast builds from both players in two windows, no crash).
- Opening the pause menu no longer drops a player or switches the build sync off; a build made right before it is not lost.
- A guest whose session ends while its world is open saves the world first.
- Very large money amounts (billions) no longer break the mod.

### Known gaps
- No internet test yet.
- Terrain editing, ground paint, trees and rocks are not shared (bulldozing a tree is refused).
- Two players building at the very same moment was not tested.
- A world closed with "Return to Main Menu" is not saved by FeverLink3 first (use Save Game in the pause menu before leaving).
