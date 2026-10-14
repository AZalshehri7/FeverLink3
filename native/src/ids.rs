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

#[cfg(windows)]
pub use imp::install;

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
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    static ADD_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
    static REMOVE_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
    static BASE: AtomicUsize = AtomicUsize::new(0);
    static SEQ: AtomicU64 = AtomicU64::new(0);
    static OUT: Mutex<Option<std::io::BufWriter<std::fs::File>>> = Mutex::new(None);
    static ENGINES: Mutex<Vec<usize>> = Mutex::new(Vec::new());

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
        // SAFETY: called with an engine pointer the game itself passed to AddEntity/RemoveEntity, on its thread.
        unsafe { core::ptr::read_unaligned((engine + off) as *const u64) }
    }

    /// FNV-1a over the free-id queue in order, and its first ids.
    fn queue_digest(engine: usize) -> (u64, String) {
        let map = rd(engine, 0xe0) as usize;
        let mapsize = rd(engine, 0xe8) as usize;
        let off = rd(engine, 0xf0) as usize;
        let size = rd(engine, 0xf8) as usize;
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut head = String::new();
        if map == 0 || mapsize == 0 || size > 50_000_000 {
            return (0, head);
        }
        for i in 0..size {
            let p = off + i;
            let block = (p / 4) & (mapsize - 1);
            // SAFETY: the deque's own map and blocks (4 ints per block in MSVC's deque<int>).
            let id = unsafe {
                let b = core::ptr::read_unaligned((map + block * 8) as *const usize);
                core::ptr::read_unaligned((b + (p % 4) * 4) as *const i32)
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
        let f = rd(engine as usize, 0xf8);
        type Orig = unsafe extern "system-unwind" fn(u64, u64, u64, u64) -> u64;
        // SAFETY: the trampoline of AddEntity, published before the jump was written.
        let orig: Orig = unsafe { core::mem::transmute(ADD_ORIGINAL.load(Ordering::Acquire)) };
        let r = unsafe { orig(engine, out, r8, r9) };
        let _ = crate::log::guarded("id trace", || {
            note_engine(engine as usize);
            // SAFETY: AddEntity wrote the new id to `out`.
            let id = unsafe { core::ptr::read_unaligned(out as *const i32) };
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            // SAFETY: no arguments.
            let tid = unsafe { GetCurrentThreadId() };
            line(&format!("{n} {} +{id} e={:x} t={tid} free={f} {}", ms(), engine & 0x00ff_ffff, chain()));
        });
        r
    }

    unsafe extern "system-unwind" fn remove_hook(engine: u64, id: u64, r8: u64, r9: u64) {
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
        type Orig = unsafe extern "system-unwind" fn(u64, u64, u64, u64);
        // SAFETY: the trampoline of RemoveEntity.
        let orig: Orig = unsafe { core::mem::transmute(REMOVE_ORIGINAL.load(Ordering::Acquire)) };
        unsafe { orig(engine, id, r8, r9) }
    }

    /// Installs the trace when the switch is `1` and the build is 40420. The detours stay for the process lifetime.
    pub fn install(build_name: &str, base: usize, dir: Option<PathBuf>, log: fn(&str)) {
        if !std::env::var(ID_TRACE_VAR).map(|v| v.trim() == "1").unwrap_or(false) {
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
        let name = format!("ids_{}.log", dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let path = dir.parent().map(|p| p.join(&name)).unwrap_or_else(|| dir.join(&name));
        match std::fs::File::create(path) {
            Ok(f) => {
                *OUT.lock().unwrap_or_else(|p| p.into_inner()) = Some(std::io::BufWriter::with_capacity(1 << 16, f))
            }
            Err(e) => {
                log(&format!("id trace: not installed: {e}"));
                return;
            }
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
}
