use anyhow::{Context, Result, bail};
use clap::Parser;
use humansize::{DECIMAL, format_size};
use tracing_subscriber::EnvFilter;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use verified_recompress::cli::{Cli, Command, RunArgs};
use verified_recompress::config::{self, Config, FileConfig, TrashPolicy};
use verified_recompress::bench;
use verified_recompress::convert;
use verified_recompress::governor::Governor;
use verified_recompress::ledger::{Ledger, State};
use verified_recompress::pipeline::{self, Pipeline};
use verified_recompress::policy::{self, Limits, SkipReason};
use verified_recompress::preflight;
use verified_recompress::progress;
use verified_recompress::remote::{Hashes, Remote, rcd::RcdRemote};
use verified_recompress::remote::rcd::JobStats;
use verified_recompress::dedup;
use verified_recompress::report::Projection;
use verified_recompress::restore;
use verified_recompress::scope::Scope;
use verified_recompress::staging;
use verified_recompress::trash;

/// Writes one of this program's results to stdout.
///
/// Results are the data a command exists to produce: a person reads them, a
/// pipe consumes them, and they carry no log decoration. Everything else the
/// tool has to say — progress, the commands it ran, advice, warnings — is a
/// diagnostic and goes to the log on stderr.
macro_rules! emit {
    ($($arg:tt)*) => {
        emit_line(format_args!($($arg)*))
    };
}

/// Steps around the spinner so a result cannot land mid-redraw. The spinner
/// draws on stderr and this writes to stdout, but they share a terminal.
fn emit_line(args: std::fmt::Arguments<'_>) {
    progress::suspend(|| println!("{args}"));
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let config_path = cli
        .config
        .clone()
        .map(config::expand_home)
        .or_else(FileConfig::default_path)
        .context("could not determine a config file location")?;
    let file = FileConfig::load(&config_path)?;

    let staging_dir = cli
        .overrides()
        .staging_dir
        .or_else(|| file.staging_dir.clone())
        .unwrap_or_else(|| std::env::temp_dir().join("verified-recompress"));
    let available = config::available_space(&staging_dir)?;

    let cfg = Config::resolve(file, cli.overrides(), available)?;

    match &cli.command {
        Command::Preflight => run_preflight(&cfg).await,
        Command::Scan { hash } => run_scan(&cfg, *hash).await,
        Command::Run(args) => {
            // Guardrail: a whole-drive write must be asked for explicitly.
            if args.execute && cfg.paths.is_empty() && !args.all {
                bail!(
                    "refusing to modify the entire remote implicitly; pass --path to scope \
                     the run, or --all to confirm you mean everything"
                );
            }
            run_convert(&cfg, args).await
        }
        Command::Plan => run_plan(&cfg).await,
        Command::Bench { sample } => run_bench(&cfg, *sample).await,
        Command::Report => run_report(&cfg).await,
        Command::Verify { sample } => run_verify(&cfg, *sample).await,
        Command::Restore { path, execute } => run_restore(&cfg, path, *execute).await,
        Command::Dedup => run_dedup(&cfg).await,
        Command::Cleanup { execute } => run_cleanup(&cfg, *execute).await,
    }
}

/// Builds the inventory. Reads only: nothing is downloaded and nothing is modified.
async fn run_scan(cfg: &Config, hash: bool) -> Result<()> {
    let ledger = Ledger::open(&cfg.staging_dir.join("ledger.sqlite"))?;
    let recovered = ledger.recover_claimed().await?;
    if recovered > 0 {
        tracing::info!("returned {recovered} file(s) stranded by a previous run to pending");
    }

    let remote = RcdRemote::spawn(cfg.remote.clone(), None).await?;

    // Advisory only: a backend that cannot report quota must not block a scan.
    if let Ok(about) = remote.about().await
        && let (Some(used), Some(total)) = (about.used, about.total)
    {
        tracing::info!(
            "remote usage {} of {}",
            format_size(used, DECIMAL),
            format_size(total, DECIMAL)
        );
    }

    // An empty scope list means the whole remote.
    let scopes: Vec<String> = if cfg.paths.is_empty() {
        vec![String::new()]
    } else {
        cfg.paths.clone()
    };

    let mut total = 0usize;
    let mut removed = 0usize;
    for scope in &scopes {
        let label = if scope.is_empty() { "/" } else { scope };
        // One recursive request covers the whole subtree, so without a counter
        // there is nothing between "listing" and the result but silence.
        let activity = progress::Activity::start(format!("listing {label}"));
        let hashes = if hash { Hashes::Include } else { Hashes::Skip };
        let entries = remote
            .list_progress(scope, hashes, |stats: &JobStats| {
                activity.set(format!(
                    "listed {} entries",
                    progress::thousands(stats.listed)
                ));
            })
            .await?;
        activity.finish();

        let bytes: u64 = entries.iter().map(|e| e.size).sum();
        tracing::info!(
            "  {} file(s), {}",
            progress::thousands(entries.len() as u64),
            format_size(bytes, DECIMAL)
        );
        let synced = ledger.sync(scope, entries).await?;
        total += synced.seen;
        removed += synced.removed;
    }

    remote.shutdown().await?;

    let counts = ledger.counts().await?;
    emit!(
        "Inventoried {} file(s) across {} scope(s).",
        progress::thousands(total as u64),
        scopes.len()
    );
    if removed > 0 {
        emit!(
            "  Dropped {} row(s) for files no longer on the remote.",
            progress::thousands(removed as u64)
        );
    }
    emit!(
        "  pending {}  done {}  skipped {}  failed {}  total {}",
        progress::thousands(counts.pending),
        progress::thousands(counts.done),
        progress::thousands(counts.skipped),
        progress::thousands(counts.failed),
        format_size(counts.total_bytes, DECIMAL)
    );
    Ok(())
}

/// Projects what a run would achieve, from the inventory alone.
///
/// Deliberately does not download anything, which is why video resolves to
/// "needs a probe" rather than a guess: probing a remote file means fetching it.
async fn run_plan(cfg: &Config) -> Result<()> {
    let ledger = Ledger::open(&cfg.staging_dir.join("ledger.sqlite"))?;
    let scope = Scope::new(&cfg.paths, &cfg.exclude)?;
    let mut rows = ledger.list_by_state(State::Pending).await?;
    // `--path` is a global flag, and a projection for one folder is the most
    // natural thing to ask for. Ignoring it here answered a question nobody
    // asked, drive-wide.
    rows.retain(|row| scope.allows(&row.path));
    if rows.is_empty() {
        emit!("No pending files in scope. Run `scan` first.");
        return Ok(());
    }

    let limits = Limits {
        max_file_bytes: u64::from(cfg.max_file_mib) * 1024 * 1024,
        // `plan` reports what the lossless tiers alone would do; the AV1 tier needs
        // both a probe and --allow-video, so including it here would overpromise.
        allow_video: false,
        min_video_secs: cfg.min_video_secs,
    };

    let mut projection = Projection::default();
    for row in &rows {
        let facts = policy::Facts::new(&row.path, row.size);
        projection.record(policy::decide(facts, limits), row.size);
    }

    emit!(
        "{} pending file(s) in the inventory.\n",
        progress::thousands(rows.len() as u64)
    );
    emit!("{}", projection.to_string().trim_end());
    Ok(())
}

/// Converts files. Without `--execute` this stops short of touching the remote.
async fn run_convert(cfg: &Config, args: &RunArgs) -> Result<()> {
    let salvage = staging::salvage_orphans(&cfg.staging_dir)?;
    if salvage.swept > 0 {
        tracing::info!(
            "cleared {} staging director(ies) left by a previous run",
            salvage.swept
        );
    }
    if !salvage.reusable.is_empty() {
        tracing::info!(
            "reusing {} download(s) a previous run had already fetched",
            salvage.reusable.len()
        );
    }

    let ledger = Ledger::open(&cfg.staging_dir.join("ledger.sqlite"))?;
    let recovered = ledger.recover_claimed().await?;
    if recovered > 0 {
        tracing::info!("returned {recovered} file(s) stranded by a previous run to pending");
    }

    // Skips caused by this run's settings, rather than by the files themselves,
    // have to be reconsidered when those settings change. Otherwise turning on
    // --allow-video would silently do nothing to files it had already excluded.
    if args.retry_failed {
        let retried = ledger.retry_failed().await?;
        tracing::info!("returning {retried} previously failed file(s) to pending");
    }

    let mut reopen = SkipReason::reopened_by(args.allow_video);
    reopen.push(pipeline::OUT_OF_SCOPE);
    let reopened = ledger.reopen_skipped(&reopen).await?;
    if reopened > 0 {
        tracing::info!("reconsidering {reopened} file(s) skipped under earlier settings");
    }

    let remote = Arc::new(RcdRemote::spawn(cfg.remote.clone(), None).await?);

    let about = remote.about().await?;
    let free = about
        .free
        .context("the remote did not report free space, so uploads cannot be budgeted")?;
    let governor = Arc::new(Governor::new(cfg, free)?);

    if let Some(dir) = &cfg.keep_converted
        && args.execute
    {
        tracing::info!(
            "keeping converted files under {} (outside the staging budget)",
            dir.display()
        );
    }
    if let Some(dir) = &cfg.keep_originals {
        if args.execute {
            tracing::info!(
                "keeping originals under {} (outside the staging budget)",
                dir.display()
            );
        } else {
            // Saying it unconditionally reads as a promise, and then the
            // directory stays empty because a dry run replaces nothing.
            tracing::info!(
                "keep_originals is set to {}, but nothing is kept on a dry run: \
                 originals are only preserved when one is about to be replaced.",
                dir.display()
            );
        }
    }

    tracing::info!(
        "budgets: {} local staging, {} remote headroom, {} cpu core(s)",
        format_size(u64::from(governor.disk_capacity_mib()) * 1024 * 1024, DECIMAL),
        format_size(u64::from(governor.cloud_capacity_mib()) * 1024 * 1024, DECIMAL),
        cfg.cpu_cores,
    );

    // Ctrl-C stops new work and lets in-flight jobs reach a point where nothing
    // is half-replaced. A second one gives up on that and leaves immediately.
    //
    // The listener has to keep listening. Installing a handler takes SIGINT away
    // from the kernel's default of killing the process, so a task that waits for
    // one signal and then exits leaves the run unable to be interrupted at all.
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        tracing::warn!(
            "interrupt received; finishing in-flight work then stopping. \
             Interrupt again to stop now."
        );
        signal.cancel();

        if tokio::signal::ctrl_c().await.is_ok() {
            // Nothing has been deleted that was not already replaced and
            // confirmed, so leaving here costs the work in flight and no more.
            tracing::warn!("second interrupt; stopping now");
            std::process::exit(130);
        }
    });

    let pipeline = Arc::new(Pipeline::new(
        Arc::clone(&remote),
        ledger.clone(),
        Arc::clone(&governor),
        cfg.staging_dir.clone(),
        u64::from(cfg.max_file_mib) * 1024 * 1024,
        cancel,
        salvage.reusable,
    ));

    if !args.execute {
        tracing::info!("Dry run: nothing on the remote will be written or deleted.\n");
    }

    let summary = pipeline
        .run(pipeline::Options {
            execute: args.execute,
            allow_video: args.allow_video,
            limit: args.limit,
            scope: Scope::new(&cfg.paths, &cfg.exclude)?,
            order: cfg.order,
            // Only on a real run: a dry run leaves the remote original where it
            // is, so there is nothing to preserve it from.
            keep_originals: args.execute.then(|| cfg.keep_originals.clone()).flatten(),
            keep_converted: args.execute.then(|| cfg.keep_converted.clone()).flatten(),
            // Dry runs replace nothing, so there is nothing billed to reclaim.
            reclaim_when_low_mib: args.execute.then_some(cfg.reclaim_when_low_mib).flatten(),
            // One job per core keeps the encoders fed; the network slots on top
            // let that many more be fetching or uploading meanwhile.
            max_in_flight: cfg.cpu_cores + cfg.net_concurrency,
            min_video_secs: cfg.min_video_secs,
            video: convert::VideoOptions {
                preset: args.preset,
                temporal_filtering_off: args.no_temporal_filtering,
                hwaccel: preflight::has_cuda().await,
                allow_discard_corrupt: args.allow_discard_corrupt,
            },
        })
        .await?;

    // Unwrapping the Arc keeps the daemon shutdown explicit rather than leaving it
    // to Drop, which can only kill rather than ask.
    if let Some(remote) = Arc::into_inner(remote) {
        remote.shutdown().await?;
    }

    let verb = if args.execute { "Converted" } else { "Would convert" };
    emit!(
        "{verb} {} file(s): {} -> {}, saving {} ({:.0}%)",
        summary.converted,
        format_size(summary.input_bytes, DECIMAL),
        format_size(summary.output_bytes, DECIMAL),
        format_size(summary.saved_bytes(), DECIMAL),
        if summary.input_bytes == 0 {
            0.0
        } else {
            summary.saved_bytes() as f64 / summary.input_bytes as f64 * 100.0
        },
    );
    emit!(
        "  skipped {}  failed {}",
        progress::thousands(summary.skipped),
        progress::thousands(summary.failed)
    );
    if summary.cancelled > 0 {
        emit!(
            "  {} file(s) were interrupted and are back at pending; nothing was \
             left half-done.",
            progress::thousands(summary.cancelled)
        );
    }

    if args.execute && trash::purges_after_run(cfg.trash_policy) {
        tracing::info!("\nEmptying the trash as configured (trash_policy = purge_now)...");
        run_cleanup(cfg, true).await?;
    } else if args.execute && cfg.trash_policy == TrashPolicy::PurgeAfterDays {
        // All or nothing: rclone cannot purge by age, so the only way to keep
        // the promise that nothing younger than the retention period is
        // destroyed is to wait until nothing is.
        let aged = ledger.pending_reclaim_aged(cfg.purge_after_days).await?;
        if aged.ready() {
            tracing::info!(
                "\nEverything in the trash is older than {} day(s); emptying it.",
                cfg.purge_after_days
            );
            run_cleanup(cfg, true).await?;
        } else if aged.too_recent > 0 {
            tracing::info!(
                "\n  {} of {} original(s) in the trash are newer than {} day(s), so it \n  \
                 stays. Emptying is all or nothing here, so it waits for the \n  \
                 youngest. `cleanup --execute` overrides that.",
                progress::thousands(aged.too_recent),
                progress::thousands(aged.pending.files),
                cfg.purge_after_days
            );
        }
    } else if args.execute && cfg.trash_policy == TrashPolicy::Keep {
        tracing::info!(
            "\n  Originals are in the trash, which still counts against your quota.\n  \
             Check the results, then run `cleanup --execute` to reclaim the space."
        );
    }
    Ok(())
}

/// Shows what the conversions achieved, keeping the three figures apart.
async fn run_report(cfg: &Config) -> Result<()> {
    let ledger = Ledger::open(&cfg.staging_dir.join("ledger.sqlite"))?;
    let counts = ledger.counts().await?;
    let savings = ledger.savings().await?;
    let pending = ledger.pending_reclaim().await?;

    // Quota is nice to have here, not essential; a report should still work when
    // the remote is unreachable.
    let remote_free = match RcdRemote::spawn(cfg.remote.clone(), None).await {
        Ok(remote) => {
            let free = remote.about().await.ok().and_then(|a| a.free);
            let _ = remote.shutdown().await;
            free
        }
        Err(e) => {
            tracing::debug!("could not reach the remote for quota: {e:#}");
            None
        }
    };

    emit!(
        "Inventory: {} pending, {} done, {} skipped, {} failed",
        counts.pending, counts.done, counts.skipped, counts.failed
    );
    emit!("\nConverted {} file(s)", savings.files);
    emit!(
        "  {:>12} in  ->  {:>12} out",
        format_size(savings.original_bytes, DECIMAL),
        format_size(savings.output_bytes, DECIMAL)
    );
    emit!(
        "{}",
        trash::Accounting {
            logical: savings.logical_bytes(),
            pending,
            remote_free,
        }
        .to_string()
        .trim_end()
    );
    if let Some(advice) = trash::advice(cfg.trash_policy, pending) {
        tracing::info!("\n  {advice}");
    }
    Ok(())
}

/// Empties the trash, which is the step that actually shrinks the drive.
async fn run_cleanup(cfg: &Config, execute: bool) -> Result<()> {
    let ledger = Ledger::open(&cfg.staging_dir.join("ledger.sqlite"))?;
    let pending = ledger.pending_reclaim().await?;

    let remote = RcdRemote::spawn(cfg.remote.clone(), None).await?;
    let before = remote.about().await.ok().and_then(|a| a.free);

    if !execute {
        emit!(
            "Would empty the trash, releasing {} held by {} replaced original(s).",
            format_size(pending.bytes, DECIMAL),
            pending.files
        );
        emit!(
            "\n  This is irreversible: once purged, the originals can no longer be \n  \
             restored from the Filen web app. Re-run with --execute to proceed."
        );
        remote.shutdown().await?;
        return Ok(());
    }

    let activity = progress::Activity::start("emptying the trash");
    remote
        .cleanup_progress(|stats| {
            // Filen may or may not account for this file by file. When it does not
            // the counter stays put and the elapsed time carries the message.
            if stats.deletes > 0 {
                activity.set(format!(
                    "{} file(s) removed",
                    progress::thousands(stats.deletes)
                ));
            }
        })
        .await?;
    activity.finish();

    let reclaimed = ledger.mark_reclaimed().await?;
    let after = remote.about().await.ok().and_then(|a| a.free);
    remote.shutdown().await?;

    emit!(
        "Emptied the trash. {} of originals released.",
        format_size(reclaimed, DECIMAL)
    );
    if let (Some(before), Some(after)) = (before, after) {
        let gained = after.saturating_sub(before);
        emit!(
            "  Remote free space: {} -> {} ({} recovered)",
            format_size(before, DECIMAL),
            format_size(after, DECIMAL),
            format_size(gained, DECIMAL)
        );
        // A large discrepancy means something else is holding space: an older
        // trash, file versions, or uploads this ledger does not know about.
        if reclaimed > 0 && gained * 2 < reclaimed {
            emit!(
                "\n  The drive freed noticeably less than the ledger expected. \n  \
                 Other things may be occupying the trash, or old file versions may \n  \
                 be retained separately."
            );
        }
    }
    Ok(())
}

/// Encodes real samples at several settings so the choice is made on evidence.
async fn run_bench(cfg: &Config, sample: usize) -> Result<()> {
    let ledger = Ledger::open(&cfg.staging_dir.join("ledger.sqlite"))?;
    let scope = Scope::new(&cfg.paths, &cfg.exclude)?;

    // Biggest first: those are the files the settings actually matter for.
    let candidates: Vec<_> = ledger
        .list_by_state(State::Pending)
        .await?
        .into_iter()
        .filter(|row| scope.allows(&row.path))
        .filter(|row| verified_recompress::classify::kind_from_extension(&row.path).is_video())
        .take(sample)
        .collect();

    if candidates.is_empty() {
        emit!("No pending video files to benchmark. Run `scan` first.");
        return Ok(());
    }

    let remote = RcdRemote::spawn(cfg.remote.clone(), None).await?;
    let hwaccel = preflight::has_cuda().await;
    let work = tempfile::tempdir()?;
    let mut report = bench::Report::default();

    for (index, row) in candidates.iter().enumerate() {
        tracing::info!("\nSample {}/{}: {}", index + 1, candidates.len(), row.path);
        let local = work.path().join(format!("sample{index}.bin"));
        remote.download(&row.path, &local, |_| {}).await?;

        for preset in bench::PRESETS {
            match bench::measure_one(&local, work.path(), preset, true, 26, &[], hwaccel).await {
                Ok(row) => report.rows.push(row),
                Err(e) => tracing::warn!("preset {preset} failed: {e:#}"),
            }
        }
        // Temporal filtering trades detail for bytes, and VMAF is not good at
        // spotting the difference, so it is measured rather than assumed.
        match bench::measure_one(&local, work.path(), bench::PRESETS[0], false, 26, &[], hwaccel).await {
            Ok(row) => report.rows.push(row),
            Err(e) => tracing::warn!("temporal-filtering comparison failed: {e:#}"),
        }
        let _ = tokio::fs::remove_file(&local).await;
    }

    remote.shutdown().await?;
    emit!("\n{report}");
    tracing::info!(
        "  VMAF alone cannot see oversmoothing. Before settling on a preset, pull a\n           few frames from each and look at them, and diff the metadata with:\n             exiftool -a -G1 <original> <converted>\n           Tags worth checking: {}",
        bench::tracked_tags().join(", ")
    );
    Ok(())
}

/// Reports files stored more than once, from the inventory alone.
async fn run_dedup(cfg: &Config) -> Result<()> {
    let ledger = Ledger::open(&cfg.staging_dir.join("ledger.sqlite"))?;
    let scope = Scope::new(&cfg.paths, &cfg.exclude)?;

    let mut rows = Vec::new();
    for state in [State::Pending, State::Done, State::Skipped, State::Failed] {
        rows.extend(ledger.list_by_state(state).await?);
    }
    rows.retain(|row| scope.allows(&row.path));

    if rows.is_empty() {
        emit!("Nothing in the inventory. Run `scan` first.");
        return Ok(());
    }
    emit!("{}", dedup::find(&rows).to_string().trim_end());
    Ok(())
}

/// Re-checks conversions that have already happened.
///
/// For the byte-exact ones this is the real thing: the converted file is fetched
/// and rebuilt into its original, then compared with the hash recorded before
/// anything was replaced. For the rest it confirms the replacement is still
/// present and the right size, and says so rather than implying more.
async fn run_verify(cfg: &Config, sample: Option<usize>) -> Result<()> {
    let ledger = Ledger::open(&cfg.staging_dir.join("ledger.sqlite"))?;
    let scope = Scope::new(&cfg.paths, &cfg.exclude)?;
    // Scoped before the sample is taken, so `--sample 5 --path X` checks five
    // conversions under X rather than whatever five of the drive happened to
    // fall in it.
    let mut records = ledger.completed(None).await?;
    records.retain(|record| scope.allows(&record.path));
    if let Some(n) = sample {
        records.truncate(n);
    }
    if records.is_empty() {
        emit!("No completed conversions to verify in scope.");
        return Ok(());
    }

    let remote = RcdRemote::spawn(cfg.remote.clone(), None).await?;
    let work = tempfile::tempdir()?;
    let (mut proven, mut present, mut failed) = (0u32, 0u32, 0u32);

    for record in &records {
        match verify_one(&remote, record, work.path()).await {
            Ok(true) => {
                proven += 1;
                emit!("  rebuilt  {}", record.path);
            }
            Ok(false) => {
                present += 1;
                emit!("  present  {} ({})", record.output_path, record.fidelity);
            }
            Err(e) => {
                failed += 1;
                emit!("  FAILED   {}: {e:#}", record.output_path);
            }
        }
    }

    remote.shutdown().await?;
    emit!(
        "\n{proven} rebuilt to the original bytes, {present} confirmed present, {failed} failed."
    );
    if failed > 0 {
        bail!("{failed} conversion(s) did not verify");
    }
    Ok(())
}

/// Returns whether the original was actually rebuilt, as opposed to merely found.
async fn verify_one(
    remote: &RcdRemote,
    record: &verified_recompress::ledger::Completed,
    work: &std::path::Path,
) -> Result<bool> {
    let Some(entry) = remote.stat(&record.output_path).await? else {
        bail!("missing from the remote");
    };
    if entry.size != record.output_size {
        bail!(
            "is {} bytes, the ledger recorded {}",
            entry.size,
            record.output_size
        );
    }
    if !restore::is_restorable(record) {
        return Ok(false);
    }

    let converted = work.join(restore::staged_name("converted", &record.output_path));
    remote.download(&record.output_path, &converted, |_| {}).await?;
    let rebuilt = work.join(restore::staged_name("rebuilt", &record.path));
    restore::rebuild(record, &converted, &rebuilt).await?;
    restore::confirm(record, &rebuilt).await?;
    let _ = tokio::fs::remove_file(&converted).await;
    let _ = tokio::fs::remove_file(&rebuilt).await;
    Ok(true)
}

/// Rebuilds one original and puts it back.
async fn run_restore(cfg: &Config, path: &str, execute: bool) -> Result<()> {
    let ledger = Ledger::open(&cfg.staging_dir.join("ledger.sqlite"))?;
    let Some(record) = ledger.completed_for(path).await? else {
        bail!("no recorded conversion for `{path}`");
    };
    if !restore::is_restorable(&record) {
        bail!("{}", restore::refusal(&record));
    }

    let remote = RcdRemote::spawn(cfg.remote.clone(), None).await?;
    let work = tempfile::tempdir()?;

    let converted = work
        .path()
        .join(restore::staged_name("converted", &record.output_path));
    remote.download(&record.output_path, &converted, |_| {}).await?;
    let rebuilt = work
        .path()
        .join(restore::staged_name("rebuilt", &record.path));
    restore::rebuild(&record, &converted, &rebuilt).await?;
    restore::confirm(&record, &rebuilt).await?;

    if !execute {
        emit!(
            "{} rebuilds from {} exactly ({} bytes).\n\n  \
             Re-run with --execute to put it back; the converted file is left in place.",
            record.path, record.output_path, record.original_size
        );
        remote.shutdown().await?;
        return Ok(());
    }

    if remote.stat(&record.path).await?.is_some() {
        remote.shutdown().await?;
        bail!("{} already exists; not overwriting it", record.path);
    }
    remote.upload(&rebuilt, &record.path, |_| {}).await?;
    remote.shutdown().await?;

    emit!(
        "Restored {} from {}. The converted file is still there; remove it yourself \n  \
         once you are satisfied.",
        record.path, record.output_path
    );
    Ok(())
}

async fn run_preflight(cfg: &Config) -> Result<()> {
    let report = preflight::run(cfg).await?;
    emit!("{}", report.to_string().trim_end());
    if report.has_errors() {
        bail!("preflight failed");
    }
    Ok(())
}

/// Writes log lines to stderr, stepping around the spinner if one is up.
///
/// Everything this tool says goes through `tracing`, including results, so the
/// stream has to be the one the spinner draws on or a redirect would capture
/// half the run. That makes collisions possible — under `-vv` the poll loop
/// traces a request every 20ms while the spinner redraws every 100ms — so each
/// write clears the spinner first.
struct LogWriter;

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        progress::suspend(|| std::io::stderr().write(buf))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        Self
    }
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "verified_recompress=info",
        1 => "verified_recompress=debug",
        _ => "verified_recompress=trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default)),
        )
        .with_target(false)
        .with_writer(LogWriter)
        .init();
}
