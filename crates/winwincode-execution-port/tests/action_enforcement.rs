// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::{
    CodexThreadId, ExecutionJobId, ExecutionMessageId, FencingToken, Instant, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RepositoryScope,
    RepositoryScopeKind, RequestId, SchemaVersion, SessionIdentity, Sha256Digest, UserActor,
    UserActorKind, UserId, WorkRunId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::action_enforcement::{
    ActionEnforcementError, ActionEnforcementIssuer, ActionEnforcementSigningKey,
    ActionReceiptClaim, ActionReceiptUseError, ActionReceiptUseStore, FileActionReceiptUseStore,
    action_enforcement_facts, prepare_action_enforcement_request,
};
use winwincode_execution_port::action_gateway::{WorkerActionAuthority, WorkerActionRequest};
use winwincode_execution_port::action_normalizer::{
    ActionIntent, ActionObject, ActionOperation, ActionPurpose, ActionRisk, ActionScope,
    ShellRequest, ToolRequest, normalize_action,
};
use winwincode_execution_port::generated::{
    ActionEnforcementDecision, ActionEnforcementReceiptMessage,
    ActionEnforcementReceiptMessageKind, ExecutionLeaseStamp, ExecutionPortMessage,
};
use winwincode_execution_port::transport::FrameDirection;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
const NOW: &str = "2027-01-15T08:00:02.000Z";

fn id(prefix: &str, suffix: char) -> String {
    format!("{prefix}_{}", suffix.to_string().repeat(26))
}

fn action() -> WorkerActionRequest {
    WorkerActionRequest {
        invocation_request_id: RequestId(id("req", 'A')),
        authority: WorkerActionAuthority {
            lease: ExecutionLeaseStamp {
                attempt: 1,
                expires_at: Instant("2027-01-15T08:05:00.000Z".to_owned()),
                fencing_token: FencingToken("7".to_owned()),
                issued_at: Instant("2027-01-15T08:00:00.000Z".to_owned()),
                job_id: ExecutionJobId(id("job", 'A')),
                lease_id: LeaseId(id("lse", 'A')),
                worker_id: WorkerId(id("wrk", 'A')),
                worker_instance_id: WorkerInstanceId(id("wki", 'A')),
            },
            worker_session_id: WorkerSessionId(id("wsn", 'A')),
            session_identity: SessionIdentity {
                codex_thread_id: CodexThreadId(id("cdx", 'A')),
                product_session_id: ProductSessionId(id("psn", 'A')),
                work_run_id: Some(WorkRunId(id("wrn", 'A'))),
                worker_session_id: WorkerSessionId(id("wsn", 'A')),
            },
            envelope: winwincode_execution_port::action_gateway::ExecutionEnvelopeToken {
                version: 1,
                digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            },
        },
        intent: ActionIntent {
            object: ActionObject::Test,
            operation: ActionOperation::Execute,
            intent: ActionPurpose::Verify,
            scope: ActionScope::Repository,
            targets: vec!["cwd:.".to_owned(), "argv:[\"cargo\",\"test\"]".to_owned()],
            requirement_refs: vec!["REQ-ACTION".to_owned()],
            plan_refs: vec!["PLAN-ACTION".to_owned()],
            expected_effect: "run the exact test command".to_owned(),
            scope_delta: None,
            rollback: Some("stop the test process".to_owned()),
            executor_risk: ActionRisk::Low,
        },
        request: ToolRequest::Shell(ShellRequest {
            program: "cargo".to_owned(),
            args: vec!["test".to_owned()],
            working_directory: ".".to_owned(),
        }),
    }
}

fn issuer() -> ActionEnforcementIssuer {
    ActionEnforcementIssuer::new(
        ActionEnforcementSigningKey::from_bytes([13_u8; 32]).expect("signing key"),
    )
}

fn receipt(action: &WorkerActionRequest) -> ActionEnforcementReceiptMessage {
    let normalization = normalize_action(&action.intent, &action.request).expect("normalization");
    let facts = action_enforcement_facts(&normalization).expect("facts");
    let mut receipt = ActionEnforcementReceiptMessage {
        actor: UserActor {
            id: UserId(id("usr", 'A')),
            kind: UserActorKind::User,
        },
        decision: ActionEnforcementDecision::Permit,
        evaluated_at: Instant(NOW.to_owned()),
        evaluation_sha256: Sha256Digest(format!("sha256:{}", "d".repeat(64))),
        job_id: action.authority.lease.job_id.clone(),
        kind: ActionEnforcementReceiptMessageKind::ActionEnforcementReceipt,
        lease: action.authority.lease.clone(),
        matched_condition_sha256: facts.matched_condition_sha256,
        message_id: ExecutionMessageId(id("xmsg", 'A')),
        policy_kind: facts.policy_kind,
        policy_mode: None,
        policy_version: None,
        receipt_signature: Sha256Digest(format!("sha256:{}", "0".repeat(64))),
        request_id: action.invocation_request_id.clone(),
        resource: facts.resource,
        schema_version: SchemaVersion::WinwincodeV1,
        scope: RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId(id("org", 'A')),
            workspace_id: WorkspaceId(id("wsp", 'A')),
            project_id: ProjectId(id("prj", 'A')),
            repository_id: RepositoryId(id("rep", 'A')),
        },
        sent_at: Instant(NOW.to_owned()),
        session_identity: action.authority.session_identity.clone(),
        subject_sha256: facts.subject_sha256,
        worker_session_id: action.authority.worker_session_id.clone(),
    };
    issuer().sign(&mut receipt).expect("signed receipt");
    receipt
}

#[test]
fn current_lease_verifies_late_evaluation_without_changing_original_receipt() {
    let action = action();
    let mut signed = receipt(&action);
    signed.evaluated_at = Instant("2027-01-15T08:06:00.000Z".into());
    signed.sent_at = signed.evaluated_at.clone();
    issuer()
        .sign(&mut signed)
        .expect("sign actual evaluation time");
    let mut current = action.authority.lease.clone();
    current.expires_at = Instant("2027-01-15T08:10:00.000Z".into());
    assert!(
        issuer()
            .verifier()
            .verify_outcome(&action, &signed)
            .is_err()
    );
    let verifier = issuer().verifier();
    verifier
        .verify_outcome_with_current_lease(&action, &signed, &current)
        .expect("independently verified renewal allows original request");
    for field in 0..9 {
        let mut changed = current.clone();
        match field {
            0 => changed.job_id = ExecutionJobId(id("job", 'B')),
            1 => changed.lease_id = LeaseId(id("lse", 'B')),
            2 => changed.worker_id = WorkerId(id("wrk", 'B')),
            3 => changed.worker_instance_id = WorkerInstanceId(id("wki", 'B')),
            4 => changed.attempt += 1,
            5 => changed.fencing_token = FencingToken("8".into()),
            6 => changed.issued_at = Instant("2027-01-15T08:00:01.000Z".into()),
            7 => changed.expires_at = Instant("2027-01-15T08:04:00.000Z".into()),
            _ => changed.expires_at = signed.evaluated_at.clone(),
        }
        assert!(
            verifier
                .verify_outcome_with_current_lease(&action, &signed, &changed)
                .is_err()
        );
    }
    let mut forged = signed.clone();
    forged.resource.push_str("changed");
    assert!(
        verifier
            .verify_outcome_with_current_lease(&action, &forged, &current)
            .is_err()
    );
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "winwincode-renewed-action-{}-{suffix}",
        std::process::id()
    ));
    let mut store = FileActionReceiptUseStore::open(&root).expect("receipt store");
    assert_eq!(
        store.claim(&signed).expect("first claim"),
        ActionReceiptClaim::Fresh
    );
    assert_eq!(
        store.claim(&signed).expect("replay claim"),
        ActionReceiptClaim::AlreadyConsumed
    );
    fs::remove_dir_all(root).expect("remove store");
}

#[test]
fn request_and_receipt_bind_exact_action_invocation_and_authority() {
    let action = action();
    let request = prepare_action_enforcement_request(
        ExecutionMessageId(id("xmsg", 'B')),
        Instant(NOW.to_owned()),
        &action,
    )
    .expect("enforcement request");
    let receipt = receipt(&action);

    assert_eq!(request.request_id, action.invocation_request_id);
    assert_eq!(
        FrameDirection::for_message(&ExecutionPortMessage::ActionEnforcementRequestMessage(
            request.clone(),
        )),
        Ok(FrameDirection::WorkerToControlPlane)
    );
    assert_eq!(
        FrameDirection::for_message(&ExecutionPortMessage::ActionEnforcementReceiptMessage(
            receipt.clone(),
        )),
        Ok(FrameDirection::ControlPlaneToWorker)
    );
    assert_eq!(request.subject_sha256, receipt.subject_sha256);
    assert!(request.resource.starts_with("action:shell:sha256:"));
    assert!(!request.resource.contains("cargo"));
    assert!(!request.resource.contains("test"));
    assert_eq!(
        request.matched_condition_sha256,
        receipt.matched_condition_sha256
    );
    issuer()
        .verifier()
        .verify(&action, &receipt)
        .expect("receipt verification");

    let mut changed = action.clone();
    changed.request = ToolRequest::Shell(ShellRequest {
        program: "cargo".to_owned(),
        args: vec!["test".to_owned(), "--release".to_owned()],
        working_directory: ".".to_owned(),
    });
    changed.intent.targets = vec![
        "cwd:.".to_owned(),
        "argv:[\"cargo\",\"test\",\"--release\"]".to_owned(),
    ];
    assert_eq!(
        issuer().verifier().verify(&changed, &receipt),
        Err(ActionEnforcementError::ReceiptMismatch)
    );

    let mut cross_tenant = receipt.clone();
    cross_tenant.scope.organization_id = OrganizationId(id("org", 'B'));
    assert_eq!(
        issuer().verifier().verify(&action, &cross_tenant),
        Err(ActionEnforcementError::InvalidSignature)
    );
}

#[test]
fn durable_claim_survives_restart_and_changed_reuse_conflicts() {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "winwincode-action-enforcement-{}-{suffix}",
        std::process::id()
    ));
    let action = action();
    let receipt = receipt(&action);
    let mut store = FileActionReceiptUseStore::open(&root).expect("receipt store");
    assert_eq!(
        store.claim(&receipt).expect("fresh claim"),
        ActionReceiptClaim::Fresh
    );
    drop(store);

    let mut restarted = FileActionReceiptUseStore::open(&root).expect("receipt store restart");
    assert_eq!(
        restarted.claim(&receipt).expect("exact replay"),
        ActionReceiptClaim::AlreadyConsumed
    );
    let mut changed = receipt;
    changed.receipt_signature = Sha256Digest(format!("sha256:{}", "f".repeat(64)));
    assert_eq!(
        restarted.claim(&changed),
        Err(ActionReceiptUseError::Conflict)
    );
    fs::remove_dir_all(root).expect("receipt directory release");
}
