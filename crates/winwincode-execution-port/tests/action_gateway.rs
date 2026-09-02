// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::{
    CodexThreadId, ExecutionJobId, FencingToken, Instant, LeaseId, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RepositoryScope, RepositoryScopeKind, RequestId,
    SchemaVersion, SessionIdentity, Sha256Digest, StageRunId, UserActor, UserActorKind, UserId,
    WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::action_enforcement::{
    ActionEnforcementIssuer, ActionEnforcementSigningKey, FileActionReceiptUseStore,
    action_enforcement_facts,
};
use winwincode_execution_port::action_gateway::{
    ActionGatewayError, ActionGatewayRejectionCode, ActiveWorkerAuthority, CodexToolExecutor,
    DeterministicActionGate, ExecutionEnvelope, ExecutionEnvelopeToken, GateDecision, GateInput,
    PreActionDecisionRecorder, WorkerActionAuthority, WorkerActionGateway, WorkerActionRequest,
};
use winwincode_execution_port::action_normalizer::{
    ActionIntent, ActionObject, ActionOperation, ActionPurpose, ActionRisk, ActionScope,
    FileAnalysis, FileOperation, FileRequest, GitOperation, GitRequest, McpRequest, NetworkRequest,
    ShellRequest, ToolRequest, normalize_action,
};
use winwincode_execution_port::generated::{
    ActionEnforcementDecision, ActionEnforcementReceiptMessage,
    ActionEnforcementReceiptMessageKind, ExecutionLeaseStamp,
};

const NOW: &str = "2027-01-15T08:00:02.000Z";
static STORE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Policy {
    name: &'static str,
}

struct RecordingGate {
    events: Rc<RefCell<Vec<String>>>,
    decision: GateDecision,
}

impl DeterministicActionGate<Policy> for RecordingGate {
    fn decide(&mut self, input: GateInput<'_, Policy>) -> GateDecision {
        self.events.borrow_mut().push(format!(
            "gate:{}:{:?}",
            input.envelope.policy.name, input.observed.source
        ));
        assert_eq!(input.intent.object, input.observed.objects[0]);
        self.decision.clone()
    }
}

struct RecordingExecutor {
    events: Rc<RefCell<Vec<String>>>,
}

struct RecordingDecisionRecorder {
    events: Rc<RefCell<Vec<String>>>,
    fail: bool,
}

impl PreActionDecisionRecorder<Policy> for RecordingDecisionRecorder {
    type Error = &'static str;

    fn record(
        &mut self,
        input: GateInput<'_, Policy>,
        decision: &GateDecision,
    ) -> Result<(), Self::Error> {
        self.events
            .borrow_mut()
            .push(format!("record:{:?}:{decision:?}", input.observed.source));
        if self.fail {
            Err("outbox unavailable")
        } else {
            Ok(())
        }
    }
}

impl CodexToolExecutor for RecordingExecutor {
    type Output = String;
    type Error = &'static str;

    fn execute(&mut self, request: &ToolRequest) -> Result<Self::Output, Self::Error> {
        self.events
            .borrow_mut()
            .push(format!("execute:{:?}", source_name(request)));
        Ok("existing-codex-result".to_owned())
    }
}

fn source_name(request: &ToolRequest) -> &'static str {
    match request {
        ToolRequest::File(_) => "file",
        ToolRequest::Git(_) => "git",
        ToolRequest::Shell(_) => "shell",
        ToolRequest::Network(_) => "network",
        ToolRequest::Mcp(_) => "mcp",
    }
}

fn id(prefix: &str, suffix: char) -> String {
    format!("{prefix}_{}", suffix.to_string().repeat(26))
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

fn session_identity() -> SessionIdentity {
    SessionIdentity {
        codex_thread_id: CodexThreadId(id("cdx", 'A')),
        product_session_id: ProductSessionId(id("psn", 'A')),
        stage_run_id: Some(StageRunId(id("run", 'A'))),
        worker_session_id: WorkerSessionId(id("wsn", 'A')),
    }
}

fn authority() -> ActiveWorkerAuthority {
    ActiveWorkerAuthority {
        lease: lease(),
        worker_session_id: WorkerSessionId(id("wsn", 'A')),
        session_identity: session_identity(),
    }
}

fn envelope(version: u64, digit: char) -> ExecutionEnvelope<Policy> {
    ExecutionEnvelope {
        token: ExecutionEnvelopeToken {
            version,
            digest: Sha256Digest(format!("sha256:{}", digit.to_string().repeat(64))),
        },
        policy: Policy { name: "fixture" },
    }
}

fn intent_for(request: &ToolRequest) -> ActionIntent {
    let (object, operation, scope, targets, risk) = match request {
        ToolRequest::File(_) => (
            ActionObject::ProductionCode,
            ActionOperation::Modify,
            ActionScope::Local,
            vec!["crates/kernel/src/lib.rs".to_owned()],
            ActionRisk::Medium,
        ),
        ToolRequest::Git(_) => (
            ActionObject::ProductionCode,
            ActionOperation::Execute,
            ActionScope::Repository,
            vec![".".to_owned()],
            ActionRisk::Low,
        ),
        ToolRequest::Shell(_) => (
            ActionObject::Test,
            ActionOperation::Execute,
            ActionScope::Repository,
            vec!["cwd:.".to_owned(), "argv:[\"cargo\",\"test\"]".to_owned()],
            ActionRisk::Low,
        ),
        ToolRequest::Network(_) => (
            ActionObject::ExternalResource,
            ActionOperation::Execute,
            ActionScope::External,
            vec!["GET https://api.example.test/v1/items".to_owned()],
            ActionRisk::Low,
        ),
        ToolRequest::Mcp(_) => (
            ActionObject::ExternalResource,
            ActionOperation::Execute,
            ActionScope::External,
            vec!["mcp://fixture.server/read_record".to_owned()],
            ActionRisk::Medium,
        ),
    };
    ActionIntent {
        object,
        operation,
        intent: ActionPurpose::Implement,
        scope,
        targets,
        requirement_refs: vec!["REQ-1".to_owned()],
        plan_refs: vec!["PLAN-1".to_owned()],
        expected_effect: "perform the approved action".to_owned(),
        scope_delta: None,
        rollback: Some("restore prior state".to_owned()),
        executor_risk: risk,
    }
}

fn action(request: ToolRequest) -> WorkerActionRequest {
    WorkerActionRequest {
        invocation_request_id: RequestId(id("req", 'R')),
        authority: WorkerActionAuthority {
            lease: lease(),
            worker_session_id: WorkerSessionId(id("wsn", 'A')),
            session_identity: session_identity(),
            envelope: envelope(1, 'a').token,
        },
        intent: intent_for(&request),
        request,
    }
}

fn issuer() -> ActionEnforcementIssuer {
    ActionEnforcementIssuer::new(
        ActionEnforcementSigningKey::from_bytes([7_u8; 32]).expect("signing key"),
    )
}

fn enforcement_receipt(action: &WorkerActionRequest) -> ActionEnforcementReceiptMessage {
    let normalization = normalize_action(&action.intent, &action.request).expect("normalization");
    let facts = action_enforcement_facts(&normalization).expect("action facts");
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
        message_id: winwincode_domain::ExecutionMessageId(id("xmsg", 'E')),
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
    issuer().sign(&mut receipt).expect("sign receipt");
    receipt
}

fn execute_action(
    gateway: &mut WorkerActionGateway<
        Policy,
        RecordingGate,
        RecordingDecisionRecorder,
        RecordingExecutor,
    >,
    now: &Instant,
    action: &WorkerActionRequest,
) -> winwincode_execution_port::action_gateway::ActionGatewayResult<
    String,
    &'static str,
    &'static str,
> {
    let receipt = enforcement_receipt(action);
    let verifier = issuer().verifier();
    let sequence = STORE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "winwincode-action-gateway-{}-{sequence}",
        std::process::id()
    ));
    let mut store = FileActionReceiptUseStore::open(&root).expect("receipt store");
    let result = gateway.execute(now, action, &receipt, &verifier, &mut store);
    let _ = std::fs::remove_dir_all(root);
    result
}

fn gateway(
    events: &Rc<RefCell<Vec<String>>>,
    decision: GateDecision,
) -> WorkerActionGateway<Policy, RecordingGate, RecordingDecisionRecorder, RecordingExecutor> {
    WorkerActionGateway::new(
        authority(),
        envelope(1, 'a'),
        RecordingGate {
            events: Rc::clone(events),
            decision,
        },
        RecordingDecisionRecorder {
            events: Rc::clone(events),
            fail: false,
        },
        RecordingExecutor {
            events: Rc::clone(events),
        },
    )
}

fn requests() -> Vec<ToolRequest> {
    vec![
        ToolRequest::File(FileRequest {
            operation: FileOperation::Write,
            paths: vec!["crates/kernel/src/lib.rs".to_owned()],
            analysis: FileAnalysis::default(),
        }),
        ToolRequest::Git(GitRequest {
            operation: GitOperation::Status,
            repository_path: ".".to_owned(),
            refs: Vec::new(),
        }),
        ToolRequest::Shell(ShellRequest {
            program: "cargo".to_owned(),
            args: vec!["test".to_owned()],
            working_directory: ".".to_owned(),
        }),
        ToolRequest::Network(NetworkRequest {
            method: "GET".to_owned(),
            url: "https://api.example.test/v1/items".to_owned(),
        }),
        ToolRequest::Mcp(McpRequest {
            server: "fixture.server".to_owned(),
            tool: "read_record".to_owned(),
            arguments: serde_json::json!({"recordId": "rec-1"}),
        }),
    ]
}

#[test]
fn every_tool_family_runs_the_gate_before_the_existing_codex_executor() {
    for request in requests() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut gateway = gateway(&events, GateDecision::Allow);
        let result = execute_action(&mut gateway, &Instant(NOW.to_owned()), &action(request))
            .expect("approved action");

        assert_eq!(result.output, "existing-codex-result");
        let events = events.borrow();
        assert_eq!(events.len(), 3);
        assert!(events[0].starts_with("gate:fixture:"));
        assert!(events[1].starts_with("record:"));
        assert!(events[2].starts_with("execute:"));
    }
}

#[test]
fn an_intent_mismatch_never_reaches_the_gate_or_executor() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut gateway = gateway(&events, GateDecision::Allow);
    let mut request = action(requests().remove(0));
    request.intent.operation = ActionOperation::Delete;

    let error = execute_action(&mut gateway, &Instant(NOW.to_owned()), &request)
        .expect_err("mismatch must fail closed");
    assert_eq!(
        error.rejection_code(),
        Some(ActionGatewayRejectionCode::IntentMismatch)
    );
    assert!(matches!(error, ActionGatewayError::IntentMismatch(_)));
    assert!(events.borrow().is_empty());
}

#[test]
fn stale_lease_worker_session_binding_and_envelope_fail_before_the_gate() {
    let cases = [
        (
            ActionGatewayRejectionCode::StaleLease,
            Box::new(|action: &mut WorkerActionRequest| {
                action.authority.lease.fencing_token = FencingToken("6".to_owned());
            }) as Box<dyn Fn(&mut WorkerActionRequest)>,
        ),
        (
            ActionGatewayRejectionCode::StaleWorkerSession,
            Box::new(|action: &mut WorkerActionRequest| {
                action.authority.worker_session_id = WorkerSessionId(id("wsn", 'B'));
            }),
        ),
        (
            ActionGatewayRejectionCode::StaleSessionIdentity,
            Box::new(|action: &mut WorkerActionRequest| {
                action.authority.session_identity.codex_thread_id = CodexThreadId(id("cdx", 'B'));
            }),
        ),
        (
            ActionGatewayRejectionCode::StaleExecutionEnvelope,
            Box::new(|action: &mut WorkerActionRequest| {
                action.authority.envelope = envelope(2, 'b').token;
            }),
        ),
    ];

    for (expected, mutate) in cases {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut gateway = gateway(&events, GateDecision::Allow);
        let mut action = action(requests().remove(0));
        mutate(&mut action);
        let error = execute_action(&mut gateway, &Instant(NOW.to_owned()), &action)
            .expect_err("stale authority must fail closed");
        assert_eq!(error.rejection_code(), Some(expected));
        assert!(events.borrow().is_empty());
    }
}

#[test]
fn expired_lease_and_replaced_envelope_cannot_execute() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut gateway = gateway(&events, GateDecision::Allow);
    let current_action = action(requests().remove(0));
    let expired = execute_action(
        &mut gateway,
        &Instant("2027-01-15T08:05:00.000Z".to_owned()),
        &current_action,
    )
    .expect_err("expiry boundary must reject");
    assert_eq!(
        expired.rejection_code(),
        Some(ActionGatewayRejectionCode::ExpiredLease)
    );

    gateway.replace_authority(authority(), envelope(2, 'b'));
    let stale = execute_action(&mut gateway, &Instant(NOW.to_owned()), &current_action)
        .expect_err("old envelope token must stop immediately");
    assert_eq!(
        stale.rejection_code(),
        Some(ActionGatewayRejectionCode::StaleExecutionEnvelope)
    );
    assert!(events.borrow().is_empty());
}

#[test]
fn every_non_allow_gate_decision_stops_before_the_executor() {
    let decisions = [
        GateDecision::RequestPlanDelta {
            reason: "scope changed".to_owned(),
        },
        GateDecision::PauseForHuman {
            reason: "approval required".to_owned(),
        },
        GateDecision::DenyAction {
            reason: "protected target".to_owned(),
        },
        GateDecision::ReplanRequired {
            reason: "plan invalidated".to_owned(),
        },
    ];

    for decision in decisions {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut gateway = gateway(&events, decision.clone());
        let error = execute_action(
            &mut gateway,
            &Instant(NOW.to_owned()),
            &action(requests().remove(0)),
        )
        .expect_err("unapproved action must stop");
        assert_eq!(error, ActionGatewayError::NotApproved(decision));
        let events = events.borrow();
        assert_eq!(events.len(), 2);
        assert!(events[0].starts_with("gate:"));
        assert!(events[1].starts_with("record:"));
    }
}

#[test]
fn allow_with_watch_is_an_explicit_approval() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let decision = GateDecision::AllowWithWatch {
        reason: "observe cumulative scope".to_owned(),
    };
    let mut gateway = gateway(&events, decision.clone());
    let executed = execute_action(
        &mut gateway,
        &Instant(NOW.to_owned()),
        &action(requests().remove(0)),
    )
    .expect("allow-with-watch authorizes execution");
    assert_eq!(executed.decision, decision);
    assert_eq!(events.borrow().len(), 3);
}

#[test]
fn an_outbox_failure_stops_an_allowed_action_before_the_executor() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut gateway = WorkerActionGateway::new(
        authority(),
        envelope(1, 'a'),
        RecordingGate {
            events: Rc::clone(&events),
            decision: GateDecision::Allow,
        },
        RecordingDecisionRecorder {
            events: Rc::clone(&events),
            fail: true,
        },
        RecordingExecutor {
            events: Rc::clone(&events),
        },
    );

    let error = execute_action(
        &mut gateway,
        &Instant(NOW.to_owned()),
        &action(requests().remove(0)),
    )
    .expect_err("unrecorded decision must stop execution");
    assert_eq!(
        error,
        ActionGatewayError::DecisionRecord("outbox unavailable")
    );
    assert_eq!(events.borrow().len(), 2);
    assert!(
        events
            .borrow()
            .iter()
            .all(|event| !event.starts_with("execute:"))
    );
}
