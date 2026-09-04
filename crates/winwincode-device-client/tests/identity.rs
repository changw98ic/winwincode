// SPDX-License-Identifier: Apache-2.0

//! Device identity persistence: first-boot generation plus the stable
//! `publicClientId` and fresh-per-launch `clientInstanceId` contract across
//! restarts. Temporary-directory infrastructure mirrors
//! `crates/winwincode-storage/tests/sqlite.rs`.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_device_client::{
    DeviceIdentitySeed, DeviceStore, DeviceStoreErrorKind, ensure_device_identity,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-device-client-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn seed() -> DeviceIdentitySeed {
    DeviceIdentitySeed {
        display_name: "Cheng's MacBook".to_owned(),
        platform: "darwin".to_owned(),
        architecture: "arm64".to_owned(),
        client_version: "0.1.0-alpha.1".to_owned(),
    }
}

#[test]
fn first_boot_creates_identity_and_restart_keeps_it_with_a_fresh_instance() {
    let root = temporary_directory("identity-restart");
    let mut store = DeviceStore::open(&root).expect("device store should open");

    let first = ensure_device_identity(&mut store, &seed(), "2026-09-04T00:00:00.000Z")
        .expect("first boot should create the identity");
    assert!(first.identity().device_id().starts_with("dvc_"));
    assert_eq!(
        first.identity().public_client_id().len(),
        10,
        "placeholder publicClientId is a 10-digit string"
    );
    assert!(
        first
            .identity()
            .public_client_id()
            .bytes()
            .all(|byte| byte.is_ascii_digit()),
        "placeholder publicClientId must be digits"
    );
    assert_eq!(first.revision(), 1);
    assert_eq!(first.created_at(), "2026-09-04T00:00:00.000Z");
    assert!(first.current_instance_id().starts_with("inst_"));
    let digest = first.credential().digest();
    assert_eq!(digest.len(), 71);
    assert!(digest.starts_with("sha256:"));
    assert_eq!(first.credential().expose_secret().len(), 32);
    assert_eq!(first.credential().generation(), 1);

    store.close().expect("store should close");
    let mut store = DeviceStore::open(&root).expect("device store should restart");
    let second = ensure_device_identity(&mut store, &seed(), "2026-09-04T01:00:00.000Z")
        .expect("restart should load the identity");

    assert_eq!(
        second.identity().device_id(),
        first.identity().device_id(),
        "device_id must be stable across restarts"
    );
    assert_eq!(
        second.identity().public_client_id(),
        first.identity().public_client_id(),
        "publicClientId must be stable across restarts"
    );
    assert_eq!(
        second.credential().digest(),
        digest,
        "the device credential must survive restarts"
    );
    assert_eq!(
        second.credential().expose_secret(),
        first.credential().expose_secret(),
        "the credential secret must survive restarts"
    );
    assert_eq!(second.credential().generation(), 1);
    assert_eq!(second.created_at(), "2026-09-04T00:00:00.000Z");
    assert_eq!(second.revision(), 2, "each launch rotates the revision");
    assert_ne!(
        second.current_instance_id(),
        first.current_instance_id(),
        "clientInstanceId must be a fresh value on every launch"
    );

    store.close().expect("restarted store should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn separate_devices_receive_distinct_stable_identities() {
    let first_root = temporary_directory("identity-device-one");
    let second_root = temporary_directory("identity-device-two");
    let mut first_store = DeviceStore::open(&first_root).expect("first device store");
    let mut second_store = DeviceStore::open(&second_root).expect("second device store");

    let first = ensure_device_identity(&mut first_store, &seed(), "2026-09-04T00:00:00.000Z")
        .expect("first device identity");
    let second = ensure_device_identity(&mut second_store, &seed(), "2026-09-04T00:00:00.000Z")
        .expect("second device identity");
    assert_ne!(
        first.identity().public_client_id(),
        second.identity().public_client_id()
    );
    assert_ne!(first.identity().device_id(), second.identity().device_id());
    assert_ne!(
        first.credential().expose_secret(),
        second.credential().expose_secret()
    );

    first_store.close().expect("first store close");
    second_store.close().expect("second store close");
    fs::remove_dir_all(first_root).expect("first directory release");
    fs::remove_dir_all(second_root).expect("second directory release");
}

#[test]
fn identity_seed_and_launch_stamp_are_validated() {
    let root = temporary_directory("identity-validation");
    let mut store = DeviceStore::open(&root).expect("device store should open");

    let empty_name = DeviceIdentitySeed {
        display_name: String::new(),
        ..seed()
    };
    let error = ensure_device_identity(&mut store, &empty_name, "2026-09-04T00:00:00.000Z")
        .expect_err("an empty display name must be rejected");
    assert_eq!(error.kind(), DeviceStoreErrorKind::InvalidInput);

    let error = ensure_device_identity(&mut store, &seed(), "")
        .expect_err("an empty launch stamp must be rejected");
    assert_eq!(error.kind(), DeviceStoreErrorKind::InvalidInput);

    let record = ensure_device_identity(&mut store, &seed(), "2026-09-04T00:00:00.000Z")
        .expect("a valid call must still create the identity after rejected calls");
    assert_eq!(record.revision(), 1);

    store.close().expect("store should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}
