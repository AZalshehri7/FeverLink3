# tools/addr: the address tool

Turns a `TransportFever3.exe` into a `build_table::BuildAddrs` row for `native/src/build_table.rs`. The
executable is read as data and never executed, modified or copied into the repository.

Hook sites located with findings from MPFever (MIT, dkdknight): the per-build RVAs for game builds 40408 and
40420, the prologue bytes and what each hooked function does come from its `native/mpfever_native.cpp`. The
tool itself is a fresh implementation.

## Run

```
python -m venv .venv
.venv\Scripts\pip install -r requirements.txt
.venv\Scripts\python -I find_sites.py "C:\Program Files (x86)\Steam\steamapps\common\Transport Fever 3\TransportFever3.exe" --out out
```

`-I` isolates the interpreter (no user site-packages, no `PYTHON*` variables), which is why the
dependencies go in a virtual environment rather than `pip install --user`. `--out DIR` also writes the
report to `DIR/<timestamp-hex>.txt`; the committed reports live in `out/`. Exit code 0 means every site was
located with medium or high confidence and the WinHTTP import list matches the proxy; 1 means something
needs a look (the report says what).

## What it prints

1. PE `TimeDateStamp` (hex and UTC date), `.text` RVA and size, the version resource (the current builds have
   none) and the game's own build number, read from `.rdata` where it sits next to `transportfever3` and
   `64-bit`.
2. The complete `WINHTTP.dll` import list, compared with the `relay!(...)` lines of `native/src/proxy.rs`. A
   function the game imports that the proxy does not export is flagged loudly: the game would not start with
   the module in place.
3. For each site of `BuildAddrs` (`command_list_add`, `apply_command`, `script_pool`, and the helpers
   `command_move`, `command_destroy`, `handle_destroy`): the RVA, how it was located, the confidence,
   agreement with MPFever's table when the build is in it, the prologue bytes with their capstone
   disassembly, the minimal prologue length that ends on an instruction boundary (at least 14 bytes for a
   detour, at least 16 for a helper) and the detour safety check (OK, or UNSAFE naming the rip-relative or
   relative-branch instruction; not applicable to a patch or to a helper that is only called).
4. The engine layout the hold path relies on: the command-kind byte offset inside a payload (read in two
   functions that must agree) and sizeof(Command).
5. The command dispatcher's jump table: every command kind with the profiler tag its case starts with or the
   first string its handler references (kind 52 is `WorldBuildProposal`), and the check that the kind names
   in the table are each found by exactly one case.
6. Every direct caller of `command_list_add` with the source file and profiler tag its function names, the
   ones on the hold list marked, and the check that the hold list names only direct calls to it.
7. A ready-to-paste `BuildAddrs { .. }` literal with the build name, the credit line and a comment per site.
8. The generic signature derived for each site: the site's bytes with wildcards for branch targets,
   rip-relative and other 32-bit displacements, stack-frame sizes and large immediates, extended instruction
   by instruction until it matches once in `.text`, plus one instruction of margin.

## How a site is located

Three methods, run independently, must agree:

- **Known table.** MPFever's `BUILDS[]` keyed by PE timestamp, verified by the bytes MPFever expects at the
  site (the "seed"). The seed is a plain prologue and matches hundreds of functions, so on its own it only
  confirms a known RVA; it cannot find a site in a new build.
- **Signature.** The generic signature stored in `SITES`, unique in `.text`. A signature derived from the very
  build being analysed is reported as a consistency check, not as evidence.
- **Anchor.** Something the compiler cannot move far: a string the function references (found in the data
  sections, then its rip-relative users in `.text`, then the enclosing function from the `.pdata` unwind
  table, following chained entries), the argument idiom every caller shares (a byte regex ending on the
  call, whose targets are tallied), or, for the helpers, how an already located function uses them (a call
  inside it, or what its callers call next).

| Site | Kind | Anchor |
|---|---|---|
| `command_list_add` | detour | callers' idiom: every UI tool queues its build with `lea rcx; mov [rsp+20h],rcx; lea r9; mov r8,rax; lea rdx; mov rcx,<list>; call Add` (31 of 32 matches call the same function in build 40420) |
| `apply_command` | detour | string `"Simulation Thread: Apply Command"`, passed by `lea rcx` at the function's start |
| `script_pool` | patch | string `"GameScriptSystem Pool"`, copied inline (`vmovups xmm0,[rip+d]`) a few instructions after the site; the seed pattern is then required once inside that function |
| `command_move` | call | the call in `command_list_add` right before `add qword [list+8], 0x38`, which appends the command to the queue vector |
| `command_destroy` | call | the `mov rcx,r15; call X; nop` that `command_list_add` runs twice on its moved-from `cmd` argument |
| `handle_destroy` | call | what every caller of `command_list_add` calls straight after it (`lea rcx,[..]; call X`; 45 of 45 in build 40420) |

Confidence: **high** when the known RVA agrees with an independent method, or two independent methods agree;
**medium** when one independent method finds it (a new build with no table entry); **low** when only the
known RVA and its seed agree; a disagreement between methods is a conflict and the site is reported as not
located. A site below medium is emitted as `None` and the report says so plainly; nothing is guessed.

## The script-pool site is a patch, not a detour

`script_pool` is the `cmp eax,8 / jge <shared pool> / mov [rsp+20h],1 / call hardware_concurrency`
sequence inside the game-script system's constructor, patched in place so the scripts always get their own
pool with one worker. Its bytes contain a relative `jge` and a relative `call`, so the detour safety check
reports UNSAFE for it by design: the bytes are rewritten where they are, never moved to a trampoline. The row
records the exact 22-byte region the patch overwrites so `detour::check_prologue` can verify the build before
anything is written.

## The helpers, the hold list and the kinds

The hold path (`native/src/engine.rs`) moves a command out of `CommandList::Add`'s caller, keeps it, and later
hands it back to the engine. That needs three engine helpers, which are only called, never hooked: the command
move constructor, the command destructor and the destructor of the `out` handle. Each is recorded as a
`CallSite` with a 16-byte whole-instruction prologue that the module compares with memory before it calls the
function. Their addresses come from MPFever and are confirmed here by how `CommandList::Add` uses them.

`ui_add_sites` is the list of calls to `command_list_add` that belong to the interface's construction tools
(bulldozer, street, track, construction, module and lane tools, the street-terminal tool, the entity window's
builtins). Only a command added from one of them may be held: a tool merely destroys the returned handle,
where the mod's own `sendCommand` waits for the result, and the terrain tool does not survive being held. The
list is MPFever's (`UI_SITES_40420`, found by playing with each tool), and the tool checks that every entry is
a direct `call` to `command_list_add`; the report lists all 45 direct callers with the source file their
function names, so a human can see why a caller is on the list or off it. A build with no list keeps the
hold hook off.

`kinds` names the command kinds the control link may hold. The engine applies a command through a jump table
indexed by the kind byte at payload offset `0x9b8` plus one; the tool walks that table and names each case by
the profiler tag it starts with or the first string its handler references. Kind 52 is the one whose handler
references `WorldBuildProposal`; the tool refuses to emit the name if that does not identify exactly one case.

## Results so far

| Build | Timestamp | `command_list_add` | `apply_command` | `script_pool` | Report |
|---|---|---|---|---|---|
| 40420 | `0x6ac50427` (2026-10-06) | `0x9d4040` high | `0x9e33b0` high | `0xab00fc` high | [`out/6ac50427.txt`](out/6ac50427.txt) |

Build 40420 is in MPFever's table; all three RVAs agree with it and with the independent anchors. The WinHTTP
import list of 40420 is exactly the 14 functions `proxy.rs` relays.

Helpers of 40420, all high: `command_move` `0x9d0430`, `command_destroy` `0x9d0670`, `handle_destroy`
`0x3040c90`. Kind byte offset `0x9b8` (two functions agree), sizeof(Command) `0x38`, `build_proposal` is kind
52, and all 12 hold-list entries are direct calls to `command_list_add`. `native/src/build_table.rs` has a
test that parses the committed report and compares its row with the table.

## Adding a build

1. Run the tool against the new executable with `--out out` and commit the report.
2. If every site is high or medium, paste the literal into `build_table::BUILDS`.
3. Paste the derived signatures into `SITES[..].signature` in `find_sites.py` with `signature_from` set to
   the new timestamp, so the build after that can be found by signature as well.
4. If a site is not located: compare the report's notes with the previous build's report, find the function
   by hand (the anchors above are the starting points), and only then extend the tool. Do not type an RVA
   into the table by hand.
