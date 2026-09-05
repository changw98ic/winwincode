// SPDX-License-Identifier: Apache-2.0

//! The `FLOW-100.5` `StrongFlow` role-to-Device `WorkerSession` routing over the
//! real durable ledgers: every Delivery role stage run whose launch anchor
//! exists dispatches to its own launched Device `WorkerSession` with its role
//! stamped on the reservation facts, concurrent role dispatches stay within
//! the Client's worker-session capacity, and a stage run without an anchor
//! (or with a dead anchor, or a denied actor) keeps or restores the exact
//! supervised local behavior — including the local queue exclusion reuse.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_control_plane::{
    DeviceExecutionBindingService, RepositoryExecutionScheduler, StrongflowDeviceDispatchErrorKind,
    WorkerLaunchGrantService, dispatch_stage_to_device_worker,
};
use winwincode_domain::{
    DeliveryId, ExecutionJobId, ExecutionMessageId, Instant, OrganizationId, ProductSessionId,
    ProjectId, RepositoryId, RequestId, Sha256Digest, StageRunId, UserId, WorkerId,
    WorkerInstanceId, WorkspaceId,
};
use winwincode_execution_port::generated::{
    DeliveryStageExecutionScope, DeliveryStageExecutionScopeKind, ExecutionJob, ExecutionLimits,
    ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
};
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, EXECUTION_PROTOCOL_VERSION,
    ExecutionJobState, ExecutionJobSubmission, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationState, GrantPermissions, GrantSource, GrantTrustMode, LaunchGrantIssuance,
    OccupancyClaim, OccupancyLeaseState, RepositoryAccessGrantIssuance, RepositoryAvailability,
    RepositoryBindingProjection, RepositoryDirtyState, RepositoryGrantPermissions,
    RepositorySchedulerClaimRequest, RepositorySchedulerScope, SqliteStorage,
    WorkerAuthenticationIdentity, WorkerPlatform, WorkerRegistrationRequest,
    WorkerRegistrationStatus,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

const DIGEST: &str = "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const T0: &str = "2026-09-04T12:00:00.000Z";
const HOLDER: &str = "usr_00000000000000000000000001";
const MEMBER: &str = "usr_00000000000000000000000002";

fn temporary_root(label: &str) -> PathBuf {
    let suffix = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-strongflow-device-execution-{label}-{}-{suffix}",
        std::process::id()
    ))
}

fn canonical_id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

/// A canonical 26-character Crockford identity body: distinct seeds differ
/// across the whole suffix, like the ULID identities production mints.
fn ulid(seed: u64) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut identity = String::with_capacity(26);
    let mut value = seed;
    for _ in 0..26 {
        identity.push(ALPHABET[usize::try_from(value % 32).expect("digit fits")] as char);
        value /= 32;
    }
    identity
}

fn ulid_id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{}", ulid(seed))
}

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

fn repository_scope(seed: u64) -> RepositorySchedulerScope {
    RepositorySchedulerScope {
        organization_id: OrganizationId(canonical_id("org", seed)),
        workspace_id: WorkspaceId(canonical_id("wsp", seed)),
        project_id: ProjectId(canonical_id("prj", seed)),
        repository_id: RepositoryId(canonical_id("rep", seed)),
    }
}

// ---- durable device fixtures (the shared launch-anchor chain) --------------

/// Seeds node, client grant, visible repository binding, and the
/// device-confirmed occupancy lease; returns the node, its client instance,
/// the lease identity, fencing token, and the binding.
fn stage_device_fixture(
    storage: &mut SqliteStorage,
    seed: u64,
    user: &str,
) -> (String, String, String, u64, String) {
    let node = canonical_id("cnd", seed);
    let instance = canonical_id("cix", seed + 1);
    {
        let registration = ClientNodeRegistration::try_new(
            node.clone(),
            format!("{seed:010}"),
            "StrongFlow Device Test Device".to_owned(),
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
            canonical_id("cag", seed + 2),
            &node,
            user,
            user,
            GrantTrustMode::Trusted,
            None,
        )
        .expect("issuance");
        storage
            .client_connect_ledger()
            .expect("ledger")
            .create_grant(
                &issuance,
                GrantSource::Administrator,
                GrantPermissions::USE,
                &instant(T0),
            )
            .expect("grant");
    }
    let binding = canonical_id("rbd", seed + 3);
    {
        let mut ledger = storage.repository_binding_ledger().expect("ledger");
        let projection = RepositoryBindingProjection::try_new(
            binding.clone(),
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
            .upsert(&projection, None, 0, &instant(T0))
            .expect("upsert");
        let issuance = RepositoryAccessGrantIssuance::try_new(
            canonical_id("rag", seed + 4),
            &binding,
            user,
            user,
        )
        .expect("repo issuance");
        ledger
            .create_grant(&issuance, RepositoryGrantPermissions::Use, &instant(T0))
            .expect("repo grant");
    }
    let (lease_id, fencing_token) = {
        let mut occupancy = storage.client_occupancy_ledger().expect("ledger");
        let claim = OccupancyClaim::try_new(
            canonical_id("ocl", seed + 5),
            &node,
            user,
            canonical_id("req", seed + 6),
        )
        .expect("claim");
        let lease = occupancy.atomic_claim(&claim, &instant(T0)).expect("claim");
        let occupied = occupancy
            .record_acknowledgement(
                &lease.occupancy_lease_id,
                lease.fencing_token,
                None,
                &instant(T0),
            )
            .expect("ack");
        assert_eq!(occupied.state, OccupancyLeaseState::Occupied);
        (occupied.occupancy_lease_id, occupied.fencing_token)
    };
    (node, instance, lease_id, fencing_token, binding)
}

/// The device identities one staged stage-run anchor exposes to the test.
struct StageAnchor {
    worker_launch_grant_id: String,
    worker_session_id: String,
}

/// Issues one live launch grant anchored to the exact (product session,
/// stage run) pair, exactly as the Client's two-phase scheduler would.
#[allow(clippy::too_many_arguments)]
fn stage_anchor(
    storage: &mut SqliteStorage,
    seed: u64,
    node: &str,
    instance: &str,
    user: &str,
    lease_id: &str,
    fencing_token: u64,
    binding: &str,
    product_session_id: &str,
    stage_run_id: &str,
) -> StageAnchor {
    let issuance = LaunchGrantIssuance::try_new(
        ulid_id("wlg", seed),
        node,
        instance,
        user,
        lease_id,
        fencing_token,
        binding,
        ulid_id("ws", seed + 1),
        ulid_id("wkr", seed + 2),
        ulid_id("winst", seed + 3),
        DIGEST,
        Some(product_session_id.to_owned()),
        Some(stage_run_id.to_owned()),
        instant("2100-01-01T00:00:00.000Z"),
    )
    .expect("issuance");
    let grant = WorkerLaunchGrantService::new(storage)
        .issue(&issuance, &instant(T0))
        .unwrap_or_else(|error| panic!("issue anchor grant (seed {seed}): {error:?}"));
    StageAnchor {
        worker_launch_grant_id: grant.worker_launch_grant_id,
        worker_session_id: grant.worker_session_id,
    }
}

// ---- Delivery stage job staging (what the dispatcher commits) --------------

/// Submits one Delivery stage job exactly like the canonical
/// `LocalExecutionJobDispatcher`: the immutable generated `ExecutionJob` in
/// the dispatch payload, the delivery scope, and the stage run reservation.
#[allow(clippy::too_many_arguments)]
fn stage_role_job(
    storage: &mut SqliteStorage,
    seed: u64,
    role: &str,
    write_mode: ExecutionWorkspaceWriteMode,
) -> (ExecutionJobId, StageRunId, ProductSessionId) {
    let delivery_id = DeliveryId(ulid_id("dlv", seed));
    let stage_run_id = StageRunId(ulid_id("run", seed + 1));
    let product_session_id = ProductSessionId(ulid_id("psn", seed + 2));
    let job_id = ExecutionJobId(ulid_id("job", seed + 3));
    let job = ExecutionJob {
        attempt: 1,
        execution_profile: role.to_owned(),
        goal: "Execute the exact sealed stage goal.".to_owned(),
        job_id: job_id.clone(),
        limits: ExecutionLimits {
            deadline_at: instant("2026-09-05T12:00:00.000Z"),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: 3_600,
        },
        payload_digest: Sha256Digest(format!("sha256:{seed:064}")),
        scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
            delivery_id,
            delivery_task_id: None,
            kind: DeliveryStageExecutionScopeKind::DeliveryStage,
            product_session_id: product_session_id.clone(),
            rework_authorization: None,
            stage_run_id: stage_run_id.clone(),
        }),
        stage_input: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            repository_id: RepositoryId(canonical_id("rep", 1)),
            write_mode,
        },
    };
    let dispatch_payload = serde_json::to_vec(&job).expect("encode ExecutionJob");
    let scheduler_scope = repository_scope(1);
    let submission = ExecutionJobSubmission {
        scope: ExecutionQueueScope {
            organization_id: scheduler_scope.organization_id.clone(),
            workspace_id: scheduler_scope.workspace_id.clone(),
            project_id: scheduler_scope.project_id.clone(),
            repository_id: scheduler_scope.repository_id.clone(),
            product_session_id: product_session_id.clone(),
            delivery_id: Some(DeliveryId(ulid_id("dlv", seed))),
        },
        job_id: job_id.clone(),
        request_id: RequestId(ulid_id("req", seed + 4)),
        payload_digest: job.payload_digest.clone(),
        dispatch_payload,
        attempt: 1,
        dependencies: Vec::new(),
        stage_run_id: Some(stage_run_id.clone()),
        submitted_at: instant(T0),
    };
    storage
        .execution_queue()
        .expect("queue")
        .submit(&submission)
        .expect("submit stage job");
    (job_id, stage_run_id, product_session_id)
}

fn register_local_worker(storage: &mut SqliteStorage, seed: u64) -> (WorkerId, WorkerInstanceId) {
    let worker_id = WorkerId(canonical_id("wrk", seed));
    let worker_instance_id = WorkerInstanceId(canonical_id("wki", seed));
    let request = WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "strongflow-device-test".to_owned(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".to_owned()],
        capability_digest: Sha256Digest(format!("sha256:{}", "0".repeat(64))),
        security_zone: "local".to_owned(),
        max_slots: 4,
        message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
        request_id: RequestId(canonical_id("req", seed)),
        sent_at: instant(T0),
        started_at: instant(T0),
        worker_id: worker_id.clone(),
        worker_instance_id: worker_instance_id.clone(),
    };
    let mut registry = storage.execution_registry().expect("registry");
    let receipt = registry.register_worker(&request).expect("register");
    assert!(matches!(
        receipt.status,
        WorkerRegistrationStatus::Accepted | WorkerRegistrationStatus::Duplicate
    ));
    (worker_id, worker_instance_id)
}

fn claim_locally(
    storage: &mut SqliteStorage,
    request: u64,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Option<ExecutionJobId> {
    let claim = RepositorySchedulerClaimRequest {
        scope: repository_scope(1),
        request_id: RequestId(canonical_id("req", request)),
        scheduler_generation: "gen-strongflow-device-test".to_owned(),
        worker_id: worker_id.clone(),
        worker_instance_id: worker_instance_id.clone(),
        issued_at: instant(T0),
        expires_at: instant("2026-09-04T13:00:00.000Z"),
    };
    RepositoryExecutionScheduler::new(storage)
        .claim_next(&claim)
        .expect("claim next job")
        .map(|dispatch| dispatch.job.job_id)
}

fn queued_jobs(storage: &mut SqliteStorage) -> Vec<ExecutionJobId> {
    storage
        .repository_scheduler()
        .expect("scheduler")
        .list_jobs(&repository_scope(1), &[ExecutionJobState::Queued])
        .expect("queued jobs")
        .into_iter()
        .map(|job| job.job_id)
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn every_role_dispatches_to_its_own_device_worker_session_within_capacity() {
    let mut storage = SqliteStorage::open(temporary_root("role-matrix")).expect("storage");
    let (node, instance, lease_id, fencing_token, binding) =
        stage_device_fixture(&mut storage, 100, HOLDER);
    // One role stage job per Delivery role with its canonical write mode:
    // writer roles execute on an isolated candidate, read-only roles on a
    // frozen read-only workspace.
    let planner = stage_role_job(
        &mut storage,
        200,
        "planner",
        ExecutionWorkspaceWriteMode::ReadOnly,
    );
    let executor = stage_role_job(
        &mut storage,
        210,
        "executor",
        ExecutionWorkspaceWriteMode::Candidate,
    );
    let reviewer = stage_role_job(
        &mut storage,
        220,
        "reviewer",
        ExecutionWorkspaceWriteMode::ReadOnly,
    );
    let verifier = stage_role_job(
        &mut storage,
        230,
        "verifier",
        ExecutionWorkspaceWriteMode::ReadOnly,
    );
    let roles = [
        ("planner", &planner),
        ("executor", &executor),
        ("reviewer", &reviewer),
        ("verifier", &verifier),
    ];
    // The Client launches one WorkerSession per role stage run, and the
    // routing dispatches each job to exactly its own session.
    for (index, (role, (job_id, stage_run_id, product_session_id))) in roles.iter().enumerate() {
        let seed = 300 + u64::try_from(index).expect("index") * 10;
        let anchor = stage_anchor(
            &mut storage,
            seed,
            &node,
            &instance,
            HOLDER,
            &lease_id,
            fencing_token,
            &binding,
            product_session_id.0.as_str(),
            stage_run_id.0.as_str(),
        );
        let now = instant("2026-09-04T12:05:00.000Z");
        let dispatch =
            dispatch_stage_to_device_worker(&mut storage, Some(HOLDER), stage_run_id, &now)
                .expect("role dispatch")
                .expect("device-anchored stage dispatches");
        // The role identity travels on the facts, together with the exact
        // device worker identities of this stage's own launch.
        assert_eq!(dispatch.facts.role.as_deref(), Some(*role));
        assert_eq!(dispatch.facts.worker_session_id, anchor.worker_session_id);
        assert_eq!(
            dispatch.facts.worker_launch_grant_id,
            anchor.worker_launch_grant_id
        );
        assert_eq!(dispatch.facts.holder_user_id, HOLDER);
        assert_eq!(
            dispatch.facts.product_session_id.as_deref(),
            Some(product_session_id.0.as_str())
        );
        assert_eq!(
            dispatch.facts.stage_run_id.as_deref(),
            Some(stage_run_id.0.as_str())
        );
        // The binding is the stage's own WorkerSession, bound once.
        assert_eq!(dispatch.binding.worker_session_id, anchor.worker_session_id);
        assert_eq!(dispatch.binding.state.as_str(), "bound");
        // The reservation runs under the StrongFlow device pool and stays
        // queued for the device worker; writer roles reserve an isolated
        // candidate worktree, read-only roles stay read-only.
        let reservation = storage
            .execution_admission()
            .expect("admission")
            .load_reservation_by_job(job_id)
            .expect("reservation lookup")
            .expect("the dispatched role holds a reservation");
        assert_eq!(reservation.state, ExecutionReservationState::Queued);
        assert_eq!(reservation.user_id, UserId(HOLDER.to_owned()));
        if *role == "executor" || *role == "remediator" {
            assert!(matches!(
                reservation.repository_access,
                ExecutionRepositoryAccess::IsolatedWrite { .. }
            ));
        } else {
            assert_eq!(
                reservation.repository_access,
                ExecutionRepositoryAccess::ReadOnly
            );
        }
    }
    // Four concurrent role dispatches stay within the Client's
    // worker-session capacity, each anchored to its own launch.
    let capacity = DeviceExecutionBindingService::new(&mut storage)
        .capacity_snapshot(&node)
        .expect("capacity")
        .expect("node");
    assert_eq!(capacity.reserved_worker_sessions, 4);
    assert_eq!(capacity.bound_bindings, 4);
    assert_eq!(capacity.free_worker_sessions, 0);
    // No role ever reuses another role's WorkerSession: the four dispatch
    // facts name four distinct sessions.
    let mut sessions = roles
        .iter()
        .map(|(_, (job_id, _, _))| {
            DeviceExecutionBindingService::new(&mut storage)
                .facts(job_id.0.as_str())
                .expect("facts")
                .expect("dispatched")
                .worker_session_id
        })
        .collect::<Vec<_>>();
    sessions.sort();
    let distinct = sessions.clone();
    sessions.dedup();
    assert_eq!(sessions.len(), 4);
    assert_eq!(distinct.len(), 4);
    assert_eq!(queued_jobs(&mut storage).len(), 4);
}

#[test]
fn device_routed_stage_jobs_are_excluded_from_local_claims() {
    let mut storage = SqliteStorage::open(temporary_root("local-exclusion")).expect("storage");
    let (node, instance, lease_id, fencing_token, binding) =
        stage_device_fixture(&mut storage, 400, HOLDER);
    // One anchored executor stage and one unanchored reviewer stage in the
    // same repository scope.
    let (job_id, stage_run_id, product_session_id) = stage_role_job(
        &mut storage,
        500,
        "executor",
        ExecutionWorkspaceWriteMode::Candidate,
    );
    let (_, unanchored_stage_run, _) = stage_role_job(
        &mut storage,
        510,
        "reviewer",
        ExecutionWorkspaceWriteMode::ReadOnly,
    );
    let anchor = stage_anchor(
        &mut storage,
        600,
        &node,
        &instance,
        HOLDER,
        &lease_id,
        fencing_token,
        &binding,
        product_session_id.0.as_str(),
        stage_run_id.0.as_str(),
    );
    // The anchored stage routes to the launched device WorkerSession; the
    // unanchored stage keeps the supervised local path unchanged.
    let dispatch = dispatch_stage_to_device_worker(
        &mut storage,
        Some(HOLDER),
        &stage_run_id,
        &instant("2026-09-04T12:05:00.000Z"),
    )
    .expect("anchored stage dispatches")
    .expect("device dispatch");
    assert_eq!(dispatch.facts.worker_session_id, anchor.worker_session_id);
    assert_eq!(dispatch.facts.role.as_deref(), Some("executor"));
    assert!(
        dispatch_stage_to_device_worker(
            &mut storage,
            Some(HOLDER),
            &unanchored_stage_run,
            &instant("2026-09-04T12:05:00.000Z"),
        )
        .expect("unanchored stage keeps the local path")
        .is_none(),
        "a stage run without an anchor must not dispatch"
    );
    // The local queue exclusion (FLOW-100.4) applies unchanged: the local
    // embedded worker claims exactly the unanchored stage job and can never
    // claim the device-owned one.
    let (worker_id, worker_instance_id) = register_local_worker(&mut storage, 700);
    let first_claim = claim_locally(&mut storage, 701, &worker_id, &worker_instance_id);
    assert_ne!(first_claim.as_ref(), Some(&job_id));
    let local_job = claim_locally(&mut storage, 702, &worker_id, &worker_instance_id);
    assert!(
        local_job.iter().all(|claimed| claimed.0 != job_id.0),
        "the device-owned stage job must stay unclaimable locally"
    );
    // The device-owned job still waits for its device worker, not for a
    // local slot.
    let queued = queued_jobs(&mut storage);
    assert!(queued.iter().any(|job| job == &job_id));
}

#[test]
fn a_dead_anchor_refuses_the_dispatch_without_binding() {
    let mut storage = SqliteStorage::open(temporary_root("dead-anchor")).expect("storage");
    let (node, instance, lease_id, fencing_token, binding) =
        stage_device_fixture(&mut storage, 800, HOLDER);
    let (job_id, stage_run_id, product_session_id) = stage_role_job(
        &mut storage,
        900,
        "verifier",
        ExecutionWorkspaceWriteMode::ReadOnly,
    );
    let anchor = stage_anchor(
        &mut storage,
        1000,
        &node,
        &instance,
        HOLDER,
        &lease_id,
        fencing_token,
        &binding,
        product_session_id.0.as_str(),
        stage_run_id.0.as_str(),
    );
    // The launch is revoked before the device ever accepted it.
    WorkerLaunchGrantService::new(&mut storage)
        .revoke(
            &anchor.worker_launch_grant_id,
            HOLDER,
            Some("test revoked"),
            &instant("2026-09-04T12:02:30.000Z"),
        )
        .expect("revoke anchor grant");

    let error = dispatch_stage_to_device_worker(
        &mut storage,
        Some(HOLDER),
        &stage_run_id,
        &instant("2026-09-04T12:05:00.000Z"),
    )
    .expect_err("a dead launch anchor must refuse the dispatch");
    assert_eq!(
        error.kind(),
        StrongflowDeviceDispatchErrorKind::AnchorNotLive
    );
    // The dead anchor never binds a worker session and never attaches facts.
    assert!(
        DeviceExecutionBindingService::new(&mut storage)
            .snapshot(&anchor.worker_session_id)
            .expect("binding lookup")
            .is_none()
    );
    assert!(
        DeviceExecutionBindingService::new(&mut storage)
            .facts(job_id.0.as_str())
            .expect("facts lookup")
            .is_none()
    );
    assert_eq!(queued_jobs(&mut storage).len(), 1);
}

#[test]
fn a_gate_denial_routes_nothing() {
    let mut storage = SqliteStorage::open(temporary_root("gate-denial")).expect("storage");
    let (node, instance, lease_id, fencing_token, binding) =
        stage_device_fixture(&mut storage, 1100, HOLDER);
    let (job_id, stage_run_id, product_session_id) = stage_role_job(
        &mut storage,
        1200,
        "planner",
        ExecutionWorkspaceWriteMode::ReadOnly,
    );
    let anchor = stage_anchor(
        &mut storage,
        1300,
        &node,
        &instance,
        HOLDER,
        &lease_id,
        fencing_token,
        &binding,
        product_session_id.0.as_str(),
        stage_run_id.0.as_str(),
    );
    // A non-holder may not dispatch to the holder's device: the gate denial
    // carries the central wire code and routes nothing.
    let error = dispatch_stage_to_device_worker(
        &mut storage,
        Some(MEMBER),
        &stage_run_id,
        &instant("2026-09-04T12:05:00.000Z"),
    )
    .expect_err("a non-holder must be refused");
    assert_eq!(error.kind(), StrongflowDeviceDispatchErrorKind::GateDenied);
    let denial = error.gate_denial().expect("gate denial facts");
    assert_eq!(denial.wire_code(), "ACCESS_DENIED");
    assert_eq!(denial.http_status(), 403);
    assert!(
        DeviceExecutionBindingService::new(&mut storage)
            .snapshot(&anchor.worker_session_id)
            .expect("binding lookup")
            .is_none()
    );
    assert!(
        DeviceExecutionBindingService::new(&mut storage)
            .facts(job_id.0.as_str())
            .expect("facts lookup")
            .is_none()
    );
    // The holder dispatches the same stage afterwards.
    let dispatch = dispatch_stage_to_device_worker(
        &mut storage,
        Some(HOLDER),
        &stage_run_id,
        &instant("2026-09-04T12:06:00.000Z"),
    )
    .expect("holder dispatch")
    .expect("device dispatch");
    assert_eq!(dispatch.facts.worker_session_id, anchor.worker_session_id);
}

#[test]
fn the_dispatch_replays_exactly_without_new_facts() {
    let mut storage = SqliteStorage::open(temporary_root("dispatch-replay")).expect("storage");
    let (node, instance, lease_id, fencing_token, binding) =
        stage_device_fixture(&mut storage, 1400, HOLDER);
    let (job_id, stage_run_id, product_session_id) = stage_role_job(
        &mut storage,
        1500,
        "executor",
        ExecutionWorkspaceWriteMode::Candidate,
    );
    let anchor = stage_anchor(
        &mut storage,
        1600,
        &node,
        &instance,
        HOLDER,
        &lease_id,
        fencing_token,
        &binding,
        product_session_id.0.as_str(),
        stage_run_id.0.as_str(),
    );
    let first = dispatch_stage_to_device_worker(
        &mut storage,
        Some(HOLDER),
        &stage_run_id,
        &instant("2026-09-04T12:05:00.000Z"),
    )
    .expect("first dispatch")
    .expect("device dispatch");
    assert_eq!(first.facts.worker_session_id, anchor.worker_session_id);
    // An exact command replay re-runs the routing and finds its durable
    // receipts: same binding, same facts, nothing new.
    let replay = dispatch_stage_to_device_worker(
        &mut storage,
        Some(HOLDER),
        &stage_run_id,
        &instant("2026-09-04T12:07:00.000Z"),
    )
    .expect("replay dispatch")
    .expect("device dispatch");
    assert_eq!(first, replay);
    assert_eq!(first.binding.bound_at, instant("2026-09-04T12:05:00.000Z"));
    assert_eq!(first.facts.attached_at, instant("2026-09-04T12:05:00.000Z"));
    assert_eq!(
        queued_jobs(&mut storage),
        vec![job_id],
        "the replay must not duplicate the stage job"
    );
}

#[test]
fn an_anchor_of_another_product_session_is_refused_as_corrupt() {
    let mut storage = SqliteStorage::open(temporary_root("foreign-session")).expect("storage");
    let (node, instance, lease_id, fencing_token, binding) =
        stage_device_fixture(&mut storage, 1700, HOLDER);
    let (job_id, stage_run_id, _) = stage_role_job(
        &mut storage,
        1800,
        "reviewer",
        ExecutionWorkspaceWriteMode::ReadOnly,
    );
    // The grant anchors the right stage run but a foreign product session:
    // the identity join must refuse instead of routing.
    let anchor = stage_anchor(
        &mut storage,
        1900,
        &node,
        &instance,
        HOLDER,
        &lease_id,
        fencing_token,
        &binding,
        &canonical_id("psn", 987_654),
        stage_run_id.0.as_str(),
    );
    let error = dispatch_stage_to_device_worker(
        &mut storage,
        Some(HOLDER),
        &stage_run_id,
        &instant("2026-09-04T12:05:00.000Z"),
    )
    .expect_err("a foreign session anchor must be refused");
    assert_eq!(
        error.kind(),
        StrongflowDeviceDispatchErrorKind::CorruptState
    );
    // The refusal binds nothing and attaches nothing.
    assert!(
        DeviceExecutionBindingService::new(&mut storage)
            .snapshot(&anchor.worker_session_id)
            .expect("binding lookup")
            .is_none()
    );
    assert!(
        DeviceExecutionBindingService::new(&mut storage)
            .facts(job_id.0.as_str())
            .expect("facts lookup")
            .is_none()
    );
}

#[test]
fn the_role_column_migrates_a_preexisting_facts_table() {
    let root = temporary_root("facts-role-migration");
    std::fs::create_dir_all(&root).expect("create root");
    // A database written by the pre-`FLOW-100.5` schema: the facts table
    // exists without the nullable `role` column.
    {
        let connection =
            rusqlite::Connection::open(root.join("control-plane.sqlite3")).expect("open database");
        connection
            .execute_batch(
                "CREATE TABLE device_execution_reservation_facts (
                    job_id TEXT PRIMARY KEY NOT NULL,
                    client_node_id TEXT NOT NULL,
                    client_instance_id TEXT NOT NULL,
                    holder_user_id TEXT NOT NULL,
                    repository_binding_id TEXT NOT NULL,
                    occupancy_lease_id TEXT NOT NULL,
                    occupancy_fencing_token INTEGER NOT NULL,
                    worker_launch_grant_id TEXT NOT NULL,
                    worker_session_id TEXT NOT NULL,
                    worker_id TEXT NOT NULL,
                    worker_instance_id TEXT NOT NULL,
                    product_session_id TEXT,
                    stage_run_id TEXT,
                    attached_at TEXT NOT NULL,
                    revision INTEGER NOT NULL CHECK (revision = 1)
                );",
            )
            .expect("old facts table");
    }
    // The routing still dispatches over the migrated table: the ledger open
    // appends the `role` column exactly where the fresh schema declares it.
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let (node, instance, lease_id, fencing_token, binding) =
        stage_device_fixture(&mut storage, 2000, HOLDER);
    let (job_id, stage_run_id, product_session_id) = stage_role_job(
        &mut storage,
        2100,
        "executor",
        ExecutionWorkspaceWriteMode::Candidate,
    );
    let anchor = stage_anchor(
        &mut storage,
        2200,
        &node,
        &instance,
        HOLDER,
        &lease_id,
        fencing_token,
        &binding,
        product_session_id.0.as_str(),
        stage_run_id.0.as_str(),
    );
    let dispatch = dispatch_stage_to_device_worker(
        &mut storage,
        Some(HOLDER),
        &stage_run_id,
        &instant("2026-09-04T12:05:00.000Z"),
    )
    .expect("dispatch over the migrated facts table")
    .expect("device dispatch");
    assert_eq!(dispatch.facts.role.as_deref(), Some("executor"));
    assert_eq!(dispatch.facts.worker_session_id, anchor.worker_session_id);
    assert_eq!(dispatch.facts.job_id, job_id.0);
    let _ = std::fs::remove_dir_all(&root);
}
