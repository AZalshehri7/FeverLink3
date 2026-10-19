---
title: FeverLink3 privacy
---

# Privacy

Last updated 2026-10-10. [Back to the start page](../index.html)

FeverLink3 has no accounts, no analytics, no advertising, and no server of the project that collects anything. This page says what leaves your PC and who sees it. The technical detail is in [What is in winhttp.dll, and how to check it](../SECURITY_FOR_USERS.html).

## What the project collects

Nothing. The program does not report to the project: no telemetry, no crash reports, no automatic update check (the developer program's `feverlink3 update` asks GitHub for the newest release only when you type it). If you send logs to the maintainer for a bug report, that is your choice; the log files hold no personal data besides your Windows user name and the paths of your Steam folders (see [Playing FeverLink3](../PLAYING.html), "If something goes wrong").

## What other players receive in a session

Sessions over Steam arrive in the next test build; this is what they are designed to share.

- The display name you use. It starts as your Windows user name; the window has a name field, so change it before hosting or joining if you prefer another.
- Your game actions, world checksums and chat lines.
- If you host: your world, as a savegame, when a player joins or you press "Sync everyone to my world".
- If you use the Steam connection test: the Steam ID you paste or share. A Steam ID identifies your Steam account. Share it only with the person you are testing with.

A join code contains the session secret, the time it was made and how to reach the host (for sessions over Steam, the host's Steam ID). Everything in a session is encrypted and authenticated with a key derived from the secret in the code. Anyone who has the join code can join, so treat it like a password for that session.

## What the connection reveals

Players are connected through Steam's peer-to-peer networking. Steam decides whether two games talk directly or through its relay servers; Steam's own documentation says that relayed connections do not reveal the players' IP addresses to each other, and a direct connection, like any direct connection between two computers, does. The in-game connection test shows which route was used ("direct" or "through a Steam relay"). Steam sees which Steam IDs connect to each other, as it does for any Steam game. This is being tested; the first two-account test is still to come. If the project ever adds a server of its own to the path, this page will say so and say what it can and cannot see.

## Websites

This site is hosted by GitHub Pages, and downloads are hosted by GitHub (and Nexus Mods when the page exists). Those services have their own logs and privacy policies. The site itself sets no cookies and has no analytics of its own; the page theme may load web fonts from Google Fonts, which is a request to Google.

## Files on your PC

The files FeverLink3 creates and where they are, including the ones the uninstaller leaves behind (logs and session files in `%LOCALAPPDATA%\FeverLink3`), are in the table in the [security page](../SECURITY_FOR_USERS.html).

## Contact

Discord `azalshehri`, or [GitHub issues](https://github.com/AZalshehri7/FeverLink3/issues).
