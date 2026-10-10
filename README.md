# FeverLink3

Cooperative multiplayer for Transport Fever 3. The goal: several players build in one world at the same time, each in their own copy of the game. Source available under the PolyForm Noncommercial License (free for noncommercial use; see Licence below), Windows and Steam only, unofficial.

**Status: early test build (0.2.0-test1), last updated 2026-10-10. It is not yet a playable multiplayer game.** The first test build has one job: to find out whether two players can reach each other through Steam. It adds a window to the game with a Steam connection test; hosting and joining a game over that connection come in the next test build. What is real and what is not:

- **Verified in the real game** (build 40420, 2026-10-09): the mod loads; its window opens from a button in the bottom bar; the mod can drive the game's own save and load and picks its session files up again after a reload; the Steam peer-to-peer listener starts. With an earlier build of the window, whose Host button used a direct connection (since taken out of the window), hosting showed a join code and a second client (a test client, not a second copy of the game) could join. On 2026-10-10 the game opened a Steam peer-to-peer connection to its own Steam ID inside one process and 6 pings were answered.
- **Not verified yet:** the Steam connection between two different accounts (the test of this build), hosting and joining over Steam (not finished), building together (replicating construction and vehicle commands between two real games), sending a savegame between two real games, the native command hooks (switched off unless a developer turns them on; never run in the game), and joining from a second real copy of the game.

Details and dates: [docs/ROADMAP.md](docs/ROADMAP.md). How to try the test build: [docs/PLAYING.md](docs/PLAYING.md). What the `winhttp.dll` file is and why antivirus programs may look at it: [docs/SECURITY_FOR_USERS.md](docs/SECURITY_FOR_USERS.md).

## How it works

Transport Fever 3 has no network code, so FeverLink3 keeps every copy of the world identical instead of sending the world around:

1. **Commands, not state.** Every player action is captured as the engine command it produces, stamped with a future simulation step, and sent to everyone. Every game, including the one where the player clicked, applies it at that step. Identical inputs on a deterministic simulation give identical worlds.
2. **Host authority.** One game is the authority. The games compare world checksums at common steps; a lasting difference is repaired by reloading the host's savegame everywhere (the host included, so entity order matches).
3. **Sealed transport.** A join code carries the session secret. Everything on the wire is encrypted and authenticated with a key derived from it; the host decides who speaks and what kinds of messages a player may send.
4. **No entity ids on the wire.** Entity ids differ between games, so roads travel as positions, constructions as file plus placement, vehicles and lines as per-session keys.
5. **Connecting the players.** The players' games reach each other through Steam's peer-to-peer networking, so a player only has to share a join code and has nothing to set up. The Steam connection is proven only inside one game so far; the first test between two different Steam accounts is what the current test build is for, and carrying whole sessions over it is the next step.

The design is written up in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). FeverLink3 is for Transport Fever 3 only; it does not support Transport Fever 2.

## Try the test build

Short version (the full guide is [docs/PLAYING.md](docs/PLAYING.md)):

1. Unzip the release, close the game, double-click `feverlink3.exe` and choose `1` (install). A terminal does the same: `feverlink3 install`.
2. Start Transport Fever 3 from Steam and activate the mod *FeverLink3 Multiplayer*.
3. Click **FeverLink3** in the bar at the bottom of the screen. Under **Steam connection test**, two testers exchange their Steam IDs, paste the other's ID and press **Test the connection**.
4. Next test build: the host clicks **Host a game** and sends the join code; a friend pastes it and presses **Join**.

To remove everything: close the game and run `feverlink3 uninstall`. The installer is a testing tool: the plan is that the mod reaches players from the in-game Mod Hub or Nexus Mods, with the native module (`winhttp.dll`) delivered separately for now ([docs/WORKSHOP.md](docs/WORKSHOP.md)).

## Credits

FeverLink3 is a fresh implementation, not a fork, but it stands on two projects that proved the model and documented their findings. Both are MIT-licensed, and their licences are reproduced in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

- **[TpF2 Multiplayer](https://github.com/silver2127/tpf2-multiplayer)** by silver2127, for Transport Fever 2 (project page: https://silver2127.github.io/tpf2-multiplayer/). Source of the three replication modes, the "no entity ids on the wire" rule, the join-code and sealed-transport trust model, and the host-included resync round. Its `docs/` folder is the best written account of lockstep on this engine.
- **[MPFever](https://github.com/dkdknight/MPFever)** by dkdknight (arthur), for Transport Fever 3. First to run lockstep co-op on TF3: the proxy-DLL hold-and-release of construction commands, the single-worker script pool fix for determinism, deterministic `math.random`, and native terrain-cell copying. When FeverLink3 reuses a specific finding or table from it, the file says so.

If you are the author of either and want the wording changed, open an issue.

FeverLink3 is not affiliated with, endorsed by or supported by Urban Games or Paradox Interactive. Transport Fever is a trademark of its owners; the name is used here only to say which game the mod is for.

## Layout

| Folder | Role | Language |
|---|---|---|
| `core/` | Crate `feverlink3_core`. Protocol: framing, sealed channel, join codes, the host and player session state machines, resync and permissions. No game or UI code. Tests in `core/tests`. | Rust |
| `launcher/` | Crate `feverlink3_launcher` and the `feverlink3` command (`install`, `uninstall`, `doctor`, `selftest`, and `host` / `join` for development). Also the session controller that runs inside the game, the file link to the mod, savegame transfer and update verification. See `launcher/README.md`. | Rust |
| `native/` | `winhttp.dll`: the proxy the game loads itself. It relays the 14 WinHTTP functions the game imports and starts the in-game session controller. It also contains the command counters, the capture-and-hold hook, the determinism patch and the control link to the mod; all of that is off unless switched on by environment variables, is tested on fake game functions, and has not yet run in the game. See `native/README.md` and `docs/NATIVE_LINK.md`. | Rust |
| `mod/feverlink3_1/` | Game-script mod: lockstep core (stamping, barrier, pause, queue, deterministic random, checksums), file link, command capture and the command schemas for everyday operations, and the in-game window. See `mod/README.md`. | Lua |
| `tools/addr/` | Address tool: finds the hook sites in a game executable and prints a build-table row. See `tools/addr/README.md`. | Python |
| `tools/release/` | Release tool: reproducible zip, manifest with SHA-256, Ed25519 key generation and signing. See `docs/RELEASING.md`. | Rust |
| `tests/lua/` | Two mocked games running the real mod through lupa (lockstep core, and the operations schemas). | Python |
| `docs/` | Architecture, roadmap, protocol, playing guide, security explainer, release and listing notes, command coverage and research notes. | |

The repository is one Cargo workspace (`core`, `native`, `launcher`, `tools/release`).

## Building

Requires [stable Rust](https://rustup.rs). No other SDK.

```
cargo test --workspace
cargo build --release -p feverlink3_native      # target/release/feverlink3_native.dll -> ship as winhttp.dll
cargo build --release -p feverlink3_launcher    # target/release/feverlink3.exe
pip install lupa && python -I tests/lua/test_lockstep.py && python -I tests/lua/test_cmds_ops.py
```

Releases are built by GitHub Actions from a tag, never on a developer's PC; see [docs/RELEASING.md](docs/RELEASING.md).

## Contributing

Issues and pull requests are welcome. Read `docs/ARCHITECTURE.md` first; design changes go through an issue before code. Every change to `core/` needs a test.

## Contact

Discord: `azalshehri`. Bug reports and feature requests go to [GitHub issues](https://github.com/AZalshehri7/FeverLink3/issues); for anything else, or to join the first test sessions, message me on Discord.

## Licence

FeverLink3 is licensed under the [PolyForm Noncommercial License 1.0.0](LICENSE): you may use, study, modify and share it for any noncommercial purpose, keeping the copyright notice. **Commercial use needs a separate commercial license**; contact me on Discord (`azalshehri`). The few parts adapted from TpF2 Multiplayer and MPFever stay under their own MIT licences, reproduced in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
