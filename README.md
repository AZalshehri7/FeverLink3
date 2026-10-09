# FeverLink3

Cooperative multiplayer for Transport Fever 3. Several players build in one world at the same time, each in their own copy of the game, over the internet or a LAN.

**Status: pre-alpha, under construction.** Nothing here plays a game yet. The protocol core and its tests exist; the game-side pieces (native module, Lua mod) are being written. See [docs/ROADMAP.md](docs/ROADMAP.md).

## How it works

Transport Fever 3 has no network code, so FeverLink3 keeps every copy of the world identical instead of sending the world around:

1. **Commands, not state.** Every player action is captured as the engine command it produces, stamped with a future simulation step, and sent to everyone. Every game, including the one where the player clicked, applies it at that step. Identical inputs on a deterministic simulation give identical worlds.
2. **Host authority.** One game is the authority. The games compare world checksums at common steps; a lasting difference is repaired by reloading the host's savegame everywhere (the host included, so entity order matches).
3. **Sealed transport.** A join code carries the session secret. Everything on the wire is encrypted and authenticated with a key derived from it; the host decides who speaks and what kinds of messages a player may send.
4. **No entity ids on the wire.** Entity ids differ between games, so roads travel as positions, constructions as file plus placement, vehicles and lines as per-session keys.

The design is written up in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). FeverLink3 is for Transport Fever 3 only; it does not support Transport Fever 2.

## Credits

FeverLink3 is a fresh implementation, not a fork, but it stands on two projects that proved the model and documented their findings. Both are MIT-licensed, and their licences are reproduced in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

- **[TpF2 Multiplayer](https://github.com/silver2127/tpf2-multiplayer)** by silver2127, for Transport Fever 2. Source of the three replication modes, the "no entity ids on the wire" rule, the join-code and sealed-transport trust model, and the host-included resync round. Its `docs/` folder is the best written account of lockstep on this engine.
- **[MPFever](https://github.com/dkdknight/MPFever)** by dkdknight (arthur), for Transport Fever 3. First to run lockstep co-op on TF3: the proxy-DLL hold-and-release of construction commands, the single-worker script pool fix for determinism, deterministic `math.random`, and native terrain-cell copying. When FeverLink3 reuses a specific finding or table from it, the file says so.

If you are the author of either and want the wording changed, open an issue.

## Layout

| Folder | Role | Language |
|---|---|---|
| `core/` | Crate `feverlink3_core`. Protocol: framing, sealed channel, join codes, session model. No game or UI code. Tests in `core/tests`. | Rust |
| `launcher/` | `FeverLink3.exe`: lobby, game start, save transfer, resync. *(planned)* | Rust |
| `native/` | Proxy DLL the game loads itself: command capture and hold, determinism patches. Skeleton: proxy, PE parsing, scanner, detour maths, build table. | Rust |
| `mod/feverlink3_1/` | Game-script mod: lockstep core (stamping, barrier, pause, queue, deterministic random, checksums), file link, command capture. In-game UI and native-build replay *(planned)*. See `mod/README.md`. | Lua |
| `tools/addr/` | Address tool: finds the hook sites in a game executable and prints a build-table row. See `tools/addr/README.md`. | Python |
| `tests/lua/` | Two mocked games running the real mod through lupa; `core/tests/` and the native crate run under `cargo test --workspace` | Python, Rust |
| `docs/` | Architecture, roadmap, protocol notes | |

The repository is one Cargo workspace (`core`, `native`). Lua lockstep tests with lupa are planned alongside the mod.

## Building

Requires [stable Rust](https://rustup.rs). No other SDK.

```
cargo test --workspace
cargo build --release -p feverlink3_native     # target/release/feverlink3_native.dll -> ship as winhttp.dll
pip install lupa && python -I tests/lua/test_lockstep.py
```

## Contributing

Issues and pull requests are welcome. Read `docs/ARCHITECTURE.md` first; design changes go through an issue before code. Every change to `core/` needs a test.

## Contact

Discord: `azalshehri`. Bug reports and feature requests go to [GitHub issues](https://github.com/AZalshehri7/FeverLink3/issues); for anything else, or to join the first test sessions, message me on Discord.

## Licence

MIT. See [LICENSE](LICENSE).
