//! The local scratch area.
//!
//! Every job gets its own directory so a crash leaves an obvious, self-contained
//! mess rather than files interleaved with someone else's. Directories are removed
//! when the job ends, and any that survive a hard kill are swept on the next start
//! — otherwise they would silently eat the staging budget forever.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Job directories live under this subdirectory of the configured staging path,
/// keeping them clearly apart from the ledger.
const WORK_SUBDIR: &str = "work";

/// Names what a job directory holds, so a later run can tell whether the
/// download inside it is still the right file.
///
/// Written before the download starts. A process killed mid-encode runs no
/// cleanup of its own, so this has to be on disk already for the leftovers to
/// mean anything.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Claimed {
    pub path: String,
    pub size: u64,
    pub mod_time: Option<String>,
}

const META: &str = "claimed.json";

/// A per-job scratch directory, removed on drop.
#[derive(Debug)]
pub struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    /// Creates a fresh directory for `job_id` under `staging_dir`.
    pub fn create(staging_dir: &Path, job_id: u64) -> Result<Self> {
        let dir = work_root(staging_dir).join(job_name(job_id));
        // A leftover from a previous incarnation of the same id would otherwise
        // contaminate this job's outputs.
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        Ok(Self { dir })
    }

    /// Adopts a directory left by an earlier run, moving it to this job's id.
    ///
    /// Reusing the directory rather than copying out of it keeps the download
    /// inside the workspace, where the disk budget already accounts for it. A
    /// cache kept anywhere else would be spent capacity nobody had reserved.
    pub fn adopt(staging_dir: &Path, job_id: u64, from: &Path) -> Result<Self> {
        let dir = work_root(staging_dir).join(job_name(job_id));
        // Job ids restart at zero every run, so the directory being adopted is
        // very often the one this id would have been given anyway. Clearing the
        // destination first would delete the very download being salvaged.
        if dir == from {
            return Ok(Self { dir });
        }
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        std::fs::create_dir_all(work_root(staging_dir))?;
        std::fs::rename(from, &dir)
            .with_context(|| format!("failed to adopt {}", from.display()))?;
        Ok(Self { dir })
    }

    /// Records what this job is working on, before the bytes arrive.
    pub fn claim(&self, claimed: &Claimed) -> Result<()> {
        let text = serde_json::to_string(claimed)?;
        std::fs::write(self.dir.join(META), text)
            .with_context(|| format!("failed to write {}", self.dir.join(META).display()))
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

/// What a previous run left behind that is still worth having.
#[derive(Debug, Default)]
pub struct Salvage {
    /// Remote path to the directory holding its download.
    pub reusable: HashMap<String, PathBuf>,
    /// Directories removed because there was nothing usable in them.
    pub swept: usize,
}

/// Sorts through job directories left by a previous run.
///
/// A killed process runs no cleanup of its own, and those directories would
/// otherwise consume the staging budget on every subsequent run without ever
/// being noticed. But one holding a complete download is worth keeping: the
/// file may be gigabytes, and fetching it again is the most expensive thing a
/// retry can do.
///
/// A half-written encode is not worth keeping, and is not kept. Neither
/// SVT-AV1 nor ffmpeg can resume one, so the output is only ever a truncated
/// file that would have to be thrown away at verification anyway.
pub fn salvage_orphans(staging_dir: &Path) -> Result<Salvage> {
    let root = work_root(staging_dir);
    let mut out = Salvage::default();
    if !root.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&root)
        .with_context(|| format!("failed to read {}", root.display()))?
        .flatten()
    {
        let dir = entry.path();
        if !dir.is_dir()
            || !dir
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("job-"))
        {
            continue;
        }
        match read_claimed(&dir) {
            Some(claimed) if input_is_complete(&dir, &claimed) => {
                out.reusable.insert(claimed.path, dir);
            }
            _ => {
                std::fs::remove_dir_all(&dir)
                    .with_context(|| format!("failed to remove {}", dir.display()))?;
                out.swept += 1;
            }
        }
    }
    Ok(out)
}

pub fn read_claimed(dir: &Path) -> Option<Claimed> {
    let text = std::fs::read_to_string(dir.join(META)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether the download in `dir` is whole.
///
/// Size is the check that matters: a killed download leaves a short file, and
/// reusing one would feed a truncated source to the encoder and call the result
/// a faithful conversion.
fn input_is_complete(dir: &Path, claimed: &Claimed) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.path()
            .file_stem()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "input")
            && e.metadata().is_ok_and(|m| m.len() == claimed.size)
    })
}

/// Whether a salvaged directory still matches what the ledger says is there.
pub fn still_matches(claimed: &Claimed, size: u64, mod_time: Option<&str>) -> bool {
    claimed.size == size && claimed.mod_time.as_deref() == mod_time
}

fn job_name(job_id: u64) -> String {
    format!("job-{job_id:012}")
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
        assert_eq!(salvage_orphans(base.path()).unwrap().swept, 3);
        assert_eq!(salvage_orphans(base.path()).unwrap().swept, 0);
    }

    #[test]
    fn sweeping_leaves_unrelated_files_alone() {
        let base = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(work_root(base.path())).unwrap();
        let keep = work_root(base.path()).join("notes.txt");
        std::fs::write(&keep, b"not a job").unwrap();

        salvage_orphans(base.path()).unwrap();
        assert!(keep.exists(), "only job- directories should be swept");
    }

    #[test]
    fn sweeping_a_fresh_staging_dir_is_fine() {
        let base = tempfile::tempdir().unwrap();
        assert_eq!(salvage_orphans(base.path()).unwrap().swept, 0);
    }

    fn orphan_with(base: &Path, id: u64, claimed: &Claimed, input_len: usize) -> PathBuf {
        let dir = work_root(base).join(job_name(id));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(META), serde_json::to_string(claimed).unwrap()).unwrap();
        std::fs::write(dir.join("input.mkv"), vec![0u8; input_len]).unwrap();
        dir
    }

    fn claimed(size: u64) -> Claimed {
        Claimed {
            path: "videos/a.mkv".into(),
            size,
            mod_time: Some("2026-09-23T00:00:00Z".into()),
        }
    }

    /// The reason any of this exists: a download that survived a kill is worth
    /// far more than the directory it sits in costs.
    #[test]
    fn a_complete_download_is_kept_and_indexed() {
        let base = tempfile::tempdir().unwrap();
        let dir = orphan_with(base.path(), 0, &claimed(100), 100);

        let salvage = salvage_orphans(base.path()).unwrap();
        assert_eq!(salvage.swept, 0);
        assert_eq!(salvage.reusable.get("videos/a.mkv"), Some(&dir));
    }

    /// A killed download leaves a short file. Reusing one would hand the encoder
    /// a truncated source and then call the result a faithful conversion.
    #[test]
    fn a_truncated_download_is_swept_not_reused() {
        let base = tempfile::tempdir().unwrap();
        orphan_with(base.path(), 0, &claimed(100), 40);

        let salvage = salvage_orphans(base.path()).unwrap();
        assert_eq!(salvage.swept, 1);
        assert!(salvage.reusable.is_empty());
    }

    /// A directory with no claim says nothing about what is in it, so there is
    /// no way to know whether it is the right file.
    #[test]
    fn an_unlabelled_directory_is_swept() {
        let base = tempfile::tempdir().unwrap();
        let dir = work_root(base.path()).join(job_name(0));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("input.mkv"), vec![0u8; 100]).unwrap();

        assert_eq!(salvage_orphans(base.path()).unwrap().swept, 1);
    }

    /// The file on the remote can change between runs. Size and modification
    /// time are what the ledger has to compare against.
    #[test]
    fn a_download_of_a_since_changed_file_is_rejected() {
        let c = claimed(100);
        assert!(still_matches(&c, 100, Some("2026-09-23T00:00:00Z")));
        assert!(!still_matches(&c, 200, Some("2026-09-23T00:00:00Z")), "resized");
        assert!(!still_matches(&c, 100, Some("2026-09-24T00:00:00Z")), "touched");
        assert!(!still_matches(&c, 100, None), "modification time lost");
    }

    /// Adoption moves the directory rather than copying out of it, so the
    /// download stays inside a workspace the disk budget accounts for.
    #[test]
    fn adopting_moves_the_directory_to_the_new_job() {
        let base = tempfile::tempdir().unwrap();
        let dir = orphan_with(base.path(), 0, &claimed(100), 100);

        let ws = Workspace::adopt(base.path(), 7, &dir).unwrap();
        assert!(!dir.exists(), "the old directory should be gone");
        assert!(ws.path().ends_with(job_name(7)));
        assert_eq!(std::fs::read(ws.input("videos/a.mkv")).unwrap().len(), 100);
    }

    /// Job ids restart at zero each run, so the directory being salvaged is
    /// usually the one the first job would be handed anyway. Clearing the
    /// destination first would delete the download being rescued.
    #[test]
    fn adopting_the_directory_a_job_would_have_used_keeps_it() {
        let base = tempfile::tempdir().unwrap();
        let dir = orphan_with(base.path(), 0, &claimed(100), 100);

        let ws = Workspace::adopt(base.path(), 0, &dir).unwrap();
        assert_eq!(ws.path(), dir);
        assert_eq!(
            std::fs::read(ws.input("videos/a.mkv")).unwrap().len(),
            100,
            "the download has to survive being adopted in place"
        );
    }

    #[test]
    fn a_claim_round_trips() {
        let base = tempfile::tempdir().unwrap();
        let ws = Workspace::create(base.path(), 1).unwrap();
        let c = claimed(4242);
        ws.claim(&c).unwrap();
        assert_eq!(read_claimed(ws.path()), Some(c));
    }
}
