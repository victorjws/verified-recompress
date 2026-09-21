use anyhow::{Context, Result, bail};
use clap::Parser;
use humansize::{DECIMAL, format_size};
use tracing_subscriber::EnvFilter;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use storage_optimizer::cli::{Cli, Command, RunArgs};
use storage_optimizer::config::{self, Config, FileConfig, TrashPolicy};
use storage_optimizer::governor::Governor;
use storage_optimizer::ledger::{Ledger, State};
use storage_optimizer::pipeline::{self, Pipeline};
use storage_optimizer::policy::{self, Limits, SkipReason};
use storage_optimizer::preflight;
use storage_optimizer::remote::{Remote, rcd::RcdRemote};
use storage_optimizer::report::Projection;
use storage_optimizer::staging;

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
        Command::Bench { .. } => bail!("not implemented yet (step 7 of the plan)"),
        Command::Report | Command::Verify { .. } | Command::Restore { .. } => {
            bail!("not implemented yet (step 6-8 of the plan)")
        }
        Command::Cleanup { .. } => bail!("not implemented yet (step 6 of the plan)"),
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
    let mut reopen: Vec<&str> = vec![SkipReason::TooLargeForBudget.as_str()];
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

    if args.execute && cfg.trash_policy == TrashPolicy::Keep {
        println!(
            "\n  Originals are in the trash, which still counts against your quota.\n  \
             Check the results, then run `cleanup --execute` to reclaim the space."
        );
    }
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
