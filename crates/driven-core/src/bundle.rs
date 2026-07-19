//! Small-file bundling archive format (V2, issue #35).
//!
//! Cold folders of many tiny files generate one upload round-trip each, which is
//! slow and burns Google Drive rate limits. Driven packs many genuinely-new tiny
//! files into a single `.tar.gz` "bundle" Drive object. This module owns the
//! archive format - building a bundle from a set of local files, and extracting
//! one member back out on restore - so the executor and the restore path share
//! exactly one definition of the on-Drive layout.
//!
//! ## Format (`driven.bundle_format = "tgz-1"`)
//! A gzip-compressed tar. Each member is one tar entry whose NAME is the member's
//! [`member_entry_name`] (a fixed-length, ASCII, collision-resistant BLAKE3-prefix
//! hash of the member's canonical relative path) - never the raw path. This keeps
//! entry names always valid as tar names (no length / unicode / separator
//! pitfalls) and lets the restore path locate a member deterministically from its
//! `file_state` relative_path without storing a second name. For an ENCRYPTED
//! source the whole `.tar.gz` object is run through the same per-object content
//! encryptor as any file (so member names inside the tar are never exposed); for
//! a plaintext source the tar is uploaded as-is.
//!
//! ## Bounds
//! Bundles are size-capped by the planner (a few MiB), so the whole archive is
//! built and extracted IN MEMORY on a blocking task - no async-streaming tar. The
//! restore extractor additionally caps total decompressed bytes as a defence
//! against a corrupt/tampered object (a "gzip bomb"); per-member BLAKE3
//! verification (done by the caller against `file_state.hash_blake3`) guards
//! content integrity.

use std::io::Read;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};

use crate::types::RelativePath;

/// The `driven.bundle_format` appProperty value stamped on every bundle object,
/// so a future reader (and any DESIGN s18.9 folder-sweep) can recognise the
/// object as a Driven bundle and pick the right extractor. Bump the suffix if the
/// archive layout ever changes incompatibly.
pub const BUNDLE_FORMAT: &str = "tgz-1";

/// The tar entry name for a bundle member: the hex of the first 16 bytes of
/// `BLAKE3(relative_path)`. Fixed 32-char ASCII, so it is always a valid tar
/// name (no ustar 100-byte / unicode / `/` issues) and is deterministic, so the
/// restore path derives it from the member's `file_state` relative_path with no
/// extra stored column. 128-bit prefix collisions between two distinct paths in
/// one (already size-capped) bundle are computationally infeasible.
pub fn member_entry_name(rel: &RelativePath) -> String {
    let hash = blake3::hash(rel.as_str().as_bytes());
    hex::encode(&hash.as_bytes()[..16])
}

/// One member successfully packed into a bundle, with the identity the executor
/// records in its `file_state` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltMember {
    /// The member's relative path (its `file_state` key).
    pub rel: RelativePath,
    /// Plaintext size in bytes of the exact bytes packed.
    pub size: u64,
    /// Modification time in signed nanoseconds since the Unix epoch, computed the
    /// SAME way the scanner does (`modified().duration_since(UNIX_EPOCH)`) so the
    /// next scan sees the member as unchanged and does not re-bundle it forever.
    pub mtime_ns: i64,
    /// Plaintext BLAKE3 (32 bytes) of the packed bytes - the `file_state`
    /// change-detection key and the restore per-member integrity check.
    pub blake3: [u8; 32],
}

/// The result of building one bundle archive.
#[derive(Debug, Clone)]
pub struct BuildOutput {
    /// The complete `.tar.gz` bytes (plaintext; the caller encrypts if the source
    /// is encrypted).
    pub tar_gz: Vec<u8>,
    /// Members actually packed, in archive order.
    pub members: Vec<BuiltMember>,
    /// Members skipped because the file vanished, could not be read, or changed
    /// mid-read (a coherent snapshot could not be captured). These are NOT packed
    /// and NOT committed, so the next scan re-detects and retries them.
    pub skipped: Vec<RelativePath>,
}

/// Signed nanoseconds since the Unix epoch, matching `scanner::mtime_ns` exactly
/// (see that fn's doc): a platform that cannot report an mtime yields `0`; a
/// pre-epoch mtime is the negated reverse magnitude. Keeping this byte-identical
/// to the scanner's computation is what stops a bundled member from looking
/// "changed" on every subsequent scan.
fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    let modified = match meta.modified() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

/// Build a `.tar.gz` bundle from `inputs` (each `(relative_path, absolute local
/// path)`), reading and hashing every member. SYNCHRONOUS + fully in-memory:
/// call it from a blocking task (`tokio::task::spawn_blocking`) - the planner
/// caps a bundle's total size so the archive fits in memory comfortably.
///
/// Per member: stat, read the whole file, re-stat, and skip it (recording it in
/// [`BuildOutput::skipped`]) if the file vanished, could not be read, or its
/// `(size, mtime)` changed between the two stats or disagreed with the bytes
/// read, so only a coherent snapshot is ever packed. The gzip layer is written
/// with a zeroed mtime for reproducibility.
pub fn build_bundle(inputs: &[(RelativePath, PathBuf)]) -> Result<BuildOutput> {
    use flate2::{Compression, GzBuilder};

    let gz = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    let mut tar = tar::Builder::new(gz);
    let mut members: Vec<BuiltMember> = Vec::with_capacity(inputs.len());
    let mut skipped: Vec<RelativePath> = Vec::new();

    for (rel, path) in inputs {
        let pre = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                skipped.push(rel.clone());
                continue;
            }
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => {
                skipped.push(rel.clone());
                continue;
            }
        };
        let post = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                skipped.push(rel.clone());
                continue;
            }
        };
        // Coherency: the file must not have changed between the two stats, and the
        // bytes we read must match the stat size. Any mismatch means we did not
        // capture a single consistent snapshot - skip and let the next scan retry.
        let size = post.len();
        if pre.len() != post.len()
            || mtime_ns(&pre) != mtime_ns(&post)
            || bytes.len() as u64 != size
        {
            skipped.push(rel.clone());
            continue;
        }

        let hash = blake3::hash(&bytes);
        let mut header = tar::Header::new_gnu();
        header.set_size(size);
        header.set_mode(0o644);
        header.set_mtime(0);
        // `append_data` sets the entry path and (re)computes the checksum.
        let entry_name = member_entry_name(rel);
        tar.append_data(&mut header, &entry_name, &bytes[..])
            .with_context(|| format!("append bundle member {rel}"))?;

        members.push(BuiltMember {
            rel: rel.clone(),
            size,
            mtime_ns: mtime_ns(&post),
            blake3: *hash.as_bytes(),
        });
    }

    // Finish the tar (writes the two zero blocks) then the gzip trailer.
    let gz = tar.into_inner().context("finish bundle tar")?;
    let tar_gz = gz.finish().context("finish bundle gzip")?;

    Ok(BuildOutput {
        tar_gz,
        members,
        skipped,
    })
}

/// Extract one member's plaintext bytes from a decompressed-in-memory `.tar.gz`
/// bundle by its [`member_entry_name`]. Returns `Ok(None)` if no such entry
/// exists. `max_decompressed` bounds the TOTAL bytes read from the gzip stream (a
/// gzip-bomb / tampered-object guard); a bundle that tries to expand past it
/// fails with an error rather than exhausting memory. SYNCHRONOUS + in-memory;
/// call from a blocking task.
pub fn extract_member(
    tar_gz: &[u8],
    entry_name: &str,
    max_decompressed: u64,
) -> Result<Option<Vec<u8>>> {
    use flate2::read::GzDecoder;

    // Cap total decompressed bytes across the WHOLE archive (not just the target
    // entry): the tar reader decompresses/skips other entries too, so bounding the
    // decoder itself is what actually caps memory. `+ 1` so we can detect an
    // overrun rather than silently truncating at exactly the cap.
    let limited = GzDecoder::new(tar_gz).take(max_decompressed.saturating_add(1));
    let mut archive = tar::Archive::new(limited);

    for entry in archive.entries().context("read bundle tar entries")? {
        let mut entry = entry.context("read bundle tar entry")?;
        let name_matches = {
            let path = entry.path().context("read bundle entry name")?;
            path.to_string_lossy() == entry_name
        };
        if !name_matches {
            continue;
        }
        let declared = entry.header().size().unwrap_or(0);
        if declared > max_decompressed {
            anyhow::bail!(
                "bundle member {entry_name} declares {declared} bytes, over the {max_decompressed} cap"
            );
        }
        let mut out = Vec::new();
        entry
            .read_to_end(&mut out)
            .context("read bundle member bytes")?;
        if out.len() as u64 > max_decompressed {
            anyhow::bail!("bundle member {entry_name} exceeds the {max_decompressed}-byte cap");
        }
        return Ok(Some(out));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(s: &str) -> RelativePath {
        RelativePath::try_from(s.to_string()).expect("valid relative path")
    }

    #[test]
    fn entry_name_is_stable_32_hex_and_distinct() {
        let a = member_entry_name(&rel("a/b/c.txt"));
        let b = member_entry_name(&rel("a/b/d.txt"));
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        // Deterministic across calls.
        assert_eq!(a, member_entry_name(&rel("a/b/c.txt")));
    }

    #[test]
    fn build_then_extract_roundtrips_each_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut inputs = Vec::new();
        let mut contents = Vec::new();
        for i in 0..12u8 {
            let name = format!("f{i}.txt");
            let body = format!("contents of file {i} - some bytes {i}{i}{i}").into_bytes();
            std::fs::write(dir.path().join(&name), &body).expect("write");
            inputs.push((rel(&name), dir.path().join(&name)));
            contents.push((rel(&name), body));
        }

        let out = build_bundle(&inputs).expect("build");
        assert_eq!(out.members.len(), 12);
        assert!(out.skipped.is_empty());

        for (r, body) in &contents {
            let member = out
                .members
                .iter()
                .find(|m| &m.rel == r)
                .expect("member present");
            assert_eq!(member.size, body.len() as u64);
            assert_eq!(member.blake3, *blake3::hash(body).as_bytes());
            let extracted = extract_member(&out.tar_gz, &member_entry_name(r), 8 * 1024 * 1024)
                .expect("extract ok")
                .expect("member found");
            assert_eq!(&extracted, body);
        }

        // A name that is not in the bundle yields None.
        let missing = extract_member(&out.tar_gz, &member_entry_name(&rel("nope.txt")), 1 << 20)
            .expect("extract ok");
        assert!(missing.is_none());
    }

    #[test]
    fn extract_enforces_decompressed_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = vec![7u8; 64 * 1024];
        std::fs::write(dir.path().join("big.bin"), &body).expect("write");
        let inputs = vec![(rel("big.bin"), dir.path().join("big.bin"))];
        let out = build_bundle(&inputs).expect("build");
        // A cap below the member size must fail rather than return truncated bytes.
        let res = extract_member(&out.tar_gz, &member_entry_name(&rel("big.bin")), 1024);
        assert!(res.is_err(), "expected decompressed-cap error");
    }

    #[test]
    fn missing_file_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("present.txt"), b"hi").expect("write");
        let inputs = vec![
            (rel("present.txt"), dir.path().join("present.txt")),
            (rel("gone.txt"), dir.path().join("gone.txt")),
        ];
        let out = build_bundle(&inputs).expect("build");
        assert_eq!(out.members.len(), 1);
        assert_eq!(out.skipped, vec![rel("gone.txt")]);
    }
}
