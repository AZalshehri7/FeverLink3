# Playing FeverLink3 (test version 0.2.0-test1)

FeverLink3 lets two or more people play the same Transport Fever 3 world together, each on their own PC. One player hosts, the others join with a code. This is an early test build: expect rough edges, read "What works and what does not (yet)" before you invite anyone, and tell the maintainer (Discord `azalshehri`) what happens.

Last updated: 2026-10-10. FeverLink3 is an unofficial community project. It is not made, endorsed or supported by Urban Games or Paradox Interactive.

## What you need

- Windows 10 or 11 (64-bit) and Transport Fever 3 on Steam. Every player needs their own copy of the game, started through Steam, with Steam running and logged in.
- **The same game version as the host.** This test build was made for game build 40420 (shown at the bottom right of the main menu). Ask the host which build they run.
- This folder, unzipped anywhere (for example the Desktop). Nothing is installed system-wide.
- Back up your savegames before the first session. During a session FeverLink3 makes every game load the host's world, and it writes files named `feverlink3-resync-<number>.sav` into your Transport Fever 3 save folder.

## 1. Install (once per PC)

1. Close Transport Fever 3.
2. Unzip `FeverLink3-<version>.zip`.
3. Open a terminal in the unzipped folder: click the folder's address bar, type `cmd`, press Enter. Then type

   ```
   feverlink3 install
   ```

   Windows may say "Windows protected your PC" the first time, because the program is not code-signed yet: click **More info**, then **Run anyway**. (Add `--dry-run` to see what it would do without changing anything.)
4. It prints one line per item, each marked `DONE`, `SKIP` or `FAILED`:
   - the mod, copied into your Transport Fever 3 user folder (one line for each Steam account on the PC),
   - `steam_appid.txt`, `winhttp.dll` and `feverlink3_path.txt` next to the game program.

   If a line says `FAILED` or `SKIP`, the reason is printed on the same line. The common ones:
   - "no Steam user folder ... exists; start the game once": start Transport Fever 3 once from Steam, close it, run the command again.
   - "Transport Fever 3 was not found": run `feverlink3 doctor`. It prints where it looked and changes nothing.
   - "a winhttp.dll that is not ours is already in the game folder": another mod uses the same slot. FeverLink3 never overwrites it. The two cannot be used together; remove the other one first.
   - "is the game running?": close the game and run the command again.

What the files are, and how to check them, is explained in `docs/SECURITY_FOR_USERS.md` (on the project page: https://github.com/AZalshehri7/FeverLink3). A guided installer is being built; this guide describes the command-line install that exists today.

To remove everything later, close the game and run `feverlink3 uninstall`.

## 2. Start the game

Start Transport Fever 3 from **Steam as usual**. No extra program needs to run.

- Starting a new game: on the settings page open the **Add Mods** tab, click **Activate** under *FeverLink3 Multiplayer*, then Start Game. (Menu names as of build 40420. If you cannot find the mod, run `feverlink3 doctor` and check that it says "mod installed".)
- The mod does nothing until you host or join: without a session, the game plays as it always does.
- Players who join do not need to prepare a world: the host's world is meant to be sent to them (see "What works" below, this step is not verified yet).

## 3. Host

1. In the game, click **FeverLink3** in the bar at the bottom of the screen (next to the item counters). A window called *FeverLink3 Multiplayer* opens.
2. Check **Your name**. It starts as your Windows user name and the other players will see it. Change it if you prefer.
3. Click **Host a game**. The window says "You are hosting" and shows a **join code**.
4. Click into the code field, press `Ctrl+A`, then `Ctrl+C`, and send the code to your friends (chat, Discord). The code is the key to your session: anyone who has it can join, so send it only to people you want to play with. Click **Stop hosting** when you are done.
5. Press **Start the session**, then let your friends join. Each friend who joins is meant to get a copy of your world, and the game reloads for them.
6. If someone's world looks different or out of date, press **Sync everyone to my world** (it appears once the session is started).

## 4. Join

Only join people you know. When you join, your game loads the host's world, and a Transport Fever savegame can carry game scripts, just as a mod can. Joining a session means trusting the host the way you would trust the author of a mod.

1. Start the game (any world, a new game is fine) with the mod activated, and click **FeverLink3** in the bar at the bottom.
2. Paste the host's code into the field **paste the join code here** and press **Join**.
3. The window shows "Connecting to the host..." and then "Connected". Wait for the host to start the session. The host's world is then sent to you and your game loads it. This is expected to take up to a minute; it is one of the steps that has not been verified between two real games yet.
4. If the window says "Could not connect", read the message, check that the whole code was pasted, and ask the host to check that they are still hosting. **Leave** disconnects you.

## How the players are connected

FeverLink3 connects the players' games through Steam's own peer-to-peer networking, using the Steam program that is already running for Transport Fever 3. By design there is nothing for you to set up: no address to type and nothing to configure. The join code is all your friend needs. Steam picks the route between the two games; if it cannot connect them directly it can use its own relay servers. A relay server operated by this project is being built as a second fallback. Whether this works on real home connections is exactly what the first two-account tests will show (see the status below).

Status, stated plainly:

- **Proven so far:** inside one running game, the game opened a Steam peer-to-peer connection to its own Steam ID and 6 pings were answered.
- **Being tested:** a connection between two *different* Steam accounts on two PCs. That first test is still to come. Until it has been done, please do not assume that a session over the internet will work.
- **Being built:** the relay server fallback.

### Help us test the connection

While you are not hosting or joined, the window has a section called **Steam connection test**:

1. Each of the two testers opens the window. It shows "Your Steam ID". Send yours to the other tester. A Steam ID identifies your Steam account, so share it only with the person you are testing with.
2. Paste the other tester's Steam ID into **paste the other player's Steam ID** and press **Test the connection**.
3. Wait up to a minute. The window shows a line like `Test: connected, answers 6, last round trip 40 ms, direct` (or `through a Steam relay`). Send that line, and a note of where each of you lives (country is enough), to the maintainer.

Both testers need to be in the game with Steam running when the test starts.

## What works and what does not (yet)

**Verified in the real game by the maintainer on 2026-10-09 (game build 40420):**

- The mod loads and the in-game window opens from a button in the bottom bar.
- "Host a game" starts a session inside the game and shows a join code.
- A second client (a test client, not a second copy of the game) can join that session by its code.
- The game's own save and load functions can be driven by the mod, and after a reload the mod finds its session files again.
- The game can open a Steam peer-to-peer listener (2026-10-10: and connect to its own Steam ID, 6 pings answered).

**Not verified yet:**

- Building together: the mod captures some player actions (buying vehicles, creating lines, putting vehicles on lines) and replays them on every game, but this has only been checked with simulated games, never with two real ones.
- Sending the host's world to a second real game, and loading it there.
- Joining from a second real copy of the game, on another PC or Steam account.
- The Steam connection between two different accounts.
- The native hooks that would hold construction commands back so every game builds at the same moment: not part of this version.

**Not built yet:** separate companies, reconnecting after a connection loss while the game keeps running, a dedicated server.

## If something goes wrong

Please send the maintainer these files. They hold no personal data besides your Windows user name:

- `%LOCALAPPDATA%\FeverLink3\native.log`
- `%LOCALAPPDATA%\FeverLink3\live\mod.log` and `ctl_status.txt`
- the game's own log, in Steam's folder: `userdata\<number>\3493540\local\crash_dump\stdout.txt`

Paste `%LOCALAPPDATA%\FeverLink3` into the Explorer address bar to open the folder.

- **The game shows a red "Reload UI" error:** note the text, send a screenshot, click *Back to Main Menu*, and run `feverlink3 uninstall` if you want to play normally again.
- **The game does not start after the install:** close Steam's launch attempt and run `feverlink3 uninstall` (or delete `winhttp.dll` from the game folder). Tell the maintainer your game build and Windows version. Known causes to check: a game update (the module can only be used with the game builds it was made for), an antivirus that blocked the file, Windows 11 Smart App Control, or another mod that also uses `winhttp.dll`.
- **Your antivirus complains about `winhttp.dll`:** the file is a small, open-source proxy that Windows loads for the game. `docs/SECURITY_FOR_USERS.md` explains exactly what it does, how to verify it with a hash, and how to report a false positive.

FeverLink3 builds on two earlier MIT-licensed projects, credited in the README: TpF2 Multiplayer by silver2127 (https://silver2127.github.io/tpf2-multiplayer/) and MPFever by dkdknight / arthur (https://github.com/dkdknight/MPFever).

Contact: Discord `azalshehri`, or https://github.com/AZalshehri7/FeverLink3/issues
