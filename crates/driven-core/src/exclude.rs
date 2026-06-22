//! gitignore + default + custom excludes; DESIGN s5.2.
//!
//! Builds ONE combined [`ignore::gitignore::Gitignore`] decision matcher
//! ([`build_source_matcher`]) that the scanner (SPEC s6) consults for every
//! entry, plus the plain [`ignore::WalkBuilder`] it walks with. ALL ignore
//! decisions are made by our matcher; the walker does NO ignore logic of its
//! own (its built-in gitignore / git-exclude / git-global layers are turned
//! off).
//!
//! ## Precedence (gitignore semantics: LAST matching rule wins)
//!
//! Rules are added LOWEST-precedence FIRST, so a later rule overrides an
//! earlier one (DESIGN s5.2):
//!
//! 1. (lowest) the DESIGN s5.2 DEFAULT EXCLUDE list (OS noise / editor swap /
//!    misc transient globs), each added as a bare exclude glob.
//! 2. the source's `.gitignore` cascade, IF [`SourceRow::respect_gitignore`]:
//!    every `.gitignore` under the source root, root-first, added in order so
//!    a deeper file's rule wins over a shallower one. A user `!Thumbs.db` in
//!    gitignore therefore beats the default Thumbs.db exclude (DESIGN s5.2:
//!    "gitignore wins where they conflict").
//! 3. (highest) the source's own `exclude_patterns` (bare globs = force-out,
//!    e.g. `*.log`) then `include_patterns` (`!`-prefixed = re-include, e.g.
//!    a bare `.env` that opts a gitignored secret back in). These are the
//!    user's source-level overrides and beat BOTH gitignore and the defaults
//!    (DESIGN s5.2: `include_patterns` opt-back-in things gitignore excludes;
//!    `exclude_patterns` force-out things gitignore includes).
//!
//! ## Glob semantics (true gitignore, NOT the inverted `Override` form)
//!
//! These are real gitignore rules: a *bare* glob EXCLUDES and a leading `!`
//! RE-INCLUDES. So `exclude_patterns` are added verbatim and `include_patterns`
//! get a `!` prepended. A source whose intent is "re-include `.env`" stores
//! the bare string `.env` in `include_patterns` (the matcher prepends the
//! `!`); the "e.g. !.env" wording in the SPEC describes the user-facing
//! *effect*, not the stored glob.
//!
//! ## Why a custom matcher (replaces the old single `Override`)
//!
//! The previous implementation packed defaults + user rules into one
//! [`ignore::overrides::Override`]. That tier is WHITELIST-mode (one whitelist
//! glob drops every unmatched file - a backup data-loss bug) and is evaluated
//! ABOVE the gitignore cascade, inverting the DESIGN defaults-vs-gitignore
//! precedence so a gitignore `!Thumbs.db` could not re-include over a default.
//! A `Gitignore` matcher has neither problem: unmatched paths stay
//! `Match::None` (included) and `!`-rules re-include naturally without
//! flipping any global mode - which is what lets `!.env` (F2) and a gitignore
//! `!Thumbs.db` (F5) work.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;

use crate::state::SourceRow;

static TARGET: &str = "driven::core::exclude";

/// The DESIGN s5.2 default exclude list, copied verbatim.
///
/// Applied to every source and AND-ed with the source's own
/// include/exclude rules plus the gitignore cascade. Exposed as a constant
/// so the Settings -> Rules surface (DESIGN s5.2) can render it as a
/// per-item toggle list later; the scanner consumes the whole slice.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    // OS noise
    ".DS_Store",
    ".AppleDouble",
    ".LSOverride",
    "._*",
    "Thumbs.db",
    "ehthumbs.db",
    "ehthumbs_vista.db",
    "Desktop.ini",
    "$RECYCLE.BIN/",
    // Editor swap / lock / temp
    "*.swp",
    "*.swo",
    "*.swn",
    "*~",
    ".~lock.*#",
    "~$*",
    // Misc transient
    "*.tmp",
    "~*.tmp",
    ".DocumentRevisions-V100/",
    ".Spotlight-V100/",
    ".fseventsd/",
    ".TemporaryItems/",
    ".Trashes/",
];

/// The combined include/exclude decision matcher for one source.
///
/// Thin wrapper over a single [`ignore::gitignore::Gitignore`] built by
/// [`build_source_matcher`]. The scanner consults it for BOTH the walk filter
/// and the excluded-orphan split (DESIGN s5.5) via [`SourceMatcher::is_included`],
/// so include/exclude semantics are identical in both places.
#[derive(Debug)]
pub struct SourceMatcher {
    inner: Gitignore,
}

impl SourceMatcher {
    /// Whether `rel` (a source-root-relative path) is INCLUDED under the
    /// current rules. `is_dir` distinguishes a directory from a file so a
    /// trailing-slash gitignore rule (`node_modules/`) applies correctly.
    ///
    /// Uses [`Gitignore::matched_path_or_any_parents`], NOT `matched`: a
    /// directory-scoped rule such as `node_modules/` matches the *directory*,
    /// and `matched("node_modules/foo.js", false)` would return `None`
    /// (included) - the file would leak in. Walking ancestors catches the
    /// excluded parent and returns `Ignore`. A `Whitelist` (a `!`-re-include)
    /// or `None` (no rule) both mean INCLUDED; only `Ignore` excludes.
    pub fn is_included(&self, rel: &Path, is_dir: bool) -> bool {
        !self
            .inner
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
    }
}

/// Builds the combined [`SourceMatcher`] for `source` (DESIGN s5.2).
///
/// Adds rules in LAST-MATCH-WINS order (see the module docs): defaults
/// (lowest), then the `.gitignore` cascade if `respect_gitignore`, then the
/// source's `exclude_patterns` and `include_patterns` (highest). The matcher
/// is anchored at the source root so all rules match against root-relative
/// paths - the same form the scanner strips each entry to.
pub fn build_source_matcher(source: &SourceRow) -> anyhow::Result<SourceMatcher> {
    let root = Path::new(&source.local_path);
    let mut gb = GitignoreBuilder::new(root);

    // 1. (lowest) DESIGN s5.2 default excludes - bare gitignore globs.
    for glob in DEFAULT_EXCLUDES {
        gb.add_line(None, glob)
            .map_err(|e| anyhow::anyhow!("adding default exclude `{glob}`: {e}"))?;
    }

    // 2. the source's `.gitignore` cascade, root-first so a deeper file's
    //    rule wins over a shallower one (approximation of the per-directory
    //    cascade for M2).
    // TODO(perf/correctness): true per-directory gitignore scoping - rules in
    //    a nested `.gitignore` should apply only under that directory, and a
    //    pattern with no slash should match at any depth below its file. This
    //    flattens all rules into one matcher rooted at the source root, which
    //    is a close-but-not-exact approximation accepted for M2.
    if source.respect_gitignore {
        for gitignore in collect_gitignore_files(root) {
            // `GitignoreBuilder::add` returns Some(err) on a partial/parse
            // error; a missing or unreadable `.gitignore` is non-fatal (we
            // simply apply no rules from it) so only log.
            if let Some(err) = gb.add(&gitignore) {
                tracing::warn!(
                    target: TARGET,
                    source_id = %source.id,
                    path = %gitignore.display(),
                    %err,
                    "failed to parse a .gitignore; ignoring its rules"
                );
            }
        }
    }

    // 3. (highest) the source's own overrides: exclude_patterns force-out
    //    (bare glob), then include_patterns opt-back-in (`!`-prefixed). Added
    //    LAST so they beat both gitignore and the defaults.
    for exc in &source.exclude_patterns {
        gb.add_line(None, exc)
            .map_err(|e| anyhow::anyhow!("adding exclude_pattern `{exc}`: {e}"))?;
    }
    for inc in &source.include_patterns {
        let reinclude = format!("!{inc}");
        gb.add_line(None, &reinclude)
            .map_err(|e| anyhow::anyhow!("adding include_pattern `{inc}`: {e}"))?;
    }

    let inner = gb
        .build()
        .map_err(|e| anyhow::anyhow!("building source matcher: {e}"))?;

    tracing::debug!(
        target: TARGET,
        source_id = %source.id,
        respect_gitignore = source.respect_gitignore,
        includes = source.include_patterns.len(),
        excludes = source.exclude_patterns.len(),
        num_ignores = inner.num_ignores(),
        "built source matcher"
    );
    Ok(SourceMatcher { inner })
}

/// Collects every `.gitignore` file under `root`, root-first (shallowest
/// first), so [`build_source_matcher`] can add them in last-match-wins order
/// where a deeper file's rule overrides a shallower one.
///
/// A dependency-free breadth-first `std::fs::read_dir` walk: BFS visits
/// shallower directories before deeper ones, giving the root-first ordering
/// directly. Symlinked directories are NOT descended (mirrors the scanner's
/// `follow_links(false)` policy, DESIGN s5.2.1, and avoids cycles). I/O errors
/// on a directory are logged and that subtree skipped - never fatal, since a
/// failed enumerate just means we apply fewer gitignore rules, never that we
/// wrongly back up or drop a file (the per-entry walk re-checks each path).
// TODO(perf): prune directories the matcher already excludes (e.g.
//   `node_modules`) so we do not descend them just to find a `.gitignore`.
fn collect_gitignore_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(root.to_path_buf());

    while let Some(dir) = queue.pop_front() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(err) => {
                tracing::debug!(target: TARGET, path = %dir.display(), %err, "read_dir failed while collecting .gitignore files; skipping subtree");
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                // Do not follow symlinks (cycle / out-of-root safety).
                Ok(ft) if ft.is_dir() => queue.push_back(path),
                Ok(ft)
                    if ft.is_file() && entry.file_name() == std::ffi::OsStr::new(".gitignore") =>
                {
                    found.push(path);
                }
                _ => {}
            }
        }
    }
    found
}

/// Builds the configured [`WalkBuilder`] for `source` (SPEC s6
/// `build_walker`).
///
/// The walker does NO ignore logic of its own: `git_ignore` /
/// `git_exclude` / `git_global` are all turned OFF because the scanner makes
/// every include/exclude decision via [`build_source_matcher`]. The walker is
/// just a plain recursive directory traversal.
///
/// `hidden(false)` is mandatory: Driven backs up dotfiles, and leaving the
/// `ignore` default `hidden(true)` would silently drop every `.env` /
/// `.config` before any rule applies.
///
/// `follow_links(false)` (the `ignore` default, set explicitly for clarity)
/// implements the [`crate::types::SymlinkPolicy::Skip`] policy from DESIGN
/// s5.2.1: symlinks are yielded as entries but never traversed, so the walk
/// can never leave the source root or loop. The scanner then drops the link
/// entries themselves.
pub fn build_walker(source: &SourceRow) -> anyhow::Result<WalkBuilder> {
    let mut wb = WalkBuilder::new(&source.local_path);
    wb.git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .require_git(false)
        .hidden(false)
        .follow_links(false);

    tracing::debug!(
        target: TARGET,
        source_id = %source.id,
        respect_gitignore = source.respect_gitignore,
        includes = source.include_patterns.len(),
        excludes = source.exclude_patterns.len(),
        "built walker"
    );
    Ok(wb)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::types::{AccountId, SourceId};

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, contents).expect("write");
    }

    /// A `SourceRow` rooted at `root` with the given rule knobs; the fields
    /// the scanner/exclude path never reads are filled with cheap dummies.
    fn source_at(
        root: &Path,
        respect_gitignore: bool,
        include: &[&str],
        exclude: &[&str],
    ) -> SourceRow {
        SourceRow {
            id: SourceId::new_v4(),
            account_id: AccountId::new_v4(),
            display_name: "t".into(),
            enabled: true,
            local_path: root.to_string_lossy().into_owned(),
            drive_folder_id: "f".into(),
            drive_folder_path: "/f".into(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore,
            include_patterns: include.iter().map(|s| s.to_string()).collect(),
            exclude_patterns: exclude.iter().map(|s| s.to_string()).collect(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 0,
        }
    }

    /// Collects the included file basenames (relative to `root`), applying
    /// the same matcher + walker the scanner uses. The walker itself no longer
    /// filters (all ignore decisions live in [`SourceMatcher`]), so this
    /// mirrors the scanner: strip to a root-relative path and ask the matcher
    /// `is_included(rel, is_dir)`.
    fn walked_names(source: &SourceRow) -> Vec<String> {
        let matcher = build_source_matcher(source).expect("matcher");
        let mut out = Vec::new();
        for res in build_walker(source).expect("walker").build() {
            let entry = res.expect("entry");
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let rel = entry
                .path()
                .strip_prefix(&source.local_path)
                .expect("under root");
            // The root entry strips to "" - skip it.
            if rel.as_os_str().is_empty() {
                continue;
            }
            if !matcher.is_included(rel, is_dir) {
                continue;
            }
            if entry.file_type().is_some_and(|t| t.is_file()) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
        out.sort();
        out
    }

    #[test]
    fn gitignore_respected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "secret.txt\n");
        write(&root.join("secret.txt"), "x");
        write(&root.join("keep.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let names = walked_names(&src);
        assert!(names.contains(&"keep.txt".to_string()), "{names:?}");
        assert!(
            !names.contains(&"secret.txt".to_string()),
            "gitignored file must be dropped: {names:?}"
        );
    }

    #[test]
    fn gitignore_disabled_includes_everything() {
        // Guards that `gitignore_respected` is not passing vacuously: with
        // respect_gitignore=false the same .gitignore must NOT take effect.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "secret.txt\n");
        write(&root.join("secret.txt"), "x");

        let src = source_at(root, false, &[], &[]);
        let names = walked_names(&src);
        assert!(
            names.contains(&"secret.txt".to_string()),
            "gitignore must not apply when respect_gitignore=false: {names:?}"
        );
    }

    #[test]
    fn include_pattern_reincludes_gitignored() {
        // F2: a bare `.env` include_pattern re-includes a path the gitignore
        // cascade would drop (ROADMAP "!.env wins" row; stored as the bare
        // glob - the matcher prepends the `!`). `keep.txt` must survive too:
        // the new Gitignore matcher never flips to whitelist-only mode, so
        // adding an include cannot silently drop unrelated files.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), ".env\n");
        write(&root.join(".env"), "x");
        write(&root.join("keep.txt"), "x");

        let src = source_at(root, true, &[".env"], &[]);
        let names = walked_names(&src);
        assert!(
            names.contains(&".env".to_string()),
            "include_pattern must re-include the gitignored .env: {names:?}"
        );
        assert!(
            names.contains(&"keep.txt".to_string()),
            "adding an include must never drop unrelated files: {names:?}"
        );
    }

    #[test]
    fn gitignore_reinclude_beats_default_exclude() {
        // F5: a gitignore `!Thumbs.db` re-includes Thumbs.db despite the
        // DESIGN s5.2 default exclude - "gitignore wins where they conflict".
        // This is the defaults-vs-gitignore precedence the old single-Override
        // inverted; the new last-match-wins matcher adds defaults BELOW the
        // gitignore cascade so the `!Thumbs.db` rule overrides them.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "!Thumbs.db\n");
        write(&root.join("Thumbs.db"), "x");
        write(&root.join("real.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let names = walked_names(&src);
        assert!(names.contains(&"real.txt".to_string()), "{names:?}");
        assert!(
            names.contains(&"Thumbs.db".to_string()),
            "gitignore !Thumbs.db must re-include over the default exclude: {names:?}"
        );
    }

    #[test]
    fn exclude_pattern_wins_over_gitignore_include() {
        // `*.log` force-out wins even though gitignore would include logs
        // (ROADMAP "*.log excluded" row).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // No gitignore rule for *.log, so gitignore "would include" it.
        write(&root.join("app.log"), "x");
        write(&root.join("keep.txt"), "x");

        let src = source_at(root, true, &[], &["*.log"]);
        let names = walked_names(&src);
        assert!(names.contains(&"keep.txt".to_string()), "{names:?}");
        assert!(
            !names.contains(&"app.log".to_string()),
            "exclude_pattern *.log must force-out: {names:?}"
        );
    }

    #[test]
    fn default_exclude_drops_os_noise() {
        // A default-exclude (.DS_Store / Thumbs.db) is dropped with no
        // user rule and no gitignore.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".DS_Store"), "x");
        write(&root.join("Thumbs.db"), "x");
        write(&root.join("real.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let names = walked_names(&src);
        assert!(names.contains(&"real.txt".to_string()), "{names:?}");
        assert!(
            !names.contains(&".DS_Store".to_string()),
            ".DS_Store must be a default-exclude: {names:?}"
        );
        assert!(
            !names.contains(&"Thumbs.db".to_string()),
            "Thumbs.db must be a default-exclude: {names:?}"
        );
    }

    #[test]
    fn matcher_passthrough_for_unmatched() {
        // An ordinary file matching no include/exclude/default still passes
        // - the Gitignore matcher returns Match::None (included) for an
        // unmatched path even when include_patterns are present, so it never
        // flips to a whitelist-only mode that would drop unrelated files.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("ordinary.dat"), "x");

        let src = source_at(root, true, &[".env"], &["*.log"]);
        let names = walked_names(&src);
        assert!(
            names.contains(&"ordinary.dat".to_string()),
            "unmatched ordinary file must pass through: {names:?}"
        );
    }
}
