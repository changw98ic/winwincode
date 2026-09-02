// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, FencingToken, Instant, LeaseId, RequestId, Sha256Digest,
    WorkerId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_storage::{
    DispatchResultRequest, DispatchResultStatus, EXECUTION_PROTOCOL_VERSION, ExecutionLeaseClaim,
    ProductStateStorage, SqliteStorage, WorkerAuthenticationIdentity, WorkerPlatform,
    WorkerRegistrationRequest,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn instant(second: u64) -> Instant {
    Instant(format!("2027-01-15T08:00:{second:02}.000Z"))
}

fn fixture() -> (
    std::path::PathBuf,
    ExecutionLeaseClaim,
    DispatchResultRequest,
) {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "winwincode-dispatch-authority-{}-{suffix}",
        std::process::id()
    ));
    let lease = ExecutionLeaseClaim {
        expires_at: instant(50),
        fencing_token: FencingToken("7".to_owned()),
        issued_at: instant(1),
        job_id: ExecutionJobId(id("job", suffix)),
        lease_id: LeaseId(id("lse", suffix)),
        message_id: ExecutionMessageId(id("xmsg", suffix)),
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        request_id: RequestId(id("req", suffix)),
        worker_id: WorkerId(id("wrk", suffix)),
        worker_instance_id: WorkerInstanceId(id("wki", suffix)),
        attempt: 1,
    };
    let result = DispatchResultRequest {
        checked_at: instant(3),
        expires_at: lease.expires_at.clone(),
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        message_id: ExecutionMessageId(id("xmsg", suffix + 100)),
        payload_digest: lease.payload_digest.clone(),
        request_id: lease.request_id.clone(),
        sent_at: instant(2),
        status: DispatchResultStatus::Accepted,
        attempt: lease.attempt,
        error: None,
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id: Some(WorkerSessionId(id("wsn", suffix))),
    };
    (root, lease, result)
}

fn install_lease(storage: &mut SqliteStorage, lease: &ExecutionLeaseClaim) {
    let mut registry = storage.execution_registry().expect("registry");
    registry
        .register_worker(&WorkerRegistrationRequest {
            authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                control_plane_principal: "embedded-control-plane".to_owned(),
            },
            protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
            platform: WorkerPlatform::Aarch64AppleDarwin,
            capabilities: vec!["codex".to_owned()],
            capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            security_zone: "local".to_owned(),
            max_slots: 1,
            message_id: ExecutionMessageId(id("xmsg", 9_001)),
            request_id: RequestId(id("req", 9_001)),
            sent_at: instant(1),
            started_at: instant(0),
            worker_id: lease.worker_id.clone(),
            worker_instance_id: lease.worker_instance_id.clone(),
        })
        .expect("worker registration");
    registry.claim_execution_job(lease).expect("lease claim");
}

#[test]
fn accepted_dispatch_is_the_only_durable_session_authority_and_survives_restart() {
    let (root, lease, result) = fixture();
    let mut storage = SqliteStorage::open(&root).expect("storage");
    install_lease(&mut storage, &lease);
    {
        let mut registry = storage.execution_registry().expect("registry");
        assert_eq!(
            registry
                .load_dispatch_authority(&lease.job_id)
                .expect("authority before dispatch"),
            None
        );

        let receipt = registry
            .record_dispatch_result(&result)
            .expect("accepted dispatch result");
        assert_eq!(receipt.status, DispatchResultStatus::Accepted);
        let authority = registry
            .load_dispatch_authority(&lease.job_id)
            .expect("authority load")
            .expect("accepted authority");
        assert_eq!(authority.lease().job_id, lease.job_id);
        assert_eq!(authority.lease().lease_id, lease.lease_id);
        assert_eq!(authority.lease().fencing_token, lease.fencing_token);
        assert_eq!(
            authority.worker_session_id(),
            result.worker_session_id.as_ref().expect("session")
        );
        assert_eq!(authority.dispatch_request_id(), &result.request_id);
        assert_eq!(authority.accepted_at(), &result.checked_at);
    }
    Box::new(storage).close().expect("storage close");
    let mut restarted = SqliteStorage::open(&root).expect("storage restart");
    let authority = restarted
        .execution_registry()
        .expect("restarted registry")
        .load_dispatch_authority(&lease.job_id)
        .expect("restarted authority load")
        .expect("restarted authority");
    assert_eq!(
        authority.worker_session_id(),
        result.worker_session_id.as_ref().expect("session")
    );
    Box::new(restarted).close().expect("restarted close");
    fs::remove_dir_all(root).expect("fixture release");
}

#[test]
fn rejected_dispatch_result_performs_zero_authority_writes() {
    let (root, lease, mut result) = fixture();
    let mut storage = SqliteStorage::open(&root).expect("storage");
    install_lease(&mut storage, &lease);
    result.status = DispatchResultStatus::RejectedCapacity;
    result.worker_session_id = None;
    storage
        .execution_registry()
        .expect("registry")
        .record_dispatch_result(&result)
        .expect("rejected result");
    assert!(
        storage
            .execution_registry()
            .expect("registry")
            .load_dispatch_authority(&lease.job_id)
            .expect("authority read")
            .is_none()
    );
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("fixture release");
}
