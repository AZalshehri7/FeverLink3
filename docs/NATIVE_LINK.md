# Native module and mod: the control link

Status: **implemented and tested without the game; the capture-and-hold hook has been run in two real games for road tools and the bulldozer, the rest has not run in the game.** Everything below that says "verified"
means verified by tests or by the address tool on the executable on disk; what only a running game can confirm is
listed in section 8, with the log lines to look for. The module is off unless an environment variable switches
a piece on (section 2). The mod's half (strict mode, section 7) is written and tested against a fake module, and has been run in two real games for the road tools and the bulldozer (stage 4 of section 8 for the rest).

## 1. What the native module is for

Lua alone can observe and issue engine commands, but it cannot stop a command the player's tool has already queued.
Lockstep needs exactly that for construction tools: the originator's game must not apply a build until its stamp,
like every other game. The module therefore does three narrow things, and only inside a FeverLink3 session:

1. **Hold** a command at the point where the engine adds it to its command list, and **release** it later, when the
   mod says (strict mode, `docs/ARCHITECTURE.md` section 6).
2. **Count** commands as the simulation applies them, so the launcher can compare games (the command audit) and the
   mod knows when a held command has actually run.
3. **Determinism patch**: make the game-script update use one worker (section 7 of the architecture).

Everything else, including deciding what to hold, stays in the mod. The module contains no session logic and no
networking.

## 2. Switches and safety rules

Nothing is installed unless the environment says so, and each piece has its own switch. The values are exactly `1`;
anything else (unset, empty, `0`, `true`) is off. Try them in this order:

| Order | Environment | What it installs | Risk |
|---|---|---|---|
| 1 | `FEVERLINK3_HOOKS=1` | A hook on the simulation's command application (`apply_command`). It counts the commands by kind and calls the original. Read-only. | A detour on one engine function. |
| 2 | `FEVERLINK3_HOOKS=1` `FEVERLINK3_HOLD=1` | Also a hook on `CommandList::Add`. It counts adds by caller and holds nothing until a `hold_on` line arrives. | A detour on the function every command passes. |
| 3 | `FEVERLINK3_POOLPATCH=1` | The single-worker script pool: 22 bytes rewritten in place. Independent of the other two. | A patch inside the game-script system's constructor. |
| 4 | `FEVERLINK3_LOG_TOWN_SEED=1` | A diagnostic hook on the town developer's task-seeding function (`sub_8d4270`, RVA `0x8d4270` in build 40420). It logs one line per call, `town-seed n=<counter> seed=<hex> caller=<rva> a3=<hex> a4=<hex>`, and calls the original with every argument unchanged, returning its result unchanged. Read-only; independent of the other switches (no counters, no control link). Details below. | A detour on one engine function; run in the game on 2026-10-11 (two runs, `research/TOWN_GROWTH_FINDINGS.md`). Writes to `native.log` on the thread that calls the function (see below). |

`FEVERLINK3_HOLD=1` without `FEVERLINK3_HOOKS=1` does nothing and says so. The switches are read once, at start.
The module itself stays inert unless a session folder exists (`FEVERLINK3_SESSION_DIR` from the launcher, or the
in-game session controller's folder), as before.

### The town-seed log (`FEVERLINK3_LOG_TOWN_SEED=1`)

Purpose: compare two real games. The hook records the random seed with which Transport Fever 3 seeds the task of
its town developer, so two logs can be diffed to see whether the seeds, the order of the calls and the call site
agree.

What static analysis of build 40420 (PE timestamp `0x6ac50427`) found, offsets are RVAs:

- `0x8d4270` allocates a 0x1408-byte task object (`mov ecx,0x1408 ; call 0x318c530`), stores its arguments in it,
  and seeds a `std::mt19937` in the object: `mov edx,[rsp+0x90]`, `mov [rbx+0x84],edx`, then the 624-word fill loop
  at `0x8d4360` (multiplier `0x6c078965`) and `mov dword [rbx+0x80],0x270`. After the prologue (`push rdi,r14,r15 ;
  sub rsp,0x30`) `[rsp+0x90]` is `[rsp+0x48]` at entry, so **the seed is the ninth argument**, a 32-bit integer on
  the stack. Arguments 1 to 4 are `rcx` (an output slot the function fills and returns in `rax`), `rdx`, `r8d`, `r9d`.
  The function returns its first argument.
- It has one caller, `0x3b9d4e` inside `0x3b9ae0` (return address RVA `0x3b9d53`). There the ninth argument is
  `[rsp+0x40]`, loaded from `r11d`, which `0x3b9ae0` read from its own fourth parameter (`mov [rsp+0x20],r9d` at
  entry, `mov r11d,[rsp+0xd8]` at `0x3b9bdd`). `0x3b9ae0` is called from `0x3b93e7` and `0x3b9fbc`, both passing that
  parameter along. Where the value is first produced (a `rand()` result, a counter, a saved value) was **not**
  followed further up: the hook logs what the task is actually seeded with, which is what matters for comparing.
- Confidence: high that argument 9 (low 32 bits) is the `mt19937` seed, from the disassembly of the callee and of
  its only caller. Not established: what `a3`/`a4` (the `r8d`/`r9d` arguments, `lea r8d,[r12+1]`, `lea r9d,[r13+1]`
  at the call) mean; they are logged raw as candidates for a town or cell index. The second argument is a pointer
  (`[rsi+0x30]` of the caller); the hook does not read behind it, so no town entity id or game time is logged.
- The start-up `srand(_time64(0))` (`~0x81a61` to `0x81b3c`, which the game logs itself as `Srand seeded with {}`)
  is not hooked: it runs before this module can be sure of its place and nothing in this hook can read it without a
  second detour. Use the game's own log line for it.

### Forcing the seed (`FEVERLINK3_FORCE_TOWN_SEED=<number>`), an experiment

**This is an experiment switch.** It has been run in two real games with the same value (`0x2cd15ea0`) in both: constructions stayed identical for about 13 checksum rounds, then differed (`research/TOWN_GROWTH_FINDINGS.md`); it has not been shown to hold over time, and it changes only the seed, no game mechanic. The value is decimal or `0x` hex and must fit 32
bits; anything else is ignored with a log line (`hooks: FEVERLINK3_FORCE_TOWN_SEED: ignored, ...`) and changes nothing.
Either this switch or `FEVERLINK3_LOG_TOWN_SEED=1` installs the hook, with the same fail-open rules (unknown build,
prologue mismatch, failed self-test: nothing installed, the reason logged). No other hook is affected.

How: the ninth argument is a stack slot (`[rsp+0x48]` at the original's entry). The hook is an ordinary function that
receives all twelve slots as parameters and calls the trampoline with twelve arguments, so the compiler writes a fresh
outgoing slot nine; the hook passes the forced value (zero-extended to 64 bits, the original reads only the low 32)
there and the other eleven slots as received. A test on the fake twelve-argument function proves that the original
reads the forced value from `[rsp+0x48]`. The log line gains a field: `town-seed n=.. seed=<original> forced=<value>
caller=.. a3=.. a4=..`.

Why this may not be enough: the seed only initialises the `mt19937` once, at the first load (the function is called
twice per process with the same seed, and a resync load never calls it again). After that the generator state
advances with every draw, and games whose draws differ in number or timing (a resync that loads a save in one game
only) leave equal seeds with unequal generators. So even if forcing works, step 2 will be a reset of the `mt19937`
state (the 624-word array and the index at `+0x80`/`+0x84` of the task object) to a common value at every sync point. That analysis is in progress (`research/TOWN_RNG_STATE.md`, in another branch).

The log lines: `town-seed n=<counter> seed=<hex> caller=<rva> a3=<hex> a4=<hex>`, in `native.log` (location in
`native/README.md`). `n` counts the calls of this run from 1; `seed` is 10 characters (`0x` and 8 hex digits);
`caller` is the RVA of the return address (so it does not move with the load address), or `abs:0x..` if the return
address lies outside the image. To compare two games, keep only the first four fields:

```
grep '^town-seed' native.log | cut -d' ' -f1-4 > game1.txt     (and the same for the second game)
diff game1.txt game2.txt
```

Safety: the hook is installed like the others (build known, prologue bytes at `0x8d4270` equal to the table,
start-up self-test passed, all-or-none, in `DllMain`) and with no other switch it installs nothing else, starts no
service thread and opens no control link. It reads only its own arguments, and calls the original with the twelve
argument slots as received (four in registers, eight on the stack; floats travel as raw 64-bit slots). Unlike the
other hooks it **does write a file line on the calling thread** (one `native.log` append per call), which breaks the
"no file I/O on the simulation thread" rule below on purpose: it is a diagnostic to be switched on for a comparison,
not left on. `native.log` stops growing at 2 MiB (about 25 000 of these lines), so the log can be cut short in a long
session. The site is not found by the address tool yet; the row of the build table was written by hand from this
analysis and from the bytes of the executable. **Not run in the game**: tested only on fake game functions (a fake
nine-argument function whose result the hook leaves unchanged, and the bytes restored after uninstalling).

Rules the code keeps:

- **Fail-open.** Any doubt means nothing is installed and the reason is logged: an unknown build; a site the table
  lacks; bytes in memory that differ from the table (each piece is compared with what is really there); a
  start-up self-test that fails; a jump that cannot be written. The game then runs as shipped.
- **Installed in `DllMain`, never later.** The process is still starting, so no thread is inside the functions
  being patched; the module never rewrites game code again after that. A misbehaving mod is handled by the hooks
  passing everything through (holding off, silence), not by uninstalling.
- **Detours are written all or none.** Every detour is prepared (prologue verified, trampoline built) before any
  jump is written; the trampolines are published to the runtime before the first jump exists; if writing any jump
  fails, the jumps already written are taken out again, newest first. The padding is written before the jump.
  Memory protection is lifted region by region and put back to each region's own earlier value.
- **A self-test runs first, in the shipped binary.** The module hooks a small function of its own the same way and
  checks arguments (one on the stack), the result, the trampoline and the identified call site. The shipped DLL is
  built for size with link-time optimisation, which `cargo test` does not exercise; if the self-test fails, the hooks
  that depend on it are not installed.
- **Nothing the module does can hang the game.** The hooks never wait for a lock (a busy state means "call the
  original"), never do file I/O on the simulation thread, and the module is pinned in memory so a hook never points
  into an unloaded DLL. Held commands cannot be held forever (section 4).
- **Panics and the game's exceptions.** The workspace builds with `panic = "unwind"` (section 10: with `abort` a C++
  exception of the game passing through a hook ends the process). The hooks' own bookkeeping runs inside
  `log::guarded`, which turns a panic in it into a log line and a call of the original; the calls into the game are
  never inside it. The game-side code returns errors; it indexes only fixed arrays by bounded values and reads foreign
  memory only after asking the OS whether it is readable.
- **Unknown kinds are never held.** A command is held only if its kind is in the set the mod named, the build table
  knows the kind, *and* it was added from one of the construction tools' call sites (section 4).
- **Solo play is stock behaviour.** With holding off, the hooks count and call the original.

## 3. The control link

Two append-only text files in the session folder, in exactly the shape of the launcher link
(`docs/ARCHITECTURE.md` section 2): one record per line, `seq<TAB>kind<TAB>from<TAB>payload`, the payload one line
of Lua literal syntax as `C.serialize` writes it, `seq` strictly increasing, a reader keeping the last `seq` it
dispatched and never dispatching a lower one. Because the shape is the same, the mod needs no new code to use it:
`C.newWriter(path, id)` / `C.writerSend` for `native_ctl.log`, `C.newReader` / `C.readerPoll` / `C.parse` for
`native_events.log`. (The first draft of this document sketched JSON with three columns; the implementation reuses
the launcher link's format instead, which `tests/lua/test_native_link.py` and `native/src/link.rs` pin from both sides.)

| File | Writer | Reader | `from` column |
|---|---|---|---|
| `native_ctl.log` | the mod | the module | the mod's player id |
| `native_events.log` | the module | the mod | `native` |

The folder is the session folder: `FEVERLINK3_SESSION_DIR` when the launcher started the game, otherwise the live
folder `%LOCALAPPDATA%\FeverLink3\live`. In live mode the in-process controller clears that folder when it starts;
the module waits for the controller's fresh `identity.txt` before it creates its events file, so the first events
are not deleted.

Reading is bounded and tolerant: a line longer than 8 KiB is thrown away, at most 64 KiB are taken per poll,
a partial last line waits for its newline, a truncated or rewritten file is read again from its top with the `seq`
filter keeping history out, and a file that disappears releases everything (below). **When the link opens, whatever
`native_ctl.log` already holds is history and is not acted on**, so lines from an earlier run can never hold or
release anything; a mod must write its first line after it has seen `ready`.

### Mod to module (`native_ctl.log`)

| Kind | Payload | Meaning |
|---|---|---|
| `hold_on` | `{kinds = {"build_proposal"}}` | Start holding commands of these kinds (names from the build's kind table; 1 to 16 names). Unknown names are reported as an `error` and ignored. |
| `hold_off` | `{}` | Stop holding; every held command is released at the next `Add`, in order. |
| `release` | `{id = n, at = gameTime}` | Release held command `n`. `at = 0` (or missing) means at the next `Add`. See "game time" below. |
| `cancel` | `{id = n}` | Destroy held command `n` instead of releasing it. |
| `terrain_apply` | `{id = n, stamp = t}` | Reserved for stage 0.4. Parsed, answered with an `error`, otherwise ignored. |
| `ping` | `{n = k}` | Liveness; answered with `pong`, and counts as a sign of life. |

Every valid line proves the mod is alive. A malformed or unknown line is answered with an `error` event and does not
count.

### Module to mod (`native_events.log`)

| Kind | Payload | Meaning |
|---|---|---|
| `ready` | `{build, version, hooks = {..}, poolpatch, clock, kinds = {name = byte}, sites}` | Written once, when the link opens: what is installed. `clock` is false in every build so far (below). The mod chooses strict or degraded mode from `hooks`. |
| `holding` | `{on, kinds = {..}, reason}` | Holding was switched on (`hold_on`) or off (`hold_off`, `silence`, `control_gone`). *(Added to the first draft: the mod learns of an implicit switch-off.)* |
| `held` | `{id, kind, site, ms}` | A command was captured. `site` is the RVA of the tool's call, `ms` the module's uptime. The mod matches it to its own capture by order and time. |
| `released` | `{id, reason, ms}` | The command was handed back to the engine. `reason`: `release`, `hold_off`, `silence`, `timeout`, `control_gone`. |
| `dropped` | `{id, reason}` | The command was destroyed (`cancel`). *(Added.)* |
| `applied` | `{id, ok}` | The simulation applied a command the module had released; `ok` is the engine's verdict byte. |
| `audit` | `{counts = {[kind] = n}, total, unreadable, adds = {tool, other, unknown}, sites = {[i] = n}}` | Cumulative counts of commands the simulation applied by kind byte, and of commands added to the list by where they came from; `sites` counts per construction-tool call site. Written at most every 30 s and only if the counts changed. |
| `pong` | `{n}` | |
| `error` | `{where, detail}` | A problem the mod should know of. At most 6 per 10 s; the next one says how many were held back. |

The module writes the same events, one line each (not `pong`, not the audit's numbers), to its own `native.log` in a
form for people (section 8).

## 4. Hold and release, step by step

1. The player confirms a build. A construction tool calls `CommandList::Add(list, out, cmd, done, progress)` on the
   interface thread.
2. The hook finds out who called: the return address, which it asks the OS to unwind to. It is looked up in the build
   table's list of **construction-tool call sites** (`ui_add_sites`: bulldozer, street, track, construction, module and
   lane tools, the street-terminal tool and the entity window's builtins; 12 in build 40420). It reads the command's
   kind byte. If holding is on, the kind is in the held set, the caller is a tool site and the ring has room, the
   command is **held**: the engine's own move constructor moves it, the `std::function` callback and the progress
   pointer move with it, and the caller is left an empty handle, which it destroys as it would any handle. The hook
   returns without queueing anything. The module reports `held`.
3. The mod, in its next frame, stamps the action and ships it to the others. At the stamp it writes `release`
   for the originator and then **sends any harmless command through the normal path** (a script event such as
   `O.event("feverlink3", "feverlink3", "nop", {})`). That command is the trigger: see below.
   (This is the design of the first draft. The mod as built does not release the originator's command at the stamp: it
   writes `cancel`, and the originator applies the build at the stamp like every other game, section 7.)
4. The next call to `Add`, any caller, finds the `release` line (the hook looks at the control file itself: a
   metadata call, and a read only if the file grew), takes the entries that are due out of the ring and **hands them
   back to the engine through the unhooked `Add`**, with the destructors a tool would run after its own call. Only
   then does the call that arrived continue. The module reports `released`, and `applied` when the simulation's hook
   sees the same command (by its payload pointer) and the engine accepted or refused it.
5. Peers rebuild the same action from the shipped description at the same stamp through ordinary Lua `sendCommand`;
   their module holds nothing for it, because the mod's own commands do not come from a tool site.

**Why tool sites and not only kinds.** Several callers add `WorldBuildProposal` commands, and not all of them
survive a delay: the mod's own `sendCommand` waits for the result through its callback, and the terrain tool crashed
when its edit was held (both findings from MPFever, MIT). So holding is by kind *and* by call site, and the list
in the build table is the one MPFever proved in play, with the terrain tool left out. The address tool checks that
every entry is a direct call to `CommandList::Add` and names the source file each caller's function mentions.

**Why a trigger.** `Add` is the only function that runs on the thread the engine queues commands on, so the module
hands commands back from there and nowhere else (a second thread calling `Add` could race the engine's queue). The
cost is that a release waits for the next `Add`. The mod sends one itself, as above, and if `released` does not
come it sends another.

**Game time.** The design said `release` waits "until the simulation reaches `at`". The module cannot read the game
time (the offset of the clock inside the simulation is not known; `ready` says `clock = false`), so a scheduled
release is due at the next `Add` whatever `at` says. That is the right reading for a mod that has already stopped
the game at the stamp, which is how MPFever's mod works: every game pauses at the stamp, the originator releases,
the others replay. The scheduling by `(at, id)` is implemented and tested, so a build whose clock is found only
needs a reader function.

**Order.** Entries leave in `(at, id)` order; a `hold_off` sends everything back in id order. One consequence to
design around: only commands of the held kinds are delayed, so a tool that issues a held command and then a command
of another kind in the same click gets them applied in the opposite order.

**Bounds, and why nothing can be held forever.**

- The ring holds 64 commands. The 65th passes through untouched and an `error` is reported.
- **Silence.** After 10 s without a valid line from the mod, holding switches itself off (an `error` and a `holding`
  with `reason = silence`) and every held command goes back at the next `Add`. A mod that holds must write something
  at least every 5 s; a `ping` is enough.
- **Age.** A command held for 30 s is released on its own (`reason = timeout`), whatever the mod says.
- **Control file gone.** If `native_ctl.log` disappears, holding switches off and everything goes back.
- If nothing calls `Add` for a long time the commands stay in the ring, which blocks nothing: the game runs, the
  tool just has not finished. At the player's next action everything goes back first. At process exit nothing is
  released (the engine is going away).

## 5. Threading

| Where | What it may do |
|---|---|
| The `Add` hook (interface thread) | The only place the engine is touched: capture, release, discard. Takes the state with a bounded `try_lock` and calls the original at once if it is busy. Looks at the control file itself when it has work (a metadata call, a read only if the file grew) and does no other file I/O. |
| The apply hook (simulation thread) | Relaxed atomics only: the per-kind counter, the watch for released commands and a lock-free queue of verdicts. No lock, no file, no allocation. |
| The service thread (`feverlink3-native`, every 20 ms) | Reads the control file, advances the state machine's time (silence, age), writes the events and the audit. Never calls the engine. |

The ring is owned by the state machine behind one mutex; the counters and the watch are lock-free. The earlier draft
said "no locks in either hook"; the `Add` hook uses a lock it never waits for.

## 6. What is verified without the game

- **The build table row against the executable on disk** (address tool, committed report
  `tools/addr/out/6ac50427.txt`): the three hook sites, the three engine helpers (found by how `CommandList::Add`
  uses them), the command-kind byte offset (read in two functions that agree), sizeof(Command), the 12 tool call
  sites (each a direct call to `CommandList::Add`), and kind 52 as `WorldBuildProposal` (the dispatcher case whose
  handler names that string). A test compares the table with the report.
- **The pure parts** (`cargo test`): the control-file codec and reader, the hold/release state machine (including a
  randomised run that checks no command is lost, repeated or overfills the ring), the counters and the audit, the
  watch for released commands (with a stress test that found and fixed a slot-reuse race), the install plan and the
  switches.
- **The mechanics on fake game functions** in executable memory (machine code written by hand, called through
  function pointers, Rust functions as hooks): detours that preserve a stack argument and a frame set up by the
  stolen bytes; identification of the call site of a detoured function; uninstall byte for byte; a write across
  pages with different protections; refusal of changed, missing or unreadable memory; the in-place patch and its
  restore; the whole installation, its rollback after a failure in the middle, and `attach()` itself.
- **The engine handling** against a fake engine and a fake `std::function` with the same layout: capture, release,
  discard, null and empty arguments.
- **The runtime** against that fake engine and real files: hold, release in order, filters by caller and kind,
  `hold_off`, silence, cancel, vanished file, full ring, a busy state, the applied verdict, bad lines.
- **The link text**, from both sides, against the mod's own Lua helpers (`tests/lua/test_native_link.py`).

## 7. The mod's side of the contract

**Built and tested without the game. The first design (cancel, replay on the originator too) ran in two real games and left the
originator's tool dead after one build; the redesign below (hold until the stamp, then release on the originator) has NOT
run in the game.** The code is the "strict mode" block of the bridge's interface half
(`mod/feverlink3_1/content/feverlink3_bridge.script.lua`: `natTick`, `natCapture`, `natDecide`, `natStop`) and
`M.captureToolBuild` of the build module (`feverlink3_cmds_build.lua`). `tests/lua/test_strict_build.py` runs two mocked games
against a fake module that honours the files and rules of sections 3 and 4 (`hold_on` / `hold_off` / `release` / `cancel` /
`ping`, `ready` / `holding` / `held` / `released` / `dropped` / `pong` / `error`, release and cancel only at the next `Add`, the
10 s silence rule).

**The design (changed 2026-10-11).** The construction tool's command is held until the stamp. At the stamp every OTHER game
replays the build from the shipped record through `simApplyAction`, and the ORIGINATING game RELEASES its own held command,
so the engine applies the tool's own command and the tool gets the "applied" callback it waits for. The first design
(TpF2 Multiplayer, MIT) cancelled the command and had the originator replay the build too, which keeps the two games in the
same game step, but the first real two-game run (`docs/research/run-2026-10-11-strict-tool-dead`, logs in
`%LOCALAPPDATA%\FeverLink3\live1` and `live2`) showed the price: after the cancel the originating game's tool was dead (no
preview, clicks did nothing, the tool stayed highlighted, and no `tool event` line was ever logged again on that game, the host
included, after it built once). The build itself landed on both games every time (`action 1:2 makeWorldBuildProposalCmd applied
at t=1304000 ... created 1 CONSTRUCTION entities, keyed`). `proposalApply` never fired for a cancelled build, only
`proposalPrepareForApply`; a cancelled command is destroyed together with its callback, so the tool's state machine never
finishes. The cost of the redesign: the originator's engine applies the tool's own command a little after the stamp (one
interface frame to see the result, one trigger, one tick) instead of inside the same simulation step as the others, and it
applies the tool's command rather than the one rebuilt from the description. Stage 4 measures that skew.

Sequence on the originating game (all of it in the bridge):

1. `natDecide` ships the action with a stamp and leaves the command held. The item is marked with a local `heldId` (never on
   the wire) and queued for the originator's own simulation half; the simulation half is told (`heldout`).
2. At the stamp, `simApplyAction` treats the item like a peer does (decode, company check, ledger, the snapshot of the entity
   lists, binders and watches that key what the build creates) but sends nothing, and appends a result `<key>#release<id>`.
3. The interface half sees the result (`collectFromSim`) and calls `natRelease`: replay-mark event, `release {id}`, trigger. The
   mark is first so that the build's `onPreBuildProposal` finds it and the simulation half does not capture the originator's own
   released build as a new one.
4. A game paused at the stamp (`L.applyNow`) goes through `guiApplyNow`, which calls `natPauseRelease` instead of sending the
   command, and finishes the ledger and the keys when the module reports `applied`.

### Switching it on

Nothing happens until `native_events.log` has a record (the module off, or not in this build: no file, no line written, no log
line; the mod looks for the file four times a second). Then:

1. The records are read from the top of the file. **The first batch after a script start is history**: only `ready` (and so the
   hooks) is taken from it; old `held`, `released`, `dropped`, `pong` and `holding` lines are never acted on. This is how a
   reloaded mod learns that the module is ready without a `ready` of its own: the events file is append-only and keeps the
   module's `ready`. No separate memo file is needed; a file that holds no `ready` at all (trimmed by hand) still works, the mod
   only requires `holding on` to answer its `hold_on` (below).
2. `ready.hooks` must contain `command_list_add`; if not, strict mode stays off and `mod.log` says so once.
3. The mod writes `ping {n}` and waits for a `pong` that arrived after the start of this script (an answer in the history proves
   nothing). Six pings 2.5 s apart without an answer: it stops asking until the module writes another `ready` (a file left by a
   crashed run).
4. `hold_on {kinds = {"build_proposal"}}`. The `holding {on = true}` event must follow within 3 s, else strict mode is given up
   (`hold_off`, one log line) until the next `ready`.
5. While holding, a line is written at least every 2 s (a `ping`; the module's limit is 10 s), and the `pong`s are watched: 8 s
   without one means the module is gone, and strict mode switches itself off.

### One build with a native tool

1. The tool's PREVIEW event `builder.proposalCreate` reaches `guiHandleEvent` every time the proposal under the cursor changes.
   If `hold_on` has been written, `M.captureToolBuild` encodes it and keeps only the newest (an encoding that failed replaces
   a good older one, so a `held` is never paired with an earlier preview than the one on screen). **Why the preview and not
   `builder.proposalApply`:** the first real test (2026-10-10) showed that the apply event only comes after the engine applied
   the build, so for a held command it arrives after the release and every capture was too late; and a tool is blocked while its
   command is held, so no newer preview can come after the click: the newest preview is the proposal that was clicked. The
   preview is used for one `held` only. Two commands held in the same batch of events are both released (one preview cannot
   tell them apart). A preview older than 30 s is not used.
2. `held {id, kind, site, ms}` arrives. The oldest capture is paired with it. If `held` is read first (the tool event is
   dispatched later in the frame), it waits for a capture for 0.4 s and 3 frames; a capture that then turns up for a `held`
   that was already released is dropped, so that it cannot be paired with the next build.
3. With a capture: the player's permission is checked (a build the host forbids is cancelled with `action_refused`, like the
   wrapped `sendCommand`), then **this game checks that it could rebuild the record itself** (never cancel what cannot be
   rebuilt, `docs/ARCHITECTURE.md` section 6), then the action is shipped exactly as a Lua command is
   (`send("action", {stamp, seq, body, category})`, stamp past the fastest clock), nothing is written for the command (it stays
   held), and the item is queued locally with `route(it)` and `heldId` set, so that at the stamp the simulation half books and
   keys it and the interface half releases the command (the design above). `body.native` is not set, and `recordOriginator` is
   not called: the normal apply path books the company ledger, with the same inputs as on the other games.
4. Without a usable capture, or when the record cannot be rebuilt here, or the action cannot be sent: `release {id}` at once.
   The tool's command goes through natively and the simulation half ships it the old way (replay on peers); this is the
   unchanged "not replicated" fallback. A held command is shipped and released at its stamp, or released at once and shipped by
   the old path, or cancelled (no permission, or refused at its stamp), never two of these; a capture is never shipped without
   its `held`.
5. A `release` or `cancel` is only carried out at the next `Add` (section 4): the mod sends a harmless script event
   (`feverlink3`, `nop`, ignored by the simulation half) at once, and again after 0.6 s, 1.2 s and so on, at most 5 times, until
   `released` or `dropped` comes back.

### Switching it off (`hold_off`, and a command to make the module run its queue)

- before a resync load (`H.resync_load`): a reload while commands are held is not safe;
- when the session ends: `identity.txt` says `role=idle` (read once a second);
- when the mod is not active, on a resync load in progress, on three faults, when `hold_on` was not confirmed, when the module
  stops answering;
- when the module switches holding off itself (`holding {on = false}` with a reason other than `hold_off`: silence, control
  file gone): strict mode notes it and asks again after 3 s, then 6 s, then 9 s; the fourth time it gives up until the next
  `ready`.

After a resync reload the new script starts at the top of the events file, finds `ready` and does steps 3 and 4 again by
itself. The same happens when a session starts again after `idle`. Every call is guarded; a fault is logged (at most 12 lines per
10 s) and the third one switches strict mode off for good. Failing open always means: the held command is released (or
nothing was held) and the old path ships the build.

### What `mod.log` says (all lines start with `strict:`)

```
native module found (build 40420, hooks apply_command,command_list_add)
mode ON (the native module holds build commands; hold_on confirmed)
held 3 (site 0x5891b4) matched the capture of streetBuilder (15241 bytes, 12 ms to capture, 31 ms old): 1:7 stamped 21600 (now 20400), held until the stamp
held 3 (1:7) released at game time 21600 (stamp 21600); the engine applies it here, the other games replay it at the stamp
held 3 (1:7) applied by the engine (accepted true) at game time 21800, stamp 21600
held 3 (1:7) was refused at its stamp: cancelled
held 3 (1:7) was released by the module (<reason>) before its stamp: the build is in this world early and is not shipped twice
held 3 (1:7) goes back to the engine before its stamp (<why strict mode stopped>)
held 4 (site 0x...): no capture of a tool build; release written, the native build goes through
held 4 (site 0x...): the capture of streetBuilder failed (<why>); release written, the native build goes through
held 4 (site 0x...): it cannot be rebuilt here (<why>); release written, the native build goes through
held 4: not allowed to build, cancelled
capture of streetBuilder dropped: no held command within 2 s (the native build went through, the simulation half ships it)
capture of streetBuilder dropped: it came after its held command had been released
mode off (<why>), hold_off written
mode is off: <the module has no command_list_add hook | switched holding off (silence) ... | hold_on was not confirmed ...>
```

### Known limits

- **A tool that issues two held commands for one click** (a depot against a road is two commands in the build record, but the
  tool may add them one after the other) and fires one event: the second `held` has no capture and is released, so it is built
  natively at once, before the first one is replayed at its stamp. Stage 4, check 5, looks for it.
- **The module gives a held command back before its stamp** (10 s silence, the 30 s age limit, `hold_off`, the control file
  gone), or strict mode stops while one is held (resync load, session end, a fault): the build enters the originator's world
  early. Worlds stay equal except for that, nothing is shipped twice (see below), and the log says so. The game would have to
  stall for seconds with the pings quiet for this to happen; the pings (every 2 s) keep the silence rule quiet.
- **How an early give-back is kept from being shipped twice.** A stop the mod causes writes the replay mark before `hold_off`
  (commands reach the engine in that order). For a give-back it did not cause the mark would come too late (the build was
  applied before the `released` line was read), so the simulation half keeps a list of shipped-but-held builds (`heldout`,
  removed by `heldgone` when the mod released or cancelled itself, 35 s of wall time, never across a reload) and the capture of
  the next native build with no replay mark is swallowed against it.
- **The skew** between the originator's apply and the others' (above). If it turns out to matter, the release could be written
  a little before the stamp; the module cannot read the game clock, so that is a guess to tune in the game.
- **A refusal at the stamp** cancels the held command; that tool may then stay dead as in the first design. It needs a build
  that was accepted when it was captured and refused a second later, which the other games would refuse too.
- **A command in the ring at the moment of a reload** is released by `hold_off` at the next `Add` and then the world is replaced;
  never tested in the game.
- **A held command and a paused session.** Items stamped past a pause wait for it; the tool stays blocked, up to the module's
  30 s age limit (then the early give-back above).
- **Echoed captures.** An earlier analysis found the replay guard (`markReplay`/`spendReplay`) leaking in some cases ('native
  build 1:14 captured' right after 'action 0:6 applied'). The cause was not found in the code or in the logs that were kept
  (they hold no `mod.log` lines of such a case); what is certain is that a mark is a count with a 12-step expiry and that
  every replay of a chain of commands adds one mark per command, so a command whose engine event does not come leaves a
  mark that swallows a later real build, and an event that comes late (after 12 steps) is captured. The originator's own
  released build no longer relies on that guard alone: its mark is written immediately before the release, in command order.

## 8. NEEDS GAME VERIFICATION

Everything below needs a running game. Try the stages in order; stop at the first surprise and read the logs. The
module's log is `native.log` (next to the session folder in a launcher session, `%LOCALAPPDATA%\FeverLink3\native.log`
in live mode); the events are in `native_events.log` in the session folder. To drive the control file by hand without
the mod (live mode shown; numbers must grow, start at 1):

```powershell
function ctl($seq, $kind, $payload) {
  Add-Content -Encoding ascii "$env:LOCALAPPDATA\FeverLink3\live\native_ctl.log" ("{0}`t{1}`t0`t{2}" -f $seq, $kind, $payload)
}
ctl 1 hold_on '{["kinds"]={[1]="build_proposal"}}'
ctl 2 release '{["id"]=1}'
ctl 3 hold_off '{}'
```

### Stage 1: the counters (`FEVERLINK3_HOOKS=1`)

Expected in `native.log`, in this order:

```
hooks: switches hooks=on hold=off poolpatch=off
hooks: self-test: detour, trampoline and stack argument work in this binary
hooks: self-test: the hook identifies the call site of a detoured function
hooks: apply_command: 14 bytes at rva 0x9e33b0 match the build table
hooks: apply_command: jump written, original reachable at 0x...
hooks: installed (jumps in place; they pass everything through until the mod switches holding on)
link: opened ... (control file from seq 0, events from seq 1)
event ready: build 40420, hooks [apply_command], poolpatch off, game clock no
```

Then play: build a road, buy a vehicle, press the speed buttons, wait 30 s. Expect
`audit: N command(s) applied, kind 17 x.., kind 42 x.., kind 52 (build_proposal) x..; added by tools 0, others 0, unknown 0`.
The kind names are in `tools/addr/out/6ac50427.txt` (17 `GameSetSpeed`, 42 `VehicleBuy`, 52 `WorldBuildProposal`, 27
`ScriptingSendEvent`, ...). Questions this answers: does the game start and run normally with a detour on the
apply function; is the kind byte offset right (`unreadable` stays 0, counts are plausible); does building a road
count as kind 52. If the game does not start or crashes, the `native.log` ends at the last line before the crash.
Any `hooks: SKIPPED` line says why a piece was left out (a changed prologue means another build).

### Stage 2: hold and release (`FEVERLINK3_HOOKS=1 FEVERLINK3_HOLD=1`)

Additionally in `native.log`: `hooks: command_list_add: 18 bytes at rva 0x9d4040 match the build table`,
`hooks: command_list_add: jump written, ...`, and the `ready` event lists `[apply_command, command_list_add]`.

- **A. Call sites.** Without any `hold_on`, build with the street, track, bulldozer, station and lane tools and wait
  for the audit: `added by tools N ... tool site i xK`. The index `i` is the position in the table's list: 0
  bulldozer, 1 construction builder, 2 an unnamed tool, 3 to 5 lane modifier, 6 module builder, 7 street builder,
  8 street-terminal tool, 9 track modifier, 10 and 11 entity window. Each tool should land on its own index. A tool
  that lands on `others` is not in the list.
- **B. Hold.** Write `hold_on`. Expect `event holding on (hold_on) [build_proposal]`. Build one road segment: nothing
  is built, the game stays responsive, and `event held: command 1, kind 52, from tool site 0x5891b4` appears. This
  answers: do the tools tolerate an empty handle and a callback that does not fire; does the street builder wait
  or hang.
- **C. Release.** Write `release` for the id, then do anything that issues a command; building another piece
  always does (the new command is itself held, but the one that is due goes back first). Expect `event released: command 1 (release)`, the
  road appears, and `event applied: command 1, accepted by the engine: true` within a second. No `applied` within
  60 s gives `event error [applied]: command 1 was released but never seen being applied`: the payload pointer is
  not stable between `Add` and `Apply`, and the verdict must come another way.
- **D. Silence.** Hold a command, write nothing for 10 s. Expect `event error [hold]: silence: holding is off, 1 held
  command(s) go back to the engine` and `event holding off (silence) []`; the next action builds the held road first.
- **E. Cancel.** `cancel` instead of `release`: `event dropped: command 1 (cancel)`, nothing is built, the tool is
  usable afterwards (the callback was destroyed, not called, and a tool might wait for it).
- **F. Each tool.** Hold and release with every tool of the list, one at a time, once with two commands held
  (released in id order). A crash here is the most likely failure and names the tool; remove its site from
  `ui_add_sites` in the address tool's list and rerun the tool.
- **G. Not held.** The terrain tool and a vehicle purchase must not be held with `hold_on` as above.
- **H. Reload while held.** Hold a command and load a savegame: the command's list pointer is stale. Expected to be
  unsafe; the mod must `hold_off` first (an item for the mod's resync code).

### Stage 3: the pool patch (`FEVERLINK3_POOLPATCH=1`)

Expect:

```
hooks: switches hooks=off hold=off poolpatch=on
hooks: script_pool: 22 bytes at rva 0xab00fc match the build table
hooks: script_pool: applied, 22 bytes at rva 0xab00fc, page protection restored
hooks: installed (patch only)
link: opened ... (control file from seq 0, events from seq 1)
event ready: build 40420, hooks [], poolpatch applied, game clock no
```

Questions: does the game start and the scripts run (towns, industries); is the pool really one worker (two games on
a CPU with 8 or more threads stay in step over a long run, which MPFever reported for this patch); the CPU load of
the game-script update.

### Stage 4: strict mode end to end (`FEVERLINK3_HOOKS=1 FEVERLINK3_HOLD=1` on both games, a session of two games)

Needs the mod's strict mode (section 7) and a host and a player in one session, each game with both switches. Read `mod.log`
(`%LOCALAPPDATA%\FeverLink3\live\mod.log`, lines marked `strict:`), `native.log`, `native_events.log` and `native_ctl.log` on
**the building game**, and `mod.log` on the other. Stages 2 A to H must have passed first: this stage builds on them.

1. **Switching on.** After the session starts, expect in `mod.log` `strict: native module found (build 40420, hooks
   apply_command,command_list_add)` and `strict: mode ON ...`; in `native_ctl.log` a `ping`, a `hold_on`, then a `ping` about
   every 2 s; in `native_events.log` `pong` and `holding` (`on = true`). Without the module (a game without the switches):
   no `native_ctl.log`, no `strict:` line, and builds behave as before (replay on peers).
2. **One road.** On the player's game build one road segment with the street tool. Expect, in this order, on the building
   game: `tool event streetBuilder/builder.proposalCreate`, `strict: held 1 (site 0x...) matched the capture of streetBuilder (N
   bytes, ...): 1:K stamped T (now t), held until the stamp`; in `native.log` `event held: command 1 ...`; then, at T,
   `strict: held 1 (1:K) released at game time ... (stamp T)` and in `native.log` `event released: command 1 (release)` and
   `event applied: command 1, accepted by the engine: true`; no `dropped`. The road must appear on **both** games, on the
   building game a moment after T (note the two game times: `released at game time` and `applied by the engine ... at game
   time`; the skew to the other game's `action 1:K ... applied at t=T`), and on the other game at T (`action 1:K
   makeWorldBuildProposalCmd applied at t=T` in the simulation lines of both `mod.log`s), and **not before** on the building
   game (about the session lead, one to two seconds, after the click). The money leaves the account at T on both. Questions this
   answers:
   - the order of the tool event and `held`: if `mod.log` says `no capture of a tool build` followed by `it came after its held
     command had been released`, the event arrives later than the 0.4 s grace and the grace (`NAT_GRACE`) is too short;
   - whether `builder.proposalApply` is fired before the command is queued at all (if no event ever appears while `held` does,
     every build is released and goes the old way: `strict: held N ...: no capture of a tool build`);
   - whether the release needs the mod's `nop` trigger (`native.log` shows `released` within a frame or two of the
     `released at game time` line; if it takes seconds the trigger is not reaching `Add`);
   - no `native build ... captured` line may follow on the building game (the replay mark worked), and no second road.
3. **The tool stays alive.** Right after step 2, build a second road with the same tool, then a third; then **three roads
   and two depots in a row** on each game (the check that failed in the first real run). After each one a `tool event
   ...builder.proposalCreate` line must appear again on that game, the preview and ghost must follow the cursor, clicks must
   build, and the tool must not stay highlighted without effect. Both worlds identical at the end (roads, depots, and the
   checksum rounds including `companies`); `binds.txt` on the building game must list the keys of its own depots
   (`1:K#c1/d1`) like the other game's. Try the bulldozer and the track tool the same way. A tool still dead here means the
   released command does not give the tool its callback either, and the redesign is wrong.
4. **Each tool.** One build per tool of the list in section 8, stage 2 F (street, track, station, depot, bulldozer, upgrade,
   lane, traffic light). Each should end as in step 2 or, if the capture or the rebuild check refuses, as `strict: held N (site
   ...): the capture of ... failed (...)` or `it cannot be rebuilt here (...)` followed by `release written`, the road built by
   the tool at once and shipped the old way (`native build captured` in the simulation lines). Stops and signals are expected to
   take the second form (`an edge object has no model`, `docs/COVERAGE_BUILD.md` section 8). Write down which tool is which
   `site`.
5. **Two commands for one click.** A road depot against a street (two commands in one record) and a station on a road. Look for
   two `held` lines for one capture: the second shows `no capture of a tool build`, is released and builds natively before the
   first is replayed; the two games then differ. If this happens the pairing needs a rule for tools that add several commands.
6. **Both players at once.** Both build different roads in the same second. Both roads appear on both games, no
   `LATE action`, and the next checksum rounds in the launcher agree (including `companies`).
7. **Resync.** Trigger a resync from the window. Expect in `native_ctl.log` a `hold_off` before the load, and after the reload
   the new script's `ping`, `hold_on`, and `strict: mode ON` again. Build a road after it: as in step 2. A crash or hang at the
   reload with commands held would show here (the likeliest sequence is a build in the second before the resync).
8. **End of the session.** Leave the session in the window. Expect `strict: mode off (the session ended), hold_off written`, no
   more lines in `native_ctl.log`, and plain native builds afterwards. Host again: the mode comes back by itself.
9. **Refusals.** Take the build permission from the player (`docs/PROTOCOL.md`) and build: `strict: held N: not allowed to
   build, cancelled`, nothing built anywhere, and the tool is usable afterwards. A build the player cannot pay for: the tool
   itself refuses it before the click, or the replay is refused at the stamp on every game alike (`action ... refused`).
10. **The module's own switch-off.** Pause the building game's process for 12 s (a debugger, or a long save). Expect
    `strict: mode is off: the native module switched holding off (silence); trying again in 3 s` and then `mode ON` again, and
    any command held at that moment built natively (the log line about a release after shipping, if one was).
11. **Pings.** Over a 5-minute play with no builds, `native_ctl.log` grows by one `ping` line every 2 s and the module never
    logs `silence`. The file grows without bound (about 45 KB an hour); note whether that matters.

### Not testable offline, for completeness

- `Command::Command(Command&&)`, `~Command()` and the handle destructor behave as their code reads (the move
  semantics of the fake engine are those of the disassembly, not the engine).
- `_Move` is slot 1 of the `std::function` implementation's vtable (slot 4, `_Delete_this`, is confirmed by
  `CommandList::Add` itself).
- Calling the unhooked `Add` from inside another `Add`'s hook, with a command taken from an earlier call.
- That `Add` is only ever called on one thread while holding is on. If the engine adds commands from two threads, a
  release could overlap another thread's `Add`; the counters (`added by ...`) and the `held` events carry no thread id,
  so look for crashes at release first.

## 9. Open questions from the first draft, with the recommended default

1. **Is a 20 ms poll of the control file acceptable, or should the mod signal through a named event object?**
   Files, as built. The service thread polls every 20 ms and the `Add` hook checks the file itself when it has
   work, so a release needs no wait for the poll. Revisit only if latency shows in play; the release is bound to the
   next `Add` anyway, which a named event could not change.
2. **Which command kinds are held in the first playable: builds only, or also vehicle purchases?** Builds and
   bulldoze only: kind 52 (`WorldBuildProposal`, which bulldozing also issues) from the 12 tool call sites.
   Purchases (kind 42) and lines (24 to 26, 47) go through Lua `sendCommand`, which waits for results and so cannot
   be held, and need no hold. The terrain tool stays unheld until its edit can be copied.
3. **Should `hold_off` be implicit when `native_ctl.log` has no new line for 10 s?** Yes, as built, with an `error`
   and a `holding` event. A hold that depends on a mod that has stopped talking is the one way this module could
   freeze a player's tools, so silence switches it off, and the 30 s age limit and the vanished-file rule back that
   up.

## 10. The crash of 2026-10-11: a game exception through a hook, and why the build unwinds

**What happened.** In a real two-game test the host process died right after it replayed the guest's second depot
build (`makeWorldBuildProposalCmd` applied, `event apply_command/onPreBuildProposal`, a `withStreet` build of 1 + 0
constructions, 2 + 1 nodes, 3 + 2 segments). `native.log` ended with `PANIC in thread 'unnamed' at
library\core\src\panicking.rs:225: panic in a function that cannot unwind`, the Application log with module
`WINHTTP.dll` (our DLL), exception `0xc0000409`, fault offset `0xb7bf5`.

**Where.** The saved crashing DLL and PDB (built from `fc78393`) put offset `0xb7bf5` in `std::panicking::panic_with_hook`
at its final `int 0x29` (fast fail), which is what every panic ends in under `panic = "abort"` after the panic hook
has run. So `0xb7bf5` is the abort, not the cause. The message is the cause: `core::panicking::panic_cannot_unwind` has
no caller in a `panic = "abort"` DLL except the terminate pads that rustc puts around calls into other languages.
There were exactly two in our code, in the hook path: the closure `pass`/`call` that called the original `Add` and the
original apply in `Runtime::on_add` / `on_apply`, and `engine::release` (which calls the original `Add` to hand back a
held command). There was no earlier panic: the panic hook ran once, for the terminate. (The log is shared by two games,
but a single `PANIC` line followed by nothing is the whole story of that process.)

**Why.** The game's `Add` and apply functions are C++ and can throw (a Lua error in a command handler, for one; the
last event was the Lua `onPreBuildProposal` hook of the replayed build). A C++ exception unwinding through Rust frames
compiled with `panic = "abort"` is not allowed to pass: such a frame has a terminate pad for it. This holds for
`extern "system-unwind"` functions too, which the hooks already were (the earlier comment "declared as one that may
unwind" was true of the ABI but not of the build). Earlier runs did not crash only because the game did not throw
there.

**The fix.**
- `[profile.release] panic = "unwind"` (root `Cargo.toml`). The DLL grows from 1 132 544 to 1 662 464 bytes (+530 kB,
  47 %). `python tools/dll_unwind_check.py DLL PDB` lists the terminate pads of a build: with unwinding none is left in
  `on_add`, `on_apply`, `release` or the hooks (only in `DllMain` and the WinHTTP relays, where no exception can come).
- The hook bodies call the originals directly from `system-unwind` functions (`Runtime::on_add`, `on_apply`,
  `engine::release`, no closures), so the exception passes through.
- The bookkeeping in the hooks (counting, the hold machine, the watch, the town-seed log, the service thread's pump)
  runs inside `log::guarded`: a panic there becomes one log line and the original is called, so our code can no longer
  end the game. The calls into the game are never inside `guarded`, because `catch_unwind` aborts on a foreign exception.
  A command already taken out of the engine's hands when the bookkeeping fails is given back to the engine.
- The panic hook (`log::install_panic_hook`) no longer takes the log's lock (it reads the path from an atomic), is safe
  against re-entry, numbers the panics (`PANIC #1` is the original cause) and writes a backtrace as `dll+0xRVA`
  addresses of the DLL, resolvable with `tools/dll_unwind_check.py DLL PDB --rva 0x...`.

**What to check in the next real run.** `native.log` has `PANIC #n` lines only if something is wrong; a line
`<site>: a panic was contained` means a bug in our bookkeeping that the game survived (report the line above it). A
game exception through a hook now shows up as nothing at all from us. If the process still dies with
`0xc0000409`, map the offset with the tool first.
