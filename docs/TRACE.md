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
| `build.result` | sim | after the build: `ok=true` with the keyed entities and their component sets (`id{CONSTRUCTION,NAME,...}`) and the key map, or `ok=false` with the new entities that were found and the whole record; the money before and after |
| `build.refused` / `build.error` | sim | a refusal text, or the error object of `sendCommand` (type and value) |

Also in `mod.log`: `action <key>: RESULT DIFFERS from the record after N steps: expected ... found ...` (expected against found counts,
what was found, how many removed buildings are still present, the verdict), `factory threw for <plan> (form i of n): ...`.

## Native events

`native <text>`: every `strict:` line of the interface half (held, released, cancelled, fire-done, applied by the engine, with ids, site,
game time and stamp). The module's own events are in `native_events.log` and `native.log` (`event held`, `event released`,
`event dropped`, `hold: command N: the tool's completion callback was fired ...`).

## Checksums and resyncs

* `checksum t=<time> light=<bool> lanes=.. items{lane=count ...} parts={...}`: every round of this game: the hash of every lane, the
  item count of each itemised lane (full rounds), and for light rounds the lanes that were left out.
* `resync saving as <name> ...`: this game saved for a resync; the next line (`detail`) says which file holds the itemised keys of its
  last full checksum round: `detail_<time>_<player>.txt`, one `lane<TAB>key` per line. Sort and diff the two games' files to see which
  edges, constructions, stations, node settings or town buildings differ (ids are never part of a key).
* The host's `session.log` has the cause of each resync (`checksum N differs in ...`, `resync N started: ...`) and the
  versions line: `version player=.. role=.. native_build=.. native_version=.. record_version=..` (`trace.log`, once per start).
