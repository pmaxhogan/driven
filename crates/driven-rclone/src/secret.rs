//! A string that cannot reach a log line by accident.
//!
//! `rclone.conf` is a file of credentials. Everything this crate parses out of
//! it that could authenticate someone is wrapped in [`Secret`], whose `Debug`
//! and `Display` render a fixed placeholder. Getting at the value requires
//! calling [`Secret::expose`], which is greppable: a reviewer can find every
//! place a credential is used by searching for that one method name.

/// What [`Secret`] renders instead of its value.
pub const REDACTED: &str = "<redacted>";

/// A credential read out of an rclone config.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The underlying value.
    ///
    /// Every call site is a deliberate decision to move a credential somewhere.
    /// Grep for `expose(` when auditing.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the wrapped value is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

// `Display` is redacted too: an `anyhow` chain, a `format!("{}", ..)` in a
// message, or a `tracing` field would otherwise print the value in full.
impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neither_debug_nor_display_renders_the_value() {
        let s = Secret::new("wJalrXUtnFEMIexampleKEY");
        assert_eq!(format!("{s:?}"), REDACTED);
        assert_eq!(format!("{s}"), REDACTED);
        // Nested inside another structure, which is how it actually escapes.
        assert_eq!(
            format!("{:?}", Some(s.clone())),
            format!("Some({REDACTED})")
        );
        assert_eq!(format!("{:?}", vec![s.clone()]), format!("[{REDACTED}]"));
    }

    #[test]
    fn expose_returns_the_value_and_is_the_only_way_to_get_it() {
        let s = Secret::new("hunter2");
        assert_eq!(s.expose(), "hunter2");
        assert!(!s.is_empty());
        assert!(Secret::new("").is_empty());
        assert_eq!(s, Secret::new("hunter2"));
        assert_ne!(s, Secret::new("hunter3"));
    }
}
