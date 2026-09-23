//! Configuration loading and resolution.
//!
//! Precedence is CLI flags > config file > auto-detection. Byte quantities are
//! resolved to MiB because `tokio::sync::Semaphore::acquire_many` takes a `u32`:
//! using raw bytes would overflow on any file past 4 GiB.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::policy::DEFAULT_MIN_VIDEO_SECS;

/// Fraction of free space used when no explicit budget is given.
const DEFAULT_BUDGET_FRACTION: f64 = 0.70;
const DEFAULT_CLOUD_RESERVE_GB: u64 = 5;
const DEFAULT_NET_CONCURRENCY: usize = 8;
const DEFAULT_API_CONCURRENCY: usize = 4;
const DEFAULT_NON_VIDEO_CORES: usize = 2;
const DEFAULT_PURGE_AFTER_DAYS: u32 = 30;
const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum TrashPolicy {
    /// Leave originals in the trash. Quota does not shrink, but they stay recoverable.
    Keep,
    /// Purge trash entries older than `purge_after_days`.
    PurgeAfterDays,
    /// Purge immediately at the end of a run.
    PurgeNow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum Order {
    Savings,
    Size,
    Path,
}

/// Raw `config.toml` contents. Every field is optional so absence is distinguishable
/// from an explicit value.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub remote: Option<String>,
    pub staging_dir: Option<PathBuf>,
    pub staging_budget_gb: Option<u64>,
    pub max_file_gb: Option<u64>,
    pub cloud_reserve_gb: Option<u64>,
    pub reclaim_when_low_gb: Option<u64>,
    pub net_concurrency: Option<usize>,
    pub api_concurrency: Option<usize>,
    /// Read under its old name too: renaming a key must not stop an existing
    /// config from loading, and `deny_unknown_fields` would reject it outright.
    #[serde(alias = "cpu_permits")]
    pub cpu_cores: Option<usize>,
    /// Read under its old name too: renaming a key must not stop an existing
    /// config from loading, and `deny_unknown_fields` would reject it outright.
    #[serde(alias = "video_reserve_cores")]
    pub non_video_cores: Option<usize>,
    pub order: Option<Order>,
    pub min_video_secs: Option<f64>,
    pub paths: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub trash_policy: Option<TrashPolicy>,
    pub purge_after_days: Option<u32>,
}

impl FileConfig {
    /// Default location: `~/.config/verified-recompress/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("verified-recompress").join("config.toml"))
    }

    /// Loads the file at `path`. A missing file yields defaults; a malformed one errors.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("failed to parse config at {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }
}

/// CLI-supplied values that take precedence over the file.
#[derive(Debug, Default)]
pub struct Overrides {
    pub remote: Option<String>,
    pub staging_dir: Option<PathBuf>,
    pub staging_budget_gb: Option<u64>,
    pub max_file_gb: Option<u64>,
    pub cloud_reserve_gb: Option<u64>,
    pub reclaim_when_low_gb: Option<u64>,
    pub net_concurrency: Option<usize>,
    pub api_concurrency: Option<usize>,
    pub cpu_cores: Option<usize>,
    pub non_video_cores: Option<usize>,
    pub order: Option<Order>,
    pub min_video_secs: Option<f64>,
    /// Replaces the file's `paths` entirely when non-empty.
    pub paths: Vec<String>,
    /// Appended to the file's `exclude`.
    pub exclude: Vec<String>,
    pub trash_policy: Option<TrashPolicy>,
    pub purge_after_days: Option<u32>,
}

/// Fully resolved, validated configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub remote: String,
    pub staging_dir: PathBuf,
    pub staging_budget_mib: u32,
    pub max_file_mib: u32,
    pub cloud_reserve_mib: u32,
    pub reclaim_when_low_mib: Option<u32>,
    pub net_concurrency: usize,
    pub api_concurrency: usize,
    pub cpu_cores: usize,
    pub non_video_cores: usize,
    pub order: Order,
    /// Duration floor for the AV1 tier, in seconds. Zero means no floor.
    pub min_video_secs: f64,
    pub paths: Vec<String>,
    pub exclude: Vec<String>,
    pub trash_policy: TrashPolicy,
    pub purge_after_days: u32,
}

fn gb_to_mib(gb: u64, field: &str) -> Result<u32> {
    let mib = gb
        .checked_mul(1024)
        .with_context(|| format!("{field} = {gb} GB overflows"))?;
    u32::try_from(mib).with_context(|| format!("{field} = {gb} GB is too large to track in MiB"))
}

impl Config {
    /// Merges file config with CLI overrides and validates the result.
    ///
    /// `available_bytes` is the free space on the staging filesystem, passed in
    /// rather than probed so this stays a pure function and can be unit tested.
    pub fn resolve(file: FileConfig, ov: Overrides, available_bytes: u64) -> Result<Self> {
        let staging_dir = ov
            .staging_dir
            .or(file.staging_dir)
            .unwrap_or_else(default_staging_dir);

        let min_video_secs = ov
            .min_video_secs
            .or(file.min_video_secs)
            .unwrap_or(DEFAULT_MIN_VIDEO_SECS);
        if !min_video_secs.is_finite() || min_video_secs < 0.0 {
            bail!("min_video_secs = {min_video_secs} must be zero or a positive number of seconds");
        }

        let budget_gb = ov.staging_budget_gb.or(file.staging_budget_gb);
        let staging_budget_mib = match budget_gb {
            Some(gb) => {
                let mib = gb_to_mib(gb, "staging_budget_gb")?;
                let requested = u64::from(mib) * MIB;
                if requested > available_bytes {
                    bail!(
                        "staging budget {gb} GB exceeds free space on {} ({:.1} GB available)",
                        staging_dir.display(),
                        available_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                    );
                }
                mib
            }
            // Auto-detect: take a fraction of what is actually free.
            None => {
                let auto = (available_bytes as f64 * DEFAULT_BUDGET_FRACTION) as u64 / MIB;
                u32::try_from(auto).unwrap_or(u32::MAX)
            }
        };

        if staging_budget_mib == 0 {
            bail!(
                "no usable staging space on {} (free space {} bytes)",
                staging_dir.display(),
                available_bytes
            );
        }

        let max_file_mib = match ov.max_file_gb.or(file.max_file_gb) {
            Some(gb) => {
                let mib = gb_to_mib(gb, "max_file_gb")?;
                if mib > staging_budget_mib {
                    bail!(
                        "max_file_gb ({gb} GB) exceeds the staging budget ({} GB)",
                        staging_budget_mib / 1024
                    );
                }
                mib
            }
            None => staging_budget_mib / 2,
        };

        if max_file_mib == 0 {
            bail!("staging budget is too small to process any file");
        }

        let cloud_reserve_mib = gb_to_mib(
            ov.cloud_reserve_gb
                .or(file.cloud_reserve_gb)
                .unwrap_or(DEFAULT_CLOUD_RESERVE_GB),
            "cloud_reserve_gb",
        )?;

        // 0 is the documented "disabled" sentinel for automatic reclamation.
        let reclaim_when_low_mib = match ov.reclaim_when_low_gb.or(file.reclaim_when_low_gb) {
            None | Some(0) => None,
            Some(gb) => Some(gb_to_mib(gb, "reclaim_when_low_gb")?),
        };

        let cpu_cores = match ov.cpu_cores.or(file.cpu_cores) {
            None | Some(0) => num_cpus::get(),
            Some(n) => n,
        };

        let non_video_cores = ov
            .non_video_cores
            .or(file.non_video_cores)
            .unwrap_or(DEFAULT_NON_VIDEO_CORES);

        if non_video_cores >= cpu_cores {
            bail!(
                "non_video_cores ({non_video_cores}) must be less than cpu_cores \
                 ({cpu_cores}); otherwise video encoding gets no cores"
            );
        }

        let net_concurrency = ov
            .net_concurrency
            .or(file.net_concurrency)
            .unwrap_or(DEFAULT_NET_CONCURRENCY);
        if net_concurrency == 0 {
            bail!("net_concurrency must be at least 1");
        }

        let api_concurrency = ov
            .api_concurrency
            .or(file.api_concurrency)
            .unwrap_or(DEFAULT_API_CONCURRENCY);
        if api_concurrency == 0 {
            bail!("api_concurrency must be at least 1");
        }

        // CLI paths replace the file's list; excludes accumulate.
        let paths = if ov.paths.is_empty() {
            file.paths.unwrap_or_default()
        } else {
            ov.paths
        };
        let mut exclude = file.exclude.unwrap_or_default();
        exclude.extend(ov.exclude);

        Ok(Self {
            remote: ov
                .remote
                .or(file.remote)
                .unwrap_or_else(|| "filen:".to_string()),
            staging_dir,
            staging_budget_mib,
            max_file_mib,
            cloud_reserve_mib,
            reclaim_when_low_mib,
            net_concurrency,
            api_concurrency,
            cpu_cores,
            non_video_cores,
            order: ov.order.or(file.order).unwrap_or(Order::Savings),
            min_video_secs,
            paths,
            exclude,
            trash_policy: ov
                .trash_policy
                .or(file.trash_policy)
                .unwrap_or(TrashPolicy::Keep),
            purge_after_days: ov
                .purge_after_days
                .or(file.purge_after_days)
                .unwrap_or(DEFAULT_PURGE_AFTER_DAYS),
        })
    }
}

fn default_staging_dir() -> PathBuf {
    std::env::temp_dir().join("verified-recompress")
}

/// Free space on the filesystem holding `dir`, walking up to the nearest existing
/// ancestor so an unconfigured staging directory can still be sized.
pub fn available_space(dir: &Path) -> Result<u64> {
    let mut probe = dir;
    loop {
        if probe.exists() {
            return fs4::available_space(probe)
                .with_context(|| format!("failed to stat filesystem at {}", probe.display()));
        }
        match probe.parent() {
            Some(parent) => probe = parent,
            None => bail!("no existing ancestor of {} to measure", dir.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn resolve(file: FileConfig, ov: Overrides, avail: u64) -> Result<Config> {
        Config::resolve(file, ov, avail)
    }

    #[test]
    fn cli_overrides_file() {
        let file = FileConfig {
            staging_budget_gb: Some(10),
            remote: Some("fromfile:".into()),
            ..Default::default()
        };
        let ov = Overrides {
            staging_budget_gb: Some(20),
            remote: Some("fromcli:".into()),
            ..Default::default()
        };
        let cfg = resolve(file, ov, 100 * GB).unwrap();
        assert_eq!(cfg.staging_budget_mib, 20 * 1024);
        assert_eq!(cfg.remote, "fromcli:");
    }

    #[test]
    fn budget_defaults_to_fraction_of_free_space() {
        let cfg = resolve(FileConfig::default(), Overrides::default(), 100 * GB).unwrap();
        assert_eq!(cfg.staging_budget_mib, 70 * 1024);
    }

    #[test]
    fn budget_larger_than_free_space_is_rejected() {
        let file = FileConfig {
            staging_budget_gb: Some(200),
            ..Default::default()
        };
        let err = resolve(file, Overrides::default(), 100 * GB).unwrap_err();
        assert!(err.to_string().contains("exceeds free space"), "{err}");
    }

    #[test]
    fn max_file_defaults_to_half_the_budget() {
        let file = FileConfig {
            staging_budget_gb: Some(50),
            ..Default::default()
        };
        let cfg = resolve(file, Overrides::default(), 100 * GB).unwrap();
        assert_eq!(cfg.max_file_mib, 25 * 1024);
    }

    #[test]
    fn max_file_larger_than_budget_is_rejected() {
        let file = FileConfig {
            staging_budget_gb: Some(10),
            max_file_gb: Some(40),
            ..Default::default()
        };
        let err = resolve(file, Overrides::default(), 100 * GB).unwrap_err();
        assert!(err.to_string().contains("exceeds the staging budget"), "{err}");
    }

    #[test]
    fn zero_free_space_is_rejected() {
        let err = resolve(FileConfig::default(), Overrides::default(), 0).unwrap_err();
        assert!(err.to_string().contains("no usable staging space"), "{err}");
    }

    /// A 100 GB budget is 102400 MiB. In bytes this would blow past `u32::MAX`,
    /// which is exactly why permits are denominated in MiB.
    #[test]
    fn large_budget_fits_in_u32_permits() {
        let file = FileConfig {
            staging_budget_gb: Some(100),
            max_file_gb: Some(8),
            ..Default::default()
        };
        let cfg = resolve(file, Overrides::default(), 200 * GB).unwrap();
        assert_eq!(cfg.staging_budget_mib, 102_400);
        assert_eq!(cfg.max_file_mib, 8 * 1024);
        // The same numbers in bytes would not fit.
        assert!(u32::try_from(u64::from(cfg.max_file_mib) * MIB).is_err());
    }

    #[test]
    fn reclaim_zero_means_disabled() {
        let file = FileConfig {
            reclaim_when_low_gb: Some(0),
            ..Default::default()
        };
        let cfg = resolve(file, Overrides::default(), 100 * GB).unwrap();
        assert_eq!(cfg.reclaim_when_low_mib, None);
    }

    /// Both settings were renamed to say which side of the split they name.
    /// `deny_unknown_fields` turns a stale key into a hard load failure, so the
    /// old spellings have to keep working.
    #[test]
    fn the_previous_key_names_still_load() {
        let file: FileConfig = toml::from_str(
            "cpu_permits = 8\nvideo_reserve_cores = 3\n",
        )
        .unwrap();
        assert_eq!(file.cpu_cores, Some(8));
        assert_eq!(file.non_video_cores, Some(3));
    }

    #[test]
    fn cpu_cores_zero_means_detect() {
        let file = FileConfig {
            cpu_cores: Some(0),
            ..Default::default()
        };
        let cfg = resolve(file, Overrides::default(), 100 * GB).unwrap();
        assert_eq!(cfg.cpu_cores, num_cpus::get());
    }

    #[test]
    fn video_reserve_must_leave_cores_for_video() {
        let file = FileConfig {
            cpu_cores: Some(2),
            non_video_cores: Some(2),
            ..Default::default()
        };
        let err = resolve(file, Overrides::default(), 100 * GB).unwrap_err();
        assert!(err.to_string().contains("must be less than cpu_cores"), "{err}");
    }

    #[test]
    fn cli_paths_replace_file_paths_but_excludes_accumulate() {
        let file = FileConfig {
            paths: Some(vec!["/FromFile".into()]),
            exclude: Some(vec!["**/a/**".into()]),
            ..Default::default()
        };
        let ov = Overrides {
            paths: vec!["/FromCli".into()],
            exclude: vec!["**/b/**".into()],
            ..Default::default()
        };
        let cfg = resolve(file, ov, 100 * GB).unwrap();
        assert_eq!(cfg.paths, vec!["/FromCli".to_string()]);
        assert_eq!(cfg.exclude, vec!["**/a/**".to_string(), "**/b/**".to_string()]);
    }

    #[test]
    fn trash_policy_defaults_to_keep() {
        let cfg = resolve(FileConfig::default(), Overrides::default(), 100 * GB).unwrap();
        assert_eq!(cfg.trash_policy, TrashPolicy::Keep);
    }

    #[test]
    fn missing_config_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = FileConfig::load(&dir.path().join("absent.toml")).unwrap();
        assert!(cfg.remote.is_none());
    }

    #[test]
    fn malformed_config_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "staging_budget_gb = \"not a number\"").unwrap();
        assert!(FileConfig::load(&path).is_err());
    }

    #[test]
    fn unknown_config_key_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("typo.toml");
        std::fs::write(&path, "staging_budget_gbb = 10").unwrap();
        assert!(FileConfig::load(&path).is_err());
    }

    #[test]
    fn config_file_round_trips_documented_example() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
remote = "filen:"
staging_budget_gb = 50
max_file_gb = 20
cloud_reserve_gb = 5
reclaim_when_low_gb = 0
net_concurrency = 8
api_concurrency = 4
cpu_cores = 0
non_video_cores = 2
order = "savings"
paths = ["/Photos/2019", "/Camera"]
exclude = ["**/.thumbnails/**"]
trash_policy = "keep"
purge_after_days = 30
"#,
        )
        .unwrap();
        let file = FileConfig::load(&path).unwrap();
        let cfg = Config::resolve(file, Overrides::default(), 200 * GB).unwrap();
        assert_eq!(cfg.staging_budget_mib, 50 * 1024);
        assert_eq!(cfg.order, Order::Savings);
        assert_eq!(cfg.trash_policy, TrashPolicy::Keep);
        assert_eq!(cfg.paths.len(), 2);
    }

    #[test]
    fn available_space_walks_up_to_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does").join("not").join("exist");
        assert!(available_space(&missing).unwrap() > 0);
    }
}
