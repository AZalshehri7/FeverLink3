"""asset_sites.py: the engine functions the native module calls to build the asset groups of a shared tree, plant or rock brush tick
(native/src/assets.rs), found in an executable by anchors, never by an address.

    python -I asset_sites.py <TransportFever3.exe>      (prints what it finds; port_build.py uses `derive`)

The anchor is the assertion text the asset eraser's builder `UI::AssetGroupRecycler::MakeRemoveAssetsProposal` names itself with
(`"UI::AssetGroupRecycler::MakeRemoveAssetsProposal"`, referenced by `lea` in its cold path, as `__FUNCTION__` of its assert), and
the same for `MakeBuildAssetsProposal` (the brush's builder). Inside the remover, each function is the one call that the code around it
singles out (build 40425, `docs/research/TREES_NATIVE_RESEARCH_20261026.md` section 1, disassembly 0x7b6a39..0x7b6c9c):

- `lea rcx,[rsp+X]; call A; ...; mov byte ptr [rsp+X+0x4b1], 1`: A = `ConstructionEntity::ConstructionEntity()` on a local at X, then
  `desc.autoRemovable` (+0x4b1) = true;
- `mov edx,1; lea rcx,[rsp+X+0xa30]; call B`: B = the resize of `construction.subconstructions` (+0xa30) to one;
- `add rcx,0x210` (on the subconstruction), then the next call C: C = `emplace_back(T&&)` of the subconstruction's 0x48-byte element;
- `add rcx,0x140`, `movabs rax,0xc30c30c30c30c30d` (a division by 0xa8: the TransformedModel stride), the next call D: D = the models'
  `assign(first, count)`;
- `add rbx,0x258` (the Proposal's `toAdd`), then `lea rdx,[rsp+X]; call E` followed by `add qword ptr [rbx+8], 0xe48` (E = the copy
  constructor into the vector's end, stride 0xe48) and `lea r8,[rsp+X]; call F` (F = the vector's reallocating insert);
- the last `lea rcx,[rsp+X]; call G`: G = `~ConstructionEntity()` of the local.

Every rule must hold exactly once, or nothing is returned (`derive` raises). The whole remover and the whole builder are recorded as
sites too: the module's search then proves at game start that the code with these offsets and this stride is unchanged.
"""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from find_sites import Image, take_prologue  # noqa: E402

REMOVER = b"UI::AssetGroupRecycler::MakeRemoveAssetsProposal\0"
BUILDER = b"UI::AssetGroupRecycler::MakeBuildAssetsProposal\0"
# the bytes recorded at each function (whole instructions, at least this many; the module compares them before a call)
RECORD = 16

# field name -> what it is (the order of the row's EngineFns)
FIELDS = {
    "asset_con_ctor": "ConstructionEntity::ConstructionEntity()",
    "asset_con_copy": "ConstructionEntity::ConstructionEntity(const ConstructionEntity&)",
    "asset_con_dtor": "ConstructionEntity::~ConstructionEntity()",
    "asset_con_insert": "std::vector<ConstructionEntity>::_Emplace_reallocate(where, const ConstructionEntity&)",
    "asset_subcon_resize": "std::vector<Subconstruction>::resize(n)",
    "asset_elem_push": "std::vector<0x48-byte element>::emplace_back(T&&)",
    "asset_models_assign": "std::vector<TransformedModel>::assign(first, count)",
    "asset_remover": "UI::AssetGroupRecycler::MakeRemoveAssetsProposal (the offsets the module writes)",
    "asset_builder": "UI::AssetGroupRecycler::MakeBuildAssetsProposal (the cost offset)",
}


class NotFound(Exception):
    pass


def _function_naming(img: Image, text: bytes) -> int:
    at = img.find_bytes(text)
    if len(at) != 1:
        raise NotFound(f"{text[:-1].decode()}: the text is in the data {len(at)} times (expected once)")
    funcs = {f.begin for r in img.rip_refs_to(at[0]) if (f := img.func_containing(r))}
    if len(funcs) != 1:
        raise NotFound(f"{text[:-1].decode()}: {len(funcs)} functions reference the text (expected one)")
    return funcs.pop()


def _insns(img: Image, begin: int) -> list:
    f = img.func_containing(begin)
    out, cur = [], f.begin
    while cur < f.end:
        ins = img.disasm(cur, 16)
        if not ins:
            raise NotFound(f"the function at {begin:#x} does not decode at {cur:#x}")
        out.append(ins[0])
        cur += ins[0].size
    return out


def _target(img: Image, ins) -> int | None:
    if ins.mnemonic == "call" and ins.bytes[0] == 0xE8:
        return ins.address - img.image_base + ins.size + int.from_bytes(bytes(ins.bytes[1:5]), "little", signed=True)
    return None


def _rsp(op: str) -> int | None:
    """X of `[rsp + X]` in an operand text, or None."""
    if not op.startswith("[rsp + 0x") or not op.endswith("]"):
        return None
    try:
        return int(op[len("[rsp + "):-1], 16)
    except ValueError:
        return None


def _one(found: list, what: str):
    if len(set(found)) != 1:
        raise NotFound(f"{what}: {len(set(found))} places (expected one)")
    return found[0]


def derive(img: Image) -> dict[str, tuple[int, int]]:
    """field -> (rva, bytes recorded). Raises NotFound with the rule that failed."""
    rm = _function_naming(img, REMOVER)
    bd = _function_naming(img, BUILDER)
    ins = _insns(img, rm)
    calls = [(i, _target(img, x)) for i, x in enumerate(ins) if _target(img, x) is not None]

    def lea_before(i: int, reg: str, back: int = 3) -> int | None:
        for x in ins[max(0, i - back):i]:
            if x.mnemonic == "lea" and x.op_str.startswith(reg + ", "):
                v = _rsp(x.op_str[len(reg) + 2:])
                if v is not None:
                    return v
        return None

    # the constructor: `lea rcx,[rsp+X]; call A`, and within the next four instructions `mov byte ptr [rsp+X+0x4b1], 1`
    ctor = []
    for i, t in calls:
        x = lea_before(i, "rcx", 1)
        if x is not None and any(y.mnemonic == "mov" and y.op_str == f"byte ptr [rsp + {x + 0x4b1:#x}], 1" for y in ins[i + 1:i + 5]):
            ctor.append((t, x))
    ctor_rva, local = _one(ctor, "the constructor followed by autoRemovable (+0x4b1) = 1")

    resize = [t for i, t in calls if lea_before(i, "rcx", 1) == local + 0xa30
              and any(y.mnemonic == "mov" and y.op_str == "edx, 1" for y in ins[max(0, i - 3):i])]
    resize_rva = _one(resize, "the resize of the subconstructions (+0xa30) to one")

    def call_after(pred, what: str) -> tuple[int, int]:
        hits = []
        for i, x in enumerate(ins):
            if pred(x):
                nxt = next(((j, t) for j, t in calls if j > i), None)
                if nxt:
                    hits.append(nxt)
        return _one(hits, what)

    push_i, push_rva = call_after(lambda x: x.mnemonic == "add" and x.op_str == "rcx, 0x210", "the call after add rcx,0x210")
    assign_i, assign_rva = call_after(lambda x: x.mnemonic == "add" and x.op_str == "rcx, 0x140", "the call after add rcx,0x140")
    if not any(x.mnemonic == "movabs" and x.op_str == "rax, 0xc30c30c30c30c30d" for x in ins[assign_i - 12:assign_i]):
        raise NotFound("the models' count is not divided by 0xa8 before their assign")
    to_add = [i for i, x in enumerate(ins) if x.mnemonic == "add" and x.op_str.endswith(", 0x258")]
    k = _one(to_add, "add <reg>,0x258 (the Proposal's toAdd)")
    after = [(i, t) for i, t in calls if i > k]
    copy = [t for i, t in after[:2] if lea_before(i, "rdx") == local
            and any(y.mnemonic == "add" and y.op_str.endswith("+ 8], 0xe48") for y in ins[i + 1:i + 3])]
    copy_rva = _one(copy, "the copy into toAdd's end followed by end += 0xe48")
    insert = [t for i, t in after[:2] if lea_before(i, "r8") == local]
    insert_rva = _one(insert, "the reallocating insert into toAdd")
    dtors = [t for i, t in calls if lea_before(i, "rcx", 1) == local and t != ctor_rva]
    if not dtors:
        raise NotFound("no destructor call on the local entity")
    dtor_rva = dtors[-1]
    if len(set(dtors)) != 1:
        raise NotFound(f"the local entity is passed to {len(set(dtors))} functions besides its constructor (expected one)")
    # the builder: the cost of each tree is added to the qword at +0xb40 (`construction.cost`) of the entity whose autoRemovable
    # (+0x4b1) it sets
    bins = _insns(img, bd)
    flags = {_rsp(x.op_str[len("byte ptr "):-3]) for x in bins if x.mnemonic == "mov" and x.op_str.startswith("byte ptr [rsp + ")
             and x.op_str.endswith("], 1")}
    cost = [y for x in bins if x.mnemonic == "add" and x.op_str.startswith("qword ptr [rsp + ") and x.op_str.endswith("], rax")
            and (y := _rsp(x.op_str[len("qword ptr "):-5])) is not None and y - 0xb40 + 0x4b1 in flags]
    _one(cost, "the builder's cost added at +0xb40 of an entity whose +0x4b1 it sets")
    rvas = {
        "asset_con_ctor": ctor_rva,
        "asset_con_copy": copy_rva,
        "asset_con_dtor": dtor_rva,
        "asset_con_insert": insert_rva,
        "asset_subcon_resize": resize_rva,
        "asset_elem_push": push_rva,
        "asset_models_assign": assign_rva,
        "asset_remover": rm,
        "asset_builder": bd,
    }
    out = {}
    for name, rva in rvas.items():
        _, n, err = take_prologue(img, rva, RECORD, exact=False)
        if err:
            raise NotFound(f"{name} at {rva:#x}: {err}")
        out[name] = (rva, n)
    return out


if __name__ == "__main__":
    img = Image(sys.argv[1])
    try:
        for k, (rva, n) in derive(img).items():
            print(f"{k:22} {rva:#x}  {n} bytes  {FIELDS[k]}")
    except NotFound as e:
        sys.exit(f"not found: {e}")
