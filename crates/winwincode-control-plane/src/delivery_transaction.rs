// SPDX-License-Identifier: Apache-2.0

//! One authoritative Delivery command transaction over product storage.

use std::sync::Mutex;

use serde_json::Value;
use winwincode_api::generated::{
    CommandEnvelope, CommandName, DeliveryAdvancePayload, DeliveryStageExecutionScope,
    ExecutionJob, ExecutionScope,
};
use winwincode_delivery::domain::Delivery;
use winwincode_delivery::store::{
    AppendDelivery, AtomicPublication, DeliveryCommand, DeliveryCommandPort, DeliveryJournalPort,
    DeliveryMutationOperation, DeliveryStore, JournalBackendError, JournalBackendErrorCode,
    JournalEntryState, JournalRecordBytes, LoadedDeliveryJournal,
};
use winwincode_domain::DeliveryId;
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord,
    LoadedAggregateJournal, NewOutboxEvent, ProductStateStorage, StorageError, StorageErrorKind,
};

use crate::delivery_execution::{
    DeliveryExecutionCommitReceipt, DeliveryExecutionDispatchReceipt, DeliveryExecutionError,
    DeliveryExecutionPortError, DeliveryExecutionTransaction, ExecutionJobDispatcher,
    PendingDeliveryExecution, commit_and_dispatch,
};
use crate::{StateChange, storage_commit};

const DELIVERY_AGGREGATE_TYPE: &str = "delivery";
const EXECUTION_JOB_TOPIC: &str = "execution.job.dispatch";
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
        let stream_id = delivery_stream_id(pending.delivery().id());
        let mut commit = storage_commit(
            self.command,
            StateChange::new(
                &stream_id,
                pending.delivery().encode_json().map_err(port_error)?,
                vec![NewOutboxEvent::new(
                    &outbox_event_id,
                    EXECUTION_JOB_TOPIC,
                    job_payload,
                )],
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
        let mutation = DeliveryStore::borrowed(&journal)
            .execute(DeliveryCommand::Append(AppendDelivery {
                delivery_id: pending.delivery().id().clone(),
                request_id: pending.request_id().clone(),
                request_digest,
                operation: DeliveryMutationOperation::StageStarted,
                expected_revision,
                snapshot: pending.delivery().clone(),
            }))
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
        committed_delivery_receipt(self.storage, &receipt, &stream_id)
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
    let expected_revision = u64::try_from(command.expected_revision.0).map_err(|_| {
        DeliveryExecutionPortError::new("Delivery expectedRevision must not be negative")
    })?;
    if pending.delivery().revision() != expected_revision.saturating_add(1) {
        return Err(DeliveryExecutionPortError::new(
            "pending Delivery revision does not follow command expectedRevision",
        ));
    }
    let ExecutionScope::DeliveryStageExecutionScope(scope) = &pending.job().scope else {
        return Err(DeliveryExecutionPortError::new(
            "pending job is not a Delivery stage execution",
        ));
    };
    if scope.delivery_id != *pending.delivery().id() {
        return Err(DeliveryExecutionPortError::new(
            "pending job belongs to another Delivery",
        ));
    }
    Ok(())
}

fn committed_delivery_receipt(
    storage: &dyn ProductStateStorage,
    receipt: &winwincode_storage::CommitReceipt,
    expected_stream_id: &str,
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
    let ExecutionScope::DeliveryStageExecutionScope(scope) = &job.scope else {
        return Err(DeliveryExecutionPortError::new(
            "durable execution job is not a Delivery stage job",
        ));
    };
    if scope.delivery_id != *delivery.id()
        || !delivery.snapshot().session_bindings.iter().any(|binding| {
            binding.execution_job_id == job.job_id
                && binding.delivery_id == scope.delivery_id
                && binding.delivery_task_id == scope.delivery_task_id
                && binding.stage_run_id == scope.stage_run_id
                && binding.product_session_id == scope.product_session_id
        })
    {
        return Err(DeliveryExecutionPortError::new(
            "durable execution job does not match the committed Delivery binding",
        ));
    }
    Ok(DeliveryExecutionCommitReceipt {
        committed_revision: receipt.revision,
        outbox_event_id: event.event_id.clone(),
        job,
        replayed: receipt.idempotent_replay,
    })
}

fn strict_execution_job(payload: &[u8]) -> Result<ExecutionJob, DeliveryExecutionPortError> {
    let value: Value = serde_json::from_slice(payload).map_err(port_error)?;
    let mut job: ExecutionJob = serde_json::from_value(value.clone())
        .map_err(|_| DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB))?;
    let scope_value = value
        .get("scope")
        .ok_or_else(|| DeliveryExecutionPortError::new("durable execution job scope is missing"))?
        .clone();
    let scope: DeliveryStageExecutionScope = serde_json::from_value(scope_value)
        .map_err(|_| DeliveryExecutionPortError::new(NON_CANONICAL_EXECUTION_JOB))?;
    if scope.kind != "delivery-stage" {
        return Err(DeliveryExecutionPortError::new(
            "durable execution job scope kind is not delivery-stage",
        ));
    }
    job.scope = ExecutionScope::DeliveryStageExecutionScope(scope);
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

fn execution_job_event_id(job: &ExecutionJob) -> String {
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
        | StorageErrorKind::Adapter
        | StorageErrorKind::Closed => JournalBackendErrorCode::Io,
    };
    JournalBackendError::new(code, error.to_string())
}
