//! Taking a command out of the engine's hands and giving it back (`docs/NATIVE_LINK.md` section 4).
//!
//! `CommandList::Add(list, out, cmd, done, progress)` queues a command: it moves `cmd` into the list, takes the
//! `done` callback, consumes `progress` and writes a handle through `out`. To hold a command the hook moves the
//! three arguments into a [`Held`] before `Add` can run, leaves the caller with an empty handle, and later calls
//! `Add` itself with the held arguments, as the tool would have. Cancelling destroys them instead.
//!
//! Every layout fact below was read out of the game's code by the address tool (`tools/addr`, report
//! `out/6ac50427.txt`) or follows from `CommandList::Add`'s own use of the argument, and the approach is
//! MPFever's (MIT, dkdknight, `native/mpfever_native.cpp`: "deferral of native builds"):
//!
//! - `Command` is 0x38 bytes: payload pointer, a vector, a weak pointer (pointer and control block, which `Add`
//!   fills from `progress`), a result byte at 0x30. The engine's move constructor and destructor are called, never
//!   re-implemented.
//! - `done` is an MSVC `std::function` of 0x40 bytes with its implementation pointer at 0x38. A small callable
//!   lives inside the object and the pointer points at the object itself; otherwise the pointer is to the heap.
//!   `Add` itself relies on slot 4 of the implementation's vtable being `_Delete_this(bool dealloc)` with
//!   `dealloc = impl != this`; slot 1 is `_Move(void* where)` of the STL's `_Func_base`.
//! - `progress` is 16 bytes (a weak pointer), `out` is a 16-byte handle that a zeroed value empties.
//!
//! All of it is `unsafe` code that can only be exercised for real in the game; the tests run it against fake
//! engine functions and a fake `std::function` laid out the same way, which checks the sequence and the offsets.

use crate::mem;

pub const COMMAND_SIZE: usize = 0x38;
/// Room for a [`COMMAND_SIZE`] command, rounded up.
const COMMAND_ROOM: usize = 0x40;
const _: () = assert!(COMMAND_ROOM >= COMMAND_SIZE);
pub const FUNCTION_SIZE: usize = 0x40;
/// Where an MSVC `std::function` keeps the pointer to its callable.
pub const FUNCTION_IMPL: usize = 0x38;
pub const PROGRESS_SIZE: usize = 0x10;
pub const HANDLE_SIZE: usize = 0x10;
/// Where the apply function stores the verdict inside a `Command`.
pub const RESULT_OFFSET: usize = 0x30;

// Every function of the game is declared `system-unwind`: it is C++ and may throw. Through a plain `system` pointer an
// exception would be undefined behaviour (the call site is compiled as one that cannot unwind: no cleanup of our
// frame, no lock let go). And none is ever called inside `log::guarded` (see there): each caller announces it with
// `log::calls_engine`.

/// `Command::Command(Command&&)`: (destination, source) -> destination.
pub type CommandMove = unsafe extern "system-unwind" fn(dst: *mut u8, src: *mut u8) -> *mut u8;
/// `Command::~Command()`.
pub type CommandDestroy = unsafe extern "system-unwind" fn(cmd: *mut u8);
/// The destructor of the handle `Add` writes through `out`.
pub type HandleDestroy = unsafe extern "system-unwind" fn(handle: *mut u8);
/// `CommandList::Add`.
pub type AddFn = unsafe extern "system-unwind" fn(
    list: *mut u8,
    out: *mut u8,
    cmd: *mut u8,
    done: *mut u8,
    progress: *mut u8,
) -> *mut u8;
type DeleteThis = unsafe extern "system-unwind" fn(this: *mut u8, dealloc: bool);
type MoveThis = unsafe extern "system-unwind" fn(this: *mut u8, to: *mut u8) -> *mut u8;

/// The engine functions the hold path calls, by address.
#[derive(Clone, Copy)]
pub struct EngineFns {
    pub command_move: CommandMove,
    pub command_destroy: CommandDestroy,
    pub handle_destroy: HandleDestroy,
    /// Offset of the command-kind byte inside a command's payload.
    pub kind_offset: usize,
    /// Absolute addresses of the `_Do_call` thunks of the completion callbacks that may be fired for a command that is dropped
    /// (the build table's `fire_done_calls`); anything else is never called.
    pub fire_calls: &'static [usize],
    /// `FEVERLINK3_STRICT_FIRE_DONE=1`: fire the tool's completion callback before a cancelled command is destroyed.
    pub fire_done: bool,
}

impl EngineFns {
    /// The functions at `base + rva`.
    ///
    /// # Safety
    /// The three addresses must be the functions the build table says they are (the plan verified their prologues).
    pub unsafe fn at(
        base: usize,
        command_move: u32,
        command_destroy: u32,
        handle_destroy: u32,
        kind_offset: u32,
    ) -> EngineFns {
        // SAFETY: the caller vouches for the addresses; the signatures are those of the engine functions.
        unsafe {
            EngineFns {
                command_move: core::mem::transmute::<usize, CommandMove>(base + command_move as usize),
                command_destroy: core::mem::transmute::<usize, CommandDestroy>(base + command_destroy as usize),
                handle_destroy: core::mem::transmute::<usize, HandleDestroy>(base + handle_destroy as usize),
                kind_offset: kind_offset as usize,
                fire_calls: &[],
                fire_done: false,
            }
        }
    }
}

/// `_Do_call` of the callable inside an MSVC `std::function` of [`FUNCTION_SIZE`] bytes: slot 2 of the implementation's vtable
/// (`_Copy`, `_Move`, `_Do_call`, `_Target_type`, `_Delete_this`). `None` for an empty function or memory that is not readable.
///
/// # Safety
/// `f` points to a live `std::function` object (or any memory: every read is checked).
pub unsafe fn done_call_target(f: *const u8) -> Option<usize> {
    unsafe {
        let slot = f.wrapping_add(FUNCTION_IMPL);
        if !mem::readable(slot, core::mem::size_of::<usize>()) {
            return None;
        }
        let callable = *(slot as *const *const u8);
        if callable.is_null() || !mem::readable(callable, core::mem::size_of::<usize>()) {
            return None;
        }
        let vtable = *(callable as *const *const usize);
        let entry = vtable.wrapping_add(2);
        if vtable.is_null() || !mem::readable(entry as *const u8, core::mem::size_of::<usize>()) {
            return None;
        }
        let target = *entry;
        (target != 0).then_some(target)
    }
}

/// The arguments of a call to `CommandList::Add`.
#[derive(Clone, Copy, Debug)]
pub struct AddArgs {
    pub list: *mut u8,
    pub out: *mut u8,
    pub cmd: *mut u8,
    pub done: *mut u8,
    pub progress: *mut u8,
}

impl AddArgs {
    fn complete(&self) -> bool {
        !(self.list.is_null()
            || self.out.is_null()
            || self.cmd.is_null()
            || self.done.is_null()
            || self.progress.is_null())
    }
}

#[repr(C, align(16))]
struct Room<const N: usize>([u8; N]);

/// A captured call: the command, its callbacks and the list it was meant for.
#[repr(C, align(16))]
pub struct Held {
    list: *mut u8,
    /// The command's payload pointer: the identity the simulation's hook recognises it by.
    payload: usize,
    cmd: Room<COMMAND_ROOM>,
    done: Room<FUNCTION_SIZE>,
    progress: Room<PROGRESS_SIZE>,
    /// Cancelling this command fires the tool's completion callback first: the switch is on and the callback's `_Do_call` is one the
    /// build table lists. Decided when the command is captured.
    done_ok: bool,
}

// SAFETY: a Held is plain bytes and engine pointers that are only used on the thread that makes the calls
// (the interface thread, in the Add hook); it travels through the state machine as an opaque value.
unsafe impl Send for Held {}

impl Held {
    pub fn payload(&self) -> usize {
        self.payload
    }

    /// Whether dropping this command fires the tool's completion callback first.
    pub fn done_ok(&self) -> bool {
        self.done_ok
    }
}

/// Reads the kind byte of the command at `cmd` (a `Command*`): payload pointer, then the byte at `kind_offset`.
/// `None` when any of it is not readable memory.
///
/// # Safety
/// Only reads after checking readability; `cmd` may be any value.
pub unsafe fn command_kind(cmd: *const u8, kind_offset: usize) -> Option<u8> {
    let payload = unsafe { payload_of(cmd) }?;
    let at = payload.checked_add(kind_offset)?;
    // SAFETY: readable was checked for exactly this byte.
    unsafe { mem::readable(at as *const u8, 1).then(|| *(at as *const u8)) }
}

/// The payload pointer of the command at `cmd`, when `cmd` is readable and it is not null.
///
/// # Safety
/// `cmd` may be any value.
pub unsafe fn payload_of(cmd: *const u8) -> Option<usize> {
    // SAFETY: readable was checked for the pointer-sized field.
    let payload = unsafe { mem::readable(cmd, core::mem::size_of::<usize>()).then(|| *(cmd as *const usize)) }?;
    (payload != 0).then_some(payload)
}

/// The flags a build command carries: the members of its Context and the two flags after it, as the engine's own Lua binding
/// names them (`docs/research/SPIKE_NATIVE_COMMAND.md` 3.1; the offsets are the layout table's in `portable.rs`). They tell the mod
/// how the tool built the command, so every game rebuilds it with the same flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildFlags {
    pub gather_buildings: bool,
    pub gather_fields: bool,
    pub check_terrain_alignment: bool,
    pub cleanup_street_graph: bool,
    pub extend_proposal_redo_pillars: bool,
    pub ignore_errors: bool,
    pub player_initiated: bool,
    /// The entries of the Context's `refundableEntities` (the tool's list of what the player built recently: the engine refunds a
    /// removal of one of them; friend test 2026-10-16, a bulldozing replayed without them gave no money back). `None` when the
    /// container could not be read whole; the mod then replays without a refund and says so.
    pub refundable: Option<Vec<RefundEntry>>,
}

/// One entry of `Context.refundableEntities`, typed in the game's API reference (script-doc `api/type.html`, 19-08-2026) as
/// `{{Engine.Entity, integer}, {Engine.Revision, integer}}`: a key of an entity and an integer, a value of the entity's revision
/// (`Engine.Revision.num: {integer, integer, integer}`) and an integer. The 24 bytes of a list node's value match that layout exactly
/// (friend test 2026-10-16 `native_inject_4.txt`: `w:ffffffff0001819f w:0000000b00000001 w:0000015300000000` = entity 0x1819f, key
/// integer -1, revision {1, 11, 0}, value integer 0x153). What the two integers mean is not documented (unproven).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefundEntry {
    pub entity: u32,
    pub key: i32,
    pub revision: [i32; 3],
    pub value: i32,
}

/// Offsets inside a build command's payload.
const FLAG_OFFSETS: [usize; 7] = [0x358, 0x359, 0x360, 0x362, 0x3b2, 0x3d1, 0x3d2];

/// The Context's `refundableEntities` in a build command's payload (`portable.rs`: `Ty::Hash(0x28)` at 0x370).
pub(crate) const REFUNDABLE_OFFSET: usize = 0x370;
/// The list nodes of `refundableEntities` are 0x28 bytes: next, prev, then the 24-byte entry ([`RefundEntry`]; real texts, e.g. friend
/// test 2026-10-16 `native_inject_4.txt`: 25 nodes, consecutive ids of two recent builds).
pub(crate) const REFUNDABLE_NODE: usize = 0x28;
pub(crate) const REFUNDABLE_VALUE: usize = 0x10;
/// More entries than this are not a tool's list (real runs: up to 25): the container is taken as unreadable.
pub const REFUNDABLE_MAX: usize = 4096;

/// Reads the flags out of a build command's payload; `None` when any of it is not readable memory.
///
/// # Safety
/// `payload` may be any value; only readable bytes are read.
pub unsafe fn build_flags(payload: usize) -> Option<BuildFlags> {
    let mut v = [false; 7];
    for (flag, off) in v.iter_mut().zip(FLAG_OFFSETS) {
        let at = payload.checked_add(off)?;
        // SAFETY: readable was checked for exactly this byte.
        *flag = unsafe { mem::readable(at as *const u8, 1).then(|| *(at as *const u8)) }? != 0;
    }
    Some(BuildFlags {
        gather_buildings: v[0],
        gather_fields: v[1],
        check_terrain_alignment: v[2],
        cleanup_street_graph: v[3],
        extend_proposal_redo_pillars: v[4],
        ignore_errors: v[5],
        player_initiated: v[6],
        // SAFETY: only readable memory is read.
        refundable: unsafe { refundable_entities(payload) },
    })
}

/// The entries of the Context's `refundableEntities` of a build command's payload, in the container's list order. MSVC's
/// `std::unordered_map`: the list head pointer at +8 and the size at +0x10 (the load factor at +0, the buckets after); the list is
/// circular through its sentinel. `None` unless exactly `size` readable nodes lead back to the sentinel.
///
/// # Safety
/// `payload` may be any value; only readable memory is read.
pub unsafe fn refundable_entities(payload: usize) -> Option<Vec<RefundEntry>> {
    let word = |at: usize| -> Option<usize> {
        // SAFETY: readable was checked for the pointer-sized word.
        unsafe { mem::readable(at as *const u8, 8).then(|| *(at as *const usize)) }
    };
    let c = payload.checked_add(REFUNDABLE_OFFSET)?;
    let head = word(c.checked_add(8)?)?;
    let size = word(c.checked_add(0x10)?)?;
    if size > REFUNDABLE_MAX || head == 0 {
        return None;
    }
    let mut ids = Vec::with_capacity(size);
    let mut node = word(head)?;
    for _ in 0..size {
        if node == head || node == 0 {
            return None;
        }
        let value = node.checked_add(REFUNDABLE_VALUE)?;
        // SAFETY: readable was checked for the node's whole block; the entry is six 4-byte words at +0x10.
        let w = unsafe {
            mem::readable(node as *const u8, REFUNDABLE_NODE).then(|| (value as *const [u32; 6]).read_unaligned())
        }?;
        ids.push(RefundEntry {
            entity: w[0],
            key: w[1] as i32,
            revision: [w[2] as i32, w[3] as i32, w[4] as i32],
            value: w[5] as i32,
        });
        node = word(node)?;
    }
    (node == head).then_some(ids)
}

/// The verdict byte the apply function wrote into the command at `cmd`: true when it ran. `None` when unreadable.
///
/// # Safety
/// `cmd` may be any value.
pub unsafe fn command_result(cmd: *const u8) -> Option<bool> {
    let at = cmd.wrapping_add(RESULT_OFFSET);
    // SAFETY: readable was checked for this byte.
    unsafe { mem::readable(at, 1).then(|| *at != 0) }
}

/// Moves an `std::function` of 0x40 bytes: `dst` is overwritten, `src` is left empty. A callable stored inside
/// the object is asked to move itself (and the original is destroyed in place); one on the heap is taken over.
///
/// # Safety
/// `src` is a live MSVC `std::function` of [`FUNCTION_SIZE`] bytes, `dst` has room for one and does not overlap.
unsafe fn move_function(dst: *mut u8, src: *mut u8) {
    unsafe {
        let callable = *(src.add(FUNCTION_IMPL) as *const *mut u8);
        core::ptr::write_bytes(dst, 0, FUNCTION_SIZE);
        if callable.is_null() {
            return;
        }
        if callable == src {
            let vtable = *(callable as *const *const usize);
            let move_this: MoveThis = core::mem::transmute::<usize, MoveThis>(*vtable.add(1));
            let moved = move_this(callable, dst);
            *(dst.add(FUNCTION_IMPL) as *mut *mut u8) = moved;
            let delete_this: DeleteThis = core::mem::transmute::<usize, DeleteThis>(*vtable.add(4));
            delete_this(callable, false);
        } else {
            *(dst.add(FUNCTION_IMPL) as *mut *mut u8) = callable;
        }
        *(src.add(FUNCTION_IMPL) as *mut *mut u8) = core::ptr::null_mut();
    }
}

/// Destroys an `std::function` that will never be called, and leaves it empty.
///
/// # Safety
/// `f` is a live MSVC `std::function` of [`FUNCTION_SIZE`] bytes.
unsafe fn destroy_function(f: *mut u8) {
    unsafe {
        let callable = *(f.add(FUNCTION_IMPL) as *const *mut u8);
        if callable.is_null() {
            return;
        }
        let vtable = *(callable as *const *const usize);
        let delete_this: DeleteThis = core::mem::transmute::<usize, DeleteThis>(*vtable.add(4));
        delete_this(callable, callable != f);
        *(f.add(FUNCTION_IMPL) as *mut *mut u8) = core::ptr::null_mut();
    }
}

/// Takes the arguments of an `Add` call: the command, `done` and `progress` move into the result, `out` is left
/// as an empty handle (so the caller destroys it as it would any handle). `None`, with nothing touched, when an
/// argument is null or the command has no payload.
///
/// From the moment this returns `Some`, the caller's `Add` arguments are empty: the result must reach the hold ring
/// (or go back to the engine through [`release`]) without anything that can fail in between. Whatever the hold
/// needs from the command is read before the capture (`Runtime::plan_add`).
///
/// # Safety
/// `args` are the arguments of a live call to `CommandList::Add` on the calling thread.
pub unsafe extern "system-unwind" fn capture(fns: &EngineFns, args: &AddArgs) -> Option<Box<Held>> {
    if !args.complete() {
        return None;
    }
    let payload = unsafe { payload_of(args.cmd) }?;
    crate::log::calls_engine("capture (the command's move constructor)");
    // SAFETY: Held is plain bytes and pointers, for which all-zero is a valid value.
    let mut held: Box<Held> = Box::new(unsafe { core::mem::zeroed() });
    held.list = args.list;
    held.payload = payload;
    unsafe {
        (fns.command_move)(held.cmd.0.as_mut_ptr(), args.cmd);
        move_function(held.done.0.as_mut_ptr(), args.done);
        core::ptr::copy_nonoverlapping(args.progress, held.progress.0.as_mut_ptr(), PROGRESS_SIZE);
        core::ptr::write_bytes(args.progress, 0, PROGRESS_SIZE);
        core::ptr::write_bytes(args.out, 0, HANDLE_SIZE);
        // the callback was moved with the command: look at what it is now (the target of the moved callable is the same function)
        held.done_ok = fns.fire_done
            && done_call_target(held.done.0.as_ptr()).is_some_and(|target| fns.fire_calls.contains(&target));
    }
    Some(held)
}

/// Hands a held command to the engine: `Add` with the held arguments, then the destructors of the handle it
/// returned and of the moved-from command, exactly what a tool does after its own call.
///
/// `system-unwind`: `Add` is a C++ function and can throw, and the exception must pass this frame as any other (it is
/// never called inside `log::guarded`).
///
/// # Safety
/// `add` is the unhooked `CommandList::Add`; the list the command was taken for must still exist.
#[allow(clippy::boxed_local)] // taking the Box is the point: a held command can be handed back only once
pub unsafe extern "system-unwind" fn release(fns: &EngineFns, mut held: Box<Held>, add: AddFn) {
    crate::log::calls_engine("release (Add)");
    let mut out = Room::<HANDLE_SIZE>([0; HANDLE_SIZE]);
    unsafe {
        add(
            held.list,
            out.0.as_mut_ptr(),
            held.cmd.0.as_mut_ptr(),
            held.done.0.as_mut_ptr(),
            held.progress.0.as_mut_ptr(),
        );
        (fns.handle_destroy)(out.0.as_mut_ptr());
        (fns.command_destroy)(held.cmd.0.as_mut_ptr());
    }
}

/// The `_Do_call` of a tool's completion callback: `(callable, &Command)`. `system-unwind`: tool code can throw, and the exception
/// must pass through our frame like through any other (see [`release`]).
type DoCall = unsafe extern "system-unwind" fn(this: *mut u8, cmd: *mut u8) -> usize;

/// Fires the tool's own completion callback for a command that was never applied, as TpF2 Multiplayer does at a cancel (idea only, MIT):
/// the tool's callback clears its "waiting for the engine" flag and re-arms its preview, which a destroyed callback never does. The
/// command's result byte is zero (it never ran), so the callback takes its "not applied" path. Called only when [`capture`] found the
/// callable's `_Do_call` in the build table's list; true when it was called.
///
/// # Safety
/// As for [`discard`].
unsafe extern "system-unwind" fn fire_done(fns: &EngineFns, held: &mut Held) -> bool {
    if !held.done_ok {
        return false;
    }
    unsafe {
        let Some(target) = done_call_target(held.done.0.as_ptr()).filter(|t| fns.fire_calls.contains(t)) else {
            return false; // changed since the capture: not what was read out of the executable
        };
        let callable = *(held.done.0.as_ptr().add(FUNCTION_IMPL) as *const *mut u8);
        *held.cmd.0.as_mut_ptr().add(RESULT_OFFSET) = 0;
        let call: DoCall = core::mem::transmute::<usize, DoCall>(target);
        call(callable, held.cmd.0.as_mut_ptr());
    }
    true
}

/// Destroys a held command that is not to be queued: first, when [`Held::done_ok`], the tool's completion callback is fired (returns
/// true), then the command by the engine's destructor and the callback by its own. The weak pointer in `progress` is left alone (a
/// leaked weak count is harmless, a wrong release is not).
///
/// # Safety
/// As for [`release`], without `add`.
#[allow(clippy::boxed_local)] // taking the Box is the point: a held command can be destroyed only once
pub unsafe extern "system-unwind" fn discard(fns: &EngineFns, mut held: Box<Held>) -> bool {
    crate::log::calls_engine("discard (callback, destructors)");
    unsafe {
        let fired = fire_done(fns, &mut held);
        (fns.command_destroy)(held.cmd.0.as_mut_ptr());
        destroy_function(held.done.0.as_mut_ptr());
        fired
    }
}

/// Destroys a held command that leaves without a stamp apply of its own (its replay was refused or failed, it got too old, holding went
/// off): the tool's completion callback is fired first with the command's own result, which says "not applied" (the command never ran,
/// its result byte is cleared), as the engine fires it for a command it could not apply. Any non-empty callback is fired, as in
/// [`complete`]: the tool must never stay busy waiting for a command that will not come back. True when it was fired.
///
/// # Safety
/// As for [`release`], without `add`.
#[allow(clippy::boxed_local)] // taking the Box is the point: a held command can be destroyed only once
pub unsafe extern "system-unwind" fn fail(fns: &EngineFns, mut held: Box<Held>) -> bool {
    unsafe {
        *held.cmd.0.as_mut_ptr().add(RESULT_OFFSET) = 0;
        complete(fns, held)
    }
}

/// Where a `Command` keeps the vector the tool's callback reads besides the payload (the construction tool's configure mode, lambda
/// 0x5123e0 of build 40420, reads `cmd+8` and `payload+0x998`).
pub const COMMAND_VEC: usize = 0x08;
const VEC_SIZE: usize = 0x18;

/// What [`adopt_result`] moved, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adopted {
    pub ok: bool,
    /// Bytes in the applied command's vector at [`COMMAND_VEC`] (end - begin) that went to the held command.
    pub vec_bytes: u64,
}

/// Gives the held command the result of a command the simulation has just applied (this game's own stamp apply of the same build):
/// their payload pointers are exchanged (each `Command` owns its payload; the engine's destructor of each frees what it then holds),
/// as are the vectors at [`COMMAND_VEC`], and the verdict byte is copied. The applied command keeps the held one's unapplied payload,
/// which nobody reads after the apply. Called on the simulation thread right after the apply returned, with the machine's lock held.
///
/// # Safety
/// `applied` is the live `Command` the apply function was just called with; `held` is not in use on another thread.
pub unsafe fn adopt_result(held: &mut Held, applied: *mut u8) -> Adopted {
    unsafe {
        let mine = held.cmd.0.as_mut_ptr();
        core::ptr::swap_nonoverlapping(mine as *mut usize, applied as *mut usize, 1);
        core::ptr::swap_nonoverlapping(mine.add(COMMAND_VEC), applied.add(COMMAND_VEC), VEC_SIZE);
        let ok = *applied.add(RESULT_OFFSET);
        *mine.add(RESULT_OFFSET) = ok;
        held.payload = *(mine as *const usize);
        let begin = *(mine.add(COMMAND_VEC) as *const u64);
        let end = *(mine.add(COMMAND_VEC + 8) as *const u64);
        Adopted { ok: ok != 0, vec_bytes: end.saturating_sub(begin) }
    }
}

/// Destroys a held command that took a real result ([`adopt_result`]): the tool's completion callback is fired with it first, as the
/// engine fires it after its own apply, so every tool (the construction tool's configure mode too) takes its normal path. Any non-empty
/// callback is fired: the build table's list only guards the empty-result firing of [`discard`]. True when it was fired.
///
/// # Safety
/// As for [`release`], without `add`.
#[allow(clippy::boxed_local)] // taking the Box is the point: a held command can be destroyed only once
pub unsafe extern "system-unwind" fn complete(fns: &EngineFns, mut held: Box<Held>) -> bool {
    crate::log::calls_engine("complete (callback, destructors)");
    unsafe {
        let fired = match done_call_target(held.done.0.as_ptr()) {
            Some(target) => {
                let callable = *(held.done.0.as_ptr().add(FUNCTION_IMPL) as *const *mut u8);
                let call: DoCall = core::mem::transmute::<usize, DoCall>(target);
                call(callable, held.cmd.0.as_mut_ptr());
                true
            }
            None => false,
        };
        (fns.command_destroy)(held.cmd.0.as_mut_ptr());
        destroy_function(held.done.0.as_mut_ptr());
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    // ---- a fake engine, laid out like the real one

    thread_local! {
        static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    fn log(s: impl Into<String>) {
        LOG.with(|l| l.borrow_mut().push(s.into()));
    }

    fn take_log() -> Vec<String> {
        LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
    }

    unsafe extern "system-unwind" fn fake_move(dst: *mut u8, src: *mut u8) -> *mut u8 {
        // the real one copies the fields and empties the source (the result byte is copied, not cleared)
        unsafe {
            core::ptr::copy_nonoverlapping(src, dst, COMMAND_SIZE);
            core::ptr::write_bytes(src, 0, RESULT_OFFSET);
        }
        log("move");
        dst
    }

    unsafe extern "system-unwind" fn fake_destroy(cmd: *mut u8) {
        let payload = unsafe { *(cmd as *const usize) };
        log(format!("destroy command payload={}", payload != 0));
        unsafe { core::ptr::write_bytes(cmd, 0, COMMAND_SIZE) };
    }

    unsafe extern "system-unwind" fn fake_handle_destroy(h: *mut u8) {
        log(format!("destroy handle empty={}", unsafe { *(h as *const u64) } == 0));
    }

    unsafe extern "system-unwind" fn fake_add(
        list: *mut u8,
        out: *mut u8,
        cmd: *mut u8,
        done: *mut u8,
        progress: *mut u8,
    ) -> *mut u8 {
        unsafe {
            log(format!(
                "add list={:#x} payload={:#x} done_impl={} progress={:#x}",
                list as usize,
                *(cmd as *const usize),
                *(done.add(FUNCTION_IMPL) as *const usize) != 0,
                *(progress as *const u64)
            ));
        }
        out
    }

    // a fake MSVC std::function: the callable has a vtable with the five slots of `_Func_base`
    unsafe extern "system-unwind" fn vt_copy(_this: *mut u8, _to: *mut u8) -> *mut u8 {
        core::ptr::null_mut()
    }
    unsafe extern "system-unwind" fn vt_move(this: *mut u8, to: *mut u8) -> *mut u8 {
        log("callable moved");
        unsafe { core::ptr::copy_nonoverlapping(this, to, 0x38) };
        to
    }
    unsafe extern "system-unwind" fn vt_call(this: *mut u8, cmd: *mut u8) -> usize {
        // the tool's completion callback: logs the command it was given and its result byte
        log(format!("callback called payload={} result={}", unsafe { *(cmd as *const usize) } != 0, unsafe {
            *cmd.add(RESULT_OFFSET)
        }));
        let _ = this;
        0
    }
    unsafe extern "system-unwind" fn vt_type(_this: *mut u8) {}
    unsafe extern "system-unwind" fn vt_delete(this: *mut u8, dealloc: bool) {
        log(format!("callable deleted dealloc={dealloc}"));
        if dealloc {
            // the heap callable made by `heap_function` is a leaked Box<[u8; 16]>
            drop(unsafe { Box::from_raw(this as *mut [u8; 16]) });
        }
    }

    fn vtable() -> *const usize {
        // built at run time because function-pointer-to-integer casts are not const
        thread_local! {
            static TABLE: [usize; 5] = [
                vt_copy as *const () as usize,
                vt_move as *const () as usize,
                vt_call as *const () as usize,
                vt_type as *const () as usize,
                vt_delete as *const () as usize,
            ];
        }
        TABLE.with(|t| t.as_ptr())
    }

    /// 0x40 bytes of function object. Inline: the vtable pointer at offset 0 and the pointer at 0x38 to itself.
    /// Heap: the pointer at 0x38 to a separate 16-byte callable whose first word is the vtable pointer.
    fn inline_function() -> Box<Room<FUNCTION_SIZE>> {
        let mut f = Box::new(Room::<FUNCTION_SIZE>([0; FUNCTION_SIZE]));
        let base = f.0.as_mut_ptr();
        unsafe {
            *(base as *mut usize) = vtable() as usize;
            *(base.add(8) as *mut u64) = 0xFEED;
            *(base.add(FUNCTION_IMPL) as *mut *mut u8) = base;
        }
        f
    }

    fn heap_function() -> (Box<Room<FUNCTION_SIZE>>, *mut u8) {
        let mut f = Box::new(Room::<FUNCTION_SIZE>([0; FUNCTION_SIZE]));
        let callable = Box::into_raw(Box::new([0u8; 16])) as *mut u8;
        unsafe {
            *(callable as *mut usize) = vtable() as usize;
            *(f.0.as_mut_ptr().add(FUNCTION_IMPL) as *mut *mut u8) = callable;
        }
        (f, callable)
    }

    fn fns() -> EngineFns {
        fns_firing(&[], false)
    }

    fn fns_firing(calls: &'static [usize], on: bool) -> EngineFns {
        EngineFns {
            command_move: fake_move,
            command_destroy: fake_destroy,
            handle_destroy: fake_handle_destroy,
            kind_offset: 0x9b8,
            fire_calls: calls,
            fire_done: on,
        }
    }

    fn call_address() -> &'static [usize] {
        Box::leak(vec![vt_call as *const () as usize].into_boxed_slice())
    }

    /// A command: 0x38 bytes pointing at a payload of 0x9c0 bytes whose kind byte is `kind`.
    struct Cmd {
        command: Box<Room<COMMAND_ROOM>>,
        payload: Box<[u8; 0x9c0]>,
    }

    fn command(kind: u8) -> Cmd {
        let mut payload = Box::new([0u8; 0x9c0]);
        payload[0x9b8] = kind;
        let mut command = Box::new(Room::<COMMAND_ROOM>([0; COMMAND_ROOM]));
        unsafe { *(command.0.as_mut_ptr() as *mut usize) = payload.as_ptr() as usize };
        command.0[0x20] = 0x77; // a field the move must carry along
        Cmd { command, payload }
    }

    #[test]
    fn the_flags_of_a_build_command_are_read_at_the_layout_offsets() {
        let mut c = command(52);
        for off in [0x359usize, 0x362, 0x3d2] {
            c.payload[off] = 1;
        }
        let f = unsafe { build_flags(c.payload.as_ptr() as usize) }.expect("readable");
        assert_eq!(
            f,
            BuildFlags {
                gather_buildings: false,
                gather_fields: true,
                check_terrain_alignment: false,
                cleanup_street_graph: true,
                extend_proposal_redo_pillars: false,
                ignore_errors: false,
                player_initiated: true,
                refundable: None,
            }
        );
        assert!(unsafe { build_flags(0) }.is_none(), "unreadable memory gives nothing");
    }

    /// Writes an MSVC hash container's list into `payload` at the `refundableEntities` offset: a sentinel and one node per id.
    /// The nodes are returned and must outlive the reads (the vector is not changed after their addresses are taken).
    fn refund_list(payload: &mut [u8], ids: &[u32]) -> Vec<[u8; REFUNDABLE_NODE]> {
        let mut nodes = Vec::with_capacity(ids.len() + 1);
        nodes.push([0u8; REFUNDABLE_NODE]); // the sentinel
        for &id in ids {
            // the words of a real entry (friend test 2026-10-16): entity, -1, revision {1, 11, 0}, 0x153
            let mut n = [0u8; REFUNDABLE_NODE];
            for (i, w) in [id, u32::MAX, 1, 11, 0, 0x153].iter().enumerate() {
                let at = REFUNDABLE_VALUE + 4 * i;
                n[at..at + 4].copy_from_slice(&w.to_le_bytes());
            }
            nodes.push(n);
        }
        let addr: Vec<usize> = nodes.iter().map(|n| n.as_ptr() as usize).collect();
        for (i, n) in nodes.iter_mut().enumerate() {
            let next = addr[(i + 1) % addr.len()];
            let prev = addr[(i + addr.len() - 1) % addr.len()];
            n[0..8].copy_from_slice(&next.to_le_bytes());
            n[8..16].copy_from_slice(&prev.to_le_bytes());
        }
        let c = REFUNDABLE_OFFSET;
        payload[c..c + 4].copy_from_slice(&1.0f32.to_le_bytes());
        payload[c + 8..c + 16].copy_from_slice(&addr[0].to_le_bytes());
        payload[c + 0x10..c + 0x18].copy_from_slice(&(ids.len() as u64).to_le_bytes());
        nodes
    }

    #[test]
    fn the_refundable_entities_of_a_build_command_are_read_from_its_context() {
        let mut c = command(52);
        let _nodes = refund_list(&mut c.payload[..], &[0x1819f, 0x181cd, 7]);
        let f = unsafe { build_flags(c.payload.as_ptr() as usize) }.expect("readable");
        let entry = |entity| RefundEntry { entity, key: -1, revision: [1, 11, 0], value: 0x153 };
        assert_eq!(
            f.refundable,
            Some(vec![entry(0x1819f), entry(0x181cd), entry(7)]),
            "every entry with its key integer, revision and value integer, in the list's order"
        );

        let mut empty = command(52);
        let _none = refund_list(&mut empty.payload[..], &[]);
        assert_eq!(
            unsafe { refundable_entities(empty.payload.as_ptr() as usize) },
            Some(vec![]),
            "an empty list is read as empty"
        );
    }

    #[test]
    fn a_refundable_list_that_does_not_close_on_its_sentinel_is_not_read() {
        let mut c = command(52);
        let _nodes = refund_list(&mut c.payload[..], &[1, 2, 3]);
        // the size says 4: the walk meets the sentinel early
        let at = REFUNDABLE_OFFSET + 0x10;
        c.payload[at..at + 8].copy_from_slice(&4u64.to_le_bytes());
        assert_eq!(unsafe { refundable_entities(c.payload.as_ptr() as usize) }, None);
        // the size says 2: the walk does not end at the sentinel
        c.payload[at..at + 8].copy_from_slice(&2u64.to_le_bytes());
        assert_eq!(unsafe { refundable_entities(c.payload.as_ptr() as usize) }, None);
        // an absurd size, and a null head
        c.payload[at..at + 8].copy_from_slice(&((REFUNDABLE_MAX + 1) as u64).to_le_bytes());
        assert_eq!(unsafe { refundable_entities(c.payload.as_ptr() as usize) }, None);
        let zero = command(52);
        assert_eq!(
            unsafe { refundable_entities(zero.payload.as_ptr() as usize) },
            None,
            "a container that was never made"
        );
        assert_eq!(
            unsafe { build_flags(zero.payload.as_ptr() as usize) }.map(|f| f.refundable),
            Some(None),
            "the flags are still read"
        );
    }

    struct Call {
        list: Box<[u8; 8]>,
        out: Box<Room<HANDLE_SIZE>>,
        progress: Box<Room<PROGRESS_SIZE>>,
    }

    fn call() -> Call {
        let mut progress = Box::new(Room::<PROGRESS_SIZE>([0; PROGRESS_SIZE]));
        progress.0[0] = 0xAB;
        progress.0[8] = 0xCD;
        Call { list: Box::new([1; 8]), out: Box::new(Room([0xFF; HANDLE_SIZE])), progress }
    }

    fn args(c: &mut Call, cmd: &mut Cmd, done: *mut u8) -> AddArgs {
        AddArgs {
            list: c.list.as_mut_ptr(),
            out: c.out.0.as_mut_ptr(),
            cmd: cmd.command.0.as_mut_ptr(),
            done,
            progress: c.progress.0.as_mut_ptr(),
        }
    }

    // ---- tests

    #[test]
    fn capture_moves_the_command_the_callback_and_the_progress_and_empties_the_handle() {
        take_log();
        let (mut c, mut cmd, mut done) = (call(), command(52), inline_function());
        let payload = cmd.payload.as_ptr() as usize;
        let a = args(&mut c, &mut cmd, done.0.as_mut_ptr());
        let held = unsafe { capture(&fns(), &a) }.expect("captured");

        assert_eq!(held.payload(), payload);
        assert_eq!(held.list, c.list.as_mut_ptr());
        assert_eq!(unsafe { *(held.cmd.0.as_ptr() as *const usize) }, payload, "the command is in the held struct");
        assert_eq!(held.cmd.0[0x20], 0x77, "and carries its fields");
        assert_eq!(unsafe { *(cmd.command.0.as_ptr() as *const usize) }, 0, "the caller's command is empty");
        assert_eq!(c.out.0, [0; HANDLE_SIZE], "the handle the caller gets is empty");
        assert_eq!(c.progress.0, [0; PROGRESS_SIZE], "progress is consumed");
        assert_eq!((held.progress.0[0], held.progress.0[8]), (0xAB, 0xCD), "and was kept");
        // the inline callable was asked to move itself into the held object and the original was destroyed in place
        let held_done = held.done.0.as_ptr();
        assert_eq!(
            unsafe { *(held_done.add(FUNCTION_IMPL) as *const usize) },
            held_done as usize,
            "now points at itself"
        );
        assert_eq!(unsafe { *(held_done.add(8) as *const u64) }, 0xFEED, "with its data");
        assert_eq!(
            unsafe { *(done.0.as_ptr().add(FUNCTION_IMPL) as *const usize) },
            0,
            "the original function is empty"
        );
        assert_eq!(take_log(), ["move", "callable moved", "callable deleted dealloc=false"]);
    }

    #[test]
    fn a_callback_on_the_heap_is_taken_over_not_copied() {
        take_log();
        let (mut c, mut cmd) = (call(), command(52));
        let (mut done, callable) = heap_function();
        let a = args(&mut c, &mut cmd, done.0.as_mut_ptr());
        let held = unsafe { capture(&fns(), &a) }.expect("captured");
        assert_eq!(unsafe { *(held.done.0.as_ptr().add(FUNCTION_IMPL) as *const *mut u8) }, callable);
        assert_eq!(unsafe { *(done.0.as_ptr().add(FUNCTION_IMPL) as *const usize) }, 0);
        assert_eq!(take_log(), ["move"], "no callable function was called");
        unsafe { discard(&fns(), held) }; // frees the heap callable
        take_log();
    }

    #[test]
    fn an_empty_callback_stays_empty() {
        take_log();
        let (mut c, mut cmd) = (call(), command(52));
        let mut done = Room::<FUNCTION_SIZE>([0; FUNCTION_SIZE]);
        let a = args(&mut c, &mut cmd, done.0.as_mut_ptr());
        let held = unsafe { capture(&fns(), &a) }.expect("captured");
        assert_eq!(held.done.0, [0; FUNCTION_SIZE]);
        unsafe { discard(&fns(), held) };
        assert_eq!(take_log(), ["move", "destroy command payload=true"]);
    }

    #[test]
    fn release_calls_add_with_the_held_arguments_then_cleans_up_in_the_tools_order() {
        take_log();
        let (mut c, mut cmd, mut done) = (call(), command(52), inline_function());
        let payload = cmd.payload.as_ptr() as usize;
        let a = args(&mut c, &mut cmd, done.0.as_mut_ptr());
        let held = unsafe { capture(&fns(), &a) }.unwrap();
        take_log();
        unsafe { release(&fns(), held, fake_add) };
        let l = take_log();
        assert_eq!(l.len(), 3, "{l:?}");
        assert_eq!(
            l[0],
            format!(
                "add list={:#x} payload={payload:#x} done_impl=true progress={:#x}",
                c.list.as_ptr() as usize,
                0xABu64
            )
        );
        assert_eq!(l[1], "destroy handle empty=true", "the handle Add returned is destroyed");
        assert_eq!(l[2], "destroy command payload=true", "then the moved-from command");
    }

    #[test]
    fn discard_destroys_the_command_and_the_callback_and_never_calls_add() {
        take_log();
        let (mut c, mut cmd, mut done) = (call(), command(52), inline_function());
        let a = args(&mut c, &mut cmd, done.0.as_mut_ptr());
        let held = unsafe { capture(&fns(), &a) }.unwrap();
        take_log();
        unsafe { discard(&fns(), held) };
        assert_eq!(take_log(), ["destroy command payload=true", "callable deleted dealloc=false"]);

        let (mut c, mut cmd) = (call(), command(52));
        let (mut done, _) = heap_function();
        let a = args(&mut c, &mut cmd, done.0.as_mut_ptr());
        let held = unsafe { capture(&fns(), &a) }.unwrap();
        take_log();
        unsafe { discard(&fns(), held) };
        assert_eq!(
            take_log(),
            ["destroy command payload=true", "callable deleted dealloc=true"],
            "a heap callable is freed"
        );
    }

    #[test]
    fn a_call_with_a_null_argument_or_an_empty_command_is_not_captured_and_not_touched() {
        take_log();
        let (mut c, mut cmd, mut done) = (call(), command(52), inline_function());
        let good = args(&mut c, &mut cmd, done.0.as_mut_ptr());
        for i in 0..5 {
            let mut a = good;
            let null = core::ptr::null_mut();
            match i {
                0 => a.list = null,
                1 => a.out = null,
                2 => a.cmd = null,
                3 => a.done = null,
                _ => a.progress = null,
            }
            assert!(unsafe { capture(&fns(), &a) }.is_none(), "argument {i}");
        }
        let mut empty = Room::<COMMAND_ROOM>([0; COMMAND_ROOM]);
        let mut a = good;
        a.cmd = empty.0.as_mut_ptr();
        assert!(unsafe { capture(&fns(), &a) }.is_none(), "a command with no payload");
        assert!(take_log().is_empty(), "no engine function was called");
        assert_eq!(c.progress.0[0], 0xAB, "progress untouched");
        assert_eq!(c.out.0, [0xFF; HANDLE_SIZE], "out untouched");
        assert_ne!(unsafe { *(done.0.as_ptr().add(FUNCTION_IMPL) as *const usize) }, 0, "done untouched");
    }

    #[test]
    fn the_kind_is_read_only_from_readable_memory() {
        let cmd = command(52);
        assert_eq!(unsafe { command_kind(cmd.command.0.as_ptr(), 0x9b8) }, Some(52));
        assert_eq!(unsafe { command_kind(cmd.command.0.as_ptr(), 0x9b9) }, Some(0));
        assert_eq!(unsafe { command_kind(core::ptr::null(), 0x9b8) }, None);
        let empty = Room::<COMMAND_ROOM>([0; COMMAND_ROOM]);
        assert_eq!(unsafe { command_kind(empty.0.as_ptr(), 0x9b8) }, None, "no payload");
        let mut wild = Room::<COMMAND_ROOM>([0; COMMAND_ROOM]);
        unsafe { *(wild.0.as_mut_ptr() as *mut usize) = 0x10 };
        assert_eq!(unsafe { command_kind(wild.0.as_ptr(), 0x9b8) }, None, "a payload pointer into nowhere");
        unsafe { *(wild.0.as_mut_ptr() as *mut usize) = usize::MAX - 8 };
        assert_eq!(unsafe { command_kind(wild.0.as_ptr(), 0x9b8) }, None, "address overflow");
        assert_eq!(unsafe { payload_of(cmd.command.0.as_ptr()) }, Some(cmd.payload.as_ptr() as usize));
    }

    #[test]
    fn the_verdict_byte_is_read_from_the_command() {
        let mut cmd = command(52);
        assert_eq!(unsafe { command_result(cmd.command.0.as_ptr()) }, Some(false));
        cmd.command.0[RESULT_OFFSET] = 1;
        assert_eq!(unsafe { command_result(cmd.command.0.as_ptr()) }, Some(true));
        assert_eq!(unsafe { command_result(core::ptr::null()) }, None);
    }

    #[test]
    fn held_buffers_are_aligned_and_big_enough() {
        assert_eq!(core::mem::align_of::<Held>(), 16);
        let held: Box<Held> = Box::new(unsafe { core::mem::zeroed() });
        for p in [held.cmd.0.as_ptr(), held.done.0.as_ptr(), held.progress.0.as_ptr()] {
            assert_eq!(p as usize % 16, 0);
        }
        assert_eq!(RESULT_OFFSET + 1, 0x31);
    }

    #[test]
    fn a_listed_callback_is_fired_once_before_the_command_is_destroyed_with_a_zero_result() {
        take_log();
        let f = fns_firing(call_address(), true);
        let (mut c, mut cmd, mut done) = (call(), command(52), inline_function());
        cmd.command.0[RESULT_OFFSET] = 1; // whatever was there: the callback must see "not applied"
        let a = args(&mut c, &mut cmd, done.0.as_mut_ptr());
        let held = unsafe { capture(&f, &a) }.expect("captured");
        assert!(held.done_ok());
        take_log();
        assert!(unsafe { discard(&f, held) }, "fired");
        assert_eq!(
            take_log(),
            ["callback called payload=true result=0", "destroy command payload=true", "callable deleted dealloc=false"],
            "the callback runs once, first, with the held command"
        );
    }

    #[test]
    fn a_callback_that_is_not_listed_or_a_switch_that_is_off_never_calls_it() {
        for (calls, on) in [(&[][..], true), (call_address(), false), (&[0x1234usize][..], true)] {
            take_log();
            let f = fns_firing(calls, on);
            let (mut c, mut cmd, mut done) = (call(), command(52), inline_function());
            let a = args(&mut c, &mut cmd, done.0.as_mut_ptr());
            let held = unsafe { capture(&f, &a) }.expect("captured");
            assert!(!held.done_ok());
            take_log();
            assert!(!unsafe { discard(&f, held) });
            assert_eq!(take_log(), ["destroy command payload=true", "callable deleted dealloc=false"]);
        }
    }

    #[test]
    fn an_empty_callback_or_one_whose_target_changed_after_the_capture_is_not_called() {
        take_log();
        let f = fns_firing(call_address(), true);
        let (mut c, mut cmd) = (call(), command(52));
        let mut empty = Room::<FUNCTION_SIZE>([0; FUNCTION_SIZE]);
        let a = args(&mut c, &mut cmd, empty.0.as_mut_ptr());
        let held = unsafe { capture(&f, &a) }.expect("captured");
        assert!(!held.done_ok(), "no callable: nothing to fire");
        unsafe { discard(&f, held) };
        take_log();
        // listed at the capture, another function at the drop (the listed table changed): not called
        let (mut c, mut cmd, mut done) = (call(), command(52), inline_function());
        let a = args(&mut c, &mut cmd, done.0.as_mut_ptr());
        let mut held = unsafe { capture(&f, &a) }.expect("captured");
        assert!(held.done_ok());
        let other = fns_firing(&[0x4242usize], true);
        take_log();
        assert!(!unsafe { discard(&other, held_clone_flag(&mut held)) });
        assert!(!take_log().iter().any(|l| l.starts_with("callback")));
    }

    #[test]
    fn a_completed_command_takes_the_applied_payload_and_fires_any_callback_with_it() {
        let f = fns(); // nothing listed: completing does not need the list
        let (mut c, mut cmd, mut done) = (call(), command(52), inline_function());
        let held_payload = cmd.payload.as_ptr() as usize;
        let a = args(&mut c, &mut cmd, done.0.as_mut_ptr());
        let mut held = unsafe { capture(&f, &a) }.expect("captured");
        assert!(!held.done_ok());
        let mut applied = command(52);
        let applied_payload = applied.payload.as_ptr() as usize;
        applied.command.0[RESULT_OFFSET] = 1;
        let got = unsafe { adopt_result(&mut held, applied.command.0.as_mut_ptr()) };
        assert!(got.ok);
        assert_eq!(held.payload(), applied_payload);
        assert_eq!(
            unsafe { *(applied.command.0.as_ptr() as *const usize) },
            held_payload,
            "the applied command owns the held one's payload now"
        );
        take_log();
        assert!(unsafe { complete(&f, held) }, "fired");
        let log = take_log();
        assert!(log.iter().any(|l| l == "callback called payload=true result=1"), "{log:?}");
    }

    fn held_clone_flag(h: &mut Box<Held>) -> Box<Held> {
        // moves the box out for the call above (the flag stays set: the re-check against the table is what refuses the call)
        std::mem::replace(h, Box::new(unsafe { core::mem::zeroed() }))
    }

    #[test]
    fn done_call_target_reads_slot_two_of_the_vtable_and_refuses_unreadable_memory() {
        let f = inline_function();
        assert_eq!(unsafe { done_call_target(f.0.as_ptr()) }, Some(vt_call as *const () as usize));
        let empty = Room::<FUNCTION_SIZE>([0; FUNCTION_SIZE]);
        assert_eq!(unsafe { done_call_target(empty.0.as_ptr()) }, None);
        let mut wild = Room::<FUNCTION_SIZE>([0; FUNCTION_SIZE]);
        unsafe { *(wild.0.as_mut_ptr().add(FUNCTION_IMPL) as *mut usize) = 0x10 };
        assert_eq!(unsafe { done_call_target(wild.0.as_ptr()) }, None);
        assert_eq!(unsafe { done_call_target(core::ptr::null()) }, None);
    }
}
