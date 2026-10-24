#!/usr/bin/env python3
"""find_sites.py: turns a TransportFever3.exe into a `build_table::BuildAddrs` row.

The executable is read as data, never executed. For every hook site the native module needs
(`native/src/build_table.rs`) the tool locates the RVA by up to three independent methods (a known
table keyed by PE timestamp, a byte signature with wildcards, a string reference or the code around
the site), reports how it was found and with what confidence, extracts the prologue the detour will
steal, checks it is safe to relocate, and prints a ready-to-paste Rust literal.

Besides the three hook sites it finds the engine helpers the hold path calls (`command_move`,
`command_destroy`, `handle_destroy`), checks the engine layout facts the module relies on (the
command-kind byte offset, sizeof(Command)), walks the command dispatcher to name each command kind,
and verifies that the list of interface tools whose commands may be held (`ui_add_sites`) names only
direct calls to `CommandList::Add`.

Hook sites located with findings from MPFever (MIT, dkdknight): the per-build RVAs for game builds
40408 and 40420, the prologue bytes and the identity of each function come from its
`native/mpfever_native.cpp`. This tool is a fresh implementation.

Usage:  python -I find_sites.py <TransportFever3.exe> [--out DIR] [--proxy native/src/proxy.rs]
"""

from __future__ import annotations

import argparse
import bisect
import datetime as _dt
import os
import re
import struct
import sys
from dataclasses import dataclass, field
from pathlib import Path

try:
    import pefile
    from capstone import CS_ARCH_X86, CS_MODE_64, Cs
    from capstone.x86 import X86_OP_IMM, X86_OP_MEM, X86_REG_RIP
except ImportError as e:  # pragma: no cover
    sys.exit(f"missing dependency: {e}. Run: pip install -r requirements.txt")

CREDIT = "sites located with findings from MPFever (MIT, dkdknight)"
JMP_ABS_LEN = 14  # detour::JMP_ABS_LEN
MAX_SIG_INSNS = 48

# WinHTTP functions the proxy relays (native/src/proxy.rs). The file is parsed when present; this is
# the fallback and the reference for the current builds.
PROXY_RELAYS_FALLBACK = [
    "WinHttpOpen", "WinHttpConnect", "WinHttpOpenRequest", "WinHttpSetOption", "WinHttpReadData",
    "WinHttpSetStatusCallback", "WinHttpCloseHandle", "WinHttpWriteData", "WinHttpQueryDataAvailable",
    "WinHttpSetTimeouts", "WinHttpQueryHeaders", "WinHttpAddRequestHeaders", "WinHttpSendRequest",
    "WinHttpReceiveResponse",
]


# --------------------------------------------------------------------------- patterns

class Pattern:
    """`"48 8B ?? 05"`: hex bytes and `??` wildcards, matched with a compiled regex."""

    def __init__(self, text: str):
        self.text = " ".join(text.split())
        self.cells: list[int | None] = []
        for tok in self.text.split():
            if tok in ("??", "?"):
                self.cells.append(None)
            elif len(tok) == 2:
                self.cells.append(int(tok, 16))
            else:
                raise ValueError(f"bad pattern token {tok!r}")
        if not self.cells or all(c is None for c in self.cells):
            raise ValueError("pattern has no fixed byte")
        body = b"".join(b"." if c is None else re.escape(bytes([c])) for c in self.cells)
        self.rx = re.compile(b"(?=" + body + b")", re.S)

    def __len__(self) -> int:
        return len(self.cells)

    def matches_at(self, hay: bytes, at: int) -> bool:
        if at < 0 or at + len(self.cells) > len(hay):
            return False
        return all(c is None or hay[at + i] == c for i, c in enumerate(self.cells))

    def find_all(self, hay: bytes, lo: int = 0, hi: int | None = None) -> list[int]:
        return [m.start() for m in self.rx.finditer(hay, lo, len(hay) if hi is None else hi)]


# --------------------------------------------------------------------------- image

@dataclass
class Func:
    begin: int
    end: int


class Image:
    """A PE32+ file read as data: headers, `.text`, the other sections, the unwind table."""

    def __init__(self, path: str):
        self.path = path
        self.pe = pefile.PE(path, fast_load=True)
        D = pefile.DIRECTORY_ENTRY
        self.pe.parse_data_directories(directories=[D["IMAGE_DIRECTORY_ENTRY_IMPORT"], D["IMAGE_DIRECTORY_ENTRY_RESOURCE"]])
        if self.pe.OPTIONAL_HEADER.Magic != 0x20B:
            sys.exit("not a PE32+ (64-bit) executable")
        self.data: bytes = self.pe.__data__
        self.timestamp: int = self.pe.FILE_HEADER.TimeDateStamp
        self.image_base: int = self.pe.OPTIONAL_HEADER.ImageBase
        self.sections = {s.Name.rstrip(b"\0").decode("latin1"): s for s in self.pe.sections}
        text = self.sections.get(".text") or sys.exit("no .text section")
        self.text_rva: int = text.VirtualAddress
        self.text_size: int = text.Misc_VirtualSize
        self.text: bytes = self.data[text.PointerToRawData: text.PointerToRawData + min(text.SizeOfRawData, text.Misc_VirtualSize)]
        self.funcs = self._load_pdata()
        self._func_starts = [f.begin for f in self.funcs]
        self.md = Cs(CS_ARCH_X86, CS_MODE_64)
        self.md.detail = True

    # -- sections and addresses
    def rva_to_off(self, rva: int) -> int | None:
        for s in self.pe.sections:
            if s.VirtualAddress <= rva < s.VirtualAddress + max(s.Misc_VirtualSize, s.SizeOfRawData):
                off = rva - s.VirtualAddress
                return s.PointerToRawData + off if off < s.SizeOfRawData else None
        return None

    def text_at(self, rva: int, n: int) -> bytes:
        o = rva - self.text_rva
        return self.text[o: o + n] if 0 <= o < len(self.text) else b""

    def in_text(self, rva: int) -> bool:
        return self.text_rva <= rva < self.text_rva + len(self.text)

    def find_bytes(self, needle: bytes, exclude_text: bool = True) -> list[int]:
        """RVAs of every occurrence of `needle` in the file's sections (data sections by default)."""
        out = []
        for s in self.pe.sections:
            if exclude_text and s.VirtualAddress == self.text_rva:
                continue
            blob = self.data[s.PointerToRawData: s.PointerToRawData + s.SizeOfRawData]
            i = blob.find(needle)
            while i != -1:
                out.append(s.VirtualAddress + i)
                i = blob.find(needle, i + 1)
        return out

    # -- unwind table: function bounds
    def _load_pdata(self) -> list[Func]:
        pd = self.sections.get(".pdata")
        if pd is None:
            return []
        blob = self.data[pd.PointerToRawData: pd.PointerToRawData + min(pd.SizeOfRawData, pd.Misc_VirtualSize)]
        raw = [struct.unpack_from("<III", blob, i) for i in range(0, len(blob) - 11, 12)]
        by_begin = {}
        for begin, end, unwind in raw:
            if not begin:
                continue
            start = self._primary_begin(begin, unwind, depth=0)
            f = by_begin.setdefault(start, Func(start, end))
            f.end = max(f.end, end)
        return sorted(by_begin.values(), key=lambda f: f.begin)

    def _primary_begin(self, begin: int, unwind_rva: int, depth: int) -> int:
        """Follows UNW_FLAG_CHAININFO to the primary RUNTIME_FUNCTION of a split function."""
        if depth > 8:
            return begin
        off = self.rva_to_off(unwind_rva)
        if off is None or off + 4 > len(self.data):
            return begin
        flags = self.data[off] >> 3
        if not flags & 4:
            return begin
        codes = self.data[off + 2]
        chain = off + 4 + ((codes + 1) & ~1) * 2
        if chain + 12 > len(self.data):
            return begin
        pb, _pe, pu = struct.unpack_from("<III", self.data, chain)
        return self._primary_begin(pb, pu, depth + 1)

    def func_containing(self, rva: int) -> Func | None:
        i = bisect.bisect_right(self._func_starts, rva) - 1
        while i >= 0:
            f = self.funcs[i]
            if f.begin <= rva < f.end:
                return f
            if f.end <= rva and i < len(self.funcs) - 1:
                break
            i -= 1
        return None

    # -- disassembly
    def disasm(self, rva: int, n: int):
        return list(self.md.disasm(self.text_at(rva, n), self.image_base + rva))

    def rip_refs_to(self, target_rva: int) -> list[int]:
        """RVAs of instructions in .text whose rip-relative memory operand addresses `target_rva`.

        Scans the encodings that reference constants: `lea r64,[rip+d]`, `mov r,[rip+d]`, `movzx`, and the
        SSE/AVX loads the compiler uses to copy short strings inline (`movups/vmovups xmm,[rip+d]`)."""
        rip_modrm = b"[\x05\x0d\x15\x1d\x25\x2d\x35\x3d]"
        forms = [
            re.compile(b"[\x48\x4c]\x8d" + rip_modrm, re.S),                   # lea
            re.compile(b"(?:[\x48\x4c\x44\x41\x49]?)\x8b" + rip_modrm, re.S),  # mov r32/r64, [rip+d]
            re.compile(b"(?:[\x48\x4c\x44]?)\x0f\xb6" + rip_modrm, re.S),       # movzx r, byte [rip+d]
            re.compile(b"\x0f\x10" + rip_modrm, re.S),                          # movups xmm, [rip+d]
            re.compile(b"\xc5[\xf8\xfa\xf9]\x10" + rip_modrm, re.S),            # vmovups/vmovss/vmovupd xmm, [rip+d]
        ]
        hits = set()
        for rx in forms:
            for m in rx.finditer(self.text):
                i = m.end()
                if i + 4 > len(self.text):
                    continue
                disp = struct.unpack_from("<i", self.text, i)[0]
                if self.text_rva + i + 4 + disp == target_rva:
                    hits.add(self.text_rva + m.start())
        return sorted(hits)

    def direct_callers(self, target_rva: int) -> list[int]:
        """RVAs of the byte after every `call rel32` in .text that reaches `target_rva` (the return addresses)."""
        out = []
        text = self.text
        i = text.find(b"\xE8")
        while i != -1:
            if i + 5 <= len(text) and self.text_rva + i + 5 + struct.unpack_from("<i", text, i + 1)[0] == target_rva:
                out.append(self.text_rva + i + 5)
            i = text.find(b"\xE8", i + 1)
        return out

    def cstring_at(self, rva: int, limit: int = 200) -> str | None:
        """A printable ASCII string of at least 3 characters at `rva`, or None."""
        off = self.rva_to_off(rva)
        if off is None:
            return None
        raw = self.data[off: off + limit]
        end = raw.find(b"\0")
        if end < 3 or not all(32 <= c < 127 for c in raw[:end]):
            return None
        return raw[:end].decode("ascii")

    def strings_in(self, func: Func) -> list[str]:
        """The strings a function loads with `lea reg,[rip+d]`, in code order."""
        out, cur = [], func.begin
        while cur < func.end:
            ins = self.disasm(cur, 16)
            if not ins:
                cur += 1
                continue
            i = ins[0]
            if i.mnemonic == "lea" and "rip" in i.op_str:
                s = self.cstring_at(cur + i.size + i.disp)
                if s:
                    out.append(s)
            cur += i.size
        return out

    # -- metadata
    def winhttp_imports(self) -> list[str] | None:
        for imp in getattr(self.pe, "DIRECTORY_ENTRY_IMPORT", []):
            if imp.dll.lower() == b"winhttp.dll":
                return [i.name.decode() if i.name else f"#{i.ordinal}" for i in imp.imports]
        return None

    def version_resource(self) -> str | None:
        """ProductVersion/FileVersion from VS_VERSIONINFO, when the executable has one."""
        for fi in getattr(self.pe, "FileInfo", []) or []:
            for entry in fi:
                if getattr(entry, "Key", b"") == b"StringFileInfo":
                    for st in entry.StringTable:
                        for k in (b"ProductVersion", b"FileVersion"):
                            if k in st.entries:
                                return st.entries[k].decode(errors="replace")
        if hasattr(self.pe, "VS_FIXEDFILEINFO") and self.pe.VS_FIXEDFILEINFO:
            f = self.pe.VS_FIXEDFILEINFO[0]
            return f"{f.FileVersionMS >> 16}.{f.FileVersionMS & 0xFFFF}.{f.FileVersionLS >> 16}.{f.FileVersionLS & 0xFFFF}"
        return None

    def build_string(self) -> str | None:
        """The game's own build number: `.rdata` holds `transportfever3\\0`, `64-bit\\0`, then the digits."""
        for rva in self.find_bytes(b"transportfever3\0"):
            off = self.rva_to_off(rva)
            if off is None:
                continue
            m = re.match(rb"transportfever3\0+(?:64|32)-bit\0+(\d{4,8})\0", self.data[off: off + 64])
            if m:
                return m.group(1).decode()
        return None


# --------------------------------------------------------------------------- sites

@dataclass
class StringAnchor:
    """A string the function references: found in the data sections, then its rip-relative users in .text,
    then the enclosing function from the unwind table."""
    text: bytes
    note: str


@dataclass
class CallerIdiom:
    """A byte regex over .text for the argument set-up every caller of the function shares, ending on the
    `E8` of the call; the call targets are tallied and the function is the target with the most votes."""
    regex: bytes
    min_votes: int
    note: str


@dataclass
class InsideParent:
    """A direct call found inside another, already located function: `regex` runs over the parent's bytes and its
    group 1 is the 4-byte displacement of an `E8` call. Every match must reach the same target."""
    parent: str
    regex: bytes
    note: str


@dataclass
class AfterCallers:
    """The function that the code right after every direct call to another located function calls in turn
    (`call parent; lea rcx,[..]; call X`). Tallied over all direct callers; the top target must hold a clear majority."""
    parent: str
    min_votes: int
    note: str


@dataclass
class Site:
    field: str
    kind: str  # "detour": the function start is stolen; "patch": bytes are rewritten in place; "call": only called
    what: str
    known: dict[int, int]            # PE timestamp -> RVA, from MPFever's BUILDS[] (credited)
    seed: str                        # bytes MPFever expects at the site: verifies a known RVA, not unique on its own
    signature: str | None            # derived by this tool from `signature_from`, meant to be unique in .text
    signature_from: int = 0          # PE timestamp of the build the signature was derived from (self-matches are not evidence)
    anchor: StringAnchor | CallerIdiom | InsideParent | AfterCallers | None = None
    patch_len: int = 0               # "patch" sites: the exact region the patch rewrites
    replacement: object = None       # "patch" sites: bytes -> bytes, the rewritten region built from the expected one


def script_pool_replacement(expected: bytes) -> bytes:
    """The script-pool region with the shared-pool branch removed and the worker count forced to 0, which makes the
    engine fall back to one worker: `cmp eax,8; nop6; mov [rsp+20h],1; xor eax,eax; nop3`. Same length as `expected`.
    Finding from MPFever (MIT, dkdknight): PatchScriptPool."""
    if (len(expected) != 22 or expected[:3] != bytes.fromhex("83F808") or expected[3:5] != bytes.fromhex("0F8D")
            or expected[9:17] != bytes.fromhex("C744242001000000") or expected[17] != 0xE8):
        raise ValueError("the script-pool region has an unexpected shape")
    return expected[:3] + bytes.fromhex("660F1F440000") + expected[9:17] + bytes.fromhex("31C00F1F00")


# From MPFever (MIT, dkdknight) native/mpfever_native.cpp: BUILDS[] rows 40408 (0x6ab69fe5) and 40420
# (0x6ac50427), the P_ADD/P_APPLY prologues, the script-pool byte pattern and the identity of each function.
# Signatures were derived by this tool from build 40420 (tools/addr/out/6ac50427.txt); every run re-derives
# them and prints the result, so a newer build's signatures can be pasted here once it has been verified.
SITES: list[Site] = [
    Site(
        field="command_list_add",
        kind="detour",
        what="CommandList::Add(list, out, cmd, done, progress): every engine command enters the queue here",
        known={0x6AB69FE5: 0x9D29C0, 0x6AC50427: 0x9D4040},
        seed="40 55 53 56 57 41 54 41 55 41 56 41 57 48 8D 6C 24 98",
        signature="40 55 53 56 57 41 54 41 55 41 56 41 57 48 8D 6C 24 98 48 81 EC ?? ?? ?? ?? 48 8B 05 ?? ?? ?? ?? "
                  "48 33 C4 48 89 45 50 49 8B F1 4D 8B F8",
        signature_from=0x6AC50427,
        # Every UI tool queues its build the same way: the `out` handle and the done/progress callbacks are set
        # up as `lea rcx,[..]; mov [rsp+20h],rcx; lea r9,[..]; mov r8,rax; lea rdx,[..]; mov rcx,<list>; call Add`.
        anchor=CallerIdiom(
            rb"\x48\x89\x4C\x24\x20\x4C\x8D.{2,5}?\x4C\x8B\xC0\x48\x8D.{2,5}?\x48\x8B[\xC8-\xCF]\xE8",
            min_votes=8,
            note="UI tools' `make_cmd; CommandList::Add(list, &out, cmd, done, progress)` argument idiom",
        ),
    ),
    Site(
        field="apply_command",
        kind="detour",
        what='the simulation thread\'s "Apply Command" task: the commands one simulation iteration applies',
        known={0x6AB69FE5: 0x9E1C10, 0x6AC50427: 0x9E33B0},
        seed="48 89 5C 24 18 48 89 74 24 20 55 57 41 54",
        signature="48 89 5C 24 18 48 89 74 24 20 55 57 41 54 41 56 41 57 48 8D 6C 24 C9 48 81 EC ?? ?? ?? ?? "
                  "48 8B 05 ?? ?? ?? ?? 48 33 C4 48 89 45 2F 4C 8B F2 4C 8B F9",
        signature_from=0x6AC50427,
        anchor=StringAnchor(b"Simulation Thread: Apply Command\0", "profiler tag passed by lea rcx at the function's start"),
    ),
    Site(
        field="script_pool",
        kind="patch",
        what="game-script update's thread pool construction: `cmp eax,8 / jge shared-pool / threads=1 / call "
             "hardware_concurrency`; patched to always build the dedicated pool with one worker",
        known={0x6AB69FE5: 0xAAEC2C, 0x6AC50427: 0xAB00FC},
        seed="83 F8 08 0F 8D ?? ?? ?? ?? C7 44 24 20 01 00 00 00 E8",
        signature="83 F8 08 0F 8D ?? ?? ?? ?? C7 44 24 20 01 00 00 00 E8 ?? ?? ?? ??",
        signature_from=0x6AC50427,
        anchor=StringAnchor(b"GameScriptSystem Pool\0", "pool name copied inline (vmovups xmm0,[rip+d]) a few instructions later"),
        patch_len=22,
        replacement=script_pool_replacement,
    ),
    # The engine helpers the hold path calls (native/src/engine.rs): never hooked, only called, so they carry a
    # whole-instruction prologue that is checked before the first call. Addresses from MPFever's BUILDS[] (move,
    # dtor, handleDtor), each confirmed here by how `CommandList::Add` itself uses it.
    Site(
        field="command_move",
        kind="call",
        what="Command::Command(Command&&): moves the 0x38-byte command (payload pointer, vector, weak progress, "
             "result byte) and empties the source",
        known={0x6AB69FE5: 0x9CEDB0, 0x6AC50427: 0x9D0430},
        seed="48 8B 02 45 33 D2 4C 89 12 48 89 01 48 8B 42 08",
        signature="48 8B 02 45 33 D2 4C 89 12 48 89 01 48 8B 42 08",
        signature_from=0x6AC50427,
        anchor=InsideParent("command_list_add", rb"\xE8(....)\x48\x83\x43\x08\x38",
                            "the call right before `add qword [list+8], 0x38` that appends the command to the queue vector"),
    ),
    Site(
        field="command_destroy",
        kind="call",
        what="Command::~Command(): releases the weak progress pointer, the vector and the payload (0x9c0 bytes, "
             "variant index at +0x9b8)",
        known={0x6AB69FE5: 0x9CEFF0, 0x6AC50427: 0x9D0670},
        seed="40 53 48 83 EC 20 48 8B D9 48 8B 49 28 48 85 C9",
        signature="40 53 48 83 EC 20 48 8B D9 48 8B 49 28 48 85 C9 74 ?? B8 ?? ?? ?? ?? F0 0F C1 41 0C 83 F8 01 75 ?? "
                  "48 8B 01 FF 50 08 48 8B 4B 08 48 85 C9",
        signature_from=0x6AC50427,
        anchor=InsideParent("command_list_add", rb"\x49\x8B\xCF\xE8(....)\x90",
                            "`mov rcx,r15; call X; nop`, which Add runs on its moved-from `cmd` argument"),
    ),
    Site(
        field="handle_destroy",
        kind="call",
        what="destructor of the 16-byte handle Add writes through `out`; a zeroed handle is a no-op",
        known={0x6AB69FE5: 0x30393D0, 0x6AC50427: 0x3040C90},
        seed="40 53 48 83 EC 20 48 8B 19 48 85 DB 74 30 48 8B 4B 08",
        signature="40 53 48 83 EC 20 48 8B 19 48 85 DB 74 ?? 48 8B 4B 08 48 85 C9 74 ?? B8 ?? ?? ?? ?? F0 0F C1 41 0C "
                  "83 F8 01 75 ?? 48 8B 01 FF 50 10 BA 10 00 00 00",
        signature_from=0x6AC50427,
        anchor=AfterCallers("command_list_add", 20,
                            "every caller of Add destroys the handle with `lea rcx,[..]; call X` straight after the call"),
    ),
]


# --------------------------------------------------------------------------- locating

@dataclass
class Located:
    rva: int | None
    method: str
    confidence: str          # high | medium | low | none
    notes: list[str] = field(default_factory=list)
    candidates: dict[str, list[int]] = field(default_factory=dict)


def locate(img: Image, site: Site, located: dict[str, int] | None = None) -> Located:
    """`located` maps the field names of the sites already found to their RVAs: the anchors of the helper sites
    work from the function of another site."""
    located = located or {}
    notes: list[str] = []
    cands: dict[str, list[int]] = {}
    seed = Pattern(site.seed)

    # 1. known table for this exact timestamp, verified by the seed bytes
    hint = site.known.get(img.timestamp)
    if hint is not None:
        if seed.matches_at(img.text, hint - img.text_rva):
            cands["known"] = [hint]
            notes.append(f"known table (MPFever BUILDS[]) has {hint:#x} for timestamp {img.timestamp:#x}; seed bytes match there")
        else:
            notes.append(f"known table has {hint:#x} for this timestamp but the seed bytes do NOT match there")
            cands["known"] = []
    else:
        notes.append("timestamp not in the known table: a build newer than 40420 (or older than 40408)")

    # 2. generic signature, unique in .text
    self_check = bool(site.signature) and site.signature_from == img.timestamp
    if site.signature:
        sig = Pattern(site.signature)
        hits = [h + img.text_rva for h in sig.find_all(img.text)]
        cands["signature"] = hits
        if len(hits) == 1:
            notes.append(f"signature matches once in .text: {hits[0]:#x}"
                         + (" (derived from this very build: a consistency check, not independent evidence)" if self_check else ""))
        elif not hits:
            notes.append("signature does not match anywhere in .text")
        else:
            notes.append(f"signature is ambiguous: {len(hits)} matches " + ", ".join(f"{h:#x}" for h in hits[:8]))

    # 3. anchor: a string the function references, or the idiom its callers share
    anchor_kind = ""
    if isinstance(site.anchor, StringAnchor):
        anchor_kind = "anchored from string reference"
        strs = img.find_bytes(site.anchor.text)
        refs = sorted({r for s in strs for r in img.rip_refs_to(s)})
        funcs = {f.begin: f for r in refs if (f := img.func_containing(r))}
        if not strs:
            notes.append(f"anchor string {site.anchor.text!r} not found")
        elif not refs:
            notes.append(f"anchor string at {', '.join(f'{s:#x}' for s in strs)} has no rip-relative reference in .text")
        else:
            notes.append(f"anchor string {site.anchor.text!r} referenced at {', '.join(f'{r:#x}' for r in refs)} -> function(s) "
                         + ", ".join(f"{f.begin:#x}..{f.end:#x}" for f in funcs.values()))
        anchored: list[int] = []
        for f in funcs.values():
            if site.kind == "detour":
                anchored.append(f.begin)
            else:
                inner = [h + img.text_rva for h in seed.find_all(img.text, f.begin - img.text_rva, f.end - img.text_rva)]
                if len(inner) == 1:
                    anchored.append(inner[0])
                    notes.append(f"seed pattern found once inside the anchored function: {inner[0]:#x}")
                else:
                    notes.append(f"seed pattern found {len(inner)} times inside the anchored function {f.begin:#x}")
        cands["anchor"] = sorted(set(anchored))
    elif isinstance(site.anchor, CallerIdiom):
        anchor_kind = "anchored from callers' idiom"
        votes: dict[int, int] = {}
        for m in re.finditer(site.anchor.regex, img.text, re.S):
            i = m.end()
            if i + 4 <= len(img.text):
                rel = struct.unpack_from("<i", img.text, i)[0]
                tgt = img.text_rva + i + 4 + rel
                votes[tgt] = votes.get(tgt, 0) + 1
        ranked = sorted(votes.items(), key=lambda kv: -kv[1])
        if not ranked:
            notes.append(f"caller idiom ({site.anchor.note}) matches nowhere")
            cands["anchor"] = []
        else:
            top, n = ranked[0]
            runner = ranked[1][1] if len(ranked) > 1 else 0
            notes.append(f"caller idiom ({site.anchor.note}): {sum(votes.values())} call sites, "
                         + ", ".join(f"{t:#x} x{c}" for t, c in ranked[:4]))
            if n >= site.anchor.min_votes and n > 2 * runner and img.func_containing(top) and img.func_containing(top).begin == top:
                cands["anchor"] = [top]
            else:
                notes.append(f"caller idiom inconclusive (top target {top:#x} has {n} votes, needs >= {site.anchor.min_votes} and a clear majority)")
                cands["anchor"] = []
    elif isinstance(site.anchor, InsideParent):
        anchor_kind = f"anchored inside {site.anchor.parent}"
        parent = located.get(site.anchor.parent)
        pf = img.func_containing(parent) if parent is not None else None
        if pf is None:
            notes.append(f"parent site {site.anchor.parent} is not located: cannot anchor")
            cands["anchor"] = []
        else:
            body = img.text_at(pf.begin, pf.end - pf.begin)
            targets = [pf.begin + m.start(1) + 4 + struct.unpack("<i", m.group(1))[0]
                       for m in re.finditer(site.anchor.regex, body, re.S)]
            reached = sorted(set(targets))
            notes.append(f"{len(targets)} call(s) inside {site.anchor.parent} match ({site.anchor.note}): "
                         + (", ".join(f"{t:#x}" for t in reached) or "none"))
            cands["anchor"] = reached if len(reached) == 1 else []
    elif isinstance(site.anchor, AfterCallers):
        anchor_kind = f"anchored from what the callers of {site.anchor.parent} call next"
        parent = located.get(site.anchor.parent)
        votes = {}
        for ret in (img.direct_callers(parent) if parent is not None else []):
            nxt = img.disasm(ret, 32)[:2]
            if (len(nxt) == 2 and nxt[0].mnemonic == "lea" and nxt[0].op_str.startswith("rcx,")
                    and nxt[1].mnemonic == "call" and nxt[1].op_str.startswith("0x")):
                tgt = int(nxt[1].op_str, 16) - img.image_base
                votes[tgt] = votes.get(tgt, 0) + 1
        ranked = sorted(votes.items(), key=lambda kv: -kv[1])
        if not ranked:
            notes.append(f"no caller of {site.anchor.parent} is followed by `lea rcx; call` ({site.anchor.note})")
            cands["anchor"] = []
        else:
            top, n = ranked[0]
            runner = ranked[1][1] if len(ranked) > 1 else 0
            notes.append(f"{sum(votes.values())} caller(s) of {site.anchor.parent} are followed by `lea rcx; call` ({site.anchor.note}): "
                         + ", ".join(f"{t:#x} x{c}" for t, c in ranked[:4]))
            if n >= site.anchor.min_votes and n > 2 * runner:
                cands["anchor"] = [top]
            else:
                notes.append(f"inconclusive (top target {top:#x} has {n} votes, needs >= {site.anchor.min_votes} and a clear majority)")
                cands["anchor"] = []

    # verdict
    single = {k: v[0] for k, v in cands.items() if len(v) == 1}
    distinct = set(single.values())
    if len(distinct) > 1:
        return Located(None, "CONFLICT: " + ", ".join(f"{k}={v:#x}" for k, v in single.items()), "none", notes, cands)
    if not distinct:
        return Located(None, "not found", "none", notes, cands)
    rva = distinct.pop()
    agreeing = [k for k in ("known", "signature", "anchor") if k in single]
    independent = [k for k in agreeing if k == "anchor" or (k == "signature" and not self_check)]
    if "known" in agreeing and independent:
        conf = "high"
    elif len(independent) >= 2:
        conf = "high"
    elif "known" in agreeing:
        conf = "low"
        notes.append("only the known RVA and its seed bytes agree; the seed prologue is not unique in .text")
    elif independent:
        conf = "medium"
    else:
        conf = "low"
        notes.append("only the self-derived signature matches; nothing independent confirms the site")
    method = " + ".join({"known": "known RVA for this timestamp (seed verified)",
                         "signature": "signature unique in .text" + (" (self-check)" if self_check else ""),
                         "anchor": anchor_kind}[k] for k in agreeing)
    return Located(rva, method, conf, notes, cands)


# --------------------------------------------------------------------------- prologue and signature

def is_rip_relative(ins) -> bool:
    return any(op.type == X86_OP_MEM and op.mem.base == X86_REG_RIP for op in ins.operands)


def is_relative_branch(ins) -> bool:
    names = {ins.group_name(g) for g in ins.groups}
    return "branch_relative" in names or ins.mnemonic in ("loop", "loope", "loopne", "jrcxz", "jecxz")


def fmt_ins(img: Image, ins) -> str:
    return f"{ins.address - img.image_base:08x}  {ins.bytes.hex(' '):<28} {ins.mnemonic} {ins.op_str}".rstrip()


def take_prologue(img: Image, rva: int, min_len: int, exact: bool) -> tuple[list, int, str | None]:
    """Instructions from `rva` until `min_len` bytes are covered (exactly `min_len` when `exact`)."""
    insns, total = [], 0
    for ins in img.disasm(rva, min_len + 32):
        insns.append(ins)
        total += ins.size
        if total >= min_len:
            break
    if total < min_len:
        return insns, total, f"could not decode {min_len} bytes"
    if exact and total != min_len:
        return insns, total, f"instruction boundary at +{total}, not +{min_len}"
    return insns, total, None


def safety(insns) -> list[str]:
    bad = []
    for ins in insns:
        if is_rip_relative(ins):
            bad.append(f"rip-relative operand: {ins.mnemonic} {ins.op_str}")
        if is_relative_branch(ins):
            bad.append(f"relative branch: {ins.mnemonic} {ins.op_str}")
    return bad


def wildcard_insn(ins) -> list[int | None]:
    """The instruction's bytes with its build-dependent parts replaced by wildcards: branch targets,
    rip-relative and other 32-bit displacements, 32/64-bit immediates (small constants are kept)."""
    cells: list[int | None] = list(ins.bytes)
    if is_relative_branch(ins):
        for k in range(ins.imm_offset, ins.imm_offset + ins.imm_size):
            cells[k] = None
        return cells
    if ins.disp_size >= 4:
        for k in range(ins.disp_offset, ins.disp_offset + ins.disp_size):
            cells[k] = None
    if ins.imm_size >= 4:
        imm = next((op.imm for op in ins.operands if op.type == X86_OP_IMM), None)
        stack_adjust = ins.mnemonic in ("sub", "add") and ins.op_str.startswith("rsp,")
        if imm is None or abs(imm) >= 0x100 or stack_adjust:
            for k in range(ins.imm_offset, ins.imm_offset + ins.imm_size):
                cells[k] = None
    return cells


def derive_signature(img: Image, rva: int) -> tuple[str | None, int, str]:
    """Extends a wildcarded pattern instruction by instruction until it matches once in .text, then adds one
    instruction of margin. Returns (pattern text, match count, note)."""
    cells: list[int | None] = []
    insns = img.disasm(rva, 16 * MAX_SIG_INSNS)[:MAX_SIG_INSNS]
    unique_at = None
    for i, ins in enumerate(insns):
        cells.extend(wildcard_insn(ins))
        if any(c is not None for c in cells):
            text = " ".join("??" if c is None else f"{c:02X}" for c in cells)
            n = len(Pattern(text).find_all(img.text))
            if n == 1:
                unique_at = i
                if i + 1 < len(insns):
                    cells.extend(wildcard_insn(insns[i + 1]))
                text = " ".join("??" if c is None else f"{c:02X}" for c in cells)
                return text, 1, f"unique after {i + 1} instructions ({len(cells)} bytes, one instruction of margin)"
            if n == 0:
                return None, 0, "derived pattern does not match its own site (decoder disagreement)"
    text = " ".join("??" if c is None else f"{c:02X}" for c in cells)
    return text, len(Pattern(text).find_all(img.text)), f"not unique within {len(insns)} instructions"


# --------------------------------------------------------------------------- report

def proxy_relays(path: Path | None) -> tuple[list[str], str]:
    if path and path.is_file():
        names = re.findall(r"relay!\(\s*(WinHttp\w+)\s*\(", path.read_text(encoding="utf-8"))
        if names:
            repo = Path(__file__).resolve().parents[2]
            shown = path.resolve()
            if shown.is_relative_to(repo):
                shown = shown.relative_to(repo)
            return names, shown.as_posix()
    return PROXY_RELAYS_FALLBACK, "built-in list (proxy.rs not found)"


def rust_bytes(b: bytes) -> str:
    return "&[" + ", ".join(f"0x{x:02X}" for x in b) + "]"


# The interface's construction tools whose `CommandList::Add` calls the hold path may hold, as the RVA of the byte
# after the `call` (the return address the hook sees). From MPFever's native/mpfever_native.cpp UI_SITES_40420
# (MIT, dkdknight), who played with them in the game; the terrain tool's call is left out on purpose (holding its
# edit crashed the tool). The tool verifies that every entry is a direct call to `command_list_add`.
UI_ADD_SITES_KNOWN: dict[int, list[int]] = {
    0x6AC50427: [
        0x4D396F,   # UI::Bulldozer::Apply
        0x51B56C,   # UI::ConstructionBuilder::MousePressed
        0x528504,   # a tool without a source-file string
        0x537EE5, 0x538649, 0x5387CD,   # UI::LaneModifier::Apply
        0x542EBA,   # UI::ModuleBuilder::MousePressed
        0x5891B4,   # UI::StreetBuilder::UpdateEngine
        0x5948DD,   # a street-terminal tool (stops, signals, waypoints) without a source-file string
        0x5BF08D,   # UI::TrackModifier::Build
        0x28A3024, 0x28A3871,   # the entity window's builtins
    ],
    # build 40423: the same twelve calls, carried over by port_build.py (same tool functions, same offsets inside them)
    0x6AD0E4E5: [
        0x4D479F, 0x51C3AC, 0x529334, 0x538D15, 0x539479, 0x5395FD, 0x543CEA, 0x589FC4, 0x5956ED, 0x5BFE9D,
        0x28AA534, 0x28AAD81,
    ],
    # build 40425: the same twelve calls at the same RVAs as 40423, carried over by port_build.py
    0x6AD9CE10: [
        0x4D479F, 0x51C3AC, 0x529334, 0x538D15, 0x539479, 0x5395FD, 0x543CEA, 0x589FC4, 0x5956ED, 0x5BFE9D,
        0x28AA534, 0x28AAD81,
    ],
}

# The command kind the hold path is about, found by the string its handler references.
KIND_ANCHORS: dict[str, str] = {"build_proposal": "WorldBuildProposal"}

CALL_PROLOGUE_LEN = 16  # "call" sites: whole instructions, at least this many bytes, compared before the first call


def source_tags(img: Image, func: Func) -> str:
    """What a function says about itself: the `UI::..` profiler tags and source file names it references."""
    strs = img.strings_in(func)
    files = sorted({s.replace("\\", "/").rsplit("/", 1)[-1] for s in strs if "train_fever" in s})
    tags = []
    for s in strs:
        if (s.startswith("UI::") or "::" in s) and s not in tags and len(s) < 60:
            tags.append(s)
    return ", ".join((tags[:2] + files)[:3]) or "-"


def analyse_layout(img: Image, located: dict[str, int]) -> tuple[list[str], int | None]:
    """Engine layout facts the hold path relies on, each read from two places that must agree.

    - the command-kind byte inside a payload: `movsx rcx, byte [reg+disp32]` in the apply function and again in
      the command destructor, both followed by `inc rcx` (the dispatcher index is kind + 1);
    - sizeof(Command): the `add qword [list+8], imm8` after the move constructor call in `CommandList::Add`."""
    lines: list[str] = []
    kind_offset = None
    apply_f = img.func_containing(located["apply_command"]) if "apply_command" in located else None
    dtor_f = img.func_containing(located["command_destroy"]) if "command_destroy" in located else None
    add_f = img.func_containing(located["command_list_add"]) if "command_list_add" in located else None
    if apply_f and dtor_f:
        a = {struct.unpack("<I", m.group(1))[0] for m in re.finditer(
            rb"\x49\x0F\xBE\x88(....)\x48\xFF\xC1", img.text_at(apply_f.begin, apply_f.end - apply_f.begin), re.S)}
        d = {struct.unpack("<I", m.group(1))[0] for m in re.finditer(
            rb"\x48\x0F\xBE\x8B(....)\x48\x8D\x54\x24.\x48\xFF\xC1", img.text_at(dtor_f.begin, dtor_f.end - dtor_f.begin), re.S)}
        lines.append(f"kind byte offset in a payload: apply_command reads {', '.join(f'{x:#x}' for x in sorted(a)) or 'nothing'}, "
                     f"command_destroy reads {', '.join(f'{x:#x}' for x in sorted(d)) or 'nothing'}")
        if len(a) == 1 and a == d:
            kind_offset = a.pop()
            lines.append(f"  AGREE: kind_offset = {kind_offset:#x}")
        else:
            lines.append("  !!! the two functions do not agree: kind_offset stays None, the counters and the hold stay off")
    else:
        lines.append("kind byte offset: apply_command or command_destroy not located; kind_offset stays None")
    if add_f:
        sizes = {m.group(1)[0] for m in re.finditer(rb"\xE8....\x48\x83\x43\x08(.)", img.text_at(add_f.begin, add_f.end - add_f.begin), re.S)}
        lines.append("sizeof(Command): CommandList::Add advances the queue vector by "
                     + (", ".join(f"{x:#x}" for x in sorted(sizes)) or "nothing found") + " bytes per command (the module's buffers hold 0x40)")
        if sizes != {0x38}:
            lines.append("  !!! expected exactly 0x38")
    return lines, kind_offset


def analyse_kinds(img: Image, located: dict[str, int]) -> tuple[list[str], list[tuple[str, int]], bool]:
    """Walks the command dispatcher's jump table (the callee `apply_command` passes `kind + 1` to) and names each
    case by the profiler tag it starts with or by the first string its handler references. Returns report lines,
    the verified (name, kind) pairs for the build table and whether everything needed was found."""
    lines: list[str] = []
    af = img.func_containing(located["apply_command"]) if "apply_command" in located else None
    if af is None:
        return ["command kinds: apply_command not located"], [], False
    body = img.text_at(af.begin, af.end - af.begin)
    m = re.search(rb"\x49\x0F\xBE\x88....\x48\xFF\xC1\x48\x8D\x55.\xE8(....)", body, re.S)
    if not m:
        return ["command kinds: the dispatcher call in apply_command was not found"], [], False
    disp = af.begin + m.start(1) + 4 + struct.unpack("<i", m.group(1))[0]
    df = img.func_containing(disp)
    if df is None or df.begin != disp:
        return [f"command kinds: {disp:#x} is not a function start"], [], False
    mt = re.search(rb"\x8B\x84\x8A(....)", img.text_at(df.begin, df.end - df.begin), re.S)
    if not mt:
        return [f"command kinds: no jump table in {disp:#x}"], [], False
    table = struct.unpack("<I", mt.group(1))[0]
    off = img.rva_to_off(table)
    if off is None:
        return [f"command kinds: jump table at {table:#x} is not in the file"], [], False
    cases: list[tuple[int, int]] = []
    for i in range(256):
        entry = struct.unpack_from("<I", img.data, off + 4 * i)[0]
        if not df.begin <= entry < df.end:
            break
        cases.append((i - 1, entry))
    names: dict[int, tuple[str, int | None]] = {}
    for kind, entry in cases:
        label, handler = None, None
        for k, ins in enumerate(img.disasm(entry, 40)[:4]):
            if label is None and ins.mnemonic == "lea" and ins.op_str.startswith("rcx, [rip"):
                label = img.cstring_at(entry + sum(x.size for x in img.disasm(entry, 40)[:k + 1]) + ins.disp)
            if handler is None and ins.mnemonic == "call" and ins.op_str.startswith("0x"):
                handler = int(ins.op_str, 16) - img.image_base
        if label is None and handler is not None:
            hf = img.func_containing(handler)
            hs = [s for s in (img.strings_in(hf) if hf else []) if "train_fever" not in s and "{}" not in s]
            label = hs[0] if hs else None
        names[kind] = (label or "-", handler)
    lines.append(f"command kinds: dispatcher {df.begin:#x} (called by apply_command), jump table {table:#x}, {len(cases)} cases "
                 f"(kind -1 is 'none'); the name is the profiler tag the case starts with or the first string its handler references")
    for kind, entry in cases:
        label, handler = names[kind]
        lines.append(f"  kind {kind:3d}  {label:<44} case {entry:#x}" + (f"  handler {handler:#x}" if handler else ""))
    verified: list[tuple[str, int]] = []
    ok = True
    for name, needle in KIND_ANCHORS.items():
        hits = [k for k, (lab, _) in names.items() if lab == needle]
        if len(hits) == 1:
            verified.append((name, hits[0]))
            lines.append(f"  verified: {name} = kind {hits[0]} (its case is named by the string '{needle}')")
        else:
            ok = False
            lines.append(f"  !!! {name}: expected exactly one case named '{needle}', found {len(hits)}: the kind table stays empty")
    return lines, verified, ok


def report_callers(img: Image, located: dict[str, int], ts: int) -> tuple[list[str], list[int], bool]:
    """Every direct caller of `command_list_add` with what its function says about itself, and the check that the
    hold list (MPFever's, credited) only names direct calls to it. Returns report lines, the verified list, ok."""
    add = located.get("command_list_add")
    if add is None:
        return ["callers of command_list_add: not located"], [], False
    known = UI_ADD_SITES_KNOWN.get(ts)
    rets = img.direct_callers(add)
    lines = [f"direct callers of command_list_add: {len(rets)} (the hold list marks the interface's construction tools)"]
    for r in rets:
        f = img.func_containing(r - 5)
        mark = "HOLD" if known and r in known else "    "
        lines.append(f"  {mark} return {r:#09x}  in {f.begin:#x}..{f.end:#x}  {source_tags(img, f) if f else '-'}")
    if known is None:
        lines.append("this build has no hold list yet: ui_add_sites stays empty and the hold hook stays off. Candidates are the "
                     "callers above whose function names a source file under Game\\UI\\Actions; test each one in the game")
        return lines, [], True
    bad = [r for r in known if r not in rets]
    if bad:
        lines.append("  !!! hold list entries that are not direct calls to command_list_add: " + ", ".join(f"{r:#x}" for r in bad))
        return lines, [], False
    lines.append(f"hold list: all {len(known)} entries are direct calls to command_list_add (MPFever's list, MIT, dkdknight)")
    return lines, sorted(known), True


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("exe", help="path to TransportFever3.exe (read only)")
    ap.add_argument("--out", help="also write the report to <DIR>/<timestamp-hex>.txt")
    ap.add_argument("--proxy", help="path to native/src/proxy.rs (default: relative to this script)")
    args = ap.parse_args()

    out: list[str] = []
    p = out.append
    status = 0

    img = Image(args.exe)
    ts = img.timestamp
    when = _dt.datetime.fromtimestamp(ts, _dt.UTC).strftime("%Y-%m-%d %H:%M:%S UTC")
    version = img.version_resource()
    build = img.build_string()
    build_name = build or (version or "unknown")

    p("FeverLink3 address tool: tools/addr/find_sites.py")
    p(f"executable       {os.path.basename(args.exe)} ({os.path.getsize(args.exe):,} bytes)")
    p(f"PE TimeDateStamp {ts:#010x}  ({when})")
    p(f"image base       {img.image_base:#x}")
    p(f".text            RVA {img.text_rva:#x}  size {img.text_size:#x} ({img.text_size:,} bytes)")
    p(f"version resource {version or 'none (the executable carries no VS_VERSIONINFO)'}")
    p(f"game build       {build or 'unknown'}" + ("  (string next to 'transportfever3' in .rdata)" if build else ""))
    p(f"unwind table     {len(img.funcs):,} functions")
    p("")

    # imports
    relays, relay_src = proxy_relays(Path(args.proxy) if args.proxy else Path(__file__).resolve().parents[2] / "native" / "src" / "proxy.rs")
    imports = img.winhttp_imports()
    p("WINHTTP.dll imports")
    if imports is None:
        p("  !!! the executable does not import WINHTTP.dll: the proxy DLL would never be loaded !!!")
        status = 1
    else:
        for name in imports:
            p(f"  {name}" + ("" if name in relays else "   <-- NOT RELAYED by proxy.rs"))
        missing = [n for n in imports if n not in relays]
        extra = [n for n in relays if n not in imports]
        p(f"  {len(imports)} imported, {len(relays)} relayed by {relay_src}")
        if missing:
            p(f"  !!! MISMATCH: the game imports {len(missing)} WinHTTP function(s) proxy.rs does not export: {', '.join(missing)}")
            p("  !!! the game would fail to start with the module in place; add the relay(s) first")
            status = 1
        if extra:
            p(f"  note: proxy.rs relays {len(extra)} function(s) the game does not import (harmless): {', '.join(extra)}")
        if not missing and not extra:
            p("  OK: import list and proxy relays are identical")
    p("")

    # MPFever cross-check
    in_table = any(ts in s.known for s in SITES)
    p("known builds (MPFever BUILDS[], MIT, dkdknight): " + ", ".join(f"{t:#x}" for t in sorted({t for s in SITES for t in s.known})))
    p(f"this build is {'IN' if in_table else 'NOT in'} the known table" + ("; RVAs are cross-checked against it" if in_table else "; sites must be found by signature and anchor"))
    p("")

    # sites
    rows: dict[str, tuple[Located, bytes | None, str]] = {}
    sigs: dict[str, str] = {}
    located: dict[str, int] = {}
    for site in SITES:
        p("=" * 100)
        p(f"site {site.field}  [{site.kind}]")
        p(f"  what: {site.what}")
        loc = locate(img, site, located)
        for n in loc.notes:
            p(f"  - {n}")
        if loc.rva is None:
            p(f"  RESULT: {loc.method}; confidence {loc.confidence}")
            for k, v in loc.candidates.items():
                p(f"    {k}: " + (", ".join(f"{x:#x}" for x in v[:10]) or "none"))
            rows[site.field] = (loc, None, "")
            status = 1
            continue
        located[site.field] = loc.rva
        p(f"  RVA {loc.rva:#x}  (VA {img.image_base + loc.rva:#x})")
        p(f"  located by: {loc.method}")
        p(f"  confidence: {loc.confidence}")
        if ts in site.known:
            agree = site.known[ts] == loc.rva
            p(f"  MPFever table says {site.known[ts]:#x}: {'AGREES' if agree else '!!! DISAGREES !!!'}")
            if not agree:
                status = 1
        f = img.func_containing(loc.rva)
        if f:
            p(f"  enclosing function (unwind table): {f.begin:#x}..{f.end:#x}" + ("" if f.begin == loc.rva or site.kind == "patch" else "  !!! site is not a function start"))
        elif site.kind == "call":
            p("  no unwind-table entry: a leaf function (no stack frame), which is normal for a small helper")

        # prologue
        exact = site.kind == "patch"
        need = site.patch_len if exact else (CALL_PROLOGUE_LEN if site.kind == "call" else JMP_ABS_LEN)
        insns, total, err = take_prologue(img, loc.rva, need, exact)
        p(f"  {'patch region' if exact else 'prologue'} ({'exactly' if exact else 'minimal >='} {need} bytes on an instruction boundary): {total} bytes"
          + (f"  !!! {err}" if err else ""))
        for ins in insns:
            p("    " + fmt_ins(img, ins))
        bad = safety(insns)
        if site.kind == "detour":
            p("  detour safety: " + ("OK (no rip-relative operand, no relative jump/call)" if not bad else "UNSAFE: " + "; ".join(bad)))
            if bad:
                status = 1
        elif site.kind == "call":
            p("  call site: never hooked or patched, so relative operands do not matter; the native module compares these bytes"
              " with the ones in memory before it calls the function")
        else:
            p("  detour safety: " + ("OK" if not bad else "UNSAFE as a detour: " + "; ".join(bad)))
            p("    (patch site: these bytes are rewritten in place, never relocated to a trampoline, so relative"
              " operands are expected here; the table records them only to verify the build before patching)")
        prologue = b"".join(bytes(i.bytes) for i in insns) if not err else None
        p("  bytes: " + (prologue.hex(" ").upper() if prologue else "n/a"))

        # derived signature
        sig, n, note = derive_signature(img, loc.rva)
        p(f"  derived signature: {note}")
        if sig:
            p(f"    {sig}")
            sigs[site.field] = sig
            if site.signature and Pattern(site.signature).text != Pattern(sig).text:
                p("    (differs from the signature stored in the tool; update SITES if this is a newer build)")
        rows[site.field] = (loc, prologue, "")
        if err:
            status = 1

    # engine layout, command kinds, callers of the command list
    p("=" * 100)
    layout_lines, kind_offset = analyse_layout(img, located)
    for ln in layout_lines:
        p(ln)
        if "!!!" in ln:
            status = 1
    p("")
    kind_lines, kinds, kinds_ok = analyse_kinds(img, located)
    for ln in kind_lines:
        p(ln)
    if not kinds_ok:
        status = 1
    p("")
    caller_lines, ui_sites, callers_ok = report_callers(img, located, ts)
    for ln in caller_lines:
        p(ln)
    if not callers_ok:
        status = 1

    # Rust literal
    p("=" * 100)
    p("BuildAddrs row for native/src/build_table.rs")
    p("")
    lit = []
    lit.append(f"    // TransportFever3.exe build {build_name}: PE timestamp {ts:#010x}, linked {when}.")
    lit.append(f"    // {CREDIT}")
    lit.append("    // Generated by tools/addr/find_sites.py; do not edit by hand.")
    lit.append("    BuildAddrs {")
    lit.append(f"        timestamp: {ts:#010x},")
    lit.append(f"        name: \"{build_name}\",")
    rust_type = {"detour": "HookSite", "call": "CallSite"}
    for site in SITES:
        loc, prologue, _ = rows[site.field]
        if loc.rva is None or prologue is None:
            lit.append(f"        // {site.field}: NOT LOCATED ({loc.method}); the feature that needs it stays off")
            lit.append(f"        {site.field}: None,")
            continue
        lit.append(f"        // {site.field}: {loc.method}; confidence {loc.confidence}" + (" (patch site)" if site.kind == "patch" else ""))
        if site.kind == "patch":
            repl = site.replacement(prologue) if callable(site.replacement) else prologue
            lit.append(f"        {site.field}: Some(PatchSite {{ rva: {loc.rva:#x}, expected: {rust_bytes(prologue)}, replacement: {rust_bytes(repl)} }}),")
        else:
            lit.append(f"        {site.field}: Some({rust_type[site.kind]} {{ rva: {loc.rva:#x}, prologue: {rust_bytes(prologue)} }}),")
    lit.append("        // ui_add_sites: return addresses of the construction tools' calls to command_list_add that may be held")
    lit.append("        ui_add_sites: &[" + ", ".join(f"{r:#x}" for r in ui_sites) + "],")
    lit.append("        // kind_offset: the command kind byte inside a payload, read by apply_command and command_destroy")
    lit.append(f"        kind_offset: {'Some(' + format(kind_offset, '#x') + ')' if kind_offset is not None else 'None'},")
    lit.append("        // kinds: command kinds the control link may name; the dispatcher case is named by its handler's string")
    lit.append("        kinds: &[" + ", ".join(f'("{n}", {k})' for n, k in kinds) + "],")
    lit.append("    },")
    out.extend(lit)
    p("")
    p("Generic signatures (wildcards for build-dependent displacements, immediates and branch targets); paste into")
    p("SITES[...].signature in find_sites.py so the next build can be found without the hint RVA:")
    for k, v in sigs.items():
        p(f"  {k}:")
        p(f"    {v}")
    p("")
    low = [k for k, (loc, pr, _) in rows.items() if loc.rva is None or pr is None or loc.confidence in ("low", "none")]
    if low:
        p("!!! NOT LOCATED WITH CONFIDENCE: " + ", ".join(low) + ". Do not paste these; investigate first.")
        status = 1
    else:
        p("all sites located with medium or high confidence")

    text = "\n".join(out) + "\n"
    sys.stdout.write(text)
    if args.out:
        os.makedirs(args.out, exist_ok=True)
        dest = os.path.join(args.out, f"{ts:08x}.txt")
        with open(dest, "w", encoding="utf-8", newline="\n") as fh:
            fh.write(text)
        sys.stdout.write(f"\nwritten to {dest}\n")
    return status


if __name__ == "__main__":
    sys.exit(main())
