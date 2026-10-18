# FeverLink3

> **Under development - please do not use it yet.** FeverLink3 is an unfinished test project. It can crash the game, drift the two worlds apart, force a reload of your world, or leave a build tool unresponsive, and a crash loses any progress you have not saved. Do not install it on a game you care about, and keep backups of your saves. The code is published so that its progress can be followed, not as a release for players.

**Cooperative multiplayer for Transport Fever 3.** Several players build in one world at the same time, each in their own copy of the game. Unofficial, Windows and Steam only, source available under the PolyForm Noncommercial License (free for noncommercial use, see Licence below).

![The host (left) and the guest (right): host, copy code, paste, join, start, speed, pause and chat](docs/media/split-screen-demo.gif)

## Features

- **The full game.** Both worlds stay identical because every game does the same thing, never because a game mechanic is switched off. Town growth, pollution, noise, traffic and ratings work as in the normal game. If two worlds drift apart, the drift is repaired, not hidden.
- **Build together.** Roads, junctions, bridges, tunnels, traffic lights, crosswalks, bulldozing, bus stops, stations and their modules, depots, rail track, switches and signals built by one player appear on the other player's game with the game's own tools.
- **Lines, vehicles and money.** Lines, vehicles bought at a depot, renames, loans, game speed, pause and chat work between the games.
- **Join with a code.** The host copies a join code, a friend pastes it. The connection runs through Steam, so there is no port to open. Everything on the wire is encrypted.
- **Checked all the time.** The games compare their worlds continuously. A lasting difference is repaired from the host's game.

How much of this is proven: everything above was seen working in two real copies of the game on one PC. A whole session between two players over the internet has not been verified yet. The open items are listed in [docs/CHANGELOG.md](docs/CHANGELOG.md) and the verification levels in [docs/site/status.md](docs/site/status.md). Terrain editing, ground paint and trees are not shared yet.

## Performance

Measured in two real games on one PC (so no internet delay). Preparing a build for the other players used to take hundreds of milliseconds; it now takes a few.

| Step of a build | Before | Now |
|---|---|---|
| Capture the game's own build command | 240 to 320 ms | 2 to 12 ms |
| Rebuild it on the other game | 290 to 925 ms | 2 to 40 ms |

A new demo with the timings will be added here later.

## Try the test build

There is no public release yet; a test package is handed out to the people taking part in a test. The full guide is [docs/PLAYING.md](docs/PLAYING.md).

1. Close the game and copy `winhttp.dll` and the `mods` folder from the package into the game's folder (in Steam: right-click Transport Fever 3, Manage, Browse local files). There is no installer.
2. Start Transport Fever 3 from Steam and activate the mod *FeverLink3 Multiplayer* when starting a game.
3. Click **FeverLink3** in the bar at the bottom of the screen. The host clicks **Host a game**, presses **Copy code** and sends the code; a friend pastes it and presses **Join**, and the host presses **Start the session**.

To remove everything: delete `winhttp.dll` and `mods\release\feverlink3` from the game's folder. What the `winhttp.dll` file is and why antivirus programs may look at it: [docs/SECURITY_FOR_USERS.md](docs/SECURITY_FOR_USERS.md).

## More

- How it works: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
- Plans and dates: [docs/ROADMAP.md](docs/ROADMAP.md)
- What changed, and the known gaps: [docs/CHANGELOG.md](docs/CHANGELOG.md)
- The protocol: [docs/PROTOCOL.md](docs/PROTOCOL.md), security review: [docs/SECURITY.md](docs/SECURITY.md)
- Releases: [docs/RELEASING.md](docs/RELEASING.md)

The research behind the project (test-run logs, analyses of the game) is kept private because of its size and the machine details in it; ask in an issue and it can be shared individually.

## Building

Requires [stable Rust](https://rustup.rs). No other SDK.

```
cargo test --workspace --exclude feverlink3_native
cargo test -p feverlink3_native -- --test-threads=1
cargo build --release -p feverlink3_native      # target/release/feverlink3_native.dll -> ship as winhttp.dll
cargo build --release -p feverlink3_launcher    # target/release/feverlink3.exe
pip install lupa && python -I -m unittest discover -s tests/lua -p "test_*.py"
```

The folders: `core/` (protocol and session logic), `launcher/` (the in-game session controller and the `feverlink3` command), `native/` (the `winhttp.dll` proxy the game loads), `mod/feverlink3/` (the game-script mod), `tests/lua/` (two mocked games running the real mod) and `docs/`.

## Credits

FeverLink3 is a fresh implementation, not a fork, but it stands on two projects that proved the model and documented their findings: [TpF2 Multiplayer](https://github.com/silver2127/tpf2-multiplayer) by silver2127 and [MPFever](https://github.com/dkdknight/MPFever) by dkdknight. Both are MIT-licensed, and their licences are reproduced in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

The mod's use of the game's scripting API follows the Transport Fever 3 modding documentation: the official [Transport Fever 3 modding wiki](https://wiki.transportfever3.com/doku.php?id=modding) by Urban Games and the [TF3 docs](https://tf3-docs.transportfever.net/en/guides/index.html) (guides and an API reference generated from the game's own files).

FeverLink3 is not affiliated with, endorsed by or supported by Urban Games or Paradox Interactive. Transport Fever is a trademark of its owners; the name is used here only to say which game the mod is for.

## Contributing and contact

Issues and pull requests are welcome. Read `docs/ARCHITECTURE.md` first; design changes go through an issue before code. Every change to `core/` needs a test. Discord: `azalshehri`. Bug reports and feature requests go to [GitHub issues](https://github.com/AZalshehri7/FeverLink3/issues).

## Licence

FeverLink3 is licensed under the [PolyForm Noncommercial License 1.0.0](LICENSE): you may use, study, modify and share it for any noncommercial purpose, keeping the copyright notice. **Commercial use needs a separate commercial license**; contact me on Discord (`azalshehri`). The few parts adapted from TpF2 Multiplayer and MPFever stay under their own MIT licences, reproduced in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
