//! Reading and parsing `rclone.conf`.
//!
//! ## The format is goconfig, not "INI"
//!
//! rclone parses its config with `github.com/unknwon/goconfig` (pinned at
//! `v1.0.0` in rclone's `go.mod`, unforked), via
//! `fs/config/configfile/configfile.go`. "INI-ish" is close enough to write a
//! file by hand and wrong in four places that matter, all reproduced here:
//!
//! 1. **The separator is `=` OR `:`**, whichever comes FIRST on the line
//!    (`strings.IndexAny(line, "=:")`). `type: s3` is as valid as `type = s3`.
//! 2. **Keys and section names are CASE-SENSITIVE.** goconfig stores them in a
//!    plain map with no folding, so a hand-written `Region = eu` is a key rclone
//!    never reads. This parser matches that rather than being helpfully
//!    lenient: reporting the settings rclone ACTUALLY used is the whole job.
//! 3. **Comments are own-line only** (`#` or `;` as the first character after
//!    the line is trimmed). `key = value # note` stores `value # note`. This is
//!    not pedantry - rclone writes credentials verbatim, and a trailing-comment
//!    rule would silently truncate any value containing `#`.
//! 4. **`%(name)s` interpolation.** `GetValue` substitutes references to other
//!    keys, up to 200 rounds. A parser that skipped it would import a literal
//!    `%(host)s` where rclone used the expanded value. See [`MAX_INTERPOLATION`].
//!
//! Everything else: a section header is a line whose first character is `[` and
//! last is `]` (only those two positions are examined, so `[a]b]` names the
//! remote `a]b`); both sides of the separator are trimmed while interior spaces
//! survive; blank lines are skipped; a duplicate section merges; a duplicate key
//! takes the last value; a leading UTF-8 BOM is consumed.
//!
//! A consequence worth knowing, because it looks like a bug: `[remote] ; hi`
//! is a **parse error**, not a commented header. The trailing text means the
//! line does not end in `]`, so goconfig falls through to the key/value branch,
//! finds no separator, and fails. This parser does the same.
//!
//! ## What is deliberately NOT supported
//!
//! - **Value quoting.** goconfig unquotes a value only when it starts with a
//!   backtick or `"""`; a plain `"quoted"` value keeps its quotes. Worse, its
//!   WRITER wraps a backtick-containing value in plain `"` quotes that its own
//!   reader will not unquote - so goconfig can emit values it cannot read back.
//!   Rather than reimplement a round-trip that is already broken upstream,
//!   values are taken literally. The affected values are ones rclone itself
//!   would misread.
//! - **The `DEFAULT` section.** goconfig files keys seen before any header into
//!   a section named `DEFAULT`. `rclone config` never writes one and no remote
//!   can live there, so such keys are dropped.
//!
//! ## Secrets
//!
//! Every error in this module carries a line NUMBER and, where useful, an option
//! NAME - never a value. `rclone.conf` is a file full of credentials, so an
//! error that echoed the offending line would put one into the user's terminal,
//! their shell history, and any bug report they paste it into. [`RcloneRemote`]
//! has a hand-written `Debug` for the same reason.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The marker line of a whole-file-encrypted config, matched EXACTLY (rclone's
/// `fs/config/crypt.go` `Decrypt` compares the first non-blank, non-comment line
/// against this literal).
pub const ENCRYPTED_MARKER: &str = "RCLONE_ENCRYPT_V0:";

/// The prefix of any encrypted-config marker, including versions this build does
/// not know. rclone itself errors with "unsupported configuration encryption -
/// update rclone for support" on a higher version, and so do we.
pub const ENCRYPTED_MARKER_PREFIX: &str = "RCLONE_ENCRYPT_V";

/// goconfig's `_DEPTH_VALUES`: the number of `%(name)s` substitution rounds
/// before it gives up. Reproduced exactly so a self-referential value
/// terminates here the same way it does in rclone, instead of hanging.
pub const MAX_INTERPOLATION: usize = 200;

/// A failure to read or parse an `rclone.conf`.
///
/// No variant carries a config VALUE - see the module docs.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigParseError {
    /// The whole config is password-encrypted, so there is nothing to parse.
    ///
    /// Driven deliberately does NOT prompt for the rclone config password.
    /// Two reasons, and the second is the one that decides it:
    ///
    /// - Decrypting means holding the key to EVERY remote the user owns -
    ///   including ones Driven cannot even target - inside Driven's process, to
    ///   read two or three settings out of one section.
    /// - The scheme (SHA-256 of `"[" + NFKC(password) + "][rclone-config]"`,
    ///   then NaCl secretbox with a 24-byte nonce prefix) has no version
    ///   negotiation and no way to distinguish "wrong password" from "we
    ///   implemented the KDF subtly wrong". A user staring at a failure could
    ///   not tell which, and would reasonably conclude their password was wrong.
    ///
    /// `rclone config show` already prints the decrypted config, so the user
    /// runs one command and the password never leaves rclone.
    #[error(
        "rclone.config_encrypted: this rclone config is password-encrypted. \
         Run `rclone config show > plain.conf`, import with `--config plain.conf`, then delete it"
    )]
    Encrypted,
    /// The file carries an encryption marker from a NEWER rclone.
    #[error(
        "rclone.config_encrypted: this config is encrypted with a newer rclone format. \
         Run `rclone config show > plain.conf` and import from that file"
    )]
    EncryptedUnsupportedVersion,
    /// A line was neither blank, a comment, a section header, nor `key = value`.
    #[error(
        "rclone.config_invalid: line {line}: expected `key = value`, a `[section]`, or a comment \
         (note that a trailing comment after a `[section]` header is a parse error in rclone too)"
    )]
    MalformedLine {
        /// 1-based line number in the file.
        line: usize,
    },
    /// A section header was empty (`[]`).
    #[error("rclone.config_invalid: line {line}: empty remote name in a section header")]
    EmptySectionName {
        /// 1-based line number in the file.
        line: usize,
    },
    /// A line began with the separator, so the option had no name.
    #[error("rclone.config_invalid: line {line}: empty option name")]
    EmptyKey {
        /// 1-based line number in the file.
        line: usize,
    },
}

/// One `[section]` from an `rclone.conf`: a remote name plus its options.
#[derive(Clone, PartialEq, Eq)]
pub struct RcloneRemote {
    /// The remote's name, as written between the brackets.
    pub name: String,
    /// Its options. Keys are CASE-SENSITIVE, exactly as goconfig stores them.
    values: BTreeMap<String, String>,
}

// Hand-written: a derived `Debug` would render `secret_access_key` in full into
// any `tracing` line, `anyhow` context or panic message that formatted a remote.
// Option NAMES are safe (a fixed vocabulary); values are not.
impl std::fmt::Debug for RcloneRemote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RcloneRemote")
            .field("name", &self.name)
            .field("options", &self.values.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl RcloneRemote {
    /// The value of `key`, or `None` when the option is absent OR present but
    /// empty.
    ///
    /// Absent and empty are deliberately the same answer: `rclone config` writes
    /// `region =` for an option the user skipped (see rclone's own documented
    /// MinIO example, which ends with three such lines), and every caller wants
    /// "did the user supply this" rather than "is the key present".
    ///
    /// Case-SENSITIVE, matching goconfig. Use the lowercase names rclone writes.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// The remote's `type` option (`s3`, `drive`, `b2`, ...), lowercased.
    ///
    /// The VALUE is lowercased even though the KEY is not: a mis-cased `type`
    /// value is a remote that is broken in rclone as well, and recognising it
    /// lets the importer explain what it is instead of reporting "no type".
    pub fn remote_type(&self) -> Option<String> {
        self.get("type").map(|t| t.to_ascii_lowercase())
    }

    /// Every option name present, sorted. Values are NOT exposed in bulk - a
    /// caller that wants one asks for it by name.
    pub fn option_names(&self) -> Vec<&str> {
        self.values.keys().map(String::as_str).collect()
    }
}

/// A parsed `rclone.conf`: its remotes in file order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RcloneConfig {
    remotes: Vec<RcloneRemote>,
}

impl RcloneConfig {
    /// Parse the text of an `rclone.conf`.
    ///
    /// Returns an encryption error before attempting any parsing when the
    /// whole-file encryption marker is present.
    pub fn parse(text: &str) -> Result<Self, ConfigParseError> {
        check_not_encrypted(text)?;

        let mut remotes: Vec<RcloneRemote> = Vec::new();
        // Index of the section currently being filled; `None` until the first
        // header (goconfig would file these under `DEFAULT`; see module docs).
        let mut current: Option<usize> = None;

        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            // goconfig consumes a leading UTF-8 BOM before anything else.
            let line = if idx == 0 {
                raw_line.trim_start_matches('\u{feff}').trim()
            } else {
                raw_line.trim()
            };

            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }

            // A header is `[` first and `]` last; goconfig examines only those
            // two positions, so `[a]b]` names the remote `a]b`.
            if line.starts_with('[') && line.ends_with(']') {
                let name = line[1..line.len() - 1].trim();
                if name.is_empty() {
                    return Err(ConfigParseError::EmptySectionName { line: line_no });
                }
                current = Some(match remotes.iter().position(|r| r.name == name) {
                    Some(existing) => existing,
                    None => {
                        remotes.push(RcloneRemote {
                            name: name.to_string(),
                            values: BTreeMap::new(),
                        });
                        remotes.len() - 1
                    }
                });
                continue;
            }

            // `strings.IndexAny(line, "=:")` - the FIRST of either separator.
            let sep = line
                .char_indices()
                .find(|(_, c)| *c == '=' || *c == ':')
                .map(|(i, _)| i)
                .ok_or(ConfigParseError::MalformedLine { line: line_no })?;
            let key = line[..sep].trim();
            if key.is_empty() {
                return Err(ConfigParseError::EmptyKey { line: line_no });
            }
            let value = line[sep + 1..].trim();
            if let Some(idx) = current {
                // Last value wins for a duplicated key.
                remotes[idx]
                    .values
                    .insert(key.to_string(), value.to_string());
            }
        }

        for remote in &mut remotes {
            resolve_interpolations(&mut remote.values);
        }

        Ok(Self { remotes })
    }

    /// Read and parse the file at `path`.
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
        // The PATH is safe to name in an error; the CONTENTS are not, so the
        // parse error is propagated as-is (it carries only line numbers).
        Self::parse(&text).map_err(|e| anyhow::anyhow!("{} ({})", e, path.display()))
    }

    /// Every remote, in file order.
    pub fn remotes(&self) -> &[RcloneRemote] {
        &self.remotes
    }

    /// The remote called `name`, if present. Remote names are case-sensitive in
    /// goconfig, so this lookup is too.
    pub fn remote(&self, name: &str) -> Option<&RcloneRemote> {
        self.remotes.iter().find(|r| r.name == name)
    }
}

/// rclone's `Decrypt` preamble, reproduced exactly.
///
/// It skips blank lines and `#`/`;` comments, then requires the FIRST remaining
/// line to be exactly [`ENCRYPTED_MARKER`]. Matching that rule rather than
/// scanning the body is what makes a plaintext config whose values happen to
/// contain the token parse correctly instead of being refused.
fn check_not_encrypted(text: &str) -> Result<(), ConfigParseError> {
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if l == ENCRYPTED_MARKER {
            return Err(ConfigParseError::Encrypted);
        }
        if l.starts_with(ENCRYPTED_MARKER_PREFIX) {
            return Err(ConfigParseError::EncryptedUnsupportedVersion);
        }
        // The first meaningful line is ordinary config: plaintext.
        return Ok(());
    }
    // Empty (or comments-only) file: plaintext with no remotes.
    Ok(())
}

/// Expand goconfig's `%(name)s` references within one section.
///
/// goconfig substitutes against the RAW stored values and re-scans, up to
/// [`MAX_INTERPOLATION`] rounds; the same raw snapshot is used here, so a chain
/// (`a` -> `b` -> `c`) resolves and a self-reference terminates at the cap
/// instead of looping. An unresolvable name is left literal rather than being
/// blanked - dropping it would silently produce a shorter endpoint or key.
fn resolve_interpolations(values: &mut BTreeMap<String, String>) {
    if !values.values().any(|v| v.contains("%(")) {
        return; // The overwhelmingly common case: nothing to do, nothing cloned.
    }
    let raw = values.clone();
    for value in values.values_mut() {
        let mut resolved = value.clone();
        for _ in 0..MAX_INTERPOLATION {
            let Some(start) = resolved.find("%(") else {
                break;
            };
            let Some(rel_end) = resolved[start + 2..].find(")s") else {
                break;
            };
            let end = start + 2 + rel_end;
            let Some(replacement) = raw.get(&resolved[start + 2..end]) else {
                break;
            };
            resolved = format!(
                "{}{}{}",
                &resolved[..start],
                replacement,
                &resolved[end + 2..]
            );
        }
        *value = resolved;
    }
}

/// One candidate location for an `rclone.conf`, with the reason it was tried,
/// so the CLI can explain "looked here, and here, and here" on a miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigCandidate {
    /// The path itself.
    pub path: PathBuf,
    /// Where it came from (`$RCLONE_CONFIG`, the XDG default, ...).
    pub origin: &'static str,
}

/// The environment lookups [`config_search_path`] needs, injected so the
/// resolution order is unit-testable without mutating the process environment
/// (which would race every other test in the binary).
pub trait EnvLookup {
    /// The value of environment variable `key`, or `None` when unset or blank.
    fn var(&self, key: &str) -> Option<String>;

    /// Whether `path` is an existing FILE. Injected alongside the variables so
    /// the portable-mode probe (is there an `rclone` binary in this PATH entry?)
    /// is testable without putting an executable on the real `PATH`.
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
}

/// [`EnvLookup`] backed by the real process environment and filesystem.
pub struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.trim().is_empty())
    }
}

/// The ordered list of places an `rclone.conf` may live, most specific first.
///
/// This mirrors rclone's own `makeConfigPath` (`fs/config/config.go`), which is
/// what `rclone config file` prints:
///
/// 1. **`$RCLONE_CONFIG`** - a full PATH to the file. Not a bespoke variable:
///    rclone maps every flag to `RCLONE_<FLAG>`, so this is `--config`. It wins
///    over everything.
/// 2. **The directory holding the `rclone` binary** - rclone's "portable mode"
///    check runs FIRST in its own resolution, so a `rclone.conf` sitting beside
///    a portable `rclone` shadows the user's home config entirely. Found here by
///    walking `$PATH` for the binary, which is the best available stand-in for
///    `os.Executable()` in another process.
/// 3. **`$RCLONE_CONFIG_DIR`** - honoured, with a caveat worth being precise
///    about: rclone SETS this variable for its own config files and
///    subprocesses (`os.Setenv("RCLONE_CONFIG_DIR", filepath.Dir(configPath))`)
///    and does NOT read it back as an override. So it is not a way for a user to
///    relocate their config - but whenever it IS set it points at the directory
///    holding the config rclone resolved, which is exactly what we want.
/// 4. **`%APPDATA%\rclone\rclone.conf`** - Windows only, and BEFORE the XDG
///    location in rclone's own order.
/// 5. **`$XDG_CONFIG_HOME/rclone/rclone.conf`** - all platforms, Windows too.
/// 6. **`~/.config/rclone/rclone.conf`** - the usual default.
/// 7. **`~/.rclone.conf`** - the legacy location.
///
/// Deliberately absent: `~/Library/Application Support` on macOS. rclone does
/// not call `os.UserConfigDir()`, so macOS follows the plain Unix path and
/// looking there would only ever produce a misleading "checked" line.
///
/// Existence is NOT checked here; [`find_config_file`] picks the first candidate
/// that exists, and the caller can list the rest in a not-found message.
pub fn config_search_path(env: &impl EnvLookup) -> Vec<ConfigCandidate> {
    let mut out: Vec<ConfigCandidate> = Vec::new();
    let push = |out: &mut Vec<ConfigCandidate>, path: PathBuf, origin: &'static str| {
        if !out.iter().any(|c| c.path == path) {
            out.push(ConfigCandidate { path, origin });
        }
    };

    if let Some(p) = env.var("RCLONE_CONFIG") {
        push(&mut out, PathBuf::from(p), "$RCLONE_CONFIG");
    }
    if let Some(dir) = rclone_binary_dir(env) {
        push(
            &mut out,
            dir.join("rclone.conf"),
            "beside the rclone binary (portable mode)",
        );
    }
    if let Some(d) = env.var("RCLONE_CONFIG_DIR") {
        push(
            &mut out,
            PathBuf::from(d).join("rclone.conf"),
            "$RCLONE_CONFIG_DIR/rclone.conf",
        );
    }
    if let Some(appdata) = env.var("APPDATA") {
        push(
            &mut out,
            PathBuf::from(appdata).join("rclone").join("rclone.conf"),
            "%APPDATA%/rclone/rclone.conf",
        );
    }
    if let Some(x) = env.var("XDG_CONFIG_HOME") {
        push(
            &mut out,
            PathBuf::from(x).join("rclone").join("rclone.conf"),
            "$XDG_CONFIG_HOME/rclone/rclone.conf",
        );
    }
    // rclone resolves `~` via `$HOME`, falling back on Windows to
    // `%USERPROFILE%` then `%HOMEDRIVE%%HOMEPATH%`. `std::env::home_dir` is
    // deprecated at this crate's MSRV, and reading the variables directly is
    // also what makes the order testable through `EnvLookup`.
    let home = env
        .var("HOME")
        .or_else(|| env.var("USERPROFILE"))
        .or_else(|| match (env.var("HOMEDRIVE"), env.var("HOMEPATH")) {
            (Some(d), Some(p)) => Some(format!("{d}{p}")),
            _ => None,
        })
        .map(PathBuf::from);
    if let Some(home) = home {
        push(
            &mut out,
            home.join(".config").join("rclone").join("rclone.conf"),
            "~/.config/rclone/rclone.conf",
        );
        push(&mut out, home.join(".rclone.conf"), "~/.rclone.conf");
    }

    out
}

/// The directory containing the `rclone` binary, found by walking `$PATH`.
///
/// A stand-in for rclone's own `os.Executable()`, which we cannot call from
/// another process. Returns the first `PATH` entry that actually holds an
/// `rclone` (or `rclone.exe`) file.
fn rclone_binary_dir(env: &impl EnvLookup) -> Option<PathBuf> {
    let path = env.var("PATH")?;
    // `;` on Windows, `:` elsewhere. Splitting on both would break a Unix path
    // that legitimately contains `;`, so the separator follows the build target.
    let sep = if cfg!(windows) { ';' } else { ':' };
    for entry in path.split(sep).filter(|e| !e.is_empty()) {
        let dir = PathBuf::from(entry);
        for exe in ["rclone", "rclone.exe"] {
            if env.is_file(&dir.join(exe)) {
                return Some(dir);
            }
        }
    }
    None
}

/// The first candidate in [`config_search_path`] that exists on disk.
pub fn find_config_file(env: &impl EnvLookup) -> Option<ConfigCandidate> {
    config_search_path(env)
        .into_iter()
        .find(|c| env.is_file(&c.path))
}

/// [`find_config_file`], with a ready-to-print explanation on a miss.
///
/// The message enumerates every place that WAS checked, because "no rclone
/// config found" on its own is unactionable - a user with a portable rclone or
/// a `--config` habit has no way to tell whether we looked in the right place.
/// It lives here rather than in the CLI so the wording is unit-tested against a
/// fake environment instead of only through a spawned binary.
pub fn locate_config(env: &impl EnvLookup) -> Result<ConfigCandidate, String> {
    if let Some(found) = find_config_file(env) {
        return Ok(found);
    }
    let mut msg = String::from("no rclone config found. Looked in:\n");
    let candidates = config_search_path(env);
    if candidates.is_empty() {
        msg.push_str("  (nowhere to look - no HOME/USERPROFILE, no rclone on PATH)\n");
    }
    for c in candidates {
        msg.push_str(&format!("  {} ({})\n", c.path.display(), c.origin));
    }
    msg.push_str(
        "Run `rclone config file` to see the path rclone uses, then pass --config <path>.",
    );
    Err(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `EnvLookup` over fixed tables, so resolution-order tests never touch
    /// the real process environment or filesystem.
    struct FakeEnv {
        vars: Vec<(&'static str, String)>,
        files: Vec<PathBuf>,
    }

    impl EnvLookup for FakeEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.vars
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
                .filter(|v| !v.trim().is_empty())
        }
        fn is_file(&self, path: &Path) -> bool {
            self.files.iter().any(|f| f == path)
        }
    }

    fn env(pairs: &[(&'static str, &str)]) -> FakeEnv {
        FakeEnv {
            vars: pairs.iter().map(|(k, v)| (*k, (*v).to_string())).collect(),
            files: Vec::new(),
        }
    }

    fn env_with_files(pairs: &[(&'static str, &str)], files: &[&str]) -> FakeEnv {
        FakeEnv {
            vars: pairs.iter().map(|(k, v)| (*k, (*v).to_string())).collect(),
            files: files.iter().map(PathBuf::from).collect(),
        }
    }

    const MULTI: &str = "\
# rclone config
; a semicolon comment

[r2]
type = s3
provider = Cloudflare
access_key_id = AKIDEXAMPLE
secret_access_key = wJalrXUtnFEMIexampleKEY
endpoint = https://abc123.r2.cloudflarestorage.com
region = auto

[gdrive]
type = drive
scope = drive
root_folder_id = 1AbCdEfGhIjKlMnOp
token = {\"access_token\":\"ya29.x\",\"refresh_token\":\"1//0eX\",\"expiry\":\"2026-01-01T00:00:00Z\"}
";

    // ---------------------------------------------------------------
    // Core parsing
    // ---------------------------------------------------------------

    #[test]
    fn parses_multiple_remotes_in_file_order() {
        let cfg = RcloneConfig::parse(MULTI).expect("parses");
        assert_eq!(
            cfg.remotes()
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            vec!["r2", "gdrive"]
        );
        let r2 = cfg.remote("r2").expect("r2");
        assert_eq!(r2.remote_type().as_deref(), Some("s3"));
        assert_eq!(r2.get("provider"), Some("Cloudflare"));
        assert_eq!(r2.get("region"), Some("auto"));
        assert_eq!(
            cfg.remote("gdrive").unwrap().remote_type().as_deref(),
            Some("drive")
        );
        assert!(cfg.remote("nope").is_none());
    }

    #[test]
    fn a_token_blob_survives_intact_despite_containing_both_separators() {
        // The `token` value contains `:` AND `=`-free JSON; splitting on the
        // first `=` (not the first `=`-or-`:`) is what keeps it whole.
        let cfg = RcloneConfig::parse(MULTI).unwrap();
        let token = cfg.remote("gdrive").unwrap().get("token").unwrap();
        assert!(token.starts_with('{') && token.ends_with('}'), "{token}");
        assert!(token.contains("\"refresh_token\":\"1//0eX\""));
    }

    #[test]
    fn a_colon_is_a_separator_exactly_like_an_equals_sign() {
        // goconfig: `strings.IndexAny(line, "=:")`.
        let cfg = RcloneConfig::parse("[a]\ntype: s3\nregion:eu-west-1\n").unwrap();
        let a = cfg.remote("a").unwrap();
        assert_eq!(a.remote_type().as_deref(), Some("s3"));
        assert_eq!(a.get("region"), Some("eu-west-1"));
    }

    #[test]
    fn the_first_separator_wins_whichever_it_is() {
        // `endpoint = https://x` -> the `=` comes first, so the URL survives.
        let cfg = RcloneConfig::parse("[a]\nendpoint = https://x:9000/p\n").unwrap();
        assert_eq!(
            cfg.remote("a").unwrap().get("endpoint"),
            Some("https://x:9000/p")
        );
        // `endpoint : https://x` -> the `:` comes first and is the separator.
        let cfg = RcloneConfig::parse("[a]\nendpoint : https://x:9000/p\n").unwrap();
        assert_eq!(
            cfg.remote("a").unwrap().get("endpoint"),
            Some("https://x:9000/p")
        );
    }

    #[test]
    fn comments_are_own_line_only_so_values_keep_their_hashes() {
        // rclone writes credentials verbatim; trimming at `#` would truncate one.
        let cfg = RcloneConfig::parse(
            "[a]\n# leading comment\n  ; indented comment\npass = p#ss;word\n\ntype = s3\n",
        )
        .unwrap();
        let a = cfg.remote("a").unwrap();
        assert_eq!(a.get("pass"), Some("p#ss;word"));
        assert_eq!(a.remote_type().as_deref(), Some("s3"));
    }

    #[test]
    fn whitespace_around_the_separator_is_trimmed_but_interior_spaces_survive() {
        let cfg =
            RcloneConfig::parse("[a]\ntype=s3\n   description   =   my home nas   \n").unwrap();
        let a = cfg.remote("a").unwrap();
        assert_eq!(a.remote_type().as_deref(), Some("s3"));
        assert_eq!(a.get("description"), Some("my home nas"));
    }

    #[test]
    fn keys_are_case_sensitive_exactly_as_goconfig_stores_them() {
        // `Type` is a key rclone never reads, so this remote genuinely has no
        // type - reporting it as `s3` would claim rclone used a remote it could
        // not have used.
        let cfg = RcloneConfig::parse("[a]\nType = s3\n").unwrap();
        let a = cfg.remote("a").unwrap();
        assert_eq!(a.remote_type(), None);
        assert_eq!(a.get("Type"), Some("s3"));
        assert!(a.option_names().contains(&"Type"));
    }

    #[test]
    fn the_last_duplicate_key_wins() {
        let cfg = RcloneConfig::parse("[a]\ntype = s3\ntype = drive\n").unwrap();
        assert_eq!(
            cfg.remote("a").unwrap().remote_type().as_deref(),
            Some("drive")
        );
    }

    #[test]
    fn a_repeated_section_merges_into_the_first() {
        let cfg =
            RcloneConfig::parse("[a]\ntype = s3\n[b]\ntype = drive\n[a]\nregion = eu\n").unwrap();
        assert_eq!(cfg.remotes().len(), 2, "no duplicate remote entry");
        let a = cfg.remote("a").unwrap();
        assert_eq!(a.remote_type().as_deref(), Some("s3"));
        assert_eq!(a.get("region"), Some("eu"));
    }

    #[test]
    fn an_empty_value_reads_as_absent_but_the_key_is_still_present() {
        // rclone's own documented MinIO config ends with three such lines.
        let cfg =
            RcloneConfig::parse("[a]\ntype = s3\nregion =\nlocation_constraint =    \n").unwrap();
        let a = cfg.remote("a").unwrap();
        assert_eq!(a.get("region"), None);
        assert_eq!(a.get("location_constraint"), None);
        assert!(a.option_names().contains(&"region"));
    }

    #[test]
    fn only_the_first_and_last_characters_decide_a_section_header() {
        // goconfig checks `line[0]=='[' && line[len-1]==']'` and nothing else.
        let cfg = RcloneConfig::parse("[a]b]\ntype = s3\n").unwrap();
        assert_eq!(cfg.remotes()[0].name, "a]b");
    }

    #[test]
    fn a_remote_name_may_contain_spaces_dashes_dots_and_unicode() {
        // rclone's own name rule allows unicode letters/numbers, `_-.+@` and
        // spaces; goconfig itself imposes no restriction at all. The non-ASCII
        // name is written as an escape so this source file stays pure ASCII
        // (repo convention) while still exercising a multi-byte section name -
        // which also proves the `line[1..len-1]` slicing is char-boundary safe.
        let cfg = RcloneConfig::parse(
            "[my remote.2-prod+eu@corp]\ntype = s3\n[\u{d8}stersund-drive]\ntype = drive\n",
        )
        .unwrap();
        assert_eq!(cfg.remotes()[0].name, "my remote.2-prod+eu@corp");
        assert_eq!(cfg.remotes()[1].name, "\u{d8}stersund-drive");
    }

    #[test]
    fn a_key_before_any_section_is_dropped() {
        // goconfig files it under `DEFAULT`; no remote can live there.
        let cfg = RcloneConfig::parse("stray = value\n[a]\ntype = s3\n").unwrap();
        assert_eq!(cfg.remotes().len(), 1);
        assert_eq!(cfg.remote("a").unwrap().get("stray"), None);
    }

    #[test]
    fn a_leading_bom_does_not_break_the_first_section() {
        let cfg = RcloneConfig::parse("\u{feff}[a]\ntype = s3\n").unwrap();
        assert_eq!(cfg.remotes()[0].name, "a");
    }

    #[test]
    fn crlf_line_endings_parse_identically() {
        // goconfig writes \r\n on Windows; the whole line is trimmed on read.
        let cfg = RcloneConfig::parse("[a]\r\ntype = s3\r\nregion = eu\r\n").unwrap();
        let a = cfg.remote("a").unwrap();
        assert_eq!(a.remote_type().as_deref(), Some("s3"));
        assert_eq!(a.get("region"), Some("eu"));
    }

    #[test]
    fn a_final_line_without_a_newline_is_still_read() {
        let cfg = RcloneConfig::parse("[a]\ntype = s3").unwrap();
        assert_eq!(
            cfg.remote("a").unwrap().remote_type().as_deref(),
            Some("s3")
        );
    }

    #[test]
    fn an_empty_config_has_no_remotes() {
        assert!(RcloneConfig::parse("").unwrap().remotes().is_empty());
        assert!(RcloneConfig::parse("\n\n# just a comment\n\n")
            .unwrap()
            .remotes()
            .is_empty());
    }

    // ---------------------------------------------------------------
    // Malformed input
    // ---------------------------------------------------------------

    #[test]
    fn malformed_lines_are_rejected_with_a_line_number_and_no_value() {
        let err = RcloneConfig::parse("[a]\nthis line has no separator\n").unwrap_err();
        assert_eq!(err, ConfigParseError::MalformedLine { line: 2 });
        assert!(
            !err.to_string().contains("this line has no separator"),
            "a parse error must never echo the line: {err}"
        );

        assert_eq!(
            RcloneConfig::parse("[unterminated\ntype = s3\n").unwrap_err(),
            ConfigParseError::MalformedLine { line: 1 },
            "an unterminated header has no separator either"
        );
        assert_eq!(
            RcloneConfig::parse("[]\ntype = s3\n").unwrap_err(),
            ConfigParseError::EmptySectionName { line: 1 }
        );
        assert_eq!(
            RcloneConfig::parse("[   ]\ntype = s3\n").unwrap_err(),
            ConfigParseError::EmptySectionName { line: 1 }
        );
        assert_eq!(
            RcloneConfig::parse("[a]\n = orphaned\n").unwrap_err(),
            ConfigParseError::EmptyKey { line: 2 }
        );
        assert_eq!(
            RcloneConfig::parse("[a]\n: orphaned\n").unwrap_err(),
            ConfigParseError::EmptyKey { line: 2 }
        );
    }

    #[test]
    fn a_trailing_comment_after_a_section_header_is_a_parse_error_as_in_rclone() {
        // Surprising but faithful: the line no longer ends with `]`, so goconfig
        // treats it as a key/value line and fails to find a separator... except
        // that `[remote] ; hi` HAS no `=`/`:`, so it is a hard error there too.
        let err = RcloneConfig::parse("[remote] ; hi\ntype = s3\n").unwrap_err();
        assert_eq!(err, ConfigParseError::MalformedLine { line: 1 });
        assert!(
            err.to_string().contains("trailing comment"),
            "the message must explain this surprise: {err}"
        );
    }

    // ---------------------------------------------------------------
    // Interpolation
    // ---------------------------------------------------------------

    #[test]
    fn percent_name_s_references_are_expanded_like_goconfig() {
        let cfg = RcloneConfig::parse(
            "[a]\ntype = s3\nhost = minio.example\nendpoint = https://%(host)s:9000\n",
        )
        .unwrap();
        assert_eq!(
            cfg.remote("a").unwrap().get("endpoint"),
            Some("https://minio.example:9000")
        );
    }

    #[test]
    fn interpolation_resolves_forward_references_and_chains() {
        // goconfig resolves at read time against the raw values, so definition
        // order does not matter and a chain collapses.
        let cfg = RcloneConfig::parse(
            "[a]\nendpoint = https://%(host)s\nhost = %(sub)s.example.com\nsub = files\n",
        )
        .unwrap();
        assert_eq!(
            cfg.remote("a").unwrap().get("endpoint"),
            Some("https://files.example.com")
        );
    }

    #[test]
    fn an_unresolvable_reference_is_left_literal_rather_than_blanked() {
        // Blanking would silently shorten an endpoint or a key.
        let cfg = RcloneConfig::parse("[a]\nendpoint = https://%(nope)s/x\n").unwrap();
        assert_eq!(
            cfg.remote("a").unwrap().get("endpoint"),
            Some("https://%(nope)s/x")
        );
    }

    #[test]
    fn a_self_referential_value_terminates_instead_of_hanging() {
        // Bounded by goconfig's own `_DEPTH_VALUES` cap.
        let cfg = RcloneConfig::parse("[a]\nloop = %(loop)s-tail\n").unwrap();
        let value = cfg.remote("a").unwrap().get("loop").expect("a value");
        assert!(value.ends_with("-tail"));
        assert!(
            value.matches("-tail").count() <= MAX_INTERPOLATION + 1,
            "expansion must be bounded"
        );
    }

    #[test]
    fn a_config_with_no_references_is_untouched() {
        let cfg =
            RcloneConfig::parse("[a]\nsecret_access_key = 100%-real\nnote = 50% off\n").unwrap();
        let a = cfg.remote("a").unwrap();
        assert_eq!(a.get("secret_access_key"), Some("100%-real"));
        assert_eq!(a.get("note"), Some("50% off"));
    }

    // ---------------------------------------------------------------
    // Encryption detection
    // ---------------------------------------------------------------

    #[test]
    fn a_whole_file_encrypted_config_is_detected_not_parsed() {
        let encrypted =
            "# Encrypted rclone configuration File\n\nRCLONE_ENCRYPT_V0:\nc3VwZXJzZWNyZXRibG9i\n";
        let err = RcloneConfig::parse(encrypted).unwrap_err();
        assert_eq!(err, ConfigParseError::Encrypted);
        assert!(
            err.to_string().contains("rclone config show"),
            "the error must point at the supported way out: {err}"
        );
        // The way out must be `rclone config show`, never "give Driven your
        // rclone password" - Driven has no prompt for it and must not grow one.
        let text = err.to_string().to_lowercase();
        for invitation in ["enter your password", "--password", "password:", "prompt"] {
            assert!(
                !text.contains(invitation),
                "the error must not ask for the rclone password ({invitation:?}): {err}"
            );
        }

        // The marker without the comment header (rclone's own check ignores it).
        assert_eq!(
            RcloneConfig::parse("RCLONE_ENCRYPT_V0:\nYWJj\n").unwrap_err(),
            ConfigParseError::Encrypted
        );
    }

    #[test]
    fn a_newer_encryption_version_is_reported_as_such() {
        assert_eq!(
            RcloneConfig::parse("RCLONE_ENCRYPT_V9:\nYWJj\n").unwrap_err(),
            ConfigParseError::EncryptedUnsupportedVersion
        );
    }

    #[test]
    fn the_marker_below_the_first_meaningful_line_does_not_trip_the_detector() {
        // rclone only inspects the FIRST non-blank, non-comment line, so a
        // plaintext config whose value happens to contain the token parses.
        let text = "[a]\ntype = s3\nnote = RCLONE_ENCRYPT_V0:\n";
        let cfg = RcloneConfig::parse(text).expect("plaintext despite the token");
        assert_eq!(
            cfg.remote("a").unwrap().remote_type().as_deref(),
            Some("s3")
        );
        assert_eq!(
            cfg.remote("a").unwrap().get("note"),
            Some("RCLONE_ENCRYPT_V0:")
        );
    }

    // ---------------------------------------------------------------
    // Secrets
    // ---------------------------------------------------------------

    #[test]
    fn debug_never_renders_an_option_value() {
        let cfg = RcloneConfig::parse(MULTI).unwrap();
        let rendered = format!("{:?}", cfg.remote("r2").unwrap());
        assert!(rendered.contains("r2"), "the name is safe to show");
        assert!(
            rendered.contains("secret_access_key"),
            "names are a fixed vocabulary"
        );
        assert!(
            !rendered.contains("wJalrXUtnFEMIexampleKEY"),
            "Debug leaked a secret: {rendered}"
        );
        assert!(!rendered.contains("AKIDEXAMPLE"));
        // ...and the same through the containing config.
        assert!(!format!("{cfg:?}").contains("wJalrXUtnFEMIexampleKEY"));
    }

    // ---------------------------------------------------------------
    // File I/O
    // ---------------------------------------------------------------

    #[test]
    fn read_parses_a_real_file_and_reports_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone.conf");
        std::fs::write(&path, MULTI).unwrap();
        let cfg = RcloneConfig::read(&path).expect("reads");
        assert_eq!(cfg.remotes().len(), 2);

        let err = RcloneConfig::read(&dir.path().join("absent.conf")).unwrap_err();
        assert!(err.to_string().contains("absent.conf"));
    }

    #[test]
    fn read_surfaces_the_encrypted_marker_with_the_path_but_not_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone.conf");
        std::fs::write(&path, "RCLONE_ENCRYPT_V0:\nc2VjcmV0Ym9keQ\n").unwrap();
        let err = RcloneConfig::read(&path).unwrap_err().to_string();
        assert!(err.contains("rclone config show"));
        assert!(err.contains("rclone.conf"));
        assert!(
            !err.contains("c2VjcmV0Ym9keQ"),
            "leaked the ciphertext: {err}"
        );
    }

    // ---------------------------------------------------------------
    // Location resolution
    // ---------------------------------------------------------------

    #[test]
    fn rclone_config_wins_over_every_other_location() {
        let path = config_search_path(&env(&[
            ("RCLONE_CONFIG", "/explicit/my.conf"),
            ("RCLONE_CONFIG_DIR", "/dir"),
            ("XDG_CONFIG_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ]));
        assert_eq!(path[0].path, PathBuf::from("/explicit/my.conf"));
        assert_eq!(path[0].origin, "$RCLONE_CONFIG");
        assert_eq!(path[1].path, PathBuf::from("/dir/rclone.conf"));
        assert_eq!(path[2].path, PathBuf::from("/xdg/rclone/rclone.conf"));
        assert_eq!(
            path[3].path,
            PathBuf::from("/home/u/.config/rclone/rclone.conf")
        );
        assert_eq!(path[4].path, PathBuf::from("/home/u/.rclone.conf"));
    }

    #[test]
    fn a_portable_rclone_on_path_shadows_the_home_config() {
        // rclone checks its own directory FIRST, so a config beside a portable
        // binary is the one it is really using.
        let exe = if cfg!(windows) {
            "rclone.exe"
        } else {
            "rclone"
        };
        let sep = if cfg!(windows) { ";" } else { ":" };
        let e = env_with_files(
            &[
                ("PATH", &format!("/empty{sep}/opt/rclone{sep}/usr/bin")),
                ("HOME", "/home/u"),
            ],
            &[&format!("/opt/rclone/{exe}")],
        );
        let path = config_search_path(&e);
        assert_eq!(path[0].path, PathBuf::from("/opt/rclone/rclone.conf"));
        assert!(path[0].origin.contains("portable"));
        assert_eq!(
            path[1].path,
            PathBuf::from("/home/u/.config/rclone/rclone.conf")
        );
    }

    #[test]
    fn no_rclone_on_path_means_no_portable_candidate() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let e = env_with_files(
            &[("PATH", &format!("/usr/bin{sep}/bin")), ("HOME", "/home/u")],
            &[],
        );
        let path = config_search_path(&e);
        assert!(
            path.iter().all(|c| !c.origin.contains("portable")),
            "{path:?}"
        );
    }

    #[test]
    fn the_default_location_is_the_xdg_one_under_home() {
        let path = config_search_path(&env(&[("HOME", "/home/u")]));
        assert_eq!(path.len(), 2);
        assert_eq!(
            path[0].path,
            PathBuf::from("/home/u/.config/rclone/rclone.conf")
        );
        assert_eq!(path[1].path, PathBuf::from("/home/u/.rclone.conf"));
    }

    #[test]
    fn windows_checks_appdata_before_xdg_and_falls_back_through_home_variables() {
        let path = config_search_path(&env(&[
            ("APPDATA", "C:/Users/u/AppData/Roaming"),
            ("XDG_CONFIG_HOME", "C:/xdg"),
            ("USERPROFILE", "C:/Users/u"),
        ]));
        assert_eq!(
            path[0].path,
            PathBuf::from("C:/Users/u/AppData/Roaming/rclone/rclone.conf"),
            "rclone checks %APPDATA% before XDG"
        );
        assert_eq!(path[1].path, PathBuf::from("C:/xdg/rclone/rclone.conf"));
        assert_eq!(
            path[2].path,
            PathBuf::from("C:/Users/u/.config/rclone/rclone.conf")
        );

        // HOMEDRIVE+HOMEPATH is the last home fallback rclone uses.
        let path = config_search_path(&env(&[("HOMEDRIVE", "D:"), ("HOMEPATH", "/Users/u")]));
        assert_eq!(
            path[0].path,
            PathBuf::from("D:/Users/u/.config/rclone/rclone.conf")
        );
    }

    #[test]
    fn blank_env_vars_are_ignored_and_duplicate_paths_collapse() {
        let path = config_search_path(&env(&[
            ("RCLONE_CONFIG", "   "),
            ("XDG_CONFIG_HOME", "/home/u/.config"),
            ("HOME", "/home/u"),
        ]));
        // `$XDG_CONFIG_HOME/rclone/rclone.conf` and `~/.config/rclone/rclone.conf`
        // are the same file here; it must appear once, under the first origin.
        assert_eq!(path[0].origin, "$XDG_CONFIG_HOME/rclone/rclone.conf");
        assert_eq!(
            path.iter()
                .filter(|c| c.path == Path::new("/home/u/.config/rclone/rclone.conf"))
                .count(),
            1
        );
    }

    #[test]
    fn with_no_environment_at_all_there_is_nothing_to_try() {
        assert!(config_search_path(&env(&[])).is_empty());
        assert!(find_config_file(&env(&[])).is_none());
    }

    #[test]
    fn find_config_file_returns_the_first_existing_candidate() {
        let e = env_with_files(
            &[
                ("RCLONE_CONFIG", "/missing/my.conf"),
                ("RCLONE_CONFIG_DIR", "/dir"),
                ("HOME", "/home/u"),
            ],
            &["/dir/rclone.conf", "/home/u/.rclone.conf"],
        );
        let found = find_config_file(&e).expect("a candidate exists");
        assert_eq!(found.path, PathBuf::from("/dir/rclone.conf"));
        assert_eq!(found.origin, "$RCLONE_CONFIG_DIR/rclone.conf");
    }

    #[test]
    fn locate_config_returns_the_hit_or_an_explanation_naming_every_place_checked() {
        let e = env_with_files(
            &[("HOME", "/home/u")],
            &["/home/u/.config/rclone/rclone.conf"],
        );
        assert_eq!(
            locate_config(&e).expect("found").path,
            PathBuf::from("/home/u/.config/rclone/rclone.conf")
        );

        // A miss must enumerate what was checked - "not found" alone gives a
        // user with a portable rclone no way to tell whether we looked right.
        let msg = locate_config(&env(&[
            ("RCLONE_CONFIG", "/explicit/my.conf"),
            ("HOME", "/home/u"),
        ]))
        .unwrap_err();
        assert!(msg.contains("/explicit/my.conf"), "{msg}");
        assert!(msg.contains("$RCLONE_CONFIG"), "{msg}");
        assert!(msg.contains("/home/u/.config/rclone/rclone.conf"), "{msg}");
        assert!(msg.contains("/home/u/.rclone.conf"), "{msg}");
        assert!(msg.contains("rclone config file"), "{msg}");
        assert!(msg.contains("--config"), "{msg}");

        // With no environment at all there is genuinely nothing to list, and the
        // message must say that rather than printing an empty bullet list.
        let msg = locate_config(&env(&[])).unwrap_err();
        assert!(msg.contains("nowhere to look"), "{msg}");
    }

    #[test]
    fn the_real_process_env_lookup_treats_blank_as_unset() {
        // ProcessEnv is the only piece the fakes above do not exercise. No other
        // test touches this variable, so the set/remove here cannot race.
        std::env::set_var("DRIVEN_RCLONE_TEST_VAR", "  ");
        assert_eq!(ProcessEnv.var("DRIVEN_RCLONE_TEST_VAR"), None);
        std::env::set_var("DRIVEN_RCLONE_TEST_VAR", "/some/path");
        assert_eq!(
            ProcessEnv.var("DRIVEN_RCLONE_TEST_VAR").as_deref(),
            Some("/some/path")
        );
        std::env::remove_var("DRIVEN_RCLONE_TEST_VAR");
        assert_eq!(ProcessEnv.var("DRIVEN_RCLONE_TEST_VAR"), None);

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        assert!(!ProcessEnv.is_file(&file));
        std::fs::write(&file, b"x").unwrap();
        assert!(ProcessEnv.is_file(&file));
        assert!(!ProcessEnv.is_file(dir.path()), "a directory is not a file");
    }
}
