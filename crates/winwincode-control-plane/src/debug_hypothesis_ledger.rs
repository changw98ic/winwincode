// SPDX-License-Identifier: Apache-2.0

//! Durable, receipt-first ownership for the `DebugProbe` hypothesis Ledger.
//!
//! The Control Plane stores one alternating `Prepared`/`Committed` stream and
//! one append-only aggregate journal. A first preparation includes the pure
//! initialization event in the same guarded transaction, so no initialized
//! Ledger is externally visible without its exact first-round context.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{
    ControlPlaneEventId, DebugSessionId, Instant, ProbeRoundId, RequestId, Sha256Digest,
    SystemActorId, WorkspaceRevision,
};
use winwincode_execution_port::{
    debug_hypothesis_ledger::{
        DebugHypothesisLedgerError, DebugHypothesisLedgerReducer,
        ValidatedDebugHypothesisRoundEvidence, canonical_debug_hypothesis_round_evidence_bytes,
        derive_debug_hypothesis_ledger_digest, derive_debug_hypothesis_round_evidence_digest,
        reopen_debug_hypothesis_round_evidence,
    },
    debug_probe_contract::{ValidatedDebugProbePlan, seal_debug_probe_plan},
    debug_probe_delta_context::{
        ValidatedDebugProbeDeltaContext, derive_debug_probe_delta_context_digest,
        reopen_debug_probe_delta_context, validate_debug_probe_delta_context_evidence,
    },
    generated::{
        DebugHypothesisLedger, DebugHypothesisLedgerEvent, DebugHypothesisLedgerSeed,
        DebugHypothesisLedgerUpdate, DebugHypothesisRoundEvidence, DebugProbeDeltaContext,
        DebugProbePlan, DebugProbeRoundAuthority, ExecutionJob, ExecutionScope,
        ExecutionWorkspaceWriteMode,
    },
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, CommitReceipt,
    ExecutionAuthorityCommitGuard, ExecutionJobRecord, ExecutionLeaseRecord,
    LoadedAggregateJournal, NewOutboxEvent, ProductStateStorage, ProjectionEventCursor,
    ProjectionEventStream, PublicEventActor, PublicEventScope, PublicEventSource, ReceiptIdentity,
    StateCommit, StorageError, StorageErrorKind, StoredState, public_receipt_identity,
};

const STATE_SCHEMA_VERSION: u8 = 1;
const JOURNAL_AGGREGATE_TYPE: &str = "debug-hypothesis-ledger";
const PREPARED_TOPIC: &str = "debug.hypothesis.round.prepared.v1";
const LEDGER_TOPIC: &str = "debug.hypothesis.ledger.changed.v1";
const SYSTEM_ACTOR_ID: &str = "sys_00000000000000000000000000";
const REQUEST_ID_DOMAIN: &[u8] = b"winwincode.debug-hypothesis-ledger.request.v1\0";
const EVENT_ID_DOMAIN: &[u8] = b"winwincode.debug-hypothesis-ledger.event.v1\0";
const RECORD_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-hypothesis-ledger.journal-record.v1\0";
const COMMAND_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-hypothesis-ledger.command.v1\0";

/// Stable failure categories for the durable Ledger transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DebugHypothesisLedgerTransactionErrorKind {
    InvalidInput,
    RequestConflict,
    StaleAuthority,
    InvalidState,
    CorruptJournal,
    Storage,
}

/// A bounded failure that never includes context or model text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugHypothesisLedgerTransactionError {
    kind: DebugHypothesisLedgerTransactionErrorKind,
    message: &'static str,
}

impl DebugHypothesisLedgerTransactionError {
    #[must_use]
    pub const fn kind(&self) -> DebugHypothesisLedgerTransactionErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for DebugHypothesisLedgerTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for DebugHypothesisLedgerTransactionError {}

/// Exact durable result of one round preparation.
#[derive(Clone, Debug)]
pub struct PreparedDebugHypothesisRound {
    context_bytes: Vec<u8>,
    context_digest: Sha256Digest,
    evidence_cut_digest: Sha256Digest,
    receipt: CommitReceipt,
}

impl PreparedDebugHypothesisRound {
    #[must_use]
    pub fn context_bytes(&self) -> &[u8] {
        &self.context_bytes
    }

    #[must_use]
    pub const fn context_digest(&self) -> &Sha256Digest {
        &self.context_digest
    }

    #[must_use]
    pub const fn evidence_cut_digest(&self) -> &Sha256Digest {
        &self.evidence_cut_digest
    }

    #[must_use]
    pub const fn receipt(&self) -> &CommitReceipt {
        &self.receipt
    }
}

/// Exact durable result of one initialized or applied Ledger projection.
#[derive(Clone, Debug)]
pub struct CommittedDebugHypothesisRound {
    event: DebugHypothesisLedgerEvent,
    ledger: DebugHypothesisLedger,
    receipt: CommitReceipt,
    cursor: ProjectionEventCursor,
}

impl CommittedDebugHypothesisRound {
    #[must_use]
    pub const fn event(&self) -> &DebugHypothesisLedgerEvent {
        &self.event
    }

    #[must_use]
    pub const fn ledger(&self) -> &DebugHypothesisLedger {
        &self.ledger
    }

    #[must_use]
    pub const fn receipt(&self) -> &CommitReceipt {
        &self.receipt
    }

    #[must_use]
    pub const fn cursor(&self) -> &ProjectionEventCursor {
        &self.cursor
    }
}

/// Recovered current state. Prepared bytes are returned exactly as committed;
/// committed projections are rebuilt through the pure Ledger reducer.
#[derive(Clone, Debug)]
pub enum RecoveredDebugHypothesisLedger {
    Prepared {
        ledger: DebugHypothesisLedger,
        context_bytes: Vec<u8>,
        context_digest: Sha256Digest,
        evidence_cut_digest: Sha256Digest,
    },
    Committed {
        ledger: DebugHypothesisLedger,
        latest_context_digest: Sha256Digest,
    },
}

/// Unique durable owner of one debug session's hypothesis Ledger.
pub struct DebugHypothesisLedgerService<'storage> {
    storage: &'storage mut dyn ProductStateStorage,
}

impl<'storage> DebugHypothesisLedgerService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut dyn ProductStateStorage) -> Self {
        Self { storage }
    }

    /// Persists one exact model context intent before the model may be called.
    ///
    /// `seed` is required only when the session has no durable state. Its
    /// canonical initialization event and projection are committed atomically
    /// with the first `Prepared` record.
    ///
    /// # Errors
    ///
    /// Rejects changed receipt bodies, stale execution authority, invalid
    /// context/evidence bindings, or an inconsistent durable journal.
    #[allow(clippy::too_many_lines)]
    pub fn prepare_round(
        &mut self,
        seed: Option<DebugHypothesisLedgerSeed>,
        plan: &ValidatedDebugProbePlan,
        evidence: &ValidatedDebugHypothesisRoundEvidence,
        context: &ValidatedDebugProbeDeltaContext,
        guard: &ExecutionAuthorityCommitGuard,
    ) -> Result<PreparedDebugHypothesisRound, DebugHypothesisLedgerTransactionError> {
        let initialized = seed
            .map(DebugHypothesisLedgerReducer::initialize)
            .transpose()
            .map_err(map_ledger_error)?;
        let initialized_event = initialized
            .as_ref()
            .map(|reducer| reducer.events()[0].event().clone());
        let authority = context.context().authority.clone();
        validate_round_inputs(plan, evidence, context, guard, initialized.as_ref())?;

        let authority_snapshot = ExecutionAuthoritySnapshot::from_guard(guard);
        let record = PreparedJournalRecord {
            schema_version: STATE_SCHEMA_VERSION,
            initialized_event,
            plan: plan.plan().clone(),
            evidence: evidence.cut().clone(),
            context_bytes: context.canonical_bytes().to_vec(),
            context_digest: context.context().context_digest.clone(),
            policy_digest: context.context().budget.policy_digest.clone(),
            authority: authority_snapshot.clone(),
        };
        let record_bytes = canonical_json(&JournalRecord::Prepared(record.clone()))?;
        let command_digest = command_digest(b"prepare", &record_bytes);
        let scope = public_scope(guard);
        let identity = phase_identity(&scope, b"prepare", &authority)?;

        match self.storage.load_receipt(&identity, &command_digest) {
            Ok(Some(receipt)) => return self.replay_prepared(receipt),
            Ok(None) => {}
            Err(error) if error.kind() == StorageErrorKind::RequestConflict => {
                return Err(request_conflict());
            }
            Err(error) => return Err(map_storage_error(error)),
        }

        let stream_id = stream_id(&authority.debug_session_id);
        let journal_key = journal_key(&authority.debug_session_id)?;
        let current = self.load_current(&stream_id, &journal_key)?;
        let (expected_revision, next_sequence, reducer, publication) = match current {
            None => {
                let reducer = initialized.ok_or_else(|| {
                    invalid_state("the first Ledger round requires one canonical seed")
                })?;
                validate_context_cursor(context.context(), reducer.ledger().ledger(), None)?;
                let digest = journal_record_digest(&record_bytes);
                let publication = AggregateJournalPublication::Create {
                    key: journal_key.clone(),
                    manifest: canonical_json(&JournalManifest::new(&authority, guard))?,
                    first_record: AggregateJournalRecord::new(1, digest.0, record_bytes.clone()),
                };
                (0, 1, reducer, publication)
            }
            Some(current) => {
                if initialized.is_some() {
                    return Err(invalid_state(
                        "a durable Ledger cannot be initialized a second time",
                    ));
                }
                let DurableLedgerState::Committed {
                    ledger,
                    latest_context_digest,
                    latest_context_ledger_digest,
                    journal_tail_sequence,
                    journal_tail_digest,
                    ..
                } = current.state
                else {
                    return Err(invalid_state("another Ledger round is already prepared"));
                };
                validate_context_cursor(
                    context.context(),
                    &ledger,
                    Some((&latest_context_digest, &latest_context_ledger_digest)),
                )?;
                let publication = AggregateJournalPublication::Append {
                    key: journal_key.clone(),
                    expected_tail_sequence: journal_tail_sequence,
                    expected_tail_digest: journal_tail_digest.0,
                    record: AggregateJournalRecord::new(
                        journal_tail_sequence + 1,
                        journal_record_digest(&record_bytes).0,
                        record_bytes.clone(),
                    ),
                };
                (
                    current.revision,
                    journal_tail_sequence + 1,
                    current.reducer,
                    publication,
                )
            }
        };
        if reducer.ledger().ledger().ledger_digest != context.context().ledger_digest
            || reducer.ledger().ledger().last_event_digest != context.context().source_event_digest
        {
            return Err(invalid_input(
                "Prepared context does not name the current durable Ledger",
            ));
        }

        let journal_digest = journal_record_digest(&record_bytes);
        let state = DurableLedgerState::Prepared {
            schema_version: STATE_SCHEMA_VERSION,
            debug_session_id: authority.debug_session_id.clone(),
            ledger: reducer.ledger().ledger().clone(),
            prepared_round_id: authority.round_id.clone(),
            context_digest: record.context_digest.clone(),
            evidence_cut_digest: record.evidence.evidence_cut_digest.clone(),
            journal_tail_sequence: next_sequence,
            journal_tail_digest: journal_digest.clone(),
            authority: authority_snapshot,
        };
        let pointer = PreparedOutboxPointer {
            schema_version: STATE_SCHEMA_VERSION,
            debug_session_id: authority.debug_session_id.clone(),
            round_id: authority.round_id.clone(),
            journal_sequence: next_sequence,
            journal_digest: journal_digest.clone(),
            context_digest: record.context_digest,
            evidence_cut_digest: record.evidence.evidence_cut_digest,
        };
        let mut events = Vec::with_capacity(2);
        if let Some(initialized) = record.initialized_event.as_ref() {
            events.push(public_ledger_event(
                &scope,
                guard,
                initialized,
                reducer.ledger().ledger(),
            )?);
        }
        events.push(NewOutboxEvent::internal(
            internal_event_id(b"prepared", &record_bytes),
            PREPARED_TOPIC,
            canonical_json(&pointer)?,
        ));
        let commit = StateCommit::new(
            identity,
            command_digest,
            stream_id,
            expected_revision,
            canonical_json(&state)?,
            events,
        )
        .with_journal_publication(publication);
        let receipt = match self
            .storage
            .commit_with_execution_authority_guard(&commit, guard)
        {
            Ok(receipt) => receipt,
            Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {
                match self
                    .storage
                    .load_receipt(&commit.receipt_identity, &commit.command_digest)
                {
                    Ok(Some(receipt)) => receipt,
                    Ok(None) => return Err(stale_authority()),
                    Err(error) if error.kind() == StorageErrorKind::RequestConflict => {
                        return Err(request_conflict());
                    }
                    Err(error) => return Err(map_storage_error(error)),
                }
            }
            Err(error) if error.kind() == StorageErrorKind::RequestConflict => {
                return Err(request_conflict());
            }
            Err(error) => return Err(map_storage_error(error)),
        };
        self.replay_prepared(receipt)
    }

    /// Applies one exact model update to the prepared round and atomically
    /// publishes the resulting generated Ledger projection.
    ///
    /// # Errors
    ///
    /// Rejects a missing/foreign Prepared intent, changed receipt bodies,
    /// stale execution authority, invalid model updates, or storage failure.
    #[allow(clippy::too_many_lines)]
    pub fn commit_round(
        &mut self,
        update: DebugHypothesisLedgerUpdate,
        guard: &ExecutionAuthorityCommitGuard,
    ) -> Result<CommittedDebugHypothesisRound, DebugHypothesisLedgerTransactionError> {
        let authority = update.source_round_receipt.authority.clone();
        let authority_snapshot = ExecutionAuthoritySnapshot::from_guard(guard);
        let body = CommitCommandBody {
            schema_version: STATE_SCHEMA_VERSION,
            update: update.clone(),
            authority: authority_snapshot.clone(),
        };
        let body_bytes = canonical_json(&body)?;
        let command_digest = command_digest(b"commit", &body_bytes);
        let scope = public_scope(guard);
        let identity = phase_identity(&scope, b"commit", &authority)?;
        match self.storage.load_receipt(&identity, &command_digest) {
            Ok(Some(receipt)) => return self.replay_committed(receipt),
            Ok(None) => {}
            Err(error) if error.kind() == StorageErrorKind::RequestConflict => {
                return Err(request_conflict());
            }
            Err(error) => return Err(map_storage_error(error)),
        }

        let stream_id = stream_id(&authority.debug_session_id);
        let journal_key = journal_key(&authority.debug_session_id)?;
        let current = self
            .load_current(&stream_id, &journal_key)?
            .ok_or_else(|| invalid_state("Ledger commit has no Prepared state"))?;
        let DurableLedgerState::Prepared {
            ledger,
            prepared_round_id,
            context_digest,
            evidence_cut_digest,
            journal_tail_sequence,
            journal_tail_digest,
            authority: prepared_authority,
            ..
        } = &current.state
        else {
            return Err(invalid_state("Ledger commit has no Prepared state"));
        };
        if current.latest_context.request_digest != update.source_request_digest {
            return Err(invalid_input(
                "Ledger update does not match the exact Prepared model request",
            ));
        }
        if prepared_round_id != &authority.round_id
            || prepared_authority != &authority_snapshot
            || context_digest != &update.source_context_digest
            || evidence_cut_digest != &current.latest_evidence.cut().evidence_cut_digest
            || current.latest_evidence.receipt_reference() != &update.source_round_receipt
            || ledger != current.reducer.ledger().ledger()
        {
            return Err(stale_authority());
        }
        let mut reducer = current.reducer;
        let outcome = reducer
            .apply_round(update, &current.latest_evidence, &authority)
            .map_err(map_ledger_error)?;
        if outcome.is_duplicate() {
            return Err(invalid_state(
                "a duplicate Ledger event has no durable command receipt",
            ));
        }
        let event = outcome.event().event().clone();
        let projection = reducer.ledger().ledger().clone();
        let record = JournalRecord::Committed(CommittedJournalRecord {
            schema_version: STATE_SCHEMA_VERSION,
            event: event.clone(),
        });
        let record_bytes = canonical_json(&record)?;
        let record_digest = journal_record_digest(&record_bytes);
        let next_sequence = journal_tail_sequence + 1;
        let state = DurableLedgerState::Committed {
            schema_version: STATE_SCHEMA_VERSION,
            debug_session_id: authority.debug_session_id.clone(),
            ledger: projection.clone(),
            latest_context_digest: context_digest.clone(),
            latest_context_ledger_digest: current.latest_context.ledger_digest,
            journal_tail_sequence: next_sequence,
            journal_tail_digest: record_digest.clone(),
            authority: authority_snapshot,
        };
        let event = public_ledger_event(&scope, guard, &event, &projection)?;
        let publication = AggregateJournalPublication::Append {
            key: journal_key,
            expected_tail_sequence: *journal_tail_sequence,
            expected_tail_digest: journal_tail_digest.0.clone(),
            record: AggregateJournalRecord::new(next_sequence, record_digest.0, record_bytes),
        };
        let commit = StateCommit::new(
            identity,
            command_digest,
            stream_id,
            current.revision,
            canonical_json(&state)?,
            vec![event],
        )
        .with_journal_publication(publication);
        let receipt = match self
            .storage
            .commit_with_execution_authority_guard(&commit, guard)
        {
            Ok(receipt) => receipt,
            Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {
                match self
                    .storage
                    .load_receipt(&commit.receipt_identity, &commit.command_digest)
                {
                    Ok(Some(receipt)) => receipt,
                    Ok(None) => return Err(stale_authority()),
                    Err(error) if error.kind() == StorageErrorKind::RequestConflict => {
                        return Err(request_conflict());
                    }
                    Err(error) => return Err(map_storage_error(error)),
                }
            }
            Err(error) if error.kind() == StorageErrorKind::RequestConflict => {
                return Err(request_conflict());
            }
            Err(error) => return Err(map_storage_error(error)),
        };
        self.replay_committed(receipt)
    }

    /// Rebuilds the current state from exact journal bytes without creating a
    /// new receipt, event, projection, or model request.
    ///
    /// # Errors
    ///
    /// Rejects missing journal halves, changed digests, invalid canonical
    /// bytes, or an event chain that does not reproduce the stored projection.
    pub fn recover(
        &self,
        debug_session_id: &DebugSessionId,
    ) -> Result<Option<RecoveredDebugHypothesisLedger>, DebugHypothesisLedgerTransactionError> {
        let stream_id = stream_id(debug_session_id);
        let journal_key = journal_key(debug_session_id)?;
        let Some(current) = self.load_current(&stream_id, &journal_key)? else {
            return Ok(None);
        };
        match current.state {
            DurableLedgerState::Prepared {
                ledger,
                context_digest,
                evidence_cut_digest,
                ..
            } => Ok(Some(RecoveredDebugHypothesisLedger::Prepared {
                ledger,
                context_bytes: current.latest_context.bytes,
                context_digest,
                evidence_cut_digest,
            })),
            DurableLedgerState::Committed {
                ledger,
                latest_context_digest,
                ..
            } => Ok(Some(RecoveredDebugHypothesisLedger::Committed {
                ledger,
                latest_context_digest,
            })),
        }
    }

    fn replay_prepared(
        &self,
        receipt: CommitReceipt,
    ) -> Result<PreparedDebugHypothesisRound, DebugHypothesisLedgerTransactionError> {
        let pointer = receipt
            .events
            .iter()
            .find(|event| event.topic == PREPARED_TOPIC)
            .ok_or_else(|| corrupt_journal("Prepared receipt has no intent event"))?;
        let pointer = decode_canonical::<PreparedOutboxPointer>(&pointer.payload)?;
        if receipt.stream_id != stream_id(&pointer.debug_session_id)
            || receipt.revision != pointer.journal_sequence
        {
            return Err(corrupt_journal(
                "Prepared receipt does not match its journal position",
            ));
        }
        let key = journal_key(&pointer.debug_session_id)?;
        let journal = self
            .storage
            .load_journal(&key)
            .map_err(map_storage_error)?
            .ok_or_else(|| corrupt_journal("Prepared journal is missing"))?;
        let history = replay_history(&journal, Some(pointer.journal_sequence))?;
        let JournalRecord::Prepared(record) = history.latest_record else {
            return Err(corrupt_journal(
                "Prepared receipt names a non-Prepared journal record",
            ));
        };
        if history.latest_digest != pointer.journal_digest
            || record.context_digest != pointer.context_digest
            || record.evidence.evidence_cut_digest != pointer.evidence_cut_digest
        {
            return Err(corrupt_journal(
                "Prepared receipt pointer differs from the durable journal",
            ));
        }
        Ok(PreparedDebugHypothesisRound {
            context_bytes: record.context_bytes,
            context_digest: record.context_digest,
            evidence_cut_digest: record.evidence.evidence_cut_digest,
            receipt,
        })
    }

    fn replay_committed(
        &self,
        receipt: CommitReceipt,
    ) -> Result<CommittedDebugHypothesisRound, DebugHypothesisLedgerTransactionError> {
        let projection_event = receipt
            .events
            .iter()
            .find(|event| event.topic == LEDGER_TOPIC)
            .ok_or_else(|| corrupt_journal("Ledger receipt has no projection event"))?;
        let ledger = decode_canonical::<DebugHypothesisLedger>(&projection_event.payload)?;
        let cursor = projection_event
            .projection_cursor
            .clone()
            .ok_or_else(|| corrupt_journal("Ledger projection has no durable cursor"))?;
        let debug_session_id = ledger.authority.debug_session_id.clone();
        if receipt.stream_id != stream_id(&debug_session_id) {
            return Err(corrupt_journal(
                "Ledger receipt belongs to another state stream",
            ));
        }
        let journal = self
            .storage
            .load_journal(&journal_key(&debug_session_id)?)
            .map_err(map_storage_error)?
            .ok_or_else(|| corrupt_journal("Ledger journal is missing"))?;
        let history = replay_history(&journal, Some(receipt.revision))?;
        let JournalRecord::Committed(record) = history.latest_record else {
            return Err(corrupt_journal(
                "Ledger receipt names a non-Committed journal record",
            ));
        };
        if history.reducer.ledger().ledger() != &ledger
            || record.event.resulting_ledger_digest != ledger.ledger_digest
        {
            return Err(corrupt_journal(
                "Ledger projection differs from its applied event chain",
            ));
        }
        Ok(CommittedDebugHypothesisRound {
            event: record.event,
            ledger,
            receipt,
            cursor,
        })
    }

    fn load_current(
        &self,
        stream_id: &str,
        journal_key: &AggregateJournalKey,
    ) -> Result<Option<CurrentLedger>, DebugHypothesisLedgerTransactionError> {
        let state = self
            .storage
            .load_state(stream_id)
            .map_err(map_storage_error)?;
        let journal = self
            .storage
            .load_journal(journal_key)
            .map_err(map_storage_error)?;
        let (state, journal) = match (state, journal) {
            (None, None) => return Ok(None),
            (Some(state), Some(journal)) => (state, journal),
            _ => {
                return Err(corrupt_journal(
                    "Ledger state and aggregate journal are incomplete",
                ));
            }
        };
        let durable = decode_canonical::<DurableLedgerState>(&state.payload)?;
        let history = replay_history(&journal, Some(state.revision))?;
        validate_current_state(&state, &durable, &history)?;
        Ok(Some(CurrentLedger {
            revision: state.revision,
            state: durable,
            reducer: history.reducer,
            latest_evidence: history.latest_evidence,
            latest_context: history.latest_context,
        }))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum DurableLedgerState {
    Prepared {
        schema_version: u8,
        debug_session_id: DebugSessionId,
        ledger: DebugHypothesisLedger,
        prepared_round_id: ProbeRoundId,
        context_digest: Sha256Digest,
        evidence_cut_digest: Sha256Digest,
        journal_tail_sequence: u64,
        journal_tail_digest: Sha256Digest,
        authority: ExecutionAuthoritySnapshot,
    },
    Committed {
        schema_version: u8,
        debug_session_id: DebugSessionId,
        ledger: DebugHypothesisLedger,
        latest_context_digest: Sha256Digest,
        latest_context_ledger_digest: Sha256Digest,
        journal_tail_sequence: u64,
        journal_tail_digest: Sha256Digest,
        authority: ExecutionAuthoritySnapshot,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalManifest {
    schema_version: u8,
    debug_session_id: DebugSessionId,
    product_session_id: winwincode_domain::ProductSessionId,
    repository_id: winwincode_domain::RepositoryId,
}

impl JournalManifest {
    fn new(authority: &DebugProbeRoundAuthority, guard: &ExecutionAuthorityCommitGuard) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            debug_session_id: authority.debug_session_id.clone(),
            product_session_id: guard.expected_job().scope.product_session_id.clone(),
            repository_id: guard.expected_job().scope.repository_id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
enum JournalRecord {
    Prepared(PreparedJournalRecord),
    Committed(CommittedJournalRecord),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedJournalRecord {
    schema_version: u8,
    initialized_event: Option<DebugHypothesisLedgerEvent>,
    plan: DebugProbePlan,
    evidence: DebugHypothesisRoundEvidence,
    context_bytes: Vec<u8>,
    context_digest: Sha256Digest,
    policy_digest: Sha256Digest,
    authority: ExecutionAuthoritySnapshot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommittedJournalRecord {
    schema_version: u8,
    event: DebugHypothesisLedgerEvent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionAuthoritySnapshot {
    job: ExecutionJobRecord,
    lease: ExecutionLeaseRecord,
    worker_session_id: winwincode_domain::WorkerSessionId,
    dispatch_request_id: RequestId,
    accepted_at: Instant,
}

impl ExecutionAuthoritySnapshot {
    fn from_guard(guard: &ExecutionAuthorityCommitGuard) -> Self {
        let dispatch = guard.expected_dispatch();
        Self {
            job: guard.expected_job().clone(),
            lease: dispatch.lease().clone(),
            worker_session_id: dispatch.worker_session_id().clone(),
            dispatch_request_id: dispatch.dispatch_request_id().clone(),
            accepted_at: dispatch.accepted_at().clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedOutboxPointer {
    schema_version: u8,
    debug_session_id: DebugSessionId,
    round_id: ProbeRoundId,
    journal_sequence: u64,
    journal_digest: Sha256Digest,
    context_digest: Sha256Digest,
    evidence_cut_digest: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitCommandBody {
    schema_version: u8,
    update: DebugHypothesisLedgerUpdate,
    authority: ExecutionAuthoritySnapshot,
}

struct CurrentLedger {
    revision: u64,
    state: DurableLedgerState,
    reducer: DebugHypothesisLedgerReducer,
    latest_evidence: ValidatedDebugHypothesisRoundEvidence,
    latest_context: ReplayedContext,
}

struct ReplayedHistory {
    reducer: DebugHypothesisLedgerReducer,
    latest_record: JournalRecord,
    latest_digest: Sha256Digest,
    latest_evidence: ValidatedDebugHypothesisRoundEvidence,
    latest_context: ReplayedContext,
    latest_authority: ExecutionAuthoritySnapshot,
}

#[derive(Clone)]
struct ReplayedContext {
    bytes: Vec<u8>,
    digest: Sha256Digest,
    ledger_digest: Sha256Digest,
    request_digest: Sha256Digest,
}

#[allow(clippy::too_many_lines)]
fn replay_history(
    journal: &LoadedAggregateJournal,
    through_sequence: Option<u64>,
) -> Result<ReplayedHistory, DebugHypothesisLedgerTransactionError> {
    let manifest = decode_canonical::<JournalManifest>(&journal.manifest)?;
    if manifest.schema_version != STATE_SCHEMA_VERSION {
        return Err(corrupt_journal(
            "Ledger journal manifest version is invalid",
        ));
    }
    let through = through_sequence.unwrap_or(u64::MAX);
    let records = journal
        .records
        .iter()
        .take_while(|record| record.sequence <= through)
        .collect::<Vec<_>>();
    if (records.is_empty() || records.last().map(|record| record.sequence) != through_sequence)
        && through_sequence.is_some()
    {
        return Err(corrupt_journal("Ledger journal position is unavailable"));
    }
    let mut events = Vec::new();
    let mut evidence = Vec::new();
    let mut previous_phase_committed = true;
    let mut latest_context: Option<ReplayedContext> = None;
    let mut latest_evidence = None;
    let mut latest_authority = None;
    let mut latest_record = None;
    let mut latest_digest = None;

    for (index, stored) in records.into_iter().enumerate() {
        let expected_sequence = u64::try_from(index + 1)
            .map_err(|_| corrupt_journal("Ledger journal sequence overflowed"))?;
        if stored.sequence != expected_sequence
            || stored.digest != journal_record_digest(&stored.payload).0
        {
            return Err(corrupt_journal(
                "Ledger journal sequence or digest is invalid",
            ));
        }
        let record = decode_canonical::<JournalRecord>(&stored.payload)?;
        match &record {
            JournalRecord::Prepared(prepared) => {
                if !previous_phase_committed || prepared.schema_version != STATE_SCHEMA_VERSION {
                    return Err(corrupt_journal("Ledger journal phase order is invalid"));
                }
                let plan = seal_debug_probe_plan(prepared.plan.clone(), &prepared.plan.authority)
                    .map_err(|_| corrupt_journal("Prepared plan is invalid"))?;
                let evidence_bytes = serde_json::to_vec(&prepared.evidence)
                    .map_err(|_| corrupt_journal("Prepared evidence cannot be encoded"))?;
                let reopened = reopen_debug_hypothesis_round_evidence(
                    &evidence_bytes,
                    &plan,
                    &prepared.evidence.evidence_cut_digest,
                )
                .map_err(map_ledger_error)?;
                if canonical_debug_hypothesis_round_evidence_bytes(&reopened) != evidence_bytes
                    || derive_debug_hypothesis_round_evidence_digest(reopened.cut())
                        .map_err(map_ledger_error)?
                        != prepared.evidence.evidence_cut_digest
                {
                    return Err(corrupt_journal("Prepared evidence is not canonical"));
                }
                let reopened_context = reopen_debug_probe_delta_context(
                    &prepared.context_bytes,
                    &prepared.context_digest,
                )
                .map_err(|_| corrupt_journal("Prepared context bytes are invalid"))?;
                validate_debug_probe_delta_context_evidence(&reopened_context, &reopened)
                    .map_err(|_| corrupt_journal("Prepared context evidence binding is invalid"))?;
                let context = reopened_context.context();
                if context.budget.policy_digest != prepared.policy_digest
                    || context.source_round_receipt != *reopened.receipt_reference()
                {
                    return Err(corrupt_journal("Prepared context binding is invalid"));
                }
                validate_authority_snapshot(&context.authority, &prepared.authority)
                    .map_err(|_| corrupt_journal("Prepared authority binding is invalid"))?;
                if let Some(initialized) = prepared.initialized_event.as_ref() {
                    if index != 0 || !events.is_empty() {
                        return Err(corrupt_journal(
                            "Ledger initialization is not the first durable fact",
                        ));
                    }
                    events.push(initialized.clone());
                } else if events.is_empty() {
                    return Err(corrupt_journal(
                        "Ledger journal has no initialization event",
                    ));
                }
                let reducer = DebugHypothesisLedgerReducer::replay(&events, &evidence)
                    .map_err(map_ledger_error)?;
                validate_context_cursor(
                    context,
                    reducer.ledger().ledger(),
                    latest_context
                        .as_ref()
                        .map(|value| (&value.digest, &value.ledger_digest)),
                )?;
                evidence.push(reopened.clone());
                latest_evidence = Some(reopened);
                latest_authority = Some(prepared.authority.clone());
                latest_context = Some(ReplayedContext {
                    bytes: prepared.context_bytes.clone(),
                    digest: prepared.context_digest.clone(),
                    ledger_digest: context.ledger_digest.clone(),
                    request_digest: context.source_request_digest.clone(),
                });
                previous_phase_committed = false;
            }
            JournalRecord::Committed(committed) => {
                if previous_phase_committed || committed.schema_version != STATE_SCHEMA_VERSION {
                    return Err(corrupt_journal("Ledger journal phase order is invalid"));
                }
                events.push(committed.event.clone());
                DebugHypothesisLedgerReducer::replay(&events, &evidence)
                    .map_err(map_ledger_error)?;
                previous_phase_committed = true;
            }
        }
        latest_digest = Some(Sha256Digest(stored.digest.clone()));
        latest_record = Some(record);
    }
    let reducer =
        DebugHypothesisLedgerReducer::replay(&events, &evidence).map_err(map_ledger_error)?;
    Ok(ReplayedHistory {
        reducer,
        latest_record: latest_record
            .ok_or_else(|| corrupt_journal("Ledger journal has no records"))?,
        latest_digest: latest_digest
            .ok_or_else(|| corrupt_journal("Ledger journal has no digest"))?,
        latest_evidence: latest_evidence
            .ok_or_else(|| corrupt_journal("Ledger journal has no evidence cut"))?,
        latest_context: latest_context
            .ok_or_else(|| corrupt_journal("Ledger journal has no context"))?,
        latest_authority: latest_authority
            .ok_or_else(|| corrupt_journal("Ledger journal has no execution authority"))?,
    })
}

fn validate_current_state(
    stored: &StoredState,
    state: &DurableLedgerState,
    history: &ReplayedHistory,
) -> Result<(), DebugHypothesisLedgerTransactionError> {
    let (schema, session, ledger, sequence, digest, authority, prepared) = match state {
        DurableLedgerState::Prepared {
            schema_version,
            debug_session_id,
            ledger,
            prepared_round_id,
            context_digest,
            evidence_cut_digest,
            journal_tail_sequence,
            journal_tail_digest,
            authority,
            ..
        } => {
            let JournalRecord::Prepared(record) = &history.latest_record else {
                return Err(corrupt_journal("Prepared state tail is not Prepared"));
            };
            if &record.context_digest != context_digest
                || &record.evidence.evidence_cut_digest != evidence_cut_digest
                || &record.evidence.source_round_receipt.authority.round_id != prepared_round_id
            {
                return Err(corrupt_journal("Prepared state differs from its journal"));
            }
            (
                *schema_version,
                debug_session_id,
                ledger,
                *journal_tail_sequence,
                journal_tail_digest,
                authority,
                true,
            )
        }
        DurableLedgerState::Committed {
            schema_version,
            debug_session_id,
            ledger,
            latest_context_digest,
            latest_context_ledger_digest,
            journal_tail_sequence,
            journal_tail_digest,
            authority,
            ..
        } => {
            if !matches!(history.latest_record, JournalRecord::Committed(_))
                || latest_context_digest != &history.latest_context.digest
                || latest_context_ledger_digest != &history.latest_context.ledger_digest
            {
                return Err(corrupt_journal("Committed state differs from its journal"));
            }
            (
                *schema_version,
                debug_session_id,
                ledger,
                *journal_tail_sequence,
                journal_tail_digest,
                authority,
                false,
            )
        }
    };
    if schema != STATE_SCHEMA_VERSION
        || stored.stream_id != stream_id(session)
        || stored.revision != sequence
        || digest != &history.latest_digest
        || authority != &history.latest_authority
        || ledger != history.reducer.ledger().ledger()
        || derive_debug_hypothesis_ledger_digest(ledger).map_err(map_ledger_error)?
            != ledger.ledger_digest
        || prepared != matches!(history.latest_record, JournalRecord::Prepared(_))
    {
        return Err(corrupt_journal(
            "Ledger state is not the exact journal projection",
        ));
    }
    Ok(())
}

fn validate_round_inputs(
    plan: &ValidatedDebugProbePlan,
    evidence: &ValidatedDebugHypothesisRoundEvidence,
    context: &ValidatedDebugProbeDeltaContext,
    guard: &ExecutionAuthorityCommitGuard,
    initialized: Option<&DebugHypothesisLedgerReducer>,
) -> Result<(), DebugHypothesisLedgerTransactionError> {
    let authority = &context.context().authority;
    validate_debug_probe_delta_context_evidence(context, evidence)
        .map_err(|_| invalid_input("Prepared context does not match the exact evidence cut"))?;
    if &plan.plan().authority != authority
        || &evidence.cut().source_round_receipt.authority != authority
        || &context.context().source_round_receipt != evidence.receipt_reference()
        || context.context().source_request_digest.0.is_empty()
        || context.context().context_digest
            != derive_debug_probe_delta_context_digest(context.context())
                .map_err(|_| invalid_input("Prepared context digest is invalid"))?
    {
        return Err(invalid_input(
            "Prepared plan, evidence and context do not share one round",
        ));
    }
    validate_execution_authority(authority, guard)?;
    if let Some(reducer) = initialized {
        validate_context_cursor(context.context(), reducer.ledger().ledger(), None)?;
    }
    Ok(())
}

fn validate_context_cursor(
    context: &DebugProbeDeltaContext,
    current: &DebugHypothesisLedger,
    previous: Option<(&Sha256Digest, &Sha256Digest)>,
) -> Result<(), DebugHypothesisLedgerTransactionError> {
    let expected_previous_context = previous.map(|(context, _)| context);
    let expected_previous_ledger = previous.map(|(_, ledger)| ledger);
    if context.ledger_digest != current.ledger_digest
        || context.source_event_digest != current.last_event_digest
        || context.previous_context_digest.as_ref() != expected_previous_context
        || context.previous_ledger_digest.as_ref() != expected_previous_ledger
    {
        return Err(invalid_input(
            "Prepared context cursor does not match the durable Ledger",
        ));
    }
    Ok(())
}

fn validate_execution_authority(
    authority: &DebugProbeRoundAuthority,
    guard: &ExecutionAuthorityCommitGuard,
) -> Result<(), DebugHypothesisLedgerTransactionError> {
    validate_authority_snapshot(authority, &ExecutionAuthoritySnapshot::from_guard(guard))
}

fn validate_authority_snapshot(
    authority: &DebugProbeRoundAuthority,
    snapshot: &ExecutionAuthoritySnapshot,
) -> Result<(), DebugHypothesisLedgerTransactionError> {
    let job = &snapshot.job;
    let lease = &snapshot.lease;
    let attempt = u64::try_from(authority.attempt)
        .map_err(|_| invalid_input("Debug authority attempt is invalid"))?;
    let wire_job = serde_json::from_slice::<ExecutionJob>(&job.dispatch_payload)
        .map_err(|_| invalid_input("Execution Job payload is invalid"))?;
    let canonical_job = serde_json::to_vec(&wire_job)
        .map_err(|_| invalid_input("Execution Job payload cannot be encoded"))?;
    let workspace_revision =
        WorkspaceRevision(format!("git-tree:{}", wire_job.workspace.checkout_revision));
    if canonical_job != job.dispatch_payload
        || wire_job.job_id != job.job_id
        || wire_job.payload_digest != job.payload_digest
        || !wire_scope_matches_job(&wire_job.scope, job)
        || wire_job.workspace.write_mode != ExecutionWorkspaceWriteMode::ReadOnly
        || authority.job_id != job.job_id
        || authority.job_id != lease.job_id
        || authority.attempt != wire_job.attempt
        || attempt != job.attempt
        || attempt != lease.attempt
        || authority.lease_id != lease.lease_id
        || authority.fencing_token != lease.fencing_token
        || authority.repository_id != job.scope.repository_id
        || authority.repository_id != wire_job.workspace.repository_id
        || authority.session_identity.product_session_id != job.scope.product_session_id
        || authority.session_identity.stage_run_id != job.stage_run_id
        || authority.session_identity.worker_session_id != snapshot.worker_session_id
        || authority.workspace_revision != workspace_revision
    {
        return Err(stale_authority());
    }
    Ok(())
}

fn wire_scope_matches_job(scope: &ExecutionScope, job: &ExecutionJobRecord) -> bool {
    match scope {
        ExecutionScope::DeliveryStageExecutionScope(scope) => {
            job.scope.product_session_id == scope.product_session_id
                && job.scope.delivery_id.as_ref() == Some(&scope.delivery_id)
                && job.stage_run_id.as_ref() == Some(&scope.stage_run_id)
        }
        ExecutionScope::ProductSessionExecutionScope(scope) => {
            job.scope.product_session_id == scope.product_session_id
                && job.scope.delivery_id.is_none()
                && job.stage_run_id.is_none()
        }
    }
}

fn public_ledger_event(
    scope: &PublicEventScope,
    guard: &ExecutionAuthorityCommitGuard,
    event: &DebugHypothesisLedgerEvent,
    ledger: &DebugHypothesisLedger,
) -> Result<NewOutboxEvent, DebugHypothesisLedgerTransactionError> {
    let payload = canonical_json(ledger)?;
    NewOutboxEvent::public_projection(
        public_event_id(&event.event_digest),
        LEDGER_TOPIC,
        payload,
        ProjectionEventStream::ProductSession(
            guard.expected_job().scope.product_session_id.clone(),
        ),
        scope.clone(),
        event.occurred_at.clone(),
        PublicEventSource::ControlPlane {
            actor: system_actor(),
            component: "debug-hypothesis-ledger".to_owned(),
        },
    )
    .map_err(map_storage_error)
}

fn phase_identity(
    scope: &PublicEventScope,
    phase: &[u8],
    authority: &DebugProbeRoundAuthority,
) -> Result<ReceiptIdentity, DebugHypothesisLedgerTransactionError> {
    public_receipt_identity(
        &system_actor(),
        scope,
        derived_request_id(phase, &authority.debug_session_id, &authority.round_id),
    )
    .map_err(map_storage_error)
}

fn derived_request_id(
    phase: &[u8],
    debug_session_id: &DebugSessionId,
    round_id: &ProbeRoundId,
) -> RequestId {
    let mut digest = Sha256::new();
    digest.update(REQUEST_ID_DOMAIN);
    update_framed(&mut digest, phase);
    update_framed(&mut digest, debug_session_id.0.as_bytes());
    update_framed(&mut digest, round_id.0.as_bytes());
    let encoded = format!("{:X}", digest.finalize());
    RequestId(format!("req_{}", &encoded[..26]))
}

fn public_event_id(digest: &Sha256Digest) -> ControlPlaneEventId {
    let mut value = Sha256::new();
    value.update(EVENT_ID_DOMAIN);
    update_framed(&mut value, digest.0.as_bytes());
    ControlPlaneEventId(format!("evt_{:x}", value.finalize()))
}

fn internal_event_id(phase: &[u8], payload: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(EVENT_ID_DOMAIN);
    update_framed(&mut digest, phase);
    update_framed(&mut digest, payload);
    format!("debug-hypothesis:{:x}", digest.finalize())
}

fn journal_record_digest(payload: &[u8]) -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(RECORD_DIGEST_DOMAIN);
    update_framed(&mut digest, payload);
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

fn command_digest(phase: &[u8], payload: &[u8]) -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(COMMAND_DIGEST_DOMAIN);
    update_framed(&mut digest, phase);
    update_framed(&mut digest, payload);
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

fn update_framed(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn stream_id(debug_session_id: &DebugSessionId) -> String {
    format!("debug-hypothesis-ledger:{}", debug_session_id.0)
}

fn journal_key(
    debug_session_id: &DebugSessionId,
) -> Result<AggregateJournalKey, DebugHypothesisLedgerTransactionError> {
    AggregateJournalKey::new(JOURNAL_AGGREGATE_TYPE, debug_session_id.0.clone())
        .map_err(map_storage_error)
}

fn public_scope(guard: &ExecutionAuthorityCommitGuard) -> PublicEventScope {
    let scope = &guard.expected_job().scope;
    PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    }
}

fn system_actor() -> PublicEventActor {
    PublicEventActor::System {
        id: SystemActorId(SYSTEM_ACTOR_ID.to_owned()),
    }
}

fn canonical_json<T: Serialize>(
    value: &T,
) -> Result<Vec<u8>, DebugHypothesisLedgerTransactionError> {
    serde_json::to_vec(value).map_err(|_| invalid_input("durable Ledger value cannot be encoded"))
}

fn decode_canonical<T>(bytes: &[u8]) -> Result<T, DebugHypothesisLedgerTransactionError>
where
    T: Serialize + for<'de> Deserialize<'de>,
{
    let value = serde_json::from_slice::<T>(bytes)
        .map_err(|_| corrupt_journal("durable Ledger bytes are invalid"))?;
    if canonical_json(&value)? != bytes {
        return Err(corrupt_journal("durable Ledger bytes are not canonical"));
    }
    Ok(value)
}

fn map_storage_error(error: StorageError) -> DebugHypothesisLedgerTransactionError {
    let kind = error.kind();
    drop(error);
    match kind {
        StorageErrorKind::RequestConflict => request_conflict(),
        StorageErrorKind::RevisionConflict => stale_authority(),
        StorageErrorKind::InvalidInput
        | StorageErrorKind::RequestReplayMissing
        | StorageErrorKind::JournalAlreadyExists
        | StorageErrorKind::JournalNotFound
        | StorageErrorKind::JournalConflict => {
            invalid_state("durable Ledger storage rejected the transaction")
        }
        StorageErrorKind::EventCursorExpired
        | StorageErrorKind::Adapter
        | StorageErrorKind::Closed => DebugHypothesisLedgerTransactionError {
            kind: DebugHypothesisLedgerTransactionErrorKind::Storage,
            message: "durable Ledger storage is unavailable",
        },
    }
}

fn map_ledger_error(_error: DebugHypothesisLedgerError) -> DebugHypothesisLedgerTransactionError {
    invalid_input("hypothesis Ledger input or replay is invalid")
}

const fn invalid_input(message: &'static str) -> DebugHypothesisLedgerTransactionError {
    DebugHypothesisLedgerTransactionError {
        kind: DebugHypothesisLedgerTransactionErrorKind::InvalidInput,
        message,
    }
}

const fn invalid_state(message: &'static str) -> DebugHypothesisLedgerTransactionError {
    DebugHypothesisLedgerTransactionError {
        kind: DebugHypothesisLedgerTransactionErrorKind::InvalidState,
        message,
    }
}

const fn request_conflict() -> DebugHypothesisLedgerTransactionError {
    DebugHypothesisLedgerTransactionError {
        kind: DebugHypothesisLedgerTransactionErrorKind::RequestConflict,
        message: "hypothesis Ledger phase identity was reused with different content",
    }
}

const fn stale_authority() -> DebugHypothesisLedgerTransactionError {
    DebugHypothesisLedgerTransactionError {
        kind: DebugHypothesisLedgerTransactionErrorKind::StaleAuthority,
        message: "hypothesis Ledger execution authority is no longer current",
    }
}

const fn corrupt_journal(message: &'static str) -> DebugHypothesisLedgerTransactionError {
    DebugHypothesisLedgerTransactionError {
        kind: DebugHypothesisLedgerTransactionErrorKind::CorruptJournal,
        message,
    }
}
