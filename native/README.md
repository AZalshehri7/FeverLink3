# native

The module the game loads itself. Rust, built as `winhttp.dll`.

## Why a proxy, why `winhttp.dll`

Transport Fever 3 imports `WINHTTP.dll`, and Windows looks for it in the game folder before the system folder. A file of that name in the game folder is loaded by the game at startup with no injection, which is the least intrusive way to run code inside the process and the one antivirus tolerates best. Every WinHTTP function is relayed unchanged to the real library in the Windows system folder (`src/proxy.rs`).

## Why Rust

- The pure parts, which are most of the module, are unit-tested on any machine: PE parsing (`pe.rs`), signature scanning (`scan.rs`), detour byte maths (`detour.rs`), the build table (`build_table.rs`), the control-file codec (`link.rs`), the hold/release state machine (`hold.rs`), the counters (`counters.rs`) and the install plan (`plan.rs`).
- Memory safety where it matters most: the control-file parsing (and, later, the terrain-cell codec) handle data from outside the process. The reader is bounded and never panics.
- Small output. Release profile: `opt-level = "z"`, fat LTO, one codegen unit, `panic = "unwind"` (required: the game's C++ exceptions pass through the hooks, `docs/NATIVE_LINK.md` section 10), stripped. Game-side code returns errors, and its bookkeeping in the hooks runs inside `log::guarded`.
- A panic still leaves a trace: `log::install_panic_hook` writes its message, thread and place to `native.log` before the process ends, and `native.log` stops growing at 2 MiB.
- The unsafe surface is confined to the relays (`proxy.rs`), memory access (`mem.rs`), detours and patches (`detour.rs`, `patch.rs`), the engine objects (`engine.rs`), the hooks and their installation (`hooks.rs`, `runtime.rs`) and the entry point. Each `unsafe` block has a `SAFETY` comment, and everything that touches game code is tested on fake game functions in executable memory.

## Build

From the repository root (one Cargo workspace with `core/`):

```
cargo test -p feverlink3_native                # pure modules on any host; the fake-game tests need Windows x86-64
cargo build --release -p feverlink3_native     # target/release/feverlink3_native.dll -> ship as winhttp.dll
```

Toolchain: stable Rust. Either `x86_64-pc-windows-msvc` (needs the Visual Studio linker) or `x86_64-pc-windows-gnu` (self-contained, used on machines without Visual Studio). CI builds the msvc one. The release profile lives in the root `Cargo.toml`.

## Activation

The module is inert unless the launcher started the game with `FEVERLINK3_SESSION_DIR` set, or the game itself hosts the session (live mode). Then it logs to `native.log`, reads the game's PE timestamp and looks it up in `build_table::BUILDS`. Unknown build: hooks off, logged, mod runs degraded.

On a known build four pieces are ON by default (hooks, hold, pool patch, Lua seed pin; `=0` turns one off, each is fail-open: an unknown build or other bytes at the site install nothing). The diagnostic pieces are off unless the environment sets them to exactly `1`:

| Environment | Installs |
|---|---|
| `FEVERLINK3_HOOKS` (default on, `=0` off) | The read-only counters: a hook on the simulation's command application. |
| `FEVERLINK3_HOOKS` and `FEVERLINK3_HOLD` (default on, `=0` off) | Also the hold/release hook on the command list (holds nothing until the mod writes `hold_on`). |
| `FEVERLINK3_POOLPATCH` (default on, `=0` off) | The single-worker script pool patch; independent of the other two. |
| `FEVERLINK3_LOG_TOWN_SEED=1` | A read-only diagnostic hook that logs the seed of the **map-generation** location factory (not town growth; `mapgen-seed n=.. seed=.. caller=..` lines in `native.log`) and calls the original unchanged; independent of the others. **Not run in the game.** `src/townseed.rs`, `docs/NATIVE_LINK.md` section 2. |
| `FEVERLINK3_FORCE_TOWN_SEED=<number>` | **Experiment, has not run in the game.** Decimal or `0x` hex, 32 bit. Installs the same map-generation seed hook and additionally replaces the seed argument with this value before calling the original, so games started with the same value generate the same map (log lines then read `seed=<original> forced=<value>`). An invalid value is ignored and logged. Equal seeds alone may not keep the generators equal after a resync (the generator state advances); a reset of the state at every sync point would be the next step. `docs/NATIVE_LINK.md` section 2. |
| `FEVERLINK3_LOG_GROWTH=1` | **Not run in the game.** Read-only log of every town-growth call (`growth n=.. town=.. seed=.. t=.. caller=..` lines in `native.log`; the first 2000, then about 1 in 50): a hook on `TownDeveloper::Develop` and one on the `TownUpdateSize` handler for the game time; both call the original unchanged. `src/growth.rs`, `docs/NATIVE_LINK.md` section 11. |
| `FEVERLINK3_GROWTH_SEED_MODE=off\|derive` | **Experiment, not run in the game.** `off` (default) logs only. `derive` implies the log and, once the switch conditions hold, replaces the random seed `Develop` reads with a hash of (town entity, game time rounded down to the quantum), so two games that run the same growth command a few ms apart draw the same numbers. Only which random numbers are drawn changes; growth, ratings and rules stay the game's own. |
| `FEVERLINK3_GROWTH_SEED_AFTER=<s>`, `FEVERLINK3_GROWTH_SEED_AFTER_GAMETIME=<s>`, `FEVERLINK3_GROWTH_SEED_QUANTUM_MS=<ms>` | When the derive switch happens: wall seconds since the hook was installed (default 0), and/or game seconds (the same moment in both games; the testable one); the quantum of the time input (default 1000 ms). Both thresholds must hold. `docs/NATIVE_LINK.md` section 11. |

`native.log` is `<session folder>\native.log` when the launcher started the game, and `%LOCALAPPDATA%\FeverLink3\native.log` when the game itself hosts the session (live mode; `FEVERLINK3_LIVE_DIR` moves its folder). A log over 1 MiB at start is moved to `native.log.1`.

Each piece is checked against the bytes in memory first, installed all-or-none from `DllMain`, preceded by a self-test of the detour machinery in the shipped binary, and logged line by line to `native.log`; anything in doubt leaves the game as shipped. `docs/NATIVE_LINK.md` is the design, the control-file protocol and the list of what still needs a running game.

## Adding a game build

1. Run the address tool (`tools/addr/find_sites.py`, see its README) against the new `TransportFever3.exe`. It finds the hook sites and the engine helpers by signature and by anchors (string references, callers' idiom, how `CommandList::Add` uses them), extracts the prologue bytes, checks they are whole, position-independent instructions where a detour needs that, reads the command-kind offset and names the command kinds, and prints a `BuildAddrs` row with a confidence per site.
2. Paste the row into `build_table::BUILDS`. The list of construction-tool call sites (`ui_add_sites`) is not found by the tool in a new build: it is verified from the previous build's list, so a new build starts with the hold hook off until a person has chosen the sites (the report lists every caller with the source file it names).
3. Verify the import table still lists only the WinHTTP functions in `proxy.rs`; add any new one.

## Size

The module links Rust's `std` for `Vec`, `String`, threads and file logging, and the launcher crate (for the in-game session controller) with `serde_json`. Measured release build with the MSVC toolchain (the reference figure, printed by CI in the job summary): 1 662 464 bytes with `panic = "unwind"`, the shipped setting; with `panic = "abort"` the same code is 1 132 544 bytes, but that build cannot let the game's C++ exceptions through the hooks (`docs/NATIVE_LINK.md` section 10; `docs/SECURITY.md` section 5 has the numbers and the audit). The planned optimisation, once the hook set is known, is `#![no_std]` + `alloc` with a `HeapAlloc`-backed allocator and a panic handler that logs and aborts; the session controller's dependencies would have to move out of the DLL first.

## Status

Implemented and tested without the game, off by default, **not yet run in the game**: the counters, the hold/release hook, the control link, the single-worker script pool. Nothing from the build table has been installed in a running game yet; `docs/NATIVE_LINK.md` section 8 is the checklist, with the log lines to look for, in the order to try them. See `docs/ROADMAP.md` stage 0.3.
