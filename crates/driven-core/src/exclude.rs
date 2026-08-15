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
//! an excluded `vendor/` re-includes the file, PROVIDED that nested ignore file
//! was collected at all (see "Pruned ignore-file collection" below). That
//! permissive choice is a backup-safety invariant (when in doubt, do not drop a
//! backed-up file); the last-match-wins evaluation across scopes preserves it
//! because the deeper whitelist scope is consulted after (and overrides) the
//! shallower exclude.
//!
//! ## Pruned ignore-file collection (perf + the walk/matcher lockstep)
//!
//! [`build_source_matcher`] discovers the nested `.gitignore` / `.ignore` files
//! with ONE breadth-first pass ([`collect_ignore_scopes`]) that PRUNES the
//! subtrees it will never need: while descending, it evaluates each candidate
//! directory against the cascade built SO FAR (defaults + the source's own
//! patterns + `.git/info/exclude` + the global gitignore + every ignore file
//! already discovered at an ancestor depth - which is exactly the set of scopes
//! that can apply to that directory) and does not descend a directory that is
//! excluded AND that no whitelist rule can reach under
//! ([`SourceMatcher::negations_could_match_under`]). This is git's own rule -
//! git never reads a `.gitignore` inside an ignored directory - and it is what
//! keeps the matcher build off a 600k-file `node_modules` sweep.
//!
//! The consequence is deliberate and load-bearing: a `!keep.txt` inside a
//! nested `.gitignore` that lives under a PRUNED directory is absent from the
//! matcher, so `vendor/keep.txt` is classified EXCLUDED rather than re-included.
//! That keeps the matcher and the walk in exact lockstep - both are derived
//! from the same collected set - which is the P1-1 data-loss invariant: the
//! scanner's orphan split reads such a stored `file_state` row as an
//! `excluded_orphan` (no trash op) and NEVER as a deletion. A negation that
//! lives at or above the excluded directory still disables pruning and is
//! honoured exactly as before.
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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    /// The scope's `!`-whitelist rules, reduced to the shape
    /// [`SourceMatcher::negations_could_match_under`] needs.
    negations: Negations,
}

/// The `!`-whitelist rules of one [`Scope`], recorded at build time so the
/// walker can ask "could a re-include reach under this directory?" without
/// descending it (P2-1 negation-aware pruning).
///
/// ALWAYS conservative: `unknown` is set whenever the rules could not be read or
/// parsed with confidence, and it makes every query answer "yes, a negation
/// could match" - which only ever costs a directory we did not have to walk,
/// never a file we should have backed up.
#[derive(Debug, Default)]
struct Negations {
    /// The parsed whitelist rules, each relative to the scope's own directory.
    patterns: Vec<NegationPattern>,
    /// Set when the scope's rules could not be analysed exactly (an unreadable
    /// ignore file, or a parse that disagreed with the `ignore` crate's own
    /// whitelist count). Treated as "a negation could match anywhere at or under
    /// this scope".
    unknown: bool,
}

/// One `!`-whitelist rule, reduced to what the reachability check needs.
#[derive(Debug)]
struct NegationPattern {
    /// `true` when gitignore lets the rule match at ANY depth below its scope
    /// directory - i.e. it carries no leading and no interior `/` (a lone
    /// TRAILING `/`, the directory marker, does not anchor a rule). Such a rule
    /// can always reach under any directory in its scope.
    unanchored: bool,
    /// The `/`-split segments of an ANCHORED rule, relative to the scope
    /// directory, with the leading `/` and the trailing `/` removed. Empty for
    /// an unanchored rule (never consulted).
    segments: Vec<String>,
}

impl Negations {
    /// Whether this scope carries any whitelist rule at all (or is `unknown`).
    fn any(&self) -> bool {
        self.unknown || !self.patterns.is_empty()
    }

    /// Whether any whitelist rule here could match a path at or under the
    /// directory whose path RELATIVE to this scope's directory is `rel`
    /// (`rel` empty = the scope directory itself).
    fn could_match_under(&self, rel: &[String]) -> bool {
        if self.unknown {
            return true;
        }
        self.patterns.iter().any(|p| p.could_match_under(rel))
    }
}

impl NegationPattern {
    /// Whether this rule could match some path at or under the directory
    /// `rel` (segments relative to the rule's scope directory).
    ///
    /// Runs the pattern's segments as a tiny NFA over `rel`'s segments, with
    /// `**` consuming zero or more segments and `*` / `?` / `[...]` matching
    /// within one segment. Three outcomes count as "could match":
    ///
    /// - the pattern still has segments left after consuming all of `rel` - it
    ///   can match something deeper;
    /// - the pattern is consumed EXACTLY at `rel` - the directory itself is
    ///   whitelisted, which re-includes everything beneath it;
    /// - the pattern is consumed at an ANCESTOR of `rel` - same thing, reached
    ///   through [`Gitignore::matched_path_or_any_parents`], which retries every
    ///   parent as a directory.
    ///
    /// Only a pattern whose segments diverge from `rel` returns `false`, which
    /// is what lets `/myrepo/.env` prune `node_modules/` while `.env` (matching
    /// at any depth) prunes nothing.
    fn could_match_under(&self, rel: &[String]) -> bool {
        if self.unanchored {
            return true;
        }
        if self.segments.is_empty() {
            // A `!` rule that reduced to nothing matches broadly; be safe.
            return true;
        }
        // Reachable pattern positions, kept sorted + deduped (patterns are tiny,
        // so a Vec beats a set).
        let mut pos = vec![0usize];
        self.close_over_doublestars(&mut pos);
        for seg in rel {
            if pos.contains(&self.segments.len()) {
                // Fully consumed at an ancestor: a whitelisted ancestor
                // directory re-includes everything beneath it.
                return true;
            }
            let mut next: Vec<usize> = Vec::with_capacity(pos.len() + 1);
            for &p in &pos {
                if p >= self.segments.len() {
                    continue;
                }
                if self.segments[p] == "**" {
                    // `**` consumes this segment and stays put; the
                    // consume-zero case is handled by the closure below.
                    push_unique(&mut next, p);
                } else if segment_matches(&self.segments[p], seg) {
                    push_unique(&mut next, p + 1);
                }
            }
            self.close_over_doublestars(&mut next);
            if next.is_empty() {
                return false;
            }
            pos = next;
        }
        // Every segment of `rel` was consumed and the pattern is still alive:
        // either it has more to match below, or it ended exactly here.
        true
    }

    /// Epsilon-closure: a `**` segment may consume ZERO segments, so any
    /// position sitting on one also reaches the position after it.
    fn close_over_doublestars(&self, pos: &mut Vec<usize>) {
        let mut i = 0;
        while i < pos.len() {
            let p = pos[i];
            if p < self.segments.len() && self.segments[p] == "**" {
                push_unique(pos, p + 1);
            }
            i += 1;
        }
    }
}

/// Push `value` unless it is already present (tiny vectors; no set needed).
fn push_unique(v: &mut Vec<usize>, value: usize) {
    if !v.contains(&value) {
        v.push(value);
    }
}

/// Parse one gitignore line into a [`NegationPattern`], or `None` when the line
/// is not a `!`-whitelist rule (a comment, a blank line, an ordinary exclude, or
/// an escaped `\!` literal).
///
/// Mirrors [`GitignoreBuilder::add_line`] step for step - the same
/// trailing-whitespace trim (skipped for a `\ ` escape), the same `\!` / `\#`
/// literal handling, the same leading-`/` anchor, the same single trailing-`/`
/// directory marker (with its `\` unescape) - so the rules this sees are exactly
/// the rules the matcher compiled. A build-time cross-check against
/// [`Gitignore::num_whitelists`] catches any drift and degrades the scope to
/// `unknown`.
fn parse_negation_line(line: &str) -> Option<NegationPattern> {
    let mut line = line;
    if line.starts_with('#') {
        return None;
    }
    if !line.ends_with("\\ ") {
        line = line.trim_end();
    }
    if line.is_empty() {
        return None;
    }
    // `\!` / `\#` escape the leading character: an ordinary rule, not a
    // whitelist.
    if line.starts_with("\\!") || line.starts_with("\\#") {
        return None;
    }
    let mut rest = line.strip_prefix('!')?;

    let mut anchored = false;
    if let Some(stripped) = rest.strip_prefix('/') {
        rest = stripped;
        anchored = true;
    }
    // A single trailing `/` marks a directory-only rule and is otherwise not
    // part of the glob; an escaped `\/` drops its escape too.
    if let Some(stripped) = rest.strip_suffix('/') {
        rest = stripped.strip_suffix('\\').unwrap_or(stripped);
    }
    // An interior slash anchors the rule to the scope directory just as a
    // leading one does.
    anchored |= rest.contains('/');

    if !anchored {
        return Some(NegationPattern {
            unanchored: true,
            segments: Vec::new(),
        });
    }
    Some(NegationPattern {
        unanchored: false,
        segments: rest
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    })
}

/// Whether one glob SEGMENT matches one path segment.
///
/// Supports the gitignore/globset syntax that can appear inside a single
/// segment: `*` (any run, never across a `/` - which cannot occur here because
/// the caller already split on `/`), `?` (one char), `[...]` character classes
/// (with a leading `!`/`^` negation and `a-z` ranges), and `\` escapes. A
/// malformed class - an unterminated `[` - is treated as matching, which is the
/// conservative direction (it can only suppress a prune).
fn segment_matches(pat: &str, seg: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = seg.chars().collect();
    if has_unterminated_class(&p) {
        return true;
    }
    glob_match(&p, &s)
}

/// Whether `pat` opens a `[` character class it never closes.
fn has_unterminated_class(pat: &[char]) -> bool {
    let mut i = 0;
    while i < pat.len() {
        match pat[i] {
            '\\' => i += 2,
            '[' => match class_end(pat, i) {
                Some(end) => i = end,
                None => return true,
            },
            _ => i += 1,
        }
    }
    false
}

/// Index just PAST the `]` closing the class that opens at `start`, or `None`
/// when it is unterminated. A `]` in the first position (after an optional
/// `!`/`^`) is a literal, per POSIX/globset.
fn class_end(pat: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if i < pat.len() && (pat[i] == '!' || pat[i] == '^') {
        i += 1;
    }
    if i < pat.len() && pat[i] == ']' {
        i += 1;
    }
    while i < pat.len() {
        match pat[i] {
            '\\' => i += 2,
            ']' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Whether the character class opening at `start` matches `ch`. The caller has
/// already verified the class is terminated.
fn class_matches(pat: &[char], start: usize, ch: char) -> bool {
    let end = match class_end(pat, start) {
        Some(e) => e - 1, // index OF the closing `]`
        None => return true,
    };
    let mut i = start + 1;
    let mut negated = false;
    if i < end && (pat[i] == '!' || pat[i] == '^') {
        negated = true;
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < end {
        let lo = if pat[i] == '\\' && i + 1 < end {
            i += 1;
            pat[i]
        } else {
            pat[i]
        };
        // A `]` immediately after the (optional) negation is a literal.
        if lo == ']' && !first {
            break;
        }
        first = false;
        i += 1;
        // A range `a-z`, unless the `-` is the last character in the class.
        if i + 1 < end && pat[i] == '-' && pat[i + 1] != ']' {
            i += 1;
            let hi = if pat[i] == '\\' && i + 1 < end {
                i += 1;
                pat[i]
            } else {
                pat[i]
            };
            i += 1;
            if lo <= ch && ch <= hi {
                matched = true;
            }
        } else if lo == ch {
            matched = true;
        }
    }
    matched != negated
}

/// Backtracking glob matcher over one segment (see [`segment_matches`]).
fn glob_match(pat: &[char], seg: &[char]) -> bool {
    let (mut p, mut s) = (0usize, 0usize);
    // The most recent `*` and the input position it was last matched against,
    // for backtracking.
    let mut star: Option<(usize, usize)> = None;
    while s < seg.len() {
        let step = if p < pat.len() {
            match pat[p] {
                '*' => {
                    star = Some((p, s));
                    p += 1;
                    continue;
                }
                '?' => Some((p + 1, s + 1)),
                '[' => {
                    if class_matches(pat, p, seg[s]) {
                        // `class_end` cannot be `None` here: the caller
                        // pre-checked that every class is terminated.
                        class_end(pat, p).map(|end| (end, s + 1))
                    } else {
                        None
                    }
                }
                '\\' if p + 1 < pat.len() => {
                    if pat[p + 1] == seg[s] {
                        Some((p + 2, s + 1))
                    } else {
                        None
                    }
                }
                c if c == seg[s] => Some((p + 1, s + 1)),
                _ => None,
            }
        } else {
            None
        };
        match step {
            Some((np, ns)) => {
                p = np;
                s = ns;
            }
            None => match star {
                // Let the last `*` swallow one more character and retry.
                Some((sp, ss)) => {
                    star = Some((sp, ss + 1));
                    p = sp + 1;
                    s = ss + 1;
                }
                None => return false,
            },
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

/// The `/`-split segments of a relative path, for the negation reachability
/// check. Empty for an empty path.
fn rel_segments(rel: &Path) -> Vec<String> {
    rel.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// Whether a whitelist rule in `scope` could match some path at or under the
/// absolute directory `abs_dir`.
///
/// Three cases: the scope sits at or above `abs_dir` (ask its rules, relative to
/// the scope dir); the scope sits strictly INSIDE `abs_dir` (any whitelist it
/// carries is by definition under `abs_dir`); or the two are disjoint (a scope
/// never applies outside its own directory).
fn scope_negations_reach(scope: &Scope, abs_dir: &Path) -> bool {
    if !scope.negations.any() {
        return false;
    }
    if let Ok(rel) = abs_dir.strip_prefix(&scope.dir) {
        return scope.negations.could_match_under(&rel_segments(rel));
    }
    // A scope nested under the queried directory carries rules that apply only
    // beneath it - i.e. beneath `abs_dir`.
    scope.dir.starts_with(abs_dir)
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
    /// Index from a scope's directory to the [`SourceMatcher::scopes`] entries
    /// rooted there, so a query consults only the scopes on its own ancestor
    /// chain instead of scanning all of them. A repo-of-repos can contribute
    /// thousands of nested ignore files, and [`SourceMatcher::is_included`] runs
    /// once per file - an O(scopes) sweep per file is the difference between a
    /// scan that finishes and one that does not.
    by_dir: HashMap<PathBuf, Vec<u32>>,
    /// Every STRICT ancestor (down to the source root) of a scope that carries
    /// whitelist rules. Lets [`SourceMatcher::negations_could_match_under`]
    /// answer the "a negation lives somewhere below this directory" case with a
    /// single hash lookup rather than a scan over every scope.
    negation_subtree: std::collections::HashSet<PathBuf>,
    /// True when ANY scope can RE-INCLUDE a path a broader rule excluded - i.e.
    /// the source has `include_patterns` (each stored as a `!`-re-include) OR
    /// any tier (`.gitignore` / `.ignore` / `.git/info/exclude` / global /
    /// `core.excludesFile`) contributed a `!`-prefixed whitelist rule.
    ///
    /// This is now only a coarse "are there any negations at all" signal for
    /// diagnostics and callers that want the cheap answer; directory pruning
    /// itself asks the far more precise
    /// [`SourceMatcher::negations_could_match_under`] per directory.
    has_negations: bool,
}

/// One scope's RESOLVED verdict for a path - the three-way outcome of
/// [`Gitignore::matched_path_or_any_parents`] flattened so it can be cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeVerdict {
    /// No rule in this scope matched the path or any of its parents.
    None,
    /// The last matching rule excluded it.
    Ignore,
    /// The last matching rule was a `!`-re-include.
    Whitelist,
}

impl ScopeVerdict {
    fn from_match<T>(m: &ignore::Match<T>) -> Self {
        if m.is_ignore() {
            ScopeVerdict::Ignore
        } else if m.is_whitelist() {
            ScopeVerdict::Whitelist
        } else {
            ScopeVerdict::None
        }
    }

    /// This verdict if the scope matched the path itself, else the verdict it
    /// INHERITS from the parent directory. This is the per-scope parent fallback
    /// that makes the cursor equivalent to `matched_path_or_any_parents`.
    fn or(self, inherited: ScopeVerdict) -> Self {
        match self {
            ScopeVerdict::None => inherited,
            decided => decided,
        }
    }

    /// Fold into a running LAST-MATCH-WINS accumulator across scopes: a decided
    /// verdict replaces the running one, `None` leaves it alone.
    fn or_keep(self, running: ScopeVerdict) -> Self {
        match self {
            ScopeVerdict::None => running,
            decided => decided,
        }
    }
}

/// The resolved include/exclude state of ONE directory: every applicable scope's
/// verdict at that directory, in ascending precedence order.
///
/// This is the cursor a recursive walk carries down by reference so each entry
/// costs O(scopes) instead of [`SourceMatcher::is_included`]'s O(depth x scopes).
/// It works because, per scope,
///
/// ```text
/// matched_path_or_any_parents(p, is_dir)
///     == matched(p, is_dir)  orelse  matched_path_or_any_parents(parent(p), true)
/// ```
///
/// (verified against `ignore`'s `gitignore.rs`), so a child resolves with ONE
/// non-parent-walking `matched` call per scope plus the parent's cached verdict.
///
/// The load-bearing subtlety: each scope's own parent fallback must be applied
/// BEFORE the last-match-wins fold across scopes. Folding first and inheriting a
/// single combined verdict is WRONG - a high-precedence scope matching an
/// ancestor must still beat a low-precedence scope matching the child itself.
///
/// Obtain the root state from [`SourceMatcher::root_decision`], then
/// [`SourceMatcher::descend`] into each subdirectory and
/// [`SourceMatcher::is_included_at`] for the entries inside it. The struct
/// records which directory it describes, so handing a mismatched cursor to
/// either method falls back to the authoritative slow path instead of answering
/// wrongly.
#[derive(Debug, Clone)]
pub struct DirDecision {
    /// The ABSOLUTE directory this state describes.
    dir: PathBuf,
    /// `(scope index, that scope's resolved verdict at `dir`)`, ascending by
    /// index - which is ascending precedence (see [`SourceMatcher::scopes`]).
    states: Vec<(u32, ScopeVerdict)>,
}

impl DirDecision {
    /// Whether this state describes the parent directory of `abs`.
    fn describes_parent_of(&self, abs: &Path) -> bool {
        abs.parent() == Some(self.dir.as_path())
    }
}

impl SourceMatcher {
    /// Whether any rule can re-include an otherwise-excluded path (see the
    /// [`SourceMatcher::has_negations`] field docs).
    ///
    /// Pruning decisions should use [`SourceMatcher::negations_could_match_under`]
    /// instead: this is the whole-source answer, so a single `!` rule anywhere
    /// disables every prune in the tree.
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
    /// restricting the sweep to ancestor scopes also keeps that call from
    /// panicking on a path outside a scope's root.
    pub fn is_included(&self, rel: &Path, is_dir: bool) -> bool {
        let abs = self.root.join(rel);
        let mut applicable = Vec::new();
        self.applicable_scopes(&abs, &mut applicable);
        // `None` = undecided so far; `Some(true)` = last match ignored;
        // `Some(false)` = last match whitelisted (re-included).
        let mut ignored: Option<bool> = None;
        for idx in applicable {
            let scope = &self.scopes[idx as usize];
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

    /// Whether ANY `!`-whitelist rule, in ANY scope, could re-include a path at
    /// or under the directory `rel_dir` (source-root-relative).
    ///
    /// This is the precise replacement for the old all-or-nothing
    /// [`SourceMatcher::has_negations`] prune gate (P2-1). The walk may prune an
    /// excluded directory exactly when this returns `false`: nothing under it
    /// can come back, so never walking it cannot hide an included file from the
    /// scanner (and therefore cannot make the orphan split mistake a live file
    /// for a deletion - the P1-1 data-loss hazard).
    ///
    /// CONSERVATIVE BY CONSTRUCTION: every uncertainty - an unreadable ignore
    /// file, a rule shape we did not parse with confidence, an unanchored rule
    /// that gitignore lets match at any depth - answers `true`, which only skips
    /// a prune. It never answers `false` for a rule that could actually reach.
    pub fn negations_could_match_under(&self, rel_dir: &Path) -> bool {
        let abs = self.root.join(rel_dir);
        // A negation-bearing scope living strictly BELOW this directory.
        if self.negation_subtree.contains(&abs) {
            return true;
        }
        // A negation in a scope at or ABOVE this directory that can reach down
        // into it.
        let mut applicable = Vec::new();
        self.applicable_scopes(&abs, &mut applicable);
        applicable
            .into_iter()
            .any(|idx| scope_negations_reach(&self.scopes[idx as usize], &abs))
    }

    /// The decision state at the SOURCE ROOT - the seed a recursive walk carries
    /// down (see [`DirDecision`]).
    pub fn root_decision(&self) -> DirDecision {
        self.decision_for_dir(Path::new(""))
    }

    /// The decision state for an ARBITRARY directory, computed from scratch.
    ///
    /// Walk-independent and O(depth x scopes) - the general form. A walk should
    /// use [`SourceMatcher::descend`] instead, which derives a child's state from
    /// its parent's in O(scopes). This is also the fallback [`is_included_at`]
    /// and [`descend`] take when a caller hands them a `parent` that does not
    /// actually describe the queried path's parent directory.
    ///
    /// [`is_included_at`]: SourceMatcher::is_included_at
    /// [`descend`]: SourceMatcher::descend
    pub fn decision_for_dir(&self, rel_dir: &Path) -> DirDecision {
        let abs = self.root.join(rel_dir);
        let mut applicable = Vec::new();
        self.applicable_scopes(&abs, &mut applicable);
        let states = applicable
            .into_iter()
            .map(|idx| {
                let m = self.scopes[idx as usize]
                    .matcher
                    .matched_path_or_any_parents(&abs, true);
                (idx, ScopeVerdict::from_match(&m))
            })
            .collect();
        DirDecision { dir: abs, states }
    }

    /// Whether the entry `rel` - which MUST live directly inside the directory
    /// `parent` describes - is included, resolved from `parent` in O(scopes)
    /// instead of the O(depth x scopes) of [`SourceMatcher::is_included`].
    ///
    /// Returns exactly what `is_included(rel, is_dir)` returns. If `parent` does
    /// not describe `rel`'s actual parent directory the call silently falls back
    /// to the authoritative slow path, so a mis-threaded cursor costs speed and
    /// never correctness.
    pub fn is_included_at(&self, parent: &DirDecision, rel: &Path, is_dir: bool) -> bool {
        let abs = self.root.join(rel);
        if !parent.describes_parent_of(&abs) {
            return self.is_included(rel, is_dir);
        }
        // Scopes rooted AT `abs` itself also apply to it (the ancestor sweep in
        // `applicable_scopes` includes `abs`), so they must be folded in here too
        // - otherwise a directory holding its own `.gitignore` would be judged
        // without that file's rules.
        let mut last = ScopeVerdict::None;
        let mut extra = self.by_dir.get(&abs).map(|v| v.as_slice()).unwrap_or(&[]);
        for &(idx, inherited) in &parent.states {
            // Merge in any scope rooted at `abs` whose precedence falls before
            // this inherited one, keeping ascending-index (precedence) order.
            while let Some((&next, rest)) = extra.split_first() {
                if next >= idx {
                    break;
                }
                last = self.fold_new_scope(next, &abs, is_dir, last);
                extra = rest;
            }
            let own = self.scopes[idx as usize].matcher.matched(&abs, is_dir);
            let resolved = ScopeVerdict::from_match(&own).or(inherited);
            last = resolved.or_keep(last);
        }
        for &idx in extra {
            last = self.fold_new_scope(idx, &abs, is_dir, last);
        }
        last != ScopeVerdict::Ignore
    }

    /// Descend into the subdirectory `rel_dir` of the directory `parent`
    /// describes: returns whether that directory is itself included, plus the
    /// [`DirDecision`] to carry into it.
    ///
    /// Both answers share the same per-scope match work, so a walk should call
    /// this once per directory rather than pairing `is_included_at` with a
    /// separate state build. Falls back to the from-scratch path when `parent`
    /// does not describe `rel_dir`'s parent (see
    /// [`SourceMatcher::decision_for_dir`]).
    pub fn descend(&self, parent: &DirDecision, rel_dir: &Path) -> (bool, DirDecision) {
        let abs = self.root.join(rel_dir);
        if !parent.describes_parent_of(&abs) {
            return (
                self.is_included(rel_dir, true),
                self.decision_for_dir(rel_dir),
            );
        }
        let mut states: Vec<(u32, ScopeVerdict)> = Vec::with_capacity(parent.states.len() + 1);
        for &(idx, inherited) in &parent.states {
            let own = self.scopes[idx as usize].matcher.matched(&abs, true);
            states.push((idx, ScopeVerdict::from_match(&own).or(inherited)));
        }
        // Scopes rooted at this directory (a `.gitignore` living here) join the
        // cascade now. Seed each with the REAL `matched_path_or_any_parents`
        // verdict for the directory itself rather than assuming `None`, so the
        // cursor reproduces `is_included` exactly even for whatever the `ignore`
        // crate returns when a path equals its own scope root.
        if let Some(idxs) = self.by_dir.get(&abs) {
            for &idx in idxs {
                let m = self.scopes[idx as usize]
                    .matcher
                    .matched_path_or_any_parents(&abs, true);
                states.push((idx, ScopeVerdict::from_match(&m)));
            }
            // `scopes` is stored in ascending precedence, so sorting by index
            // restores precedence order after the merge.
            states.sort_unstable_by_key(|(idx, _)| *idx);
        }
        let included = states
            .iter()
            .fold(ScopeVerdict::None, |acc, (_, v)| v.or_keep(acc))
            != ScopeVerdict::Ignore;
        (included, DirDecision { dir: abs, states })
    }

    /// Fold a scope rooted exactly at the queried path into a running verdict.
    fn fold_new_scope(
        &self,
        idx: u32,
        abs: &Path,
        is_dir: bool,
        running: ScopeVerdict,
    ) -> ScopeVerdict {
        let m = self.scopes[idx as usize]
            .matcher
            .matched_path_or_any_parents(abs, is_dir);
        ScopeVerdict::from_match(&m).or_keep(running)
    }

    /// Collect - in ascending precedence order - the indices of the scopes that
    /// apply to the absolute path `abs`, i.e. those rooted at `abs` itself or at
    /// one of its ancestors down to the source root.
    fn applicable_scopes(&self, abs: &Path, out: &mut Vec<u32>) {
        out.clear();
        for ancestor in abs.ancestors() {
            if let Some(idxs) = self.by_dir.get(ancestor) {
                out.extend_from_slice(idxs);
            }
            if ancestor == self.root {
                break;
            }
        }
        // `scopes` is stored in ascending precedence, so index order IS
        // precedence order; the ancestor walk visits deepest-first.
        out.sort_unstable();
    }
}

/// Assemble the by-directory index and the negation-subtree set for a finished
/// scope list (see the [`SourceMatcher`] field docs).
fn index_scopes(
    root: &Path,
    scopes: &[Scope],
) -> (
    HashMap<PathBuf, Vec<u32>>,
    std::collections::HashSet<PathBuf>,
) {
    let mut by_dir: HashMap<PathBuf, Vec<u32>> = HashMap::new();
    let mut negation_subtree: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for (i, scope) in scopes.iter().enumerate() {
        by_dir.entry(scope.dir.clone()).or_default().push(i as u32);
        if !scope.negations.any() {
            continue;
        }
        // Every STRICT ancestor of this scope, down to (and including) the
        // source root, has a negation somewhere beneath it.
        let mut cur = scope.dir.parent();
        while let Some(dir) = cur {
            if !dir.starts_with(root) {
                break;
            }
            negation_subtree.insert(dir.to_path_buf());
            if dir == root {
                break;
            }
            cur = dir.parent();
        }
    }
    (by_dir, negation_subtree)
}

/// Builds one directory-scoped [`Scope`] from a set of gitignore lines rooted at
/// `dir` (the source-level tiers: defaults + the source's own patterns).
fn scope_from_lines<'a>(
    dir: &Path,
    lines: impl IntoIterator<Item = &'a str>,
    label: &str,
) -> anyhow::Result<Scope> {
    let mut gb = GitignoreBuilder::new(dir);
    let mut negations = Negations::default();
    for line in lines {
        gb.add_line(None, line)
            .map_err(|e| anyhow::anyhow!("adding {label} `{line}`: {e}"))?;
        if let Some(pat) = parse_negation_line(line) {
            negations.patterns.push(pat);
        }
    }
    let matcher = gb
        .build()
        .map_err(|e| anyhow::anyhow!("building {label} matcher: {e}"))?;
    reconcile_negations(&mut negations, &matcher);
    Ok(Scope {
        dir: dir.to_path_buf(),
        matcher,
        negations,
    })
}

/// Resolve `path` and confirm it still lives inside `boundary`, returning the
/// canonical path to open.
///
/// Every ignore file this module reads is reached from the source's own
/// `local_path`, which is user-chosen, so the path handed to `File::open` is
/// only ever as trustworthy as the tree it was enumerated from. Canonicalising
/// both sides and requiring the file to stay under its boundary means a
/// symlinked (or raced) `.gitignore` pointing at, say, `/etc/shadow` is refused
/// rather than read - the walk already refuses to FOLLOW symlinked directories,
/// and this closes the same hole for the ignore files themselves.
///
/// A path that cannot be canonicalised (deleted mid-scan, a permission error) is
/// an error too: the caller treats any failure the same way it treats an
/// unreadable file - no rules, and no pruning under that scope.
fn resolve_within(path: &Path, boundary: &Path) -> std::io::Result<PathBuf> {
    let resolved = path.canonicalize()?;
    let base = boundary.canonicalize()?;
    if !resolved.starts_with(&base) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} resolves to {}, outside {}",
                path.display(),
                resolved.display(),
                base.display()
            ),
        ));
    }
    Ok(resolved)
}

/// Cross-check our own whitelist parse against the `ignore` crate's count and
/// degrade to `unknown` on any shortfall.
///
/// [`parse_negation_line`] deliberately mirrors [`GitignoreBuilder::add_line`],
/// but the two are separate implementations, and pruning is only safe while they
/// agree. [`Gitignore::num_whitelists`] is the authority on how many `!`-rules
/// actually compiled: if we recorded FEWER than that, some rule exists that we
/// cannot reason about, so the whole scope answers "a negation could match
/// anywhere" and nothing under it is ever pruned. (Recording more is harmless -
/// e.g. a line the builder rejected as an invalid glob - and only costs prunes.)
fn reconcile_negations(negations: &mut Negations, matcher: &Gitignore) {
    if (negations.patterns.len() as u64) < matcher.num_whitelists() {
        negations.unknown = true;
    }
}

/// Builds one directory-scoped [`Scope`] from an ignore FILE, rooted at the
/// EXPLICIT `scope_dir` its rules apply under. For a nested `.gitignore` /
/// `.ignore` that is the file's own directory (the true per-dir cascade); for
/// the repo-level `.git/info/exclude` and the global gitignore it is the SOURCE
/// ROOT (git anchors both at the repo root, not at their on-disk location), so
/// the caller passes the source root there rather than the file's parent
/// (`.git/info`), which would scope the rules to a directory no real path lives
/// under. A missing/unreadable file or a parse error is non-fatal - the scope
/// simply contributes no rules (or is skipped), never aborting the whole matcher
/// (a scan must not fail because one `.gitignore` was malformed). Returns `None`
/// when the scope could not be built at all.
fn scope_from_file(
    source: &SourceRow,
    scope_dir: &Path,
    ignore_file: &Path,
    boundary: &Path,
    label: &str,
) -> Option<Scope> {
    use std::io::BufRead;

    let mut gb = GitignoreBuilder::new(scope_dir);
    let mut negations = Negations::default();

    // Read the file ONCE and feed each line to both the glob builder and the
    // whitelist parser, rather than calling `GitignoreBuilder::add` and then
    // re-reading the file for the `!`-rules. Mirrors `add`'s own handling: the
    // leading UTF-8 BOM is stripped from line 1, a read error stops the file
    // (partial rules stand), and a per-line parse error is logged, not fatal.
    match resolve_within(ignore_file, boundary).and_then(std::fs::File::open) {
        Ok(file) => {
            for (i, line) in std::io::BufReader::new(file).lines().enumerate() {
                let line = match line {
                    Ok(line) => line,
                    Err(err) => {
                        tracing::warn!(
                            target: TARGET,
                            source_id = %source.id,
                            path = %ignore_file.display(),
                            %err,
                            "failed to read {label}; applying the rules read so far",
                        );
                        // We cannot know what the unread tail held, so no prune
                        // under this scope is safe.
                        negations.unknown = true;
                        break;
                    }
                };
                const UTF8_BOM: &str = "\u{feff}";
                let line = if i == 0 {
                    line.trim_start_matches(UTF8_BOM)
                } else {
                    line.as_str()
                };
                if let Err(err) = gb.add_line(Some(ignore_file.to_path_buf()), line) {
                    tracing::warn!(
                        target: TARGET,
                        source_id = %source.id,
                        path = %ignore_file.display(),
                        line = i + 1,
                        %err,
                        "failed to parse a {label} line; ignoring that rule",
                    );
                }
                if let Some(pat) = parse_negation_line(line) {
                    negations.patterns.push(pat);
                }
            }
        }
        Err(err) => {
            // A missing or unreadable file is non-fatal (no rules applied), so
            // only log - but its unknown contents must not enable a prune.
            tracing::warn!(
                target: TARGET,
                source_id = %source.id,
                path = %ignore_file.display(),
                %err,
                "failed to open {label}; ignoring its rules",
            );
            negations.unknown = true;
        }
    }

    match gb.build() {
        Ok(matcher) => {
            reconcile_negations(&mut negations, &matcher);
            Some(Scope {
                dir: scope_dir.to_path_buf(),
                matcher,
                negations,
            })
        }
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

    // (lowest) DESIGN s5.2 default excludes - one root-rooted scope of bare
    // gitignore globs, so they apply at every depth below the root.
    let defaults = scope_from_lines(&root, DEFAULT_EXCLUDES.iter().copied(), "default exclude")?;

    // (highest) the source's own overrides: exclude_patterns force-out (bare
    // glob), then include_patterns opt-back-in (`!`-prefixed), one root-rooted
    // scope added LAST so it beats both gitignore and the defaults. Built up
    // front because the pruned ignore-file collection below evaluates the same
    // cascade.
    let overrides = override_scope(&root, &source.include_patterns, &source.exclude_patterns)?;

    // The gitignore tier (DESIGN s5.2: respect .gitignore, .ignore,
    // .git/info/exclude, and the global gitignore), each ABOVE the defaults and
    // BELOW the source's own overrides. The `.gitignore` cascade then the
    // `.ignore` cascade (`.ignore` overrides `.gitignore`, matching the `ignore`
    // crate) - each file a per-directory scope, root-first so a deeper file's
    // rule wins over a shallower one.
    let mut root_tiers: Vec<Scope> = Vec::new();
    let (mut gitignores, mut ignores) = (Vec::new(), Vec::new());
    if source.respect_gitignore {
        // `<root>/.git/info/exclude` - the repo-local private exclude list,
        // rooted at the source root.
        let info_exclude = root.join(".git").join("info").join("exclude");
        if info_exclude.is_file() {
            // The repo-local exclude list must live inside the source itself.
            if let Some(scope) =
                scope_from_file(source, &root, &info_exclude, &root, ".git/info/exclude")
            {
                root_tiers.push(scope);
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
                // The global gitignore lives OUTSIDE the source (git config /
                // XDG), so its boundary is its own directory rather than the
                // source root.
                let global_dir = global.parent().unwrap_or(&global).to_path_buf();
                if let Some(scope) =
                    scope_from_file(source, &root, &global, &global_dir, "global gitignore")
                {
                    root_tiers.push(scope);
                }
            }
        }

        // ONE pruned breadth-first pass discovers both cascades (see the module
        // docs): the two full-tree sweeps this replaced were the single largest
        // cost of starting a scan on a big source, and they descended every
        // excluded `node_modules` to do it.
        let collected = collect_ignore_scopes(source, &root, &defaults, &root_tiers, &overrides);
        gitignores = collected.gitignores;
        ignores = collected.ignores;
    }

    // Assemble in ascending precedence: defaults, `.gitignore` cascade,
    // `.ignore` cascade, the root-rooted tiers, then the source's own overrides.
    let mut scopes: Vec<Scope> =
        Vec::with_capacity(2 + gitignores.len() + ignores.len() + root_tiers.len());
    scopes.push(defaults);
    scopes.append(&mut gitignores);
    scopes.append(&mut ignores);
    scopes.append(&mut root_tiers);
    scopes.push(overrides);

    // P1-1: a source has negations when it carries `include_patterns` (each
    // added as a `!`-re-include) OR any scope contributed a `!`-prefixed
    // whitelist rule. `Gitignore::num_whitelists` counts exactly those `!`-rules
    // per scope, straight from the globs the matcher itself compiled.
    let num_whitelists: u64 = scopes.iter().map(|s| s.matcher.num_whitelists()).sum();
    let has_negations = !source.include_patterns.is_empty() || num_whitelists > 0;

    let (by_dir, negation_subtree) = index_scopes(&root, &scopes);

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
        by_dir,
        negation_subtree,
        has_negations,
    })
}

/// Build the source's OWN override scope: `exclude_patterns` verbatim (bare
/// glob = force-out) then `include_patterns` `!`-prefixed (re-include), ONE
/// root-rooted scope so the include rules override the exclude rules within it
/// (last-match-wins inside the one [`Gitignore`]).
///
/// Shared by [`build_source_matcher`] (which appends it LAST, making it the
/// highest-precedence tier of the full cascade) and
/// [`own_rules_exclude_subtree`] (which evaluates it ALONE) - sharing the
/// construction is what keeps the standalone evaluation's verdicts identical
/// to this tier's verdicts inside the full matcher.
fn override_scope(
    root: &Path,
    include_patterns: &[String],
    exclude_patterns: &[String],
) -> anyhow::Result<Scope> {
    let override_lines: Vec<String> = exclude_patterns
        .iter()
        .cloned()
        .chain(include_patterns.iter().map(|inc| format!("!{inc}")))
        .collect();
    scope_from_lines(
        root,
        override_lines.iter().map(String::as_str),
        "source override pattern",
    )
}

/// Whether a source's OWN patterns (`exclude_patterns` + `include_patterns`),
/// evaluated alone, force-exclude the ENTIRE subtree rooted at `rel_dir`
/// (source-root-relative): the directory itself is excluded and no own
/// `!`-re-include rule could match at or under it.
///
/// This is the predicate behind allowing NESTED source roots (DESIGN s5.2.2):
/// a source may contain another source's root exactly when its own patterns
/// guarantee it never backs up that subtree. The guarantee is sound against
/// the FULL matcher because the own-override scope is that matcher's
/// highest-precedence tier: an `Ignore` verdict from it cannot be flipped by
/// the defaults or any gitignore tier (last-match-wins, own scope last), and
/// a directory rule reaches every descendant through
/// [`Gitignore::matched_path_or_any_parents`]. The only rules that could
/// re-include a descendant past the own tier's exclude are the own
/// `!`-re-includes - ruled out by [`SourceMatcher::negations_could_match_under`]
/// on the single-scope matcher (conservative: any uncertain rule answers
/// "could match" and fails this predicate).
///
/// DELIBERATELY ignores the gitignore cascade and the default excludes: a
/// `.gitignore` on disk can change (or vanish) at any time after the check,
/// so it must never be what keeps two sources' trees disjoint. Only the
/// stored patterns - which change exclusively through the guarded
/// `update_source` path - count. Purely computational: never touches the
/// filesystem, so it answers the same while a root is offline.
pub fn own_rules_exclude_subtree(
    root: &Path,
    include_patterns: &[String],
    exclude_patterns: &[String],
    rel_dir: &Path,
) -> anyhow::Result<bool> {
    let overrides = override_scope(root, include_patterns, exclude_patterns)?;
    let scopes = vec![overrides];
    let (by_dir, negation_subtree) = index_scopes(root, &scopes);
    let num_whitelists: u64 = scopes.iter().map(|s| s.matcher.num_whitelists()).sum();
    let matcher = SourceMatcher {
        root: root.to_path_buf(),
        scopes,
        by_dir,
        negation_subtree,
        has_negations: !include_patterns.is_empty() || num_whitelists > 0,
    };
    Ok(!matcher.is_included(rel_dir, true) && !matcher.negations_could_match_under(rel_dir))
}

/// The glob a UI affordance emits to target EXACTLY ONE path in a source, and
/// nothing else (the exclusion editor's per-row "+" / "-" buttons).
///
/// `rel` is the path RELATIVE to the source root, forward-slashed (the form the
/// exclusion preview streams). The returned glob is ROOT-ANCHORED - it starts
/// with `/`, which [`GitignoreBuilder::add_line`] reads as "match only at the
/// root of this scope" - so `/docs/notes.txt` hits that one file and never a
/// same-named `sub/docs/notes.txt`. A directory gets a trailing `/`
/// (`/docs/build/`), which sets the `ignore` crate's `is_only_dir` flag: the
/// directory itself matches, and every path beneath it matches through
/// [`Gitignore::matched_path_or_any_parents`], which retries each parent as a
/// directory. That is the SAME call [`SourceMatcher::is_included`] makes, so the
/// glob flips the whole subtree exactly as the walk would see it.
///
/// The string is written for BOTH sides: stored verbatim in `exclude_patterns`
/// it force-excludes the path, and stored in `include_patterns` (where
/// [`build_source_matcher`] prepends the `!`) it re-includes it. Because the
/// source's own override scope is added LAST and the include rules go in after
/// the exclude rules, a generated pattern always beats the gitignore cascade and
/// the defaults.
///
/// Every glob metacharacter in a path component is backslash-escaped so a
/// literal `[`, `{` or `*` in a filename cannot widen the match, and a single
/// trailing space is escaped (`add_line` trims trailing whitespace unless the
/// line ends with `\ `).
///
/// Returns `None` when the path CANNOT be expressed as one glob line: an empty
/// path, one holding a newline or carriage return (the UI stores patterns one
/// per line, so it would split into two broken rules), or one ending in
/// non-space whitespace such as a tab (`add_line` trims it and only a trailing
/// SPACE can be protected). The caller withholds the affordance rather than
/// emitting a rule that would silently match the wrong thing.
pub fn anchored_pattern_for_path(rel: &str, is_dir: bool) -> Option<String> {
    if rel.is_empty() || rel.contains('\n') || rel.contains('\r') {
        return None;
    }
    // Only a trailing SPACE survives `add_line`'s trim (via the `\ ` escape);
    // any other trailing whitespace would be silently stripped, leaving a glob
    // that misses the very path it was generated for.
    if rel.ends_with(|c: char| c.is_whitespace() && c != ' ') {
        return None;
    }

    let mut out = String::with_capacity(rel.len() + 4);
    out.push('/');
    for ch in rel.chars() {
        // The metacharacters `GlobBuilder` (with `backslash_escape(true)`)
        // treats as syntax; a backslash itself must be escaped so a Windows-ish
        // name cannot start an escape sequence of its own.
        if matches!(ch, '\\' | '*' | '?' | '[' | ']' | '{' | '}') {
            out.push('\\');
        }
        out.push(ch);
    }
    if is_dir {
        out.push('/');
    } else if out.ends_with(' ') {
        // `add_line` trims trailing whitespace unless the line ends with `\ `.
        out.pop();
        out.push_str("\\ ");
    }
    Some(out)
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

/// The two per-directory cascades [`collect_ignore_scopes`] discovers, each
/// root-first (shallowest first) so [`build_source_matcher`] can add them in
/// last-match-wins order where a deeper file's rule overrides a shallower one.
struct CollectedScopes {
    /// Every nested `.gitignore`, as its own directory-rooted scope.
    gitignores: Vec<Scope>,
    /// Every nested `.ignore`, likewise (a higher tier than `.gitignore`).
    ignores: Vec<Scope>,
}

/// Discovers the `.gitignore` and `.ignore` cascades under `root` with ONE
/// pruned breadth-first pass (see the module docs' "Pruned ignore-file
/// collection").
///
/// A dependency-free `std::fs::read_dir` walk: BFS visits shallower directories
/// before deeper ones, which gives the root-first ordering directly AND means
/// that when a candidate subdirectory is considered, EVERY scope that can apply
/// to it (its ancestors' ignore files, plus the root-rooted tiers) has already
/// been built - so the prune decision here is exactly the decision the finished
/// matcher would make.
///
/// A subdirectory is not descended when it is excluded by the cascade so far AND
/// no whitelist rule in that cascade could reach under it - git's own rule, and
/// what keeps a 600k-file `node_modules` off the matcher build. Both cascades
/// come from this one pass, so a `!` rule inside a pruned directory is invisible
/// to the matcher and the walk ALIKE (the P1-1 lockstep invariant; the scanner's
/// orphan split then reads any stored row under it as an excluded-orphan, never
/// a deletion).
///
/// Symlinked directories are NOT descended (mirrors the scanner's
/// `follow_links(false)` policy, DESIGN s5.2.1, and avoids cycles). I/O errors on
/// a directory are logged and that subtree skipped - never fatal, since a failed
/// enumerate just means we apply fewer ignore rules, never that we wrongly back
/// up or drop a file (the per-entry walk re-checks each path).
fn collect_ignore_scopes(
    source: &SourceRow,
    root: &Path,
    defaults: &Scope,
    root_tiers: &[Scope],
    overrides: &Scope,
) -> CollectedScopes {
    let gitignore_name = std::ffi::OsStr::new(".gitignore");
    let ignore_name = std::ffi::OsStr::new(".ignore");

    let mut collected = CollectedScopes {
        gitignores: Vec::new(),
        ignores: Vec::new(),
    };
    // Directory -> indices into `collected.gitignores` / `.ignores`, so the
    // per-directory decision below consults only ancestor scopes.
    let mut gi_by_dir: HashMap<PathBuf, Vec<u32>> = HashMap::new();
    let mut ig_by_dir: HashMap<PathBuf, Vec<u32>> = HashMap::new();

    let mut queue = std::collections::VecDeque::new();
    queue.push_back(root.to_path_buf());
    let mut pruned: u64 = 0;

    while let Some(dir) = queue.pop_front() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(err) => {
                tracing::debug!(target: TARGET, path = %dir.display(), %err, "read_dir failed while collecting ignore files; skipping subtree");
                continue;
            }
        };

        // Collect this directory's own ignore files FIRST, so its children are
        // judged with them in the cascade (a `.gitignore` governs its own
        // directory's subtree).
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                // Do not follow symlinks (cycle / out-of-root safety).
                Ok(ft) if ft.is_dir() => subdirs.push(path),
                Ok(ft) if ft.is_file() => {
                    let name = entry.file_name();
                    let (bucket, index) = if name == gitignore_name {
                        (&mut collected.gitignores, &mut gi_by_dir)
                    } else if name == ignore_name {
                        (&mut collected.ignores, &mut ig_by_dir)
                    } else {
                        continue;
                    };
                    if let Some(scope) =
                        scope_from_file(source, &dir, &path, root, "an ignore file")
                    {
                        index
                            .entry(dir.clone())
                            .or_default()
                            .push(bucket.len() as u32);
                        bucket.push(scope);
                    }
                }
                _ => {}
            }
        }

        for sub in subdirs {
            let cascade = PartialCascade {
                defaults,
                gitignores: &collected.gitignores,
                gi_by_dir: &gi_by_dir,
                ignores: &collected.ignores,
                ig_by_dir: &ig_by_dir,
                root_tiers,
                overrides,
                root,
            };
            if cascade.should_descend(&sub) {
                queue.push_back(sub);
            } else {
                pruned += 1;
                tracing::trace!(target: TARGET, source_id = %source.id, path = %sub.display(), "pruned an excluded directory while collecting ignore files");
            }
        }
    }

    tracing::debug!(
        target: TARGET,
        source_id = %source.id,
        gitignores = collected.gitignores.len(),
        dot_ignores = collected.ignores.len(),
        pruned_dirs = pruned,
        "collected the ignore-file cascades"
    );
    collected
}

/// The cascade [`collect_ignore_scopes`] has built so far, in the exact
/// precedence order [`build_source_matcher`] will assemble: defaults, the
/// `.gitignore` cascade, the `.ignore` cascade, the root-rooted tiers, then the
/// source's own overrides.
///
/// Only scopes rooted at an ANCESTOR of the queried directory can apply to it,
/// and BFS has already discovered all of those, so this partial view answers
/// exactly as the finished [`SourceMatcher`] would.
struct PartialCascade<'a> {
    defaults: &'a Scope,
    gitignores: &'a [Scope],
    gi_by_dir: &'a HashMap<PathBuf, Vec<u32>>,
    ignores: &'a [Scope],
    ig_by_dir: &'a HashMap<PathBuf, Vec<u32>>,
    root_tiers: &'a [Scope],
    overrides: &'a Scope,
    root: &'a Path,
}

impl PartialCascade<'_> {
    /// Whether the BFS should descend into the absolute directory `dir`:
    /// always when it is included, and otherwise only when a whitelist rule
    /// could re-include something beneath it.
    fn should_descend(&self, dir: &Path) -> bool {
        let mut included: Option<bool> = None;
        let mut negation_reaches = false;
        self.for_each_applicable(dir, |scope| {
            let m = scope.matcher.matched_path_or_any_parents(dir, true);
            if m.is_ignore() {
                included = Some(false);
            } else if m.is_whitelist() {
                included = Some(true);
            }
            negation_reaches |= scope_negations_reach(scope, dir);
        });
        included != Some(false) || negation_reaches
    }

    /// Call `f` for every scope applicable to `dir`, in ascending precedence.
    fn for_each_applicable(&self, dir: &Path, mut f: impl FnMut(&Scope)) {
        f(self.defaults);
        for (bucket, index) in [
            (self.gitignores, self.gi_by_dir),
            (self.ignores, self.ig_by_dir),
        ] {
            let mut idxs: Vec<u32> = Vec::new();
            for ancestor in dir.ancestors() {
                if let Some(found) = index.get(ancestor) {
                    idxs.extend_from_slice(found);
                }
                if ancestor == self.root {
                    break;
                }
            }
            idxs.sort_unstable();
            for idx in idxs {
                f(&bucket[idx as usize]);
            }
        }
        for scope in self.root_tiers {
            f(scope);
        }
        f(self.overrides);
    }
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
/// A [`WalkBuilder::filter_entry`] closure prunes any directory that the matcher
/// EXCLUDES and that no whitelist rule could reach under - i.e. exactly when
/// `!matcher.is_included(dir, true) && !matcher.negations_could_match_under(dir)`
/// (P2-1). The closure only ever prunes directories (files are still decided
/// per-entry by the scanner's matcher check) and never prunes the root.
///
/// The per-directory negation check replaced an all-or-nothing gate on
/// [`SourceMatcher::has_negations`], which disabled pruning for the WHOLE source
/// as soon as a single `!` rule existed anywhere - so one `include_patterns`
/// entry (say `/myrepo/.env`) meant descending every `node_modules` in the tree.
/// The invariant it protected (P1-1) is preserved exactly, and more precisely: a
/// directory is pruned only when nothing under it can be re-included, so the
/// walk can never hide a file the matcher would classify INCLUDED. That lockstep
/// is what keeps the scanner's orphan split from reading a still-present file's
/// stored `file_state` row as a deletion and trashing it on Drive.
///
/// The closure needs a `'static + Send + Sync` predicate, so it holds an
/// [`Arc<SourceMatcher>`] - the SAME matcher the scanner uses for its per-entry
/// and orphan-split checks when it calls [`build_walker_with_matcher`], rather
/// than a second full build of the cascade.
pub fn build_walker(source: &SourceRow) -> anyhow::Result<WalkBuilder> {
    let matcher = Arc::new(build_source_matcher(source)?);
    Ok(build_walker_with_matcher(source, matcher))
}

/// [`build_walker`] over an ALREADY-BUILT matcher.
///
/// Building the cascade means a full (pruned) breadth-first pass over the source
/// to discover its nested ignore files, so a caller that already holds a matcher
/// (the scanner does) must not pay for a second one just to get the walker's
/// prune predicate. Sharing the same [`Arc`] also guarantees the walk filter and
/// the per-entry / orphan-split decisions are made by literally the same rules.
pub fn build_walker_with_matcher(source: &SourceRow, matcher: Arc<SourceMatcher>) -> WalkBuilder {
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
    // node_modules just to discard each file - unless a `!`-re-include could
    // actually reach under this particular directory (P1-1).
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
        // Descend an included directory, and an excluded one only while a
        // negation could still re-include something beneath it.
        matcher.is_included(rel, true) || matcher.negations_could_match_under(rel)
    });

    tracing::debug!(
        target: TARGET,
        source_id = %source.id,
        respect_gitignore = source.respect_gitignore,
        includes = source.include_patterns.len(),
        excludes = source.exclude_patterns.len(),
        "built walker"
    );
    wb
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

    /// Every path the WALKER yields, relative to the root and `/`-joined, with
    /// NO matcher filtering. Lets a test distinguish "the matcher excluded it"
    /// from "the walk never descended there" - i.e. prove a prune actually
    /// happened rather than just that the file was filtered out afterwards.
    fn raw_walked_paths(source: &SourceRow) -> Vec<String> {
        let mut out = Vec::new();
        for res in build_walker(source).expect("walker").build() {
            let entry = res.expect("entry");
            let rel = entry
                .path()
                .strip_prefix(&source.local_path)
                .expect("under root");
            if rel.as_os_str().is_empty() {
                continue;
            }
            out.push(rel.to_string_lossy().replace('\\', "/"));
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
    fn a_negation_above_an_excluded_dir_keeps_it_walked() {
        // P1-1: a `!`-rule that can reach INTO an excluded directory must keep
        // that directory walked, so the re-included file is seen and its stored
        // row is never false-deleted. Here the negation lives at the ROOT (a
        // scope at or above `vendor/`), which is exactly the case the
        // per-directory reachability check must answer "yes" to.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "vendor/\n!vendor/keep.txt\n");
        write(&root.join("vendor/keep.txt"), "x");
        write(&root.join("vendor/drop.txt"), "x");
        write(&root.join("top.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(
            matcher.has_negations(),
            "a `!`-negation in any tier must set has_negations"
        );
        assert!(
            matcher.negations_could_match_under(Path::new("vendor")),
            "the root-level !vendor/keep.txt reaches under vendor/"
        );
        assert!(
            matcher.is_included(Path::new("vendor/keep.txt"), false),
            "the !vendor/keep.txt must re-include the file in the matcher"
        );

        let names = walked_names(&src);
        assert!(names.contains(&"top.txt".to_string()), "{names:?}");
        assert!(
            names.contains(&"vendor/keep.txt".to_string()),
            "vendor/ must not be pruned, so the re-included file is still walked: {names:?}"
        );
        assert!(
            !names.contains(&"vendor/drop.txt".to_string()),
            "its excluded sibling stays out: {names:?}"
        );
    }

    #[test]
    fn a_negation_nested_inside_a_pruned_dir_is_not_collected() {
        // The git-semantics half of the lockstep invariant (see the module
        // docs): `vendor/` is excluded and NOTHING in the cascade above it can
        // re-include anything beneath it, so the pruned collection never reads
        // `vendor/.gitignore` - and the walk never descends `vendor/` either.
        //
        // The two staying in lockstep is the whole point. The matcher classifies
        // `vendor/keep.txt` EXCLUDED, exactly as the walk (which never saw it)
        // implies, so the scanner's orphan split reads a stored row for it as an
        // excluded-orphan and NEVER as a deletion - see the scanner's
        // `nested_negation_under_pruned_dir_is_orphan_not_deleted`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "vendor/\n");
        write(&root.join("vendor/.gitignore"), "!keep.txt\n");
        write(&root.join("vendor/keep.txt"), "x");
        write(&root.join("top.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(
            !matcher.negations_could_match_under(Path::new("vendor")),
            "nothing at or above vendor/ carries a `!`-rule, so it is prunable"
        );
        assert!(
            !matcher.is_included(Path::new("vendor/keep.txt"), false),
            "the un-collected nested negation cannot re-include the file"
        );

        let names = walked_names(&src);
        assert!(names.contains(&"top.txt".to_string()), "{names:?}");
        assert!(
            !names.contains(&"vendor/keep.txt".to_string()),
            "the walk agrees with the matcher: nothing under vendor/ is backed up: {names:?}"
        );
        // And the walk really did prune - it never even yielded the directory's
        // contents to be filtered.
        let raw = raw_walked_paths(&src);
        assert!(
            !raw.iter().any(|p| p.starts_with("vendor/")),
            "vendor/ must not be descended at all: {raw:?}"
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

    /// The exact glob strings [`anchored_pattern_for_path`] produces, shared with
    /// the webview's `patternForPath` (ui/src/stores/exclusionPreview.ts) - the
    /// exclusion editor's "+" / "-" buttons build the pattern in TypeScript, so
    /// the two implementations MUST agree character for character or a click
    /// would append a rule this matcher reads differently. The same table is
    /// asserted by the vitest `exclusion-preview-store` suite; change both sides
    /// together.
    const PATTERN_VECTORS: &[(&str, bool, Option<&str>)] = &[
        ("notes.txt", false, Some("/notes.txt")),
        ("docs/notes.txt", false, Some("/docs/notes.txt")),
        ("docs", true, Some("/docs/")),
        ("a/b/c", true, Some("/a/b/c/")),
        // Glob metacharacters in a real filename are escaped, never syntax.
        ("odd[1].txt", false, Some("/odd\\[1\\].txt")),
        ("alt{a,b}.txt", false, Some("/alt\\{a,b\\}.txt")),
        ("star*.txt", false, Some("/star\\*.txt")),
        ("q?.txt", false, Some("/q\\?.txt")),
        ("back\\slash.txt", false, Some("/back\\\\slash.txt")),
        // A leading `!` / `#` is inert because the glob is `/`-anchored.
        ("!bang.txt", false, Some("/!bang.txt")),
        ("#hash.txt", false, Some("/#hash.txt")),
        // Spaces are ordinary; only a TRAILING one needs the `\ ` guard against
        // `GitignoreBuilder::add_line`'s trailing-whitespace trim.
        ("My Documents/a.txt", false, Some("/My Documents/a.txt")),
        ("trailing .txt", false, Some("/trailing .txt")),
        ("trails ", false, Some("/trails\\ ")),
        // Inexpressible as a single glob line - the UI withholds the button.
        ("", false, None),
        ("two\nlines.txt", false, None),
        ("carriage\rreturn.txt", false, None),
        ("tabbed\t", false, None),
    ];

    #[test]
    fn anchored_pattern_vectors_are_stable() {
        for &(rel, is_dir, expected) in PATTERN_VECTORS {
            assert_eq!(
                anchored_pattern_for_path(rel, is_dir).as_deref(),
                expected,
                "pattern for {rel:?} (is_dir={is_dir})"
            );
        }
    }

    #[test]
    fn generated_exclude_pattern_flips_only_its_own_path() {
        // The "-" button on an INCLUDED row: the generated glob goes into
        // `exclude_patterns` verbatim and must exclude exactly that path - not a
        // same-named file in a sibling directory, not a sibling in the same
        // directory, and not a prefix-sharing neighbour.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("docs/notes.txt"), "x");
        write(&root.join("docs/notes.txt.bak"), "x");
        write(&root.join("docs/other.txt"), "x");
        write(&root.join("sub/docs/notes.txt"), "x");

        let pattern = anchored_pattern_for_path("docs/notes.txt", false).expect("expressible");
        assert_eq!(pattern, "/docs/notes.txt");
        // The generated glob must survive the same validator the IPC layer runs
        // before persisting it.
        validate_patterns(&[], std::slice::from_ref(&pattern)).expect("generated glob validates");

        let before = build_source_matcher(&source_at(root, false, &[], &[])).expect("matcher");
        assert!(before.is_included(Path::new("docs/notes.txt"), false));

        let after =
            build_source_matcher(&source_at(root, false, &[], &["/docs/notes.txt"])).expect("m");
        assert!(
            !after.is_included(Path::new("docs/notes.txt"), false),
            "the clicked path flips to excluded"
        );
        for sibling in ["docs/notes.txt.bak", "docs/other.txt", "sub/docs/notes.txt"] {
            assert!(
                after.is_included(Path::new(sibling), false),
                "{sibling} must be untouched by the anchored glob"
            );
        }
    }

    #[test]
    fn generated_dir_exclude_pattern_covers_the_subtree_only() {
        // The "-" button on an INCLUDED folder row: the trailing-slash glob
        // excludes the folder AND everything under it (via the matcher's
        // parent retry), and nothing outside it.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("build/out/app.js"), "x");
        write(&root.join("build.txt"), "x");
        write(&root.join("builder/keep.js"), "x");

        let pattern = anchored_pattern_for_path("build", true).expect("expressible");
        assert_eq!(pattern, "/build/");
        validate_patterns(&[], std::slice::from_ref(&pattern)).expect("generated glob validates");

        let after = build_source_matcher(&source_at(root, false, &[], &[&pattern])).expect("m");
        assert!(
            !after.is_included(Path::new("build"), true),
            "folder itself"
        );
        assert!(
            !after.is_included(Path::new("build/out/app.js"), false),
            "a file deep inside the folder"
        );
        assert!(
            !after.is_included(Path::new("build/out"), true),
            "a nested folder inside it"
        );
        assert!(
            after.is_included(Path::new("build.txt"), false),
            "a prefix-sharing FILE is untouched"
        );
        assert!(
            after.is_included(Path::new("builder/keep.js"), false),
            "a prefix-sharing FOLDER is untouched"
        );
    }

    #[test]
    fn generated_include_pattern_reincludes_only_its_own_path() {
        // The "+" button on an EXCLUDED row: the generated glob goes into
        // `include_patterns` (where `build_source_matcher` prepends the `!`) and
        // must re-include exactly that path, even when a broader rule - here a
        // `.gitignore` AND an exclude glob - put it out.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "*.log\n");
        write(&root.join("logs/keep.log"), "x");
        write(&root.join("logs/drop.log"), "x");
        write(&root.join("other/keep.log"), "x");

        let pattern = anchored_pattern_for_path("logs/keep.log", false).expect("expressible");
        assert_eq!(pattern, "/logs/keep.log");
        validate_patterns(std::slice::from_ref(&pattern), &[]).expect("generated glob validates");

        let before = build_source_matcher(&source_at(root, true, &[], &[])).expect("matcher");
        assert!(!before.is_included(Path::new("logs/keep.log"), false));

        let after =
            build_source_matcher(&source_at(root, true, &[&pattern], &["*.log"])).expect("m");
        assert!(
            after.is_included(Path::new("logs/keep.log"), false),
            "the clicked path flips to included, beating BOTH gitignore and the exclude glob"
        );
        for sibling in ["logs/drop.log", "other/keep.log"] {
            assert!(
                !after.is_included(Path::new(sibling), false),
                "{sibling} must stay excluded"
            );
        }
    }

    #[test]
    fn generated_dir_include_pattern_reincludes_the_subtree_only() {
        // The "+" button on an EXCLUDED folder row: `!/vendor/keep/` re-includes
        // the folder and its contents (the permissive re-include this module
        // documents), while its excluded siblings stay out.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("vendor/keep/lib.js"), "x");
        write(&root.join("vendor/drop/lib.js"), "x");

        let pattern = anchored_pattern_for_path("vendor/keep", true).expect("expressible");
        assert_eq!(pattern, "/vendor/keep/");

        let after =
            build_source_matcher(&source_at(root, false, &[&pattern], &["/vendor/"])).expect("m");
        assert!(after.is_included(Path::new("vendor/keep"), true));
        assert!(
            after.is_included(Path::new("vendor/keep/lib.js"), false),
            "a file under the re-included folder comes back too"
        );
        assert!(!after.is_included(Path::new("vendor/drop"), true));
        assert!(!after.is_included(Path::new("vendor/drop/lib.js"), false));
        assert!(
            after.has_negations(),
            "an include pattern must disable directory pruning so the walk can \
             actually reach the re-included subtree"
        );
    }

    // --- negation reachability + per-directory pruning (P2-1) ---------------

    /// The reachability answer for `dir` under a source whose only rule is the
    /// include pattern `include` (gitignore tier off, so nothing else can add
    /// whitelist rules and the answer is purely the pattern's own reach).
    fn reaches(root: &Path, include: &str, dir: &str) -> bool {
        let src = source_at(root, false, &[include], &[]);
        build_source_matcher(&src)
            .expect("matcher")
            .negations_could_match_under(Path::new(dir))
    }

    #[test]
    fn negations_could_match_under_truth_table() {
        // The core of negation-aware pruning: which directories can a given
        // `!`-rule still reach into? A `true` only costs a prune; a wrong
        // `false` would hide a re-included file from the walk, so every
        // uncertain shape must answer `true`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // UNANCHORED (no leading or interior `/`): gitignore lets it match at
        // any depth, so it reaches everywhere - this is why a bare `.env`
        // include disables pruning across the whole source.
        for probe in ["node_modules", "node_modules/sub", "a/b/c", ""] {
            assert!(
                reaches(root, ".env", probe),
                "an unanchored `.env` reaches under {probe:?}"
            );
        }
        // A trailing slash marks a directory but does NOT anchor.
        assert!(reaches(root, "build/", "node_modules/sub"));

        // ANCHORED, one wildcard segment: reaches every DEPTH-1 directory and
        // nothing deeper.
        for pattern in ["*/.env", "/*/.env"] {
            assert!(
                reaches(root, pattern, "node_modules"),
                "{pattern} can match node_modules/.env"
            );
            assert!(
                !reaches(root, pattern, "node_modules/sub"),
                "{pattern} cannot match anything under node_modules/sub"
            );
        }

        // ANCHORED, fully literal: only its own branch.
        assert!(reaches(root, "/x/.env", "x"));
        assert!(!reaches(root, "/x/.env", "node_modules"));
        assert!(!reaches(root, "/x/.env", "x/y"));

        // `**` consumes zero or more segments, so it reaches arbitrarily deep -
        // but still only inside its own branch.
        for probe in ["a", "a/b", "a/b/c"] {
            assert!(reaches(root, "/a/**/.env", probe), "under {probe:?}");
        }
        assert!(!reaches(root, "/a/**/.env", "b"));

        // A DIRECTORY rule re-includes everything beneath it, so it reaches the
        // directory itself and every descendant.
        for probe in ["a", "a/b", "a/b/c"] {
            assert!(reaches(root, "/a/b/", probe), "under {probe:?}");
        }
        assert!(!reaches(root, "/a/b/", "a/c"));

        // A fully-consumed FILE rule reaches its descendants too: the matcher
        // retries every parent as a directory, so a whitelisted `x/.env` that
        // turns out to BE a directory re-includes what is inside it.
        assert!(reaches(root, "/x/.env", "x/.env"));

        // Metacharacters inside a segment are honoured, not treated as literal.
        assert!(reaches(root, "/node_modules?/keep", "node_modules1"));
        assert!(!reaches(root, "/node_modules?/keep", "node_modules12"));
        assert!(reaches(root, "/[ab]/keep", "a"));
        assert!(!reaches(root, "/[ab]/keep", "c"));
        assert!(reaches(root, "/[!ab]/keep", "c"));
        assert!(!reaches(root, "/[!ab]/keep", "a"));
        assert!(reaches(root, "/pre*post/keep", "prefixpost"));
        assert!(!reaches(root, "/pre*post/keep", "prefix"));

        // A source with NO whitelist rule at all reaches nothing - the common
        // fast path where every excluded directory is prunable.
        let plain = source_at(root, false, &[], &["/vendor/"]);
        let plain = build_source_matcher(&plain).expect("matcher");
        assert!(!plain.negations_could_match_under(Path::new("vendor")));
        assert!(!plain.negations_could_match_under(Path::new("anything/else")));
    }

    #[test]
    fn a_nested_scopes_negation_is_reachable_from_above() {
        // A `!`-rule in a nested `.gitignore` must make every ancestor
        // directory un-prunable (the walk has to reach the scope to honour it),
        // and must not leak into a sibling branch.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("sub/deep/.gitignore"), "!keep.txt\n");
        write(&root.join("sub/deep/keep.txt"), "x");
        write(&root.join("other/file.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");
        for probe in ["", "sub", "sub/deep"] {
            assert!(
                matcher.negations_could_match_under(Path::new(probe)),
                "the nested negation must be reachable from {probe:?}"
            );
        }
        assert!(
            !matcher.negations_could_match_under(Path::new("other")),
            "but never from a sibling branch"
        );
    }

    #[test]
    fn parse_negation_line_vectors() {
        // The whitelist-line parser mirrors `GitignoreBuilder::add_line`; a
        // drift here is what the `num_whitelists` cross-check catches, but the
        // shapes themselves are pinned right here.
        /// `(line, Some((unanchored, segments)))`, or `None` when the line is
        /// not a whitelist rule at all.
        type Vector<'a> = (&'a str, Option<(bool, &'a [&'a str])>);
        let cases: &[Vector<'_>] = &[
            ("!foo", Some((true, &[]))),
            ("!foo/", Some((true, &[]))), // a trailing `/` marks a dir, not an anchor
            ("!/foo", Some((false, &["foo"]))),
            ("!a/b", Some((false, &["a", "b"]))),
            ("!/a/b/", Some((false, &["a", "b"]))),
            ("!/a/**/c", Some((false, &["a", "**", "c"]))),
            ("!foo   ", Some((true, &[]))), // trailing whitespace is trimmed
            ("\\!foo", None),               // an escaped `!` is a literal
            ("#!foo", None),                // a comment
            ("foo", None),                  // an ordinary exclude
            ("", None),
            ("   ", None),
        ];
        for &(line, expected) in cases {
            let parsed = parse_negation_line(line);
            match (parsed, expected) {
                (None, None) => {}
                (Some(p), Some((unanchored, segments))) => {
                    assert_eq!(p.unanchored, unanchored, "unanchored for {line:?}");
                    assert_eq!(p.segments, segments, "segments for {line:?}");
                }
                (got, want) => panic!(
                    "{line:?}: got {:?}, wanted {:?}",
                    got.map(|p| (p.unanchored, p.segments)),
                    want
                ),
            }
        }
    }

    #[test]
    fn resolve_within_refuses_a_path_outside_its_boundary() {
        // Every ignore file is reached from the user-chosen source root, so the
        // open is confined: a file that resolves outside its boundary (a
        // symlinked or raced `.gitignore`) is refused rather than read.
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(&inside.path().join("sub/.gitignore"), "x\n");
        write(&outside.path().join("elsewhere"), "x\n");

        let ok = resolve_within(&inside.path().join("sub/.gitignore"), inside.path())
            .expect("a file under the boundary resolves");
        assert!(ok.ends_with(".gitignore"));

        resolve_within(&outside.path().join("elsewhere"), inside.path())
            .expect_err("a file outside the boundary is refused");
        resolve_within(&inside.path().join("missing"), inside.path())
            .expect_err("a path that cannot be resolved at all is an error");
    }

    #[test]
    fn segment_matches_handles_glob_syntax() {
        // The single-segment glob matcher behind the anchored reachability
        // walk. `*` never crosses a `/` because the caller already split there.
        assert!(segment_matches("*", "anything"));
        assert!(segment_matches("*", ""));
        assert!(segment_matches("a*c", "abbbc"));
        assert!(!segment_matches("a*c", "abbb"));
        assert!(segment_matches("a?c", "abc"));
        assert!(!segment_matches("a?c", "ac"));
        assert!(segment_matches("[abc]", "b"));
        assert!(!segment_matches("[abc]", "d"));
        assert!(segment_matches("[!abc]", "d"));
        assert!(!segment_matches("[!abc]", "a"));
        assert!(segment_matches("[a-z]9", "c9"));
        assert!(!segment_matches("[a-z]9", "C9"));
        assert!(segment_matches("\\*lit", "*lit"));
        assert!(!segment_matches("\\*lit", "xlit"));
        assert!(segment_matches("node_modules", "node_modules"));
        assert!(!segment_matches("node_modules", "node_module"));
        // A malformed class cannot be reasoned about, so it matches - the
        // conservative direction (it only ever suppresses a prune).
        assert!(segment_matches("[abc", "anything at all"));
    }

    #[test]
    fn an_anchored_include_prunes_unrelated_excluded_dirs() {
        // The flagship case: a user with a 600k-file source, `node_modules`
        // gitignored, who wants ONE file re-included. Before the per-directory
        // check, that single `!`-rule disabled pruning everywhere and the walk
        // descended all of node_modules; now the anchored pattern cannot reach
        // there, so the whole subtree is pruned while the target file is still
        // backed up.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "node_modules/\n");
        write(&root.join("myrepo/.env"), "secret");
        write(&root.join("myrepo/src.txt"), "x");
        write(&root.join("node_modules/pkg/index.js"), "x");
        write(&root.join("node_modules/pkg/deep/nested/file.js"), "x");

        let src = source_at(root, true, &["/myrepo/.env"], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(
            !matcher.negations_could_match_under(Path::new("node_modules")),
            "an anchored /myrepo/.env cannot reach into node_modules"
        );

        let names = walked_names(&src);
        assert!(
            names.contains(&"myrepo/.env".to_string()),
            "the re-included file is still backed up: {names:?}"
        );
        assert!(names.contains(&"myrepo/src.txt".to_string()), "{names:?}");

        // `filter_entry` drops a pruned directory outright, so neither it nor
        // anything beneath it is ever yielded - that whole subtree costs the
        // walk one `read_dir` entry instead of a full descent.
        let raw = raw_walked_paths(&src);
        assert!(
            !raw.iter().any(|p| p.starts_with("node_modules")),
            "the excluded subtree is never visited: {raw:?}"
        );
    }

    #[test]
    fn a_depth_one_include_walks_only_the_depth_it_needs() {
        // `/*/.env` re-includes a `.env` sitting DIRECTLY in any top-level
        // directory. The walk must therefore descend one level into an excluded
        // `node_modules` - and stop there, because the pattern cannot match any
        // deeper.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "node_modules/\n");
        write(&root.join("node_modules/.env"), "secret");
        write(&root.join("node_modules/a/.env"), "deeper");
        write(&root.join("node_modules/a/deep/file.js"), "x");

        let src = source_at(root, true, &["/*/.env"], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(
            matcher.is_included(Path::new("node_modules/.env"), false),
            "the depth-1 .env is re-included"
        );
        assert!(
            !matcher.is_included(Path::new("node_modules/a/.env"), false),
            "a deeper .env is NOT - `/*/.env` is exactly one segment deep"
        );

        let names = walked_names(&src);
        assert_eq!(
            names,
            // The `.gitignore` itself is an ordinary backed-up file.
            vec![".gitignore".to_string(), "node_modules/.env".to_string()],
            "only the depth-1 .env is backed up out of node_modules: {names:?}"
        );

        let raw = raw_walked_paths(&src);
        assert!(
            raw.contains(&"node_modules/.env".to_string()),
            "node_modules itself is descended one level: {raw:?}"
        );
        assert!(
            !raw.iter().any(|p| p.starts_with("node_modules/a/")),
            "but its subdirectories are pruned: {raw:?}"
        );
        // The matcher still classifies the un-walked files EXCLUDED, which is
        // what stops the scanner's orphan split from reading a stored row for
        // one of them as a deletion.
        assert!(!matcher.is_included(Path::new("node_modules/a/deep/file.js"), false));
    }

    #[test]
    fn generated_pattern_escapes_metacharacters_against_the_real_matcher() {
        // A filename holding glob syntax must be matched LITERALLY: the escaped
        // glob hits the odd name and leaves the name the unescaped glob would
        // have swallowed alone. (`*` / `?` are illegal in Windows filenames, so
        // the on-disk fixtures use the class / alternation characters that are
        // legal everywhere.)
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("odd[1].txt"), "x");
        write(&root.join("odd1.txt"), "x");
        write(&root.join("alt{a,b}.txt"), "x");
        write(&root.join("alta.txt"), "x");

        let bracket = anchored_pattern_for_path("odd[1].txt", false).expect("expressible");
        let brace = anchored_pattern_for_path("alt{a,b}.txt", false).expect("expressible");
        validate_patterns(&[], &[bracket.clone(), brace.clone()])
            .expect("generated globs validate");

        let after = build_source_matcher(&source_at(root, false, &[], &[&bracket, &brace]))
            .expect("matcher");
        assert!(!after.is_included(Path::new("odd[1].txt"), false));
        assert!(
            after.is_included(Path::new("odd1.txt"), false),
            "an UNescaped `odd[1].txt` would read `[1]` as a character class and \
             also exclude odd1.txt"
        );
        assert!(!after.is_included(Path::new("alt{a,b}.txt"), false));
        assert!(
            after.is_included(Path::new("alta.txt"), false),
            "an UNescaped `alt{{a,b}}.txt` would read `{{a,b}}` as an alternation \
             and also exclude alta.txt"
        );
    }

    // --- DirDecision cursor (per-directory decision cache) ------------------

    /// BFS the real tree under `root` with the cursor, asserting at EVERY entry
    /// (files and directories alike) that the cursor's verdict is identical to
    /// [`SourceMatcher::is_included`] - the authority. Returns how many entries
    /// were compared so a test can prove it did not pass vacuously.
    ///
    /// This mirrors exactly how `stream_classify_tree` drives the cursor: state
    /// rides on the queue entry, so a child is always resolved from its true
    /// parent.
    fn assert_cursor_matches_walk(root: &Path, matcher: &SourceMatcher) -> usize {
        let mut compared = 0usize;
        let mut queue: std::collections::VecDeque<(PathBuf, DirDecision)> =
            std::collections::VecDeque::new();
        queue.push_back((root.to_path_buf(), matcher.root_decision()));

        while let Some((dir, state)) = queue.pop_front() {
            let entries = match fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                let rel = path.strip_prefix(root).expect("under root");
                let is_dir = ft.is_dir();

                let expected = matcher.is_included(rel, is_dir);
                if is_dir {
                    let (got, child) = matcher.descend(&state, rel);
                    assert_eq!(
                        got,
                        expected,
                        "cursor disagreed on DIR {}: cursor={got} is_included={expected}",
                        rel.display()
                    );
                    // `is_included_at` must agree with `descend` on the same dir.
                    assert_eq!(
                        matcher.is_included_at(&state, rel, true),
                        expected,
                        "is_included_at disagreed on DIR {}",
                        rel.display()
                    );
                    queue.push_back((path, child));
                } else {
                    let got = matcher.is_included_at(&state, rel, false);
                    assert_eq!(
                        got,
                        expected,
                        "cursor disagreed on FILE {}: cursor={got} is_included={expected}",
                        rel.display()
                    );
                }
                compared += 1;
            }
        }
        compared
    }

    #[test]
    fn cursor_matches_is_included_across_the_full_semantics_matrix() {
        // One tree exercising every gitignore shape the matcher supports, walked
        // with the cursor and compared against `is_included` at every entry:
        // anchored vs unanchored, dir-only rules, `**`, character classes, the
        // nested cascade with a deeper file overriding a shallower one, a nested
        // negation under an excluded parent (the P1-1 permissive re-include), a
        // directory that carries its OWN .gitignore, and `.ignore` +
        // .git/info/exclude tiers.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write(
            &root.join(".gitignore"),
            "*.log\nbuild/\n/anchored.txt\n**/deep/*.tmpx\nclass[0-9].dat\n",
        );
        write(&root.join(".ignore"), "hidden.dat\n");
        write(&root.join(".git/info/exclude"), "private.bin\n");

        write(&root.join("anchored.txt"), "x");
        write(&root.join("sub/anchored.txt"), "x");
        write(&root.join("top.log"), "x");
        write(&root.join("keep.txt"), "x");
        write(&root.join("hidden.dat"), "x");
        write(&root.join("private.bin"), "x");
        write(&root.join("class3.dat"), "x");
        write(&root.join("classX.dat"), "x");
        write(&root.join("build/out/app.js"), "x");
        write(&root.join("a/deep/x.tmpx"), "x");
        write(&root.join("a/deep/keep.txt"), "x");

        // A nested .gitignore that re-includes under an excluded parent, plus a
        // deeper file overriding a shallower rule.
        write(&root.join("vendor/.gitignore"), "!keep.log\n*.dat\n");
        write(&root.join("vendor/keep.log"), "x");
        write(&root.join("vendor/drop.log"), "x");
        write(&root.join("vendor/thing.dat"), "x");
        write(&root.join("vendor/nested/keep.log"), "x");

        // A directory carrying its own .gitignore, one level down.
        write(&root.join("pkg/sub/.gitignore"), "/local.txt\n");
        write(&root.join("pkg/sub/local.txt"), "x");
        write(&root.join("pkg/sub/deeper/local.txt"), "x");
        write(&root.join("pkg/keep.md"), "x");

        let src = source_at(root, true, &[".env"], &["*.bak", "/forced/"]);
        write(&root.join(".env"), "x");
        write(&root.join("stale.bak"), "x");
        write(&root.join("forced/inside.txt"), "x");

        let matcher = build_source_matcher(&src).expect("matcher");
        let compared = assert_cursor_matches_walk(root, &matcher);
        assert!(
            compared >= 25,
            "the fixture must actually exercise the cursor, compared only {compared}"
        );
    }

    #[test]
    fn cursor_applies_each_scopes_parent_fallback_before_combining() {
        // THE precedence trap, and the reason a naive per-directory cache is
        // wrong. Two scopes disagree at different depths:
        //   - the .gitignore cascade (LOWER precedence) whitelists the FILE
        //     itself (`!keep.txt`);
        //   - the source's own exclude_patterns (HIGHEST precedence) exclude the
        //     PARENT DIRECTORY (`/vendor/`).
        // `is_included` consults each scope with matched_path_or_any_parents, so
        // the high-precedence scope's match on the ancestor wins and the file is
        // EXCLUDED.
        //
        // A cache that folded scopes into ONE verdict per directory and then let
        // a child's own match override it would answer INCLUDED here: it would
        // see only "parent excluded" plus "child whitelisted by some scope". The
        // cursor keeps every scope's verdict separate and applies each scope's
        // own parent fallback BEFORE the cross-scope fold, so it agrees with
        // `is_included`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "!keep.txt\n");
        write(&root.join("vendor/keep.txt"), "x");
        write(&root.join("vendor/other.txt"), "x");
        write(&root.join("keep.txt"), "x");

        let src = source_at(root, true, &[], &["/vendor/"]);
        let matcher = build_source_matcher(&src).expect("matcher");

        // The authority says excluded - the high-precedence dir rule wins.
        assert!(
            !matcher.is_included(Path::new("vendor/keep.txt"), false),
            "a higher-precedence rule matching the PARENT must beat a \
             lower-precedence whitelist matching the child"
        );
        // ...and the cursor must reproduce that exactly.
        let (vendor_inc, vendor_state) =
            matcher.descend(&matcher.root_decision(), Path::new("vendor"));
        assert!(!vendor_inc, "vendor/ itself is excluded");
        assert!(
            !matcher.is_included_at(&vendor_state, Path::new("vendor/keep.txt"), false),
            "the cursor must NOT let the lower-precedence !keep.txt re-include it"
        );
        // The root-level keep.txt is genuinely whitelisted (nothing excludes it).
        assert!(matcher.is_included(Path::new("keep.txt"), false));
        assert!(matcher.is_included_at(&matcher.root_decision(), Path::new("keep.txt"), false));

        assert_cursor_matches_walk(root, &matcher);
    }

    #[test]
    fn cursor_reproduces_a_reinclude_that_reaches_into_an_excluded_dir() {
        // The re-include that survives the pruned collection: the negation lives
        // in a scope ABOVE the excluded directory (here the source's own
        // include_patterns, which are always loaded), so it IS in the cascade,
        // pruning is disabled, and the file comes back even though its parent
        // directory is excluded - the P1-1 permissive re-include.
        //
        // Contrast `a_negation_nested_inside_a_pruned_dir_is_not_collected`: a
        // negation living INSIDE a prunable excluded dir is never collected, so
        // it cannot re-include. The cursor must reproduce BOTH outcomes, since
        // it is only ever a faster spelling of `is_included`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("vendor/keep.txt"), "x");
        write(&root.join("vendor/drop.txt"), "x");
        write(&root.join("top.txt"), "x");

        let src = source_at(root, true, &["/vendor/keep.txt"], &["/vendor/"]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(
            matcher.is_included(Path::new("vendor/keep.txt"), false),
            "an include_pattern re-includes through an excluded parent dir"
        );

        let (vendor_inc, vendor_state) =
            matcher.descend(&matcher.root_decision(), Path::new("vendor"));
        assert!(!vendor_inc, "the directory itself stays excluded");
        assert!(
            matcher.is_included_at(&vendor_state, Path::new("vendor/keep.txt"), false),
            "the re-include must survive through the cursor too"
        );
        assert!(!matcher.is_included_at(&vendor_state, Path::new("vendor/drop.txt"), false));

        assert_cursor_matches_walk(root, &matcher);
    }

    #[test]
    fn cursor_agrees_when_a_nested_negation_is_never_collected() {
        // The other half: `vendor/.gitignore: !keep.txt` inside a prunable
        // excluded `vendor/` is NOT collected (git's own rule, and what keeps the
        // walk and the matcher in lockstep), so the file stays EXCLUDED. The
        // cursor must agree - it must not resurrect a rule the cascade never
        // loaded.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "vendor/\n");
        write(&root.join("vendor/.gitignore"), "!keep.txt\n");
        write(&root.join("vendor/keep.txt"), "x");
        write(&root.join("top.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");
        assert!(!matcher.is_included(Path::new("vendor/keep.txt"), false));

        let (vendor_inc, vendor_state) =
            matcher.descend(&matcher.root_decision(), Path::new("vendor"));
        assert!(!vendor_inc);
        assert!(
            !matcher.is_included_at(&vendor_state, Path::new("vendor/keep.txt"), false),
            "the cursor must not re-include via an uncollected nested negation"
        );

        assert_cursor_matches_walk(root, &matcher);
    }

    #[test]
    fn cursor_folds_in_a_scope_rooted_at_the_queried_directory() {
        // A directory that holds its OWN .gitignore: those rules are in scope for
        // the directory itself (the ancestor sweep includes the path), so both
        // `descend` and `is_included_at` must merge that scope in at the right
        // precedence rather than only from the next level down.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("pkg/.gitignore"), "*.gen\n!important.gen\n");
        write(&root.join("pkg/a.gen"), "x");
        write(&root.join("pkg/important.gen"), "x");
        write(&root.join("pkg/plain.txt"), "x");
        write(&root.join("pkg/nested/b.gen"), "x");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");

        let (_, pkg_state) = matcher.descend(&matcher.root_decision(), Path::new("pkg"));
        assert!(!matcher.is_included_at(&pkg_state, Path::new("pkg/a.gen"), false));
        assert!(matcher.is_included_at(&pkg_state, Path::new("pkg/important.gen"), false));
        assert!(matcher.is_included_at(&pkg_state, Path::new("pkg/plain.txt"), false));

        let (_, nested_state) = matcher.descend(&pkg_state, Path::new("pkg/nested"));
        assert!(
            !matcher.is_included_at(&nested_state, Path::new("pkg/nested/b.gen"), false),
            "the pkg-level rule reaches one level deeper too"
        );

        assert_cursor_matches_walk(root, &matcher);
    }

    #[test]
    fn a_mismatched_cursor_falls_back_instead_of_answering_wrongly() {
        // The API's safety net: handing a cursor for the WRONG directory must
        // never produce a wrong verdict. Both entry points detect it and fall
        // back to the authoritative from-scratch path.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "*.log\n");
        write(&root.join("a/keep.txt"), "x");
        write(&root.join("a/drop.log"), "x");
        write(&root.join("b/keep.txt"), "x");
        write(&root.join("b/deep/keep.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");

        // A cursor for `a/`, used to judge paths under `b/` and the root.
        let (_, a_state) = matcher.descend(&matcher.root_decision(), Path::new("a"));
        for (rel, is_dir) in [
            ("b/keep.txt", false),
            ("b/deep/keep.txt", false),
            ("b/deep", true),
            ("a/keep.txt", false),
        ] {
            let p = Path::new(rel);
            assert_eq!(
                matcher.is_included_at(&a_state, p, is_dir),
                matcher.is_included(p, is_dir),
                "mismatched cursor must fall back for {rel}"
            );
        }

        // `descend` with a mismatched parent must still produce a USABLE state.
        let (inc, deep_state) = matcher.descend(&a_state, Path::new("b/deep"));
        assert_eq!(inc, matcher.is_included(Path::new("b/deep"), true));
        assert_eq!(
            matcher.is_included_at(&deep_state, Path::new("b/deep/keep.txt"), false),
            matcher.is_included(Path::new("b/deep/keep.txt"), false),
            "the recovered state must be correct for its own children"
        );
    }

    #[test]
    fn decision_for_dir_matches_a_descend_chain() {
        // The from-scratch builder and the incremental one must agree, since the
        // fallback path silently swaps one for the other.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "*.log\nbuild/\n");
        write(&root.join("a/.gitignore"), "!special.log\n");
        write(&root.join("a/b/.gitignore"), "*.dat\n");
        write(&root.join("a/b/c/x.dat"), "x");
        write(&root.join("a/b/c/special.log"), "x");
        write(&root.join("a/b/c/plain.txt"), "x");

        let src = source_at(root, true, &[], &[]);
        let matcher = build_source_matcher(&src).expect("matcher");

        // Walk down incrementally...
        let mut state = matcher.root_decision();
        for rel in ["a", "a/b", "a/b/c"] {
            state = matcher.descend(&state, Path::new(rel)).1;
        }
        // ...versus building the same directory's state from scratch.
        let scratch = matcher.decision_for_dir(Path::new("a/b/c"));

        for name in ["a/b/c/x.dat", "a/b/c/special.log", "a/b/c/plain.txt"] {
            let p = Path::new(name);
            let expected = matcher.is_included(p, false);
            assert_eq!(
                matcher.is_included_at(&state, p, false),
                expected,
                "descend chain wrong for {name}"
            );
            assert_eq!(
                matcher.is_included_at(&scratch, p, false),
                expected,
                "decision_for_dir wrong for {name}"
            );
        }
    }

    #[test]
    fn cursor_matches_when_gitignore_is_disabled_and_only_overrides_apply() {
        // respect_gitignore=false leaves only the defaults + override scopes, so
        // the cursor runs with a minimal cascade - the common case for a plain
        // documents folder.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "ignored.txt\n");
        write(&root.join("ignored.txt"), "x");
        write(&root.join("notes/a.txt"), "x");
        write(&root.join("notes/b.log"), "x");
        write(&root.join("Thumbs.db"), "x");

        let src = source_at(root, false, &[], &["*.log"]);
        let matcher = build_source_matcher(&src).expect("matcher");
        let compared = assert_cursor_matches_walk(root, &matcher);
        assert!(compared >= 5, "compared only {compared}");
    }

    /// [`own_rules_exclude_subtree`] with `Vec<String>` args, against a root
    /// that never needs to exist (the predicate is purely computational).
    fn own_covers(includes: &[&str], excludes: &[&str], rel: &str) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let includes: Vec<String> = includes.iter().map(|s| s.to_string()).collect();
        let excludes: Vec<String> = excludes.iter().map(|s| s.to_string()).collect();
        own_rules_exclude_subtree(dir.path(), &includes, &excludes, Path::new(rel))
            .expect("valid patterns")
    }

    #[test]
    fn own_rules_exclude_subtree_requires_a_covering_exclude() {
        // The anchored dir glob the exclusion editor emits covers the root and
        // everything beneath it.
        assert!(own_covers(&[], &["/Documents/"], "Documents"));
        assert!(own_covers(&[], &["/Documents/"], "Documents/projects"));
        // An unanchored form covers too (matched at the top level).
        assert!(own_covers(&[], &["Documents"], "Documents"));
        // No patterns, or an exclusion of an unrelated folder, is not coverage.
        assert!(!own_covers(&[], &[], "Documents"));
        assert!(!own_covers(&[], &["/Downloads/"], "Documents"));
    }

    #[test]
    fn own_rules_exclude_subtree_denies_reachable_reinclude() {
        // An include pattern (stored bare; the matcher prepends the `!`) that
        // could re-include a path UNDER the excluded root breaks the
        // whole-subtree guarantee.
        assert!(!own_covers(
            &["Documents/keep.txt"],
            &["/Documents/"],
            "Documents"
        ));
        // An unanchored include can match at any depth, so it reaches under
        // the root as well (the conservative answer).
        assert!(!own_covers(&["keep.txt"], &["/Documents/"], "Documents"));
        // The subtree check is what rejects this - the root itself still
        // counts as excluded (the `!` rule targets a file beneath it).
        // Deliberately NOT asserted the other way: gitignore precision for
        // negations anchored elsewhere is conservative, and this predicate is
        // allowed to say "not covered" whenever it is unsure.
    }
}
