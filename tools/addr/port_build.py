#!/usr/bin/env python3
"""port_build.py: carries a build table row from one TransportFever3.exe to the next.

    python -I port_build.py <OLD TransportFever3.exe> <NEW TransportFever3.exe> [--out DIR]
    python -I port_build.py <OLD TransportFever3.exe> --export-proof FILE   (the proof native/src/search.rs repeats at game start)

Both executables are read as data, never run. The old one must be a build a committed port report (`out/port-*.txt`) names
as its old or new build: its sites are read from that report (`known_sites`), this tool keeps no addresses of its own. For every site the tool
takes the function that holds it in the old executable (its unwind-table entry, or for a leaf function the code up to its
first `ret`/`jmp`), turns the whole function into a pattern and searches the new `.text` for it.

Two strengths of pattern:

- **exact**: only what a relink moves is a wildcard (rel32 branch and call targets, rip-relative displacements, image RVAs
  used as the displacement of an indexed operand, which is how MSVC reads its jump tables). One match means the function
  is the same code in the new build. Every `call rel32` in it is then checked one level deep: the old and the new callee
  must start with the same code (32 bytes), so a match cannot hide a call that now leads to a different function.
- **shape**: every 32-bit displacement is a wildcard too. One match means the same instructions in the same order with
  some structure offsets moved (a class that grew); the report lists every displacement that moved. A shape match is
  accepted only for a place INSIDE a function that the module merely calls through or patches a call in (the hold list,
  the callbacks' vtable, the preview listener's construction); a hooked or called function itself must be exact.

Several matches (small functions that exist in several copies) are resolved by the shift of the nearest neighbour functions
that match once, if exactly one copy sits at that shift. No match: the site is not located. Nothing is guessed: a site
that is not located is printed as `None` and the feature that needs it stays off.

A call site (`CallPatchSite`) must lead in the new build to a callee whose bytes equal the old callee's outside relocations.
The bytes the row records are taken from the new executable, as long as the old ones, and must equal them outside the
relocation fields. The tools' completion callbacks (`fire_done_calls`) are the `_Do_call` slot (2) of the `std::function`
vtable the tool's function loads, and the callback itself must be exact. The offsets the module reads through the preview
listener's captures are carried by the shape match of the listener's call operator. The layout witnesses (the engine
functions the portable form's field offsets were read from) must all be exact for the new build to use the old layout.

Exit code 0: every site located; 1: something is not (the report says what).
"""

from __future__ import annotations

import argparse
import bisect
import os
import re
import struct
import sys
from collections import Counter
from dataclasses import dataclass, field

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from find_sites import Image, Pattern, is_relative_branch, is_rip_relative, fmt_ins  # noqa: E402
from capstone.x86 import X86_OP_MEM, X86_REG_RIP  # noqa: E402

CALLEE_PROBE = 32     # bytes compared at the start of each callee
NEIGHBOURS = 12       # old functions tried on each side for the neighbour shift


# --------------------------------------------------------------------------- the sites of a known build

@dataclass
class S:
    """One site of the old row. `kind`: `func` (a function start, `n` bytes recorded), `inside` (a place inside a function,
    `n` bytes recorded), `ret` (a return address of the hold list: the call before it), `call` (a `call rel32` patched
    in place; `n` bytes of its callee `callee` recorded), `slot` (`_Do_call` of the `std::function` vtable loaded by the
    function that holds the hold-list return address `host`)."""
    field: str
    kind: str
    rva: int
    n: int = 0
    callee: int = 0
    host: int = 0
    rust: str = ""     # HookSite | CallSite | PatchSite | CallPatchSite | Rva | Engine | Witness
    offsets: dict[str, int] = field(default_factory=dict)  # `call` only: offsets read through the callee's lambda


# the Rust type of each field of the row (the schema of `build_table::BuildAddrs`, the same for every build)
RUST_OF = {
    **{f: "HookSite" for f in ("command_list_add", "apply_command", "town_seed", "town_develop", "script_convert",
                               "convert_update", "preview_event", "town_update_size", "town_candidates", "town_edges")},
    **{f: "CallSite" for f in ("game_time_getter", "command_move", "command_destroy", "handle_destroy")},
    **{f: "PatchSite" for f in ("tick_count_getter", "script_pool", "lua_makeseed")},
    **{f: "Engine" for f in ("proposal_ctor", "proposal_dtor", "proposal_copy", "proposal_assign", "engine_new",
                             "engine_delete", "refund_insert", "add_entity", "remove_entity", "game_time_advance",
                             "component_lookup")},
}
REPORTS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "out")


@dataclass
class Known:
    name: str
    layout: str
    sites: list[S]


def sites_from_report(text: str, side: str) -> Known:
    """The sites of one side (`old` or `new`) of a committed port report: the report is the data, this tool keeps no
    addresses of its own. A slot's host (the hold-list site whose tool function loads its vtable) is filled in later from the
    executable (`fill_hosts`)."""
    head = re.search(r"old 0x([0-9a-f]+) build (\S+)  ->  new 0x([0-9a-f]+) build (\S+)", text)
    lay = re.search(r"uses the layout table of (\S+)", text)
    if not head or not lay or "all sites located" not in text:
        raise SystemExit("port report: not a complete report")
    name = head.group(2) if side == "old" else head.group(4)
    sites = []
    for blk in text.split("=" * 100)[1:]:
        m = re.match(r"\n(\S+)  \[(\w+)\]  old (0x[0-9a-f]+)", blk)
        if not m:
            continue
        fld, kind, old = m.group(1), m.group(2), int(m.group(3), 16)
        new = int(re.search(r"\n  new (0x[0-9a-f]+)", blk).group(1), 16)
        n = re.search(r"bytes \((\d+)\)", blk)
        callee = re.search(r"callee: old (0x[0-9a-f]+), the new call leads to (0x[0-9a-f]+)", blk)
        offs = {k: int(o if side == "old" else v, 16) for k, o, v in re.findall(r"offset (\w+): (0x[0-9a-f]+) -> (0x[0-9a-f]+)", blk)}
        rust = ("Witness" if fld.startswith("layout:") else "Rva" if kind in ("ret", "slot")
                else "CallPatchSite" if kind == "call" else RUST_OF[fld])
        sites.append(S(fld, kind, old if side == "old" else new, int(n.group(1)) if n else 0,
                       callee=int(callee.group(1 if side == "old" else 2), 16) if callee else 0, rust=rust, offsets=offs))
    return Known(name, lay.group(1), sites)


def known_sites(timestamp: int) -> Known | None:
    """The sites of a build some committed port report names (as its old or its new build)."""
    for fn in sorted(os.listdir(REPORTS)):
        m = re.fullmatch(r"port-([0-9a-f]{8})-([0-9a-f]{8})\.txt", fn)
        if m and timestamp in (int(m.group(1), 16), int(m.group(2), 16)):
            with open(os.path.join(REPORTS, fn), encoding="utf-8") as fh:
                return sites_from_report(fh.read(), "old" if int(m.group(1), 16) == timestamp else "new")
    return None


def fill_hosts(img: Image, sites: list[S]) -> None:
    """A slot's host: the one hold-list site whose tool function loads a vtable whose slot 2 is the callback."""
    rets = [s for s in sites if s.kind == "ret"]
    for s in sites:
        if s.kind == "slot":
            hosts = []
            for r in rets:
                f = img.func_containing(r.rva - 5)
                if f and any(v[2] == s.rva for v in vtables(img, f.begin, f.end)):
                    hosts.append(r.rva)
            if len(set(img.func_containing(h - 5).begin for h in hosts)) != 1:
                raise SystemExit(f"{s.field}: {len(hosts)} hold-list tool functions load its vtable (expected one)")
            s.host = hosts[0]


# --------------------------------------------------------------------------- patterns of code

IMAGE_SIZE = 0x8000000  # set from the images in main()


def mem_disp(ins) -> int | None:
    """The 32-bit displacement of the instruction's memory operand, when it is not rip-relative. Capstone reports no
    `disp_size` for some operand-size-prefixed stores, so a value outside the disp8 range counts as a disp32 too."""
    for op in ins.operands:
        if op.type == X86_OP_MEM and op.mem.base != X86_REG_RIP and (ins.disp_size == 4 or not -128 <= op.mem.disp <= 127):
            return op.mem.disp
    return None


def any_disp(ins) -> int | None:
    for op in ins.operands:
        if op.type == X86_OP_MEM and op.mem.base != X86_REG_RIP:
            return op.mem.disp
    return None


def reloc_cells(ins, shape: bool = False) -> list[int | None]:
    cells: list[int | None] = list(ins.bytes)
    if is_relative_branch(ins) and ins.imm_size == 4:
        for k in range(ins.imm_offset, ins.imm_offset + 4):
            cells[k] = None
        return cells
    wild = is_rip_relative(ins) and ins.disp_size == 4
    if not wild:
        d = mem_disp(ins)
        indexed = any(op.type == X86_OP_MEM and op.mem.index != 0 for op in ins.operands)
        wild = d is not None and (shape or (indexed and 0x10000 <= d < IMAGE_SIZE))
    if wild:
        at = disp_at(ins)
        for k in range(at, at + 4):
            cells[k] = None
    return cells


def disp_at(ins) -> int:
    """Where the 32-bit displacement sits in the instruction. Capstone's `disp_offset` is off by one for some
    operand-size-prefixed stores (`66 41 89 9d <disp>`), so the encoded value is looked for when the offset does not hold it."""
    raw = bytes(ins.bytes)
    disp = next((op.mem.disp for op in ins.operands if op.type == X86_OP_MEM), 0)
    want = struct.pack("<i", disp) if -(1 << 31) <= disp < (1 << 31) else struct.pack("<I", disp & 0xFFFFFFFF)
    if raw[ins.disp_offset: ins.disp_offset + 4] == want:
        return ins.disp_offset
    at = raw.find(want, 1)
    return at if at > 0 else ins.disp_offset


def decode(img: Image, begin: int, end: int) -> list | None:
    out, cur = [], begin
    while cur < end:
        ins = img.disasm(cur, 16)
        if not ins:
            return None
        out.append(ins[0])
        cur += ins[0].size
    return out if cur == end else None


def pattern_of(insns, shape: bool = False) -> list[int | None]:
    return [c for i in insns for c in reloc_cells(i, shape)]


def to_pattern(cells: list[int | None]) -> Pattern:
    return Pattern(" ".join("??" if c is None else f"{c:02X}" for c in cells))


def leaf_extent(img: Image, rva: int) -> int | None:
    cur = rva
    for _ in range(64):
        ins = img.disasm(cur, 16)
        if not ins:
            return None
        cur += ins[0].size
        if ins[0].mnemonic in ("ret", "jmp"):
            return cur
    return None


def extent(img: Image, rva: int) -> tuple[int, int] | None:
    f = img.func_containing(rva)
    if f is not None:
        return f.begin, f.end
    end = leaf_extent(img, rva)
    return (rva, end) if end else None


def call_target(img: Image, at: int) -> int | None:
    b = img.text_at(at, 5)
    if len(b) == 5 and b[0] == 0xE8:
        return at + 5 + struct.unpack("<i", b[1:])[0]
    return None


def qword(img: Image, rva: int) -> int | None:
    off = img.rva_to_off(rva)
    return None if off is None or off + 8 > len(img.data) else struct.unpack_from("<Q", img.data, off)[0]


def vtables(img: Image, begin: int, end: int) -> list[tuple[int, int, int]]:
    """(offset in the function, vtable, slot 2) of every `std::function`-like vtable the code loads with `lea r,[rip+d]`."""
    out = []
    for ins in decode(img, begin, end) or []:
        if ins.mnemonic == "lea" and "rip" in ins.op_str:
            at = ins.address - img.image_base
            t = at + ins.size + ins.disp
            if img.in_text(t):
                continue
            slots = [qword(img, t + 8 * k) for k in range(5)]
            if all(s and img.in_text(s - img.image_base) for s in slots):
                out.append((at - begin, t, slots[2] - img.image_base))
    return out


# --------------------------------------------------------------------------- mapping

@dataclass
class Mapped:
    rva: int | None
    how: str
    shape: bool = False
    moved: list[tuple[int, int]] = field(default_factory=list)   # (old, new) displacements of a shape match


class Porter:
    def __init__(self, old: Image, new: Image):
        self.old, self.new = old, new
        self.cache: dict[tuple[int, bool], tuple[list[int], str]] = {}

    def hits(self, begin: int, shape: bool) -> tuple[list[int], str]:
        key = (begin, shape)
        if key not in self.cache:
            ext = extent(self.old, begin)
            if ext is None or ext[0] != begin:
                self.cache[key] = ([], "no function starts there in the old build")
            else:
                insns = decode(self.old, *ext)
                if insns is None:
                    self.cache[key] = ([], "the old function does not decode as one run of instructions")
                else:
                    pat = to_pattern(pattern_of(insns, shape))
                    self.cache[key] = ([h + self.new.text_rva for h in pat.find_all(self.new.text)], f"{ext[1] - ext[0]:#x} bytes")
        return self.cache[key]

    def neighbours(self, begin: int) -> list[tuple[int, int]]:
        starts = self.old._func_starts
        i = bisect.bisect_left(starts, begin)
        found = []
        for rng in (range(i - 1, max(-1, i - 1 - NEIGHBOURS), -1), range(i + 1, min(len(starts), i + 1 + NEIGHBOURS))):
            for j in rng:
                h, _ = self.hits(starts[j], False)
                if len(h) == 1:
                    found.append((starts[j], h[0] - starts[j]))
                    break
        return found

    def map_function(self, begin: int, allow_shape: bool = False) -> Mapped:
        for shape in ((False, True) if allow_shape else (False,)):
            h, what = self.hits(begin, shape)
            label = "shape (structure offsets moved)" if shape else "exact"
            pick, how = None, ""
            if len(h) == 1:
                pick, how = h[0], f"whole function {label} ({what}), one match"
            elif h:
                nb = self.neighbours(begin)
                at = sorted({x for x in h for _, d in nb if x - begin == d})
                note = ", ".join(f"{o:#x}{d:+#x}" for o, d in nb)
                if len(at) == 1:
                    pick, how = at[0], f"whole function {label} ({what}), {len(h)} copies; the one at the shift of its neighbours ({note})"
                else:
                    return Mapped(None, f"function {begin:#x} ({what}) has {len(h)} {label} copies; the neighbours ({note}) do not single one out")
            if pick is not None:
                m = Mapped(pick, how, shape)
                if shape:
                    m.moved = self.moved(begin, pick)
                return m
        return Mapped(None, f"function {begin:#x} changed ({self.hits(begin, False)[1]}): no "
                            + ("exact or shape" if allow_shape else "exact") + " match of its whole body")

    def pairs(self, old_begin: int, new_begin: int):
        oe = extent(self.old, old_begin)
        a = decode(self.old, *oe) or []
        b = decode(self.new, new_begin, new_begin + oe[1] - oe[0]) or []
        return list(zip(a, b))

    def moved(self, old_begin: int, new_begin: int) -> list[tuple[int, int]]:
        return [(mem_disp(x), mem_disp(y)) for x, y in self.pairs(old_begin, new_begin)
                if mem_disp(x) is not None and mem_disp(x) != mem_disp(y)]

    def callees_agree(self, old_begin: int, new_begin: int) -> tuple[bool, str]:
        bad, n = [], 0
        for x, y in self.pairs(old_begin, new_begin):
            if x.mnemonic == "call" and x.bytes[0] == 0xE8:
                ot = call_target(self.old, x.address - self.old.image_base)
                nt = call_target(self.new, y.address - self.new.image_base)
                n += 1
                take = lambda img, t: [c for i in img.disasm(t, CALLEE_PROBE + 16) for c in reloc_cells(i)][:CALLEE_PROBE]
                if take(self.old, ot) != take(self.new, nt):
                    bad.append(f"call at {x.address - self.old.image_base:#x}: old callee {ot:#x} and new callee {nt:#x} start differently")
        return not bad, "; ".join(bad) or f"{n} call(s) checked one level deep: every callee starts with the same code"


def summary(moved: list[tuple[int, int]]) -> str:
    if not moved:
        return "no displacement moved"
    by = Counter(n - o for o, n in moved)
    lo = min(o for o, _ in moved)
    return (f"{len(moved)} displacement(s) moved (old {lo:#x} and up), by " + ", ".join(f"{d:+#x} x{c}" for d, c in by.most_common(4))
            + (" ..." if len(by) > 4 else ""))


def same_but_relocs(old: Image, ro: int, new: Image, rn: int, n: int) -> tuple[bool, bytes]:
    oi, total = [], 0
    for ins in old.disasm(ro, n + 16):
        if total >= n:
            break
        oi.append(ins)
        total += ins.size
    b = new.text_at(rn, n)
    cells = pattern_of(oi)[:n]
    return len(b) == n and all(c is None or b[k] == c for k, c in enumerate(cells)), b


def rust_bytes(b: bytes) -> str:
    return "&[" + ", ".join(f"0x{x:02X}" for x in b) + "]"


# --------------------------------------------------------------------------- the proof for the runtime search

PROOF_MAGIC = b"FL3PROOF"
PROOF_VERSION = 2
SITE_KIND = {"func": 0, "inside": 1, "ret": 2, "call": 3, "slot": 4, "witness": 5}
NO_PATTERN = 0xFFFF
PROOF_SMALL = 0x400        # functions up to this size get neighbours even when unique in the reference
PROOF_NEIGHBOURS = 3       # neighbours kept per side


class Proof:
    """What `native/src/search.rs` needs to repeat this tool's proof on an unknown build at game start, without a disassembler:
    every pattern this tool would search for (the whole function, relink fields wildcarded; `shape` with every 32-bit
    displacement wildcarded too), where the module's sites sit in them, the callees' first bytes, and the neighbour functions
    that single out one of several copies. Written by `--export-proof`; the format is described in `write`."""

    def __init__(self, ref: Image, known: Known):
        self.ref, self.known = ref, known
        self.P = Porter(ref, ref)   # hits in the reference itself: which functions exist in several copies
        self.pats: list[tuple] = []
        self.index: dict[tuple[int, bool], int] = {}
        self.sites: list[bytes] = []

    def pattern(self, begin: int, shape: bool = False, neighbours: bool = True, calls: bool = False) -> int:
        key = (begin, shape)
        if key in self.index:
            i = self.index[key]
            if calls and not self.pats[i][4]:
                self.pats[i] = self.pats[i][:4] + (self.calls(begin),) + self.pats[i][5:]
            return i
        ext = extent(self.ref, begin)
        if ext is None or ext[0] != begin:
            raise SystemExit(f"proof: no function starts at {begin:#x}")
        insns = decode(self.ref, *ext)
        if insns is None:
            raise SystemExit(f"proof: the function at {begin:#x} does not decode")
        f = self.ref.func_containing(begin)
        at_start = f is not None and f.begin == begin
        cells = pattern_of(insns, shape)
        i = len(self.pats)
        self.index[key] = i
        self.pats.append((begin, at_start, shape, cells, [], [], []))
        if calls:
            self.pats[i] = self.pats[i][:4] + (self.calls(begin),) + self.pats[i][5:]
        # the neighbours that single out one of several copies (Porter.neighbours): for a function with copies in the reference,
        # and for every small one, which may have copies in the target although it has none here (40425 -> 40420: a 0xb5-byte
        # layout witness, unique in 40425, has four copies in 40420). A side keeps the first PROOF_NEIGHBOURS of its NEIGHBOURS
        # nearest functions that are unique in the reference and small (the proof stays small); the module takes the first that
        # is unique in the target.
        if neighbours and ((not shape and len(cells) <= PROOF_SMALL) or len(self.P.hits(begin, shape)[0]) > 1):
            starts = self.ref._func_starts
            k = bisect.bisect_left(starts, begin)
            sides = []
            for rng in (range(k - 1, max(-1, k - 1 - NEIGHBOURS), -1), range(k + 1, min(len(starts), k + 1 + NEIGHBOURS))):
                side = []
                for j in rng:
                    small = (ext := extent(self.ref, starts[j])) is not None and ext[1] - ext[0] <= PROOF_SMALL
                    if len(side) < PROOF_NEIGHBOURS and small and len(self.P.hits(starts[j], False)[0]) == 1:
                        side.append(self.pattern(starts[j], False, neighbours=False))
                sides.append(side)
            self.pats[i] = self.pats[i][:5] + (sides[0], sides[1])
        return i

    def calls(self, begin: int) -> list[tuple[int, list]]:
        """(offset, the callee's first CALLEE_PROBE cells) of every `call rel32` in the function: Porter.callees_agree."""
        out = []
        for ins in decode(self.ref, *extent(self.ref, begin)):
            if ins.mnemonic == "call" and ins.bytes[0] == 0xE8:
                at = ins.address - self.ref.image_base
                t = call_target(self.ref, at)
                probe = [c for i in self.ref.disasm(t, CALLEE_PROBE + 16) for c in reloc_cells(i)][:CALLEE_PROBE]
                out.append((at - begin, probe + [None] * (CALLEE_PROBE - len(probe))))
        return out

    def lea_vtable(self, begin: int, want_slot: int | None = None) -> tuple[int, int]:
        """(where the displacement sits, where the instruction ends), both as offsets in the function, of the one `lea r,[rip+d]`
        that loads a `std::function` vtable (or the one whose slot 2 is `want_slot`)."""
        vt = [v for v in vtables(self.ref, begin, extent(self.ref, begin)[1]) if want_slot is None or v[2] == want_slot]
        if len(vt) != 1:
            raise SystemExit(f"proof: the function at {begin:#x} loads {len(vt)} matching vtables (expected one)")
        ins = self.ref.disasm(begin + vt[0][0], 16)[0]
        return vt[0][0] + disp_at(ins), vt[0][0] + ins.size

    def disp_positions(self, begin: int, value: int) -> list[tuple[int, int]]:
        """(offset in the function, size) of each memory displacement equal to `value` (Porter's `any_disp` pairs)."""
        out = []
        for ins in decode(self.ref, *extent(self.ref, begin)):
            if any_disp(ins) == value:
                at = ins.address - self.ref.image_base - begin
                if ins.disp_size == 4 or not -128 <= value <= 127:
                    out.append((at + disp_at(ins), 4))
                else:
                    out.append((at + ins.disp_offset, 1))
                    assert ins.bytes[ins.disp_offset] == value & 0xFF
        if not out:
            raise SystemExit(f"proof: no displacement {value:#x} in the function at {begin:#x}")
        return out

    def emits(self, begin: int) -> list[int]:
        return [ins.address - self.ref.image_base - begin for ins in decode(self.ref, *extent(self.ref, begin))
                if ins.mnemonic == "call" and ins.bytes[0] == 0xE8]

    def add_site(self, s: S) -> None:
        ref = self.ref
        u16, u32 = (lambda v: struct.pack("<H", v)), (lambda v: struct.pack("<I", v))
        kind = "witness" if s.rust == "Witness" else s.kind
        name = s.field.encode()
        out = bytes([len(name)]) + name + bytes([SITE_KIND[kind]])
        if kind in ("func", "witness"):
            out += u16(self.pattern(s.rva, calls=True)) + u16(NO_PATTERN) + u32(0) + u32(s.n)
        else:
            at = s.rva - 5 if s.kind == "ret" else (s.host - 5 if s.kind == "slot" else s.rva)
            f = ref.func_containing(at)
            # a shape match only where the module merely calls through (the hold list, the callbacks' tool functions, the
            # preview listener's construction): never for a patch region or another patched call
            shape = s.kind in ("ret", "slot") or s.field == "preview_owner"
            out += u16(self.pattern(f.begin)) + u16(self.pattern(f.begin, True) if shape else NO_PATTERN)
            out += u32(0 if s.kind == "slot" else at - f.begin) + u32(s.n)
            if s.kind == "call":
                out += u16(self.pattern(s.callee))
                if s.offsets:
                    d, e = self.lea_vtable(s.callee)
                    listener = qword(ref, s.callee + e + struct.unpack_from("<i", ref.text_at(s.callee + d, 4))[0] + 16) - ref.image_base
                    out += b"\x01" + u32(d) + u32(e) + u16(self.pattern(listener)) + u16(self.pattern(listener, True))
                    out += bytes([len(s.offsets)])
                    for k, v in s.offsets.items():
                        pos = self.disp_positions(listener, v)
                        out += bytes([len(k)]) + k.encode() + bytes([len(pos)]) + b"".join(u32(p) + bytes([z]) for p, z in pos)
                    em = self.emits(listener)
                    out += bytes([len(em)]) + b"".join(u32(x) for x in em)
                else:
                    out += b"\x00"
            elif s.kind == "slot":
                d, e = self.lea_vtable(f.begin, s.rva)
                out += u32(d) + u32(e) + u16(self.pattern(s.rva))
        self.sites.append(out)

    def write(self, path: str) -> None:
        """Little-endian:
            magic "FL3PROOF", u32 version (2), u32 reference PE timestamp, u8 + the reference build's name, u8 + the name of the
              portable layout table its witnesses were read from, u32 pattern count, the patterns, u32 site count, the sites
            pattern: u32 rva in the reference, u8 flags (1: starts at an unwind-table entry, 2: shape, 4: the bytes are those of
              an earlier pattern), u32 length n, n bytes (flag 4: u16 index of that pattern instead),
              (n+7)/8 mask bytes (bit i, low bit first: cell i is fixed), u8 + u16 indices of the neighbours below (nearest first),
              u8 + u16 indices above, u16 call count + per call (u32 offset, 32 bytes, 4 mask bytes of the callee's start)
            site: u8 + name, u8 kind (0 func, 1 inside, 2 ret, 3 call, 4 slot, 5 witness), u16 exact pattern of the function (or of
              the host), u16 shape pattern of the host (0xffff: none), u32 offset of the site in the host, u32 bytes recorded;
              call: u16 callee pattern, u8 listener (0/1); listener: u32 + u32 the vtable lea's displacement and end in the callee,
                u16 + u16 the listener's exact and shape patterns, u8 offset count + per offset (u8 + name, u8 count + per
                place (u32 offset in the listener, u8 size 1 or 4)), u8 count + u32 offsets of the listener's `call rel32`s
              slot: u32 + u32 the vtable lea's displacement and end in the host, u16 the callback's pattern"""
        u16, u32 = (lambda v: struct.pack("<H", v)), (lambda v: struct.pack("<I", v))

        def cells(cs: list) -> bytes:
            mask = bytearray((len(cs) + 7) // 8)
            for k, c in enumerate(cs):
                if c is not None:
                    mask[k >> 3] |= 1 << (k & 7)
            return bytes(0 if c is None else c for c in cs) + bytes(mask)

        name, layout = self.known.name.encode(), self.known.layout.encode()
        out = bytearray(PROOF_MAGIC + u32(PROOF_VERSION) + u32(self.ref.timestamp) + bytes([len(name)]) + name
                        + bytes([len(layout)]) + layout + u32(len(self.pats)))
        for begin, at_start, shape, cs, calls, left, right in self.pats:
            flags = (1 if at_start else 0) | (2 if shape else 0)
            same = self.index.get((begin, False)) if shape else None
            if same is not None:
                # a shape pattern only wildcards more cells of the exact one: its bytes are the exact pattern's
                out += u32(begin) + bytes([flags | 4]) + u32(len(cs)) + u16(same) + cells(cs)[len(cs):]
            else:
                out += u32(begin) + bytes([flags]) + u32(len(cs)) + cells(cs)
            for side in (left, right):
                out += bytes([len(side)]) + b"".join(u16(j) for j in side)
            out += u16(len(calls)) + b"".join(u32(o) + cells(p) for o, p in calls)
        out += u32(len(self.sites)) + b"".join(self.sites)
        with open(path, "wb") as fh:
            fh.write(out)


def export_proof(ref: Image, path: str) -> int:
    known = known_sites(ref.timestamp)
    if known is None:
        sys.exit(f"the executable (timestamp {ref.timestamp:#x}) is in no committed port report")
    sites = known.sites
    fill_hosts(ref, sites)
    pr = Proof(ref, known)
    for s in sites:
        pr.add_site(s)
    pr.write(path)
    print(f"proof of {ref.timestamp:#010x} ({ref.build_string()}): {len(pr.pats)} patterns, {len(pr.sites)} sites, "
          f"{os.path.getsize(path)} bytes -> {path}")
    return 0


# --------------------------------------------------------------------------- main

def main() -> int:
    global IMAGE_SIZE
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("old")
    ap.add_argument("new", nargs="?")
    ap.add_argument("--out", help="also write the report to <DIR>/port-<old>-<new>.txt")
    ap.add_argument("--export-proof", metavar="FILE", help="write the proof of the OLD build for native/src/search.rs and stop")
    a = ap.parse_args()
    if a.export_proof:
        ref = Image(a.old)
        IMAGE_SIZE = ref.pe.OPTIONAL_HEADER.SizeOfImage
        return export_proof(ref, a.export_proof)
    if not a.new:
        ap.error("the NEW executable is needed (or --export-proof)")
    old, new = Image(a.old), Image(a.new)
    IMAGE_SIZE = min(old.pe.OPTIONAL_HEADER.SizeOfImage, new.pe.OPTIONAL_HEADER.SizeOfImage)
    known = known_sites(old.timestamp)
    if known is None:
        sys.exit(f"the old executable (timestamp {old.timestamp:#x}) is in no committed port report (out/port-*.txt)")
    sites = known.sites
    fill_hosts(old, sites)
    P = Porter(old, new)
    out: list[str] = []
    p = out.append
    p("FeverLink3 port tool: tools/addr/port_build.py")
    p(f"old {old.timestamp:#010x} build {old.build_string()}  ->  new {new.timestamp:#010x} build {new.build_string()}")
    found: dict[str, int] = {}
    data: dict[str, bytes] = {}
    offs: dict[str, dict[str, int]] = {}
    status = 0
    for s in sites:
        p("=" * 100)
        p(f"{s.field}  [{s.kind}]  old {s.rva:#x}")
        lines: list[str] = []
        ok = True
        if s.kind == "func":
            m = P.map_function(s.rva)
            if m.rva is not None:
                good, why = P.callees_agree(s.rva, m.rva)
                lines.append(why)
                ok &= good
        elif s.kind in ("inside", "ret", "call"):
            at = s.rva - 5 if s.kind == "ret" else s.rva
            f = old.func_containing(at)
            hm = P.map_function(f.begin, allow_shape=True) if f else Mapped(None, "no unwind entry")
            m = Mapped(None if hm.rva is None else hm.rva + at - f.begin, hm.how + (f"; the site keeps its offset +{at - f.begin:#x}" if f else ""))
            if hm.shape:
                lines.append("host function: " + summary(hm.moved))
                if s.kind == "inside":
                    m = Mapped(None, m.how + "; a patch region inside a function whose offsets moved is not taken")
            if m.rva is not None and s.kind in ("ret", "call"):
                ot, nt = call_target(old, at), call_target(new, m.rva)
                if nt is None:
                    ok = False
                    lines.append("!!! no `call rel32` at the new site")
                elif s.kind == "ret":
                    want = found.get("command_list_add")
                    lines.append(f"the call leads to {nt:#x}" + ("" if nt == want else f"  !!! not command_list_add ({want and hex(want)})"))
                    ok &= nt == want and ot is not None
                    m.rva += 5
                else:
                    good, cb = same_but_relocs(old, s.callee, new, nt, s.n)
                    cm = P.map_function(s.callee)
                    lines.append(f"callee: old {s.callee:#x}, the new call leads to {nt:#x}; bytes ({s.n}) {cb.hex(' ').upper()} "
                                 + ("same as the old outside relocations" if good else "!!! DIFFER"))
                    lines.append(f"callee by its own body: {cm.how}" + ("" if cm.rva in (None, nt) else f"  !!! at {cm.rva:#x}, not where the call leads"))
                    # a callee with several copies is fixed by the call itself (same offset in a matched host); its bytes
                    # were compared just above
                    ok &= good and cm.rva in (None, nt)
                    found[s.field + ".callee"], data[s.field + ".callee"] = nt, cb
                    data[s.field] = new.text_at(m.rva, 5)
                    if s.offsets:
                        ov, nv = vtables(old, s.callee, extent(old, s.callee)[1]), vtables(new, nt, extent(new, nt)[1])
                        if len(ov) != 1 or len(nv) != 1:
                            ok = False
                            lines.append(f"!!! the callee loads {len(ov)} / {len(nv)} vtables (expected one)")
                        else:
                            ol, nl = ov[0][2], nv[0][2]
                            lm = P.map_function(ol, allow_shape=True)
                            lines.append(f"listener call operator (vtable slot 2): old {ol:#x}, new {nl:#x}; {lm.how}")
                            if lm.rva != nl:
                                ok = False
                                lines.append("!!! the listener does not map to the new vtable's slot")
                            else:
                                prs = P.pairs(ol, nl)
                                got = {}
                                for name, v in s.offsets.items():
                                    news = {any_disp(y) for x, y in prs if any_disp(x) == v}
                                    if len(news) == 1:
                                        got[name] = news.pop()
                                        lines.append(f"offset {name}: {v:#x} -> {got[name]:#x}")
                                    else:
                                        ok = False
                                        lines.append(f"!!! offset {name} ({v:#x}) is read {len(news)} different ways in the new listener")
                                offs[s.field] = got
                                emits = {call_target(new, y.address - new.image_base) for _, y in prs if y.mnemonic == "call" and y.bytes[0] == 0xE8}
                                pe = found.get("preview_event")
                                lines.append(f"the listener calls preview_event ({pe and hex(pe)}): {'yes' if pe in emits else '!!! NO'}")
                                ok &= pe in emits
        elif s.kind == "slot":
            hf = old.func_containing(s.host - 5)
            ov = [v for v in vtables(old, hf.begin, hf.end) if v[2] == s.rva]
            hm = P.map_function(hf.begin, allow_shape=True)
            if len(ov) != 1 or hm.rva is None:
                m = Mapped(None, f"the tool function {hf.begin:#x} loads {len(ov)} vtable(s) whose slot 2 is {s.rva:#x}; {hm.how}")
            else:
                nf = new.func_containing(hm.rva)
                nv = {o: d for o, _, d in vtables(new, nf.begin, nf.end)}
                d = nv.get(ov[0][0])
                m = Mapped(d, f"slot 2 (_Do_call) of the vtable the tool function {hf.begin:#x} loads at +{ov[0][0]:#x}; tool function: {hm.how}")
                if hm.shape:
                    lines.append("tool function: " + summary(hm.moved))
                if d is not None:
                    cm = P.map_function(s.rva)
                    lines.append(f"the callback itself: {cm.how}")
                    # exact: the one match is the slot, or the slot is one of the exact copies of a small callback
                    if cm.rva != d and d not in P.hits(s.rva, False)[0]:
                        ok = False
                        lines.append(f"!!! the callback's body is not exactly the old one at {d:#x}")
        else:
            raise ValueError(s.kind)
        p(f"  {m.how}")
        for ln in lines:
            p(f"  - {ln}")
        if m.rva is None or not ok:
            p("  NOT LOCATED: stays None, the feature that needs it stays off")
            status = 1
            continue
        p(f"  new {m.rva:#x}  (shift {m.rva - s.rva:+#x})")
        if s.n and s.kind != "call":
            good, b = same_but_relocs(old, s.rva, new, m.rva, s.n)
            p(f"  bytes ({s.n}): {b.hex(' ').upper()}  " + ("same as the old outside relocations" if good else "!!! DIFFER"))
            if not good:
                p("  NOT LOCATED")
                status = 1
                continue
            data[s.field] = b
        found[s.field] = m.rva

    wit = [s for s in sites if s.rust == "Witness"]
    nwit = sum(s.field in found for s in wit)
    layout = known.layout if nwit == len(wit) else None
    p("=" * 100)
    p(f"layout witnesses: {nwit} of {len(wit)} exact -> "
      + (f"the new build uses the layout table of {layout}" if layout else "the layout table does NOT carry over (layout: None)"))
    p("")
    p("Rust for native/src/build_table.rs (new build):")

    def site(s: S) -> str:
        f = s.field
        if f not in found:
            return "None"
        if s.rust == "HookSite":
            return f"Some(HookSite {{ rva: {found[f]:#x}, prologue: {rust_bytes(data[f])} }})"
        if s.rust == "CallSite":
            return f"Some(CallSite {{ rva: {found[f]:#x}, prologue: {rust_bytes(data[f])} }})"
        if s.rust == "PatchSite":
            return f"rva {found[f]:#x}, expected {rust_bytes(data[f])}"
        if s.rust == "CallPatchSite":
            body = (f"CallPatchSite {{ rva: {found[f]:#x}, expected: {rust_bytes(data[f])}, callee: CallSite {{ rva: "
                    f"{found[f + '.callee']:#x}, prologue: {rust_bytes(data[f + '.callee'])} }} }}")
            if f in offs:
                body += " " + ", ".join(f"{k}: {v:#x}" for k, v in offs[f].items())
            return f"Some({body})"
        return f"{found[f]:#x}"

    for s in sites:
        if s.rust in ("HookSite", "CallSite", "PatchSite", "CallPatchSite"):
            p(f"  {s.field}: {site(s)}")
    for group in ("ui_add_sites", "fire_done_calls"):
        rows = [s for s in sites if s.field.startswith(group + "[")]
        if all(s.field in found for s in rows):
            p(f"  {group}: &[" + ", ".join(f"{found[s.field]:#x}" for s in rows) + "]")
        else:
            p(f"  {group}: &[]  (not every entry was located)")
    p("  engine: EngineFns { " + ", ".join(f"{s.field}: " + (f"Some({found[s.field]:#x})" if s.field in found else "None")
                                          for s in sites if s.rust == "Engine") + " }")
    p(f"  layout: " + (f'Some("{layout}")' if layout else "None"))
    p("")
    p("all sites located" if status == 0 else "!!! NOT EVERY SITE WAS LOCATED: the ones marked NOT LOCATED stay None")
    text = "\n".join(out) + "\n"
    sys.stdout.write(text)
    if a.out:
        os.makedirs(a.out, exist_ok=True)
        with open(os.path.join(a.out, f"port-{old.timestamp:08x}-{new.timestamp:08x}.txt"), "w", encoding="utf-8", newline="\n") as fh:
            fh.write(text)
    return status


if __name__ == "__main__":
    sys.exit(main())
