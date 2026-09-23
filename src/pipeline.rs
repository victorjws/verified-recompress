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

use std::path::Path;
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
use crate::staging::{self, Workspace};

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
    /// Where to leave a copy of each original before its replacement takes over.
    pub keep_originals: Option<std::path::PathBuf>,
    /// Empty the trash mid-run once remote free space falls below this many MiB.
    pub reclaim_when_low_mib: Option<u32>,
    /// How many files may be in flight at once.
    ///
    /// Enough to keep every budget busy and no more. Claiming past that point
    /// starts no work sooner — the job queues on a semaphore either way — but it
    /// does take a ledger row, a disk reservation and a line on the display for
    /// work that cannot begin. On a drive of small images the claim loop will
    /// otherwise spawn a task per file, all at once.
    pub max_in_flight: usize,
    /// Duration floor for the AV1 tier, in seconds. Zero converts every length.
    pub min_video_secs: f64,
    /// Encoder settings the video recipes need.
    pub video: convert::VideoOptions,
}

pub struct Pipeline<R: Remote + 'static> {
    board: Arc<crate::progress::Board>,
    remote: Arc<R>,
    ledger: Ledger,
    governor: Arc<Governor>,
    staging_dir: std::path::PathBuf,
    max_file_bytes: u64,
    cancel: CancellationToken,
    /// Downloads a previous run already fetched, by remote path. Each is used at
    /// most once; taking it out is what stops two jobs adopting the same one.
    salvaged: std::sync::Mutex<std::collections::HashMap<String, std::path::PathBuf>>,
    /// Serialises mid-run trash emptying. Every job in flight notices the
    /// shortage at about the same moment, and one purge answers all of them.
    reclaiming: tokio::sync::Mutex<()>,
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
        salvaged: std::collections::HashMap<String, std::path::PathBuf>,
    ) -> Self {
        Self {
            board: Arc::new(crate::progress::Board::start(None)),
            remote,
            ledger,
            governor,
            staging_dir,
            max_file_bytes,
            cancel,
            salvaged: std::sync::Mutex::new(salvaged),
            reclaiming: tokio::sync::Mutex::new(()),
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

            // Wait for room before claiming anything, so a file is only taken
            // out of the ledger when there is somewhere for it to go.
            while tasks.len() >= opts.max_in_flight {
                match tasks.join_next().await {
                    Some(done) => {
                        summary.merge(done.unwrap_or(Outcome::Failed));
                        self.show(&summary, started);
                    }
                    None => break,
                }
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
            // Claiming changes the denominator, so the summary is restated here
            // as well as on completion; otherwise it reads 0/0 until the first
            // file lands, which on a long video is a very long time.
            self.show(&summary, started);

            // Collect anything that finished while this one was being claimed.
            while let Some(done) = tasks.try_join_next() {
                summary.merge(done.unwrap_or(Outcome::Failed));
                self.show(&summary, started);
            }
        }

        while let Some(done) = tasks.join_next().await {
            summary.merge(done.unwrap_or(Outcome::Failed));
            self.show(&summary, started);
        }
        self.board.finish();
        Ok(summary)
    }

    /// Empties the trash when the remote budget has run down past `floor`.
    ///
    /// Replaced originals stay billed until the trash goes, so a long run spends
    /// quota it has already earned back. Emptying it mid-run returns that space
    /// and lets the run continue instead of stalling on a budget that is only
    /// notionally full.
    ///
    /// Irreversible, which is why it is opt-in and why the run must also be
    /// keeping originals locally: this is the point past which a replaced file
    /// cannot be recovered from the remote at all.
    async fn reclaim_if_low(&self, floor: u32, slot: &crate::progress::Slot) {
        if self.governor.cloud_available_mib() >= floor {
            return;
        }
        // One purge answers every job that noticed the shortage together.
        let _one_at_a_time = self.reclaiming.lock().await;
        if self.governor.cloud_available_mib() >= floor {
            return;
        }

        slot.stage("emptying the trash");
        tracing::info!(
            "remote budget down to {} MiB; emptying the trash",
            self.governor.cloud_available_mib()
        );
        if let Err(e) = self.remote.cleanup().await {
            // Not fatal: the run carries on and blocks on the budget instead,
            // which is the behaviour it would have had anyway.
            tracing::warn!("could not empty the trash: {e:#}");
            return;
        }
        match self.ledger.mark_reclaimed().await {
            Ok(bytes) => {
                let mib = governor::bytes_to_mib(bytes);
                self.governor.release_cloud(mib);
                tracing::info!(
                    "reclaimed {}; remote budget returned",
                    humansize::format_size(bytes, humansize::DECIMAL)
                );
            }
            Err(e) => tracing::warn!("trash emptied but the ledger did not record it: {e:#}"),
        }
    }

    /// Takes a salvaged download for `path`, if one is there and still current.
    ///
    /// Removed from the index whether or not it matches: a stale directory is
    /// not going to become current, and leaving it would keep it from being
    /// swept at the end of the run.
    fn take_salvaged(
        &self,
        path: &str,
        claimed: &staging::Claimed,
    ) -> Option<std::path::PathBuf> {
        let dir = self.salvaged.lock().ok()?.remove(path)?;
        match staging::read_claimed(&dir) {
            Some(found) if staging::still_matches(&found, claimed.size, claimed.mod_time.as_deref()) => {
                Some(dir)
            }
            _ => {
                // The remote has moved on since that download. Nothing here is
                // usable, and it is still occupying the staging budget.
                let _ = std::fs::remove_dir_all(&dir);
                None
            }
        }
    }

    /// Restates the run as a whole, so a file-level line is never the only thing
    /// on screen.
    fn show(&self, summary: &Summary, started: usize) {
        let done = summary.converted + summary.skipped + summary.failed;
        let text = format!(
            "converting {}/{} · saved {}",
            crate::progress::thousands(done),
            crate::progress::thousands(started as u64),
            humansize::format_size(summary.saved_bytes(), humansize::DECIMAL),
        );
        if self.board.is_hidden() {
            if self.board.due_to_log(std::time::Instant::now()) {
                tracing::info!("{text}");
            }
            return;
        }
        self.board.summarise(text);
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
        let slot = self.board.slot(short_name(&row.path));
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
        let reservation =
            governor::reservation_mib(provisional, row.size, opts.keep_originals.is_some());
        if reservation > self.governor.disk_capacity_mib() {
            return self.record_skip(&row.path, SkipReason::TooLargeForBudget).await;
        }
        // The first thing a claimed file does is queue for staging space, and
        // with a full budget it can sit here a long while before anything
        // happens to it at all.
        slot.stage(format!("waiting for {reservation} MiB of staging"));
        let _disk = self.governor.disk(reservation).await?;

        let job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
        let claimed = staging::Claimed {
            path: row.path.clone(),
            size: row.size,
            mod_time: row.mod_time.clone(),
        };

        // A download this file already has from an interrupted run is worth more
        // than anything else here: it may be gigabytes, and fetching it again is
        // the most expensive thing a retry can do.
        let workspace = match self.take_salvaged(&row.path, &claimed) {
            Some(dir) => Workspace::adopt(&self.staging_dir, job_id, &dir)?,
            None => Workspace::create(&self.staging_dir, job_id)?,
        };
        let input = workspace.input(&row.path);
        // `adopt` moved the directory whole, so the download is there or it was
        // never salvaged; either way the file on disk is the authority.
        let reused = input.exists();
        if !reused {
            // Before the bytes, so a process killed mid-download still leaves
            // something that says what the directory was for.
            workspace.claim(&claimed)?;
        }

        if reused {
            slot.stage("reusing download");
            tracing::debug!("{}: reusing the download from a previous run", row.path);
        } else {
            slot.stage("downloading");
            let _net = self.governor.network().await;
            self.remote
                .download(&row.path, &input, |t| {
                    slot.detail(describe_transfer("downloading", t, row.size))
                })
                .await?;
        }

        // Hash the original now, while it is certainly still on disk: some
        // recipes delete it before verification. This is what a restore is
        // checked against, and taking it from the bytes that were actually
        // converted beats trusting whatever a listing reported.
        let original_hash = crate::hash::blake3_file(&input).await?;

        slot.stage("inspecting");
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
            slot.stage("waiting for cores");
            let cpu = self.governor.cpu(recipe).await;
            let duration = probe.duration_secs;
            // Only for the preset it was found at; a different one puts the
            // encoder on a different curve entirely.
            let hint = row
                .crf_hint
                .filter(|_| row.crf_hint_preset == Some(opts.video.preset_used()));
            let (fidelity, attempt) = convert::convert_av1(
                convert::Workbench {
                    input: &input,
                    output: &output,
                    work_dir: workspace.path(),
                    cores: cpu.cores(),
                },
                probe,
                opts.video,
                hint,
                &mut |stage| slot.detail(describe_stage(stage, duration)),
            )
            .await?;

            // Recorded as soon as it is known, so an interrupted run still
            // saves the next one the search.
            if hint != Some(attempt.crf) {
                let _ = self
                    .ledger
                    .record_crf_hint(&row.path, attempt.crf, opts.video.preset_used())
                    .await;
            }
            tracing::info!("{}: crf {}, {}", row.path, attempt.crf, attempt.scores.summary());
            fidelity
        } else {
            slot.stage("fingerprinting");
            let fingerprint = convert::fingerprint(recipe, &input).await?;
            slot.stage("waiting for cores");
            {
                let cpu = self.governor.cpu(recipe).await;
                slot.stage(format!("encoding ({})", recipe.as_str()));
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
            // Kept when the run was asked to preserve originals: the copy is
            // taken below, once the conversion has proven itself.
            if !keeps_source_for_verification(recipe) && opts.keep_originals.is_none() {
                let _ = tokio::fs::remove_file(&input).await;
            }

            slot.stage("waiting for cores");
            let cpu = self.governor.cpu(recipe).await;
            slot.stage("verifying");
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

        // Before anything on the remote changes. A failure to keep the original
        // must not leave a run that has already replaced it, which is the one
        // outcome this option exists to prevent.
        if let Some(dir) = &opts.keep_originals {
            slot.stage("keeping the original");
            keep_original(&input, dir, &row.path)
                .await
                .with_context(|| format!("could not keep a copy of {}", row.path))?;
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
            // The API budget is small (four by default), so with many files in
            // flight most of them queue here. Saying so is the difference
            // between "waiting its turn" and "stuck".
            slot.stage("waiting for an api slot");
            let _api = self.governor.api().await;
            slot.stage("checking the destination");
            if self.remote.stat(&remote_output).await?.is_some() {
                bail!("{remote_output} already exists; refusing to overwrite it");
            }
        }

        // Hold remote quota for the upload. It is only released once the trash has
        // actually been emptied, because until then the original is still billed.
        let cloud_mib = governor::bytes_to_mib(output_bytes);
        if let Some(floor) = opts.reclaim_when_low_mib {
            self.reclaim_if_low(floor, &slot).await;
        }
        // This one can wait forever. Permits are held past the end of the job
        // that took them, because a replaced original stays billed until the
        // trash goes, so the budget only ever shrinks during a run. Once it is
        // spent every remaining file waits here until something empties the
        // trash — which is what `reclaim_when_low_gb` is for.
        let available = self.governor.cloud_available_mib();
        if available < cloud_mib {
            slot.stage(format!(
                "waiting for remote quota ({cloud_mib} MiB needed, {available} MiB left)"
            ));
        }
        let cloud = self.governor.cloud(cloud_mib).await?;

        slot.stage("hashing the result");
        let local_hash = crate::hash::blake3_file(&output).await?;
        slot.stage("uploading");
        {
            let _net = self.governor.network().await;
            self.remote
                .upload(&output, &upload_target, |t| {
                    slot.detail(describe_transfer("uploading", t, output_bytes))
                })
                .await?;
        }

        slot.stage("confirming");
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
/// Puts the original under `dir`, keeping its remote path so two files with the
/// same name do not collide.
///
/// Moved rather than copied where the filesystem allows it: the staging copy is
/// finished with by this point, and a rename costs nothing. Across filesystems
/// there is no choice but to copy.
async fn keep_original(input: &Path, dir: &Path, remote_path: &str) -> Result<()> {
    let target = dir.join(safe_relative(remote_path)?);
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    if tokio::fs::rename(input, &target).await.is_ok() {
        return Ok(());
    }
    tokio::fs::copy(input, &target)
        .await
        .with_context(|| format!("failed to write {}", target.display()))?;
    Ok(())
}

/// Reduces a remote path to something that can only land under the directory it
/// is joined to.
///
/// `Path::join` replaces the whole path when given an absolute one, so a single
/// leading slash would write the original to the filesystem root instead — and
/// silently, since the write itself would succeed. `..` would climb out the same
/// way.
fn safe_relative(remote_path: &str) -> Result<std::path::PathBuf> {
    use std::path::Component;

    let mut out = std::path::PathBuf::new();
    for part in Path::new(remote_path).components() {
        match part {
            Component::Normal(name) => out.push(name),
            // A leading slash or drive letter is dropped; the path stays relative.
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => bail!("refusing to keep `{remote_path}` outside the directory"),
        }
    }
    if out.as_os_str().is_empty() {
        bail!("`{remote_path}` has no file name to keep it under");
    }
    Ok(out)
}

/// The last path segment, which is what identifies a file at a glance. Full
/// paths are long enough to push everything else off the line.
fn short_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Turns an AV1 stage into a line. Video is the reason this exists: one file can
/// sit in a single stage for hours, so the stage has to say which of several it
/// is, and how far through.
fn describe_stage(stage: convert::video_av1::Stage, duration_secs: Option<f64>) -> String {
    use convert::video_av1::{ATTEMPTS, Stage};
    match stage {
        Stage::CrfSearch => "av1 crf-search".to_string(),
        Stage::Encoding { attempt, crf, tick } => {
            let head = format!("av1 encode {attempt}/{ATTEMPTS} crf {crf}");
            match tick.and_then(|t| {
                Some((t.fraction(duration_secs)?, t.eta_secs(duration_secs)))
            }) {
                Some((done, eta)) => match eta {
                    Some(eta) => format!(
                        "{head}  {:.0}%  ETA {}",
                        done * 100.0,
                        short_duration(eta)
                    ),
                    None => format!("{head}  {:.0}%", done * 100.0),
                },
                None => head,
            }
        }
        Stage::Scoring { attempt } => format!("av1 scoring {attempt}/{ATTEMPTS}"),
    }
}

/// Renders a transfer, falling back to the size we already know when the
/// backend does not report a total of its own.
fn describe_transfer(verb: &str, t: crate::remote::Transfer, expected: u64) -> String {
    use humansize::{DECIMAL, format_size};
    let total = if t.total_bytes > 0 { t.total_bytes } else { expected };
    if t.bytes == 0 {
        return verb.to_string();
    }
    let rate = if t.speed > 0.0 {
        format!("  {}/s", format_size(t.speed as u64, DECIMAL))
    } else {
        String::new()
    };
    format!(
        "{verb} {}/{}{rate}",
        format_size(t.bytes, DECIMAL),
        format_size(total, DECIMAL)
    )
}

/// Compact enough to sit at the end of a line that already has a filename on it.
fn short_duration(secs: f64) -> String {
    let secs = secs.max(0.0) as u64;
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, _) => format!("{h}h{m:02}m"),
    }
}

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

    /// `Path::join` throws away everything before an absolute path, so a single
    /// leading slash would put the original at the filesystem root instead of
    /// under the chosen directory — and the write would succeed, so the only
    /// symptom is a directory that never fills up.
    #[test]
    fn a_kept_original_cannot_escape_its_directory() {
        assert_eq!(
            safe_relative("Photos/2019/a.jpg").unwrap(),
            std::path::Path::new("Photos/2019/a.jpg")
        );
        assert_eq!(
            safe_relative("/Photos/a.jpg").unwrap(),
            std::path::Path::new("Photos/a.jpg"),
            "an absolute path is made relative, not honoured"
        );
        assert_eq!(
            safe_relative("./a.jpg").unwrap(),
            std::path::Path::new("a.jpg")
        );

        for bad in ["../a.jpg", "Photos/../../a.jpg", "/", ""] {
            assert!(safe_relative(bad).is_err(), "{bad} should be refused");
        }
    }

    /// The remote path is kept so two originals with the same name do not
    /// overwrite each other, which is the whole point of preserving them.
    #[tokio::test]
    async fn keeping_an_original_mirrors_the_remote_path() {
        let work = tempfile::tempdir().unwrap();
        let keep = tempfile::tempdir().unwrap();
        let input = work.path().join("input.jpg");
        std::fs::write(&input, b"original bytes").unwrap();

        keep_original(&input, keep.path(), "Photos/2019/summer/IMG_0421.jpg")
            .await
            .unwrap();

        let kept = keep.path().join("Photos/2019/summer/IMG_0421.jpg");
        assert_eq!(std::fs::read(&kept).unwrap(), b"original bytes");

        // A second file of the same name from a different folder must survive
        // alongside the first.
        let other = work.path().join("other.jpg");
        std::fs::write(&other, b"different bytes").unwrap();
        keep_original(&other, keep.path(), "Photos/2020/IMG_0421.jpg")
            .await
            .unwrap();
        assert_eq!(std::fs::read(&kept).unwrap(), b"original bytes");
        assert_eq!(
            std::fs::read(keep.path().join("Photos/2020/IMG_0421.jpg")).unwrap(),
            b"different bytes"
        );
    }

    #[test]
    fn a_line_is_labelled_by_the_file_not_the_path() {
        assert_eq!(short_name("Photos/2019/summer/IMG_0421.MOV"), "IMG_0421.MOV");
        assert_eq!(short_name("top.jpg"), "top.jpg");
        assert_eq!(short_name(""), "");
    }

    #[test]
    fn durations_stay_short_enough_to_sit_on_one_line() {
        assert_eq!(short_duration(0.0), "0s");
        assert_eq!(short_duration(45.0), "45s");
        assert_eq!(short_duration(200.0), "3m20s");
        assert_eq!(short_duration(3_600.0), "1h00m");
        assert_eq!(short_duration(7_845.0), "2h10m");
        // Never a negative reading, whatever the encoder claims.
        assert_eq!(short_duration(-5.0), "0s");
    }

    /// The stage has to say which attempt it is on. An AV1 file can spend hours
    /// in each of three rounds, and "still encoding" is not the same news as
    /// "still encoding, on the last try".
    #[test]
    fn av1_stages_name_the_attempt() {
        use convert::video_av1::Stage;

        assert_eq!(describe_stage(Stage::CrfSearch, Some(120.0)), "av1 crf-search");
        assert_eq!(
            describe_stage(Stage::Scoring { attempt: 2 }, Some(120.0)),
            "av1 scoring 2/3"
        );

        let bare = describe_stage(
            Stage::Encoding {
                attempt: 1,
                crf: 27,
                tick: None,
            },
            Some(120.0),
        );
        assert_eq!(bare, "av1 encode 1/3 crf 27");
    }

    #[test]
    fn an_encoding_stage_carries_its_progress() {
        use convert::video_av1::Stage;
        let tick = convert::Tick {
            out_time_us: 30_000_000,
            speed: 2.0,
        };
        let line = describe_stage(
            Stage::Encoding {
                attempt: 2,
                crf: 25,
                tick: Some(tick),
            },
            Some(120.0),
        );
        assert_eq!(line, "av1 encode 2/3 crf 25  25%  ETA 45s");
    }

    /// Without a duration there is no percentage to show, and inventing one
    /// would be worse than the plain stage name.
    #[test]
    fn an_unknown_duration_shows_no_percentage() {
        use convert::video_av1::Stage;
        let line = describe_stage(
            Stage::Encoding {
                attempt: 1,
                crf: 27,
                tick: Some(convert::Tick {
                    out_time_us: 30_000_000,
                    speed: 2.0,
                }),
            },
            None,
        );
        assert_eq!(line, "av1 encode 1/3 crf 27");
    }

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
