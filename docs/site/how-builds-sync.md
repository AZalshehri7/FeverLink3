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

Some builds, such as a depot with its own entrance placed over a street, cannot be rebuilt from a description, because the game decides the details itself. For those the mod can copy the game's own finished build to the other game. This is a developer test option and is **off by default**. In the real game it has been seen working for depots only; stops, stations and rail with it are not verified.

No game rule is turned off to make this work: towns grow, traffic runs and ratings change on every game as usual.
