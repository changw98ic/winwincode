// SPDX-License-Identifier: Apache-2.0

use std::{cell::Cell, fs, rc::Rc};

use winwincode_domain::{
    CodexThreadId, ExecutionJobId, ExecutionMessageId, FencingToken, Instant, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RepositoryScope,
    RepositoryScopeKind, RequestId, SchemaVersion, SessionIdentity, Sha256Digest, StageRunId,
    UserActor, UserActorKind, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::{
    action_enforcement::{
        ActionEnforcementIssuer, ActionEnforcementSigningKey, action_enforcement_facts,
    },
    action_gateway::{
        ActionGatewayRejectionCode, ActiveWorkerAuthority, CodexToolExecutor,
        DeterministicActionGate, ExecutionEnvelope, ExecutionEnvelopeToken, GateDecision,
        GateInput, PreActionDecisionRecorder, WorkerActionAuthority, WorkerActionGateway,
        WorkerActionRequest,
    },
    action_normalizer::{
        ActionIntent, ActionObject, ActionOperation, ActionPurpose, ActionRisk, ActionScope,
        NetworkRequest, ToolRequest, normalize_action,
    },
    generated::{
        ActionEnforcementDecision, ActionEnforcementReceiptMessage,
        ActionEnforcementReceiptMessageKind, ExecutionLeaseStamp,
    },
};
use winwincode_worker::action_enforcement::DurableWorkerActionEnforcement;

const NOW: &str = "2027-01-15T08:00:02.000Z";

fn id(prefix: &str, value: char) -> String {
    format!("{prefix}_{}", value.to_string().repeat(26))
}

#[derive(Clone)]
struct Policy;
struct Gate;
struct Journal;
struct Executor(Rc<Cell<u64>>);

impl DeterministicActionGate<Policy> for Gate {
    fn decide(&mut self, _input: GateInput<'_, Policy>) -> GateDecision {
        GateDecision::Allow
    }
}

impl PreActionDecisionRecorder<Policy> for Journal {
    type Error = &'static str;

    fn record(
        &mut self,
        _input: GateInput<'_, Policy>,
        _decision: &GateDecision,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl CodexToolExecutor for Executor {
    type Output = ();
    type Error = &'static str;

    fn execute(&mut self, _request: &ToolRequest) -> Result<Self::Output, Self::Error> {
        self.0.set(self.0.get() + 1);
        Ok(())
    }
}

fn lease() -> ExecutionLeaseStamp {
    ExecutionLeaseStamp {
        attempt: 1,
        expires_at: Instant("2027-01-15T08:05:00.000Z".to_owned()),
        fencing_token: FencingToken("7".to_owned()),
        issued_at: Instant("2027-01-15T08:00:00.000Z".to_owned()),
        job_id: ExecutionJobId(id("job", 'A')),
        lease_id: LeaseId(id("lse", 'A')),
        worker_id: WorkerId(id("wrk", 'A')),
        worker_instance_id: WorkerInstanceId(id("wki", 'A')),
    }
}

fn session() -> SessionIdentity {
    SessionIdentity {
        codex_thread_id: CodexThreadId(id("cdx", 'A')),
        product_session_id: ProductSessionId(id("psn", 'A')),
        stage_run_id: Some(StageRunId(id("run", 'A'))),
        worker_session_id: WorkerSessionId(id("wsn", 'A')),
    }
}

fn action() -> WorkerActionRequest {
    WorkerActionRequest {
        invocation_request_id: RequestId(id("req", 'A')),
        authority: WorkerActionAuthority {
            lease: lease(),
            worker_session_id: WorkerSessionId(id("wsn", 'A')),
            session_identity: session(),
            envelope: ExecutionEnvelopeToken {
                version: 1,
                digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            },
        },
        intent: ActionIntent {
            object: ActionObject::ExternalResource,
            operation: ActionOperation::Execute,
            intent: ActionPurpose::Implement,
            scope: ActionScope::External,
            targets: vec!["GET https://api.example.test/v1/items".to_owned()],
            requirement_refs: vec!["REQ-ACTION".to_owned()],
            plan_refs: vec!["PLAN-ACTION".to_owned()],
            expected_effect: "read the approved endpoint".to_owned(),
            scope_delta: None,
            rollback: Some("discard the response".to_owned()),
            executor_risk: ActionRisk::Low,
        },
        request: ToolRequest::Network(NetworkRequest {
            method: "GET".to_owned(),
            url: "https://api.example.test/v1/items?secret=not-normalized".to_owned(),
        }),
    }
}

fn gateway(executions: &Rc<Cell<u64>>) -> WorkerActionGateway<Policy, Gate, Journal, Executor> {
    WorkerActionGateway::new(
        ActiveWorkerAuthority {
            lease: lease(),
            worker_session_id: WorkerSessionId(id("wsn", 'A')),
            session_identity: session(),
        },
        ExecutionEnvelope {
            token: ExecutionEnvelopeToken {
                version: 1,
                digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            },
            policy: Policy,
        },
        Gate,
        Journal,
        Executor(Rc::clone(executions)),
    )
}

fn signing_key() -> ActionEnforcementSigningKey {
    ActionEnforcementSigningKey::from_bytes([17_u8; 32]).expect("signing key")
}

fn receipt(action: &WorkerActionRequest) -> ActionEnforcementReceiptMessage {
    let facts = action_enforcement_facts(
        &normalize_action(&action.intent, &action.request).expect("normalization"),
    )
    .expect("action facts");
    let issuer = ActionEnforcementIssuer::new(signing_key());
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
    issuer.sign(&mut receipt).expect("signed receipt");
    receipt
}

#[test]
fn worker_executes_once_and_restart_replay_never_repeats_the_side_effect() {
    let root = std::env::temp_dir().join(format!(
        "winwincode-worker-action-enforcement-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let executions = Rc::new(Cell::new(0));
    let action = action();
    let receipt = receipt(&action);
    let mut runtime =
        DurableWorkerActionEnforcement::open(&root, signing_key(), gateway(&executions))
            .expect("Worker action runtime");
    let request = runtime
        .prepare_request(
            ExecutionMessageId(id("xmsg", 'B')),
            Instant(NOW.to_owned()),
            &action,
        )
        .expect("action enforcement request");
    assert_eq!(request.request_id, action.invocation_request_id);
    assert_eq!(request.subject_sha256, receipt.subject_sha256);
    runtime
        .execute(&Instant(NOW.to_owned()), &action, &receipt)
        .expect("first authorized execution");
    assert_eq!(executions.get(), 1);
    drop(runtime);

    let mut restarted =
        DurableWorkerActionEnforcement::open(&root, signing_key(), gateway(&executions))
            .expect("Worker action runtime restart");
    let replay = restarted
        .execute(&Instant(NOW.to_owned()), &action, &receipt)
        .expect_err("receipt replay must not execute");
    assert_eq!(
        replay.rejection_code(),
        Some(ActionGatewayRejectionCode::ActionReceiptConsumed)
    );
    assert_eq!(executions.get(), 1);

    let mut cross_tenant = receipt;
    cross_tenant.scope.repository_id = RepositoryId(id("rep", 'B'));
    let foreign = restarted
        .execute(&Instant(NOW.to_owned()), &action, &cross_tenant)
        .expect_err("cross-tenant receipt must not execute");
    assert_eq!(
        foreign.rejection_code(),
        Some(ActionGatewayRejectionCode::InvalidActionReceipt)
    );
    assert_eq!(executions.get(), 1);
    fs::remove_dir_all(root).expect("Worker action directory release");
}
