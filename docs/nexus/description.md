# FeverLink3 Multiplayer - early connection-test build

> **Experimental, and not a playable multiplayer game yet.** This first test build has one job: to find out whether two players can reach each other through Steam. It adds a window to the game with a **Steam connection test**. Hosting and joining a game over that connection come in the next test build, and building together after that. What is verified and what is not is listed below. If you would like to help test, you are very welcome.

## What it is

FeverLink3 aims to let several players build in the same Transport Fever 3 world at the same time, each in their own copy of the game.

- **Same commands, same step.** Every action a player takes is sent to all games and applied at the same simulation step on every one of them, so the worlds stay identical without sending the world around.
- **One host.** The host decides who may do what. Friends join with a short code. The games compare checksums, and a lasting difference is repaired by loading the host's savegame everywhere.
- **Encrypted.** Everything between the games is encrypted and authenticated with a key derived from the join code.
- **Connected through Steam.** The games reach each other through Steam's peer-to-peer networking, so a friend only needs the join code and, by design, nothing has to be set up. Whether that holds on real home connections is what this test build is for.

It is a fresh implementation in Rust and Lua, with the source on GitHub under the PolyForm Noncommercial License (free for noncommercial use). It stands on two earlier projects that proved the approach, credited at the bottom. It is **not official**: it is not made, endorsed or supported by Urban Games or Paradox Interactive.

## What this test build does, and what it does not

- **Does:** adds a button called **FeverLink3** to the bar at the bottom of the screen. It opens a window with a **Steam connection test**: two players exchange Steam IDs, test the connection to each other and read the result (connected or not, how many answers, the round-trip time, and whether Steam used a direct route or its own relay).
- **Does not (yet):** host or join a game. The window's **Host a game** button answers that hosting over Steam is not finished in this build yet. Those come in the next test build.

## Status, 2026-10-10 (game build 40420)

**Seen working in the real game**

- The mod loads, and its window opens from a button in the bottom bar.
- A Steam peer-to-peer connection from the game to its own Steam ID works inside one game (6 pings answered).
- The mod can drive the game's own save and load, and picks up its session files again after a reload.
- With an earlier build of the window, whose Host button used a direct connection (since taken out of the window), hosting showed a join code and a second client (a test client, not a second copy of the game) could join.

**Not verified yet**

- The Steam connection between two different Steam accounts. This is what the test build is for; please do not assume it works over the internet until the results are in.
- Hosting and joining over Steam, and sending the host's world to a second real game.
- Building together: replicating construction and vehicle commands between two real games.
- Joining from a second real copy of the game.
- The native hooks that would hold construction commands back are switched off in this version and have never run in the real game.

## Requirements

- Transport Fever 3 on Steam, game build **40420 or 40423** (shown at the bottom right of the main menu). Each player needs their own copy, started through Steam, and the same build as the other player.
- Windows 10 or 11, 64-bit. Steam running and logged in.
- No other mod is needed. It cannot be used together with MPFever or any mod that installs its own `winhttp.dll`.

## Installation

The zip holds a testing installer, the native module (`winhttp.dll`) and the mod. The installer is a testing tool: the plan is that the mod reaches players from the game's own Mod Hub and from Nexus Mods like any other mod, with the native module as a separate download for now.

1. Close Transport Fever 3.
2. Download the zip, unzip it anywhere (Extract All) and double-click `feverlink3.exe`; a menu opens, type `1` and press Enter to install. (From a terminal opened in that folder the same is `feverlink3 install`.) Windows SmartScreen may say "Windows protected your PC" the first time, because the program is not code-signed yet: click More info, then Run anyway. Add `--dry-run` to see what it would do first.
3. It prints one line per item (the mod, `steam_appid.txt`, `winhttp.dll`, `feverlink3_path.txt`), each marked DONE, SKIP or FAILED with the reason. If a line says the Steam user folder does not exist, start the game once, close it and run the command again.
4. Start Transport Fever 3 from Steam. When you start a game, open the mod list on the settings page, find *FeverLink3 Multiplayer* and activate it. The **FeverLink3** button appears in the bar at the bottom of the screen.

Without the installer: copy `winhttp.dll` into the Transport Fever 3 game folder and the folder `mod\feverlink3` into `<Steam>\userdata\<your id>\3493540\local\mods\`. That is what the installer does for the in-game window (the other two files it writes are not used by the in-game window). This route has not been tested separately; if the button does not appear, use the installer and tell us.

## Running the connection test

Two testers, both with the game running through Steam:

1. Click **FeverLink3** in the bottom bar. Under *Steam connection test* the window shows **Your Steam ID**. Send it to the other tester, who sends you theirs. A Steam ID identifies your Steam account, so share it only with the person you test with.
2. Paste the other tester's Steam ID into **paste the other player's Steam ID** and press **Test the connection**.
3. Within about 20 seconds the window shows a line like `Test: connected, answers 6, last round trip 40 ms, direct` (or `through a Steam relay`). The other tester's window shows how many pings it received.
4. Try both directions, and send the result lines, your two countries and whether anything failed to the maintainer (Discord `azalshehri`, or GitHub issues).

If the window says "The FeverLink3 module is not running", close the game, run `feverlink3 install` again and start the game from Steam. The full guide is in the zip (`HOW-TO-PLAY.txt`) and here: https://github.com/AZalshehri7/FeverLink3/blob/main/docs/PLAYING.md

## The file winhttp.dll, safety and antivirus

The mod ships one native file, `winhttp.dll`. The game imports that name, so Windows loads it from the game folder when the game starts. It passes the game's 14 WinHTTP functions to the real Windows file unchanged and, inside the game, runs the Steam connection test (and, in later test builds, the multiplayer session) on one thread. It does not inject into other programs, does not start with Windows, needs no administrator rights, sends no telemetry and does not download or run programs. By default it installs no hooks into the game: the hook code is switched off unless a developer sets environment variables, and it has never run in the real game. The source is public, and the page below lists exactly what it does and does not do, which files it creates, how to check the hash, and how to report a false positive to Microsoft, Kaspersky, ESET and others.

The same loading technique is used by malware, and the file is **not code-signed yet**, so some antivirus programs may flag it. Code signing is planned. https://github.com/AZalshehri7/FeverLink3/blob/main/docs/SECURITY_FOR_USERS.md

## What goes over the network

- Between the players: Steam's peer-to-peer networking through the Steam library the game already runs. Steam may relay the traffic through its own servers. No server of the project is involved, and the host does not open an inbound TCP port in normal use.
- Automatic updates: once per game start FeverLink3 asks GitHub for a newer version of itself and checks every file's SHA-256 before it uses it (releases are unsigned until 1.0.0; you can turn it off in the window's Troubleshooting section).
- Nothing else: no analytics, no advertising, no account, no other web requests. The mod needs the network for one thing only: connecting the players.
- What the other tester's game receives in the connection test: short ping messages sent from your Steam account (their window only counts them). In later builds, other players will see your display name (it starts as your Windows user name; change it in the window), your game actions and chat; a host will also send the world.
- Session traffic is encrypted and authenticated by the mod itself (AES-256-GCM, key derived from the join code).

## Uninstall

Close the game and run `feverlink3 uninstall` (add `--dry-run` to see the plan). It removes the mod, our `winhttp.dll`, `feverlink3_path.txt` and, if it holds exactly the game's app id, `steam_appid.txt`. It never touches another mod's `winhttp.dll`. Optional clean-up: delete `%LOCALAPPDATA%\FeverLink3` (logs and session files), `%TEMP%\feverlink3`, and files named `feverlink3-resync-*.sav` in your save folder.

## Known issues

- Not a finished multiplayer game: this build tests the Steam connection only, and everything under "Not verified yet" above.
- The native hooks are switched off (only a developer can turn them on) and have never run in the real game, so construction commands are not held back to the same moment on every game yet.
- Only game builds 40420 and 40423 are known. On another build the FeverLink3 window says that an update of FeverLink3 is needed; hosting is refused and a host refuses that player's join. If a game update changes the web functions the game imports, the game may fail to start while `winhttp.dll` is installed; uninstalling fixes it.
- Not code-signed: SmartScreen and some antivirus programs warn. Windows 11 Smart App Control in its "On" setting may block the unsigned DLL; this has not been tested.
- Another mod's `winhttp.dll` conflicts with ours; the installer refuses to overwrite it.
- The window is English only. A bug in the native module can crash the game (it aborts on an internal error).
- The installer's double-click menu has been tested on a fake PC and a fake Steam tree, not yet on a clean PC by someone else.

## FAQ

**Does it work?** Partly; see the status above. This build tests the Steam connection; hosting and joining over Steam are the next test build, and playing together in two real games has not been verified yet.

**How do the players connect?** Through Steam's peer-to-peer networking. By design nothing has to be set up. It is being tested: inside one game it is proven, between two different accounts it has not been tried yet.

**Is winhttp.dll safe? My antivirus flags it.** See the security page above. Check the hash against the release notes, and report false positives; the page lists where.

**Windows says "Windows protected your PC".** SmartScreen reacts to a new, unsigned program. More info, then Run anyway. It appears once for the installer.

**Is it official?** No. It is an unofficial community project.

**Can I use it with MPFever?** No; both use the `winhttp.dll` slot, and our installer will not overwrite another mod's file.

**Will it break my saves?** The mod does nothing until you host or join. Later builds will make every game load the host's world and write `feverlink3-resync-<number>.sav` files into your save folder, so back up your saves first.

**Can I play on my own with it installed?** Yes, by design: without a session the game plays as it always does.

**Which game version?** Builds 40420 and 40423 for now. After a game update the module may need an update; if the game fails to start, run `feverlink3 uninstall` and tell us your build.

**Consoles, Mac, Linux, Epic or GOG?** No. The native part is a Windows DLL and the installer finds the game through Steam.

**Will it be in the in-game Mod Hub or on the Steam Workshop?** Transport Fever 3 has no Steam Workshop. The plan is to offer the mod in the game's own Mod Hub (which runs on mod.io) and here on Nexus Mods, with the native module as a separate download for now. Whether the Mod Hub accepts a mod that needs a separate native component is still being checked: its published rules allow only the official scripting API. Details: https://github.com/AZalshehri7/FeverLink3/blob/main/docs/WORKSHOP.md

**How is it different from MPFever?** It is a separate, fresh implementation (Rust and Lua) built on the same idea; both projects are credited below. They cannot be installed together.

**How do I help or report a bug?** Discord `azalshehri`, or https://github.com/AZalshehri7/FeverLink3/issues. Please attach `%LOCALAPPDATA%\FeverLink3\native.log` and `live\mod.log`.

## Changelog

**Connection-test build (2026-10-10)**

- In-game window with a button in the bottom bar and a Steam connection test (your Steam ID, test the connection to another player's Steam ID, result with round-trip time and route).
- Hosting and joining over Steam are not finished in this build: the Host button says so.
- Under the window, for later builds: the lockstep core (actions stamped and applied at the same step on every game, world checksums, repair by loading the host's savegame, a permissions model, command schemas for everyday operations), tested with simulated games only.
- Encrypted sessions (AES-256-GCM, key from the join code); the host decides who may send what.
- Native module `winhttp.dll`: passes the game's 14 WinHTTP functions through and runs the in-game controller. Its hook code is switched off by default and has never run in the real game.
- Testing installer: a double-click menu (install or repair, uninstall, compare, start the game, open the logs, copy diagnostics) and the commands `feverlink3 install`, `uninstall`, `doctor`, with `--dry-run`.
- Unsigned. Seen working in the real game: see the status above.

## Credits and permissions

- **Author:** Abdulaziz Alshehri (Discord `azalshehri`). Source and issues: https://github.com/AZalshehri7/FeverLink3
- **TpF2 Multiplayer** by silver2127 (Transport Fever 2, MIT): https://silver2127.github.io/tpf2-multiplayer/ - the replication modes, the "no entity ids on the wire" rule, the join-code and sealed-transport trust model and the host-included resync round.
- **MPFever** by dkdknight / arthur (Transport Fever 3, MIT): https://github.com/dkdknight/MPFever - the first lockstep co-op on this game: the proxy-DLL approach, the single-worker script pool fix, deterministic `math.random`, and the per-build game addresses and API names that FeverLink3 reuses with credit in the files that contain them.
- Both licences are reproduced in THIRD_PARTY_NOTICES.md in the zip. FeverLink3 contains no files or assets from other authors.
- Rust libraries for cryptography, serialisation and Windows access, under their own licences (list in `Cargo.lock`).
- **Transport Fever 3** belongs to Urban Games and Paradox Interactive. This project is not affiliated with them.
- **AI assistance:** much of the code and documentation was written with Claude (Anthropic) under the maintainer's direction. The claims on this page rest on the tests and real-game checks listed in the status.
- **Licence:** [PolyForm Noncommercial License 1.0.0](https://polyformproject.org/licenses/noncommercial/1.0.0/). You may use, copy, study, modify and share FeverLink3 for any noncommercial purpose, as long as every copy keeps the line "Required Notice: Copyright (c) 2026 Abdulaziz Alshehri (https://github.com/AZalshehri7)" and comes with the licence terms or their web address (the `LICENSE` file in the zip). **Commercial use needs a separate commercial licence** from the author: ask on Discord, `azalshehri`. This is source available, not "open source" in the sense of the Open Source Initiative. The parts adapted from TpF2 Multiplayer and MPFever stay under their own MIT licences. It comes without warranty.
- **Permissions:** uploading elsewhere, modifying and converting are allowed for noncommercial purposes under the same terms. Use in sold or monetised mods, and use in mods that ask for donation points, is not allowed without a commercial licence. No donation points on this page.
