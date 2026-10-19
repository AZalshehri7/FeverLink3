# FeverLink3

**Play Transport Fever 3 together: several players build in one shared world, each in their own copy of the game.**

[![Latest pre-release](https://img.shields.io/github/v/release/AZalshehri7/FeverLink3?include_prereleases&label=pre-release)](https://github.com/AZalshehri7/FeverLink3/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/AZalshehri7/FeverLink3/ci.yml?branch=main&label=CI)](https://github.com/AZalshehri7/FeverLink3/actions/workflows/ci.yml)
[![License: PolyForm Noncommercial 1.0.0](https://img.shields.io/badge/license-PolyForm%20Noncommercial%201.0.0-blue)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Transport%20Fever%203%20%C2%B7%20Windows-informational)](#install)
[![Status: alpha](https://img.shields.io/badge/status-alpha-orange)](docs/site/status.md)

[Playing guide](docs/PLAYING.md) · [Changelog](docs/CHANGELOG.md) · [Roadmap](docs/ROADMAP.md) · [Status](docs/site/status.md) · [FAQ](docs/site/faq.md) · [Architecture](docs/ARCHITECTURE.md) · [Protocol](docs/PROTOCOL.md) · [Privacy](docs/site/privacy.md) · [Security for players](docs/SECURITY_FOR_USERS.md)

> **Under development.** FeverLink3 is an early test project. It can crash the game, make the worlds drift apart, reload your world for a repair, or leave a build tool stuck. Use a save you do not mind losing and keep backups.

![The host (left) draws a road with the game's own road tool; the guest's game (right) shows it a moment later](docs/media/demo-build.gif)

## Features

How far each feature is proven:
✅ seen in the real game (two game windows on one PC, or a test with a friend over the internet) ·
🧪 lab only (two game windows on one PC, driven by scripts) ·
⏳ automatic tests only, not yet seen in the game.

**What sets it apart**

- ✅ **Steam peer-to-peer.** Players reach each other through Steam: no port forwarding, no router settings, no IP addresses. The host copies a join code and sends it to a friend; the friend pastes it and joins.
- ⏳ **Steam invites.** Invite friends with Steam's own invite dialog or the friends list (Invite to Game, Join Game); the host lets them in, and the session key never appears in what Steam shows other friends.
- ✅ **Runs inside the game.** No launcher or extra program to start. You start Transport Fever 3 from Steam as usual and open the FeverLink3 window from the game's bottom bar.
- ✅ **The full game.** Town growth, traffic, pollution, noise and ratings work as in single player. Nothing is switched off to keep the worlds equal.

**Playing together**

- ✅ **One shared world.** The host's world is sent to every player, and a player can join a running session later.
- ✅ **Everyone builds.** Roads, junctions, bridges, tunnels, traffic lights, crosswalks, bulldozing, bus stops, stations and their modules, depots, rail track, switches and signals appear on every game.
- ✅ **Lines, vehicles and money.** Lines, vehicles bought at a depot, renames and loans are shared. 🧪 Line edits while vehicles run, cloning vehicles, and the bulldozer's refund for something you have just built.
- ✅ **Speed, pause and chat.** The game's own speed and pause buttons, and the window's chat, work for everyone.
- ✅ **Encrypted.** Everything between the games is encrypted, and the join code works like a password for the session.
- ✅ **Automatic world repair.** If the worlds differ, every game reloads the host's world and play goes on.
- 🧪 **Regular world checks.** The games compare their worlds about every 5 seconds, spread over many small steps so the game does not stutter.
- 🧪 **Stall tolerance.** A player whose game freezes for a few seconds (an autosave, a slow PC) does not stop the others; that game catches up afterwards.
- 🧪 **Smooth pacing.** When one game falls a little behind, the other slows down slightly instead of standing still.
- ⏳ **Short drops.** A player who loses the connection for less than a minute comes back without a reload.
- ⏳ **Up to eight players** in one session.

**Not yet:** terrain editing, ground paint and trees are not shared; one shared company for all players (separate companies are planned).

## Performance

Measured in the lab: **one PC, two game windows**, so there is no internet delay. **Internet numbers are not measured yet.**

| What | Result | Conditions |
|---|---|---|
| Click to build on both games | median 600 to 650 ms | lab, game speed x3, real tool clicks and scripted builds |
| World check | about every 5 s (12 rounds per minute), 0 differences in 41 rounds | lab, x4 |
| Cost of the world check | at most 15 to 19 ms per step, inside a 30 ms budget | lab, x4 |
| Standing still to wait for the other game | 0.0 to 0.5 s in the worst minute | lab, x3 |
| One game frozen for 5.5 s on purpose | the other game ran on; the frozen one caught up in about 3.8 s, no repair needed | lab, x4 |
| Copy of a build on the other game | capture 2 to 12 ms, rebuild 2 to 40 ms | two real windows, one PC |

## Install

There is no installer: you copy two things into the game's folder.

1. Download `FeverLink3-<version>.zip` from [Releases](https://github.com/AZalshehri7/FeverLink3/releases) and extract it (right-click, **Extract All**). Inside the folder `FeverLink3-<version>` you find:
   - `winhttp.dll` (the small native part the game loads at start),
   - a `mods` folder with the mod in `mods\release\feverlink3`,
   - `README.txt`, `HOW-TO-PLAY.txt`, `LICENSE` and `THIRD_PARTY_NOTICES.md` (to read, not to copy).
2. Close the game. In Steam, right-click **Transport Fever 3**, choose **Manage**, then **Browse local files**. The game's folder opens (it holds `TransportFever3.exe`).
3. Drag `winhttp.dll` and the `mods` folder into the game's folder. When Windows asks about the existing `mods` folder, choose to merge; nothing of the game is replaced. If Windows says a `winhttp.dll` is already there, stop: another mod uses that file name.
4. Start the game from Steam as usual. When you start or load a game, activate **FeverLink3 Multiplayer** in the mod list.

**Host:** click **FeverLink3** in the bottom bar, press **Host a game**, press **Copy code** and send the code to your friend. When the friend shows up under **Players**, press **Start the session**.

**Join:** open the FeverLink3 window, click the field **paste the join code here**, paste with `Ctrl+V` and press **Join**. Your game loads the host's world when the host starts the session. Only join people you trust, as you would trust the author of a mod.

All players need the same game version. The full guide, including what to send when something goes wrong, is in [docs/PLAYING.md](docs/PLAYING.md).

**Uninstall:** close the game and delete `winhttp.dll` and the folder `mods\release\feverlink3` from the game's folder. Without a session, the mod does nothing and the game plays as usual.

## Build from source

Requires [stable Rust](https://rustup.rs) (and Python with `lupa` for the Lua tests).

```
cargo test --workspace --exclude feverlink3_native
cargo test -p feverlink3_native -- --test-threads=1
cargo build --release -p feverlink3_native      # target/release/feverlink3_native.dll -> ship as winhttp.dll
pip install lupa && python -I -m unittest discover -s tests/lua -p "test_*.py"
```

Issues and pull requests are welcome; read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) first. Bug reports go to [GitHub issues](https://github.com/AZalshehri7/FeverLink3/issues); Discord: `azalshehri`.

## License

[PolyForm Noncommercial 1.0.0](LICENSE): free to use, study, change and share for noncommercial purposes; commercial use needs a separate license (ask on Discord).

## Credits

FeverLink3 builds on what earlier Transport Fever multiplayer projects found and documented; their licenses and credits are in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). The mod's use of the game's scripting API follows the official [Transport Fever 3 modding wiki](https://wiki.transportfever3.com/doku.php?id=modding) and the [TF3 docs](https://tf3-docs.transportfever.net/en/guides/index.html).

## Unofficial

FeverLink3 is an unofficial fan project for Transport Fever 3 only (Windows, Steam). It is not affiliated with, endorsed by or supported by Urban Games or Paradox Interactive. Transport Fever is a trademark of its owners.
