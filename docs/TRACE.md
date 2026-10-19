# trace.log: what a real run records

`FEVERLINK3_TRACE` is on by default in test builds (`FEVERLINK3_TRACE=0` turns it off). Each game writes `trace.log` into its session
folder (`%LOCALAPPDATA%\FeverLink3\live1` for the host, `live2` for a second window), next to `mod.log`. The file rolls over to
`trace.2.log`, `trace.3.log`, ... every 8 MB of the file, whichever half wrote it (`trace.gen` names the current one; nothing is deleted; after
12 segments it says `TRACE FULL`; the limit holds across reloads). Lines are written per build and
per checksum round, never per tick; the heavy dump is only for builds.

Line format: `<HH:MM:SS> +<cpu seconds> <state> <tag> <text>`; `state` is `gui` (interface half) or `sim` (simulation half) or `build`
(the build module). Every key such as `1:7` is `<player id>:<sequence>`: player 0 is the host. Grep the same key in both games' files.

## Per build

| tag | where | content |
|---|---|---|
| `build.ship` | originator, gui | the build is shipped: key, held command id, tool site, `done` (the native module can fire the tool's callback), tool, plan, stamp, game time, lead, bytes, capture ms, balance, and the whole record (`record={...}`, the serialised record every game receives) |
| `build.record` | every game, sim | the record this game is about to apply: key, `role=replay|originator`, sender, stamp, now, late steps, balance, the whole record |
| `build.plan` | build module | the form of the plan that was built (`form=1`, ...), ms, or the error (`error(type)=...`) |
| `build.factory` | build module | the engine's command factory for that form: `ok` or `THREW`, ms, `ie pi dust`, list lengths of the proposal (constructionsToAdd/Remove, nodes/edges to add/remove, objects, node configs), and the error object's type and text when it threw |
| `build.decode` | sim | decode finished: ok, ms, or the refusal text |
| `build.line` | sim | also in `mod.log` as `build <key> (replay|originator): ...`: plan, counts, adds, removes, resolved removals `n of m [ids]`, the engine's verdict (`critical`, messages, warnings, `collisions=[kind and place]`), `ie`, `pi` |
| `build.send` | sim | what goes to `sendCommand`: `ie pi dust`, the context flags (alignment, cleanup, gather buildings/fields, player), chain length, game time, balance |
| `build.result` | sim (gui for a build applied during a pause) | exactly one line for EVERY build this game applies at a stamp, every kind (street, track, construction, stop, removal, bulldozing), this game's own and the others', refused ones too; also in `mod.log` as `build.result ...`. Format below |
| `build.bind` | sim | a build that keys entities (constructions, stops, depots): `ok=true` with the keyed entities and their component sets (`id{CONSTRUCTION,NAME,...}`) and the key map, or `ok=false` with the new entities that were found and the whole record; the money before and after (was `build.result` before 2026-10-19) |
| `build.refused` / `build.error` | sim | a refusal text, or the error object of `sendCommand` (type and value) |

`build.result` line:

```
build.result <key> role=replay|originator|originator(script) kind=build|bulldoze plan=<plan> verdict=<v> announced=<a> entities[<changes>]
  money <before> -> <after> spent=<before-after> engine_costs=<n|n/a> refund=<r> applied_t=<stamp step> measured_t=<t> [shared=[keys]] [detail=<text>]
```

* `verdict`: `refused(<class>)` (`decode`, `inject_refused`, `company`, `engine`: refused here, nothing sent), `applied` (the world
  changed or the engine announced the build), `unseen` (sent, but nothing changed and the engine announced nothing: refused by the engine
  or emptied by the native module; the interface half's `build.verdict` line of the same key has the module's answer), `native` (this
  game's own build, applied by its tool when it was made; the record only reaches the other games).
* `announced`: `yes` the engine's apply event spent this build's replay mark, `no` it did not, `callback` the engine's callback answered
  (pause path), `n/a` nothing was sent (or a held command, released by the interface half).
* `entities`: what changed in the world between the apply and the next step, per list, at most 12 ids each:
  `+BASE_EDGE[2:50002 50003] -BASE_EDGE[1:110] +CONSTRUCTION[1:50004]`; `none` = nothing changed; `n/a` = not measured. Lists: nodes and
  segments from `streetSystem.getNode2SegmentMap`, CONSTRUCTION, EDGE_OBJECT, TOWN_BUILDING.
* Money: the acting company's account (`api.engine.util.finance.getPlayersBalance`) or this game's player's ACCOUNT component
  (`Account.balance`), read at the apply and at the line. `engine_costs`: `ProposalData.costs` of the replay's own verdict
  (`makeProposalData`, SimpleProposal only). `refund`: `<n>(engine)` when those costs are negative, `<n>(account)` when the account rose,
  else `none`.
* Written at the step after the apply (the engine runs a command the simulation sent after that step), by whichever simulation Lua state
  runs it (everything waits in the game-script state); a held command is measured a step later. Builds of one step share that step's
  changes and money (`shared=[keys]`); a town's own build in that step is in the figures too.

Also in `mod.log`: `action <key>: RESULT DIFFERS from the record after N steps: expected ... found ...` (expected against found counts,
what was found, how many removed buildings are still present, the verdict), `factory threw for <plan> (form i of n): ...`.

## Native events

`native <text>`: every `strict:` line of the interface half (held, released, cancelled, fire-done, applied by the engine, with ids, site,
game time and stamp). The module's own events are in `native_events.log` and `native.log` (`event held`, `event released`,
`event dropped`, `hold: command N: the tool's completion callback was fired ...`).

## Free-id order (native)

The native module (build 40420) writes `idorder_live1.log` / `idorder_live2.log` next to `native.log` in `%LOCALAPPDATA%\FeverLink3`
(the session controller clears the live folders at its start) under the same switch. Per ECS engine (a world has two engine copies), in the
engine's own order: the ids `AddEntity` hands out (`+id`) and the ids that joined the free-id queue (`-id`, read from the queue itself, so in
the order the engine appended them, which is the order they will be handed out again). One line per burst of the engine's work (calls
less than 10 ms apart), only when something happened:

```
idorder e=<engine> g=<burst> n=<ops so far> at=<HH:MM:SS.mmm> ents=<id slots> free=<queue size> r=<removal requests> h=<rolling hash> ops=+12,-9,-7
idorder e=<engine> first seen at=.. ents=.. free=.. fifo=<hash of the whole queue> head=<first ids>
idorder e=<engine> reset at=.. free=.. (was ..)        the queue shrank without an AddEntity: a load
```

A burst is not a simulation step (the native module has no step clock; a game catching up runs several steps in one burst), so compare
the flattened streams: `python -I tools/gametest/freed_id_diff.py` (local tool) prints the first op where the two games differ and whether
only the order of a freed batch differs. The verbose per-call trace with call chains stays `FEVERLINK3_ID_TRACE=1` (`ids_live1.log`).

## Hooks a game thread is inside (native, before the simulation starts)

Always on (two atomic adds per hook call). Until the simulation applies its first command, the service thread writes to `native.log`
every 5 s which function of the module a game thread is inside now (each hook, and the module's own file writes and the id-order lock),
so a game that hangs while it loads names the place (labs L/L2 of 2026-10-19: a game hung loading with the deep growth log on):

```
hookwatch #<n> pid=<process> t=<ms since start> sim=not started: INSIDE <name> <threads> (entered <calls>) | <name> <entered>/<left>, ...
hookwatch #<n> pid=.. t=.. sim=not started: none inside | ...      no thread in the module: the game waits in its own code
hookwatch: pid=<process> the simulation applied its first command by t=<ms>: dumps stop (<n> written)
```

The same name INSIDE in dump after dump, its `entered` not moving, is where the thread is stuck. `native.log` is shared by both games:
`pid=` says which game (as on the `growth` and `gdeep-` lines).

## Checksums and resyncs

* `checksum t=<time> light=<bool> lanes=.. items{lane=count ...} parts={...}`: every round of this game: the hash of every lane, the
  item count of each itemised lane (full rounds), and for light rounds the lanes that were left out.
* `resync saving as <name> ...`: this game saved for a resync; the next line (`detail`) says which file holds the itemised keys of its
  last full checksum round: `detail_<time>_<player>.txt`, one `lane<TAB>key` per line. Sort and diff the two games' files to see which
  edges, constructions, stations, node settings or town buildings differ (ids are never part of a key).
* The host's `session.log` has the cause of each resync (`checksum N differs in ...`, `resync N started: ...`) and the
  versions line: `version player=.. role=.. native_build=.. native_version=.. record_version=..` (`trace.log`, once per start).
