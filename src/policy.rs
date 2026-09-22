//! Decides what to do with each file, and why.
//!
//! Every decision is explicit: either a recipe or a named reason for leaving the
//! file alone. Nothing is skipped silently, because "why is my drive still full"
//! has to be answerable from the report.
//!
//! The function is pure. It takes the facts and returns a verdict, so the whole
//! decision table is unit-testable without touching a remote or an encoder.

use std::fmt;

use crate::classify::{Kind, MediaProbe, kind_from_extension};

/// Below this saving the rewrite is not worth the upload, the risk, or the loss of
/// the original's exact bytes.
pub const MIN_GAIN: f64 = 0.03;

/// Default duration floor for the AV1 tier: none.
///
/// A short clip is not a worse candidate per byte — both the encode cost and the
/// saving scale with its length. What does not scale is the per-file overhead:
/// the round trips, the CRF search, and up to three full encodes each scored by
/// decoding both files in full. Whether that trade is worth making is a
/// judgement about the drive, so it is a setting rather than a constant.
pub const DEFAULT_MIN_VIDEO_SECS: f64 = 0.0;

/// Bits per pixel per second below which a video is already so compressed that
/// re-encoding it at visually-lossless quality would not shrink it.
///
/// Derived from the intended rule of thumb, 1080p below roughly 2 Mbps:
/// 2_000_000 / (1920 * 1080) = 0.96. The same figure falls out at 4K and 8 Mbps
/// (8_000_000 / (3840 * 2160) = 0.96), which is the point of normalising by area.
const MIN_BITS_PER_PIXEL_SEC: f64 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipe {
    /// JPEG to JPEG XL. Byte-reversible: `djxl -J` rebuilds the original exactly.
    JxlFromJpeg,
    /// PNG/GIF/BMP/TIFF to JPEG XL. Pixel-identical, not byte-identical.
    JxlFromRaster,
    /// Lossless WebP to JPEG XL. Pixel-identical, and a much smaller win than
    /// the other raster sources, which is why it is not folded into them.
    JxlFromWebp,
    /// PCM to FLAC. Sample-identical, verified against the stored PCM MD5.
    Flac,
    /// Re-encode an existing lossless audio file at maximum compression.
    FlacRecompress,
    /// MPEG-TS to MP4 by stream copy. Lossless; recovers container overhead only.
    TsRemux,
    /// Intra-only or uncompressed video to FFV1. Truly lossless.
    Ffv1,
    /// Lossy video to AV1 under a measured VMAF gate. Not reversible.
    Av1,
}

impl Recipe {
    /// Expected output size as a fraction of the input, for projecting savings
    /// before anything is encoded. Deliberately conservative.
    pub fn expected_ratio(self) -> f64 {
        match self {
            Recipe::JxlFromJpeg => 0.80,
            Recipe::JxlFromRaster => 0.65,
            // Measured on real images rather than assumed: lossless WebP is
            // already well compressed, so JXL buys 6-7%, not the third that a
            // PNG gives up.
            Recipe::JxlFromWebp => 0.93,
            Recipe::Flac => 0.55,
            Recipe::FlacRecompress => 0.94,
            Recipe::TsRemux => 0.96,
            Recipe::Ffv1 => 0.50,
            Recipe::Av1 => 0.70,
        }
    }

    /// Whether the original bytes can be reconstructed from the output.
    pub fn is_byte_reversible(self) -> bool {
        matches!(self, Recipe::JxlFromJpeg)
    }

    /// Whether the media content survives bit-for-bit, even if the container differs.
    pub fn is_lossless(self) -> bool {
        !matches!(self, Recipe::Av1)
    }

    pub fn output_extension(self, source_ext: &str) -> &'static str {
        match self {
            Recipe::JxlFromJpeg | Recipe::JxlFromRaster | Recipe::JxlFromWebp => "jxl",
            Recipe::Flac | Recipe::FlacRecompress => "flac",
            Recipe::TsRemux => "mp4",
            Recipe::Ffv1 => "mkv",
            // AV1 keeps the source container so QuickTime metadata survives.
            Recipe::Av1 => match source_ext.to_ascii_lowercase().as_str() {
                "mov" | "qt" => "mov",
                "mkv" | "webm" => "mkv",
                _ => "mp4",
            },
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Recipe::JxlFromJpeg => "jxl-from-jpeg",
            Recipe::JxlFromRaster => "jxl-from-raster",
            Recipe::JxlFromWebp => "jxl-from-webp",
            Recipe::Flac => "flac",
            Recipe::FlacRecompress => "flac-recompress",
            Recipe::TsRemux => "ts-remux",
            Recipe::Ffv1 => "ffv1",
            Recipe::Av1 => "av1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Already in an efficient format; re-encoding would only add loss.
    AlreadyOptimal,
    /// Lossy source with no lossless path to a smaller file.
    LossyNoGain,
    /// Would not fit in the staging budget.
    TooLargeForBudget,
    /// Zero bytes, or too small for the overhead to pay off.
    TooSmall,
    /// HDR10, HLG, or Dolby Vision. Re-encoding loses the mastering metadata.
    VideoHdr,
    /// Already AV1; another pass would only stack generation loss.
    VideoAlreadyAv1,
    /// Already compressed hard enough that AV1 would not beat it.
    VideoLowBitrate,
    VideoTooShort,
    /// Multiple video streams, attachments, or other structure we will not risk.
    VideoComplexStructure,
    /// The AV1 tier is irreversible and needs --allow-video.
    VideoTierDisabled,
    /// Needs a probe before it can be judged; `plan` cannot download.
    NeedsProbe,
    /// Nothing we know how to improve.
    Unsupported,
}

impl SkipReason {
    /// Whether this verdict depends on how the run was configured rather than on
    /// the file itself.
    ///
    /// A file left alone because `--allow-video` was absent, or because the staging
    /// budget was small, must be reconsidered when those change. A file left alone
    /// because it is HDR or already AV1 never needs looking at again.
    pub fn depends_on_settings(self) -> bool {
        matches!(
            self,
            SkipReason::VideoTierDisabled | SkipReason::TooLargeForBudget
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::AlreadyOptimal => "already_optimal",
            SkipReason::LossyNoGain => "lossy_no_gain",
            SkipReason::TooLargeForBudget => "too_large_for_budget",
            SkipReason::TooSmall => "too_small",
            SkipReason::VideoHdr => "video_hdr",
            SkipReason::VideoAlreadyAv1 => "video_already_av1",
            SkipReason::VideoLowBitrate => "video_low_bitrate",
            SkipReason::VideoTooShort => "video_too_short",
            SkipReason::VideoComplexStructure => "video_complex_structure",
            SkipReason::VideoTierDisabled => "video_tier_disabled",
            SkipReason::NeedsProbe => "needs_probe",
            SkipReason::Unsupported => "unsupported",
        }
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Convert(Recipe),
    Skip(SkipReason),
}

impl Decision {
    pub fn recipe(self) -> Option<Recipe> {
        match self {
            Decision::Convert(r) => Some(r),
            Decision::Skip(_) => None,
        }
    }
}

/// Inputs the decision depends on, gathered so the rules read as one table.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest input we can stage, in bytes.
    pub max_file_bytes: u64,
    /// Whether the irreversible AV1 tier is permitted.
    pub allow_video: bool,
    /// Videos shorter than this are left alone. Zero means no floor.
    pub min_video_secs: f64,
}

impl Limits {
    /// The most permissive limits there are, used where the question is what a
    /// file could yield rather than what this run will do.
    pub fn unbounded() -> Self {
        Self {
            max_file_bytes: u64::MAX,
            allow_video: true,
            min_video_secs: 0.0,
        }
    }
}

/// Bytes a conversion would be expected to save, for ordering intake alone.
///
/// Deliberately independent of the run's settings: if the order changed with the
/// flags, a run resumed with different ones would revisit files in a different
/// sequence. It is also only as good as [`Recipe::expected_ratio`], which is a
/// per-recipe rule of thumb rather than a measurement — good enough to sort by,
/// not good enough to report.
pub fn projected_saving(path: &str, size: u64) -> u64 {
    let ratio = match decide(Facts::new(path, size), Limits::unbounded()) {
        Decision::Convert(recipe) => recipe.expected_ratio(),
        // Some files cannot be judged without their bytes, so at intake time
        // their saving is unknown. Guessing is right here and wrong in `report`:
        // a projection shown to a person must not promise a saving nobody
        // measured, but a sort key that treated every video as worthless would
        // put the biggest wins on the drive dead last. The guess follows the
        // recipe each kind would most likely reach, so a WebP is not queued as
        // if it were a film.
        Decision::Skip(SkipReason::NeedsProbe) => match kind_from_extension(path) {
            Kind::Webp => Recipe::JxlFromWebp.expected_ratio(),
            _ => Recipe::Av1.expected_ratio(),
        },
        Decision::Skip(_) => return 0,
    };
    size.saturating_sub((size as f64 * ratio) as u64)
}

/// Files below this are not worth a round trip.
const MIN_USEFUL_BYTES: u64 = 4096;

/// Everything known about one file at the moment of the decision.
///
/// `probe` and `head` are absent at plan time, because filling them in means
/// downloading the file, which is exactly what a projection must not do.
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts<'a> {
    pub path: &'a str,
    pub size: u64,
    /// ffprobe output, once the bytes are local.
    pub probe: Option<&'a MediaProbe>,
    /// First few KiB, used to confirm ambiguous extensions.
    pub head: Option<&'a [u8]>,
}

impl<'a> Facts<'a> {
    pub fn new(path: &'a str, size: u64) -> Self {
        Self {
            path,
            size,
            probe: None,
            head: None,
        }
    }

    pub fn with_probe(mut self, probe: &'a MediaProbe) -> Self {
        self.probe = Some(probe);
        self
    }

    pub fn with_head(mut self, head: &'a [u8]) -> Self {
        self.head = Some(head);
        self
    }
}

/// Decides what to do with one file.
pub fn decide(facts: Facts<'_>, limits: Limits) -> Decision {
    let Facts {
        path,
        size,
        probe,
        head,
    } = facts;

    if size < MIN_USEFUL_BYTES {
        return Decision::Skip(SkipReason::TooSmall);
    }
    if size > limits.max_file_bytes {
        return Decision::Skip(SkipReason::TooLargeForBudget);
    }

    let mut kind = kind_from_extension(path);

    // `.ts` is claimed by both MPEG transport streams and TypeScript. When the bytes
    // are available, the packet structure settles it; handing a source file to
    // ffmpeg would otherwise be a confusing failure rather than a clean skip.
    if kind == Kind::MpegTs
        && let Some(head) = head
        && !crate::classify::is_mpeg_ts(head)
    {
        kind = Kind::Other;
    }

    match kind {
        Kind::Jpeg => Decision::Convert(Recipe::JxlFromJpeg),

        // Only the bytes say whether a WebP is lossless, and only a lossless one
        // is worth touching: re-encoding a lossy WebP losslessly multiplies its
        // size, and re-encoding it lossily would sell quality for bytes, which
        // is not a trade this tool makes.
        Kind::Webp => match head {
            None => Decision::Skip(SkipReason::NeedsProbe),
            Some(head) if crate::classify::is_lossless_webp(head) => {
                Decision::Convert(Recipe::JxlFromWebp)
            }
            Some(_) => Decision::Skip(SkipReason::LossyNoGain),
        },

        Kind::Png | Kind::Gif | Kind::Bmp | Kind::Tiff => Decision::Convert(Recipe::JxlFromRaster),
        Kind::EfficientImage => Decision::Skip(SkipReason::AlreadyOptimal),

        Kind::Wav | Kind::Aiff => Decision::Convert(Recipe::Flac),
        Kind::Flac => Decision::Convert(Recipe::FlacRecompress),
        // `.m4a` is ALAC or AAC and only a probe can say which.
        Kind::M4a => match probe {
            None => Decision::Skip(SkipReason::NeedsProbe),
            Some(p) if p.audio_codecs.iter().any(|c| c == "alac") => {
                Decision::Convert(Recipe::FlacRecompress)
            }
            Some(_) => Decision::Skip(SkipReason::LossyNoGain),
        },
        Kind::LossyAudio => Decision::Skip(SkipReason::LossyNoGain),

        k if k.is_video() => decide_video(kind, probe, limits),
        _ => Decision::Skip(SkipReason::Unsupported),
    }
}

fn decide_video(kind: Kind, probe: Option<&MediaProbe>, limits: Limits) -> Decision {
    let Some(probe) = probe else {
        return Decision::Skip(SkipReason::NeedsProbe);
    };

    let Some(video) = probe.video.first() else {
        return Decision::Skip(SkipReason::Unsupported);
    };

    // More than one video stream means multi-angle or attachment structure that a
    // straight re-encode would mangle.
    if probe.video.len() > 1 {
        return Decision::Skip(SkipReason::VideoComplexStructure);
    }

    // Uncompressed and losslessly-coded sources are the only ones FFV1 actually
    // helps, and it helps enormously: rawvideo shrinks by around 93%.
    if is_uncompressed_or_lossless(&video.codec) {
        return Decision::Convert(Recipe::Ffv1);
    }

    // Transport streams get their container overhead back for free whenever the
    // AV1 tier declines them, so the fallback is decided before the AV1 gates.
    let ts_fallback = if kind == Kind::MpegTs {
        Decision::Convert(Recipe::TsRemux)
    } else {
        Decision::Skip(SkipReason::LossyNoGain)
    };

    if video.is_hdr() {
        return if kind == Kind::MpegTs {
            ts_fallback
        } else {
            Decision::Skip(SkipReason::VideoHdr)
        };
    }
    if video.codec == "av1" {
        return if kind == Kind::MpegTs {
            ts_fallback
        } else {
            Decision::Skip(SkipReason::VideoAlreadyAv1)
        };
    }
    if probe
        .duration_secs
        .is_some_and(|d| d < limits.min_video_secs)
    {
        return if kind == Kind::MpegTs {
            ts_fallback
        } else {
            Decision::Skip(SkipReason::VideoTooShort)
        };
    }
    if is_already_lean(video.pixels(), probe.video_bit_rate()) {
        return if kind == Kind::MpegTs {
            ts_fallback
        } else {
            Decision::Skip(SkipReason::VideoLowBitrate)
        };
    }
    if !limits.allow_video {
        // The lossless remux is still allowed: it needs no quality judgement.
        return if kind == Kind::MpegTs {
            ts_fallback
        } else {
            Decision::Skip(SkipReason::VideoTierDisabled)
        };
    }

    Decision::Convert(Recipe::Av1)
}

/// Codecs that store pixels uncompressed, or compress them without loss.
///
/// Only these are worth sending to FFV1. Intra-only is not the same thing as
/// lossless: ProRes, DNxHD, DV and MJPEG are all lossy DCT codecs that happen to
/// code each frame independently, and re-coding them losslessly preserves their
/// existing artefacts at a size FFV1 cannot beat — measured, ProRes at 520 KB
/// became 523 KB. They belong on the ordinary lossy path, where AV1 can actually
/// reduce them.
fn is_uncompressed_or_lossless(codec: &str) -> bool {
    matches!(
        codec,
        "rawvideo"
            | "huffyuv"
            | "ffvhuff"
            | "ffv1"
            | "magicyuv"
            | "utvideo"
            | "qtrle"
            | "v210"
            | "v410"
            | "r210"
            | "bmp"
            | "png"
    )
}

/// Normalises bitrate by frame area so the threshold means the same thing at 1080p
/// and 4K. A 1080p stream under roughly 2 Mbps, or 4K under about 8 Mbps, is
/// already compressed past the point where a transparent re-encode helps.
fn is_already_lean(pixels: u64, bit_rate: Option<u64>) -> bool {
    let (Some(bit_rate), true) = (bit_rate, pixels > 0) else {
        // Without the numbers, do not skip: the size gate will catch it later.
        return false;
    };
    (bit_rate as f64 / pixels as f64) < MIN_BITS_PER_PIXEL_SEC
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{MediaProbe, VideoStream};

    const BIG: u64 = 100 * 1024 * 1024;

    fn limits() -> Limits {
        Limits {
            max_file_bytes: 20 * 1024 * 1024 * 1024,
            allow_video: true,
            min_video_secs: DEFAULT_MIN_VIDEO_SECS,
        }
    }

    fn no_video() -> Limits {
        Limits {
            allow_video: false,
            ..limits()
        }
    }

    fn video(codec: &str, w: u32, h: u32, bit_rate: u64, secs: f64) -> MediaProbe {
        MediaProbe {
            format_names: vec!["mov".into(), "mp4".into()],
            duration_secs: Some(secs),
            bit_rate: Some(bit_rate),
            video: vec![VideoStream {
                codec: codec.into(),
                width: w,
                height: h,
                color_transfer: None,
                bit_rate: Some(bit_rate),
                has_dolby_vision: false,
            }],
            audio_codecs: vec!["aac".into()],
            subtitle_count: 0,
        }
    }

    /// A typical 1080p phone clip: the case the whole video tier exists for.
    fn phone_clip() -> MediaProbe {
        video("h264", 1920, 1080, 12_000_000, 120.0)
    }

    #[test]
    fn jpeg_gets_the_byte_reversible_recipe() {
        let d = decide(Facts::new("a/IMG_1.JPG", BIG), limits());
        assert_eq!(d, Decision::Convert(Recipe::JxlFromJpeg));
        assert!(Recipe::JxlFromJpeg.is_byte_reversible());
    }

    #[test]
    fn raster_images_get_pixel_identical_jxl() {
        for path in ["a.png", "a.gif", "a.bmp", "a.tiff"] {
            assert_eq!(
                decide(Facts::new(path, BIG), limits()),
                Decision::Convert(Recipe::JxlFromRaster),
                "{path}"
            );
        }
        // Pixel-identical is not byte-reversible, and must not claim to be.
        assert!(!Recipe::JxlFromRaster.is_byte_reversible());
        assert!(Recipe::JxlFromRaster.is_lossless());
    }

    #[test]
    fn efficient_images_are_left_alone() {
        for path in ["a.heic", "a.avif", "a.jxl"] {
            assert_eq!(
                decide(Facts::new(path, BIG), limits()),
                Decision::Skip(SkipReason::AlreadyOptimal),
                "{path}"
            );
        }
    }

    fn webp(chunk: &[u8; 4]) -> Vec<u8> {
        let mut head = b"RIFF\0\0\0\0WEBP".to_vec();
        head.extend_from_slice(chunk);
        head
    }

    /// A lossless WebP re-encodes about 6-7% smaller with its pixels intact; a
    /// lossy one grows severalfold. Only the bytes tell them apart, so the whole
    /// decision hangs on reading them.
    #[test]
    fn only_lossless_webp_goes_to_jxl() {
        let lossless = webp(b"VP8L");
        assert_eq!(
            decide(Facts::new("a.webp", BIG).with_head(&lossless), limits()),
            Decision::Convert(Recipe::JxlFromWebp)
        );

        let lossy = webp(b"VP8 ");
        assert_eq!(
            decide(Facts::new("a.webp", BIG).with_head(&lossy), limits()),
            Decision::Skip(SkipReason::LossyNoGain)
        );

        // The extended form can hold either, plus alpha and animation. Reading it
        // means walking the chunk list, so it is left alone rather than guessed at.
        let extended = webp(b"VP8X");
        assert_eq!(
            decide(Facts::new("a.webp", BIG).with_head(&extended), limits()),
            Decision::Skip(SkipReason::LossyNoGain)
        );
    }

    /// `plan` does not download, so it cannot know which kind of WebP it has.
    #[test]
    fn a_webp_without_its_bytes_needs_a_probe() {
        assert_eq!(
            decide(Facts::new("a.webp", BIG), limits()),
            Decision::Skip(SkipReason::NeedsProbe)
        );
    }

    /// Intake order guesses a recipe for anything it cannot judge yet. A WebP
    /// must not be queued as though it were a film.
    #[test]
    fn an_unjudged_webp_is_not_ranked_like_a_video() {
        let size = 1_000_000;
        assert_eq!(
            projected_saving("a.webp", size),
            size - (size as f64 * Recipe::JxlFromWebp.expected_ratio()) as u64
        );
        // And a WebP projects far less than a PNG of the same size, because it
        // is already compressed.
        assert!(projected_saving("a.png", size) > projected_saving("a.webp", size));
    }

    #[test]
    fn pcm_audio_becomes_flac() {
        assert_eq!(decide(Facts::new("a.wav", BIG), limits()), Decision::Convert(Recipe::Flac));
        assert_eq!(decide(Facts::new("a.aiff", BIG), limits()), Decision::Convert(Recipe::Flac));
    }

    #[test]
    fn lossy_audio_is_left_alone() {
        for path in ["a.mp3", "a.aac", "a.opus"] {
            assert_eq!(
                decide(Facts::new(path, BIG), limits()),
                Decision::Skip(SkipReason::LossyNoGain),
                "{path}"
            );
        }
    }

    /// `.m4a` may be ALAC or AAC; only a probe can tell, and guessing wrong would
    /// either waste work or transcode lossy audio a second time.
    #[test]
    fn m4a_needs_a_probe_to_tell_alac_from_aac() {
        assert_eq!(
            decide(Facts::new("a.m4a", BIG), limits()),
            Decision::Skip(SkipReason::NeedsProbe)
        );

        let alac = MediaProbe {
            audio_codecs: vec!["alac".into()],
            ..Default::default()
        };
        assert_eq!(
            decide(Facts::new("a.m4a", BIG).with_probe(&alac), limits()),
            Decision::Convert(Recipe::FlacRecompress)
        );

        let aac = MediaProbe {
            audio_codecs: vec!["aac".into()],
            ..Default::default()
        };
        assert_eq!(
            decide(Facts::new("a.m4a", BIG).with_probe(&aac), limits()),
            Decision::Skip(SkipReason::LossyNoGain)
        );
    }

    #[test]
    fn video_without_a_probe_is_deferred_not_guessed() {
        assert_eq!(
            decide(Facts::new("a.mp4", BIG), limits()),
            Decision::Skip(SkipReason::NeedsProbe)
        );
    }

    #[test]
    fn ordinary_lossy_video_goes_to_av1() {
        assert_eq!(
            decide(Facts::new("a.mp4", BIG).with_probe(&phone_clip()), limits()),
            Decision::Convert(Recipe::Av1)
        );
    }

    #[test]
    fn av1_tier_requires_explicit_permission() {
        assert_eq!(
            decide(Facts::new("a.mp4", BIG).with_probe(&phone_clip()), no_video()),
            Decision::Skip(SkipReason::VideoTierDisabled)
        );
    }

    #[test]
    fn av1_is_the_only_irreversible_recipe() {
        assert!(!Recipe::Av1.is_lossless());
        for r in [
            Recipe::JxlFromJpeg,
            Recipe::JxlFromRaster,
            Recipe::Flac,
            Recipe::FlacRecompress,
            Recipe::TsRemux,
            Recipe::Ffv1,
        ] {
            assert!(r.is_lossless(), "{r:?} should be lossless");
        }
    }

    #[test]
    fn hdr_video_is_never_touched() {
        let mut hdr = phone_clip();
        hdr.video[0].color_transfer = Some("smpte2084".into());
        assert_eq!(
            decide(Facts::new("a.mp4", BIG).with_probe(&hdr), limits()),
            Decision::Skip(SkipReason::VideoHdr)
        );
    }

    #[test]
    fn dolby_vision_is_never_touched() {
        let mut dv = phone_clip();
        dv.video[0].has_dolby_vision = true;
        assert_eq!(
            decide(Facts::new("a.mp4", BIG).with_probe(&dv), limits()),
            Decision::Skip(SkipReason::VideoHdr)
        );
    }

    #[test]
    fn av1_sources_are_not_re_encoded() {
        let probe = video("av1", 1920, 1080, 8_000_000, 120.0);
        assert_eq!(
            decide(Facts::new("a.mp4", BIG).with_probe(&probe), limits()),
            Decision::Skip(SkipReason::VideoAlreadyAv1)
        );
    }

    /// The floor is a setting, and its default is off: a short clip saves in
    /// proportion to its length like any other, so excluding it is a judgement
    /// about CPU time rather than a property of the file.
    #[test]
    fn short_clips_convert_by_default() {
        let probe = video("h264", 1920, 1080, 12_000_000, 10.0);
        assert_eq!(DEFAULT_MIN_VIDEO_SECS, 0.0);
        assert_eq!(
            decide(Facts::new("a.mp4", BIG).with_probe(&probe), limits()),
            Decision::Convert(Recipe::Av1)
        );
    }

    #[test]
    fn a_configured_floor_skips_clips_under_it() {
        let floor = Limits {
            min_video_secs: 30.0,
            ..limits()
        };
        let short = video("h264", 1920, 1080, 12_000_000, 10.0);
        assert_eq!(
            decide(Facts::new("a.mp4", BIG).with_probe(&short), floor),
            Decision::Skip(SkipReason::VideoTooShort)
        );

        // The boundary belongs to the longer side: exactly the floor passes.
        let exact = video("h264", 1920, 1080, 12_000_000, 30.0);
        assert_eq!(
            decide(Facts::new("a.mp4", BIG).with_probe(&exact), floor),
            Decision::Convert(Recipe::Av1)
        );
    }

    /// A video whose duration ffprobe could not report must not be caught by the
    /// floor; an unknown length is not a short one.
    #[test]
    fn an_unknown_duration_is_not_treated_as_short() {
        let mut probe = video("h264", 1920, 1080, 12_000_000, 10.0);
        probe.duration_secs = None;
        let floor = Limits {
            min_video_secs: 30.0,
            ..limits()
        };
        assert_eq!(
            decide(Facts::new("a.mp4", BIG).with_probe(&probe), floor),
            Decision::Convert(Recipe::Av1)
        );
    }

    /// The bitrate gate is normalised by area, so the same rule works at any size.
    #[test]
    fn already_lean_video_is_skipped_at_both_1080p_and_4k() {
        let lean_1080p = video("h264", 1920, 1080, 1_500_000, 120.0);
        assert_eq!(
            decide(Facts::new("a.mp4", BIG).with_probe(&lean_1080p), limits()),
            Decision::Skip(SkipReason::VideoLowBitrate)
        );

        let lean_4k = video("hevc", 3840, 2160, 6_000_000, 120.0);
        assert_eq!(
            decide(Facts::new("a.mp4", BIG).with_probe(&lean_4k), limits()),
            Decision::Skip(SkipReason::VideoLowBitrate)
        );

        // The same 6 Mbps at 1080p is not lean, and should convert.
        let fat_1080p = video("h264", 1920, 1080, 6_000_000, 120.0);
        assert_eq!(
            decide(Facts::new("a.mp4", BIG).with_probe(&fat_1080p), limits()),
            Decision::Convert(Recipe::Av1)
        );
    }

    #[test]
    fn uncompressed_sources_get_truly_lossless_ffv1() {
        for codec in ["rawvideo", "huffyuv", "ffvhuff", "utvideo", "v210"] {
            let probe = video(codec, 1920, 1080, 100_000_000, 120.0);
            assert_eq!(
                decide(Facts::new("a.mov", BIG).with_probe(&probe), limits()),
                Decision::Convert(Recipe::Ffv1),
                "{codec}"
            );
        }
    }

    /// Intra-only is not the same as lossless. ProRes, DNxHD, DV and MJPEG are
    /// lossy DCT codecs, and coding them losslessly to FFV1 makes them *bigger*
    /// (measured: 520 KB of ProRes became 523 KB). They belong on the lossy path.
    #[test]
    fn lossy_intra_codecs_do_not_go_to_ffv1() {
        for codec in ["prores", "dnxhd", "dvvideo", "mjpeg"] {
            let probe = video(codec, 1920, 1080, 100_000_000, 120.0);
            assert_eq!(
                decide(Facts::new("a.mov", BIG).with_probe(&probe), limits()),
                Decision::Convert(Recipe::Av1),
                "{codec}"
            );
        }
    }

    /// FFV1 is lossless, so it does not need the --allow-video gate.
    #[test]
    fn ffv1_does_not_need_the_video_gate() {
        let probe = video("rawvideo", 1920, 1080, 100_000_000, 120.0);
        assert_eq!(
            decide(Facts::new("a.mov", BIG).with_probe(&probe), no_video()),
            Decision::Convert(Recipe::Ffv1)
        );
    }

    #[test]
    fn multiple_video_streams_are_left_alone() {
        let mut probe = phone_clip();
        probe.video.push(probe.video[0].clone());
        assert_eq!(
            decide(Facts::new("a.mkv", BIG).with_probe(&probe), limits()),
            Decision::Skip(SkipReason::VideoComplexStructure)
        );
    }

    /// A transport stream that the AV1 tier declines still gets its container
    /// overhead back, which is free and lossless.
    #[test]
    fn transport_streams_fall_back_to_a_lossless_remux() {
        let cases = [
            ("hdr", {
                let mut p = phone_clip();
                p.video[0].color_transfer = Some("smpte2084".into());
                p
            }),
            ("already av1", video("av1", 1920, 1080, 8_000_000, 120.0)),
            ("low bitrate", video("h264", 1920, 1080, 1_000_000, 120.0)),
        ];
        for (label, probe) in cases {
            assert_eq!(
                decide(Facts::new("a.ts", BIG).with_probe(&probe), limits()),
                Decision::Convert(Recipe::TsRemux),
                "{label}"
            );
        }

        // The duration floor declines the same way, so it needs a run that sets one.
        let floor = Limits {
            min_video_secs: 30.0,
            ..limits()
        };
        let short = video("h264", 1920, 1080, 12_000_000, 5.0);
        assert_eq!(
            decide(Facts::new("a.ts", BIG).with_probe(&short), floor),
            Decision::Convert(Recipe::TsRemux),
            "too short"
        );
    }

    #[test]
    fn transport_streams_remux_even_without_the_video_gate() {
        assert_eq!(
            decide(Facts::new("a.ts", BIG).with_probe(&phone_clip()), no_video()),
            Decision::Convert(Recipe::TsRemux)
        );
    }

    /// With the gate open, a qualifying transport stream goes all the way to AV1,
    /// which subsumes the container win.
    #[test]
    fn qualifying_transport_streams_still_reach_av1() {
        assert_eq!(
            decide(Facts::new("a.ts", BIG).with_probe(&phone_clip()), limits()),
            Decision::Convert(Recipe::Av1)
        );
    }

    #[test]
    fn oversized_files_are_skipped() {
        let limits = Limits {
            max_file_bytes: 1024,
            ..limits()
        };
        assert_eq!(
            decide(Facts::new("a.jpg", 10 * 1024), limits),
            Decision::Skip(SkipReason::TooLargeForBudget)
        );
    }

    /// Ordering must not depend on how the run was configured, or a run resumed
    /// with different flags would revisit files in a different sequence.
    #[test]
    fn projected_saving_ignores_run_settings() {
        // Far past any plausible max_file_bytes, and video, which needs a flag.
        let huge = 500u64 * 1024 * 1024 * 1024;
        assert!(projected_saving("a.jpg", huge) > 0);
        assert!(projected_saving("a.mkv", huge) > 0);
    }

    /// Video is where the space is. It cannot be judged without a probe, so the
    /// sort key falls back to the AV1 ratio rather than ranking it at zero.
    #[test]
    fn video_is_ranked_on_the_av1_ratio_not_at_zero() {
        let size = 1_000_000;
        assert_eq!(
            decide(Facts::new("a.mkv", size), Limits::unbounded()),
            Decision::Skip(SkipReason::NeedsProbe),
            "a video with no probe cannot be judged"
        );
        assert_eq!(projected_saving("a.mkv", size), 300_000);
        assert!(
            projected_saving("a.mkv", size) > projected_saving("a.jpg", size),
            "a video must outrank an image of the same size"
        );
    }

    #[test]
    fn projected_saving_follows_the_recipe_ratio() {
        // jpeg -> jxl keeps 80%, png -> jxl keeps 65%.
        assert_eq!(projected_saving("a.jpg", 100_000), 20_000);
        assert_eq!(projected_saving("a.png", 100_000), 35_000);
        // Nothing to do, nothing projected.
        assert_eq!(projected_saving("a.bin", 100_000), 0);
        assert_eq!(projected_saving("a.jpg", 100), 0, "below the useful floor");
    }

    #[test]
    fn tiny_files_are_skipped() {
        assert_eq!(
            decide(Facts::new("a.jpg", 100), limits()),
            Decision::Skip(SkipReason::TooSmall)
        );
        assert_eq!(
            decide(Facts::new("a.jpg", 0), limits()),
            Decision::Skip(SkipReason::TooSmall)
        );
    }

    /// A TypeScript source file also ends in `.ts`. Without the byte check it would
    /// be sent to ffmpeg, which is a confusing failure instead of a clean skip.
    #[test]
    fn typescript_named_ts_is_not_treated_as_video() {
        let source = b"import { Foo } from './foo';\nexport const bar = 1;\n".repeat(200);
        let decision = decide(
            Facts::new("src/app.ts", source.len() as u64).with_head(&source),
            limits(),
        );
        assert_eq!(decision, Decision::Skip(SkipReason::Unsupported));
    }

    #[test]
    fn a_real_transport_stream_survives_the_byte_check() {
        let mut ts = vec![0u8; 188 * 8];
        for i in 0..8 {
            ts[i * 188] = 0x47;
        }
        let decision = decide(
            Facts::new("rec.ts", BIG)
                .with_head(&ts)
                .with_probe(&phone_clip()),
            no_video(),
        );
        assert_eq!(decision, Decision::Convert(Recipe::TsRemux));
    }

    /// Without the bytes we cannot tell, so the file waits for a probe rather than
    /// being guessed either way.
    #[test]
    fn ambiguous_ts_without_bytes_defers() {
        assert_eq!(
            decide(Facts::new("rec.ts", BIG), limits()),
            Decision::Skip(SkipReason::NeedsProbe)
        );
    }

    #[test]
    fn unknown_files_are_skipped() {
        assert_eq!(
            decide(Facts::new("notes.txt", BIG), limits()),
            Decision::Skip(SkipReason::Unsupported)
        );
    }

    /// AV1 output keeps the source container so QuickTime metadata survives; only
    /// containers that cannot hold AV1 are rewritten.
    #[test]
    fn av1_keeps_the_source_container() {
        assert_eq!(Recipe::Av1.output_extension("mov"), "mov");
        assert_eq!(Recipe::Av1.output_extension("MOV"), "mov");
        assert_eq!(Recipe::Av1.output_extension("mp4"), "mp4");
        assert_eq!(Recipe::Av1.output_extension("mkv"), "mkv");
        // MPEG-TS cannot carry AV1 in any shipping ffmpeg muxer.
        assert_eq!(Recipe::Av1.output_extension("ts"), "mp4");
    }

    #[test]
    fn output_extensions_are_fixed_for_the_rest() {
        assert_eq!(Recipe::JxlFromJpeg.output_extension("jpg"), "jxl");
        assert_eq!(Recipe::Flac.output_extension("wav"), "flac");
        assert_eq!(Recipe::TsRemux.output_extension("ts"), "mp4");
        assert_eq!(Recipe::Ffv1.output_extension("mov"), "mkv");
    }

    #[test]
    fn projected_ratios_are_plausible() {
        for r in [
            Recipe::JxlFromJpeg,
            Recipe::JxlFromRaster,
            Recipe::Flac,
            Recipe::FlacRecompress,
            Recipe::TsRemux,
            Recipe::Ffv1,
            Recipe::Av1,
        ] {
            let ratio = r.expected_ratio();
            assert!(ratio > 0.0 && ratio < 1.0, "{r:?} ratio {ratio}");
            assert!(
                1.0 - ratio >= MIN_GAIN,
                "{r:?} projects less than the minimum worthwhile gain"
            );
        }
    }
}
