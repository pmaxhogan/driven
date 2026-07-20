//! gitignore + default + custom excludes; DESIGN s5.2.
//!
//! Builds the [`SourceMatcher`] ([`build_source_matcher`]) the scanner (SPEC
//! s6) consults for every entry, plus the plain [`ignore::WalkBuilder`] it
//! walks with. ALL ignore decisions are made by our matcher; the walker does
//! NO ignore logic of its own (its built-in gitignore / git-exclude /
//! git-global layers are turned off).
//!
//! ## True per-directory cascade (DESIGN s5.2)
//!
//! The matcher is a STACK of per-directory [`ignore::gitignore::Gitignore`]
//! scopes rather than one matcher flattened at the source root. Each nested
//! `.gitignore` / `.ignore` becomes its own scope rooted at that file's
//! directory, and a scope is consulted ONLY for paths at or under its
//! directory. This gives real gitignore scoping: a rule in `sub/.gitignore`
//! applies only under `sub/` (an unanchored pattern matches at any depth BELOW
//! its own file, an anchored `/foo` only at that directory) - it can no longer
//! leak into a sibling `other/` tree the way the old single flattened matcher
//! did. The matcher stays fully QUERYABLE for an arbitrary path (including one
//! not on disk), which the scanner's deletion / excluded-orphan split (DESIGN
//! s5.5) depends on - so ignore decisions are NOT delegated to the walker's
//! native layer (that would only decide entries it actually visits).
//!
//! ## Precedence (LAST matching scope wins; permissive re-include)
//!
//! Scopes are ordered LOWEST-precedence FIRST; [`SourceMatcher::is_included`]
//! evaluates a path against every applicable scope and the LAST non-`None`
//! match decides (a deeper / higher-tier scope overrides a shallower one):
//!
//! 1. (lowest) the DESIGN s5.2 DEFAULT EXCLUDE list (OS noise / editor swap /
//!    misc transient globs), one root-rooted scope of bare exclude globs.
//! 2. the `.gitignore` cascade, then the `.ignore` cascade, IF
//!    [`SourceRow::respect_gitignore`]: every such file becomes a per-directory
//!    scope, root-first, so a deeper file's rule wins over a shallower one. A
//!    user `!Thumbs.db` in gitignore therefore beats the default Thumbs.db
//!    exclude (DESIGN s5.2: "gitignore wins where they conflict").
//! 3. the repo-local `<root>/.git/info/exclude`, then the global gitignore -
//!    each a root-rooted scope above the cascade.
//! 4. (highest) the source's own `exclude_patterns` (bare globs = force-out,
//!    e.g. `*.log`) then `include_patterns` (`!`-prefixed = re-include, e.g.
//!    a bare `.env` that opts a gitignored secret back in), one root-rooted
//!    scope added LAST so it beats BOTH gitignore and the defaults.
//!
//! Note we DELIBERATELY do NOT replicate git's rule that "a file cannot be
//! re-included if a parent directory is excluded": a nested `!keep.txt` under
//! an excluded `vendor/` re-includes the file. That permissive choice is a
//! backup-safety invariant (when in doubt, do not drop a backed-up file); the
//! last-match-wins evaluation across scopes preserves it because the deeper
//! whitelist scope is consulted after (and overrides) the shallower exclude.
//!
//! ## Glob semantics (true gitignore, NOT the inverted `Override` form)
//!
//! These are real gitignore rules: a *bare* glob EXCLUDES and a leading `!`
//! RE-INCLUDES. So `exclude_patterns` are added verbatim and `include_patterns`
//! get a `!` prepended. A source whose intent is "re-include `.env`" stores
//! the bare string `.env` in `include_patterns` (the matcher prepends the
//! `!`); the "e.g. !.env" wording in the SPEC describes the user-facing
//! *effect*, not the stored glob. Unmatched paths stay `Match::None`
//! (included) and `!`-rules re-include naturally without any whitelist-only
//! mode dropping unrelated files.

use std::path::{Path, PathBuf};

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
    // VCS internals. Excluded by default (toggleable): a working tree's files
    // are backed up as ordinary files, but .git/ itself is large, churns on
    // every commit/fetch/gc, and is mostly redundant with a remote. Backing up
    // only unpushed objects is not feasible in a file-copy model (it needs
    // git bundle synthesis + git-aware restore - a V2+ feature), so .git/ is
    // excluded by default. A user with local-only/unpushed repos (incl.
    // stashes, which live only in .git/) re-includes it per-source via
    // include_patterns. See DESIGN s5.2.
    ".git/",
];

/// One directory-scoped [`Gitignore`] in a [`SourceMatcher`]'s cascade.
///
/// `matcher` is built rooted at `dir`, and it is consulted ONLY for paths at or
/// under `dir` (the per-directory scoping). For the source-level tiers
/// (defaults, `.git/info/exclude`, global, and the source's own
/// include/exclude patterns) `dir` is the source root; for a nested
/// `.gitignore` / `.ignore` it is that file's own directory.
#[derive(Debug)]
struct Scope {
    /// Absolute directory this scope applies at or under.
    dir: PathBuf,
    /// The rules for this scope, rooted at `dir`.
    matcher: Gitignore,
}

/// The per-directory include/exclude decision cascade for one source
/// ([`build_source_matcher`]). A STACK of [`Scope`]s ordered lowest-precedence
/// first; the scanner consults it for BOTH the walk filter and the
/// excluded-orphan split (DESIGN s5.5) via [`SourceMatcher::is_included`], so
/// include/exclude semantics are identical in both places (and queryable for
/// paths that are not on disk).
#[derive(Debug)]
pub struct SourceMatcher {
    /// Absolute source root; every queried relative path is joined onto it so
    /// each scope matches an absolute path (the form the `ignore` crate strips
    /// against its scope root).
    root: PathBuf,
    /// Scopes in ascending precedence: defaults, the `.gitignore` cascade, the
    /// `.ignore` cascade, `.git/info/exclude`, global, then the source's own
    /// exclude/include overrides (highest). Last matching scope wins.
    scopes: Vec<Scope>,
    /// True when ANY scope can RE-INCLUDE a path a broader rule excluded - i.e.
    /// the source has `include_patterns` (each stored as a `!`-re-include) OR
    /// any tier (`.gitignore` / `.ignore` / `.git/info/exclude` / global /
    /// `core.excludesFile`) contributed a `!`-prefixed whitelist rule. Used to
    /// disable directory pruning in [`build_walker`]: a nested `!keep.txt`
    /// under an excluded parent dir is classified INCLUDED (the permissive
    /// re-include), so pruning that parent (never walking it) would leave the
    /// file unseen and the orphan split would false-classify it `deleted` and
    /// trash a file that still exists (P1-1, data loss). When this is set the
    /// walker prunes nothing and the per-file matcher decides every path,
    /// keeping the walk filter and orphan split in lockstep.
    has_negations: bool,
}

impl SourceMatcher {
    /// Whether any rule can re-include an otherwise-excluded path (see the
    /// [`SourceMatcher::has_negations`] field docs). [`build_walker`] gates
    /// excluded-directory pruning on this being `false`.
    pub fn has_negations(&self) -> bool {
        self.has_negations
    }

    /// Whether `rel` (a source-root-relative path) is INCLUDED under the
    /// current rules. `is_dir` distinguishes a directory from a file so a
    /// trailing-slash gitignore rule (`node_modules/`) applies correctly.
    ///
    /// Joins `rel` onto the source root and evaluates the absolute path against
    /// every scope whose directory is an ancestor (per-directory scoping); the
    /// LAST non-`None` match decides (a deeper / higher-tier scope overrides a
    /// shallower one), and only an `Ignore` excludes - a `Whitelist`
    /// (`!`-re-include) or no match at all means INCLUDED. Each scope is
    /// consulted with [`Gitignore::matched_path_or_any_parents`], NOT `matched`,
    /// so a directory-scoped rule (`node_modules/`) excludes files beneath it;
    /// the `starts_with` ancestor guard also keeps that call from panicking on a
    /// path outside a scope's root.
    pub fn is_included(&self, rel: &Path, is_dir: bool) -> bool {
        let abs = self.root.join(rel);
        // `None` = undecided so far; `Some(true)` = last match ignored;
        // `Some(false)` = last match whitelisted (re-included).
        let mut ignored: Option<bool> = None;
        for scope in &self.scopes {
            // A scope applies only to paths at or under its directory. This is
            // the per-directory scoping AND it guards the call below, which
            // panics if `abs` is not under the scope's root.
            if !abs.starts_with(&scope.dir) {
                continue;
            }
            let m = scope.matcher.matched_path_or_any_parents(&abs, is_dir);
            if m.is_ignore() {
                ignored = Some(true);
            } else if m.is_whitelist() {
                ignored = Some(false);
            }
            // `Match::None` leaves the running decision unchanged.
        }
        !matches!(ignored, Some(true))
    }
}

/// Builds one directory-scoped [`Scope`] from a set of gitignore lines rooted at
/// `dir` (the source-level tiers: defaults + the source's own patterns).
fn scope_from_lines<'a>(
    dir: &Path,
    lines: impl IntoIterator<Item = &'a str>,
    label: &str,
) -> anyhow::Result<Scope> {
    let mut gb = GitignoreBuilder::new(dir);
    for line in lines {
        gb.add_line(None, line)
            .map_err(|e| anyhow::anyhow!("adding {label} `{line}`: {e}"))?;
    }
    let matcher = gb
        .build()
        .map_err(|e| anyhow::anyhow!("building {label} matcher: {e}"))?;
    Ok(Scope {
        dir: dir.to_path_buf(),
        matcher,
    })
}

/// Builds one directory-scoped [`Scope`] from a nested ignore FILE, rooted at
/// that file's own directory (the true per-dir cascade). A missing/unreadable
/// file or a parse error is non-fatal - the scope simply contributes no rules
/// (or is skipped), never aborting the whole matcher (a scan must not fail
/// because one `.gitignore` was malformed). Returns `None` when the scope could
/// not be built at all.
fn scope_from_file(
    source: &SourceRow,
    root: &Path,
    ignore_file: &Path,
    label: &str,
) -> Option<Scope> {
    let dir = ignore_file.parent().unwrap_or(root).to_path_buf();
    let mut gb = GitignoreBuilder::new(&dir);
    // `GitignoreBuilder::add` returns Some(err) on a partial/parse error; a
    // missing or unreadable file is non-fatal (no rules applied) so only log.
    if let Some(err) = gb.add(ignore_file) {
        tracing::warn!(
            target: TARGET,
            source_id = %source.id,
            path = %ignore_file.display(),
            %err,
            "failed to parse {label}; ignoring its rules",
        );
    }
    match gb.build() {
        Ok(matcher) => Some(Scope { dir, matcher }),
        Err(err) => {
            tracing::warn!(
                target: TARGET,
                source_id = %source.id,
                path = %ignore_file.display(),
                %err,
                "failed to build {label} scope; skipping it",
            );
            None
        }
    }
}

/// Builds the per-directory [`SourceMatcher`] cascade for `source` (DESIGN
/// s5.2). Scopes are pushed in LAST-MATCH-WINS order (see the module docs):
/// defaults (lowest), then - if `respect_gitignore` - the `.gitignore` cascade,
/// the `.ignore` cascade, `<root>/.git/info/exclude`, and the global gitignore,
/// then the source's `exclude_patterns` + `include_patterns` (highest). Each
/// nested ignore file is its OWN scope rooted at that file's directory, so its
/// rules apply only under that directory.
pub fn build_source_matcher(source: &SourceRow) -> anyhow::Result<SourceMatcher> {
    let root = PathBuf::from(&source.local_path);
    let mut scopes: Vec<Scope> = Vec::new();

    // 1. (lowest) DESIGN s5.2 default excludes - one root-rooted scope of bare
    //    gitignore globs, so they apply at every depth below the root.
    scopes.push(scope_from_lines(
        &root,
        DEFAULT_EXCLUDES.iter().copied(),
        "default exclude",
    )?);

    // 2. the gitignore tier (DESIGN s5.2: respect .gitignore, .ignore,
    //    .git/info/exclude, and the global gitignore), each ABOVE the defaults
    //    and BELOW the source's own overrides. The `.gitignore` cascade then the
    //    `.ignore` cascade (`.ignore` overrides `.gitignore`, matching the
    //    `ignore` crate) - each file a per-directory scope, root-first so a
    //    deeper file's rule wins over a shallower one.
    if source.respect_gitignore {
        for filename in [".gitignore", ".ignore"] {
            for ignore_file in collect_ignore_files(&root, filename) {
                if let Some(scope) = scope_from_file(source, &root, &ignore_file, "an ignore file")
                {
                    scopes.push(scope);
                }
            }
        }

        // `<root>/.git/info/exclude` - the repo-local private exclude list,
        // rooted at the source root.
        let info_exclude = root.join(".git").join("info").join("exclude");
        if info_exclude.is_file() {
            if let Some(scope) = scope_from_file(source, &root, &info_exclude, ".git/info/exclude")
            {
                scopes.push(scope);
            }
        }

        // Global gitignore (DESIGN s5.2). Resolved by [`global_gitignore_path`]:
        // git's own `core.excludesFile` when set, else `$XDG_CONFIG_HOME/git/ignore`,
        // else `~/.config/git/ignore`. Wired here but not hermetically tested -
        // $XDG_CONFIG_HOME / $HOME and the machine-global git config would race
        // parallel tests (see the exclude tests' note); a focused unit test
        // instead proves the tier loads via `.git/info/exclude` + `.ignore`.
        // Rooted at the source root so its rules apply tree-wide.
        if let Some(global) = global_gitignore_path() {
            if global.is_file() {
                if let Some(scope) = scope_from_file(source, &root, &global, "global gitignore") {
                    scopes.push(scope);
                }
            }
        }
    }

    // 3. (highest) the source's own overrides: exclude_patterns force-out (bare
    //    glob), then include_patterns opt-back-in (`!`-prefixed), one root-rooted
    //    scope added LAST so it beats both gitignore and the defaults. Built in a
    //    single scope so the include (`!`) rules override the exclude rules
    //    within it (last-match-wins inside the one Gitignore).
    let override_lines: Vec<String> = source
        .exclude_patterns
        .iter()
        .cloned()
        .chain(source.include_patterns.iter().map(|inc| format!("!{inc}")))
        .collect();
    scopes.push(scope_from_lines(
        &root,
        override_lines.iter().map(String::as_str),
        "source override pattern",
    )?);

    // P1-1: a source has negations when it carries `include_patterns` (each
    // added as a `!`-re-include) OR any scope contributed a `!`-prefixed
    // whitelist rule. `Gitignore::num_whitelists` counts exactly those `!`-rules
    // per scope (cheaper and more correct than re-reading each ignore file to
    // scan for `!` lines - it handles escaped `\!`, every tier, and read
    // failures identically to how the matcher itself parsed them).
    let num_whitelists: u64 = scopes.iter().map(|s| s.matcher.num_whitelists()).sum();
    let has_negations = !source.include_patterns.is_empty() || num_whitelists > 0;

    tracing::debug!(
        target: TARGET,
        source_id = %source.id,
        respect_gitignore = source.respect_gitignore,
        includes = source.include_patterns.len(),
        excludes = source.exclude_patterns.len(),
        num_scopes = scopes.len(),
        num_whitelists,
        has_negations,
        "built source matcher"
    );
    Ok(SourceMatcher {
        root,
        scopes,
        has_negations,
    })
}

/// Max TOTAL number of include + exclude patterns a single source may carry
/// (R3-P2-1, DESIGN 18.8: "per-source max 256 patterns total"). A backup
/// source's rule list is small in practice; an unbounded list from a compromised
/// renderer would bloat the matcher build + every scan decision, so the COMBINED
/// include + exclude count is capped here.
pub const MAX_PATTERNS_TOTAL: usize = 256;

/// Max length (in CHARS) of a single include / exclude glob pattern (R3-P2-1,
/// DESIGN 18.8: "per-pattern max 512 chars"). A real glob is short; a
/// pathologically long one is rejected before it can reach the matcher / SQLite.
pub const MAX_PATTERN_LEN: usize = 512;

/// An invalid include / exclude pattern rejected by [`validate_patterns`]
/// (R2-P1-3). Carries a human-readable reason; the IPC layer maps it to the
/// stable `internal.invalid_input` SPEC s24 code.
#[derive(Debug, thiserror::Error)]
#[error("invalid backup pattern: {0}")]
pub struct PatternValidationError(pub String);

/// Validate a source's candidate include + exclude glob patterns BEFORE they
/// are persisted (R2-P1-3, DESIGN s5.2). Called by `add_source` AND
/// `update_source` so an invalid / oversized glob can never reach SQLite and
/// then break the next scan's matcher build.
///
/// Enforces (DESIGN 18.8):
/// 1. the COMBINED include + exclude count is at most [`MAX_PATTERNS_TOTAL`];
/// 2. each pattern at most [`MAX_PATTERN_LEN`] chars, and non-empty after trim;
/// 3. each pattern COMPILES under the SAME [`GitignoreBuilder::add_line`] the
///    scanner uses in [`build_source_matcher`] (an `exclude` verbatim, an
///    `include` as its `!`-re-include form) - so a glob the scanner would later
///    reject is rejected up front instead.
///
/// Returns `Ok(())` when every pattern is valid, else the first
/// [`PatternValidationError`].
pub fn validate_patterns(
    include_patterns: &[String],
    exclude_patterns: &[String],
) -> Result<(), PatternValidationError> {
    let total = include_patterns.len() + exclude_patterns.len();
    if total > MAX_PATTERNS_TOTAL {
        return Err(PatternValidationError(format!(
            "too many patterns ({total} include+exclude, max {MAX_PATTERNS_TOTAL})"
        )));
    }

    // Compile each candidate with the SAME builder the scanner uses, rooted at a
    // neutral path (pattern syntax validity does not depend on the root). An
    // `exclude` is added verbatim; an `include` is added as `!<pat>` (the
    // re-include form `build_source_matcher` uses), so the validation matches the
    // exact line each side will produce later.
    let mut builder = GitignoreBuilder::new(Path::new("/"));
    for exc in exclude_patterns {
        check_one_pattern(exc)?;
        builder
            .add_line(None, exc)
            .map_err(|e| PatternValidationError(format!("exclude pattern `{exc}`: {e}")))?;
    }
    for inc in include_patterns {
        check_one_pattern(inc)?;
        let reinclude = format!("!{inc}");
        builder
            .add_line(None, &reinclude)
            .map_err(|e| PatternValidationError(format!("include pattern `{inc}`: {e}")))?;
    }
    // Build once to surface any defect the per-line add did not catch.
    builder.build().map_err(|e| {
        PatternValidationError(format!("patterns do not form a valid matcher: {e}"))
    })?;
    Ok(())
}

/// Per-pattern shape checks shared by both sides of [`validate_patterns`]:
/// reject an empty / whitespace-only pattern and one over [`MAX_PATTERN_LEN`]
/// chars (DESIGN 18.8 caps the length in CHARS, not bytes).
fn check_one_pattern(pat: &str) -> Result<(), PatternValidationError> {
    if pat.trim().is_empty() {
        return Err(PatternValidationError(
            "pattern must not be empty or whitespace-only".to_string(),
        ));
    }
    let char_len = pat.chars().count();
    if char_len > MAX_PATTERN_LEN {
        return Err(PatternValidationError(format!(
            "pattern is too long ({char_len} chars, max {MAX_PATTERN_LEN})"
        )));
    }
    Ok(())
}

/// Collects every file named `filename` (e.g. `.gitignore` or `.ignore`)
/// under `root`, root-first (shallowest first), so [`build_source_matcher`]
/// can add them in last-match-wins order where a deeper file's rule overrides
/// a shallower one.
///
/// A dependency-free breadth-first `std::fs::read_dir` walk: BFS visits
/// shallower directories before deeper ones, giving the root-first ordering
/// directly. Symlinked directories are NOT descended (mirrors the scanner's
/// `follow_links(false)` policy, DESIGN s5.2.1, and avoids cycles). I/O errors
/// on a directory are logged and that subtree skipped - never fatal, since a
/// failed enumerate just means we apply fewer ignore rules, never that we
/// wrongly back up or drop a file (the per-entry walk re-checks each path).
// TODO(perf): prune directories the matcher already excludes (e.g.
//   `node_modules`) so we do not descend them just to find an ignore file.
fn collect_ignore_files(root: &Path, filename: &str) -> Vec<std::path::PathBuf> {
    let target_name = std::ffi::OsStr::new(filename);
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(root.to_path_buf());

    while let Some(dir) = queue.pop_front() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(err) => {
                tracing::debug!(target: TARGET, path = %dir.display(), %err, "read_dir failed while collecting ignore files; skipping subtree");
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                // Do not follow symlinks (cycle / out-of-root safety).
                Ok(ft) if ft.is_dir() => queue.push_back(path),
                Ok(ft) if ft.is_file() && entry.file_name() == target_name => {
                    found.push(path);
                }
                _ => {}
            }
        }
    }
    found
}

/// Resolves the global gitignore path (DESIGN s5.2).
///
/// Mirrors git's own resolution order: a configured `core.excludesFile`
/// (P1-2) takes precedence and REPLACES the default slot; when it is unset (or
/// git is unavailable) this falls back to `$XDG_CONFIG_HOME/git/ignore`, then
/// to `~/.config/git/ignore` when `$XDG_CONFIG_HOME` is unset/empty. Returns
/// `None` when none of those resolve to a usable path.
///
/// `core.excludesFile` is read by shelling out to `git config --get
/// core.excludesFile` (no in-process git-config parser). Driven must NOT
/// hard-require git: a missing binary or an unset key is a graceful skip (the
/// XDG/`~` fallback still applies), never an error.
fn global_gitignore_path() -> Option<std::path::PathBuf> {
    // 1. `core.excludesFile` from git config, if git is present and it is set.
    if let Some(path) = git_core_excludes_file() {
        return Some(path);
    }

    // 2. XDG / `~/.config` fallback.
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(xdg) if !xdg.is_empty() => std::path::PathBuf::from(xdg),
        _ => {
            // `~/.config`. `HOME` covers Unix; `USERPROFILE` covers Windows.
            let home = home_dir()?;
            home.join(".config")
        }
    };
    Some(base.join("git").join("ignore"))
}

/// Reads git's `core.excludesFile` via `git config --get core.excludesFile`
/// (P1-2). Returns the resolved path only when git is on PATH, the key is set,
/// and the value (after `~` expansion) names an existing file; otherwise
/// `None` so the caller falls back to the XDG/`~` default.
///
/// Driven does not hard-require git: any failure to run git, a non-zero exit
/// (key unset), unreadable output, or a non-file path all yield `None` rather
/// than propagating an error.
fn git_core_excludes_file() -> Option<std::path::PathBuf> {
    let output = std::process::Command::new("git")
        .args(["config", "--get", "core.excludesFile"])
        .output()
        .ok()?;
    if !output.status.success() {
        // Exit 1 = key not set; any other failure also falls back gracefully.
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let expanded = expand_tilde(trimmed);
    if expanded.is_file() {
        Some(expanded)
    } else {
        None
    }
}

/// The user's home directory: `HOME` (Unix) or `USERPROFILE` (Windows).
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// Expands a leading `~` (a `~`-only string, or `~/...`) to the home dir.
/// Any other input - including a `~user` form we do not resolve - is returned
/// verbatim as a path.
fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

/// Builds the configured [`WalkBuilder`] for `source` (SPEC s6
/// `build_walker`).
///
/// The walker does NO include/exclude decision of its own: `git_ignore` /
/// `git_exclude` / `git_global` / `ignore` (`.ignore` files) / `parents`
/// (parent-directory ignore files ABOVE the source root) are all turned OFF
/// because [`build_source_matcher`] is the SOLE ignore authority. Leaving the
/// `ignore` crate's native `.ignore` handling on (its default) would let the
/// WalkBuilder silently drop a `.ignore`-hidden file the matcher does not know
/// about, so the scanner's orphan split would misclassify it as `deleted` and
/// trash it on Drive; turning it off here and loading `.ignore` into the
/// matcher keeps both in lockstep. `parents(false)` scopes the walk to the
/// source root, which is the backup boundary.
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
///
/// ## Excluded-directory pruning (perf)
///
/// When the matcher has NO negations, a [`WalkBuilder::filter_entry`] closure
/// prunes any directory the matcher excludes (e.g. `node_modules`, `build`),
/// so the walk never descends it - the whole point of P2-1. The closure only
/// ever prunes directories (files are still decided per-entry by the scanner's
/// matcher check) and never prunes the root. It is GATED on
/// [`SourceMatcher::has_negations`] being `false` (P1-1): ANY `!`-re-include -
/// whether from `include_patterns` OR a nested `.gitignore`/`.ignore`
/// negation - could re-include a file INSIDE an excluded directory, and
/// pruning that directory would leave the file un-walked. Worse than a missed
/// file: because the flattened matcher would still classify that path INCLUDED,
/// the scanner's orphan split would read a stored `file_state` row for it as a
/// genuine deletion and trash a file that still exists (data loss). With no
/// negations, an excluded directory contains only excluded files, so pruning
/// is safe and the walk filter / orphan split stay consistent. `has_negations`
/// subsumes the old `include_patterns.is_empty()` gate (every include pattern
/// becomes a `!`-rule, so it sets `has_negations`). The closure owns its own
/// [`SourceMatcher`] because `filter_entry` requires a `'static + Send + Sync`
/// predicate; the scanner builds a separate matcher for its per-entry /
/// orphan-split checks.
pub fn build_walker(source: &SourceRow) -> anyhow::Result<WalkBuilder> {
    let mut wb = WalkBuilder::new(&source.local_path);
    wb.git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .ignore(false)
        .parents(false)
        .require_git(false)
        .hidden(false)
        .follow_links(false);

    // P2-1: prune excluded DIRECTORIES so the walk never descends e.g.
    // node_modules just to discard each file. P1-1: only safe when the matcher
    // has NO negations - any `!`-re-include (from include_patterns OR a nested
    // .gitignore/.ignore) could live inside an excluded dir, and pruning that
    // dir while the flattened matcher still classifies the re-included file as
    // INCLUDED would make the orphan split false-delete a still-present file.
    let prune_matcher = build_source_matcher(source)?;
    if !prune_matcher.has_negations() {
        // TODO(perf): prune excluded dirs even with negations (needs ancestor check)
        let root = std::path::PathBuf::from(&source.local_path);
        wb.filter_entry(move |entry| {
            // Never prune the root, and only ever act on directories - files
            // are decided per-entry by the scanner's matcher check.
            if entry.depth() == 0 {
                return true;
            }
            if !entry.file_type().is_some_and(|t| t.is_dir()) {
                return true;
            }
            let rel = match entry.path().strip_prefix(&root) {
                Ok(r) if !r.as_os_str().is_empty() => r,
                // Not under root or empty - leave it to the per-entry check.
                _ => return true,
            };
            // Keep (descend) iff the directory is included.
            prune_matcher.is_included(rel, true)
        });
    }

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

    #[test]
    fn validate_patterns_accepts_valid_and_rejects_count_length_and_invalid() {
        // R2-P1-3: the matcher-backed pattern validator the IPC layer calls on
        // both add_source and update_source.

        // Valid globs (the kind the scanner compiles) are accepted.
        validate_patterns(
            &["*.md".to_string()],
            &["*.log".to_string(), "build/".to_string()],
        )
        .expect("valid include/exclude globs accepted");
        validate_patterns(&[], &[]).expect("empty rule sets are valid");

        // R3-P2-1 (DESIGN 18.8): the TOTAL include+exclude count is capped at
        // MAX_PATTERNS_TOTAL. Exactly at the cap is allowed; one past is rejected.
        let at_cap: Vec<String> = (0..MAX_PATTERNS_TOTAL).map(|i| format!("f{i}")).collect();
        assert_eq!(at_cap.len(), MAX_PATTERNS_TOTAL);
        validate_patterns(&[], &at_cap).expect("exactly the total cap is allowed");

        // One past the total (split across both sides) is rejected - proving the
        // cap is COMBINED, not per-side.
        let half = MAX_PATTERNS_TOTAL / 2;
        let inc: Vec<String> = (0..=half).map(|i| format!("i{i}")).collect();
        let exc: Vec<String> = (0..=half).map(|i| format!("e{i}")).collect();
        assert!(inc.len() + exc.len() > MAX_PATTERNS_TOTAL);
        validate_patterns(&inc, &exc).expect_err("over-count combined include+exclude rejected");
        let too_many: Vec<String> = (0..=MAX_PATTERNS_TOTAL).map(|i| format!("f{i}")).collect();
        assert_eq!(too_many.len(), MAX_PATTERNS_TOTAL + 1);
        validate_patterns(&[], &too_many).expect_err("over-count excludes rejected");
        validate_patterns(&too_many, &[]).expect_err("over-count includes rejected");

        // Over-length (one past the per-pattern CHAR cap) is rejected; exactly at
        // the cap is allowed.
        let at_len = "a".repeat(MAX_PATTERN_LEN);
        validate_patterns(&[], std::slice::from_ref(&at_len))
            .expect("exactly the length cap is allowed");
        let too_long = "a".repeat(MAX_PATTERN_LEN + 1);
        validate_patterns(&[], std::slice::from_ref(&too_long))
            .expect_err("over-length exclude rejected");
        validate_patterns(std::slice::from_ref(&too_long), &[])
            .expect_err("over-length include rejected");

        // Empty / whitespace-only patterns are rejected (they would be a no-op or
        // a footgun in the matcher).
        validate_patterns(&[], &["   ".to_string()]).expect_err("blank pattern rejected");

        // An invalid glob the matcher cannot compile is rejected. The gitignore
        // builder rejects a pattern ending in an unescaped trailing backslash
        // (a dangling escape), which is exactly the kind of glob that would later
        // fail the scanner's matcher build - caught here up front instead.
        let bad = "abc\\".to_string();
        validate_patterns(&[], std::slice::from_ref(&bad))
            .expect_err("an uncompilable glob must be rejected");
        validate_patterns(std::slice::from_ref(&bad), &[])
            .expect_err("an uncompilable include glob must be rejected");
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
            drive_id: None,
            drive_folder_path: "/f".into(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore,
            include_patterns: include.iter().map(|s| s.to_string()).collect(),
            exclude_patterns: exclude.iter().map(|s| s.to_string()).collect(),
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            mtime_granularity_ns: None,
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
    fn dotgit_excluded_by_default_and_reincludable() {
        // `.git/` is a default-exclude (DESIGN s5.2 VCS internals): its contents
        // are dropped by default, but the working-tree files alongside it stay,
        // and a source can opt the whole dir back in via include_patterns.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".git/config"), "x");
        write(&root.join(".git/objects/pack/p.pack"), "x");
        write(&root.join("src.txt"), "x");

        let default_src = source_at(root, true, &[], &[]);
        let names = walked_names(&default_src);
        assert!(names.contains(&"src.txt".to_string()), "{names:?}");
        assert!(
            !names.iter().any(|n| n.starts_with(".git/")),
            ".git/ must be excluded by default: {names:?}"
        );

        // Re-include via include_patterns (the per-source escape hatch).
        let reinclude_src = source_at(root, true, &[".git/"], &[]);
        let reincluded = walked_names(&reinclude_src);
        assert!(
            reincluded.iter().any(|n| n.starts_with(".git/")),
            ".git/ must be re-includable via include_patterns: {reincluded:?}"
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

    #[test]
    fn dot_ignore_file_excludes_from_walk() {
        // P1-2: a `.ignore` rule (identical gitignore syntax) must exclude a
        // file from the walk. Before the fix the matcher only loaded
        // `.gitignore`, so a `.ignore`-hidden file leaked through here while
        // the WalkBuilder's native `.ignore` layer dropped it - the
        // misclassification the orphan split would later read as `deleted`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".ignore"), "hidden.txt\n");
        write(&root.join("hidden.txt"), "x");
        write(&root.join("keep.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let names = walked_names(&src);
        assert!(names.contains(&"keep.txt".to_string()), "{names:?}");
        assert!(
            !names.contains(&"hidden.txt".to_string()),
            ".ignore rule must exclude the file from the walk: {names:?}"
        );
    }

    #[test]
    fn git_info_exclude_excludes_from_walk() {
        // P1-2: `<root>/.git/info/exclude` rules must be honoured (DESIGN s5.2;
        // the matcher previously ignored them entirely - a privacy regression).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".git/info/exclude"), "secret.bin\n");
        write(&root.join("secret.bin"), "x");
        write(&root.join("keep.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let names = walked_names(&src);
        assert!(names.contains(&"keep.txt".to_string()), "{names:?}");
        assert!(
            !names.contains(&"secret.bin".to_string()),
            ".git/info/exclude rule must exclude the file from the walk: {names:?}"
        );
    }

    #[test]
    fn gitignore_tier_loads_ignore_and_info_exclude() {
        // Focused unit test proving the gitignore TIER wires both `.ignore`
        // and `.git/info/exclude` into a SINGLE matcher (the global gitignore
        // is also wired in `build_source_matcher` but is NOT hermetically
        // tested here because $XDG_CONFIG_HOME / $HOME are process-global and
        // would race parallel tests). Asserts on the matcher directly rather
        // than driving a walk so it does not depend on the WalkBuilder.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".ignore"), "a.dat\n");
        write(&root.join(".git/info/exclude"), "b.dat\n");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(
            !matcher.is_included(Path::new("a.dat"), false),
            ".ignore rule must exclude via the matcher"
        );
        assert!(
            !matcher.is_included(Path::new("b.dat"), false),
            ".git/info/exclude rule must exclude via the matcher"
        );
        assert!(
            matcher.is_included(Path::new("c.dat"), false),
            "an unmatched file must remain included"
        );
    }

    #[test]
    fn excluded_dir_is_pruned_when_no_include_patterns() {
        // P2-1: a gitignored directory (`skip/`) must NOT be descended when
        // there are no include_patterns, so the WalkBuilder::filter_entry
        // prune closure skips its whole subtree. A direct "was-it-traversed"
        // assertion is not feasible here (the matcher's `.gitignore`
        // collection is a SEPARATE read_dir BFS, independent of filter_entry),
        // so this keeps the correctness assertion: no file inside the excluded
        // dir leaks into the walk, and unrelated files are unaffected.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "skip/\n");
        write(&root.join("skip/inside.txt"), "x");
        write(&root.join("keep.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let names = walked_names(&src);
        assert!(names.contains(&"keep.txt".to_string()), "{names:?}");
        assert!(
            !names.contains(&"skip/inside.txt".to_string()),
            "file inside an excluded dir must not be walked: {names:?}"
        );
    }

    #[test]
    fn nested_negation_disables_pruning_and_keeps_reincluded_file() {
        // P1-1: a nested `.gitignore` negation (`vendor/.gitignore: !keep.txt`)
        // under an excluded parent (`vendor/`) sets has_negations, which MUST
        // disable dir-pruning so `vendor/` is walked and `keep.txt` survives.
        // If pruning stayed on, the walk would skip `vendor/` entirely and the
        // file would be lost from the walk (and the scanner's orphan split
        // would then false-delete its stored row - see the scanner test).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "vendor/\n");
        write(&root.join("vendor/.gitignore"), "!keep.txt\n");
        write(&root.join("vendor/keep.txt"), "x");
        write(&root.join("top.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(
            matcher.has_negations(),
            "a `!`-negation in any tier must set has_negations"
        );
        assert!(
            matcher.is_included(Path::new("vendor/keep.txt"), false),
            "the nested !keep.txt must re-include the file in the matcher"
        );

        let names = walked_names(&src);
        assert!(names.contains(&"top.txt".to_string()), "{names:?}");
        assert!(
            names.contains(&"vendor/keep.txt".to_string()),
            "pruning must be disabled so the re-included file is still walked: {names:?}"
        );
    }

    #[test]
    fn no_negations_when_only_plain_excludes() {
        // The gate's negative case: a source with only plain exclude rules and
        // no include_patterns / `!`-rules has NO negations, so pruning stays
        // enabled (the common, fast path).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "build/\n");
        write(&root.join("keep.txt"), "x");

        let src = source_at(root, true, &[], &["*.log"]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(
            !matcher.has_negations(),
            "plain excludes with no `!`-rule and no include_patterns must NOT set has_negations"
        );
    }

    // --- true per-directory cascade (DESIGN s5.2) ---------------------------

    #[test]
    fn nested_gitignore_is_scoped_to_its_directory() {
        // The core cascade fix: an unanchored rule in `sub/.gitignore` applies
        // ONLY under `sub/`. The old flattened matcher rooted every rule at the
        // source root, so `secret.txt` in `sub/.gitignore` wrongly excluded a
        // sibling `other/secret.txt` too. With true per-dir scoping only the
        // file under `sub/` is excluded.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("sub/.gitignore"), "secret.txt\n");
        write(&root.join("sub/secret.txt"), "x");
        write(&root.join("other/secret.txt"), "x");
        write(&root.join("keep.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(
            !matcher.is_included(Path::new("sub/secret.txt"), false),
            "the rule must exclude the file under its own directory"
        );
        assert!(
            matcher.is_included(Path::new("other/secret.txt"), false),
            "a nested rule must NOT leak into a sibling directory"
        );
        assert!(matcher.is_included(Path::new("keep.txt"), false));

        // And the same through the walk (belt-and-suspenders).
        let names = walked_names(&src);
        assert!(names.contains(&"other/secret.txt".to_string()), "{names:?}");
        assert!(names.contains(&"keep.txt".to_string()), "{names:?}");
        assert!(!names.contains(&"sub/secret.txt".to_string()), "{names:?}");
    }

    #[test]
    fn deeper_gitignore_overrides_shallower() {
        // A deeper `.gitignore` wins over a shallower one (last-match-wins across
        // scopes): root excludes `*.log`, `sub/.gitignore` re-includes
        // `important.log`. So `sub/important.log` survives, `sub/other.log` and
        // the root-level `top.log` are excluded.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "*.log\n");
        write(&root.join("sub/.gitignore"), "!important.log\n");
        write(&root.join("sub/important.log"), "x");
        write(&root.join("sub/other.log"), "x");
        write(&root.join("top.log"), "x");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(
            matcher.is_included(Path::new("sub/important.log"), false),
            "deeper !important.log must re-include over the shallower *.log"
        );
        assert!(!matcher.is_included(Path::new("sub/other.log"), false));
        assert!(!matcher.is_included(Path::new("top.log"), false));
        // A negation in a nested tier must set has_negations (pruning off).
        assert!(matcher.has_negations());
    }

    #[test]
    fn anchored_rule_in_nested_dir_is_directory_local() {
        // An ANCHORED rule (`/foo`) in a nested `.gitignore` matches only at that
        // directory, not deeper - the per-dir scope roots the anchor at `sub/`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("sub/.gitignore"), "/foo\n");
        write(&root.join("sub/foo"), "x");
        write(&root.join("sub/deep/foo"), "x");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(
            !matcher.is_included(Path::new("sub/foo"), false),
            "the anchored /foo matches at the nested dir"
        );
        assert!(
            matcher.is_included(Path::new("sub/deep/foo"), false),
            "the anchored /foo must NOT match one level deeper"
        );
    }

    #[test]
    fn unanchored_nested_rule_matches_any_depth_below_its_dir() {
        // An UNANCHORED rule in a nested `.gitignore` matches at any depth BELOW
        // its own directory (but still not in a sibling tree).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("sub/.gitignore"), "*.tmpx\n");
        write(&root.join("sub/a.tmpx"), "x");
        write(&root.join("sub/deep/b.tmpx"), "x");
        write(&root.join("other/c.tmpx"), "x");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(!matcher.is_included(Path::new("sub/a.tmpx"), false));
        assert!(
            !matcher.is_included(Path::new("sub/deep/b.tmpx"), false),
            "unanchored rule reaches any depth below its dir"
        );
        assert!(
            matcher.is_included(Path::new("other/c.tmpx"), false),
            "but never a sibling tree"
        );
    }
}
