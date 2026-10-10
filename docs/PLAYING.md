# Playing FeverLink3 (test version)

FeverLink3 lets two or more people play the same Transport Fever 3 game together. One player **hosts**, the others **join** with a code. Players connect to each other **through Steam's peer-to-peer networking**: no port forwarding, no router settings, no VPN, nothing to open in the firewall. This is an early test build: expect rough edges, and tell the host's contact (Discord `azalshehri`) what happens.

## What you need

- Windows 10 or 11, and Transport Fever 3 on Steam, **the same game version as the host** (the host says which; the test build was made for build 40420, shown at the bottom right of the main menu).
- Steam running and logged in (it is, if you start the game from Steam).
- This folder (unzipped anywhere, for example the Desktop). Nothing is installed system-wide.

## 1. Install (once)

1. Unzip `FeverLink3-<version>.zip`: right-click it, **Extract All**. Keep the unzipped folder, for example on the Desktop. Do not run the program from inside the zip file.
2. Close Transport Fever 3 if it is running, then double-click `feverlink3.exe`. Windows may say "Windows protected your PC" the first time (the file is not code-signed yet): click **More info**, then **Run anyway**.
3. A black window opens and checks your PC. Every line starts with `[ok]` (fine), `[??]` (have a look), `[!!]` (must be fixed) or `[--]` (nothing yet). The line **Next step** says what to do.
4. Type `1` and press Enter (*Install or repair*). When the window says `[ok] FeverLink3 is installed.`, you are done. If something is wrong it says in plain words what to do; fix it and choose `1` again, installing twice does no harm. If it says Transport Fever 3 has not created its user folder yet, start the game once from Steam, close it, and choose `1` again.
5. After every choice the window waits for Enter, so you can read the answer. `0` closes it.

The other choices: `2` removes everything FeverLink3 put on your PC; `3` compares this folder with what is installed (to update, unzip the new zip into a new folder, double-click its `feverlink3.exe`, choose `3` to see what differs and `1` to update); `4` starts Transport Fever 3 through Steam; `5` opens the folder with the logs; `6` copies a text for the host to the clipboard (see "If something goes wrong").

**With a terminal instead** (the same result): open a terminal in the unzipped folder (click the folder's address bar, type `cmd`, press Enter) and type

```
feverlink3 install
```

It prints four `DONE` / `SKIP` lines: the mod into your Transport Fever 3 user folder, and two small files next to the game. If it says the game folder was not found, start Transport Fever 3 once from Steam, close it, and run the command again. To remove everything later: `feverlink3 uninstall`.

## 2. Start the game

Start Transport Fever 3 from **Steam as usual** (no extra program needs to run), or choose `4` in the FeverLink3 window. Start a new game: on the settings page open the **Add Mods** tab, click **Activate** under *FeverLink3 Multiplayer*, then Start Game. In the game, click **FeverLink3** in the bar at the bottom of the screen to open the window.

## 3. The Steam connection test (this test build)

This build proves that two players can reach each other through Steam. In the window, under **Steam connection test**:

1. Your **Steam ID** is shown (17 digits). Send it to the other player; they send you theirs.
2. Paste the other player's Steam ID into the field and press **Test the connection**.
3. Within about 20 seconds the window shows `Test: connected, answers N, last round trip X ms` and whether the route is `direct` or `through a Steam relay`. The other player sees `Pings received from other players`.

Both directions are worth trying (A tests B, then B tests A). If it says `failed`, or nothing happens after a minute, send the host a screenshot and the log files below.

## 4. Hosting and joining (next test build)

Hosting and joining a game with a join code travel over the same Steam connection. They arrive in the next test build. The flow will be: the host clicks **Host a game** and sends the code, the friend pastes it and presses **Join**, the host's world is sent to the friend and loads. Nothing about ports or routers is needed.

## If something goes wrong

The easiest way: double-click `feverlink3.exe`, type `6` and press Enter, then paste (`Ctrl+V`) into your message to the host. The text holds what FeverLink3 found on your PC and the end of the two logs; your Windows user name is replaced by `<user>` and join codes are left out. It can still hold folder names.

Or send the host these files (they hold no personal data besides your Windows user name):

- `%LOCALAPPDATA%\FeverLink3\native.log`
- `%LOCALAPPDATA%\FeverLink3\live\mod.log` and `ctl_status.txt`
- the game's own log, in Steam's folder: `userdata\<number>\3493540\local\crash_dump\stdout.txt`

Paste `%LOCALAPPDATA%\FeverLink3` into the Explorer address bar to open the folder.

**The game shows a red "Reload UI" error:** note the text, send a screenshot, click *Back to Main Menu*, and run `feverlink3 uninstall` if you want to play normally again.

Contact: Discord `azalshehri`.
