# Steam Workshop and the Mod Hub: publishing plan

Written on 2026-10-10 from public pages read that day (the list, with addresses, is at the end). Some of the wiki pages are marked "old revision" by the wiki, so check the current text before acting on any rule quoted here. Anything that could not be confirmed is marked as such.

## Owner decision, and what this plan is for

Decision of 2026-10-10: the end state is **the mod offered in the game's own Mod Hub (mod.io backend) and on Nexus Mods like any other mod, with the native module (`winhttp.dll`) delivered separately for now**, and `feverlink3.exe` kept as a testing tool. This document says what stands in the way, what to ask Urban Games, what to change in the mod so that it has a chance, and what to do if the Hub says no. **It does not say that the Hub will accept the mod: that is unconfirmed**, and nothing about a Hub listing should be announced to players before the Hub has accepted it.

## What the sources say

1. **Transport Fever 3 has no Steam Workshop.** The developers' support page says: "the Steam Workshop would undermine this single-destination approach to modding so we are not including it in the game." The developer blog (episode 5) says the Workshop is "not yet confirmed" and would be decided after the Mod Hub matches its functions. We could not establish which of the two pages is newer, but the game's Steam store page (read on 2026-10-10) lists Single-player, Steam Achievements, Includes level editor and Family Sharing as features, does not list Steam Workshop, and speaks of an in-game mod browser. The plan therefore assumes there is no Workshop for the game.
2. **The official channel is the in-game Mod Hub, built on mod.io.** Mods are uploaded from the game (from the `staging_area` folder of the user data) or on mod.io, pass an automatic validation for PC and console, and are shared for every platform from one library. A separate Curated Mods Program exists; its entry conditions (10 published mods, 100,000 downloads in total, a record of well-rated mods) are not something this project meets.
3. **The Hub cannot deliver a native DLL for the game folder.** A Hub mod is a folder under the user mods folder (`mods/<author>_<modname>/` with `mod.json`, `_metadata/` and `content/`); nothing in that structure puts a file into the game folder, and the published rules are about game resources and scripts. The wiki's technical requirements add: "Only the official Transport Fever 3 scripting API may be used" and "Executable files such as batch files and shell scripts are not allowed". mod.io's own documentation says its schema check flags archive, DLL and executable types and lets each game decide whether to block the upload or send it to the game's admin. So the DLL has to stay a separate download, as decided. The open question is whether the Hub accepts the Lua part, which does nothing without the DLL.
4. **Nexus Mods can take the whole package.** The game has a page there (`nexusmods.com/transportfever3`) and an experimental Windows-only co-op mod with a native component is already listed (read from a search snapshot on 2026-10-10). The official publishing page itself says that uploads to other hosts are manual and follow each host's terms, and names the German transportfever.net file base as an example.

## What FeverLink3 consists of

| Part | Where it lives | Channel |
|---|---|---|
| `mod/feverlink3_1` (Lua game scripts and the in-game window) | the user mods folder | the Mod Hub, if accepted; Nexus Mods and GitHub in any case |
| `winhttp.dll` (native module and the in-game controller) | the game folder | Nexus Mods and GitHub only, as a separate file |
| `feverlink3.exe` (testing installer, `doctor`, development commands) | anywhere | the test zip only; not part of the end state |

## The likely blockers, honestly

**B1. The native DLL cannot go through the Hub.** By the decision above it is delivered separately. Without it there is no session controller, no Steam networking and no connection to other players: Lua alone has no network access. Consoles cannot load native code from a mod at all.

**B2. The Lua part is not "official API only".** The game removes the `io` global from its scripts. The mod's window script gets it back through `package.loaded.io` (a finding from MPFever) and uses it, and `os.getenv`, to read and write plain text files in `%LOCALAPPDATA%\FeverLink3\live\`, which is how the mod talks to the native module. That is outside what the wiki calls the official API. Automatic validation may or may not notice it (unknown), a human reviewer would, and a future game update could close the loophole. There is no known official way for a script to exchange data with an outside program. **This is the largest technical risk for a Hub listing.**

**B3. Useless on its own.** Installed from the Hub without the DLL, the mod does nothing. The window now has a clear message for this case (after 15 seconds without an answer from the native module it says "The FeverLink3 module is not running" and what to do), so the failure is at least visible; but that message still tells the player to run `feverlink3 install` from a FeverLink3 folder, which a Hub user does not have. A Hub listing that cannot work without a separate manual download will still collect bad ratings, and sits uneasily with the Hub's idea of one library that works the same on every platform and with the rule that "mods must be non-commercial and accessible in full for free to all users" (it is free, but not usable in full from the Hub alone).

**B4. Consoles.** Hub mods are offered to PlayStation and Xbox as well (the Hub even generates a console-optimised version on upload). We found no "PC only" flag in the pages we could read, so console players could download a mod that can never work for them unless the description is very clear.

**B5. Multiplayer is not on the developers' list.** The support page says: "The goal has been to make Transport Fever 3 the best single player experience possible. Until that has been accomplished, it would be the wrong time to shift focus and split resources in order to introduce multiplayer in either a co-operative or competitive fashion." That is a deferral, not a ban, but it means there is no sanctioned interface for what we do and the studio could object. We have not reviewed the game's or the publisher's licence terms for modifying the game or for using Steam networking through the game's own Steam library with the game's app id from a third-party DLL. **These are open questions to ask, not a legal opinion.**

**B6. Presentation rules** (these we can meet): name at most 32 characters, summary at most 100 characters, description in English, cover image 1920x1080 (16:9, at most 8 MB, no text or logos; script mods may use an icon or a screenshot of their window), text files UTF-8 without BOM, lowercase file and folder names, at most 500 MB unpacked, `severityRemove` set in `mod.json`, tags from the official list. Checked on the mod folder in this repository: no file has a byte-order mark, all names are lowercase, `modinfo.json` is within the length limits (name 22, summary 57 characters), `mod.json` declares `severityAdd` and `severityRemove` as `None`. Not checked: whether the tag "Script Mod" is on the official list.

## The staged plan

1. **Now: GitHub pre-release and Nexus Mods** with the test zip (`docs/nexus/` has the page text). Nexus's rule about executables that connect to the internet needs a message to their staff first (drafted in `docs/nexus/fields.md`).
2. **Ask Urban Games** (draft below) whether a Hub listing of the Lua part, with a clear "PC only, needs a separate Windows component" description, is welcome, and how they see B2. Do this before investing in Hub-specific work.
3. **Make the mod Hub-ready** (the changes under "What to change in the mod" below), without changing what it does.
4. **Try the Hub's own validation.** The upload dialog has a visibility setting and the validation runs before upload, and the entry's page on mod.io shows the approval status per platform. Whether a hidden upload makes the result visible without making the mod public is unconfirmed; do it only with the owner's go-ahead, and delete the entry afterwards if it is refused. Read the result per platform.
5. **Publish:** if accepted, the mod on the Hub and on Nexus Mods, and the native module as a separate file on Nexus Mods and GitHub, each description linking the other and saying that the mod needs the native module and works only on Windows PCs.

### If the Hub says no

| Answer | What to do |
|---|---|
| Accepts the mod as it is | Publish (step 5). |
| Accepts it after changes | Make them, unless they remove the file link (then B2 decides: see the next row). |
| Refuses because of the file access (B2) | Ask for an official way to exchange data with an external program; until there is one, stay on Nexus Mods, GitHub and transportfever.net with the whole package. |
| Refuses a mod that needs an external download (B3) | A small "guide" item is the only Hub option left (a mod whose purpose is its description, saying where to get FeverLink3). It is likely to be treated as spam or low quality, and links in descriptions may be restricted (the `url` field is not shown on consoles), so ask first. Otherwise stay outside the Hub. |
| No answer | Do not publish on the Hub. |

Moving the session controller out of the DLL into the external program (the original design) would not help: it removes the DLL but not the file access of the Lua part, and loses the planned hold-and-release hooks.

## What to change in the mod (recommendations, not done here)

This work is documentation only, so none of these is made; each is small.

- **`mod/feverlink3_1/_metadata/modinfo.json` is wrong today.** The description says the mod "does nothing unless the game was started by FeverLink3.exe" and the summary says "start the game through FeverLink3.exe". Both have been untrue since live mode, and the game shows these texts in its mod list. They should say what the mod does now (a multiplayer window and the lockstep scripts, needs the separate native module, Windows PC only) and `url` should point to the project; the wiki recommends it so that players can find the source.
- When the native module is missing, the window's message should name the download page instead of `feverlink3 install`, in the build meant for the Hub.
- Keep the use of files in one small place with a clear comment, so that it can be replaced if the game offers an official data exchange.
- Prepare the cover image and the English description per B6, and check the tag list.

## Draft message to Urban Games

The wiki points to the contact form on https://www.transportfever3.com/support/.

> Subject: Question about a PC-only multiplayer mod with a native component (FeverLink3)
>
> Hello Urban Games team, I am the author of FeverLink3, an open-source (MIT) cooperative multiplayer mod for Transport Fever 3 (https://github.com/AZalshehri7/FeverLink3). It builds on two earlier MIT-licensed community projects, MPFever and TpF2 Multiplayer, which are credited in its README. It has a normal Lua mod and a small Windows DLL that the game loads from its own folder as `winhttp.dll` (it passes the game's 14 WinHTTP imports through unchanged; the page https://github.com/AZalshehri7/FeverLink3/blob/main/docs/SECURITY_FOR_USERS.md explains it in plain words). We would like to do this the way you prefer and have a few questions:
> 1. May the Lua mod be listed in the Mod Hub if its description says clearly that it needs a separate, manually installed Windows component and does nothing without it, or should the whole thing stay outside the Hub?
> 2. Can a mod be marked PC-only so it is not offered on consoles?
> 3. Our window script reads and writes plain text files in `%LOCALAPPDATA%` through `package.loaded.io`, because the `io` global is not available to UI scripts. Is that acceptable, or will it be blocked? Is there an official way for a script to exchange data with an external program?
> 4. We connect the players with Steam's peer-to-peer networking through the game's already loaded `steam_api64.dll`, using the game's app id. Do you or Paradox have a position on that?
> 5. Do you have objections to a proxy DLL placed in the game folder? We can add version information and code signing.
>
> Thank you, <name>, Discord `azalshehri`

## Safe things to do before asking

1. Check the tag list and the limits above against the current wiki text (the pages we read are marked "old revision").
2. Read the game's and the publisher's licence text (Steam shows the game's EULA on its store page) for clauses on modification and on network services, and note the exact wording in the message to Urban Games.
3. Re-check the Steam store page's feature list before any release: a Steam Workshop integration would change this plan.

## Sources (all read on 2026-10-10)

- Support FAQ on mods, multiplayer and Steam Workshop: https://www.transportfever3.com/support/
- The game's Steam store page: https://store.steampowered.com/app/3493540/Transport_Fever_3/
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

- Whether a Steam Workshop integration is added later (re-check the store page's feature list before a release).
- Whether the Mod Hub's automatic validation rejects a DLL, or `package.loaded.io` use, outright, or sends it to human review.
- Whether a PC-only flag exists, and whether the Hub accepts a mod that needs an external download.
- The current text of the wiki pages (the copies we could read are marked "old revision").
- The licence terms of the game and the publisher on modification and on use of Steam networking by a third-party DLL.
