//! Snapshot assertions for remote-store listings.
//!
//! [`assert_remote_eq!`] compares an iterator of
//! [`RemoteEntry`](driven_drive::remote_store::RemoteEntry) values
//! against a declarative expected listing and prints a pretty diff on
//! mismatch (via [`pretty_assertions`]).
//!
//! Why a normalising compare rather than a raw `assert_eq!` on
//! `Vec<RemoteEntry>`? Two reasons:
//! - [`RemoteEntry`] does not derive [`PartialEq`] (the
//!   `app_properties` map and the generated Drive `id` make
//!   trait-derived equality the wrong default for most tests).
//! - Tests almost always care about the *shape* of the listing - which
//!   names exist, at what sizes, and whether the entry is trashed - not
//!   about the exact `file_id` or `modified_time` the fake or Drive
//!   assigned.
//!
//! Both sides are normalised to a stable `BTreeMap<String,
//! RemoteSnapshot>` keyed by entry name. The expected side is supplied as
//! a slice of [`ExpectedEntry`] (constructed by the macro from a tuple
//! DSL); the actual side is any iterator of `RemoteEntry`. Trashed
//! entries are skipped on both sides by default - tests for trashing
//! semantics should use the lower-level
//! [`normalize_actual`] / [`normalize_expected`] helpers directly.
//!
//! ```ignore
//! use driven_test_fixtures::assert_remote_eq;
//! # let actual: Vec<driven_drive::remote_store::RemoteEntry> = vec![];
//! assert_remote_eq!(actual, [
//!     ("a.txt", 5u64),
//!     ("b.bin", 1024u64),
//!     ("sub", dir),
//! ]);
//! ```

use std::collections::BTreeMap;

use driven_drive::remote_store::RemoteEntry;

#[doc(hidden)]
pub use pretty_assertions;

/// Stable, comparable projection of a [`RemoteEntry`].
///
/// Only the fields tests routinely care about; deliberately omits the
/// Drive `file_id`, `app_properties`, and `modified_time` (those are
/// covered by lower-level tests that touch a single entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSnapshot {
    /// Size in bytes; `None` for folders, `Some(0)` for empty files.
    pub size: Option<u64>,
}

/// One row of an expected listing, as constructed by the
/// [`assert_remote_eq!`] macro from its tuple DSL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedEntry {
    /// Entry name.
    pub name: String,
    /// Expected size; `None` for folders.
    pub size: Option<u64>,
}

/// Normalises an iterator of actual [`RemoteEntry`] values into the
/// comparable map shape. Trashed entries are dropped.
pub fn normalize_actual<I>(entries: I) -> BTreeMap<String, RemoteSnapshot>
where
    I: IntoIterator<Item = RemoteEntry>,
{
    entries
        .into_iter()
        .filter(|e| !e.trashed)
        .map(|e| (e.name.clone(), RemoteSnapshot { size: e.size }))
        .collect()
}

/// Normalises a slice of [`ExpectedEntry`] into the comparable map shape.
pub fn normalize_expected(expected: &[ExpectedEntry]) -> BTreeMap<String, RemoteSnapshot> {
    expected
        .iter()
        .map(|e| (e.name.clone(), RemoteSnapshot { size: e.size }))
        .collect()
}

/// Helper trait that lets the [`assert_remote_eq!`] DSL accept either a
/// plain integer (interpreted as a file size in bytes) or an
/// `Option<u64>` (used for folders, with `None`, or for explicit
/// `Some(size)`).
///
/// Implemented for `u64`, `u32`, `usize`, and `Option<u64>` so the most
/// common expected-size literals "just work".
pub trait IntoExpectedSize {
    /// Returns the expected size, or `None` to mark a folder entry.
    fn into_expected_size(self) -> Option<u64>;
}

impl IntoExpectedSize for u64 {
    fn into_expected_size(self) -> Option<u64> {
        Some(self)
    }
}

impl IntoExpectedSize for u32 {
    fn into_expected_size(self) -> Option<u64> {
        Some(self as u64)
    }
}

impl IntoExpectedSize for usize {
    fn into_expected_size(self) -> Option<u64> {
        Some(self as u64)
    }
}

impl IntoExpectedSize for i32 {
    fn into_expected_size(self) -> Option<u64> {
        // Negative sizes are nonsense; clamp to 0 (a test passing a
        // negative literal almost certainly has a bug we want surfaced
        // by the mismatch, not the conversion).
        Some(self.max(0) as u64)
    }
}

impl IntoExpectedSize for Option<u64> {
    fn into_expected_size(self) -> Option<u64> {
        self
    }
}

/// Assert that a remote listing matches an expected shape.
///
/// `actual` must be `IntoIterator<Item = RemoteEntry>`. `expected` is a
/// bracket-delimited tuple list - each tuple is `(name, size)` where
/// `size` is one of:
/// - an integer literal: interpreted as a file size in bytes;
/// - the keyword `dir`: declares the entry is a folder (no size);
/// - any other expression of type `Option<u64>` or `u64`.
///
/// On mismatch, prints a pretty diff via [`pretty_assertions`].
#[macro_export]
macro_rules! assert_remote_eq {
    ($actual:expr, [ $( ( $name:expr, $size:tt ) ),* $(,)? ] $(,)?) => {{
        let __actual = $crate::assert::normalize_actual($actual);
        let __expected_vec: ::std::vec::Vec<$crate::assert::ExpectedEntry> = ::std::vec![
            $(
                $crate::assert::ExpectedEntry {
                    name: ::std::string::String::from($name),
                    size: $crate::__assert_remote_size!($size),
                }
            ),*
        ];
        let __expected = $crate::assert::normalize_expected(&__expected_vec);
        $crate::assert::pretty_assertions::assert_eq!(__actual, __expected);
    }};
}

/// Internal helper of [`assert_remote_eq!`]: maps the per-tuple size token
/// into an `Option<u64>`. The `dir` keyword expands to `None`; anything
/// else is converted via [`IntoExpectedSize`].
#[doc(hidden)]
#[macro_export]
macro_rules! __assert_remote_size {
    (dir) => {
        ::std::option::Option::<u64>::None
    };
    ($other:tt) => {{
        use $crate::assert::IntoExpectedSize as _;
        ($other).into_expected_size()
    }};
}

#[cfg(test)]
mod tests {
    use driven_drive::remote_store::RemoteEntry;
    use std::collections::HashMap;

    fn entry(name: &str, size: Option<u64>, trashed: bool) -> RemoteEntry {
        RemoteEntry {
            id: format!("id-{name}"),
            name: name.to_string(),
            parents: vec!["root".into()],
            size,
            md5: None,
            mime_type: "application/octet-stream".into(),
            modified_time: 0,
            trashed,
            app_properties: HashMap::new(),
        }
    }

    #[test]
    fn matches_simple_listing() {
        let actual = vec![
            entry("a.txt", Some(5), false),
            entry("b.bin", Some(1024), false),
        ];
        assert_remote_eq!(actual, [("a.txt", 5u64), ("b.bin", 1024u64),]);
    }

    #[test]
    fn ignores_trashed_actual_entries() {
        let actual = vec![
            entry("a.txt", Some(5), false),
            entry("gone.txt", Some(99), true),
        ];
        assert_remote_eq!(actual, [("a.txt", 5u64),]);
    }

    #[test]
    fn folder_entry_uses_dir_keyword() {
        let actual = vec![entry("sub", None, false), entry("a.txt", Some(5), false)];
        assert_remote_eq!(actual, [("sub", dir), ("a.txt", 5u64),]);
    }

    #[test]
    #[should_panic(expected = "assertion")]
    fn mismatch_panics() {
        let actual = vec![entry("a.txt", Some(5), false)];
        assert_remote_eq!(actual, [("a.txt", 6u64),]);
    }
}
