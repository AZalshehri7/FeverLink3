# native

The module the game loads itself. Rust, built as `winhttp.dll`.

## Why a proxy, why `winhttp.dll`

Transport Fever 3 imports `WINHTTP.dll`, and Windows looks for it in the game folder before the system folder. A file of that name in the game folder is loaded by the game at startup with no injection, which is the least intrusive way to run code inside the process and the one antivirus tolerates best. Every WinHTTP function is relayed unchanged to the real library in the Windows system folder (`src/proxy.rs`).

## Why Rust

- The pure parts, which are most of the module, are unit-tested on any machine: PE parsing (`pe.rs`), signature scanning (`scan.rs`), detour byte maths (`detour.rs`), the build table (`build_table.rs`).
- Memory safety where it matters most: the terrain-cell codec and the control-file parsing (to come) handle data from other players.
- Small output. Release profile: `opt-level = "z"`, fat LTO, one codegen unit, `panic = "abort"`, stripped.
- The unsafe surface is three places: the relays, `detour::install` and the entry point. Each `unsafe` block has a `SAFETY` comment.

## Build

```
cd native
cargo test                       # pure modules, any host
cargo build --release            # target/release/feverlink3_native.dll -> ship as winhttp.dll
```

Toolchain: stable Rust. Either `x86_64-pc-windows-msvc` (needs the Visual Studio linker) or `x86_64-pc-windows-gnu` (self-contained, used on machines without Visual Studio). CI builds the msvc one.

## Activation

The module is inert unless the launcher started the game with `FEVERLINK3_SESSION_DIR` set. Then it logs to `native.log` in that folder, reads the game's PE timestamp, looks it up in `build_table::BUILDS`, and (later stages) installs the hooks of that build. Unknown build: hooks off, logged, mod runs degraded.

## Adding a game build

1. Run the address tool (`tools/`, to come) against the new `TransportFever3.exe`. It finds the hook sites by signature, extracts the prologue bytes, checks they are whole, position-independent instructions, and prints a `BuildAddrs` row.
2. Paste the row into `build_table::BUILDS`. Nothing else in the module changes.
3. Verify the import table still lists only the WinHTTP functions in `proxy.rs`; add any new one.

## Size

The skeleton links Rust's `std` for `Vec`, `String` and file logging. Measured release build: about 900 KB on the GNU target, which statically links the MinGW C runtime; the MSVC build in CI is the reference figure. The planned optimisation, once the hook set is known, is `#![no_std]` + `alloc` with a `HeapAlloc`-backed allocator and a panic handler that logs and aborts: that removes the runtime and lands in the tens of kilobytes. The pure modules already avoid anything `std`-only, so the switch is mechanical.

## Status

Skeleton: proxy, entry point, logging, PE parsing, scanner, detour maths, empty build table. No hooks yet; see `docs/ROADMAP.md` stage 0.3.
