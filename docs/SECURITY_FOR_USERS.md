# What is in winhttp.dll, and how to check it

For players, and for anyone reading an antivirus report about FeverLink3. Last updated 2026-10-22 (version 0.2.1). Everything here can be checked against the source code in this repository (public, under the PolyForm Noncommercial License 1.0.0: free for noncommercial use), and where something has not been verified this page says so. If a build you downloaded behaves differently from what is written here, that is a bug in the build or in this page: please tell us (Discord `azalshehri`, or https://github.com/AZalshehri7/FeverLink3/issues).

## The short version

- A player installs two things by hand, from the release zip: `winhttp.dll` and the mod folder `mods\release\feverlink3`, both in the game folder. There is no installer and no FeverLink3 program to run.
- FeverLink3 puts one DLL named `winhttp.dll` in the Transport Fever 3 game folder. Windows loads it automatically when the game starts, because the game imports `WINHTTP.dll` and Windows looks in the game's own folder before the system folder.
- The DLL passes the 14 WinHTTP functions the game uses to the real Windows file, unchanged. The game's own web traffic is not read or modified.
- Besides that, inside the game it does three things: it writes a small log, it reads the game's build number from the game's own memory, and it starts one thread that runs the multiplayer session the player starts from the in-game window. In any other program it does nothing but pass calls through.
- It does not inject into other programs, does not start with Windows, does not need administrator rights and sends no telemetry.
- **It updates itself** (from 0.2.0 on): once per game start it asks GitHub for the newest FeverLink3 release and, if it is newer, downloads it inside the game with Windows' own WinHTTP (no other program is started), checks every file's SHA-256 and installs it for the next start. Releases are **not signed until 1.0.0**: you trust this repository's release page. You can turn it off in the window's Troubleshooting section. Details: "Automatic updates" below.
- It is **not code-signed until 1.0.0**, which is one reason some antivirus programs may look at it closely. The sections below explain how to verify the file, how to remove it, and how to report a false positive.

## Why a file called winhttp.dll

Windows programs find the DLLs they import in a fixed order, and the folder the program was started from comes before the Windows system folder (for DLLs that are not on the "known DLLs" list, which `winhttp.dll` is not). A DLL with the right name in the game folder is therefore loaded by the game itself, with no injection. FeverLink3 uses that to get a small piece of code running inside the game. The idea comes from MPFever (by dkdknight), which does the same, and is credited in the README.

We want to be plain about this: the same loading trick is used by malware, and security people have a name for it (DLL search order hijacking and side-loading, MITRE ATT&CK T1574.001 and T1574.002). That is why scanners are suspicious of any `winhttp.dll` that sits next to a program, and why a new, unsigned, rarely-seen file is more likely to be flagged. What separates FeverLink3 from the malware version of the trick is not the technique but what the file does, that the source is public, and that you can check the file against the release. The rest of this page is about those things.

## What the DLL does, exactly

### 1. It passes the WinHTTP calls through

These are the 14 functions the game imports from `WINHTTP.dll` (the address tool in `tools/addr` lists them from the game executable and checks them against the proxy; for game builds 40420 and 40423 the lists are identical, see `tools/addr/out/6ac50427.txt` and `6ad0e4e5.txt`):

`WinHttpOpen`, `WinHttpConnect`, `WinHttpOpenRequest`, `WinHttpSetOption`, `WinHttpReadData`, `WinHttpSetStatusCallback`, `WinHttpCloseHandle`, `WinHttpWriteData`, `WinHttpQueryDataAvailable`, `WinHttpSetTimeouts`, `WinHttpQueryHeaders`, `WinHttpAddRequestHeaders`, `WinHttpSendRequest`, `WinHttpReceiveResponse`.

On the first call of each one, the DLL loads the real library by its full path (`<Windows system folder>\winhttp.dll`, so it can never find itself), looks the function up by name, remembers the address and calls it with exactly the arguments it was given. If the real function cannot be found, the call returns an error to the game. The code is `native/src/proxy.rs`, about 130 lines.

### 2. What happens when the game loads it

When Windows loads the DLL, its start routine does this and nothing else:

1. It checks two things: whether the environment variable `FEVERLINK3_SESSION_DIR` is set (only the developers' command-line program sets it; it is not part of the release), and whether the program that loaded the DLL is `TransportFever3.exe`.
2. If neither is true, it returns at once. In any other program the DLL is only the pass-through of step 1.
3. Otherwise it creates the folder `%LOCALAPPDATA%\FeverLink3\` (or the session folder, when the developers' program started the game) and appends lines to `native.log` there: that it loaded, the game's build timestamp, and whether that build is one the module knows. The build is identified from the game's own program headers in memory (the link timestamp). Build 40420 is the only known build.
4. Inside the game, if an earlier game run downloaded an update, it finishes it now (renames files in the game folder, before the game reads its mods) or puts the previous version back ("Automatic updates" below).
5. Inside the game it starts one thread named `feverlink3-live` and one named `feverlink3-update` (the update check, once, then it ends).

**On a known game build it patches the game's code in memory, and only while a FeverLink3 session exists.** The DLL contains inline detours and byte patches with the byte patterns of builds 40420 and 40423 (`native/src/hooks.rs`, `detour.rs`, `patch.rs`, `build_table.rs`). Five pieces are on by default: a hook that counts the simulation's commands, a hook on the command list that can hold a construction command until the session says when to apply it, a patch that makes the script pool single-threaded, a patch that pins the seed of Lua's string hash, and a one-byte patch that makes the engine's tickCount getter return the simulation's step count. Each checks the bytes it is about to change against the build table and installs nothing (and logs why) on an unknown build or different bytes; each is turned off by setting its variable to `0` (`FEVERLINK3_HOOKS`, `FEVERLINK3_HOLD`, `FEVERLINK3_POOLPATCH`, `FEVERLINK3_LUA_SEED`, `FEVERLINK3_STEP_SEED`). The diagnostic hooks (town seed and growth logs) are off unless a developer sets their variable to `1`. See "What may change" below.

### 3. What the thread does while the game runs

The thread (`launcher/src/live.rs`) wakes every 100 milliseconds and:

- Reads a text file written by the in-game window (`ctl_cmd.log`) to see whether the player clicked Host, Join, Leave, Start and so on.
- Writes its answer to another text file (`ctl_status.txt`) that the window shows. Both files are in `%LOCALAPPDATA%\FeverLink3\live\`, which is emptied at every game start.
- At start it reads (does not write) the Windows registry values that tell where Steam is installed (`HKCU\Software\Valve\Steam\SteamPath`, falling back to `HKLM\SOFTWARE\WOW6432Node\Valve\Steam\InstallPath`), then Steam's `libraryfolders.vdf` and the names of the folders under `userdata`, so that it knows where the game and its saves are.
- Looks up Steam's networking functions by name inside `steam_api64.dll`, the library the game has already loaded, and opens a Steam peer-to-peer listener. In this version that listener is opened when the game starts, not only when you host; it accepts incoming Steam connections and answers messages that start with `ping ` with a `pong`, and ignores all other data. This only happens when Steam is running; the DLL does not start Steam.
- Only when the player starts or joins a session from the window does it run one. A session talks to the other players (see "Network" below), and exchanges text files with the Lua mod in the game. In a world sync the host's game saves a file named `feverlink3-resync-<number>.sav` in the game's save folder; the DLL sends it to the other players, and a joining player's copy is written to the same folder. The receiver checks the SHA-256 of the whole file before writing it.

### 4. What it does not do

These are statements about the code in this repository; you can search for the calls yourself.

- It does not open other processes, write into their memory, create remote threads or install Windows hooks (the source contains none of `OpenProcess`, `WriteProcessMemory`, `CreateRemoteThread`, `SetWindowsHookEx`).
- It does not make itself start with Windows: no registry writes at all, no startup entries, no scheduled tasks, no services. It does not need or ask for administrator rights.
- It does not read your documents, browser data, passwords or other programs' files. The files it reads are its own text files, the game's save folder entry named above, and the Steam path in the registry.
- It does not log keys, take screenshots or record audio.
- It does not contact any server of the project or send analytics or ads. Its only web requests are the update check at game start and the update download, both to GitHub ("Automatic updates" below; off when you turn it off).
- It does not download or run programs other than FeverLink3's own release (the automatic update, which downloads inside the game with Windows' WinHTTP and starts no program). (A joined session does load the host's savegame in your game, and a savegame can carry game scripts, as a mod can. That is why you should only join people you trust; see "Who you are trusting".)
- It does not use WinHTTP for its own traffic, so your game's web traffic and FeverLink3's session traffic are separate.

### 5. How the patches are put in place

For the pieces of section 2 (the hooks in the game's own code and the small byte patches), on a known build the module first checks the exact bytes it expects at each place, then runs a self-test of its own detour machinery (it writes a few instructions of its own into a small block of executable memory, which it allocates for the purpose and never frees, and runs them), then writes the changes all-or-none, rolls back if one fails, keeps the DLL loaded for the rest of the process, and starts a second thread that exchanges two text files with the mod (`native_ctl.log`, `native_events.log`). Every step is written to `native.log`. Patching another program's code in memory and running generated code are exactly what security products watch for; any new patch is announced in the changelog and described on this page.

### Known gaps

- The DLL does not yet have a version-information resource, so the Details tab in its Properties is empty. That makes it look less trustworthy than it is.
- A bug in FeverLink3's code running inside the game can crash the game, because the build aborts instead of unwinding on an internal error (open point in [the architecture](ARCHITECTURE.html), section 12).
- If a game update makes the game import one more WinHTTP function than the 14 listed above, the game may fail to start while this DLL is in place. Removing the DLL fixes that. The address tool checks the import list of every game build we add to the module.
- Not tested yet: Windows 11 with Smart App Control switched on (see below), and other mods that also place a `winhttp.dll` in the game folder (when you copy the files, Windows asks before it replaces an existing `winhttp.dll`: do not replace it, see the [playing guide](PLAYING.html)).

## No program to run

The release zip holds `winhttp.dll`, the mod folder `mods\release\feverlink3` and some text files. You copy the DLL and the mod folder into the game folder yourself; there is no installer and no FeverLink3 program for players to start. (The repository also holds a command-line program for developers and lab tests; it is not in the release zip, and nothing on this page depends on it.)

## Files FeverLink3 creates or changes

| Where | What | Who writes it | When you uninstall |
|---|---|---|---|
| `<game folder>\winhttp.dll` | the DLL | you, when you install; the automatic update | delete it |
| `<game folder>\mods\release\feverlink3\` | the mod (Lua game scripts and the window) | you, when you install; the automatic update | delete it |
| `%LOCALAPPDATA%\FeverLink3\native.log` | the DLL's text log | the DLL | optional: delete the folder |
| `%LOCALAPPDATA%\FeverLink3\live\` | session files: `ctl_cmd.log`, `ctl_status.txt`, `identity.txt`, `to_game.log`, `from_game.log`, `mod.log`, `hud_state.txt`, `ui_out.log`, `resync_cursor.txt`; emptied at every game start | the DLL and the mod | optional: delete the folder |
| `<Steam>\userdata\<id>\3493540\local\save\feverlink3-resync-<number>.sav` | the host's world while it is synced to the players. These are ordinary game saves and can be large | the game and the DLL | optional: delete them |
| `<game folder>\feverlink3_update\` | the automatic update: `state.json` (what the next start must finish), `<version>\` (the checked new files), `backup-<version>\` (the previous mod, until the new version started well), `download\` (during a download) | the DLL | delete it (only while the game is closed) |
| `<game folder>\winhttp.dll.old-<version>`, `winhttp.dll.failed-<version>`, `feverlink3_update_skip.txt` | the previous DLL until the new one started well; a DLL that failed and was taken back; versions not to offer again | the DLL | delete them |
| `%LOCALAPPDATA%\FeverLink3\settings.txt` | `autoupdate=off` when you turned automatic updates off | the DLL, when you click the switch | optional: delete the folder |

`<game folder>` is `<Steam library>\steamapps\common\Transport Fever 3\`. To open it: in Steam, right-click Transport Fever 3, Manage, Browse local files.

## Network

What FeverLink3 uses to connect players, in this version:

- **Steam networking (primary).** The DLL asks the Steam library that the game already runs for a peer-to-peer connection to the other player's Steam ID. Steam finds the route between the two PCs and may relay the traffic through its own servers. The sockets belong to Steam's library and use Steam's ports; FeverLink3 does not open a listening port of its own for this. Nothing about it needs to be configured by the player.
- **Status, plainly:** whole sessions between two different Steam accounts on two PCs were played over the internet with a friend through Steam, in sessions of up to 3.5 hours: joining with a code, sending the host's world, building together and repair syncs.
- **No server of the project is involved.** The path between two players is Steam's. If that ever changes, this page will say what is added and what it can see.
- **The host does not open an inbound TCP port.** The game with FeverLink3 listens on no TCP port of its own. (A TCP connection for local tests exists only in the developers' command-line program, which is not in the release.)
- **Nothing else leaves the PC.** No analytics, no advertising, no account. The only web requests are the automatic update's (at game start, to GitHub's API and release files, "Automatic updates" below).

What the other players receive: the display name you chose (it starts as your Windows user name, so change it if you prefer), your game actions, world checksums, chat lines, and, if you host, the world itself. A player who tests the connection by Steam ID shares that Steam ID with the person they test with.

How the session traffic is protected: every frame is encrypted and authenticated by FeverLink3 itself (AES-256-GCM, a key derived with HKDF from the secret inside the join code, a replay window per sender), independent of the path it travels on. The host decides who may send what. Anyone who has the join code can join, so treat the code like a password for that session. A join code holds the session secret, the time it was made and how to reach the host (for sessions over Steam, the host's Steam ID). The details are in [the wire protocol](PROTOCOL.html) and section 3 of [the architecture](ARCHITECTURE.html).

### Check it yourself

With the game running, in PowerShell:

```powershell
$p = (Get-Process TransportFever3).Id
Get-NetTCPConnection -OwningProcess $p -State Listen      # listening TCP sockets of the game: expect none from FeverLink3
Get-NetUDPEndpoint   -OwningProcess $p                    # UDP sockets of the game (these may include Steam's networking sockets)
Get-NetTCPConnection -OwningProcess $p -State Established # outgoing TCP connections (the game and Steam have their own)
```

(An error saying that no matching connection was found also means there is none.) For a closer look, Sysinternals Process Monitor with the filter "Process Name is TransportFever3.exe" and "Path contains FeverLink3" lists every file the DLL and the mod touch, and Wireshark or Resource Monitor shows the traffic. If you see a listening TCP socket you do not expect, or a file access outside the places in the table above, please tell us.

## Automatic updates

From 0.2.0 on, FeverLink3 updates itself. This section says exactly what it does and what it trusts. The code is `launcher/src/autoupdate.rs`; the checks are in `launcher/src/update.rs`.

**When.** Once per game start, right after the module has loaded, on a thread of its own. It does not start while you are in a session and never changes a file during one (it waits until the session is over). It is not a background service: when the game is closed, nothing runs.

**What it asks.** One HTTPS request to `https://api.github.com/repos/AZalshehri7/FeverLink3/releases/latest`, GitHub's page of the newest full release of this repository (drafts and pre-releases are never listed there, and the module refuses them again). If that release is not newer than yours, nothing else happens. The request carries no personal data beyond what any web request carries (your IP address reaches GitHub; the user agent is `feverlink3-updater/<version>`).

**What it downloads, and the checks.** The release's `manifest.json`, its zip and the zip's `.sha256` file, only from `https://github.com/AZalshehri7/FeverLink3/releases/download/<that release's tag>/`, inside the game with Windows' own WinHTTP (`C:\Windows\System32\winhttp.dll`; no program is started; HTTPS only, also for redirects, only to GitHub's addresses `api.github.com`, `github.com`, `objects.githubusercontent.com` and `release-assets.githubusercontent.com`, TLS 1.2 or newer, the server's certificate checked by Windows, size and time limits). The first check waits two minutes after the game started. The update is used only if all of these hold:

1. the version is strictly newer than the one you run, and the manifest names that same version and zip;
2. the zip's SHA-256 equals the manifest's `zip_sha256` and the published `.sha256` file;
3. every file taken from the zip has exactly the size and SHA-256 the manifest lists;
4. every file name is `winhttp.dll`, under `mods/release/feverlink3/`, or one of the documents (which are never copied), with no `..`, drive letter, absolute path or other trick; a release that carries anything else is refused as a whole.

**Unsigned until 1.0.0.** Releases before 1.0.0 carry no signature. The checks above prove that the files are the ones published on this repository's release page and that nothing was changed on the way; they do **not** protect you if someone takes over the repository (a stolen GitHub account or token) and publishes a release. From 1.0.0 on the release manifest is to be signed with a key built into FeverLink3, and from then on an update without a good signature is refused (planned, [signing plan](https://github.com/AZalshehri7/FeverLink3/blob/main/docs/SIGNING.md)). If you do not want to trust the repository's release page automatically, turn automatic updates off (below) and update by hand after checking a release yourself ("Verify what you downloaded").

**How it changes your files.** The new files are checked and stored in `<game folder>\feverlink3_update\<version>\` first. Then, still in that game run, the running `winhttp.dll` is renamed to `winhttp.dll.old-<old version>` (Windows allows renaming a loaded DLL; the game keeps using the old one in memory) and the new one is put in its place. The window says "Updated to X. Restart the game to finish." At the next game start the new DLL loads first and, before the game reads any mod, moves `mods\release\feverlink3` to `feverlink3_update\backup-<old version>\` and the new mod folder into its place. 30 seconds into that game the backup, the old DLL and the staged files are deleted, and so is the `feverlink3_update` folder once it is empty. Nothing else in the game folder is touched, and nothing outside it.

**If something goes wrong.** If the new version does not stay up for 30 seconds in two starts, or the mod folder cannot be swapped, the old DLL and the old mod are put back, the version goes on the skip list (`feverlink3_update_skip.txt`) and is not offered again, and the window says "Update to X failed: <reason>. You keep <old version>." A game run whose files on disk no longer match the DLL in memory does not let you host or join until you restart it.

**Turning it off.** In the FeverLink3 window, Troubleshooting, "Automatic updates: on" (click to switch to off). The setting is stored in `%LOCALAPPDATA%\FeverLink3\settings.txt` (`autoupdate=off`) and counts from the next game start. A FeverLink3 that you built yourself from the source never updates itself (only the release workflow's builds do).

## Verify what you downloaded

Each release has these files: `FeverLink3-<version>.zip`, `FeverLink3-<version>.zip.sha256` and `manifest.json` (every file in the zip with its size and SHA-256, and the zip's SHA-256). The release notes repeat the zip's SHA-256. Releases are built by GitHub Actions from a commit on `main`; nothing is built on a developer's PC ([release process](https://github.com/AZalshehri7/FeverLink3/blob/main/docs/RELEASING.md)).

In PowerShell, in the folder with the downloaded files:

```powershell
(Get-FileHash .\FeverLink3-0.2.1.zip -Algorithm SHA256).Hash.ToLower()
Get-Content .\FeverLink3-0.2.1.zip.sha256

# the DLL you installed against the manifest (your game folder: Steam, right-click Transport Fever 3, Manage, Browse local files)
$m = Get-Content .\manifest.json -Raw | ConvertFrom-Json
($m.files | Where-Object path -eq 'winhttp.dll').sha256
(Get-FileHash 'C:\Program Files (x86)\Steam\steamapps\common\Transport Fever 3\winhttp.dll' -Algorithm SHA256).Hash.ToLower()
```

The values in each pair must be identical, and equal to the ones in the release notes. If they differ, do not use the file. Use the path of your own game folder in the last command.

What this does and does not prove: the zip is assembled reproducibly (the same inputs give the same bytes), so the zip hash proves that you have the file the release job produced. We have not verified that two independent compilations of the DLL give identical bytes, so the way to check the code itself is to read it and build it yourself (`cargo build --release -p feverlink3_native`, see the README) and compare behaviour, not to expect the same hash. Releases before 1.0.0 are not signed: neither the manifest nor the DLL carries a signature (the [signing plan](https://github.com/AZalshehri7/FeverLink3/blob/main/docs/SIGNING.md) is for 1.0.0).

Get the file only from the project's release page. A file called `winhttp.dll` from anywhere else is not ours.

## If your antivirus flags it

As a player:

1. Do not turn your antivirus off. Compare the file's hash with the release (above).
2. If the hash matches and you trust the project: restore the file (Windows Security, Virus and threat protection, Protection history, pick the item, Actions, Restore or Allow) and, if it comes back, add an exclusion for that one file instead of a whole folder. If you do not trust it, remove FeverLink3 ("Remove it" below).
3. Report the false positive, so it is fixed for everyone. A report that names the exact detection and carries the file's SHA-256 is the most useful, and vendors take more notice when several people report the same file.

Things in the file that make heuristics suspicious, so you know what to expect: it is a proxy DLL next to a program, it is not signed, it carries no version information, it is new and few people have it (reputation systems distrust rare files), it starts a thread, reads the Steam path from the registry, and uses networking.

### Where to report a false positive

Checked on 2026-10-10 by loading or searching the vendor's own page: Microsoft, Kaspersky, ESET, Malwarebytes. The other rows are taken from the list that AV-Comparatives published in May 2023 (https://av-comparatives.org/dealing-with-false-positives-reporting-issues-to-antivirus-vendors/); those pages refused automatic access when this was written, so open the page and check it before relying on it.

| Product | Where | Notes |
|---|---|---|
| Microsoft Defender | https://www.microsoft.com/en-us/wdsi/filesubmission (short link https://aka.ms/wdsi) | Choose "Software developer" if you are the author or "Home" for a user, then "Incorrectly detected as malware" (or "as PUA"). Fields: the product that flagged it, the detection name (Windows Security, Protection history), the file (up to 50 MB), extra information in English (up to 1,900 characters). A Microsoft account is optional and lets you track the result. Low-priority submissions may not reach an analyst. Guide: https://learn.microsoft.com/en-us/defender-xdr/submission-guide |
| Microsoft SmartScreen | https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation | "Windows protected your PC" is a reputation prompt, not a malware verdict. There is no form to ask for a review of an unsigned file on a consumer PC; reputation builds with signing and downloads. |
| Kaspersky | https://support.kaspersky.com/1870 | Look the file up in OpenTip (https://opentip.kaspersky.com), then use "Submit to reanalyze"; the form asks for an email address. |
| ESET | samples@eset.com (https://support.eset.com/en/kb141-submit-a-virus-website-or-potential-false-positive-sample-to-the-eset-lab) | Put the file in a zip with the password `infected`, subject "False positive", attach it with a screenshot of the detection. Or use the product: Tools, Quarantine, right-click, Submit sample for analysis. |
| Malwarebytes | https://help.malwarebytes.com/hc/en-us/articles/31589211404571-Report-a-false-positive-to-Malwarebytes-Support | Paid subscribers go through support; others post in the Malwarebytes forum (https://forums.malwarebytes.com), which needs an account. |
| Avast | https://www.avast.com/false-positive-file-form.php | |
| AVG | https://www.avg.com/en-us/false-positive-file-form | |
| Bitdefender | https://www.bitdefender.com/consumer/support/answer/29358/ | |
| Norton | https://submit.norton.com/ | |
| McAfee | https://www.mcafee.com/support/?articleId=TS103032 | |
| Sophos | https://support.sophos.com/support/s/article/KBA-000001443 | |
| Trend Micro | https://success.trendmicro.com/en-US/solution/KA-0008383 | |
| F-Secure / WithSecure | https://www.withsecure.com/en/support/contact-support/submit-a-sample | |
| G DATA | https://www.gdatasoftware.com/faq/consumer/submit-a-suspicious-file-app-or-url | |
| Avira | https://www.avira.com/en/analysis/submit | |
| Panda / WatchGuard | https://www.watchguard.com/wgrd-support/security-portal/report-false-positive | |

Tips that every vendor asks for: name the product and the detection, send the exact file and its SHA-256, and say what the file is. Scanning the file at https://www.virustotal.com first shows whether one engine is the outlier (an outlier is usually a heuristic false positive); note that VirusTotal shares what you upload with its partners, which is fine here because the file's source code is public.

### Text you can paste into a report

> False positive: `winhttp.dll` from FeverLink3 (public source code, PolyForm Noncommercial licence).
> File: `winhttp.dll`, SHA-256 `<paste the hash>`, from https://github.com/AZalshehri7/FeverLink3/releases (tag `<version>`).
> What it is: a proxy DLL for the game Transport Fever 3 (Steam app 3493540). It passes the 14 WinHTTP functions the game imports to the Windows system library unchanged, and runs a multiplayer session controller in one thread inside the game. It does not inject into other processes, persist, collect data or download code. Source and a description of its behaviour: https://github.com/AZalshehri7/FeverLink3 (folders `native/` and `launcher/`, page `docs/SECURITY_FOR_USERS.md`). It is built by GitHub Actions from the tag and is not code-signed yet.
> Detection seen: `<product and detection name>`.
> Why we think it is a false positive: a DLL placed in an application's folder matches the generic pattern for DLL side-loading, but the file's behaviour is documented and its source is public.
> Please re-analyse it and remove the detection if you agree. Contact: `<your email or GitHub issue link>`.

## Windows warnings: SmartScreen and Smart App Control

- **SmartScreen** ("Windows protected your PC") checks programs that you start yourself. Players start no FeverLink3 program (the game loads the DLL), so it is not expected to appear (not yet tested on a clean PC).
- **Smart App Control** (Windows 11) in its "On" setting blocks programs and DLLs that Microsoft does not recognise unless they are signed with a trusted certificate (https://learn.microsoft.com/en-us/windows/apps/develop/smart-app-control/overview). We have not tested FeverLink3 with it on. If it is on and the game will not start after the install, delete `winhttp.dll` from the game folder. Code signing is the real fix and is planned for 1.0.0 ([signing plan](https://github.com/AZalshehri7/FeverLink3/blob/main/docs/SIGNING.md)).

## Who you are trusting

- **This project's code and release.** You can read it, build it for noncommercial use and compare the release against the checks above. With automatic updates on, you also trust this repository's release page for every later version (unsigned until 1.0.0, see "Automatic updates").
- **The host of a session.** When you join, your game loads the host's savegame. A Transport Fever savegame can carry game scripts, as a mod can, so joining a session means trusting the host as you would trust a mod author. Only join people you know.
- **Everyone who has the join code.** They can join the session and act in the world, within the permissions the host sets. Do not post a code in public, and stop hosting when you are done.
- **Steam,** for the route between the games.

## Remove it

1. Close the game. In the game folder delete `winhttp.dll` (check first that it is ours, with the hash above, if another mod might have placed one) and the folder `mods\release\feverlink3`. Also delete, if they are there, `winhttp.dll.old-*`, `winhttp.dll.failed-*`, the folder `feverlink3_update` and `feverlink3_update_skip.txt`. Steam's "Verify integrity of game files" does not remove these files.
2. Optional: delete `%LOCALAPPDATA%\FeverLink3\` (logs and session files) and the files named `feverlink3-resync-*.sav` in your save folder.

The game keeps no other trace of FeverLink3. The mod declares that removing it has no effect on a save (`severityRemove: None` in its `mod.json`); the game may still list it as missing when you load a save that was made with it.

## Reporting a security problem

If you think you found a vulnerability, or a build that does more than this page says, tell the maintainer directly on Discord (`azalshehri`) before posting details in public, or open an issue at https://github.com/AZalshehri7/FeverLink3/issues without sensitive detail and ask for a private channel. The project's threat model and policy are in [Security](SECURITY.html).
