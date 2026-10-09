# Playing FeverLink3 (test version)

FeverLink3 lets two or more people play the same Transport Fever 3 game together. One player **hosts**, the others **join** with a code. Players connect to each other **through Steam's peer-to-peer networking**: no port forwarding, no router settings, no VPN, nothing to open in the firewall. This is an early test build: expect rough edges, and tell the host's contact (Discord `azalshehri`) what happens.

## What you need

- Windows 10 or 11, and Transport Fever 3 on Steam, **the same game version as the host** (the host says which; the test build was made for build 40420, shown at the bottom right of the main menu).
- Steam running and logged in (it is, if you start the game from Steam).
- This folder (unzipped anywhere, for example the Desktop). Nothing is installed system-wide.

## 1. Install (once)

1. Unzip `FeverLink3-<version>.zip`.
2. Open a terminal in the unzipped folder: click the folder's address bar, type `cmd`, press Enter. Then type

   ```
   feverlink3 install
   ```

   Windows may say "Windows protected your PC" the first time (the file is not code-signed yet): click **More info**, then **Run anyway**.
3. It prints four `DONE` / `SKIP` lines: the mod into your Transport Fever 3 user folder, and two small files next to the game. If it says the game folder was not found, start Transport Fever 3 once from Steam, close it, and run the command again.

To remove everything later: `feverlink3 uninstall`.

## 2. Start the game

Start Transport Fever 3 from **Steam as usual** (no extra program needs to run). Start a new game: on the settings page open the **Add Mods** tab, click **Activate** under *FeverLink3 Multiplayer*, then Start Game. In the game, click **FeverLink3** in the bar at the bottom of the screen to open the window.

## 3. The Steam connection test (this test build)

This build proves that two players can reach each other through Steam. In the window, under **Steam connection test**:

1. Your **Steam ID** is shown (17 digits). Send it to the other player; they send you theirs.
2. Paste the other player's Steam ID into the field and press **Test the connection**.
3. Within about 20 seconds the window shows `Test: connected, answers N, last round trip X ms` and whether the route is `direct` or `through a Steam relay`. The other player sees `Pings received from other players`.

Both directions are worth trying (A tests B, then B tests A). If it says `failed`, or nothing happens after a minute, send the host a screenshot and the log files below.

## 4. Hosting and joining (next test build)

Hosting and joining a game with a join code travel over the same Steam connection. They arrive in the next test build. The flow will be: the host clicks **Host a game** and sends the code, the friend pastes it and presses **Join**, the host's world is sent to the friend and loads. Nothing about ports or routers is needed.

## If something goes wrong

Please send the host these files (they hold no personal data besides your Windows user name):

- `%LOCALAPPDATA%\FeverLink3\native.log`
- `%LOCALAPPDATA%\FeverLink3\live\mod.log` and `ctl_status.txt`
- the game's own log, in Steam's folder: `userdata\<number>\3493540\local\crash_dump\stdout.txt`

Paste `%LOCALAPPDATA%\FeverLink3` into the Explorer address bar to open the folder.

**The game shows a red "Reload UI" error:** note the text, send a screenshot, click *Back to Main Menu*, and run `feverlink3 uninstall` if you want to play normally again.

Contact: Discord `azalshehri`.
