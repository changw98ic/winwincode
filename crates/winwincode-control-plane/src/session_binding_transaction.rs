// SPDX-License-Identifier: Apache-2.0

//! Receipt-first transaction for one generated `session.binding` message.

use std::fmt;

use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    ControlPlaneWebSocketDeliveryGetReloadQuery,
    ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent,
    ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventScopeKind,
    ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventTypeValue,
    ControlPlaneWebSocketRuntimeProjectionGetReloadQuery,
};
use winwincode_audit::{
    AuditAction, AuditBindingPhase, AuditBindingSource, AuditEvent, AuditEventId,
    AuditExecutionIdentity, AuditExecutionSubjectKind, AuditSubject,
};
use winwincode_delivery::{
    application::{
        session_binding::{
            DeliveryExecutionAttemptReplacement,
            SessionBindingAuthority as DeliverySessionBindingAuthority, SessionBindingIdentity,
        },
        stage::SessionBindingAuthority as SchedulerSessionBindingAuthority,
    },
    domain::{Delivery, StageRunStatus},
    store::{
        AcceptDeliveryWorkerSession, DeliveryCommand, DeliveryCommandPort, DeliveryStore,
        ReplaceDeliveryExecutionAttempt, ReportDeliveryCodexThread,
    },
};
use winwincode_domain::{
    ControlPlaneEventId, DeliveryId, ExecutionMessageId, Instant, RequestId, Revision,
    SchemaVersion, SessionIdentity, Sha256Digest,
};
use winwincode_execution_port::generated::{
    DeliveryStageExecutionScope, ExecutionJob, ExecutionScope, SessionBindingMessage,
    SessionBindingMessageKind,
};
use winwincode_storage::{
    CommitReceipt, DurableOutboxEvent, ExecutionScopeReplacementAuthority, NewOutboxEvent,
    PendingAuditEvent, ProductStateStorage, ProjectionEventStream, PublicEventScope,
    PublicEventSource, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, StateCommit,
    StorageError,
};

use crate::delivery_transaction::{
    StagedDeliveryJournal, delivery_journal_key, delivery_stream_id, load_durable_execution_job,
};
use crate::session_identity::{SessionBindingAcceptance, validate_session_binding};
use crate::{
    DeliveryChangeKind, OutboxError, delivery_changed_event_for_scope, execution_audit_event,
    repository_scope_from_receipt_key, validate_delivery_changed_receipt,
};

const WORKER_SESSION_PHASE: &str = "worker-session";
const CODEX_THREAD_PHASE: &str = "codex-thread";
const RUNTIME_INVALIDATED_TOPIC: &str = "runtime-projection.invalidated.v1";
const REQUEST_ID_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Clone, Copy)]
enum SessionBindingCommitPhase {
    WorkerSession,
    CodexThread,
}

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

#[allow(clippy::too_many_lines)]
pub(crate) fn execute_at(
    storage: &mut dyn ProductStateStorage,
    message: &SessionBindingMessage,
    authority: &SchedulerSessionBindingAuthority,
    server_time: &Instant,
) -> Result<DeliverySessionBindingCommitReceipt, DeliverySessionBindingCommitError> {
    // Read the immutable intent first. Phase request identities/digests are
    // rooted in this event, while queue/replacement state is mutable and is
    // deliberately deferred until a new or partially committed binding is
    // proven to need it.
    let (durable, immutable_job) =
        crate::delivery_transaction::load_durable_execution_intent(storage, &message.lease.job_id)?;
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
    let bound_at_millis = validate_message_shape(message)?;
    // A complete receipt replay still has to identify the current attempt.
    // Once the scheduler has sealed a successor, an exact old-attempt phase
    // receipt is historical evidence, not permission to bind the fenced
    // predecessor again. Read only the durable replacement seal here; do not
    // inspect or mutate the current Delivery snapshot.
    if let Some(replacement) =
        storage.load_execution_scope_replacement_authority(&message.lease.job_id)?
    {
        let message_attempt = u64::try_from(message.lease.attempt)
            .map_err(|_| StorageError::invalid_input("SessionBinding attempt is out of range"))?;
        if message_attempt != replacement.replacement_attempt() {
            return Err(StorageError::invalid_input(
                "SessionBinding message belongs to a fenced predecessor attempt",
            )
            .into());
        }
    }
    if let (Some(worker_session_receipt), Some(codex_thread_receipt)) =
        (worker_replay.as_ref(), codex_replay.as_ref())
    {
        // The two phase receipts are the complete replay authority. Derive
        // the predecessor revision from their sealed revision instead of
        // loading mutable queue, replacement, or Delivery state. This also
        // permits replay after a successor replacement or a damaged current
        // snapshot, as long as the original receipts and audit event remain
        // intact.
        let mut context = BindingContext::from_durable(&durable, &immutable_job, bound_at_millis)?;
        context.job_revision = worker_session_receipt
            .revision
            .checked_sub(1)
            .ok_or_else(|| {
                StorageError::invalid_input("SessionBinding replay revision is invalid")
            })?;
        let expected_identity = expected_session_identity(&context, message);
        validate_complete_replay(
            worker_session_receipt,
            codex_thread_receipt,
            &context,
            &worker_phase,
            &codex_phase,
            &expected_identity,
        )?;
        validate_binding_pending_audit_event(storage, codex_thread_receipt, &codex_phase, message)?;
        return Ok(DeliverySessionBindingCommitReceipt {
            worker_session_receipt: worker_session_receipt.clone(),
            codex_thread_receipt: codex_thread_receipt.clone(),
        });
    }

    // New and partially committed paths need the scheduler's current attempt
    // and replacement seal. Only these paths consult mutable queue state.
    let (_, job) = load_durable_execution_job(storage, &message.lease.job_id)?;
    validate_durable_job(&durable, &job, message)?;
    let mut context = BindingContext::from_durable(&durable, &job, bound_at_millis)?;
    // Validate the frame's internal identity before applying the scheduler
    // replacement owner phase. A malformed frame must not advance Delivery
    // merely because it carries the current attempt number.
    validate_message_session_identity(message)?;
    let replacement = replacement_for_job(storage, &job)?;

    // Validate scheduler authority before applying a replacement owner phase.
    // Partial/new binding paths are the only paths that may mutate Delivery.
    let acceptance = validate_session_binding(message, authority, delivery_scope(&job)?)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if let Some(replacement) = replacement {
        context.job_revision =
            ensure_delivery_replacement_applied(storage, &context, &replacement)?;
    }
    let expected_identity = expected_session_identity(&context, message);

    // A partially committed binding already has an owner receipt and must be
    // able to finish after response loss. Only a genuinely new binding is
    // authorized by the Server clock captured at ingress.
    if worker_replay.is_none() && codex_replay.is_none() {
        validate_trusted_lease_time(message, server_time)?;
    }

    let worker_session_receipt = if let Some(receipt) = worker_replay {
        validate_phase_receipt(
            &receipt,
            &context,
            &worker_phase,
            context.job_revision + 1,
            true,
            &expected_identity,
        )?;
        receipt
    } else {
        if codex_replay.is_some() {
            return Err(StorageError::invalid_input(
                "CodexThread receipt exists without its WorkerSession receipt",
            )
            .into());
        }
        commit_worker_session(storage, message, &context, &worker_phase, &acceptance).or_else(
            |source| {
                recover_raced_phase_receipt(
                    storage,
                    &context,
                    &worker_phase,
                    context.job_revision + 1,
                    &expected_identity,
                    source,
                )
            },
        )?
    };

    let codex_thread_receipt = match codex_replay {
        Some(receipt) => {
            validate_phase_receipt(
                &receipt,
                &context,
                &codex_phase,
                context.job_revision + 2,
                true,
                &expected_identity,
            )
            .map_err(
                |source| DeliverySessionBindingCommitError::CodexThreadPhase {
                    worker_session_receipt: Box::new(worker_session_receipt.clone()),
                    source,
                },
            )?;
            validate_binding_pending_audit_event(storage, &receipt, &codex_phase, message)
                .map_err(
                    |source| DeliverySessionBindingCommitError::CodexThreadPhase {
                        worker_session_receipt: Box::new(worker_session_receipt.clone()),
                        source,
                    },
                )?;
            receipt
        }
        None => commit_codex_thread(storage, message, &context, &codex_phase, &acceptance)
            .or_else(|source| {
                recover_raced_phase_receipt(
                    storage,
                    &context,
                    &codex_phase,
                    context.job_revision + 2,
                    &expected_identity,
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

    validate_binding_pending_audit_event(storage, &codex_thread_receipt, &codex_phase, message)
        .map_err(
            |source| DeliverySessionBindingCommitError::CodexThreadPhase {
                worker_session_receipt: Box::new(worker_session_receipt.clone()),
                source,
            },
        )?;

    Ok(DeliverySessionBindingCommitReceipt {
        worker_session_receipt,
        codex_thread_receipt,
    })
}

struct BindingContext {
    scope_key: ReceiptScopeKey,
    repository_scope: winwincode_domain::RepositoryScope,
    delivery_id: DeliveryId,
    identity: SessionBindingIdentity,
    job_revision: u64,
    attempt: u64,
    bound_at_millis: u64,
}

fn delivery_scope(job: &ExecutionJob) -> Result<&DeliveryStageExecutionScope, StorageError> {
    match &job.scope {
        ExecutionScope::DeliveryStageExecutionScope(scope) => Ok(scope),
        ExecutionScope::ProductSessionExecutionScope(_) => Err(StorageError::invalid_input(
            "SessionBinding ExecutionJob is not a Delivery stage job",
        )),
    }
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
            repository_scope: repository_scope_from_receipt_key(
                durable.receipt_identity().scope_key(),
            )?,
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

fn replacement_for_job(
    storage: &dyn ProductStateStorage,
    job: &ExecutionJob,
) -> Result<Option<ExecutionScopeReplacementAuthority>, StorageError> {
    let replacement = storage.load_execution_scope_replacement_authority(&job.job_id)?;
    let Some(replacement) = replacement else {
        return Ok(None);
    };
    let attempt = u64::try_from(job.attempt).map_err(|_| {
        StorageError::invalid_input("SessionBinding ExecutionJob attempt is out of range")
    })?;
    if replacement.replacement_attempt() != attempt || replacement.job_id() != &job.job_id {
        return Err(StorageError::invalid_input(
            "SessionBinding replacement does not own the current ExecutionJob attempt",
        ));
    }
    Ok(Some(replacement))
}

fn ensure_delivery_replacement_applied(
    storage: &mut dyn ProductStateStorage,
    context: &BindingContext,
    replacement: &ExecutionScopeReplacementAuthority,
) -> Result<u64, StorageError> {
    let phase = ReplacementPhase::new(context, replacement)?;
    if let Some(receipt) = storage.load_receipt(&phase.receipt_identity, &phase.command_digest)? {
        validate_replacement_receipt(&receipt, context, &phase, true)?;
        return Ok(receipt.revision);
    }
    let current = load_current_delivery_state(storage, context)?;
    let journal_key = delivery_journal_key(&context.delivery_id)?;
    let loaded = storage.load_journal(&journal_key)?;
    let journal = StagedDeliveryJournal::new(context.delivery_id.clone(), loaded);
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::ReplaceExecutionAttempt(Box::new(
            ReplaceDeliveryExecutionAttempt {
                expected_revision: current.revision(),
                identity: context.identity.clone(),
                replacement: DeliveryExecutionAttemptReplacement::from_scheduler(replacement),
                now_millis: instant_millis(replacement.created_at())?,
            },
        )))
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if mutation.replayed {
        return Err(StorageError::invalid_input(
            "Delivery replacement journal replay has no matching owner receipt",
        ));
    }
    let publication = journal
        .into_publication()
        .map_err(|error| StorageError::adapter(error.to_string()))?
        .ok_or_else(|| {
            StorageError::invalid_input(
                "execution.attempt.replaced did not stage a journal publication",
            )
        })?;
    let commit = StateCommit::new(
        phase.receipt_identity.clone(),
        phase.command_digest.clone(),
        delivery_stream_id(&context.delivery_id),
        current.revision(),
        mutation.snapshot.encode_json().map_err(|error| {
            StorageError::invalid_input(format!(
                "failed to encode replaced Delivery snapshot: {error}"
            ))
        })?,
        vec![NewOutboxEvent::internal(
            phase.event_id.clone(),
            "execution.scope-replacement.applied.v1",
            serde_json::to_vec(&serde_json::json!({
                "deliveryId": context.delivery_id,
                "executionJobId": replacement.job_id(),
                "replacementAttempt": replacement.replacement_attempt(),
                "stageRunId": context.identity.stage_run_id,
            }))
            .map_err(|error| StorageError::adapter(error.to_string()))?,
        )],
    )
    .with_journal_publication(publication);
    let receipt = storage.commit(&commit)?;
    validate_replacement_receipt(&receipt, context, &phase, receipt.idempotent_replay)?;
    Ok(receipt.revision)
}

struct ReplacementPhase {
    receipt_identity: ReceiptIdentity,
    command_digest: Sha256Digest,
    event_id: String,
}

impl ReplacementPhase {
    fn new(
        context: &BindingContext,
        replacement: &ExecutionScopeReplacementAuthority,
    ) -> Result<Self, StorageError> {
        let mut actor = Vec::new();
        actor.extend_from_slice(b"winwincode.execution-replacement-owner.v1\0");
        append_phase_fact(&mut actor, replacement.job_id().0.as_bytes());
        Ok(Self {
            receipt_identity: ReceiptIdentity::new(
                ReceiptActorKey::from_encoded(actor)?,
                context.scope_key.clone(),
                replacement.receipt_id().clone(),
            )?,
            command_digest: replacement.receipt_digest().clone(),
            event_id: format!("execution-replacement-owner:{}", replacement.receipt_id().0),
        })
    }
}

fn validate_replacement_receipt(
    receipt: &CommitReceipt,
    context: &BindingContext,
    phase: &ReplacementPhase,
    expected_replay: bool,
) -> Result<(), StorageError> {
    if receipt.receipt_identity != phase.receipt_identity
        || receipt.command_digest != phase.command_digest
        || receipt.idempotent_replay != expected_replay
        || receipt.stream_id != delivery_stream_id(&context.delivery_id)
        || receipt.revision == 0
        || receipt.events.len() != 1
        || receipt.events[0].event_id != phase.event_id
        || receipt.events[0].topic != "execution.scope-replacement.applied.v1"
    {
        return Err(StorageError::invalid_input(
            "Delivery replacement owner receipt is incomplete or foreign",
        ));
    }
    Ok(())
}

fn load_current_delivery_state(
    storage: &dyn ProductStateStorage,
    context: &BindingContext,
) -> Result<Delivery, StorageError> {
    let stream_id = delivery_stream_id(&context.delivery_id);
    let state = storage
        .load_state(&stream_id)?
        .ok_or_else(|| StorageError::invalid_input("SessionBinding Delivery state is missing"))?;
    let delivery = Delivery::decode_json(&state.payload).map_err(|error| {
        StorageError::invalid_input(format!("SessionBinding Delivery state is invalid: {error}"))
    })?;
    if delivery.id() != &context.delivery_id || delivery.revision() != state.revision {
        return Err(StorageError::invalid_input(
            "SessionBinding Delivery snapshot does not match durable state",
        ));
    }
    Ok(delivery)
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
    acceptance: &SessionBindingAcceptance<'_>,
) -> Result<CommitReceipt, StorageError> {
    let current = load_current_delivery(storage, context, context.job_revision)?;
    validate_current_binding(&current, context, message, BindingProgress::Pending)?;
    let session_identity = session_identity_for_current_binding(&current, context, message)?;
    let authority = delivery_authority(acceptance)?;
    commit_phase(
        storage,
        &current,
        context,
        phase,
        &session_identity,
        &authority,
        &message.codex_thread_id,
        SessionBindingCommitPhase::WorkerSession,
        message,
    )
}

fn commit_codex_thread(
    storage: &mut dyn ProductStateStorage,
    message: &SessionBindingMessage,
    context: &BindingContext,
    phase: &Phase,
    acceptance: &SessionBindingAcceptance<'_>,
) -> Result<CommitReceipt, StorageError> {
    let expected_revision = context
        .job_revision
        .checked_add(1)
        .ok_or_else(|| StorageError::invalid_input("Delivery revision overflow"))?;
    let current = load_current_delivery(storage, context, expected_revision)?;
    validate_current_binding(&current, context, message, BindingProgress::WorkerAccepted)?;
    let session_identity = session_identity_for_current_binding(&current, context, message)?;
    let authority = delivery_authority(acceptance)?;
    let receipt = commit_phase(
        storage,
        &current,
        context,
        phase,
        &session_identity,
        &authority,
        &message.codex_thread_id,
        SessionBindingCommitPhase::CodexThread,
        message,
    )?;
    validate_binding_pending_audit_event(storage, &receipt, phase, message)?;
    Ok(receipt)
}

fn session_identity_for_current_binding(
    delivery: &Delivery,
    context: &BindingContext,
    message: &SessionBindingMessage,
) -> Result<SessionIdentity, StorageError> {
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
            "SessionBinding event requires one exact current binding",
        ));
    };
    if let Some(worker_session_id) = &binding.worker_session_id
        && worker_session_id != &message.worker_session_id
    {
        return Err(StorageError::invalid_input(
            "SessionBinding event WorkerSession is foreign",
        ));
    }
    if let Some(codex_thread_id) = &binding.codex_thread_id
        && codex_thread_id != &message.codex_thread_id
    {
        return Err(StorageError::invalid_input(
            "SessionBinding event CodexThread is foreign",
        ));
    }
    Ok(SessionIdentity {
        codex_thread_id: message.codex_thread_id.clone(),
        product_session_id: binding.product_session_id.clone(),
        stage_run_id: Some(binding.stage_run_id.clone()),
        worker_session_id: message.worker_session_id.clone(),
    })
}

fn expected_session_identity(
    context: &BindingContext,
    message: &SessionBindingMessage,
) -> SessionIdentity {
    SessionIdentity {
        codex_thread_id: message.codex_thread_id.clone(),
        product_session_id: context.identity.product_session_id.clone(),
        stage_run_id: Some(context.identity.stage_run_id.clone()),
        worker_session_id: message.worker_session_id.clone(),
    }
}

fn delivery_authority(
    acceptance: &SessionBindingAcceptance<'_>,
) -> Result<DeliverySessionBindingAuthority, StorageError> {
    let message = acceptance.message();
    let active = acceptance.authority().active_lease();
    let attempt = u64::try_from(message.lease.attempt)
        .map_err(|_| StorageError::invalid_input("SessionBinding attempt is out of range"))?;
    Ok(DeliverySessionBindingAuthority::from_execution_port(
        active.worker_id().clone(),
        active.worker_instance_id().clone(),
        active.lease_id().clone(),
        attempt,
        active.fencing_token().clone(),
        active.worker_session_id().clone(),
        message.message_id.clone(),
    ))
}

fn binding_pending_audit_event(
    phase: &Phase,
    context: &BindingContext,
    session_identity: &SessionIdentity,
    message: &SessionBindingMessage,
    before: &Delivery,
    after: &Delivery,
) -> Result<PendingAuditEvent, StorageError> {
    let source =
        AuditBindingSource::try_new(message.message_id.clone(), AuditBindingPhase::CodexThread)
            .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let identity = AuditExecutionIdentity::try_new_binding(
        session_identity.product_session_id.clone(),
        session_identity.worker_session_id.clone(),
        session_identity.codex_thread_id.clone(),
        session_identity.stage_run_id.clone().ok_or_else(|| {
            StorageError::invalid_input("SessionBinding SessionIdentity has no StageRun")
        })?,
        context.identity.execution_job_id.clone(),
        context.delivery_id.clone(),
        context.identity.delivery_task_id.clone(),
        message.lease.worker_id.clone(),
        message.lease.worker_instance_id.clone(),
        message.lease.lease_id.clone(),
        u64::try_from(message.lease.attempt)
            .map_err(|_| StorageError::invalid_input("SessionBinding attempt is out of range"))?,
        message.lease.fencing_token.clone(),
        source,
    )
    .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let event_id = AuditEventId::from_digest(&phase.command_digest)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let subject = AuditSubject::accepted_binding(identity);
    execution_audit_event(
        event_id,
        context.bound_at_millis,
        phase.receipt_identity.request_id().clone(),
        &context.repository_scope,
        AuditAction::worker_lease("session.binding.accepted")
            .map_err(|error| StorageError::invalid_input(error.to_string()))?,
        before,
        after,
        subject,
        "execution.binding.accepted",
    )
}

#[allow(clippy::too_many_arguments)]
fn commit_phase(
    storage: &mut dyn ProductStateStorage,
    current: &Delivery,
    context: &BindingContext,
    phase: &Phase,
    session_identity: &SessionIdentity,
    authority: &DeliverySessionBindingAuthority,
    codex_thread_id: &winwincode_domain::CodexThreadId,
    binding_phase: SessionBindingCommitPhase,
    message: &SessionBindingMessage,
) -> Result<CommitReceipt, StorageError> {
    let journal_key = delivery_journal_key(&context.delivery_id)?;
    let loaded = storage.load_journal(&journal_key)?;
    let journal = StagedDeliveryJournal::new(context.delivery_id.clone(), loaded);
    let mutation = match binding_phase {
        SessionBindingCommitPhase::WorkerSession => DeliveryStore::borrowed(&journal).execute(
            DeliveryCommand::AcceptWorkerSession(Box::new(AcceptDeliveryWorkerSession {
                request_id: phase.receipt_identity.request_id().clone(),
                request_digest: phase.request_digest()?,
                expected_revision: current.revision(),
                identity: context.identity.clone(),
                authority: authority.clone(),
                now_millis: context.bound_at_millis,
            })),
        ),
        SessionBindingCommitPhase::CodexThread => DeliveryStore::borrowed(&journal).execute(
            DeliveryCommand::ReportCodexThread(Box::new(ReportDeliveryCodexThread {
                request_id: phase.receipt_identity.request_id().clone(),
                request_digest: phase.request_digest()?,
                expected_revision: current.revision(),
                identity: context.identity.clone(),
                authority: authority.clone(),
                codex_thread_id: codex_thread_id.clone(),
                now_millis: context.bound_at_millis,
            })),
        ),
    }
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
    let events = phase_events(context, revision, session_identity, message)?;
    let pending_audit_event = match binding_phase {
        SessionBindingCommitPhase::WorkerSession => None,
        SessionBindingCommitPhase::CodexThread => Some(binding_pending_audit_event(
            phase,
            context,
            session_identity,
            message,
            current,
            &mutation.snapshot,
        )?),
    };
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
    let commit = if let Some(pending_audit_event) = pending_audit_event {
        commit.with_pending_audit_event(pending_audit_event)
    } else {
        commit
    };
    let receipt = storage.commit(&commit)?;
    validate_phase_receipt(
        &receipt,
        context,
        phase,
        revision,
        receipt.idempotent_replay,
        session_identity,
    )?;
    Ok(receipt)
}

fn validate_binding_pending_audit_event(
    storage: &dyn ProductStateStorage,
    receipt: &CommitReceipt,
    phase: &Phase,
    message: &SessionBindingMessage,
) -> Result<(), StorageError> {
    let expected_event_id = AuditEventId::from_digest(&phase.command_digest)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let Some(stored) = storage.load_pending_audit_event(&receipt.receipt_identity)? else {
        return Err(StorageError::invalid_input(
            "accepted binding receipt has no pending audit event",
        ));
    };
    let event: AuditEvent = serde_json::from_slice(stored.payload()).map_err(|error| {
        StorageError::invalid_input(format!("accepted binding audit event is invalid: {error}"))
    })?;
    let canonical = serde_json::to_vec(&event).map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode accepted binding audit event: {error}"
        ))
    })?;
    if stored.event_id() != expected_event_id.as_str()
        || canonical != stored.payload()
        || event.event_id() != &expected_event_id
        || event.request_id() != phase.receipt_identity.request_id()
        || event.subject().execution_kind() != Some(AuditExecutionSubjectKind::AcceptedBinding)
        || event
            .subject()
            .execution()
            .and_then(|identity| identity.binding_source())
            .is_none_or(|source| source.message_id() != &message.message_id)
        || event
            .subject()
            .execution()
            .is_none_or(|identity| identity.source_sequence().is_some())
    {
        return Err(StorageError::invalid_input(
            "accepted binding audit event does not match its trusted receipt",
        ));
    }
    Ok(())
}

fn recover_raced_phase_receipt(
    storage: &dyn ProductStateStorage,
    context: &BindingContext,
    phase: &Phase,
    expected_revision: u64,
    expected_identity: &SessionIdentity,
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
    validate_phase_receipt(
        &receipt,
        context,
        phase,
        expected_revision,
        true,
        expected_identity,
    )?;
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

fn validate_trusted_lease_time(
    message: &SessionBindingMessage,
    server_time: &Instant,
) -> Result<(), StorageError> {
    let issued_at = instant_millis(&message.lease.issued_at)?;
    let expires_at = instant_millis(&message.lease.expires_at)?;
    let server_time = instant_millis(server_time)?;
    if server_time < issued_at || server_time >= expires_at {
        return Err(StorageError::invalid_input(
            "SessionBinding Server time is outside its active lease",
        ));
    }
    Ok(())
}

fn validate_message_session_identity(message: &SessionBindingMessage) -> Result<(), StorageError> {
    if message.session_identity.product_session_id != message.product_session_id
        || message.session_identity.worker_session_id != message.worker_session_id
        || message.session_identity.codex_thread_id != message.codex_thread_id
        || message.session_identity.stage_run_id != message.stage_run_id
    {
        return Err(StorageError::invalid_input(
            "SessionBinding message SessionIdentity is internally inconsistent",
        ));
    }
    Ok(())
}

fn validate_durable_job(
    _durable: &DurableOutboxEvent,
    job: &ExecutionJob,
    message: &SessionBindingMessage,
) -> Result<(), StorageError> {
    let ExecutionScope::DeliveryStageExecutionScope(scope) = &job.scope else {
        return Err(StorageError::invalid_input(
            "SessionBinding durable job has foreign scope",
        ));
    };
    if job.job_id != message.lease.job_id
        || job.attempt != message.lease.attempt
        || scope.product_session_id != message.product_session_id
        || message.stage_run_id.as_ref() != Some(&scope.stage_run_id)
    {
        return Err(StorageError::invalid_input(
            "SessionBinding message does not match the durable ExecutionJob",
        ));
    }
    Ok(())
}

fn validate_complete_replay(
    worker_receipt: &CommitReceipt,
    codex_receipt: &CommitReceipt,
    context: &BindingContext,
    worker_phase: &Phase,
    codex_phase: &Phase,
    expected_identity: &SessionIdentity,
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
    validate_phase_receipt(
        worker_receipt,
        context,
        worker_phase,
        worker_revision,
        true,
        expected_identity,
    )?;
    validate_phase_receipt(
        codex_receipt,
        context,
        codex_phase,
        codex_revision,
        true,
        expected_identity,
    )
}

fn phase_events(
    context: &BindingContext,
    revision: u64,
    session_identity: &SessionIdentity,
    message: &SessionBindingMessage,
) -> Result<Vec<NewOutboxEvent>, StorageError> {
    let scope = crate::public_repository_scope(&context.repository_scope);
    let source = PublicEventSource::SessionExecutionWorker {
        worker_id: message.lease.worker_id.clone(),
        worker_session_id: message.worker_session_id.clone(),
        lease_id: message.lease.lease_id.clone(),
        codex_thread_id: message.codex_thread_id.clone(),
        session_identity: session_identity.clone(),
    };
    Ok(vec![
        delivery_changed_event_for_scope(
            scope.clone(),
            &context.delivery_id,
            revision,
            DeliveryChangeKind::Advanced,
            message.sent_at.clone(),
            source.clone(),
        )?,
        runtime_invalidated_event(
            context,
            revision,
            session_identity,
            scope,
            message.sent_at.clone(),
            source,
        )?,
    ])
}

fn runtime_invalidated_event(
    context: &BindingContext,
    revision: u64,
    session_identity: &SessionIdentity,
    scope: PublicEventScope,
    occurred_at: Instant,
    source: PublicEventSource,
) -> Result<NewOutboxEvent, StorageError> {
    delivery_stage_runtime_invalidated_event(&DeliveryStageRuntimeInvalidation {
        scope_key: &context.scope_key,
        delivery_id: &context.delivery_id,
        stage_run_id: &context.identity.stage_run_id,
        product_session_id: &context.identity.product_session_id,
        session_identity,
        revision,
        event_namespace: b"winwincode.session-binding-runtime-invalidation.v1",
        scope,
        occurred_at,
        source,
    })
}

pub(crate) struct DeliveryStageRuntimeInvalidation<'facts> {
    pub scope_key: &'facts ReceiptScopeKey,
    pub delivery_id: &'facts DeliveryId,
    pub stage_run_id: &'facts winwincode_domain::StageRunId,
    pub product_session_id: &'facts winwincode_domain::ProductSessionId,
    pub session_identity: &'facts SessionIdentity,
    pub revision: u64,
    pub event_namespace: &'facts [u8],
    pub scope: PublicEventScope,
    pub occurred_at: Instant,
    pub source: PublicEventSource,
}

pub(crate) fn delivery_stage_runtime_invalidated_event(
    facts: &DeliveryStageRuntimeInvalidation<'_>,
) -> Result<NewOutboxEvent, StorageError> {
    let revision = i64::try_from(facts.revision)
        .map(Revision)
        .map_err(|_| StorageError::invalid_input("Delivery revision exceeds public range"))?;
    let payload = ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent {
        delivery_id: facts.delivery_id.clone(),
        last_projection_sequence: 0,
        product_session_id: facts.product_session_id.clone(),
        projection_revision: revision,
        reload_queries: (
            ControlPlaneWebSocketDeliveryGetReloadQuery::DeliveryGet,
            ControlPlaneWebSocketRuntimeProjectionGetReloadQuery::RuntimeProjectionGet,
        ),
        scope_kind:
            ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventScopeKind::DeliveryStage,
        session_identity: facts.session_identity.clone(),
        stage_run_id: facts.stage_run_id.clone(),
        type_value:
            ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventTypeValue::RuntimeProjectionInvalidatedV1,
    };
    let payload = serde_json::to_vec(&payload).map_err(|error| {
        StorageError::adapter(format!("failed to encode runtime invalidation: {error}"))
    })?;
    let event_id = projection_event_id(facts.event_namespace, facts.scope_key, &payload);
    NewOutboxEvent::public_projection(
        event_id,
        RUNTIME_INVALIDATED_TOPIC,
        payload,
        ProjectionEventStream::Delivery(facts.delivery_id.clone()),
        facts.scope.clone(),
        facts.occurred_at.clone(),
        facts.source.clone(),
    )
}

fn validate_phase_receipt(
    receipt: &CommitReceipt,
    context: &BindingContext,
    phase: &Phase,
    expected_revision: u64,
    expected_replay: bool,
    expected_identity: &SessionIdentity,
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
    validate_delivery_stage_runtime_invalidation(
        receipt,
        &context.delivery_id,
        &context.identity.stage_run_id,
        &context.identity.product_session_id,
        expected_identity,
        expected_revision,
        b"winwincode.session-binding-runtime-invalidation.v1",
    )
}

pub(crate) fn validate_delivery_stage_runtime_invalidation(
    receipt: &CommitReceipt,
    delivery_id: &DeliveryId,
    stage_run_id: &winwincode_domain::StageRunId,
    product_session_id: &winwincode_domain::ProductSessionId,
    expected_identity: &SessionIdentity,
    expected_revision: u64,
    event_namespace: &[u8],
) -> Result<(), StorageError> {
    let matching = receipt
        .events
        .iter()
        .filter(|event| event.topic == RUNTIME_INVALIDATED_TOPIC)
        .collect::<Vec<_>>();
    let [event] = matching.as_slice() else {
        return Err(StorageError::invalid_input(
            "receipt must contain one runtime projection invalidation",
        ));
    };
    let payload: ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent =
        serde_json::from_slice(&event.payload).map_err(|_| {
            StorageError::invalid_input("runtime projection invalidation is not canonical")
        })?;
    let canonical = serde_json::to_vec(&payload).map_err(|error| {
        StorageError::adapter(format!("failed to encode runtime invalidation: {error}"))
    })?;
    let expected_revision = i64::try_from(expected_revision).unwrap_or(-1);
    let cursor = event.projection_cursor.as_ref().ok_or_else(|| {
        StorageError::invalid_input("runtime projection invalidation has no cursor")
    })?;
    if canonical != event.payload
        || payload.delivery_id != *delivery_id
        || payload.stage_run_id != *stage_run_id
        || payload.product_session_id != *product_session_id
        || payload.session_identity.codex_thread_id != expected_identity.codex_thread_id
        || payload.session_identity.product_session_id != expected_identity.product_session_id
        || payload.session_identity.stage_run_id != expected_identity.stage_run_id
        || payload.session_identity.worker_session_id != expected_identity.worker_session_id
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
        || cursor.key().stream() != &ProjectionEventStream::Delivery(delivery_id.clone())
        || event.event_id
            != projection_event_id(
                event_namespace,
                receipt.receipt_identity.scope_key(),
                &event.payload,
            )
            .0
    {
        return Err(StorageError::invalid_input(
            "runtime projection invalidation does not match durable Delivery stage facts",
        ));
    }
    Ok(())
}

fn phase_request_id(
    message: &SessionBindingMessage,
    phase: &'static str,
) -> Result<RequestId, StorageError> {
    execution_message_request_id(&message.message_id, phase)
}

pub(crate) fn execution_message_request_id(
    message_id: &ExecutionMessageId,
    phase: &'static str,
) -> Result<RequestId, StorageError> {
    // The generated message identity owns a stable two-slot idempotency key.
    // Mutable message and durable-job facts belong in the phase digest so a
    // changed payload reaches storage as a request conflict instead of a new
    // request.
    let mut bytes = Vec::with_capacity(message_id.0.len() + phase.len() + 64);
    bytes.extend_from_slice(b"winwincode.session-binding-phase-request.v2\0");
    append_phase_fact(&mut bytes, phase.as_bytes());
    append_phase_fact(&mut bytes, message_id.0.as_bytes());
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

pub(crate) fn append_phase_fact(target: &mut Vec<u8>, value: &[u8]) {
    target.extend_from_slice(&(value.len() as u64).to_be_bytes());
    target.extend_from_slice(value);
}

fn phase_actor_key(message: &SessionBindingMessage) -> Result<ReceiptActorKey, StorageError> {
    execution_message_actor_key(&message.message_id)
}

pub(crate) fn execution_message_actor_key(
    message_id: &ExecutionMessageId,
) -> Result<ReceiptActorKey, StorageError> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"winwincode.execution-port-message-actor.v1\0");
    append_phase_fact(&mut bytes, message_id.0.as_bytes());
    ReceiptActorKey::from_encoded(bytes)
}

pub(crate) fn projection_event_id(
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

pub(crate) fn require_id(value: &str, prefix: &str, field: &str) -> Result<(), StorageError> {
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

pub(crate) fn instant_millis(instant: &Instant) -> Result<u64, StorageError> {
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
