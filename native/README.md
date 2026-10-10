# native

The module the game loads itself. Rust, built as `winhttp.dll`.

## Why a proxy, why `winhttp.dll`

Transport Fever 3 imports `WINHTTP.dll`, and Windows looks for it in the game folder before the system folder. A file of that name in the game folder is loaded by the game at startup with no injection, which is the least intrusive way to run code inside the process and the one antivirus tolerates best. Every WinHTTP function is relayed unchanged to the real library in the Windows system folder (`src/proxy.rs`).

## Why Rust

- The pure parts, which are most of the module, are unit-tested on any machine: PE parsing (`pe.rs`), signature scanning (`scan.rs`), detour byte maths (`detour.rs`), the build table (`build_table.rs`), the control-file codec (`link.rs`), the hold/release state machine (`hold.rs`), the counters (`counters.rs`) and the install plan (`plan.rs`).
- Memory safety where it matters most: the control-file parsing (and, later, the terrain-cell codec) handle data from outside the process. The reader is bounded and never panics.
- Small output. Release profile: `opt-level = "z"`, fat LTO, one codegen unit, `panic = "abort"`, stripped. A panic inside the game would kill it, so game-side code returns errors.
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

On a known build it still touches nothing in the game's code unless the environment switches a piece on, with exactly the value `1`:

| Environment | Installs |
|---|---|
| `FEVERLINK3_HOOKS=1` | The read-only counters: a hook on the simulation's command application. |
| `FEVERLINK3_HOOKS=1` `FEVERLINK3_HOLD=1` | Also the hold/release hook on the command list (holds nothing until the mod writes `hold_on`). |
| `FEVERLINK3_POOLPATCH=1` | The single-worker script pool patch; independent of the other two. |

Each piece is checked against the bytes in memory first, installed all-or-none from `DllMain`, preceded by a self-test of the detour machinery in the shipped binary, and logged line by line to `native.log`; anything in doubt leaves the game as shipped. `docs/NATIVE_LINK.md` is the design, the control-file protocol and the list of what still needs a running game.

## Adding a game build

1. Run the address tool (`tools/addr/find_sites.py`, see its README) against the new `TransportFever3.exe`. It finds the hook sites and the engine helpers by signature and by anchors (string references, callers' idiom, how `CommandList::Add` uses them), extracts the prologue bytes, checks they are whole, position-independent instructions where a detour needs that, reads the command-kind offset and names the command kinds, and prints a `BuildAddrs` row with a confidence per site.
2. Paste the row into `build_table::BUILDS`. The list of construction-tool call sites (`ui_add_sites`) is not found by the tool in a new build: it is verified from the previous build's list, so a new build starts with the hold hook off until a person has chosen the sites (the report lists every caller with the source file it names).
3. Verify the import table still lists only the WinHTTP functions in `proxy.rs`; add any new one.

## Size

The module links Rust's `std` for `Vec`, `String`, threads and file logging, and the launcher crate (for the in-game session controller) with `serde_json`. Measured release build with the MSVC toolchain (the reference figure, printed by CI in the job summary): 903 168 bytes with `panic = "abort"`. With `panic = "unwind"` the same code is 1 247 232 bytes (+344 064, 38 %), which is why the profile stays on abort; `docs/SECURITY.md` section 5 has the numbers and the audit. The planned optimisation, once the hook set is known, is `#![no_std]` + `alloc` with a `HeapAlloc`-backed allocator and a panic handler that logs and aborts; the session controller's dependencies would have to move out of the DLL first.

## Status

Implemented and tested without the game, off by default, **not yet run in the game**: the counters, the hold/release hook, the control link, the single-worker script pool. Nothing from the build table has been installed in a running game yet; `docs/NATIVE_LINK.md` section 8 is the checklist, with the log lines to look for, in the order to try them. See `docs/ROADMAP.md` stage 0.3.
