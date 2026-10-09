# mod

Game-script mod `feverlink3_1` for Transport Fever 3. Design: `docs/ARCHITECTURE.md` sections 2, 5, 6, 7 and 8; file link kinds: `docs/PROTOCOL.md`.

## What it does

Nothing, unless the game was started by `FeverLink3.exe`: without the session folder in the environment every entry point returns at once and the game plays stock. With it, the mod:

- holds the game at the save time until the host starts the session, then runs it at the session speed, never more than `ahead` steps past the slowest other player (barrier), and stops at exactly the step the host names for a pause;
- captures the commands a player issues from game windows (vehicle purchase, line creation, vehicle-to-line assignment) through a wrapped `api.cmd.sendCommand`, stamps them `now + lead` rounded up to a step, ships them to the launcher as `action` lines, and applies them on every game, the originator included, at that stamp, in (stamp, sender, sequence) order. The originator's window gets its callback with the real local result once the engine has produced it. A stamp that has already passed is applied at once and logged `LATE`;
- never ships engine entity ids: the player entity travels as `{__ref="player"}`, entities from the savegame as `s:<id>`, entities created during the session as `<sender>:<seq>` of the action that created them, bound on each game when the entity appears. A reference a game cannot resolve refuses the action (`action_refused`) instead of applying it with a wrong id;
- replaces `math.random` in the simulation state with a generator reseeded from the game time each step;
- answers `checksum_request` lines by hashing the world at the named stamp (parts: `time`, `balance`, `loan`, `constructions`, `lines`) and writing a `checksum_report`;
- turns the player's speed buttons into `speed_request` lines; the speed itself comes back from the host in the session state;
- corrects money to the host (`authority` line): every checksum round the game keeps its own balance and loan; the host's report carries its values as `authority`, and when the matching `authority` line comes back a player books the balance difference as a journal entry (`makeJournalBookAssetCmd`) and moves the loan through `adjustLoan` when the build has one. The host's own game is never corrected;
- resyncs by savegame: `resync_save {id, name}` saves the game under that name and writes `save_done {id, name, ok, detail?}`; `resync_load {id, name}` writes the line's sequence number to `resync_cursor.txt`, then loads the named save. The reloaded scripts start the reader after that sequence (nothing is dispatched again), send `hello` again, and the window keeps its roster and chat. An unknown save name writes `replay_failed` and the game carries on;
- enforces permissions: a `permissions` line `{build, bulldoze, spend, other}` (booleans; only an explicit `false` denies, a missing key keeps the grant, everything is allowed until the first line) gates local commands. A command belongs to one category (`makeVehicleBuyCmd` is `spend`, `makeLineCreateCmd` is `build`, `makeVehicleSetLineCmd` is `other`; other names are classed by their wording: bulldoze/remove/demolish, buy/purchase, construct/build/create, else `other`). A denied command is cancelled before it is stamped: nothing is shipped or applied, its callback gets a failed result, and `action_refused {seq = 0, detail}` is written. An `action` line carrying `allowed = false` is logged and not applied, and the game writes `replay_failed` to ask for a resync;
- shows an in-game window (status, player list, chat). `welcome`, `player_joined`, `player_left`, `session` and `chat` lines feed a model of at most 200 chat lines; a typed line becomes a `chat {text}` line.

Files in `feverlink3_1/content/`:

| File | Role |
|---|---|
| `feverlink3.gs.lua` | Game-script registration (`update`, `handleEvent`, `guiUpdate`, `guiHandleEvent`). |
| `feverlink3_common.lua` | Environment, deterministic serialisation, FNV-1a hash, link reader and writer. Pure Lua 5.2. |
| `feverlink3_lockstep.lua` | The lockstep core: session, peers, pacing, stamps, queue, command encoding, deterministic random, checksum parts. Pure functions over state tables. |
| `feverlink3_hud.lua` | The window's model: player roster, bounded chat history (200), status text, and the two file hand-offs below. Pure Lua, tested. |
| `feverlink3_ui.script.lua` | The window itself, drawn with the game's react toolkit. **Engine-bound and untested** (never run in a game). |
| `feverlink3_entry.res.lua` | Registers the window on the game's mod entry point (react-plugin). Untested. |
| `feverlink3_bridge.script.lua` | Glue to the engine: the real `api.*` accessor, the two script halves, the `sendCommand` wrap. Thin. |

## Environment

| Variable | Meaning |
|---|---|
| `FEVERLINK3_SESSION_DIR` | Folder holding the file link. Absent or empty: the mod does nothing. |
| `FEVERLINK3_PLAYER_ID` | This player's numeric sender id (0 = the host's own game). Required. |
| `FEVERLINK3_PLAYER_NAME` | Display name; defaults to `player <id>`. |

## File link

Two append-only text files in the session folder, one record per line, fields separated by tabs:

```
seq <TAB> kind <TAB> from <TAB> payload
```

- `to_game.log`: launcher → mod. `from` is the sender id the launcher attached (0 = host). The mod only accepts session control (`session`, `peerclock`, `checksum_request`, `player_left`, `chat`, `welcome`, `player_joined`, `authority`, `permissions`, `resync_save`, `resync_load`) from 0; `action` lines carry the originating player's id.
- `from_game.log`: mod → launcher. `from` is this player's id. Kinds: `hello`, `clock`, `speed_request`, `action`, `action_refused`, `checksum_report`, `save_done`, `replay_failed`, `chat`.
- `seq` increases by one per line per file. A reader remembers the last sequence it dispatched and never dispatches a lower one, so a truncated or re-written file cannot replay history. The mod's writer continues the sequence from what the file already holds, so a savegame reload does not restart it.
- `payload` is one line of Lua literal syntax written by `feverlink3_common.lua`: keys sorted, integers as `%d`, other numbers as `%.17g`, strings escaped. It is parsed in an empty environment. An `action` payload is `{stamp, seq, body, from}` where `body` is itself a serialised command `{fn, args}`, opaque to the launcher.
- `mod.log` in the same folder is the mod's own log (both script halves append to it).

## Window and the bridge

The window runs in the UI and does not touch the file link; two files in the session folder carry everything. `hud_state.txt` is a versioned snapshot of the roster, status and chat that the bridge rewrites when something changes (also read back after a resync reload). `ui_out.log` holds the window's typed lines as `chat` records with their own sequence; the bridge reads it, sends the `chat` line and adds the message to its own history (the host does not echo a message to its sender).

## Injection points

The bridge builds one `engine` table; a global `FEVERLINK3_ENGINE` table (set by the test harness, or by a build that knows better) overrides any entry: `bookJournal(amount)`, `adjustLoan(delta)` (absent by default: no loan command is known), `saveGame(name, done)`, `loadGame(name)`, plus the reads. The defaults use the `app` object of the interface state and `api.type.JournalEntry`.

## Tests

`tests/lua/test_lockstep.py` runs two complete mocked games (simulation half and interface half in separate Lua environments, as in the engine) through the real mod files with lupa (Lua 5.2), plus a mocked host relay. It checks the hold before start, the barrier with a lagging client, same actions at the same stamps with differing entity ids, refusal of unresolvable references, pauses and actions during a pause, checksum rounds (identical worlds, one differing balance), that truncating `to_game.log` does not re-apply old lines, `LATE` handling, authority correction (balance and loan, next round identical), resync save and load with a mocked `app` (save then `save_done`, load then reload then `hello`, nothing re-dispatched, an unknown save), permissions and `allowed = false`, roster/status/chat through the window's two files including a reload, and the pure parts (serialisation, hash, reader, writer, pacing, stamps, queue order, random, command encoding).

```
pip install lupa
python -I tests/lua/test_lockstep.py
```

The harness is Python because mlua's vendored Lua needs a C compiler, which the Rust GNU toolchain alone does not provide; the mod under test is unchanged by that choice.
