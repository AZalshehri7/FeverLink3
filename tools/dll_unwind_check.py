#!/usr/bin/env python3
"""Lists the places in feverlink3_native.dll where an unwinding foreign (C++) exception would be turned into the
abort "panic in a function that cannot unwind", and maps an RVA to the nearest PDB public symbol.

    python tools/dll_unwind_check.py target/release/feverlink3_native.dll target/release/feverlink3_native.pdb
    python tools/dll_unwind_check.py DLL PDB --rva 0xb7bf5       # which function is this address in?
    python tools/dll_unwind_check.py DLL PDB --check             # exit 1 unless the build unwinds

Why: the DLL is built with panic = "abort". In such a build a plain Rust function (or closure) that calls a game
function which may throw has a terminate pad, and the game's own exceptions (script errors in command handlers, say)
then end the process when they pass through it. The code between a hook and the game function it calls must be
`extern "system-unwind"`; this tool shows what is left. Every call to `panic_cannot_unwind` listed outside `std` and
`core` should be explained (or be a function that never sits between a hook and the engine). Needs: pip install
pefile capstone.

The listing is only as exact as the PDB's public symbols (an address is named after the nearest symbol before it).
"""
import bisect
import struct
import sys

import capstone
import pefile


def read_publics(pdb_path, pe):
    d = open(pdb_path, "rb").read()
    bs, _fbm, _nblocks, dirsize, _unk, dirptr = struct.unpack_from("<IIIIII", d, 32)

    def blocks(lst, size):
        return b"".join(d[b * bs:(b + 1) * bs] for b in lst)[:size]

    nd = (dirsize + bs - 1) // bs
    dirblocks = struct.unpack_from("<%dI" % nd, d, dirptr * bs)
    dr = blocks(dirblocks, dirsize)
    ns = struct.unpack_from("<I", dr, 0)[0]
    sizes = struct.unpack_from("<%dI" % ns, dr, 4)
    off = 4 + 4 * ns
    streams = []
    for s in sizes:
        if s == 0xFFFFFFFF:
            streams.append(b"")
            continue
        n = (s + bs - 1) // bs
        lst = struct.unpack_from("<%dI" % n, dr, off)
        off += 4 * n
        streams.append(blocks(lst, s))
    sym = streams[3]  # DBI
    rec = streams[struct.unpack_from("<H", sym, 20)[0]]
    secs = [s.VirtualAddress for s in pe.sections]
    out = []
    i = 0
    while i + 4 <= len(rec):
        ln, kind = struct.unpack_from("<HH", rec, i)
        if kind == 0x110E:  # S_PUB32
            _flags, o, sg = struct.unpack_from("<IIH", rec, i + 4)
            if 1 <= sg <= len(secs):
                out.append((secs[sg - 1] + o, rec[i + 14:i + 2 + ln].split(b"\0")[0].decode("latin1")))
        i += 2 + ln
    out.sort()
    return out


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    rva = None
    if "--rva" in sys.argv:
        rva = int(sys.argv[sys.argv.index("--rva") + 1], 0)
        args = [a for a in args if a != sys.argv[sys.argv.index("--rva") + 1]]
    dll, pdb = args[0], args[1]
    pe = pefile.PE(dll)
    pubs = read_publics(pdb, pe)
    addrs = [a for a, _ in pubs]

    def name(x):
        i = bisect.bisect_right(addrs, x) - 1
        return pubs[i][1][:110] if i >= 0 else "?"

    if "--check" in sys.argv:
        # A build with panic = "abort" has no foreign-exception runtime; an unwinding one does (and passes the game's
        # exceptions through the hooks, docs/NATIVE_LINK.md section 10).
        unwinds = any("__rust_foreign_exception" in n for _, n in pubs)
        print("unwinding build" if unwinds else "panic = abort build: the game's exceptions would end the process")
        sys.exit(0 if unwinds else 1)
    if rva is not None:
        print(hex(rva), name(rva))
        return
    img = pe.get_memory_mapped_image()
    text = pe.sections[0]
    md = capstone.Cs(capstone.CS_ARCH_X86, capstone.CS_MODE_64)
    target = None
    for a, n in pubs:
        if "panic_cannot_unwind" in n:
            target = a
    if target is None:
        print("no panic_cannot_unwind in the binary: nothing can terminate on a foreign exception")
        return
    start = text.VirtualAddress
    code = img[start:start + text.Misc_VirtualSize]
    hits = []
    for ins in md.disasm(code, start):
        if ins.mnemonic in ("call", "jmp") and ins.op_str.startswith("0x") and int(ins.op_str, 16) == target:
            hits.append(ins.address)
    ours = [h for h in hits if "feverlink3_native" in name(h)]
    print(f"{len(hits)} call(s) of panic_cannot_unwind; {len(ours)} named after this crate:")
    for h in hits:
        print(f"  {h:#x}  {name(h)}{'   <== ours' if h in ours else ''}")


if __name__ == "__main__":
    main()
