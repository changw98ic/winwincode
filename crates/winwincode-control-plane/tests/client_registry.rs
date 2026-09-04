// SPDX-License-Identifier: Apache-2.0

//! `ClientNode` registry application service vertical tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_control_plane::{ClientRegistryService, ClientRegistryServiceErrorKind};
use winwincode_domain::Instant;
use winwincode_storage::{ClientPresenceState, SqliteStorage};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-client-registry-service-{name}-{}-{suffix}",
        std::process::id()
    ))
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

fn registration(seed: u64) -> winwincode_storage::ClientNodeRegistration {
    winwincode_storage::ClientNodeRegistration::try_new(
        node_id(seed),
        format!("{seed:09}"),
        format!("Device {seed}"),
        "aarch64-unknown-linux-gnu",
        "aarch64",
        "1.0.0",
        Some(digest(seed)),
        Some(instance_id(seed)),
        2,
    )
    .expect("registration")
}

const T0: &str = "2026-01-01T00:00:00.000Z";
const T1: &str = "2026-01-01T00:01:00.000Z";
const T2: &str = "2026-01-01T00:02:00.000Z";
const T3: &str = "2026-01-01T00:03:00.000Z";

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

#[test]
fn service_registers_projects_heartbeats_and_recovers_cursors() {
    let mut storage = SqliteStorage::open(temporary_directory("vertical")).expect("storage");
    let mut service = ClientRegistryService::new(&mut storage);

    let receipt = service
        .register(&registration(1), 0, &instant(T0))
        .expect("register");
    assert!(receipt.enrolled);
    assert_eq!(
        receipt.record.presence_state,
        ClientPresenceState::PendingEnrollment
    );
    let node = node_id(1);

    let snapshot = service
        .snapshot(&node)
        .expect("snapshot")
        .expect("registered record");
    assert_eq!(snapshot.revision, 1);
    assert!(snapshot.accepting_connections);

    // Cursor facts are durable before any exchange happens.
    let cursors = service
        .exchange_cursors(&node)
        .expect("cursors")
        .expect("zeroed cursors");
    assert_eq!(cursors.client_to_server_ack_sequence, 0);
    assert_eq!(cursors.server_to_client_ack_sequence, 0);

    // pending_enrollment -> online on enrollment acceptance, then heartbeat.
    let online = service
        .update_presence(&node, ClientPresenceState::Online, snapshot.revision)
        .expect("accept enrollment");
    assert_eq!(online.presence_state, ClientPresenceState::Online);
    let beat = service
        .heartbeat(&node, 1, &instant(T1), online.revision)
        .expect("heartbeat");
    assert_eq!(beat.reported_running_worker_sessions, 1);
    assert_eq!(beat.last_heartbeat_at, Some(instant(T1)));

    // Device-reported projection refresh under the live revision.
    let refreshed = winwincode_storage::ClientNodeRegistration::try_new(
        node.clone(),
        registration(1).public_client_id().to_owned(),
        "Renamed Device",
        "aarch64-unknown-linux-gnu",
        "aarch64",
        "1.1.0",
        Some(digest(1)),
        Some(instance_id(1)),
        3,
    )
    .expect("refreshed registration");
    let refreshed = service
        .register(&refreshed, beat.revision, &instant(T2))
        .expect("refresh registration");
    assert!(!refreshed.enrolled);
    assert_eq!(refreshed.record.display_name, "Renamed Device");
    assert_eq!(refreshed.record.client_version, "1.1.0");
    assert_eq!(refreshed.record.presence_state, ClientPresenceState::Online);

    // Exchange acknowledgements persist monotonically for restart recovery.
    let advanced = service
        .advance_exchange_cursors(&node, 7, 11)
        .expect("advance cursors");
    assert_eq!(advanced.client_to_server_ack_sequence, 7);
    assert_eq!(advanced.server_to_client_ack_sequence, 11);
}

#[test]
fn service_maps_revision_conflicts_and_offline_projection_errors() {
    let mut storage = SqliteStorage::open(temporary_directory("errors")).expect("storage");
    let mut service = ClientRegistryService::new(&mut storage);

    let registered = service
        .register(&registration(2), 0, &instant(T0))
        .expect("register")
        .record;
    let node = node_id(2);

    let stale = service
        .update_presence(&node, ClientPresenceState::Online, 99)
        .expect_err("stale revision must fail");
    assert_eq!(
        stale.kind(),
        ClientRegistryServiceErrorKind::RevisionConflict
    );

    let unknown = service
        .heartbeat(&node_id(3), 0, &instant(T1), 0)
        .expect_err("unknown client node must fail");
    assert_eq!(
        unknown.kind(),
        ClientRegistryServiceErrorKind::UnknownClientNode
    );

    service
        .update_presence(&node, ClientPresenceState::Online, registered.revision)
        .expect("accept enrollment");
    service
        .heartbeat(&node, 0, &instant(T1), registered.revision + 1)
        .expect("heartbeat");

    let swept = service.sweep_offline(&instant(T2)).expect("sweep");
    assert_eq!(swept, vec![node.clone()]);
    let offline = service.snapshot(&node).expect("snapshot").expect("record");
    assert_eq!(offline.presence_state, ClientPresenceState::Offline);

    // Reconnect heartbeats project offline back to online.
    let reconnected = service
        .heartbeat(&node, 0, &instant(T3), offline.revision)
        .expect("reconnect heartbeat");
    assert_eq!(reconnected.presence_state, ClientPresenceState::Online);
    assert_eq!(reconnected.last_heartbeat_at, Some(instant(T3)));
}
