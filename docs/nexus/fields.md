# Nexus Mods page: the form fields

Paste-ready text for each field of the mod page, in the order of the upload form. The long description is in `description.bbcode.txt` (paste into the Description box) and `description.md` (same text in Markdown, for GitHub or another host). Limits that Nexus does not publish are marked "check the form"; the texts are kept short enough to fit conservative limits.

Everything here is written for a Steam connection-test build (never released) and the facts of 2026-10-10. **Out of date: rewrite it for 0.2.0 before publishing.** `README.md` in this folder lists what to re-check against the tagged code before publishing.

## Name

```
FeverLink3 Multiplayer
```

(22 characters. This is also the name the mod shows in the game's mod list.)

## Summary (short description)

```
Early test of co-op multiplayer for Transport Fever 3, free for noncommercial use. This first build only tests the Steam connection between two players; hosting and joining come next.
```

(183 characters. Check the form for the limit.)

## Version

```
0.2.0
```

## Category, tags, flags

- Category: the closest of "Gameplay", "Utilities" or "Miscellaneous" in the game's category list (check the list; it was not readable when this was written).
- Tags to look for: multiplayer, co-op, script. Do not add tags that do not describe the mod.
- Adult content: no.
- Generative AI tag: the file submission guidelines say that mods primarily made with AI should carry the "AI-Generated Content" tag and that undisclosed AI use may lead to moderation. The code and documentation of FeverLink3 were largely written with Claude under the maintainer's direction (the commit messages say so). The owner decides the tag; the description already discloses it. See `README.md`, step "AI disclosure".
- Language: English.

## Main file

- File name on the upload: `FeverLink3-0.2.0.zip` (the zip from the GitHub release, unchanged, so that its SHA-256 matches the release notes). It holds the testing installer, the native module and the mod.
- Category: Main files.
- File description (keep under 255 characters; the limit is reported by users, not published):

```
FeverLink3 0.2.0: testing installer (feverlink3.exe), winhttp.dll and the mod. Windows, Steam, game build 40420. Experimental, unsigned. SHA-256 is in the release notes on GitHub.
```

(185 characters.)

**Owner decision, 2026-10-10:** the end state is the mod offered in the game's Mod Hub (mod.io backend) and on Nexus Mods like any other mod, with the native module delivered separately for now, and the installer kept for testing. When the packaging changes, upload two files instead of this one: the mod (the `mod/feverlink3` folder, as a zip) as the main file, and the native module (`winhttp.dll` with a short text on where it goes and the hash) as a second file, and rewrite the Installation section of the description to match. Do not announce the Mod Hub listing before the Hub has accepted the mod (`docs/WORKSHOP.md`).

## Requirements tab

Required:

```
Transport Fever 3 (Steam), game build 40420
Steam client running and logged in
Windows 10 or 11, 64-bit
```

Conflicts, in the notes: `MPFever and any other mod that installs its own winhttp.dll in the game folder.`

## Permissions and credits

Nexus asks for these and has no free-text licence field, so the licence goes into the permission notes and into the description ("Credits and permissions"). The licence is the PolyForm Noncommercial License 1.0.0: free for noncommercial use, commercial use only with a separate licence from the author. The answers below are the strictest reading, so that the form never promises more than the licence gives.

| Form item | Answer |
|---|---|
| Other users' assets | The mod contains no files or assets from other authors. It reuses findings (per-build game addresses, API names) documented by MPFever, MIT, credited in the files that use them. |
| Upload permission (other sites) | Allowed for noncommercial purposes, with the `LICENSE` file or its web address, the "Required Notice" line and the credits kept. |
| Modification permission | Allowed for noncommercial purposes under the same licence. |
| Conversion permission | Allowed for noncommercial purposes under the same licence. |
| Asset use permission | Allowed for noncommercial purposes under the same licence. |
| Use in mods with donations / sold mods | **Not allowed** without a separate commercial licence from the author. |
| Donation points | Off. |

Credits and notes box:

```
Author: Abdulaziz Alshehri (Discord azalshehri). Licence: PolyForm Noncommercial License 1.0.0 (https://polyformproject.org/licenses/noncommercial/1.0.0/): free for noncommercial use, commercial use needs a separate licence from the author. Source: https://github.com/AZalshehri7/FeverLink3
Credits: TpF2 Multiplayer by silver2127 (Transport Fever 2, MIT, https://silver2127.github.io/tpf2-multiplayer/) - replication modes, "no entity ids on the wire", join-code and sealed-transport trust model, host-included resync round. MPFever by dkdknight / arthur (Transport Fever 3, MIT, https://github.com/dkdknight/MPFever) - proxy-DLL approach, single-worker script pool fix, deterministic math.random, per-build game addresses and API names. Both licences are in THIRD_PARTY_NOTICES.md in the zip.
Not affiliated with Urban Games or Paradox Interactive. Much of the code and documentation was written with Claude (Anthropic) under the maintainer's direction.
```

## Changelog tab (connection-test build)

```
First public test build. Its job is the Steam connection test between two players.
- In-game window (button in the bottom bar) with a Steam connection test: your Steam ID, test the connection to another player's Steam ID, result with round-trip time and route.
- Hosting and joining over Steam are not finished in this build: the Host button says so.
- For later builds, tested with simulated games only: lockstep core (actions applied at the same step on every game, world checksums, repair by loading the host's savegame, permissions model, command schemas for everyday operations).
- Encrypted sessions (AES-256-GCM, key from the join code).
- Native module winhttp.dll: passes the game's 14 WinHTTP functions through, runs the in-game controller. Its hook code is switched off by default and has never run in the real game.
- Testing installer: feverlink3 install / uninstall / doctor, with --dry-run.
- Unsigned. Seen working in the real game (build 40420): window, Steam connection to the game's own Steam ID, save/load. Not verified: the Steam connection between two accounts, hosting and joining over Steam, building together, world transfer between two real games.
```

If more landed before the tag, add only what has been seen working, with the same wording rules as the description.

## Pinned post (known issues and how to report)

```
Known issues of the connection-test build
- Not a finished multiplayer game: this build tests the Steam connection only. Hosting and joining over Steam, building together, world transfer and the Steam connection between two accounts are not verified yet.
- Game build 40420 only. After a game update, if the game does not start, run feverlink3 uninstall and tell us your build.
- Unsigned: SmartScreen/antivirus may warn. Windows 11 Smart App Control may block the DLL (untested).
- Not compatible with MPFever or any other winhttp.dll mod.
- The installer is a testing tool (double-click menu or command line), not yet tried on a clean PC by someone else.

How to report: Discord azalshehri or https://github.com/AZalshehri7/FeverLink3/issues. Attach %LOCALAPPDATA%\FeverLink3\native.log and live\mod.log, your game build and Windows version.
```

## Images

Captions and the order are in `shot-list.md`. The first image becomes the thumbnail.

## Message to Nexus staff (before publishing)

The file submission guidelines say that executables or tools that connect to the internet are prohibited unless crucial to the mod's function, that an auto-update does not count, and that the author must contact staff or support with the reasoning and the source code. Send this through the support contact the site offers, and wait for the answer before publishing.

```
Subject: Request before publishing: multiplayer mod with a native component that uses the network (FeverLink3, Transport Fever 3)

Hello,

I would like to publish FeverLink3 Multiplayer for Transport Fever 3 on Nexus Mods. It is a cooperative multiplayer mod whose source code is public (PolyForm Noncommercial License 1.0.0), so network access is its function: the native module (a winhttp.dll that the game loads from its own folder) connects the players' games to each other through Steam's peer-to-peer networking.

Per your file submission guidelines I am contacting you first, with the reasoning and the source:
- Source code: https://github.com/AZalshehri7/FeverLink3 (folders native/ and launcher/ for the DLL). It is a fresh implementation that builds on two earlier MIT-licensed projects, TpF2 Multiplayer by silver2127 and MPFever by dkdknight (arthur), both credited in the README and on the page.
- What the DLL does and does not do, file by file, and how to verify it: https://github.com/AZalshehri7/FeverLink3/blob/main/docs/SECURITY_FOR_USERS.md
- Network use: player-to-player traffic through Steam's networking only; no server of the project is involved. No auto-update, no analytics, no telemetry, no advertising, no account, no download of code.
- The release is built by GitHub Actions from a tag; the zip, its SHA-256 and a manifest are on the GitHub release page. The files are not code-signed yet.
- The page states plainly what is verified in the real game and what is not (it is an experimental test build).
- Another experimental multiplayer mod with a native component is already listed for this game, so I assume this kind of mod is acceptable; please tell me if you need anything changed.

Please tell me whether I may publish it as described, or what you would like changed.

Thank you,
Abdulaziz Alshehri (azalshehri on Discord)
```
