mod cli;
mod config;
mod preflight;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};
use crate::config::{Config, FileConfig};

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
        Command::Scan | Command::Plan => bail!("not implemented yet (step 2-3 of the plan)"),
        Command::Bench { .. } => bail!("not implemented yet (step 7 of the plan)"),
        Command::Report | Command::Verify { .. } | Command::Restore { .. } => {
            bail!("not implemented yet (step 6-8 of the plan)")
        }
        Command::Cleanup { .. } => bail!("not implemented yet (step 6 of the plan)"),
    }
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
