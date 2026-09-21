//! The local scratch area.
//!
//! Every job gets its own directory so a crash leaves an obvious, self-contained
//! mess rather than files interleaved with someone else's. Directories are removed
//! when the job ends, and any that survive a hard kill are swept on the next start
//! — otherwise they would silently eat the staging budget forever.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Job directories live under this subdirectory of the configured staging path,
/// keeping them clearly apart from the ledger.
const WORK_SUBDIR: &str = "work";

/// A per-job scratch directory, removed on drop.
#[derive(Debug)]
pub struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    /// Creates a fresh directory for `job_id` under `staging_dir`.
    pub fn create(staging_dir: &Path, job_id: u64) -> Result<Self> {
        let dir = work_root(staging_dir).join(format!("job-{job_id:012}"));
        // A leftover from a previous incarnation of the same id would otherwise
        // contaminate this job's outputs.
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Path for the downloaded source, keeping the original extension so the
    /// encoders can recognise the format.
    pub fn input(&self, remote_path: &str) -> PathBuf {
        match Path::new(remote_path).extension().and_then(|e| e.to_str()) {
            Some(ext) => self.dir.join(format!("input.{ext}")),
            None => self.dir.join("input"),
        }
    }

    pub fn output(&self, extension: &str) -> PathBuf {
        self.dir.join(format!("output.{extension}"))
    }

    /// Bytes currently held, for reconciling against the reservation.
    pub fn bytes_used(&self) -> u64 {
        directory_size(&self.dir)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn work_root(staging_dir: &Path) -> PathBuf {
    staging_dir.join(WORK_SUBDIR)
}

fn directory_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => directory_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Removes job directories left behind by a previous run.
///
/// Returns how many were swept. A killed process cannot run its own cleanup, and
/// those directories would otherwise consume the staging budget on every
/// subsequent run without ever being noticed.
pub fn sweep_orphans(staging_dir: &Path) -> Result<usize> {
    let root = work_root(staging_dir);
    if !root.exists() {
        return Ok(0);
    }
    let mut swept = 0;
    for entry in std::fs::read_dir(&root)
        .with_context(|| format!("failed to read {}", root.display()))?
        .flatten()
    {
        let path = entry.path();
        if path.is_dir()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("job-"))
        {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            swept += 1;
        }
    }
    Ok(swept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_is_created_and_removed() {
        let base = tempfile::tempdir().unwrap();
        let path = {
            let ws = Workspace::create(base.path(), 7).unwrap();
            let path = ws.path().to_path_buf();
            assert!(path.is_dir());
            std::fs::write(ws.input("a.jpg"), b"x").unwrap();
            path
        };
        assert!(!path.exists(), "the workspace should be gone after drop");
    }

    #[test]
    fn input_keeps_the_source_extension() {
        let base = tempfile::tempdir().unwrap();
        let ws = Workspace::create(base.path(), 1).unwrap();
        // The encoders dispatch on extension, so it has to survive the download.
        assert!(ws.input("photos/IMG_1.JPG").ends_with("input.JPG"));
        assert!(ws.input("a/b/clip.mp4").ends_with("input.mp4"));
        assert!(ws.input("noext").ends_with("input"));
    }

    #[test]
    fn output_uses_the_recipe_extension() {
        let base = tempfile::tempdir().unwrap();
        let ws = Workspace::create(base.path(), 1).unwrap();
        assert!(ws.output("jxl").ends_with("output.jxl"));
    }

    #[test]
    fn reports_bytes_held() {
        let base = tempfile::tempdir().unwrap();
        let ws = Workspace::create(base.path(), 1).unwrap();
        assert_eq!(ws.bytes_used(), 0);
        std::fs::write(ws.input("a.bin"), vec![0u8; 5000]).unwrap();
        assert_eq!(ws.bytes_used(), 5000);
    }

    #[test]
    fn a_stale_directory_for_the_same_id_is_cleared_first() {
        let base = tempfile::tempdir().unwrap();
        let stale = work_root(base.path()).join("job-000000000042");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("leftover.bin"), b"old").unwrap();

        let ws = Workspace::create(base.path(), 42).unwrap();
        assert_eq!(ws.bytes_used(), 0, "a stale workspace must not leak in");
    }

    /// A killed process leaves its directories behind; they must not accumulate.
    #[test]
    fn orphans_are_swept_on_startup() {
        let base = tempfile::tempdir().unwrap();
        for id in 0..3 {
            let dir = work_root(base.path()).join(format!("job-{id:012}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("partial.bin"), vec![0u8; 100]).unwrap();
        }
        assert_eq!(sweep_orphans(base.path()).unwrap(), 3);
        assert_eq!(sweep_orphans(base.path()).unwrap(), 0);
    }

    #[test]
    fn sweeping_leaves_unrelated_files_alone() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(work_root(base.path())).unwrap();
        let keep = work_root(base.path()).join("notes.txt");
        std::fs::write(&keep, b"not a job").unwrap();

        sweep_orphans(base.path()).unwrap();
        assert!(keep.exists(), "only job- directories should be swept");
    }

    #[test]
    fn sweeping_a_fresh_staging_dir_is_fine() {
        let base = tempfile::tempdir().unwrap();
        assert_eq!(sweep_orphans(base.path()).unwrap(), 0);
    }
}
