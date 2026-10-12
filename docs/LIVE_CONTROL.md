# Live control: `ctl_cmd.log` and `ctl_status.txt`

The in-game window drives the session through two files in one folder. This page lists every command the window may
write and every field the controller answers with. The code is `launcher/src/live.rs`; the design is
`docs/ARCHITECTURE.md` section 12. **Other pieces add rows to the tables below** when they add a command or a field
(see "Adding a row" at the end).

## The folder and the files

The folder is `%LOCALAPPDATA%\FeverLink3\live`. The controller (the native module's `launcher::live::spawn()` thread,
or a test) clears it when the game starts, then owns these files:

| File | Written by | Read by | Content |
|---|---|---|---|
| `ctl_cmd.log` | the window (the mod's UI script) | the controller | Commands, one per line, append-only. Format below. |
| `ctl_status.txt` | the controller | the window | The controller's whole state as one Lua table, rewritten whenever something in it changed. |
| `identity.txt` | the controller | the mod | `id=`, `name=`, `role=` lines. `role` is `idle` until a session exists, then `host` or `player` with the id the host assigned. The mod becomes active on `host` or `player` without reloading the game. |
| `to_game.log`, `from_game.log` | the controller / the mod | the mod / the controller | The session's file link with the game (`launcher/README.md`, "File link"). Not part of this page. |

The controller reads new commands and rewrites the status about every 100 ms. The window re-reads the status when its
`version` changed (it checks every 0.5 s).

## `ctl_cmd.log`

One command per line, in the file link's line format:

```
seq<TAB>kind<TAB>from<TAB>payload<LF>
```

- `seq` is a number that only grows within the file. The controller remembers the highest it has seen and ignores
  anything at or below it, so a rewritten or truncated file never repeats a command. The mod's writer (`C.newWriter`)
  does this.
- `kind` is the command name below. A kind the controller does not know is ignored.
- `from` is ignored.
- `payload` is one line of **Lua literal syntax**: a table of strings, numbers, booleans and tables, as the mod's
  serialiser writes it (`{["name"]="Pat",["port"]=0}`; `{name="Pat"}` is accepted too). No code runs: names, calls and
  `(0/0)` are refused, and a payload that does not parse counts as `{}`.

### Commands

*State* is the controller state the command is accepted in (`status.state`, below); in any other state it is ignored
without an answer, except `kick` and `permissions`, which always answer in `result`.

| Kind | Payload | State | Effect |
|---|---|---|---|
| `host` | `{transport, name?, port?, password?}` | `idle`, `failed` | Becomes `hosting`. **`transport` must be `"tcp"`** for now, the direct developer transport (same PC or LAN): any other value, or none, fails to `failed` with a message that hosting over Steam is not finished (the Steam transport plugs in here). `name`: display name, trimmed, at most 24 characters, default the status `name`. `port`: TCP port, `0` for any free one, default 28090. `password`: lobby password, empty or absent for none; the join code alone is then not enough, so give the password separately. Fails to `failed` with `message` when the port cannot be bound. |
| `join` | `{code, name?, password?}` | `idle`, `failed` | Becomes `joining` (up to 5 s per address in the code), then `joined`. Fails to `failed` with `message`: not a valid join code, cannot reach the host, or the host's reject (wrong code or password, other version, session full, ...). `password` must equal the host's. |
| `leave` | `{}` | any | Stops hosting, or leaves the session (a player says goodbye first, so the host frees its slot at once), cancels a `join` in progress, clears `result` and `notice`, writes `role=idle`. State `idle`. A host that stops tells every connected player first (reason `ended`), so their windows show "The host ended the session" at once instead of reconnecting. |
| `start` | `{}` | `hosting` | Starts the session: every game is held at the time all of them share, and a checksum round is requested. Speed stays 0 until `speed` or a player's speed request. |
| `pause` | `{}` | `hosting` | Every game stops at one future step. |
| `speed` | `{n}` | `hosting` | Runs the session at speed `n`, 1 to 4 (default 1, out of range clamped). Ignored while a resync round runs. |
| `check` | `{}` | `hosting` | Asks every game for a world checksum now. |
| `resync` | `{}` | `hosting` | Starts a resync round: every game loads the host's save, the host's own game included (set `FEVERLINK3_HOST_RELOAD=0` in the game's environment to keep the host's live world, which drifts). Needs a started session; ignored while a round runs. |
| `kick` | `{id}` | `hosting` | Removes player `id`: it is told "kicked", disconnected, its slot freed, and its launcher does not reconnect. For a player who is `away` it releases the held slot. It may join again with the code like anyone else. Answers in `result`; refuses the host's own id 0, an id that is not in `players`, and a missing `id`. The window's **Remove** button next to each player in the host's Players list sends it and shows `result.message` under the list. |
| `permissions` | `{build?, bulldoze?, spend?, speed?, other?}` | `hosting` | Sets who may do what and tells every game. Each value is `"everyone"`, `"hostOnly"` or `{only = {ids}}`. **Keys not named keep their value**, so a command may carry one key. At least one key is required; a value that is none of the three refuses the whole command (nothing changes) and says which key. Answers in `result`. See below. |
| `steamtest` | `{id, mb?}` | any | Steam connection test: pings the Steam ID (17 digits, as a string) through Steam's networking. The progress is `steam.test`; a bad ID or no Steam shows in `steam.note`. With `mb` (1 to 128; the window offers 8, 32 and 80) it runs the speed test of that many MB each way instead (docs/STEAM_TRANSPORT.md section 7.1); `mode` picks how the data travels: `reliable` (default), `dgram`, `both`, `parallel`, `auto` (the resync's parallel connections, section 7.3) or `all`; the progress and result are `steam.speed`. |

#### `permissions` values

| Key | What it gates |
|---|---|
| `build` | Construction actions (roads, rails, buildings, stations, lines being created). |
| `bulldoze` | Demolition actions. |
| `spend` | Purchases (vehicles). |
| `speed` | Changing the session speed or pausing from the player's own game (a speed request). |
| `other` | Every action that is none of the above: renames, colours, line and vehicle settings. |

| Value | Meaning |
|---|---|
| `"everyone"` | Every player. |
| `"hostOnly"` | Nobody but the host's own game. |
| `{only = {1, 3}}` | These player ids, and the host. `{only = {}}` is the same as `"hostOnly"`. Ids are sorted and repeats dropped; an id nobody has yet is allowed (ids are never reused). |

The host's own game is allowed everything. A forbidden action is still relayed, marked `allowed = false`, so every game
sees the same stream and the player's own game can refuse it and roll back (`docs/PROTOCOL.md`, "Permissions"). A
player's mod gets its own view as booleans: `{build, bulldoze, spend, other}`.

## `ctl_status.txt`

One Lua table literal (`launcher/src/luatext.rs`): keys sorted, `nil` fields left out, lists as `{[1]=a,[2]=b}`. **An
empty list and an empty table are both `{}`**, so test with `next(t) == nil`, not with the type of the JSON it came from.
The file is replaced through a temporary file and a rename; if the window has the file open the controller writes in
place and the next read is right. `version` is a number that goes up by one each time anything else in the table
changed.

### Top level

| Field | Type | Present | Meaning |
|---|---|---|---|
| `version` | number | always | Changes when, and only when, something else in the table changed. |
| `state` | string | always | `idle` (no session), `joining` (connecting, or waiting for the welcome, or reconnecting), `joined` (welcomed by the host), `hosting`, `failed`. The window shows `starting` itself while the file does not exist yet. |
| `name` | string | always | The name the window offers before the player types one (the Windows user name). Does not change during a session; the name the host settled on is in `players`. |
| `steam` | table | always | The Steam networking probe, below. |
| `message` | string | `failed` | Why: what went wrong, in words for the player. |
| `notice` | string | after a session that ended by itself | One line for the window's start screen about how the last session ended: `Your world was closed, so the session ended` (state `idle`, the heartbeat below), `The host ended the session`, `The host removed you from the session`, ... Present in `idle` after a world was closed and in `failed` after a joined session ended (`message` carries the same text in lower case, as before). The window shows it instead of "Could not connect: ..." when present. Cleared by the next `host`, `join` or `leave`. |
| `code` | string | `hosting` | The join code to give to the other players. |
| `endpoints` | list of string | `hosting` | The `ip:port` addresses inside the code: this PC's local addresses (the direct developer transport). |
| `password` | boolean | `hosting` | A lobby password is set. |
| `capacity` | number | `hosting` | Most games in the session, the host's included (8). |
| `me` | number | `hosting`, `joined`, `joining` with a session | This game's player id: 0 for the host, the id the host assigned for a player; absent before the welcome and while reconnecting. |
| `players` | list of table | `hosting`, `joined`, `joining` with a session | Everyone the session knows, by id, the host first, **away players included**. Fields below. Empty before the welcome. |
| `permissions` | table | same | What players may do. On the host the setting it enforces, on a player what the host last announced. Fields below. |
| `started` | boolean | same, once known | The session runs (has been started by the host). |
| `speed` | number | same, once known | 0 while paused or not started, else 1 to 4. |
| `resync` | string | `hosting`, after the first round | State of the last resync round: `Pausing`, `Saving`, `Transferring`, `Loading`, `Verifying`, `Idle` (finished and resumed) or `failed: <reason>`. |
| `reconnecting` | boolean | `joining` after a lost connection | The connection to the host dropped; the launcher is trying again, asking for its old id and slot. |
| `attempts` | number | with `reconnecting` | Attempts so far in this outage. |
| `result` | table | after a `kick` or `permissions` | How the last command that can be refused ended. Fields below. Cleared by `leave` and when a host session starts. |

### `players[i]`

| Field | Type | Meaning |
|---|---|---|
| `id` | number | Player id. 0 is the host's own game. Ids are given out from 1 and never reused in a session. |
| `name` | string | Display name as the host settled on it (a clash gets `#id`). |
| `role` | string | `host` or `player`. |
| `state` | string | `connected`, or `away`: the connection dropped and the host holds the slot for 5.5 minutes for the player to return as itself. |
| `ready` | boolean | The game said hello after its last load. Current on the host; on a player it is the value when that player was announced. |
| `rtt` | number | Round-trip time between the host and this player in milliseconds, smoothed over recent pings (every 2 s). Rounded to 5 ms below 20 ms, 10 ms below 100 ms and 25 ms above, so the file is not rewritten for a millisecond. 0 for the host. Absent until the first answer and while `away`. On a player the list carries every player's value, as the host reports them. |

### `permissions`

`build`, `bulldoze`, `spend`, `speed`, `other`: each `"everyone"`, `"hostOnly"` or `{only = {ids}}`, exactly as the
`permissions` command takes them. All five keys are always present.

### `result`

| Field | Type | Meaning |
|---|---|---|
| `seq` | number | The `seq` of the command line this answers, so a window knows whether the answer is to its latest command. |
| `kind` | string | `kick` or `permissions`. |
| `ok` | boolean | The command was accepted. For `kick` that means the request was passed on; the player disappears from `players` when the host has done it. |
| `message` | string | In words for the host: `Pat was removed`, `the slot of Pat was released`, `permissions sent to everyone`, or why not (`there is no player #9`, `the host cannot be removed`, `only the host can remove a player`, `build: "nobody" is not everyone, hostOnly or {only = ids}`, ...). |

### `steam`

The Steam networking probe behind the connection test; `launcher/src/steamnet.rs`.

| Field | Type | Meaning |
|---|---|---|
| `state` | string | `looking for Steam...` (the controller looks for the Steam API again every 2 s for about 5 minutes when it was not loaded yet), `not available` (it never was), `waiting for Steam`, `ready`, or an error text. |
| `id` | string | This account's Steam ID (digits), once Steam answered. |
| `events` | list of string | The latest events, newest last, at most 8. |
| `pings` | number | Pings other players sent to this account. |
| `note` | string | A message about the last `steamtest` command (bad ID, no Steam). |
| `test` | table | The connection test in progress or done: `target` (Steam ID), `state` (`connecting`, `finding a route`, `connected`, `failed: ...`), `pongs` (answers received), `rtt` (last round trip in ms), `route` (`direct`, `through a Steam relay` or `unknown`). Absent until `steamtest`. |
| `speed` | table | The speed test: `running` (bool), `status` (the progress line, the failure reason, or the headline of the result) and `lines` (the four result lines of a finished test, otherwise empty). A finished or failed test also writes one `steam speed test: ...` line to `session.log`. Absent until a speed test was started. |

## Which fields each state has

| State | Fields besides `version`, `state`, `name`, `steam` (and `result` after a command, `notice` after a session that ended by itself) |
|---|---|
| `idle` | none |
| `joining`, connecting | none |
| `joining`, after the connection or during an outage | `me` (absent while reconnecting), `players`, `permissions`, `started`, `speed`, and `reconnecting`, `attempts` during an outage |
| `joined` | `me`, `players`, `permissions`, `started`, `speed` |
| `hosting` | `code`, `endpoints`, `password`, `capacity`, `me`, `players`, `permissions`, `started`, `speed`, `resync` |
| `failed` | `message` |

A joined session that ends by itself (the host ended it, removed this player or rejected it, or the launcher gave up
reconnecting after 5 minutes) turns into `failed` with a `message` (and a `notice`, above), and the window may `host` or
`join` again at once. `message` for the host stopping or its world being closed is "the host ended the session"; for a
removal "the host removed you from the session"; for a rejection "the host rejected us: <reason>" with the reason
`bad-proof` (wrong code or password), `version` or `full` (with the player limit); for flooding "the host disconnected
you: flood (more than N frames per second)".

`players` keeps a player whose connection dropped for 5.5 minutes, with `state = "away"`: a window should label or hide
those entries, they are not at the table.

## When the world is closed: the heartbeat

A session lives in the world of each game. When a player closes the save to the main menu, the mod's script is gone and
nothing tells the controller, so the controller watches `from_game.log`: the mod appends a `clock` line to it at least
every 30 frames (about half a second) while its script runs, even when the game is paused. A game that **has spoken in
this session** (at least one new line since the session started) and then falls **silent for 8 seconds** has no world
any more, and the controller ends the session by itself:

- a **host** stops hosting. Every connected player is told first (the sealed `Kicked` frame with reason `ended`,
  `docs/PROTOCOL.md`), so each player's controller ends its own session at once: `state` `failed`, `message` "the host
  ended the session", `notice` "The host ended the session". The same happens when the window's `leave` stops hosting.
- a **player** leaves, as with `leave`: the host frees its slot and keeps hosting.
- the controller is `idle` again (`identity.txt` says `role=idle`) with `notice` "Your world was closed, so the session
  ended". The window may `host` or `join` again at once; that clears the notice.

What does **not** end a session:

- a game that has not yet written its first line since the session started (the world may still be loading when the
  host starts hosting; lines of an earlier session in the file do not count);
- the silence of a **resync reload**. A resync load replaces the world and the mod is silent until the new script says
  hello (about 30 s). On a host, a resync round that is running (`resync` is `Pausing`, `Saving`, `Transferring`,
  `Loading` or `Verifying`; the host's game also stalls while it saves) excuses the silence; on a player, the time from
  the host's load command to the game's next hello does (`Status::reloading` in `launcher/src/session.rs`). The silence
  counts again from the end of the round. A round or a load that lasts more than 5 minutes no longer excuses it.

`FEVERLINK3_HEARTBEAT_SECONDS` (environment of the game process, read when the controller starts) changes the 8
seconds, for tests: a number of seconds, `0` turns the check off. `Controller::with_heartbeat` does the same from a test.

The verification is by the tests in `launcher/src/live.rs` (a host and a player controller wired through loopback with
fake games that write lines): a host whose game falls silent ends and its player is told; a player whose game falls
silent leaves; neither ends before the first line; neither ends in a resync round or a load; both end after it.

## Example

A host with one player who is away, after the host restricted building:

```lua
{["capacity"]=8,["code"]="AFVM...",["endpoints"]={[1]="192.168.0.141:28090"},["me"]=0,
 ["name"]="Pat",["password"]=false,
 ["permissions"]={["build"]="hostOnly",["bulldoze"]="everyone",["other"]="everyone",["speed"]="everyone",
                  ["spend"]={["only"]={[1]=1}}},
 ["players"]={[1]={["id"]=0,["name"]="Pat",["ready"]=true,["role"]="host",["rtt"]=0,["state"]="connected"},
              [2]={["id"]=1,["name"]="Bob",["ready"]=false,["role"]="player",["state"]="away"}},
 ["result"]={["kind"]="permissions",["message"]="permissions sent to everyone",["ok"]=true,["seq"]=14},
 ["speed"]=0,["started"]=false,["state"]="hosting",["steam"]={["state"]="not available"},["version"]=21}
```

Removing Bob from the window is one line appended to `ctl_cmd.log`:

```
15	kick	0	{["id"]=1}
```

## Adding a row

1. **A command**: add a match arm to `Controller::command` in `launcher/src/live.rs`. If it can be refused, return a
   `Result<String, String>` and pass it to `Controller::note` so the window gets a `result`; otherwise ignore it
   silently like `start`. Add the row to the Commands table above and a test next to
   `the_status_lists_every_player_with_role_state_and_round_trip_and_kick_removes_one` in the same file (a `Window`
   writes the line, `until` steps the controller, the status is read back).
2. **A field**: add it to `Controller::status_json` (or `add_session` when it describes a running session), keep it a
   plain JSON value (`nil` is dropped), and add the row to the table of the state that has it. Fields that change often
   must be rounded or throttled like `rtt`, or the file is rewritten and the window redrawn at every change.
3. Keep names lower-case words as the existing ones are. Never remove or rename a field the window reads without
   changing `mod/feverlink3_1/content/feverlink3_ui.script.lua` in the same change.
