//! Deterministic fixture generation for the benchmark suite.
//!
//! Two shapes cover the cases the suite is meant to answer (bench/README.md):
//!
//! - [`Shape::Huge`] - a handful of multi-hundred-megabyte files. Exercises raw
//!   upload throughput, chunking and the resumable path; almost no per-file
//!   overhead.
//! - [`Shape::TinyDeep`] - up to a million small files spread over a deeply
//!   nested tree. Exercises walking, hashing, per-file bookkeeping and request
//!   round-trips; almost no raw byte throughput.
//!
//! Everything is a pure function of `(shape, scale, seed)`: the same triple
//! always produces byte-identical trees, on any machine, so two tools are
//! compared against the same input and a re-run compares against the same input
//! as last week.
//!
//! # Why the content is pseudo-random
//!
//! File bodies are filled from a seeded SplitMix64 stream, which is effectively
//! incompressible. That is a deliberate choice: a tool that compresses on the
//! wire (or, in Driven's case, packs cold small files into a `.tar.gz` bundle)
//! would otherwise score wildly better on zero-filled or text-like fixtures than
//! it ever would on the photos, videos and archives that dominate a real backup
//! set. Incompressible content measures the transport, not the entropy of the
//! test data. The trade-off is documented in bench/README.md so nobody reads
//! these numbers as "compression does not help" - on a compressible corpus it
//! very much does.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Bytes written per `write_all` call while filling a file body.
const WRITE_CHUNK: usize = 1 << 20;

/// Small files per leaf directory in the [`Shape::TinyDeep`] tree. Keeping this
/// low forces a wide, genuinely deep tree rather than a few fat directories.
const FILES_PER_LEAF: usize = 4;

/// Smallest / largest body size for a [`Shape::TinyDeep`] file. The spread is
/// what makes the shape realistic - a fixed size would let a tool tune to it.
const TINY_MIN_BYTES: u64 = 64;
const TINY_MAX_BYTES: u64 = 4096;

/// The file name of the manifest describing a materialised fixture. It lives
/// BESIDE the tree (not inside it) so it is never itself uploaded.
const MANIFEST_NAME: &str = "manifest.json";

/// The subdirectory holding the actual files. The tool under test is pointed at
/// this path, never at the fixture root.
const TREE_DIR: &str = "tree";

/// The two fixture shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Shape {
    /// A few very large files in one flat directory.
    Huge,
    /// Very many very small files in a deeply nested tree.
    TinyDeep,
}

impl Shape {
    /// The stable slug used in fixture directory names and report tables.
    pub fn slug(self) -> &'static str {
        match self {
            Shape::Huge => "huge",
            Shape::TinyDeep => "tiny-deep",
        }
    }
}

impl std::fmt::Display for Shape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

/// A fully-resolved description of one fixture tree.
///
/// This is the single source of truth for "what does this fixture contain":
/// path layout, per-file sizes and file contents are all derived from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixtureSpec {
    /// Which shape to build.
    pub shape: Shape,
    /// How many files the tree holds.
    pub files: usize,
    /// For [`Shape::Huge`], the exact size of every file. Ignored for
    /// [`Shape::TinyDeep`], whose sizes are drawn per file from the seed.
    pub huge_file_bytes: u64,
    /// Directory nesting depth for [`Shape::TinyDeep`]. Ignored for
    /// [`Shape::Huge`], which is flat.
    pub depth: usize,
    /// The PRNG seed. Same seed, same bytes, forever.
    pub seed: u64,
}

impl FixtureSpec {
    /// The directory name this spec materialises into, unique per spec so two
    /// scales can coexist in the fixture cache.
    pub fn dir_name(&self) -> String {
        match self.shape {
            Shape::Huge => format!(
                "huge-{}x{}-s{}",
                self.files, self.huge_file_bytes, self.seed
            ),
            Shape::TinyDeep => format!("tiny-deep-{}-d{}-s{}", self.files, self.depth, self.seed),
        }
    }

    /// Total byte size of the tree, computed without touching the disk.
    pub fn total_bytes(&self) -> u64 {
        match self.shape {
            Shape::Huge => self.huge_file_bytes * self.files as u64,
            Shape::TinyDeep => (0..self.files).map(|i| self.file_size(i)).sum(),
        }
    }

    /// The size of file `index`, derived from the seed.
    pub fn file_size(&self, index: usize) -> u64 {
        match self.shape {
            Shape::Huge => self.huge_file_bytes,
            Shape::TinyDeep => {
                let r =
                    splitmix64(self.seed ^ mix(index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                TINY_MIN_BYTES + (r % (TINY_MAX_BYTES - TINY_MIN_BYTES + 1))
            }
        }
    }

    /// The tree-relative path of file `index`.
    ///
    /// For [`Shape::TinyDeep`] the leaf directory is the base-`fanout`
    /// representation of the file's leaf index, padded to exactly `depth`
    /// components - so every file really does sit `depth` levels down, and no
    /// empty directories are ever created.
    pub fn file_path(&self, index: usize) -> PathBuf {
        match self.shape {
            Shape::Huge => PathBuf::from(format!("file_{index:04}.bin")),
            Shape::TinyDeep => {
                let leaf = index / FILES_PER_LEAF;
                let fanout = self.fanout();
                let mut path = PathBuf::new();
                let mut rest = leaf;
                // Least-significant digit first is fine: the mapping only has to
                // be a stable bijection, not sorted.
                for _ in 0..self.depth {
                    path.push(format!("d{:02}", rest % fanout));
                    rest /= fanout;
                }
                path.push(format!("f{index:07}.bin"));
                path
            }
        }
    }

    /// The directory branching factor: the smallest `f >= 2` with
    /// `f^depth >= leaves`, so the tree is exactly `depth` deep and no wider
    /// than it needs to be.
    fn fanout(&self) -> usize {
        let leaves = self.files.div_ceil(FILES_PER_LEAF).max(1);
        let mut f = 2usize;
        loop {
            // Saturating power: a big fanout with a big depth overflows long
            // before it stops satisfying the bound.
            let mut acc: u128 = 1;
            for _ in 0..self.depth {
                acc = acc.saturating_mul(f as u128);
            }
            if acc >= leaves as u128 || f >= 64 {
                return f;
            }
            f += 1;
        }
    }

    /// The PRNG state that seeds file `index`'s body. `generation` is bumped by
    /// [`Fixture::mutate`] so a mutated file gets genuinely different bytes.
    fn content_seed(&self, index: usize, generation: u64) -> u64 {
        splitmix64(self.seed ^ mix(index as u64) ^ generation.wrapping_mul(0xD1B5_4A32_D192_ED03))
    }
}

/// The on-disk record of a materialised fixture, stored beside the tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    spec: FixtureSpec,
    /// Indices whose bodies are currently at generation 1 (mutated) rather than
    /// generation 0 (pristine). Persisted so an interrupted run leaves enough
    /// information to restore the tree instead of rebuilding it.
    mutated: Vec<usize>,
}

/// A materialised fixture on disk.
pub struct Fixture {
    root: PathBuf,
    spec: FixtureSpec,
    mutated: Vec<usize>,
}

impl Fixture {
    /// The directory to point a tool at.
    pub fn tree(&self) -> PathBuf {
        self.root.join(TREE_DIR)
    }

    /// Materialises `spec` under `cache_root`, reusing an existing tree when one
    /// matches.
    ///
    /// A reused tree is always returned PRISTINE: if a previous run mutated
    /// files and did not restore them (or crashed midway), the recorded indices
    /// are rewritten back to their generation-0 bodies. That matters because the
    /// cold-upload phase of the next tool must see byte-identical input to the
    /// one the previous tool saw.
    pub fn build(cache_root: &Path, spec: &FixtureSpec) -> Result<Self> {
        let root = cache_root.join(spec.dir_name());
        let manifest_path = root.join(MANIFEST_NAME);

        if let Ok(bytes) = fs::read(&manifest_path) {
            if let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) {
                if manifest.spec == *spec {
                    let mut fixture = Fixture {
                        root,
                        spec: spec.clone(),
                        mutated: manifest.mutated,
                    };
                    if !fixture.mutated.is_empty() {
                        eprintln!(
                            "fixture {}: restoring {} previously-mutated file(s)",
                            spec.dir_name(),
                            fixture.mutated.len()
                        );
                        fixture.restore()?;
                    }
                    return Ok(fixture);
                }
            }
        }

        // No usable cache: rebuild from scratch.
        if root.exists() {
            fs::remove_dir_all(&root)
                .with_context(|| format!("clearing stale fixture {}", root.display()))?;
        }
        fs::create_dir_all(root.join(TREE_DIR))
            .with_context(|| format!("creating fixture root {}", root.display()))?;

        let fixture = Fixture {
            root,
            spec: spec.clone(),
            mutated: Vec::new(),
        };
        fixture.write_range(0..spec.files, 0)?;
        fixture.save_manifest()?;
        Ok(fixture)
    }

    /// Rewrites a deterministic `fraction` of the tree's files with fresh
    /// content, modelling the "small changes since the last backup" case.
    ///
    /// Selection is a fixed stride derived from the seed, so the same fixture
    /// always mutates the same files - both tools see the identical delta.
    /// Returns the indices touched.
    ///
    /// Content changes but the file set does not: no creates, no deletes. That
    /// keeps `rclone copy` and Driven comparable (see bench/README.md - a delete
    /// would need `rclone sync` to be a fair match for Driven's trash pass).
    pub fn mutate(&mut self, fraction: f64) -> Result<Vec<usize>> {
        let count = ((self.spec.files as f64 * fraction).round() as usize)
            .clamp(1, self.spec.files)
            .min(self.spec.files);
        let stride = (self.spec.files / count).max(1);
        let offset = (splitmix64(self.spec.seed ^ 0xBEEF) as usize) % stride;

        let indices: Vec<usize> = (0..count)
            .map(|n| (offset + n * stride) % self.spec.files)
            .collect();

        for &index in &indices {
            self.write_one(index, 1)?;
        }

        self.mutated = indices.clone();
        self.save_manifest()?;
        Ok(indices)
    }

    /// Rewrites every mutated file back to its pristine body.
    pub fn restore(&mut self) -> Result<()> {
        let indices = std::mem::take(&mut self.mutated);
        for index in indices {
            self.write_one(index, 0)?;
        }
        self.save_manifest()
    }

    /// Writes files `range` at content `generation`, spreading the work over the
    /// available cores (a million small files is a thread-bound workload, and a
    /// single-threaded generator would dominate the time spent benchmarking).
    fn write_range(&self, range: std::ops::Range<usize>, generation: u64) -> Result<()> {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 16);
        let total = range.len();
        let chunk = total.div_ceil(workers).max(1);

        eprintln!(
            "fixture {}: writing {} file(s), {} across {} worker(s)",
            self.spec.dir_name(),
            total,
            human_bytes(self.spec.total_bytes()),
            workers
        );

        let results: Vec<Result<()>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|w| {
                    let start = range.start + w * chunk;
                    let end = (start + chunk).min(range.end);
                    scope.spawn(move || {
                        for index in start..end {
                            self.write_one(index, generation)?;
                        }
                        Ok(())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("fixture worker panicked")))
                })
                .collect()
        });
        for r in results {
            r?;
        }
        Ok(())
    }

    /// Writes exactly one file's body at `generation`, creating parent
    /// directories as needed.
    fn write_one(&self, index: usize, generation: u64) -> Result<()> {
        let rel = self.spec.file_path(index);
        let path = self.tree().join(&rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let size = self.spec.file_size(index);
        let mut state = self.spec.content_seed(index, generation);
        let mut file = fs::File::create(&path)
            .with_context(|| format!("creating fixture file {}", path.display()))?;

        let mut written = 0u64;
        let mut buf = vec![0u8; WRITE_CHUNK.min(size.max(1) as usize)];
        while written < size {
            let want = ((size - written) as usize).min(buf.len());
            fill_random(&mut buf[..want], &mut state);
            file.write_all(&buf[..want])
                .with_context(|| format!("writing fixture file {}", path.display()))?;
            written += want as u64;
        }
        file.flush()?;
        Ok(())
    }

    fn save_manifest(&self) -> Result<()> {
        let manifest = Manifest {
            spec: self.spec.clone(),
            mutated: self.mutated.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        fs::write(self.root.join(MANIFEST_NAME), bytes)
            .with_context(|| format!("writing manifest under {}", self.root.display()))?;
        Ok(())
    }
}

/// Deletes every cached fixture under `cache_root`.
pub fn clean(cache_root: &Path) -> Result<()> {
    if cache_root.exists() {
        fs::remove_dir_all(cache_root)
            .with_context(|| format!("removing fixture cache {}", cache_root.display()))?;
    }
    Ok(())
}

/// Fills `buf` with a SplitMix64 stream, advancing `state`.
fn fill_random(buf: &mut [u8], state: &mut u64) {
    for word in buf.chunks_mut(8) {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let value = splitmix64(*state);
        let bytes = value.to_le_bytes();
        word.copy_from_slice(&bytes[..word.len()]);
    }
}

/// The SplitMix64 finaliser - a fast, well-distributed 64-bit mixer.
fn splitmix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Mixes an index into a well-spread 64-bit value.
fn mix(x: u64) -> u64 {
    splitmix64(x.wrapping_add(0x2545_F491_4F6C_DD1D))
}

/// Formats a byte count for human-facing log lines.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_spec() -> FixtureSpec {
        FixtureSpec {
            shape: Shape::TinyDeep,
            files: 40,
            huge_file_bytes: 0,
            depth: 4,
            seed: 7,
        }
    }

    fn read_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((rel, fs::read(&path).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn same_seed_produces_byte_identical_trees() {
        let a = tempdir();
        let b = tempdir();
        let spec = tiny_spec();
        let fa = Fixture::build(a.path(), &spec).unwrap();
        let fb = Fixture::build(b.path(), &spec).unwrap();
        assert_eq!(read_tree(&fa.tree()), read_tree(&fb.tree()));
    }

    #[test]
    fn different_seeds_produce_different_content() {
        let a = tempdir();
        let b = tempdir();
        let fa = Fixture::build(a.path(), &tiny_spec()).unwrap();
        let mut other = tiny_spec();
        other.seed = 8;
        let fb = Fixture::build(b.path(), &other).unwrap();
        assert_ne!(read_tree(&fa.tree()), read_tree(&fb.tree()));
    }

    #[test]
    fn tree_is_exactly_depth_deep_and_has_the_requested_file_count() {
        let dir = tempdir();
        let spec = tiny_spec();
        let f = Fixture::build(dir.path(), &spec).unwrap();
        let files = read_tree(&f.tree());
        assert_eq!(files.len(), spec.files);
        for (rel, _) in &files {
            // depth directory components plus the file name.
            assert_eq!(
                rel.split('/').count(),
                spec.depth + 1,
                "{rel} is not {} levels deep",
                spec.depth
            );
        }
    }

    #[test]
    fn total_bytes_matches_what_was_written() {
        let dir = tempdir();
        let spec = tiny_spec();
        let f = Fixture::build(dir.path(), &spec).unwrap();
        let on_disk: u64 = read_tree(&f.tree())
            .iter()
            .map(|(_, b)| b.len() as u64)
            .sum();
        assert_eq!(on_disk, spec.total_bytes());
    }

    #[test]
    fn huge_shape_is_flat_and_exact_size() {
        let dir = tempdir();
        let spec = FixtureSpec {
            shape: Shape::Huge,
            files: 3,
            huge_file_bytes: 4096,
            depth: 0,
            seed: 1,
        };
        let f = Fixture::build(dir.path(), &spec).unwrap();
        let files = read_tree(&f.tree());
        assert_eq!(files.len(), 3);
        for (rel, bytes) in files {
            assert!(!rel.contains('/'), "huge shape must be flat, got {rel}");
            assert_eq!(bytes.len(), 4096);
        }
    }

    #[test]
    fn mutate_changes_only_the_selected_files_and_restore_undoes_it() {
        let dir = tempdir();
        let spec = tiny_spec();
        let mut f = Fixture::build(dir.path(), &spec).unwrap();
        let before = read_tree(&f.tree());

        let touched = f.mutate(0.1).unwrap();
        assert_eq!(touched.len(), 4, "10% of 40 files");
        let after = read_tree(&f.tree());
        assert_eq!(
            after.len(),
            before.len(),
            "mutate must not add or remove files"
        );
        let changed = before
            .iter()
            .zip(after.iter())
            .filter(|(a, b)| a.1 != b.1)
            .count();
        assert_eq!(changed, touched.len());

        f.restore().unwrap();
        assert_eq!(read_tree(&f.tree()), before, "restore must be exact");
    }

    #[test]
    fn mutation_selection_is_deterministic() {
        let a = tempdir();
        let b = tempdir();
        let mut fa = Fixture::build(a.path(), &tiny_spec()).unwrap();
        let mut fb = Fixture::build(b.path(), &tiny_spec()).unwrap();
        assert_eq!(fa.mutate(0.1).unwrap(), fb.mutate(0.1).unwrap());
    }

    #[test]
    fn rebuild_restores_a_fixture_left_mutated_by_a_crashed_run() {
        let dir = tempdir();
        let spec = tiny_spec();
        let mut f = Fixture::build(dir.path(), &spec).unwrap();
        let pristine = read_tree(&f.tree());
        f.mutate(0.1).unwrap();
        assert_ne!(read_tree(&f.tree()), pristine);
        drop(f);

        // A fresh build over the same cache must hand back a pristine tree.
        let reused = Fixture::build(dir.path(), &spec).unwrap();
        assert_eq!(read_tree(&reused.tree()), pristine);
    }

    #[test]
    fn a_changed_spec_rebuilds_rather_than_reusing() {
        let dir = tempdir();
        let f = Fixture::build(dir.path(), &tiny_spec()).unwrap();
        assert_eq!(read_tree(&f.tree()).len(), 40);
        let mut bigger = tiny_spec();
        bigger.files = 12;
        let f2 = Fixture::build(dir.path(), &bigger).unwrap();
        assert_eq!(read_tree(&f2.tree()).len(), 12);
    }

    #[test]
    fn human_bytes_is_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1 << 30), "1.0 GiB");
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }
}
