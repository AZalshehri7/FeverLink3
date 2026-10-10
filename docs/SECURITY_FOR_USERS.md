# What is in winhttp.dll, and how to check it

For players, and for anyone reading an antivirus report about FeverLink3. Written on 2026-10-10 for the test build 0.2.0-test1. Everything here can be checked against the source code in this repository (public, under the PolyForm Noncommercial License 1.0.0: free for noncommercial use), and where something has not been verified this page says so. If a build you downloaded behaves differently from what is written here, that is a bug in the build or in this page: please tell us (Discord `azalshehri`, or https://github.com/AZalshehri7/FeverLink3/issues).

## The short version

- FeverLink3 puts one DLL named `winhttp.dll` in the Transport Fever 3 game folder. Windows loads it automatically when the game starts, because the game imports `WINHTTP.dll` and Windows looks in the game's own folder before the system folder.
- The DLL passes the 14 WinHTTP functions the game uses to the real Windows file, unchanged. The game's own web traffic is not read or modified.
- Besides that, inside the game it does three things: it writes a small log, it reads the game's build number from the game's own memory, and it starts one thread that runs the Steam connection test and, in later test builds, the multiplayer session that the player starts from the in-game window. In any other program it does nothing but pass calls through.
- It does not inject into other programs, does not start with Windows, does not need administrator rights, sends no telemetry, and does not download or run programs.
- It is **not code-signed yet**. That is why Windows shows a warning the first time, and why some antivirus programs may look at it. The sections below explain how to verify the file, how to remove it, and how to report a false positive.

## Why a file called winhttp.dll

Windows programs find the DLLs they import in a fixed order, and the folder the program was started from comes before the Windows system folder (for DLLs that are not on the "known DLLs" list, which `winhttp.dll` is not). A DLL with the right name in the game folder is therefore loaded by the game itself, with no injection. FeverLink3 uses that to get a small piece of code running inside the game. The idea comes from MPFever (by dkdknight), which does the same, and is credited in the README.

We want to be plain about this: the same loading trick is used by malware, and security people have a name for it (DLL search order hijacking and side-loading, MITRE ATT&CK T1574.001 and T1574.002). That is why scanners are suspicious of any `winhttp.dll` that sits next to a program, and why a new, unsigned, rarely-seen file is more likely to be flagged. What separates FeverLink3 from the malware version of the trick is not the technique but what the file does, that the source is public, and that you can check the file against the release. The rest of this page is about those things.

## What the DLL does, exactly

### 1. It passes the WinHTTP calls through

These are the 14 functions the game imports from `WINHTTP.dll` (the address tool in `tools/addr` lists them from the game executable and checks them against the proxy; for game build 40420 the lists are identical, see `tools/addr/out/6ac50427.txt`):

`WinHttpOpen`, `WinHttpConnect`, `WinHttpOpenRequest`, `WinHttpSetOption`, `WinHttpReadData`, `WinHttpSetStatusCallback`, `WinHttpCloseHandle`, `WinHttpWriteData`, `WinHttpQueryDataAvailable`, `WinHttpSetTimeouts`, `WinHttpQueryHeaders`, `WinHttpAddRequestHeaders`, `WinHttpSendRequest`, `WinHttpReceiveResponse`.

On the first call of each one, the DLL loads the real library by its full path (`<Windows system folder>\winhttp.dll`, so it can never find itself), looks the function up by name, remembers the address and calls it with exactly the arguments it was given. If the real function cannot be found, the call returns an error to the game. The code is `native/src/proxy.rs`, about 130 lines.

### 2. What happens when the game loads it

When Windows loads the DLL, its start routine does this and nothing else:

1. It checks two things: whether the environment variable `FEVERLINK3_SESSION_DIR` is set (only the command-line development launcher sets it), and whether the program that loaded the DLL is `TransportFever3.exe`.
2. If neither is true, it returns at once. In any other program the DLL is only the pass-through of step 1.
3. Otherwise it creates the folder `%LOCALAPPDATA%\FeverLink3\` (or the session folder, when the development launcher started the game) and appends lines to `native.log` there: that it loaded, the game's build timestamp, and whether that build is one the module knows. The build is identified from the game's own program headers in memory (the link timestamp). Build 40420 is the only known build.
4. Inside the game it starts one thread named `feverlink3-live`.

**By default it hooks nothing.** The DLL does contain code that can patch the game's own code in memory (inline detours and one byte patch, with the byte patterns of build 40420: `native/src/hooks.rs`, `detour.rs`, `patch.rs`, `build_table.rs`). The start routine calls it, but it returns at once unless the game was started with the environment variable `FEVERLINK3_HOOKS=1` or `FEVERLINK3_POOLPATCH=1` (and `FEVERLINK3_HOLD=1` for the second hook), each with exactly the value `1`. The installer, the window and the release do not set these variables; a player would have to set them on purpose, and this is for developers testing the hooks. That code is tested on fake game functions only and has never run in the real game. See "What may change" below.

### 3. What the thread does while the game runs

The thread (`launcher/src/live.rs`) wakes every 100 milliseconds and:

- Reads a text file written by the in-game window (`ctl_cmd.log`) to see whether the player clicked Host, Join, Leave, Start and so on.
- Writes its answer to another text file (`ctl_status.txt`) that the window shows. Both files are in `%LOCALAPPDATA%\FeverLink3\live\`, which is emptied at every game start.
- At start it reads (does not write) the Windows registry values that tell where Steam is installed (`HKCU\Software\Valve\Steam\SteamPath`, falling back to `HKLM\SOFTWARE\WOW6432Node\Valve\Steam\InstallPath`), then Steam's `libraryfolders.vdf` and the names of the folders under `userdata`, so that it knows where the game and its saves are.
- Looks up Steam's networking functions by name inside `steam_api64.dll`, the library the game has already loaded, and opens a Steam peer-to-peer listener. In this version that listener is opened when the game starts, not only when you host; it accepts incoming Steam connections and answers messages that start with `ping ` with a `pong`, and ignores all other data. This only happens when Steam is running; the DLL does not start Steam.
- Only when the player starts or joins a session from the window does it run one (in this test build the window's Host button answers that hosting over Steam is not finished). A session talks to the other players (see "Network" below), and exchanges text files with the Lua mod in the game. In a world sync the host's game saves a file named `feverlink3-resync-<number>.sav` in the game's save folder; the DLL sends it to the other players, and a joining player's copy is written to the same folder. The receiver checks the SHA-256 of the whole file before writing it.

### 4. What it does not do

These are statements about the code in this repository; you can search for the calls yourself.

- It does not open other processes, write into their memory, create remote threads or install Windows hooks (the source contains none of `OpenProcess`, `WriteProcessMemory`, `CreateRemoteThread`, `SetWindowsHookEx`).
- It does not make itself start with Windows: no registry writes at all, no startup entries, no scheduled tasks, no services. It does not need or ask for administrator rights.
- It does not read your documents, browser data, passwords or other programs' files. The files it reads are its own text files, the game's save folder entry named above, and the Steam path in the registry.
- It does not log keys, take screenshots or record audio.
- It does not contact any server of the project, send analytics or ads, or check for updates (the update check is not written yet in this version).
- It does not download or run programs. (A joined session does load the host's savegame in your game, and a savegame can carry game scripts, as a mod can. That is why you should only join people you trust; see "Who you are trusting".)
- It does not use WinHTTP for its own traffic, so your game's web traffic and FeverLink3's session traffic are separate.

### 5. What may change

The switched-off code is meant to be turned on for players in a later version: two inline hooks in the game's own code (one counts the commands the simulation applies, one can hold a construction command back until every player's game can apply it at the same moment) and one small byte patch (22 bytes in build 40420) that makes the game's script system use a single worker thread, so that all games run scripts in the same order. When a switch is on, on a known build, the module first checks the exact bytes it expects at each place, then runs a self-test of its own detour machinery (it writes a few instructions of its own into a small block of executable memory, which it allocates for the purpose and never frees, and runs them), then writes the changes all-or-none, rolls back if one fails, keeps the DLL loaded for the rest of the process, and starts a second thread that exchanges two text files with the mod (`native_ctl.log`, `native_events.log`). Every step is written to `native.log`. Patching another program's code in memory and running generated code are exactly what security products watch for, so switching this on for players will be announced in the changelog and described on this page before it ships. `docs/NATIVE_LINK.md` is the design.

### Known gaps

- `feverlink3.exe` carries an icon, version information and a manifest (no administrator rights asked); the DLL does not yet have a version-information resource, so the Details tab in its Properties is empty. That makes it look less trustworthy than it is.
- A bug in FeverLink3's code running inside the game can crash the game, because the build aborts instead of unwinding on an internal error (open point in `docs/ARCHITECTURE.md` section 12).
- If a game update makes the game import one more WinHTTP function than the 14 listed above, the game may fail to start while this DLL is in place. Removing the DLL fixes that. The address tool checks the import list of every game build we add to the module.
- Not tested yet: Windows 11 with Smart App Control switched on (see below), and other mods that also place a `winhttp.dll` in the game folder (the installer refuses to overwrite another mod's file).

## The installer program, `feverlink3.exe`

It is a separate program that you start yourself (a double-click shows a numbered menu; `install`, `uninstall` and `doctor` also work on the command line). It checks your PC (the same Steam registry values and folders as above), writes or removes the files in the table below, and on request starts `steam.exe` from your Steam folder to launch the game, opens `%LOCALAPPDATA%\FeverLink3` in Explorer, and copies a diagnostics text to the clipboard through `clip.exe` (the text replaces your Windows user name with `<user>` and leaves join codes out; it can still contain folder names). It starts those programs by their full paths, makes no network connection, and asks for no administrator rights. The development commands `host` and `join` are described in `launcher/README.md`.

## Files FeverLink3 creates or changes

| Where | What | Who writes it | Removed by `feverlink3 uninstall` |
|---|---|---|---|
| `<game folder>\winhttp.dll` | the DLL | install | yes, only when it is ours (it carries the marker `feverlink3_native`) |
| `<game folder>\steam_appid.txt` | one line, `3493540` (the game's Steam app id), so that the game program can also be started outside Steam | install, only if the file is absent | yes, only when it holds exactly `3493540` |
| `<game folder>\feverlink3_path.txt` | the path of `feverlink3.exe`; read only by `feverlink3 doctor` and the installer's menu (to find out which version installed), never by the DLL | install | yes |
| `<Steam>\userdata\<id>\3493540\local\mods\feverlink3_1\` | the mod (Lua game scripts and the window) | install, one copy for each Steam account on the PC | yes |
| `%LOCALAPPDATA%\FeverLink3\native.log` | the DLL's text log | the DLL | no, delete the folder by hand |
| `%LOCALAPPDATA%\FeverLink3\live\` | session files: `ctl_cmd.log`, `ctl_status.txt`, `identity.txt`, `to_game.log`, `from_game.log`, `mod.log`, `hud_state.txt`, `ui_out.log`, `resync_cursor.txt`; emptied at every game start | the DLL and the mod | no, delete the folder by hand |
| `<Steam>\userdata\<id>\3493540\local\save\feverlink3-resync-<number>.sav` | the host's world while it is synced to the players. These are ordinary game saves and can be large | the game and the DLL | no, delete them when you do not need them |
| `%TEMP%\feverlink3\` | session files of the command-line development launcher (`host` / `join`) | only if you run those commands | no |

`<game folder>` is `<Steam library>\steamapps\common\Transport Fever 3\`. `feverlink3 doctor` prints where it is on your PC and changes nothing.

## Network

What FeverLink3 uses to connect players, in this version:

- **Steam networking (primary).** The DLL asks the Steam library that the game already runs for a peer-to-peer connection to the other player's Steam ID. Steam finds the route between the two PCs and may relay the traffic through its own servers. The sockets belong to Steam's library and use Steam's ports; FeverLink3 does not open a listening port of its own for this. Nothing about it needs to be configured by the player.
- **Status, plainly:** inside one running game, the game opened a Steam peer-to-peer connection to its own Steam ID and 6 pings were answered (2026-10-10). A connection between two different Steam accounts, and a whole session carried over it, has not been tested yet; the first is what this test build is for.
- **No server of the project is involved.** The path between two players is Steam's. If that ever changes, this page will say what is added and what it can see.
- **The host does not open an inbound TCP port in normal use.** The command-line development launcher (`feverlink3.exe host`, not used by the in-game window) is the only part that listens on a TCP port (default 28090, or the port you give it), and only if you type that command yourself.
- **Nothing else leaves the PC.** No analytics, no advertising, no update check, no account, no web requests of its own.

What the other players receive: the display name you chose (it starts as your Windows user name, so change it if you prefer), your game actions, world checksums, chat lines, and, if you host, the world itself. A player who tests the connection by Steam ID shares that Steam ID with the person they test with.

How the session traffic is protected: every frame is encrypted and authenticated by FeverLink3 itself (AES-256-GCM, a key derived with HKDF from the secret inside the join code, a replay window per sender), independent of the path it travels on. The host decides who may send what. Anyone who has the join code can join, so treat the code like a password for that session. A join code holds the session secret, the time it was made and how to reach the host (for sessions over Steam, the host's Steam ID; for the developer transport, the host's local-network addresses). The details are in `docs/PROTOCOL.md` and section 3 of `docs/ARCHITECTURE.md`.

### Check it yourself

With the game running, in PowerShell:

```powershell
$p = (Get-Process TransportFever3).Id
Get-NetTCPConnection -OwningProcess $p -State Listen      # listening TCP sockets of the game: expect none from FeverLink3
Get-NetUDPEndpoint   -OwningProcess $p                    # UDP sockets of the game (these may include Steam's networking sockets)
Get-NetTCPConnection -OwningProcess $p -State Established # outgoing TCP connections (the game and Steam have their own)
```

(An error saying that no matching connection was found also means there is none.) For a closer look, Sysinternals Process Monitor with the filter "Process Name is TransportFever3.exe" and "Path contains FeverLink3" lists every file the DLL and the mod touch, and Wireshark or Resource Monitor shows the traffic. If you see a listening TCP socket you do not expect, or a file access outside the places in the table above, please tell us.

## Verify what you downloaded

Each release has these files: `FeverLink3-<version>.zip`, `FeverLink3-<version>.zip.sha256`, `manifest.json` (every file in the zip with its size and SHA-256, and the zip's SHA-256), and, when the release is signed, `manifest.json.sig`. The release notes repeat the zip's SHA-256. Releases are built by GitHub Actions from a tag; nothing is built on a developer's PC (`docs/RELEASING.md`).

In PowerShell, in the folder with the downloaded files:

```powershell
(Get-FileHash .\FeverLink3-0.2.0-test1.zip -Algorithm SHA256).Hash.ToLower()
Get-Content .\FeverLink3-0.2.0-test1.zip.sha256

# the DLL you installed against the manifest (feverlink3 doctor prints the game folder)
$m = Get-Content .\manifest.json -Raw | ConvertFrom-Json
($m.files | Where-Object path -eq 'winhttp.dll').sha256
(Get-FileHash 'C:\Program Files (x86)\Steam\steamapps\common\Transport Fever 3\winhttp.dll' -Algorithm SHA256).Hash.ToLower()
```

The values in each pair must be identical, and equal to the ones in the release notes. If they differ, do not use the file. Use the path of your own game folder in the last command.

What this does and does not prove: the zip is assembled reproducibly (the same inputs give the same bytes), so the zip hash proves that you have the file the release job produced. We have not verified that two independent compilations of the DLL give identical bytes, so the way to check the code itself is to read it and build it yourself (`cargo build --release -p feverlink3_native`, see the README) and compare behaviour, not to expect the same hash. The manifest signature (Ed25519) is meant to be checked by the launcher's updater (whose download step is not written yet); a check by hand needs an Ed25519 tool and the project's release public key. As of 2026-10-10 the key in the repository is a placeholder, and the first release's notes will say whether its manifest is signed. The programs themselves are not Authenticode-signed (`docs/SIGNING.md` has the plan).

Get the file only from the project's release page. A file called `winhttp.dll` from anywhere else is not ours.

## If your antivirus flags it

As a player:

1. Do not turn your antivirus off. Compare the file's hash with the release (above).
2. If the hash matches and you trust the project: restore the file (Windows Security, Virus and threat protection, Protection history, pick the item, Actions, Restore or Allow) and, if it comes back, add an exclusion for that one file instead of a whole folder. If you do not trust it, run `feverlink3 uninstall`.
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

- **SmartScreen** ("Windows protected your PC") appears the first time you run `feverlink3.exe` from a browser download, because the program is not code-signed and is new. Click More info, then Run anyway. It should not appear for the DLL, because the installer writes it as a new file in the game folder, which carries no "downloaded from the internet" mark (not yet tested on a clean PC).
- **Smart App Control** (Windows 11) in its "On" setting blocks programs and DLLs that Microsoft does not recognise unless they are signed with a trusted certificate (https://learn.microsoft.com/en-us/windows/apps/develop/smart-app-control/overview). We have not tested FeverLink3 with it on. If it is on and the game will not start after the install, run `feverlink3 uninstall`. Code signing is the real fix and is planned (`docs/SIGNING.md`); it removes the first-run SmartScreen warning only gradually, because reputation also needs downloads.

## Who you are trusting

- **This project's code and release.** You can read it, build it for noncommercial use and compare the release against the checks above.
- **The host of a session.** When you join, your game loads the host's savegame. A Transport Fever savegame can carry game scripts, as a mod can, so joining a session means trusting the host as you would trust a mod author. Only join people you know.
- **Everyone who has the join code.** They can join the session and act in the world, within the permissions the host sets. Do not post a code in public, and stop hosting when you are done.
- **Steam,** for the route between the games.

## Remove it

1. Close the game. Run `feverlink3 uninstall` (add `--dry-run` first to see the plan). It removes the mod, our `winhttp.dll`, `feverlink3_path.txt` and, if it holds our app id, `steam_appid.txt`. It never touches a `winhttp.dll` that is not ours.
2. Optional: delete `%LOCALAPPDATA%\FeverLink3\` (logs and session files), `%TEMP%\feverlink3\`, and the files named `feverlink3-resync-*.sav` in your save folder.
3. By hand, if you no longer have `feverlink3.exe`: delete `winhttp.dll` from the game folder (check that it is ours first, with the hash above) and the folder `userdata\<id>\3493540\local\mods\feverlink3_1`.

The game keeps no other trace of FeverLink3. The mod declares that removing it has no effect on a save (`severityRemove: None` in its `mod.json`); the game may still list it as missing when you load a save that was made with it.

## Reporting a security problem

If you think you found a vulnerability, or a build that does more than this page says, tell the maintainer directly on Discord (`azalshehri`) before posting details in public, or open an issue at https://github.com/AZalshehri7/FeverLink3/issues without sensitive detail and ask for a private channel. The project's threat model and policy are in `docs/SECURITY.md`.
