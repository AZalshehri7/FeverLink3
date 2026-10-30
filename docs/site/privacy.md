---
title: FeverLink3 privacy
---

# Privacy

Last updated 2026-10-30 (version 0.2.10). [Back to the start page](../index.html)

FeverLink3 has no accounts, no analytics, no advertising, and no server of the project that collects anything. This page says what leaves your PC and who sees it. The technical detail is in [What is in winhttp.dll, and how to check it](../SECURITY_FOR_USERS.html).

## What the project collects

Nothing. The program does not report to the project: no telemetry, no crash reports, no usage data. The automatic update asks GitHub (`api.github.com` and the release files on `github.com`) for the newest FeverLink3 release once per game start, and downloads a newer one inside the game (no other program is started); GitHub sees your IP address, as for any download. You can turn it off in the window's Troubleshooting section (details: [What is in winhttp.dll](../SECURITY_FOR_USERS.html), "Automatic updates"). If you send logs to the maintainer for a bug report, that is your choice; the log files hold no personal data besides your Windows user name and the paths of your Steam folders (see [Playing FeverLink3](../PLAYING.html), "If something goes wrong").

## If you make a bug report

The **Report a problem** button makes a zip file on your PC. Nothing is sent to the project: you decide whether to attach the zip to a GitHub issue (public) or send it to the maintainer.

- **What it collects from each player.** In a session, every player's game adds its own part, and the parts travel to the host over the session's encrypted connection, like the world does; the player who pressed the button gets the zip, and the host keeps a copy in `Documents\FeverLink3\reports`. Each player sees a notice that their logs were sent to the host. A part holds that game's FeverLink3 logs (`mod.log`, `session.log`, `native.log`, `native_events.log`, the window's state `ctl_status.txt`, the end of the id-order trace), the game's own error files when they are from the last day (`error.json`, `stdout.txt` and the newest error report of the `crash_dump` folder, never the memory dumps), and a short `report.txt`: the FeverLink3 version, the game build, your mod list, the name of your newest save file, your role, the players' display names, the connection type and ping, and the last sync. No savegame is included.
- **What is taken out, on your PC, before your part leaves it.** Your Windows user name becomes `%USER%`; Steam IDs become a code that is the same within one report and different in the next (each player's own code becomes `player-1`, `player-2`, ...); your Steam account number becomes `%STEAMACCOUNT%`; IP addresses become `<ipv4>` / `<ipv6>`; join codes are removed. Display names, the mod list and what the logs say about the world (build numbers, places, times) stay, because they are what makes the report useful.
- **Size.** The zip stays under 24 MB (GitHub's limit for an attachment is 25 MB); when the logs are bigger, the oldest lines of the biggest logs are left out, and `report.txt` says so.

## What other players receive in a session

- The display name you use. It starts as your Windows user name; the window has a name field, so change it before hosting or joining if you prefer another.
- Your game actions, world checksums and chat lines.
- Since 0.2.10, once a session uses companies: your display name and your company. They are saved in the world (the savegame), so whoever gets a copy of that save sees them; that is how a rejoining player finds their company again.
- If you host: your world, as a savegame, when a player joins or you press "Sync everyone to my world".
- If you use the Steam connection test: the Steam ID you paste or share. A Steam ID identifies your Steam account. Share it only with the person you are testing with.

A join code contains the session secret, the time it was made and how to reach the host (for sessions over Steam, the host's Steam ID). Everything in a session is encrypted and authenticated with a key derived from the secret in the code. Anyone who has the join code can join, so treat it like a password for that session.

## What the connection reveals

Players are connected through Steam's peer-to-peer networking. Steam decides whether two games talk directly or through its relay servers; Steam's own documentation says that relayed connections do not reveal the players' IP addresses to each other, and a direct connection, like any direct connection between two computers, does. The in-game connection test shows which route was used ("direct" or "through a Steam relay"). Steam sees which Steam IDs connect to each other, as it does for any Steam game. If the project ever adds a server of its own to the path, this page will say so and say what it can and cannot see.

## Websites

This site is hosted by GitHub Pages, and downloads are hosted by GitHub (and Nexus Mods when the page exists). Those services have their own logs and privacy policies. The site itself sets no cookies and has no analytics of its own; the page theme may load web fonts from Google Fonts, which is a request to Google.

## Files on your PC

The files FeverLink3 creates and where they are, including the logs and session files in `%LOCALAPPDATA%\FeverLink3` that stay when you delete the DLL and the mod, are in the table in the [security page](../SECURITY_FOR_USERS.html).

## Contact

Discord `azalshehri`, or [GitHub issues](https://github.com/AZalshehri7/FeverLink3/issues).
