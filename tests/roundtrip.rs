//! Round-trip tests against the real encoders.
//!
//! These are the tests that actually back the project's central claim: that a file
//! can be replaced by a smaller one and nothing is lost. They run the same code the
//! pipeline runs, on real files produced by ffmpeg, and check the guarantee each
//! recipe advertises rather than merely that a command exited zero.

use std::path::{Path, PathBuf};
use std::process::Command;

use storage_optimizer::convert::{self, Fidelity};
use storage_optimizer::policy::Recipe;

fn have(tool: &str, version_flag: &str) -> bool {
    Command::new(tool)
        .arg(version_flag)
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

    /// Builds a deterministic test image or sound with ffmpeg.
    fn make(&self, name: &str, args: &[&str]) -> PathBuf {
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
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

/// Runs one recipe end to end: fingerprint, encode, verify.
async fn round_trip(recipe: Recipe, input: &Path, work: &Work, out_name: &str) -> (Fidelity, u64, u64) {
    let output = work.path(out_name);
    let fp = convert::fingerprint(recipe, input).await.unwrap();
    convert::encode(recipe, input, &output, &[], Default::default())
        .await
        .unwrap();
    let fidelity = convert::verify(recipe, &output, &fp, work.dir.path(), &[])
        .await
        .unwrap();
    (fidelity, size(input), size(&output))
}

/// The headline case: an existing JPEG becomes a smaller JXL that rebuilds the
/// original file byte for byte.
#[tokio::test]
async fn jpeg_round_trips_byte_for_byte_and_shrinks() {
    require!("ffmpeg" => "-version", "cjxl" => "--version", "djxl" => "--version");
    let work = Work::new();
    let input = work.make(
        "in.jpg",
        &["-f", "lavfi", "-i", "testsrc2=size=640x480", "-frames:v", "1", "-q:v", "3"],
    );

    let (fidelity, before, after) = round_trip(Recipe::JxlFromJpeg, &input, &work, "out.jxl").await;

    assert_eq!(fidelity, Fidelity::ByteExact);
    assert!(after < before, "{after} should be smaller than {before}");
}

/// A JXL that cannot rebuild its source must be rejected, not accepted on the
/// strength of its pixels. This is the failure that would otherwise delete an
/// original that can never be recovered.
#[tokio::test]
async fn a_jxl_without_reconstruction_data_fails_verification() {
    require!("ffmpeg" => "-version", "cjxl" => "--version", "djxl" => "--version");
    let work = Work::new();
    let input = work.make(
        "in.jpg",
        &["-f", "lavfi", "-i", "testsrc2=size=320x240", "-frames:v", "1", "-q:v", "3"],
    );

    let fp = convert::fingerprint(Recipe::JxlFromJpeg, &input).await.unwrap();

    // Encode deliberately without the reconstruction box. The image is intact, so
    // any pixel-based check would happily pass this.
    let output = work.path("no-jbrd.jxl");
    let status = Command::new("cjxl")
        .args(["-j", "1", "--allow_jpeg_reconstruction=0", "-d", "0", "-e", "7"])
        .arg(&input)
        .arg(&output)
        .output()
        .expect("failed to run cjxl");
    assert!(status.status.success());

    let result = convert::verify(Recipe::JxlFromJpeg, &output, &fp, work.dir.path(), &[]).await;
    assert!(
        result.is_err(),
        "a JXL that cannot rebuild the original JPEG must not verify"
    );
    let message = result.unwrap_err().to_string();
    assert!(message.contains("cannot be rebuilt"), "{message}");
}

/// PNG has no reconstruction box, so the promise is pixel identity, and that is
/// what gets checked.
#[tokio::test]
async fn png_round_trips_pixel_identically_and_shrinks() {
    require!("ffmpeg" => "-version", "cjxl" => "--version", "djxl" => "--version");
    let work = Work::new();
    let input = work.make(
        "in.png",
        &["-f", "lavfi", "-i", "testsrc2=size=640x480", "-frames:v", "1"],
    );

    let (fidelity, before, after) = round_trip(Recipe::JxlFromRaster, &input, &work, "out.jxl").await;

    assert_eq!(fidelity, Fidelity::ContentExact);
    assert!(after < before, "{after} should be smaller than {before}");
}

/// Corrupting the output must be caught. Without this, "verification" could be
/// passing vacuously and nobody would know.
#[tokio::test]
async fn a_damaged_raster_output_fails_verification() {
    require!("ffmpeg" => "-version", "cjxl" => "--version", "djxl" => "--version");
    let work = Work::new();
    let input = work.make(
        "in.png",
        &["-f", "lavfi", "-i", "testsrc2=size=320x240", "-frames:v", "1"],
    );
    let fp = convert::fingerprint(Recipe::JxlFromRaster, &input).await.unwrap();

    // Encode a *different* image to the expected output path.
    let other = work.make(
        "other.png",
        &["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=1", "-frames:v", "1", "-vf", "negate"],
    );
    let output = work.path("out.jxl");
    convert::encode(Recipe::JxlFromRaster, &other, &output, &[], Default::default())
        .await
        .unwrap();

    assert!(
        convert::verify(Recipe::JxlFromRaster, &output, &fp, work.dir.path(), &[])
            .await
            .is_err(),
        "pixels that do not match the source must fail"
    );
}

/// WAV to FLAC. `--keep-foreign-metadata` carries the container's own chunks, so
/// this reaches byte-exactness rather than stopping at sample identity.
#[tokio::test]
async fn wav_round_trips_byte_for_byte_and_shrinks() {
    require!("ffmpeg" => "-version", "flac" => "--version");
    let work = Work::new();
    let input = work.make(
        "in.wav",
        &["-f", "lavfi", "-i", "sine=frequency=440:duration=3", "-c:a", "pcm_s16le"],
    );

    let (fidelity, before, after) = round_trip(Recipe::Flac, &input, &work, "out.flac").await;

    assert_eq!(fidelity, Fidelity::ByteExact);
    assert!(after < before, "{after} should be smaller than {before}");
}

#[tokio::test]
async fn flac_output_that_does_not_match_the_source_fails() {
    require!("ffmpeg" => "-version", "flac" => "--version");
    let work = Work::new();
    let input = work.make(
        "in.wav",
        &["-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:a", "pcm_s16le"],
    );
    let other = work.make(
        "other.wav",
        &["-f", "lavfi", "-i", "sine=frequency=880:duration=2", "-c:a", "pcm_s16le"],
    );

    let fp = convert::fingerprint(Recipe::Flac, &input).await.unwrap();
    let output = work.path("out.flac");
    convert::encode(Recipe::Flac, &other, &output, &[], Default::default())
        .await
        .unwrap();

    assert!(
        convert::verify(Recipe::Flac, &output, &fp, work.dir.path(), &[])
            .await
            .is_err(),
        "audio that does not match the source must fail"
    );
}

/// Verification must not leave its scratch files behind: they would count against
/// the staging budget and, worse, could be mistaken for outputs.
#[tokio::test]
async fn verification_cleans_up_after_itself() {
    require!("ffmpeg" => "-version", "cjxl" => "--version", "djxl" => "--version");
    let work = Work::new();
    let input = work.make(
        "in.jpg",
        &["-f", "lavfi", "-i", "testsrc2=size=320x240", "-frames:v", "1", "-q:v", "3"],
    );
    round_trip(Recipe::JxlFromJpeg, &input, &work, "out.jxl").await;

    let leftovers: Vec<_> = std::fs::read_dir(work.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("roundtrip"))
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}

/// The fingerprint must be taken before encoding so the source can be deleted to
/// keep peak disk down. Verification therefore has to work with the source gone.
#[tokio::test]
async fn verification_works_after_the_source_is_deleted() {
    require!("ffmpeg" => "-version", "cjxl" => "--version", "djxl" => "--version");
    let work = Work::new();
    let input = work.make(
        "in.jpg",
        &["-f", "lavfi", "-i", "testsrc2=size=320x240", "-frames:v", "1", "-q:v", "3"],
    );
    let output = work.path("out.jxl");

    let fp = convert::fingerprint(Recipe::JxlFromJpeg, &input).await.unwrap();
    convert::encode(Recipe::JxlFromJpeg, &input, &output, &[], Default::default())
        .await
        .unwrap();
    std::fs::remove_file(&input).unwrap();

    let fidelity = convert::verify(Recipe::JxlFromJpeg, &output, &fp, work.dir.path(), &[])
        .await
        .unwrap();
    assert_eq!(fidelity, Fidelity::ByteExact);
}
