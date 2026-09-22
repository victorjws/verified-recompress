//! Command-line surface.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::{Order, Overrides, TrashPolicy};

#[derive(Debug, Parser)]
#[command(
    name = "verified-recompress",
    version,
    about = "Re-encode Filen cloud files to more efficient formats without losing data"
)]
pub struct Cli {
    /// Config file path (default: ~/.config/verified-recompress/config.toml).
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// rclone remote to operate on, e.g. `filen:`.
    #[arg(long, global = true, value_name = "REMOTE")]
    pub remote: Option<String>,

    /// Remote path to scope to. Repeatable. Replaces `paths` from the config file.
    #[arg(long, global = true, value_name = "PATH")]
    pub path: Vec<String>,

    /// Glob to exclude. Repeatable. Added to `exclude` from the config file.
    #[arg(long, global = true, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Increase log verbosity (repeatable).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check that every external tool exists and supports the flags we use.
    Preflight,

    /// Build the remote inventory into the ledger. Downloads nothing.
    Scan,

    /// Assign recipes and report projected savings. Read-only.
    Plan,

    /// Measure encoder settings on real sample files before committing to them.
    Bench {
        /// Number of sample files to encode.
        #[arg(long, default_value_t = 3)]
        sample: usize,
    },

    /// Convert files. Without --execute this is a dry run.
    Run(RunArgs),

    /// Show logical savings, actual quota change, and trash pending.
    Report,

    /// Re-verify already-converted files.
    Verify {
        /// Check this many random conversions.
        #[arg(long)]
        sample: Option<usize>,
    },

    /// Restore an original from its converted form. Byte-reversible classes only.
    Restore {
        /// Either the original's path or the converted file's.
        path: String,
        /// Actually write the restored file back to the remote.
        #[arg(long)]
        execute: bool,
    },

    /// Report files stored more than once. Read-only; deletes nothing.
    Dedup,

    /// Empty the trash so freed space is actually reclaimed.
    Cleanup {
        /// Without this flag, only report what would be purged.
        #[arg(long)]
        execute: bool,
    },
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Actually modify the remote. Without it, nothing is written.
    #[arg(long)]
    pub execute: bool,

    /// Operate on the whole remote. Required if --path is not given with --execute.
    #[arg(long)]
    pub all: bool,

    /// Allow the AV1 video tier, which is not reversible.
    #[arg(long)]
    pub allow_video: bool,

    /// Stop after this many files.
    #[arg(long)]
    pub limit: Option<usize>,

    /// Local scratch directory.
    #[arg(long, value_name = "DIR")]
    pub staging_dir: Option<PathBuf>,

    /// Local staging budget in GB.
    #[arg(long, value_name = "GB")]
    pub budget_gb: Option<u64>,

    /// Skip files whose staging reservation would exceed this, in GB.
    #[arg(long, value_name = "GB")]
    pub max_file_gb: Option<u64>,

    /// Keep this much cloud quota free as a safety margin, in GB.
    #[arg(long, value_name = "GB")]
    pub cloud_reserve_gb: Option<u64>,

    /// Auto-purge trash when cloud free space drops below this, in GB. 0 disables.
    #[arg(long, value_name = "GB")]
    pub reclaim_when_low_gb: Option<u64>,

    /// Concurrent uploads and downloads.
    #[arg(long, value_name = "N")]
    pub net_concurrency: Option<usize>,

    /// Total CPU permits. 0 detects the core count.
    #[arg(long, value_name = "N")]
    pub cpu_permits: Option<usize>,

    /// Concurrent lightweight remote API calls.
    #[arg(long, value_name = "N")]
    pub api_concurrency: Option<usize>,

    /// Cores video encoding leaves free so image jobs keep flowing.
    #[arg(long, value_name = "N")]
    pub video_reserve_cores: Option<usize>,

    /// Order files are picked up in.
    #[arg(long, value_enum)]
    pub order: Option<Order>,

    /// Leave videos shorter than this alone. 0 converts them all.
    #[arg(long, value_name = "SECONDS")]
    pub min_video_secs: Option<f64>,

    /// SVT-AV1 preset. Lower is smaller and slower.
    #[arg(long, value_name = "N")]
    pub preset: Option<u8>,

    /// Permit ffmpeg to drop corrupt MPEG-TS packets. This makes the remux lossy.
    #[arg(long)]
    pub allow_discard_corrupt: bool,

    /// When to empty the trash.
    #[arg(long, value_enum)]
    pub trash_policy: Option<TrashPolicy>,

    /// Days to keep trash before purging, with --trash-policy purge-after-days.
    #[arg(long, value_name = "DAYS")]
    pub purge_after_days: Option<u32>,
}

impl Cli {
    /// Collects every value that should take precedence over the config file.
    pub fn overrides(&self) -> Overrides {
        let mut ov = Overrides {
            remote: self.remote.clone(),
            paths: self.path.clone(),
            exclude: self.exclude.clone(),
            ..Default::default()
        };

        if let Command::Run(args) = &self.command {
            ov.staging_dir = args.staging_dir.clone();
            ov.staging_budget_gb = args.budget_gb;
            ov.max_file_gb = args.max_file_gb;
            ov.cloud_reserve_gb = args.cloud_reserve_gb;
            ov.reclaim_when_low_gb = args.reclaim_when_low_gb;
            ov.net_concurrency = args.net_concurrency;
            ov.api_concurrency = args.api_concurrency;
            ov.cpu_permits = args.cpu_permits;
            ov.video_reserve_cores = args.video_reserve_cores;
            ov.order = args.order;
            ov.min_video_secs = args.min_video_secs;
            ov.trash_policy = args.trash_policy;
            ov.purge_after_days = args.purge_after_days;
        }

        ov
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_flags_become_overrides() {
        let cli = Cli::try_parse_from([
            "verified-recompress",
            "--remote",
            "test:",
            "--path",
            "/Photos",
            "run",
            "--budget-gb",
            "120",
            "--trash-policy",
            "purge-now",
        ])
        .unwrap();
        let ov = cli.overrides();
        assert_eq!(ov.remote.as_deref(), Some("test:"));
        assert_eq!(ov.paths, vec!["/Photos".to_string()]);
        assert_eq!(ov.staging_budget_gb, Some(120));
        assert_eq!(ov.trash_policy, Some(TrashPolicy::PurgeNow));
    }

    #[test]
    fn repeated_path_and_exclude_accumulate() {
        let cli = Cli::try_parse_from([
            "verified-recompress",
            "--path",
            "/A",
            "--path",
            "/B",
            "--exclude",
            "**/x/**",
            "--exclude",
            "**/y/**",
            "scan",
        ])
        .unwrap();
        assert_eq!(cli.path, vec!["/A".to_string(), "/B".to_string()]);
        assert_eq!(
            cli.exclude,
            vec!["**/x/**".to_string(), "**/y/**".to_string()]
        );
    }

    /// Run-only flags must not leak into other subcommands' overrides.
    #[test]
    fn non_run_subcommand_has_no_run_overrides() {
        let cli = Cli::try_parse_from(["verified-recompress", "scan"]).unwrap();
        let ov = cli.overrides();
        assert_eq!(ov.staging_budget_gb, None);
        assert_eq!(ov.trash_policy, None);
    }
}
