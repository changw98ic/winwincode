// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use winwincode_domain::{Instant, WorkerId, WorkerInstanceId};
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, GrantPermissions,
    GrantSource, GrantTrustMode, LaunchGrantIssuance, OccupancyClaim,
    RepositoryAccessGrantIssuance, RepositoryAvailability, RepositoryBindingProjection,
    RepositoryDirtyState, RepositoryGrantPermissions, SqliteStorage,
};

#[allow(clippy::too_many_lines)]
pub fn stage(root: &Path, worker: &WorkerId, instance: &WorkerInstanceId) -> String {
    let mut storage = SqliteStorage::open(root).unwrap();
    let now = Instant("2027-01-15T08:00:00.000Z".into());
    let node = "cnd_00000000000000000000000001";
    let client_instance = "cix_00000000000000000000000001";
    let user = "usr_00000000000000000000000001";
    let binding = "rbd_00000000000000000000000001";
    let registration = ClientNodeRegistration::try_new(
        node,
        "0000000001",
        "Relay Device",
        "aarch64-apple-darwin",
        "aarch64",
        "0.1.0",
        Some(client_instance.to_owned()),
        4,
    )
    .unwrap();
    storage
        .client_node_registry()
        .unwrap()
        .register(&registration, 0, &now)
        .unwrap();
    storage
        .client_node_registry()
        .unwrap()
        .update_presence(node, ClientPresenceState::Online, 1)
        .unwrap();
    let grant = AccessGrantIssuance::try_new(
        "cag_00000000000000000000000001",
        node,
        user,
        user,
        GrantTrustMode::Trusted,
        None,
    )
    .unwrap();
    storage
        .client_connect_ledger()
        .unwrap()
        .create_grant(
            &grant,
            GrantSource::Administrator,
            GrantPermissions::USE,
            &now,
        )
        .unwrap();
    let projection = RepositoryBindingProjection::try_new(
        binding,
        node,
        "relay",
        Some("main".to_owned()),
        Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
        RepositoryDirtyState::Clean,
        RepositoryAvailability::Available,
        format!("sha256:{}", "a".repeat(64)),
    )
    .unwrap();
    storage
        .repository_binding_ledger()
        .unwrap()
        .upsert(&projection, None, 0, &now)
        .unwrap();
    let grant = RepositoryAccessGrantIssuance::try_new(
        "rag_00000000000000000000000001",
        binding,
        user,
        user,
    )
    .unwrap();
    storage
        .repository_binding_ledger()
        .unwrap()
        .create_grant(&grant, RepositoryGrantPermissions::Use, &now)
        .unwrap();
    let claim = OccupancyClaim::try_new(
        "ocl_00000000000000000000000001",
        node,
        user,
        "req_00000000000000000000000001",
    )
    .unwrap();
    let lease = storage
        .client_occupancy_ledger()
        .unwrap()
        .atomic_claim(&claim, &now)
        .unwrap();
    let lease = storage
        .client_occupancy_ledger()
        .unwrap()
        .record_acknowledgement(&lease.occupancy_lease_id, lease.fencing_token, None, &now)
        .unwrap();
    let grant = LaunchGrantIssuance::try_new(
        "wlg_00000000000000000000000001",
        node,
        client_instance,
        user,
        &lease.occupancy_lease_id,
        lease.fencing_token,
        binding,
        "wsn_00000000000000000000000001",
        worker.0.clone(),
        instance.0.clone(),
        Some("psn_00000000000000000000000001".to_owned()),
        None,
        Instant("2100-01-01T00:00:00.000Z".into()),
    )
    .unwrap();
    storage
        .worker_launch_grant_ledger()
        .unwrap()
        .issue(&grant, &now)
        .unwrap();
    node.to_owned()
}
