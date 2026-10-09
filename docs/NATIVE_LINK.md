# Native module and mod: the control link

Status: **design, to be agreed before code.** Nothing here is implemented. It describes what the native module (Rust, `native/`) does inside the game and how it talks to the Lua mod.

## 1. What the native module is for

Lua alone can observe and issue engine commands, but it cannot stop a command the player's tool has already queued. Lockstep needs exactly that for construction tools: the originator's game must not apply a build until its stamp, like every other game. The module therefore does three narrow things, and only inside a FeverLink3 session:

1. **Hold** a command at the point where the engine adds it to its command list, and **release** it later at an exact game time (strict mode, `docs/ARCHITECTURE.md` section 6).
2. **Count** commands as the simulation applies them, so the launcher can compare games (the command audit) and the mod knows when a held command has actually run.
3. **Determinism patch**: make the game-script update use one worker (section 7 of the architecture).

Everything else, including deciding what to hold, stays in the mod. The module contains no session logic and no networking.

## 2. Activation and safety rules

- Inert unless `FEVERLINK3_SESSION_DIR` is set (already implemented).
- Hooks are installed only when the game's PE timestamp is in `build_table::BUILDS`. Unknown build: no hooks, one log line, the mod runs in degraded mode and says so in its window.
- Every byte patch verifies the exact bytes it expects first; a mismatch means no patch for that site and a log line. Detours write padding before the jump (implemented in `detour.rs`).
- Held commands are never lost: on shutdown, on session end, and when the control file disappears, every held command is released immediately in order, so a crash of the launcher cannot freeze a player's tools.
- A command whose kind the module does not recognise is never held.
- Solo play (no session) is bit-for-bit stock behaviour.

## 3. The control link

Two append-only text files in the session folder, same shape as the launcher link (`seq<TAB>kind<TAB>payload`, one line per message, monotonically increasing `seq`, readers keep the last `seq` seen and never re-dispatch):

| File | Writer | Reader |
|---|---|---|
| `native_ctl.log` | the mod | the module |
| `native_events.log` | the module | the mod |

The module polls `native_ctl.log` from the interface thread at most every 20 ms (a cheap `GetFileAttributesEx` size check; the file is only opened when it grew), instead of reading it on every command as earlier designs did.

### Mod to module (`native_ctl.log`)

| Kind | Payload | Meaning |
|---|---|---|
| `hold_on` | `{ "kinds": [..] }` | Start holding commands of these kinds (names from the module's kind table, e.g. `build_proposal`). |
| `hold_off` | `{}` | Stop holding; release everything now, in order. |
| `release` | `{ "id": n, "at": gameTime }` | Release held command `id` when the simulation reaches `at`; `at = 0` means now. |
| `cancel` | `{ "id": n }` | Drop held command `id` (the originator's game was told "refused"). |
| `terrain_apply` | `{ "id": n, "stamp": t }` | Apply the staged terrain edit `id` at `stamp` (stage 0.4). |
| `ping` | `{ "n": n }` | Liveness; the module answers with `pong`. |

### Module to mod (`native_events.log`)

| Kind | Payload | Meaning |
|---|---|---|
| `ready` | `{ "build": "40420", "hooks": [..], "version": ".." }` | Written once at start: which hooks are active. The mod uses it to choose strict or degraded mode. |
| `held` | `{ "id": n, "kind": "build_proposal", "time": gameTime }` | A command was held. The mod learns the proposal content from its own `onPreBuildProposal` capture and matches by order and time. |
| `released` | `{ "id": n, "time": gameTime }` | The command was handed to the engine. |
| `applied` | `{ "id": n, "time": gameTime, "ok": bool }` | The simulation applied it (or refused). |
| `audit` | `{ "counts": { kind: n } }` | Per-kind counts of commands the simulation applied, every 30 s. |
| `pong` | `{ "n": n }` | |
| `error` | `{ "where": "..", "detail": ".." }` | A patch failed or an invariant broke; hooks affected are listed. |

## 4. Hold and release, step by step

1. The player confirms a build. The engine calls `CommandList::Add(list, handle_out, command, on_done, on_progress)` on the interface thread.
2. The detour inspects the command's kind. If it is in the held set and the session is active, it moves the command and its callbacks into a bounded ring (64 entries), returns an empty handle to the caller so the tool believes the build is pending, and appends `held`.
3. The mod, in its next frame, stamps the action, ships it to the others, and writes `release {id, at}` for the originator.
4. When the simulation reaches `at`, the module re-issues the command through the original `Add` (via the trampoline), so the engine runs its normal path, and appends `released`, then `applied` when the simulation reports the result.
5. Peers rebuild the same action from the shipped description at the same `at` through ordinary Lua `sendCommand`; their module holds nothing for it.

Bounds: the ring is bounded (a 65th simultaneous hold is released immediately and logged as `error`); a held command older than 30 s of real time is released automatically. Memory of released entries is freed.

## 5. Threading

`Add` runs on the interface thread and the simulation applies on its own thread, as observed by the earlier project (`finding from MPFever (MIT)`). The module follows the same split: the ring and its counters are owned by the interface thread; the only values the simulation thread touches are `current game time` (a relaxed atomic written by the apply hook) and the per-kind counters (atomics). No locks are taken inside either hook.

## 6. What is verified without the game

- The build table row and every patch's expected bytes against the executable on disk (the address tool).
- Detour byte maths, trampoline layout, prologue checks (`cargo test`).
- The control-file parser, the ring buffer behaviour, ordering and bounds, as pure modules (`cargo test`).

## 7. What needs a game run

Whether the held command's callbacks survive being delayed; whether the tools tolerate an empty handle; the exact engine kind values for each tool; the effect of the one-worker pool patch. Each is a numbered checklist item in `docs/ROADMAP.md` stage 0.3 and is tested with two real games before the stage closes.

## 8. Open questions for review

1. Is a 20 ms poll of the control file acceptable, or should the mod signal through a named event object? (Files first; revisit if latency shows in play.)
2. Which command kinds are held in the first playable: builds only, or also vehicle purchases? (Proposal: builds and bulldoze only; purchases and lines go through Lua `sendCommand` in the mod, which needs no hold.)
3. Should `hold_off` be implicit when `native_ctl.log` has no new line for 10 s? (Proposal: yes, with an `error` event.)
