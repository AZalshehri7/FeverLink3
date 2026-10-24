//! Everything that depends on a particular game build lives here and nowhere else.
//!
//! A build is identified by the link timestamp in its PE header. Each entry carries the relative virtual
//! addresses the hooks need and the exact prologue bytes [`crate::detour::install`] must find there. An unknown
//! build means no hooks: the mod runs degraded and says so. Entries are produced by the address tool in
//! `tools/` from a copy of the game executable, never typed by hand.

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

/// The whole tickCount getter: `sub rsp,28h ; mov rax,[rcx+10h] ; mov rcx,[rcx+8] ; mov edx,[rax] ; call <component lookup> ;
/// mov eax,[rax+3Ch] ; add rsp,28h ; ret`, with the call's displacement of one build.
pub const fn tick_getter(call: [u8; 4]) -> [u8; 27] {
    [
        0x48,
        0x83,
        0xEC,
        0x28,
        0x48,
        0x8B,
        0x41,
        0x10,
        0x48,
        0x8B,
        0x49,
        0x08,
        0x8B,
        0x10,
        0xE8,
        call[0],
        call[1],
        call[2],
        call[3],
        0x8B,
        0x40,
        TICK_COUNT_OFFSET,
        0x48,
        0x83,
        0xC4,
        0x28,
        0xC3,
    ]
}

static TICK_GETTER_40420: [u8; 27] = tick_getter([0xAD, 0xAF, 0xE9, 0xFF]);
static STEP_GETTER_40420: [u8; 27] = step_getter_patch(TICK_GETTER_40420);
static TICK_GETTER_40423: [u8; 27] = tick_getter([0x9D, 0xAF, 0xE9, 0xFF]);
static STEP_GETTER_40423: [u8; 27] = step_getter_patch(TICK_GETTER_40423);

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
}

impl EngineFns {
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

/// Known builds. Rows are generated by `tools/addr/find_sites.py` (the first) and `tools/addr/port_build.py` (each later build,
/// from the one before); do not edit by hand. The tests compare each row with its committed report.
pub const BUILDS: &[BuildAddrs] = &[
    // TransportFever3.exe build 40420: PE timestamp 0x6ac50427, linked 2026-10-06 14:22:31 UTC.
    // sites located with findings from MPFever (MIT, dkdknight)
    BuildAddrs {
        timestamp: 0x6ac5_0427,
        name: "40420",
        command_list_add: Some(HookSite {
            rva: 0x009d_4040,
            prologue: &[
                0x40, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x8D, 0x6C, 0x24,
                0x98,
            ],
        }),
        apply_command: Some(HookSite {
            rva: 0x009e_33b0,
            prologue: &[0x48, 0x89, 0x5C, 0x24, 0x18, 0x48, 0x89, 0x74, 0x24, 0x20, 0x55, 0x57, 0x41, 0x54],
        }),
        // sub_8d4270; its only caller is 0x3b9d4e inside 0x3b9ae0. The prologue is the four stores to the argument
        // home slots (`mov [rsp+10h],rbx ; mov [rsp+18h],rbp ; mov [rsp+20h],rsi ; mov [rsp+8],rcx`). The seed is
        // the ninth argument: `mov edx,[rsp+90h]` after `push rdi,r14,r15 ; sub rsp,30h`, stored at [rbx+84h].
        town_seed: Some(HookSite {
            rva: 0x008d_4270,
            prologue: &[
                0x48, 0x89, 0x5C, 0x24, 0x10, 0x48, 0x89, 0x6C, 0x24, 0x18, 0x48, 0x89, 0x74, 0x24, 0x20, 0x48, 0x89,
                0x4C, 0x24, 0x08,
            ],
        }),
        // TownDeveloper::Develop: `mov [rsp+18h],r8d ; push rbp,rbx,rsi,rdi,r12,r13,r14` (15 bytes, no rip-relative
        // operand). Stack argument 5 at entry is `[rsp+28h]`: a pointer to the u32 minstd state.
        town_develop: Some(HookSite {
            rva: 0x008d_e0b0,
            prologue: &[0x44, 0x89, 0x44, 0x24, 0x18, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56],
        }),
        // `mov rax,rsp ; push rbx,rsi,rdi,r12,r13,r14,r15` (14 bytes, no rip-relative operand; the next instruction is `sub rsp,0xc00`)
        script_convert: Some(HookSite {
            rva: 0x010c_6750,
            prologue: &[0x48, 0x8B, 0xC4, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57],
        }),
        // `mov [rsp+8],rbx ; push rbp,rsi,rdi,r12,r13,r14` (14 bytes, no rip-relative operand)
        convert_update: Some(HookSite {
            rva: 0x010c_6020,
            prologue: &[0x48, 0x89, 0x5C, 0x24, 0x08, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56],
        }),
        // `mov [rsp+8],rbx ; mov [rsp+10h],rsi ; push rbp,rdi,r14` (14 bytes, no rip-relative operand; next `lea rbp,[rsp-1Fh]`)
        preview_event: Some(HookSite {
            rva: 0x00f4_4c00,
            prologue: &[0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x74, 0x24, 0x10, 0x55, 0x57, 0x41, 0x56],
        }),
        // `call 0x642dc0` at `0x64cb41` in UI::CGameUI::CGameUI (`0x647cb0`), after `mov [rbp+7A0h],r13 ; mov [rbp+7A8h],r12 ;
        // mov rax,[rbp-48h] ; mov [rbp+7B0h],rax ; lea rdx,[rbp+7A0h]`; callee: `mov qword [rcx+38h],0 ; lea rax,[rip+...]` (the
        // lambda's vtable `0x36cdee8`, whose call slot is `0x666420`)
        preview_owner: Some(PreviewOwnerSite {
            call: CallPatchSite {
                rva: 0x0064_cb41,
                expected: &[0xE8, 0x7A, 0x62, 0xFF, 0xFF],
                callee: CallSite {
                    rva: 0x0064_2dc0,
                    prologue: &[
                        0x48, 0xC7, 0x41, 0x38, 0x00, 0x00, 0x00, 0x00, 0x48, 0x8D, 0x05, 0x19, 0xB1, 0x08, 0x03,
                    ],
                },
            },
            y_a: 0x18,
            y_b: 0xc8,
            z_getter_slot: 1,
            r_c: 0x18,
            r_dispatcher: 0x1d8,
            ui_e: 0xb98,
        }),
        // `push rbx,rbp,rsi,rdi,r12,r14,r15 ; sub rsp,50h`
        town_update_size: Some(HookSite {
            rva: 0x009e_12b0,
            prologue: &[0x40, 0x53, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x56, 0x41, 0x57, 0x48, 0x83, 0xEC, 0x50],
        }),
        // `mov [rsp+10h],rbx ; vmovss [rsp+18h],xmm2 ; push rbp,rsi,rdi` (14 bytes, no rip-relative operand)
        town_candidates: Some(HookSite {
            rva: 0x0096_77c0,
            prologue: &[0x48, 0x89, 0x5C, 0x24, 0x10, 0xC5, 0xFA, 0x11, 0x54, 0x24, 0x18, 0x55, 0x56, 0x57],
        }),
        // `mov rax,rsp ; mov [rax+8],rbx ; push rbp,rsi,rdi,r12,r13` (14 bytes, no rip-relative operand)
        town_edges: Some(HookSite {
            rva: 0x0095_d100,
            prologue: &[0x48, 0x8B, 0xC4, 0x48, 0x89, 0x58, 0x08, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55],
        }),
        // `call 0x966150` (std::sort of the gathered records) at the end of `0x9677c0`; callee: `mov [rsp+8],rbx ;
        // mov [rsp+10h],rbp ; mov [rsp+18h],rsi ; push rdi`
        town_sort_call: Some(CallPatchSite {
            rva: 0x0096_7c16,
            expected: &[0xE8, 0x35, 0xE5, 0xFF, 0xFF],
            callee: CallSite {
                rva: 0x0096_6150,
                prologue: &[
                    0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x6C, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57,
                ],
            },
        }),
        // `call 0x3055870` (RandomInt(rng, 0, n)) at `0x95d627` in `0x95d100`; callee: `sub rsp,28h ; lea r9d,[r8-1] ;
        // cmp edx,r9d ; jg ; mov r8d,edx`
        town_pick_call: Some(CallPatchSite {
            rva: 0x0095_d627,
            expected: &[0xE8, 0x44, 0x82, 0x6F, 0x02],
            callee: CallSite {
                rva: 0x0305_5870,
                prologue: &[
                    0x48, 0x83, 0xEC, 0x28, 0x45, 0x8D, 0x48, 0xFF, 0x41, 0x3B, 0xD1, 0x7F, 0x14, 0x44, 0x8B, 0xC2,
                ],
            },
        }),
        // `call 0x2b7a1b0` (RiverField::At, our name; TPF3-MP calls it StreetField::At) at `0x96962e` in the street developer's
        // open pass `0x969140`, its only caller; callee: `push rbp,rsi,r12,r13,r14,r15 ; sub rsp,98h` (18 bytes)
        town_field_call: Some(CallPatchSite {
            rva: 0x0096_962e,
            expected: &[0xE8, 0x7D, 0x0B, 0x21, 0x02],
            callee: CallSite {
                rva: 0x02b7_a1b0,
                prologue: &[
                    0x40, 0x55, 0x56, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x81, 0xEC, 0x98, 0x00,
                    0x00, 0x00,
                ],
            },
        }),
        // `call 0x11f690` (CGame::Sync) at `0x11f446` in CGame::Step (`0x11f3f0`), after `movsxd rdi,[rdx+218h] ; mov rcx,rbx ;
        // add rdi,[rdx+1A8h] ; mov rdx,rsi` and before `test al,al ; je`; callee: `mov [rsp+10h],rbx ; push rbp,rsi,rdi,r12,r13,r14,r15`
        batch_pace: Some(CallPatchSite {
            rva: 0x0011_f446,
            expected: &[0xE8, 0x45, 0x02, 0x00, 0x00],
            callee: CallSite {
                rva: 0x0011_f690,
                prologue: &[
                    0x48, 0x89, 0x5C, 0x24, 0x10, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57,
                ],
            },
        }),
        // `call 0x2bb62a0` (deque<int>::_Insert_range, its only caller) at `0x2bba103` in ecs::Engine::EndModification (`0x2bb9ea0`),
        // after `lea rcx,[r13+0D8h] ; mov r9,[r12] ; mov rax,[r9+8] ; mov [rsp+20h],rax ; mov r9,[r9] ; mov r8,[r13+0F8h] ;
        // lea rdx,[rbp-79h]` (r12 = &engine->modification, its removed ids at [+0]..[+8]) and before `mov rdx,[r12] ; mov [r12],r15`
        // (the modification is released); callee: `mov [rsp+8],rbx ; mov [rsp+10h],rbp ; mov [rsp+18h],rsi ; push rdi ; push r12`
        free_order: Some(CallPatchSite {
            rva: 0x02bb_a103,
            expected: &[0xE8, 0x98, 0xC1, 0xFF, 0xFF],
            callee: CallSite {
                rva: 0x02bb_62a0,
                prologue: &[
                    0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x6C, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57,
                    0x41, 0x54,
                ],
            },
        }),
        // `sub rsp,28h ; mov rax,[rcx+10h] ; mov rcx,[rcx+8] ; mov edx,[rax]` (then a call to the component lookup)
        game_time_getter: Some(CallSite {
            rva: 0x002a_96c0,
            prologue: &[0x48, 0x83, 0xEC, 0x28, 0x48, 0x8B, 0x41, 0x10, 0x48, 0x8B, 0x49, 0x08, 0x8B, 0x10],
        }),
        // the tickCount getter, the whole function: `sub rsp,28h ; mov rax,[rcx+10h] ; mov rcx,[rcx+8] ; mov edx,[rax] ;
        // call 0x1445c0 (component lookup) ; mov eax,[rax+3Ch] ; add rsp,28h ; ret`; only the load's displacement changes.
        // Its direct callers (rel32 scan of .text): 0x577e0b, 0x5a51f1 (interface tools), 0x900170, 0x90edc0, 0x96ad13,
        // 0xa38dda, 0xa7db6d, 0xac2dd3, 0x2698385.
        tick_count_getter: Some(PatchSite {
            rva: 0x002a_9600,
            expected: &TICK_GETTER_40420,
            replacement: &STEP_GETTER_40420,
        }),
        script_pool: Some(PatchSite {
            rva: 0x00ab_00fc,
            // cmp eax,8 ; jge +0x125 ; mov dword [rsp+20h],1 ; call hardware_concurrency
            expected: &[
                0x83, 0xF8, 0x08, 0x0F, 0x8D, 0x25, 0x01, 0x00, 0x00, 0xC7, 0x44, 0x24, 0x20, 0x01, 0x00, 0x00, 0x00,
                0xE8, 0xBE, 0x24, 0x5A, 0x02,
            ],
            // cmp eax,8 ; nop6 ; mov dword [rsp+20h],1 ; xor eax,eax ; nop3   (same 22 bytes)
            // Follows the patch MPFever applies; its effect in the game is verified in stage 0.3 before use.
            replacement: &SCRIPT_POOL_PATCH,
        }),
        // makeseed (lua 5.2.2, RVA 0x2fc8fe0): `push rbx ; sub rsp,50h ; mov rax,[rip+..] ; xor rax,rsp ;
        // mov [rsp+48h],rax ; mov rbx,rcx ; xor ecx,ecx`
        // site located with findings from MPFever (MIT, dkdknight)
        lua_makeseed: Some(PatchSite {
            rva: 0x02fc_8fe0,
            expected: &[
                0x40, 0x53, 0x48, 0x83, 0xEC, 0x50, 0x48, 0x8B, 0x05, 0x4B, 0x4A, 0xD2, 0x00, 0x48, 0x33, 0xC4, 0x48,
                0x89, 0x44, 0x24, 0x48, 0x48, 0x8B, 0xD9, 0x33, 0xC9,
            ],
            replacement: &LUA_SEED_PATCH,
        }),
        // Command::Command(Command&&): `mov rax,[rdx] ; xor r10d,r10d ; mov [rdx],r10 ; mov [rcx],rax ; mov rax,[rdx+8]`
        command_move: Some(CallSite {
            rva: 0x009d_0430,
            prologue: &[0x48, 0x8B, 0x02, 0x45, 0x33, 0xD2, 0x4C, 0x89, 0x12, 0x48, 0x89, 0x01, 0x48, 0x8B, 0x42, 0x08],
        }),
        // Command::~Command(): `push rbx ; sub rsp,20h ; mov rbx,rcx ; mov rcx,[rcx+28h] ; test rcx,rcx`
        command_destroy: Some(CallSite {
            rva: 0x009d_0670,
            prologue: &[0x40, 0x53, 0x48, 0x83, 0xEC, 0x20, 0x48, 0x8B, 0xD9, 0x48, 0x8B, 0x49, 0x28, 0x48, 0x85, 0xC9],
        }),
        // the out handle's destructor: `push rbx ; sub rsp,20h ; mov rbx,[rcx] ; test rbx,rbx ; je ; mov rcx,[rbx+8]`
        handle_destroy: Some(CallSite {
            rva: 0x0304_0c90,
            prologue: &[
                0x40, 0x53, 0x48, 0x83, 0xEC, 0x20, 0x48, 0x8B, 0x19, 0x48, 0x85, 0xDB, 0x74, 0x30, 0x48, 0x8B, 0x4B,
                0x08,
            ],
        }),
        // Bulldozer, ConstructionBuilder, an unnamed tool, LaneModifier (3), ModuleBuilder, StreetBuilder, a
        // street-terminal tool, TrackModifier and the entity window's two builtins (names in the address tool's report).
        ui_add_sites: &[
            0x004d_396f,
            0x0051_b56c,
            0x0052_8504,
            0x0053_7ee5,
            0x0053_8649,
            0x0053_87cd,
            0x0054_2eba,
            0x0058_91b4,
            0x0059_48dd,
            0x005b_f08d,
            0x028a_3024,
            0x028a_3871,
        ],
        fire_done_calls: &[0x004d_8860, 0x0058_c0b0, 0x0059_6a70],
        // movsx rcx, byte [payload + 0x9b8] in apply_command and in Command::~Command
        kind_offset: Some(0x9b8),
        // the dispatcher's case 52 is the handler that references the string "WorldBuildProposal"
        kinds: &[("build_proposal", 52)],
        engine: EngineFns {
            proposal_ctor: Some(0x004c_6870),
            proposal_dtor: Some(0x0048_e220),
            proposal_copy: Some(0x004c_6640),
            proposal_assign: Some(0x004c_ed30),
            engine_new: Some(0x0318_c530),
            engine_delete: Some(0x0318_c56c),
            refund_insert: Some(0x004b_e210),
            add_entity: Some(0x02bb_8940),
            remove_entity: Some(0x02bb_c700),
            game_time_advance: Some(0x00ba_d780),
            component_lookup: Some(0x0014_45c0),
        },
        layout: Some("40420"),
    },
    // TransportFever3.exe build 40423 ("Second Stability Update"): PE timestamp 0x6ad0e4e5, linked 2026-10-15 14:36:21 UTC.
    // Carried over from 40420 by tools/addr/port_build.py (report tools/addr/out/port-6ac50427-6ad0e4e5.txt): every hooked,
    // patched or called function below is the same code as in 40420 apart from relink fields (whole function compared, its
    // callees one level deep); the hold list, the callbacks' vtables and the preview listener's construction sit in functions
    // of the game interface whose class grew by 0x228 bytes (same instructions, structure offsets above 0x4b0 moved), so
    // `ui_e` moved from 0xb98 to 0xdc0. tools/addr/out/6ad0e4e5.txt is find_sites.py's independent report of the hold sites.
    BuildAddrs {
        timestamp: 0x6ad0_e4e5,
        name: "40423",
        command_list_add: Some(HookSite {
            rva: 0x009d_9140,
            prologue: &[
                0x40, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x8D, 0x6C, 0x24,
                0x98,
            ],
        }),
        apply_command: Some(HookSite {
            rva: 0x009e_84b0,
            prologue: &[0x48, 0x89, 0x5C, 0x24, 0x18, 0x48, 0x89, 0x74, 0x24, 0x20, 0x55, 0x57, 0x41, 0x54],
        }),
        town_seed: Some(HookSite {
            rva: 0x008d_9380,
            prologue: &[
                0x48, 0x89, 0x5C, 0x24, 0x10, 0x48, 0x89, 0x6C, 0x24, 0x18, 0x48, 0x89, 0x74, 0x24, 0x20, 0x48, 0x89,
                0x4C, 0x24, 0x08,
            ],
        }),
        town_develop: Some(HookSite {
            rva: 0x008e_31c0,
            prologue: &[0x44, 0x89, 0x44, 0x24, 0x18, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56],
        }),
        script_convert: Some(HookSite {
            rva: 0x010c_d5a0,
            prologue: &[0x48, 0x8B, 0xC4, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57],
        }),
        convert_update: Some(HookSite {
            rva: 0x010c_ce70,
            prologue: &[0x48, 0x89, 0x5C, 0x24, 0x08, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56],
        }),
        preview_event: Some(HookSite {
            rva: 0x00f4_ba50,
            prologue: &[0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x74, 0x24, 0x10, 0x55, 0x57, 0x41, 0x56],
        }),
        // `call 0x643c40` at `0x64d9c1` in UI::CGameUI::CGameUI (`0x648b30`); the listener's call operator is `0x6671b0`, which
        // reads the game interface's `e` at +0xdc0 (40420: +0xb98), the other captures' offsets unchanged
        preview_owner: Some(PreviewOwnerSite {
            call: CallPatchSite {
                rva: 0x0064_d9c1,
                expected: &[0xE8, 0x7A, 0x62, 0xFF, 0xFF],
                callee: CallSite {
                    rva: 0x0064_3c40,
                    prologue: &[
                        0x48, 0xC7, 0x41, 0x38, 0x00, 0x00, 0x00, 0x00, 0x48, 0x8D, 0x05, 0x59, 0x14, 0x0A, 0x03,
                    ],
                },
            },
            y_a: 0x18,
            y_b: 0xc8,
            z_getter_slot: 1,
            r_c: 0x18,
            r_dispatcher: 0x1d8,
            ui_e: 0xdc0,
        }),
        town_update_size: Some(HookSite {
            rva: 0x009e_63b0,
            prologue: &[0x40, 0x53, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x56, 0x41, 0x57, 0x48, 0x83, 0xEC, 0x50],
        }),
        town_candidates: Some(HookSite {
            rva: 0x0096_c8c0,
            prologue: &[0x48, 0x89, 0x5C, 0x24, 0x10, 0xC5, 0xFA, 0x11, 0x54, 0x24, 0x18, 0x55, 0x56, 0x57],
        }),
        town_edges: Some(HookSite {
            rva: 0x0096_2200,
            prologue: &[0x48, 0x8B, 0xC4, 0x48, 0x89, 0x58, 0x08, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55],
        }),
        town_sort_call: Some(CallPatchSite {
            rva: 0x0096_cd16,
            expected: &[0xE8, 0x35, 0xE5, 0xFF, 0xFF],
            callee: CallSite {
                rva: 0x0096_b250,
                prologue: &[
                    0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x6C, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57,
                ],
            },
        }),
        town_pick_call: Some(CallPatchSite {
            rva: 0x0096_2727,
            expected: &[0xE8, 0x94, 0x88, 0x70, 0x02],
            callee: CallSite {
                rva: 0x0306_afc0,
                prologue: &[
                    0x48, 0x83, 0xEC, 0x28, 0x45, 0x8D, 0x48, 0xFF, 0x41, 0x3B, 0xD1, 0x7F, 0x14, 0x44, 0x8B, 0xC2,
                ],
            },
        }),
        town_field_call: Some(CallPatchSite {
            rva: 0x0096_e72e,
            expected: &[0xE8, 0x1D, 0x31, 0x21, 0x02],
            callee: CallSite {
                rva: 0x02b8_1850,
                prologue: &[
                    0x40, 0x55, 0x56, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x81, 0xEC, 0x98, 0x00,
                    0x00, 0x00,
                ],
            },
        }),
        batch_pace: Some(CallPatchSite {
            rva: 0x0011_f456,
            expected: &[0xE8, 0x45, 0x02, 0x00, 0x00],
            callee: CallSite {
                rva: 0x0011_f6a0,
                prologue: &[
                    0x48, 0x89, 0x5C, 0x24, 0x10, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57,
                ],
            },
        }),
        free_order: Some(CallPatchSite {
            rva: 0x02bc_1a23,
            expected: &[0xE8, 0x98, 0xC1, 0xFF, 0xFF],
            callee: CallSite {
                rva: 0x02bb_dbc0,
                prologue: &[
                    0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x6C, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57,
                    0x41, 0x54,
                ],
            },
        }),
        game_time_getter: Some(CallSite {
            rva: 0x002a_96e0,
            prologue: &[0x48, 0x83, 0xEC, 0x28, 0x48, 0x8B, 0x41, 0x10, 0x48, 0x8B, 0x49, 0x08, 0x8B, 0x10],
        }),
        // the call leads to the component lookup 0x1445d0
        tick_count_getter: Some(PatchSite {
            rva: 0x002a_9620,
            expected: &TICK_GETTER_40423,
            replacement: &STEP_GETTER_40423,
        }),
        script_pool: Some(PatchSite {
            rva: 0x00ab_528c,
            expected: &[
                0x83, 0xF8, 0x08, 0x0F, 0x8D, 0x25, 0x01, 0x00, 0x00, 0xC7, 0x44, 0x24, 0x20, 0x01, 0x00, 0x00, 0x00,
                0xE8, 0x7E, 0x2A, 0x5B, 0x02,
            ],
            replacement: &SCRIPT_POOL_PATCH,
        }),
        lua_makeseed: Some(PatchSite {
            rva: 0x02fd_e370,
            expected: &[
                0x40, 0x53, 0x48, 0x83, 0xEC, 0x50, 0x48, 0x8B, 0x05, 0xDB, 0x96, 0xD2, 0x00, 0x48, 0x33, 0xC4, 0x48,
                0x89, 0x44, 0x24, 0x48, 0x48, 0x8B, 0xD9, 0x33, 0xC9,
            ],
            replacement: &LUA_SEED_PATCH,
        }),
        command_move: Some(CallSite {
            rva: 0x009d_5530,
            prologue: &[0x48, 0x8B, 0x02, 0x45, 0x33, 0xD2, 0x4C, 0x89, 0x12, 0x48, 0x89, 0x01, 0x48, 0x8B, 0x42, 0x08],
        }),
        command_destroy: Some(CallSite {
            rva: 0x009d_5770,
            prologue: &[0x40, 0x53, 0x48, 0x83, 0xEC, 0x20, 0x48, 0x8B, 0xD9, 0x48, 0x8B, 0x49, 0x28, 0x48, 0x85, 0xC9],
        }),
        handle_destroy: Some(CallSite {
            rva: 0x0305_63e0,
            prologue: &[
                0x40, 0x53, 0x48, 0x83, 0xEC, 0x20, 0x48, 0x8B, 0x19, 0x48, 0x85, 0xDB, 0x74, 0x30, 0x48, 0x8B, 0x4B,
                0x08,
            ],
        }),
        // the same tools in the same order as 40420
        ui_add_sites: &[
            0x004d_479f,
            0x0051_c3ac,
            0x0052_9334,
            0x0053_8d15,
            0x0053_9479,
            0x0053_95fd,
            0x0054_3cea,
            0x0058_9fc4,
            0x0059_56ed,
            0x005b_fe9d,
            0x028a_a534,
            0x028a_ad81,
        ],
        fire_done_calls: &[0x004d_9690, 0x0058_cec0, 0x0059_7880],
        kind_offset: Some(0x9b8),
        kinds: &[("build_proposal", 52)],
        engine: EngineFns {
            proposal_ctor: Some(0x004c_76a0),
            proposal_dtor: Some(0x0048_f050),
            proposal_copy: Some(0x004c_7470),
            proposal_assign: Some(0x004c_fb60),
            engine_new: Some(0x031a_1c80),
            engine_delete: Some(0x031a_1cbc),
            refund_insert: Some(0x004b_f040),
            add_entity: Some(0x02bc_0260),
            remove_entity: Some(0x02bc_4020),
            game_time_advance: Some(0x00bb_2970),
            component_lookup: Some(0x0014_45d0),
        },
        // all 22 layout witnesses are the same code as in 40420
        layout: Some("40420"),
    },
    // TransportFever3.exe build 40425: PE timestamp 0x6ad9ce10, linked 2026-10-22 08:49:20 UTC. Carried over from 40420 by
    // tools/addr/port_build.py (report tools/addr/out/port-6ac50427-6ad9ce10.txt; no copy of the 40423 executable was kept):
    // the report equals the 40420 -> 40423 one line for line, so every site, prologue and offset below is the one of 40423
    // (same code at the same places, game interface class 0x228 bytes larger than 40420, `ui_e` 0xdc0).
    // tools/addr/out/6ad9ce10.txt is find_sites.py's independent report of the hold sites.
    BuildAddrs {
        timestamp: 0x6ad9_ce10,
        name: "40425",
        command_list_add: Some(HookSite {
            rva: 0x009d_9140,
            prologue: &[
                0x40, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x8D, 0x6C, 0x24,
                0x98,
            ],
        }),
        apply_command: Some(HookSite {
            rva: 0x009e_84b0,
            prologue: &[0x48, 0x89, 0x5C, 0x24, 0x18, 0x48, 0x89, 0x74, 0x24, 0x20, 0x55, 0x57, 0x41, 0x54],
        }),
        town_seed: Some(HookSite {
            rva: 0x008d_9380,
            prologue: &[
                0x48, 0x89, 0x5C, 0x24, 0x10, 0x48, 0x89, 0x6C, 0x24, 0x18, 0x48, 0x89, 0x74, 0x24, 0x20, 0x48, 0x89,
                0x4C, 0x24, 0x08,
            ],
        }),
        town_develop: Some(HookSite {
            rva: 0x008e_31c0,
            prologue: &[0x44, 0x89, 0x44, 0x24, 0x18, 0x55, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56],
        }),
        script_convert: Some(HookSite {
            rva: 0x010c_d5a0,
            prologue: &[0x48, 0x8B, 0xC4, 0x53, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57],
        }),
        convert_update: Some(HookSite {
            rva: 0x010c_ce70,
            prologue: &[0x48, 0x89, 0x5C, 0x24, 0x08, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56],
        }),
        preview_event: Some(HookSite {
            rva: 0x00f4_ba50,
            prologue: &[0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x74, 0x24, 0x10, 0x55, 0x57, 0x41, 0x56],
        }),
        // `call 0x643c40` at `0x64d9c1` in UI::CGameUI::CGameUI (`0x648b30`); the listener's call operator is `0x6671b0`, which
        // reads the game interface's `e` at +0xdc0 (40420: +0xb98), the other captures' offsets unchanged
        preview_owner: Some(PreviewOwnerSite {
            call: CallPatchSite {
                rva: 0x0064_d9c1,
                expected: &[0xE8, 0x7A, 0x62, 0xFF, 0xFF],
                callee: CallSite {
                    rva: 0x0064_3c40,
                    prologue: &[
                        0x48, 0xC7, 0x41, 0x38, 0x00, 0x00, 0x00, 0x00, 0x48, 0x8D, 0x05, 0x59, 0x14, 0x0A, 0x03,
                    ],
                },
            },
            y_a: 0x18,
            y_b: 0xc8,
            z_getter_slot: 1,
            r_c: 0x18,
            r_dispatcher: 0x1d8,
            ui_e: 0xdc0,
        }),
        town_update_size: Some(HookSite {
            rva: 0x009e_63b0,
            prologue: &[0x40, 0x53, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x56, 0x41, 0x57, 0x48, 0x83, 0xEC, 0x50],
        }),
        town_candidates: Some(HookSite {
            rva: 0x0096_c8c0,
            prologue: &[0x48, 0x89, 0x5C, 0x24, 0x10, 0xC5, 0xFA, 0x11, 0x54, 0x24, 0x18, 0x55, 0x56, 0x57],
        }),
        town_edges: Some(HookSite {
            rva: 0x0096_2200,
            prologue: &[0x48, 0x8B, 0xC4, 0x48, 0x89, 0x58, 0x08, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55],
        }),
        town_sort_call: Some(CallPatchSite {
            rva: 0x0096_cd16,
            expected: &[0xE8, 0x35, 0xE5, 0xFF, 0xFF],
            callee: CallSite {
                rva: 0x0096_b250,
                prologue: &[
                    0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x6C, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57,
                ],
            },
        }),
        town_pick_call: Some(CallPatchSite {
            rva: 0x0096_2727,
            expected: &[0xE8, 0x94, 0x88, 0x70, 0x02],
            callee: CallSite {
                rva: 0x0306_afc0,
                prologue: &[
                    0x48, 0x83, 0xEC, 0x28, 0x45, 0x8D, 0x48, 0xFF, 0x41, 0x3B, 0xD1, 0x7F, 0x14, 0x44, 0x8B, 0xC2,
                ],
            },
        }),
        town_field_call: Some(CallPatchSite {
            rva: 0x0096_e72e,
            expected: &[0xE8, 0x1D, 0x31, 0x21, 0x02],
            callee: CallSite {
                rva: 0x02b8_1850,
                prologue: &[
                    0x40, 0x55, 0x56, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57, 0x48, 0x81, 0xEC, 0x98, 0x00,
                    0x00, 0x00,
                ],
            },
        }),
        batch_pace: Some(CallPatchSite {
            rva: 0x0011_f456,
            expected: &[0xE8, 0x45, 0x02, 0x00, 0x00],
            callee: CallSite {
                rva: 0x0011_f6a0,
                prologue: &[
                    0x48, 0x89, 0x5C, 0x24, 0x10, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57,
                ],
            },
        }),
        free_order: Some(CallPatchSite {
            rva: 0x02bc_1a23,
            expected: &[0xE8, 0x98, 0xC1, 0xFF, 0xFF],
            callee: CallSite {
                rva: 0x02bb_dbc0,
                prologue: &[
                    0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x6C, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57,
                    0x41, 0x54,
                ],
            },
        }),
        game_time_getter: Some(CallSite {
            rva: 0x002a_96e0,
            prologue: &[0x48, 0x83, 0xEC, 0x28, 0x48, 0x8B, 0x41, 0x10, 0x48, 0x8B, 0x49, 0x08, 0x8B, 0x10],
        }),
        // the call leads to the component lookup 0x1445d0; the same bytes as 40423
        tick_count_getter: Some(PatchSite {
            rva: 0x002a_9620,
            expected: &TICK_GETTER_40423,
            replacement: &STEP_GETTER_40423,
        }),
        script_pool: Some(PatchSite {
            rva: 0x00ab_528c,
            expected: &[
                0x83, 0xF8, 0x08, 0x0F, 0x8D, 0x25, 0x01, 0x00, 0x00, 0xC7, 0x44, 0x24, 0x20, 0x01, 0x00, 0x00, 0x00,
                0xE8, 0x7E, 0x2A, 0x5B, 0x02,
            ],
            replacement: &SCRIPT_POOL_PATCH,
        }),
        lua_makeseed: Some(PatchSite {
            rva: 0x02fd_e370,
            expected: &[
                0x40, 0x53, 0x48, 0x83, 0xEC, 0x50, 0x48, 0x8B, 0x05, 0xDB, 0x96, 0xD2, 0x00, 0x48, 0x33, 0xC4, 0x48,
                0x89, 0x44, 0x24, 0x48, 0x48, 0x8B, 0xD9, 0x33, 0xC9,
            ],
            replacement: &LUA_SEED_PATCH,
        }),
        command_move: Some(CallSite {
            rva: 0x009d_5530,
            prologue: &[0x48, 0x8B, 0x02, 0x45, 0x33, 0xD2, 0x4C, 0x89, 0x12, 0x48, 0x89, 0x01, 0x48, 0x8B, 0x42, 0x08],
        }),
        command_destroy: Some(CallSite {
            rva: 0x009d_5770,
            prologue: &[0x40, 0x53, 0x48, 0x83, 0xEC, 0x20, 0x48, 0x8B, 0xD9, 0x48, 0x8B, 0x49, 0x28, 0x48, 0x85, 0xC9],
        }),
        handle_destroy: Some(CallSite {
            rva: 0x0305_63e0,
            prologue: &[
                0x40, 0x53, 0x48, 0x83, 0xEC, 0x20, 0x48, 0x8B, 0x19, 0x48, 0x85, 0xDB, 0x74, 0x30, 0x48, 0x8B, 0x4B,
                0x08,
            ],
        }),
        // the same tools in the same order as 40420 and 40423
        ui_add_sites: &[
            0x004d_479f,
            0x0051_c3ac,
            0x0052_9334,
            0x0053_8d15,
            0x0053_9479,
            0x0053_95fd,
            0x0054_3cea,
            0x0058_9fc4,
            0x0059_56ed,
            0x005b_fe9d,
            0x028a_a534,
            0x028a_ad81,
        ],
        fire_done_calls: &[0x004d_9690, 0x0058_cec0, 0x0059_7880],
        kind_offset: Some(0x9b8),
        kinds: &[("build_proposal", 52)],
        engine: EngineFns {
            proposal_ctor: Some(0x004c_76a0),
            proposal_dtor: Some(0x0048_f050),
            proposal_copy: Some(0x004c_7470),
            proposal_assign: Some(0x004c_fb60),
            engine_new: Some(0x031a_1c80),
            engine_delete: Some(0x031a_1cbc),
            refund_insert: Some(0x004b_f040),
            add_entity: Some(0x02bc_0260),
            remove_entity: Some(0x02bc_4020),
            game_time_advance: Some(0x00bb_2970),
            component_lookup: Some(0x0014_45d0),
        },
        // all 22 layout witnesses are the same code as in 40420
        layout: Some("40420"),
    },
];

pub fn select(timestamp: u32) -> Option<&'static BuildAddrs> {
    select_in(BUILDS, timestamp)
}

pub fn select_in(table: &'static [BuildAddrs], timestamp: u32) -> Option<&'static BuildAddrs> {
    table.iter().find(|b| b.timestamp == timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    static FAKE: &[BuildAddrs] = &[BuildAddrs {
        timestamp: 0x1234,
        name: "test",
        command_list_add: Some(HookSite { rva: 0x1000, prologue: &[0x48; 14] }),
        apply_command: None,
        town_seed: None,
        town_develop: None,
        town_update_size: None,
        script_convert: None,
        convert_update: None,
        preview_event: None,
        preview_owner: None,
        town_candidates: None,
        town_edges: None,
        town_sort_call: None,
        town_pick_call: None,
        town_field_call: None,
        batch_pace: None,
        free_order: None,
        game_time_getter: None,
        tick_count_getter: None,
        script_pool: None,
        lua_makeseed: None,
        command_move: None,
        command_destroy: None,
        handle_destroy: None,
        ui_add_sites: &[0x1100, 0x1200],
        fire_done_calls: &[],
        kind_offset: Some(0x20),
        kinds: &[("build_proposal", 7), ("other", 9)],
        engine: EngineFns::NONE,
        layout: None,
    }];

    #[test]
    fn unknown_build_selects_nothing() {
        assert!(select(0).is_none());
        assert!(select_in(FAKE, 0x9999).is_none());
    }

    #[test]
    fn known_build_selects_its_row() {
        let b = select_in(FAKE, 0x1234).unwrap();
        assert_eq!(b.name, "test");
        assert_eq!(b.command_list_add.as_ref().unwrap().rva, 0x1000);
    }

    #[test]
    fn every_recorded_prologue_is_long_enough_for_a_detour() {
        for b in BUILDS.iter().chain(FAKE) {
            for site in [
                &b.command_list_add,
                &b.apply_command,
                &b.town_seed,
                &b.town_develop,
                &b.town_update_size,
                &b.script_convert,
                &b.convert_update,
                &b.preview_event,
                &b.town_candidates,
                &b.town_edges,
            ]
            .into_iter()
            .flatten()
            {
                assert!(site.prologue.len() >= crate::detour::JMP_ABS_LEN, "{} has a short prologue", b.name);
            }
        }
    }

    #[test]
    fn patch_sites_replace_exactly_as_many_bytes_as_they_expect() {
        for b in BUILDS.iter().chain(FAKE) {
            for p in [&b.script_pool, &b.lua_makeseed, &b.tick_count_getter].into_iter().flatten() {
                assert_eq!(p.expected.len(), p.replacement.len(), "{}: patch length mismatch", b.name);
                assert_ne!(p.expected, p.replacement, "{}: patch changes nothing", b.name);
            }
        }
    }

    #[test]
    fn the_seed_patch_is_mov_eax_ret_padded_with_int3() {
        let p = select(0x6ac5_0427).unwrap().lua_makeseed.as_ref().unwrap();
        assert_eq!(p.rva, 0x02fc_8fe0);
        assert_eq!(p.expected.len(), 26);
        assert_eq!(&p.expected[..6], &[0x40, 0x53, 0x48, 0x83, 0xEC, 0x50], "push rbx ; sub rsp,50h");
        assert_eq!(LUA_SEED, 0x464C_3353);
        assert_eq!(&p.replacement[..6], &[0xB8, 0x53, 0x33, 0x4C, 0x46, 0xC3]);
        assert!(p.replacement[6..].iter().all(|&b| b == 0xCC));
        assert_eq!(p.replacement.len(), p.expected.len());
        assert_eq!(lua_seed_patch::<8>(1), [0xB8, 1, 0, 0, 0, 0xC3, 0xCC, 0xCC]);
    }

    #[test]
    fn the_step_seed_patch_changes_only_the_tick_count_load() {
        let b = select(0x6ac5_0427).unwrap();
        let p = b.tick_count_getter.as_ref().unwrap();
        assert_eq!(p.rva, 0x002a_9600);
        // the same getter shape as the updateCount getter the growth log calls (first 14 bytes, up to the call)
        assert_eq!(&p.expected[..14], b.game_time_getter.as_ref().unwrap().prologue);
        // `call rel32` to the component lookup 0x1445c0, then `mov eax,[rax+3Ch] ; add rsp,28h ; ret`
        let rel = i32::from_le_bytes(p.expected[15..19].try_into().unwrap());
        assert_eq!(p.expected[14], 0xE8);
        assert_eq!((p.rva + 19).wrapping_add(rel as u32), 0x0014_45c0);
        assert_eq!(&p.expected[19..22], &[0x8B, 0x40, TICK_COUNT_OFFSET]);
        assert_eq!(&p.replacement[19..22], &[0x8B, 0x40, UPDATE_COUNT_OFFSET]);
        assert_eq!(&p.expected[22..], &[0x48, 0x83, 0xC4, 0x28, 0xC3]);
        let changed: Vec<usize> = (0..p.expected.len()).filter(|&i| p.expected[i] != p.replacement[i]).collect();
        assert_eq!(changed, [TICK_GETTER_DISP_AT], "one byte: the displacement of the load");
        assert_eq!(step_getter_patch([0u8; 27])[TICK_GETTER_DISP_AT], 0x40);
    }

    #[test]
    fn call_sites_record_a_prologue_to_compare() {
        for b in BUILDS.iter().chain(FAKE) {
            for site in
                [&b.command_move, &b.command_destroy, &b.handle_destroy, &b.game_time_getter].into_iter().flatten()
            {
                assert!(site.prologue.len() >= CALL_PROLOGUE_MIN, "{}: call site prologue too short", b.name);
            }
        }
    }

    #[test]
    fn tool_sites_are_nonzero_and_strictly_ascending() {
        // the address tool prints them sorted; a duplicate or a stray edit shows here
        for b in BUILDS.iter().chain(FAKE) {
            assert!(b.ui_add_sites.iter().all(|&r| r != 0), "{}: zero tool site", b.name);
            assert!(b.ui_add_sites.windows(2).all(|w| w[0] < w[1]), "{}: tool sites not strictly ascending", b.name);
        }
    }

    #[test]
    fn kinds_have_distinct_names_and_numbers() {
        for b in BUILDS.iter().chain(FAKE) {
            let names: std::collections::BTreeSet<_> = b.kinds.iter().map(|(n, _)| *n).collect();
            let numbers: std::collections::BTreeSet<_> = b.kinds.iter().map(|(_, k)| *k).collect();
            assert_eq!(names.len(), b.kinds.len(), "{}: duplicate kind name", b.name);
            assert_eq!(numbers.len(), b.kinds.len(), "{}: duplicate kind number", b.name);
            assert!(b
                .kinds
                .iter()
                .all(|(n, _)| !n.is_empty() && n.bytes().all(|c| c.is_ascii_lowercase() || c == b'_')));
        }
    }

    #[test]
    fn kind_and_site_lookups() {
        let b = select_in(FAKE, 0x1234).unwrap();
        assert_eq!(b.kind_by_name("build_proposal"), Some(7));
        assert_eq!(b.kind_by_name("nope"), None);
        assert_eq!(b.kind_name(9), Some("other"));
        assert_eq!(b.kind_name(10), None);
        assert_eq!(b.ui_site_index(0x1200), Some(1));
        assert_eq!(b.ui_site_index(0x1201), None);
    }

    #[test]
    fn build_40420_is_known() {
        let b = select(0x6ac5_0427).expect("40420 row");
        assert_eq!(b.name, "40420");
        assert_eq!(b.command_list_add.as_ref().unwrap().rva, 0x9d4040);
        assert_eq!(b.apply_command.as_ref().unwrap().rva, 0x9e33b0);
        assert_eq!(b.script_pool.as_ref().unwrap().rva, 0xab00fc);
        assert_eq!(b.town_seed.as_ref().unwrap().rva, 0x8d4270);
        assert_eq!(b.town_develop.as_ref().unwrap().rva, 0x8de0b0);
        assert_eq!(b.town_update_size.as_ref().unwrap().rva, 0x9e12b0);
        assert_eq!(b.town_candidates.as_ref().unwrap().rva, 0x9677c0);
        assert_eq!(b.town_edges.as_ref().unwrap().rva, 0x95d100);
        let owner = b.preview_owner.as_ref().map(|o| o.call.clone());
        for (site, call) in [
            (&owner, 0x64cb41u32),
            (&b.town_sort_call, 0x967c16),
            (&b.town_pick_call, 0x95d627),
            (&b.town_field_call, 0x96962e),
            (&b.batch_pace, 0x11f446),
            (&b.free_order, 0x2bba103),
        ] {
            let s = site.as_ref().unwrap();
            assert_eq!(s.rva, call);
            // the displacement leads to the recorded callee
            let rel = i32::from_le_bytes(s.expected[1..5].try_into().unwrap());
            assert_eq!(s.expected[0], 0xE8);
            assert_eq!(s.rva.wrapping_add(5).wrapping_add(rel as u32), s.callee.rva);
            assert!(s.callee.prologue.len() >= CALL_PROLOGUE_MIN);
        }
        assert_eq!(b.game_time_getter.as_ref().unwrap().rva, 0x2a96c0);
    }

    /// The committed report of the address tool for build 40420: the audit trail of the row above.
    const REPORT_40420: &str = include_str!("../../tools/addr/out/6ac50427.txt");

    fn report_line(field: &str) -> &'static str {
        let key = format!("{field}: ");
        REPORT_40420
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with(&key))
            .unwrap_or_else(|| panic!("the report has no `{field}` row line"))
    }

    /// The numbers (`0x..`) of the `&[..]` list that follows `label` on a row line.
    fn numbers_after(line: &str, label: &str) -> Vec<u64> {
        let from = line.find(&format!("{label}: &[")).unwrap_or_else(|| panic!("no `{label}` list in `{line}`"));
        let list = &line[from + label.len() + 4..];
        let list = &list[..list.find(']').expect("closing bracket")];
        list.split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| u64::from_str_radix(t.trim_start_matches("0x"), 16).expect("hex number"))
            .collect()
    }

    fn rva_of(line: &str) -> u32 {
        let from = line.find("rva: 0x").expect("rva") + 7;
        let digits: String = line[from..].chars().take_while(char::is_ascii_hexdigit).collect();
        u32::from_str_radix(&digits, 16).expect("hex rva")
    }

    fn bytes_of(line: &str, label: &str) -> Vec<u8> {
        numbers_after(line, label).into_iter().map(|n| u8::try_from(n).expect("byte")).collect()
    }

    #[test]
    fn build_40420_row_equals_the_committed_address_tool_report() {
        let b = select(0x6ac5_0427).expect("40420 row");
        for (field, site) in [("command_list_add", &b.command_list_add), ("apply_command", &b.apply_command)] {
            let (line, site) = (report_line(field), site.as_ref().unwrap());
            assert_eq!(rva_of(line), site.rva, "{field}: rva");
            assert_eq!(bytes_of(line, "prologue"), site.prologue, "{field}: prologue");
        }
        for (field, site) in [
            ("command_move", &b.command_move),
            ("command_destroy", &b.command_destroy),
            ("handle_destroy", &b.handle_destroy),
        ] {
            let (line, site) = (report_line(field), site.as_ref().unwrap());
            assert_eq!(rva_of(line), site.rva, "{field}: rva");
            assert_eq!(bytes_of(line, "prologue"), site.prologue, "{field}: prologue");
        }
        let (line, pool) = (report_line("script_pool"), b.script_pool.as_ref().unwrap());
        assert_eq!(rva_of(line), pool.rva, "script_pool: rva");
        assert_eq!(bytes_of(line, "expected"), pool.expected, "script_pool: expected");
        assert_eq!(bytes_of(line, "replacement"), pool.replacement, "script_pool: replacement");

        let sites: Vec<u32> = numbers_after(report_line("ui_add_sites"), "ui_add_sites")
            .into_iter()
            .map(|n| u32::try_from(n).expect("rva"))
            .collect();
        assert_eq!(sites, b.ui_add_sites, "ui_add_sites");
        assert!(report_line("kind_offset").contains(&format!("Some({:#x})", b.kind_offset.unwrap())));
        assert!(report_line("kinds").contains(r#"("build_proposal", 52)"#));
    }

    /// The port tool's report from 40420 to 40423: the audit trail of the 40423 row.
    const PORT_40423: &str = include_str!("../../tools/addr/out/port-6ac50427-6ad0e4e5.txt");

    /// A row written the way `tools/addr/port_build.py` prints it in its "Rust" section.
    fn port_lines(b: &BuildAddrs) -> Vec<String> {
        let bytes = |v: &[u8]| format!("&[{}]", v.iter().map(|x| format!("0x{x:02X}")).collect::<Vec<_>>().join(", "));
        let hook = |s: &Option<HookSite>| {
            s.as_ref().map_or("None".into(), |s| {
                format!("Some(HookSite {{ rva: {:#x}, prologue: {} }})", s.rva, bytes(s.prologue))
            })
        };
        let call = |s: &Option<CallSite>| {
            s.as_ref().map_or("None".into(), |s| {
                format!("Some(CallSite {{ rva: {:#x}, prologue: {} }})", s.rva, bytes(s.prologue))
            })
        };
        let patch = |s: &Option<PatchSite>| {
            s.as_ref().map_or("None".into(), |s| format!("rva {:#x}, expected {}", s.rva, bytes(s.expected)))
        };
        let cp = |s: &CallPatchSite| {
            format!(
                "CallPatchSite {{ rva: {:#x}, expected: {}, callee: CallSite {{ rva: {:#x}, prologue: {} }} }}",
                s.rva,
                bytes(s.expected),
                s.callee.rva,
                bytes(s.callee.prologue)
            )
        };
        let cps = |s: &Option<CallPatchSite>| s.as_ref().map_or("None".into(), |s| format!("Some({})", cp(s)));
        let owner = b.preview_owner.as_ref().map_or("None".into(), |o| {
            format!(
                "Some({} y_a: {:#x}, y_b: {:#x}, r_c: {:#x}, r_dispatcher: {:#x}, ui_e: {:#x})",
                cp(&o.call),
                o.y_a,
                o.y_b,
                o.r_c,
                o.r_dispatcher,
                o.ui_e
            )
        });
        let list = |v: &[Rva]| format!("&[{}]", v.iter().map(|r| format!("{r:#x}")).collect::<Vec<_>>().join(", "));
        let opt = |r: Option<Rva>| r.map_or("None".into(), |r| format!("Some({r:#x})"));
        let e = &b.engine;
        vec![
            format!("command_list_add: {}", hook(&b.command_list_add)),
            format!("apply_command: {}", hook(&b.apply_command)),
            format!("town_seed: {}", hook(&b.town_seed)),
            format!("town_develop: {}", hook(&b.town_develop)),
            format!("script_convert: {}", hook(&b.script_convert)),
            format!("convert_update: {}", hook(&b.convert_update)),
            format!("preview_event: {}", hook(&b.preview_event)),
            format!("preview_owner: {owner}"),
            format!("town_update_size: {}", hook(&b.town_update_size)),
            format!("town_candidates: {}", hook(&b.town_candidates)),
            format!("town_edges: {}", hook(&b.town_edges)),
            format!("town_sort_call: {}", cps(&b.town_sort_call)),
            format!("town_pick_call: {}", cps(&b.town_pick_call)),
            format!("town_field_call: {}", cps(&b.town_field_call)),
            format!("batch_pace: {}", cps(&b.batch_pace)),
            format!("free_order: {}", cps(&b.free_order)),
            format!("game_time_getter: {}", call(&b.game_time_getter)),
            format!("tick_count_getter: {}", patch(&b.tick_count_getter)),
            format!("script_pool: {}", patch(&b.script_pool)),
            format!("lua_makeseed: {}", patch(&b.lua_makeseed)),
            format!("command_move: {}", call(&b.command_move)),
            format!("command_destroy: {}", call(&b.command_destroy)),
            format!("handle_destroy: {}", call(&b.handle_destroy)),
            format!("ui_add_sites: {}", list(b.ui_add_sites)),
            format!("fire_done_calls: {}", list(b.fire_done_calls)),
            format!(
                "engine: EngineFns {{ proposal_ctor: {}, proposal_dtor: {}, proposal_copy: {}, proposal_assign: {}, engine_new: {}, \
                 engine_delete: {}, refund_insert: {}, add_entity: {}, remove_entity: {}, game_time_advance: {}, component_lookup: {} }}",
                opt(e.proposal_ctor),
                opt(e.proposal_dtor),
                opt(e.proposal_copy),
                opt(e.proposal_assign),
                opt(e.engine_new),
                opt(e.engine_delete),
                opt(e.refund_insert),
                opt(e.add_entity),
                opt(e.remove_entity),
                opt(e.game_time_advance),
                opt(e.component_lookup)
            ),
            format!("layout: {}", b.layout.map_or("None".into(), |l| format!("Some(\"{l}\")"))),
        ]
    }

    /// A row carried over from 40420 equals its port report's "Rust" section, and its patches keep their shape.
    fn assert_row_equals_port_report(timestamp: u32, name: &str, port: &str) {
        let b = select(timestamp).unwrap_or_else(|| panic!("{name} row"));
        assert_eq!(b.name, name);
        let report: Vec<&str> = port.lines().map(str::trim).collect();
        assert!(report.contains(&"all sites located"), "the port report located every site");
        for line in port_lines(b) {
            assert!(report.contains(&line.as_str()), "the port report has no line `{line}`");
        }
        // the patches keep their shape: one byte of the tick getter, the pool region's call and branch, the seed body
        let old = select(0x6ac5_0427).unwrap();
        assert_eq!(b.script_pool.as_ref().unwrap().replacement, old.script_pool.as_ref().unwrap().replacement);
        assert_eq!(b.lua_makeseed.as_ref().unwrap().replacement, old.lua_makeseed.as_ref().unwrap().replacement);
        let t = b.tick_count_getter.as_ref().unwrap();
        let rel = i32::from_le_bytes(t.expected[15..19].try_into().unwrap());
        assert_eq!(
            (t.rva + 19).wrapping_add(rel as u32),
            b.engine.component_lookup.unwrap(),
            "the getter calls the lookup"
        );
        // the 40420 row prints the same way as its own values (the renderer itself is checked on a known row)
        assert!(port_lines(old).iter().any(|l| l == "layout: Some(\"40420\")"));
    }

    #[test]
    fn build_40423_row_equals_the_committed_port_report() {
        assert_row_equals_port_report(0x6ad0_e4e5, "40423", PORT_40423);
    }

    /// The port tool's report from 40420 to 40425: the audit trail of the 40425 row.
    const PORT_40425: &str = include_str!("../../tools/addr/out/port-6ac50427-6ad9ce10.txt");

    #[test]
    fn build_40425_row_equals_the_committed_port_report() {
        assert_row_equals_port_report(0x6ad9_ce10, "40425", PORT_40425);
    }

    #[test]
    fn build_40420_hold_inputs_match_the_address_tool_report() {
        // tools/addr/out/6ac50427.txt: helper functions, the kind byte offset, the build proposal kind, 12 tool sites
        let b = select(0x6ac5_0427).expect("40420 row");
        assert_eq!(b.command_move.as_ref().unwrap().rva, 0x9d0430);
        assert_eq!(b.command_destroy.as_ref().unwrap().rva, 0x9d0670);
        assert_eq!(b.handle_destroy.as_ref().unwrap().rva, 0x3040c90);
        assert_eq!(b.kind_offset, Some(0x9b8));
        assert_eq!(b.kind_by_name("build_proposal"), Some(52));
        assert_eq!(b.ui_add_sites.len(), 12);
        assert_eq!(b.ui_site_index(0x4d396f), Some(0));
        assert_eq!(b.ui_site_index(0x28a3871), Some(11));
    }
}
