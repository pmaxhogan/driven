//! Mutation-pattern (soak) scenarios (STRESS_HARNESS s3.6).
//!
//! `frequent-edits`, `frequent-lock-unlock`, `constantly-locked-db`,
//! `truncate-and-rewrite`, `append-only-log`, `rename-storm`,
//! `editor-tilde-dance`, `replace-via-atomic-rename`. Each drives an
//! [`crate::mutator::FsMutation`] loop alongside Driven sync.
//!
//! Phase-2 fills these and returns them from [`scenarios`].

use crate::scenario::Scenario;

/// Every mutation-pattern soak scenario (STRESS_HARNESS s3.6). Empty until Phase-2.
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    Vec::new()
}
