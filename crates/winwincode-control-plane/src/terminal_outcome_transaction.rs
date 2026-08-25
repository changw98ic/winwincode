// SPDX-License-Identifier: Apache-2.0

//! Receipt-first transaction for one generated Worker `job.outcome`.

use std::{collections::HashSet, fmt};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    ArtifactReference, ExecutionJob, ExecutionOutcomeStatus, ExecutionScope, JobOutcomeMessage,
    JobOutcomeMessageKind, RepositoryScope, SchemaVersion,
};
use winwincode_delivery::{
    application::stage::{DeliveryTerminalOutcomeFacts, TerminalOutcomeStatus},
    domain::Delivery,
    store::{ApplyDeliveryTerminalOutcome, DeliveryCommand, DeliveryCommandPort, DeliveryStore},
};
use winwincode_domain::{
    CodexThreadId, ControlPlaneEventId, DeliveryId, DeliveryTaskId, ExecutionAckSequence,
    ExecutionJobId, ExecutionMessageId, ProductSessionId, RequestId, Sha256Digest, StageRunId,
};
use winwincode_storage::{
    CommitReceipt, DurableOutboxEvent, NewOutboxEvent, ProductStateStorage, ReceiptIdentity,
    ReceiptScopeKey, StateCommit, StorageError,
};

use crate::delivery_transaction::{
    StagedDeliveryJournal, delivery_journal_key, delivery_stream_id, load_durable_execution_job,
};
use crate::session_binding_transaction::{
    delivery_stage_runtime_invalidated_event, execution_message_actor_key,
    execution_message_request_id, instant_millis, projection_event_id, require_id,
    validate_delivery_stage_runtime_invalidation,
};
use crate::{
    DeliveryChangeKind, OutboxError, delivery_changed_event_for_scope, repository_scope_key,
    validate_delivery_changed_receipt,
};

const TERMINAL_PHASE: &str = "terminal-outcome";
const TERMINAL_TOPIC: &str = "delivery.stage.terminal";
const TERMINAL_EVENT_NAMESPACE: &[u8] = b"winwincode.delivery-stage-terminal.v1";
const TERMINAL_RUNTIME_NAMESPACE: &[u8] =
    b"winwincode.delivery-stage-terminal-runtime-invalidation.v1";

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
    PublicationPending {
        commit: Box<DeliveryTerminalOutcomeCommitReceipt>,
        source: OutboxError,
    },
}

impl DeliveryTerminalOutcomeCommitError {
    #[must_use]
    pub fn committed_receipt(&self) -> Option<&DeliveryTerminalOutcomeCommitReceipt> {
        match self {
            Self::PublicationPending { commit, .. } => Some(commit),
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

pub(crate) fn execute(
    storage: &mut dyn ProductStateStorage,
    scope: &RepositoryScope,
    message: &JobOutcomeMessage,
    facts: &DeliveryTerminalOutcomeFacts,
) -> Result<DeliveryTerminalOutcomeCommitReceipt, DeliveryTerminalOutcomeCommitError> {
    validate_message_shape(message)?;
    let phase = TerminalPhase::new(scope, message)?;
    if let Some(receipt) = storage.load_receipt(&phase.receipt_identity, &phase.command_digest)? {
        validate_receipt(&receipt, &phase, message, true)?;
        return Ok(DeliveryTerminalOutcomeCommitReceipt { receipt });
    }

    let (durable, job) = load_durable_execution_job(storage, &message.lease.job_id)?;
    let context = TerminalContext::from_durable(scope, &durable, &job)?;
    validate_message_authority(message, &job, &context, facts)?;
    let current = load_current_delivery(storage, &context.delivery_id)?;
    validate_current_job_binding(&current, &job, &context)?;
    let receipt = commit_terminal(storage, message, facts, &phase, &context, &current)
        .or_else(|source| recover_raced_receipt(storage, &phase, message, source))?;
    Ok(DeliveryTerminalOutcomeCommitReceipt { receipt })
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
    scope_key: ReceiptScopeKey,
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
            scope_key,
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
    codex_thread_id: Option<CodexThreadId>,
    finished_at_millis: u64,
    last_event_sequence: ExecutionAckSequence,
    status: ExecutionOutcomeStatus,
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
) -> Result<(), StorageError> {
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
                    winwincode_delivery::domain::StageRunStatus::Running
                        | winwincode_delivery::domain::StageRunStatus::Waiting
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
    Ok(())
}

fn commit_terminal(
    storage: &mut dyn ProductStateStorage,
    message: &JobOutcomeMessage,
    facts: &DeliveryTerminalOutcomeFacts,
    phase: &TerminalPhase,
    context: &TerminalContext,
    current: &Delivery,
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
    let accepted = terminal_accepted_event(message, phase, context, revision)?;
    let changed = delivery_changed_event_for_scope(
        &context.scope_key,
        &context.delivery_id,
        revision,
        DeliveryChangeKind::Advanced,
    )?;
    let invalidated = delivery_stage_runtime_invalidated_event(
        &context.scope_key,
        &context.delivery_id,
        &context.stage_run_id,
        &context.product_session_id,
        revision,
        TERMINAL_RUNTIME_NAMESPACE,
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
    .with_journal_publication(publication);
    let receipt = storage.commit(&commit)?;
    validate_receipt(&receipt, phase, message, receipt.idempotent_replay)?;
    Ok(receipt)
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
    Ok(receipt)
}

fn terminal_accepted_event(
    message: &JobOutcomeMessage,
    phase: &TerminalPhase,
    context: &TerminalContext,
    revision: u64,
) -> Result<NewOutboxEvent, StorageError> {
    let payload = TerminalAcceptedEvent {
        delivery_id: context.delivery_id.clone(),
        execution_job: context.job_event.clone(),
        job_id: message.lease.job_id.clone(),
        message_digest: phase.command_digest.clone(),
        message_id: message.message_id.clone(),
        outcome: AcceptedTerminalOutcome {
            artifacts: message.outcome.artifacts.clone(),
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
        || receipt.events.len() != 3
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
    if canonical != event.payload
        || payload.schema_version != 1
        || payload.message_digest != phase.command_digest
        || payload.message_id != message.message_id
        || payload.job_id != message.lease.job_id
        || payload.outcome.artifacts != message.outcome.artifacts
        || payload.outcome.codex_thread_id != message.outcome.codex_thread_id
        || payload.outcome.finished_at_millis != expected_finished_at
        || payload.outcome.last_event_sequence != message.outcome.last_event_sequence
        || payload.outcome.status != message.outcome.status
        || payload.execution_job.event_id != format!("execution-job:{}", payload.job_id.0)
        || payload.execution_job.stream_id != delivery_stream_id(&payload.delivery_id)
        || validate_durable_job_ref(&payload.execution_job).is_err()
        || payload.stage_run_id.0.is_empty()
        || payload.product_session_id.0.is_empty()
        || receipt.stream_id != delivery_stream_id(&payload.delivery_id)
        || receipt.revision != payload.revision
        || event.projection_cursor.is_some()
        || event.event_id
            != terminal_event_id(receipt.receipt_identity.scope_key(), &event.payload).0
    {
        return Err(StorageError::invalid_input(
            "terminal outcome accepted event does not match its durable receipt",
        ));
    }
    validate_delivery_changed_receipt(
        receipt,
        &payload.delivery_id,
        payload.revision,
        DeliveryChangeKind::Advanced,
    )?;
    validate_delivery_stage_runtime_invalidation(
        receipt,
        &payload.delivery_id,
        &payload.stage_run_id,
        &payload.product_session_id,
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
