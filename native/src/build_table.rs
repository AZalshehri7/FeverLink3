//! Everything that depends on a particular game build lives here and nowhere else.
//!
//! A build is identified by the link timestamp in its PE header. A row carries the relative virtual addresses the hooks need
//! and the exact prologue bytes [`crate::detour::install`] must find there. No row is written in this code: every row is
//! found at game start by `search.rs` from the generated proof file, and the generated list [`VERIFIED`] says which rows
//! were played and checked. A build the search cannot prove means no hooks: the mod runs degraded and says so.

/// A relative virtual address inside the game executable.
pub type Rva = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSite {
    pub rva: Rva,
    /// The first bytes of the function, whole instructions, position-independent, at least 14.
    pub prologue: &'static [u8],
}

/// A place inside a function whose bytes are rewritten in place (no detour, no trampoline). `expected` is what
/// must be there before patching; `replacement` has the same length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchSite {
    pub rva: Rva,
    pub expected: &'static [u8],
    pub replacement: &'static [u8],
}

/// An engine function the module calls but never hooks or patches. `prologue` (whole instructions, at least
/// [`CALL_PROLOGUE_MIN`] bytes, relative operands allowed) is compared with the bytes in memory before the first
/// call, so a different build never gets a call into the middle of something else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub rva: Rva,
    pub prologue: &'static [u8],
}

/// The seed the pinned `makeseed` returns: the ASCII of "FL3S" as a big-endian number, `0x464C3353`. Arbitrary but
/// fixed, so it is the same on every machine.
pub const LUA_SEED: u32 = u32::from_be_bytes(*b"FL3S");

/// `mov eax, seed ; ret`, then `int3` up to `N` bytes: the body that replaces `makeseed`.
pub const fn lua_seed_patch<const N: usize>(seed: u32) -> [u8; N] {
    let mut out = [0xCC; N];
    let s = seed.to_le_bytes();
    out[0] = 0xB8;
    out[1] = s[0];
    out[2] = s[1];
    out[3] = s[2];
    out[4] = s[3];
    out[5] = 0xC3;
    out
}

/// The pinned `makeseed` of every build so far (the function's first 26 bytes are replaced).
static LUA_SEED_PATCH: [u8; 26] = lua_seed_patch(LUA_SEED);

/// The script-pool region after the patch, the same in every build so far (the `jge` and the `call` it removes are the only
/// build-dependent bytes of the region): `cmp eax,8 ; nop6 ; mov dword [rsp+20h],1 ; xor eax,eax ; nop3`.
static SCRIPT_POOL_PATCH: [u8; 22] = [
    0x83, 0xF8, 0x08, 0x66, 0x0F, 0x1F, 0x44, 0x00, 0x00, 0xC7, 0x44, 0x24, 0x20, 0x01, 0x00, 0x00, 0x00, 0x31, 0xC0,
    0x0F, 0x1F, 0x00,
];

/// Displacement of `GameTime.tickCount` in the component (`mov eax,[rax+3Ch]` in its getter).
pub const TICK_COUNT_OFFSET: u8 = 0x3c;
/// Displacement of `GameTime.updateCount`, the simulation step count (`mov eax,[rax+40h]` in its getter).
pub const UPDATE_COUNT_OFFSET: u8 = 0x40;
/// Index of the load's displacement byte inside the getter.
pub const TICK_GETTER_DISP_AT: usize = 21;

/// A tickCount getter turned into an updateCount getter: the same bytes, the load's displacement replaced.
pub const fn step_getter_patch<const N: usize>(tick_getter: [u8; N]) -> [u8; N] {
    let mut out = tick_getter;
    out[TICK_GETTER_DISP_AT] = UPDATE_COUNT_OFFSET;
    out
}

/// Shortest prologue a [`CallSite`] may record.
pub const CALL_PROLOGUE_MIN: usize = 8;

/// One `call rel32` inside a function whose target is redirected (`FEVERLINK3_LOG_GROWTH_DEEP`, read-only): the 4-byte
/// displacement is rewritten to lead to a thunk that reaches a hook, which calls `callee` itself. Used where the function
/// around the call must not be detoured (it recurses) or the callee cannot be (a relative jump in its first bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallPatchSite {
    /// The RVA of the `call` instruction (its `E8` byte).
    pub rva: Rva,
    /// The five bytes there: `E8` and the displacement, which fixes the callee.
    pub expected: &'static [u8],
    /// The callee the displacement leads to; its prologue is compared too, because the hook calls it directly.
    pub callee: CallSite,
}

/// The game interface's builder-preview listener, read where it is made (`BuildAddrs::preview_owner`).
///
/// Static analysis of build 40420: each of the six listener lambdas that call the `builder.proposalCreate` emitter (`0xf44c00`)
/// captures three pointers `[ui, y, z]` (closure `+8`, `+0x10`, `+0x18`) and computes the emitter's first five arguments from them
/// when it runs: `a = [y+0x18]`, `b = [y+0xc8]`, `r = z->vtable[1](z)`, `c = [r+0x18]`, `dispatcher = [r+0x1d8]`, `e = [ui+0xb98]`
/// (the same instructions before each of the six calls, `0x665ba5`..`0x667891`). The lambdas are made in `UI::CGameUI::CGameUI` (`0x647cb0`, its
/// assertion string; `ui` is its `this`), a new game interface for every loaded world. `call` is the one call of the
/// `std::function` constructor of the lambda whose call operator is `0x666420` (`0x642dc0`: its first bytes store that lambda's
/// vtable) inside that constructor; `rdx` there points at the three captures.
///
/// Why (friend test 2026-10-21, 22:44-22:45): after a resync load the module had no emitter arguments (they come from a tool preview of
/// the world being played, and are dropped when holding switches), the friend's first builds were seven station configures
/// (`UI::ModuleBuilder`, which sends no preview): no record, all seven refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewOwnerSite {
    pub call: CallPatchSite,
    /// `a` and `b`: offsets into the second capture.
    pub y_a: usize,
    pub y_b: usize,
    /// The vtable slot of the third capture's getter (called with the capture as `this`).
    pub z_getter_slot: usize,
    /// `c` and the dispatcher: offsets into what the getter returns.
    pub r_c: usize,
    pub r_dispatcher: usize,
    /// `e`: offset into the game interface (the first capture).
    pub ui_e: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildAddrs {
    /// PE link timestamp.
    pub timestamp: u32,
    /// Human name: the game's own build number.
    pub name: &'static str,
    /// `CommandList::Add`: where every engine command enters the queue; construction tools are held here.
    pub command_list_add: Option<HookSite>,
    /// `scripting::Convert(out optional<Proposal>*, state*, SimpleProposal*)` (proposal_util.cpp): turns a script's `SimpleProposal`
    /// into the engine's `Proposal` (constructions and all). Hooked only with `FEVERLINK3_NATIVE_PATCH=1`, to write `frozenNodes`
    /// into the result before anything checks it (`docs/research/SPIKE_NATIVE_COMMAND.md`, "Input-level conversion").
    pub script_convert: Option<HookSite>,
    /// `ConstructionUpdate(bool, state*, construction*, ctx*, a5, Proposal* being built)` (0x10c6020, called only from `Convert`, once per
    /// construction): runs the construction's own update, which adds its entrance street to the proposal. Hooked only with
    /// `FEVERLINK3_NATIVE_PATCH=1`, to put `frozenNodes` into the proposal before that update runs.
    pub convert_update: Option<HookSite>,
    /// The interface's emitter of the tool event `builder.proposalCreate` (the preview every builder tool gives the game scripts'
    /// `guiHandleEvent`): `(a, b, c, dispatcher, e, const std::string* tool, const Proposal*, const ProposalData*, bool* out)`, the
    /// event name built inside, the call into the scripts synchronous. Hooked with holding: its first five arguments are kept from each
    /// tool preview, and a held build command is then given to the scripts the same way, as the preview of the tool `held:<id>` (its
    /// own Proposal and the ProposalData slot of its payload), so the mod has the record of every held command BEFORE the build, also
    /// for a tool that sends no preview (UI::ModuleBuilder, the station configure: run s4 2026-10-14). Static analysis of build 40420:
    /// the only function that references the string, called by one listener lambda per builder tool.
    pub preview_event: Option<HookSite>,
    /// Where the game interface (`UI::CGameUI::CGameUI`, once per loaded world) makes one builder tool's preview listener: the emitter's
    /// first five arguments are computed from that lambda's three captures each time it runs, so a held command can be given to the
    /// scripts before any tool preview of this world (`PreviewOwnerSite`).
    pub preview_owner: Option<PreviewOwnerSite>,
    /// The simulation's command application: where held commands are counted and released at their stamp.
    pub apply_command: Option<HookSite>,
    /// Inside the game-script system's pool construction: `cmp eax,8 / jge / mov [rsp+20h],1 / call
    /// hardware_concurrency`. The replacement neutralises the branch and makes the thread count 0 (the engine
    /// then falls back to one worker, as it does itself on small CPUs), which removes the timing-dependent
    /// script update order. Finding from MPFever (MIT): see docs/ARCHITECTURE.md section 7.
    pub script_pool: Option<PatchSite>,
    /// The seeding function of the town developer's task (`FEVERLINK3_LOG_TOWN_SEED`, read-only logging): it builds
    /// a 0x1408-byte task object and seeds its `std::mt19937` from its ninth argument. Found by hand from a static
    /// analysis of build 40420, not by the address tool (docs/NATIVE_LINK.md section 2).
    pub town_seed: Option<HookSite>,
    /// `TownDeveloper::Develop(this, world, town entity r8d, flag r9b, minstd state pointer [stack arg 5], arg 6,
    /// arg 7)`: draws a town's growth from a `std::minstd_rand` whose single 32-bit state the caller owns
    /// (`FEVERLINK3_LOG_GROWTH`, `FEVERLINK3_GROWTH_SEED_MODE`). Static analysis of build 40420, verified again in
    /// `docs/NATIVE_LINK.md` section 11.
    pub town_develop: Option<HookSite>,
    /// The `TownUpdateSize` command handler (kind 40): the only caller of `Develop` that seeds from the game time.
    /// Hooked read-only to read the game time the seed is made from.
    pub town_update_size: Option<HookSite>,
    /// Candidate gather of the octree walk (`FEVERLINK3_LOG_GROWTH_DEEP`, read-only): `(out vector*, float2* centre,
    /// float radius [xmm2], ctx* [r9], 5 stack args) -> out`; the vector `[rcx]..[rcx+8]` holds 0x40-byte records,
    /// sorted by `std::sort` (`0x966150`) before the function returns.
    pub town_candidates: Option<HookSite>,
    /// Country-edge selection (`FEVERLINK3_LOG_GROWTH_DEEP`, read-only): `(float2* centre, float radius [xmm1], world
    /// [r8], ctx* [r9], arg5, u32* rng [stack arg 6])`, returns nothing; draws `vec[RandomInt(0,n)]`.
    pub town_edges: Option<HookSite>,
    /// The call `std::sort(begin, end, count, ctx)` at the end of the candidate gather (`0x967c16` in `0x9677c0`):
    /// logged BEFORE the sort (the input order) and after it (distance ties).
    pub town_sort_call: Option<CallPatchSite>,
    /// The call `RandomInt(rng, 0, n)` that picks the country edge in `0x95d100` (`0x95d627`): the id vector at
    /// `[rbp-0x40, rbp-0x38)` is read just before it.
    pub town_pick_call: Option<CallPatchSite>,
    /// The call `RiverField::At(field, out, point, flag)` in the street developer's open pass (`0x96962e` in `0x969140`;
    /// `FEVERLINK3_LOG_GROWTH_DEEP`, read-only): the river direction field a new town street is bent towards. `At` answers
    /// from a per-50-m-cell `std::map` cache at `field+0x18` (size at `+0x20`) that is not saved and exists once per
    /// engine; the trace logs whether the answer came from that cache (`docs/research/TOWN_CACHE_20261019.md`, local).
    pub town_field_call: Option<CallPatchSite>,
    /// The call of `CGame::Sync` in `CGame::Step` (main thread, every frame): smooth pacing redirects it and, after a `Sync` that handed
    /// the simulation a batch, stretches the batch interval by the mod's factor (`pace.rs`). Static analysis of build 40420 (`Step` at
    /// `0x11f3f0`); the place is TPF3-MP's (MIT) "batch interval" by way of MPFever 0.3.15 (MIT), found again here in the executable.
    pub batch_pace: Option<CallPatchSite>,
    /// The call `deque<int>::_Insert_range(free queue, &ret, where = size, first, last)` in `ecs::Engine::EndModification`, where the
    /// ids a modification removed join the back of the FIFO free-id queue (`AddEntity` takes its front). `FEVERLINK3_FREE_ORDER`
    /// redirects it and sorts `[first, last)` by entity id first, so the order the ids are reused in no longer depends on the order
    /// the engine visited the removed entities (`freeorder.rs`). Static analysis of build 40420; the place is MPFever 0.3.14's (MIT,
    /// from TPF3-MP, MIT) "freed ids", found again here in the executable and patched at the call instead of the function.
    pub free_order: Option<CallPatchSite>,
    /// `GameTime` getter: `fn(system*) -> u32` (the dword at `+0x40` of the `GameTime` component). Called, never
    /// hooked; the prologue is compared first.
    pub game_time_getter: Option<CallSite>,
    /// The engine's `GameTime.tickCount` getter, `fn(system*) -> u32` (the dword at `+0x3c`). The official API reference
    /// (`api/tealdef/api/engine.d.tl`, record `GameTime`) says tickCount "ticks also when the game is paused (once per frame)",
    /// updateCount (`+0x40`) counts simulation updates only; the engine increments both in one function (`0xbad6b0`:
    /// `inc [x+0x3c]`, then `inc [x+0x40]` only when the frame ran an update). The simulation seeds random draws from this
    /// getter: the land vehicles' reservation shuffle (`LandVehicleMoveSystem::Update2`, minstd seed), `AccountSystem`,
    /// `TownDeveloper::Develop` (directly and through the street developer) and `MakeStreetProposal`, so two games that
    /// stood still for a different number of frames (the input barrier sets speed 0) draw differently. The patch turns the
    /// getter's one load `mov eax,[rax+0x3c]` into `mov eax,[rax+0x40]`: every caller of the getter reads the step count,
    /// the same on every game; the field itself (what Lua reads) stays as it is. Static analysis of build 40420 (getter
    /// callers by a rel32 scan of `.text`, their functions by unwind data, vtables and assert strings); the finding comes
    /// from TPF3-MP (Juliansgith, MIT) as ported by MPFever, the getter-level patch is ours (THIRD_PARTY_NOTICES.md).
    pub tick_count_getter: Option<PatchSite>,
    /// Lua 5.2's `makeseed`, the seed of the string hash, called once by `lua_newstate`. The replacement is
    /// `mov eax, LUA_SEED ; ret` padded with `int3` (see [`lua_seed_patch`]), so every process hashes strings the
    /// same way and `pairs` order over string keys no longer differs between processes. `expected` is the
    /// function's 26-byte prologue. Site located with findings from MPFever (MIT, dkdknight).
    pub lua_makeseed: Option<PatchSite>,
    /// `Command::Command(Command&&)`: the hold path moves a command out of the caller's hands with it.
    pub command_move: Option<CallSite>,
    /// `Command::~Command()`: runs on a released command (moved-from) and on a cancelled one.
    pub command_destroy: Option<CallSite>,
    /// Destructor of the 16-byte handle `CommandList::Add` writes through its `out` argument.
    pub handle_destroy: Option<CallSite>,
    /// Return addresses (RVA of the byte after the `call`) of the calls to `command_list_add` made by the
    /// interface's construction tools. Only a command added from one of them may be held: the tool merely destroys
    /// the returned handle, where other callers (the mod's own `sendCommand` among them) wait for the result. The
    /// terrain tool is left out on purpose. Finding from MPFever (MIT).
    pub ui_add_sites: &'static [Rva],
    /// `_Do_call` thunks of the completion callbacks (`std::function` of the tools' `done` argument of `CommandList::Add`) that were read
    /// out of the executable and are safe to call with a command that was never applied (result byte 0): the street tool's, the
    /// bulldozer's and the stop tool's (`0x596a70`, no thunk: the lambda itself; it clears its flag at `+0x2c8` and calls the same hook;
    /// like the street tool's it first runs a helper pair with the command's result vector, which is empty for a command that never ran:
    /// read from the disassembly only, not yet seen in a real game). Any other callback (the construction tool's does real work with the result) is never fired.
    pub fire_done_calls: &'static [Rva],
    /// Offset of the command-kind byte (the dispatcher index) inside a command's payload.
    pub kind_offset: Option<u32>,
    /// Command kinds the control link may name: (name, kind byte).
    pub kinds: &'static [(&'static str, u8)],
    /// Engine functions the id trace, the native transport and the refund list call or hook (their RVAs; each module compares
    /// the first bytes it keeps itself before the first call).
    pub engine: EngineFns,
    /// The layout table of `portable.rs` that describes this build's engine objects (`portable::LAYOUT_BUILD`), when every
    /// function it was read from is unchanged in this build (`tools/addr/port_build.py`, its layout witnesses); `None`: the
    /// native transport and the other pieces that read engine objects through it stay off.
    pub layout: Option<&'static str>,
}

/// RVAs of the engine functions called or hooked outside the build table's own sites; `None`: the piece that needs it stays off.
/// The bytes each one must start with are kept by its module (`native_copy.rs`, `rebuild.rs`, `refund.rs`, `ids.rs`): they are the
/// same in every build so far, only the place moves, and `port_build.py` checks both for a new build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFns {
    /// `Proposal::Proposal()`, `~Proposal()`, `Proposal(const Proposal&)`, `operator=(const Proposal&)`.
    pub proposal_ctor: Option<Rva>,
    pub proposal_dtor: Option<Rva>,
    pub proposal_copy: Option<Rva>,
    pub proposal_assign: Option<Rva>,
    /// The game's `operator new(size_t)` and `operator delete(void*)`.
    pub engine_new: Option<Rva>,
    pub engine_delete: Option<Rva>,
    /// `_Hash::_Insert_range_unchecked` of the Context's `refundableEntities`.
    pub refund_insert: Option<Rva>,
    /// `ecs::Engine::AddEntity`, `ecs::Engine::RemoveEntity`, the game-time advance and the component lookup (id trace).
    pub add_entity: Option<Rva>,
    pub remove_entity: Option<Rva>,
    pub game_time_advance: Option<Rva>,
    pub component_lookup: Option<Rva>,
    /// The shared asset brush (`assets.rs`): `ConstructionEntity::ConstructionEntity()`, its copy constructor, its destructor,
    /// `std::vector<ConstructionEntity>::_Emplace_reallocate`, the resize of its subconstructions, `emplace_back` of a subconstruction's
    /// 0x48-byte element, the models' `assign(first, count)`; and the two builders whose code proves the offsets the module writes
    /// (`UI::AssetGroupRecycler::MakeRemoveAssetsProposal`, `MakeBuildAssetsProposal`; found by `tools/addr/asset_sites.py`).
    pub asset_con_ctor: Option<Rva>,
    pub asset_con_copy: Option<Rva>,
    pub asset_con_dtor: Option<Rva>,
    pub asset_con_insert: Option<Rva>,
    pub asset_subcon_resize: Option<Rva>,
    pub asset_elem_push: Option<Rva>,
    pub asset_models_assign: Option<Rva>,
    pub asset_remover: Option<Rva>,
    pub asset_builder: Option<Rva>,
    /// The first bytes of each of them as the search recorded them (`port_build.py`'s byte counts), by field name: what a module
    /// compares with memory before it calls or hooks one.
    pub prologues: &'static [(&'static str, &'static [u8])],
}

/// The engine functions of [`EngineFns`], by field name.
pub const ENGINE_NAMES: [&str; 20] = [
    "proposal_ctor",
    "proposal_dtor",
    "proposal_copy",
    "proposal_assign",
    "engine_new",
    "engine_delete",
    "refund_insert",
    "add_entity",
    "remove_entity",
    "game_time_advance",
    "component_lookup",
    "asset_con_ctor",
    "asset_con_copy",
    "asset_con_dtor",
    "asset_con_insert",
    "asset_subcon_resize",
    "asset_elem_push",
    "asset_models_assign",
    "asset_remover",
    "asset_builder",
];

impl EngineFns {
    /// The recorded first bytes of one engine function, or why the piece that needs it stays off.
    pub fn prologue(&self, name: &str) -> Result<&'static [u8], String> {
        self.prologues
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, b)| *b)
            .ok_or_else(|| format!("{name} has no recorded bytes for this game build"))
    }

    pub const NONE: EngineFns = EngineFns {
        proposal_ctor: None,
        proposal_dtor: None,
        proposal_copy: None,
        proposal_assign: None,
        engine_new: None,
        engine_delete: None,
        refund_insert: None,
        add_entity: None,
        remove_entity: None,
        game_time_advance: None,
        component_lookup: None,
        asset_con_ctor: None,
        asset_con_copy: None,
        asset_con_dtor: None,
        asset_con_insert: None,
        asset_subcon_resize: None,
        asset_elem_push: None,
        asset_models_assign: None,
        asset_remover: None,
        asset_builder: None,
        prologues: &[],
    };
}

/// The RVA of an engine function, or why the piece that needs it stays off.
pub fn engine_rva(rva: Option<Rva>, what: &str) -> Result<Rva, String> {
    rva.ok_or_else(|| format!("{what} is not in the build table for this game build"))
}

impl BuildAddrs {
    /// The kind byte a control-link name stands for.
    pub fn kind_by_name(&self, name: &str) -> Option<u8> {
        self.kinds.iter().find(|(n, _)| *n == name).map(|(_, k)| *k)
    }

    /// The name of a kind byte, when the table has one.
    pub fn kind_name(&self, kind: u8) -> Option<&'static str> {
        self.kinds.iter().find(|(_, k)| *k == kind).map(|(n, _)| *n)
    }

    /// The name the portable form's layout check compares with `portable::LAYOUT_BUILD`: the layout table this build uses, or
    /// its own name when none was verified (a real row's own name is never a layout table's name unless the row says so).
    pub fn layout_name(&self) -> &'static str {
        self.layout.unwrap_or(self.name)
    }

    /// Index in [`ui_add_sites`](Self::ui_add_sites) of a return address, when it is one of the tool sites.
    pub fn ui_site_index(&self, return_rva: Rva) -> Option<usize> {
        self.ui_add_sites.iter().position(|&r| r == return_rva)
    }
}

/// Facts of the portable layout table (`portable::LAYOUT_BUILD`): they hold for a build whose layout witnesses and hooked
/// functions are the reference build's code exactly, which is what the search proves before it names the layout. The kind
/// byte sits at payload offset `kind_offset` (read by `command_list_add` and the dispatcher), the build proposal is kind 52,
/// and the preview listener's third capture is asked through its vtable slot 1. Read from build 40420 by
/// `tools/addr/find_sites.py` (`out/6ac50427.txt`) and the listener's code (`PreviewOwnerSite`).
pub struct LayoutFacts {
    pub name: &'static str,
    pub kind_offset: u32,
    pub kinds: &'static [(&'static str, u8)],
    pub z_getter_slot: usize,
}

pub const LAYOUT_FACTS: LayoutFacts =
    LayoutFacts { name: "40420", kind_offset: 0x9b8, kinds: &[("build_proposal", 52)], z_getter_slot: 1 };

/// What a search (or a port report) found, by field name: `rva` (`<field>`, `<field>.callee`, `ui_add_sites[i]`, the engine
/// functions), the bytes recorded there, the preview listener's offsets.
#[derive(Default, Debug)]
pub struct Parts {
    pub rva: std::collections::HashMap<String, u32>,
    pub bytes: std::collections::HashMap<String, Vec<u8>>,
    pub offsets: std::collections::HashMap<String, usize>,
}

fn leak(b: &[u8]) -> &'static [u8] {
    Box::leak(b.to_vec().into_boxed_slice())
}

/// The row of one build from its parts; `None` when any field is missing or a patch region has another length than the
/// patch. The patches' replacements are the same in every build: the pinned seed body, the pool region, and the tick getter
/// with its one displacement byte changed.
pub fn assemble(p: &Parts, timestamp: u32, name: &str, layout: &str) -> Option<BuildAddrs> {
    let facts = (layout == LAYOUT_FACTS.name).then_some(&LAYOUT_FACTS)?;
    let rva = |k: &str| p.rva.get(k).copied();
    let bytes = |k: &str| p.bytes.get(k).map(|b| leak(b));
    let hook = |k: &str| Some(HookSite { rva: rva(k)?, prologue: bytes(k)? });
    let call = |k: &str| Some(CallSite { rva: rva(k)?, prologue: bytes(k)? });
    let call_patch = |k: &str| {
        let c = format!("{k}.callee");
        Some(CallPatchSite {
            rva: rva(k)?,
            expected: bytes(k)?,
            callee: CallSite { rva: rva(&c)?, prologue: bytes(&c)? },
        })
    };
    let patch = |k: &str, replacement: &'static [u8]| {
        let expected = bytes(k)?;
        (expected.len() == replacement.len()).then_some(PatchSite { rva: rva(k)?, expected, replacement })
    };
    let list = |group: &str| -> Option<&'static [Rva]> {
        let v: Vec<Rva> = (0..).map_while(|i| rva(&format!("{group}[{i}]"))).collect();
        (!v.is_empty()).then(|| &*Box::leak(v.into_boxed_slice()))
    };
    let tick = {
        let expected = bytes("tick_count_getter")?;
        if expected.len() != 27 || expected[TICK_GETTER_DISP_AT] != TICK_COUNT_OFFSET {
            return None;
        }
        let mut replacement = expected.to_vec();
        replacement[TICK_GETTER_DISP_AT] = UPDATE_COUNT_OFFSET;
        PatchSite { rva: rva("tick_count_getter")?, expected, replacement: leak(&replacement) }
    };
    let off = |k: &str| p.offsets.get(k).copied();
    // an engine function that is not found fails the row too (`Some(None)` is never produced)
    let e = |k: &str| rva(k).map(Some);
    Some(BuildAddrs {
        timestamp,
        name: Box::leak(name.to_owned().into_boxed_str()),
        command_list_add: Some(hook("command_list_add")?),
        script_convert: Some(hook("script_convert")?),
        convert_update: Some(hook("convert_update")?),
        preview_event: Some(hook("preview_event")?),
        preview_owner: Some(PreviewOwnerSite {
            call: call_patch("preview_owner")?,
            y_a: off("y_a")?,
            y_b: off("y_b")?,
            z_getter_slot: facts.z_getter_slot,
            r_c: off("r_c")?,
            r_dispatcher: off("r_dispatcher")?,
            ui_e: off("ui_e")?,
        }),
        apply_command: Some(hook("apply_command")?),
        script_pool: Some(patch("script_pool", &SCRIPT_POOL_PATCH)?),
        town_seed: Some(hook("town_seed")?),
        town_develop: Some(hook("town_develop")?),
        town_update_size: Some(hook("town_update_size")?),
        town_candidates: Some(hook("town_candidates")?),
        town_edges: Some(hook("town_edges")?),
        town_sort_call: Some(call_patch("town_sort_call")?),
        town_pick_call: Some(call_patch("town_pick_call")?),
        town_field_call: Some(call_patch("town_field_call")?),
        batch_pace: Some(call_patch("batch_pace")?),
        free_order: Some(call_patch("free_order")?),
        game_time_getter: Some(call("game_time_getter")?),
        tick_count_getter: Some(tick),
        lua_makeseed: Some(patch("lua_makeseed", &LUA_SEED_PATCH)?),
        command_move: Some(call("command_move")?),
        command_destroy: Some(call("command_destroy")?),
        handle_destroy: Some(call("handle_destroy")?),
        ui_add_sites: list("ui_add_sites")?,
        fire_done_calls: list("fire_done_calls")?,
        kind_offset: Some(facts.kind_offset),
        kinds: facts.kinds,
        engine: EngineFns {
            proposal_ctor: e("proposal_ctor")?,
            proposal_dtor: e("proposal_dtor")?,
            proposal_copy: e("proposal_copy")?,
            proposal_assign: e("proposal_assign")?,
            engine_new: e("engine_new")?,
            engine_delete: e("engine_delete")?,
            refund_insert: e("refund_insert")?,
            add_entity: e("add_entity")?,
            remove_entity: e("remove_entity")?,
            game_time_advance: e("game_time_advance")?,
            component_lookup: e("component_lookup")?,
            asset_con_ctor: e("asset_con_ctor")?,
            asset_con_copy: e("asset_con_copy")?,
            asset_con_dtor: e("asset_con_dtor")?,
            asset_con_insert: e("asset_con_insert")?,
            asset_subcon_resize: e("asset_subcon_resize")?,
            asset_elem_push: e("asset_elem_push")?,
            asset_models_assign: e("asset_models_assign")?,
            asset_remover: e("asset_remover")?,
            asset_builder: e("asset_builder")?,
            prologues: Box::leak(
                ENGINE_NAMES.iter().map(|&n| Some((n, bytes(n)?))).collect::<Option<Vec<_>>>()?.into_boxed_slice(),
            ),
        },
        layout: Some(facts.name),
    })
}

/// The new build's row from a port report of `tools/addr/port_build.py` (its "Rust for native/src/build_table.rs" lines),
/// assembled exactly as a search would: the reports are the data the tests and the verified list are checked against.
pub fn parse_report(report: &str) -> Option<BuildAddrs> {
    fn hexes(s: &str) -> Vec<u64> {
        s.split("0x")
            .skip(1)
            .filter_map(|t| {
                u64::from_str_radix(&t[..t.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(t.len())], 16).ok()
            })
            .collect()
    }
    fn list(s: &str) -> Option<Vec<u8>> {
        let (_, rest) = s.split_once("&[")?;
        let body = &rest[..rest.find(']')?];
        hexes(body).into_iter().map(|v| u8::try_from(v).ok()).collect()
    }
    let head = report.lines().find(|l| l.starts_with("old 0x"))?;
    let (_, new) = head.split_once("->")?;
    let mut w = new.split_whitespace();
    let (_, ts, _, name) = (w.next()?, w.next()?, w.next()?, w.next()?);
    let timestamp = u32::try_from(hexes(ts).first().copied()?).ok()?;
    if !report.lines().any(|l| l.trim() == "all sites located") {
        return None;
    }
    let from = report.find("Rust for native/src/build_table.rs")?;
    let mut p = Parts::default();
    // the engine functions' recorded bytes: `  bytes (n): XX XX ..` in each one's block
    for block in report[..from].split(&"=".repeat(100)) {
        let Some(field) = block.split_whitespace().next() else { continue };
        if !ENGINE_NAMES.contains(&field) {
            continue;
        }
        let line = block.lines().find_map(|l| l.trim().strip_prefix("bytes ("))?;
        let (n, rest) = line.split_once("): ")?;
        let b: Vec<u8> =
            rest.split(' ').map_while(|t| (t.len() == 2).then(|| u8::from_str_radix(t, 16).ok()).flatten()).collect();
        if b.len() != n.parse::<usize>().ok()? {
            return None;
        }
        p.bytes.insert(field.to_owned(), b);
    }
    let mut layout = None;
    for line in report[from..].lines().filter_map(|l| l.strip_prefix("  ")) {
        let Some((field, value)) = line.split_once(": ") else { continue };
        let n = hexes(value);
        match field {
            "ui_add_sites" | "fire_done_calls" => {
                for (i, v) in n.iter().enumerate() {
                    p.rva.insert(format!("{field}[{i}]"), u32::try_from(*v).ok()?);
                }
            }
            "engine" => {
                for part in value.trim_start_matches("EngineFns {").split(", ") {
                    let (k, v) = part.trim().split_once(": ")?;
                    p.rva.insert(k.to_owned(), u32::try_from(*hexes(v).first()?).ok()?);
                }
            }
            "layout" => layout = value.split('"').nth(1),
            _ if value.contains("CallPatchSite") => {
                let (outer, callee) = value.split_once("callee:")?;
                p.rva.insert(field.to_owned(), u32::try_from(*hexes(outer).first()?).ok()?);
                p.bytes.insert(field.to_owned(), list(outer)?);
                p.rva.insert(format!("{field}.callee"), u32::try_from(*hexes(callee).first()?).ok()?);
                p.bytes.insert(format!("{field}.callee"), list(callee)?);
                if let Some((_, tail)) = value.rsplit_once("} }") {
                    for part in tail.trim().trim_end_matches(')').split(", ").filter(|s| !s.is_empty()) {
                        let (k, v) = part.split_once(": ")?;
                        p.offsets.insert(k.to_owned(), usize::try_from(*hexes(v).first()?).ok()?);
                    }
                }
            }
            _ => {
                p.rva.insert(field.to_owned(), u32::try_from(*n.first()?).ok()?);
                p.bytes.insert(field.to_owned(), list(value)?);
            }
        }
    }
    assemble(&p, timestamp, name, layout?)
}

/// Builds that were played and checked: `name timestamp row-hash` per line (`tools/addr/out/verified-builds.txt`). A build whose
/// search finds a row with this hash is confirmed; any other build that the search proves plays as "not yet confirmed".
/// Generated, never typed: `cargo run -p feverlink3_native --example verified_line -- <exe or port report>`.
pub const VERIFIED: &str = include_str!("../../tools/addr/out/verified-builds.txt");

/// `(name, timestamp, row hash)` of each verified build.
pub fn verified() -> impl Iterator<Item = (&'static str, u32, &'static str)> {
    VERIFIED.lines().filter(|l| !l.starts_with('#')).filter_map(|l| {
        let mut w = l.split_whitespace();
        let (name, ts, hash) = (w.next()?, w.next()?, w.next()?);
        Some((name, u32::from_str_radix(ts.strip_prefix("0x")?, 16).ok()?, hash))
    })
}

/// FNV-1a 64 of the row's every field: two games that found the same addresses have the same value (the join compares it,
/// and the verified list holds it).
pub fn row_hash(b: &BuildAddrs) -> String {
    let h = format!("{b:?}")
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, c| (h ^ u64::from(c)).wrapping_mul(0x100_0000_01b3));
    format!("{h:016x}")
}

/// A real row for tests: build 40420 as `port_build.py` carried it from 40425 (`tools/addr/out/port-6ad9ce10-6ac50427.txt`).
pub fn fixture() -> &'static BuildAddrs {
    static ROW: std::sync::OnceLock<BuildAddrs> = std::sync::OnceLock::new();
    ROW.get_or_init(|| {
        parse_report(include_str!("../../tools/addr/out/port-6ad9ce10-6ac50427.txt")).expect("the report")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORTS: [&str; 3] = [
        include_str!("../../tools/addr/out/port-6ac50427-6ad0e4e5.txt"),
        include_str!("../../tools/addr/out/port-6ac50427-6ad9ce10.txt"),
        include_str!("../../tools/addr/out/port-6ad9ce10-6ac50427.txt"),
    ];

    /// Every verified build has a committed port report whose row hashes to the verified value, and every row keeps the
    /// shapes the module relies on (prologue lengths, patch lengths, ascending tool sites, the getter calling the lookup).
    #[test]
    fn the_verified_builds_are_the_rows_of_the_committed_reports() {
        let rows: Vec<BuildAddrs> = REPORTS.iter().map(|r| parse_report(r).expect("a complete report")).collect();
        let verified: Vec<_> = verified().collect();
        assert_eq!(verified.len(), 3);
        for (name, ts, hash) in verified {
            let b = rows.iter().find(|b| b.timestamp == ts).unwrap_or_else(|| panic!("no report for {name}"));
            assert_eq!((b.name, row_hash(b).as_str()), (name, hash));
            let hooks = [&b.command_list_add, &b.apply_command, &b.town_seed, &b.preview_event, &b.town_edges];
            assert!(hooks.into_iter().flatten().all(|s| s.prologue.len() >= crate::detour::JMP_ABS_LEN));
            assert!(b.ui_add_sites.windows(2).all(|w| w[0] < w[1]) && b.ui_add_sites.len() == 12);
            let t = b.tick_count_getter.as_ref().unwrap();
            let rel = i32::from_le_bytes(t.expected[15..19].try_into().unwrap());
            assert_eq!(
                (t.rva + 19).wrapping_add(rel as u32),
                b.engine.component_lookup.unwrap(),
                "the getter calls the lookup"
            );
            let changed: Vec<usize> = (0..27).filter(|&i| t.expected[i] != t.replacement[i]).collect();
            assert_eq!(changed, [TICK_GETTER_DISP_AT]);
        }
        assert_eq!(fixture().preview_owner.as_ref().unwrap().ui_e, 0xb98);
        assert_eq!(rows[1].preview_owner.as_ref().unwrap().ui_e, 0xdc0, "40425: the game interface grew by 0x228");
    }

    #[test]
    fn the_seed_patch_is_mov_eax_ret_padded_with_int3() {
        assert_eq!(LUA_SEED, 0x464C_3353);
        assert_eq!(&LUA_SEED_PATCH[..6], &[0xB8, 0x53, 0x33, 0x4C, 0x46, 0xC3]);
        assert!(LUA_SEED_PATCH[6..].iter().all(|&b| b == 0xCC));
        assert_eq!(fixture().kind_by_name("build_proposal"), Some(52));
        assert_eq!(fixture().ui_site_index(fixture().ui_add_sites[11]), Some(11));
    }
}
