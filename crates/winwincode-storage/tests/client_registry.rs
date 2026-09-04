// SPDX-License-Identifier: Apache-2.0

//! Durable `ClientNode` registry and exchange cursor contract tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::Instant;
use winwincode_storage::{
    ClientExchangeCursors, ClientLockState, ClientNodeRegistration, ClientPresenceState,
    ClientRegistryErrorKind, SqliteStorage,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-client-registry-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

fn node_id(seed: u64) -> String {
    format!("cnd_{seed:026}")
}

fn instance_id(seed: u64) -> String {
    format!("cix_{seed:026}")
}

fn digest(seed: u64) -> String {
    format!("sha256:{seed:064x}")
}

const T0: &str = "2026-01-01T00:00:00.000Z";
const T1: &str = "2026-01-01T00:01:00.000Z";
const T2: &str = "2026-01-01T00:02:00.000Z";
const T3: &str = "2026-01-01T00:03:00.000Z";

fn registration(seed: u64) -> ClientNodeRegistration {
    ClientNodeRegistration::try_new(
        node_id(seed),
        format!("{seed:09}"),
        format!("Device {seed}"),
        "aarch64-apple-darwin",
        "aarch64",
        "1.2.3",
        Some(digest(seed)),
        Some(instance_id(seed)),
        4,
    )
    .expect("registration")
}

fn open(path: PathBuf) -> SqliteStorage {
    SqliteStorage::open(path).expect("storage")
}

#[test]
fn registers_new_client_node_in_pending_enrollment_with_zeroed_cursors() {
    let mut storage = open(temporary_directory("fresh"));
    let mut registry = storage.client_node_registry().expect("registry");
    let receipt = registry
        .register(&registration(1), 0, &instant(T0))
        .expect("register");
    assert!(receipt.enrolled);
    let record = receipt.record;
    assert_eq!(record.client_node_id, node_id(1));
    assert_eq!(record.public_client_id, "000000001");
    assert_eq!(record.display_name, "Device 1");
    assert_eq!(record.platform, "aarch64-apple-darwin");
    assert_eq!(record.architecture, "aarch64");
    assert_eq!(record.client_version, "1.2.3");
    assert_eq!(
        record.device_credential_digest.as_deref(),
        Some(digest(1).as_str())
    );
    assert_eq!(
        record.current_instance_id.as_deref(),
        Some(instance_id(1).as_str())
    );
    assert_eq!(
        record.presence_state,
        ClientPresenceState::PendingEnrollment
    );
    assert!(record.accepting_connections);
    assert_eq!(record.lock_state, ClientLockState::Unlocked);
    assert_eq!(record.max_concurrent_worker_sessions, 4);
    assert_eq!(record.reported_running_worker_sessions, 0);
    assert!(record.last_heartbeat_at.is_none());
    assert_eq!(record.created_at, instant(T0));
    assert_eq!(record.revision, 1);
    assert_eq!(
        registry
            .exchange_cursors(&record.client_node_id)
            .expect("cursors"),
        Some(ClientExchangeCursors {
            client_to_server_ack_sequence: 0,
            server_to_client_ack_sequence: 0,
        })
    );
}

#[test]
fn refreshes_device_reported_projection_on_re_registration() {
    let mut storage = open(temporary_directory("refresh"));
    let mut registry = storage.client_node_registry().expect("registry");
    let seed_registration = registration(2);
    let first = registry
        .register(&seed_registration, 0, &instant(T0))
        .expect("first register");
    assert!(first.enrolled);
    let updated = ClientNodeRegistration::try_new(
        seed_registration.client_node_id().to_owned(),
        seed_registration.public_client_id().to_owned(),
        "Renamed Device",
        "x86_64-unknown-linux-gnu",
        "x86_64",
        "2.0.0",
        None,
        Some("cix_99999999999999999999999999".to_owned()),
        8,
    )
    .expect("updated registration");
    let second = registry
        .register(&updated, first.record.revision, &instant(T1))
        .expect("second register");
    assert!(!second.enrolled);
    assert_eq!(second.record.display_name, "Renamed Device");
    assert_eq!(second.record.platform, "x86_64-unknown-linux-gnu");
    assert_eq!(second.record.architecture, "x86_64");
    assert_eq!(second.record.client_version, "2.0.0");
    assert_eq!(second.record.device_credential_digest, None);
    assert_eq!(
        second.record.current_instance_id.as_deref(),
        Some("cix_99999999999999999999999999")
    );
    assert_eq!(second.record.max_concurrent_worker_sessions, 8);
    // Presence, lock, and connection facts are not part of registration.
    assert_eq!(
        second.record.presence_state,
        ClientPresenceState::PendingEnrollment
    );
    assert_eq!(second.record.lock_state, ClientLockState::Unlocked);
    assert!(second.record.accepting_connections);
    assert_eq!(second.record.revision, 2);
}

#[test]
fn rejects_public_client_id_rebinding_and_stale_revisions() {
    let mut storage = open(temporary_directory("conflict"));
    let mut registry = storage.client_node_registry().expect("registry");
    let first = registration(3);
    registry
        .register(&first, 0, &instant(T0))
        .expect("first register");
    let rebound = ClientNodeRegistration::try_new(
        node_id(4),
        first.public_client_id().to_owned(),
        "Other Device",
        "aarch64-apple-darwin",
        "aarch64",
        "1.2.3",
        None,
        None,
        2,
    )
    .expect("rebound registration");
    let conflict = registry
        .register(&rebound, 0, &instant(T0))
        .expect_err("public client id rebinding must conflict");
    assert_eq!(conflict.kind(), ClientRegistryErrorKind::IdentityConflict);

    let stale = registry
        .register(&first, 99, &instant(T0))
        .expect_err("stale expected revision must conflict");
    assert_eq!(stale.kind(), ClientRegistryErrorKind::RevisionConflict);
}

#[test]
fn enforces_frozen_presence_transitions() {
    let mut storage = open(temporary_directory("transitions"));
    let mut registry = storage.client_node_registry().expect("registry");
    let seed = registration(5);
    let registered = registry
        .register(&seed, 0, &instant(T0))
        .expect("register")
        .record;
    let node = registered.client_node_id.clone();

    // pending_enrollment -> offline is not legal.
    let illegal = registry
        .update_presence(&node, ClientPresenceState::Offline, 1)
        .expect_err("pending_enrollment -> offline must be illegal");
    assert_eq!(illegal.kind(), ClientRegistryErrorKind::PresenceTransition);

    // pending_enrollment -> online on enrollment acceptance.
    let online = registry
        .update_presence(&node, ClientPresenceState::Online, 1)
        .expect("pending_enrollment -> online");
    assert_eq!(online.presence_state, ClientPresenceState::Online);
    assert_eq!(online.revision, 2);

    // online -> degraded while local reconciliation is unfinished.
    let degraded = registry
        .update_presence(&node, ClientPresenceState::Degraded, 2)
        .expect("online -> degraded");
    assert_eq!(degraded.presence_state, ClientPresenceState::Degraded);

    // degraded -> online once the reconcile result is accepted.
    let online_again = registry
        .update_presence(&node, ClientPresenceState::Online, 3)
        .expect("degraded -> online");
    assert_eq!(online_again.presence_state, ClientPresenceState::Online);

    // online -> locked and locked -> online.
    let locked = registry
        .update_presence(&node, ClientPresenceState::Locked, 4)
        .expect("online -> locked");
    assert_eq!(locked.presence_state, ClientPresenceState::Locked);
    let unlocked = registry
        .update_presence(&node, ClientPresenceState::Online, 5)
        .expect("locked -> online");
    assert_eq!(unlocked.presence_state, ClientPresenceState::Online);

    // online -> offline -> revoked (terminal).
    let offline = registry
        .update_presence(&node, ClientPresenceState::Offline, 6)
        .expect("online -> offline");
    assert_eq!(offline.presence_state, ClientPresenceState::Offline);
    let revoked = registry
        .update_presence(&node, ClientPresenceState::Revoked, 7)
        .expect("offline -> revoked");
    assert_eq!(revoked.presence_state, ClientPresenceState::Revoked);

    // revoked is terminal.
    let revive = registry
        .update_presence(&node, ClientPresenceState::Online, 8)
        .expect_err("revoked is terminal");
    assert_eq!(revive.kind(), ClientRegistryErrorKind::PresenceTransition);
}

#[test]
fn update_presence_accepts_same_state_replay_and_rejects_stale_revision() {
    let mut storage = open(temporary_directory("replay"));
    let mut registry = storage.client_node_registry().expect("registry");
    let registered = registry
        .register(&registration(6), 0, &instant(T0))
        .expect("register")
        .record;
    let replay = registry
        .update_presence(
            &registered.client_node_id,
            ClientPresenceState::PendingEnrollment,
            1,
        )
        .expect("same state replay");
    assert_eq!(replay.revision, 1);
    let stale = registry
        .update_presence(&registered.client_node_id, ClientPresenceState::Online, 99)
        .expect_err("stale revision must conflict");
    assert_eq!(stale.kind(), ClientRegistryErrorKind::RevisionConflict);
    let unknown = registry
        .update_presence(&node_id(7), ClientPresenceState::Online, 0)
        .expect_err("unknown client node must fail");
    assert_eq!(unknown.kind(), ClientRegistryErrorKind::UnknownClientNode);
}

#[test]
fn heartbeat_reconnects_offline_and_rejects_unenrolled_devices() {
    let mut storage = open(temporary_directory("heartbeat"));
    let mut registry = storage.client_node_registry().expect("registry");
    let registered = registry
        .register(&registration(8), 0, &instant(T0))
        .expect("register")
        .record;
    let node = registered.client_node_id.clone();

    // Enrollment acceptance has not happened yet.
    let early = registry
        .heartbeat(&node, 0, &instant(T1), 1)
        .expect_err("pending_enrollment heartbeat must be rejected");
    assert_eq!(early.kind(), ClientRegistryErrorKind::PresenceTransition);

    registry
        .update_presence(&node, ClientPresenceState::Online, 1)
        .expect("accept enrollment");
    let beat = registry
        .heartbeat(&node, 2, &instant(T1), 2)
        .expect("online heartbeat");
    assert_eq!(beat.presence_state, ClientPresenceState::Online);
    assert_eq!(beat.reported_running_worker_sessions, 2);
    assert_eq!(beat.last_heartbeat_at, Some(instant(T1)));
    assert_eq!(beat.revision, 3);

    registry.sweep_offline(&instant(T2)).expect("sweep offline");
    let reconnected = registry
        .heartbeat(&node, 1, &instant(T2), 4)
        .expect("offline heartbeat reconnects");
    assert_eq!(reconnected.presence_state, ClientPresenceState::Online);
    assert_eq!(reconnected.reported_running_worker_sessions, 1);
}

#[test]
fn sweep_offline_projects_only_stale_online_and_degraded_devices() {
    let mut storage = open(temporary_directory("sweep"));
    let mut registry = storage.client_node_registry().expect("registry");

    let online = registry
        .register(&registration(9), 0, &instant(T0))
        .expect("register")
        .record;
    registry
        .update_presence(&online.client_node_id, ClientPresenceState::Online, 1)
        .expect("online");
    registry
        .heartbeat(&online.client_node_id, 0, &instant(T2), 2)
        .expect("heartbeat");

    let degraded = registry
        .register(&registration(10), 0, &instant(T0))
        .expect("register")
        .record;
    registry
        .update_presence(&degraded.client_node_id, ClientPresenceState::Online, 1)
        .expect("online");
    registry
        .heartbeat(&degraded.client_node_id, 0, &instant(T2), 2)
        .expect("heartbeat");
    registry
        .update_presence(&degraded.client_node_id, ClientPresenceState::Degraded, 3)
        .expect("degraded");

    let locked = registry
        .register(&registration(11), 0, &instant(T0))
        .expect("register")
        .record;
    registry
        .update_presence(&locked.client_node_id, ClientPresenceState::Online, 1)
        .expect("online");
    registry
        .heartbeat(&locked.client_node_id, 0, &instant(T2), 2)
        .expect("heartbeat");
    registry
        .update_presence(&locked.client_node_id, ClientPresenceState::Locked, 3)
        .expect("locked");

    let pending = registry
        .register(&registration(12), 0, &instant(T0))
        .expect("register")
        .record;

    // Nothing expires before the cutoff.
    let early = registry.sweep_offline(&instant(T1)).expect("early sweep");
    assert!(early.is_empty());

    let swept = registry.sweep_offline(&instant(T3)).expect("sweep");
    assert_eq!(swept.len(), 2);
    assert!(swept.contains(&online.client_node_id));
    assert!(swept.contains(&degraded.client_node_id));

    let online_after = registry
        .snapshot(&online.client_node_id)
        .expect("online snapshot")
        .expect("online record");
    assert_eq!(online_after.presence_state, ClientPresenceState::Offline);
    let degraded_after = registry
        .snapshot(&degraded.client_node_id)
        .expect("degraded snapshot")
        .expect("degraded record");
    assert_eq!(degraded_after.presence_state, ClientPresenceState::Offline);
    let locked_after = registry
        .snapshot(&locked.client_node_id)
        .expect("locked snapshot")
        .expect("locked record");
    assert_eq!(locked_after.presence_state, ClientPresenceState::Locked);
    let pending_after = registry
        .snapshot(&pending.client_node_id)
        .expect("pending snapshot")
        .expect("pending record");
    assert_eq!(
        pending_after.presence_state,
        ClientPresenceState::PendingEnrollment
    );
}

#[test]
fn exchange_cursors_advance_monotonically_per_direction() {
    let mut storage = open(temporary_directory("cursors"));
    let mut registry = storage.client_node_registry().expect("registry");
    let registered = registry
        .register(&registration(13), 0, &instant(T0))
        .expect("register")
        .record;
    let node = registered.client_node_id.clone();

    let advanced = registry
        .advance_exchange_cursors(&node, 3, 5)
        .expect("advance");
    assert_eq!(
        advanced,
        ClientExchangeCursors {
            client_to_server_ack_sequence: 3,
            server_to_client_ack_sequence: 5,
        }
    );
    // Replayed or older acknowledgements never regress a direction.
    let replayed = registry
        .advance_exchange_cursors(&node, 1, 9)
        .expect("replayed advance");
    assert_eq!(
        replayed,
        ClientExchangeCursors {
            client_to_server_ack_sequence: 3,
            server_to_client_ack_sequence: 9,
        }
    );
    let unknown = registry
        .advance_exchange_cursors(&node_id(14), 1, 1)
        .expect_err("unknown client node must fail");
    assert_eq!(unknown.kind(), ClientRegistryErrorKind::UnknownClientNode);
    assert!(
        registry
            .exchange_cursors(&node_id(14))
            .expect("unknown cursors")
            .is_none()
    );
}

#[test]
fn rejects_malformed_registration_facts() {
    let invalid_node_id = ClientNodeRegistration::try_new(
        "node_01j2",
        "000000001",
        "Device",
        "aarch64-apple-darwin",
        "aarch64",
        "1.2.3",
        None,
        None,
        1,
    )
    .expect_err("non-canonical client node id must be rejected");
    assert_eq!(
        invalid_node_id.kind(),
        ClientRegistryErrorKind::InvalidInput
    );

    let invalid_public_id = ClientNodeRegistration::try_new(
        node_id(1),
        "public-1",
        "Device",
        "aarch64-apple-darwin",
        "aarch64",
        "1.2.3",
        None,
        None,
        1,
    )
    .expect_err("non-numeric public client id must be rejected");
    assert_eq!(
        invalid_public_id.kind(),
        ClientRegistryErrorKind::InvalidInput
    );

    let invalid_platform = ClientNodeRegistration::try_new(
        node_id(1),
        "000000001",
        "Device",
        "windows-x86_64",
        "x86_64",
        "1.2.3",
        None,
        None,
        1,
    )
    .expect_err("unsupported platform must be rejected");
    assert_eq!(
        invalid_platform.kind(),
        ClientRegistryErrorKind::InvalidInput
    );

    let invalid_version = ClientNodeRegistration::try_new(
        node_id(1),
        "000000001",
        "Device",
        "aarch64-apple-darwin",
        "aarch64",
        "one.two.three",
        None,
        None,
        1,
    )
    .expect_err("non-semantic version must be rejected");
    assert_eq!(
        invalid_version.kind(),
        ClientRegistryErrorKind::InvalidInput
    );

    let invalid_digest = ClientNodeRegistration::try_new(
        node_id(1),
        "000000001",
        "Device",
        "aarch64-apple-darwin",
        "aarch64",
        "1.2.3",
        Some("sha256:not-a-digest".to_owned()),
        None,
        1,
    )
    .expect_err("non-canonical credential digest must be rejected");
    assert_eq!(invalid_digest.kind(), ClientRegistryErrorKind::InvalidInput);

    let invalid_capacity = ClientNodeRegistration::try_new(
        node_id(1),
        "000000001",
        "Device",
        "aarch64-apple-darwin",
        "aarch64",
        "1.2.3",
        None,
        None,
        2000,
    )
    .expect_err("capacity above the schema maximum must be rejected");
    assert_eq!(
        invalid_capacity.kind(),
        ClientRegistryErrorKind::InvalidInput
    );
}

#[test]
fn rejects_invalid_instant_and_unknown_snapshots() {
    let mut storage = open(temporary_directory("instant"));
    let mut registry = storage.client_node_registry().expect("registry");
    let invalid_time = registry
        .register(
            &registration(15),
            0,
            &Instant("2026-01-01 00:00:00".to_owned()),
        )
        .expect_err("non-canonical instant must be rejected");
    assert_eq!(invalid_time.kind(), ClientRegistryErrorKind::InvalidInput);

    assert!(
        registry
            .snapshot(&node_id(16))
            .expect("unknown snapshot")
            .is_none()
    );
}
