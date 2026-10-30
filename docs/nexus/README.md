# Nexus Mods and mod.io listing material (0.2.10)

Maintainer notes for publishing FeverLink3 0.2.10 on Nexus Mods and mod.io. Rewritten on 2026-10-22 for the release https://github.com/AZalshehri7/FeverLink3/releases/tag/v0.2.10 (it replaces the stale 2026-10-10 connection-test text). **Nothing here has been published or sent to anyone.** This folder is excluded from the project site on purpose.

| File | What it is |
|---|---|
| `README.md` | This checklist: rules found, risks, exact upload steps, the message to Nexus staff |
| `description.bbcode.txt` | The Nexus long description in BBCode, ready to paste |
| `fields.md` | Every other Nexus form field: name, summary, category, version, file description, requirements, permissions, changelog, pinned post |
| `MODIO.md` | The mod.io / in-game Mod Hub listing: texts, tags, package layout, a PowerShell command to build it, images |
| `modio-description.html` | The mod.io / Mod Hub description as HTML (goes into the mod's `_metadata/description.html`, see `MODIO.md`) |
| `media/` | Images made for the listings (see "Images" below). `docs/media` is untouched. |
| `description.md`, `shot-list.md` | **Old (2026-10-10, connection-test build). Do not use.** Delete them or rewrite them later. |

## Rules found (read 2026-10-22)

**Nexus Mods** ([file submission guidelines](https://help.nexusmods.com/article/28-file-submission-guidelines)):

1. "Files (especially executables) that connect to the internet to download or send information and/or files are prohibited unless where it is crucial for the functioning of the mod/utility." The same rule says **"auto update" functionality does not qualify as crucial**, and asks authors of such tools to contact staff or support with the source code. Nexus "reserve[s] the right to moderate and/or remove" files that talk to the internet.
2. Generative AI: two tags. **AI-Generated Content** covers "AI-Generated code, user interface elements, ..."; **AI Assisted** is for "Developer-led code ... with limited AI involvement" and needs evidence such as commit history, or moderators may replace the tag. "If your mod is primarily generated with AI, you should apply the AI-Generated Content tag." Undisclosed or wrongly tagged AI use "may be subject to moderation action". Authors are responsible for the accuracy of every claim on the page, including AI-written text.
3. Files must be functional, properly categorised and tagged; images relevant and not misleading; no placeholders.
4. Game page: Nexus serves `https://www.nexusmods.com/games/transportfever3` behind a bot check, so its existence could not be confirmed from here (an earlier search snapshot on 2026-10-10 said it exists). If the game is missing in the "Add a mod" search, request it there with the official title "Transport Fever 3" ([how to add a game](https://help.nexusmods.com/article/104-how-can-i-add-a-new-game-to-nexus-mods)).
5. Image size limits are not published; users report about 2 MB per image. `media/` has images under that.

**mod.io and the TF3 Mod Hub**: see `MODIO.md` (summary: the Mod Hub package can only be the mod folder; `winhttp.dll` cannot go through it).

## Risks that could get an upload removed (highest first)

1. **Nexus: the automatic updater.** The `winhttp.dll` in the official zip downloads new versions from GitHub at game start (it can be turned off in Troubleshooting). Nexus explicitly says auto-update is not "crucial", so this is the most likely reason for removal. The Steam peer-to-peer traffic *is* the mod's function and should be acceptable after the staff message. Options, owner decides:
   - A. Ask staff first (message below) and publish only after a yes. Recommended.
   - B. A Nexus-specific build without the updater (the release workflow already enables updates only with `FEVERLINK3_OFFICIAL_BUILD=1`), uploaded as its own file. Needs a code/release change; not done.
   - C. Publish the page without a file and link to GitHub. **Not allowed**: Nexus forbids non-functional pages and links to files it would not host.
2. **AI tag.** The code and docs were written largely by Claude under the owner's direction. That matches **AI-Generated Content**, not "AI Assisted" ("limited AI involvement"). Choosing "AI Assisted" risks the tag being replaced and a moderation note. Recommendation: AI-Generated Content, and say in the description that the owner directs, reviews and tests every change.
3. **Unsigned DLL.** Antivirus or Nexus's own scan may flag an unsigned proxy `winhttp.dll`. The page says it is unsigned until 1.0.0 and gives the SHA-256 on GitHub.
4. **Mod Hub (mod.io):** see `MODIO.md`: the mod's `io` use (outside "only the official scripting API"), console players, and the stale `modinfo.json` description.

## Checklist for the owner, in order

### Before anything

1. Read `description.bbcode.txt` and `fields.md` and approve the wording (CLAUDE.md rule 6 applies to public text too).
2. AI tag: AI Assisted (owner decision 2026-10-22).
3. **Send the staff message** (below) through Nexus support (https://help.nexusmods.com, "Contact us" / submit a request) while logged in. Wait for the answer. If they want the updater off, that is option B above.

### Creating the Nexus page (after a yes)

1. nexusmods.com → **Upload** (Add a mod) → game **Transport Fever 3**.
2. Paste name, summary, version, category, language from `fields.md`. Tick the AI tag. Adult content: no.
3. Description: paste `description.bbcode.txt`, open the preview. If a tag shows as text, remove that tag.
4. **Files tab:** upload `FeverLink3-0.2.10.zip` **unchanged**, exactly as downloaded from the GitHub release (its SHA-256 must match the release's `.sha256`). Category: Main files. Version 0.2.10. File description from `fields.md`. Do not upload `manifest.json` or the `.sha256`.
5. **Images tab** (first image = thumbnail):
   1. `docs/nexus/media/cover-1920x1080.png` (the in-game window and bar icon; 0.4 MB)
   2. `docs/nexus/media/demo-build-480.gif` (the build demo, 480 px wide, 1.6 MB; if Nexus refuses a GIF, use `demo-build-still-1920x1080.png`)
   3. `docs/media/ui-overview.png` (1.1 MB, the full interface sheet)
   4. `mod/feverlink3/_metadata/0.png` (the mod picture, optional)
6. **Requirements** and **Permissions & credits** tabs: from `fields.md`.
7. Publish. Then pin the known-issues post from `fields.md`.

### After publishing

1. Put the Nexus link in the release notes, `README.md`, the site and the Reddit draft (`docs/research/REDDIT_POST_0.2.2.md`). Owner approves before any push.
2. Every full release: the release workflow (job `nexus`) uploads the zip as a new version of the main file, archives the old one, updates the mod version and sends the version's CHANGELOG section as plain text (docs/RELEASING.md). Check the Files and Changelog tabs afterwards.

## Message to Nexus staff (send before publishing)

```
Subject: Request before publishing: co-op multiplayer mod with a native DLL that uses the network (FeverLink3, Transport Fever 3)

Hello,

I would like to publish FeverLink3 Multiplayer (version 0.2.2) for Transport Fever 3. It is a cooperative multiplayer mod: several players build in one world, each in their own game. The source is public under the PolyForm Noncommercial License 1.0.0 (source available, free for noncommercial use).

Your file submission guidelines ask authors of files that connect to the internet to contact you first with the reasoning and the source, so here they are:

- Source: https://github.com/AZalshehri7/FeverLink3 (native/ is the DLL, launcher/ the session code it runs).
- The download contains a Lua game mod and winhttp.dll, a proxy DLL the game loads from its own folder. No .exe, no installer.
- Network use 1 (the mod's function): game-to-game traffic through Steam's peer-to-peer networking, encrypted with a key from the join code. No server of mine is involved, no ports are opened.
- Network use 2 (I want to be upfront): at game start the DLL checks GitHub once for a newer FeverLink3 release and, if there is one, downloads it from the GitHub release only (HTTPS, GitHub hosts only), checks every file's SHA-256 against the release manifest and installs it after one restart. Players can turn this off in the in-game Troubleshooting section. I know your rules say auto-update does not count as crucial: if you want it off for the Nexus file, I can upload a build without the updater. Please tell me which you prefer.
- No telemetry, no analytics, no ads, no account. "Report a problem" collects the session's logs (cleaned of personal details) from the players into one zip on the player's own PC, over the same encrypted session; nothing goes to any server.
- What the DLL does, file by file: https://github.com/AZalshehri7/FeverLink3/blob/main/docs/SECURITY_FOR_USERS.md
- Releases are built by GitHub Actions; the zip, its SHA-256 and a manifest are on the release page. The DLL is not code-signed yet.
- The code and docs were written largely with an AI assistant (Claude) under my direction; I will set the AI Assisted taged Content tag and say so on the page.

May I publish it as described, and should the Nexus file have the updater on (with the off switch) or off?

Thank you,
Abdulaziz Alshehri (azalshehri on Discord)
```

## Images

Made with Pillow from `docs/media` (which was not modified):

| File | Size | Use |
|---|---|---|
| `media/cover-1920x1080.png` | 1920x1080, 0.4 MB | Nexus thumbnail; mod.io / Mod Hub cover (16:9, at least 512x288, under 8 MB; real game UI, no added text as the TF3 wiki asks) |
| `media/demo-build-480.gif` | 480x353, 23 frames, 1.6 MB | Nexus image (the 5.4 MB original is likely over Nexus's limit). Same real-time timing as the original. |
| `media/demo-build-still-1920x1080.png` | 1920x1080, 1.6 MB | Last frame of the demo on a 16:9 canvas, for mod.io's gallery (mod.io galleries take jpg/png only, no GIF) |

Nothing else is missing. A video (YouTube link) would be the way to show the build demo moving on mod.io; none exists yet.
