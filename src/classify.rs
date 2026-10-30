//! Works out what a file actually is.
//!
//! Classification happens in two tiers. The cheap tier reads only the path, which
//! is all `plan` can afford: probing a remote file means downloading it. The full
//! tier adds an `ffprobe` result and is used by the pipeline once the bytes are
//! local. Both feed the same [`crate::policy`] decision function.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::process::Command;

/// Transport-stream packets are 188 bytes and each starts with this sync byte.
const TS_SYNC: u8 = 0x47;
const TS_PACKET: usize = 188;

/// What the file extension suggests. Extensions lie, so anything consequential is
/// confirmed against the bytes or `ffprobe` before it is acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Jpeg,
    Png,
    Gif,
    Bmp,
    Tiff,
    /// Already-efficient stills: HEIC, AVIF, existing JXL.
    EfficientImage,
    /// `.webp`, which may be lossless (VP8L) or lossy (VP8). Only the magic
    /// bytes say which, and the answer decides whether JXL can help at all.
    Webp,
    /// Uncompressed PCM containers.
    Wav,
    Aiff,
    Flac,
    /// `.m4a`, which may hold ALAC (lossless) or AAC (lossy). Needs a probe.
    M4a,
    LossyAudio,
    Mp4,
    Mov,
    Mkv,
    /// `.ts`, `.m2ts`, `.mts`. Ambiguous with TypeScript; confirmed by magic bytes.
    MpegTs,
    /// Containers that cannot hold AV1 and must be rewritten to MP4.
    LegacyVideo,
    Other,
}

impl Kind {
    /// The still-image kinds, whose extension is checked against the bytes.
    pub fn is_image(self) -> bool {
        matches!(
            self,
            Kind::Jpeg
                | Kind::Png
                | Kind::Gif
                | Kind::Bmp
                | Kind::Tiff
                | Kind::EfficientImage
                | Kind::Webp
        )
    }

    pub fn is_video(self) -> bool {
        matches!(
            self,
            Kind::Mp4 | Kind::Mov | Kind::Mkv | Kind::MpegTs | Kind::LegacyVideo
        )
    }
}

pub fn kind_from_extension(path: &str) -> Kind {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    match ext.as_str() {
        "jpg" | "jpeg" | "jpe" | "jfif" => Kind::Jpeg,
        "png" => Kind::Png,
        "gif" => Kind::Gif,
        "bmp" | "dib" => Kind::Bmp,
        "tif" | "tiff" => Kind::Tiff,
        "heic" | "heif" | "avif" | "jxl" => Kind::EfficientImage,
        "webp" => Kind::Webp,
        "wav" | "wave" => Kind::Wav,
        "aif" | "aiff" | "aifc" => Kind::Aiff,
        "flac" => Kind::Flac,
        "m4a" => Kind::M4a,
        "mp3" | "aac" | "opus" | "ogg" | "oga" | "wma" => Kind::LossyAudio,
        "mp4" | "m4v" => Kind::Mp4,
        "mov" | "qt" => Kind::Mov,
        "mkv" | "webm" => Kind::Mkv,
        "ts" | "m2ts" | "mts" | "m2t" => Kind::MpegTs,
        "avi" | "wmv" | "flv" | "3gp" | "mpg" | "mpeg" | "vob" => Kind::LegacyVideo,
        _ => Kind::Other,
    }
}

/// What an image file's bytes say it is, whatever its name claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Gif,
    Jpeg,
    Webp,
    Bmp,
    Tiff,
    /// HEIC, HEIF or AVIF.
    Heif,
    Jxl,
}

impl ImageFormat {
    pub fn kind(self) -> Kind {
        match self {
            ImageFormat::Png => Kind::Png,
            ImageFormat::Gif => Kind::Gif,
            ImageFormat::Jpeg => Kind::Jpeg,
            ImageFormat::Webp => Kind::Webp,
            ImageFormat::Bmp => Kind::Bmp,
            ImageFormat::Tiff => Kind::Tiff,
            ImageFormat::Heif | ImageFormat::Jxl => Kind::EfficientImage,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Gif => "gif",
            ImageFormat::Jpeg => "jpeg",
            ImageFormat::Webp => "webp",
            ImageFormat::Bmp => "bmp",
            ImageFormat::Tiff => "tiff",
            ImageFormat::Heif => "heif",
            ImageFormat::Jxl => "jxl",
        }
    }
}

/// Identifies an image by its leading bytes.
///
/// Files downloaded from the web or a messenger routinely carry the wrong
/// extension — a WebP saved as `.png` is the common one — and the encoder
/// picked by name then fails on a format it cannot read. `None` means the bytes
/// are not a recognised image, which says nothing about the extension.
pub fn sniff_image(head: &[u8]) -> Option<ImageFormat> {
    const HEIF_BRANDS: [&[u8; 4]; 10] = [
        b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"mif1", b"msf1", b"avif", b"avis",
    ];
    // The DIB header sizes in use. "BM" alone is two bytes and would claim any
    // text file that begins with those letters.
    const BMP_HEADERS: [u32; 7] = [12, 40, 52, 56, 64, 108, 124];

    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(ImageFormat::Png);
    }
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(ImageFormat::Jpeg);
    }
    if head.starts_with(b"GIF8") {
        return Some(ImageFormat::Gif);
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Some(ImageFormat::Webp);
    }
    if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        return Some(ImageFormat::Tiff);
    }
    if head.starts_with(b"BM")
        && head.len() >= 18
        && BMP_HEADERS.contains(&u32::from_le_bytes([head[14], head[15], head[16], head[17]]))
    {
        return Some(ImageFormat::Bmp);
    }
    if head.len() >= 12
        && &head[4..8] == b"ftyp"
        && HEIF_BRANDS.iter().any(|b| &head[8..12] == b.as_slice())
    {
        return Some(ImageFormat::Heif);
    }
    if head.starts_with(&[0xFF, 0x0A])
        || head.starts_with(&[0, 0, 0, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A])
    {
        return Some(ImageFormat::Jxl);
    }
    None
}

/// Distinguishes an MPEG transport stream from a TypeScript source file, both of
/// which claim `.ts`.
///
/// A transport stream is a run of 188-byte packets each beginning with 0x47.
/// Checking several packets in a row rather than just the first byte avoids
/// matching text that happens to start with `G`.
/// Whether a WebP is the lossless (VP8L) variety.
///
/// Measured on real images: lossless WebP re-encodes to JXL about 6-7% smaller
/// with the pixels intact, while a lossy one grows sevenfold or more, because
/// its compression artifacts are structure the new encoder has to spend bits
/// reproducing. So the two have to be told apart before anything is downloaded,
/// and the RIFF chunk at byte 12 says which is which.
///
/// `VP8X` is the extended form and may hold either, plus alpha or animation.
/// Reading it properly means walking the chunk list, so it reads as "not
/// known lossless" and is left alone.
pub fn is_lossless_webp(head: &[u8]) -> bool {
    const CHUNK: usize = 12;
    if head.len() < CHUNK + 4 {
        return false;
    }
    &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" && &head[CHUNK..CHUNK + 4] == b"VP8L"
}

pub fn is_mpeg_ts(head: &[u8]) -> bool {
    const REQUIRED: usize = 4;
    if head.len() < TS_PACKET * REQUIRED {
        // Too little data to be sure; a real transport stream is never this small.
        return false;
    }
    (0..REQUIRED).all(|i| head[i * TS_PACKET] == TS_SYNC)
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoStream {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// `smpte2084` (PQ) or `arib-std-b67` (HLG) mean HDR.
    pub color_transfer: Option<String>,
    pub bit_rate: Option<u64>,
    pub has_dolby_vision: bool,
}

impl VideoStream {
    pub fn is_hdr(&self) -> bool {
        self.has_dolby_vision
            || matches!(
                self.color_transfer.as_deref(),
                Some("smpte2084") | Some("arib-std-b67")
            )
    }

    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MediaProbe {
    /// ffprobe reports a comma-separated list, e.g. `mov,mp4,m4a,3gp,3g2,mj2`.
    pub format_names: Vec<String>,
    pub duration_secs: Option<f64>,
    pub bit_rate: Option<u64>,
    pub video: Vec<VideoStream>,
    pub audio_codecs: Vec<String>,
    pub subtitle_count: usize,
}

impl MediaProbe {
    pub fn has_format(&self, name: &str) -> bool {
        self.format_names.iter().any(|f| f == name)
    }

    /// Bits per second for the video, falling back to the container rate when the
    /// stream does not carry its own.
    pub fn video_bit_rate(&self) -> Option<u64> {
        self.video.first().and_then(|v| v.bit_rate).or(self.bit_rate)
    }
}

#[derive(Debug, Deserialize)]
struct RawProbe {
    #[serde(default)]
    format: RawFormat,
    #[serde(default)]
    streams: Vec<RawStream>,
}

#[derive(Debug, Default, Deserialize)]
struct RawFormat {
    format_name: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    color_transfer: Option<String>,
    bit_rate: Option<String>,
    #[serde(default)]
    side_data_list: Vec<RawSideData>,
}

#[derive(Debug, Deserialize)]
struct RawSideData {
    side_data_type: Option<String>,
}

/// ffprobe emits every numeric field as a string.
fn parse_num<T: std::str::FromStr>(s: &Option<String>) -> Option<T> {
    s.as_deref().and_then(|v| v.parse().ok())
}

pub fn parse_probe(json: &str) -> Result<MediaProbe> {
    let raw: RawProbe = serde_json::from_str(json).context("failed to parse ffprobe JSON")?;

    let mut probe = MediaProbe {
        format_names: raw
            .format
            .format_name
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        duration_secs: parse_num(&raw.format.duration),
        bit_rate: parse_num(&raw.format.bit_rate),
        ..Default::default()
    };

    for stream in raw.streams {
        match stream.codec_type.as_deref() {
            Some("video") => probe.video.push(VideoStream {
                codec: stream.codec_name.unwrap_or_default(),
                width: stream.width,
                height: stream.height,
                color_transfer: stream.color_transfer,
                bit_rate: parse_num(&stream.bit_rate),
                has_dolby_vision: stream.side_data_list.iter().any(|sd| {
                    sd.side_data_type
                        .as_deref()
                        .is_some_and(|t| t.to_ascii_lowercase().contains("dovi"))
                }),
            }),
            Some("audio") => probe.audio_codecs.push(stream.codec_name.unwrap_or_default()),
            Some("subtitle") => probe.subtitle_count += 1,
            _ => {}
        }
    }
    Ok(probe)
}

/// Runs `ffprobe` against a local file.
pub async fn probe_file(path: &Path) -> Result<MediaProbe> {
    let mut cmd = Command::new("ffprobe");
    cmd.args([
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ])
    .arg(path);
    tracing::debug!("run {}", crate::proc::describe(cmd.as_std()));
    let output = cmd
        .output()
        .await
        .context("failed to execute ffprobe")?;

    if !output.status.success() {
        bail!(
            "ffprobe failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_probe(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from ffprobe 9.0.1 on an H.264 + AAC MP4.
    const H264_MP4: &str = r#"{
        "streams": [
            {"index":0,"codec_name":"h264","codec_type":"video","width":320,"height":240,
             "bit_rate":"288136","nb_frames":"60","duration":"2.000000"},
            {"index":1,"codec_name":"aac","codec_type":"audio","bit_rate":"69734"}
        ],
        "format": {"format_name":"mov,mp4,m4a,3gp,3g2,mj2","duration":"2.000000",
                   "bit_rate":"372836","size":"93209"}
    }"#;

    const PRORES_MOV: &str = r#"{
        "streams": [{"codec_name":"prores","codec_type":"video","width":320,"height":240,
                     "bit_rate":"6249428"}],
        "format": {"format_name":"mov,mp4,m4a,3gp,3g2,mj2","duration":"2.000000",
                   "bit_rate":"6253416"}
    }"#;

    #[test]
    fn sniffs_images_by_their_bytes() {
        let cases: [(&[u8], ImageFormat); 7] = [
            (b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR", ImageFormat::Png),
            (&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10], ImageFormat::Jpeg),
            (b"GIF89a\x01\0\x01\0", ImageFormat::Gif),
            (b"RIFF\x10\0\0\0WEBPVP8L", ImageFormat::Webp),
            (b"II*\0\x08\0\0\0", ImageFormat::Tiff),
            (b"MM\0*\0\0\0\x08", ImageFormat::Tiff),
            (&[0xFF, 0x0A, 0, 0], ImageFormat::Jxl),
        ];
        for (head, want) in cases {
            assert_eq!(sniff_image(head), Some(want), "{head:?}");
        }

        let mut bmp = b"BM\0\0\0\0\0\0\0\0\x36\0\0\0".to_vec();
        bmp.extend_from_slice(&40u32.to_le_bytes());
        assert_eq!(sniff_image(&bmp), Some(ImageFormat::Bmp));

        let heic = b"\0\0\0\x18ftypheic\0\0\0\0";
        assert_eq!(sniff_image(heic), Some(ImageFormat::Heif));
        let avif = b"\0\0\0\x1cftypavif\0\0\0\0";
        assert_eq!(sniff_image(avif), Some(ImageFormat::Heif));
        let boxed = [0, 0, 0, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A];
        assert_eq!(sniff_image(&boxed), Some(ImageFormat::Jxl));
    }

    #[test]
    fn sniffing_declines_what_it_cannot_vouch_for() {
        assert_eq!(sniff_image(b""), None);
        assert_eq!(sniff_image(b"\x89PN"), None);
        // Text that happens to begin with the BMP magic.
        assert_eq!(sniff_image(b"BM is a stock ticker, not a bitmap."), None);
        // An MP4 is an ftyp box too, but not an image brand.
        assert_eq!(sniff_image(b"\0\0\0\x18ftypisom\0\0\0\0"), None);
        assert_eq!(sniff_image(b"just some text"), None);
    }

    #[test]
    fn sniffed_formats_map_to_kinds() {
        assert_eq!(ImageFormat::Webp.kind(), Kind::Webp);
        assert_eq!(ImageFormat::Heif.kind(), Kind::EfficientImage);
        assert_eq!(ImageFormat::Jxl.kind(), Kind::EfficientImage);
        assert!(Kind::Png.is_image());
        assert!(!Kind::Mp4.is_image());
        assert!(!Kind::Wav.is_image());
    }

    #[test]
    fn maps_extensions() {
        assert_eq!(kind_from_extension("a/b/IMG_0001.JPG"), Kind::Jpeg);
        assert_eq!(kind_from_extension("x.jpeg"), Kind::Jpeg);
        assert_eq!(kind_from_extension("x.PNG"), Kind::Png);
        assert_eq!(kind_from_extension("x.heic"), Kind::EfficientImage);
        assert_eq!(kind_from_extension("x.jxl"), Kind::EfficientImage);
        assert_eq!(kind_from_extension("x.webp"), Kind::Webp);
        assert_eq!(kind_from_extension("x.wav"), Kind::Wav);
        assert_eq!(kind_from_extension("x.m4a"), Kind::M4a);
        assert_eq!(kind_from_extension("x.mp3"), Kind::LossyAudio);
        assert_eq!(kind_from_extension("x.mov"), Kind::Mov);
        assert_eq!(kind_from_extension("x.m2ts"), Kind::MpegTs);
        assert_eq!(kind_from_extension("x.avi"), Kind::LegacyVideo);
        assert_eq!(kind_from_extension("README"), Kind::Other);
        assert_eq!(kind_from_extension("x.rs"), Kind::Other);
    }

    #[test]
    fn video_kinds_are_grouped() {
        assert!(Kind::Mp4.is_video());
        assert!(Kind::MpegTs.is_video());
        assert!(!Kind::Jpeg.is_video());
        assert!(!Kind::Wav.is_video());
    }

    /// `.ts` is both MPEG transport stream and TypeScript. Acting on the extension
    /// alone would hand a source file to ffmpeg.
    #[test]
    fn transport_stream_is_detected_by_sync_bytes() {
        let mut ts = vec![0u8; TS_PACKET * 4];
        for i in 0..4 {
            ts[i * TS_PACKET] = TS_SYNC;
        }
        assert!(is_mpeg_ts(&ts));
    }

    #[test]
    fn typescript_source_is_not_a_transport_stream() {
        let source = b"import { Foo } from './foo';\nexport const bar = 1;\n".repeat(20);
        assert!(!is_mpeg_ts(&source));
    }

    /// Text beginning with 'G' (0x47) must not pass on the first byte alone.
    #[test]
    fn leading_g_alone_is_not_a_transport_stream() {
        let mut text = vec![b'G'; TS_PACKET * 4];
        text[TS_PACKET] = b'x';
        assert!(!is_mpeg_ts(&text));
    }

    #[test]
    fn too_short_to_judge_is_not_a_transport_stream() {
        assert!(!is_mpeg_ts(&[TS_SYNC; 100]));
        assert!(!is_mpeg_ts(&[]));
    }

    #[test]
    fn parses_h264_mp4_probe() {
        let probe = parse_probe(H264_MP4).unwrap();
        // ffprobe reports a comma list here, not a single name.
        assert!(probe.has_format("mp4"));
        assert!(probe.has_format("mov"));
        assert_eq!(probe.duration_secs, Some(2.0));
        assert_eq!(probe.bit_rate, Some(372_836));
        assert_eq!(probe.video.len(), 1);
        assert_eq!(probe.video[0].codec, "h264");
        assert_eq!(probe.video[0].width, 320);
        assert_eq!(probe.audio_codecs, vec!["aac".to_string()]);
        assert_eq!(probe.subtitle_count, 0);
    }

    #[test]
    fn parses_prores_probe() {
        let probe = parse_probe(PRORES_MOV).unwrap();
        assert_eq!(probe.video[0].codec, "prores");
        assert_eq!(probe.video_bit_rate(), Some(6_249_428));
    }

    #[test]
    fn video_bit_rate_falls_back_to_container() {
        let json = r#"{"streams":[{"codec_name":"h264","codec_type":"video"}],
                       "format":{"format_name":"matroska,webm","bit_rate":"5000000"}}"#;
        assert_eq!(parse_probe(json).unwrap().video_bit_rate(), Some(5_000_000));
    }

    #[test]
    fn sdr_video_is_not_hdr() {
        assert!(!parse_probe(H264_MP4).unwrap().video[0].is_hdr());
    }

    #[test]
    fn pq_and_hlg_transfers_are_hdr() {
        for transfer in ["smpte2084", "arib-std-b67"] {
            let json = format!(
                r#"{{"streams":[{{"codec_name":"hevc","codec_type":"video","color_transfer":"{transfer}"}}],
                     "format":{{"format_name":"mov,mp4"}}}}"#
            );
            assert!(
                parse_probe(&json).unwrap().video[0].is_hdr(),
                "{transfer} should count as HDR"
            );
        }
    }

    /// Dolby Vision metadata cannot survive a re-encode, so it must be detected.
    #[test]
    fn dolby_vision_side_data_is_hdr() {
        let json = r#"{"streams":[{"codec_name":"hevc","codec_type":"video",
                        "side_data_list":[{"side_data_type":"DOVI configuration record"}]}],
                       "format":{"format_name":"mov,mp4"}}"#;
        let probe = parse_probe(json).unwrap();
        assert!(probe.video[0].has_dolby_vision);
        assert!(probe.video[0].is_hdr());
    }

    #[test]
    fn counts_multiple_streams() {
        let json = r#"{"streams":[
            {"codec_name":"h264","codec_type":"video"},
            {"codec_name":"hevc","codec_type":"video"},
            {"codec_name":"aac","codec_type":"audio"},
            {"codec_name":"ac3","codec_type":"audio"},
            {"codec_name":"subrip","codec_type":"subtitle"}],
            "format":{"format_name":"matroska,webm"}}"#;
        let probe = parse_probe(json).unwrap();
        assert_eq!(probe.video.len(), 2);
        assert_eq!(probe.audio_codecs.len(), 2);
        assert_eq!(probe.subtitle_count, 1);
    }

    #[test]
    fn empty_probe_is_tolerated() {
        let probe = parse_probe(r#"{"streams":[],"format":{}}"#).unwrap();
        assert!(probe.video.is_empty());
        assert_eq!(probe.duration_secs, None);
    }

    #[test]
    fn malformed_probe_errors() {
        assert!(parse_probe("not json").is_err());
    }
}
