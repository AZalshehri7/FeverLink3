---
title: FeverLink3 changelog
---

# Changelog

The changes of each version that matter to a player or tester. Earlier test builds were folded into these versions.

## 0.2.5 (2026-10-23)

### Added
- When an update is ready, the FeverLink3 window and the pop-up have a **Save and quit** button (outside a session): it saves your world as "feverlink3-restart" and closes the game; start the game again from Steam and load that save to play the new version. Not yet seen in the game.

### Fixed
- A build the game applied at once while the session was paused (for example a change to a crossing's settings) no longer forces a resync: it was sent as if made at the moment of the pause, long before where every game stood, so the other games applied it late. It now takes its place in the host's order of the pause, at the step where it was made, and every game applies it there. Not yet seen in the game.
- In the FeverLink3 window (every tab and screen) and in the pop-up of the bar icon, the text no longer touches the edges: there is now a small margin on every side, the same as in the game's own windows.
- Roundabouts could not be built in a session: every game refused them and the player was told "Your build could not be built". The engine stores one rotation number of a roundabout as minus zero, which FeverLink3 took for a different placement; tiny rounding differences in where a construction stands no longer count, a different construction or place still does. Seen with two games on one PC.
- Re-laying or extending a street that has a bus or tram stop on it (the street tool shows the stop moved, yellow) was always cancelled with "Your build could not be shared with the other players"; the same happened to a street station whose street carries a stop. Such builds are now shared like any other. Seen with two games on one PC.
- A repair that was needed shortly after a sync (a build that failed on one game, or worlds that differed) was dropped, so the worlds stayed different until someone synced by hand (once for 8 minutes, with builds refused on one game). Now the repair waits and starts by itself as soon as the 2-minute pause between syncs is over, and worlds that differ in a changing set of places count as different. Not yet seen in the real game.
- A build that was built on a game although FeverLink3's native module reported it as not taken is now reported to the host as built, so the host sees when another game refused it and repairs the difference instead of logging "refused alike, no repair needed" (it stayed a hidden difference between the worlds). A player whose build the game showed as failed (for example cut by a sync) and that was built all the same is now told "Your build was built after all". Not yet seen in the game.
- A friend who joins through a Steam invite now has their mods compared with the host's: the host no longer warns that the player's game "could not list its mods", and the friend's part of a bug report shows their game build and mods instead of "unknown" (the game lets the window read the mod list only while a world loads, so FeverLink3 keeps that list). Seen with two games on one PC; not yet in a real invite join.

## 0.2.4 (2026-10-23)

### Added
- Support for Transport Fever 3 game build 40423 (the game's "Second Stability Update" of 2026-10-22). With an older FeverLink3 that game build ran without FeverLink3's build sync and drift fixes, and nobody was told.

### Changed
- Mods that change lines by themselves (for example Auto Line Namer, Auto Color Lines, Auto Assign Terminals) now work in a session: the host's copy of the mod makes the changes and every game applies them at the same moment, and a setting changed in such a mod's window reaches every game. The warning now appears only for a mod change FeverLink3 still cannot share. Seen with two games on one PC; a setting changed in such a mod's window not yet.

### Fixed
- On a game build FeverLink3 does not support yet, the FeverLink3 window now says "Transport Fever 3 was updated (build N); this FeverLink3 does not support it yet. An update of FeverLink3 is needed before you can play together." Hosting is refused; a player with such a game who tries to join is refused by the host, and the host's window names that player.
- The warning about another mod that changes the world on its own names the mod as the game's mod list shows it (for example "Auto Color Lines"), not its folder number. One warning per mod and session.
- A library FeverLink3's encryption uses (ctutils) is updated for a security advisory published on 2026-10-23 (RUSTSEC-2026-0385).

## 0.2.3 (2026-10-22)

### Fixed
- A road that joins or splits an existing road no longer says "Your build could not be built ... Try again" when it was in fact built on every game. The game moves a few points of such a build by a few centimetres before it applies it; FeverLink3 now still recognises it as the player's own build. A player is never told a build failed when it was applied.
- Configuring a station (adding or changing a module) right after a sync, before any other build tool was used, was refused with "Try again". It is now copied to every game like any other build.
- Every game, not only the host, keeps its world details when a sync starts, before the sync replaces its world, so a difference between the games can be traced in the logs afterwards.
- "Report a problem": when the host and a player asked for a report at nearly the same moment, the player's request could overtake the host's, and the host's own report was refused as busy. The host's own requests are now always handled in the order they were given.
- `feverlink3.exe` never passes FeverLink3's own settings on to Steam when it starts the game, and its developer start of a game waits for Steam to be running.

## 0.2.2 (2026-10-22)

### Changed
- Faster builds: a build stands on every game 1.4 s after the click at game speed x1 and 0.8 s at x4 (measured with two games on one PC). At x4 the game really runs at four times the speed now.

### Fixed
- Update messages in the pop-up and the window are shorter and no longer cut off at the edge: "Updated to <version>. Restart the game to finish." and "Update to <version> failed: <reason>. You keep <old version>." Every long text in the window and the pop-up (update, Report a problem, events) now breaks into several lines.
- After a finished update the empty `feverlink3_update` folder is removed from the game folder.

## 0.2.1 (2026-10-22)

### Fixed
- The automatic update check could make the game show 'Internal error' at start (it started a helper program that held a game file open); downloads now run inside the game, a while after it has started. 0.2.0 was withdrawn.
- Report a problem opens the report's folder and the issue page through Windows' shell instead of starting Explorer from the game.

## 0.2.0 (2026-10-22: first public beta)

A new in-game interface, a way to report a problem, a faster start of a session, and many fixes. The interface, the faster start and Report a problem were played over the internet with a friend through Steam; the fixes for road upgrades that move a town building, references after a repair sync, Steam invites and the bar icon size are covered by the automatic tests and by two game windows on one PC.

### Added
- **New in-game interface:** an icon in the game's bottom bar whose colour shows the session state (white, green, yellow, red), a pop-up above it with what matters now, and a window with tabs. Sync steps show their progress.
- **Plain messages:** one set of short, plain player messages ("Your ..." for you, "<name>'s ..." for the others). Session notices no longer fill the chat, and the events of the session are listed in the window.
- **Automatic updates:** at a game start FeverLink3 checks GitHub for a newer version, checks every file's SHA-256 against the release, gets it ready, and asks for one restart; a version that does not start is taken back. Never during a session; can be turned off in Troubleshooting. When two players have different FeverLink3 versions, the older one is told an update is available. Releases are unsigned until 1.0.0.
- **Report a problem:** one button (in the red pop-up, in the Players tab and under Troubleshooting) makes one zip with every player's logs, collected by the host over the session. The logs are cleaned of personal details on each player's own PC first, and nothing is sent anywhere by itself.

### Fixed
- Object ids stay in the same order on every game: a build the game holds without a FeverLink3 record is refused on every game, never let through on one game only.
- The terrain tools are switched off during a session (a terrain edit ran on its maker's game alone).
- A bus stop whose build moves town buildings no longer crashes the game: it is handled as FeverLink3's own build again.
- An engine error halfway through applying a build now asks for a repair sync, and a repair never copies a world that may be partly built.
- The world check also compares the order of free object ids, so such a split is found at once.
- A road upgrade that moves a town building is copied exactly now (it was refused on every game); a build refused because its copy differs now says so instead of "the world changed".
- After a repair sync, a reference to something built before it can no longer point at something new (line assignments were refused).
- A Steam invite joins by itself when a world is loaded; with no world loaded the window shows the invite with a Join button. Joining with the code cancels a waiting invite (it kept the icon yellow).
- The bar icon is drawn at the size of the game's own icons; a sync names the players still loading the world.
- Faster start of a session: a game whose world is about to be replaced no longer holds a pause. The pause at a session start is now under 2 s and the whole sync about 17 s (before: a pause of about 37 s; measured with two games on one PC).

### Changed
- README rewritten in plain words.
- A mod that changes the world on its own during a session (for example an automatic line namer) is named in a warning: its changes are not shared and cause extra syncs. Turn its automatic changes off while playing together.
- The id-order trace (test logs) names the game step and thread of every change.

### Known gaps
- The fixes for road upgrades, references after a repair sync, Steam invites and the bar icon are not yet played over the internet; the Steam invite fix needs a test between two PCs.
- The engine sometimes frees object ids in a different order on two games (5 times in a 3.5-hour session): the world check finds it within seconds and a repair sync follows. The cause is being traced.
- Terrain editing, ground paint, trees and rocks are not shared (the terrain tools are off during a session).
- Automatic updates are tested with fake releases only; the first real update is the 0.2.0 test described in docs/RELEASING.md ("Testing the auto update").
- Two players building at the same spot within about a second: one of the two builds can be refused on both games (the worlds stay equal).

## 0.1.5 (2026-10-19)

The feature-complete test version. Its last build was played over the internet with a friend through Steam; that session showed the problems fixed in 0.2.0. Before that it was tested in the automatic tests and in lab runs with two game windows on one PC.

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
