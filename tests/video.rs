//! Video tier tests against the real encoders.
//!
//! The AV1 tier is the only conversion in the project that cannot be undone, so
//! these check that its guard rails actually hold: that a remux really is a
//! stream copy, that dropped packets are detected rather than tolerated, and that
//! an encode which misses the quality gate is rejected instead of accepted.

use std::path::{Path, PathBuf};
use std::process::Command;

use verified_recompress::classify;
use verified_recompress::convert::{self, Fidelity, VideoOptions};
use verified_recompress::convert::video_av1;
use verified_recompress::convert::video_lossless::{self, StreamDigest};
use verified_recompress::policy::Recipe;
use verified_recompress::vmaf;

fn have(tool: &str, flag: &str) -> bool {
    Command::new(tool)
        .arg(flag)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

macro_rules! require {
    ($($tool:expr => $flag:expr),+ $(,)?) => {
        $(if !have($tool, $flag) {
            eprintln!("skipping: {} is not installed", $tool);
            return;
        })+
    };
}

struct Work {
    dir: tempfile::TempDir,
}

impl Work {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn ffmpeg(&self, name: &str, args: &[&str]) -> PathBuf {
        let out = self.path(name);
        let status = Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y"])
            .args(args)
            .arg(&out)
            .status()
            .expect("failed to run ffmpeg");
        assert!(status.success(), "ffmpeg could not build {name}");
        out
    }

    /// A short H.264 clip with AAC audio: what a phone produces, in miniature.
    fn h264_clip(&self, name: &str, seconds: u32) -> PathBuf {
        self.ffmpeg(
            name,
            &[
                "-f", "lavfi", "-i", &format!("testsrc2=size=320x240:rate=15:duration={seconds}"),
                "-f", "lavfi", "-i", &format!("sine=frequency=440:duration={seconds}"),
                "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest",
            ],
        )
    }
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

/// A transport stream rewritten as MP4 must carry the encoded streams across
/// untouched, and should be smaller for the container overhead alone.
#[tokio::test]
async fn transport_stream_remux_is_a_true_stream_copy() {
    require!("ffmpeg" => "-version");
    let work = Work::new();
    let source = work.h264_clip("src.mp4", 2);
    let ts = work.path("src.ts");
    let status = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-i"])
        .arg(&source)
        .args(["-c", "copy"])
        .arg(&ts)
        .status()
        .unwrap();
    assert!(status.success());

    let output = work.path("out.mp4");
    let fp = convert::fingerprint(Recipe::TsRemux, &ts).await.unwrap();
    convert::encode(Recipe::TsRemux, &ts, &output, work.dir.path(), &[], VideoOptions::default())
        .await
        .unwrap();

    let fidelity = convert::verify(Recipe::TsRemux, &output, &fp, work.dir.path(), &[])
        .await
        .unwrap();
    assert_eq!(fidelity, Fidelity::ContentExact);
    assert!(
        size(&output) < size(&ts),
        "MP4 {} should undercut TS {}",
        size(&output),
        size(&ts)
    );
}

/// Both elementary streams must survive, not just the video.
#[tokio::test]
async fn remux_preserves_the_audio_stream_too() {
    require!("ffmpeg" => "-version");
    let work = Work::new();
    let source = work.h264_clip("src.mp4", 2);
    let ts = work.path("src.ts");
    Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-i"])
        .arg(&source)
        .args(["-c", "copy"])
        .arg(&ts)
        .status()
        .unwrap();

    let before = StreamDigest::of(&ts).await.unwrap();
    assert!(before.audio.is_some(), "the fixture should have audio");

    let output = work.path("out.mp4");
    video_lossless::remux_to_mp4(&ts, &output, &[], false)
        .await
        .unwrap();
    let after = StreamDigest::of(&output).await.unwrap();

    assert_eq!(before.video, after.video);
    assert_eq!(before.audio, after.audio);
    assert_eq!(before.frames, after.frames);
}

/// A remux that lost frames must fail rather than be reported as lossless. This
/// is what stops `+discardcorrupt` silently degrading a file.
#[tokio::test]
async fn a_remux_that_drops_frames_is_rejected() {
    require!("ffmpeg" => "-version");
    let work = Work::new();
    let full = work.h264_clip("full.mp4", 3);
    let fp = convert::fingerprint(Recipe::TsRemux, &full).await.unwrap();

    // Stand in for packet loss by remuxing only part of the clip.
    let truncated = work.ffmpeg("short.mp4", &["-i", full.to_str().unwrap(), "-t", "1", "-c", "copy"]);

    let result = convert::verify(Recipe::TsRemux, &truncated, &fp, work.dir.path(), &[]).await;
    assert!(result.is_err(), "a shortened remux must not verify");
}

/// FFV1 is genuinely lossless, so every decoded frame must match.
#[tokio::test]
async fn ffv1_preserves_every_frame() {
    require!("ffmpeg" => "-version");
    let work = Work::new();
    // A genuinely uncompressed source, which is what this recipe is for. ProRes
    // would be the wrong fixture: it is a lossy codec, and FFV1 makes it bigger.
    let source = work.ffmpeg(
        "src.avi",
        &[
            "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=10:duration=2",
            "-c:v", "rawvideo", "-pix_fmt", "yuv420p",
        ],
    );

    let output = work.path("out.mkv");
    let fp = convert::fingerprint(Recipe::Ffv1, &source).await.unwrap();
    convert::encode(Recipe::Ffv1, &source, &output, work.dir.path(), &[], VideoOptions::default())
        .await
        .unwrap();

    let fidelity = convert::verify(Recipe::Ffv1, &output, &fp, work.dir.path(), &[])
        .await
        .unwrap();
    assert_eq!(fidelity, Fidelity::ContentExact);
    assert!(
        size(&output) < size(&source),
        "FFV1 {} should undercut the uncompressed source {}",
        size(&output),
        size(&source)
    );
}

/// The AV1 tier's whole justification: the result is measured, and only accepted
/// if it clears the gate.
#[tokio::test]
async fn av1_encodes_and_scores_above_the_gate() {
    require!("ffmpeg" => "-version");
    let work = Work::new();
    let source = work.h264_clip("src.mp4", 2);
    let probe = classify::probe_file(&source).await.unwrap();

    let output = work.path("out.mp4");
    let settings = video_av1::Settings {
        // A fast preset keeps the test quick; the gate is what is under test.
        preset: 10,
        ..Default::default()
    };

    // Encode well inside the transparent range so the gate should pass.
    video_av1::encode(&source, &output, 18, &settings, &[], |_| {})
        .await
        .unwrap();

    let model = vmaf::model_for(probe.video[0].width, probe.video[0].height);
    let scores = vmaf::measure(&output, &source, model, work.dir.path(), &[], false)
        .await
        .unwrap();

    assert!(scores.frames > 0);
    assert!(
        scores.passes(),
        "a high-quality encode should clear the gate: {}",
        scores.summary()
    );
}

/// The other half: a deliberately poor encode must be caught, or the gate is
/// decorative.
#[tokio::test]
async fn a_low_quality_av1_encode_fails_the_gate() {
    require!("ffmpeg" => "-version");
    let work = Work::new();
    let source = work.h264_clip("src.mp4", 2);
    let probe = classify::probe_file(&source).await.unwrap();

    let output = work.path("bad.mp4");
    let settings = video_av1::Settings {
        preset: 10,
        ..Default::default()
    };
    // A CRF this high is visibly degraded.
    video_av1::encode(&source, &output, 60, &settings, &[], |_| {})
        .await
        .unwrap();

    let model = vmaf::model_for(probe.video[0].width, probe.video[0].height);
    let scores = vmaf::measure(&output, &source, model, work.dir.path(), &[], false)
        .await
        .unwrap();

    assert!(
        !scores.passes(),
        "a degraded encode must not pass: {}",
        scores.summary()
    );
}

/// Audio must be copied, never re-encoded: a second lossy generation treats the
/// first encoder's quantisation noise as signal.
#[tokio::test]
async fn av1_copies_the_audio_stream_untouched() {
    require!("ffmpeg" => "-version");
    let work = Work::new();
    let source = work.h264_clip("src.mp4", 2);
    let before = StreamDigest::of(&source).await.unwrap();

    let output = work.path("out.mp4");
    video_av1::encode(
        &source,
        &output,
        30,
        &video_av1::Settings {
            preset: 10,
            ..Default::default()
        },
        &[],
        |_| {},
    )
    .await
    .unwrap();

    let after = StreamDigest::of(&output).await.unwrap();
    assert_eq!(
        before.audio, after.audio,
        "the audio stream must be byte-identical"
    );
    assert_ne!(before.video, after.video, "the video should have been re-encoded");
}

/// Re-encoding must not rotate the picture. ffmpeg 7.0 onwards auto-rotates on
/// decode, which would bake the rotation in while also leaving it in the display
/// matrix, so the result plays rotated twice.
#[tokio::test]
async fn av1_does_not_double_apply_rotation() {
    require!("ffmpeg" => "-version");
    let work = Work::new();
    let source = work.ffmpeg(
        "rot.mp4",
        &[
            "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=10:duration=1",
            "-c:v", "libx264", "-pix_fmt", "yuv420p",
            "-metadata:s:v:0", "rotate=90",
        ],
    );

    let output = work.path("out.mp4");
    video_av1::encode(
        &source,
        &output,
        30,
        &video_av1::Settings {
            preset: 10,
            ..Default::default()
        },
        &[],
        |_| {},
    )
    .await
    .unwrap();

    let before = classify::probe_file(&source).await.unwrap();
    let after = classify::probe_file(&output).await.unwrap();
    // The stored frame must keep its original orientation.
    assert_eq!(before.video[0].width, after.video[0].width);
    assert_eq!(before.video[0].height, after.video[0].height);
}
