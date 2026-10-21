//! Entity id trace (`FEVERLINK3_ID_TRACE=1`, diagnostic, off by default): logs every entity id the game's ECS engine
//! hands out (`ecs::Engine::AddEntity`) and every removal request (`ecs::Engine::RemoveEntity`), with the engine, the
//! thread, the free-id queue's size and a short return-address chain, to `ids.log` in the game's live folder.
//!
//! Why (2026-10-14): the same build gets other entity ids on the two games (inj7: segment 49287 vs 49351). Build
//! 40420's `AddEntity` (RVA 0x2bb8940) takes the FRONT of a `std::deque<int>` of free ids at engine+0xe0 (map +0xe0,
//! map size +0xe8, offset +0xf0, size +0xf8) and otherwise appends a new id (entity vector at +0x90, 0x18 bytes each);
//! `EndModification` (0x2bb9ea0) appends the ids removed in the modification at the BACK. A FIFO: an entity added and
//! removed on one game only rotates that game's queue, and every later id differs. This log shows which creations
//! happen on one game only. Read-only: the original is always called with the arguments it was given.
//!
//! The same two hooks feed the compact free-id order trace ([`crate::idorder`], `FEVERLINK3_TRACE`, on unless `=0`):
//! `idorder_<live>.log` next to native.log, one line per burst. Either switch installs the hooks; each writes only its own file.

#[cfg(windows)]
pub use imp::install;

/// Marks the order trace (`idorder_<live>.log`) with a session's holding switch, e.g. `holding off (hold_off)`: the diff
/// tool compares two games only up to their session ends. Nothing when the trace is off.
pub fn mark(what: &str) {
    #[cfg(windows)]
    imp::mark(what);
    #[cfg(not(windows))]
    let _ = what;
}

/// The checksum's `freeq` part of this game ([`crate::idorder::EngineOrder::check_part`] of the engine that worked last), `None`
/// when the order trace is off, `FEVERLINK3_FREEQ_CHECK=0`, or no engine has a checkpoint yet. Read by the session controller when
/// it forwards a checksum report (`feverlink3_launcher::session::set_checksum_part`); never called from a hook.
pub fn checksum_part() -> Option<(String, String)> {
    #[cfg(windows)]
    {
        imp::checksum_part().map(|v| (FREEQ_PART.to_owned(), v))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// The checksum part's name, compared by the host as the core says (`feverlink3_core::session::FREEQ_PART`).
pub const FREEQ_PART: &str = feverlink3_core::session::FREEQ_PART;
/// `FEVERLINK3_FREEQ_CHECK=0` leaves the `freeq` part out of this game's checksum reports (on with the order trace otherwise).
pub const FREEQ_CHECK_VAR: &str = "FEVERLINK3_FREEQ_CHECK";

/// The switch.
pub const ID_TRACE_VAR: &str = "FEVERLINK3_ID_TRACE";

/// Build 40420: `ecs::Engine::AddEntity(Engine*, Entity* out) -> Entity*` and its first three instructions.
pub const ADD_ENTITY_RVA: u32 = 0x2bb_8940;
pub const ADD_ENTITY_PROLOGUE: [u8; 15] =
    [0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x6C, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18];
/// Build 40420: `ecs::Engine::RemoveEntity(Engine*, Entity, bool)` and its first instructions.
pub const REMOVE_ENTITY_RVA: u32 = 0x2bb_c700;
pub const REMOVE_ENTITY_PROLOGUE: [u8; 15] =
    [0x48, 0x89, 0x5C, 0x24, 0x10, 0x44, 0x88, 0x44, 0x24, 0x18, 0x55, 0x56, 0x57, 0x41, 0x54];

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::detour::Detour;
    use crate::idorder::EngineOrder;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    static ADD_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
    static REMOVE_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
    static BASE: AtomicUsize = AtomicUsize::new(0);
    static SEQ: AtomicU64 = AtomicU64::new(0);
    static OUT: Mutex<Option<std::io::BufWriter<std::fs::File>>> = Mutex::new(None);
    static ENGINES: Mutex<Vec<usize>> = Mutex::new(Vec::new());
    /// The verbose trace (`FEVERLINK3_ID_TRACE=1`) and the compact order trace (`FEVERLINK3_TRACE`) are on.
    static VERBOSE: AtomicBool = AtomicBool::new(false);
    static COMPACT: AtomicBool = AtomicBool::new(false);
    /// The `freeq` checksum part is sent (the order trace on and `FEVERLINK3_FREEQ_CHECK` not `0`).
    static FREEQ: AtomicBool = AtomicBool::new(false);
    /// Per engine (its address) what the order trace has seen; a handful of engines per process (two per loaded world).
    static ORDER: Mutex<Vec<(usize, EngineOrder)>> = Mutex::new(Vec::new());
    /// The order trace's lines not yet written, and when the file was last written (ms, `pace::clock_ms`). Hooks only append
    /// here (a short lock, no I/O).
    static ORDER_PENDING: Mutex<(Vec<u8>, u64)> = Mutex::new((Vec::new(), 0));
    /// The order trace's file. Whoever writes takes it with `try_lock` and then the pending lines, so no game thread ever waits
    /// for another one's file write, and the lines reach the file in the order they were appended.
    static ORDER_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
    /// Pending bytes past this are written at once, without waiting for the second to pass.
    const ORDER_CHUNK: usize = 1 << 16;

    /// A queue larger than this is not the field expected: not read.
    const QUEUE_MAX: usize = 50_000_000;

    /// The free-id queue's ids `from..to` in queue order (empty when the deque does not look like one).
    fn queue_ids(engine: usize, from: usize, to: usize) -> Vec<i32> {
        let map = rd(engine, 0xe0) as usize;
        let mapsize = rd(engine, 0xe8) as usize;
        let off = rd(engine, 0xf0) as usize;
        let size = rd(engine, 0xf8) as usize;
        if map == 0 || mapsize == 0 || !mapsize.is_power_of_two() || size > QUEUE_MAX || to > size {
            return Vec::new();
        }
        // a block pointer that is not readable ends the list there (the trace is short, the game goes on)
        (from..to).map_while(|i| deque_id(map, mapsize, off + i)).collect()
    }

    /// Entry `p` of MSVC's `deque<int>` (4 ints per block) through the guarded reader (`memguard`): the map and its blocks are the
    /// engine's heap, read on the engine's thread inside its own AddEntity/RemoveEntity. Until 2026-10-19 this read them unchecked:
    /// a stale block pointer was a crash of the game for the sake of a trace. `mapsize` is a power of two.
    fn deque_id(map: usize, mapsize: usize, p: usize) -> Option<i32> {
        let block = (p / 4) & (mapsize - 1);
        let b = crate::memguard::word(map.checked_add(block * 8)?)? as usize;
        crate::memguard::read_value::<i32>(b.checked_add((p % 4) * 4)?)
    }

    /// The number of id slots (the entity vector's length).
    fn ents(engine: usize) -> u64 {
        (rd(engine, 0x98).wrapping_sub(rd(engine, 0x90))) / 0x18
    }

    /// Queues `lines`; writes the queue when a second has passed since the last write (at most a second of lines lost in a
    /// crash) or it is large, unless another thread is writing right now (it or the next call takes these lines too).
    fn order_write(lines: Vec<String>) {
        if lines.is_empty() || !COMPACT.load(Ordering::Relaxed) {
            return;
        }
        let now = crate::pace::clock_ms();
        let due = {
            let mut p = ORDER_PENDING.lock().unwrap_or_else(|p| p.into_inner());
            for l in &lines {
                p.0.extend_from_slice(l.as_bytes());
                p.0.push(b'\n');
            }
            p.0.len() >= ORDER_CHUNK || now.saturating_sub(p.1) >= 1000
        };
        if due {
            order_flush(now);
        }
    }

    /// Writes what is pending, when no other thread is writing (never waits for the file).
    fn order_flush(now: u64) {
        let Ok(mut file) = ORDER_FILE.try_lock() else { return };
        let Some(f) = file.as_mut() else { return };
        let bytes = {
            let mut p = ORDER_PENDING.lock().unwrap_or_else(|p| p.into_inner());
            p.1 = now;
            std::mem::take(&mut p.0)
        };
        if !bytes.is_empty() {
            let _inside = crate::hookwatch::IDORDER_FILE.enter();
            let _ = f.write_all(&bytes);
        }
    }

    /// Runs `f` on this engine's order state (created, with its first-seen line, when new) after the other engines' idle
    /// lines are written and this call has been entered (`enter`: the ids appended to the queue since are taken).
    fn order_call(engine: usize, f: impl FnOnce(&mut EngineOrder, &mut Vec<String>)) {
        let now = crate::pace::clock_ms();
        let free = rd(engine, 0xf8);
        let mut out = Vec::new();
        let inside = crate::hookwatch::IDORDER_LOCK.enter();
        if let Ok(mut g) = ORDER.lock() {
            let i = match g.iter().position(|(e, _)| *e == engine) {
                Some(i) => i,
                None => {
                    let mut o = EngineOrder::new(engine & 0x00ff_ffff, free, ents(engine));
                    out.push(o.first_line(&ms(), &queue_ids(engine, 0, free as usize)));
                    g.push((engine, o));
                    g.len() - 1
                }
            };
            for (j, (_, o)) in g.iter_mut().enumerate() {
                if j != i {
                    o.flush_idle(now, &mut out);
                }
            }
            let o = &mut g[i].1;
            o.enter(now, ms, free, |k| queue_ids(engine, (free as usize).saturating_sub(k), free as usize), &mut out);
            f(o, &mut out);
        }
        drop(inside);
        order_write(out);
    }

    /// `AddEntity` returned `id`: the op is recorded with the queue size after the pop.
    fn order_alloc(engine: usize, id: i32) {
        let (free, slots) = (rd(engine, 0xf8), ents(engine));
        let mut out = Vec::new();
        {
            let _inside = crate::hookwatch::IDORDER_LOCK.enter();
            if let Ok(mut g) = ORDER.lock() {
                if let Some((_, o)) = g.iter_mut().find(|(e, _)| *e == engine) {
                    o.alloc(id, free, slots, &mut out);
                }
            }
        }
        order_write(out);
    }

    /// A mark line in the order trace (`what`: the session's holding switched), after every engine's pending line, written
    /// at once. Called from the module's runtime thread, never from a hook.
    pub fn mark(what: &str) {
        if !COMPACT.load(Ordering::Relaxed) {
            return;
        }
        let mut out = Vec::new();
        {
            let _inside = crate::hookwatch::IDORDER_LOCK.enter();
            if let Ok(mut g) = ORDER.lock() {
                let mut engines: Vec<&mut EngineOrder> = g.iter_mut().map(|(_, o)| o).collect();
                crate::idorder::mark_line(&ms(), what, &mut engines, &mut out);
            }
        }
        order_write(out);
        order_flush(crate::pace::clock_ms());
    }

    /// The `freeq` part of the engine that worked last (the two engines of a loaded world run the same stream; an engine of a world
    /// left behind, e.g. the menu's or the one before a resync load, stops working), `None` while that engine has no checkpoint yet:
    /// never an older engine's part (two games' engines of the world before a resync load may have forked; that is what the load
    /// repaired). From the session controller's thread; a hook holds the lock only briefly.
    pub fn checksum_part() -> Option<String> {
        if !FREEQ.load(Ordering::Relaxed) {
            return None;
        }
        let _inside = crate::hookwatch::IDORDER_LOCK.enter();
        let g = ORDER.lock().ok()?;
        g.iter().map(|(_, o)| o).max_by_key(|o| o.last_ms())?.check_part()
    }

    /// Local wall time, as in trace.log.
    fn ms() -> String {
        // SAFETY: fills the struct.
        let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { core::mem::zeroed() };
        unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
        format!("{:02}:{:02}:{:02}.{:03}", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds)
    }

    /// Return addresses above the hook, as RVAs (0 = outside the image).
    fn chain() -> String {
        let mut frames = [core::ptr::null_mut(); 10];
        // SAFETY: six output slots; the hash pointer may be null.
        let got = unsafe { RtlCaptureStackBackTrace(5, 10, frames.as_mut_ptr(), core::ptr::null_mut()) } as usize;
        let base = BASE.load(Ordering::Relaxed);
        let mut s = String::new();
        for f in &frames[..got.min(10)] {
            let a = *f as usize;
            let r = if a >= base && a < base + 0x0400_0000 { a - base } else { 0 };
            s.push_str(&format!("{r:x},"));
        }
        s
    }

    /// Reads a u64 of the engine; the engine object is live while its own method runs.
    fn rd(engine: usize, off: usize) -> u64 {
        // an engine pointer the game itself passed to AddEntity/RemoveEntity, on its thread; read guarded all the same (0 if not)
        engine.checked_add(off).and_then(crate::memguard::word).unwrap_or(0)
    }

    /// FNV-1a over the free-id queue in order, and its first ids.
    fn queue_digest(engine: usize) -> (u64, String) {
        let map = rd(engine, 0xe0) as usize;
        let mapsize = rd(engine, 0xe8) as usize;
        let off = rd(engine, 0xf0) as usize;
        let size = rd(engine, 0xf8) as usize;
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut head = String::new();
        if map == 0 || mapsize == 0 || !mapsize.is_power_of_two() || size > 50_000_000 {
            return (0, head);
        }
        for i in 0..size {
            let Some(id) = deque_id(map, mapsize, off + i) else {
                head.push_str("unreadable,");
                break;
            };
            h = (h ^ id as u32 as u64).wrapping_mul(0x0000_0100_0000_01b3);
            if i < 8 {
                head.push_str(&format!("{id},"));
            }
        }
        (h, head)
    }

    fn line(s: &str) {
        if let Ok(mut g) = OUT.lock() {
            if let Some(w) = g.as_mut() {
                let _ = writeln!(w, "{s}");
                if SEQ.load(Ordering::Relaxed).is_multiple_of(64) {
                    let _ = w.flush();
                }
            }
        }
    }

    fn note_engine(engine: usize) {
        let new = ENGINES
            .lock()
            .map(|mut e| {
                if e.contains(&engine) {
                    false
                } else {
                    e.push(engine);
                    true
                }
            })
            .unwrap_or(false);
        if new {
            let (h, head) = queue_digest(engine);
            let n = (rd(engine, 0x98).wrapping_sub(rd(engine, 0x90))) / 0x18;
            line(&format!(
                "engine {engine:#x} first seen: entities={n} free={} fifo_hash={h:016x} head={head}",
                rd(engine, 0xf8)
            ));
        }
    }

    unsafe extern "system-unwind" fn add_hook(engine: u64, out: u64, r8: u64, r9: u64) -> u64 {
        let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::IDS_ADD_HOOK);
        let f = rd(engine as usize, 0xf8);
        let compact = COMPACT.load(Ordering::Relaxed);
        if compact {
            // before the pop: the ids the engine appended since the last call are still at the queue's back
            let _ = crate::log::guarded("id order trace", || order_call(engine as usize, |_, _| {}));
        }
        type Orig = unsafe extern "system-unwind" fn(u64, u64, u64, u64) -> u64;
        // SAFETY: the trampoline of AddEntity, published before the jump was written.
        let orig: Orig = unsafe { core::mem::transmute(ADD_ORIGINAL.load(Ordering::Acquire)) };
        let r = unsafe { orig(engine, out, r8, r9) };
        // SAFETY: AddEntity wrote the new id to `out`.
        let id = unsafe { core::ptr::read_unaligned(out as *const i32) };
        if compact {
            let _ = crate::log::guarded("id order trace", || order_alloc(engine as usize, id));
        }
        if VERBOSE.load(Ordering::Relaxed) {
            let _ = crate::log::guarded("id trace", || {
                note_engine(engine as usize);
                let n = SEQ.fetch_add(1, Ordering::Relaxed);
                // SAFETY: no arguments.
                let tid = unsafe { GetCurrentThreadId() };
                line(&format!("{n} {} +{id} e={:x} t={tid} free={f} {}", ms(), engine & 0x00ff_ffff, chain()));
            });
        }
        r
    }

    unsafe extern "system-unwind" fn remove_hook(engine: u64, id: u64, r8: u64, r9: u64) {
        let _barrier = crate::failsafe::Barrier::new(&crate::hookwatch::IDS_REMOVE_HOOK);
        if COMPACT.load(Ordering::Relaxed) {
            let _ = crate::log::guarded("id order trace", || order_call(engine as usize, |o, _| o.removal()));
        }
        if VERBOSE.load(Ordering::Relaxed) {
            let _ = crate::log::guarded("id trace", || {
                note_engine(engine as usize);
                let n = SEQ.fetch_add(1, Ordering::Relaxed);
                // SAFETY: no arguments.
                let tid = unsafe { GetCurrentThreadId() };
                line(&format!(
                    "{n} {} -{} e={:x} t={tid} free={} {}",
                    ms(),
                    id as i32,
                    engine & 0x00ff_ffff,
                    rd(engine as usize, 0xf8),
                    chain()
                ));
            });
        }
        type Orig = unsafe extern "system-unwind" fn(u64, u64, u64, u64);
        // SAFETY: the trampoline of RemoveEntity.
        let orig: Orig = unsafe { core::mem::transmute(REMOVE_ORIGINAL.load(Ordering::Acquire)) };
        unsafe { orig(engine, id, r8, r9) }
    }

    /// Installs the hooks when the verbose trace (`FEVERLINK3_ID_TRACE=1`) or the order trace (`FEVERLINK3_TRACE`, on
    /// unless `0`) is on and the build is 40420. The detours stay for the process lifetime.
    pub fn install(build_name: &str, base: usize, dir: Option<PathBuf>, log: fn(&str)) {
        let verbose = std::env::var(ID_TRACE_VAR).map(|v| v.trim() == "1").unwrap_or(false);
        let compact = crate::idorder::trace_on(std::env::var(crate::idorder::TRACE_VAR).ok().as_deref());
        if !verbose && !compact {
            return;
        }
        if build_name != "40420" {
            log("id trace: not installed (only build 40420 is known)");
            return;
        }
        let Some(dir) = dir else {
            log("id trace: not installed (no live folder)");
            return;
        };
        // next to native.log: the session controller clears the live folder when it starts
        let live = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // A game started again (after a crash, or a quick start that is closed at once) used to truncate the trace of the game
        // before it: the previous file is kept as `<prefix>_<live>.prev.log` (one generation). The full path goes to native.log:
        // native.log is shared by every game of the PC and keeps old runs, so a bare name sent the reader of the friend test
        // 2026-10-21 to an old empty `idorder_live1.log` of a lab run while the host's real trace was `idorder_live.log`.
        let open = |prefix: &str| {
            let name = format!("{prefix}_{live}.log");
            let path = dir.parent().map(|p| p.join(&name)).unwrap_or_else(|| dir.join(&name));
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > 0) {
                let _ = std::fs::rename(&path, path.with_file_name(format!("{prefix}_{live}.prev.log")));
            }
            std::fs::File::create(&path)
                .map(|f| (std::io::BufWriter::with_capacity(1 << 16, f), path.clone()))
                .map_err(|e| log(&format!("id trace: {} not opened: {e}", path.display())))
                .ok()
        };
        if verbose {
            if let Some((w, _)) = open("ids") {
                *OUT.lock().unwrap_or_else(|p| p.into_inner()) = Some(w);
                VERBOSE.store(true, Ordering::Release);
            }
        }
        if compact {
            if let Some((w, path)) = open("idorder") {
                // the order trace queues its own lines (order_write): the plain file, not a buffered writer
                *ORDER_FILE.lock().unwrap_or_else(|p| p.into_inner()) = w.into_inner().ok();
                COMPACT.store(true, Ordering::Release);
                let freeq = std::env::var(FREEQ_CHECK_VAR).map(|v| v.trim() != "0").unwrap_or(true);
                FREEQ.store(freeq, Ordering::Release);
                log(&format!(
                    "id order trace: on, pid {} writes {} (the previous run's as idorder_{live}.prev.log; FEVERLINK3_TRACE=0 turns                      it off); checksum part {FREEQ_PART}: {} ({FREEQ_CHECK_VAR}=0 off)",
                    std::process::id(),
                    path.display(),
                    if freeq { "on" } else { "off" }
                ));
            }
        }
        if !VERBOSE.load(Ordering::Acquire) && !COMPACT.load(Ordering::Acquire) {
            return;
        }
        BASE.store(base, Ordering::Release);
        for (name, rva, pro, hook, slot) in [
            ("add_entity", ADD_ENTITY_RVA, &ADD_ENTITY_PROLOGUE, add_hook as *const () as usize, &ADD_ORIGINAL),
            (
                "remove_entity",
                REMOVE_ENTITY_RVA,
                &REMOVE_ENTITY_PROLOGUE,
                remove_hook as *const () as usize,
                &REMOVE_ORIGINAL,
            ),
        ] {
            // SAFETY: called from DllMain before the game runs; the prologue is checked against the bytes in memory.
            match unsafe { Detour::prepare((base + rva as usize) as *mut u8, pro) } {
                Ok(mut d) => {
                    slot.store(d.original() as usize, Ordering::Release);
                    match unsafe { d.commit(hook) } {
                        Ok(()) => {
                            log(&format!("id trace: {name} hooked (ids_<live>.log next to native.log)"));
                            std::mem::forget(d);
                        }
                        Err(e) => log(&format!("id trace: {name} NOT hooked: {e}")),
                    }
                }
                Err(e) => log(&format!("id trace: {name} NOT hooked: {e}")),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A game thread that logs while another one is inside the file write does not wait for it, and the lines still reach
        /// the file in the order they were queued.
        #[test]
        fn a_hook_never_waits_for_another_threads_file_write_and_the_order_is_kept() {
            let dir = crate::testutil::TempDir::new("idorder-write");
            let path = dir.path().join("idorder_test.log");
            *ORDER_FILE.lock().unwrap() = Some(std::fs::File::create(&path).unwrap());
            *ORDER_PENDING.lock().unwrap() = (Vec::new(), 0);
            COMPACT.store(true, Ordering::Release);
            order_write(vec!["one".into()]);
            // another thread is writing (holds the file) and does not come back for a while
            let held = ORDER_FILE.lock().unwrap();
            let t = std::time::Instant::now();
            let h = std::thread::spawn(|| {
                order_write(vec!["two".into()]);
                order_write(vec!["three".into(); ORDER_CHUNK / 5]);
            });
            h.join().unwrap();
            assert!(t.elapsed() < std::time::Duration::from_secs(2), "the logging thread waited for the file");
            drop(held);
            order_flush(crate::pace::clock_ms());
            let text = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(&lines[..2], &["one", "two"]);
            assert_eq!(lines.len(), 2 + ORDER_CHUNK / 5);
            COMPACT.store(false, Ordering::Release);
            *ORDER_FILE.lock().unwrap() = None;
        }

        /// A fake engine whose free-id queue is an MSVC `deque<int>` (4 ints per block, the map a power of two long)
        /// holding `ids` from position `off` on, so that the queue wraps around the map's end.
        #[test]
        fn the_queue_is_read_in_order_across_blocks_and_the_maps_end() {
            let ids: Vec<i32> = (100..111).collect();
            let (mapsize, off) = (4usize, 9usize);
            let mut blocks = vec![[0i32; 4]; mapsize];
            for (i, &id) in ids.iter().enumerate() {
                let p = off + i;
                blocks[(p / 4) & (mapsize - 1)][p % 4] = id;
            }
            let map: Vec<usize> = blocks.iter().map(|b| b.as_ptr() as usize).collect();
            let mut engine = vec![0u64; 0x100 / 8];
            engine[0xe0 / 8] = map.as_ptr() as u64;
            engine[0xe8 / 8] = mapsize as u64;
            engine[0xf0 / 8] = off as u64;
            engine[0xf8 / 8] = ids.len() as u64;
            let e = engine.as_ptr() as usize;
            assert_eq!(queue_ids(e, 0, ids.len()), ids);
            assert_eq!(queue_ids(e, 8, 11), vec![108, 109, 110], "the tail: the last ids appended");
            assert!(queue_ids(e, 0, 12).is_empty(), "past the size: nothing");
            engine[0xe8 / 8] = 3;
            assert!(queue_ids(e, 0, 2).is_empty(), "a map that is not a power of two is not a deque");
        }
    }
}
