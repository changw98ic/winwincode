// SPDX-License-Identifier: Apache-2.0

//! Exact, replay-safe routing for human decisions and session cancellation.
//!
//! The router owns no HTTP DTOs, scheduler store, Worker transport, or model
//! runtime. It accepts already-authenticated actors and sealed execution facts,
//! then returns explicit commands for those injected boundaries.

use std::collections::HashMap;
use std::fmt;

use winwincode_domain::{
    ApprovalId, AttentionItemId, ExecutionJobId, FencingToken, InputRequestId, Instant, LeaseId,
    ModelExchangeId, ProductSessionId, RequestId, ServiceAccountId, Sha256Digest, StageRunId,
    SystemActorId, UserId, WorkerId, WorkerInstanceId, WorkerSessionId,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ACTION_ID_LENGTH: usize = 200;
const MAX_DECISION_LENGTH: usize = 200;
const MAX_REASON_LENGTH: usize = 2_000;

/// Authenticated decision maker supplied by the Control Plane command adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthenticatedActor {
    User(UserId),
    ServiceAccount(ServiceAccountId),
    System(SystemActorId),
}

/// Stable identity of the pending user-facing interaction.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum InteractionSubject {
    UserInput(InputRequestId),
    Approval(ApprovalId),
    Attention(AttentionItemId),
}

/// Exact active lease and Worker process authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeRouteAuthority {
    pub lease_id: LeaseId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub worker_session_id: WorkerSessionId,
    pub attempt: u64,
    pub fencing_token: FencingToken,
}

/// Exact execution identity for one active or queued Job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionRoute {
    pub product_session_id: ProductSessionId,
    pub stage_run_id: Option<StageRunId>,
    pub execution_job_id: ExecutionJobId,
    pub job_revision: u64,
    pub runtime: Option<RuntimeRouteAuthority>,
    pub worker_slot_revision: Option<u64>,
    pub model_exchange_id: Option<ModelExchangeId>,
}

/// Complete authority for a pending input, approval, or Attention decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecisionRouteBinding {
    pub execution: ExecutionRoute,
    pub action_id: String,
    pub decision_revision: u64,
}

/// Registration of one pending decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionRegistration {
    pub subject: InteractionSubject,
    pub binding: DecisionRouteBinding,
    pub authorized_actor: AuthenticatedActor,
    pub expires_at: Instant,
    /// Empty except for Attention; these are the only accepted exact choices.
    pub attention_decisions: Vec<String>,
}

/// A typed human response. Each variant is accepted only for its matching subject.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InteractionDecision {
    UserInput {
        value_sha256: Sha256Digest,
    },
    Approve {
        reason_sha256: Sha256Digest,
    },
    Reject {
        reason_sha256: Sha256Digest,
    },
    ResolveAttention {
        decision: String,
        resolution_sha256: Sha256Digest,
    },
}

/// One authenticated response command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionResponse {
    pub request_id: RequestId,
    pub actor: AuthenticatedActor,
    pub subject: InteractionSubject,
    pub binding: DecisionRouteBinding,
    pub decision: InteractionDecision,
    pub responded_at: Instant,
}

/// Final state selected for the pending interaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionOutcome {
    InputReceived,
    Approved,
    Rejected,
    AttentionResolved,
    Expired,
}

/// Whether the router created a new route result or returned an exact replay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteWriteStatus {
    Applied,
    Duplicate,
}

/// Frozen output handed to the input/approval/Attention execution adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionRouteReceipt {
    pub status: RouteWriteStatus,
    pub request_id: RequestId,
    pub subject: InteractionSubject,
    pub binding: DecisionRouteBinding,
    pub previous_revision: u64,
    pub current_revision: u64,
    pub outcome: InteractionOutcome,
}

/// A trusted Control Plane expiry command for an unresolved interaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionExpiry {
    pub request_id: RequestId,
    pub subject: InteractionSubject,
    pub binding: DecisionRouteBinding,
    pub expired_at: Instant,
}

/// Current cancellation scope and its active execution routes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCancellationSnapshot {
    pub product_session_id: ProductSessionId,
    pub revision: u64,
    pub authorized_actor: AuthenticatedActor,
    pub active_executions: Vec<ExecutionRoute>,
}

/// Authenticated request to cancel exactly one `ProductSession` revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCancellationRequest {
    pub request_id: RequestId,
    pub actor: AuthenticatedActor,
    pub product_session_id: ProductSessionId,
    pub expected_revision: u64,
    pub reason: String,
    pub requested_at: Instant,
}

/// Durable Job mutation the scheduler/queue adapter must apply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobCancellationRoute {
    pub product_session_id: ProductSessionId,
    pub stage_run_id: Option<StageRunId>,
    pub execution_job_id: ExecutionJobId,
    pub expected_revision: u64,
}

/// Worker-slot cancellation bound to the current process, lease, and fencing token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerCancellationRoute {
    pub product_session_id: ProductSessionId,
    pub stage_run_id: Option<StageRunId>,
    pub execution_job_id: ExecutionJobId,
    pub runtime: RuntimeRouteAuthority,
    pub expected_revision: u64,
}

/// Model-stream cancellation bound to the same exact active runtime authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelStreamCancellationRoute {
    pub product_session_id: ProductSessionId,
    pub stage_run_id: Option<StageRunId>,
    pub execution_job_id: ExecutionJobId,
    pub runtime: RuntimeRouteAuthority,
    pub model_exchange_id: ModelExchangeId,
}

/// All cancellation messages for one execution, with no scope-wide wildcard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionCancellationRoutes {
    pub job: JobCancellationRoute,
    pub worker: Option<WorkerCancellationRoute>,
    pub model_stream: Option<ModelStreamCancellationRoute>,
}

/// Frozen cancellation output handed to queue, Worker, and model-stream adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCancellationReceipt {
    pub status: RouteWriteStatus,
    pub request_id: RequestId,
    pub product_session_id: ProductSessionId,
    pub previous_revision: u64,
    pub current_revision: u64,
    pub routes: Vec<ExecutionCancellationRoutes>,
}

/// Stable rejection returned before any route is emitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InteractionRoutingError {
    InvalidField(&'static str),
    DuplicateRegistration,
    UnknownInteraction,
    UnknownProductSession,
    ActorMismatch,
    SubjectMismatch,
    BindingMismatch,
    RevisionConflict { expected: u64, actual: u64 },
    DecisionKindMismatch,
    AttentionDecisionNotAllowed,
    AlreadyResolved,
    IdempotencyConflict,
    SessionAlreadyCancelled,
}

impl fmt::Display for InteractionRoutingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidField(field) => write!(formatter, "invalid routing field: {field}"),
            Self::DuplicateRegistration => formatter.write_str("interaction is already registered"),
            Self::UnknownInteraction => formatter.write_str("interaction is not registered"),
            Self::UnknownProductSession => formatter.write_str("ProductSession is not registered"),
            Self::ActorMismatch => {
                formatter.write_str("authenticated actor does not own this route")
            }
            Self::SubjectMismatch => formatter.write_str("interaction subject does not match"),
            Self::BindingMismatch => formatter.write_str("interaction binding does not match"),
            Self::RevisionConflict { expected, actual } => {
                write!(
                    formatter,
                    "revision conflict: expected {expected}, actual {actual}"
                )
            }
            Self::DecisionKindMismatch => {
                formatter.write_str("decision kind does not match interaction kind")
            }
            Self::AttentionDecisionNotAllowed => {
                formatter.write_str("Attention decision is not one of the sealed choices")
            }
            Self::AlreadyResolved => formatter.write_str("interaction is already resolved"),
            Self::IdempotencyConflict => {
                formatter.write_str("request ID was replayed with different input")
            }
            Self::SessionAlreadyCancelled => {
                formatter.write_str("ProductSession is already cancelled")
            }
        }
    }
}

impl std::error::Error for InteractionRoutingError {}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedInteraction {
    request_id: RequestId,
    request_fingerprint: String,
    outcome: InteractionOutcome,
    current_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InteractionRecord {
    registration: InteractionRegistration,
    resolved: Option<ResolvedInteraction>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CancelledSession {
    request: SessionCancellationRequest,
    request_fingerprint: String,
    receipt: SessionCancellationReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionCancellationRecord {
    snapshot: SessionCancellationSnapshot,
    cancelled: Option<CancelledSession>,
}

/// In-memory aggregate used behind a durable adapter. All outputs are complete,
/// deterministic route facts and can be persisted atomically by that adapter.
#[derive(Default)]
pub struct InteractionRouter {
    interactions: HashMap<InteractionSubject, InteractionRecord>,
    sessions: HashMap<ProductSessionId, SessionCancellationRecord>,
}

#[allow(clippy::missing_errors_doc)]
impl InteractionRouter {
    /// Registers one immutable pending interaction.
    pub fn register_interaction(
        &mut self,
        mut registration: InteractionRegistration,
    ) -> Result<(), InteractionRoutingError> {
        validate_registration(&registration)?;
        registration.attention_decisions.sort_unstable();
        registration.attention_decisions.dedup();
        if self.interactions.contains_key(&registration.subject) {
            return Err(InteractionRoutingError::DuplicateRegistration);
        }
        self.interactions.insert(
            registration.subject.clone(),
            InteractionRecord {
                registration,
                resolved: None,
            },
        );
        Ok(())
    }

    /// Routes a response only when actor, subject, execution, lease, action,
    /// and revision all match the sealed registration.
    pub fn respond(
        &mut self,
        response: &InteractionResponse,
    ) -> Result<InteractionRouteReceipt, InteractionRoutingError> {
        validate_response(response)?;
        let record = self
            .interactions
            .get_mut(&response.subject)
            .ok_or(InteractionRoutingError::UnknownInteraction)?;
        require_response_authority(record, response)?;
        let fingerprint = response_fingerprint(response);
        if let Some(resolved) = &record.resolved {
            return replay_interaction(record, &response.request_id, &fingerprint, resolved);
        }
        let outcome = if response.responded_at.0 > record.registration.expires_at.0 {
            InteractionOutcome::Expired
        } else {
            validate_decision(&record.registration, &response.decision)?
        };
        let current_revision = next_revision(record.registration.binding.decision_revision)?;
        record.resolved = Some(ResolvedInteraction {
            request_id: response.request_id.clone(),
            request_fingerprint: fingerprint,
            outcome,
            current_revision,
        });
        Ok(interaction_receipt(
            record,
            response.request_id.clone(),
            RouteWriteStatus::Applied,
            outcome,
            current_revision,
        ))
    }

    /// Expires one exact unresolved interaction. Replaying the same expiry is
    /// a duplicate; a different command cannot replace the final outcome.
    pub fn expire(
        &mut self,
        expiry: &InteractionExpiry,
    ) -> Result<InteractionRouteReceipt, InteractionRoutingError> {
        validate_request_id(&expiry.request_id)?;
        validate_binding(&expiry.binding)?;
        validate_instant(&expiry.expired_at)?;
        let record = self
            .interactions
            .get_mut(&expiry.subject)
            .ok_or(InteractionRoutingError::UnknownInteraction)?;
        if record.registration.subject != expiry.subject {
            return Err(InteractionRoutingError::SubjectMismatch);
        }
        if record.registration.binding != expiry.binding {
            return Err(InteractionRoutingError::BindingMismatch);
        }
        if expiry.expired_at.0 < record.registration.expires_at.0 {
            return Err(InteractionRoutingError::InvalidField("expiredAt"));
        }
        let fingerprint = expiry_fingerprint(expiry);
        if let Some(resolved) = &record.resolved {
            return replay_interaction(record, &expiry.request_id, &fingerprint, resolved);
        }
        let current_revision = next_revision(record.registration.binding.decision_revision)?;
        record.resolved = Some(ResolvedInteraction {
            request_id: expiry.request_id.clone(),
            request_fingerprint: fingerprint,
            outcome: InteractionOutcome::Expired,
            current_revision,
        });
        Ok(interaction_receipt(
            record,
            expiry.request_id.clone(),
            RouteWriteStatus::Applied,
            InteractionOutcome::Expired,
            current_revision,
        ))
    }

    /// Registers one `ProductSession` cancellation snapshot. Every execution in
    /// the snapshot must name this exact `ProductSession`.
    pub fn register_cancellation_scope(
        &mut self,
        mut snapshot: SessionCancellationSnapshot,
    ) -> Result<(), InteractionRoutingError> {
        validate_actor(&snapshot.authorized_actor)?;
        validate_id(&snapshot.product_session_id.0, "productSessionId", "psn_")?;
        validate_revision(snapshot.revision, "revision")?;
        for execution in &snapshot.active_executions {
            validate_execution(execution)?;
            if execution.product_session_id != snapshot.product_session_id {
                return Err(InteractionRoutingError::BindingMismatch);
            }
        }
        snapshot
            .active_executions
            .sort_unstable_by(|left, right| left.execution_job_id.0.cmp(&right.execution_job_id.0));
        if snapshot
            .active_executions
            .windows(2)
            .any(|pair| pair[0].execution_job_id == pair[1].execution_job_id)
        {
            return Err(InteractionRoutingError::DuplicateRegistration);
        }
        if self.sessions.contains_key(&snapshot.product_session_id) {
            return Err(InteractionRoutingError::DuplicateRegistration);
        }
        self.sessions.insert(
            snapshot.product_session_id.clone(),
            SessionCancellationRecord {
                snapshot,
                cancelled: None,
            },
        );
        Ok(())
    }

    /// Produces exact, independently applicable Job, Worker, and model-stream
    /// cancellation routes for one `ProductSession` only.
    pub fn cancel_session(
        &mut self,
        request: &SessionCancellationRequest,
    ) -> Result<SessionCancellationReceipt, InteractionRoutingError> {
        validate_cancellation_request(request)?;
        let record = self
            .sessions
            .get_mut(&request.product_session_id)
            .ok_or(InteractionRoutingError::UnknownProductSession)?;
        let fingerprint = cancellation_fingerprint(request);
        if let Some(cancelled) = &record.cancelled {
            if cancelled.request.request_id == request.request_id {
                if cancelled.request_fingerprint != fingerprint {
                    return Err(InteractionRoutingError::IdempotencyConflict);
                }
                let mut duplicate = cancelled.receipt.clone();
                duplicate.status = RouteWriteStatus::Duplicate;
                return Ok(duplicate);
            }
            return Err(InteractionRoutingError::SessionAlreadyCancelled);
        }
        if record.snapshot.authorized_actor != request.actor {
            return Err(InteractionRoutingError::ActorMismatch);
        }
        if record.snapshot.revision != request.expected_revision {
            return Err(InteractionRoutingError::RevisionConflict {
                expected: request.expected_revision,
                actual: record.snapshot.revision,
            });
        }
        let current_revision = next_revision(record.snapshot.revision)?;
        let routes = record
            .snapshot
            .active_executions
            .iter()
            .map(cancellation_routes)
            .collect();
        let receipt = SessionCancellationReceipt {
            status: RouteWriteStatus::Applied,
            request_id: request.request_id.clone(),
            product_session_id: request.product_session_id.clone(),
            previous_revision: record.snapshot.revision,
            current_revision,
            routes,
        };
        record.cancelled = Some(CancelledSession {
            request: request.clone(),
            request_fingerprint: fingerprint,
            receipt: receipt.clone(),
        });
        Ok(receipt)
    }
}

fn validate_registration(
    registration: &InteractionRegistration,
) -> Result<(), InteractionRoutingError> {
    validate_subject(&registration.subject)?;
    validate_binding(&registration.binding)?;
    validate_actor(&registration.authorized_actor)?;
    validate_instant(&registration.expires_at)?;
    match &registration.subject {
        InteractionSubject::Attention(_) => {
            if registration.attention_decisions.is_empty() {
                return Err(InteractionRoutingError::InvalidField("attentionDecisions"));
            }
            for decision in &registration.attention_decisions {
                validate_bounded_text(decision, MAX_DECISION_LENGTH, "attentionDecision")?;
            }
        }
        InteractionSubject::UserInput(_) | InteractionSubject::Approval(_) => {
            if !registration.attention_decisions.is_empty() {
                return Err(InteractionRoutingError::InvalidField("attentionDecisions"));
            }
        }
    }
    Ok(())
}

fn validate_response(response: &InteractionResponse) -> Result<(), InteractionRoutingError> {
    validate_request_id(&response.request_id)?;
    validate_actor(&response.actor)?;
    validate_subject(&response.subject)?;
    validate_binding(&response.binding)?;
    validate_instant(&response.responded_at)?;
    match &response.decision {
        InteractionDecision::UserInput { value_sha256 }
        | InteractionDecision::Approve {
            reason_sha256: value_sha256,
        }
        | InteractionDecision::Reject {
            reason_sha256: value_sha256,
        } => validate_digest(value_sha256),
        InteractionDecision::ResolveAttention {
            decision,
            resolution_sha256,
        } => {
            validate_bounded_text(decision, MAX_DECISION_LENGTH, "attentionDecision")?;
            validate_digest(resolution_sha256)
        }
    }
}

fn require_response_authority(
    record: &InteractionRecord,
    response: &InteractionResponse,
) -> Result<(), InteractionRoutingError> {
    if record.registration.subject != response.subject {
        return Err(InteractionRoutingError::SubjectMismatch);
    }
    if record.registration.authorized_actor != response.actor {
        return Err(InteractionRoutingError::ActorMismatch);
    }
    if record.registration.binding != response.binding {
        if record.registration.binding.decision_revision != response.binding.decision_revision {
            return Err(InteractionRoutingError::RevisionConflict {
                expected: response.binding.decision_revision,
                actual: record.registration.binding.decision_revision,
            });
        }
        return Err(InteractionRoutingError::BindingMismatch);
    }
    Ok(())
}

fn validate_decision(
    registration: &InteractionRegistration,
    decision: &InteractionDecision,
) -> Result<InteractionOutcome, InteractionRoutingError> {
    match (&registration.subject, decision) {
        (InteractionSubject::UserInput(_), InteractionDecision::UserInput { .. }) => {
            Ok(InteractionOutcome::InputReceived)
        }
        (InteractionSubject::Approval(_), InteractionDecision::Approve { .. }) => {
            Ok(InteractionOutcome::Approved)
        }
        (InteractionSubject::Approval(_), InteractionDecision::Reject { .. }) => {
            Ok(InteractionOutcome::Rejected)
        }
        (
            InteractionSubject::Attention(_),
            InteractionDecision::ResolveAttention { decision, .. },
        ) => {
            if registration
                .attention_decisions
                .binary_search(decision)
                .is_err()
            {
                return Err(InteractionRoutingError::AttentionDecisionNotAllowed);
            }
            Ok(InteractionOutcome::AttentionResolved)
        }
        _ => Err(InteractionRoutingError::DecisionKindMismatch),
    }
}

fn replay_interaction(
    record: &InteractionRecord,
    request_id: &RequestId,
    fingerprint: &str,
    resolved: &ResolvedInteraction,
) -> Result<InteractionRouteReceipt, InteractionRoutingError> {
    if &resolved.request_id != request_id {
        return Err(InteractionRoutingError::AlreadyResolved);
    }
    if resolved.request_fingerprint != fingerprint {
        return Err(InteractionRoutingError::IdempotencyConflict);
    }
    Ok(interaction_receipt(
        record,
        request_id.clone(),
        RouteWriteStatus::Duplicate,
        resolved.outcome,
        resolved.current_revision,
    ))
}

fn interaction_receipt(
    record: &InteractionRecord,
    request_id: RequestId,
    status: RouteWriteStatus,
    outcome: InteractionOutcome,
    current_revision: u64,
) -> InteractionRouteReceipt {
    InteractionRouteReceipt {
        status,
        request_id,
        subject: record.registration.subject.clone(),
        binding: record.registration.binding.clone(),
        previous_revision: record.registration.binding.decision_revision,
        current_revision,
        outcome,
    }
}

fn cancellation_routes(execution: &ExecutionRoute) -> ExecutionCancellationRoutes {
    let job = JobCancellationRoute {
        product_session_id: execution.product_session_id.clone(),
        stage_run_id: execution.stage_run_id.clone(),
        execution_job_id: execution.execution_job_id.clone(),
        expected_revision: execution.job_revision,
    };
    let worker = execution
        .runtime
        .clone()
        .zip(execution.worker_slot_revision)
        .map(|(runtime, expected_revision)| WorkerCancellationRoute {
            product_session_id: execution.product_session_id.clone(),
            stage_run_id: execution.stage_run_id.clone(),
            execution_job_id: execution.execution_job_id.clone(),
            runtime,
            expected_revision,
        });
    let model_stream = execution
        .runtime
        .clone()
        .zip(execution.model_exchange_id.clone())
        .map(
            |(runtime, model_exchange_id)| ModelStreamCancellationRoute {
                product_session_id: execution.product_session_id.clone(),
                stage_run_id: execution.stage_run_id.clone(),
                execution_job_id: execution.execution_job_id.clone(),
                runtime,
                model_exchange_id,
            },
        );
    ExecutionCancellationRoutes {
        job,
        worker,
        model_stream,
    }
}

fn validate_cancellation_request(
    request: &SessionCancellationRequest,
) -> Result<(), InteractionRoutingError> {
    validate_request_id(&request.request_id)?;
    validate_actor(&request.actor)?;
    validate_id(&request.product_session_id.0, "productSessionId", "psn_")?;
    validate_revision(request.expected_revision, "expectedRevision")?;
    validate_bounded_text(&request.reason, MAX_REASON_LENGTH, "reason")?;
    validate_instant(&request.requested_at)
}

fn validate_binding(binding: &DecisionRouteBinding) -> Result<(), InteractionRoutingError> {
    validate_execution(&binding.execution)?;
    validate_revision(binding.decision_revision, "decisionRevision")?;
    validate_action_id(&binding.action_id)
}

fn validate_execution(execution: &ExecutionRoute) -> Result<(), InteractionRoutingError> {
    validate_id(&execution.product_session_id.0, "productSessionId", "psn_")?;
    if let Some(stage_run_id) = &execution.stage_run_id {
        validate_id(&stage_run_id.0, "stageRunId", "run_")?;
    }
    validate_id(&execution.execution_job_id.0, "executionJobId", "job_")?;
    validate_revision(execution.job_revision, "jobRevision")?;
    match (&execution.runtime, execution.worker_slot_revision) {
        (Some(runtime), Some(worker_revision)) => {
            validate_runtime(runtime)?;
            validate_revision(worker_revision, "workerSlotRevision")?;
        }
        (None, None) => {}
        _ => return Err(InteractionRoutingError::InvalidField("runtimeAuthority")),
    }
    if execution.model_exchange_id.is_some() && execution.runtime.is_none() {
        return Err(InteractionRoutingError::InvalidField("modelExchangeId"));
    }
    if let Some(model_exchange_id) = &execution.model_exchange_id {
        validate_id(&model_exchange_id.0, "modelExchangeId", "mdl_")?;
    }
    Ok(())
}

fn validate_runtime(runtime: &RuntimeRouteAuthority) -> Result<(), InteractionRoutingError> {
    validate_id(&runtime.lease_id.0, "leaseId", "lse_")?;
    validate_id(&runtime.worker_id.0, "workerId", "wrk_")?;
    validate_id(&runtime.worker_instance_id.0, "workerInstanceId", "wki_")?;
    validate_id(&runtime.worker_session_id.0, "workerSessionId", "wsn_")?;
    if !(1..=1_000).contains(&runtime.attempt) {
        return Err(InteractionRoutingError::InvalidField("attempt"));
    }
    let token = runtime.fencing_token.0.as_bytes();
    if token.is_empty()
        || token.len() > 20
        || token[0] == b'0'
        || !token.iter().all(u8::is_ascii_digit)
    {
        return Err(InteractionRoutingError::InvalidField("fencingToken"));
    }
    Ok(())
}

fn validate_subject(subject: &InteractionSubject) -> Result<(), InteractionRoutingError> {
    match subject {
        InteractionSubject::UserInput(id) => validate_id(&id.0, "inputRequestId", "inp_"),
        InteractionSubject::Approval(id) => validate_id(&id.0, "approvalId", "apr_"),
        InteractionSubject::Attention(id) => validate_id(&id.0, "attentionItemId", "att_"),
    }
}

fn validate_actor(actor: &AuthenticatedActor) -> Result<(), InteractionRoutingError> {
    match actor {
        AuthenticatedActor::User(id) => validate_id(&id.0, "actorId", "usr_"),
        AuthenticatedActor::ServiceAccount(id) => validate_id(&id.0, "actorId", "svc_"),
        AuthenticatedActor::System(id) => validate_id(&id.0, "actorId", "sys_"),
    }
}

fn validate_request_id(request_id: &RequestId) -> Result<(), InteractionRoutingError> {
    validate_id(&request_id.0, "requestId", "req_")
}

fn validate_digest(digest: &Sha256Digest) -> Result<(), InteractionRoutingError> {
    let Some(value) = digest.0.strip_prefix("sha256:") else {
        return Err(InteractionRoutingError::InvalidField("sha256"));
    };
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(InteractionRoutingError::InvalidField("sha256"));
    }
    Ok(())
}

fn validate_action_id(action_id: &str) -> Result<(), InteractionRoutingError> {
    if action_id.is_empty()
        || action_id.len() > MAX_ACTION_ID_LENGTH
        || !action_id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
    {
        return Err(InteractionRoutingError::InvalidField("actionId"));
    }
    Ok(())
}

fn validate_bounded_text(
    value: &str,
    max: usize,
    field: &'static str,
) -> Result<(), InteractionRoutingError> {
    if value.is_empty() || value.chars().count() > max {
        return Err(InteractionRoutingError::InvalidField(field));
    }
    Ok(())
}

fn validate_revision(revision: u64, field: &'static str) -> Result<(), InteractionRoutingError> {
    if !(1..=MAX_SAFE_INTEGER).contains(&revision) {
        return Err(InteractionRoutingError::InvalidField(field));
    }
    Ok(())
}

fn next_revision(revision: u64) -> Result<u64, InteractionRoutingError> {
    revision
        .checked_add(1)
        .filter(|next| *next <= MAX_SAFE_INTEGER)
        .ok_or(InteractionRoutingError::InvalidField("revision"))
}

fn validate_id(
    value: &str,
    field: &'static str,
    prefix: &str,
) -> Result<(), InteractionRoutingError> {
    let valid = value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
            })
    });
    if !valid {
        return Err(InteractionRoutingError::InvalidField(field));
    }
    Ok(())
}

fn validate_instant(instant: &Instant) -> Result<(), InteractionRoutingError> {
    let value = instant.0.as_bytes();
    let valid = value.len() == 24
        && value[4] == b'-'
        && value[7] == b'-'
        && value[10] == b'T'
        && value[13] == b':'
        && value[16] == b':'
        && value[19] == b'.'
        && value[23] == b'Z'
        && value.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        });
    if !valid {
        return Err(InteractionRoutingError::InvalidField("instant"));
    }
    Ok(())
}

fn response_fingerprint(response: &InteractionResponse) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{}",
        response.actor,
        response.subject,
        response.binding,
        response.decision,
        response.responded_at,
        response.request_id.0
    )
}

fn expiry_fingerprint(expiry: &InteractionExpiry) -> String {
    format!(
        "{:?}|{:?}|{:?}|{}",
        expiry.subject, expiry.binding, expiry.expired_at, expiry.request_id.0
    )
}

fn cancellation_fingerprint(request: &SessionCancellationRequest) -> String {
    format!(
        "{:?}|{}|{}|{}|{:?}|{}",
        request.actor,
        request.product_session_id.0,
        request.expected_revision,
        request.reason,
        request.requested_at,
        request.request_id.0
    )
}
