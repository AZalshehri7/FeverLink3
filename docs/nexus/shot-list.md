# Screenshots and GIFs for the Nexus page

For the person who drives the game. Each entry says what to capture, the exact steps in the game, what to hide, the file name, the caption to use, and whether it can be captured today. **Do not capture or publish the blocked entries (S15 to S17) before the feature behind them has been seen working**; the page says plainly that those things are not verified, and a picture must not suggest otherwise.

## Setup that every entry assumes

- Windows 10 or 11, Transport Fever 3 build 40420 from Steam, 1920x1080, game UI scale at its default, borderless or windowed (not smaller than 1600x900).
- FeverLink3 installed with `feverlink3 install` from the unzipped release folder (the release being tested, so that `native.log` shows its version).
- To start a game with the mod: start the game from Steam, **New game**, on the settings page open the **Add Mods** tab, click **Activate** under *FeverLink3 Multiplayer*, then **Start Game**. (Menu names as of build 40420; use what the game shows.)
- In the game, the window opens from the button **FeverLink3** in the bar at the bottom of the screen (hovering shows the tooltip "FeverLink3 multiplayer"). The window is called *FeverLink3 Multiplayer*.
- Tools: Steam's screenshot key (F12 by default) or Win+Shift+S for crops, saved as PNG. GIFs: ScreenToGif or OBS plus ffmpeg, 10 to 15 seconds, 15 frames per second, at most 960 pixels wide, aimed at under 5 MB each (these are our targets; the form's own limits were not readable, so check them).
- Save everything outside the repository, in one folder, with the names below.

## What must be hidden in every picture

- **The join code** while a session is live: anyone who sees it can join. Either blur it, or capture and then press **Stop hosting** at once so the code is dead, and say so in the caption if it is readable.
- **Steam IDs** (17 digits) and your Steam profile name or friends list.
- **Your Windows user name** in paths (`C:\Users\<name>\...`, the title bar of Explorer, terminal prompts). Use a terminal opened in a folder like `C:\FeverLink3`.
- Email addresses, Discord names other than `azalshehri`, anything from other windows on the desktop.

## The list

| ID | Title | Can be captured today? |
|---|---|---|
| S01 | Cover image: the window with the connection test result, over a town | yes |
| S02 | Main menu with the game build number | yes |
| S03 | The mod in the mod list, activated | yes |
| S04 | The FeverLink3 button in the bottom bar | yes |
| S05 | The window before a session (host, join, Steam test) | yes |
| S06 | The window after pressing Host a game: the "not finished" answer | yes, optional (honest about the scope) |
| S07 | GIF: open the window and run the connection test against your own Steam ID | yes |
| S08 | Steam connection test against your own Steam ID | yes (one game, own ID; say so) |
| S09 | `feverlink3 doctor` | yes |
| S10 | `feverlink3 install` | yes |
| S11 | Files after the install (game folder and mod folder) | yes |
| S12 | The SmartScreen prompt and "Run anyway" | yes, needs a browser download |
| S13 | `native.log` | yes |
| S14 | `feverlink3 uninstall` | yes |
| S15 | Two players in one session | **blocked: needs hosting over Steam (P7) and two real games (P8)** |
| S16 | GIF: a road built in one game appears in the other | **blocked: needs command replication (P9)** |
| S17 | Steam connection test between two accounts | **blocked: needs the first two-account test (P6); this is the headline picture of the page** |

## S01 - Cover image

- **Purpose:** the thumbnail. Name: `nexus-01-cover.png`, 1920x1080.
- **Steps:** start a new game with the mod activated (setup above). Move the camera over a town with some roads so the picture is not empty. Click **FeverLink3** in the bottom bar and run the Steam connection test as in S08 so that the result line is showing. Drag the window to a side so it does not hide the town. Take the screenshot with the bar and the window visible.
- **Hide:** both Steam ID fields.
- **Caption:** "The FeverLink3 window with the Steam connection test (test build 0.2.0-test1, tested against my own Steam ID)."
- **Replace later:** when S17 (the test between two accounts) exists, use it as the cover instead.

## S02 - The game build

- **Purpose:** shows which build the mod is made for. Name: `nexus-02-build.png`.
- **Steps:** start the game from Steam and wait for the main menu. Take the screenshot; the build number is at the bottom right of the menu. Crop to the lower right quarter or keep the full frame.
- **Caption:** "Game build 40420, the build this test version is made for."

## S03 - The mod in the mod list

- **Purpose:** how to activate the mod. Name: `nexus-03-modlist.png`.
- **Steps:** main menu, **New game**, settings page, **Add Mods** tab. Find *FeverLink3 Multiplayer*, click **Activate** so that it shows as active, then capture before pressing Start Game.
- **Caption:** "Activate FeverLink3 Multiplayer in the mod list when you start a game."

## S04 - The bar button

- **Purpose:** where to find it. Name: `nexus-04-button.png`.
- **Steps:** in a running game, hover the mouse over the **FeverLink3** button in the bottom bar until the tooltip "FeverLink3 multiplayer" shows. Crop to the bottom bar, wide enough to show the neighbouring item counters.
- **Caption:** "The FeverLink3 button in the bar at the bottom of the screen."

## S05 - The window before a session

- **Purpose:** the first thing a player sees. Name: `nexus-05-window-idle.png`.
- **Steps:** click the button. The window shows "Play together: one player hosts, the others join with the host's code.", the field *Your name*, **Host a game**, the field "paste the join code here" with **Join**, and the *Steam connection test* section. Capture the whole window.
- **Hide:** the Steam ID in the *Your Steam ID* field.
- **Caption:** "Before a session: the Steam connection test is what this build is for. Hosting and joining come in the next test build."

## S06 - The window after pressing Host a game (optional)

- **Purpose:** be honest about the scope of the test build. Name: `nexus-06-host-not-finished.png`.
- **Steps:** from S05, click **Host a game**. The window shows "Could not connect:" with the message that hosting over Steam is not finished in this build yet. Capture the whole window.
- **Hide:** the Steam ID field.
- **Caption:** "In this build the Host button says that hosting over Steam is not finished yet. The connection test below it works."
- **If the window shows a join code instead,** the build is not the one this page describes: do not publish this picture, and report it to the maintainer.

## S07 - GIF: run the connection test

- **Purpose:** the whole test flow in one loop. Name: `nexus-07-steam-test.gif`.
- **Steps:** start recording on the game view with the bar visible. Click **FeverLink3**. In *Steam connection test*, click into the Steam ID field, press Ctrl+A, then Ctrl+C; click into **paste the other player's Steam ID**, press Ctrl+V, click **Test the connection**. Keep recording until the result line shows (about 15 seconds), then stop. Blur both Steam ID fields in every frame, or crop the frames so that they are outside the picture.
- **Caption:** "Run the Steam connection test: copy your Steam ID, paste the other player's, press Test the connection. (Recorded against my own Steam ID.)"

## S08 - Steam connection test, own ID

- **Purpose:** the proof that is real today (P5). Name: `nexus-08-steam-test.png`.
- **Steps:** in a running game that is not hosting or joined, open the window. In *Steam connection test*, click into the field showing your Steam ID, press Ctrl+A, Ctrl+C. Click into **paste the other player's Steam ID**, press Ctrl+V, click **Test the connection**. Wait about 15 seconds. A line appears below the button, in the form `Test: connected, answers N, last round trip X ms, direct` (or `through a Steam relay`). Capture the window.
- **Hide:** both Steam ID fields (they hold the same ID).
- **Caption (use exactly this):** "Steam connection test against my own Steam ID, inside one game. A test between two different accounts has not been run yet."

## S09 - `feverlink3 doctor`

- **Purpose:** transparency about what the installer sees. Name: `nexus-09-doctor.png`.
- **Steps:** double-click `feverlink3.exe` in the unzipped release folder: the checks screen (lines marked `[ok]`, `[??]`, `[!!]`, `[--]` and the line **Next step**) is the picture. For the command-line version, open a terminal in that folder and run `feverlink3 doctor`. Capture the output.
- **Hide:** Windows user name in any path.
- **Caption:** "The installer checks your PC and says what to do next. It changes nothing until you choose."

## S10 - `feverlink3 install`

- **Purpose:** what the install prints and changes. Name: `nexus-10-install.png`.
- **Steps:** close the game. Double-click `feverlink3.exe`, type `1` and press Enter, and capture the screen with "[ok] FeverLink3 is installed." Or, in a terminal, run `feverlink3 install --dry-run` first (lines start with WOULD), then, on a clean install, `feverlink3 install` (lines start with DONE) and capture the real run.
- **Hide:** Windows user name in paths.
- **Caption:** "The installer lists every item it installs: the mod, steam_appid.txt, winhttp.dll and feverlink3_path.txt. --dry-run shows the plan first."

## S11 - Files after the install

- **Purpose:** shows the footprint. Name: `nexus-11-files.png`.
- **Steps:** open the game folder (Steam, right-click the game, Manage, Browse local files). Show `winhttp.dll`, `steam_appid.txt` and `feverlink3_path.txt` next to `TransportFever3.exe`. In a second Explorer window open `userdata\<id>\3493540\local\mods` and show the folder `feverlink3_1`. Combine the two windows in one image.
- **Hide:** the Steam user id folder name (crop the address bar or blur the digits) and any user name.
- **Caption:** "Everything FeverLink3 puts on your PC: three small files next to the game program and one mod folder."

## S12 - SmartScreen

- **Purpose:** show the first-run warning honestly, with the way through it. Names: `nexus-12-smartscreen-1.png`, `nexus-12-smartscreen-2.png`.
- **Steps:** download the release zip with a browser (so that it carries the download mark), extract it with Explorer's Extract All, double-click `feverlink3.exe`. Capture "Windows protected your PC" (publisher: Unknown publisher). Click **More info** and capture the dialog with **Run anyway**.
- **Caption:** "Windows shows this once because the program is not code-signed yet. More info, then Run anyway. Code signing is planned."

## S13 - `native.log`

- **Purpose:** the module's own log is short and readable. Name: `nexus-13-native-log.png`.
- **Steps:** after a game start, open `%LOCALAPPDATA%\FeverLink3\native.log` in Notepad. It holds lines such as "FeverLink3 native 0.2.0-test1 loaded", "game build 40420 (timestamp 0x6ac50427): hooks available" and "live session controller started".
- **Hide:** nothing in the file, but check that the title bar and path do not show the user name.
- **Caption:** "The module's whole log. 'hooks available' only means that the game build is one the module knows; no hooks are installed unless a developer switches them on, and they have never run in the real game."

## S14 - `feverlink3 uninstall`

- **Purpose:** show that removal is one command. Name: `nexus-14-uninstall.png`.
- **Steps:** close the game, double-click `feverlink3.exe`, type `2`, answer yes, and capture the result; or run `feverlink3 uninstall` in a terminal and capture the lines marked DONE.
- **Hide:** user name in paths.
- **Caption:** "One command removes the mod and our winhttp.dll. It never touches another mod's file."

## S15 - Two players in one session (BLOCKED)

- **Needs:** hosting and joining over Steam (P7), and two real copies of the game connected with the host's world loaded on the second (P8).
- **When unblocked:** capture the *joined* window ("Connected", "Wait for the host to start the session.", *Players* with both names) and the host's window showing both names. Name: `nexus-15-two-players.png`. Hide join codes and Steam IDs. Caption: "A second player has joined" and nothing about building together.

## S16 - GIF: a road built in one game appears in the other (BLOCKED)

- **Needs:** command replication between two real games (P9).
- **When unblocked:** two game windows side by side; build one road piece in the host's game and show it appearing in the other. Name: `nexus-16-build-together.gif`. Hide codes and IDs. This is the headline picture of the page; it must not exist before it is true.

## S17 - Steam connection test between two accounts (BLOCKED)

- **Needs:** the first run of the test between two different Steam accounts on two PCs (P6).
- **When unblocked:** the same screenshot as S08, taken on one of the two PCs, with the result line. Name: `nexus-17-steam-two-accounts.png`. Caption states both facts: the result line, and that the two PCs were on different accounts and networks. If the test fails, do not publish; report it.

## After capturing

Put the files in the order S01, S05, S06, S08 (this one only with its exact caption), S10, S11, S12, S13, with S07 as the first video or GIF slot if the form allows it. Update `README.md` in this folder if any entry changes.
