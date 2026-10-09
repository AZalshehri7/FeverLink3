---
title: FeverLink3 FAQ
---

# Frequently asked questions

Last updated 2026-10-10, for the test build 0.2.0-test1. [Back to the start page](../index.html).

## The project

**What is FeverLink3?**
An unofficial, open-source (MIT) mod for Transport Fever 3 that aims to let several players build in one world at the same time, each in their own copy of the game. It is an early test build.

**What is the first test build for?**
It has one job: to find out whether two players can reach each other through Steam. Its in-game window has a Steam connection test, which two testers run against each other. Hosting and joining a game over that connection come in the next test build, and building together after that.

**Does it work?**
Partly, and we say exactly which part. Verified in the real game (build 40420, 2026-10-09): the mod loads, its window opens from a button in the bottom bar, and the mod can drive the game's own save and load. With an earlier build of the window, whose Host button used a direct connection (since taken out of the window), hosting showed a join code and a test client (not a second copy of the game) could join. On 2026-10-10 the game opened a Steam peer-to-peer connection to its own Steam ID inside one process and 6 pings were answered. Not verified yet: the Steam connection between two different accounts, hosting and joining over Steam (not finished), building together between two real games, sending the world to a second real game, and joining from a second real copy of the game. See the [status page](status.html).

**Who makes it? Is it official?**
It is made by Abdulaziz Alshehri (Discord `azalshehri`). It is not made, endorsed or supported by Urban Games or Paradox Interactive.

**Is it a fork of MPFever or TpF2 Multiplayer?**
No, it is a fresh implementation written in Rust and Lua, but it stands on both and credits them: [TpF2 Multiplayer](https://silver2127.github.io/tpf2-multiplayer/) by silver2127 (Transport Fever 2) and [MPFever](https://github.com/dkdknight/MPFever) by dkdknight (arthur, Transport Fever 3). Both are MIT-licensed. The approach (every game applies the same commands at the same step, a host as the reference) is theirs as much as ours.

**Can I use it together with MPFever?**
No. Both use the `winhttp.dll` slot in the game folder, and the FeverLink3 installer will not overwrite another mod's file. Pick one.

**Is Transport Fever 2 supported?**
No. For Transport Fever 2 see TpF2 Multiplayer above.

## Playing

**What do I need?**
Windows 10 or 11 (64-bit); Transport Fever 3 on Steam, one copy per player, started through Steam with Steam running; the same game version as the host (the test build was made for build 40420). Consoles, macOS and Linux are not supported: the native part is a Windows DLL.

**How do the players connect?**
Through Steam's peer-to-peer networking. The join code is all a friend needs, and by design nothing has to be set up. This is being tested: inside one game it is proven, between two different Steam accounts it has not been tried yet, so nobody should assume it works over the internet today. If you want to help, [Playing FeverLink3](../PLAYING.html) describes the in-game connection test.

**How many players?**
The design is for several players. Only two-person tests are planned first, and none has been done between two real games yet.

**Does everyone need the mod?**
Yes, every player installs it. A player who joins will not need to prepare a world: the host's world is meant to be sent to them (hosting and joining over Steam are the next test build).

**Can I use other mods?**
Not tested. All games must run the same simulation, so every player should have the same mods as the host.

**Will it break my savegames?**
The mod does nothing until you host or join. A session makes every game load the host's world, and a sync writes files named `feverlink3-resync-<number>.sav` into your save folder. Back up your saves before the first session.

**Can I still play on my own with it installed?**
Yes, by design. Without a session the game plays as it always does. The native part still sits in the game folder and runs one small background thread.

**What happens when the game updates?**
The native module recognises game build 40420. If an update changes which Windows web functions the game imports, the game may fail to start while the module is installed. Run `feverlink3 uninstall` and tell us your build number.

**Is there a dedicated server? Separate companies?**
Not yet. Separate companies are planned for 0.6 and a dedicated server for 1.0; see the [roadmap](../ROADMAP.html).

## Safety

**Is it safe? What is `winhttp.dll`?**
It is a small open-source proxy DLL that Windows loads when the game starts. It passes the game's 14 WinHTTP functions to the real Windows file unchanged and, inside the game, runs the Steam connection test (and, in later builds, the multiplayer session) on one thread. [What is in winhttp.dll, and how to check it](../SECURITY_FOR_USERS.html) explains every part, lists the files FeverLink3 creates, and says what is not verified.

**My antivirus flags it.**
The same page tells you how to verify the file with a hash and how to report a false positive to Microsoft, Kaspersky, ESET and others. The file is not code-signed yet; the plan is to apply to a free code-signing program for open-source projects once there is a release ([plan](../SIGNING.html)).

**Windows says "Windows protected your PC".**
That is SmartScreen reacting to an unsigned, new program. More info, then Run anyway. It appears once for `feverlink3.exe`.

**Does it collect data?**
No telemetry, no accounts, no ads. See the [privacy notes](privacy.html) for what the other players in a session receive.

**Is joining a session safe?**
Join only people you know. Your game loads the host's world, and a Transport Fever savegame can carry game scripts, as a mod can. Treat the join code like a password for that session.

## Getting it

**Where do I download it?**
From the [GitHub releases page](https://github.com/AZalshehri7/FeverLink3/releases). The first test build, 0.2.0-test1, is a pre-release containing a testing installer, the native module and the mod. A Nexus Mods page is planned.

**Will it come to the in-game Mod Hub or the Steam Workshop?**
Transport Fever 3 has no Steam Workshop (the developers' support page says so). The plan is to offer the mod in the game's own Mod Hub (which runs on mod.io) and on Nexus Mods like any other mod, with the native module as a separate download for now, and the installer then only for testing. Whether the Mod Hub accepts a mod that needs a separate native component is still being checked: its published rules allow only the official scripting API and no executable files. [The plan](../WORKSHOP.html) lists the options and the open questions.

**How do I uninstall?**
Close the game and run `feverlink3 uninstall` ([Playing FeverLink3](../PLAYING.html)). The optional clean-up of logs and sync files is described in the [security page](../SECURITY_FOR_USERS.html).

**How do I report a bug or help test?**
Discord `azalshehri`, or [GitHub issues](https://github.com/AZalshehri7/FeverLink3/issues). Logs to attach are listed at the end of [Playing FeverLink3](../PLAYING.html).
