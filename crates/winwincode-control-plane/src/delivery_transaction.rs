// SPDX-License-Identifier: Apache-2.0

//! One authoritative Delivery command transaction over product storage.

use std::sync::Mutex;

use serde_json::Value;
use sha2::{Digest, Sha256};
use winwincode_api::generated::{CommandEnvelope, CommandName, DeliveryAdvancePayload};
use winwincode_delivery::domain::Delivery;
use winwincode_delivery::store::{
    AtomicPublication, DeliveryCommand, DeliveryCommandPort, DeliveryJournalPort, DeliveryStore,
    JournalBackendError, JournalBackendErrorCode, JournalEntryState, JournalRecordBytes,
    LoadedDeliveryJournal, StartDeliveryWorkRunDispatch,
};
use winwincode_domain::{DeliveryId, ExecutionJobId, Sha256Digest};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionScope, ProductSessionExecutionScope, ProductSessionExecutionScopeKind,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, DurableOutboxEvent,
    ExecutionJobRecord, LoadedAggregateJournal, NewOutboxEvent, ProductStateStorage, StorageError,
    StorageErrorKind,
};

use crate::delivery_execution::{
    DeliveryExecutionCommitReceipt, DeliveryExecutionDispatchReceipt, DeliveryExecutionError,
    DeliveryExecutionPortError, DeliveryExecutionTransaction, ExecutionJobDispatcher,
    PendingDeliveryExecution, commit_and_dispatch,
};
use crate::{
    DeliveryChangeKind, StateChange, delivery_changed_event, storage_commit,
    validate_delivery_changed_receipt,
};

const DELIVERY_AGGREGATE_TYPE: &str = "delivery";
pub(crate) const EXECUTION_JOB_TOPIC: &str = "execution.job.dispatch";
const NON_CANONICAL_EXECUTION_JOB: &str =
    "durable execution job payload has unknown or non-canonical fields";

pub(crate) fn execute(
    storage: &mut dyn ProductStateStorage,
    command: &CommandEnvelope,
    pending: &PendingDeliveryExecution,
    dispatcher: &mut dyn ExecutionJobDispatcher,
) -> Result<DeliveryExecutionDispatchReceipt, DeliveryExecutionError> {
    let mut transaction = AtomicDeliveryExecutionTransaction { storage, command };
    commit_and_dispatch(pending, &mut transaction, dispatcher)
}

struct AtomicDeliveryExecutionTransaction<'storage, 'command> {
    storage: &'storage mut dyn ProductStateStorage,
    command: &'command CommandEnvelope,
}

impl DeliveryExecutionTransaction for AtomicDeliveryExecutionTransaction<'_, '_> {
    fn commit_delivery_and_job_intent(
        &mut self,
        pending: &PendingDeliveryExecution,
    ) -> Result<DeliveryExecutionCommitReceipt, DeliveryExecutionPortError> {
        validate_command(self.command, pending)?;
        let outbox_event_id = execution_job_event_id(pending.job());
        let job_payload = serde_json::to_vec(pending.job()).map_err(port_error)?;
        let changed_event = delivery_changed_event(
            self.command,
            pending.delivery().id(),
            pending.delivery().revision(),
            DeliveryChangeKind::Advanced,
            crate::instant_from_millis(pending.delivery().snapshot().updated_at_millis)
                .map_err(port_error)?,
            "delivery-execution-transaction",
        )
        .map_err(port_error)?;
        let stream_id = delivery_stream_id(pending.delivery().id());
        let mut commit = storage_commit(
            self.command,
            StateChange::new(
                &stream_id,
                pending.delivery().encode_json().map_err(port_error)?,
                vec![
                    NewOutboxEvent::internal(&outbox_event_id, EXECUTION_JOB_TOPIC, job_payload),
                    changed_event,
                ],
            ),
        )
        .map_err(port_error)?;
        let request_digest = commit
            .command_digest
            .0
            .strip_prefix("sha256:")
            .ok_or_else(|| DeliveryExecutionPortError::new("command digest is not canonical"))?
            .to_owned();
        let journal_key = delivery_journal_key(pending.delivery().id()).map_err(port_error)?;
        let loaded = self
            .storage
            .load_journal(&journal_key)
            .map_err(port_error)?;
        let journal = StagedDeliveryJournal::new(pending.delivery().id().clone(), loaded);
        let expected_revision = u64::try_from(self.command.expected_revision.0).map_err(|_| {
            DeliveryExecutionPortError::new("Delivery expectedRevision must not be negative")
        })?;
        let command =
            DeliveryCommand::StartWorkRunDispatch(Box::new(StartDeliveryWorkRunDispatch {
                request_id: pending.request_id().clone(),
                request_digest,
                expected_revision,
                transition: pending.stage_transition().clone(),
            }));
        let mutation = DeliveryStore::borrowed(&journal)
            .execute(command)
            .map_err(port_error)?;
        commit.state = mutation.snapshot.encode_json().map_err(port_error)?;
        if mutation.replayed {
            commit = commit.require_receipt_replay();
        }
        if let Some(publication) = journal.into_publication()? {
            if mutation.replayed {
                return Err(DeliveryExecutionPortError::new(
                    "replayed Delivery mutation unexpectedly staged another journal record",
                ));
            }
            commit = commit.with_journal_publication(publication);
        } else if !mutation.replayed {
            return Err(DeliveryExecutionPortError::new(
                "new Delivery mutation did not stage a journal publication",
            ));
        }

        let receipt = self.storage.commit(&commit).map_err(port_error)?;
        committed_delivery_receipt(self.storage, &receipt, &stream_id, pending)
    }

    fn mark_job_dispatched(
        &mut self,
        outbox_event_id: &str,
    ) -> Result<(), DeliveryExecutionPortError> {
        self.storage
            .mark_published(outbox_event_id)
            .map_err(port_error)
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep all request-to-sealed-intent comparisons in one validation boundary"
)]
fn validate_command(
    command: &CommandEnvelope,
    pending: &PendingDeliveryExecution,
) -> Result<(), DeliveryExecutionPortError> {
    if command.command != CommandName::DeliveryAdvance {
        return Err(DeliveryExecutionPortError::new(
            "Delivery execution transaction requires delivery.advance",
        ));
    }
    if command.request_id != *pending.request_id() {
        return Err(DeliveryExecutionPortError::new(
            "command requestId does not match the pending Delivery execution",
        ));
    }
    let payload: DeliveryAdvancePayload =
        serde_json::from_value(command.payload.clone()).map_err(|error| {
            DeliveryExecutionPortError::new(format!(
                "delivery.advance payload is not canonical: {error}"
            ))
        })?;
    if serde_json::to_value(&payload).map_err(port_error)? != command.payload
        || payload.delivery_id != *pending.delivery().id()
    {
        return Err(DeliveryExecutionPortError::new(
            "delivery.advance payload does not identify the pending Delivery exactly",
        ));
    }
    pending
        .stage_transition()
        .validate_projection()
        .map_err(port_error)?;
    let winwincode_delivery::application::stage::StageAdvanceEffect::Dispatch(intent) =
        &pending.stage_transition().effect
    else {
        return Err(DeliveryExecutionPortError::new(
            "new execution requires a sealed dispatch intent",
        ));
    };
    let expected_rework = intent.rework_authorization().map(|authorization| {
        winwincode_api::generated::DeliveryReworkInput {
            candidate_ref: authorization.candidate_ref().into(),
            diff_sha256: authorization.diff_sha256().into(),
            targets: authorization
                .targets()
                .iter()
                .map(
                    |target| winwincode_api::generated::DeliveryReworkTargetInput {
                        work_item_id: target.work_item_id().clone(),
                        file_path: target.file_path().into(),
                        source_hunk_sha256: target.hunk_sha256().into(),
                        evidence_ref_ids: target.evidence_ref_ids().to_vec(),
                    },
                )
                .collect(),
        }
    });
    let mut requested_rework = payload.rework.clone();
    if let Some(input) = &mut requested_rework {
        for target in &mut input.targets {
            target
                .evidence_ref_ids
                .sort_by(|left, right| left.0.cmp(&right.0));
        }
    }
    if requested_rework != expected_rework {
        return Err(DeliveryExecutionPortError::new(
            "requested rework scope differs from the sealed dispatch authorization",
        ));
    }
    let expected_job = crate::delivery_execution::prepare_workrun_advance(
        pending.request_id(),
        &pending.delivery().snapshot().work_run_aggregate,
        &pending.delivery().snapshot().spec,
        intent,
        crate::delivery_execution::DeliveryExecutionConfig {
            payload_digest: pending.job().payload_digest.clone(),
            candidate_ref: pending
                .job()
                .work_input
                .as_ref()
                .and_then(|input| input.candidate_ref.clone()),
            workspace: pending.job().workspace.clone(),
            limits: pending.job().limits.clone(),
        },
    )
    .map_err(port_error)?;
    let winwincode_api::generated::Scope::RepositoryScope(repository) = &command.scope else {
        return Err(DeliveryExecutionPortError::new(
            "WorkRun dispatch requires repository scope",
        ));
    };
    if expected_job != *pending.job()
        || pending.job().workspace.repository_id != repository.repository_id
        || serde_json::to_value(&payload.dispatch_profile).map_err(port_error)?
            != Value::String(pending.job().execution_profile.clone())
    {
        return Err(DeliveryExecutionPortError::new(
            "ExecutionJob does not match its sealed dispatch and command",
        ));
    }
    let expected_revision = u64::try_from(command.expected_revision.0).map_err(|_| {
        DeliveryExecutionPortError::new("Delivery expectedRevision must not be negative")
    })?;
    if pending.delivery().revision() != expected_revision.saturating_add(1) {
        return Err(DeliveryExecutionPortError::new(
            "pending Delivery revision does not follow command expectedRevision",
        ));
    }
    let ExecutionScope::WorkRunExecutionScope(scope) = &pending.job().scope else {
        return Err(DeliveryExecutionPortError::new(
            "pending job is not a Delivery stage execution",
        ));
    };
    if pending
        .job()
        .work_input
        .as_ref()
        .is_none_or(|input| input.work_contract.id != scope.work_contract_id)
    {
        return Err(DeliveryExecutionPortError::new(
            "pending WorkRun input does not match its scope",
        ));
    }
    Ok(())
}

fn committed_delivery_receipt(
    storage: &dyn ProductStateStorage,
    receipt: &winwincode_storage::CommitReceipt,
    expected_stream_id: &str,
    pending: &PendingDeliveryExecution,
) -> Result<DeliveryExecutionCommitReceipt, DeliveryExecutionPortError> {
    if receipt.stream_id != expected_stream_id {
        return Err(DeliveryExecutionPortError::new(
            "durable command receipt belongs to another Delivery stream",
        ));
    }
    let state = storage
        .load_state(&receipt.stream_id)
        .map_err(port_error)?
        .ok_or_else(|| DeliveryExecutionPortError::new("durable Delivery state is missing"))?;
    if state.revision != receipt.revision {
        return Err(DeliveryExecutionPortError::new(
            "durable Delivery state and receipt revisions differ",
        ));
    }
    let delivery = Delivery::decode_json(&state.payload).map_err(port_error)?;
    if delivery.revision() != receipt.revision {
        return Err(DeliveryExecutionPortError::new(
            "durable Delivery snapshot and receipt revisions differ",
        ));
    }
    let matching_events = receipt
        .events
        .iter()
        .filter(|event| event.topic == EXECUTION_JOB_TOPIC)
        .collect::<Vec<_>>();
    let [event] = matching_events.as_slice() else {
        return Err(DeliveryExecutionPortError::new(
            "durable receipt must contain exactly one execution job event",
        ));
    };
    let job = strict_execution_job(&event.payload)?;
    if event.event_id != execution_job_event_id(&job) {
        return Err(DeliveryExecutionPortError::new(
            "durable execution job event id does not match its job",
        ));
    }
    if job.job_id != pending.job().job_id || job.scope != pending.job().scope {
        return Err(DeliveryExecutionPortError::new(
            "durable execution job does not match the committed Delivery binding",
        ));
    }
    let ExecutionScope::WorkRunExecutionScope(_scope) = &job.scope else {
        return Err(DeliveryExecutionPortError::new(
            "durable execution job is not a Delivery stage job",
        ));
    };
    // The WorkRun and SessionBinding are intentionally absent at this point.
    // They are appended only after the scheduler accepts this immutable job
    // and the ExecutionPort verifies its dispatch authority.
    if delivery
        .snapshot()
        .session_bindings
        .iter()
        .any(|binding| binding.execution_job_id == job.job_id)
    {
        return Err(DeliveryExecutionPortError::new(
            "durable execution job was already bound before dispatch acceptance",
        ));
    }
    validate_delivery_changed_receipt(
        receipt,
        delivery.id(),
        delivery.revision(),
        DeliveryChangeKind::Advanced,
    )
    .map_err(port_error)?;
    Ok(DeliveryExecutionCommitReceipt {
        committed_revision: receipt.revision,
        outbox_event_id: event.event_id.clone(),
        job,
        replayed: receipt.idempotent_replay,
    })
}

pub(crate) fn strict_execution_job(
    payload: &[u8],
) -> Result<ExecutionJob, DeliveryExecutionPortError> {
    let value: Value = serde_json::from_slice(payload).map_err(port_error)?;
    let mut job: ExecutionJob = serde_json::from_value(value.clone())
        .map_err(|_| DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB))?;
    let scope_value = value
        .get("scope")
        .ok_or_else(|| DeliveryExecutionPortError::new("durable execution job scope is missing"))?
        .clone();
    let scope: winwincode_execution_port::generated::WorkRunExecutionScope =
        serde_json::from_value(scope_value)
            .map_err(|_| DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB))?;
    if scope.kind != winwincode_execution_port::generated::WorkRunExecutionScopeKind::WorkRun {
        return Err(DeliveryExecutionPortError::new(
            "durable execution job scope kind is not delivery-stage",
        ));
    }
    job.scope = ExecutionScope::WorkRunExecutionScope(scope);
    let canonical = serde_json::to_value(&job).map_err(port_error)?;
    if value != canonical {
        return Err(DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB));
    }
    Ok(job)
}

/// Loads and validates the one immutable dispatch intent owned by a generated
/// `ExecutionPort` message. Session binding and terminal outcome transactions
/// share this authority join rather than interpreting Worker-supplied scope.
pub(crate) fn load_durable_execution_job(
    storage: &dyn ProductStateStorage,
    job_id: &ExecutionJobId,
) -> Result<(DurableOutboxEvent, ExecutionJob), StorageError> {
    let (durable, original) = load_durable_execution_intent(storage, job_id)?;
    let Some(record) = storage.load_execution_job_record(job_id)? else {
        return Ok((durable, original));
    };
    let current = strict_scheduled_execution_job(&record)?;
    if current.attempt == original.attempt {
        if current != original {
            return Err(StorageError::adapter(
                "scheduled ExecutionJob differs from its immutable durable intent",
            ));
        }
        return Ok((durable, current));
    }
    let replacement = storage
        .load_execution_scope_replacement_authority(job_id)?
        .ok_or_else(|| StorageError::adapter("scheduled replacement has no sealed authority"))?;
    let current_attempt = u64::try_from(current.attempt)
        .map_err(|_| StorageError::adapter("scheduled replacement attempt is invalid"))?;
    if replacement.replacement_attempt() != current_attempt
        || replacement.job_id() != job_id
        || replacement.logical_job_digest() != &logical_job_digest(&record.dispatch_payload)?
    {
        return Err(StorageError::adapter(
            "scheduled replacement differs from its sealed authority",
        ));
    }
    let mut immutable = current.clone();
    immutable.attempt = original.attempt;
    match (&mut immutable.scope, &original.scope) {
        (
            ExecutionScope::WorkRunExecutionScope(current_scope),
            ExecutionScope::WorkRunExecutionScope(original_scope),
        ) => {
            current_scope.attempt = original_scope.attempt;
            current_scope.work_run_id = original_scope.work_run_id.clone();
        }
        (
            ExecutionScope::ProductSessionExecutionScope(_),
            ExecutionScope::ProductSessionExecutionScope(_),
        ) => {}
        _ => {
            return Err(StorageError::adapter(
                "scheduled replacement changed ExecutionJob scope kind",
            ));
        }
    }
    if immutable != original {
        return Err(StorageError::adapter(
            "scheduled replacement changed immutable ExecutionJob fields",
        ));
    }
    Ok((durable, current))
}

/// Loads only the immutable `ExecutionJob` intent event. This read is kept
/// separate from [`load_durable_execution_job`] so a complete phase-receipt
/// replay can resolve before consulting mutable queue/replacement state.
pub(crate) fn load_durable_execution_intent(
    storage: &dyn ProductStateStorage,
    job_id: &ExecutionJobId,
) -> Result<(DurableOutboxEvent, ExecutionJob), StorageError> {
    let event_id = format!("execution-job:{}", job_id.0);
    let durable = storage
        .load_outbox_event(&event_id)?
        .ok_or_else(|| StorageError::invalid_input("ExecutionJob event does not exist"))?;
    let event = durable.event();
    if event.event_id != event_id
        || event.topic != EXECUTION_JOB_TOPIC
        || event.projection_cursor.is_some()
    {
        return Err(StorageError::invalid_input(
            "durable event is not the exact internal ExecutionJob intent",
        ));
    }
    let original = strict_any_execution_job(&event.payload)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if &original.job_id != job_id {
        return Err(StorageError::invalid_input(
            "durable ExecutionJob event identity does not match its payload",
        ));
    }
    Ok((durable, original))
}

fn strict_scheduled_execution_job(
    record: &ExecutionJobRecord,
) -> Result<ExecutionJob, StorageError> {
    let job = strict_any_execution_job(&record.dispatch_payload)
        .map_err(|error| StorageError::adapter(error.to_string()))?;
    let attempt = u64::try_from(job.attempt)
        .map_err(|_| StorageError::adapter("scheduled ExecutionJob attempt is invalid"))?;
    if job.job_id != record.job_id
        || job.payload_digest != record.payload_digest
        || attempt != record.attempt
        || job.workspace.repository_id != record.scope.repository_id
    {
        return Err(StorageError::adapter(
            "scheduled ExecutionJob row differs from its canonical payload",
        ));
    }
    let scope_matches = match &job.scope {
        ExecutionScope::ProductSessionExecutionScope(scope) => {
            scope.product_session_id == record.scope.product_session_id
                && record.scope.delivery_id.is_none()
                && record.work_run_id.is_none()
        }
        ExecutionScope::WorkRunExecutionScope(scope) => {
            scope.product_session_id == record.scope.product_session_id
                && Some(&scope.work_run_id) == record.work_run_id.as_ref()
        }
    };
    if !scope_matches {
        return Err(StorageError::adapter(
            "scheduled ExecutionJob scope differs from its canonical payload",
        ));
    }
    Ok(job)
}

fn logical_job_digest(payload: &[u8]) -> Result<Sha256Digest, StorageError> {
    let mut value: Value = serde_json::from_slice(payload)
        .map_err(|_| StorageError::adapter("scheduled ExecutionJob is not valid JSON"))?;
    value
        .as_object_mut()
        .and_then(|object| object.remove("attempt"))
        .ok_or_else(|| StorageError::adapter("scheduled ExecutionJob attempt is missing"))?;
    if let Some(scope) = value.get_mut("scope").and_then(Value::as_object_mut) {
        scope.remove("attempt");
        scope.remove("workRunId");
    }
    let encoded = serde_json::to_vec(&value)
        .map_err(|_| StorageError::adapter("scheduled logical Job cannot encode"))?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

fn strict_any_execution_job(payload: &[u8]) -> Result<ExecutionJob, DeliveryExecutionPortError> {
    let value: Value = serde_json::from_slice(payload).map_err(port_error)?;
    let mut job: ExecutionJob = serde_json::from_value(value.clone())
        .map_err(|_| DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB))?;
    let scope_value = value
        .get("scope")
        .ok_or_else(|| DeliveryExecutionPortError::new("durable execution job scope is missing"))?
        .clone();
    let kind = scope_value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB))?;
    job.scope = match kind {
        "work-run" => {
            let scope: winwincode_execution_port::generated::WorkRunExecutionScope =
                serde_json::from_value(scope_value)
                    .map_err(|_| DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB))?;
            if scope.kind
                != winwincode_execution_port::generated::WorkRunExecutionScopeKind::WorkRun
            {
                return Err(DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB));
            }
            ExecutionScope::WorkRunExecutionScope(scope)
        }
        "product-session" => {
            let scope: ProductSessionExecutionScope = serde_json::from_value(scope_value)
                .map_err(|_| DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB))?;
            if scope.kind != ProductSessionExecutionScopeKind::ProductSession {
                return Err(DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB));
            }
            ExecutionScope::ProductSessionExecutionScope(scope)
        }
        _ => return Err(DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB)),
    };
    let canonical = serde_json::to_value(&job).map_err(port_error)?;
    if value != canonical {
        return Err(DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB));
    }
    Ok(job)
}

pub(crate) fn delivery_stream_id(delivery_id: &DeliveryId) -> String {
    format!("delivery:{}", delivery_id.0)
}

pub(crate) fn delivery_journal_key(
    delivery_id: &DeliveryId,
) -> Result<AggregateJournalKey, StorageError> {
    AggregateJournalKey::new(DELIVERY_AGGREGATE_TYPE, &delivery_id.0)
}

pub(crate) fn execution_job_event_id(job: &ExecutionJob) -> String {
    format!("execution-job:{}", job.job_id.0)
}

fn port_error(error: impl std::fmt::Display) -> DeliveryExecutionPortError {
    DeliveryExecutionPortError::new(error.to_string())
}

pub(crate) struct StagedDeliveryJournal {
    delivery_id: DeliveryId,
    loaded: Option<LoadedAggregateJournal>,
    publication: Mutex<Option<AggregateJournalPublication>>,
}

impl StagedDeliveryJournal {
    pub(crate) fn new(delivery_id: DeliveryId, loaded: Option<LoadedAggregateJournal>) -> Self {
        Self {
            delivery_id,
            loaded,
            publication: Mutex::new(None),
        }
    }

    pub(crate) fn into_publication(
        self,
    ) -> Result<Option<AggregateJournalPublication>, DeliveryExecutionPortError> {
        self.publication
            .into_inner()
            .map_err(|_| DeliveryExecutionPortError::new("staged journal lock is poisoned"))
    }
}

impl DeliveryJournalPort for StagedDeliveryJournal {
    fn load(
        &self,
        delivery_id: &DeliveryId,
    ) -> Result<Option<LoadedDeliveryJournal>, JournalBackendError> {
        if delivery_id != &self.delivery_id {
            return Err(JournalBackendError::new(
                JournalBackendErrorCode::Io,
                "transaction journal was queried for another Delivery",
            ));
        }
        Ok(self.loaded.as_ref().map(storage_journal_to_delivery))
    }

    fn publish(&self, publication: AtomicPublication) -> Result<(), JournalBackendError> {
        let publication = delivery_publication_to_storage(&self.delivery_id, publication)?;
        let mut staged = self.publication.lock().map_err(|_| {
            JournalBackendError::new(
                JournalBackendErrorCode::Io,
                "staged journal lock is poisoned",
            )
        })?;
        if staged.is_some() {
            return Err(JournalBackendError::new(
                JournalBackendErrorCode::Io,
                "transaction staged more than one Delivery publication",
            ));
        }
        *staged = Some(publication);
        Ok(())
    }
}

fn storage_journal_to_delivery(journal: &LoadedAggregateJournal) -> LoadedDeliveryJournal {
    LoadedDeliveryJournal {
        manifest: journal.manifest.clone(),
        records: journal
            .records
            .iter()
            .map(|record| JournalRecordBytes {
                sequence: record.sequence,
                state: JournalEntryState::Published,
                digest: record.digest.clone(),
                bytes: record.payload.clone(),
            })
            .collect(),
    }
}

fn delivery_publication_to_storage(
    expected_delivery_id: &DeliveryId,
    publication: AtomicPublication,
) -> Result<AggregateJournalPublication, JournalBackendError> {
    match publication {
        AtomicPublication::Create {
            delivery_id,
            manifest,
            first_record,
        } => {
            require_delivery_id(expected_delivery_id, &delivery_id)?;
            require_published_record(&first_record)?;
            Ok(AggregateJournalPublication::Create {
                key: delivery_journal_key(&delivery_id)
                    .map_err(|error| storage_journal_error(&error))?,
                manifest,
                first_record: AggregateJournalRecord::new(
                    first_record.sequence,
                    first_record.digest,
                    first_record.bytes,
                ),
            })
        }
        AtomicPublication::Append {
            delivery_id,
            expected_tail_sequence,
            expected_tail_digest,
            record,
        } => {
            require_delivery_id(expected_delivery_id, &delivery_id)?;
            require_published_record(&record)?;
            Ok(AggregateJournalPublication::Append {
                key: delivery_journal_key(&delivery_id)
                    .map_err(|error| storage_journal_error(&error))?,
                expected_tail_sequence,
                expected_tail_digest,
                record: AggregateJournalRecord::new(record.sequence, record.digest, record.bytes),
            })
        }
    }
}

fn require_delivery_id(
    expected: &DeliveryId,
    actual: &DeliveryId,
) -> Result<(), JournalBackendError> {
    if expected == actual {
        Ok(())
    } else {
        Err(JournalBackendError::new(
            JournalBackendErrorCode::Io,
            "Delivery publication belongs to another aggregate",
        ))
    }
}

fn require_published_record(record: &JournalRecordBytes) -> Result<(), JournalBackendError> {
    if record.state != JournalEntryState::Published
        || record.digest.len() != 64
        || !record
            .digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(JournalBackendError::new(
            JournalBackendErrorCode::Io,
            "Delivery publication record is not canonical",
        ));
    }
    Ok(())
}

fn storage_journal_error(error: &StorageError) -> JournalBackendError {
    let code = match error.kind() {
        StorageErrorKind::JournalAlreadyExists => JournalBackendErrorCode::AlreadyExists,
        StorageErrorKind::JournalNotFound => JournalBackendErrorCode::NotFound,
        StorageErrorKind::JournalConflict => JournalBackendErrorCode::Conflict,
        StorageErrorKind::InvalidInput
        | StorageErrorKind::RevisionConflict
        | StorageErrorKind::RequestConflict
        | StorageErrorKind::RequestReplayMissing
        | StorageErrorKind::EventCursorExpired
        | StorageErrorKind::Adapter
        | StorageErrorKind::Closed => JournalBackendErrorCode::Io,
    };
    JournalBackendError::new(code, error.to_string())
}
