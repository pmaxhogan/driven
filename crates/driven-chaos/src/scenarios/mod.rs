//! The scenario catalogue (STRESS_HARNESS s3), one submodule per category.
//!
//! Each submodule holds the [`crate::scenario::Scenario`] impls for one s3
//! category. The Phase-1 interface declares the modules as empty stubs;
//! the Phase-2 implementer agents fill each with the concrete scenarios
//! for their category and register them via [`crate::registry::registry`].
//!
//! Category -> STRESS_HARNESS s3 section mapping:
//!
//! `storage` -> s3.1 storage and disk
//! `file_size` -> s3.2 file-size extremes
//! `permissions` -> s3.3 permissions and ACLs
//! `filenames` -> s3.4 pathological filenames
//! `ntfs` -> s3.5 NTFS / Win32 hazards
//! `mutation` -> s3.6 mutation patterns (soak)
//! `drive_side` -> s3.7 Drive-side fuckery
//! `concurrency` -> s3.8 concurrency edge

pub mod concurrency;
pub mod drive_side;
pub mod file_size;
pub mod filenames;
pub mod mutation;
pub mod ntfs;
pub mod permissions;
pub mod storage;
