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

static LUA_SEED_PATCH_40420: [u8; 26] = lua_seed_patch(LUA_SEED);

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildAddrs {
    /// PE link timestamp.
    pub timestamp: u32,
    /// Human name: the game's own build number.
    pub name: &'static str,
    /// `CommandList::Add`: where every engine command enters the queue; construction tools are held here.
    pub command_list_add: Option<HookSite>,
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
    /// `GameTime` getter: `fn(system*) -> u32` (the dword at `+0x40` of the `GameTime` component). Called, never
    /// hooked; the prologue is compared first.
    pub game_time_getter: Option<CallSite>,
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
    /// out of the executable and are safe to call with a command that was never applied (result byte 0): the street tool's and the
    /// bulldozer's. Any other callback (the construction tool's does real work with the result) is never fired.
    pub fire_done_calls: &'static [Rva],
    /// Offset of the command-kind byte (the dispatcher index) inside a command's payload.
    pub kind_offset: Option<u32>,
    /// Command kinds the control link may name: (name, kind byte).
    pub kinds: &'static [(&'static str, u8)],
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

    /// Index in [`ui_add_sites`](Self::ui_add_sites) of a return address, when it is one of the tool sites.
    pub fn ui_site_index(&self, return_rva: Rva) -> Option<usize> {
        self.ui_add_sites.iter().position(|&r| r == return_rva)
    }
}

/// Known builds. Rows are generated by `tools/addr/find_sites.py`; do not edit by hand.
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
        // `sub rsp,28h ; mov rax,[rcx+10h] ; mov rcx,[rcx+8] ; mov edx,[rax]` (then a call to the component lookup)
        game_time_getter: Some(CallSite {
            rva: 0x002a_96c0,
            prologue: &[0x48, 0x83, 0xEC, 0x28, 0x48, 0x8B, 0x41, 0x10, 0x48, 0x8B, 0x49, 0x08, 0x8B, 0x10],
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
            replacement: &[
                0x83, 0xF8, 0x08, 0x66, 0x0F, 0x1F, 0x44, 0x00, 0x00, 0xC7, 0x44, 0x24, 0x20, 0x01, 0x00, 0x00, 0x00,
                0x31, 0xC0, 0x0F, 0x1F, 0x00,
            ],
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
            replacement: &LUA_SEED_PATCH_40420,
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
        fire_done_calls: &[0x004d_8860, 0x0058_c0b0],
        // movsx rcx, byte [payload + 0x9b8] in apply_command and in Command::~Command
        kind_offset: Some(0x9b8),
        // the dispatcher's case 52 is the handler that references the string "WorldBuildProposal"
        kinds: &[("build_proposal", 52)],
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
        town_candidates: None,
        town_edges: None,
        town_sort_call: None,
        town_pick_call: None,
        game_time_getter: None,
        script_pool: None,
        lua_makeseed: None,
        command_move: None,
        command_destroy: None,
        handle_destroy: None,
        ui_add_sites: &[0x1100, 0x1200],
        fire_done_calls: &[],
        kind_offset: Some(0x20),
        kinds: &[("build_proposal", 7), ("other", 9)],
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
            for p in [&b.script_pool, &b.lua_makeseed].into_iter().flatten() {
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
        for (site, call) in [(&b.town_sort_call, 0x967c16u32), (&b.town_pick_call, 0x95d627)] {
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
