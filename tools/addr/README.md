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
3. For each site of `BuildAddrs` (`command_list_add`, `apply_command`, `script_pool`): the RVA, how it was
   located, the confidence, agreement with MPFever's table when the build is in it, the prologue bytes with
   their capstone disassembly, the minimal prologue length of at least 14 bytes that ends on an instruction
   boundary, and the detour safety check (OK, or UNSAFE naming the rip-relative or relative-branch
   instruction).
4. A ready-to-paste `BuildAddrs { .. }` literal with the build name, the credit line and a comment per site.
5. The generic signature derived for each site: the site's bytes with wildcards for branch targets,
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
  table, following chained entries), or the argument idiom every caller shares (a byte regex ending on the
  call, whose targets are tallied).

| Site | Kind | Anchor |
|---|---|---|
| `command_list_add` | detour | callers' idiom: every UI tool queues its build with `lea rcx; mov [rsp+20h],rcx; lea r9; mov r8,rax; lea rdx; mov rcx,<list>; call Add` (31 of 32 matches call the same function in build 40420) |
| `apply_command` | detour | string `"Simulation Thread: Apply Command"`, passed by `lea rcx` at the function's start |
| `script_pool` | patch | string `"GameScriptSystem Pool"`, copied inline (`vmovups xmm0,[rip+d]`) a few instructions after the site; the seed pattern is then required once inside that function |

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

## Results so far

| Build | Timestamp | `command_list_add` | `apply_command` | `script_pool` | Report |
|---|---|---|---|---|---|
| 40420 | `0x6ac50427` (2026-10-06) | `0x9d4040` high | `0x9e33b0` high | `0xab00fc` high | [`out/6ac50427.txt`](out/6ac50427.txt) |

Build 40420 is in MPFever's table; all three RVAs agree with it and with the independent anchors. The WinHTTP
import list of 40420 is exactly the 14 functions `proxy.rs` relays.

## Adding a build

1. Run the tool against the new executable with `--out out` and commit the report.
2. If every site is high or medium, paste the literal into `build_table::BUILDS`.
3. Paste the derived signatures into `SITES[..].signature` in `find_sites.py` with `signature_from` set to
   the new timestamp, so the build after that can be found by signature as well.
4. If a site is not located: compare the report's notes with the previous build's report, find the function
   by hand (the anchors above are the starting points), and only then extend the tool. Do not type an RVA
   into the table by hand.
