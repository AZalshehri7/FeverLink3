---
title: FeverLink3 FAQ
---

# Frequently asked questions

Last updated 2026-10-22, for version 0.2.1. [Back to the start page](../index.html)

## The project

**What is FeverLink3?**
A mod that lets several players build in one Transport Fever 3 world at the same time, each in their own game. It is built into the game: there is no separate launcher or program to run. You start Transport Fever 3 from Steam as usual, and hosting, joining, chat, sync and updates all live in the game's bottom bar and the FeverLink3 window. It is in beta, its source code is public, and it is unofficial.

**Does it work?**
Yes, for two players over Steam. In two tests with a friend over the internet (2026-10-20 and 2026-10-21, about 3.5 hours) the players joined with a join code and built together: roads, rail, stations, stops, depots, lines and vehicles. Towns grew as in single player, speed, pause and chat were shared, and when the worlds drifted apart the repair sync brought them back (about 15 s for an 80 MB world over Steam). The [status page](status.html) lists what has been seen in the real game and what not yet.

**What is the licence? Can I use it commercially?**
FeverLink3 is licensed under the [PolyForm Noncommercial License 1.0.0](https://github.com/AZalshehri7/FeverLink3/blob/main/LICENSE): you may use, study, change and share it for any noncommercial purpose, keeping the copyright notice. That makes it source available, not "open source" in the sense of the Open Source Initiative. For commercial use, ask on Discord (`azalshehri`). The parts adapted from earlier projects stay under their own MIT licences ([notices](https://github.com/AZalshehri7/FeverLink3/blob/main/THIRD_PARTY_NOTICES.md)).

**Who makes it? Is it official?**
It is made by Abdulaziz Alshehri (Discord `azalshehri`). It is not made, endorsed or supported by Urban Games or Paradox Interactive.

**Is Transport Fever 2 supported?**
No, only Transport Fever 3.

## Playing

**What do I need?**
Windows 10 or 11 (64-bit), Transport Fever 3 on Steam (one copy per player, started from Steam with Steam running), and the same game version as the host. macOS, Linux and consoles are not supported.

**Does every player install it?**
Yes, every player installs FeverLink3. A player who joins does not need to prepare a world: the host's world is sent to them.

**How do the players connect?**
Through Steam. No ports, no router settings, no IP addresses: the host sends a join code and the friend pastes it into the FeverLink3 window. Treat the code like a password for that session.

**Can I join from a Steam invite?**
The host can invite friends from Steam's friends list, and the host lets each friend in. Joining from an invite by itself had a bug that is fixed in 0.2.0, but the fix has not yet been tested between two PCs. The join code always works.

**How many players?**
The tests with real games were with two players. More are planned for in the design but have not been played yet.

**Can I use other mods?**
Yes, but every player needs the same mods as the host, because every game runs the same simulation. Mods that change the world on their own during a session (for example an automatic line namer) cause extra repair syncs, because their changes are not shared: turn their automatic changes off while playing together. FeverLink3 warns about such mods (this warning has not yet been seen in a real game). FeverLink3 cannot be used together with another mod that also places a `winhttp.dll` in the game folder.

**Will it break my savegames?**
The mod does nothing until you host or join. In a session every game loads the host's world, and a sync writes files named `feverlink3-resync-<number>.sav` into your save folder. FeverLink3 is in beta: keep backups of the saves you play with.

**Can I still play on my own with it installed?**
Yes. Without a session the game plays as it always does.

**Are terrain editing, ground paint and trees shared?**
Not yet. The terrain tools are switched off during a session. Everything else in the game works as usual.

**What happens when the game updates?**
Every player in a session needs the same game version. If a game update changes what FeverLink3 depends on, it switches its hooks off instead of risking your game; an update of FeverLink3 follows. If the game does not start at all after an update, delete `winhttp.dll` from the game folder.

**Separate companies? A dedicated server?**
Not yet. All players share one company for now. See the [roadmap](../ROADMAP.html).

## Updates

**How do updates work?**
Once per game start (never during a session) FeverLink3 asks GitHub whether a newer version exists. If so, it downloads it inside the game, only from the official GitHub releases over HTTPS, and checks every file's SHA-256 checksum against the release. One restart of the game finishes the update. This was tested on the maintainer's PC with the real 0.2.1 release. If a new version does not start, FeverLink3 is meant to put the previous version back (not yet seen in a real game).

**Can I turn automatic updates off?**
Yes: in the FeverLink3 window, under **Troubleshooting**. To update by hand, close the game and copy the new files over the old ones.

## Safety

**What is `winhttp.dll`? Is it safe?**
It is the part of FeverLink3 that the game loads at start, with public source code. [Security for players](../SECURITY_FOR_USERS.html) explains what it does and does not do, and how to check it.

**My antivirus or Windows warns about it.**
The DLL is new and not code-signed yet (signing is planned for 1.0.0), so some antivirus programs and Windows may warn about it. You can check that your download is the official one: compare the zip's SHA-256 with the `.sha256` file on the release page. [Security for players](../SECURITY_FOR_USERS.html) shows how, and how to report a false positive.

**Does it collect data?**
No telemetry, no accounts, no ads. See the [privacy notes](privacy.html).

**Is joining a session safe?**
Only join people you trust. Your game loads the host's world, and a Transport Fever savegame can carry game scripts, as a mod can.

## Getting it

**Where do I download it?**
From the [GitHub releases page](https://github.com/AZalshehri7/FeverLink3/releases/latest). Each release is one zip with `winhttp.dll` and the mod. The [start page](../index.html#quick-start) shows how to install it.

**Is it on the Steam Workshop or the Mod Hub?**
Transport Fever 3 has no Steam Workshop. Pages on the game's Mod Hub and on Nexus Mods are planned.

**How do I uninstall it?**
Close the game and delete `winhttp.dll` and the folder `mods\release\feverlink3` from the game's folder (in Steam: right-click Transport Fever 3, Manage, Browse local files).

## Problems

**How do I report a bug?**
Press **Report a problem** in the FeverLink3 window (also in the red pop-up of the bar icon). In a session it collects the logs of every player into one zip, with personal data removed, and opens a GitHub issue form: attach the zip and say what happened. Nothing is sent anywhere by itself. You can also open a [GitHub issue](https://github.com/AZalshehri7/FeverLink3/issues) directly or write on Discord (`azalshehri`).
