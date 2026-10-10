# launcher

The FeverLink3 launcher as a Rust crate (`feverlink3_launcher`) and the `feverlink3` command. It is the part of
`docs/ARCHITECTURE.md` section 2 that touches the machine: Steam, the game folder, the user folder, the session
files, the game process, and the network. The protocol itself (framing, sealing, state machines) lives in the
core crate; this crate moves bytes between sockets, the state machines and the game's files. Started without
arguments (which is what a double-click does), `feverlink3.exe` shows a console menu, see *Wizard* below.

The crate has two faces. The `feverlink3` executable is the installer (the wizard menu, and `install`, `uninstall`,
`doctor` on the command line) and a development tool (`host`, `join`, `selftest`); it stays a testing tool, because
the end state is the mod coming from the in-game Mod Hub or Nexus Mods with the native module delivered separately
(`docs/WORKSHOP.md`). The same library also runs **inside the game process**: the native module (`winhttp.dll`,
crate `feverlink3_native`) links it and calls [`live::spawn`](src/live.rs), which starts the session controller that
the in-game window drives, without any terminal (see "Live mode" below). The in-game window itself is the Lua mod's
(`mod/feverlink3_1`); this crate has no window of its own.

| Module | What it does | Pure part (tested without Steam or the game) |
|---|---|---|
| `steam.rs` | Finds Steam (`HKCU\Software\Valve\Steam\SteamPath`, fallback `HKLM\SOFTWARE\WOW6432Node\Valve\Steam\InstallPath`), reads `steamapps/libraryfolders.vdf`, finds `steamapps/common/Transport Fever 3/TransportFever3.exe`, lists `userdata/<id>/3493540/local/` | tolerant VDF parser, library and path arithmetic |
| `install.rs` | Installs and uninstalls the game-side footprint idempotently and returns a report | everything, run against temporary folders |
| `gamelink.rs` | The file link with one game: session folder, `to_game.log`, `from_game.log` tail, `identity.txt` | line format, sequence tracking, truncation handling |
| `process.rs` | Starts `TransportFever3.exe` with the session environment, reports the pid, says whether a game runs | the `Command` is built and checked without being run |
| `net.rs` | TCP transport on `std::net` and threads: `HostNet` (listen, accept, one reader thread per connection), `ClientNet` (connect to the endpoints of a join code), `NetEvent`, the LAN addresses for a join code | loopback sockets in the tests |
| `session.rs` | The session thread: `run_host`, `run_player`, `SessionHandle` (`stop()`, `status()`), the line-kind table | kind table, payload translation; end to end in `selftest.rs` |
| `luatext.rs` | JSON payloads of the wire to and from the Lua literal syntax of the file link | everything |
| `resync.rs` | The launcher's half of a resync round: finds the host game's save, hashes and slices it into chunks, writes a received save where the game looks for it, and builds the `permissions` line the mod reads | file and byte handling, run against temporary folders |
| `live.rs` | Live mode: the session controller that runs inside the game process (see "Live mode" below) | the controller is driven through files in the tests, no game needed |
| `steamnet.rs` | Steam peer-to-peer probe: looks up the flat Steam API in the game's already loaded `steam_api64.dll`, listens on a P2P virtual port, answers pings and can ping a Steam ID. It carries no session traffic yet | structure layouts, ping protocol, Steam ID parsing |
| `update.rs` | Update logic over bytes: GitHub release JSON, version comparison, Ed25519 signature and SHA-256 checks, the file-swap plan. No network fetch yet | everything |
| `selftest.rs` | Host, two players, fake games, wrong password and garbage over loopback | it is the test |
| `main.rs` | `feverlink3` (the wizard), `host`, `join`, `selftest`, `doctor`, `install [--dry-run]`, `uninstall [--dry-run]` | option parsing |
| `wizard.rs`, `wizard/` | What a double-click does: the checks screen and the numbered menu (install or repair, uninstall, compare with the installed files, start the game through Steam, open the logs, copy diagnostics). Part of the binary, not of the library. | the checks, what each choice may do, the plain-language messages, the version comparison, the diagnostics text and its redaction; the whole menu loop over a fake PC |
| `build.rs`, `resources/` | The icon, version information and manifest of `feverlink3.exe`, written as a `.res` file in pure Rust and linked by `link.exe` (no Windows SDK tool) | the resource writer, read back in its tests |

## Command line

```
feverlink3                       no arguments (a double-click): checks and menu, see Wizard
feverlink3 host [--port 28090] [--password P] [--name N] [--launch]
feverlink3 join CODE [--password P] [--name N] [--launch]
feverlink3 selftest
feverlink3 doctor | install [--dry-run] | uninstall [--dry-run]
```

- **`host` and `join` are development and test commands, not how players connect.** They run the same session
  code over a plain TCP transport (the developer transport, same PC or same local network) so that the protocol,
  the file link and the resync round can be exercised without the game. Players connect through the in-game window
  over Steam's peer-to-peer networking (`docs/PLAYING.md`); carrying whole sessions over that connection is still
  being wired in, and until then the window's Host button says so (it only starts a TCP host when asked for the
  `tcp` transport by name, which the tests do).
- `host` listens on `0.0.0.0:<port>` (default 28090), prints the join code and the endpoints in it, and keeps
  running. The endpoints are this PC's local-network IPv4 addresses (the interface the OS would use towards the
  internet, plus what the machine name resolves to); nothing is looked up on the internet, so the code is only
  useful on the same PC or the same local network. With `--password` the code alone is not enough: the key is
  stretched from code secret and password (`docs/PROTOCOL.md`, "Key"), so give the password separately.
  While it runs, type `start`, `pause`, `speed N` (1 to 4), `check` (checksum round now), `resync` (start a resync
  round now), `status` or `quit`.
- `join CODE` connects to the endpoints of the code in order (5 s each) and fails at once if none answers. It runs
  until the session ends or `quit`. If the connection drops it reconnects (see below).
- `--name` defaults to the Windows user name. `--password` must equal the host's.
- **The game starts only with `--launch`.** Without it nothing but the session folder is created, and the folder
  is printed so a game can be pointed at it by hand (`FEVERLINK3_SESSION_DIR`, see Game start). With it the
  host starts its game at once as player 0; a joiner waits (up to 15 s) for the host's welcome, because the game
  must be told the id the host assigns.
- `selftest` runs the loopback test below and exits 0 or 1. It never starts the game.
- `doctor` prints what was found (Steam folder, libraries, the game folder, the per-user folders and whether the
  mod is installed in each, the state of `winhttp.dll` and `steam_appid.txt`, the recorded launcher path, where
  the payload to install sits, whether a game is running) and changes nothing.
- `install` and `uninstall` take `--dry-run` and print one `DONE` / `WOULD` / `SKIP` / `FAILED` line per item with
  the reason (see "Install footprint"). Both refuse to run while the game is running, unless `--dry-run`.

## Wizard (double-click)

`feverlink3.exe` without arguments is what a double-click does, so it must never print a line and vanish. It draws a
checks screen and a numbered menu, waits for Enter after every action and returns to the menu until `0`; when its
input ends (a closed pipe) it ends too. The code is the `wizard` module of the binary, [`wizard.rs`](src/wizard.rs)
(the menu loop and the actions) and [`wizard/`](src/wizard) (one module per concern); the library does not contain it,
so it is not in `winhttp.dll`.

**Checks**, from one snapshot of the PC (`Facts`) taken before every screen and every action:

| Line | Where it comes from |
|---|---|
| Steam, Transport Fever 3 | `steam::discover` (registry, library folders), as `doctor` |
| Game build | the PE link timestamp of `TransportFever3.exe`, looked up in a copy of the builds of `native/src/build_table.rs` (`KNOWN_BUILDS`; the native crate links the launcher, so the launcher cannot link it. A test reads `build_table.rs` and fails when the two lists differ). An unknown build means the mod runs in reduced mode, and the line says so |
| Game running | `process::any_game_running` |
| FeverLink3 | `winhttp.dll` ours (marker), the mod in every Steam user folder, and which version put them there (below) |
| This folder | `mod/feverlink3_1` and `winhttp.dll` beside the exe, and not running from inside a zip file (`Temp1_*.zip`, WinRAR, 7-Zip) or `%TEMP%`, where `feverlink3_path.txt` would record a path Windows deletes |

Under the checks, **Next step** says the first thing to do in a sentence. The banner, the diagnostics text and the
file properties of the exe name the license (`CARGO_PKG_LICENSE`, read from `Cargo.toml`, never typed again).

**Menu**

| Key | What it does |
|---|---|
| `1` | Install or repair: `install::install`, after the checks of `plan_install` (Steam, game, a complete folder, game closed, a `winhttp.dll` that can be read and is not another mod's: a locked one stops the install before anything changes). It asks first when this folder is older than what is installed |
| `2` | Uninstall: `install::uninstall`, after a yes/no question (default no) |
| `3` | This folder against the installed files, no network: the **dry run** of `install::install` is the comparison (what it would change is what differs), so the answer and a real install cannot disagree. Says newer, older, same or not installed |
| `4` | `steam.exe -applaunch 3493540` from the Steam folder; refuses when the game already runs |
| `5` | Opens `%LOCALAPPDATA%\FeverLink3` in Explorer, when it exists |
| `6` | Copies a diagnostics text to the clipboard through `clip.exe` (UTF-16 with a byte order mark, so every alphabet survives; `clip.exe` keeps the mark as an invisible first character, so the text starts with a blank line to keep it off the first real line): the checks, the `doctor` text, and the last 40 lines of `native.log` and `live\mod.log`. The Windows user name (and its 8.3 short form, in any case) becomes `<user>`; a join code, which is the credential of a session, is taken out (only a run that decodes as a join code, checksum included). If the clipboard cannot be written the text is shown on the screen |
| `0` | Exit (`q` and `exit` too). Digits typed on an Arabic or full-width keyboard layout count |

**The installed version.** The install writes nothing that says which version it is (the footprint is fixed, see
*Install footprint*). The launcher carries `feverlink3-launcher-version=<version>` in its data, and
`feverlink3_path.txt` names the launcher that installed; if that file still exists, the wizard reads the marker from
it. Otherwise the line says "version not known" and the comparison says only that the files differ. (A launcher on a
network share is not read: with the share gone, Windows can take half a minute to say so.)

**Failures** are explained in plain words (`wizard/problems.rs`): the game is running, Steam or the game is missing,
the folder is incomplete or zipped, another mod's `winhttp.dll` is in the way, files are in use, Windows refused
access. The install report only keeps errors as text, so they are sorted by the `(os error N)` code (32, 33 and 1224
are "in use", 5 is "refused", 112 is "disk full"), which does not change with the language of Windows, then by the
words `install.rs` itself uses.

**How it is tested.** Every decision is a function of `Facts`; the menu loop runs over the `System` trait and
`BufRead`/`Write`, and the tests drive it with scripted lines on a PC made of temporary folders (`wizard/testkit.rs`)
that records what it was asked to start and never starts anything. Install and uninstall run for real, on those
folders. Not tested without a person: that Steam starts the game, that Explorer opens the folder, that `clip.exe`
takes the text, what a console looks like.

**Programs it starts**, all by absolute path: `steam.exe` of the Steam folder, `%SystemRoot%\explorer.exe`,
`%SystemRoot%\System32\clip.exe`, and `cmd.exe /C pause` to wait for a key after a failure (a bare name would be looked
up in the folder the player unzipped into first). A panic (the release profile aborts) prints what happened and waits
for a key before the window closes.

**Icon, version information, manifest.** `build.rs` links `resources/feverlink3.ico`, the version information from
`Cargo.toml` and `resources/feverlink3.manifest` into the exe (Explorer, Task Manager and the file properties then say
what it is; the manifest asks for no administrator rights and declares Windows 10 and 11). The `.res` file is written by
`resources/winres.rs` in pure Rust, because `link.exe` takes a `.res` and converts it with `cvtres.exe`, both part of the
MSVC tools; `rc.exe` of the Windows SDK is not needed. Other targets get nothing, and a problem only warns.
`resources/make_icon.py` redraws the icon (pure Python, run it only to change the picture).

## Threading model

One thread owns each session's state machine; everything else only feeds it events.

```
 accept thread ──Opened──┐                       ┌──> net.send / net.close   (any thread may write: per-connection
 reader thread per conn ─┼─ NetEvent ─> mpsc ─> session thread ──> to_game.log   Mutex<BufWriter>, flushed per frame)
   (FrameCodec::read)    │   Frame, Closed       │ HostSession / PlayerSession
                          └───────────────────────┘ loop: wait up to 20 ms for a NetEvent, then
 SessionHandle (any thread): status(), command(), stop()   drain commands, tail from_game.log, tick every 250 ms
```

- **Host:** `HostNet` runs one accept thread (non-blocking accept, 20 ms poll, so it can stop) and one reader
  thread per connection; connection ids start at 1. `HostSession` is never shared: the session thread calls
  `connection_opened`, `frame_received`, `connection_closed`, `local_game_message` and `tick`, and acts on every
  `HostOutput` in order: frames to `HostNet::send`, `CloseConnection` to `HostNet::close`, `DeliverToLocalGame`
  to `to_game.log`, `Log` to the log, `ChecksumCompleted` to the status.
- **Player:** `ClientNet` runs one reader thread; the session thread feeds `PlayerSession` the same way and acts
  on every `PlayerOutput`. A connection attempt has a generation number so events of a connection that was
  replaced are ignored.
- **Closing.** A write error, a frame `FrameCodec` rejects (length under 12 or over 1 MiB, a stream ending inside
  a frame), a peer that closes, or `close()` all end the connection the same way: the socket is shut down, the
  reader thread ends and sends `Closed` exactly once, last. The state machine then removes the player and
  broadcasts `PlayerLeft`. A bad connection never touches another.
- **Reconnecting (player).** When the connection drops after the player was accepted, the session thread writes a
  `session` line with `started=false` to the game, then tries again every 5 s for 5 minutes with a fresh
  `PlayerSession` (new challenge, new counters); `status().reconnecting` is true meanwhile. The host sees a new
  player and assigns a new id, so a returning player is treated like any other joiner: in a started session the
  host runs the hot-join round of `docs/PROTOCOL.md` for it. Keeping the old id is not done. The reconnect itself is
  tested over loopback (the host restarts on the same port); the round that follows has not been run between two
  real games. A reject is final and not retried. If the 5 minutes pass the
  session ends with `status().error` set. The first connection is not retried: `join` fails at once.
- **Status** (`SessionHandle::status()`): role, name, own id, connected / reconnecting / attempts, reject reason,
  error, players (id, name, ready), clocks (host: time and speed per game; player: peer clocks), session state
  (started, speed, pause stamp, lead), last checksum round (`last_sync`: round, time, differing parts), actions
  relayed, frames refused.

## Kind mapping

The file link carries one line per message: `seq<TAB>kind<TAB>from<TAB>payload`. `kind` is the snake_case of the
protocol kind in `docs/PROTOCOL.md`, with one exception the mod already depends on (`peerclock`). The payload is
the wire payload (JSON, camelCase) rewritten as Lua literal syntax by `luatext.rs`, which is what `mod/` writes
and reads; the mapping of the format is described under *File link* below. Direction is from the launcher's side.

| Line kind | Protocol kind (value) | Direction | `from` column |
|---|---|---|---|
| `welcome` | Welcome (10) | to game (players only) | `0` |
| `session` | Session (11) | to game | `0` |
| `peerclock` | PeerClock (12) | to game | `0` |
| `player_joined` | PlayerJoined (13) | to game | `0` |
| `player_left` | PlayerLeft (14) | to game | `0` |
| `checksum_request` | ChecksumRequest (15) | to game | `0` |
| `authority` | Authority (16) | to game (players) | `0` |
| `resync_save` | ResyncSave (17) | to game (host) | `0` |
| `resync_load` | ResyncLoad (19) | to game | `0` |
| `permissions` | Permissions (20) | to game (rewritten per player, see "Resync and permissions") | `0` |
| `hello` | Hello (30) | from game | ignored |
| `clock` | Clock (31) | from game | ignored |
| `speed_request` | SpeedRequest (32) | from game | ignored |
| `checksum_report` | ChecksumReport (33) | from game | ignored |
| `save_done` | SaveDone (34) | from game | ignored |
| `replay_failed` | ReplayFailed (35) | from game | ignored |
| `action_refused` | ActionRefused (36) | from game | ignored |
| `action` | Action (50) | both | to game: the origin player's id, as the host filled it into the payload; from game: ignored |
| `chat` | Chat (51) | both | to game: `0` (the payload carries `from` and `name`); from game: ignored |

Rules that follow from `docs/PROTOCOL.md` and `mod/README.md`:

- Challenge, Join and Reject (1 to 3) and ResyncChunk (18) never reach the game; the launcher streams savegame
  chunks itself.
- A game can only send the kinds marked "from game" (exactly the player allow-list). A line of any other kind,
  such as `session`, is logged and dropped: a game cannot speak for the host. A payload that is not valid literal
  syntax is logged and dropped.
- The `from` column of a line from the game is not used. The state machine knows who the game is: the host's own
  game is player 0 and trusted like the host; a player's frames are sealed with the id the host assigned, and the
  host checks it against the connection and fills `from` into relayed actions and chat, whatever the payload
  claimed.
- Lines to the game from anyone but the host are only `action` lines, as the mod requires.
- Unknown extra fields in a payload (such as `name` in `hello`) are ignored by the core.

## Loopback self-test

`feverlink3 selftest` and `cargo test -p feverlink3_launcher --test selftest` run the same code
(`src/selftest.rs`): on `127.0.0.1` with an ephemeral port, a host with a fake game thread (it appends to
`from_game.log` and reads `to_game.log` exactly as the mod does) and two players with fake games; the command
prints every check that passed and how long it took. It asserts: both players are welcomed with distinct ids; the host's session state reaches all three
games' files; an action written by player 1's game (claiming to be from player 2) arrives in the host's and
player 2's `to_game.log` with `from` = player 1, intact and not echoed to player 1; a wrong-password player is
rejected with `bad-proof`, and a raw socket with a wrong proof gets the reject and then EOF; a 2 MiB garbage line
on a raw socket closes that connection while the others stay connected and keep relaying. Two more tests in
`tests/selftest.rs` cover the reconnect (host restarts on the same port) and giving up after the window. The game
is never started.

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
   so that a game started by Steam can find the launcher. Today only `feverlink3 doctor` reads it: the in-game
   controller (live mode) does not need the launcher executable, and nothing in the native module reads this file.

One more file that Steam, not FeverLink3, needs: **`steam_appid.txt`** in the game folder with the app id
`3493540`, so that `TransportFever3.exe` can be started directly, with the session environment and more than
once on a PC (finding from MPFever, MIT). It is created only when absent, never rewritten, and removed by the
uninstall only when it holds exactly our app id.

No registry writes, no admin rights, nothing outside these four paths. `install` and `uninstall` are idempotent:
a second run reports every step as skipped. Both take `dry_run` and return a `Report` listing each item with
*done*, *skipped* or *failed* and the reason, which `main.rs` prints. The wizard (below) calls the same two functions,
and uses the dry run to compare a folder with what is installed.

Not done yet: adding the mod to the game's default mod list in `settings.lua` (`activeModsState`). The pure
transformation exists (`install::settings_with_mod_active`) but is not applied until the file's shape is confirmed
against a real `settings.lua` from Transport Fever 3; the game rewrites that file on exit, so it may only be
edited while the game is closed.

## Live mode (inside the game)

`live.rs` is the session controller that runs inside the game, so that a player needs no terminal: the in-game
window drives it. The native module (`winhttp.dll`) calls `live::spawn()` from `DllMain` when the host process is
`TransportFever3.exe` and `FEVERLINK3_SESSION_DIR` is not set (a game started by the `--launch` option of the
command line keeps using that variable instead). `spawn` starts one thread named `feverlink3-live` that steps a
`Controller` every 100 ms and never returns an error to the game: whatever fails there must not stop the game from
starting. The part of the window that works end to end today is the Steam connection test; carrying whole sessions
over Steam is still being wired in (see "Commands").

- **The folder is fixed:** `%LOCALAPPDATA%\FeverLink3\live\`, so the mod finds it without an environment variable.
  Every file in it is deleted when the controller starts, so each game start begins clean. The native module's own
  log is `%LOCALAPPDATA%\FeverLink3\native.log` (the parent folder).
- **`identity.txt`** says who the game is: `role=idle` until a session exists, then `role=host` or `role=player` with
  the id the host assigned. The mod becomes active when the role is `host` or `player`, without reloading the game.
- **Commands.** The window (the Lua mod) appends file-link lines to `ctl_cmd.log`: `host`, `join`, `leave`,
  `start`, `pause`, `speed`, `check`, `resync` and `steamtest`. The payload is a Lua literal table (`host`:
  `{name, transport}`; `join`: `{name, code}`; `speed`: `{n}` clamped to 1 to 4; `steamtest`: `{id}`). `host` and
  `join` are accepted only while idle or after a failure; the other session commands only while hosting. **`host`
  starts a session only with `transport = "tcp"`, the developer transport that the tests use; without it the
  controller answers `failed` with "Hosting over Steam is not finished in this build yet; the Steam connection
  test below works"**, which is what the window's **Host a game** button shows until the Steam transport is wired
  in. A host's join code is made from the machine's local-network addresses and the session secret.
- **Status.** The controller rewrites `ctl_status.txt`, one Lua literal table, whenever something changed (its
  `version` counter moves only then): `state` (`idle`, `hosting`, `joining`, `joined`, `failed`), the join `code`,
  `players`, `started`, `speed`, `resync` progress, a `message` after a failure, and the state of the Steam probe.
- **Sessions.** Hosting and joining call the same `run_host` / `run_player` as the command line, so the protocol,
  the file link and the tests are the same code. The controller finds the `save` folders of the Steam users
  (`steam::discover`) for the resync round. A join runs on its own thread because connecting can take seconds.
- **Steam probe.** `steamnet.rs` is loaded when the process has `steam_api64.dll` loaded; if it is not loaded yet
  at start, the lookup is repeated every 2 s for about five minutes (the window shows "looking for Steam..."), then
  the window reports "not available". It is a connectivity probe only, with no session traffic yet.
- **A panic aborts the game** (the workspace release profile has `panic = "abort"`). The controller code returns
  errors instead of panicking; see the open point in `docs/ARCHITECTURE.md` section 12.

The tests drive a `Controller` through files only (`host_then_join_then_leave_through_files_only` and others), so
they need neither the game nor Steam.

## File link

Per command-line session, one folder `%TEMP%\feverlink3\<name>-<unix time>-<8 hex digits>\`; in live mode the fixed
folder above. The files:

| File | Writer | Reader |
|---|---|---|
| `to_game.log` | launcher | Lua mod |
| `from_game.log` | Lua mod | launcher |
| `identity.txt` | launcher (`id=`, `name=`, `role=` lines) | Lua mod |
| `native.log` | native module (command-line session: in this folder; live mode: in the parent folder) | people |
| `ctl_cmd.log`, `ctl_status.txt` | window / controller (live mode only) | controller / window |
| `mod.log`, `hud_state.txt`, `ui_out.log`, `resync_cursor.txt` | Lua mod | people / the mod itself |

Each log line is `seq<TAB>kind<TAB>from<TAB>payload\n`. `seq` is strictly increasing within a file; the payload
is one line of Lua literal syntax, written verbatim (the mod's serialiser already escapes control characters in
strings, and its reader does not unescape, so the launcher adds no escaping layer; an earlier version of
`gamelink.rs` did and would have corrupted every string containing `"` or `\`). `luatext.rs` converts between
that and the wire's JSON: objects become `{["key"]=value}` with sorted keys, arrays `{[1]=a,[2]=b}`, `null`
fields are left out; going back, a table whose keys are exactly 1..n is an array, any other table an object, and
only literal syntax is accepted (no calls, no names, no `(0/0)`). The reader remembers
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
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo run -p feverlink3_launcher -- selftest
cargo run -p feverlink3_launcher -- doctor
```

Member of the repository's Cargo workspace; the crate builds the `feverlink3` executable and is also linked into
the native module. Dependencies: `feverlink3_core` (protocol), `serde` and `serde_json` (payload translation),
`sha2` and `ed25519-dalek` (hashes and update signatures), `zip` (update archives), `winreg` (registry, read-only),
`rand` (session folder names), `tempfile` (tests). Versions are held to ones whose Windows bindings ship prebuilt
import libraries, because the `x86_64-pc-windows-gnu` toolchain without MinGW cannot run `dlltool` for crates that
use `raw-dylib` linking (`rand` 0.9+, `tempfile` 3.11+). The MSVC toolchain has no such limit.

## Status

As of 2026-10-10, with what has and has not been run in the real game kept apart:

- **Done and tested without the game:** host / join over the development TCP transport with the player allow-list
  and sender-id rewrite, `selftest` over loopback, the player-side reconnect, game launch with the session
  environment, the file link with sequence numbers, install and uninstall of the game-side footprint (run against
  temporary folders), the resync round and savegame transfer (`tests/resync.rs`, a multi-chunk save over loopback),
  permissions, the live-mode controller (driven through files), update verification (`update.rs`), and the release
  zip and manifest (`tools/release`).
- **Run in the real game (2026-10-09/10):** the live-mode controller starts inside the game; on 2026-10-09, with the
  TCP host that the window started then, hosting showed a join code and a second client could join that session
  (the window's Host button no longer starts a TCP host, see "Live mode"); the Steam probe listens and, inside one
  process, reached its own Steam ID (6 pings answered). See `docs/ROADMAP.md` for the dated list.
- **Not verified in the real game:** the savegame transfer and the resync load between two real games, the
  reconnect, permissions in the window, and any connection between two different Steam accounts.
- **Not written yet:** the network fetch of the updater (the signature, hash, version and file-swap logic exist) and
  a real release signing key (the key in the repository is a placeholder whose private half is public, see
  `docs/RELEASING.md`); the guided installer; the Steam connection as the session transport (hosting and joining
  over Steam).

The release zip holds `feverlink3.exe`, `winhttp.dll` and `mod/feverlink3_1/` side by side; in a source tree
`main.rs` finds them by walking up from the executable's folder (`mod/feverlink3_1/`, and `winhttp.dll` or
`target/release/feverlink3_native.dll` in each folder on the way).

## Resync and permissions (launcher side)

`src/resync.rs` is the launcher's half of a resync round; the state machine itself is in `core`.

- **Host:** when the core reports `Transferring`, the launcher looks for `feverlink3-resync-<id>.sav` in the
  `save` folders of the Steam users (`HostConfig::save_dirs`), hashes it by streaming, announces it with
  `resync_file_ready`, then reads it from disk in 256 KiB chunks and passes each to `resync_chunk`; every chunk is
  sealed and written to the sockets before the next is read, so a large save never sits in memory.
- **Player:** `PlayerOutput::SaveReceived` (already verified against the announced SHA-256 by the core) is written
  to every `save` folder in `PlayerConfig::save_dirs` as `feverlink3-resync-<id>.sav`, through a temporary file and
  a rename. The `resync_load` line that follows names that file.
- **Permissions:** the core sends set-based `Permissions` to everyone. Each launcher writes the `permissions` line
  as the mod reads it, for its own player id: `{build, bulldoze, spend, other}` as booleans. The host is always
  allowed. `Command::SetPermissions` changes them from the host's side; the in-game window has no control for it yet.
- Host console: `resync` starts a round by hand.
- Tested end to end over loopback in `tests/resync.rs` (a multi-chunk save arrives byte for byte, the round returns
  to `Idle` with identical checksums; permissions arrive per player).
