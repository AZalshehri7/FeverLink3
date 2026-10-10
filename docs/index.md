---
title: FeverLink3
---

# FeverLink3

**Cooperative multiplayer for Transport Fever 3.** The goal: several players build in one world at the same time, each in their own copy of the game. Source available (PolyForm Noncommercial, free for noncommercial use), Windows and Steam, unofficial.

> **Early test build, not yet a playable multiplayer game.** Last updated 2026-10-10. The first test build has one job: to find out whether two players can reach each other through Steam. Its in-game window has a Steam connection test; hosting and joining a game over that connection come in the next test build. The Steam connection between two different accounts, building together between two real games, and sending the world between two real games have **not** been verified yet. [What is verified and what is not](site/status.html).

[Quick start](#quick-start) · [Download](#download) · [FAQ](site/faq.html) · [Status](site/status.html) · [Security and antivirus](SECURITY_FOR_USERS.html) · [Roadmap](ROADMAP.html) · [Architecture](ARCHITECTURE.html) · [Protocol](PROTOCOL.html) · [Security review](SECURITY.html) · [Source on GitHub](https://github.com/AZalshehri7/FeverLink3)

## What it is

Transport Fever 3 has no network code. FeverLink3 adds a small mod and a small native module so that several players can share one world. Every action a player takes is sent to everyone and applied at the same simulation step on every game, so the worlds stay identical without sending the world around. One player is the host and decides who may do what; the others join with a code.

- **Commands, not state.** Every game applies every player's action at the same step, the one where the player clicked included.
- **Host authority.** The games compare world checksums; a lasting difference is repaired by loading the host's savegame everywhere.
- **Sealed transport.** The join code carries the session secret; everything on the wire is encrypted and authenticated.
- **Connecting the players.** Through Steam's peer-to-peer networking, so a player only has to share a join code and by design has nothing to set up. Inside one running game the Steam connection is proven; the first test between two different Steam accounts is what the current test build is for.

The full design is in the [architecture document](ARCHITECTURE.html) and the [protocol notes](PROTOCOL.html).

## Quick start

For the test build, once it is published (full guide: [Playing FeverLink3](PLAYING.html)):

1. Download and unzip the release, close Transport Fever 3, and copy `winhttp.dll` and the `mods` folder from the zip into the game's folder (in Steam: right-click Transport Fever 3, Manage, Browse local files). There is no installer and no program to run.
2. Start Transport Fever 3 from Steam and activate the mod *FeverLink3 Multiplayer* when starting a game.
3. Click **FeverLink3** in the bar at the bottom of the screen. The host clicks **Host a game**, copies the join code and sends it to a friend, who pastes it and presses **Join**.
4. For connection problems the window also has a **Steam connection test**.

Each player needs their own copy of the game and the same game version (the test build was made for build 40420). To remove everything, close the game and run `feverlink3 uninstall`. The installer is a testing tool: the plan is that the mod reaches players from the in-game Mod Hub or Nexus Mods, with the native module delivered separately for now ([plan](WORKSHOP.html)).

## Download

Releases are published at [github.com/AZalshehri7/FeverLink3/releases](https://github.com/AZalshehri7/FeverLink3/releases). The first test build, **0.2.0-test1**, is a pre-release; until it has been published that page may be empty. Every release comes with a zip, a `.sha256` file and a `manifest.json`, so you can check what you downloaded: see [What is in winhttp.dll, and how to check it](SECURITY_FOR_USERS.html). A Nexus Mods page is planned.

## FAQ

**Does it work?** Partly. The in-game window opens, and inside one game the Steam connection test works. Hosting and joining over Steam are not finished, and playing together in two real games has not been verified. The [status page](site/status.html) lists every item with its date.

**How do the players connect?** Through Steam's peer-to-peer networking. The join code is all a friend needs, and by design nothing has to be set up. This is being tested: inside one game it is proven, between two different accounts it has not been tried yet, so nobody should assume it works over the internet today.

**Is the `winhttp.dll` file safe?** It is a small proxy, with public source code, that Windows loads for the game. [This page](SECURITY_FOR_USERS.html) lists exactly what it does and does not do, how to verify it and how to report an antivirus false positive. It is not code-signed yet.

**Is it official?** No. It is not made, endorsed or supported by Urban Games or Paradox Interactive.

**Why is it not on the Steam Workshop?** Transport Fever 3 has none. The plan is to offer the mod in the game's own Mod Hub (which runs on mod.io) and on Nexus Mods, with the native module as a separate download for now. Whether the Mod Hub accepts a mod that needs a separate native component is still being checked: its published rules allow only the official scripting API. [The plan](WORKSHOP.html) has the details.

More answers: the [full FAQ](site/faq.html) and the [privacy notes](site/privacy.html).

## Credits

FeverLink3 is a fresh implementation, not a fork, but it stands on two projects that proved the model and documented their findings. Both are MIT-licensed; their licences are in [THIRD_PARTY_NOTICES.md](https://github.com/AZalshehri7/FeverLink3/blob/main/THIRD_PARTY_NOTICES.md).

- [TpF2 Multiplayer](https://silver2127.github.io/tpf2-multiplayer/) by silver2127, for Transport Fever 2 ([source](https://github.com/silver2127/tpf2-multiplayer)): the three replication modes, the "no entity ids on the wire" rule, the join-code and sealed-transport trust model, and the host-included resync round.
- [MPFever](https://github.com/dkdknight/MPFever) by dkdknight (arthur), for Transport Fever 3: the first lockstep co-op on this game, with the proxy-DLL approach, the single-worker script pool fix and deterministic `math.random`.

If you are the author of either and want the wording changed, open an issue.

## Contact

Discord: `azalshehri`. Bugs and requests: [GitHub issues](https://github.com/AZalshehri7/FeverLink3/issues). Want to be in the first test sessions? Message me on Discord.

## Licence

[PolyForm Noncommercial 1.0.0](https://github.com/AZalshehri7/FeverLink3/blob/main/LICENSE): free for noncommercial use. Commercial use needs a separate license: message `azalshehri` on Discord.
