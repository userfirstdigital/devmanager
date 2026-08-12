//! Real, process-local executable identities shared by provider unit tests.
//!
//! `ProviderExecutable::new` is intentionally an attestation boundary: it
//! cannot manufacture an identity for a path that does not exist or whose
//! digest is not the digest of the file.  Tests that used fake `C:\\bin` or
//! `/fixture` paths therefore bypassed the contract only accidentally.  Keep
//! the test fixtures real by copying the current test harness into a private
//! temporary directory and retaining that directory for the test process.

use super::capabilities::ProviderExecutable;
use std::sync::OnceLock;

#[derive(Clone, Copy)]
pub(crate) enum TestExecutableSlot {
    Primary,
    Alternate,
    Replacement,
}

struct ProviderTestExecutables {
    // The identities retain open handles into these files, so the root must
    // live for the entire test process.
    _root: tempfile::TempDir,
    primary: ProviderExecutable,
    alternate: ProviderExecutable,
    replacement: ProviderExecutable,
}

static EXECUTABLES: OnceLock<ProviderTestExecutables> = OnceLock::new();

pub(crate) fn executable(slot: TestExecutableSlot) -> ProviderExecutable {
    let fixtures = EXECUTABLES.get_or_init(|| {
        let source = std::env::current_exe().expect("current test executable");
        let root = tempfile::Builder::new()
            .prefix("devmanager-provider-identities-")
            .tempdir()
            .expect("provider identity fixture directory");

        let primary_path = root.path().join("provider-primary.exe");
        let alternate_path = root.path().join("provider-alternate.exe");
        let replacement_path = root.path().join("provider-replacement.exe");
        for path in [&primary_path, &alternate_path, &replacement_path] {
            std::fs::copy(&source, path).expect("copy current test executable fixture");
        }

        ProviderTestExecutables {
            primary: ProviderExecutable::from_path(&primary_path)
                .expect("primary provider executable identity"),
            alternate: ProviderExecutable::from_path(&alternate_path)
                .expect("alternate provider executable identity"),
            replacement: ProviderExecutable::from_path(&replacement_path)
                .expect("replacement provider executable identity"),
            _root: root,
        }
    });

    match slot {
        TestExecutableSlot::Primary => fixtures.primary.clone(),
        TestExecutableSlot::Alternate => fixtures.alternate.clone(),
        TestExecutableSlot::Replacement => fixtures.replacement.clone(),
    }
}
