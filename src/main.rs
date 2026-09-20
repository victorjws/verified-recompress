use anyhow::{Context, Result, bail};
use clap::Parser;
use humansize::{DECIMAL, format_size};
use tracing_subscriber::EnvFilter;

use storage_optimizer::cli::{Cli, Command};
use storage_optimizer::config::{self, Config, FileConfig};
use storage_optimizer::ledger::Ledger;
use storage_optimizer::preflight;
use storage_optimizer::remote::{Remote, rcd::RcdRemote};

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
            bail!("`run` is not implemented yet (step 5 of the plan)")
        }
        Command::Plan => bail!("not implemented yet (step 3 of the plan)"),
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
