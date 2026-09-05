// SPDX-License-Identifier: Apache-2.0

//! `DeviceExecutionBinding` application service over the durable binding
//! ledger and its reservation capacity view.
//!
//! The Control Plane is the authoritative owner of the task-execution
//! identity chain (plan 7.8, 14, 17.2): it binds one `WorkerSession` durably
//! to the client node, occupancy lease, fencing token, and repository
//! binding of its live `WorkerLaunchGrant`, attaches those device facts to
//! one execution admission reservation so every Job identity stays
//! traceable, and judges worker-session capacity for both the occupancy
//! claim and the launch issue gates against one durable reservation ledger
//! (`reserved` = non-terminal launch grants). Commands echo the stored
//! authority exactly; any divergence is refused without a durable change,
//! and the fixed CAS/replay rules make every transition at-most-once
//! idempotent.

use std::fmt;

use winwincode_domain::Instant;
use winwincode_storage::{
    DeviceExecutionBindingIssuance, DeviceExecutionBindingRelease, SqliteStorage,
};

/// Re-exported so service consumers can name the durable records and
/// commands without importing the storage crate directly.
pub use winwincode_storage::DeviceBindingReceipt;
pub use winwincode_storage::DeviceExecutionBindingRecord;
pub use winwincode_storage::DeviceExecutionBindingState as DeviceBindingState;
pub use winwincode_storage::DeviceExecutionBindingStoreError;
pub use winwincode_storage::DeviceExecutionBindingStoreErrorKind;
pub use winwincode_storage::DeviceExecutionCapacitySnapshot;
pub use winwincode_storage::DeviceExecutionFactsAttachment;
pub use winwincode_storage::DeviceExecutionReservationFacts;
pub use winwincode_storage::DeviceFactsReceipt;

/// Stable service failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceExecutionBindingServiceErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// No launch grant matches the requested identity.
    UnknownLaunchGrant,
    /// The launch grant is not live (`issued` or `consumed`).
    LaunchGrantNotLive,
    /// An echoed binding or attachment field differs from the authority.
    FieldMismatch,
    /// The worker session or grant already carries a binding, or the binding
    /// identity is already used.
    BindingConflict,
    /// No bound binding matches the requested worker session.
    UnknownBinding,
    /// The execution Job does not exist.
    UnknownExecutionJob,
    /// The execution Job already carries device facts.
    FactsAlreadyAttached,
    /// The requested change is not a legal state machine transition.
    IllegalStateTransition,
    /// A compare-and-swap guard lost a race.
    RevisionConflict,
    /// A request identity was reused with a different body.
    RequestConflict,
    /// A stored row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free device execution binding service error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceExecutionBindingServiceError {
    kind: DeviceExecutionBindingServiceErrorKind,
    message: String,
}

impl DeviceExecutionBindingServiceError {
    #[must_use]
    pub const fn kind(&self) -> DeviceExecutionBindingServiceErrorKind {
        self.kind
    }
}

impl fmt::Display for DeviceExecutionBindingServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DeviceExecutionBindingServiceError {}

impl From<DeviceExecutionBindingStoreError> for DeviceExecutionBindingServiceError {
    fn from(source: DeviceExecutionBindingStoreError) -> Self {
        Self {
            kind: match source.kind() {
                DeviceExecutionBindingStoreErrorKind::InvalidInput => {
                    DeviceExecutionBindingServiceErrorKind::InvalidInput
                }
                DeviceExecutionBindingStoreErrorKind::UnknownLaunchGrant => {
                    DeviceExecutionBindingServiceErrorKind::UnknownLaunchGrant
                }
                DeviceExecutionBindingStoreErrorKind::LaunchGrantNotLive => {
                    DeviceExecutionBindingServiceErrorKind::LaunchGrantNotLive
                }
                DeviceExecutionBindingStoreErrorKind::FieldMismatch => {
                    DeviceExecutionBindingServiceErrorKind::FieldMismatch
                }
                DeviceExecutionBindingStoreErrorKind::BindingConflict => {
                    DeviceExecutionBindingServiceErrorKind::BindingConflict
                }
                DeviceExecutionBindingStoreErrorKind::UnknownBinding => {
                    DeviceExecutionBindingServiceErrorKind::UnknownBinding
                }
                DeviceExecutionBindingStoreErrorKind::UnknownExecutionJob => {
                    DeviceExecutionBindingServiceErrorKind::UnknownExecutionJob
                }
                DeviceExecutionBindingStoreErrorKind::FactsAlreadyAttached => {
                    DeviceExecutionBindingServiceErrorKind::FactsAlreadyAttached
                }
                DeviceExecutionBindingStoreErrorKind::IllegalStateTransition => {
                    DeviceExecutionBindingServiceErrorKind::IllegalStateTransition
                }
                DeviceExecutionBindingStoreErrorKind::RevisionConflict => {
                    DeviceExecutionBindingServiceErrorKind::RevisionConflict
                }
                DeviceExecutionBindingStoreErrorKind::RequestConflict => {
                    DeviceExecutionBindingServiceErrorKind::RequestConflict
                }
                DeviceExecutionBindingStoreErrorKind::CorruptState => {
                    DeviceExecutionBindingServiceErrorKind::CorruptState
                }
                DeviceExecutionBindingStoreErrorKind::Storage => {
                    DeviceExecutionBindingServiceErrorKind::Storage
                }
            },
            message: source.to_string(),
        }
    }
}

/// Device execution binding application service over one storage connection.
pub struct DeviceExecutionBindingService<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl<'storage> DeviceExecutionBindingService<'storage> {
    /// Builds one service over the sole product-state storage authority.
    #[must_use]
    pub fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Atomically binds one worker session to the identities of its live
    /// launch grant behind the exact-echo gate.
    ///
    /// # Errors
    ///
    /// Rejects an unknown or terminal grant, any field mismatch, a reused
    /// binding identity, a conflicting replay, or storage failure.
    pub fn bind(
        &mut self,
        command: &DeviceExecutionBindingIssuance,
        now: &Instant,
    ) -> Result<DeviceBindingReceipt, DeviceExecutionBindingServiceError> {
        Ok(self
            .storage
            .device_execution_binding_ledger()?
            .bind(command, now)?)
    }

    /// Releases the `bound` binding of one worker session through the fixed
    /// compare-and-swap transition.
    ///
    /// # Errors
    ///
    /// Rejects an unknown bound binding, a lost revision race, a conflicting
    /// replay, or storage failure.
    pub fn release(
        &mut self,
        command: &DeviceExecutionBindingRelease,
        now: &Instant,
    ) -> Result<DeviceBindingReceipt, DeviceExecutionBindingServiceError> {
        Ok(self
            .storage
            .device_execution_binding_ledger()?
            .release(command, now)?)
    }

    /// Atomically attaches the device facts of one execution admission
    /// reservation, copied verbatim from the launch grant.
    ///
    /// # Errors
    ///
    /// Rejects an unknown grant or Job, a terminal grant or reservation, a
    /// missing binding, an already attached Job, a conflicting replay, or
    /// storage failure.
    pub fn attach_facts(
        &mut self,
        command: &DeviceExecutionFactsAttachment,
        now: &Instant,
    ) -> Result<DeviceFactsReceipt, DeviceExecutionBindingServiceError> {
        Ok(self
            .storage
            .device_execution_binding_ledger()?
            .attach_facts(command, now)?)
    }

    /// Returns the newest binding of a worker session.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical worker session identity or storage failure.
    pub fn snapshot(
        &mut self,
        worker_session_id: &str,
    ) -> Result<Option<DeviceExecutionBindingRecord>, DeviceExecutionBindingServiceError> {
        Ok(self
            .storage
            .device_execution_binding_ledger()?
            .snapshot(worker_session_id)?)
    }

    /// Returns one binding by its own identity.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical binding identity or storage failure.
    pub fn snapshot_by_binding_id(
        &mut self,
        device_execution_binding_id: &str,
    ) -> Result<Option<DeviceExecutionBindingRecord>, DeviceExecutionBindingServiceError> {
        Ok(self
            .storage
            .device_execution_binding_ledger()?
            .snapshot_by_binding_id(device_execution_binding_id)?)
    }

    /// Returns the durable device facts of one execution reservation.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical Job identity or storage failure.
    pub fn facts(
        &mut self,
        job_id: &str,
    ) -> Result<Option<DeviceExecutionReservationFacts>, DeviceExecutionBindingServiceError> {
        Ok(self
            .storage
            .device_execution_binding_ledger()?
            .facts(job_id)?)
    }

    /// Reads the durable worker-session capacity ledger of one client node;
    /// `None` when the node does not exist. Both the occupancy claim gate and
    /// the launch issue gate judge against this one view.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node identity or storage failure.
    pub fn capacity_snapshot(
        &mut self,
        client_node_id: &str,
    ) -> Result<Option<DeviceExecutionCapacitySnapshot>, DeviceExecutionBindingServiceError> {
        Ok(self
            .storage
            .device_execution_binding_ledger()?
            .capacity_snapshot(client_node_id)?)
    }

    /// Counts the durable reservation view of one client node: its
    /// non-terminal launch grants.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node identity or storage failure.
    pub fn reserved_worker_sessions_for_node(
        &mut self,
        client_node_id: &str,
    ) -> Result<u64, DeviceExecutionBindingServiceError> {
        Ok(self
            .storage
            .device_execution_binding_ledger()?
            .reserved_worker_sessions_for_node(client_node_id)?)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use winwincode_domain::{
        DeliveryId, ExecutionJobId, Instant, OrganizationId, ProductSessionId, ProjectId,
        RepositoryId, RequestId, UserId, WorkspaceId,
    };
    use winwincode_storage::{
        AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState,
        DeviceExecutionBindingIssuance, DeviceExecutionBindingRelease,
        DeviceExecutionFactsAttachment, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
        ExecutionAdmissionPolicy, ExecutionQueueScope, ExecutionRepositoryAccess,
        ExecutionReservationRequest, GrantPermissions, GrantSource, GrantTrustMode,
        LaunchGrantIssuance, OccupancyClaim, OccupancyLeaseState, RepositoryAccessGrantIssuance,
        RepositoryAvailability, RepositoryBindingProjection, RepositoryDirtyState,
        RepositoryGrantPermissions, WorkerLaunchGrantRecord, WorkerPoolId,
    };

    use super::*;

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory(name: &str) -> PathBuf {
        let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-binding-service-{name}-{}-{suffix}-{nanos}",
            std::process::id()
        ))
    }

    fn instant(value: &str) -> Instant {
        Instant(value.to_owned())
    }

    fn id(prefix: &str, seed: u64) -> String {
        format!("{prefix}_{seed:026}")
    }

    const T0: &str = "2026-01-01T00:00:00.000Z";
    const T2: &str = "2026-01-01T00:02:00.000Z";
    const T3: &str = "2026-01-01T00:03:00.000Z";
    const GRANT_EXPIRES: &str = "2026-01-01T01:00:00.000Z";

    const DIGEST: &str = "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    /// Every authoritative identity one binding needs.
    struct Fixture {
        node: String,
        instance: String,
        holder: String,
        lease_id: String,
        fencing_token: u64,
        binding_id: String,
    }

    /// Seeds the registry, access grants, occupancy lease, and repository
    /// binding (the shared launch-grant fixture chain).
    #[allow(clippy::too_many_lines)]
    fn seed_fixture(storage: &mut SqliteStorage, seed: u64) -> Fixture {
        let node = id("cnd", seed);
        let instance = id("cix", seed + 2);
        let holder = id("usr", seed + 1);
        {
            let registration = ClientNodeRegistration::try_new(
                node.clone(),
                format!("{seed:010}"),
                "Binding Service Test Device".to_owned(),
                "aarch64-apple-darwin",
                "aarch64",
                "1.2.3",
                None,
                Some(instance.clone()),
                4,
            )
            .expect("registration");
            let mut registry = storage.client_node_registry().expect("registry");
            registry
                .register(&registration, 0, &instant(T0))
                .expect("register");
            registry
                .update_presence(&node, ClientPresenceState::Online, 1)
                .expect("presence");
        }
        {
            let issuance = AccessGrantIssuance::try_new(
                id("cag", seed + 3),
                &node,
                &holder,
                &holder,
                GrantTrustMode::Trusted,
                None,
            )
            .expect("issuance");
            let mut ledger = storage.client_connect_ledger().expect("ledger");
            ledger
                .create_grant(
                    &issuance,
                    GrantSource::Administrator,
                    GrantPermissions::USE,
                    &instant("2026-01-01T00:00:10.000Z"),
                )
                .expect("grant");
        }
        let (lease_id, fencing_token) = {
            let mut occupancy = storage.client_occupancy_ledger().expect("ledger");
            let claim =
                OccupancyClaim::try_new(id("ocl", seed + 4), &node, &holder, id("req", seed + 5))
                    .expect("claim");
            let lease = occupancy
                .atomic_claim(&claim, &instant("2026-01-01T00:01:00.000Z"))
                .expect("claim");
            let occupied = occupancy
                .record_acknowledgement(
                    &lease.occupancy_lease_id,
                    lease.fencing_token,
                    None,
                    &instant("2026-01-01T00:01:01.000Z"),
                )
                .expect("ack");
            assert_eq!(occupied.state, OccupancyLeaseState::Occupied);
            (occupied.occupancy_lease_id, occupied.fencing_token)
        };
        let binding_id = id("rbd", seed + 6);
        {
            let mut ledger = storage.repository_binding_ledger().expect("ledger");
            let projection = RepositoryBindingProjection::try_new(
                binding_id.clone(),
                &node,
                "winwincode",
                Some("main".to_owned()),
                Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
                RepositoryDirtyState::Clean,
                RepositoryAvailability::Available,
                format!("sha256:{seed:064}"),
            )
            .expect("projection");
            ledger
                .upsert(&projection, None, 0, &instant("2026-01-01T00:00:30.000Z"))
                .expect("upsert");
            let issuance = RepositoryAccessGrantIssuance::try_new(
                id("rag", seed + 7),
                &binding_id,
                &holder,
                &holder,
            )
            .expect("repo issuance");
            ledger
                .create_grant(
                    &issuance,
                    RepositoryGrantPermissions::Use,
                    &instant("2026-01-01T00:00:31.000Z"),
                )
                .expect("repo grant");
        }
        Fixture {
            node,
            instance,
            holder,
            lease_id,
            fencing_token,
            binding_id,
        }
    }

    /// Issues one live launch grant over the seeded fixture.
    fn issue_launch_grant(
        storage: &mut SqliteStorage,
        seed: u64,
        fixture: &Fixture,
    ) -> WorkerLaunchGrantRecord {
        let issuance = LaunchGrantIssuance::try_new(
            id("wlg", seed),
            &fixture.node,
            &fixture.instance,
            &fixture.holder,
            &fixture.lease_id,
            fixture.fencing_token,
            &fixture.binding_id,
            id("ws", seed + 8),
            id("wkr", seed + 9),
            id("winst", seed + 10),
            DIGEST,
            Some(id("ps", seed + 11)),
            Some(id("run", seed + 12)),
            instant(GRANT_EXPIRES),
        )
        .expect("grant issuance");
        storage
            .worker_launch_grant_ledger()
            .expect("ledger")
            .issue(&issuance, &instant(T0))
            .expect("issue")
    }

    /// Echoes every grant field into a validated bind command.
    fn bind_command(seed: u64, grant: &WorkerLaunchGrantRecord) -> DeviceExecutionBindingIssuance {
        DeviceExecutionBindingIssuance::try_new(
            id("deb", seed),
            id("req", seed + 1),
            &grant.worker_launch_grant_id,
            &grant.client_node_id,
            &grant.client_instance_id,
            &grant.holder_user_id,
            &grant.occupancy_lease_id,
            grant.occupancy_fencing_token,
            &grant.repository_binding_id,
            &grant.worker_session_id,
            grant.product_session_id.clone(),
            grant.stage_run_id.clone(),
        )
        .expect("bind command")
    }

    /// Configures every admission boundary and reserves one queued Job for
    /// the fixture holder.
    fn seed_reservation(storage: &mut SqliteStorage, seed: u64, fixture: &Fixture) -> String {
        let scope = ExecutionQueueScope {
            organization_id: OrganizationId(id("org", seed)),
            workspace_id: WorkspaceId(id("wsp", seed)),
            project_id: ProjectId(id("prj", seed)),
            repository_id: RepositoryId(id("rep", seed)),
            product_session_id: ProductSessionId(id("psn", seed)),
            delivery_id: Some(DeliveryId(id("dlv", seed))),
        };
        let pool = WorkerPoolId(id("wpl", seed));
        let limits = ExecutionAdmissionLimits {
            max_concurrent: 4,
            max_queued: 4,
            token_budget: 10_000,
            cost_budget_microunits: 10_000,
            max_runtime_millis: 60_000,
        };
        {
            let mut admission = storage.execution_admission().expect("admission");
            for boundary in [
                ExecutionAdmissionBoundary::Organization {
                    organization_id: scope.organization_id.clone(),
                },
                ExecutionAdmissionBoundary::Project {
                    organization_id: scope.organization_id.clone(),
                    project_id: scope.project_id.clone(),
                },
                ExecutionAdmissionBoundary::Repository {
                    organization_id: scope.organization_id.clone(),
                    project_id: scope.project_id.clone(),
                    repository_id: scope.repository_id.clone(),
                },
                ExecutionAdmissionBoundary::Delivery {
                    organization_id: scope.organization_id.clone(),
                    delivery_id: scope.delivery_id.clone().expect("delivery"),
                },
                ExecutionAdmissionBoundary::ProductSession {
                    organization_id: scope.organization_id.clone(),
                    project_id: scope.project_id.clone(),
                    product_session_id: scope.product_session_id.clone(),
                },
                ExecutionAdmissionBoundary::WorkerPool {
                    organization_id: scope.organization_id.clone(),
                    worker_pool_id: pool.clone(),
                },
            ] {
                admission
                    .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
                    .expect("policy configure");
            }
            admission
                .reserve(&ExecutionReservationRequest {
                    scope,
                    user_id: UserId(fixture.holder.clone()),
                    worker_pool_id: pool,
                    job_id: ExecutionJobId(id("job", seed)),
                    request_id: RequestId(id("req", seed + 13)),
                    repository_access: ExecutionRepositoryAccess::ReadOnly,
                    reserved_tokens: 100,
                    reserved_cost_microunits: 1_000,
                    runtime_limit_millis: 30_000,
                    submitted_at: instant(T2),
                })
                .expect("reserve");
        }
        id("job", seed)
    }

    #[test]
    fn representative_store_errors_map_one_to_one_onto_the_service_taxonomy() {
        // Input validation failures need no database.
        let invalid = DeviceExecutionBindingIssuance::try_new(
            "nope",
            id("req", 1),
            id("wlg", 2),
            id("cnd", 3),
            id("cix", 4),
            id("usr", 5),
            id("ocl", 6),
            1,
            id("rbd", 7),
            id("ws", 8),
            None,
            None,
        )
        .expect_err("a non-canonical binding id is invalid input");
        assert_eq!(
            DeviceExecutionBindingServiceError::from(invalid).kind(),
            DeviceExecutionBindingServiceErrorKind::InvalidInput
        );

        let mut storage = SqliteStorage::open(temporary_directory("mapping")).expect("storage");
        let mut service = DeviceExecutionBindingService::new(&mut storage);
        let unknown = DeviceExecutionBindingIssuance::try_new(
            id("deb", 10),
            id("req", 11),
            id("wlg", 12),
            id("cnd", 13),
            id("cix", 14),
            id("usr", 15),
            id("ocl", 16),
            1,
            id("rbd", 17),
            id("ws", 18),
            None,
            None,
        )
        .expect("unknown command");
        let error = service
            .bind(&unknown, &instant(T2))
            .expect_err("an unknown grant must be refused");
        assert_eq!(
            error.kind(),
            DeviceExecutionBindingServiceErrorKind::UnknownLaunchGrant
        );
        let error = service
            .release(
                &DeviceExecutionBindingRelease::try_new(
                    id("ws", 20),
                    id("req", 21),
                    1,
                    instant(T2),
                )
                .expect("release"),
                &instant(T2),
            )
            .expect_err("an unknown binding must be refused");
        assert_eq!(
            error.kind(),
            DeviceExecutionBindingServiceErrorKind::UnknownBinding
        );
        let error = service
            .attach_facts(
                &DeviceExecutionFactsAttachment::try_new(
                    id("req", 22),
                    id("job", 23),
                    id("wlg", 24),
                )
                .expect("attachment"),
                &instant(T2),
            )
            .expect_err("an unknown grant must be refused");
        assert_eq!(
            error.kind(),
            DeviceExecutionBindingServiceErrorKind::UnknownLaunchGrant
        );
        assert!(service.snapshot(&id("ws", 30)).expect("snapshot").is_none());
        assert!(
            service
                .snapshot_by_binding_id(&id("deb", 31))
                .expect("snapshot")
                .is_none()
        );
        assert!(service.facts(&id("job", 32)).expect("facts").is_none());
        assert!(
            service
                .capacity_snapshot(&id("cnd", 33))
                .expect("capacity")
                .is_none()
        );
    }

    #[test]
    fn the_service_drives_bind_attach_capacity_and_release_end_to_end() {
        let mut storage = SqliteStorage::open(temporary_directory("happy")).expect("storage");
        let fixture = seed_fixture(&mut storage, 100);
        let grant = issue_launch_grant(&mut storage, 200, &fixture);
        let job = seed_reservation(&mut storage, 300, &fixture);
        let command = bind_command(400, &grant);
        {
            let mut service = DeviceExecutionBindingService::new(&mut storage);
            // The capacity ledger counts the issued grant before the binding.
            let reserved = service
                .reserved_worker_sessions_for_node(&fixture.node)
                .expect("reserved");
            assert_eq!(reserved, 1);
            let receipt = service.bind(&command, &instant(T2)).expect("bind");
            assert!(!receipt.replayed);
            assert_eq!(receipt.binding.state.as_str(), "bound");
            let facts = service
                .attach_facts(
                    &DeviceExecutionFactsAttachment::try_new(
                        id("req", 410),
                        &job,
                        &grant.worker_launch_grant_id,
                    )
                    .expect("attachment"),
                    &instant(T2),
                )
                .expect("attach");
            assert_eq!(facts.facts.worker_session_id, grant.worker_session_id);
            let capacity = service
                .capacity_snapshot(&fixture.node)
                .expect("capacity")
                .expect("node");
            assert_eq!(capacity.reserved_worker_sessions, 1);
            assert_eq!(capacity.bound_bindings, 1);
            assert_eq!(capacity.free_worker_sessions, 3);
        }
        {
            let mut service = DeviceExecutionBindingService::new(&mut storage);
            let release = service
                .release(
                    &DeviceExecutionBindingRelease::try_new(
                        &grant.worker_session_id,
                        id("req", 420),
                        1,
                        instant(T3),
                    )
                    .expect("release"),
                    &instant(T3),
                )
                .expect("release");
            assert_eq!(release.binding.state.as_str(), "released");
            let snapshot = service
                .snapshot(&grant.worker_session_id)
                .expect("snapshot")
                .expect("binding");
            assert_eq!(snapshot, release.binding);
            let by_id = service
                .snapshot_by_binding_id(&release.binding.device_execution_binding_id)
                .expect("snapshot")
                .expect("binding");
            assert_eq!(by_id, release.binding);
            assert!(
                service
                    .facts(&job)
                    .expect("facts")
                    .expect("stored facts")
                    .worker_launch_grant_id
                    == grant.worker_launch_grant_id
            );
        }
    }

    #[test]
    fn the_service_surfaces_the_gate_rejections_for_the_boundary() {
        let mut storage = SqliteStorage::open(temporary_directory("gate")).expect("storage");
        let fixture = seed_fixture(&mut storage, 500);
        let grant = issue_launch_grant(&mut storage, 600, &fixture);
        let job = seed_reservation(&mut storage, 700, &fixture);
        let mut service = DeviceExecutionBindingService::new(&mut storage);
        // A guessed field refuses the binding without a durable change.
        let mut guessed = bind_command(800, &grant);
        guessed.expected_occupancy_fencing_token += 1;
        let error = service
            .bind(&guessed, &instant(T2))
            .expect_err("a guessed token must be refused");
        assert_eq!(
            error.kind(),
            DeviceExecutionBindingServiceErrorKind::FieldMismatch
        );
        // Facts cannot attach before the session is bound.
        let error = service
            .attach_facts(
                &DeviceExecutionFactsAttachment::try_new(
                    id("req", 810),
                    &job,
                    &grant.worker_launch_grant_id,
                )
                .expect("attachment"),
                &instant(T2),
            )
            .expect_err("an unbound session must be refused");
        assert_eq!(
            error.kind(),
            DeviceExecutionBindingServiceErrorKind::UnknownBinding
        );
        // The happy path then succeeds and stays idempotent.
        let receipt = service
            .bind(&bind_command(820, &grant), &instant(T2))
            .expect("bind");
        let replay = service
            .bind(&bind_command(820, &grant), &instant(T3))
            .expect("replay");
        assert!(!receipt.replayed);
        assert!(replay.replayed);
    }
}
