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

The module is inert unless the launcher started the game with `FEVERLINK3_SESSION_DIR` set, or the game itself hosts the session (live mode). Then it logs to `native.log`, reads the game's PE timestamp and finds the build's places by search (below); when the search does not prove every site: hooks off, logged, mod runs degraded.

On a known build four pieces are ON by default (hooks, hold, pool patch, Lua seed pin; `=0` turns one off, each is fail-open: an unknown build or other bytes at the site install nothing). The diagnostic pieces are off unless the environment sets them to exactly `1`:

| Environment | Installs |
|---|---|
| `FEVERLINK3_HOOKS` (default on, `=0` off) | The read-only counters: a hook on the simulation's command application. |
| `FEVERLINK3_HOOKS` and `FEVERLINK3_HOLD` (default on, `=0` off) | Also the hold/release hook on the command list (holds nothing until the mod writes `hold_on`). |
| `FEVERLINK3_POOLPATCH` (default on, `=0` off) | The single-worker script pool patch; independent of the other two. |
| `FEVERLINK3_ACTING_PLAYER` (default on since 0.2.10, `=0` off; must be the same on every game) | No patch of its own: when the hold hook applies a build command it held, the owners in that command that are the tool's own player become the carrier command's player (the player's company in a "Company for each player" session; `acting.rs`, `docs/COMPANIES.md`). Pieces of another company's split road keep their owner. Announced as `acting_player` in the `ready` line. Seen in two game windows on one PC (2026-10-27). |
| `FEVERLINK3_LOG_TOWN_SEED=1` | A read-only diagnostic hook that logs the seed of the **map-generation** location factory (not town growth; `mapgen-seed n=.. seed=.. caller=..` lines in `native.log`) and calls the original unchanged; independent of the others. **Not run in the game.** `src/townseed.rs`, `docs/NATIVE_LINK.md` section 2. |
| `FEVERLINK3_FORCE_TOWN_SEED=<number>` | **Experiment, has not run in the game.** Decimal or `0x` hex, 32 bit. Installs the same map-generation seed hook and additionally replaces the seed argument with this value before calling the original, so games started with the same value generate the same map (log lines then read `seed=<original> forced=<value>`). An invalid value is ignored and logged. Equal seeds alone may not keep the generators equal after a resync (the generator state advances); a reset of the state at every sync point would be the next step. `docs/NATIVE_LINK.md` section 2. |
| `FEVERLINK3_LOG_GROWTH=1` | **Not run in the game.** Read-only log of every town-growth call (`growth n=.. town=.. seed=.. t=.. caller=..` lines in `native.log`; the first 2000, then about 1 in 50): a hook on `TownDeveloper::Develop` and one on the `TownUpdateSize` handler for the game time; both call the original unchanged. `src/growth.rs`, `docs/NATIVE_LINK.md` section 11. |
| `FEVERLINK3_GROWTH_SEED_MODE=off\|derive` | **Experiment, not run in the game.** `off` (default) logs only. `derive` implies the log and, once the switch conditions hold, replaces the random seed `Develop` reads with a hash of (town entity, game time rounded down to the quantum), so two games that run the same growth command a few ms apart draw the same numbers. Only which random numbers are drawn changes; growth, ratings and rules stay the game's own. |
| `FEVERLINK3_GROWTH_SEED_AFTER=<s>`, `FEVERLINK3_GROWTH_SEED_AFTER_GAMETIME=<s>`, `FEVERLINK3_GROWTH_SEED_QUANTUM_MS=<ms>` | When the derive switch happens: wall seconds since the hook was installed (default 0), and/or game seconds (the same moment in both games; the testable one); the quantum of the time input (default 1000 ms). Both thresholds must hold. `docs/NATIVE_LINK.md` section 11. |

`native.log` is `<session folder>\native.log` when the launcher started the game, and `%LOCALAPPDATA%\FeverLink3\native.log` when the game itself hosts the session (live mode; `FEVERLINK3_LIVE_DIR` moves its folder). A log over 1 MiB at start is moved to `native.log.1`.

Each piece is checked against the bytes in memory first, installed all-or-none from `DllMain`, preceded by a self-test of the detour machinery in the shipped binary, and logged line by line to `native.log`; anything in doubt leaves the game as shipped. `docs/NATIVE_LINK.md` is the design, the control-file protocol and the list of what still needs a running game.

## How a game build is found

No game build is written in the module's code. At every start `search.rs` repeats the address tool's proof on the running
game's image, from a generated proof file embedded in the DLL (`tools/addr/out/proof-6ad9ce10.bin`, build 40425, written by
`tools/addr/port_build.py <exe> --export-proof`). Every function of the row must match exactly once at a function start of the
image's unwind table (or one copy at the neighbours' shift), its callees must start with the same code, all 22 layout witnesses
must match exactly, and the preview listener's offsets are read from the matched listener; shape matches only where
`port_build.py` allows them. Anything not proven: the whole row is refused, nothing is hooked, the game runs degraded (the
in-game window says so, hosting is refused and a join is refused by the host with the reason, `docs/LIVE_CONTROL.md`).

The result is cached by PE timestamp in `%LOCALAPPDATA%\FeverLink3\build-search-<timestamp>.txt`; the cache is checked against
the proof at every start and deleted when it no longer matches. `FEVERLINK3_BUILD_SEARCH=0` turns the search off. A full search
takes about 0.3 s for 57 MB of `.text` (release build), the cached check under a millisecond.

`tools/addr/out/verified-builds.txt` (generated, embedded) lists the builds that were played and checked: name, PE timestamp and
the hash of the row (`build_table::row_hash`). A build whose search gives that hash is confirmed; any other proven build plays
and the window says it was found by search and is not yet confirmed. Every join carries `build:hash` (`searched`), which host and
player must share. Checked on the real executables (read as data): the embedded proof gives on 40420 and 40425 exactly the rows of
the committed port reports, with the verified hashes (`search::tests::the_search_reproduces_the_committed_port_reports_and_the_verified_hashes`,
ignored: the executables are not in the repository). Seen in two game windows on build 40425 ("every site found by search", a verified build).

Left in the code as facts of the reference code, which the search proves unchanged before they are used: the portable layout
table (`portable.rs`, named 40420) with `build_table::LAYOUT_FACTS` (kind byte offset, build-proposal kind, the listener's getter
slot) and the two call offsets inside the refund insert (`refund::INSERT_CALLS`). A build whose witnesses differ gets no row.

## Adding a game build

Keep a copy of the executable of every verified build outside the repository before Steam updates the game.

1. Start the game: if the search proves the new build, it plays as "not yet confirmed". Nothing else is needed to play.
2. To confirm it after a real test, run the port tool from the last verified build
   (`tools/addr/port_build.py OLD.exe NEW.exe --out tools/addr/out`, see its README) and commit the report: the reports are the
   data the tests and the next port read. Run `tools/addr/find_sites.py NEW.exe --out tools/addr/out` as the independent check
   of the hold sites, the command kinds and the WinHTTP import list (add any new import to `proxy.rs`).
3. Add the build's line to `verified-builds.txt`, printed by
   `cargo run --release -p feverlink3_native --example verified_line -- NEW.exe` (the module's own search on the executable).
   The launcher reads the same file.
4. When the search no longer proves a new build (the code of a hooked function changed), make a new proof from a build whose
   port report exists: `port_build.py <exe> --export-proof tools/addr/out/proof-<timestamp>.bin`, and point `search::PROOF` at it.

## Size

The module links Rust's `std` for `Vec`, `String`, threads and file logging, and the launcher crate (for the in-game session controller) with `serde_json`. Measured release build with the MSVC toolchain (the reference figure, printed by CI in the job summary): 1 662 464 bytes with `panic = "unwind"`, the shipped setting; with `panic = "abort"` the same code is 1 132 544 bytes, but that build cannot let the game's C++ exceptions through the hooks (`docs/NATIVE_LINK.md` section 10; `docs/SECURITY.md` section 5 has the numbers and the audit). The planned optimisation, once the hook set is known, is `#![no_std]` + `alloc` with a `HeapAlloc`-backed allocator and a panic handler that logs and aborts; the session controller's dependencies would have to move out of the DLL first.

## Status

Running in the real game in every session: the counters, the hold/release hook, the control link and the single-worker script pool (played over Steam between two PCs, and on builds 40423 and 40425 in two game windows on one PC). Since 0.2.7 the build search finds their places at start (seen on 40425 in two game windows: "every site found by search"). The tree, plant and rock carrier (`assets.rs`, 0.2.8) has run only in the lab's two game windows on one PC so far. The acting-player rewrite (0.2.10) has run only in the lab's two game windows on one PC so far. The hold of builds another mod's script sends (site 0, 0.2.9) is the safety net behind the mod's API layer: in the two-window test the API layer held those builds; the native hold itself is not yet seen in the game. Not run in the game: the diagnostic and experimental pieces in the table above (town seed, growth log and growth seed). `docs/NATIVE_LINK.md` section 8 lists the log lines to look for. See `docs/ROADMAP.md`.
