# FeverLink3

> **Under development - please do not use it yet.** FeverLink3 is an unfinished test project. It can crash the game, drift the two worlds apart, force a reload of your world, or leave a build tool unresponsive, and a crash loses any progress you have not saved. Do not install it on a game you care about, and keep backups of your saves. The code is published so that its progress can be followed, not as a release for players.
>
> The research behind the project (test-run logs, analyses of the game) is kept private because of its size and the machine details in it; ask in an issue and it can be shared individually (see `docs/research/README.md`).

Cooperative multiplayer for Transport Fever 3: several players build in one world at the same time, each in their own copy of the game. Unofficial, Windows and Steam only, source available under the PolyForm Noncommercial License (free for noncommercial use, see Licence below).

**The rule: the full game.** FeverLink3 keeps the two worlds identical by making every game do the same thing, never by switching a game mechanic off. Town growth, pollution, noise, traffic and every rating work as in the normal game. If two worlds drift apart, the drift is repaired, not hidden.

## Status

Early test build. Nothing here is a release for players yet. Three levels, kept apart:

**Seen working in the real game** (build 40420, two real copies of the game, details and dates in [docs/site/status.md](docs/site/status.md)):

- the mod loads and its window opens from a button in the bottom bar;
- the Steam peer-to-peer connection between two different accounts (a friend's test: connected, 199 ms round trip through a Steam relay);
- host, copy code, paste, join, start: the host's world is sent and the guest's game loads it;
- speed, pause and chat from either side show up in both games; leave, remove, rejoin and the end of a session by the host;
- loans, renames and creating a line sync both ways; a road built with the real road tool, and bulldozing, show on the other game.

**Seen in a lab only** (two games on one PC, a scripted test harness, a converted 21-town world, game speed x3; not the same as two players over the internet):

- two games that both start from a fresh load of the same save stay identical, with every checksum equal and every town-growth input equal, for 20 to 30 game minutes;
- a game that kept running and a game that freshly loaded the same save drift apart a few minutes later, in the towns that transport serves; this is why a resync now reloads the host's game too;
- road builds in the host window and the guest window, also right after a resync, show up in the other game.

**Not verified yet:** a whole session over Steam between two accounts, stations, depots, rail, vehicles and terrain between two real games, long sessions in a busy world, and whether the lab results hold in a real two-player game. The one open technical question is what exactly differs between a running world and a loaded one; the current repair (every game reloads at a resync) works around it.

Plans and dates: [docs/ROADMAP.md](docs/ROADMAP.md). How to try the test build: [docs/PLAYING.md](docs/PLAYING.md). What the `winhttp.dll` file is and why antivirus programs may look at it: [docs/SECURITY_FOR_USERS.md](docs/SECURITY_FOR_USERS.md).

## How it works

Transport Fever 3 has no network code, so FeverLink3 keeps every copy of the world identical instead of sending the world around:

1. **Commands, not state.** Every player action is captured as the engine command it produces, stamped with a future simulation step, and sent to everyone. Every game, including the one where the player clicked, applies it at that step. Identical inputs on a deterministic simulation give identical worlds.
2. **Builds happen once, everywhere.** With the native hold switched on (`FEVERLINK3_HOOKS=1 FEVERLINK3_HOLD=1`, off by default for now), the native module holds a build back until the host has named the step, then every game builds at that step. A veto-only check refuses a build that the other game cannot make, instead of building two different things.
3. **Host authority and repair.** The games compare world checksums (about a dozen lanes: time, money, constructions, lines, town buildings, stations, vehicles and more). Money differences are corrected in place from the host. A lasting difference in anything else is repaired by a resync: the host saves, the save is sent, and every game, the host's own included, loads it.
4. **Fast saves over Steam.** One Steam connection carries about 1 MB/s, so a save of 75 to 80 MB is sent over several connections at once (the number scales with the size, up to 16), with a SHA-256 check, and falls back to a single connection if that fails.
5. **Determinism patches** (on by default, each can be switched off). The game's script pool runs on one worker and the seed of the Lua string hash is fixed, so the game's own scripts walk their tables in the same order in every process; `math.random` is a function of the game time.
6. **Sealed transport.** A join code carries the session secret. Everything on the wire is encrypted and authenticated with a key derived from it; the host decides who speaks and what kinds of messages a player may send.
7. **No entity ids on the wire.** Entity ids differ between games, so roads travel as positions, constructions as file plus placement, vehicles and lines as per-session keys.
8. **Connecting the players.** The players' games reach each other through Steam's peer-to-peer networking, so a player only has to share a join code. There is no port to open.

The design is written up in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). FeverLink3 is for Transport Fever 3 only; it does not support Transport Fever 2.

## Try the test build

Short version (the full guide is [docs/PLAYING.md](docs/PLAYING.md)). There is no public release yet; a test package is handed out to the people taking part in a test.

1. Close the game and copy `winhttp.dll` and the `mods` folder from the package into the game's folder (in Steam: right-click Transport Fever 3, Manage, Browse local files). There is no installer. If another mod already uses a `winhttp.dll` in that folder, only one of them can be installed at a time.
2. Start Transport Fever 3 from Steam and activate the mod *FeverLink3 Multiplayer* when starting a game.
3. Click **FeverLink3** in the bar at the bottom of the screen. The host clicks **Host a game**, presses **Copy code** and sends the code; a friend pastes it (Ctrl+V) and presses **Join**, and the host presses **Start the session**.
4. For connection problems, the window has a **Connection test...** section. The same section has a speed test for sending a save over Steam (steps in [docs/PLAYING.md](docs/PLAYING.md) section 5, design in [docs/STEAM_TRANSPORT.md](docs/STEAM_TRANSPORT.md) section 7).
5. If something goes wrong, send `%LOCALAPPDATA%\FeverLink3\live\session.log` and `mod.log` from both PCs, and `%LOCALAPPDATA%\FeverLink3\native.log`.

To remove everything: delete `winhttp.dll` and `mods\release\feverlink3_1` from the game's folder.

## Compared with MPFever

[MPFever](https://github.com/dkdknight/MPFever) is the other Transport Fever 3 multiplayer mod, and it is further along in what it has been used for: terrain edits, separate companies, a launcher and installer, and many build cases fixed from player reports. FeverLink3's differences are the full-game rule (MPFever sets pollution, noise and traffic congestion to zero), Steam peer-to-peer without opening a port, an encrypted and authenticated connection, and an automated test suite. Both are young and change quickly.

## Credits

FeverLink3 is a fresh implementation, not a fork, but it stands on two projects that proved the model and documented their findings. Both are MIT-licensed, and their licences are reproduced in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

- **[TpF2 Multiplayer](https://github.com/silver2127/tpf2-multiplayer)** by silver2127, for Transport Fever 2 (project page: https://silver2127.github.io/tpf2-multiplayer/). Source of the three replication modes, the "no entity ids on the wire" rule, the join-code and sealed-transport trust model, and the host-included resync round. Its `docs/` folder is the best written account of lockstep on this engine.
- **[MPFever](https://github.com/dkdknight/MPFever)** by dkdknight (arthur), for Transport Fever 3. First to run lockstep co-op on TF3: the proxy-DLL hold-and-release of construction commands, the single-worker script pool fix for determinism, deterministic `math.random`, the fixed Lua string-hash seed, and native terrain-cell copying. When FeverLink3 reuses a specific finding or table from it, the file says so.

If you are the author of either and want the wording changed, open an issue.

FeverLink3 is not affiliated with, endorsed by or supported by Urban Games or Paradox Interactive. Transport Fever is a trademark of its owners; the name is used here only to say which game the mod is for.

## Layout

| Folder | Role | Language |
|---|---|---|
| `core/` | Crate `feverlink3_core`. Protocol: framing, sealed channel, join codes, the host and player session state machines, resync and permissions. No game or UI code. Tests in `core/tests`. | Rust |
| `launcher/` | Crate `feverlink3_launcher` and the `feverlink3` command (`install`, `uninstall`, `doctor`, `selftest`, and `host` / `join` for development). Also the session controller that runs inside the game, the file link to the mod, the savegame transfer (single and parallel Steam connections) and update verification. See `launcher/README.md`. | Rust |
| `native/` | `winhttp.dll`: the proxy the game loads itself. It relays the 14 WinHTTP functions the game imports and starts the in-game session controller. It also holds the capture-and-hold hook for builds, the determinism patches and the control link to the mod. See `native/README.md` and `docs/NATIVE_LINK.md`. | Rust |
| `mod/feverlink3_1/` | Game-script mod: lockstep core (stamping, barrier, pause, queue, deterministic random, checksums), file link, command capture and the command schemas for everyday operations, and the in-game window. See `mod/README.md`. | Lua |
| `tools/addr/` | Address tool: finds the hook sites in a game executable and prints a build-table row. See `tools/addr/README.md`. | Python |
| `tools/release/` | Release tool: reproducible zip, manifest with SHA-256, Ed25519 key generation and signing. See `docs/RELEASING.md`. | Rust |
| `tests/lua/` | Two mocked games running the real mod through lupa. `core/tests/` and the native crate run under `cargo test --workspace`. | Python, Rust |
| `docs/` | Architecture, roadmap, protocol, playing guide, security review (`SECURITY.md`) and explainer, release and listing notes, command coverage. | |

The repository is one Cargo workspace (`core`, `native`, `launcher`, `tools/release`).

## Building

Requires [stable Rust](https://rustup.rs). No other SDK.

```
cargo test --workspace --exclude feverlink3_native
cargo test -p feverlink3_native -- --test-threads=1
cargo build --release -p feverlink3_native      # target/release/feverlink3_native.dll -> ship as winhttp.dll
cargo build --release -p feverlink3_launcher    # target/release/feverlink3.exe
pip install lupa && python -I -m unittest discover -s tests/lua -p "test_*.py"
```

A release is built by GitHub Actions from a commit on `main` whose message starts with the version, for example `V0.1.0: First beta release`; every other commit only runs the tests. Nothing is built or uploaded from a developer's PC. See [docs/RELEASING.md](docs/RELEASING.md).

## Contributing

Issues and pull requests are welcome. Read `docs/ARCHITECTURE.md` first; design changes go through an issue before code. Every change to `core/` needs a test.

## Contact

Discord: `azalshehri`. Bug reports and feature requests go to [GitHub issues](https://github.com/AZalshehri7/FeverLink3/issues); for anything else, or to join the first test sessions, message me on Discord.

## Licence

FeverLink3 is licensed under the [PolyForm Noncommercial License 1.0.0](LICENSE): you may use, study, modify and share it for any noncommercial purpose, keeping the copyright notice. **Commercial use needs a separate commercial license**; contact me on Discord (`azalshehri`). The few parts adapted from TpF2 Multiplayer and MPFever stay under their own MIT licences, reproduced in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
