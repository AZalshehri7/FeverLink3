//! Diagnostic side files of the session folder, bounded (review 2026-10-31): a long session wrote one set of files per
//! injection and never deleted one (one real session left 1256 files, about 285 MB), and the control reader lists that
//! folder. Each kind keeps only its newest [`KEEP`] files: writing a new one deletes the oldest this run wrote.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// How many files of one kind stay in the folder.
pub const KEEP: usize = 20;

/// The files of each kind written in this run, oldest first.
#[derive(Debug, Default)]
pub struct Bounded {
    written: Mutex<HashMap<&'static str, VecDeque<PathBuf>>>,
}

impl Bounded {
    /// Writes `dir/name` as a file of `kind` and deletes the oldest file of that kind beyond [`KEEP`].
    pub fn write(&self, kind: &'static str, dir: &Path, name: &str, body: &str) -> io::Result<()> {
        let path = dir.join(name);
        std::fs::write(&path, body)?;
        self.keep(kind, path);
        Ok(())
    }

    /// Renames `dir/from` to `dir/name` as a file of `kind` and deletes the oldest file of that kind beyond [`KEEP`]: a file another
    /// writer made (up to 64 MB, an inject order's text) joins the family without being read and written again.
    pub fn rename(&self, kind: &'static str, dir: &Path, from: &str, name: &str) -> io::Result<()> {
        let path = dir.join(name);
        std::fs::rename(dir.join(from), &path)?;
        self.keep(kind, path);
        Ok(())
    }

    fn keep(&self, kind: &'static str, path: PathBuf) {
        let mut written = self.written.lock().unwrap_or_else(|p| p.into_inner());
        let files = written.entry(kind).or_default();
        // a file written again is the newest of its kind: it moves to the back, so it is not the next one pruned
        files.retain(|p| p != &path);
        files.push_back(path);
        while files.len() > KEEP {
            if let Some(old) = files.pop_front() {
                let _ = std::fs::remove_file(old);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_keeps_only_its_newest_files() {
        let dir = tempfile::tempdir().unwrap();
        let b = Bounded::default();
        for k in 0..KEEP + 5 {
            b.write("report", dir.path(), &format!("report_{k}.txt"), "x").unwrap();
        }
        b.write("diff", dir.path(), "diff_0.txt", "y").unwrap();
        b.write("report", dir.path(), &format!("report_{}.txt", KEEP + 4), "again").unwrap();
        let count = |p: &str| {
            std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(p))
                .count()
        };
        assert_eq!(count("report_"), KEEP);
        assert!(!dir.path().join("report_4.txt").exists(), "the oldest are deleted");
        assert!(dir.path().join("report_5.txt").exists());
        assert_eq!(count("diff_"), 1, "another kind is not touched");
        // a file renamed into the kind counts as its newest
        std::fs::write(dir.path().join("input_1.txt"), "z").unwrap();
        b.rename("report", dir.path(), "input_1.txt", "report_99.txt").unwrap();
        assert_eq!(count("report_"), KEEP);
        assert!(!dir.path().join("input_1.txt").exists() && dir.path().join("report_99.txt").exists());
        assert!(!dir.path().join("report_5.txt").exists(), "the oldest went");
    }
}
