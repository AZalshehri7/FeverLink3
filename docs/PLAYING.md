# Playing FeverLink3 (test version 0.2.0-test1)

FeverLink3 lets two or more people play the same Transport Fever 3 world together, each on their own PC: one player hosts, the others join with a code. **This first test build has one job: to find out whether two players can reach each other through Steam** (the connection test in section 3). Hosting and joining a game over that connection arrive in the next test build, so the window's **Host a game** button will tell you that it is not finished yet. Expect rough edges, and tell the maintainer (Discord `azalshehri`) what happens.

Players are connected through Steam's own peer-to-peer networking. By design that means no port forwarding, no router settings, no VPN and nothing to open in the firewall. Whether this holds on real home connections is exactly what this build is testing; until two different accounts have connected, please do not assume it works over the internet.

Last updated: 2026-10-10. FeverLink3 is an unofficial community project. It is not made, endorsed or supported by Urban Games or Paradox Interactive.

## What you need

- Windows 10 or 11 (64-bit) and Transport Fever 3 on Steam. Every player needs their own copy of the game, started through Steam, with Steam running and logged in.
- **The same game version as the other player.** This test build was made for game build 40420 (shown at the bottom right of the main menu).
- This folder, unzipped anywhere (for example the Desktop). Nothing is installed system-wide.
- Back up your savegames before the first real session (next test build). Sessions will make every game load the host's world and will write files named `feverlink3-resync-<number>.sav` into your Transport Fever 3 save folder.

## 1. Install (once per PC)

`feverlink3.exe` is a testing tool for this phase. The plan is that the mod reaches players from the in-game Mod Hub or from Nexus Mods like any other mod, with the native module (`winhttp.dll`) delivered separately for now. Neither is available yet, and whether the Mod Hub accepts the mod is still being checked (`docs/WORKSHOP.md`).

1. Unzip `FeverLink3-<version>.zip`: right-click it, **Extract All**. Keep the unzipped folder, for example on the Desktop. Do not run the program from inside the zip file.
2. Close Transport Fever 3 if it is running, then double-click `feverlink3.exe`. Windows may say "Windows protected your PC" the first time, because the program is not code-signed yet: click **More info**, then **Run anyway**.
3. A black window opens and checks your PC. Every line starts with `[ok]` (fine), `[??]` (have a look), `[!!]` (must be fixed) or `[--]` (nothing yet). The line **Next step** says what to do.
4. Type `1` and press Enter (*Install or repair*). When the window says `[ok] FeverLink3 is installed.`, you are done. If something is wrong it says in plain words what to do; fix it and choose `1` again, installing twice does no harm. If it says Transport Fever 3 has not created its user folder yet, start the game once from Steam, close it, and choose `1` again.
5. After every choice the window waits for Enter, so you can read the answer. `0` closes it.

The other choices: `2` removes everything FeverLink3 put on your PC; `3` compares this folder with what is installed (to update, unzip the new zip into a new folder, double-click its `feverlink3.exe`, choose `3` to see what differs and `1` to update); `4` starts Transport Fever 3 through Steam; `5` opens the folder with the logs; `6` copies a text for the host to the clipboard (see "If something goes wrong").

**With a terminal instead** (the same result): open a terminal in the unzipped folder (click the folder's address bar, type `cmd`, press Enter) and type

```
feverlink3 install
```

(Add `--dry-run` to see what it would do without changing anything.) It prints one line per item, each marked `DONE`, `SKIP` or `FAILED`:

- the mod, copied into your Transport Fever 3 user folder (one line for each Steam account on the PC),
- `steam_appid.txt`, `winhttp.dll` and `feverlink3_path.txt` next to the game program.

If a line says `FAILED` or `SKIP`, the reason is printed on the same line. The common ones:

- "no Steam user folder ... exists; start the game once": start Transport Fever 3 once from Steam, close it, run the command again.
- "Transport Fever 3 was not found": run `feverlink3 doctor`. It prints where it looked and changes nothing.
- "a winhttp.dll that is not ours is already in the game folder": another mod uses the same slot. FeverLink3 never overwrites it. The two cannot be used together; remove the other one first.
- "is the game running?": close the game and run the command again.

What the files are, and how to check them, is explained in `docs/SECURITY_FOR_USERS.md` (on the project page: https://github.com/AZalshehri7/FeverLink3).

To remove everything later, close the game and run `feverlink3 uninstall` (or choose `2` in the window).

## 2. Start the game

Start Transport Fever 3 from **Steam as usual** (no extra program needs to run), or choose `4` in the FeverLink3 window.

- Starting a new game: on the settings page open the **Add Mods** tab, click **Activate** under *FeverLink3 Multiplayer*, then Start Game. (Menu names as of build 40420. If you cannot find the mod, run `feverlink3 doctor` and check that it says "mod installed".)
- In the game, click **FeverLink3** in the bar at the bottom of the screen (next to the item counters). A window called *FeverLink3 Multiplayer* opens.
- The mod does nothing until you host or join: without a session, the game plays as it always does.
- If the window says "The FeverLink3 module is not running", close the game, install again (choice `1`), and start the game from Steam. If it says "looking for Steam..." for more than a minute, make sure the game was started from Steam.

## 3. The Steam connection test (this test build)

This build proves that two players can reach each other through Steam. In the window, under **Steam connection test**:

1. Your **Steam ID** is shown (17 digits). Send it to the other player; they send you theirs. A Steam ID identifies your Steam account, so share it only with the person you are testing with.
2. Paste the other player's Steam ID into **paste the other player's Steam ID** and press **Test the connection**.
3. Within about 20 seconds the window shows a line like `Test: connected, answers 6, last round trip 40 ms, direct` (or `through a Steam relay`, which means Steam used its own relay servers). The other player's window shows `Pings received from other players`.

Both directions are worth trying (A tests B, then B tests A). Both players need to be in the game with Steam running when the test starts. If it says `failed`, or nothing happens after a minute, send the maintainer a screenshot and the log files below, and say in which countries you and the other player live (the country is enough).

## 4. Hosting and joining (next test build)

Hosting and joining a game with a join code will travel over the same Steam connection. They arrive in the next test build; until then **Host a game** answers that hosting over Steam is not finished in this build yet. The flow will be: the host clicks **Host a game**, copies the join code and sends it to a friend, who pastes it into **paste the join code here** and presses **Join**; the host's world is then sent to the friend and loads. There will be nothing to forward, open or configure.

Only join people you know. When you join, your game loads the host's world, and a Transport Fever savegame can carry game scripts, just as a mod can. Joining a session means trusting the host the way you would trust the author of a mod. Treat a join code like a password for that session.

## What works and what does not (yet)

**Verified in the real game by the maintainer on 2026-10-09 (game build 40420):**

- The mod loads and the in-game window opens from a button in the bottom bar.
- With an earlier build of the window, whose Host button used a direct connection (since taken out of the window), hosting showed a join code and a second client (a test client, not a second copy of the game) could join that session by its code.
- The game's own save and load functions can be driven by the mod, and after a reload the mod finds its session files again.
- The game can open a Steam peer-to-peer listener.

**Proven on 2026-10-10:** inside one running game, the game opened a Steam peer-to-peer connection to its own Steam ID and 6 pings were answered.

**Not verified yet:**

- The Steam connection between two different accounts on two PCs (the test in section 3 is for this).
- Hosting and joining over Steam, and sending the host's world to a second real game and loading it there.
- Joining from a second real copy of the game, on another PC or Steam account.
- Building together: the mod captures player actions (buying vehicles, creating lines, putting vehicles on lines, and more) and replays them on every game, but this has only been checked with simulated games, never with two real ones.
- The native hooks that would hold construction commands back so every game builds at the same moment: switched off in this version, and never run in the real game.

**Not built yet:** separate companies, reconnecting after a connection loss while the game keeps running, a dedicated server.

## If something goes wrong

The easiest way: double-click `feverlink3.exe`, type `6` and press Enter, then paste (`Ctrl+V`) into your message to the maintainer. The text holds what FeverLink3 found on your PC and the end of the two logs; your Windows user name is replaced by `<user>` and join codes are left out. It can still hold folder names.

Or send the maintainer these files (they hold no personal data besides your Windows user name):

- `%LOCALAPPDATA%\FeverLink3\native.log`
- `%LOCALAPPDATA%\FeverLink3\live\mod.log` and `ctl_status.txt`
- the game's own log, in Steam's folder: `userdata\<number>\3493540\local\crash_dump\stdout.txt`

Paste `%LOCALAPPDATA%\FeverLink3` into the Explorer address bar to open the folder.

- **The game shows a red "Reload UI" error:** note the text, send a screenshot, click *Back to Main Menu*, and run `feverlink3 uninstall` if you want to play normally again.
- **The game does not start after the install:** close Steam's launch attempt and run `feverlink3 uninstall` (or delete `winhttp.dll` from the game folder). Tell the maintainer your game build and Windows version. Known causes to check: a game update (the module can only be used with the game builds it was made for), an antivirus that blocked the file, Windows 11 Smart App Control, or another mod that also uses `winhttp.dll`.
- **Your antivirus complains about `winhttp.dll`:** the file is a small proxy, with public source code, that Windows loads for the game. `docs/SECURITY_FOR_USERS.md` explains exactly what it does, how to verify it with a hash, and how to report a false positive.

FeverLink3 builds on two earlier MIT-licensed projects, credited in the README: TpF2 Multiplayer by silver2127 (https://silver2127.github.io/tpf2-multiplayer/) and MPFever by dkdknight / arthur (https://github.com/dkdknight/MPFever).

Contact: Discord `azalshehri`, or https://github.com/AZalshehri7/FeverLink3/issues
