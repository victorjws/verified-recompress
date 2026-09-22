//! Runs conversions concurrently.
//!
//! There are no explicit stage queues. Each file is one task that walks the whole
//! sequence, taking a resource budget only for the step that needs it and giving it
//! straight back. Because a task waiting on a core is not holding a network slot,
//! downloads, encodes and uploads overlap on their own, which is all the stage
//! pipeline would have bought at a fraction of the machinery.
//!
//! How many files are in flight is decided by the staging budget rather than a job
//! count: a lease is taken before the download and held until the job ends, so the
//! disk can never be oversubscribed.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::classify;
use crate::config::Order;
use crate::convert;
use crate::governor::{self, Governor};
use crate::ledger::{Conversion, FileRow, Ledger, State};
use crate::policy::{self, Decision, Facts, Limits, Recipe, SkipReason};
use crate::remote::Remote;
use crate::scope::Scope;
use crate::staging::Workspace;

/// How much of a file to read when confirming an ambiguous extension.
const HEAD_BYTES: usize = 188 * 8;

/// Recorded for files an exclude pattern kept out. Reopened on every run, since
/// the patterns are configuration rather than a property of the file.
pub const OUT_OF_SCOPE: &str = "out_of_scope";

#[derive(Debug, Clone, Copy, Default)]
pub struct Summary {
    pub converted: u64,
    pub skipped: u64,
    pub failed: u64,
    pub input_bytes: u64,
    pub output_bytes: u64,
}

impl Summary {
    pub fn saved_bytes(&self) -> u64 {
        self.input_bytes.saturating_sub(self.output_bytes)
    }

    fn merge(&mut self, other: Outcome) {
        match other {
            Outcome::Converted { input, output } => {
                self.converted += 1;
                self.input_bytes += input;
                self.output_bytes += output;
            }
            Outcome::Skipped => self.skipped += 1,
            Outcome::Failed => self.failed += 1,
        }
    }
}

enum Outcome {
    Converted { input: u64, output: u64 },
    Skipped,
    Failed,
}

pub struct Options {
    /// Without this nothing on the remote is written to or deleted.
    pub execute: bool,
    /// Permits the irreversible AV1 tier.
    pub allow_video: bool,
    /// Stop after this many files.
    pub limit: Option<usize>,
    /// Which files this run may touch.
    pub scope: Scope,
    /// Sequence files are claimed in.
    pub order: Order,
    /// Duration floor for the AV1 tier, in seconds. Zero converts every length.
    pub min_video_secs: f64,
    /// Encoder settings the video recipes need.
    pub video: convert::VideoOptions,
}

pub struct Pipeline<R: Remote + 'static> {
    remote: Arc<R>,
    ledger: Ledger,
    governor: Arc<Governor>,
    staging_dir: std::path::PathBuf,
    max_file_bytes: u64,
    cancel: CancellationToken,
    next_job_id: AtomicU64,
}

impl<R: Remote + 'static> Pipeline<R> {
    pub fn new(
        remote: Arc<R>,
        ledger: Ledger,
        governor: Arc<Governor>,
        staging_dir: std::path::PathBuf,
        max_file_bytes: u64,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            remote,
            ledger,
            governor,
            staging_dir,
            max_file_bytes,
            cancel,
            next_job_id: AtomicU64::new(0),
        }
    }

    /// Claims files from the ledger and processes them until it runs out, hits the
    /// limit, or is cancelled.
    pub async fn run(self: Arc<Self>, opts: Options) -> Result<Summary> {
        let opts = Arc::new(opts);
        let mut tasks: JoinSet<Outcome> = JoinSet::new();
        let mut summary = Summary::default();
        let mut started = 0usize;

        loop {
            if self.cancel.is_cancelled() {
                tracing::info!("cancelled; letting in-flight jobs finish");
                break;
            }
            if opts.limit.is_some_and(|l| started >= l) {
                break;
            }

            let Some(row) = self
                .ledger
                .claim_next(opts.scope.prefixes(), opts.order)
                .await?
            else {
                break;
            };

            // Prefixes are enforced in the query; the glob excludes are checked
            // here, where a compiled GlobSet is available.
            if opts.scope.is_excluded(&row.path) {
                self.ledger
                    .set_state(&row.path, State::Skipped, Some(OUT_OF_SCOPE.to_string()))
                    .await?;
                summary.skipped += 1;
                continue;
            }
            started += 1;

            let this = Arc::clone(&self);
            let opts = Arc::clone(&opts);
            tasks.spawn(async move { this.process(row, &opts).await });

            // Keep the in-flight set from growing without bound while the disk
            // budget is the real limiter; drain whatever has already finished.
            while let Some(done) = tasks.try_join_next() {
                summary.merge(done.unwrap_or(Outcome::Failed));
            }
        }

        while let Some(done) = tasks.join_next().await {
            summary.merge(done.unwrap_or(Outcome::Failed));
        }
        Ok(summary)
    }

    /// One file, start to finish. Any error here leaves the remote untouched.
    async fn process(&self, row: FileRow, opts: &Options) -> Outcome {
        let path = row.path.clone();
        match self.try_process(&row, opts).await {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!("{path}: {e:#}");
                let _ = self
                    .ledger
                    .set_state(&path, State::Failed, Some(format!("{e:#}")))
                    .await;
                Outcome::Failed
            }
        }
    }

    async fn try_process(&self, row: &FileRow, opts: &Options) -> Result<Outcome> {
        let limits = Limits {
            max_file_bytes: self.max_file_bytes,
            allow_video: opts.allow_video,
            min_video_secs: opts.min_video_secs,
        };

        // Decide once on the cheap facts. Anything that needs the bytes is deferred
        // rather than guessed, and re-decided below once they are local.
        let cheap = policy::decide(Facts::new(&row.path, row.size), limits);
        if let Decision::Skip(reason) = cheap
            && reason != SkipReason::NeedsProbe
        {
            return self.record_skip(&row.path, reason).await;
        }

        // Reserve the worst case for any recipe this file might take, so the disk
        // cannot be oversubscribed by a decision that changes after the download.
        let provisional = cheap.recipe().unwrap_or(Recipe::Av1);
        let reservation = governor::reservation_mib(provisional, row.size);
        if reservation > self.governor.disk_capacity_mib() {
            return self.record_skip(&row.path, SkipReason::TooLargeForBudget).await;
        }
        let _disk = self.governor.disk(reservation).await?;

        let job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
        let workspace = Workspace::create(&self.staging_dir, job_id)?;
        let input = workspace.input(&row.path);

        {
            let _net = self.governor.network().await;
            self.remote.download(&row.path, &input).await?;
        }

        // Hash the original now, while it is certainly still on disk: some
        // recipes delete it before verification. This is what a restore is
        // checked against, and taking it from the bytes that were actually
        // converted beats trusting whatever a listing reported.
        let original_hash = crate::hash::blake3_file(&input).await?;

        // Now that the bytes are here, settle the decision properly.
        let head = read_head(&input).await?;
        let probe = if classify::kind_from_extension(&row.path).is_video()
            || row.path.to_ascii_lowercase().ends_with(".m4a")
        {
            classify::probe_file(&input).await.ok()
        } else {
            None
        };

        let mut facts = Facts::new(&row.path, row.size).with_head(&head);
        if let Some(probe) = &probe {
            facts = facts.with_probe(probe);
        }
        let recipe = match policy::decide(facts, limits) {
            Decision::Convert(recipe) => recipe,
            Decision::Skip(reason) => return self.record_skip(&row.path, reason).await,
        };

        let output = workspace.output(recipe.output_extension(extension_of(&row.path)));

        // AV1 chooses its own CRF by scoring the result, so encoding and
        // verification are one step; every other recipe measures the source first
        // and checks the output against that.
        let fidelity = if recipe == Recipe::Av1 {
            let probe = probe
                .as_ref()
                .context("AV1 requires a probe, which should have been taken already")?;
            let cpu = self.governor.cpu(recipe).await;
            let (fidelity, attempt) = convert::convert_av1(
                &input,
                &output,
                probe,
                workspace.path(),
                cpu.cores(),
                opts.video,
            )
            .await?;
            tracing::info!("{}: crf {}, {}", row.path, attempt.crf, attempt.scores.summary());
            fidelity
        } else {
            let fingerprint = convert::fingerprint(recipe, &input).await?;
            {
                let cpu = self.governor.cpu(recipe).await;
                convert::encode(
                    recipe,
                    &input,
                    &output,
                    workspace.path(),
                    cpu.cores(),
                    opts.video,
                )
                .await?;
            }

            // Verification works from the recorded fingerprint, so the source can
            // go now and the peak footprint stays near one copy plus the output.
            if !keeps_source_for_verification(recipe) {
                let _ = tokio::fs::remove_file(&input).await;
            }

            let cpu = self.governor.cpu(recipe).await;
            convert::verify(recipe, &output, &fingerprint, workspace.path(), cpu.cores()).await?
        };

        let output_bytes = tokio::fs::metadata(&output).await?.len();
        if !is_worthwhile(row.size, output_bytes) {
            return self.record_skip(&row.path, SkipReason::AlreadyOptimal).await;
        }

        if !opts.execute {
            tracing::info!(
                "{}: would convert with {} ({} -> {} bytes, {})",
                row.path,
                recipe.as_str(),
                row.size,
                output_bytes,
                fidelity.as_str()
            );
            // A dry run must leave the ledger where it found it, or the next real
            // run would think this file was already handled.
            self.ledger
                .set_state(&row.path, State::Pending, None)
                .await?;
            return Ok(Outcome::Converted {
                input: row.size,
                output: output_bytes,
            });
        }

        let remote_output =
            swap_extension(&row.path, recipe.output_extension(extension_of(&row.path)));

        // A recipe that keeps the container produces the same path it started
        // from. Uploading straight over it and then deleting "the original" would
        // delete the replacement, so the write goes to a staging name first and
        // only takes the final path once it is confirmed. That also keeps the
        // invariant that nothing is removed before its replacement exists.
        let replaces_in_place = remote_output == row.path;
        let upload_target = if replaces_in_place {
            format!("{}.verified-recompress-{job_id}.part", row.path)
        } else {
            remote_output.clone()
        };

        if !replaces_in_place {
            let _api = self.governor.api().await;
            if self.remote.stat(&remote_output).await?.is_some() {
                bail!("{remote_output} already exists; refusing to overwrite it");
            }
        }

        // Hold remote quota for the upload. It is only released once the trash has
        // actually been emptied, because until then the original is still billed.
        let cloud_mib = governor::bytes_to_mib(output_bytes);
        let cloud = self.governor.cloud(cloud_mib).await?;

        let local_hash = crate::hash::blake3_file(&output).await?;
        {
            let _net = self.governor.network().await;
            self.remote.upload(&output, &upload_target).await?;
        }

        // Confirm what landed before touching the original. `hashsum` is answered
        // server-side, so this costs no download.
        {
            let _api = self.governor.api().await;
            if let Err(e) = self
                .confirm_upload(&upload_target, output_bytes, &local_hash)
                .await
            {
                // Leave nothing half-written behind for the next run to trip over.
                let _ = self.remote.delete(&upload_target).await;
                return Err(e);
            }
        }

        // Only now is it safe. The original goes to the trash, where it stays
        // recoverable until `cleanup` runs.
        {
            let _api = self.governor.api().await;
            self.remote.delete(&row.path).await?;
        }

        if replaces_in_place {
            let _api = self.governor.api().await;
            self.remote
                .move_to(&upload_target, &remote_output)
                .await
                .with_context(|| {
                    format!(
                        "the replacement is uploaded but still named {upload_target}; \
                         the original is recoverable from the trash"
                    )
                })?;
        }

        self.ledger
            .record_conversion(Conversion {
                path: row.path.clone(),
                output_path: remote_output.clone(),
                output_size: output_bytes,
                recipe: recipe.as_str().to_string(),
                fidelity: fidelity.as_str().to_string(),
                original_blake3: original_hash,
            })
            .await?;

        // The quota stays spent past the end of this job: the original is in the
        // trash and still billed until `cleanup` empties it.
        cloud.hold();

        tracing::info!(
            "{} -> {} ({} -> {} bytes, {})",
            row.path,
            remote_output,
            row.size,
            output_bytes,
            fidelity.as_str()
        );
        Ok(Outcome::Converted {
            input: row.size,
            output: output_bytes,
        })
    }

    /// Checks the uploaded object is the file we meant to upload.
    ///
    /// A size match alone would not catch a truncated or corrupted transfer, so the
    /// hash is compared too whenever the backend can produce one. Filen reports
    /// blake3; a backend that reports nothing leaves only the size check, and that
    /// is stated rather than passed off as a full verification.
    async fn confirm_upload(&self, path: &str, expected_size: u64, expected_hash: &str) -> Result<()> {
        let Some(entry) = self.remote.stat(path).await? else {
            bail!("{path} is missing after upload");
        };
        if entry.size != expected_size {
            bail!(
                "{path} is {} bytes on the remote, expected {expected_size}",
                entry.size
            );
        }
        match self.remote.hashsum(path).await? {
            Some(remote_hash) if remote_hash.eq_ignore_ascii_case(expected_hash) => Ok(()),
            Some(remote_hash) => bail!(
                "{path} hashes to {remote_hash} on the remote but {expected_hash} locally"
            ),
            None => {
                tracing::warn!(
                    "{path}: the remote reports no hash, so only the size was confirmed"
                );
                Ok(())
            }
        }
    }

    async fn record_skip(&self, path: &str, reason: SkipReason) -> Result<Outcome> {
        self.ledger
            .set_state(path, State::Skipped, Some(reason.as_str().to_string()))
            .await?;
        Ok(Outcome::Skipped)
    }
}

/// Whether the source file must survive until verification.
///
/// VMAF scores the encode against the original, so both have to be present. Every
/// other recipe checks against a digest taken earlier.
fn keeps_source_for_verification(recipe: Recipe) -> bool {
    matches!(recipe, Recipe::Av1 | Recipe::Ffv1)
}

/// Whether the saving clears the threshold that makes a rewrite worth doing.
fn is_worthwhile(input: u64, output: u64) -> bool {
    if input == 0 {
        return false;
    }
    let saved = input.saturating_sub(output) as f64 / input as f64;
    saved >= policy::MIN_GAIN
}

fn extension_of(path: &str) -> &str {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

/// Replaces a path's extension, keeping everything else exactly as it was.
///
/// Operates on the string rather than `Path::set_extension` so that directory
/// separators stay `/` regardless of the host platform: these are remote paths,
/// not local ones.
fn swap_extension(path: &str, new_extension: &str) -> String {
    let (dir, name) = match path.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, path),
    };
    // A leading dot is part of the name, not an extension separator.
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    };
    let renamed = format!("{stem}.{new_extension}");
    match dir {
        Some(dir) => format!("{dir}/{renamed}"),
        None => renamed,
    }
}

/// Reads the first few KiB, for confirming extensions that lie.
async fn read_head(path: &std::path::Path) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; HEAD_BYTES];
    let n = file.read(&mut buf).await?;
    buf.truncate(n);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saving_under_the_threshold_is_not_worth_it() {
        // 3% is the documented floor.
        assert!(is_worthwhile(1000, 970));
        assert!(!is_worthwhile(1000, 971));
        assert!(is_worthwhile(1000, 500));
    }

    #[test]
    fn growth_is_never_worthwhile() {
        assert!(!is_worthwhile(1000, 1000));
        assert!(!is_worthwhile(1000, 2000));
        assert!(!is_worthwhile(0, 0));
    }

    /// VMAF needs the original alongside the encode; everything else compares
    /// against a digest and can drop the source early to save disk.
    #[test]
    fn only_video_recipes_hold_on_to_the_source() {
        assert!(keeps_source_for_verification(Recipe::Av1));
        assert!(keeps_source_for_verification(Recipe::Ffv1));
        assert!(!keeps_source_for_verification(Recipe::JxlFromJpeg));
        assert!(!keeps_source_for_verification(Recipe::Flac));
        assert!(!keeps_source_for_verification(Recipe::TsRemux));
    }

    #[test]
    fn extracts_extensions() {
        assert_eq!(extension_of("a/b/c.JPG"), "JPG");
        assert_eq!(extension_of("noext"), "");
    }

    #[test]
    fn swaps_extensions_without_disturbing_the_path() {
        assert_eq!(swap_extension("photos/shot.jpg", "jxl"), "photos/shot.jxl");
        assert_eq!(swap_extension("shot.JPEG", "jxl"), "shot.jxl");
        assert_eq!(swap_extension("noext", "jxl"), "noext.jxl");
    }

    /// Names with dots in them, and dotfiles, must survive intact.
    #[test]
    fn swapping_handles_awkward_names() {
        assert_eq!(
            swap_extension("a/my.holiday.2019.jpg", "jxl"),
            "a/my.holiday.2019.jxl"
        );
        // A leading dot is part of the name, not an extension marker.
        assert_eq!(swap_extension(".hidden", "jxl"), ".hidden.jxl");
        assert_eq!(swap_extension("dir.with.dots/x.png", "jxl"), "dir.with.dots/x.jxl");
    }

    /// A recipe that keeps the container produces the same path it started from.
    ///
    /// This is not a curiosity: AV1 keeps the source container so QuickTime
    /// metadata survives, so `clip.mp4` converts to `clip.mp4`. Uploading straight
    /// over it and then deleting "the original" deletes the replacement, which is
    /// exactly what happened before the write path staged under a temporary name.
    #[test]
    fn a_same_extension_recipe_leaves_the_path_unchanged() {
        assert_eq!(swap_extension("clip.mp4", "mp4"), "clip.mp4");
        assert_eq!(swap_extension("videos/clip.mov", "mov"), "videos/clip.mov");
        // Which is what the in-place branch keys off.
        assert_eq!(Recipe::Av1.output_extension("mp4"), "mp4");
        assert_ne!(Recipe::TsRemux.output_extension("ts"), "ts");
    }

    /// The staging name must be distinct from the file it will replace, and
    /// recognisable enough to explain itself if a crash leaves one behind.
    #[test]
    fn the_in_place_staging_name_is_distinct_and_self_describing() {
        let path = "videos/clip.mp4";
        let staged = format!("{path}.verified-recompress-{}.part", 7u64);
        assert_ne!(staged, path);
        assert!(staged.starts_with(path));
        assert!(staged.ends_with(".part"));
    }

    #[test]
    fn summary_accumulates() {
        let mut s = Summary::default();
        s.merge(Outcome::Converted {
            input: 1000,
            output: 700,
        });
        s.merge(Outcome::Converted {
            input: 500,
            output: 400,
        });
        s.merge(Outcome::Skipped);
        s.merge(Outcome::Failed);
        assert_eq!(s.converted, 2);
        assert_eq!(s.skipped, 1);
        assert_eq!(s.failed, 1);
        assert_eq!(s.saved_bytes(), 400);
    }
}
