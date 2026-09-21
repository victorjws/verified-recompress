//! Turning a converted file back into its original.
//!
//! Only the byte-exact recipes can do this, and the distinction is not academic:
//! a JPEG that became a JXL can be rebuilt exactly, whereas a PNG that became a
//! JXL only has its pixels preserved, and an AV1 re-encode cannot be undone at
//! all. Rather than produce something approximate and call it a restore, the
//! recipes that cannot deliver the original are refused outright.
//!
//! The rebuilt file is checked against the hash the inventory recorded before
//! anything was replaced, so a restore either produces the original or fails.

use std::path::Path;

use anyhow::{Result, bail};

use crate::convert::{command, try_run};
use crate::hash;
use crate::ledger::Completed;

/// Whether a recorded conversion can be undone exactly.
///
/// Keyed on the fidelity that was actually achieved, not on the recipe alone: a
/// FLAC encode reaches byte-exactness only when the source's container metadata
/// came along, and the ledger records which happened.
pub fn is_restorable(record: &Completed) -> bool {
    record.fidelity == "byte-exact"
}

/// Explains why a conversion cannot be undone.
pub fn refusal(record: &Completed) -> String {
    match record.recipe.as_str() {
        "av1" => format!(
            "{} was re-encoded to AV1, which cannot be undone. If it is still in the \
             trash, restore it from the Filen web app instead.",
            record.path
        ),
        "jxl-from-raster" | "ffv1" | "ts-remux" => format!(
            "{} kept its pixels but not its original bytes, so the exact file cannot \
             be rebuilt. The content is intact in {}.",
            record.path, record.output_path
        ),
        other => format!(
            "{} was converted with {other} at {} fidelity, which does not support an \
             exact restore.",
            record.path, record.fidelity
        ),
    }
}

/// Builds a staging filename that keeps `remote_path`'s extension.
///
/// The encoders dispatch on extension, and the output one in particular: `djxl`
/// writes a JPEG because it was asked for `.jpg`, and refuses outright if the
/// target is something it does not recognise. Downloading to a neutral name like
/// `converted.bin` therefore breaks the restore.
pub fn staged_name(stem: &str, remote_path: &str) -> String {
    match Path::new(remote_path).extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_string(),
    }
}

/// Rebuilds the original from `converted`, writing it to `restored`.
pub async fn rebuild(record: &Completed, converted: &Path, restored: &Path) -> Result<()> {
    match record.recipe.as_str() {
        "jxl-from-jpeg" => {
            // `-J` refuses rather than falling back to a freshly encoded, lossy JPEG.
            let mut cmd = command("djxl", &[]);
            cmd.arg("-J").arg(converted).arg(restored);
            if !try_run(cmd).await? {
                bail!("djxl could not rebuild the original JPEG from {}", converted.display());
            }
            Ok(())
        }
        "flac" | "flac-recompress" => {
            // The foreign metadata is what makes the original container rebuildable.
            let mut cmd = command("flac", &[]);
            cmd.args(["-d", "--keep-foreign-metadata", "-f", "-o"])
                .arg(restored)
                .arg(converted);
            if !try_run(cmd).await? {
                bail!(
                    "flac could not rebuild the original container from {}",
                    converted.display()
                );
            }
            Ok(())
        }
        other => bail!("no exact restore exists for {other}"),
    }
}

/// Confirms a rebuilt file is the original, byte for byte.
pub async fn confirm(record: &Completed, restored: &Path) -> Result<()> {
    let Some(expected) = record.original_blake3.as_deref() else {
        bail!(
            "the inventory has no hash for {}, so a restore cannot be proven correct",
            record.path
        );
    };
    let actual = hash::blake3_file(restored).await?;
    if actual != expected {
        bail!(
            "the rebuilt file does not match the original (expected {expected}, got {actual})"
        );
    }
    let size = tokio::fs::metadata(restored).await?.len();
    if size != record.original_size {
        bail!(
            "the rebuilt file is {size} bytes, the original was {}",
            record.original_size
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(recipe: &str, fidelity: &str) -> Completed {
        Completed {
            path: "photos/shot.jpg".into(),
            output_path: "photos/shot.jxl".into(),
            original_size: 1000,
            output_size: 800,
            original_blake3: Some("aa".into()),
            recipe: recipe.into(),
            fidelity: fidelity.into(),
        }
    }

    #[test]
    fn byte_exact_conversions_can_be_undone() {
        assert!(is_restorable(&record("jxl-from-jpeg", "byte-exact")));
        assert!(is_restorable(&record("flac", "byte-exact")));
    }

    /// Pixel identity is not the same as having the original file, and the
    /// difference has to be refused rather than glossed over.
    #[test]
    fn content_exact_conversions_cannot() {
        assert!(!is_restorable(&record("jxl-from-raster", "content-exact")));
        assert!(!is_restorable(&record("ts-remux", "content-exact")));
        assert!(!is_restorable(&record("av1", "content-exact")));
    }

    /// The same recipe can land on either fidelity depending on the source, so the
    /// recorded outcome decides, not the recipe name.
    #[test]
    fn fidelity_decides_rather_than_the_recipe_name() {
        assert!(is_restorable(&record("flac", "byte-exact")));
        assert!(!is_restorable(&record("flac", "content-exact")));
    }

    /// An AV1 refusal should point at the one recovery route that does exist.
    #[test]
    fn the_av1_refusal_points_at_the_trash() {
        let message = refusal(&record("av1", "content-exact"));
        assert!(message.contains("cannot be undone"));
        assert!(message.contains("trash"));
    }

    #[test]
    fn a_content_exact_refusal_says_the_content_is_still_intact() {
        let message = refusal(&record("jxl-from-raster", "content-exact"));
        assert!(message.contains("pixels"));
        assert!(message.contains("photos/shot.jxl"));
    }

    #[tokio::test]
    async fn a_restore_without_a_recorded_hash_cannot_be_proven() {
        let mut record = record("jxl-from-jpeg", "byte-exact");
        record.original_blake3 = None;
        let file = tempfile::NamedTempFile::new().unwrap();
        let err = confirm(&record, file.path()).await.unwrap_err();
        assert!(err.to_string().contains("no hash"), "{err}");
    }

    #[tokio::test]
    async fn a_mismatched_restore_is_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"not the original").unwrap();
        assert!(confirm(&record("jxl-from-jpeg", "byte-exact"), file.path())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_matching_restore_is_accepted() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"hello").unwrap();
        let mut record = record("jxl-from-jpeg", "byte-exact");
        record.original_blake3 =
            Some("ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f".into());
        record.original_size = 5;
        assert!(confirm(&record, file.path()).await.is_ok());
    }

    /// A file with the right hash but the wrong length would mean the hash was
    /// computed over something else; both are checked.
    #[tokio::test]
    async fn a_size_mismatch_is_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"hello").unwrap();
        let mut record = record("jxl-from-jpeg", "byte-exact");
        record.original_blake3 =
            Some("ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f".into());
        record.original_size = 999;
        let err = confirm(&record, file.path()).await.unwrap_err();
        assert!(err.to_string().contains("bytes"), "{err}");
    }

    /// djxl picks its output format from the extension and rejects one it does
    /// not know, so staging paths cannot use a neutral name.
    #[test]
    fn staging_names_keep_the_extension() {
        assert_eq!(staged_name("rebuilt", "photos/shot.jpg"), "rebuilt.jpg");
        assert_eq!(staged_name("converted", "photos/shot.jxl"), "converted.jxl");
        assert_eq!(staged_name("rebuilt", "audio/tone.wav"), "rebuilt.wav");
        assert_eq!(staged_name("rebuilt", "noext"), "rebuilt");
    }

    #[tokio::test]
    async fn an_unknown_recipe_has_no_restore_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = rebuild(
            &record("something-new", "byte-exact"),
            &dir.path().join("in"),
            &dir.path().join("out"),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no exact restore"), "{err}");
    }
}
