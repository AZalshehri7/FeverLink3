//! Append-only text log in the session folder. Silent when the module runs as a plain proxy.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

pub fn init(path: PathBuf) {
    let cell = PATH.get_or_init(|| Mutex::new(None));
    if let Ok(mut p) = cell.lock() {
        *p = Some(path);
    }
}

pub fn line(text: &str) {
    let Some(cell) = PATH.get() else { return };
    let Ok(guard) = cell.lock() else { return };
    let Some(path) = guard.as_ref() else { return };
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{text}");
    }
}
