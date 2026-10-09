# Steam Workshop and the Mod Hub: publishing plan

Written on 2026-10-10 from public pages read that day (the list, with addresses, is at the end). Some of the wiki pages are marked "old revision" by the wiki, so check the current text before acting on any rule quoted here. Anything that could not be confirmed is marked as such.

## The short answer

1. **Transport Fever 3 has no Steam Workshop.** The developers' support page says: "the Steam Workshop would undermine this single-destination approach to modding so we are not including it in the game." An earlier developer blog (episode 5) had called it "not yet confirmed"; the support page is the later and firmer statement. The old roadmap item "Mod Hub and, if TF3 supports it, the Steam Workshop" can therefore not be done as written.
2. **The official channel is the in-game Mod Hub, built on mod.io.** Mods are uploaded from the game (from the `staging_area` folder of the user data) or on mod.io, pass an automatic validation for PC and console, and are shared for every platform from one library. A separate Curated Mods Program exists; its entry conditions (10 published mods, 100,000 downloads in total, a record of well-rated mods) are not something this project meets.
3. **A mod that ships a native DLL for the game folder does not fit that channel.** A Mod Hub mod is a folder under the user mods folder (`mods/<author>_<modname>/` with `mod.json`, `_metadata/` and `content/`); nothing in that structure puts a file into the game folder, and the published rules are about game resources and scripts. The wiki's technical requirements add: "Only the official Transport Fever 3 scripting API may be used" and "Executable files such as batch files and shell scripts are not allowed". mod.io's own documentation says its schema check flags archive, DLL and executable types and lets each game decide whether to block the upload or send it to the game's admin. None of these pages says "DLLs are rejected" in so many words, so the exact outcome is unconfirmed, but every sign points the same way.
4. **The practical plan: ship the whole thing outside the Hub** (GitHub Releases, then Nexus Mods, and the German transportfever.net file base that the official wiki itself names as an example of other hosts), and **ask Urban Games** whether a Hub listing of a Lua-only part is welcome at all. The questions are drafted below.

## What FeverLink3 consists of

| Part | Where it lives | Could it go through the Mod Hub? |
|---|---|---|
| `mod/feverlink3_1` (Lua game scripts and the in-game window) | the user mods folder | Possibly as a mod, but see the Lua blockers below |
| `winhttp.dll` (native module and the in-game session controller) | the game folder | No |
| `feverlink3.exe` (installer, `doctor`, development commands) | anywhere | No, it is an external program |

## The likely blockers, honestly

**B1. The native DLL.** The Hub cannot deliver it, the rules allow only the official scripting API and no executables, mod.io flags DLL/EXE types, and consoles cannot load native code from a mod. Without the DLL there is no session controller, no Steam networking and no connection to other players: Lua alone has no network access.

**B2. The Lua part is not "official API only".** The game removes the `io` global from its scripts. The mod's window script gets it back through `package.loaded.io` (a finding from MPFever) and uses it, and `os.getenv`, to read and write plain text files in `%LOCALAPPDATA%\FeverLink3\live\`, which is how the mod talks to the native module. That is outside what the wiki calls the official API. Automatic validation may or may not notice it (unknown), a human reviewer would, and a future game update could close the loophole. There is no known official way for a script to exchange data with an outside program.

**B3. Useless on its own.** Installed without the DLL, the mod's window never leaves "Starting the multiplayer module...". A Hub listing that cannot work without a separate manual download would collect bad ratings, clashes with the Hub's idea of one library that works the same on every platform, and sits uneasily with the rule that "mods must be non-commercial and accessible in full for free to all users" (it is free, but not usable in full from the Hub alone).

**B4. Consoles.** Hub mods are offered to PlayStation and Xbox as well (the Hub even generates a console-optimised version on upload). We found no "PC only" flag in the pages we could read.

**B5. Multiplayer is not on the developers' list.** The support page says the priority is the best possible solo experience and that co-operative or competitive modes would be premature now. That is a deferral, not a ban, but it means there is no sanctioned interface for what we do and the studio could object. We have not reviewed the game's or the publisher's licence terms for modifying the game or for using Steam networking through the game's own Steam library with the game's app id from a third-party DLL. **These are open questions to ask, not a legal opinion.**

**B6. Presentation rules** (these we can meet): name at most 32 characters, summary at most 100 characters, description in English, cover image 1920x1080 (16:9, at most 8 MB, no text or logos), text files UTF-8 without BOM, lowercase file and folder names, at most 500 MB unpacked, `severityRemove` set in `mod.json`, tags from the official list. The mod's current `modinfo.json` is within the length limits; whether its tag "Script Mod" is on the official list is unchecked.

## Options

**A. Stay outside the Hub (recommended now).** The whole package goes to GitHub Releases, Nexus Mods (the game has a page there, `nexusmods.com/transportfever3`, and an experimental Windows-only co-op mod is already listed) and optionally transportfever.net. The official publishing page says uploads to other hosts are manual, follow each host's terms, and that the download address should go into `modinfo.json` so players can find the source. Nexus's rules: executables or tools that connect to the internet are prohibited unless crucial to the mod's function, an auto-update does not count, and the author must contact staff with the reasoning and the source code. FeverLink3 needs its network access to function, and its code is open; the contact is a step in `docs/nexus/README.md`.

**B. Split into a Lua-only Hub item plus a native part from GitHub or Nexus.** Not recommended. The Lua part cannot talk to anything without the unofficial file access (B2), so a Hub-compliant Lua-only mod cannot do the job, and a non-compliant one invites removal.

**C. A "guide" item on the Hub.** A small mod whose purpose is its description (and perhaps a window text) telling players where to get FeverLink3 and how to install it. It would be within the technical rules, but a mod with no gameplay content is likely to be treated as spam or low quality, links in descriptions may be restricted (the `url` field of `modinfo.json` is not shown on consoles), and it would still need the answers from Urban Games first. Only worth trying if they say yes.

**D. Ask Urban Games for a sanctioned path.** Cheap and decisive: a listing as "PC only, needs a separate component", an official way to exchange data with an external program, or a statement that none will come. The draft is below. The wiki points to the contact form on https://www.transportfever3.com/support/.

**E. Remove the native module.** Moving the session controller back into the external `feverlink3.exe` (the original design) removes the DLL but not the file access of the Lua part (B2), and loses the planned hold-and-release hooks. Not a way out.

**Recommendation:** A now, D in parallel, C only on a positive answer, never B or E.

## Safe things to try before asking

1. Check the tag list and the limits above against the current wiki text (the pages we read are marked "old revision").
2. The upload dialog in the Mod Hub has a visibility setting and the validation runs before upload; whether a hidden upload shows the validation result without making the mod public is unconfirmed. Do not try it without the owner's decision, because even a hidden entry exists on mod.io until deleted.
3. Read the game's and the publisher's licence text (Steam shows the game's EULA on its store page) for clauses on modification and on network services, and note the exact wording in the message to Urban Games.

## Draft message to Urban Games

> Subject: Question about a PC-only multiplayer mod with a native component (FeverLink3)
>
> Hello Urban Games team, I am the author of FeverLink3, an open-source (MIT) cooperative multiplayer mod for Transport Fever 3 (https://github.com/AZalshehri7/FeverLink3). It has a normal Lua mod and a small Windows DLL that the game loads from its own folder as `winhttp.dll` (it passes the game's 14 WinHTTP imports through unchanged; the page https://github.com/AZalshehri7/FeverLink3/blob/main/docs/SECURITY_FOR_USERS.md explains it in plain words). We would like to do this the way you prefer and have a few questions:
> 1. May a mod that only works with a separate, manually installed PC component be listed in the Mod Hub if the description says so clearly, or should it stay outside the Hub?
> 2. Can a mod be marked PC-only so it is not offered on consoles?
> 3. Our window script reads and writes plain text files in `%LOCALAPPDATA%` through `package.loaded.io`, because the `io` global is not available to UI scripts. Is that acceptable, or will it be blocked? Is there an official way for a script to exchange data with an external program?
> 4. We connect the players with Steam's peer-to-peer networking through the game's already loaded `steam_api64.dll`, using the game's app id. Do you or Paradox have a position on that?
> 5. Do you have objections to a proxy DLL placed in the game folder? We can add version information and code signing.
>
> Thank you, <name>, Discord `azalshehri`

## Changes in FeverLink3 that help whichever way this goes

These are recommendations; they are not made here because this work is documentation only.

- When the native part is missing, say so in the window (for example after ten seconds without a `ctl_status.txt`): "The FeverLink3 native part is not installed. Download it from <address>."
- `mod/feverlink3_1/_metadata/modinfo.json` still says the mod "does nothing unless the game was started by FeverLink3.exe". That has been wrong since live mode; the description should say what the mod does now, and its `url` field should point to the project (the wiki recommends it).
- Keep the mod's use of files in one small place, with a clear comment, so that it can be replaced if the game offers an official data exchange.

## Sources (all read on 2026-10-10)

- Support FAQ on mods and Steam Workshop: https://www.transportfever3.com/support/
- Curated Mods Program: https://www.transportfever3.com/curated-mods-program/
- Dev blog, episode 5: https://www.transportfever3.com/news/dev-blog-episode-5-highlights/
- Modding manual, publishing a mod (old revision): https://wiki.transportfever3.com/doku.php?id=modding:general:publishing
- Modding manual, guidelines and requirements (old revision): https://wiki.transportfever3.com/doku.php?id=modding:general:guidelines
- Modding manual, mod definition: https://wiki.transportfever3.com/doku.php?id=modding:general:moddefinition
- mod.io on automated scanning: https://docs.mod.io/moderation/automated-scanning
- mod.io terms of use: https://mod.io/legal/terms
- Nexus Mods file submission guidelines: https://help.nexusmods.com/article/28-file-submission-guidelines
- Nexus Mods page of the game: https://www.nexusmods.com/transportfever3

## What we could not confirm

- Whether the Mod Hub's automatic validation rejects a DLL, or `package.loaded.io` use, outright, or sends it to human review.
- Whether a PC-only flag exists, and whether the Hub accepts a mod that needs an external download.
- The current text of the wiki pages (the copies we could read are marked "old revision").
- The licence terms of the game and the publisher on modification and on use of Steam networking by a third-party DLL.
