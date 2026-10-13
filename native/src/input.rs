//! The input-level text of every held build command, for the mod (B2).
//!
//! One general rule for every build kind: the held command's payload is encoded by the portable layout table with
//! [`crate::portable::Settings::input_level`] (the named members only: what a `SimpleProposal` is made of, without the converted
//! construction's description or any heap graph). The text goes to `native_input_<hold id>.txt` next to the link files; the
//! mod reads it when the `held` event arrives. Read-only on the game; only the last [`KEEP`] files are kept.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

/// How many files stay (older ones are removed as new ones are written).
pub const KEEP: u64 = 64;

pub fn file_name(id: u64) -> String {
    format!("native_input_{id}.txt")
}

#[derive(Default)]
pub struct InputOut {
    dir: Mutex<Option<PathBuf>>,
}

impl InputOut {
    pub fn set_dir(&self, dir: &std::path::Path) {
        *self.dir.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir.to_path_buf());
    }

    /// Writes the text of hold `id` (to a temporary name first, so the mod never reads half of it) and removes an old one.
    pub fn write(&self, id: u64, text: &str) -> io::Result<()> {
        let Some(dir) = self.dir.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return Ok(());
        };
        let tmp = dir.join(format!("{}.tmp", file_name(id)));
        fs::write(&tmp, text)?;
        fs::rename(&tmp, dir.join(file_name(id)))?;
        if id > KEEP {
            let _ = fs::remove_file(dir.join(file_name(id - KEEP)));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_is_written_whole_and_old_ones_go() {
        let dir = std::env::temp_dir().join(format!("fl3-input-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let o = InputOut::default();
        o.write(1, "lost: no folder yet").unwrap();
        o.set_dir(&dir);
        o.write(1, "a").unwrap();
        o.write(1 + KEEP, "b").unwrap();
        assert!(!dir.join(file_name(1)).exists());
        assert_eq!(fs::read_to_string(dir.join(file_name(1 + KEEP))).unwrap(), "b");
        assert!(!dir.join(format!("{}.tmp", file_name(1 + KEEP))).exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
