# Playing FeverLink3 (test version 0.2.0-test5)

FeverLink3 lets two or more people play the same Transport Fever 3 game together. One player **hosts**, the others **join** with a code. Players connect to each other through Steam's own peer-to-peer networking: by design that means no port forwarding, no router settings, no VPN and nothing to open in the firewall. A first test between two Steam accounts reached through Steam's relay servers; the full host-and-join test is what this build is for. This is an early test build: expect rough edges, and tell the maintainer (Discord `azalshehri`) what happens.

**Licence:** FeverLink3 is free for noncommercial use (PolyForm Noncommercial 1.0.0, see `LICENSE`); commercial use needs a separate licence from the maintainer.

## What you need

- Windows 10 or 11, and Transport Fever 3 on Steam, **the same game version as the host** (the host says which; this build was made for build 40420, shown at the bottom right of the main menu).
- Steam running and logged in (it is, if you start the game from Steam).

## 1. Install (once per PC): copy two things into the game folder

There is no installer. You copy the files into the folder of the game:

1. Unzip `FeverLink3-<version>.zip`: right-click it, **Extract All**. Open the unzipped folder: it holds `winhttp.dll`, a folder called `mods`, and some text files. Do not run anything from inside the zip file itself.
2. Close Transport Fever 3 if it is running.
3. Open the game's folder: in Steam, right-click **Transport Fever 3**, choose **Manage**, then **Browse local files**. A folder opens that contains `TransportFever3.exe` and a folder called `mods`.
4. In the unzipped folder select **`winhttp.dll`** and the **`mods`** folder, copy them (`Ctrl+C`), go to the game's folder and paste (`Ctrl+V`).
   - Windows asks about the `mods` folder that already exists: choose **Yes / Merge**. Nothing of the game is replaced; the mod is added as `mods\release\feverlink3_1`.
   - If Windows asks for administrator permission to paste, click **Continue**.
   - If Windows says `winhttp.dll` already exists, **stop**: another mod uses the same file name. Do not replace it; tell the maintainer.
5. Check: the game's folder now contains `winhttp.dll` next to `TransportFever3.exe`, and `mods\release\feverlink3_1` exists.

**To update:** close the game and copy the new files over the old ones in the same way. **To remove FeverLink3:** delete `winhttp.dll` and the folder `mods\release\feverlink3_1` from the game's folder. Steam's "Verify integrity of game files" leaves both in place.

## 2. Start the game

Start Transport Fever 3 from **Steam as usual**.

- Starting a new game: on the settings page open the **Add Mods** tab, click **Activate** under *FeverLink3 Multiplayer* (it is listed with the game's own mods), then Start Game. (Menu names as of build 40420.)
- In the game, click **FeverLink3** in the bar at the bottom of the screen (next to the item counters). A window called *FeverLink3 Multiplayer* opens.
- The mod does nothing until you host or join: without a session, the game plays as it always does.
- If the window says "The FeverLink3 module is not running", `winhttp.dll` is not next to `TransportFever3.exe`: close the game, check step 1, start the game from Steam again.

## 3. Hosting and joining

**The host:**

1. Start a game (a new game, or load one) with *FeverLink3 Multiplayer* activated, and click **FeverLink3** in the bottom bar.
2. Press **Host a game**. The window shows a **join code**. Press **Copy code** (the code is put on your clipboard; the game's text fields cannot copy by themselves) and paste it into a message to your friend with `Ctrl+V` (chat, Discord). Treat it like a password for the session.
3. When your friend appears under **Players**, press **Start the session**. Your world is sent to the other player, whose game reloads into it; yours keeps running without a pause or a loading screen. The games compare a checksum before the session runs, so a difference would stop it instead of going unnoticed. The same happens for a player who joins later.
4. While you play, **Pause**, **1x**, **2x** and **3x** in the window's **Session** section change the speed in every game, and the chat line under them sends a message to the others. Every player can do both.

**The player who joins:**

1. Start any game with *FeverLink3 Multiplayer* activated (a new game is fine; it is replaced) and click **FeverLink3** in the bottom bar.
2. Click the field **paste the join code here**, press `Ctrl+V` to paste the host's code, and press **Join**. The window shows what it is doing (`contacting the host`, `finding a route`, `connected`).
3. Keep the window open. When the host presses **Start the session**, the host's world is sent to you (about 30 MB; it takes up to a minute or two), your game reloads into it, and the session runs on both games. Open the window again after the reload (the reload closes it).

Only join people you know. When you join, your game loads the host's world, and a Transport Fever savegame can carry game scripts, just as a mod can. Joining a session means trusting the host the way you would trust the author of a mod.

If joining fails, the window says why (wrong code, the host is not hosting, no route found). Send the maintainer a screenshot and the log files below.

## 4. The Steam connection test (for problems)

Press **Connection test...** in the window (under the join code field) to open the **Steam connection test**. It checks that two players can reach each other through Steam, without hosting: your **Steam ID** (17 digits) is shown; paste the other player's Steam ID into **paste the other player's Steam ID** and press **Test the connection**. Within about 20 seconds the window shows `connected` with the round-trip time and whether the route is `direct` or `through a Steam relay`. A Steam ID identifies your Steam account, so share it only with the person you are testing with.

## What works and what does not (yet)

**Verified in the real game by the maintainer (game build 40420):**

- The mod loads from the game's own `mods` folder, the in-game window opens from a button in the bottom bar, and **Host a game** over Steam shows a join code.
- Two real copies of the game on one PC (joined over a developer connection that players never use) stay in step: the host's world is sent, the second game loads it, and both report identical world checksums.
- The game's own save and load functions can be driven by the mod.
- **Steam peer-to-peer works between two different accounts** (2026-10-10: a friend's connection test reached the maintainer through a Steam relay), and inside one game the transport's nine self-checks pass, including a frame the size of a savegame chunk.

**Not verified yet:**

- Joining a host over Steam from a second Steam account, the world transfer over that connection, and playing together afterwards (this build is for that test).
- Building together: the mod captures player actions (buying vehicles, creating lines, roads and rails built with the tools in some cases, and more) and replays them on every game, but this has only been checked with simulated games, never with two real ones.
- The native hooks that would hold construction commands back so every game builds at the same moment: switched off in this version, and never run in the real game.

**Written and tested without the game, not yet seen in a real session:** reconnecting after a connection loss (the same player comes back within about five minutes), the host removing a player, up to eight players.

**Not built yet:** separate companies (one shared company for now), a dedicated server.

## If something goes wrong

Send the maintainer these files (they hold no personal data besides your Windows user name and folder names):

- `%LOCALAPPDATA%\FeverLink3\native.log`
- `%LOCALAPPDATA%\FeverLink3\live\mod.log` and `ctl_status.txt`
- the game's own log, in Steam's folder: `userdata\<number>\3493540\local\crash_dump\stdout.txt`

Paste `%LOCALAPPDATA%\FeverLink3` into the Explorer address bar to open the folder.

**The game shows a red "Reload UI" error:** note the text, send a screenshot, click *Back to Main Menu*, and remove FeverLink3 (step 1, "To remove") if you want to play normally again.

Contact: Discord `azalshehri`.
