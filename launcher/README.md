# launcher

The Windows platform layer of the FeverLink3 launcher, as a Rust crate (`feverlink3_launcher`). It is the part of
`docs/ARCHITECTURE.md` section 2 that touches the machine: Steam, the game folder, the user folder, the session
files and the game process. No networking and no UI: the protocol lives in the core crate, the lobby and the
window come in a later stage and plug into this one.

| Module | What it does | Pure part (tested without Steam or the game) |
|---|---|---|
| `steam.rs` | Finds Steam (`HKCU\Software\Valve\Steam\SteamPath`, fallback `HKLM\SOFTWARE\WOW6432Node\Valve\Steam\InstallPath`), reads `steamapps/libraryfolders.vdf`, finds `steamapps/common/Transport Fever 3/TransportFever3.exe`, lists `userdata/<id>/3493540/local/` | tolerant VDF parser, library and path arithmetic |
| `install.rs` | Installs and uninstalls the game-side footprint idempotently and returns a report | everything, run against temporary folders |
| `gamelink.rs` | The file link with one game: session folder, `to_game.log`, `from_game.log` tail, `identity.txt` | line format, sequence tracking, truncation handling |
| `process.rs` | Starts `TransportFever3.exe` with the session environment, reports the pid, says whether a game runs | the `Command` is built and checked without being run |
| `main.rs` | `feverlink3 doctor`, `feverlink3 install [--dry-run]`, `feverlink3 uninstall [--dry-run]` | |

## Install footprint

Section 11 of the architecture promises that the first run installs the game-side pieces and nothing else, and
that one *Uninstall* button removes exactly those. The three pieces:

1. **The native module**, copied into the game folder as `winhttp.dll`. The game imports `WINHTTP.dll` and
   Windows searches the application folder before the system folder, so the game loads it itself at startup.
   The file is only written when no `winhttp.dll` is there or the one there carries our marker string
   (`feverlink3_native`, which the module's PE export directory keeps even after the file is renamed). A
   `winhttp.dll` from another mod is never overwritten and never removed; the report says so and the install
   counts as failed.
2. **The mod**, mirrored into `userdata/<id>/3493540/local/mods/feverlink3_1/` for every Steam user of the PC.
   Mirror means the folder is replaced, so files removed from the mod disappear too; nothing else in `mods/` is
   touched.
3. **`feverlink3_path.txt`** next to `TransportFever3.exe`, holding the launcher's own path (UTF-8, one line),
   so that a game started by Steam can find the launcher.

One more file that Steam, not FeverLink3, needs: **`steam_appid.txt`** in the game folder with the app id
`3493540`, so that `TransportFever3.exe` can be started directly, with the session environment and more than
once on a PC (finding from MPFever, MIT). It is created only when absent, never rewritten, and removed by the
uninstall only when it holds exactly our app id.

No registry writes, no admin rights, nothing outside these four paths. `install` and `uninstall` are idempotent:
a second run reports every step as skipped. Both take `dry_run` and return a `Report` listing each item with
*done*, *skipped* or *failed* and the reason, which `main.rs` prints.

Not done yet: adding the mod to the game's default mod list in `settings.lua` (`activeModsState`). The pure
transformation exists (`install::settings_with_mod_active`) but is not applied until the file's shape is confirmed
against a real `settings.lua` from Transport Fever 3; the game rewrites that file on exit, so it may only be
edited while the game is closed.

## File link

Per session, one folder `%TEMP%\feverlink3\<name>-<unix time>-<8 hex digits>\`:

| File | Writer | Reader |
|---|---|---|
| `to_game.log` | launcher | Lua mod |
| `from_game.log` | Lua mod | launcher |
| `identity.txt` | launcher (`id=`, `name=`, `role=` lines) | Lua mod |
| `native.log` | native module | people |

Each log line is `seq<TAB>kind<TAB>from<TAB>payload\n`. `seq` is strictly increasing within a file; the payload
is one line, with `\`, newline, carriage return and tab escaped as `\\`, `\n`, `\r`, `\t`. The reader remembers
the highest `seq` it dispatched and ignores anything at or below it, so a truncated or rewritten file never
replays history; a file that shrinks is re-read from its start. A partial last line waits for its newline.
`Tail::poll()` returns the new lines; `Tail::spawn()` runs the same on a thread with a callback.

## Game start

`process::start` runs `TransportFever3.exe` with working directory = game folder and the environment
`FEVERLINK3_SESSION_DIR`, `FEVERLINK3_PLAYER_ID`, `FEVERLINK3_PLAYER_NAME`, `FEVERLINK3_ROLE` (`host` or `player`),
and returns a handle with the pid, `is_running()` and `wait()`. `process::any_game_running()` asks `tasklist`
whether any game instance runs, which the install checks before touching `winhttp.dll` (Windows locks it while the
game runs).

## Build

```
cd launcher
cargo test
cargo clippy --all-targets -- -D warnings
cargo run -- doctor
cargo run -- install --dry-run
```

Standalone crate for now (the repository has no Cargo workspace yet; `native/` is standalone too). Dependencies:
`winreg` (registry), `rand` (session folder names), `tempfile` (tests). Versions are held to ones whose Windows
bindings ship prebuilt import libraries, because the `x86_64-pc-windows-gnu` toolchain without MinGW cannot run
`dlltool` for crates that use `raw-dylib` linking (`rand` 0.9+, `tempfile` 3.11+). The MSVC toolchain has no such
limit.

## Status

Platform layer only. Stage 0.2 items this covers: game launch with session environment, file link with sequence
numbers, first-run install and one-button uninstall (library side; the button comes with the UI). The release zip
is expected to hold `mod/feverlink3_1/` and `winhttp.dll` next to the executable; in a source tree `main.rs` finds
them by walking up to the repository root.
