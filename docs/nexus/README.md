# Nexus Mods listing material

Maintainer notes for publishing FeverLink3 on Nexus Mods (the game has a page at https://www.nexusmods.com/transportfever3). Written on 2026-10-10 for the test build 0.2.0-test1. **Nothing here has been published or sent to anyone.** This folder is excluded from the project site on purpose.

| File | What it is |
|---|---|
| `description.bbcode.txt` | The long description in Nexus BBCode, ready to paste into the Description box (about 15,600 characters; tags checked for balance) |
| `description.md` | The same text in Markdown, for GitHub or another host |
| `fields.md` | Every other form field as paste-ready text: name, summary, version, file description, requirements, permissions and credits, changelog, pinned post, and the message to Nexus staff |
| `shot-list.md` | The screenshots and GIFs to capture, with exact steps, what to hide, captions, and which ones are blocked |

## What the listing says `0.2.0-test1` is

Following the owner's decisions of 2026-10-10: the first test build is the **Steam connection test** (two testers exchange Steam IDs in the window and read the result); hosting and joining over Steam are "the next test build" (in the code, the window's Host button answers that hosting over Steam is not finished); the zip holds a testing installer, the native module and the mod; the end state is the mod offered in the game's Mod Hub (mod.io backend) and on Nexus Mods like any other mod, with the native module delivered separately for now. The description says all of this and promises nothing beyond it. If the build you tag does more (hosting over Steam works, for example), rewrite the sections "What this test build does" and "Status" first.

## The rules the text follows

1. **Only what has been seen working.** The status block lists what was seen in the real game (build 40420, 2026-10-09 and 2026-10-10) and what was not. A claim moves from "not verified" to "seen working" only after it has been seen, with the date. The proof milestones are in `docs/ROADMAP.md`.
2. **The connection is described as Steam peer-to-peer, "being tested".** Proven so far: inside one game, a connection to its own Steam ID (6 pings answered). The first test between two different Steam accounts is what `0.2.0-test1` is for; until it has been done the text must never say that it works over the internet. Hosting and joining over Steam are "the next test build". No other transport is announced (the owner has parked any server of the project; do not mention one). The text asks the player to do nothing to connect except share and paste the join code.
3. **Both predecessors are credited** wherever the project is described: TpF2 Multiplayer by silver2127 and MPFever by dkdknight (arthur), both MIT.
4. **Nexus's own rules are respected**: an executable that connects to the internet needs the staff's go-ahead with reasons and source code (message drafted in `fields.md`); AI-assisted work must be tagged and disclosed (see step 1 below); claims must be accurate.

## Checklist, in order

### Before the page

1. **AI disclosure.** The Nexus guidelines require generative-AI tags and say that undisclosed AI use may be moderated. The code and docs of this project were largely written with Claude under your direction, and the commit messages say so. The description already says it in the credits. Decide which Nexus AI tag applies and set it when you create the page.
2. **Cut the release.** Set the workspace version to the release version in `Cargo.toml` (so that `native.log` and the zip name agree), tag `v0.2.0-test1`, push the tag, wait for the release job (`docs/RELEASING.md`). The real signing key must exist first; the key in the repository is a placeholder whose private half is public. Keep the zip byte for byte as the release asset: the SHA-256 in the release notes is what players compare.
3. **Re-check the claims against the tagged code** (table below). Fix the text, not the table, if the code differs.
4. **Capture the screenshots** (`shot-list.md`). Hide join codes, Steam IDs and your Windows user name.
5. **Write to Nexus staff** with the message in `fields.md` and wait for the answer.
6. **Look at the live form** for what could not be read: the summary and description length limits, the category list, the tag list, the file description limit, the image limits. If the description is too long, take the FAQ and the changelog spoiler blocks out (about 5,000 characters), put the changelog in the Changelogs tab and the FAQ in a pinned post.

### Creating the page

1. New mod, game "Transport Fever 3". Paste the name, summary and the fields from `fields.md`. Paste `description.bbcode.txt` into the description and open the preview: if a tag shows up as text, remove that tag (Nexus validates BBCode strictly).
2. Upload the zip from the GitHub release as the main file, with the file description from `fields.md`. Set the permissions, the requirements and the credits as written there. Add the images in the order given in `shot-list.md`.
3. Pin the "known issues" post.

### After publishing

1. Put the Nexus link on `docs/index.md`, `README.md` and the release notes, and change "planned" to the address.
2. Update `docs/ROADMAP.md` (the stage 0.3 item about the Nexus listing).
3. Watch the comments and the bug reports; update the pinned post when an issue is fixed or a new one appears. When the first two-account Steam test has been done, change every "being tested" accordingly: `Select-String -Path docs -Pattern "being tested" -Recurse`.

## Claims to re-check against the tagged code

The listing and the user security page make these statements. Each one is true of the code only if the check gives the expected result. Claims 1 and 2 depend on the in-game path not using the development transport (plain TCP, only started when the window asks for `transport = "tcp"`); check them first. If a check fails, the sentence is false: change the code, or change the sentences in `description.*`, `docs/SECURITY_FOR_USERS.md`, `docs/PLAYING.md`, `docs/site/privacy.md` and `docs/site/faq.md` before anything is published.

| # | Claim | Check | Expected |
|---|---|---|---|
| 1 | The host does not open an inbound TCP port in normal use | With the game running and hosting: `Get-NetTCPConnection -OwningProcess (Get-Process TransportFever3).Id -State Listen`. In the source: where `TcpListener::bind` is reachable from `live::spawn` | no listening TCP socket owned by the game process; no call path from `live::spawn` to `TcpListener::bind` |
| 2 | Nothing else leaves the PC besides the player-to-player path through Steam | `Select-String -Path launcher\src\*.rs -Pattern "ipify\|checkip\|icanhazip\|M-SEARCH\|PortMapping"`, and the outgoing connections listed by `Get-NetTCPConnection -OwningProcess ... -State Established` while hosting | no matches reachable from `live::spawn`; only the game's and Steam's own connections |
| 3 | The Steam listener answers `ping ` messages only and ignores other data | read `launcher/src/steamnet.rs` (`reply_for`, `receive`) | unchanged, or update `docs/SECURITY_FOR_USERS.md` |
| 4 | No server of the project is involved in the connection | `Select-String -Path launcher\src\*.rs,native\src\*.rs -Pattern "RelayAddr\|relay::"` | no use outside tests. If a server of the project is ever added to the path, `docs/SECURITY_FOR_USERS.md`, `docs/site/privacy.md` and the listing must say so first |
| 5 | Steam between two accounts is "being tested" | the result of the first run (P6) | change the wording everywhere once it has been done, in either direction; hosting and joining stay "the next test build" until P7 |
| 6 | The DLL passes 14 WinHTTP functions through | `Select-String -Path native\src\proxy.rs -Pattern "^relay!"` gives 14 lines; the address tool's report for the supported build says the lists are identical | 14, identical |
| 7 | This version installs no hooks into the game | `Select-String -Path native\src\*.rs -Pattern "detour::install"` outside tests | no call. If hooks land, rewrite the sections "What may change" and "Known gaps" and the changelog |
| 8 | Only game build 40420 is known | `native/src/build_table.rs`, `BUILDS` | one row |
| 9 | No telemetry, no update check | no network client code in `launcher/` or `native/` other than the session and Steam paths; `launcher/src/update.rs` has no fetch | none |
| 10 | Names of buttons and texts in the window | `mod/feverlink3_1/content/feverlink3_ui.script.lua` | the strings used in `docs/PLAYING.md`, the description and `shot-list.md` |
| 11 | What `feverlink3 install` prints and installs | run `feverlink3 install --dry-run` | one line per item: mod, `steam_appid.txt`, `winhttp.dll`, `feverlink3_path.txt` |
| 12 | The files and folders in the table of `docs/SECURITY_FOR_USERS.md` | `launcher/src/install.rs`, `launcher/src/live.rs`, `mod/feverlink3_1/content/feverlink3_common.lua` | same names and places |
| 13 | The verified and not-verified lists | `docs/ROADMAP.md`, proof milestones | the same everywhere |
| 14 | `modinfo.json` description | `mod/feverlink3_1/_metadata/modinfo.json` | **It still says the mod "does nothing unless the game was started by FeverLink3.exe", which has been wrong since live mode, and the game shows this text in its mod list.** Fix it before the release |

## Placeholders

None of the text contains a placeholder. The hash of the zip is deliberately not in the listing: the page points to the release notes, which are written by the release job.
