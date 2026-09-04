// SPDX-License-Identifier: Apache-2.0

//! Receipt-first transaction for one generated Worker `job.outcome`.

use std::{collections::HashSet, fmt};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{CommandEnvelope, CommandName};
use winwincode_audit::{
    AuditAction, AuditEvent, AuditEventId, AuditExecutionIdentity, AuditExecutionSubjectKind,
    AuditState, AuditSubject,
};
use winwincode_delivery::{
    application::stage::{
        DeliveryTerminalOutcomeFacts, DurableTerminalOutcomeInput, TerminalArtifactReference,
        TerminalOutcomeStatus, reconcile_durable_settled_terminal_outcome,
        reconcile_durable_terminal_outcome,
    },
    domain::{Delivery, StageRunStatus},
    store::{ApplyDeliveryTerminalOutcome, DeliveryCommand, DeliveryCommandPort, DeliveryStore},
};
use winwincode_domain::RepositoryScope;
use winwincode_domain::{
    CodexThreadId, ControlPlaneEventId, DeliveryId, DeliveryTaskId, ExecutionAckSequence,
    ExecutionJobId, ExecutionMessageId, Instant, ProductSessionId, RequestId, SchemaVersion,
    SessionIdentity, Sha256Digest, StageRunId,
};
use winwincode_execution_port::generated::{
    ArtifactReference, ExecutionJob, ExecutionOutcomeStatus, ExecutionScope, JobOutcomeMessage,
    JobOutcomeMessageKind,
};
use winwincode_storage::{
    CommitReceipt, DurableOutboxEvent, NewOutboxEvent, PendingAuditEvent, ProductStateStorage,
    PublicEventActor, PublicEventSource, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit, StateMutation, StorageError, public_actor_from_receipt_key,
};

use crate::delivery_transaction::{
    StagedDeliveryJournal, delivery_journal_key, delivery_stream_id, load_durable_execution_job,
};

use crate::session_binding_transaction::{
    DeliveryStageRuntimeInvalidation, delivery_stage_runtime_invalidated_event,
    execution_message_actor_key, execution_message_request_id, instant_millis, projection_event_id,
    require_id, validate_delivery_stage_runtime_invalidation,
};
use crate::worker_policy::VerifierPolicyAuthority;
use crate::{
    ArtifactEnterpriseQuotaSagaError, DeliveryChangeKind, OutboxError,
    delivery_changed_event_for_scope, execution_audit_event, execution_audit_event_with_state,
    repository_scope_key, validate_delivery_changed_receipt,
};

const TERMINAL_PHASE: &str = "terminal-outcome";
const TERMINAL_TOPIC: &str = "delivery.stage.terminal";
const TERMINAL_EVENT_NAMESPACE: &[u8] = b"winwincode.delivery-stage-terminal.v1";
const TERMINAL_RUNTIME_NAMESPACE: &[u8] =
    b"winwincode.delivery-stage-terminal-runtime-invalidation.v1";
const TERMINAL_AUTHORITY_STREAM_PREFIX: &str = "delivery-terminal-authority:";

/// Durable receipt for one accepted Worker terminal outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryTerminalOutcomeCommitReceipt {
    receipt: CommitReceipt,
}

impl DeliveryTerminalOutcomeCommitReceipt {
    #[must_use]
    pub const fn receipt(&self) -> &CommitReceipt {
        &self.receipt
    }
}

/// Failure of a terminal-outcome transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryTerminalOutcomeCommitError {
    Storage(StorageError),
    WorkerQuotaPending {
        commit: Box<DeliveryTerminalOutcomeCommitReceipt>,
        source: crate::WorkerExecutionLifecycleError,
    },
    ArtifactQuotaPending {
        commit: Box<DeliveryTerminalOutcomeCommitReceipt>,
        source: ArtifactEnterpriseQuotaSagaError,
    },
    PublicationPending {
        commit: Box<DeliveryTerminalOutcomeCommitReceipt>,
        source: OutboxError,
    },
}

impl DeliveryTerminalOutcomeCommitError {
    #[must_use]
    pub fn committed_receipt(&self) -> Option<&DeliveryTerminalOutcomeCommitReceipt> {
        match self {
            Self::WorkerQuotaPending { commit, .. }
            | Self::ArtifactQuotaPending { commit, .. }
            | Self::PublicationPending { commit, .. } => Some(commit),
            Self::Storage(_) => None,
        }
    }
}

impl fmt::Display for DeliveryTerminalOutcomeCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => {
                write!(formatter, "terminal outcome transaction failed: {error}")
            }
            Self::WorkerQuotaPending { source, .. } => write!(
                formatter,
                "terminal outcome committed, but Worker quota settlement remains pending: {source}"
            ),
            Self::ArtifactQuotaPending { source, .. } => write!(
                formatter,
                "terminal outcome committed, but Artifact quota release remains pending: {source}"
            ),
            Self::PublicationPending { source, .. } => write!(
                formatter,
                "terminal outcome committed, but its events remain pending: {source}"
            ),
        }
    }
}

impl std::error::Error for DeliveryTerminalOutcomeCommitError {}

impl From<StorageError> for DeliveryTerminalOutcomeCommitError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

pub(crate) fn execute_at(
    storage: &mut dyn ProductStateStorage,
    scope: &RepositoryScope,
    message: &JobOutcomeMessage,
    facts: &DeliveryTerminalOutcomeFacts,
    server_time: &Instant,
) -> Result<DeliveryTerminalOutcomeCommitReceipt, DeliveryTerminalOutcomeCommitError> {
    let phase = TerminalPhase::new(scope, message)?;
    if let Some(receipt) = storage.load_receipt(&phase.receipt_identity, &phase.command_digest)? {
        validate_receipt(&receipt, &phase, message, true)?;
        validate_terminal_pending_audit_event(storage, &receipt, &phase)?;
        return Ok(DeliveryTerminalOutcomeCommitReceipt { receipt });
    }
    validate_message_shape(message)?;
    validate_trusted_lease_time(facts, server_time)?;

    let (durable, job) = load_durable_execution_job(storage, &message.lease.job_id)?;
    let context = TerminalContext::from_durable(scope, &durable, &job, server_time)?;
    let current = load_current_delivery(storage, &context.delivery_id)?;
    let session_identity = match validate_current_job_binding(&current, &job, &context) {
        Ok(identity) => identity,
        Err(source) => {
            let receipt = recover_raced_receipt(storage, &phase, message, source)?;
            return Ok(DeliveryTerminalOutcomeCommitReceipt { receipt });
        }
    };
    validate_message_authority(message, &job, &context, facts, &session_identity)?;
    let commit = if requires_atomic_handoff(&current, &context, facts)? {
        commit_pending_handoff
    } else {
        commit_terminal
    };
    let receipt = commit(
        storage,
        message,
        facts,
        &phase,
        &context,
        &current,
        &session_identity,
    )
    .or_else(|source| recover_raced_receipt(storage, &phase, message, source))?;
    Ok(DeliveryTerminalOutcomeCommitReceipt { receipt })
}

pub(crate) fn verifier_policy_authority_at(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    message: &JobOutcomeMessage,
    facts: &DeliveryTerminalOutcomeFacts,
    server_time: &Instant,
) -> Result<Option<VerifierPolicyAuthority>, StorageError> {
    let phase = TerminalPhase::new(scope, message)?;
    if let Some(receipt) = storage.load_receipt(&phase.receipt_identity, &phase.command_digest)? {
        validate_receipt(&receipt, &phase, message, true)?;
        validate_terminal_pending_audit_event(storage, &receipt, &phase)?;
        return Ok(None);
    }
    validate_message_shape(message)?;
    validate_trusted_lease_time(facts, server_time)?;
    let (durable, job) = load_durable_execution_job(storage, &message.lease.job_id)?;
    let context = TerminalContext::from_durable(scope, &durable, &job, server_time)?;
    let current = load_current_delivery(storage, &context.delivery_id)?;
    let session_identity = match validate_current_job_binding(&current, &job, &context) {
        Ok(identity) => identity,
        Err(source) => {
            recover_raced_receipt(storage, &phase, message, source)?;
            return Ok(None);
        }
    };
    validate_message_authority(message, &job, &context, facts, &session_identity)?;
    let PublicEventActor::User { id: user_id } =
        public_actor_from_receipt_key(durable.receipt_identity().actor_key())?
    else {
        return Err(StorageError::invalid_input(
            "Verifier Policy requires the authenticated User actor",
        ));
    };
    let mut runs = current
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| run.id == context.stage_run_id);
    let run = runs.next().ok_or_else(|| {
        StorageError::invalid_input("Verifier Policy StageRun authority is missing")
    })?;
    if runs.next().is_some() {
        return Err(StorageError::invalid_input(
            "Verifier Policy StageRun authority is ambiguous",
        ));
    }
    let subject_sha256 = crate::enterprise_policy_subject_sha256(&(
        &context.job_event,
        &job,
        message,
        &session_identity,
    ))
    .map_err(|_| StorageError::adapter("Verifier Policy subject encoding failed"))?;
    Ok(Some(VerifierPolicyAuthority {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
        user_id,
        request_id: execution_message_request_id(
            &message.message_id,
            "enterprise-verifier-policy",
        )?,
        evaluated_at: message.sent_at.clone(),
        verifier_resource: format!("verifier:{}", run.role),
        subject_sha256,
    }))
}

fn validate_trusted_lease_time(
    facts: &DeliveryTerminalOutcomeFacts,
    server_time: &Instant,
) -> Result<(), StorageError> {
    let now = instant_millis(server_time)?;
    let issued_at = instant_millis(facts.authority().issued_at())?;
    let expires_at = instant_millis(facts.authority().expires_at())?;
    if now < issued_at {
        return Err(StorageError::invalid_input(
            "terminal ingress precedes the scheduler-owned lease",
        ));
    }
    if now >= expires_at {
        return Err(StorageError::invalid_input(
            "terminal ingress observed an expired scheduler-owned lease",
        ));
    }
    Ok(())
}

fn requires_atomic_handoff(
    delivery: &Delivery,
    context: &TerminalContext,
    facts: &DeliveryTerminalOutcomeFacts,
) -> Result<bool, StorageError> {
    if facts.status() != TerminalOutcomeStatus::Succeeded {
        return Ok(false);
    }
    let mut runs = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| run.id == context.stage_run_id);
    let run = runs.next().ok_or_else(|| {
        StorageError::invalid_input("terminal outcome current StageRun is missing")
    })?;
    if runs.next().is_some() {
        return Err(StorageError::invalid_input(
            "terminal outcome StageRun identity is ambiguous",
        ));
    }
    Ok(
        run.stage != winwincode_delivery::domain::DeliveryStage::Verifying
            || !matches!(run.role.as_str(), "verifier" | "adversarial-verifier"),
    )
}

struct TerminalPhase {
    receipt_identity: ReceiptIdentity,
    command_digest: Sha256Digest,
}

impl TerminalPhase {
    fn new(scope: &RepositoryScope, message: &JobOutcomeMessage) -> Result<Self, StorageError> {
        let receipt_identity = ReceiptIdentity::new(
            execution_message_actor_key(&message.message_id)?,
            repository_scope_key(scope)?,
            execution_message_request_id(&message.message_id, TERMINAL_PHASE)?,
        )?;
        let encoded = serde_json::to_vec(message).map_err(|error| {
            StorageError::adapter(format!("failed to encode job.outcome: {error}"))
        })?;
        let command_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(encoded)));
        Ok(Self {
            receipt_identity,
            command_digest,
        })
    }

    fn request_digest(&self) -> Result<String, StorageError> {
        self.command_digest
            .0
            .strip_prefix("sha256:")
            .map(str::to_owned)
            .ok_or_else(|| StorageError::invalid_input("terminal outcome digest is not canonical"))
    }
}

struct TerminalContext {
    accepted_at: Instant,
    scope_key: ReceiptScopeKey,
    repository_scope: RepositoryScope,
    delivery_id: DeliveryId,
    delivery_task_id: Option<DeliveryTaskId>,
    stage_run_id: StageRunId,
    product_session_id: ProductSessionId,
    job_event: DurableExecutionJobRef,
}

impl TerminalContext {
    fn from_durable(
        scope: &RepositoryScope,
        durable: &DurableOutboxEvent,
        job: &ExecutionJob,
        accepted_at: &Instant,
    ) -> Result<Self, StorageError> {
        let ExecutionScope::DeliveryStageExecutionScope(job_scope) = &job.scope else {
            return Err(StorageError::invalid_input(
                "terminal outcome ExecutionJob has foreign scope",
            ));
        };
        let scope_key = repository_scope_key(scope)?;
        if durable.receipt_identity().scope_key() != &scope_key
            || durable.stream_id() != delivery_stream_id(&job_scope.delivery_id)
            || durable.revision() == 0
            || job.workspace.repository_id != scope.repository_id
        {
            return Err(StorageError::invalid_input(
                "terminal outcome ExecutionJob does not belong to the trusted repository scope",
            ));
        }
        let job_event = DurableExecutionJobRef::new(durable);
        validate_durable_job_ref(&job_event)?;
        Ok(Self {
            accepted_at: accepted_at.clone(),
            scope_key,
            repository_scope: scope.clone(),
            delivery_id: job_scope.delivery_id.clone(),
            delivery_task_id: job_scope.delivery_task_id.clone(),
            stage_run_id: job_scope.stage_run_id.clone(),
            product_session_id: job_scope.product_session_id.clone(),
            job_event,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableExecutionJobRef {
    event_id: String,
    event_sequence: u64,
    receipt_actor_key_sha256: String,
    receipt_command_digest: Sha256Digest,
    receipt_request_id: RequestId,
    receipt_revision: u64,
    receipt_scope_key_sha256: String,
    stream_id: String,
}

impl DurableExecutionJobRef {
    fn new(durable: &DurableOutboxEvent) -> Self {
        Self {
            event_id: durable.event().event_id.clone(),
            event_sequence: durable.event().sequence,
            receipt_actor_key_sha256: encoded_key_digest(
                durable.receipt_identity().actor_key().as_bytes(),
            ),
            receipt_command_digest: durable.command_digest().clone(),
            receipt_request_id: durable.receipt_identity().request_id().clone(),
            receipt_revision: durable.revision(),
            receipt_scope_key_sha256: encoded_key_digest(
                durable.receipt_identity().scope_key().as_bytes(),
            ),
            stream_id: durable.stream_id().to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TerminalAcceptedEvent {
    delivery_id: DeliveryId,
    execution_job: DurableExecutionJobRef,
    job_id: ExecutionJobId,
    message_digest: Sha256Digest,
    message_id: ExecutionMessageId,
    outcome: AcceptedTerminalOutcome,
    product_session_id: ProductSessionId,
    revision: u64,
    schema_version: u8,
    stage_run_id: StageRunId,
}

/// Secret-safe terminal facts kept with the accepted message digest. Worker
/// prose, diagnostics, and raw lease authority never enter the durable event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AcceptedTerminalOutcome {
    artifacts: Vec<ArtifactReference>,
    accepted_at_millis: u64,
    codex_thread_id: Option<CodexThreadId>,
    finished_at_millis: u64,
    last_event_sequence: ExecutionAckSequence,
    status: ExecutionOutcomeStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedTerminalAuthority {
    schema_version: u8,
    delivery_id: DeliveryId,
    stage_run_id: StageRunId,
    job_id: ExecutionJobId,
    attempt: u64,
    lease_id: winwincode_domain::LeaseId,
    fencing_token: winwincode_domain::FencingToken,
    worker_id: winwincode_domain::WorkerId,
    worker_instance_id: winwincode_domain::WorkerInstanceId,
    worker_session_id: winwincode_domain::WorkerSessionId,
    issued_at: Instant,
    expires_at: Instant,
    artifacts: Vec<ArtifactReference>,
    accepted_at_millis: u64,
    codex_thread_id: Option<CodexThreadId>,
    finished_at_millis: u64,
    last_event_sequence: ExecutionAckSequence,
    status: ExecutionOutcomeStatus,
    disposition: PersistedTerminalDisposition,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum PersistedTerminalDisposition {
    PendingHandoff,
    Settled {
        delivery_revision: u64,
    },
    Consumed {
        advance_request_id: RequestId,
        advance_command_digest: Sha256Digest,
        delivery_revision: u64,
    },
}

/// Opaque durable success fact consumed by one exact `delivery.advance` state
/// transaction. Construction stays inside the canonical terminal adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeliveryTerminalHandoff {
    facts: DeliveryTerminalOutcomeFacts,
    consumption: StateMutation,
}

impl DeliveryTerminalHandoff {
    pub(crate) const fn facts(&self) -> &DeliveryTerminalOutcomeFacts {
        &self.facts
    }

    pub(crate) fn consumption(&self) -> StateMutation {
        self.consumption.clone()
    }
}

fn persisted_terminal_authority(
    message: &JobOutcomeMessage,
    context: &TerminalContext,
    disposition: PersistedTerminalDisposition,
    accepted_at: &Instant,
) -> Result<PersistedTerminalAuthority, StorageError> {
    let finished_at_millis = instant_millis(&message.outcome.finished_at)?;
    let accepted_at_millis = instant_millis(accepted_at)?;
    if accepted_at_millis < finished_at_millis {
        return Err(StorageError::invalid_input(
            "terminal acceptance precedes the Worker finish time",
        ));
    }
    Ok(PersistedTerminalAuthority {
        schema_version: 1,
        delivery_id: context.delivery_id.clone(),
        stage_run_id: context.stage_run_id.clone(),
        job_id: message.lease.job_id.clone(),
        attempt: u64::try_from(message.lease.attempt)
            .map_err(|_| StorageError::invalid_input("terminal attempt is invalid"))?,
        lease_id: message.lease.lease_id.clone(),
        fencing_token: message.lease.fencing_token.clone(),
        worker_id: message.lease.worker_id.clone(),
        worker_instance_id: message.lease.worker_instance_id.clone(),
        worker_session_id: message.worker_session_id.clone(),
        issued_at: message.lease.issued_at.clone(),
        expires_at: message.lease.expires_at.clone(),
        artifacts: message.outcome.artifacts.clone(),
        accepted_at_millis,
        codex_thread_id: message.outcome.codex_thread_id.clone(),
        finished_at_millis,
        last_event_sequence: message.outcome.last_event_sequence.clone(),
        status: message.outcome.status.clone(),
        disposition,
    })
}

fn validate_message_shape(message: &JobOutcomeMessage) -> Result<(), StorageError> {
    if message.kind != JobOutcomeMessageKind::JobOutcome
        || message.schema_version != SchemaVersion::WinwincodeV1
    {
        return Err(StorageError::invalid_input(
            "job.outcome message discriminator is not canonical",
        ));
    }
    require_id(&message.message_id.0, "xmsg_", "messageId")?;
    require_id(&message.lease.job_id.0, "job_", "lease.jobId")?;
    require_id(&message.lease.lease_id.0, "lse_", "lease.leaseId")?;
    require_id(&message.lease.worker_id.0, "wrk_", "lease.workerId")?;
    require_id(
        &message.lease.worker_instance_id.0,
        "wki_",
        "lease.workerInstanceId",
    )?;
    require_id(&message.worker_session_id.0, "wsn_", "workerSessionId")?;
    if let Some(thread_id) = &message.outcome.codex_thread_id {
        require_id(&thread_id.0, "cdx_", "outcome.codexThreadId")?;
    }
    if message.lease.attempt <= 0
        || message.lease.attempt > 1_000
        || message.lease.fencing_token.0.is_empty()
        || message.lease.fencing_token.0.len() > 20
        || message.lease.fencing_token.0.starts_with('0')
        || !message
            .lease
            .fencing_token
            .0
            .bytes()
            .all(|byte| byte.is_ascii_digit())
        || message.outcome.summary.is_empty()
        || message.outcome.summary.chars().count() > 4_000
    {
        return Err(StorageError::invalid_input(
            "job.outcome lease or summary is invalid",
        ));
    }
    if !(0..=9_007_199_254_740_991).contains(&message.outcome.last_event_sequence.0) {
        return Err(StorageError::invalid_input(
            "job.outcome lastEventSequence is outside the public integer range",
        ));
    }
    if message.outcome.status == ExecutionOutcomeStatus::Succeeded
        && message.outcome.usage.is_none()
    {
        return Err(StorageError::invalid_input(
            "successful job.outcome is missing immutable Usage",
        ));
    }
    if let Some(usage) = &message.outcome.usage
        && (!(0..=9_007_199_254_740_991).contains(&usage.tokens)
            || !(0..=9_007_199_254_740_991).contains(&usage.cost_microunits)
            || !(0..=9_007_199_254_740_991).contains(&usage.runtime_millis))
    {
        return Err(StorageError::invalid_input(
            "job.outcome Usage is outside the public integer range",
        ));
    }
    if message
        .outcome
        .error
        .as_ref()
        .is_some_and(|error| error.message.is_empty() || error.message.chars().count() > 500)
    {
        return Err(StorageError::invalid_input(
            "job.outcome error message does not match the generated schema",
        ));
    }
    let mut artifact_ids = HashSet::with_capacity(message.outcome.artifacts.len());
    for artifact in &message.outcome.artifacts {
        require_id(
            &artifact.artifact_id.0,
            "art_",
            "outcome.artifacts.artifactId",
        )?;
        if !canonical_digest(&artifact.digest.0) || !artifact_ids.insert(&artifact.artifact_id) {
            return Err(StorageError::invalid_input(
                "job.outcome Artifacts must have unique identities and canonical digests",
            ));
        }
    }
    let issued_at = instant_millis(&message.lease.issued_at)?;
    let expires_at = instant_millis(&message.lease.expires_at)?;
    let finished_at = instant_millis(&message.outcome.finished_at)?;
    let sent_at = instant_millis(&message.sent_at)?;
    if issued_at >= expires_at
        || finished_at < issued_at
        || finished_at >= expires_at
        || sent_at < finished_at
        || sent_at > expires_at
    {
        return Err(StorageError::invalid_input(
            "job.outcome time is outside its active lease",
        ));
    }
    Ok(())
}

fn validate_message_authority(
    message: &JobOutcomeMessage,
    job: &ExecutionJob,
    context: &TerminalContext,
    facts: &DeliveryTerminalOutcomeFacts,
    session_identity: &SessionIdentity,
) -> Result<(), StorageError> {
    let active = facts.authority().active_lease();
    let attempt = u64::try_from(message.lease.attempt)
        .map_err(|_| StorageError::invalid_input("job.outcome attempt is out of range"))?;
    let expected_status = terminal_status(&message.outcome.status);
    let metadata = facts.metadata();
    let message_finished_at = instant_millis(&message.outcome.finished_at)?;
    let exact_artifacts = metadata.artifacts().len() == message.outcome.artifacts.len()
        && metadata
            .artifacts()
            .iter()
            .zip(&message.outcome.artifacts)
            .all(|(trusted, reported)| {
                trusted.artifact_id == reported.artifact_id && trusted.digest == reported.digest
            });
    if job.job_id != message.lease.job_id
        || job.attempt != message.lease.attempt
        || context.stage_run_id != *facts.stage_run_id()
        || active.execution_job_id() != &message.lease.job_id
        || active.attempt() != attempt
        || active.lease_id() != &message.lease.lease_id
        || active.fencing_token() != &message.lease.fencing_token
        || active.worker_id() != &message.lease.worker_id
        || active.worker_instance_id() != &message.lease.worker_instance_id
        || active.worker_session_id() != &message.worker_session_id
        || facts.authority().issued_at() != &message.lease.issued_at
        || facts.authority().expires_at() != &message.lease.expires_at
        || facts.status() != expected_status
        || metadata.codex_thread_id() != message.outcome.codex_thread_id.as_ref()
        || metadata.finished_at_millis() != message_finished_at
        || metadata.last_event_sequence() != &message.outcome.last_event_sequence
        || !exact_artifacts
        || &message.session_identity != session_identity
    {
        return Err(StorageError::invalid_input(
            "job.outcome does not match its durable job and sealed terminal authority",
        ));
    }
    Ok(())
}

fn load_current_delivery(
    storage: &dyn ProductStateStorage,
    delivery_id: &DeliveryId,
) -> Result<Delivery, StorageError> {
    let stream_id = delivery_stream_id(delivery_id);
    let state = storage
        .load_state(&stream_id)?
        .ok_or_else(|| StorageError::invalid_input("terminal outcome Delivery state is missing"))?;
    if state.stream_id != stream_id || state.revision == 0 {
        return Err(StorageError::invalid_input(
            "terminal outcome Delivery state is foreign",
        ));
    }
    let delivery = Delivery::decode_json(&state.payload).map_err(|error| {
        StorageError::invalid_input(format!("terminal outcome Delivery is invalid: {error}"))
    })?;
    if delivery.id() != delivery_id || delivery.revision() != state.revision {
        return Err(StorageError::invalid_input(
            "terminal outcome Delivery snapshot differs from durable state",
        ));
    }
    Ok(delivery)
}

fn validate_current_job_binding(
    delivery: &Delivery,
    job: &ExecutionJob,
    context: &TerminalContext,
) -> Result<SessionIdentity, StorageError> {
    let attempt = u64::try_from(job.attempt)
        .map_err(|_| StorageError::invalid_input("ExecutionJob attempt is out of range"))?;
    let matching_runs = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| {
            run.id == context.stage_run_id
                && run.delivery_id == context.delivery_id
                && run.delivery_task_id == context.delivery_task_id
                && run.attempt == attempt
                && matches!(
                    run.status,
                    StageRunStatus::Running | StageRunStatus::Waiting
                )
        })
        .collect::<Vec<_>>();
    let matching_bindings = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| {
            binding.delivery_id == context.delivery_id
                && binding.delivery_task_id == context.delivery_task_id
                && binding.stage_run_id == context.stage_run_id
                && binding.product_session_id == context.product_session_id
                && binding.execution_job_id == job.job_id
        })
        .collect::<Vec<_>>();
    if matching_runs.len() != 1 || matching_bindings.len() != 1 {
        return Err(StorageError::invalid_input(
            "terminal outcome ExecutionJob does not match one current StageRun and SessionBinding",
        ));
    }
    let binding = matching_bindings
        .first()
        .ok_or_else(|| StorageError::invalid_input("terminal outcome SessionBinding is missing"))?;
    let worker_session_id = binding.worker_session_id.clone().ok_or_else(|| {
        StorageError::invalid_input("terminal outcome SessionBinding has no WorkerSession")
    })?;
    let codex_thread_id = binding.codex_thread_id.clone().ok_or_else(|| {
        StorageError::invalid_input("terminal outcome SessionBinding has no CodexThread")
    })?;
    Ok(SessionIdentity {
        codex_thread_id,
        product_session_id: binding.product_session_id.clone(),
        stage_run_id: Some(binding.stage_run_id.clone()),
        worker_session_id,
    })
}

fn terminal_pending_audit_event(
    facts: &DeliveryTerminalOutcomeFacts,
    phase: &TerminalPhase,
    context: &TerminalContext,
    session_identity: &SessionIdentity,
    before: &Delivery,
    after: &Delivery,
) -> Result<PendingAuditEvent, StorageError> {
    let subject = terminal_audit_subject(facts, context, session_identity)?;
    let event_id = AuditEventId::from_digest(&phase.command_digest)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let result_code = terminal_result_code(facts.status());
    execution_audit_event(
        event_id,
        facts.metadata().finished_at_millis(),
        phase.receipt_identity.request_id().clone(),
        &context.repository_scope,
        AuditAction::delivery_state("stage.terminal.accepted")
            .map_err(|error| StorageError::invalid_input(error.to_string()))?,
        before,
        after,
        subject,
        result_code,
    )
}

fn terminal_audit_subject(
    facts: &DeliveryTerminalOutcomeFacts,
    context: &TerminalContext,
    session_identity: &SessionIdentity,
) -> Result<AuditSubject, StorageError> {
    let active = facts.authority().active_lease();
    let codex_thread_id = facts.metadata().codex_thread_id().cloned().ok_or_else(|| {
        StorageError::invalid_input(
            "terminal outcome audit event requires a trusted CodexThread identity",
        )
    })?;
    if session_identity.codex_thread_id != codex_thread_id {
        return Err(StorageError::invalid_input(
            "terminal outcome audit CodexThread differs from the accepted binding",
        ));
    }
    AuditExecutionIdentity::try_new(
        context.product_session_id.clone(),
        active.worker_session_id().clone(),
        codex_thread_id,
        context.stage_run_id.clone(),
        active.execution_job_id().clone(),
        context.delivery_id.clone(),
        context.delivery_task_id.clone(),
        active.worker_id().clone(),
        active.worker_instance_id().clone(),
        active.lease_id().clone(),
        active.attempt(),
        active.fencing_token().clone(),
        facts.metadata().last_event_sequence().clone(),
    )
    .map(AuditSubject::terminal)
    .map_err(|error| StorageError::invalid_input(error.to_string()))
}

const fn terminal_result_code(status: TerminalOutcomeStatus) -> &'static str {
    match status {
        TerminalOutcomeStatus::Succeeded => "execution.terminal.succeeded",
        TerminalOutcomeStatus::Failed => "execution.terminal.failed",
        TerminalOutcomeStatus::InfrastructureError => "execution.terminal.infrastructure_error",
        TerminalOutcomeStatus::Cancelled => "execution.terminal.cancelled",
    }
}

fn commit_pending_handoff(
    storage: &mut dyn ProductStateStorage,
    message: &JobOutcomeMessage,
    facts: &DeliveryTerminalOutcomeFacts,
    phase: &TerminalPhase,
    context: &TerminalContext,
    current: &Delivery,
    session_identity: &SessionIdentity,
) -> Result<CommitReceipt, StorageError> {
    let authority = persisted_terminal_authority(
        message,
        context,
        PersistedTerminalDisposition::PendingHandoff,
        &context.accepted_at,
    )?;
    let authority_payload = serde_json::to_vec(&authority).map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode terminal handoff authority: {error}"
        ))
    })?;
    let accepted = terminal_accepted_event(
        message,
        phase,
        context,
        current.revision(),
        &context.accepted_at,
    )?;
    let public_scope = crate::public_repository_scope(&context.repository_scope);
    let public_source = PublicEventSource::SessionExecutionWorker {
        worker_id: message.lease.worker_id.clone(),
        worker_session_id: message.worker_session_id.clone(),
        lease_id: message.lease.lease_id.clone(),
        codex_thread_id: session_identity.codex_thread_id.clone(),
        session_identity: session_identity.clone(),
    };
    let invalidated =
        delivery_stage_runtime_invalidated_event(&DeliveryStageRuntimeInvalidation {
            scope_key: &context.scope_key,
            delivery_id: &context.delivery_id,
            stage_run_id: &context.stage_run_id,
            product_session_id: &context.product_session_id,
            session_identity,
            revision: current.revision(),
            event_namespace: TERMINAL_RUNTIME_NAMESPACE,
            scope: public_scope,
            occurred_at: message.sent_at.clone(),
            source: public_source,
        })?;
    let state_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&authority_payload)));
    let audit_state = AuditState::changed(None, state_digest)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let pending_audit_event = execution_audit_event_with_state(
        AuditEventId::from_digest(&phase.command_digest)
            .map_err(|error| StorageError::invalid_input(error.to_string()))?,
        facts.metadata().finished_at_millis(),
        phase.receipt_identity.request_id().clone(),
        &context.repository_scope,
        AuditAction::delivery_state("stage.terminal.handoff_pending")
            .map_err(|error| StorageError::invalid_input(error.to_string()))?,
        audit_state,
        terminal_audit_subject(facts, context, session_identity)?,
        terminal_result_code(facts.status()),
    )?;
    let commit = StateCommit::new(
        phase.receipt_identity.clone(),
        phase.command_digest.clone(),
        terminal_authority_stream_id(&message.lease.job_id),
        0,
        authority_payload,
        vec![accepted, invalidated],
    )
    .with_pending_audit_event(pending_audit_event);
    let receipt = storage.commit(&commit)?;
    validate_receipt(&receipt, phase, message, receipt.idempotent_replay)?;
    validate_terminal_pending_audit_event(storage, &receipt, phase)?;
    Ok(receipt)
}

fn commit_terminal(
    storage: &mut dyn ProductStateStorage,
    message: &JobOutcomeMessage,
    facts: &DeliveryTerminalOutcomeFacts,
    phase: &TerminalPhase,
    context: &TerminalContext,
    current: &Delivery,
    session_identity: &SessionIdentity,
) -> Result<CommitReceipt, StorageError> {
    let journal_key = delivery_journal_key(&context.delivery_id)?;
    let loaded = storage.load_journal(&journal_key)?;
    let journal = StagedDeliveryJournal::new(context.delivery_id.clone(), loaded);
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::ApplyTerminalOutcome(Box::new(
            ApplyDeliveryTerminalOutcome {
                delivery_id: context.delivery_id.clone(),
                request_id: phase.receipt_identity.request_id().clone(),
                request_digest: phase.request_digest()?,
                expected_revision: current.revision(),
                facts: facts.clone(),
            },
        )))
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if mutation.replayed {
        return Err(StorageError::invalid_input(
            "terminal outcome journal replay has no matching scoped receipt",
        ));
    }
    let publication = journal
        .into_publication()
        .map_err(|error| StorageError::adapter(error.to_string()))?
        .ok_or_else(|| {
            StorageError::invalid_input("stage.terminal did not stage a journal publication")
        })?;
    let revision = mutation.snapshot.revision();
    let accepted =
        terminal_accepted_event(message, phase, context, revision, &context.accepted_at)?;
    let public_scope = crate::public_repository_scope(&context.repository_scope);
    let public_source = PublicEventSource::SessionExecutionWorker {
        worker_id: message.lease.worker_id.clone(),
        worker_session_id: message.worker_session_id.clone(),
        lease_id: message.lease.lease_id.clone(),
        codex_thread_id: session_identity.codex_thread_id.clone(),
        session_identity: session_identity.clone(),
    };
    let changed = delivery_changed_event_for_scope(
        public_scope.clone(),
        &context.delivery_id,
        revision,
        DeliveryChangeKind::Advanced,
        message.sent_at.clone(),
        public_source.clone(),
    )?;
    let invalidated =
        delivery_stage_runtime_invalidated_event(&DeliveryStageRuntimeInvalidation {
            scope_key: &context.scope_key,
            delivery_id: &context.delivery_id,
            stage_run_id: &context.stage_run_id,
            product_session_id: &context.product_session_id,
            session_identity,
            revision,
            event_namespace: TERMINAL_RUNTIME_NAMESPACE,
            scope: public_scope,
            occurred_at: message.sent_at.clone(),
            source: public_source,
        })?;
    let pending_audit_event = terminal_pending_audit_event(
        facts,
        phase,
        context,
        session_identity,
        current,
        &mutation.snapshot,
    )?;
    let terminal_authority = persisted_terminal_authority(
        message,
        context,
        PersistedTerminalDisposition::Settled {
            delivery_revision: revision,
        },
        &context.accepted_at,
    )?;
    let commit = StateCommit::new(
        phase.receipt_identity.clone(),
        phase.command_digest.clone(),
        delivery_stream_id(&context.delivery_id),
        current.revision(),
        mutation.snapshot.encode_json().map_err(|error| {
            StorageError::invalid_input(format!("failed to encode terminal Delivery: {error}"))
        })?,
        vec![accepted, changed, invalidated],
    )
    .with_journal_publication(publication)
    .with_pending_audit_event(pending_audit_event)
    .with_state_mutation(StateMutation::new(
        terminal_authority_stream_id(&message.lease.job_id),
        0,
        serde_json::to_vec(&terminal_authority).map_err(|error| {
            StorageError::adapter(format!("failed to encode terminal authority: {error}"))
        })?,
    )?);
    let receipt = storage.commit(&commit)?;
    validate_receipt(&receipt, phase, message, receipt.idempotent_replay)?;
    validate_terminal_pending_audit_event(storage, &receipt, phase)?;
    Ok(receipt)
}

pub(crate) fn load_active_terminal_handoff(
    storage: &mut SqliteStorage,
    delivery: &Delivery,
    command: &CommandEnvelope,
) -> Result<Option<DeliveryTerminalHandoff>, StorageError> {
    if command.command != CommandName::DeliveryAdvance
        || u64::try_from(command.expected_revision.0).ok() != Some(delivery.revision())
    {
        return Err(StorageError::invalid_input(
            "terminal handoff requires the exact current delivery.advance command",
        ));
    }
    let mut active_runs = delivery.snapshot().stage_runs.iter().filter(|run| {
        matches!(
            run.status,
            StageRunStatus::Running | StageRunStatus::Waiting
        )
    });
    let Some(run) = active_runs.next() else {
        return Ok(None);
    };
    if active_runs.next().is_some() {
        return Err(StorageError::invalid_input(
            "Delivery has multiple active StageRuns",
        ));
    }
    let mut bindings = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| binding.stage_run_id == run.id);
    let binding = bindings.next().ok_or_else(|| {
        StorageError::invalid_input("active Delivery StageRun has no SessionBinding")
    })?;
    if bindings.next().is_some() {
        return Err(StorageError::invalid_input(
            "active Delivery StageRun has multiple SessionBindings",
        ));
    }
    let stored = storage
        .load_state(&terminal_authority_stream_id(&binding.execution_job_id))?
        .ok_or_else(|| StorageError::invalid_input("active terminal handoff is missing"))?;
    if stored.revision != 1 {
        return Err(StorageError::invalid_input(
            "active terminal handoff revision is invalid",
        ));
    }
    let persisted = decode_terminal_authority(&stored.payload)?;
    if persisted.schema_version != 1
        || persisted.delivery_id != *delivery.id()
        || persisted.stage_run_id != run.id
        || persisted.job_id != binding.execution_job_id
        || persisted.status != ExecutionOutcomeStatus::Succeeded
        || persisted.disposition != PersistedTerminalDisposition::PendingHandoff
        || !persisted_matches_binding(&persisted, binding)
    {
        return Err(StorageError::invalid_input(
            "terminal handoff does not match the current Delivery binding",
        ));
    }
    let facts = reconcile_durable_terminal_outcome(
        delivery,
        durable_terminal_outcome_input(persisted.clone()),
    )
    .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let (_, advance_command_digest) = crate::command_receipt(command)?;
    let mut consumed = persisted;
    let delivery_revision = delivery.revision().checked_add(1).ok_or_else(|| {
        StorageError::invalid_input("terminal handoff Delivery revision exceeds the durable range")
    })?;
    consumed.disposition = PersistedTerminalDisposition::Consumed {
        advance_request_id: command.request_id.clone(),
        advance_command_digest,
        delivery_revision,
    };
    let payload = serde_json::to_vec(&consumed).map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode consumed terminal authority: {error}"
        ))
    })?;
    Ok(Some(DeliveryTerminalHandoff {
        facts,
        consumption: StateMutation::new(stored.stream_id, stored.revision, payload)?,
    }))
}

pub(crate) fn load_settled_terminal_authority(
    storage: &dyn ProductStateStorage,
    delivery: &Delivery,
    job_id: &ExecutionJobId,
) -> Result<DeliveryTerminalOutcomeFacts, StorageError> {
    let stored = storage
        .load_state(&terminal_authority_stream_id(job_id))?
        .ok_or_else(|| StorageError::invalid_input("settled terminal authority is missing"))?;
    let persisted = decode_terminal_authority(&stored.payload)?;
    let disposition_revision = match (&persisted.disposition, stored.revision) {
        (PersistedTerminalDisposition::Settled { delivery_revision }, 1)
        | (
            PersistedTerminalDisposition::Consumed {
                delivery_revision, ..
            },
            2,
        ) => *delivery_revision,
        _ => {
            return Err(StorageError::invalid_input(
                "terminal authority has not been settled exactly once",
            ));
        }
    };
    if persisted.schema_version != 1
        || persisted.delivery_id != *delivery.id()
        || persisted.job_id != *job_id
        || persisted.status != ExecutionOutcomeStatus::Succeeded
        || disposition_revision > delivery.revision()
    {
        return Err(StorageError::invalid_input(
            "settled terminal authority is stale or foreign",
        ));
    }
    reconcile_durable_settled_terminal_outcome(delivery, durable_terminal_outcome_input(persisted))
        .map_err(|error| StorageError::invalid_input(error.to_string()))
}

/// Durable CP terminal facts used by the closed performance-evaluation join.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EvaluationTerminalAuthorityProjection {
    pub(crate) artifacts: Vec<ArtifactReference>,
    pub(crate) worker_finished_at_millis: u64,
    pub(crate) accepted_at_millis: u64,
    pub(crate) terminal_revision: u64,
}

/// Reads one successfully settled terminal authority without trusting a
/// caller-supplied finish or acceptance time.
pub(crate) fn load_evaluation_terminal_authority(
    storage: &dyn ProductStateStorage,
    job_id: &ExecutionJobId,
) -> Result<Option<EvaluationTerminalAuthorityProjection>, StorageError> {
    let Some(stored) = storage.load_state(&terminal_authority_stream_id(job_id))? else {
        return Ok(None);
    };
    let persisted = decode_terminal_authority(&stored.payload)?;
    let terminal_revision = match (&persisted.disposition, stored.revision) {
        (PersistedTerminalDisposition::Settled { delivery_revision }, 1)
        | (
            PersistedTerminalDisposition::Consumed {
                delivery_revision, ..
            },
            2,
        ) => *delivery_revision,
        (PersistedTerminalDisposition::PendingHandoff, 1) => return Ok(None),
        _ => {
            return Err(StorageError::invalid_input(
                "performance terminal authority revision is invalid",
            ));
        }
    };
    if persisted.schema_version != 1
        || persisted.job_id != *job_id
        || persisted.status != ExecutionOutcomeStatus::Succeeded
        || terminal_revision == 0
        || persisted.accepted_at_millis < persisted.finished_at_millis
    {
        return Err(StorageError::invalid_input(
            "performance terminal authority is incomplete or foreign",
        ));
    }
    Ok(Some(EvaluationTerminalAuthorityProjection {
        artifacts: persisted.artifacts,
        worker_finished_at_millis: persisted.finished_at_millis,
        accepted_at_millis: persisted.accepted_at_millis,
        terminal_revision,
    }))
}

fn decode_terminal_authority(payload: &[u8]) -> Result<PersistedTerminalAuthority, StorageError> {
    let persisted: PersistedTerminalAuthority =
        serde_json::from_slice(payload).map_err(|error| {
            StorageError::invalid_input(format!("terminal authority is invalid: {error}"))
        })?;
    if serde_json::to_vec(&persisted).map_err(|error| {
        StorageError::adapter(format!("failed to encode terminal authority: {error}"))
    })? != payload
    {
        return Err(StorageError::invalid_input(
            "terminal authority is not canonical",
        ));
    }
    Ok(persisted)
}

fn persisted_matches_binding(
    persisted: &PersistedTerminalAuthority,
    binding: &winwincode_delivery::domain::SessionBinding,
) -> bool {
    binding.worker_session_id.as_ref() == Some(&persisted.worker_session_id)
        && binding.worker_id.as_ref() == Some(&persisted.worker_id)
        && binding.worker_instance_id.as_ref() == Some(&persisted.worker_instance_id)
        && binding.lease_id.as_ref() == Some(&persisted.lease_id)
        && binding.fencing_token.as_ref() == Some(&persisted.fencing_token)
        && binding.attempt == persisted.attempt
}

fn durable_terminal_outcome_input(
    persisted: PersistedTerminalAuthority,
) -> DurableTerminalOutcomeInput {
    DurableTerminalOutcomeInput {
        execution_job_id: persisted.job_id,
        attempt: persisted.attempt,
        lease_id: persisted.lease_id,
        fencing_token: persisted.fencing_token,
        worker_id: persisted.worker_id,
        worker_instance_id: persisted.worker_instance_id,
        worker_session_id: persisted.worker_session_id,
        issued_at: persisted.issued_at,
        expires_at: persisted.expires_at,
        stage_run_id: persisted.stage_run_id,
        status: terminal_status(&persisted.status),
        codex_thread_id: persisted.codex_thread_id,
        finished_at_millis: persisted.finished_at_millis,
        last_event_sequence: persisted.last_event_sequence,
        artifacts: persisted
            .artifacts
            .into_iter()
            .map(|artifact| TerminalArtifactReference {
                artifact_id: artifact.artifact_id,
                digest: artifact.digest,
            })
            .collect(),
    }
}

fn terminal_authority_stream_id(job_id: &ExecutionJobId) -> String {
    format!("{TERMINAL_AUTHORITY_STREAM_PREFIX}{}", job_id.0)
}

fn validate_terminal_pending_audit_event(
    storage: &dyn ProductStateStorage,
    receipt: &CommitReceipt,
    phase: &TerminalPhase,
) -> Result<(), StorageError> {
    let expected_event_id = AuditEventId::from_digest(&phase.command_digest)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let Some(stored) = storage.load_pending_audit_event(&receipt.receipt_identity)? else {
        return Err(StorageError::invalid_input(
            "terminal outcome receipt has no pending audit event",
        ));
    };
    let event: AuditEvent = serde_json::from_slice(stored.payload()).map_err(|error| {
        StorageError::invalid_input(format!("terminal audit event is invalid: {error}"))
    })?;
    let canonical = serde_json::to_vec(&event).map_err(|error| {
        StorageError::adapter(format!("failed to encode terminal audit event: {error}"))
    })?;
    if stored.event_id() != expected_event_id.as_str()
        || canonical != stored.payload()
        || event.event_id() != &expected_event_id
        || event.request_id() != phase.receipt_identity.request_id()
        || event.subject().execution_kind() != Some(AuditExecutionSubjectKind::Terminal)
        || event
            .subject()
            .execution()
            .is_none_or(|identity| identity.source_sequence().is_none())
    {
        return Err(StorageError::invalid_input(
            "terminal outcome audit event does not match its trusted receipt",
        ));
    }
    Ok(())
}

fn recover_raced_receipt(
    storage: &dyn ProductStateStorage,
    phase: &TerminalPhase,
    message: &JobOutcomeMessage,
    source: StorageError,
) -> Result<CommitReceipt, StorageError> {
    let Some(receipt) = storage.load_receipt(&phase.receipt_identity, &phase.command_digest)?
    else {
        return Err(source);
    };
    validate_receipt(&receipt, phase, message, true)?;
    validate_terminal_pending_audit_event(storage, &receipt, phase)?;
    Ok(receipt)
}

fn terminal_accepted_event(
    message: &JobOutcomeMessage,
    phase: &TerminalPhase,
    context: &TerminalContext,
    revision: u64,
    accepted_at: &Instant,
) -> Result<NewOutboxEvent, StorageError> {
    let payload = TerminalAcceptedEvent {
        delivery_id: context.delivery_id.clone(),
        execution_job: context.job_event.clone(),
        job_id: message.lease.job_id.clone(),
        message_digest: phase.command_digest.clone(),
        message_id: message.message_id.clone(),
        outcome: AcceptedTerminalOutcome {
            artifacts: message.outcome.artifacts.clone(),
            accepted_at_millis: instant_millis(accepted_at)?,
            codex_thread_id: message.outcome.codex_thread_id.clone(),
            finished_at_millis: instant_millis(&message.outcome.finished_at)?,
            last_event_sequence: message.outcome.last_event_sequence.clone(),
            status: message.outcome.status.clone(),
        },
        product_session_id: context.product_session_id.clone(),
        revision,
        schema_version: 1,
        stage_run_id: context.stage_run_id.clone(),
    };
    let payload = serde_json::to_vec(&payload).map_err(|error| {
        StorageError::adapter(format!("failed to encode terminal outcome event: {error}"))
    })?;
    let event_id = terminal_event_id(&context.scope_key, &payload);
    Ok(NewOutboxEvent::internal(
        event_id.0,
        TERMINAL_TOPIC,
        payload,
    ))
}

fn validate_receipt(
    receipt: &CommitReceipt,
    phase: &TerminalPhase,
    message: &JobOutcomeMessage,
    expected_replay: bool,
) -> Result<(), StorageError> {
    if receipt.receipt_identity != phase.receipt_identity
        || receipt.command_digest != phase.command_digest
        || receipt.idempotent_replay != expected_replay
    {
        return Err(StorageError::invalid_input(
            "terminal outcome durable receipt is incomplete or foreign",
        ));
    }
    let matching = receipt
        .events
        .iter()
        .filter(|event| event.topic == TERMINAL_TOPIC)
        .collect::<Vec<_>>();
    let [event] = matching.as_slice() else {
        return Err(StorageError::invalid_input(
            "terminal outcome receipt must contain one accepted outcome event",
        ));
    };
    let payload: TerminalAcceptedEvent = serde_json::from_slice(&event.payload).map_err(|_| {
        StorageError::invalid_input("terminal outcome accepted event is not canonical")
    })?;
    let canonical = serde_json::to_vec(&payload).map_err(|error| {
        StorageError::adapter(format!("failed to encode terminal outcome event: {error}"))
    })?;
    let expected_finished_at = instant_millis(&message.outcome.finished_at)?;
    let lease_expires_at = instant_millis(&message.lease.expires_at)?;
    let pending_handoff = receipt.stream_id == terminal_authority_stream_id(&payload.job_id);
    let receipt_shape_is_valid = if pending_handoff {
        receipt.revision == 1 && receipt.events.len() == 2
    } else {
        receipt.stream_id == delivery_stream_id(&payload.delivery_id)
            && receipt.revision == payload.revision
            && receipt.events.len() == 3
    };
    if canonical != event.payload
        || payload.schema_version != 1
        || payload.message_digest != phase.command_digest
        || payload.message_id != message.message_id
        || payload.job_id != message.lease.job_id
        || payload.outcome.artifacts != message.outcome.artifacts
        || payload.outcome.codex_thread_id != message.outcome.codex_thread_id
        || payload.outcome.finished_at_millis != expected_finished_at
        || payload.outcome.accepted_at_millis < expected_finished_at
        || payload.outcome.accepted_at_millis >= lease_expires_at
        || payload.outcome.last_event_sequence != message.outcome.last_event_sequence
        || payload.outcome.status != message.outcome.status
        || payload.execution_job.event_id != format!("execution-job:{}", payload.job_id.0)
        || payload.execution_job.stream_id != delivery_stream_id(&payload.delivery_id)
        || validate_durable_job_ref(&payload.execution_job).is_err()
        || payload.stage_run_id.0.is_empty()
        || payload.product_session_id.0.is_empty()
        || !receipt_shape_is_valid
        || event.projection_cursor.is_some()
        || event.event_id
            != terminal_event_id(receipt.receipt_identity.scope_key(), &event.payload).0
    {
        return Err(StorageError::invalid_input(
            "terminal outcome accepted event does not match its durable receipt",
        ));
    }
    if pending_handoff {
        let delivery_topic = crate::delivery_changed_topic()?;
        if receipt
            .events
            .iter()
            .any(|event| event.topic == delivery_topic)
        {
            return Err(StorageError::invalid_input(
                "pending terminal handoff must not publish a Delivery change",
            ));
        }
    } else {
        validate_delivery_changed_receipt(
            receipt,
            &payload.delivery_id,
            payload.revision,
            DeliveryChangeKind::Advanced,
        )?;
    }
    validate_delivery_stage_runtime_invalidation(
        receipt,
        &payload.delivery_id,
        &payload.stage_run_id,
        &payload.product_session_id,
        &message.session_identity,
        payload.revision,
        TERMINAL_RUNTIME_NAMESPACE,
    )
}

fn validate_durable_job_ref(reference: &DurableExecutionJobRef) -> Result<(), StorageError> {
    require_id(
        &reference.receipt_request_id.0,
        "req_",
        "executionJob.receiptRequestId",
    )?;
    if reference.event_sequence == 0
        || reference.receipt_revision == 0
        || !canonical_digest(&reference.receipt_actor_key_sha256)
        || !canonical_digest(&reference.receipt_scope_key_sha256)
        || !canonical_digest(&reference.receipt_command_digest.0)
    {
        return Err(StorageError::invalid_input(
            "terminal outcome durable ExecutionJob reference is not canonical",
        ));
    }
    Ok(())
}

fn canonical_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|suffix| {
        suffix.len() == 64
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

const fn terminal_status(status: &ExecutionOutcomeStatus) -> TerminalOutcomeStatus {
    match status {
        ExecutionOutcomeStatus::Succeeded => TerminalOutcomeStatus::Succeeded,
        ExecutionOutcomeStatus::Failed => TerminalOutcomeStatus::Failed,
        ExecutionOutcomeStatus::InfrastructureError => TerminalOutcomeStatus::InfrastructureError,
        ExecutionOutcomeStatus::Cancelled => TerminalOutcomeStatus::Cancelled,
    }
}

fn terminal_event_id(scope_key: &ReceiptScopeKey, payload: &[u8]) -> ControlPlaneEventId {
    projection_event_id(TERMINAL_EVENT_NAMESPACE, scope_key, payload)
}

fn encoded_key_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
