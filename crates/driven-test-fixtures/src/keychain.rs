//! Process-wide keychain isolation for tests.
//!
//! Driven keeps every credential in the OS keychain: the account master key
//! (`dev.maxhogan.driven`, `driven_crypto::Keystore`), the Google refresh token
//! (`driven.google.refresh_token`), the per-account BYO OAuth client creds
//! (`driven.google.client_creds`, both in
//! [`driven_drive::google::token_store`]) and the S3 key pair
//! (`driven.s3.credentials`). A test that exercises any of those paths must
//! NOT reach the developer's real login keychain: on macOS every
//! `cargo test` rebuild produces a binary with a new identity, so the OS treats
//! each rebuild as a new app asking for access and raises a modal
//! "allow ... to access ..." prompt - one per binary, forever, and the run
//! blocks on it. On headless CI there is no keychain to reach at all.
//!
//! [`isolated`] is the ONE way to make a test keychain-safe. Call it at the top
//! of any test that can reach a keychain entry, directly or transitively:
//!
//! ```no_run
//! # fn main() {
//! let Some(_guard) = driven_test_fixtures::keychain::isolated() else {
//!     return; // the mock could not be installed; skip rather than write for real
//! };
//! // ... the test body may now touch the keychain paths freely.
//! # }
//! ```
//!
//! # Ordering is load-bearing
//!
//! `keyring` 4.x's [`keyring::Entry::new`] installs the PLATFORM-NATIVE store
//! as the process default on its FIRST call, overwriting whatever default is
//! already installed (see `keyring-4.1.5/src/v1.rs`, the `SET_CREDENTIAL_STORE`
//! latch). So the obvious recipe - "install `keyring_core`'s mock as the
//! default store" - is silently undone by the first real [`keyring::Entry`],
//! and every "mock" write lands in the OS keychain instead. That is not
//! hypothetical: it is exactly how `acct-with-token` and `acct-byo` (test-only
//! account ids) ended up as real items in a maintainer's login keychain.
//!
//! So [`isolated`] burns that latch FIRST with a throwaway entry (constructing
//! an entry performs no credential I/O in any of the platform stores), THEN
//! installs the mock, and only then reports success.
//!
//! # Proof, not hope
//!
//! A defeated mock looks exactly like a working one until a dialog appears, so
//! [`isolated`] proves the mock is the effective store before any test is
//! allowed to store a secret, in two steps:
//!
//! 1. An I/O-FREE precondition: the installed default store must advertise
//!    [`CredentialPersistence::ProcessOnly`], which by definition means its
//!    credentials cannot outlive the process - no disk, no OS keychain. This
//!    check writes nothing, so a defeated mock is caught BEFORE anything could
//!    leak into a real keychain.
//! 2. A functional round trip of a sentinel secret through the same `keyring`
//!    facade production uses. Step 1 has already established that this cannot
//!    touch a real store, so the round trip only confirms the mock works.
//!
//! If either step fails, [`isolated`] returns `None` and the caller MUST skip.

use std::sync::{Mutex, MutexGuard, OnceLock};

use keyring_core::CredentialPersistence;

/// Service namespace for the throwaway entry that burns `keyring`'s one-shot
/// platform-store latch. Never carries a secret: only [`keyring::Entry::new`]
/// is called on it, which performs no credential I/O.
const LATCH_SERVICE: &str = "driven.test.keyring-latch";

/// Service namespace for the sentinel round trip that proves the mock works.
const SENTINEL_SERVICE: &str = "driven.test.keyring-sentinel";

/// The sentinel value round-tripped through the mock.
const SENTINEL_VALUE: &str = "mock-is-active";

/// Exclusive access to the process-global keychain, held for the duration of a
/// test. See [`isolated`].
pub type KeychainGuard = MutexGuard<'static, ()>;

/// Serializes the tests that share the process-global default store.
fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Installs `keyring_core`'s IN-MEMORY mock as the process-global default
/// credential store (once per process) and returns a guard serializing the
/// tests that use it.
///
/// Returns `None` when the mock could NOT be made the effective store. The
/// caller MUST skip in that case - the alternative is writing test secrets into
/// a real OS keychain, which on macOS also blocks the run on a modal permission
/// prompt.
///
/// The installation is permanent for the process (the default store is global
/// and this never uninstalls it), so once ANY test in a binary has called this,
/// the rest of that binary is isolated too. Do not rely on that: call it from
/// every test that can reach the keychain, because test order is not
/// guaranteed.
#[must_use]
pub fn isolated() -> Option<KeychainGuard> {
    let guard = lock().lock().unwrap_or_else(|e| e.into_inner());
    if !install() {
        eprintln!(
            "skipping the keychain test: the in-memory keyring store is not the effective \
             default, and this test will not write to a real OS keychain"
        );
        return None;
    }
    Some(guard)
}

/// Whether the in-memory mock is the effective default store for this process.
///
/// Installs it if it is not already installed, so this doubles as the assertion
/// a crate can make in a guard test: `assert!(driven_test_fixtures::keychain::
/// is_isolated())` fails loudly if the mechanism ever stops working for that
/// crate's test binary.
///
/// Deliberately does NOT take the [`lock`]: that lock serializes access to the
/// process-global *entries*, which this never touches, and taking it would make
/// a guard test stall behind whichever test currently holds a
/// [`KeychainGuard`]. [`install`] is idempotent and internally synchronized.
#[must_use]
pub fn is_isolated() -> bool {
    install()
}

/// Installs + proves the mock exactly once per process; later calls return the
/// cached verdict. Internally synchronized (`OnceLock`), so it is safe to call
/// with or without [`lock`] held.
fn install() -> bool {
    static ACTIVE: OnceLock<bool> = OnceLock::new();
    *ACTIVE.get_or_init(install_once)
}

/// The one-shot install + proof. See the module docs for why each step exists.
fn install_once() -> bool {
    // 1. Burn `keyring`'s one-shot platform-store latch, so it cannot overwrite
    //    the mock later. Constructing an entry performs no credential I/O, so
    //    this raises no prompt and creates nothing. Its error on a headless box
    //    (no Secret Service) is expected and ignored - the latch is set either
    //    way, which is all this call is for.
    let _ = keyring::Entry::new(LATCH_SERVICE, "latch");

    // 2. Now the mock sticks.
    match keyring_core::mock::Store::new() {
        Ok(store) => keyring_core::set_default_store(store),
        Err(_) => return false,
    }

    // 3. I/O-free precondition: the effective store must be one whose
    //    credentials cannot outlive the process. A real OS keychain reports
    //    `UntilDelete` / `UntilReboot`, so this catches a defeated mock WITHOUT
    //    writing anything anywhere.
    let Some(store) = keyring_core::get_default_store() else {
        return false;
    };
    if !matches!(store.persistence(), CredentialPersistence::ProcessOnly) {
        return false;
    }

    // 4. Functional proof, through the SAME `keyring` facade production uses.
    //    Step 3 already established this cannot reach a real store.
    let Ok(sentinel) = keyring::Entry::new(SENTINEL_SERVICE, "probe") else {
        return false;
    };
    if sentinel.set_password(SENTINEL_VALUE).is_err() {
        return false;
    }
    let proved = sentinel.get_password().ok().as_deref() == Some(SENTINEL_VALUE);
    let _ = sentinel.delete_credential();
    proved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_installs_a_process_only_store() {
        let Some(_g) = isolated() else {
            return;
        };
        let store = keyring_core::get_default_store().expect("a default store is installed");
        assert!(
            matches!(store.persistence(), CredentialPersistence::ProcessOnly),
            "the effective store must not be able to persist beyond the process (vendor: {})",
            store.vendor()
        );
    }

    #[test]
    fn secrets_round_trip_through_the_mock_and_stay_in_memory() {
        let Some(_g) = isolated() else {
            return;
        };
        let entry = keyring::Entry::new("driven.test.round-trip", "user").expect("entry");
        entry.set_password("s3cret").expect("store");
        assert_eq!(entry.get_password().expect("load"), "s3cret");
        entry.delete_credential().expect("delete");
        assert!(
            entry.get_password().is_err(),
            "a deleted credential must not read back"
        );
    }

    #[test]
    fn isolation_is_idempotent_and_reported() {
        let Some(_g) = isolated() else {
            return;
        };
        drop(_g);
        assert!(is_isolated(), "the verdict is cached and stays true");
    }
}
