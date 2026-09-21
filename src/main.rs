use anyhow::{Context, Result, bail};
use clap::Parser;
use humansize::{DECIMAL, format_size};
use tracing_subscriber::EnvFilter;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use storage_optimizer::cli::{Cli, Command, RunArgs};
use storage_optimizer::config::{self, Config, FileConfig, TrashPolicy};
use storage_optimizer::bench;
use storage_optimizer::convert;
use storage_optimizer::governor::Governor;
use storage_optimizer::ledger::{Ledger, State};
use storage_optimizer::pipeline::{self, Pipeline};
use storage_optimizer::policy::{self, Limits, SkipReason};
use storage_optimizer::preflight;
use storage_optimizer::remote::{Remote, rcd::RcdRemote};
use storage_optimizer::dedup;
use storage_optimizer::report::Projection;
use storage_optimizer::restore;
use storage_optimizer::scope::Scope;
use storage_optimizer::staging;
use storage_optimizer::trash;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let config_path = cli
        .config
        .clone()
        .or_else(FileConfig::default_path)
        .context("could not determine a config file location")?;
    let file = FileConfig::load(&config_path)?;

    let staging_dir = cli
        .overrides()
        .staging_dir
        .or_else(|| file.staging_dir.clone())
        .unwrap_or_else(|| std::env::temp_dir().join("storage-optimizer"));
    let available = config::available_space(&staging_dir)?;

    let cfg = Config::resolve(file, cli.overrides(), available)?;

    match &cli.command {
        Command::Preflight => run_preflight(&cfg).await,
        Command::Scan => run_scan(&cfg).await,
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
async fn run_scan(cfg: &Config) -> Result<()> {
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
    for scope in &scopes {
        let label = if scope.is_empty() { "/" } else { scope };
        tracing::info!("listing {label}");
        let entries = remote.list(scope).await?;
        let bytes: u64 = entries.iter().map(|e| e.size).sum();
        tracing::info!(
            "  {} file(s), {}",
            entries.len(),
            format_size(bytes, DECIMAL)
        );
        total += ledger.upsert(entries).await?;
    }

    remote.shutdown().await?;

    let counts = ledger.counts().await?;
    println!("Inventoried {total} file(s) across {} scope(s).", scopes.len());
    println!(
        "  pending {}  done {}  skipped {}  failed {}  total {}",
        counts.pending,
        counts.done,
        counts.skipped,
        counts.failed,
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
    let rows = ledger.list_by_state(State::Pending).await?;
    if rows.is_empty() {
        println!("No pending files. Run `scan` first.");
        return Ok(());
    }

    let limits = Limits {
        max_file_bytes: u64::from(cfg.max_file_mib) * 1024 * 1024,
        // `plan` reports what the lossless tiers alone would do; the AV1 tier needs
        // both a probe and --allow-video, so including it here would overpromise.
        allow_video: false,
    };

    let mut projection = Projection::default();
    for row in &rows {
        let facts = policy::Facts::new(&row.path, row.size);
        projection.record(policy::decide(facts, limits), row.size);
    }

    println!("{} pending file(s) in the inventory.\n", rows.len());
    print!("{projection}");
    Ok(())
}

/// Converts files. Without `--execute` this stops short of touching the remote.
async fn run_convert(cfg: &Config, args: &RunArgs) -> Result<()> {
    let swept = staging::sweep_orphans(&cfg.staging_dir)?;
    if swept > 0 {
        tracing::info!("cleared {swept} staging director(ies) left by a previous run");
    }

    let ledger = Ledger::open(&cfg.staging_dir.join("ledger.sqlite"))?;
    let recovered = ledger.recover_claimed().await?;
    if recovered > 0 {
        tracing::info!("returned {recovered} file(s) stranded by a previous run to pending");
    }

    // Skips caused by this run's settings, rather than by the files themselves,
    // have to be reconsidered when those settings change. Otherwise turning on
    // --allow-video would silently do nothing to files it had already excluded.
    let mut reopen: Vec<&str> = vec![
        SkipReason::TooLargeForBudget.as_str(),
        pipeline::OUT_OF_SCOPE,
    ];
    if args.allow_video {
        reopen.push(SkipReason::VideoTierDisabled.as_str());
    }
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

    tracing::info!(
        "budgets: {} local staging, {} remote headroom, {} cpu permit(s)",
        format_size(u64::from(governor.disk_capacity_mib()) * 1024 * 1024, DECIMAL),
        format_size(u64::from(governor.cloud_capacity_mib()) * 1024 * 1024, DECIMAL),
        cfg.cpu_permits,
    );

    // Ctrl-C stops new work but lets in-flight jobs finish or roll back, so the
    // remote is never left mid-replacement.
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::warn!("interrupt received; finishing in-flight work then stopping");
            signal.cancel();
        }
    });

    let pipeline = Arc::new(Pipeline::new(
        Arc::clone(&remote),
        ledger.clone(),
        Arc::clone(&governor),
        cfg.staging_dir.clone(),
        u64::from(cfg.max_file_mib) * 1024 * 1024,
        cancel,
    ));

    if !args.execute {
        println!("Dry run: nothing on the remote will be written or deleted.\n");
    }

    let summary = pipeline
        .run(pipeline::Options {
            execute: args.execute,
            allow_video: args.allow_video,
            limit: args.limit,
            scope: Scope::new(&cfg.paths, &cfg.exclude)?,
            video: convert::VideoOptions {
                preset: args.preset,
                temporal_filtering_off: false,
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
    println!(
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
    println!("  skipped {}  failed {}", summary.skipped, summary.failed);

    if args.execute && trash::purges_after_run(cfg.trash_policy) {
        println!("\nEmptying the trash as configured (trash_policy = purge_now)...");
        run_cleanup(cfg, true).await?;
    } else if args.execute && cfg.trash_policy == TrashPolicy::Keep {
        println!(
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

    println!(
        "Inventory: {} pending, {} done, {} skipped, {} failed",
        counts.pending, counts.done, counts.skipped, counts.failed
    );
    println!("\nConverted {} file(s)", savings.files);
    println!(
        "  {:>12} in  ->  {:>12} out",
        format_size(savings.original_bytes, DECIMAL),
        format_size(savings.output_bytes, DECIMAL)
    );
    print!(
        "{}",
        trash::Accounting {
            logical: savings.logical_bytes(),
            pending,
            remote_free,
        }
    );
    if let Some(advice) = trash::advice(cfg.trash_policy, pending) {
        println!("\n  {advice}");
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
        println!(
            "Would empty the trash, releasing {} held by {} replaced original(s).",
            format_size(pending.bytes, DECIMAL),
            pending.files
        );
        println!(
            "\n  This is irreversible: once purged, the originals can no longer be \n  \
             restored from the Filen web app. Re-run with --execute to proceed."
        );
        remote.shutdown().await?;
        return Ok(());
    }

    remote.cleanup().await?;
    let reclaimed = ledger.mark_reclaimed().await?;
    let after = remote.about().await.ok().and_then(|a| a.free);
    remote.shutdown().await?;

    println!(
        "Emptied the trash. {} of originals released.",
        format_size(reclaimed, DECIMAL)
    );
    if let (Some(before), Some(after)) = (before, after) {
        let gained = after.saturating_sub(before);
        println!(
            "  Remote free space: {} -> {} ({} recovered)",
            format_size(before, DECIMAL),
            format_size(after, DECIMAL),
            format_size(gained, DECIMAL)
        );
        // A large discrepancy means something else is holding space: an older
        // trash, file versions, or uploads this ledger does not know about.
        if reclaimed > 0 && gained * 2 < reclaimed {
            println!(
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
        .filter(|row| storage_optimizer::classify::kind_from_extension(&row.path).is_video())
        .take(sample)
        .collect();

    if candidates.is_empty() {
        println!("No pending video files to benchmark. Run `scan` first.");
        return Ok(());
    }

    let remote = RcdRemote::spawn(cfg.remote.clone(), None).await?;
    let hwaccel = preflight::has_cuda().await;
    let work = tempfile::tempdir()?;
    let mut report = bench::Report::default();

    for (index, row) in candidates.iter().enumerate() {
        println!("\nSample {}/{}: {}", index + 1, candidates.len(), row.path);
        let local = work.path().join(format!("sample{index}.bin"));
        remote.download(&row.path, &local).await?;

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
    println!("\n{report}");
    println!(
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
        println!("Nothing in the inventory. Run `scan` first.");
        return Ok(());
    }
    print!("{}", dedup::find(&rows));
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
    let records = ledger.completed(sample).await?;
    if records.is_empty() {
        println!("No completed conversions to verify.");
        return Ok(());
    }

    let remote = RcdRemote::spawn(cfg.remote.clone(), None).await?;
    let work = tempfile::tempdir()?;
    let (mut proven, mut present, mut failed) = (0u32, 0u32, 0u32);

    for record in &records {
        match verify_one(&remote, record, work.path()).await {
            Ok(true) => {
                proven += 1;
                println!("  rebuilt  {}", record.path);
            }
            Ok(false) => {
                present += 1;
                println!("  present  {} ({})", record.output_path, record.fidelity);
            }
            Err(e) => {
                failed += 1;
                println!("  FAILED   {}: {e:#}", record.output_path);
            }
        }
    }

    remote.shutdown().await?;
    println!(
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
    record: &storage_optimizer::ledger::Completed,
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
    remote.download(&record.output_path, &converted).await?;
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
    remote.download(&record.output_path, &converted).await?;
    let rebuilt = work
        .path()
        .join(restore::staged_name("rebuilt", &record.path));
    restore::rebuild(&record, &converted, &rebuilt).await?;
    restore::confirm(&record, &rebuilt).await?;

    if !execute {
        println!(
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
    remote.upload(&rebuilt, &record.path).await?;
    remote.shutdown().await?;

    println!(
        "Restored {} from {}. The converted file is still there; remove it yourself \n  \
         once you are satisfied.",
        record.path, record.output_path
    );
    Ok(())
}

async fn run_preflight(cfg: &Config) -> Result<()> {
    let report = preflight::run(cfg).await?;
    print!("{report}");
    if report.has_errors() {
        bail!("preflight failed");
    }
    Ok(())
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "storage_optimizer=info",
        1 => "storage_optimizer=debug",
        _ => "storage_optimizer=trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default)),
        )
        .with_target(false)
        .init();
}
