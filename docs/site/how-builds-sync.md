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

Many builds cannot be rebuilt exactly from a description, because the game decides details itself (a depot's entrance over a street, a stop's exact place). So the mod copies the game's own build from the game you clicked in, and every game, yours included, applies that same copy at the same moment. This is **on by default** in the test builds. In two-window tests on one PC it has been seen working for roads (also through houses), bus stops (also a stop replacing a stop), road depots over houses and a train station.

Known limits right now:
- A build that touches something built earlier in the same session can fail on the other game; the games then repair themselves with a sync and that build is lost (seen once, after a long automated test).
- Adding a module to a station afterwards (the station's Configure window) still goes the old way: it reaches the other game, but a moment early on your own game, and the money is corrected by the host.
- Opening the game's pause menu during a session switches the build sync off on that game, and the player can drop out of the session.
- Not tested with this yet: rail track and splitting a track, harbours, two players building at the very same moment, and play between two players over the internet.

No game rule is turned off to make this work: towns grow, traffic runs and ratings change on every game as usual.
