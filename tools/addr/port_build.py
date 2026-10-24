#!/usr/bin/env python3
"""port_build.py: carries a build table row from one TransportFever3.exe to the next.

    python -I port_build.py <OLD TransportFever3.exe> <NEW TransportFever3.exe> [--out DIR]

Both executables are read as data, never run. The old one must be a build whose sites this tool lists (`OLD_SITES`, keyed
by PE timestamp: the row of `native/src/build_table.rs` and the engine functions of `EngineFns`). For every site the tool
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


# build 40420 (PE timestamp 0x6ac50427)
OLD_SITES: dict[int, list[S]] = {
    0x6AC50427: [
        S("command_list_add", "func", 0x9D4040, 18, rust="HookSite"),
        S("apply_command", "func", 0x9E33B0, 14, rust="HookSite"),
        S("town_seed", "func", 0x8D4270, 20, rust="HookSite"),
        S("town_develop", "func", 0x8DE0B0, 15, rust="HookSite"),
        S("script_convert", "func", 0x10C6750, 14, rust="HookSite"),
        S("convert_update", "func", 0x10C6020, 14, rust="HookSite"),
        S("preview_event", "func", 0xF44C00, 14, rust="HookSite"),
        S("preview_owner", "call", 0x64CB41, 15, callee=0x642DC0, rust="CallPatchSite",
          offsets={"y_a": 0x18, "y_b": 0xC8, "r_c": 0x18, "r_dispatcher": 0x1D8, "ui_e": 0xB98}),
        S("town_update_size", "func", 0x9E12B0, 15, rust="HookSite"),
        S("town_candidates", "func", 0x9677C0, 14, rust="HookSite"),
        S("town_edges", "func", 0x95D100, 14, rust="HookSite"),
        S("town_sort_call", "call", 0x967C16, 16, callee=0x966150, rust="CallPatchSite"),
        S("town_pick_call", "call", 0x95D627, 16, callee=0x3055870, rust="CallPatchSite"),
        S("town_field_call", "call", 0x96962E, 18, callee=0x2B7A1B0, rust="CallPatchSite"),
        S("batch_pace", "call", 0x11F446, 16, callee=0x11F690, rust="CallPatchSite"),
        S("free_order", "call", 0x2BBA103, 18, callee=0x2BB62A0, rust="CallPatchSite"),
        S("game_time_getter", "func", 0x2A96C0, 14, rust="CallSite"),
        S("tick_count_getter", "func", 0x2A9600, 27, rust="PatchSite"),
        S("script_pool", "inside", 0xAB00FC, 22, rust="PatchSite"),
        S("lua_makeseed", "func", 0x2FC8FE0, 26, rust="PatchSite"),
        S("command_move", "func", 0x9D0430, 16, rust="CallSite"),
        S("command_destroy", "func", 0x9D0670, 16, rust="CallSite"),
        S("handle_destroy", "func", 0x3040C90, 18, rust="CallSite"),
        *[S(f"ui_add_sites[{i}]", "ret", r, rust="Rva") for i, r in enumerate([
            0x4D396F, 0x51B56C, 0x528504, 0x537EE5, 0x538649, 0x5387CD, 0x542EBA, 0x5891B4, 0x5948DD, 0x5BF08D,
            0x28A3024, 0x28A3871])],
        S("fire_done_calls[0]", "slot", 0x4D8860, host=0x4D396F, rust="Rva"),   # the bulldozer's
        S("fire_done_calls[1]", "slot", 0x58C0B0, host=0x5891B4, rust="Rva"),   # the street tool's
        S("fire_done_calls[2]", "slot", 0x596A70, host=0x5948DD, rust="Rva"),   # the stop tool's
        # EngineFns: called or hooked by the id trace, the native transport and the refund list
        S("proposal_ctor", "func", 0x4C6870, 23, rust="Engine"),
        S("proposal_dtor", "func", 0x48E220, 20, rust="Engine"),
        S("proposal_copy", "func", 0x4C6640, 24, rust="Engine"),
        S("proposal_assign", "func", 0x4CED30, 24, rust="Engine"),
        S("engine_new", "func", 0x318C530, 11, rust="Engine"),
        S("engine_delete", "func", 0x318C56C, 5, rust="Engine"),
        S("refund_insert", "func", 0x4BE210, 0xEA, rust="Engine"),
        S("add_entity", "func", 0x2BB8940, 15, rust="Engine"),
        S("remove_entity", "func", 0x2BBC700, 15, rust="Engine"),
        S("game_time_advance", "func", 0xBAD780, 16, rust="Engine"),
        S("component_lookup", "func", 0x1445C0, 15, rust="Engine"),
        # layout witnesses: the engine functions the portable layout table (native/src/portable.rs) was read from
        *[S(f"layout:{r:#x}", "func", r, rust="Witness") for r in [
            0x4C6640, 0x4C6870, 0x48E220, 0x4CED30, 0x4C78C0, 0x4BDA90, 0x4C31A0, 0x4BEC80, 0x4C1DF0, 0x4BC770, 0x4BD1F0,
            0x4C5FA0, 0x63C10, 0x4BD430, 0x4C2250, 0x284FC0, 0x4BC8D0, 0xDF480, 0x4BE210, 0x10C6750, 0x10C6020, 0x9E2900]],
    ],
    # build 40423 (PE timestamp 0x6ad0e4e5): the 40420 sites where port-6ac50427-6ad0e4e5.txt found them
    0x6AD0E4E5: [
        S("command_list_add", "func", 0x9D9140, 18, rust="HookSite"),
        S("apply_command", "func", 0x9E84B0, 14, rust="HookSite"),
        S("town_seed", "func", 0x8D9380, 20, rust="HookSite"),
        S("town_develop", "func", 0x8E31C0, 15, rust="HookSite"),
        S("script_convert", "func", 0x10CD5A0, 14, rust="HookSite"),
        S("convert_update", "func", 0x10CCE70, 14, rust="HookSite"),
        S("preview_event", "func", 0xF4BA50, 14, rust="HookSite"),
        S("preview_owner", "call", 0x64D9C1, 15, callee=0x643C40, rust="CallPatchSite", offsets={"y_a": 0x18, "y_b": 0xC8, "r_c": 0x18, "r_dispatcher": 0x1D8, "ui_e": 0xDC0}),
        S("town_update_size", "func", 0x9E63B0, 15, rust="HookSite"),
        S("town_candidates", "func", 0x96C8C0, 14, rust="HookSite"),
        S("town_edges", "func", 0x962200, 14, rust="HookSite"),
        S("town_sort_call", "call", 0x96CD16, 16, callee=0x96B250, rust="CallPatchSite"),
        S("town_pick_call", "call", 0x962727, 16, callee=0x306AFC0, rust="CallPatchSite"),
        S("town_field_call", "call", 0x96E72E, 18, callee=0x2B81850, rust="CallPatchSite"),
        S("batch_pace", "call", 0x11F456, 16, callee=0x11F6A0, rust="CallPatchSite"),
        S("free_order", "call", 0x2BC1A23, 18, callee=0x2BBDBC0, rust="CallPatchSite"),
        S("game_time_getter", "func", 0x2A96E0, 14, rust="CallSite"),
        S("tick_count_getter", "func", 0x2A9620, 27, rust="PatchSite"),
        S("script_pool", "inside", 0xAB528C, 22, rust="PatchSite"),
        S("lua_makeseed", "func", 0x2FDE370, 26, rust="PatchSite"),
        S("command_move", "func", 0x9D5530, 16, rust="CallSite"),
        S("command_destroy", "func", 0x9D5770, 16, rust="CallSite"),
        S("handle_destroy", "func", 0x30563E0, 18, rust="CallSite"),
        S("ui_add_sites[0]", "ret", 0x4D479F, rust="Rva"),
        S("ui_add_sites[1]", "ret", 0x51C3AC, rust="Rva"),
        S("ui_add_sites[2]", "ret", 0x529334, rust="Rva"),
        S("ui_add_sites[3]", "ret", 0x538D15, rust="Rva"),
        S("ui_add_sites[4]", "ret", 0x539479, rust="Rva"),
        S("ui_add_sites[5]", "ret", 0x5395FD, rust="Rva"),
        S("ui_add_sites[6]", "ret", 0x543CEA, rust="Rva"),
        S("ui_add_sites[7]", "ret", 0x589FC4, rust="Rva"),
        S("ui_add_sites[8]", "ret", 0x5956ED, rust="Rva"),
        S("ui_add_sites[9]", "ret", 0x5BFE9D, rust="Rva"),
        S("ui_add_sites[10]", "ret", 0x28AA534, rust="Rva"),
        S("ui_add_sites[11]", "ret", 0x28AAD81, rust="Rva"),
        S("fire_done_calls[0]", "slot", 0x4D9690, host=0x4D479F, rust="Rva"),
        S("fire_done_calls[1]", "slot", 0x58CEC0, host=0x589FC4, rust="Rva"),
        S("fire_done_calls[2]", "slot", 0x597880, host=0x5956ED, rust="Rva"),
        S("proposal_ctor", "func", 0x4C76A0, 23, rust="Engine"),
        S("proposal_dtor", "func", 0x48F050, 20, rust="Engine"),
        S("proposal_copy", "func", 0x4C7470, 24, rust="Engine"),
        S("proposal_assign", "func", 0x4CFB60, 24, rust="Engine"),
        S("engine_new", "func", 0x31A1C80, 11, rust="Engine"),
        S("engine_delete", "func", 0x31A1CBC, 5, rust="Engine"),
        S("refund_insert", "func", 0x4BF040, 234, rust="Engine"),
        S("add_entity", "func", 0x2BC0260, 15, rust="Engine"),
        S("remove_entity", "func", 0x2BC4020, 15, rust="Engine"),
        S("game_time_advance", "func", 0xBB2970, 16, rust="Engine"),
        S("component_lookup", "func", 0x1445D0, 15, rust="Engine"),
        S("layout:0x4c7470", "func", 0x4C7470, rust="Witness"),
        S("layout:0x4c76a0", "func", 0x4C76A0, rust="Witness"),
        S("layout:0x48f050", "func", 0x48F050, rust="Witness"),
        S("layout:0x4cfb60", "func", 0x4CFB60, rust="Witness"),
        S("layout:0x4c86f0", "func", 0x4C86F0, rust="Witness"),
        S("layout:0x4be8c0", "func", 0x4BE8C0, rust="Witness"),
        S("layout:0x4c3fd0", "func", 0x4C3FD0, rust="Witness"),
        S("layout:0x4bfab0", "func", 0x4BFAB0, rust="Witness"),
        S("layout:0x4c2c20", "func", 0x4C2C20, rust="Witness"),
        S("layout:0x4bd5a0", "func", 0x4BD5A0, rust="Witness"),
        S("layout:0x4be020", "func", 0x4BE020, rust="Witness"),
        S("layout:0x4c6dd0", "func", 0x4C6DD0, rust="Witness"),
        S("layout:0x63c20", "func", 0x63C20, rust="Witness"),
        S("layout:0x4be260", "func", 0x4BE260, rust="Witness"),
        S("layout:0x4c3080", "func", 0x4C3080, rust="Witness"),
        S("layout:0x284fd0", "func", 0x284FD0, rust="Witness"),
        S("layout:0x4bd700", "func", 0x4BD700, rust="Witness"),
        S("layout:0xdf490", "func", 0xDF490, rust="Witness"),
        S("layout:0x4bf040", "func", 0x4BF040, rust="Witness"),
        S("layout:0x10cd5a0", "func", 0x10CD5A0, rust="Witness"),
        S("layout:0x10cce70", "func", 0x10CCE70, rust="Witness"),
        S("layout:0x9e7a00", "func", 0x9E7A00, rust="Witness"),
    ],
}

# the portable layout table each old build uses (native/src/portable.rs LAYOUT_BUILD)
OLD_LAYOUT = {0x6AC50427: "40420", 0x6AD0E4E5: "40420"}


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


# --------------------------------------------------------------------------- main

def main() -> int:
    global IMAGE_SIZE
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("old")
    ap.add_argument("new")
    ap.add_argument("--out", help="also write the report to <DIR>/port-<old>-<new>.txt")
    a = ap.parse_args()
    old, new = Image(a.old), Image(a.new)
    IMAGE_SIZE = min(old.pe.OPTIONAL_HEADER.SizeOfImage, new.pe.OPTIONAL_HEADER.SizeOfImage)
    sites = OLD_SITES.get(old.timestamp)
    if sites is None:
        sys.exit(f"the old executable (timestamp {old.timestamp:#x}) is not a build this tool has the sites of")
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
    layout = OLD_LAYOUT.get(old.timestamp) if nwit == len(wit) else None
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
