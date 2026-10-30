//! Round-trip tests against the real encoders.
//!
//! These are the tests that actually back the project's central claim: that a file
//! can be replaced by a smaller one and nothing is lost. They run the same code the
//! pipeline runs, on real files produced by ffmpeg, and check the guarantee each
//! recipe advertises rather than merely that a command exited zero.

use std::path::{Path, PathBuf};
use std::process::Command;

use verified_recompress::convert::jxl::DEFAULT_RASTER_EFFORT;
use verified_recompress::convert::{self, Fidelity};
use verified_recompress::policy::Recipe;

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
    convert::encode(
        recipe,
        input,
        &output,
        work.dir.path(),
        &[],
        DEFAULT_RASTER_EFFORT,
        Default::default(),
    )
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

/// The effort setting has to survive the whole way down to cjxl's command line,
/// and a value that quietly went nowhere would look exactly like one that worked:
/// the conversion still succeeds and still verifies. What it would not do is
/// change the output, so that is what this checks.
///
/// Effort 1 against 9, because the gap is unambiguous. It is deliberately not an
/// assertion that more effort is always smaller — measured on a gradient, 9 beats
/// 10 — so the claim here is only that the number reaches the encoder.
#[tokio::test]
async fn the_raster_effort_setting_reaches_the_encoder() {
    require!("ffmpeg" => "-version", "cjxl" => "--version");
    let work = Work::new();
    let input = work.make(
        "gradient.png",
        &["-f", "lavfi", "-i", "gradients=size=320x240", "-frames:v", "1"],
    );

    let mut sizes = Vec::new();
    for (effort, name) in [(1, "cheap.jxl"), (9, "thorough.jxl")] {
        let output = work.path(name);
        convert::encode(
            Recipe::JxlFromRaster,
            &input,
            &output,
            work.dir.path(),
            &[],
            effort,
            Default::default(),
        )
        .await
        .unwrap();
        sizes.push(size(&output));
    }

    let (cheap, thorough) = (sizes[0], sizes[1]);
    assert!(
        cheap > thorough,
        "effort 1 produced {cheap} bytes and effort 9 produced {thorough}; \
         identical or inverted sizes mean the setting never reached cjxl"
    );
}

/// Lossless WebP is the one member of the "already efficient" family JXL can
/// still beat, and cjxl cannot read it, so the recipe goes through a decode
/// step. This checks the whole path, pixels included.
#[tokio::test]
async fn lossless_webp_round_trips_pixel_identically_and_shrinks() {
    require!(
        "ffmpeg" => "-version",
        "cwebp" => "-version",
        "dwebp" => "-version",
        "cjxl" => "--version",
        "djxl" => "--version"
    );
    let work = Work::new();
    let png = work.make(
        "in.png",
        &["-f", "lavfi", "-i", "testsrc2=size=640x480", "-frames:v", "1"],
    );

    let webp = work.path("in.webp");
    let status = Command::new("cwebp")
        .args(["-quiet", "-lossless"])
        .arg(&png)
        .arg("-o")
        .arg(&webp)
        .status()
        .unwrap();
    assert!(status.success());

    let (fidelity, before, after) =
        round_trip(Recipe::JxlFromWebp, &webp, &work, "out.jxl").await;

    assert_eq!(fidelity, Fidelity::ContentExact);
    assert!(after < before, "{after} should be smaller than {before}");
}

/// The guarantee is pixel identity, so a WebP whose JXL decodes to something
/// else has to fail rather than be accepted on size alone.
#[tokio::test]
async fn a_webp_conversion_that_changes_pixels_fails_verification() {
    require!(
        "ffmpeg" => "-version",
        "cwebp" => "-version",
        "dwebp" => "-version",
        "cjxl" => "--version",
        "djxl" => "--version"
    );
    let work = Work::new();
    let png = work.make(
        "a.png",
        &["-f", "lavfi", "-i", "testsrc2=size=320x240", "-frames:v", "1"],
    );
    let webp = work.path("a.webp");
    assert!(
        Command::new("cwebp")
            .args(["-quiet", "-lossless"])
            .arg(&png)
            .arg("-o")
            .arg(&webp)
            .status()
            .unwrap()
            .success()
    );

    // Fingerprint the real source, then hand verification a JXL of a different
    // image entirely.
    let fp = convert::fingerprint(Recipe::JxlFromWebp, &webp).await.unwrap();
    let other = work.make(
        "other.png",
        &["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=1", "-frames:v", "1", "-ss", "3"],
    );
    let output = work.path("out.jxl");
    convert::encode(
        Recipe::JxlFromRaster,
        &other,
        &output,
        work.dir.path(),
        &[],
        DEFAULT_RASTER_EFFORT,
        Default::default(),
    )
    .await
    .unwrap();

    let result = convert::verify(Recipe::JxlFromWebp, &output, &fp, work.dir.path(), &[]).await;
    assert!(result.is_err(), "different pixels must not verify");
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
    convert::encode(
        Recipe::JxlFromRaster,
        &other,
        &output,
        work.dir.path(),
        &[],
        DEFAULT_RASTER_EFFORT,
        Default::default(),
    )
        .await
        .unwrap();

    assert!(
        convert::verify(Recipe::JxlFromRaster, &output, &fp, work.dir.path(), &[])
            .await
            .is_err(),
        "pixels that do not match the source must fail"
    );
}

/// cjxl reads neither BMP nor TIFF, so these used to fail every time with
/// "Getting pixel data failed". They now go through an ffmpeg-decoded PNG.
#[tokio::test]
async fn bmp_and_tiff_round_trip_pixel_identically() {
    require!("ffmpeg" => "-version", "cjxl" => "--version", "djxl" => "--version");
    let work = Work::new();
    let src = ["-f", "lavfi", "-i", "testsrc2=size=320x240", "-frames:v", "1"];
    let with = |fmt: &'static str| -> Vec<&'static str> {
        let mut args = vec!["-f", "lavfi", "-i", "testsrc2=size=320x240", "-frames:v", "1"];
        args.extend(["-pix_fmt", fmt]);
        args
    };

    let cases = [
        ("in.bmp", src.to_vec()),
        ("in24.tiff", with("rgb24")),
        ("in48.tiff", with("rgb48le")),
        ("inalpha.tiff", with("rgba")),
        ("inalpha64.tiff", with("rgba64le")),
        ("ingray.tiff", with("gray")),
        ("inmono.png", with("monob")),
        ("inpal.png", with("pal8")),
    ];
    for (name, args) in cases {
        let input = work.make(name, &args);
        let (fidelity, _, _) =
            round_trip(Recipe::JxlFromRaster, &input, &work, &format!("{name}.jxl")).await;
        assert_eq!(fidelity, Fidelity::ContentExact, "{name}");
    }
}

/// The classic failure: web and messenger downloads that are WebP inside but
/// named `.png`. Routed by name, cjxl is handed a WebP and gives up.
#[tokio::test]
async fn a_webp_named_png_is_converted_by_what_it_is() {
    require!(
        "ffmpeg" => "-version",
        "cwebp" => "-version",
        "dwebp" => "-version",
        "cjxl" => "--version",
        "djxl" => "--version"
    );
    let work = Work::new();
    let png = work.make(
        "real.png",
        &["-f", "lavfi", "-i", "testsrc2=size=320x240", "-frames:v", "1"],
    );
    let disguised = work.path("disguised.png");
    let status = Command::new("cwebp")
        .args(["-quiet", "-lossless"])
        .arg(&png)
        .arg("-o")
        .arg(&disguised)
        .status()
        .unwrap();
    assert!(status.success());

    let head = std::fs::read(&disguised).unwrap();
    let decision = verified_recompress::policy::decide(
        verified_recompress::policy::Facts::new("disguised.png", 1 << 20).with_head(&head),
        verified_recompress::policy::Limits::unbounded(),
    );
    let verified_recompress::policy::Decision::Convert(recipe) = decision else {
        panic!("a lossless WebP should be converted, got {decision:?}");
    };
    assert_eq!(recipe, Recipe::JxlFromWebp);

    let (fidelity, _, _) = round_trip(recipe, &disguised, &work, "out.jxl").await;
    assert_eq!(fidelity, Fidelity::ContentExact);

    // The raster recipe on the same bytes must cope too, rather than depending
    // on the caller having sniffed first.
    let (fidelity, _, _) = round_trip(Recipe::JxlFromRaster, &disguised, &work, "out2.jxl").await;
    assert_eq!(fidelity, Fidelity::ContentExact);
}

/// A genuine PNG that libjxl refuses. ffmpeg writes the colour tags a decoder
/// inferred as `cICP`/`gAMA` chunks, and a PNG made from an untagged gray TIFF
/// carries "primaries unspecified", which cjxl rejects with "Getting pixel data
/// failed" even though every other tool opens the file. The conversion has to
/// get past that rather than report the file as failed.
#[tokio::test]
async fn a_png_that_cjxl_rejects_still_converts_through_the_ffmpeg_fallback() {
    require!("ffmpeg" => "-version", "cjxl" => "--version", "djxl" => "--version");
    let work = Work::new();
    let tiff = work.make(
        "gray.tiff",
        &["-f", "lavfi", "-i", "testsrc2=size=320x240", "-frames:v", "1", "-pix_fmt", "gray"],
    );
    let stubborn = work.path("stubborn.png");
    let status = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-i"])
        .arg(&tiff)
        .args(["-map", "0:v:0", "-frames:v", "1", "-c:v", "png"])
        .arg(&stubborn)
        .status()
        .unwrap();
    assert!(status.success());

    // Without this the test would pass without exercising the fallback at all.
    let direct = Command::new("cjxl")
        .args(["-d", "0", "-e", "1"])
        .arg(&stubborn)
        .arg(work.path("direct.jxl"))
        .output()
        .unwrap();
    if direct.status.success() {
        eprintln!("skipping: this cjxl accepts the file, so there is no fallback to test");
        return;
    }

    let (fidelity, _, _) = round_trip(Recipe::JxlFromRaster, &stubborn, &work, "out.jxl").await;
    assert_eq!(fidelity, Fidelity::ContentExact);
}

/// Grayscale and 16-bit sources are hashed in a normalised format, and that
/// normalisation must not collapse distinct values: a ramp touching every
/// level, then the same ramp with a single level moved.
#[tokio::test]
async fn gray_and_deep_color_round_trip_and_a_single_changed_value_is_caught() {
    require!("ffmpeg" => "-version", "cjxl" => "--version", "djxl" => "--version");
    let work = Work::new();
    let ramp = |expr: &str| {
        let lavfi = format!("color=gray:size=256x16,format=gray,geq=lum='{expr}'");
        vec!["-f".to_string(), "lavfi".into(), "-i".into(), lavfi, "-frames:v".into(), "1".into(), "-pix_fmt".into(), "gray".into()]
    };
    let make = |name: &str, expr: &str| {
        let args = ramp(expr);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        work.make(name, &refs)
    };

    let gray = make("ramp.png", "X");
    let (fidelity, _, _) = round_trip(Recipe::JxlFromRaster, &gray, &work, "ramp.jxl").await;
    assert_eq!(fidelity, Fidelity::ContentExact);

    let deep = work.make(
        "deep.png",
        &[
            "-f", "lavfi", "-i", "testsrc2=size=320x240", "-frames:v", "1", "-pix_fmt", "rgb48be",
        ],
    );
    let (fidelity, _, _) = round_trip(Recipe::JxlFromRaster, &deep, &work, "deep.jxl").await;
    assert_eq!(fidelity, Fidelity::ContentExact);

    let fp = convert::fingerprint(Recipe::JxlFromRaster, &gray).await.unwrap();
    let nudged = make("nudged.png", "if(eq(X,100),101,X)");
    let output = work.path("nudged.jxl");
    convert::encode(
        Recipe::JxlFromRaster,
        &nudged,
        &output,
        work.dir.path(),
        &[],
        DEFAULT_RASTER_EFFORT,
        Default::default(),
    )
    .await
    .unwrap();
    assert!(
        convert::verify(Recipe::JxlFromRaster, &output, &fp, work.dir.path(), &[])
            .await
            .is_err(),
        "one moved level must not verify"
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
    convert::encode(
        Recipe::Flac,
        &other,
        &output,
        work.dir.path(),
        &[],
        DEFAULT_RASTER_EFFORT,
        Default::default(),
    )
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
    convert::encode(
        Recipe::JxlFromJpeg,
        &input,
        &output,
        work.dir.path(),
        &[],
        DEFAULT_RASTER_EFFORT,
        Default::default(),
    )
        .await
        .unwrap();
    std::fs::remove_file(&input).unwrap();

    let fidelity = convert::verify(Recipe::JxlFromJpeg, &output, &fp, work.dir.path(), &[])
        .await
        .unwrap();
    assert_eq!(fidelity, Fidelity::ByteExact);
}
