//! Environment checks.
//!
//! Checking that a binary merely exists is not enough: the flags we depend on have
//! moved between releases (SVT-AV1 redefined `--lp`, libvmaf replaced `model_path=`
//! with `model=version=`). So every check probes for the specific capability we use
//! and fails loudly rather than letting a run break halfway through.
//!
//! The decision logic takes captured command output as plain strings so it can be
//! unit tested on machines that do not have the tools installed.

use std::fmt;

use anyhow::Result;
use tokio::process::Command;

use crate::config::Config;

/// Minimum rclone that ships the Filen backend (Tier 1 as of 1.73).
const MIN_RCLONE: Version = Version::new(1, 73, 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Extracts the first `x.y[.z]` sequence in `text`.
    ///
    /// Tools disagree on layout: rclone prints `rclone v1.75.1`, ffmpeg prints
    /// `ffmpeg version 9.0.1`, cjxl prints `cjxl v0.12.0 0.12.0 [...]`.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if !bytes[i].is_ascii_digit() {
                i += 1;
                continue;
            }
            // A digit preceded by a digit or '.' is mid-number; skip to the end of it.
            if i > 0 && (bytes[i - 1].is_ascii_digit() || bytes[i - 1] == b'.') {
                i += 1;
                continue;
            }
            let rest = &text[i..];
            if let Some(v) = Self::parse_at(rest) {
                return Some(v);
            }
            i += 1;
        }
        None
    }

    fn parse_at(s: &str) -> Option<Self> {
        let mut parts = s.split('.');
        let major = take_number(parts.next()?)?;
        let minor = take_number(parts.next()?)?;
        // Reject a bare `x.y` that is really a decimal inside a longer token.
        let patch = parts.next().and_then(take_number).unwrap_or(0);
        Some(Self::new(major, minor, patch))
    }
}

/// Reads the leading digits of `s`, requiring at least one.
fn take_number(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub tool: &'static str,
    pub severity: Severity,
    pub message: String,
}

impl Finding {
    fn error(tool: &'static str, message: impl Into<String>) -> Self {
        Self {
            tool,
            severity: Severity::Error,
            message: message.into(),
        }
    }

    fn warn(tool: &'static str, message: impl Into<String>) -> Self {
        Self {
            tool,
            severity: Severity::Warn,
            message: message.into(),
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub versions: Vec<(&'static str, Option<Version>)>,
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn has_errors(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == Severity::Error)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (tool, version) in &self.versions {
            match version {
                Some(v) => writeln!(f, "  {tool:<10} {v}")?,
                None => writeln!(f, "  {tool:<10} not found")?,
            }
        }
        if self.findings.is_empty() {
            writeln!(f, "\nAll checks passed.")?;
            return Ok(());
        }
        writeln!(f)?;
        for finding in &self.findings {
            let label = match finding.severity {
                Severity::Error => "error",
                Severity::Warn => "warn ",
            };
            writeln!(f, "  {label} [{}] {}", finding.tool, finding.message)?;
        }
        Ok(())
    }
}

/// Output captured from one probe command. `None` means the binary was not runnable.
type Captured = Option<String>;

/// Runs `program args...` and returns stdout+stderr combined.
///
/// Both streams are merged because tools disagree about where version banners go.
async fn capture(program: &str, args: &[&str]) -> Captured {
    let mut cmd = Command::new(program);
    cmd.args(args);
    tracing::debug!("run {}", crate::proc::describe(cmd.as_std()));
    let output = cmd.output().await.ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(text)
}

/// Asserts that `haystack` mentions every capability in `needles`.
fn require_all(
    tool: &'static str,
    haystack: &str,
    needles: &[&str],
    context: &str,
    out: &mut Vec<Finding>,
) {
    for needle in needles {
        if !haystack.contains(needle) {
            out.push(Finding::error(
                tool,
                format!("{context} does not offer `{needle}`; this build is unusable here"),
            ));
        }
    }
}

pub fn check_rclone(version_text: &str, listremotes: &str, remote: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    match Version::parse(version_text) {
        Some(v) if v < MIN_RCLONE => out.push(Finding::error(
            "rclone",
            format!("version {v} predates the Filen backend; need {MIN_RCLONE} or newer"),
        )),
        None => out.push(Finding::error(
            "rclone",
            "could not parse a version from `rclone version`",
        )),
        Some(_) => {}
    }

    // `filen:sub/dir` still lives on the `filen:` remote.
    let name = match remote.split_once(':') {
        Some((name, _)) => format!("{name}:"),
        None => format!("{remote}:"),
    };
    if !listremotes.lines().any(|l| l.trim() == name) {
        out.push(Finding::error(
            "rclone",
            format!("remote `{name}` is not configured; run `rclone config`"),
        ));
    }
    out
}

pub fn check_cjxl(help: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    require_all(
        "cjxl",
        help,
        &["--lossless_jpeg", "--allow_jpeg_reconstruction", "--num_threads"],
        "cjxl",
        &mut out,
    );
    out
}

pub fn check_djxl(help: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    // Byte-exact JPEG verification is impossible without JPEG output support.
    if !help.contains("JPEG") {
        out.push(Finding::error(
            "djxl",
            "this build cannot write JPEG, so JPEG round-trip verification is impossible",
        ));
        return out;
    }
    // Without --reconstruct_jpeg, a JXL lacking reconstruction data silently decodes to
    // pixels and encodes a *new lossy* JPEG. The hash comparison would still fail, but
    // this flag turns a confusing mismatch into an explicit error.
    require_all(
        "djxl",
        help,
        &["--reconstruct_jpeg"],
        "djxl",
        &mut out,
    );
    out
}

/// Capability probes for one ffmpeg build. The version itself is recorded separately
/// in [`Report::versions`]; what matters here is which features were compiled in.
pub struct FfmpegProbe<'a> {
    pub encoder_libsvtav1: &'a str,
    pub filter_libvmaf: &'a str,
    pub muxers: &'a str,
    pub bitstream_filters: &'a str,
    pub hwaccels: &'a str,
}

pub fn check_ffmpeg(probe: &FfmpegProbe<'_>) -> Vec<Finding> {
    let mut out = Vec::new();

    // Match the positive banner, not the bare name: a build without the encoder still
    // echoes it back as "Codec 'libsvtav1' is not recognized by FFmpeg."
    if !probe.encoder_libsvtav1.contains("Encoder libsvtav1") {
        out.push(Finding::error(
            "ffmpeg",
            "not built with libsvtav1; the video tier cannot run",
        ));
    } else {
        require_all(
            "ffmpeg",
            probe.encoder_libsvtav1,
            &["svtav1-params", "preset", "crf"],
            "libsvtav1",
            &mut out,
        );
        if !probe.encoder_libsvtav1.contains("yuv420p10le") {
            out.push(Finding::warn(
                "ffmpeg",
                "libsvtav1 does not advertise yuv420p10le; 10-bit encoding will fall back to 8-bit",
            ));
        }
    }

    // Likewise: a missing filter reports "Unknown filter 'libvmaf'.", which contains
    // the name and would otherwise read as success.
    if !probe.filter_libvmaf.contains("Filter libvmaf") {
        out.push(Finding::error(
            "ffmpeg",
            "not built with libvmaf; the video quality gate cannot run",
        ));
    } else {
        require_all(
            "ffmpeg",
            probe.filter_libvmaf,
            &["model", "log_fmt", "log_path"],
            "libvmaf",
            &mut out,
        );
        // Pre-2.x libvmaf took `model_path=`; our command builder emits `model=version=`.
        if probe.filter_libvmaf.contains("model_path") {
            out.push(Finding::error(
                "ffmpeg",
                "libvmaf exposes the legacy `model_path` option; this build predates the \
                 `model=version=` syntax we emit",
            ));
        }
    }

    require_all(
        "ffmpeg",
        probe.muxers,
        &["framemd5", "framehash", "streamhash"],
        "ffmpeg muxers",
        &mut out,
    );

    require_all(
        "ffmpeg",
        probe.bitstream_filters,
        &["aac_adtstoasc"],
        "ffmpeg bitstream filters",
        &mut out,
    );

    // NVDEC only saves time; its absence is not fatal.
    if !probe.hwaccels.contains("cuda") {
        out.push(Finding::warn(
            "ffmpeg",
            "no cuda hwaccel; decoding falls back to CPU and competes with encoding",
        ));
    }

    out
}

/// `taskset` is how CPU budgets are enforced. It only exists on Linux, which is the
/// deployment target; elsewhere the tool still runs but cannot bound encoder cores.
pub fn check_taskset(found: bool, target_is_linux: bool) -> Vec<Finding> {
    match (found, target_is_linux) {
        (true, _) => Vec::new(),
        (false, true) => vec![Finding::error(
            "taskset",
            "missing; install util-linux. Without it, video encoding cannot be confined \
             to its allotted cores and will starve the image pipeline",
        )],
        (false, false) => vec![Finding::warn(
            "taskset",
            "not available on this platform; CPU budgets will not be enforced",
        )],
    }
}

/// Probes the environment and reports what is missing or too old.
pub async fn run(cfg: &Config) -> Result<Report> {
    let mut report = Report::default();

    let rclone_version = capture("rclone", &["version"]).await;
    report
        .versions
        .push(("rclone", rclone_version.as_deref().and_then(Version::parse)));
    match &rclone_version {
        Some(text) => {
            let listremotes = capture("rclone", &["listremotes"]).await.unwrap_or_default();
            report
                .findings
                .extend(check_rclone(text, &listremotes, &cfg.remote));
        }
        None => report.findings.push(Finding::error("rclone", "not found")),
    }

    let cjxl_version = capture("cjxl", &["--version"]).await;
    report
        .versions
        .push(("cjxl", cjxl_version.as_deref().and_then(Version::parse)));
    // libjxl hides advanced flags behind verbose help; plain `--help` lists only basics.
    match capture("cjxl", &["-v", "-v", "--help"]).await {
        Some(help) => report.findings.extend(check_cjxl(&help)),
        None => report.findings.push(Finding::error("cjxl", "not found")),
    }

    let djxl_version = capture("djxl", &["--version"]).await;
    report
        .versions
        .push(("djxl", djxl_version.as_deref().and_then(Version::parse)));
    match capture("djxl", &["-v", "-v", "--help"]).await {
        Some(help) => report.findings.extend(check_djxl(&help)),
        None => report.findings.push(Finding::error("djxl", "not found")),
    }

    // cjxl reads no WebP at all, so a lossless one is expanded to PNG first.
    // Only the WebP recipe needs this, which is why its absence is a warning
    // rather than an error: everything else still runs.
    match capture("dwebp", &["-version"]).await {
        Some(version) => report
            .versions
            .push(("dwebp", Version::parse(&version))),
        None => report.findings.push(Finding::warn(
            "dwebp",
            "not found, so lossless WebP files will fail to convert; the other recipes are unaffected",
        )),
    }

    let ffmpeg_version = capture("ffmpeg", &["-hide_banner", "-version"]).await;
    report
        .versions
        .push(("ffmpeg", ffmpeg_version.as_deref().and_then(Version::parse)));
    match &ffmpeg_version {
        Some(_) => {
            let encoder = capture("ffmpeg", &["-hide_banner", "-h", "encoder=libsvtav1"])
                .await
                .unwrap_or_default();
            let filter = capture("ffmpeg", &["-hide_banner", "-h", "filter=libvmaf"])
                .await
                .unwrap_or_default();
            let muxers = capture("ffmpeg", &["-hide_banner", "-muxers"])
                .await
                .unwrap_or_default();
            let bsfs = capture("ffmpeg", &["-hide_banner", "-bsfs"])
                .await
                .unwrap_or_default();
            let hwaccels = capture("ffmpeg", &["-hide_banner", "-hwaccels"])
                .await
                .unwrap_or_default();
            report.findings.extend(check_ffmpeg(&FfmpegProbe {
                encoder_libsvtav1: &encoder,
                filter_libvmaf: &filter,
                muxers: &muxers,
                bitstream_filters: &bsfs,
                hwaccels: &hwaccels,
            }));
        }
        None => report.findings.push(Finding::error("ffmpeg", "not found")),
    }

    for (tool, args) in [
        ("ffprobe", &["-version"][..]),
        ("flac", &["--version"][..]),
        ("ab-av1", &["--version"][..]),
        ("exiftool", &["-ver"][..]),
    ] {
        let text = capture(tool, args).await;
        report
            .versions
            .push((leak(tool), text.as_deref().and_then(Version::parse)));
        if text.is_none() {
            report.findings.push(Finding::error(leak(tool), "not found"));
        }
    }

    let taskset_found = capture("taskset", &["--version"]).await.is_some();
    report.versions.push(("taskset", None));
    report
        .findings
        .extend(check_taskset(taskset_found, cfg!(target_os = "linux")));

    Ok(report)
}

/// Whether ffmpeg can decode on the GPU.
///
/// Only a speed question: VMAF has to decode two streams at once, and moving that
/// off the CPU leaves the cores for encoding. Its absence changes nothing else.
pub async fn has_cuda() -> bool {
    capture("ffmpeg", &["-hide_banner", "-hwaccels"])
        .await
        .is_some_and(|text| text.contains("cuda"))
}

/// The tool names above are all string literals; this keeps `Finding` on `&'static str`.
fn leak(name: &str) -> &'static str {
    match name {
        "ffprobe" => "ffprobe",
        "flac" => "flac",
        "ab-av1" => "ab-av1",
        "exiftool" => "exiftool",
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rclone_banner() {
        assert_eq!(
            Version::parse("rclone v1.75.1\n- os/version: arch"),
            Some(Version::new(1, 75, 1))
        );
    }

    #[test]
    fn parses_ffmpeg_banner() {
        assert_eq!(
            Version::parse("ffmpeg version 9.0.1 Copyright (c) 2000-2026"),
            Some(Version::new(9, 0, 1))
        );
    }

    #[test]
    fn parses_cjxl_banner_with_repeated_version() {
        assert_eq!(
            Version::parse("cjxl v0.12.0 0.12.0 [_NEON_BF16_,NEON]"),
            Some(Version::new(0, 12, 0))
        );
    }

    #[test]
    fn parses_two_component_version() {
        assert_eq!(Version::parse("flac 1.5"), Some(Version::new(1, 5, 0)));
    }

    #[test]
    fn no_version_present() {
        assert_eq!(Version::parse("command not found"), None);
    }

    #[test]
    fn versions_order_correctly() {
        assert!(Version::new(1, 72, 9) < MIN_RCLONE);
        assert!(Version::new(1, 73, 0) >= MIN_RCLONE);
        assert!(Version::new(1, 75, 1) >= MIN_RCLONE);
    }

    #[test]
    fn old_rclone_is_rejected() {
        let findings = check_rclone("rclone v1.72.0", "filen:\n", "filen:");
        assert!(findings.iter().any(|f| f.message.contains("predates")));
    }

    #[test]
    fn current_rclone_with_configured_remote_passes() {
        assert!(check_rclone("rclone v1.75.1", "filen:\n", "filen:").is_empty());
    }

    #[test]
    fn remote_subpath_matches_its_remote() {
        assert!(check_rclone("rclone v1.75.1", "filen:\n", "filen:Photos/2019").is_empty());
    }

    #[test]
    fn unconfigured_remote_is_rejected() {
        let findings = check_rclone("rclone v1.75.1", "other:\n", "filen:");
        assert!(findings.iter().any(|f| f.message.contains("not configured")));
    }

    #[test]
    fn cjxl_without_reconstruction_support_is_rejected() {
        // A build lacking --allow_jpeg_reconstruction cannot guarantee byte-exact
        // JPEG round-trips, which is the whole basis of the JPEG tier.
        let findings = check_cjxl("-d DISTANCE\n-e EFFORT\n--num_threads\n--lossless_jpeg=0|1");
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("--allow_jpeg_reconstruction"));
    }

    #[test]
    fn complete_cjxl_passes() {
        let help = "-j 0|1, --lossless_jpeg=0|1\n--allow_jpeg_reconstruction=0|1\n--num_threads=N";
        assert!(check_cjxl(help).is_empty());
    }

    #[test]
    fn djxl_without_jpeg_output_is_rejected() {
        let findings = check_djxl("The output format can be PPM, PNM, PNG");
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("cannot write JPEG"));
    }

    /// Without `--reconstruct_jpeg`, djxl falls back to encoding a *new lossy* JPEG
    /// when reconstruction data is absent, which is a silently wrong verification path.
    #[test]
    fn djxl_without_reconstruct_jpeg_flag_is_rejected() {
        let findings = check_djxl("output format can be PPM, PNG, JPEG, EXR\n --pixels_to_jpeg");
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("--reconstruct_jpeg"));
    }

    #[test]
    fn complete_djxl_passes() {
        let help = "output format can be PPM, PNM, PFM, PAM, PGX, PNG, APNG, JPEG, EXR\n \
                    -J, --reconstruct_jpeg";
        assert!(check_djxl(help).is_empty());
    }

    fn good_ffmpeg_probe() -> FfmpegProbe<'static> {
        FfmpegProbe {
            encoder_libsvtav1: "Encoder libsvtav1 [...]\n  Supported pixel formats: yuv420p yuv420p10le\n  -preset <int>\n  -crf <int>\n  -svtav1-params <dictionary>",
            filter_libvmaf: "Filter libvmaf\n log_path <string>\n log_fmt <string>\n model <string> (default \"version=vmaf_v0.6.1\")",
            muxers: "E framemd5\nE framehash\nE streamhash\nE hash",
            bitstream_filters: "aac_adtstoasc\nh264_mp4toannexb",
            hwaccels: "cuda\nvideotoolbox",
        }
    }

    #[test]
    fn well_equipped_ffmpeg_passes() {
        assert!(check_ffmpeg(&good_ffmpeg_probe()).is_empty());
    }

    /// ffmpeg echoes the requested name back when it is absent, so a naive
    /// `contains("libsvtav1")` would read the rejection as success.
    #[test]
    fn ffmpeg_without_libsvtav1_is_rejected() {
        let probe = FfmpegProbe {
            encoder_libsvtav1: "Codec 'libsvtav1' is not recognized by FFmpeg.",
            ..good_ffmpeg_probe()
        };
        let findings = check_ffmpeg(&probe);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("not built with libsvtav1"));
    }

    /// Same trap for filters: the absent case is "Unknown filter 'libvmaf'.".
    #[test]
    fn ffmpeg_without_libvmaf_is_rejected() {
        let probe = FfmpegProbe {
            filter_libvmaf: "Unknown filter 'libvmaf'.",
            ..good_ffmpeg_probe()
        };
        let findings = check_ffmpeg(&probe);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("quality gate"));
    }

    /// The command builder emits `model=version=...`; a build offering the pre-2.x
    /// `model_path=` option would silently ignore it and score against the wrong model.
    #[test]
    fn legacy_libvmaf_model_path_is_rejected() {
        let probe = FfmpegProbe {
            filter_libvmaf: "Filter libvmaf\n model_path <string>\n log_fmt <string>\n log_path <string>\n model <string>",
            ..good_ffmpeg_probe()
        };
        let findings = check_ffmpeg(&probe);
        assert!(findings.iter().any(|f| f.message.contains("legacy `model_path`")));
    }

    #[test]
    fn ffmpeg_without_aac_adtstoasc_is_rejected() {
        let probe = FfmpegProbe {
            bitstream_filters: "h264_mp4toannexb",
            ..good_ffmpeg_probe()
        };
        let findings = check_ffmpeg(&probe);
        assert!(findings.iter().any(|f| f.message.contains("aac_adtstoasc")));
    }

    #[test]
    fn missing_cuda_is_only_a_warning() {
        let probe = FfmpegProbe {
            hwaccels: "videotoolbox",
            ..good_ffmpeg_probe()
        };
        let findings = check_ffmpeg(&probe);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warn);
    }

    #[test]
    fn taskset_is_required_on_linux_only() {
        assert!(check_taskset(true, true).is_empty());
        assert_eq!(check_taskset(false, true)[0].severity, Severity::Error);
        assert_eq!(check_taskset(false, false)[0].severity, Severity::Warn);
    }

    #[test]
    fn report_flags_errors_but_not_warnings() {
        let mut report = Report::default();
        report.findings.push(Finding::warn("ffmpeg", "no cuda"));
        assert!(!report.has_errors());
        report.findings.push(Finding::error("rclone", "not found"));
        assert!(report.has_errors());
    }
}
