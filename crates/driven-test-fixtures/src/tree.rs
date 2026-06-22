//! Declarative temp-directory builder used across scanner / planner /
//! orchestrator tests.
//!
//! The [`tree!`] macro takes a JSON-shaped DSL of file / folder entries
//! and materialises them under a fresh
//! [`tempfile::TempDir`](::tempfile::TempDir). The returned `TempDir`
//! handle keeps the directory alive for the test's scope; once dropped,
//! the temp tree is cleaned up.
//!
//! ```ignore
//! use driven_test_fixtures::tree;
//!
//! let dir = tree! {
//!     "a.txt" => "hello",
//!     "sub" => {
//!         "b.bin" => &[0u8; 1024][..],
//!         "nested" => { "c.txt" => "deep" },
//!     },
//! };
//! assert!(dir.path().join("a.txt").exists());
//! assert!(dir.path().join("sub/nested/c.txt").exists());
//! ```
//!
//! Leaves accept any value implementing [`AsRef<[u8]>`] (so both `&str`
//! and `&[u8]` work). Directories nest with the brace syntax.
//!
//! Macro hygiene: every path inside the expansion is `$crate::`-prefixed
//! so consumers do not need to add `tempfile` to their `[dev-dependencies]`
//!   - the macro routes through `driven_test_fixtures::tempfile`,
//!     re-exported from this crate.

use std::path::Path;

#[doc(hidden)]
pub use tempfile;

/// Internal helper used by the [`tree!`] macro. Not part of the public
/// API; signature may change between minor versions.
///
/// Writes `contents` to `root.join(name)`, creating parent directories
/// as needed. Errors propagate as a panic - tests live and die by this.
#[doc(hidden)]
pub fn _write_file(root: &Path, name: &str, contents: &[u8]) {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("tree!: create_dir_all({}): {e}", parent.display()));
    }
    std::fs::write(&path, contents)
        .unwrap_or_else(|e| panic!("tree!: write({}): {e}", path.display()));
}

/// Internal helper used by the [`tree!`] macro to create an empty
/// directory entry (used when the brace-block is empty).
#[doc(hidden)]
pub fn _make_dir(root: &Path, name: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(&path)
        .unwrap_or_else(|e| panic!("tree!: create_dir_all({}): {e}", path.display()));
}

/// Builds a temp directory tree from a declarative DSL.
///
/// Returns a [`tempfile::TempDir`](::tempfile::TempDir). The returned
/// handle keeps the directory alive for the test's scope; let-binding
/// it (`let dir = tree! { ... }`) is the intended pattern, and
/// `dir.path()` yields the root.
///
/// Syntax:
/// - `"name" => "string"` writes a UTF-8 file.
/// - `"name" => &[u8][..]` (or any `AsRef<[u8]>`) writes a binary file.
/// - `"name" => { ... }` recurses into a subdirectory.
/// - `"name" => {}` creates an empty subdirectory.
///
/// See the module docs for a full example.
#[macro_export]
macro_rules! tree {
    ( $( $tt:tt )* ) => {{
        let __td = $crate::tree::tempfile::tempdir()
            .expect("tree!: failed to create tempdir");
        $crate::tree_impl!(@root __td.path(), "", $( $tt )* );
        __td
    }};
}

/// Internal recursive worker for [`tree!`]. Not part of the public API.
#[doc(hidden)]
#[macro_export]
macro_rules! tree_impl {
    // Empty body terminator.
    (@root $root:expr, $prefix:expr,) => {};
    (@root $root:expr, $prefix:expr) => {};

    // Directory entry with non-empty body, with trailing comma.
    (@root $root:expr, $prefix:expr, $name:literal => { $( $inner:tt )* }, $( $rest:tt )* ) => {{
        let __sub = if $prefix.is_empty() {
            ::std::string::String::from($name)
        } else {
            ::std::format!("{}/{}", $prefix, $name)
        };
        $crate::tree::_make_dir($root, &__sub);
        $crate::tree_impl!(@root $root, __sub.as_str(), $( $inner )* );
        $crate::tree_impl!(@root $root, $prefix, $( $rest )* );
    }};

    // Directory entry with non-empty body, no trailing comma (last entry).
    (@root $root:expr, $prefix:expr, $name:literal => { $( $inner:tt )* } ) => {{
        let __sub = if $prefix.is_empty() {
            ::std::string::String::from($name)
        } else {
            ::std::format!("{}/{}", $prefix, $name)
        };
        $crate::tree::_make_dir($root, &__sub);
        $crate::tree_impl!(@root $root, __sub.as_str(), $( $inner )* );
    }};

    // File entry, with trailing comma.
    (@root $root:expr, $prefix:expr, $name:literal => $contents:expr, $( $rest:tt )* ) => {{
        let __path = if $prefix.is_empty() {
            ::std::string::String::from($name)
        } else {
            ::std::format!("{}/{}", $prefix, $name)
        };
        let __contents = $contents;
        let __bytes: &[u8] = ::std::convert::AsRef::<[u8]>::as_ref(&__contents);
        $crate::tree::_write_file($root, &__path, __bytes);
        $crate::tree_impl!(@root $root, $prefix, $( $rest )* );
    }};

    // File entry, no trailing comma (last entry).
    (@root $root:expr, $prefix:expr, $name:literal => $contents:expr ) => {{
        let __path = if $prefix.is_empty() {
            ::std::string::String::from($name)
        } else {
            ::std::format!("{}/{}", $prefix, $name)
        };
        let __contents = $contents;
        let __bytes: &[u8] = ::std::convert::AsRef::<[u8]>::as_ref(&__contents);
        $crate::tree::_write_file($root, &__path, __bytes);
    }};
}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_single_file() {
        let dir = tree! {
            "a.txt" => "hello",
        };
        let body = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
        assert_eq!(body, "hello");
    }

    #[test]
    fn writes_nested_tree() {
        let dir = tree! {
            "a.txt" => "top",
            "sub" => {
                "b.bin" => &[1u8, 2, 3, 4][..],
                "nested" => {
                    "c.txt" => "deep",
                },
            },
        };
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "top"
        );
        let bin = std::fs::read(dir.path().join("sub/b.bin")).unwrap();
        assert_eq!(bin, vec![1u8, 2, 3, 4]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/nested/c.txt")).unwrap(),
            "deep"
        );
    }

    #[test]
    fn writes_empty_dir() {
        let dir = tree! {
            "empty_sub" => {},
        };
        assert!(dir.path().join("empty_sub").is_dir());
    }
}
