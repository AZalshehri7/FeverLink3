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

## 5. How to run the Steam speed test with a friend

This tells us whether a save of about 80 MB can be sent over Steam, and how long it takes. It needs two Steam accounts on two PCs (one account can only talk to itself). Both of you do steps 1 to 3; only one of you does step 4.

1. Both start Transport Fever 3 from Steam, start a game with the mod active, and open the window (**FeverLink3** in the bottom bar). Stay in the game for the whole test; do not pause it with the main menu.
2. Both press **Connection test...**. Each sees their own **Steam ID**. Send yours to the other (any chat), and paste the other's into **paste the other player's Steam ID**.
3. Optional: to try a faster send rate for the reliable mode, both start the game with the environment variable `FEVERLINK3_STEAM_SENDRATE` set (for example 4194304 for 4 MiB/s; the default is 1048576). Without it you measure the default. The datagram mode does not depend on it: it raises the connection's Steam rate itself while it runs and puts it back after.
4. One of you picks the size (**8 MB**, **32 MB** or **80 MB**; start with 32 MB) and presses **Run the speed test**. The other does nothing. The note under the button shows the progress ("sending to your friend: 12.4 of 32 MB"), then the result, four lines: MB/s each direction and total seconds; the route and round trip; the queue and any stall; and how long an 80 MB save would take if you host and if your friend hosts. The test sends the data both ways, so it takes about twice the size divided by the speed (a minute or two for 32 MB at 1 MB/s).
5. Send us the result: the lines under the button, or the lines starting `steam speed test` in `session.log` (the file `%LOCALAPPDATA%\FeverLink3\live\session.log` on the PC that pressed the button). Then swap roles and run it once the other way, and once with 80 MB.

**Two ways to move the data.** Under the size buttons choose how the data travels: **Reliable** (Steam's reliable stream, what a resync uses today), **Datagrams** (our own protocol over Steam's unreliable messages, see `docs/STEAM_TRANSPORT.md` section 7.2; it has not run on real Steam yet), **Both** (reliable, then datagrams, with the same size), **Parallel** (the reliable stream on 1, 2 and 4 connections at once, to see whether the limit is per connection) or **Auto (scaled)** (the way a synced world is sent: several Steam connections at once, as many as the size needs, 3 for 32 MB and 8 for 80 MB, added one by one while the speed keeps growing; the result says how many connections were used, MB/s, how long it took and why it stopped adding connections; it has not run on real Steam yet). A series shows one block per run, each under a line `== reliable (32 MB) ==`; a run that fails stops the series. The reliable result keeps its four lines (plus a fifth, "Steam's own view", with the extremes of what Steam reported while it ran). A datagram block has: the two rates; the route and any route change during the run; for each direction how many datagrams were lost and sent again, the round trip when idle and the extra delay under load (the "queue", the number that tells whether a voice call would suffer), the rate the controller settled at and the most datagrams in flight; the longest pause; and the time of an 80 MB save without the base64 growth.

Suggested order with a friend: **Reliable 32 MB**, then **Auto (scaled) 32 MB** and **Auto (scaled) 80 MB** (each player presses it once, so both directions are measured by both), then, if you like, **Datagrams 32 MB**. While it runs, a voice call on the same line shows whether the datagram mode leaves room for it. The detailed Steam status (`steam speed test status ...`) and one sample line per second go to `session.log`; send those lines too.

If it says `Speed test failed: ...` the reason follows (no route to the other PC, the connection ended, no data for 60 s). A line saying `STALLED` means the data stopped for more than 5 seconds at some point; say when it happened.

**How a synced world travels.** When the host presses **Sync everyone to my world**, the host's game saves the world and the launcher sends the file (about 70 to 80 MB) to every player. A player whose launcher has announced it (`bulk-par-1`, the default in this version) receives it over several Steam connections at once, as raw bytes written straight to the save file: the host opens as many connections as the file's size needs (7 for 75 MB, 8 for 80 MB, none below 6 MB, at most 16) and adds them one by one only while each really adds speed and the ping of the session stays low. The window shows the progress: "Syncing: Receiving the host's world: 63% (7 connections, 6.9 MB/s)". The lockstep and the chat keep using the session's own connection. If the extra connections cannot be opened, the file does not arrive complete, or nothing moves for 10 seconds, the host sends that player the world in the old way instead (slower, in pieces on the session connection) and says so in `session.log` (`bulk transfer to <name> failed (<why>); sending the chunks instead`); the other players are not affected. `FEVERLINK3_BULK=off` (chunks only), `FEVERLINK3_BULK_CONNS=<n>` (a fixed number of connections) and `FEVERLINK3_BULK_MAX=<n>` (another cap) are set in the host's environment. **This has not run on real Steam yet**; the speed test **Auto (scaled)** above runs the same code between two real PCs.

### Without Transport Fever 3

A friend who does not own the game can take part with the small program `feverlink3-speedtest.exe`, which needs only Steam (running and logged in). **It has not run on real Steam yet; it is tested only against the fake Steam of the test suite.** It runs the same speed test as the window between two PCs, using Valve's test app id 480 (Spacewar) on both. Its default run takes about 10 minutes: the idle round trip; the rate ladder (1, 2 and 4 MiB/s on one connection); 2, 4, 8 and 16 connections at 1 MiB/s each (a direction skips the larger counts after one that did not scale); **Auto (scaled)** with 80 MB each way (exactly how a world is sent in a sync: the number of connections the size needs, the connections used, MB/s and time are in the result); and a datagram test. `--classic` lists the older modes, including Auto (scaled) with 8, 32 and 80 MB.

- The maintainer builds two packages with `tools/speedtest/make_package.ps1`: the **tester** package (made with `-PeerSteamId <the other player's SteamID64>`, which writes `peer.txt`) and the **responder** package (made without it). Each is a folder and a zip next to the repository, holding the exe, Valve's `steam_api64.dll`, `steam_appid.txt` (480), `peer.txt` (tester only), a `README.txt` and a SHA-256 file. The Steam ID is written nowhere except `peer.txt`.
- Unzip, double-click `feverlink3-speedtest.exe`. With a `peer.txt` next to it the program is the **tester**: it connects to that Steam ID ("waiting for the other PC to start its program..." for up to 120 seconds, then once more), runs the tests one after another and writes `speedtest-results.txt` (date, program version, both Steam IDs, route and ping, the four-line result of each run); at the end it says which file to send. Without a `peer.txt` it is the **responder**: it prints its own Steam ID, waits, answers every tester that connects (several, one after another) and logs each test to `speedtest-responder.txt`. Start the responder first, then the tester.
- `feverlink3-speedtest.exe --peer <SteamID64>` and `feverlink3-speedtest.exe --listen` override the files.
- If it cannot start it says why in plain words: Steam is not running, not logged in or in Offline Mode, `steam_api64.dll` missing from the folder, or Steam's own message.
- The program needs no administrator rights and only talks to the Steam network and the one other PC. To remove it, delete the folder.

## What works and what does not (yet)

**Verified in the real game by the maintainer (game build 40420):**

- The mod loads from the game's own `mods` folder, the in-game window opens from a button in the bottom bar, and **Host a game** over Steam shows a join code.
- Two real copies of the game on one PC (joined over a developer connection that players never use) stay in step: the host's world is sent, the second game loads it, and both report identical world checksums.
- The game's own save and load functions can be driven by the mod.
- **Steam peer-to-peer works between two different accounts** (2026-10-10: a friend's connection test reached the maintainer through a Steam relay), and inside one game the transport's nine self-checks pass, including a frame the size of a savegame chunk.

**Not verified yet:**

- Joining a host over Steam from a second Steam account, the world transfer over that connection, and playing together afterwards (this build is for that test).
- Building together beyond the slice that was seen: the mod captures player actions (buying vehicles, stations, depots, rail, and more) and replays them on every game, but only roads, bulldozing, loans and lines have been checked with two real games.
- The native hooks that hold construction commands back so every game builds at the same moment: switched off unless a developer turns them on; run in two real games for the road tools and the bulldozer, not for anything else.

**Written and tested without the game, not yet seen in a real session:** reconnecting after a connection loss (the same player comes back within about five minutes), the host removing a player, up to eight players.

**Towns and sync.** The game grows towns with choices that can differ from one computer to the next, so after a while two players' towns can differ, and a road or station built through such a town may fail on the other computer; FeverLink3 then repairs the world with a resync. Town growth is never switched off and no rating is changed; FeverLink3 does not alter how the game works. In our tests the two games' growth started from different random seeds and drifted apart about 10 seconds after each resync; making the seeds equal helped for a while (about 13 checks) but not for good, so the work continues on keeping growth identical on every computer. Until then, expect an occasional resync after building in or near a town.

**Not built yet:** separate companies (one shared company for now), a dedicated server, towns that grow during a session.

## If something goes wrong

Send the maintainer these files (they hold no personal data besides your Windows user name and folder names):

- `%LOCALAPPDATA%\FeverLink3\native.log`
- `%LOCALAPPDATA%\FeverLink3\live\mod.log` and `ctl_status.txt`
- the game's own log, in Steam's folder: `userdata\<number>\3493540\local\crash_dump\stdout.txt`

Paste `%LOCALAPPDATA%\FeverLink3` into the Explorer address bar to open the folder.

**The game shows a red "Reload UI" error:** note the text, send a screenshot, click *Back to Main Menu*, and remove FeverLink3 (step 1, "To remove") if you want to play normally again.

Contact: Discord `azalshehri`.
