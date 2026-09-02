// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::json;
use winwincode_domain::{
    CodexThreadId, ExecutionJobId, FencingToken, Instant, LeaseId, OrganizationId,
    ProductSessionId, ProjectId, RepositoryId, RepositoryScope, RepositoryScopeKind, RequestId,
    SchemaVersion, SessionIdentity, Sha256Digest, StageRunId, UserActor, UserActorKind, UserId,
    WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::action_enforcement::{
    ActionEnforcementIssuer, ActionEnforcementSigningKey, ActionReceiptClaim,
    ActionReceiptUseError, ActionReceiptUseStore, action_enforcement_facts,
};
use winwincode_execution_port::action_gateway::{
    ActionGatewayError, ActiveWorkerAuthority, CodexToolExecutor, DeterministicActionGate,
    ExecutionEnvelope, ExecutionEnvelopeToken, GateDecision, GateInput, PreActionDecisionRecorder,
    WorkerActionAuthority, WorkerActionGateway, WorkerActionRequest,
};
use winwincode_execution_port::action_normalizer::{
    ActionIntent, ActionObject, ActionOperation, ActionPurpose, ActionRisk, ActionScope,
    McpRequest, NetworkRequest, ToolRequest, normalize_action,
};
use winwincode_execution_port::capability_adapter::{
    CAPABILITY_ADAPTER_VERSION, CapabilityAdapterError, CapabilityCatalogErrorCode,
    CapabilityDescriptor, CapabilityHealth, CapabilityInvocationRequest, CapabilityOrigin,
    CapabilityRejectionCode, CapabilityWarning, UnmanagedCapabilityPolicy, WorkerCapabilityAdapter,
    WorkerCapabilityCatalog,
};
use winwincode_execution_port::generated::{
    ActionEnforcementDecision, ActionEnforcementReceiptMessage,
    ActionEnforcementReceiptMessageKind, ExecutionLeaseStamp, WorkerCapabilityFeature,
    WorkerCapabilitySet, WorkerCapabilitySetPlatform,
};

const NOW: &str = "2027-01-15T08:00:02.000Z";
const CAPABILITY_ID: &str = "mcp://fixture.server/read_record";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Policy;

struct RecordingGate {
    events: Rc<RefCell<Vec<&'static str>>>,
    decision: GateDecision,
}

impl DeterministicActionGate<Policy> for RecordingGate {
    fn decide(&mut self, _input: GateInput<'_, Policy>) -> GateDecision {
        self.events.borrow_mut().push("gate");
        self.decision.clone()
    }
}

struct RecordingJournal {
    events: Rc<RefCell<Vec<&'static str>>>,
}

impl PreActionDecisionRecorder<Policy> for RecordingJournal {
    type Error = &'static str;

    fn record(
        &mut self,
        _input: GateInput<'_, Policy>,
        _decision: &GateDecision,
    ) -> Result<(), Self::Error> {
        self.events.borrow_mut().push("trace");
        Ok(())
    }
}

struct RecordingExecutor {
    events: Rc<RefCell<Vec<&'static str>>>,
}

impl CodexToolExecutor for RecordingExecutor {
    type Output = &'static str;
    type Error = &'static str;

    fn execute(&mut self, _request: &ToolRequest) -> Result<Self::Output, Self::Error> {
        self.events.borrow_mut().push("codex");
        Ok("codex-result")
    }
}

type TestAdapter =
    WorkerCapabilityAdapter<Policy, RecordingGate, RecordingJournal, RecordingExecutor>;

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

fn session_identity(suffix: char) -> SessionIdentity {
    SessionIdentity {
        codex_thread_id: CodexThreadId(id("cdx", suffix)),
        product_session_id: ProductSessionId(id("psn", suffix)),
        stage_run_id: Some(StageRunId(id("run", suffix))),
        worker_session_id: WorkerSessionId(id("wsn", suffix)),
    }
}

fn envelope(version: u64, digit: char) -> ExecutionEnvelopeToken {
    ExecutionEnvelopeToken {
        version,
        digest: Sha256Digest(format!("sha256:{}", digit.to_string().repeat(64))),
    }
}

fn registered(features: Vec<WorkerCapabilityFeature>) -> WorkerCapabilitySet {
    WorkerCapabilitySet {
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        features,
        max_concurrent_jobs: 4,
        platform: WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
    }
}

fn descriptor(
    version: &str,
    health: CapabilityHealth,
    origin: CapabilityOrigin,
) -> CapabilityDescriptor {
    CapabilityDescriptor::mcp("Fixture.Server", "Read_Record", version, health, origin).unwrap()
}

fn catalog_with(descriptor: CapabilityDescriptor) -> WorkerCapabilityCatalog {
    WorkerCapabilityCatalog::discover(
        &registered(vec![
            WorkerCapabilityFeature::Mcp,
            WorkerCapabilityFeature::Shell,
        ]),
        vec![descriptor],
    )
    .unwrap()
}

fn mcp_action(envelope_token: ExecutionEnvelopeToken) -> WorkerActionRequest {
    let request = ToolRequest::Mcp(McpRequest {
        server: "Fixture.Server".to_owned(),
        tool: "Read_Record".to_owned(),
        arguments: json!({"accessToken": "MCP_SECRET_MUST_NOT_BE_OBSERVABLE"}),
    });
    WorkerActionRequest {
        invocation_request_id: RequestId(id("req", 'R')),
        authority: WorkerActionAuthority {
            lease: lease(),
            worker_session_id: WorkerSessionId(id("wsn", 'A')),
            session_identity: session_identity('A'),
            envelope: envelope_token,
        },
        intent: ActionIntent {
            object: ActionObject::ExternalResource,
            operation: ActionOperation::Execute,
            intent: ActionPurpose::Implement,
            scope: ActionScope::External,
            targets: vec![CAPABILITY_ID.to_owned()],
            requirement_refs: vec!["REQ-CAPABILITY".to_owned()],
            plan_refs: vec!["PLAN-CAPABILITY".to_owned()],
            expected_effect: "read the approved MCP record".to_owned(),
            scope_delta: None,
            rollback: Some("discard the external read result".to_owned()),
            executor_risk: ActionRisk::Medium,
        },
        request,
    }
}

fn network_action(envelope_token: ExecutionEnvelopeToken) -> WorkerActionRequest {
    WorkerActionRequest {
        invocation_request_id: RequestId(id("req", 'R')),
        authority: WorkerActionAuthority {
            lease: lease(),
            worker_session_id: WorkerSessionId(id("wsn", 'A')),
            session_identity: session_identity('A'),
            envelope: envelope_token,
        },
        intent: ActionIntent {
            object: ActionObject::ExternalResource,
            operation: ActionOperation::Execute,
            intent: ActionPurpose::Implement,
            scope: ActionScope::External,
            targets: vec!["GET https://example.test/".to_owned()],
            requirement_refs: vec!["REQ-CAPABILITY".to_owned()],
            plan_refs: vec!["PLAN-CAPABILITY".to_owned()],
            expected_effect: "read the approved network resource".to_owned(),
            scope_delta: None,
            rollback: Some("discard the network response".to_owned()),
            executor_risk: ActionRisk::Low,
        },
        request: ToolRequest::Network(NetworkRequest {
            method: "GET".to_owned(),
            url: "https://example.test/".to_owned(),
        }),
    }
}

fn adapter(
    catalog: WorkerCapabilityCatalog,
    grants: Vec<winwincode_execution_port::capability_adapter::CapabilityGrant>,
    unmanaged_policy: UnmanagedCapabilityPolicy,
    decision: GateDecision,
    events: &Rc<RefCell<Vec<&'static str>>>,
) -> TestAdapter {
    let session = session_identity('A');
    let active = ActiveWorkerAuthority {
        lease: lease(),
        worker_session_id: session.worker_session_id.clone(),
        session_identity: session,
    };
    let gateway = WorkerActionGateway::new(
        active,
        ExecutionEnvelope {
            token: envelope(1, 'b'),
            policy: Policy,
        },
        RecordingGate {
            events: Rc::clone(events),
            decision,
        },
        RecordingJournal {
            events: Rc::clone(events),
        },
        RecordingExecutor {
            events: Rc::clone(events),
        },
    );
    WorkerCapabilityAdapter::new(catalog, grants, unmanaged_policy, gateway)
}

fn issuer() -> ActionEnforcementIssuer {
    ActionEnforcementIssuer::new(
        ActionEnforcementSigningKey::from_bytes([9_u8; 32]).expect("signing key"),
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

struct FreshReceiptStore;

impl ActionReceiptUseStore for FreshReceiptStore {
    fn claim(
        &mut self,
        _receipt: &ActionEnforcementReceiptMessage,
    ) -> Result<ActionReceiptClaim, ActionReceiptUseError> {
        Ok(ActionReceiptClaim::Fresh)
    }
}

fn invoke_action(
    adapter: &mut TestAdapter,
    now: &Instant,
    action: &WorkerActionRequest,
    capability_id: &str,
    version: &str,
) -> Result<
    winwincode_execution_port::capability_adapter::CapabilityInvocation<&'static str>,
    CapabilityAdapterError<&'static str, &'static str>,
> {
    let receipt = enforcement_receipt(action);
    let invocation = CapabilityInvocationRequest {
        capability_id,
        capability_version: version,
        action,
        enforcement_receipt: &receipt,
    };
    adapter.invoke(
        now,
        invocation,
        &issuer().verifier(),
        &mut FreshReceiptStore,
    )
}

fn rejection_code(
    error: &CapabilityAdapterError<&'static str, &'static str>,
) -> CapabilityRejectionCode {
    match error {
        CapabilityAdapterError::Rejected { code, .. } => *code,
        CapabilityAdapterError::InvalidMcpTarget | CapabilityAdapterError::Gateway(_) => {
            panic!("expected capability rejection")
        }
    }
}

#[test]
fn discovery_is_versioned_sorted_deterministic_and_bound_to_registry() {
    let a = CapabilityDescriptor::mcp(
        "alpha.server",
        "read",
        "1.2.3",
        CapabilityHealth::Healthy,
        CapabilityOrigin::CodexCoreMcp,
    )
    .unwrap();
    let b = CapabilityDescriptor::mcp(
        "beta.server",
        "write",
        "2027.01",
        CapabilityHealth::Degraded,
        CapabilityOrigin::MappedPluginManifest,
    )
    .unwrap();
    let registry = registered(vec![WorkerCapabilityFeature::Mcp]);
    let forward = WorkerCapabilityCatalog::discover(&registry, vec![a.clone(), b.clone()]).unwrap();
    let reverse = WorkerCapabilityCatalog::discover(&registry, vec![b, a]).unwrap();

    assert_eq!(forward.adapter_version(), CAPABILITY_ADAPTER_VERSION);
    assert_eq!(forward.catalog_digest(), reverse.catalog_digest());
    assert_eq!(
        forward.registry_capability_digest(),
        &registry.capability_digest
    );
    assert_eq!(forward.capabilities()[0].id(), "mcp://alpha.server/read");
    assert_eq!(forward.capabilities()[0].version(), "1.2.3");
    assert_eq!(
        forward.capabilities()[1].health(),
        CapabilityHealth::Degraded
    );
    assert_eq!(
        forward.capabilities()[1].origin(),
        CapabilityOrigin::MappedPluginManifest
    );
}

#[test]
fn discovery_rejects_registry_mismatch_duplicate_target_and_invalid_version() {
    let item = descriptor(
        "1.0.0",
        CapabilityHealth::Healthy,
        CapabilityOrigin::CodexCoreMcp,
    );
    let missing = WorkerCapabilityCatalog::discover(
        &registered(vec![WorkerCapabilityFeature::Shell]),
        vec![item.clone()],
    )
    .unwrap_err();
    assert_eq!(
        missing.code,
        CapabilityCatalogErrorCode::McpFeatureNotRegistered
    );

    let duplicate = WorkerCapabilityCatalog::discover(
        &registered(vec![WorkerCapabilityFeature::Mcp]),
        vec![item.clone(), item],
    )
    .unwrap_err();
    assert_eq!(
        duplicate.code,
        CapabilityCatalogErrorCode::DuplicateCapability
    );

    let invalid = CapabilityDescriptor::mcp(
        "fixture.server",
        "read_record",
        "version includes secret=value",
        CapabilityHealth::Healthy,
        CapabilityOrigin::CodexCoreMcp,
    )
    .unwrap_err();
    assert!(!invalid.to_string().contains("secret=value"));
}

#[test]
fn authorized_mcp_invocation_uses_gate_trace_and_existing_codex_executor_in_order() {
    let catalog = catalog_with(descriptor(
        "1.0.0",
        CapabilityHealth::Healthy,
        CapabilityOrigin::MappedPluginManifest,
    ));
    let grant = catalog
        .authorize(
            CAPABILITY_ID,
            WorkerSessionId(id("wsn", 'A')),
            envelope(1, 'b'),
        )
        .unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut adapter = adapter(
        catalog,
        vec![grant],
        UnmanagedCapabilityPolicy::Deny,
        GateDecision::Allow,
        &events,
    );
    let action = mcp_action(envelope(1, 'b'));

    let result = invoke_action(
        &mut adapter,
        &Instant(NOW.to_owned()),
        &action,
        CAPABILITY_ID,
        "1.0.0",
    )
    .unwrap();

    assert!(result.warnings.is_empty());
    assert_eq!(result.executed.output, "codex-result");
    assert_eq!(&*events.borrow(), &["gate", "trace", "codex"]);
    assert_eq!(
        result.executed.normalization.observed.targets,
        [CAPABILITY_ID]
    );
    assert!(!format!("{:?}", adapter.catalog()).contains("MCP_SECRET"));
}

#[test]
fn cross_session_and_stale_envelope_grants_fail_before_gate_or_executor() {
    let catalog = catalog_with(descriptor(
        "1.0.0",
        CapabilityHealth::Healthy,
        CapabilityOrigin::CodexCoreMcp,
    ));
    let other_session_grant = catalog
        .authorize(
            CAPABILITY_ID,
            WorkerSessionId(id("wsn", 'B')),
            envelope(1, 'b'),
        )
        .unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut adapter = adapter(
        catalog.clone(),
        vec![other_session_grant],
        UnmanagedCapabilityPolicy::Deny,
        GateDecision::Allow,
        &events,
    );
    let action = mcp_action(envelope(1, 'b'));
    let error = invoke_action(
        &mut adapter,
        &Instant(NOW.to_owned()),
        &action,
        CAPABILITY_ID,
        "1.0.0",
    )
    .unwrap_err();
    assert_eq!(
        rejection_code(&error),
        CapabilityRejectionCode::StaleWorkerSession
    );
    assert!(events.borrow().is_empty());

    let stale_envelope_grant = catalog
        .authorize(
            CAPABILITY_ID,
            WorkerSessionId(id("wsn", 'A')),
            envelope(2, 'c'),
        )
        .unwrap();
    adapter.replace_grants(vec![stale_envelope_grant]);
    let error = invoke_action(
        &mut adapter,
        &Instant(NOW.to_owned()),
        &action,
        CAPABILITY_ID,
        "1.0.0",
    )
    .unwrap_err();
    assert_eq!(
        rejection_code(&error),
        CapabilityRejectionCode::StaleExecutionEnvelope
    );
    assert!(events.borrow().is_empty());
}

#[test]
fn catalog_replacement_revokes_old_grants_until_explicit_reauthorization() {
    let original = catalog_with(descriptor(
        "1.0.0",
        CapabilityHealth::Healthy,
        CapabilityOrigin::CodexCoreMcp,
    ));
    let old_grant = original
        .authorize(
            CAPABILITY_ID,
            WorkerSessionId(id("wsn", 'A')),
            envelope(1, 'b'),
        )
        .unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut adapter = adapter(
        original,
        vec![old_grant],
        UnmanagedCapabilityPolicy::Deny,
        GateDecision::Allow,
        &events,
    );
    let replacement = catalog_with(descriptor(
        "2.0.0",
        CapabilityHealth::Healthy,
        CapabilityOrigin::CodexCoreMcp,
    ));
    adapter.replace_catalog(replacement);
    let action = mcp_action(envelope(1, 'b'));

    let error = invoke_action(
        &mut adapter,
        &Instant(NOW.to_owned()),
        &action,
        CAPABILITY_ID,
        "2.0.0",
    )
    .unwrap_err();

    assert_eq!(
        rejection_code(&error),
        CapabilityRejectionCode::StaleCapabilityCatalog
    );
    assert!(events.borrow().is_empty());
}

#[test]
fn unmanaged_and_degraded_capability_warns_or_is_rejected_by_policy() {
    let catalog = catalog_with(descriptor(
        "1.0.0",
        CapabilityHealth::Degraded,
        CapabilityOrigin::Unmanaged,
    ));
    let grant = catalog
        .authorize(
            CAPABILITY_ID,
            WorkerSessionId(id("wsn", 'A')),
            envelope(1, 'b'),
        )
        .unwrap();
    let action = mcp_action(envelope(1, 'b'));

    let warn_events = Rc::new(RefCell::new(Vec::new()));
    let mut warning_adapter = adapter(
        catalog.clone(),
        vec![grant.clone()],
        UnmanagedCapabilityPolicy::Warn,
        GateDecision::Allow,
        &warn_events,
    );
    let result = invoke_action(
        &mut warning_adapter,
        &Instant(NOW.to_owned()),
        &action,
        CAPABILITY_ID,
        "1.0.0",
    )
    .unwrap();
    assert_eq!(
        result.warnings,
        [
            CapabilityWarning::UnmanagedCapability,
            CapabilityWarning::DegradedCapability,
        ]
    );
    assert_eq!(&*warn_events.borrow(), &["gate", "trace", "codex"]);

    let deny_events = Rc::new(RefCell::new(Vec::new()));
    let mut denying_adapter = adapter(
        catalog,
        vec![grant],
        UnmanagedCapabilityPolicy::Deny,
        GateDecision::Allow,
        &deny_events,
    );
    let error = invoke_action(
        &mut denying_adapter,
        &Instant(NOW.to_owned()),
        &action,
        CAPABILITY_ID,
        "1.0.0",
    )
    .unwrap_err();
    assert_eq!(
        rejection_code(&error),
        CapabilityRejectionCode::UnmanagedCapabilityDenied
    );
    assert!(deny_events.borrow().is_empty());
}

#[test]
fn unavailable_wrong_family_target_or_version_never_reaches_gateway() {
    let catalog = catalog_with(descriptor(
        "1.0.0",
        CapabilityHealth::Unavailable,
        CapabilityOrigin::CodexCoreMcp,
    ));
    let grant = catalog
        .authorize(
            CAPABILITY_ID,
            WorkerSessionId(id("wsn", 'A')),
            envelope(1, 'b'),
        )
        .unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut adapter = adapter(
        catalog,
        vec![grant],
        UnmanagedCapabilityPolicy::Warn,
        GateDecision::Allow,
        &events,
    );
    let action = mcp_action(envelope(1, 'b'));

    let unavailable = invoke_action(
        &mut adapter,
        &Instant(NOW.to_owned()),
        &action,
        CAPABILITY_ID,
        "1.0.0",
    )
    .unwrap_err();
    assert_eq!(
        rejection_code(&unavailable),
        CapabilityRejectionCode::CapabilityUnavailable
    );

    let wrong_version = invoke_action(
        &mut adapter,
        &Instant(NOW.to_owned()),
        &action,
        CAPABILITY_ID,
        "9.9.9",
    )
    .unwrap_err();
    assert_eq!(
        rejection_code(&wrong_version),
        CapabilityRejectionCode::CapabilityVersionMismatch
    );

    let error = invoke_action(
        &mut adapter,
        &Instant(NOW.to_owned()),
        &action,
        "mcp://another.server/read_record",
        "1.0.0",
    )
    .unwrap_err();
    assert_eq!(
        rejection_code(&error),
        CapabilityRejectionCode::CapabilityTargetMismatch
    );

    let network = network_action(envelope(1, 'b'));
    let error = invoke_action(
        &mut adapter,
        &Instant(NOW.to_owned()),
        &network,
        CAPABILITY_ID,
        "1.0.0",
    )
    .unwrap_err();
    assert_eq!(
        rejection_code(&error),
        CapabilityRejectionCode::RequestFamilyMismatch
    );
    assert!(events.borrow().is_empty());
}

#[test]
fn gate_denial_is_traced_and_never_reaches_codex_executor() {
    let catalog = catalog_with(descriptor(
        "1.0.0",
        CapabilityHealth::Healthy,
        CapabilityOrigin::CodexCoreMcp,
    ));
    let grant = catalog
        .authorize(
            CAPABILITY_ID,
            WorkerSessionId(id("wsn", 'A')),
            envelope(1, 'b'),
        )
        .unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut adapter = adapter(
        catalog,
        vec![grant],
        UnmanagedCapabilityPolicy::Deny,
        GateDecision::DenyAction {
            reason: "policy denied capability".to_owned(),
        },
        &events,
    );
    let action = mcp_action(envelope(1, 'b'));

    let error = invoke_action(
        &mut adapter,
        &Instant(NOW.to_owned()),
        &action,
        CAPABILITY_ID,
        "1.0.0",
    )
    .unwrap_err();

    assert!(matches!(
        error,
        CapabilityAdapterError::Gateway(boxed)
            if matches!(*boxed, ActionGatewayError::NotApproved(GateDecision::DenyAction { .. }))
    ));
    assert_eq!(&*events.borrow(), &["gate", "trace"]);
}
