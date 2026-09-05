// SPDX-License-Identifier: Apache-2.0

//! Coverage for the `wwc device` local display surface (plan 11.1, 16.8):
//! status is secret-free, `refresh-code` reveals the plaintext exactly once
//! and publishes only its digest, and lock/unlock persist the durable policy.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_cli::{
    DeviceAdminError, DeviceAdminOutcome, SystemLocalLauncher, WwcCliExit, device_status,
    refresh_device_connect_code, run_cli, set_device_lock,
};
use winwincode_device_client::{
    DeviceStore, IssuedEnrollment, adopt_enrollment, ensure_device_identity, load_device_identity,
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

const ASSIGNED_NODE: &str = "cnd_B1B1B1B1B1B1B1B1B1B1B1B1B1";
const ASSIGNED_PUBLIC_CLIENT_ID: &str = "9876543210";
const ISSUED_SECRET: [u8; 32] = [0xbe; 32];
const STAMP: &str = "2026-09-04T00:00:00.000Z";

struct Fixture {
    data_directory: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let data_directory = std::env::temp_dir().join(format!(
            "winwincode-cli-device-tests-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&data_directory);
        Self { data_directory }
    }

    fn data_dir(&self) -> &str {
        self.data_directory.to_str().expect("UTF-8 fixture path")
    }

    fn cli(&self, arguments: &[&str]) -> WwcCliExit {
        let mut full = vec!["device"];
        full.extend(arguments.iter().copied());
        full.extend(["--data-dir", self.data_dir()]);
        run_cli(
            &full
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
            &SystemLocalLauncher::new(std::env::temp_dir()),
        )
    }

    /// Creates the device identity and adopts the enrollment, the state the
    /// Device Client reaches after its first accepted exchange.
    fn enroll(&self) {
        let mut store = DeviceStore::open(&self.data_directory).expect("device store opens");
        ensure_device_identity(&mut store, &seed(), STAMP).expect("identity loads");
        let device_id = load_device_identity(&store)
            .expect("identity read")
            .expect("fresh identity")
            .identity()
            .device_id()
            .to_owned();
        let mut secret_hex = String::with_capacity(ISSUED_SECRET.len() * 2);
        for byte in ISSUED_SECRET {
            use std::fmt::Write as _;
            let _ = write!(secret_hex, "{byte:02x}");
        }
        adopt_enrollment(
            &mut store,
            &device_id,
            &IssuedEnrollment {
                client_node_id: ASSIGNED_NODE.to_owned(),
                public_client_id: ASSIGNED_PUBLIC_CLIENT_ID.to_owned(),
                credential_material: secret_hex,
                credential_digest: format!("sha256:{:x}", Sha256::digest(ISSUED_SECRET)),
            },
            STAMP,
        )
        .expect("enrollment adoption");
        store.close().expect("store closes");
    }

    /// The pending durable outbox rows (the publication frame's home).
    fn pending_kinds(&self) -> Vec<String> {
        let store = DeviceStore::open(&self.data_directory).expect("device store opens");
        let kinds = store
            .pending_outbox_envelopes()
            .expect("pending rows")
            .into_iter()
            .map(|entry| entry.kind)
            .collect();
        store.close().expect("store closes");
        kinds
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.data_directory);
    }
}

fn seed() -> winwincode_device_client::DeviceIdentitySeed {
    winwincode_device_client::DeviceIdentitySeed {
        display_name: "Cheng's MacBook".to_owned(),
        platform: "darwin".to_owned(),
        architecture: "arm64".to_owned(),
        client_version: "0.1.0-alpha.1".to_owned(),
    }
}

#[test]
fn status_reports_not_initialized_before_the_first_device_boot() {
    let fixture = Fixture::new();
    let result = device_status(&fixture.data_directory);
    assert_eq!(result.unwrap_err(), DeviceAdminError::NotInitialized);

    // The CLI surface maps the same state onto the action-required exit.
    let exit = fixture.cli(&["status"]);
    assert_eq!(exit.code, 3);
    assert!(exit.stdout.contains("尚未初始化"), "{}", exit.stdout);
}

#[test]
fn refresh_code_requires_an_adopted_enrollment() {
    let fixture = Fixture::new();
    {
        let mut store = DeviceStore::open(&fixture.data_directory).expect("device store opens");
        ensure_device_identity(&mut store, &seed(), STAMP).expect("identity loads");
        store.close().expect("store closes");
    }
    let result = refresh_device_connect_code(&fixture.data_directory);
    assert_eq!(result.unwrap_err(), DeviceAdminError::NotEnrolled);

    let exit = fixture.cli(&["refresh-code"]);
    assert_eq!(exit.code, 3);
    assert!(exit.stdout.contains("enrollment"), "{}", exit.stdout);
}

#[test]
fn refresh_code_reveals_the_plaintext_once_and_publishes_only_the_digest() {
    let fixture = Fixture::new();
    fixture.enroll();

    let outcome = refresh_device_connect_code(&fixture.data_directory).expect("refresh");
    let DeviceAdminOutcome::CodeRefreshed {
        code,
        connect_code,
        valid_seconds,
    } = &outcome
    else {
        panic!("expected CodeRefreshed: {outcome:?}");
    };
    assert_eq!(*valid_seconds, 120);
    assert_eq!(connect_code.len(), 8);
    assert!(connect_code.bytes().all(|byte| byte.is_ascii_digit()));
    assert_eq!(code.state, "active");
    assert_eq!(code.generation, 1);
    assert_eq!(
        code.remaining_seconds.map(|seconds| seconds > 0),
        Some(true)
    );

    // The durable outbox carries the publication frame: digest only.
    assert!(
        fixture
            .pending_kinds()
            .iter()
            .any(|kind| kind == "client.connect_code.published"),
        "the publication frame must be enqueued durably"
    );
    let store = DeviceStore::open(&fixture.data_directory).expect("store reopens");
    for entry in store.pending_outbox_envelopes().expect("rows") {
        let blob = String::from_utf8_lossy(&entry.payload);
        assert!(
            !blob.contains(connect_code.as_str()),
            "no outbox row may carry the plaintext: {}",
            entry.message_id
        );
        if entry.kind == "client.connect_code.published" {
            let expected_digest = format!("sha256:{:x}", Sha256::digest(connect_code.as_bytes()));
            assert!(
                blob.contains(&expected_digest),
                "the published frame carries the sha256 digest: {blob}"
            );
        }
    }
    store.close().expect("store closes");

    // A later status never reveals the plaintext, in any rendering.
    let status = device_status(&fixture.data_directory).expect("status");
    let rendered = serde_json::to_string(&status).expect("status json");
    assert!(!rendered.contains(connect_code.as_str()));
    let exit = fixture.cli(&["status", "--json"]);
    assert_eq!(exit.code, 0);
    assert!(!exit.stdout.contains(connect_code.as_str()));

    // A refresh supersedes the previous generation.
    let second = refresh_device_connect_code(&fixture.data_directory).expect("second refresh");
    let DeviceAdminOutcome::CodeRefreshed { code, .. } = &second else {
        panic!("expected CodeRefreshed: {second:?}");
    };
    assert_eq!(code.generation, 2);
}

#[test]
fn lock_and_unlock_persist_the_policy_for_status() {
    let fixture = Fixture::new();
    fixture.enroll();

    let locked = set_device_lock(&fixture.data_directory, true).expect("lock");
    let DeviceAdminOutcome::PolicyUpdated {
        accepting_connections,
        lock_state,
    } = &locked
    else {
        panic!("expected PolicyUpdated: {locked:?}");
    };
    assert!(!accepting_connections);
    assert_eq!(lock_state, "locked");

    let DeviceAdminOutcome::Status { device } =
        device_status(&fixture.data_directory).expect("status")
    else {
        panic!("expected Status");
    };
    assert!(!device.accepting_connections);
    assert_eq!(device.lock_state, "locked");

    let unlocked = set_device_lock(&fixture.data_directory, false).expect("unlock");
    let DeviceAdminOutcome::PolicyUpdated {
        accepting_connections,
        lock_state,
    } = &unlocked
    else {
        panic!("expected PolicyUpdated: {unlocked:?}");
    };
    assert!(accepting_connections);
    assert_eq!(lock_state, "unlocked");

    let exit = fixture.cli(&["lock", "--json"]);
    assert_eq!(exit.code, 0);
    let value: serde_json::Value = serde_json::from_str(&exit.stdout).expect("pretty JSON outcome");
    assert_eq!(value["status"], "policy-updated");
    assert_eq!(value["lockState"], "locked");
    assert_eq!(value["acceptingConnections"], false);
}

#[test]
fn lock_requires_an_existing_device_identity() {
    let fixture = Fixture::new();
    // A lock on a never-booted directory must fail loudly instead of
    // silently locking a nonexistent device.
    assert_eq!(
        set_device_lock(&fixture.data_directory, true).unwrap_err(),
        DeviceAdminError::NotInitialized
    );
    {
        let mut store = DeviceStore::open(&fixture.data_directory).expect("device store opens");
        ensure_device_identity(&mut store, &seed(), STAMP).expect("identity loads");
        store.close().expect("store closes");
    }
    let locked = set_device_lock(&fixture.data_directory, true).expect("lock");
    let DeviceAdminOutcome::PolicyUpdated { lock_state, .. } = &locked else {
        panic!("expected PolicyUpdated: {locked:?}");
    };
    assert_eq!(lock_state, "locked");
}

#[test]
fn unknown_device_actions_are_usage_errors() {
    let fixture = Fixture::new();
    let exit = fixture.cli(&["bogus"]);
    assert_eq!(exit.code, 2);
    assert!(exit.stderr.contains("未知 device 命令"), "{}", exit.stderr);

    // Missing --data-dir is a usage error, not a service failure.
    let mut full = vec!["device".to_owned(), "status".to_owned()];
    let exit = run_cli(&full, &SystemLocalLauncher::new(std::env::temp_dir()));
    full.clear();
    assert_eq!(exit.code, 2);
    assert!(exit.stderr.contains("--data-dir"), "{}", exit.stderr);
}

#[test]
fn help_names_the_device_commands() {
    let help = winwincode_cli::render_help();
    assert!(help.contains("wwc device status"));
    assert!(help.contains("refresh-code"));
}
