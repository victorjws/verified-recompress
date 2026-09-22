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
    /// Already-efficient stills: HEIC, AVIF, lossy WebP, existing JXL.
    EfficientImage,
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
        "heic" | "heif" | "avif" | "webp" | "jxl" => Kind::EfficientImage,
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

/// Distinguishes an MPEG transport stream from a TypeScript source file, both of
/// which claim `.ts`.
///
/// A transport stream is a run of 188-byte packets each beginning with 0x47.
/// Checking several packets in a row rather than just the first byte avoids
/// matching text that happens to start with `G`.
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
    fn maps_extensions() {
        assert_eq!(kind_from_extension("a/b/IMG_0001.JPG"), Kind::Jpeg);
        assert_eq!(kind_from_extension("x.jpeg"), Kind::Jpeg);
        assert_eq!(kind_from_extension("x.PNG"), Kind::Png);
        assert_eq!(kind_from_extension("x.heic"), Kind::EfficientImage);
        assert_eq!(kind_from_extension("x.jxl"), Kind::EfficientImage);
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
