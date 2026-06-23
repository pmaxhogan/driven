//! [`VssMode`] - the persisted `windows.vss_mode` setting (SPEC s22).

use serde::{Deserialize, Serialize};

/// How aggressively to use VSS for reading source files (SPEC s22
/// `windows.vss_mode`).
///
/// Persisted under the `windows` settings key (Windows-only). Read once per
/// orchestrator cycle; a change takes effect on the next cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VssMode {
    /// Try a direct (live) open first; fall back to a VSS snapshot only on
    /// `ERROR_SHARING_VIOLATION` (a locked file). The default - it pays the
    /// snapshot cost only when a file actually needs it.
    #[default]
    Auto,
    /// Snapshot the volume per cycle and route EVERY read through the snapshot,
    /// even for files that are not locked (paranoid: guarantees a single
    /// point-in-time view of the whole source per cycle).
    Always,
    /// Never use VSS. Locked files are always skipped and surfaced
    /// (`local.file_locked` / `local.vss_unavailable`) - the no-elevation
    /// behaviour, made explicit.
    Never,
}

impl VssMode {
    /// The stable string used in the `windows.vss_mode` JSON setting.
    pub fn as_str(self) -> &'static str {
        match self {
            VssMode::Auto => "auto",
            VssMode::Always => "always",
            VssMode::Never => "never",
        }
    }

    /// Parse a `windows.vss_mode` string; an unknown value falls back to the
    /// safe default ([`VssMode::Auto`]) rather than erroring, so a settings
    /// row written by a newer build never bricks an older one.
    pub fn from_str_lenient(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "always" => VssMode::Always,
            "never" => VssMode::Never,
            // "auto" and anything unrecognised.
            _ => VssMode::Auto,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_via_serde() {
        for mode in [VssMode::Auto, VssMode::Always, VssMode::Never] {
            let json = serde_json::to_string(&mode).unwrap();
            let back: VssMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, back);
            // The serde wire form matches `as_str` (snake_case).
            assert_eq!(json, format!("\"{}\"", mode.as_str()));
        }
    }

    #[test]
    fn default_is_auto() {
        assert_eq!(VssMode::default(), VssMode::Auto);
    }

    #[test]
    fn lenient_parse_handles_case_and_unknown() {
        assert_eq!(VssMode::from_str_lenient(" Always "), VssMode::Always);
        assert_eq!(VssMode::from_str_lenient("NEVER"), VssMode::Never);
        assert_eq!(VssMode::from_str_lenient("auto"), VssMode::Auto);
        assert_eq!(VssMode::from_str_lenient("nonsense"), VssMode::Auto);
    }
}
