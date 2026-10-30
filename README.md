# FeverLink3

**Play Transport Fever 3 together. Several players build in one shared world, each in their own game.**

[![Latest release](https://img.shields.io/github/v/release/AZalshehri7/FeverLink3?include_prereleases&label=release)](https://github.com/AZalshehri7/FeverLink3/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/AZalshehri7/FeverLink3/ci.yml?branch=main&label=CI)](https://github.com/AZalshehri7/FeverLink3/actions/workflows/ci.yml)
[![License: PolyForm Noncommercial 1.0.0](https://img.shields.io/badge/license-PolyForm%20Noncommercial%201.0.0-blue)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Transport%20Fever%203%20%C2%B7%20Windows-informational)](#install)
[![Status: beta](https://img.shields.io/badge/status-beta-yellow)](docs/site/status.md)

[Playing guide](docs/PLAYING.md) · [Changelog](docs/CHANGELOG.md) · [Roadmap](docs/ROADMAP.md) · [FAQ](docs/site/faq.md) · [Privacy](docs/site/privacy.md) · [Architecture](docs/ARCHITECTURE.md)

![A road drawn on the host appears on the second game: at game speed x1 after 1.4 s, at x4 after 0.8 s, in real time](docs/media/demo-build.gif)

*Real time: the road stands on both games 1.4 s after the click at game speed x1 (normal) and 0.8 s at x4 (two games on one PC, a grown save with 6,393 inhabitants). The wait is a safety margin: every game builds at the same game step. In the beta, stability comes first.*

## What you get

- **Built into the game, no separate launcher.** Start Transport Fever 3 from Steam as usual: hosting, joining, chat, syncing and updates all live in the game's bottom bar and window. There is no extra program to run.
- **Build together.** Roads, bridges, tunnels, rail, signals, stations, stops, depots, lines and vehicles: what one player builds appears in every game.
- **Your other mods work together too.** Mods that change your world by themselves, such as Auto Line Namer, Auto Color Lines or Auto Assign Terminals, stay in sync: the host's copy makes the change and every game applies it at the same moment, and what you set in such a mod's own window reaches every game (new in 0.2.4; seen with two games on one PC).
- **Companies.** The host starts the session with one shared company: everyone builds and pays from one account, as before. A company for each player, with its own money, is upcoming: it shows as a grayed button until it has been played between two PCs.
- **The full game.** Towns grow, traffic flows, ratings change, exactly as in single player. Nothing is switched off.
- **Easy to join.** Over Steam: no ports, no router settings, no IP addresses. The host sends a join code, the friend pastes it.
- **Steam invites.** Invite friends from Steam's own invite dialog; with a world loaded they join by themselves (played between two PCs over the internet).
- **Always the same world.** The games check each other all the time. If they ever drift apart, everyone gets the host's world back in a few seconds and play goes on.
- **Shared speed, pause and chat.**
- **Encrypted.** Everything between the games is encrypted; the join code works like a password.
- **One-click bug reports.** "Report a problem" collects the logs of every player in the session into one zip, with personal data removed, ready to attach to a GitHub issue.

## The interface

A coloured icon in the game's bottom bar shows the session at a glance: white (no session), yellow (waiting), green (in sync), red (needs attention). Click it for the details.

![The FeverLink3 window: pop-up, host and join, players, sync, chat, host tools and bug report](docs/media/ui-overview.png)

## Tested

- Played with friends over the internet, and in long two-game sessions on one PC.
- In a 3.5-hour session over Steam with a friend, an automatic repair was needed about once every 40 minutes (about 15 s each); other mods that change the world on their own can add more.
- About 1,600 automatic tests run on every change: the network protocol, the sessions, the world checks and two complete games simulated side by side.
- A test lab drives two real games with scripts to measure speed and catch drift.

Measured with two games on one PC (internet numbers follow):

| | Result |
|---|---|
| Click to build on both games | 1.4 s at game speed x1, 0.8 s at x4 (the recording above: real time, a grown save with 6,393 inhabitants) |
| World check | every 5 s, without stutter |
| A game freezing for a few seconds | the others play on; it catches up by itself |
| Repair when the worlds differ | about 15 s for a 45 MB world |

## Coming next

- Coming back after a short drop without a reload
- A company for each player, with its own money

FeverLink3 is in beta: keep backups of the saves you play with.

## Install

1. Download `FeverLink3-<version>.zip` from [GitHub Releases](https://github.com/AZalshehri7/FeverLink3/releases/latest) or [Nexus Mods](https://www.nexusmods.com/transportfever3/mods/19) and extract it.
2. In Steam, right-click **Transport Fever 3** → **Manage** → **Browse local files**.
3. Drag `winhttp.dll` and the `mods` folder into that folder (merge the folders when Windows asks).
4. Start the game, and turn on **FeverLink3 Multiplayer** in the mod list when you start or load a game.

**Host:** click the FeverLink3 icon in the bottom bar → **Host a game** → **Copy code** and send it to your friend → when they appear, start the session with **One shared company**.

**Join:** open the FeverLink3 window, paste the code (`Ctrl+V`) and press **Join**. Only join people you trust.

Everyone needs the same game version. More in the [playing guide](docs/PLAYING.md).

**Uninstall:** delete `winhttp.dll` and `mods\release\feverlink3` from the game's folder.

## Build from source

Needs [stable Rust](https://rustup.rs), and Python with `lupa` for the Lua tests.

```
cargo test --workspace --exclude feverlink3_native
cargo test -p feverlink3_native -- --test-threads=1
cargo build --release -p feverlink3_native      # target/release/feverlink3_native.dll -> ship as winhttp.dll
pip install lupa && python -I -m unittest discover -s tests/lua -p "test_*.py"
powershell -NoProfile -ExecutionPolicy Bypass -File tools\check.ps1   # all of CI's checks at once, a table, exit code 0 = all passed
```

Issues and pull requests are welcome. Bug reports: [GitHub issues](https://github.com/AZalshehri7/FeverLink3/issues) (use **Report a problem** in the game to make the zip); Discord: `azalshehri`.

## License

[PolyForm Noncommercial 1.0.0](LICENSE): free to use, study, change and share for noncommercial purposes. For commercial use, ask on Discord.

## Credits

FeverLink3 builds on what earlier Transport Fever multiplayer projects found and documented; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). It uses the game's official scripting API as described in the [Transport Fever 3 modding wiki](https://wiki.transportfever3.com/doku.php?id=modding) and the [TF3 docs](https://tf3-docs.transportfever.net/en/guides/index.html).

## Unofficial

FeverLink3 is an unofficial fan project for Transport Fever 3 (Windows, Steam). It is not affiliated with or endorsed by Urban Games or Paradox Interactive. Transport Fever is a trademark of its owners.
