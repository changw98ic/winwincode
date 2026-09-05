// SPDX-License-Identifier: Apache-2.0

//! Device identity persistence: first-boot generation, the canonical
//! fresh-per-launch `clientInstanceId`, and the server-issued enrollment
//! identity adopted after the exchange, across restarts.
//! Temporary-directory infrastructure mirrors
//! `crates/winwincode-storage/tests/sqlite.rs`.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_device_client::{
    DeviceIdentitySeed, DeviceStore, DeviceStoreErrorKind, IssuedEnrollment, adopt_enrollment,
    ensure_device_identity,
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

fn issued_enrollment(secret_byte: u8) -> IssuedEnrollment {
    fn hex(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(HEX[usize::from(byte >> 4)] as char);
            encoded.push(HEX[usize::from(byte & 0x0f)] as char);
        }
        encoded
    }
    let secret = [secret_byte; 32];
    IssuedEnrollment {
        client_node_id: "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1".to_owned(),
        public_client_id: "0123456789".to_owned(),
        credential_material: hex(&secret),
        credential_digest: format!("sha256:{:x}", Sha256::digest(secret)),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn first_boot_creates_identity_and_the_enrollment_completes_it_across_restarts() {
    let root = temporary_directory("identity-restart");
    let mut store = DeviceStore::open(&root).expect("device store should open");

    let first = ensure_device_identity(&mut store, &seed(), "2026-09-04T00:00:00.000Z")
        .expect("first boot should create the identity");
    assert!(first.identity().device_id().starts_with("dvc_"));
    assert_eq!(
        first.identity().client_node_id(),
        "",
        "no clientNodeId exists before the server assigns one"
    );
    assert_eq!(
        first.identity().public_client_id(),
        "",
        "publicClientId is never generated locally"
    );
    assert!(!first.identity().is_enrolled());
    let instance_shape = first.current_instance_id();
    assert!(
        instance_shape.starts_with("cix_") && instance_shape.len() == 30,
        "clientInstanceId must be a canonical cix_ identity: {instance_shape}"
    );
    assert!(
        instance_shape[4..]
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()),
        "clientInstanceId suffix must be Crockford Base32: {instance_shape}"
    );
    let local_digest = first.credential().digest();
    assert_eq!(local_digest.len(), 71);
    assert!(local_digest.starts_with("sha256:"));
    assert_eq!(first.credential().expose_secret().len(), 32);
    assert_eq!(first.credential().generation(), 1);

    // The enrollment adoption backfills the server-issued identity and
    // replaces the local credential with the issued Device Credential.
    let issued = issued_enrollment(0xab);
    adopt_enrollment(
        &mut store,
        first.identity().device_id(),
        &issued,
        "2026-09-04T00:00:30.000Z",
    )
    .expect("the enrollment adoption should complete the identity");

    let reloaded = ensure_device_identity(&mut store, &seed(), "2026-09-04T00:01:00.000Z")
        .expect("the adopted identity should reload");
    assert_eq!(reloaded.identity().client_node_id(), issued.client_node_id);
    assert_eq!(
        reloaded.identity().public_client_id(),
        issued.public_client_id
    );
    assert!(reloaded.identity().is_enrolled());
    let mut secret_bytes = [0_u8; 32];
    for (index, slot) in secret_bytes.iter_mut().enumerate() {
        let high = (issued.credential_material.as_bytes()[index * 2] as char)
            .to_digit(16)
            .expect("high nibble");
        let low = (issued.credential_material.as_bytes()[index * 2 + 1] as char)
            .to_digit(16)
            .expect("low nibble");
        *slot = u8::try_from(high << 4 | low).expect("byte");
    }
    assert_eq!(reloaded.credential().expose_secret(), &secret_bytes[..]);
    assert_eq!(
        reloaded.credential().digest(),
        issued.credential_digest,
        "the issued digest persists exactly the sha256 of the issued material"
    );
    assert_eq!(reloaded.credential().generation(), 2);
    assert_eq!(
        reloaded.credential().material_hex(),
        issued.credential_material
    );

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
        second.identity().client_node_id(),
        issued.client_node_id,
        "the server-assigned clientNodeId must be stable across restarts"
    );
    assert_eq!(
        second.identity().public_client_id(),
        issued.public_client_id,
        "the server-assigned publicClientId must be stable across restarts"
    );
    assert_eq!(
        second.credential().digest(),
        issued.credential_digest,
        "the issued credential must survive restarts"
    );
    assert_eq!(second.created_at(), "2026-09-04T00:00:00.000Z");
    assert_eq!(second.revision(), 4, "launch rotations plus the adoption");
    assert_ne!(
        second.current_instance_id(),
        first.current_instance_id(),
        "clientInstanceId must be a fresh value on every launch"
    );

    // A replayed adoption can never rotate the adopted identity.
    let error = adopt_enrollment(
        &mut store,
        second.identity().device_id(),
        &issued_enrollment(0xcd),
        "2026-09-04T01:00:30.000Z",
    )
    .expect_err("a second adoption must be refused");
    assert_eq!(error.kind(), DeviceStoreErrorKind::Conflict);

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
    assert_ne!(first.identity().device_id(), second.identity().device_id());
    assert_ne!(
        first.current_instance_id(),
        second.current_instance_id(),
        "every launch draws a fresh canonical instance id"
    );
    assert_ne!(
        first.credential().expose_secret(),
        second.credential().expose_secret()
    );
    assert_eq!(
        first.identity().client_node_id(),
        second.identity().client_node_id(),
        "both stay unenrolled until their own server issues identities"
    );

    // Two devices adopt distinct issued identities independently.
    let first_issued = IssuedEnrollment {
        client_node_id: "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1".to_owned(),
        ..issued_enrollment(0x01)
    };
    let second_issued = IssuedEnrollment {
        client_node_id: "cnd_B2B2B2B2B2B2B2B2B2B2B2B2B2".to_owned(),
        public_client_id: "9876543210".to_owned(),
        ..issued_enrollment(0x02)
    };
    adopt_enrollment(
        &mut first_store,
        first.identity().device_id(),
        &first_issued,
        "2026-09-04T00:00:30.000Z",
    )
    .expect("first adoption");
    adopt_enrollment(
        &mut second_store,
        second.identity().device_id(),
        &second_issued,
        "2026-09-04T00:00:30.000Z",
    )
    .expect("second adoption");
    assert_ne!(
        first_issued.client_node_id, second_issued.client_node_id,
        "the server assigns distinct canonical identities"
    );

    first_store.close().expect("first store close");
    second_store.close().expect("second store close");
    fs::remove_dir_all(first_root).expect("first directory release");
    fs::remove_dir_all(second_root).expect("second directory release");
}

#[test]
fn identity_seed_launch_stamp_and_issued_identity_are_validated() {
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

    // The issued identity is validated before anything persists.
    let cases = vec![
        IssuedEnrollment {
            client_node_id: "cnd_NOTCANONICAL".to_owned(),
            ..issued_enrollment(0x11)
        },
        IssuedEnrollment {
            public_client_id: "12".to_owned(),
            ..issued_enrollment(0x11)
        },
        IssuedEnrollment {
            credential_material: "zz".repeat(32),
            ..issued_enrollment(0x11)
        },
        IssuedEnrollment {
            credential_digest: "sha256:deadbeef".to_owned(),
            ..issued_enrollment(0x11)
        },
    ];
    for issued in cases {
        let error = adopt_enrollment(
            &mut store,
            record.identity().device_id(),
            &issued,
            "2026-09-04T00:00:30.000Z",
        )
        .expect_err("a malformed issuance must be rejected");
        assert_eq!(
            error.kind(),
            DeviceStoreErrorKind::InvalidInput,
            "{issued:?}"
        );
    }
    // Every rejection changed nothing: the identity is still unenrolled and
    // the local credential untouched.
    let unchanged = ensure_device_identity(&mut store, &seed(), "2026-09-04T00:01:00.000Z")
        .expect("identity reload");
    assert_eq!(unchanged.identity().client_node_id(), "");
    assert_eq!(unchanged.credential().generation(), 1);

    let foreign = adopt_enrollment(
        &mut store,
        "dvc_missing",
        &issued_enrollment(0x11),
        "2026-09-04T00:00:30.000Z",
    )
    .expect_err("an unknown device row must be refused");
    assert_eq!(foreign.kind(), DeviceStoreErrorKind::Adapter);

    store.close().expect("store should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}
