# mod.io / in-game Mod Hub listing (0.2.11)

Written 2026-10-22. Transport Fever 3's official mod browser (the in-game Mod Hub) is hosted by mod.io (https://mod.io/g/transportfever3). **Nothing has been uploaded.** Read the "Decision" and "Risks" sections before uploading anything.

## Sources (read 2026-10-22)

- TF3 wiki, Publish a Mod: https://wiki.transportfever3.com/doku.php?id=modding:general:publishing
- TF3 wiki, Guidelines & Requirements: https://wiki.transportfever3.com/doku.php?id=modding:general:guidelines
- TF3 wiki, Mod Definition: https://wiki.transportfever3.com/doku.php?id=modding:general:moddefinition
- TF3 wiki, Mod Tags: https://wiki.transportfever3.com/doku.php?id=modding:general:modtags
- TF3 wiki, Monetization: https://wiki.transportfever3.com/doku.php?id=modding:monetization
- mod.io Terms of Use: https://mod.io/legal/terms (the Acceptable Use Policy at https://mod.io/aup is part of them; that page renders only with JavaScript and its text could not be read here)
- mod.io automated scanning: https://docs.mod.io/moderation/automated-scanning ; API reference (field limits): https://api.mod.io/v1/swagger
- The TF3 game page and mod pages on mod.io render only with JavaScript and the API needs a key, so **no public TF3 mod's file structure could be inspected**. The layout below comes from the wiki's Mod Definition page.

## What the rules say

**TF3 (Urban Games):**
- Upload path: copy the mod folder into the staging area (`Steam\userdata\<your Steam ID>\3493540\local\staging_area\`), Main menu > **Mod Hub** > **My Mods** > select the mod > set name, tags, cover and gallery images, visibility, changelog > **Upload**. The game validates the mod first; if it also passes the stricter console checks, a console version is made and **automatically approved for PS5 and Xbox Series**.
- Afterwards the entry can be edited on mod.io (profile, summary, description, logo, tags, media, files, dependencies). **At every update from the game, the name and description on mod.io are overwritten with `modinfo.json` / `_metadata/description.html`.** `_metadata/mod.io_fileid.txt` is created at the first upload and is needed to update the same entry: keep it.
- Mandatory: own rights to the content; "non-commercial and accessible in full for free to all users"; comply with mod.io's terms; follow the modding manual.
- Technical requirements: "Batch files, shell scripts or other executable files are not allowed"; text files UTF-8 without BOM; lowercase file and folder names, "avoid whitespaces and special characters"; only essential files; at most 500 MB unpacked; **"Only use the official Transport Fever 3 scripting API"**; `severityRemove` set in `mod.json`.
- Name at most 32 characters, Title Case. Summary (`modinfo.json`) at most 100 characters, one line. Description mandatory in English; supported formatting: paragraph, headers 1-3, bold, lists, horizontal lines (no tables, no embedded images or videos). A console player cannot open links, so the description must hold every relevant instruction.
- Cover: 1920x1080, 16:9, at most 8 MB, show the main subject; no added text or logos (script mods may use a GUI screenshot or simple icons). `_metadata/0.png` is the grid image, `1.png`, `2.png`... the detail view.
- Tags: from the official list, at most about three. For this mod: **Script Mod** (already in `modinfo.json`). "Misc" is the only other plausible one; Script Mod alone is right.

**mod.io:**
- The Terms contain no clause on DLLs or native code that could be read here; the Acceptable Use Policy text could not be retrieved.
- mod.io's schema validation flags archives, `.dll`, `.exe` and similar files inside a mod; each game decides whether to block the upload or send it to the game's admins. A malware scan disables files it flags.
- Web upload fields: summary 20-250 characters, no links; description is HTML (up to 150,000 characters); logo jpg/png, at least 512x288, 16:9 recommended, at most 8 MB (thumbnails 320x180, 640x360, 1280x720); gallery images jpg/png only (no GIF; YouTube links possible); modfile is a zip of "the base folder of your mod", at most 500 MB.

## Decision: the Mod Hub package is only the mod folder

No documented way exists to ship `winhttp.dll` through the Mod Hub: a Hub mod is a folder under the mods directory, nothing in it can place a file next to `TransportFever3.exe`, executables are not allowed in mods, and mod.io flags `.dll` files. So:

- **Mod Hub / mod.io:** only `feverlink3/` (the Lua mod), with a description that says in its first line that it is Windows-only and needs `winhttp.dll` from GitHub (or Nexus), with the steps.
- **winhttp.dll:** from the GitHub release zip (and Nexus once listed). Players copy only `winhttp.dll` if they use the Hub version.

## Risks that could get the Hub upload refused or removed

1. **"Only the official scripting API."** The mod reads and writes files through `io`/`os` (`feverlink3_link.lua`, `feverlink3_log.lua`) to talk to the DLL. The owner has accepted this (CLAUDE.md, 2026-10-18), but it is outside the wiki's rule. The automatic validation may not notice; a human review or a report would.
2. **Consoles.** If the mod passes the console checks it is auto-approved for PS5 and Xbox, where it can never work. Bad ratings and reports follow. After the upload, open the entry on mod.io > **Files** and check the platform status; if a platform can be withdrawn there, withdraw PS5 and Xbox. Whether that is possible is **unconfirmed**. The description's first line says "Windows PC only".
3. **"Accessible in full for free"** and a mod that does nothing without a second download from another site. It is free, but a curator could see it as incomplete.
4. **Stale `modinfo.json`.** The released `_metadata/modinfo.json` says the DLL is "installed by FeverLink3.exe", which no longer exists. The game shows this text, and an in-game upload copies it to mod.io. **Fix before uploading:** ship `_metadata/description.html` (it replaces the description; the command below adds it to the package), and correct the `modinfo.json` description in the repository for the next release.
5. **File names with `@`** (`feverlink3_bar_green@2x.tga` and others): the wiki says to avoid special characters. If validation refuses them, that is the reason (unconfirmed).
6. **Staging area shadows the game-folder copy.** The game prefers Staging Area > manual installation directory > subscribed mods for the same `modId`. After uploading, move `feverlink3` out of the staging area (keep `_metadata/mod.io_fileid.txt` for the next update), or your own tests will run the staged copy instead of the one in the game folder.
7. **Two copies and versions.** A player who copies the whole release zip has `mods\release\feverlink3` in the game folder, which the game prefers over the Hub copy; the DLL's auto-update keeps that copy current. A Hub-only player depends on the Hub copy matching the DLL's version: upload every release to the Hub on the same day. No mod-to-DLL version check was found in the Lua code (unverified).

## Listing texts

**Name** (22 characters, also in `modinfo.json`):
```
FeverLink3 Multiplayer
```

**Summary** for `modinfo.json` (at most 100 characters; 84):
```
Co-op multiplayer. Windows PC only, needs winhttp.dll from GitHub (see description).
```
The current `modinfo.json` summary ("Co-op multiplayer (needs the FeverLink3 native module)", 57) is acceptable too. mod.io's own summary field allows 250 characters but is overwritten from `modinfo.json` at each in-game update.

**Description:** `docs/nexus/modio-description.html` (HTML with only paragraph, h2, bold, lists, hr, as the wiki recommends). It goes into the package as `_metadata/description.html`, which the game shows in the Mod Hub and uploads to mod.io. If you upload on the website instead, paste the same HTML into the description field.

**Tags:** `Script Mod`.

**Version / changelog:** version `0.2.11`; `mod.json` revision: raise it at every Hub update (currently `1`; the release workflow does not change it). Changelog:
```
0.2.11: the junction lane tool works again in a session.
0.2.10: the games autosave together; one shared company (a company for each player is upcoming); bus stops on other mods' bay roads shared; street splits no longer shown as failed; tool notices once a session; version in the window title.
0.2.9: other mods' build tools and windows held for every game; big builds (3-track stations) sent in parts; mod track constructions sent exactly; earlier update check when a newer host refuses you.
0.2.8: trees, plants, rocks and the vegetation eraser shared in a session (new).
0.2.7: terrain heights and ground paint shared; builds stand sooner; finds its places in a new game build by itself; players with different game languages no longer resync on new industries; trees, plants and rocks grayed out in a session.
0.2.6: game build 40425 supported.
0.2.5: roundabouts and streets with bus stops build in a session; repairs no longer dropped; text margins; Save and quit after an update.
0.2.4: game build 40423 supported; other mods' own changes stay in sync.
0.2.3: no false "could not be built" notice on roads that join a road; station configure right after a sync works; every game keeps its world details at a sync.
```

**Images:**
- Cover / logo: `docs/nexus/media/cover-1920x1080.png` (1920x1080, 16:9, 0.4 MB, the real in-game bar icon and window, no added text). The current `_metadata/0.png` (1920x1080, the FeverLink3 word mark) also meets the size rule but has text, which the wiki asks to avoid. To use the new cover in the Hub, copy it as `_metadata/0.png` in the staged copy (the command below does this).
- Gallery: `docs/nexus/media/demo-build-still-1920x1080.png` (the build demo's last frame with its timings) and `docs/media/ui-overview.png`. mod.io takes no GIF.

## The package (exact layout)

```
feverlink3/
  mod.json
  _content.json
  _metadata/
    modinfo.json
    0.png                 (cover, replaced by cover-1920x1080.png)
    description.html      (added: modio-description.html)
  content/
    *.lua, *.tga          (unchanged from the release)
```

Nothing else: no `winhttp.dll`, no README/LICENSE (the Hub asks for essential files only; the licence line is in the description).

## Build it from the release zip (PowerShell)

Download `FeverLink3-0.2.11.zip` from the GitHub release into `Downloads` first. Then run, in Windows PowerShell, from the repository folder (`Documents\AI\FeverLink3`):

```powershell
$zip = "$env:USERPROFILE\Downloads\FeverLink3-0.2.11.zip"; $work = Join-Path $env:TEMP 'fl3-modio'; if (Test-Path $work) { Remove-Item $work -Recurse -Force }; Expand-Archive -LiteralPath $zip -DestinationPath "$work\release"; $mod = Get-ChildItem "$work\release" -Recurse -Directory -Filter feverlink3 | Where-Object { Test-Path (Join-Path $_.FullName 'mod.json') } | Select-Object -First 1; if (-not $mod) { throw 'mod folder not found in the zip' }; $pkg = (New-Item -ItemType Directory "$work\pkg").FullName; Copy-Item $mod.FullName "$pkg\feverlink3" -Recurse; Copy-Item 'docs\nexus\modio-description.html' "$pkg\feverlink3\_metadata\description.html"; Copy-Item 'docs\nexus\media\cover-1920x1080.png' "$pkg\feverlink3\_metadata\0.png" -Force; Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem; $out = "$env:USERPROFILE\Desktop\feverlink3-modio-0.2.11.zip"; if (Test-Path $out) { Remove-Item $out }; $za = [System.IO.Compression.ZipFile]::Open($out, 'Create'); Get-ChildItem "$pkg\feverlink3" -Recurse -File | ForEach-Object { $rel = $_.FullName.Substring($pkg.Length + 1).Replace('\', '/'); [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($za, $_.FullName, $rel, 'Optimal'); $rel }; $za.Dispose()
```

It prints every file in the package (`mod.json`, `_content.json`, `_metadata/` with `0.png`, `description.html`, `modinfo.json`, and `content/`) and writes `feverlink3-modio-0.2.11.zip` (top folder `feverlink3/`) to the Desktop, for a website upload. The zip entries use forward slashes on purpose: Windows PowerShell 5.1's `Compress-Archive` / `CreateFromDirectory` write backslashes, which other tools may misread. (Tested on 2026-10-22 against the 0.2.0-pre7 zip, which has the same layout.) For the **in-game upload** (recommended: it runs the game's own validation), copy the folder into the staging area instead:

```powershell
$staging = Get-ChildItem "${env:ProgramFiles(x86)}\Steam\userdata\*\3493540\local\staging_area" -Directory | Select-Object -First 1; if (-not $staging) { throw 'staging_area not found: create it under Steam\userdata\<id>\3493540\local\' }; Copy-Item (Join-Path $env:TEMP 'fl3-modio\pkg\feverlink3') $staging.FullName -Recurse
```

(If Steam is not in `C:\Program Files (x86)\Steam`, change the path. The Steam ID folder is matched with `*` so it never has to be typed.)

Whether a website upload wants the `feverlink3/` top folder or its contents directly is **not confirmed** for TF3 (mod.io says "ZIP the base folder of your mod"); the in-game upload avoids the question.

## Steps for the owner, in order

1. Decide whether to accept risks 1-3 above. Optional: ask Urban Games first (Discord or support) whether a PC-only script mod that needs a separate DLL is welcome on the Hub.
2. Build the package (first command), then stage it (second command).
3. Start the game > **Mod Hub** > **My Mods** > FeverLink3 Multiplayer. Name as above, tag **Script Mod**, cover `cover-1920x1080.png`, gallery images, visibility **Hidden** first if the dialog offers it, changelog. **Upload**. Read the validation result.
4. On mod.io, check the entry: description, platform status in **Files** (risk 2), summary.
5. Copy `_metadata/mod.io_fileid.txt` from the staged folder somewhere safe, then remove `feverlink3` from the staging area (risk 6).
6. Make it public only when everything reads right. Then add the mod.io link to the README, site and Reddit draft (owner approval before any push).
7. For the next release: fix the `modinfo.json` description and add `_metadata/description.html` in the repository, so the GitHub zip and the Hub copy are the same.
