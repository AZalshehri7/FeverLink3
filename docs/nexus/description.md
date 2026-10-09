# FeverLink3 Multiplayer - early test build 0.2.0-test1

> **Experimental. Please read this first.** This is the first public test of an open-source multiplayer mod. It is **not yet a playable multiplayer game**: the in-game window works and a session can be hosted from it, but building together between two real games has not been verified yet. The full list is below. If you want a finished co-op experience, wait for a later version. If you would like to help test, you are very welcome.

## What it is

FeverLink3 aims to let several players build in the same Transport Fever 3 world at the same time, each in their own copy of the game.

- **Same commands, same step.** Every action a player takes is sent to all games and applied at the same simulation step on every one of them, so the worlds stay identical without sending the world around.
- **One host.** The host decides who may do what. Friends join with a short code. The games compare checksums, and a lasting difference is repaired by loading the host's savegame everywhere.
- **Encrypted.** Everything between the games is encrypted and authenticated with a key derived from the join code.
- **Connected through Steam.** The games reach each other through Steam's peer-to-peer networking, and a relay server is being built as a fallback. The join code is all a friend needs. This part is still being tested; see the status.

It is a fresh implementation in Rust and Lua, MIT licensed, with the source on GitHub. It stands on two earlier projects that proved the approach, credited at the bottom. It is **not official**: it is not made, endorsed or supported by Urban Games or Paradox Interactive.

## Status, 2026-10-10 (game build 40420)

**Seen working in the real game**

- The mod loads, and its window opens from a button in the bottom bar.
- "Host a game" starts a session inside the game and shows a join code.
- A second client (a test client, not a second copy of the game) can join that session by its code.
- The mod can drive the game's own save and load, and picks up its session files again after a reload.
- A Steam peer-to-peer connection from the game to its own Steam ID works inside one game (6 pings answered).

**Not verified yet**

- Building together: replicating construction and vehicle commands between two real games.
- Sending the host's world to a second real game and loading it there.
- Joining from a second real copy of the game.
- The Steam connection between two different Steam accounts. The first test is still to come, so this is "being tested", and please do not assume it works over the internet yet. The relay fallback is being built.
- The native hooks that would hold construction commands back are not part of this version.

## Requirements

- Transport Fever 3 on Steam, game build **40420** (shown at the bottom right of the main menu). Every player needs their own copy and the same build as the host.
- Windows 10 or 11, 64-bit. Steam running and logged in.
- No other mod is needed. It cannot be used together with MPFever or any mod that installs its own `winhttp.dll`.

## Installation

1. Close Transport Fever 3.
2. Download the zip and unzip it anywhere. Open a terminal in that folder (type `cmd` into the folder's address bar and press Enter) and run `feverlink3 install`. Windows SmartScreen may say "Windows protected your PC" the first time, because the program is not code-signed yet: click More info, then Run anyway. Add `--dry-run` to see what it would do first.
3. It prints one line per item (the mod, `steam_appid.txt`, `winhttp.dll`, `feverlink3_path.txt`), each marked DONE, SKIP or FAILED with the reason. If a line says the Steam user folder does not exist, start the game once, close it and run the command again.
4. Start Transport Fever 3 from Steam. When you start a game, open the mod list on the settings page, find *FeverLink3 Multiplayer* and activate it. A button called **FeverLink3** appears in the bar at the bottom of the screen.

Without the installer: copy `winhttp.dll` into the Transport Fever 3 game folder and the folder `mod\feverlink3_1` into `<Steam>\userdata\<your id>\3493540\local\mods\`. That is what the installer does for the in-game window (the other two files it writes are not used by the in-game window). This route has not been tested separately; if the button does not appear, use the installer and tell us.

## Using it

**Host:** click FeverLink3 in the bottom bar, check your name, click **Host a game**. The window shows a join code: click into it, press Ctrl+A and Ctrl+C, and send it to your friends. Press **Start the session**. **Sync everyone to my world** appears once the session has started. **Stop hosting** ends it.

**Join:** start the game with the mod activated, click FeverLink3, paste the code into the field, press **Join**, and wait for the host to start the session. The host's world is meant to be sent to you then.

Only join people you know: your game loads the host's world, and a Transport Fever savegame can carry game scripts, as a mod can. Treat the join code like a password for that session.

**Help test the connection:** while you are not hosting or joined, the window has a *Steam connection test*. Two testers each send the other their Steam ID, paste it in, press **Test the connection** and report the result line (for example "connected, answers 6, last round trip 40 ms, direct"). A Steam ID identifies your Steam account, so share it only with the person you test with.

The full guide is in the zip (`HOW-TO-PLAY.txt`) and at https://github.com/AZalshehri7/FeverLink3/blob/main/docs/PLAYING.md

## The file winhttp.dll, safety and antivirus

The mod ships one native file, `winhttp.dll`. The game imports that name, so Windows loads it from the game folder when the game starts. It passes the game's 14 WinHTTP functions to the real Windows file unchanged and, inside the game, runs the multiplayer session on one thread. It does not inject into other programs, does not start with Windows, needs no administrator rights, sends no telemetry and does not download or run programs. This version installs no hooks into the game. The source is public and the page below lists exactly what it does and does not do, which files it creates, how to check the hash, and how to report a false positive to Microsoft, Kaspersky, ESET and others.

The same loading technique is used by malware, and the file is **not code-signed yet**, so some antivirus programs may flag it. Code signing is planned. https://github.com/AZalshehri7/FeverLink3/blob/main/docs/SECURITY_FOR_USERS.md

## What goes over the network

- Between the players: Steam's peer-to-peer networking through the Steam library the game already runs. Steam may relay the traffic through its own servers. A relay server operated by the project is being built as a fallback. The host does not open an inbound TCP port in normal use.
- Nothing else: no analytics, no update check, no advertising, no account. The mod needs the network for one thing only: connecting the players.
- The other players see your display name (it starts as your Windows user name; change it in the window), your game actions and chat; a host also sends the world.
- Session traffic is encrypted and authenticated by the mod itself (AES-256-GCM, key derived from the join code).

## Uninstall

Close the game and run `feverlink3 uninstall` (add `--dry-run` to see the plan). It removes the mod, our `winhttp.dll`, `feverlink3_path.txt` and, if it holds exactly the game's app id, `steam_appid.txt`. It never touches another mod's `winhttp.dll`. Optional clean-up: delete `%LOCALAPPDATA%\FeverLink3` (logs and session files), `%TEMP%\feverlink3`, and files named `feverlink3-resync-*.sav` in your save folder.

## FAQ

**Does it work?** Partly; see the status above. Playing together in two real games has not been verified yet.

**How do the players connect?** Through Steam's peer-to-peer networking, with a relay as a fallback that is being built. By design nothing has to be set up. It is being tested: inside one game it is proven, between two different accounts it has not been tried yet.

**Is winhttp.dll safe? My antivirus flags it.** See the security page above. Check the hash against the release notes, and report false positives; the page lists where.

**Windows says "Windows protected your PC".** SmartScreen reacts to a new, unsigned program. More info, then Run anyway. It appears once for the installer.

**Is it official?** No. It is an unofficial community project.

**Can I use it with MPFever?** No; both use the `winhttp.dll` slot, and our installer will not overwrite another mod's file.

**Will it break my saves?** The mod does nothing until you host or join. A session makes every game load the host's world and writes `feverlink3-resync-<number>.sav` files into your save folder. Back up your saves first.

**Can I play on my own with it installed?** Yes, by design: without a session the game plays as it always does.

**Which game version?** Build 40420 for now. After a game update the module may need an update; if the game fails to start, run `feverlink3 uninstall` and tell us your build.

**Consoles, Mac, Linux, Epic or GOG?** No. The native part is a Windows DLL and the installer finds the game through Steam.

**Why is it not on the in-game Mod Hub or the Steam Workshop?** Transport Fever 3 has no Steam Workshop, and the Mod Hub's published rules allow only the official scripting API, so a mod with a native DLL is not expected to fit. Details: https://github.com/AZalshehri7/FeverLink3/blob/main/docs/WORKSHOP.md

**How is it different from MPFever?** It is a separate, fresh implementation (Rust and Lua) built on the same idea; both projects are credited below. They cannot be installed together.

**How do I help or report a bug?** Discord `azalshehri`, or https://github.com/AZalshehri7/FeverLink3/issues. Please attach `%LOCALAPPDATA%\FeverLink3\native.log` and `live\mod.log`.

## Known issues

- Not a finished multiplayer game: everything under "Not verified yet" above.
- The native hooks are not enabled, so construction commands are not held back to the same moment on every game yet.
- Only game build 40420 is known. If a game update changes the web functions the game imports, the game may fail to start while `winhttp.dll` is installed; uninstalling fixes it.
- Not code-signed: SmartScreen and some antivirus programs warn. Windows 11 Smart App Control in its "On" setting may block the unsigned DLL; this has not been tested.
- Another mod's `winhttp.dll` conflicts with ours; the installer refuses to overwrite it.
- The window has no password field and no controls for the host's permissions yet; it is English only.
- A bug in the native module can crash the game (it aborts on an internal error).
- The install is command-line only; a guided installer is being built.

## Changelog

**0.2.0-test1 - first public test build**

- In-game window with a button in the bottom bar: host a game, join with a code, player list, chat, sync button.
- Lockstep core: actions stamped and applied at the same step on every game, world checksums, repair by loading the host's savegame, a permissions model.
- Encrypted sessions (AES-256-GCM, key from the join code); the host decides who may send what.
- Native module `winhttp.dll`: passes the game's 14 WinHTTP functions through and runs the session controller inside the game. No game hooks in this version.
- Steam peer-to-peer connection test in the window. Connection between two accounts still being tested; relay being built.
- Command-line installer: `feverlink3 install`, `uninstall`, `doctor`, with `--dry-run`.
- Unsigned. Verified in the real game: see the status above.

## Credits and permissions

- **Author:** Abdulaziz Alshehri (Discord `azalshehri`). Source and issues: https://github.com/AZalshehri7/FeverLink3
- **TpF2 Multiplayer** by silver2127 (Transport Fever 2, MIT): https://silver2127.github.io/tpf2-multiplayer/ - the replication modes, the "no entity ids on the wire" rule, the join-code and sealed-transport trust model and the host-included resync round.
- **MPFever** by dkdknight / arthur (Transport Fever 3, MIT): https://github.com/dkdknight/MPFever - the first lockstep co-op on this game: the proxy-DLL approach, the single-worker script pool fix, deterministic `math.random`, and the per-build game addresses and API names that FeverLink3 reuses with credit in the files that contain them.
- Both licences are reproduced in THIRD_PARTY_NOTICES.md in the zip. FeverLink3 contains no files or assets from other authors.
- Open-source Rust libraries for cryptography, serialisation and Windows access (list in `Cargo.lock`).
- **Transport Fever 3** belongs to Urban Games and Paradox Interactive. This project is not affiliated with them.
- **AI assistance:** much of the code and documentation was written with Claude (Anthropic) under the maintainer's direction. The claims on this page rest on the tests and real-game checks listed in the status.
- **Permissions:** MIT licence. You may use, copy, modify, merge, publish, distribute and sell copies of FeverLink3, provided the licence notice stays with it; it comes without warranty. Uploading elsewhere, modifying and converting are allowed on the same condition. No donation points.
