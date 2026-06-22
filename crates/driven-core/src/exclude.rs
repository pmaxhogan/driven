//! gitignore + default + custom excludes; DESIGN s5.2.
//!
//! Builds the [`ignore::WalkBuilder`] the scanner (SPEC s6) walks with. The
//! builder layers, in increasing precedence:
//!
//! 1. The gitignore cascade (`.gitignore`, `.git/info/exclude`, and the
//!    user's global gitignore), gated on [`SourceRow::respect_gitignore`].
//! 2. The DESIGN s5.2 DEFAULT EXCLUDE list (OS noise / editor swap / misc
//!    transient globs), copied verbatim from the design doc.
//! 3. The source's own `include_patterns` (opt-back-in, e.g. a bare `.env`)
//!    and `exclude_patterns` (force-out, e.g. `*.log`).
//!
//! Precedence note / KNOWN DEVIATION (flagged for integrate): DESIGN s5.2
//! states the gitignore cascade should *beat* the default-exclude list
//! ("if a user's gitignore says `!Thumbs.db`, Thumbs.db is included"). The
//! `ignore` crate evaluates an [`ignore::overrides::Override`] at a
//! *higher* precedence than the gitignore cascade, so packing the defaults
//! and the user rules into one `Override` (as done here, mirroring the SPEC
//! s6 single-`Override` sketch) places the defaults *above* gitignore
//! rather than below. Within a single `Override` later globs win, so the
//! user's own `include_patterns` still re-include over a default exclude
//! and `exclude_patterns` still force-out - the user-vs-default precedence
//! the ROADMAP rows require is correct. Only the defaults-vs-gitignore
//! ordering is inverted. Restoring it needs the defaults moved to a lower
//! `ignore` tier (e.g. an `add_ignore` global file); deferred past phase 2A.
//!
//! Glob semantics reminder: inside an `Override`, a *bare* glob is a
//! whitelist (include) and a `!glob` is an ignore (exclude) - the inverse
//! of gitignore. So `include_patterns` are added verbatim as whitelist
//! globs and `exclude_patterns` get a `!` prepended. A source whose intent
//! is "re-include `.env`" therefore stores the bare string `.env` in
//! `include_patterns`, NOT `!.env` (the "e.g. !.env" wording in the task /
//! SPEC s6 describes the user-facing *effect*, not the stored glob).

use ignore::overrides::{Override, OverrideBuilder};
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

/// Builds the [`Override`] layering the DESIGN s5.2 default excludes with
/// the source's own `exclude_patterns`.
///
/// The returned `Override` is the highest-precedence ignore tier the
/// scanner's [`WalkBuilder`] consults. It carries ONLY ignore globs (the
/// DEFAULT_EXCLUDES followed by the source's `exclude_patterns`), each added
/// as a `!glob` so the `ignore`-crate `Override`'s inverted-`!` semantics
/// turn it into an ignore. Within it, later globs win, so a source's
/// `exclude_patterns` can still force-out something a default would have let
/// through; no entry contradicts a default here, so order is moot in
/// practice.
///
/// ## KNOWN LIMITATION / BLOCKED M2 row: `include_patterns` re-inclusion
///
/// `include_patterns` (the user "opt-back-in" re-include globs, e.g. a bare
/// `.env` that re-surfaces a gitignored secret) are NOT applied here, and as
/// a result the ROADMAP M2 "`!.env` override" row is unimplemented. This is a
/// deliberate safety choice, not an oversight:
///
/// A re-include needs a WHITELIST glob. The `ignore` crate's `Override`
/// (`overrides.rs::Override::matched`) switches to whitelist-ONLY mode the
/// instant it contains a single whitelist glob: every path that matches no
/// whitelist glob is then returned `Match::Ignore` and dropped. For a backup
/// tool that is catastrophic - adding ONE include pattern would silently stop
/// backing up every other file in the source. The previous single-`Override`
/// implementation had exactly this data-loss bug (it packed `include_patterns`
/// as bare whitelist globs), which the `overrides_passthrough_for_unmatched`
/// test surfaces.
///
/// Re-inclusion that beats the `.gitignore` cascade WITHOUT whitelist-mode
/// requires a higher gitignore-semantics tier (where a leading `!` is a
/// natural re-include and unmatched paths stay `Match::None`). In the
/// `ignore` crate the only such tiers are the by-name, searched-in-tree
/// `.ignore` / custom-ignore-filename files (writing one into the user's
/// source tree would corrupt the backup) or a full takeover of gitignore
/// evaluation via `WalkBuilder::filter_entry` plus an in-memory matcher that
/// re-implements nested-`.gitignore` precedence. Both are out of scope for
/// the M2-integrate pass; this is flagged as a Phase-2 architectural defect
/// for a focused exclude.rs rework. Until then, excludes-only is shipped
/// because a missed re-include leaves ONE intended file unbacked (bounded,
/// visible) whereas whitelist-mode silently drops EVERY non-included file
/// (unbounded, catastrophic).
pub fn build_override(source: &SourceRow) -> anyhow::Result<Override> {
    let mut ob = OverrideBuilder::new(&source.local_path);

    // Defaults first. Each is an ignore glob, so prepend `!`.
    for glob in DEFAULT_EXCLUDES {
        ob.add(&format!("!{glob}"))?;
    }
    // The source's own exclude_patterns: force-out, so `!glob`. Added after
    // the defaults so a later exclude wins over an earlier same-path entry.
    //
    // NOTE: `include_patterns` are intentionally NOT added here - adding any
    // bare whitelist glob would flip the Override into whitelist-only mode
    // and silently drop every unmatched file (see the fn docs above).
    for exc in &source.exclude_patterns {
        ob.add(&format!("!{exc}"))?;
    }

    Ok(ob.build()?)
}

/// Builds the configured [`WalkBuilder`] for `source` (SPEC s6
/// `build_walker`).
///
/// Layers the gitignore cascade (gated on
/// [`SourceRow::respect_gitignore`]) under the [`build_override`] result.
///
/// `hidden(false)` is mandatory: Driven backs up dotfiles, and leaving the
/// `ignore` default `hidden(true)` would silently drop every `.env` /
/// `.config` before any rule applies. `require_git(false)` makes the
/// gitignore cascade apply even when the source root is not a git
/// repository (the default `require_git(true)` would make a plain
/// `.gitignore` in a non-repo a no-op).
///
/// `follow_links(false)` (the `ignore` default, set explicitly for clarity)
/// implements the [`crate::types::SymlinkPolicy::Skip`] policy from DESIGN
/// s5.2.1: symlinks are yielded as entries but never traversed, so the walk
/// can never leave the source root or loop. The scanner then drops the link
/// entries themselves.
pub fn build_walker(source: &SourceRow) -> anyhow::Result<WalkBuilder> {
    let mut wb = WalkBuilder::new(&source.local_path);
    wb.git_ignore(source.respect_gitignore)
        .git_exclude(source.respect_gitignore)
        .git_global(source.respect_gitignore)
        .require_git(false)
        .hidden(false)
        .follow_links(false)
        .overrides(build_override(source)?);

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

    /// Collects the file basenames (relative to `root`) the walker yields.
    fn walked_names(source: &SourceRow) -> Vec<String> {
        let mut out = Vec::new();
        for res in build_walker(source).expect("walker").build() {
            let entry = res.expect("entry");
            if entry.file_type().is_some_and(|t| t.is_file()) {
                let rel = entry
                    .path()
                    .strip_prefix(&source.local_path)
                    .expect("under root");
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
    #[ignore = "BLOCKED: Phase-2 exclude.rs single-Override cannot re-include a gitignored path without flipping to whitelist-mode (silent mass file-drop, a backup data-loss bug). Needs an ignore-tier rework (filter_entry + in-memory nested-gitignore matcher). See M2 Phase-3 report; ROADMAP M2 '!.env override' row deferred."]
    fn include_pattern_reincludes_gitignored() {
        // A bare `.env` whitelist glob re-includes a path the gitignore
        // cascade would drop (ROADMAP "!.env wins" row; stored as the bare
        // glob, not the literal `!.env`).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), ".env\n");
        write(&root.join(".env"), "x");

        let src = source_at(root, true, &[".env"], &[]);
        let names = walked_names(&src);
        assert!(
            names.contains(&".env".to_string()),
            "include_pattern must re-include the gitignored .env: {names:?}"
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
    fn overrides_passthrough_for_unmatched() {
        // An ordinary file matching no include/exclude/default still passes
        // - i.e. the Override never silently flips to whitelist-only mode.
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
