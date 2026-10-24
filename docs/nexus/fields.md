# Nexus Mods page: the form fields (0.2.5)

Paste-ready text for each field, in the order of the upload form. The long description is `description.bbcode.txt`. Nexus does not publish most field limits; the texts are kept under conservative limits, and the form itself is the final word. Facts as of 2026-10-23, release https://github.com/AZalshehri7/FeverLink3/releases/tag/v0.2.5.

## Name

```
FeverLink3 Multiplayer
```

22 characters; the same name the game shows in its mod list.

## Summary (brief overview)

```
Play Transport Fever 3 together: build, run lines and grow towns in one shared world. Fully integrated into the game, no launcher needed. Join friends over Steam.
```

162 characters. Nexus does not publish the limit (not confirmed); if the form counts fewer, cut the last sentence.

## Version

```
0.2.5
```

## Category, tags, flags

- Category: the game's list was not readable from outside. Pick the first that exists: **Gameplay**, then **Utilities**, then **Miscellaneous**.
- Tags (only those that describe the mod): Multiplayer, Co-op, Gameplay, Scripts / Script mod, Windows.
- **Generative AI tag: AI Assisted** (owner decision 2026-10-22). The description says how the mod was made in "How it was made".
- Adult content: no. Language: English.

## Main file

- Upload: `FeverLink3-0.2.5.zip` exactly as downloaded from the GitHub release. Do not repack it: players compare its SHA-256 with `FeverLink3-0.2.5.zip.sha256` on the release page.
- Name on Nexus: `FeverLink3 0.2.5`
- Category: Main files. Version: 0.2.5.
- File description (under 255 characters):

```
Copy winhttp.dll and the mods folder into the game folder (next to TransportFever3.exe). Windows, Steam. Unsigned until 1.0.0; SHA-256 on the GitHub release v0.2.5. Updates itself (can be turned off).
```

(200 characters.)

## Requirements tab

Required (off-site):

```
Transport Fever 3 (Steam, Windows 10/11 64-bit), game build 40420, 40423 or 40425 - all players on the same game version
Steam client running and logged in
```

Notes / conflicts:

```
Not compatible with other mods that install their own winhttp.dll in the game folder. Mods that change the world on their own (for example automatic line namers) cause extra syncs; turn their automatic changes off while playing together.
```

## Permissions and credits

Nexus has no licence field, so the licence goes into the permissions and the credits box. The answers are the strictest reading of PolyForm Noncommercial 1.0.0.

| Form item | Answer |
|---|---|
| Other users' assets | No files or assets from other authors. Findings and designs from TpF2 Multiplayer and MPFever (both MIT) are credited. |
| Upload permission | Allowed for noncommercial purposes, with LICENSE (or its address), the Required Notice line and the credits kept |
| Modification permission | Allowed for noncommercial purposes under the same licence |
| Conversion permission | Allowed for noncommercial purposes under the same licence |
| Asset use permission | Allowed for noncommercial purposes under the same licence |
| Asset use in mods/files being sold | Not allowed |
| Asset use in mods/files that earn donation points | Not allowed without the author's permission |
| Donation points | Off |

Credits and notes box:

```
Author: Abdulaziz Alshehri (Discord azalshehri). Source: https://github.com/AZalshehri7/FeverLink3
Licence: PolyForm Noncommercial License 1.0.0 (https://polyformproject.org/licenses/noncommercial/1.0.0/) - source available, free for noncommercial use; commercial use needs a separate licence from the author.
Required Notice: Copyright (c) 2026 Abdulaziz Alshehri (https://github.com/AZalshehri7)
Credits: FeverLink3 builds on what earlier Transport Fever multiplayer projects found and documented: TpF2 Multiplayer by silver2127 and MPFever by dkdknight (both MIT; licences in THIRD_PARTY_NOTICES.md in the zip).
Most of the code and documentation was written with Claude (Anthropic) under the author's direction.
Unofficial: not affiliated with or endorsed by Urban Games or Paradox Interactive.
```

## Changelog tab

```
0.2.5
- Roundabouts and streets with a bus stop on them now build in a session.
- A repair needed right after a sync is no longer dropped; it starts by itself.
- A change made while paused no longer forces a sync.
- Text in the window keeps a margin from the edges; Save and quit button after an update.
- A friend who joins through a Steam invite has their mods compared.

0.2.4
- Supports Transport Fever 3 build 40425 (the game update of 2026-10-22); 40420 and 40423 stay supported; on a game build FeverLink3 does not know, the window says so.
- Other mods that change the world on their own (Auto Line Namer, Auto Color Lines, Auto Assign Terminals) stay in sync: the host's copy makes the change, every game applies it at the same moment.
- The warning about such a mod names it.

0.2.3
- A road that joins or splits an existing road no longer says "Your build could not be built" when it was built on every game.
- Configuring a station right after a sync, before using another build tool, is copied to every game instead of refused.
- Every game keeps its world details when a sync starts, so differences can be traced in the logs.
- Report a problem: the host's own report is no longer refused as busy when a player asks at the same moment.

0.2.2
- Faster builds: a build stands on every game 1.4 s after the click at game speed x1 and 0.8 s at x4 (two games on one PC). At x4 the game really runs at four times the speed now.
- Update messages in the pop-up and the window are shorter and no longer cut off; long texts break into several lines.
- The empty feverlink3_update folder is removed after an update.

0.2.1
- Fixed: the update check could make the game show "Internal error" at start; downloads now run inside the game.
- Report a problem opens its folder and the issue page through Windows' shell.

0.2.0 (first public beta)
- New in-game interface (bar icon with the session colour, pop-up, window with tabs), plain messages, automatic updates, Report a problem, a faster session start, and many sync fixes.
```

## Pinned post (known issues and how to report)

```
Known issues in 0.2.5
- Terrain editing, ground paint and trees are not shared; the terrain tools are off during a session.
- A mod change FeverLink3 cannot share yet causes extra syncs (FeverLink3 names the mod in a warning).
- After a Transport Fever 3 update, FeverLink3 needs an update too (the window says so).
- About one automatic repair (about 15 s) every 40 minutes in a long session over the internet.
- winhttp.dll is not code-signed until 1.0.0; antivirus software may warn. Compare the SHA-256 with the GitHub release.
- Not compatible with other mods that use their own winhttp.dll.

How to report: press "Report a problem" in the FeverLink3 window (it makes one zip with every player's logs, personal details removed) and attach it to https://github.com/AZalshehri7/FeverLink3/issues or the Bugs tab here. Say what you were doing. Discord: azalshehri.
```

## Images

Order and files are in `README.md` ("Creating the Nexus page", step 5).
