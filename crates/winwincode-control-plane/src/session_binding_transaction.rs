// SPDX-License-Identifier: Apache-2.0

//! Receipt-first transaction for one generated `session.binding` message.

use std::fmt;

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    ControlPlaneWebSocketDeliveryGetReloadQuery,
    ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent,
    ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventScopeKind,
    ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventTypeValue,
    ControlPlaneWebSocketRuntimeProjectionGetReloadQuery, ExecutionJob, ExecutionScope,
    SchemaVersion, SessionBindingMessage, SessionBindingMessageKind,
};
use winwincode_delivery::{
    application::{
        session_binding::{SessionBindingIdentity, accept_worker_session, report_codex_thread},
        stage::SessionBindingAuthority,
    },
    domain::{Delivery, StageRunStatus},
    store::{
        AppendDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryMutationOperation,
        DeliveryStore,
    },
};
use winwincode_domain::{ControlPlaneEventId, DeliveryId, RequestId, Revision, Sha256Digest};
use winwincode_storage::{
    CommitReceipt, DurableOutboxEvent, NewOutboxEvent, ProductStateStorage, ProjectionEventStream,
    ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, StateCommit, StorageError,
};

use crate::delivery_transaction::{
    EXECUTION_JOB_TOPIC, StagedDeliveryJournal, delivery_journal_key, delivery_stream_id,
    strict_execution_job,
};
use crate::{
    DeliveryChangeKind, OutboxError, delivery_changed_event_for_scope,
    validate_delivery_changed_receipt,
};

const WORKER_SESSION_PHASE: &str = "worker-session";
const CODEX_THREAD_PHASE: &str = "codex-thread";
const RUNTIME_INVALIDATED_TOPIC: &str = "runtime-projection.invalidated.v1";
const REQUEST_ID_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Durable receipts for the two canonical `session.bound` revisions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliverySessionBindingCommitReceipt {
    worker_session_receipt: CommitReceipt,
    codex_thread_receipt: CommitReceipt,
}

impl DeliverySessionBindingCommitReceipt {
    #[must_use]
    pub const fn worker_session_receipt(&self) -> &CommitReceipt {
        &self.worker_session_receipt
    }

    #[must_use]
    pub const fn codex_thread_receipt(&self) -> &CommitReceipt {
        &self.codex_thread_receipt
    }
}

/// Failure of the two-phase Delivery `SessionBinding` transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliverySessionBindingCommitError {
    /// No `SessionBinding` phase committed.
    Storage(StorageError),
    /// `WorkerSession` attachment is durable; a retry resumes at `CodexThread`.
    CodexThreadPhase {
        worker_session_receipt: Box<CommitReceipt>,
        source: StorageError,
    },
    /// Both phases are durable, but one or more events remain in the outbox.
    PublicationPending {
        commit: Box<DeliverySessionBindingCommitReceipt>,
        source: OutboxError,
    },
}

impl DeliverySessionBindingCommitError {
    #[must_use]
    pub fn committed_worker_session_receipt(&self) -> Option<&CommitReceipt> {
        match self {
            Self::CodexThreadPhase {
                worker_session_receipt,
                ..
            } => Some(worker_session_receipt),
            Self::PublicationPending { commit, .. } => Some(commit.worker_session_receipt()),
            Self::Storage(_) => None,
        }
    }
}

impl fmt::Display for DeliverySessionBindingCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "SessionBinding transaction failed: {error}"),
            Self::CodexThreadPhase { source, .. } => write!(
                formatter,
                "WorkerSession committed, but CodexThread attachment failed: {source}"
            ),
            Self::PublicationPending { source, .. } => write!(
                formatter,
                "SessionBinding committed, but its events remain pending: {source}"
            ),
        }
    }
}

impl std::error::Error for DeliverySessionBindingCommitError {}

impl From<StorageError> for DeliverySessionBindingCommitError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

pub(crate) fn execute(
    storage: &mut dyn ProductStateStorage,
    message: &SessionBindingMessage,
    authority: &SessionBindingAuthority,
) -> Result<DeliverySessionBindingCommitReceipt, DeliverySessionBindingCommitError> {
    let bound_at_millis = validate_message_shape(message)?;
    let durable = load_execution_job_event(storage, message)?;
    let job = validate_durable_job(&durable, message)?;
    let context = BindingContext::from_durable(&durable, &job, bound_at_millis)?;
    let worker_phase = Phase::new(message, &durable, WORKER_SESSION_PHASE)?;
    let codex_phase = Phase::new(message, &durable, CODEX_THREAD_PHASE)?;
    let mut worker_replay =
        storage.load_receipt(&worker_phase.receipt_identity, &worker_phase.command_digest)?;
    let codex_replay =
        storage.load_receipt(&codex_phase.receipt_identity, &codex_phase.command_digest)?;
    if worker_replay.is_none() && codex_replay.is_some() {
        worker_replay =
            storage.load_receipt(&worker_phase.receipt_identity, &worker_phase.command_digest)?;
    }
    if let (Some(worker_session_receipt), Some(codex_thread_receipt)) =
        (worker_replay.as_ref(), codex_replay.as_ref())
    {
        validate_complete_replay(
            worker_session_receipt,
            codex_thread_receipt,
            &context,
            &worker_phase,
            &codex_phase,
        )?;
        return Ok(DeliverySessionBindingCommitReceipt {
            worker_session_receipt: worker_session_receipt.clone(),
            codex_thread_receipt: codex_thread_receipt.clone(),
        });
    }

    validate_authority(message, authority)?;
    let worker_session_receipt = if let Some(receipt) = worker_replay {
        validate_phase_receipt(
            &receipt,
            &context,
            &worker_phase,
            context.job_revision + 1,
            true,
        )?;
        receipt
    } else {
        if codex_replay.is_some() {
            return Err(StorageError::invalid_input(
                "CodexThread receipt exists without its WorkerSession receipt",
            )
            .into());
        }
        commit_worker_session(storage, message, &context, &worker_phase).or_else(|source| {
            recover_raced_phase_receipt(
                storage,
                &context,
                &worker_phase,
                context.job_revision + 1,
                source,
            )
        })?
    };

    let codex_thread_receipt = match codex_replay {
        Some(receipt) => {
            validate_phase_receipt(
                &receipt,
                &context,
                &codex_phase,
                context.job_revision + 2,
                true,
            )
            .map_err(
                |source| DeliverySessionBindingCommitError::CodexThreadPhase {
                    worker_session_receipt: Box::new(worker_session_receipt.clone()),
                    source,
                },
            )?;
            receipt
        }
        None => commit_codex_thread(storage, message, &context, &codex_phase)
            .or_else(|source| {
                recover_raced_phase_receipt(
                    storage,
                    &context,
                    &codex_phase,
                    context.job_revision + 2,
                    source,
                )
            })
            .map_err(
                |source| DeliverySessionBindingCommitError::CodexThreadPhase {
                    worker_session_receipt: Box::new(worker_session_receipt.clone()),
                    source,
                },
            )?,
    };

    Ok(DeliverySessionBindingCommitReceipt {
        worker_session_receipt,
        codex_thread_receipt,
    })
}

struct BindingContext {
    scope_key: ReceiptScopeKey,
    delivery_id: DeliveryId,
    identity: SessionBindingIdentity,
    job_revision: u64,
    attempt: u64,
    bound_at_millis: u64,
}

impl BindingContext {
    fn from_durable(
        durable: &DurableOutboxEvent,
        job: &ExecutionJob,
        bound_at_millis: u64,
    ) -> Result<Self, StorageError> {
        let ExecutionScope::DeliveryStageExecutionScope(scope) = &job.scope else {
            return Err(StorageError::invalid_input(
                "SessionBinding ExecutionJob is not a Delivery stage job",
            ));
        };
        let attempt = u64::try_from(job.attempt).map_err(|_| {
            StorageError::invalid_input("SessionBinding ExecutionJob attempt is out of range")
        })?;
        let stream_id = delivery_stream_id(&scope.delivery_id);
        if durable.stream_id() != stream_id || durable.revision() == 0 {
            return Err(StorageError::invalid_input(
                "SessionBinding ExecutionJob receipt does not identify its Delivery state",
            ));
        }
        Ok(Self {
            scope_key: durable.receipt_identity().scope_key().clone(),
            delivery_id: scope.delivery_id.clone(),
            identity: SessionBindingIdentity {
                delivery_id: scope.delivery_id.clone(),
                delivery_task_id: scope.delivery_task_id.clone(),
                stage_run_id: scope.stage_run_id.clone(),
                product_session_id: scope.product_session_id.clone(),
                execution_job_id: job.job_id.clone(),
            },
            job_revision: durable.revision(),
            attempt,
            bound_at_millis,
        })
    }
}

struct Phase {
    receipt_identity: ReceiptIdentity,
    command_digest: Sha256Digest,
}

impl Phase {
    fn new(
        message: &SessionBindingMessage,
        durable: &DurableOutboxEvent,
        name: &'static str,
    ) -> Result<Self, StorageError> {
        let request_id = phase_request_id(message, name)?;
        let receipt_identity = ReceiptIdentity::new(
            phase_actor_key(message)?,
            durable.receipt_identity().scope_key().clone(),
            request_id,
        )?;
        Ok(Self {
            receipt_identity,
            command_digest: phase_digest(message, durable, name)?,
        })
    }

    fn request_digest(&self) -> Result<String, StorageError> {
        self.command_digest
            .0
            .strip_prefix("sha256:")
            .map(str::to_owned)
            .ok_or_else(|| StorageError::invalid_input("phase digest is not canonical"))
    }
}

fn commit_worker_session(
    storage: &mut dyn ProductStateStorage,
    message: &SessionBindingMessage,
    context: &BindingContext,
    phase: &Phase,
) -> Result<CommitReceipt, StorageError> {
    let current = load_current_delivery(storage, context, context.job_revision)?;
    validate_current_binding(&current, context, message, BindingProgress::Pending)?;
    let next = accept_worker_session(
        &current,
        current.revision(),
        &context.identity,
        message.worker_session_id.clone(),
        context.bound_at_millis,
    )
    .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    commit_phase(storage, &current, next, context, phase)
}

fn commit_codex_thread(
    storage: &mut dyn ProductStateStorage,
    message: &SessionBindingMessage,
    context: &BindingContext,
    phase: &Phase,
) -> Result<CommitReceipt, StorageError> {
    let expected_revision = context
        .job_revision
        .checked_add(1)
        .ok_or_else(|| StorageError::invalid_input("Delivery revision overflow"))?;
    let current = load_current_delivery(storage, context, expected_revision)?;
    validate_current_binding(&current, context, message, BindingProgress::WorkerAccepted)?;
    let next = report_codex_thread(
        &current,
        current.revision(),
        &context.identity,
        &message.worker_session_id,
        message.codex_thread_id.clone(),
        context.bound_at_millis,
    )
    .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    commit_phase(storage, &current, next, context, phase)
}

fn commit_phase(
    storage: &mut dyn ProductStateStorage,
    current: &Delivery,
    next: Delivery,
    context: &BindingContext,
    phase: &Phase,
) -> Result<CommitReceipt, StorageError> {
    let journal_key = delivery_journal_key(&context.delivery_id)?;
    let loaded = storage.load_journal(&journal_key)?;
    let journal = StagedDeliveryJournal::new(context.delivery_id.clone(), loaded);
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::Append(AppendDelivery {
            delivery_id: context.delivery_id.clone(),
            request_id: phase.receipt_identity.request_id().clone(),
            request_digest: phase.request_digest()?,
            operation: DeliveryMutationOperation::SessionBound,
            expected_revision: current.revision(),
            snapshot: next,
        }))
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if mutation.replayed {
        return Err(StorageError::invalid_input(
            "Delivery journal replay has no matching durable phase receipt",
        ));
    }
    let publication = journal
        .into_publication()
        .map_err(|error| StorageError::adapter(error.to_string()))?
        .ok_or_else(|| {
            StorageError::invalid_input("session.bound did not stage a journal publication")
        })?;
    let revision = mutation.snapshot.revision();
    let events = phase_events(context, revision)?;
    let commit = StateCommit::new(
        phase.receipt_identity.clone(),
        phase.command_digest.clone(),
        delivery_stream_id(&context.delivery_id),
        current.revision(),
        mutation.snapshot.encode_json().map_err(|error| {
            StorageError::invalid_input(format!("failed to encode Delivery snapshot: {error}"))
        })?,
        events,
    )
    .with_journal_publication(publication);
    let receipt = storage.commit(&commit)?;
    validate_phase_receipt(
        &receipt,
        context,
        phase,
        revision,
        receipt.idempotent_replay,
    )?;
    Ok(receipt)
}

fn recover_raced_phase_receipt(
    storage: &dyn ProductStateStorage,
    context: &BindingContext,
    phase: &Phase,
    expected_revision: u64,
    source: StorageError,
) -> Result<CommitReceipt, StorageError> {
    // Callers enter here only after this phase was absent at the first receipt
    // read and their commit attempt lost a race. A second exact receipt proof
    // resolves that race; ordinary retries use the receipt-first branches in
    // `execute` instead.
    let Some(receipt) = storage.load_receipt(&phase.receipt_identity, &phase.command_digest)?
    else {
        return Err(source);
    };
    validate_phase_receipt(&receipt, context, phase, expected_revision, true)?;
    Ok(receipt)
}

fn load_current_delivery(
    storage: &dyn ProductStateStorage,
    context: &BindingContext,
    expected_revision: u64,
) -> Result<Delivery, StorageError> {
    let stream_id = delivery_stream_id(&context.delivery_id);
    let state = storage
        .load_state(&stream_id)?
        .ok_or_else(|| StorageError::invalid_input("SessionBinding Delivery state is missing"))?;
    if state.stream_id != stream_id || state.revision != expected_revision {
        return Err(StorageError::revision_conflict(
            expected_revision,
            state.revision,
        ));
    }
    let delivery = Delivery::decode_json(&state.payload).map_err(|error| {
        StorageError::invalid_input(format!("SessionBinding Delivery state is invalid: {error}"))
    })?;
    if delivery.id() != &context.delivery_id || delivery.revision() != expected_revision {
        return Err(StorageError::invalid_input(
            "SessionBinding Delivery snapshot does not match durable state",
        ));
    }
    Ok(delivery)
}

#[derive(Clone, Copy)]
enum BindingProgress {
    Pending,
    WorkerAccepted,
}

fn validate_current_binding(
    delivery: &Delivery,
    context: &BindingContext,
    message: &SessionBindingMessage,
    progress: BindingProgress,
) -> Result<(), StorageError> {
    let matches = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| {
            binding.delivery_id == context.identity.delivery_id
                && binding.delivery_task_id == context.identity.delivery_task_id
                && binding.stage_run_id == context.identity.stage_run_id
                && binding.product_session_id == context.identity.product_session_id
                && binding.execution_job_id == context.identity.execution_job_id
        })
        .collect::<Vec<_>>();
    let [binding] = matches.as_slice() else {
        return Err(StorageError::invalid_input(
            "SessionBinding message does not match exactly one current binding",
        ));
    };
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == context.identity.stage_run_id)
        .ok_or_else(|| StorageError::invalid_input("SessionBinding StageRun is missing"))?;
    if run.delivery_id != context.delivery_id
        || run.delivery_task_id != context.identity.delivery_task_id
        || run.attempt != context.attempt
        || !matches!(
            run.status,
            StageRunStatus::Running | StageRunStatus::Waiting
        )
        || context.bound_at_millis < run.started_at_millis
        || context.bound_at_millis < binding.bound_at_millis
    {
        return Err(StorageError::invalid_input(
            "SessionBinding message does not match the current active StageRun",
        ));
    }
    let exact_progress = match progress {
        BindingProgress::Pending => {
            binding.worker_session_id.is_none() && binding.codex_thread_id.is_none()
        }
        BindingProgress::WorkerAccepted => {
            binding.worker_session_id.as_ref() == Some(&message.worker_session_id)
                && binding.codex_thread_id.is_none()
        }
    };
    if !exact_progress {
        return Err(StorageError::invalid_input(
            "SessionBinding current attachment state is stale or foreign",
        ));
    }
    Ok(())
}

fn validate_message_shape(message: &SessionBindingMessage) -> Result<u64, StorageError> {
    if message.kind != SessionBindingMessageKind::SessionBinding
        || message.schema_version != SchemaVersion::WinwincodeV1
    {
        return Err(StorageError::invalid_input(
            "SessionBinding message discriminator is not canonical",
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
    require_id(&message.product_session_id.0, "psn_", "productSessionId")?;
    require_id(&message.worker_session_id.0, "wsn_", "workerSessionId")?;
    require_id(&message.codex_thread_id.0, "cdx_", "codexThreadId")?;
    if message.lease.attempt <= 0
        || message.lease.fencing_token.0.is_empty()
        || message.lease.fencing_token.0.len() > 20
        || message.lease.fencing_token.0.starts_with('0')
        || !message
            .lease
            .fencing_token
            .0
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return Err(StorageError::invalid_input(
            "SessionBinding lease attempt or fencingToken is invalid",
        ));
    }
    let _attempt = u64::try_from(message.lease.attempt)
        .map_err(|_| StorageError::invalid_input("SessionBinding attempt is out of range"))?;
    let issued_at = instant_millis(&message.lease.issued_at)?;
    let expires_at = instant_millis(&message.lease.expires_at)?;
    let bound_at = instant_millis(&message.bound_at)?;
    let sent_at = instant_millis(&message.sent_at)?;
    if issued_at > bound_at || bound_at >= expires_at || sent_at < bound_at || sent_at > expires_at
    {
        return Err(StorageError::invalid_input(
            "SessionBinding message time is outside its active lease",
        ));
    }
    Ok(bound_at)
}

fn validate_authority(
    message: &SessionBindingMessage,
    authority: &SessionBindingAuthority,
) -> Result<(), StorageError> {
    let active_lease = authority.active_lease();
    let attempt = u64::try_from(message.lease.attempt)
        .map_err(|_| StorageError::invalid_input("SessionBinding attempt is out of range"))?;
    if active_lease.execution_job_id() != &message.lease.job_id
        || active_lease.attempt() != attempt
        || active_lease.lease_id() != &message.lease.lease_id
        || active_lease.fencing_token() != &message.lease.fencing_token
        || active_lease.worker_id() != &message.lease.worker_id
        || active_lease.worker_instance_id() != &message.lease.worker_instance_id
        || active_lease.worker_session_id() != &message.worker_session_id
    {
        return Err(StorageError::invalid_input(
            "SessionBinding message does not match the scheduler-owned active lease",
        ));
    }
    if authority.issued_at() != &message.lease.issued_at
        || authority.expires_at() != &message.lease.expires_at
    {
        return Err(StorageError::invalid_input(
            "SessionBinding message changed the scheduler-owned lease window",
        ));
    }
    Ok(())
}

fn load_execution_job_event(
    storage: &dyn ProductStateStorage,
    message: &SessionBindingMessage,
) -> Result<DurableOutboxEvent, StorageError> {
    let event_id = format!("execution-job:{}", message.lease.job_id.0);
    let durable = storage.load_outbox_event(&event_id)?.ok_or_else(|| {
        StorageError::invalid_input("SessionBinding ExecutionJob event does not exist")
    })?;
    let event = durable.event();
    if event.event_id != event_id
        || event.topic != EXECUTION_JOB_TOPIC
        || event.projection_cursor.is_some()
    {
        return Err(StorageError::invalid_input(
            "SessionBinding durable event is not the exact internal ExecutionJob intent",
        ));
    }
    Ok(durable)
}

fn validate_durable_job(
    durable: &DurableOutboxEvent,
    message: &SessionBindingMessage,
) -> Result<ExecutionJob, StorageError> {
    let event = durable.event();
    if event.event_id != format!("execution-job:{}", message.lease.job_id.0)
        || event.topic != EXECUTION_JOB_TOPIC
        || event.projection_cursor.is_some()
    {
        return Err(StorageError::invalid_input(
            "SessionBinding durable event is not the exact internal ExecutionJob intent",
        ));
    }
    let job = strict_execution_job(&event.payload)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let ExecutionScope::DeliveryStageExecutionScope(scope) = &job.scope else {
        return Err(StorageError::invalid_input(
            "SessionBinding durable job has foreign scope",
        ));
    };
    if job.job_id != message.lease.job_id
        || job.attempt != message.lease.attempt
        || scope.product_session_id != message.product_session_id
    {
        return Err(StorageError::invalid_input(
            "SessionBinding message does not match the durable ExecutionJob",
        ));
    }
    Ok(job)
}

fn validate_complete_replay(
    worker_receipt: &CommitReceipt,
    codex_receipt: &CommitReceipt,
    context: &BindingContext,
    worker_phase: &Phase,
    codex_phase: &Phase,
) -> Result<(), StorageError> {
    let worker_revision = context
        .job_revision
        .checked_add(1)
        .ok_or_else(|| StorageError::invalid_input("SessionBinding replay revision overflow"))?;
    let codex_revision = worker_revision
        .checked_add(1)
        .ok_or_else(|| StorageError::invalid_input("SessionBinding replay revision overflow"))?;
    if worker_receipt.revision != worker_revision || codex_receipt.revision != codex_revision {
        return Err(StorageError::invalid_input(
            "SessionBinding replay receipts are not consecutive",
        ));
    }
    validate_phase_receipt(worker_receipt, context, worker_phase, worker_revision, true)?;
    validate_phase_receipt(codex_receipt, context, codex_phase, codex_revision, true)
}

fn phase_events(
    context: &BindingContext,
    revision: u64,
) -> Result<Vec<NewOutboxEvent>, StorageError> {
    Ok(vec![
        delivery_changed_event_for_scope(
            &context.scope_key,
            &context.delivery_id,
            revision,
            DeliveryChangeKind::Advanced,
        )?,
        runtime_invalidated_event(context, revision)?,
    ])
}

fn runtime_invalidated_event(
    context: &BindingContext,
    revision: u64,
) -> Result<NewOutboxEvent, StorageError> {
    let revision = i64::try_from(revision)
        .map(Revision)
        .map_err(|_| StorageError::invalid_input("Delivery revision exceeds public range"))?;
    let payload = ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent {
        delivery_id: context.delivery_id.clone(),
        last_projection_sequence: 0,
        product_session_id: context.identity.product_session_id.clone(),
        projection_revision: revision,
        reload_queries: (
            ControlPlaneWebSocketDeliveryGetReloadQuery::DeliveryGet,
            ControlPlaneWebSocketRuntimeProjectionGetReloadQuery::RuntimeProjectionGet,
        ),
        scope_kind:
            ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventScopeKind::DeliveryStage,
        stage_run_id: context.identity.stage_run_id.clone(),
        type_value:
            ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventTypeValue::RuntimeProjectionInvalidatedV1,
    };
    let payload = serde_json::to_vec(&payload).map_err(|error| {
        StorageError::adapter(format!("failed to encode runtime invalidation: {error}"))
    })?;
    let event_id = projection_event_id(
        b"winwincode.session-binding-runtime-invalidation.v1",
        &context.scope_key,
        &payload,
    );
    Ok(NewOutboxEvent::projection(
        event_id,
        RUNTIME_INVALIDATED_TOPIC,
        payload,
        ProjectionEventStream::Delivery(context.delivery_id.clone()),
    ))
}

fn validate_phase_receipt(
    receipt: &CommitReceipt,
    context: &BindingContext,
    phase: &Phase,
    expected_revision: u64,
    expected_replay: bool,
) -> Result<(), StorageError> {
    if receipt.receipt_identity != phase.receipt_identity
        || receipt.command_digest != phase.command_digest
        || receipt.idempotent_replay != expected_replay
        || receipt.stream_id != delivery_stream_id(&context.delivery_id)
        || receipt.revision != expected_revision
        || receipt.events.len() != 2
    {
        return Err(StorageError::invalid_input(
            "SessionBinding durable phase receipt is incomplete or foreign",
        ));
    }
    validate_delivery_changed_receipt(
        receipt,
        &context.delivery_id,
        expected_revision,
        DeliveryChangeKind::Advanced,
    )?;
    let matching = receipt
        .events
        .iter()
        .filter(|event| event.topic == RUNTIME_INVALIDATED_TOPIC)
        .collect::<Vec<_>>();
    let [event] = matching.as_slice() else {
        return Err(StorageError::invalid_input(
            "SessionBinding receipt must contain one runtime invalidation",
        ));
    };
    let payload: ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent =
        serde_json::from_slice(&event.payload).map_err(|_| {
            StorageError::invalid_input("SessionBinding runtime invalidation is not canonical")
        })?;
    let canonical = serde_json::to_vec(&payload).map_err(|error| {
        StorageError::adapter(format!("failed to encode runtime invalidation: {error}"))
    })?;
    let expected_revision = i64::try_from(expected_revision).unwrap_or(-1);
    let cursor = event.projection_cursor.as_ref().ok_or_else(|| {
        StorageError::invalid_input("SessionBinding runtime invalidation has no cursor")
    })?;
    if canonical != event.payload
        || payload.delivery_id != context.delivery_id
        || payload.stage_run_id != context.identity.stage_run_id
        || payload.product_session_id != context.identity.product_session_id
        || payload.projection_revision.0 != expected_revision
        || payload.last_projection_sequence != 0
        || payload.reload_queries
            != (
                ControlPlaneWebSocketDeliveryGetReloadQuery::DeliveryGet,
                ControlPlaneWebSocketRuntimeProjectionGetReloadQuery::RuntimeProjectionGet,
            )
        || payload.scope_kind
            != ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventScopeKind::DeliveryStage
        || payload.type_value
            != ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventTypeValue::RuntimeProjectionInvalidatedV1
        || cursor.sequence() == 0
        || cursor.event_id().map(|id| id.0.as_str()) != Some(event.event_id.as_str())
        || cursor.key().scope_key() != receipt.receipt_identity.scope_key()
        || cursor.key().stream() != &ProjectionEventStream::Delivery(context.delivery_id.clone())
        || event.event_id
            != projection_event_id(
                b"winwincode.session-binding-runtime-invalidation.v1",
                receipt.receipt_identity.scope_key(),
                &event.payload,
            )
            .0
    {
        return Err(StorageError::invalid_input(
            "SessionBinding runtime invalidation does not match durable phase facts",
        ));
    }
    Ok(())
}

fn phase_request_id(
    message: &SessionBindingMessage,
    phase: &'static str,
) -> Result<RequestId, StorageError> {
    // The generated message identity owns a stable two-slot idempotency key.
    // Mutable message and durable-job facts belong in the phase digest so a
    // changed payload reaches storage as a request conflict instead of a new
    // request.
    let mut bytes = Vec::with_capacity(message.message_id.0.len() + phase.len() + 64);
    bytes.extend_from_slice(b"winwincode.session-binding-phase-request.v2\0");
    append_phase_fact(&mut bytes, phase.as_bytes());
    append_phase_fact(&mut bytes, message.message_id.0.as_bytes());
    let digest = Sha256::digest(bytes);
    let mut value_bytes = [0_u8; 16];
    value_bytes.copy_from_slice(&digest[..16]);
    let mut value = u128::from_be_bytes(value_bytes);
    let mut suffix = [b'0'; 26];
    for index in (0..suffix.len()).rev() {
        suffix[index] = REQUEST_ID_ALPHABET[(value & 31) as usize];
        value >>= 5;
    }
    let suffix = std::str::from_utf8(&suffix)
        .map_err(|_| StorageError::adapter("request id encoding failed"))?;
    Ok(RequestId(format!("req_{suffix}")))
}

fn phase_digest(
    message: &SessionBindingMessage,
    durable: &DurableOutboxEvent,
    phase: &'static str,
) -> Result<Sha256Digest, StorageError> {
    let digest = Sha256::digest(canonical_phase_bytes(message, durable, phase)?);
    Ok(Sha256Digest(format!("sha256:{digest:x}")))
}

fn canonical_phase_bytes(
    message: &SessionBindingMessage,
    durable: &DurableOutboxEvent,
    phase: &'static str,
) -> Result<Vec<u8>, StorageError> {
    let message = serde_json::to_vec(message).map_err(|error| {
        StorageError::adapter(format!("failed to encode SessionBinding message: {error}"))
    })?;
    // A phase receipt authorizes exactly one generated message against one
    // immutable ExecutionJob row and the receipt that owns that row. Binding
    // every membership field keeps a complete replay independent of current
    // Delivery state without accepting a re-parented or rewritten job.
    let event = durable.event();
    let receipt = durable.receipt_identity();
    let mut bytes = Vec::with_capacity(message.len() + event.payload.len() + 512);
    bytes.extend_from_slice(b"winwincode.session-binding-phase.v2\0");
    append_phase_fact(&mut bytes, phase.as_bytes());
    append_phase_fact(&mut bytes, &message);
    append_phase_fact(&mut bytes, receipt.actor_key().as_bytes());
    append_phase_fact(&mut bytes, receipt.scope_key().as_bytes());
    append_phase_fact(&mut bytes, receipt.request_id().0.as_bytes());
    append_phase_fact(&mut bytes, durable.command_digest().0.as_bytes());
    append_phase_fact(&mut bytes, durable.stream_id().as_bytes());
    append_phase_fact(&mut bytes, &durable.revision().to_be_bytes());
    append_phase_fact(&mut bytes, &event.sequence.to_be_bytes());
    append_phase_fact(&mut bytes, event.event_id.as_bytes());
    append_phase_fact(&mut bytes, event.topic.as_bytes());
    append_phase_fact(&mut bytes, &event.payload);
    Ok(bytes)
}

fn append_phase_fact(target: &mut Vec<u8>, value: &[u8]) {
    target.extend_from_slice(&(value.len() as u64).to_be_bytes());
    target.extend_from_slice(value);
}

fn phase_actor_key(message: &SessionBindingMessage) -> Result<ReceiptActorKey, StorageError> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"winwincode.execution-port-message-actor.v1\0");
    append_phase_fact(&mut bytes, message.message_id.0.as_bytes());
    ReceiptActorKey::from_encoded(bytes)
}

fn projection_event_id(
    namespace: &[u8],
    scope_key: &ReceiptScopeKey,
    payload: &[u8],
) -> ControlPlaneEventId {
    let mut digest = Sha256::new();
    digest.update(namespace);
    digest.update([0]);
    digest.update((scope_key.as_bytes().len() as u64).to_be_bytes());
    digest.update(scope_key.as_bytes());
    digest.update((payload.len() as u64).to_be_bytes());
    digest.update(payload);
    ControlPlaneEventId(format!("evt_{:x}", digest.finalize()))
}

fn require_id(value: &str, prefix: &str, field: &str) -> Result<(), StorageError> {
    let suffix = value.strip_prefix(prefix).ok_or_else(|| {
        StorageError::invalid_input(format!("SessionBinding {field} has the wrong prefix"))
    })?;
    if suffix.len() != 26
        || !suffix.bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(
                    byte,
                    b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
                )
        })
    {
        return Err(StorageError::invalid_input(format!(
            "SessionBinding {field} is not canonical"
        )));
    }
    Ok(())
}

fn instant_millis(instant: &winwincode_domain::Instant) -> Result<u64, StorageError> {
    let bytes = instant.0.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return Err(StorageError::invalid_input(
            "SessionBinding Instant is not canonical UTC milliseconds",
        ));
    }
    let year = decimal(bytes, 0, 4)?;
    let month = decimal(bytes, 5, 2)?;
    let day = decimal(bytes, 8, 2)?;
    let hour = decimal(bytes, 11, 2)?;
    let minute = decimal(bytes, 14, 2)?;
    let second = decimal(bytes, 17, 2)?;
    let millis = decimal(bytes, 20, 3)?;
    if year < 1970
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(StorageError::invalid_input(
            "SessionBinding Instant contains an invalid date-time",
        ));
    }
    let days = days_from_civil(year, month, day)?;
    days.checked_mul(86_400_000)
        .and_then(|value| value.checked_add(hour * 3_600_000))
        .and_then(|value| value.checked_add(minute * 60_000))
        .and_then(|value| value.checked_add(second * 1_000))
        .and_then(|value| value.checked_add(millis))
        .ok_or_else(|| StorageError::invalid_input("SessionBinding Instant is out of range"))
}

fn decimal(bytes: &[u8], start: usize, length: usize) -> Result<u64, StorageError> {
    bytes[start..start + length]
        .iter()
        .try_fold(0_u64, |value, byte| {
            if byte.is_ascii_digit() {
                Ok(value * 10 + u64::from(byte - b'0'))
            } else {
                Err(StorageError::invalid_input(
                    "SessionBinding Instant contains a non-decimal component",
                ))
            }
        })
}

const fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

fn days_from_civil(year: u64, month: u64, day: u64) -> Result<u64, StorageError> {
    let year = i64::try_from(year)
        .map_err(|_| StorageError::invalid_input("SessionBinding year is out of range"))?;
    let month = i64::try_from(month)
        .map_err(|_| StorageError::invalid_input("SessionBinding month is out of range"))?;
    let day = i64::try_from(day)
        .map_err(|_| StorageError::invalid_input("SessionBinding day is out of range"))?;
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let since_epoch = era * 146_097 + day_of_era - 719_468;
    u64::try_from(since_epoch)
        .map_err(|_| StorageError::invalid_input("SessionBinding Instant predates Unix epoch"))
}
