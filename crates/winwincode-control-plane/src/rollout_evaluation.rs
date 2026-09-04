// SPDX-License-Identifier: Apache-2.0

//! Persistent, one-shot admission for pre-Go evaluation Jobs.
//!
//! Evaluation assignments are frozen before execution and are not rollout
//! decisions. A writer Job may consume one exact delegated assignment even
//! while the production gate is closed. The immutable Job receipt records that
//! consumption, and ordinary production Jobs continue to require a real Go.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_audit::{AuditAction, AuditEventId, AuditState, AuditSubject};
use winwincode_domain::{ExecutionJobId, RepositoryScope, RequestId, Sha256Digest};
use winwincode_execution_port::{
    generated::ExecutionJob,
    performance_evaluation::{EvaluationArmV1, EvaluationAssignmentV1, PerformancePairedSampleV1},
    runtime_trace_outbox::ExecutionMode,
};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity,
    StateCommit, StateMutation, StateRevisionGuard, StorageError, StorageErrorKind, StoredState,
};

use crate::{execution_audit_event_with_state, repository_scope_key};

const STATE_SCHEMA: &str = "winwincode.rollout-evaluation-assignment.v1";
const SLOT_SCHEMA: &str = "winwincode.rollout-evaluation-slot.v1";
const CLAIM_SCHEMA: &str = "winwincode.rollout-evaluation-identity-claim.v1";
const RECEIPT_TOPIC: &str = "rollout.evaluation.assignment.created";
const PAIR_SCHEMA: &str = "winwincode.rollout-evaluation-pair.v1";
const PAIR_RECEIPT_TOPIC: &str = "rollout.evaluation.pair.recorded";
const ACTOR_KEY: &[u8] = b"winwincode.rollout-evaluation.actor.v1";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Idempotent command that freezes one pre-Go evaluation assignment.
#[derive(Clone, Debug)]
pub struct CreateEvaluationAssignment {
    pub scope: RepositoryScope,
    pub request_id: RequestId,
    pub expected_gate_revision: u64,
    pub assignment: EvaluationAssignmentV1,
    pub occurred_at_millis: u64,
}

/// Internal output of the authority join that freezes one complete pair.
#[derive(Clone, Debug)]
pub(crate) struct RecordAuthorizedEvaluationPair {
    pub scope: RepositoryScope,
    pub request_id: RequestId,
    pub expected_gate_revision: u64,
    pub pair: PerformancePairedSampleV1,
    pub occurred_at_millis: u64,
}

/// Opaque pair produced only by the durable authority projection.
#[derive(Clone)]
pub struct ProjectedEvaluationPair {
    pair: PerformancePairedSampleV1,
}

impl fmt::Debug for ProjectedEvaluationPair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectedEvaluationPair")
            .field("digest", self.digest())
            .finish_non_exhaustive()
    }
}

impl ProjectedEvaluationPair {
    pub(crate) fn try_from_authority(
        pair: PerformancePairedSampleV1,
    ) -> Result<Self, RolloutEvaluationError> {
        pair.validate()
            .map_err(|_| RolloutEvaluationError::invalid("projected evaluation pair is invalid"))?;
        Ok(Self { pair })
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        self.pair.digest()
    }
}

/// Narrow command that persists one pair from the durable projection seam.
#[derive(Clone, Debug)]
pub struct RecordProjectedEvaluationPair {
    pub scope: RepositoryScope,
    pub expected_gate_revision: u64,
    pub projected_pair: ProjectedEvaluationPair,
    pub occurred_at_millis: u64,
}

/// Durable creation receipt for one evaluation assignment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvaluationAssignmentReceipt {
    assignment_digest: Sha256Digest,
    replayed: bool,
}

impl EvaluationAssignmentReceipt {
    #[must_use]
    pub const fn assignment_digest(&self) -> &Sha256Digest {
        &self.assignment_digest
    }

    #[must_use]
    pub const fn replayed(&self) -> bool {
        self.replayed
    }
}

/// Stable assignment authority failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RolloutEvaluationErrorKind {
    Invalid,
    RevisionConflict,
    Storage,
    Corrupt,
}

/// Secret-safe evaluation authority failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RolloutEvaluationError {
    kind: RolloutEvaluationErrorKind,
    message: &'static str,
}

impl RolloutEvaluationError {
    const fn invalid(message: &'static str) -> Self {
        Self {
            kind: RolloutEvaluationErrorKind::Invalid,
            message,
        }
    }

    const fn corrupt() -> Self {
        Self {
            kind: RolloutEvaluationErrorKind::Corrupt,
            message: "rollout evaluation state is invalid",
        }
    }

    fn storage(error: &StorageError) -> Self {
        if error.kind() == StorageErrorKind::RevisionConflict {
            Self {
                kind: RolloutEvaluationErrorKind::RevisionConflict,
                message: "rollout evaluation revision changed",
            }
        } else {
            Self {
                kind: RolloutEvaluationErrorKind::Storage,
                message: "rollout evaluation storage is unavailable",
            }
        }
    }

    #[must_use]
    pub const fn kind(&self) -> RolloutEvaluationErrorKind {
        self.kind
    }
}

impl fmt::Display for RolloutEvaluationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for RolloutEvaluationError {}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AssignmentState {
    Active,
    Consumed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SlotState {
    Assigned,
    Consumed,
    Paired,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EvaluationClaimKind {
    Job,
    Run,
}

/// The one durable logical-sample slot admitted by a policy for an arm.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EvaluationSlotKey {
    repository_scope: RepositoryScope,
    policy_revision: u64,
    pair_id: Sha256Digest,
    arm: EvaluationArmV1,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssignmentConsumption {
    job_id: ExecutionJobId,
    job_payload_digest: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredAssignment {
    schema: String,
    revision: u64,
    slot_digest: Sha256Digest,
    assignment: EvaluationAssignmentV1,
    state: AssignmentState,
    consumption: Option<AssignmentConsumption>,
    updated_at_millis: u64,
    digest: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredEvaluationSlot {
    schema: String,
    revision: u64,
    key: EvaluationSlotKey,
    slot_digest: Sha256Digest,
    assignment_digest: Sha256Digest,
    state: SlotState,
    consumption: Option<AssignmentConsumption>,
    authorization_digest: Option<Sha256Digest>,
    pair_digest: Option<Sha256Digest>,
    updated_at_millis: u64,
    digest: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EvaluationIdentityClaimKey {
    repository_scope: RepositoryScope,
    policy_revision: u64,
    kind: EvaluationClaimKind,
    identity: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredEvaluationIdentityClaim {
    schema: String,
    key: EvaluationIdentityClaimKey,
    key_digest: Sha256Digest,
    slot_digest: Sha256Digest,
    assignment_digest: Sha256Digest,
    digest: Sha256Digest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredIdentityClaimDigest<'facts> {
    key: &'facts Sha256Digest,
    slot: &'facts Sha256Digest,
    assignment: &'facts Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssignmentReceiptEvent {
    schema: String,
    assignment: EvaluationAssignmentV1,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredAuthorizedPair {
    schema: String,
    revision: u64,
    pair: PerformancePairedSampleV1,
    recorded_at_millis: u64,
    digest: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PairReceiptEvent {
    schema: String,
    pair: PerformancePairedSampleV1,
}

struct ConsumedAssignmentSlot {
    assignment_stream_id: String,
    assignment_revision: u64,
    slot_stream_id: String,
    slot: StoredEvaluationSlot,
}

/// Exact two-arm assignment authority loaded only from durable slots.
pub(crate) struct ConsumedEvaluationPairAssignments {
    pub(crate) react: EvaluationAssignmentV1,
    pub(crate) delegated: EvaluationAssignmentV1,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AssignmentCommandDigest<'facts> {
    scope: &'facts RepositoryScope,
    expected_gate_revision: u64,
    assignment_digest: &'facts Sha256Digest,
    occurred_at_millis: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredAssignmentDigest<'facts> {
    revision: u64,
    slot_digest: &'facts Sha256Digest,
    assignment_digest: &'facts Sha256Digest,
    state: AssignmentState,
    consumption: &'facts Option<AssignmentConsumption>,
    updated_at_millis: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredSlotDigest<'facts> {
    revision: u64,
    slot_digest: &'facts Sha256Digest,
    assignment_digest: &'facts Sha256Digest,
    state: SlotState,
    consumption: &'facts Option<AssignmentConsumption>,
    authorization_digest: &'facts Option<Sha256Digest>,
    pair_digest: &'facts Option<Sha256Digest>,
    updated_at_millis: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairCommandDigest<'facts> {
    scope: &'facts RepositoryScope,
    expected_gate_revision: u64,
    pair_digest: &'facts Sha256Digest,
    occurred_at_millis: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredPairDigest<'facts> {
    pair_digest: &'facts Sha256Digest,
    recorded_at_millis: u64,
}

/// Persistent assignment service used by the evaluation orchestrator.
pub struct RolloutEvaluationService<'storage> {
    storage: &'storage mut dyn ProductStateStorage,
}

impl<'storage> RolloutEvaluationService<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut dyn ProductStateStorage) -> Self {
        Self { storage }
    }

    /// Freezes an assignment under the exact active rollout policy revision.
    ///
    /// # Errors
    ///
    /// Rejects missing/stale policy, malformed facts, request conflicts, and
    /// an assignment digest already owned by another command.
    pub fn create_assignment(
        &mut self,
        command: CreateEvaluationAssignment,
    ) -> Result<EvaluationAssignmentReceipt, RolloutEvaluationError> {
        validate_create(&command)?;
        let command_digest = digest_json(&AssignmentCommandDigest {
            scope: &command.scope,
            expected_gate_revision: command.expected_gate_revision,
            assignment_digest: command.assignment.digest(),
            occurred_at_millis: command.occurred_at_millis,
        })?;
        let identity = receipt_identity(&command.scope, command.request_id.clone())?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&identity, &command_digest)
            .map_err(|error| RolloutEvaluationError::storage(&error))?
        {
            return decode_creation_receipt(&receipt, &command.assignment, true);
        }
        let Some((gate_stream, gate_revision, policy, statistical_policy)) =
            crate::rollout_gate::active_statistical_policy(self.storage, &command.scope)
                .map_err(|_| RolloutEvaluationError::corrupt())?
        else {
            return Err(RolloutEvaluationError::invalid(
                "evaluation assignment requires an active rollout policy",
            ));
        };
        let spec = command.assignment.spec();
        if gate_revision != command.expected_gate_revision
            || spec.policy_revision != policy.revision()
            || &spec.policy_digest != policy.digest()
        {
            return Err(RolloutEvaluationError {
                kind: RolloutEvaluationErrorKind::RevisionConflict,
                message: "evaluation assignment policy changed",
            });
        }
        statistical_policy
            .authorizes_assignment(&command.assignment)
            .map_err(|_| {
                RolloutEvaluationError::invalid(
                    "evaluation assignment is outside the frozen cohort",
                )
            })?;
        let slot = assigned_slot(&command.assignment, command.occurred_at_millis)?;
        if let Some(existing) = load_slot_optional(self.storage, &slot.slot_digest)? {
            return replay_existing_slot(&existing, &command.assignment);
        }
        let assignment = command.assignment.clone();
        let commit = prepare_assignment_commit(
            command,
            identity,
            command_digest,
            gate_stream,
            gate_revision,
            &slot,
        )?;
        let receipt = match self.storage.commit(&commit) {
            Ok(receipt) => receipt,
            Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {
                let existing = load_slot_optional(self.storage, &slot.slot_digest)?
                    .ok_or_else(|| RolloutEvaluationError::storage(&error))?;
                return replay_existing_slot(&existing, &assignment);
            }
            Err(error) => return Err(RolloutEvaluationError::storage(&error)),
        };
        decode_creation_receipt(&receipt, &assignment, receipt.idempotent_replay)
    }

    /// Persists an opaque pair produced by the durable authority projection.
    ///
    /// The request identity is derived from the pair digest, so an exact
    /// restart replays while any changed command conflicts closed.
    ///
    /// # Errors
    ///
    /// Rejects stale policy authority, malformed projected facts, request
    /// conflicts, and unavailable storage.
    pub fn record_projected_pair(
        &mut self,
        command: RecordProjectedEvaluationPair,
    ) -> Result<Sha256Digest, RolloutEvaluationError> {
        let request_id = pair_request_id(command.projected_pair.digest())?;
        self.record_authorized_pair(RecordAuthorizedEvaluationPair {
            scope: command.scope,
            request_id,
            expected_gate_revision: command.expected_gate_revision,
            pair: command.projected_pair.pair,
            occurred_at_millis: command.occurred_at_millis,
        })
    }

    /// Persists a complete pair produced by the internal terminal/Artifact and
    /// Provider-ledger authority join.
    pub(crate) fn record_authorized_pair(
        &mut self,
        command: RecordAuthorizedEvaluationPair,
    ) -> Result<Sha256Digest, RolloutEvaluationError> {
        validate_pair_command(&command)?;
        let command_digest = digest_json(&PairCommandDigest {
            scope: &command.scope,
            expected_gate_revision: command.expected_gate_revision,
            pair_digest: command.pair.digest(),
            occurred_at_millis: command.occurred_at_millis,
        })?;
        let identity = receipt_identity(&command.scope, command.request_id.clone())?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&identity, &command_digest)
            .map_err(|error| RolloutEvaluationError::storage(&error))?
        {
            decode_pair_receipt(&receipt, &command.pair)?;
            return Ok(command.pair.digest().clone());
        }
        let Some((gate_stream, gate_revision, policy, statistical_policy)) =
            crate::rollout_gate::active_statistical_policy(self.storage, &command.scope)
                .map_err(|_| RolloutEvaluationError::corrupt())?
        else {
            return Err(RolloutEvaluationError::invalid(
                "authorized pair requires an active rollout policy",
            ));
        };
        let react = command.pair.react_authorization().assignment();
        let delegated = command.pair.delegated_authorization().assignment();
        if gate_revision != command.expected_gate_revision
            || react.spec().policy_revision != policy.revision()
            || &react.spec().policy_digest != policy.digest()
            || statistical_policy.authorizes_assignment(react).is_err()
            || statistical_policy.authorizes_assignment(delegated).is_err()
        {
            return Err(RolloutEvaluationError {
                kind: RolloutEvaluationErrorKind::RevisionConflict,
                message: "authorized pair policy changed",
            });
        }
        if !pair_authorizations_match_revision(&command.pair, 1) {
            return Err(RolloutEvaluationError::invalid(
                "authorized pair revision is invalid",
            ));
        }
        let assignments = [react, delegated]
            .map(|assignment| load_consumed_assignment(self.storage, assignment))
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        commit_authorized_pair(
            self.storage,
            command,
            command_digest,
            identity,
            gate_stream,
            gate_revision,
            assignments,
        )
    }
}

fn prepare_assignment_commit(
    command: CreateEvaluationAssignment,
    identity: ReceiptIdentity,
    command_digest: Sha256Digest,
    gate_stream: String,
    gate_revision: u64,
    slot: &StoredEvaluationSlot,
) -> Result<StateCommit, RolloutEvaluationError> {
    let CreateEvaluationAssignment {
        scope,
        request_id: _,
        expected_gate_revision: _,
        assignment,
        occurred_at_millis,
    } = command;
    let mut stored = StoredAssignment {
        schema: STATE_SCHEMA.to_owned(),
        revision: 1,
        slot_digest: slot.slot_digest.clone(),
        assignment: assignment.clone(),
        state: AssignmentState::Active,
        consumption: None,
        updated_at_millis: occurred_at_millis,
        digest: Sha256Digest(String::new()),
    };
    stored.digest = stored_assignment_digest(&stored)?;
    let event = AssignmentReceiptEvent {
        schema: STATE_SCHEMA.to_owned(),
        assignment: assignment.clone(),
    };
    let audit = execution_audit_event_with_state(
        AuditEventId::from_digest(&command_digest)
            .map_err(|_| RolloutEvaluationError::corrupt())?,
        occurred_at_millis,
        identity.request_id().clone(),
        &scope,
        AuditAction::policy("rollout.evaluation.assignment.created")
            .map_err(|_| RolloutEvaluationError::corrupt())?,
        AuditState::changed(None, digest_json(&stored)?)
            .map_err(|_| RolloutEvaluationError::corrupt())?,
        AuditSubject::new(),
        "accepted",
    )
    .map_err(|error| RolloutEvaluationError::storage(&error))?;
    let mut commit = StateCommit::new(
        identity,
        command_digest,
        assignment_stream_id(assignment.digest())?,
        0,
        serde_json::to_vec(&stored).map_err(|_| RolloutEvaluationError::corrupt())?,
        vec![NewOutboxEvent::internal(
            assignment_event_id(assignment.digest()),
            RECEIPT_TOPIC,
            serde_json::to_vec(&event).map_err(|_| RolloutEvaluationError::corrupt())?,
        )],
    )
    .with_state_guard(
        StateRevisionGuard::new(gate_stream, gate_revision)
            .map_err(|error| RolloutEvaluationError::storage(&error))?,
    )
    .with_state_mutation(
        StateMutation::new(
            slot_stream_id(&slot.slot_digest)?,
            0,
            serde_json::to_vec(slot).map_err(|_| RolloutEvaluationError::corrupt())?,
        )
        .map_err(|error| RolloutEvaluationError::storage(&error))?,
    )
    .with_pending_audit_event(audit);
    for claim in identity_claims(&assignment, &slot.slot_digest)? {
        commit = commit.with_state_mutation(
            StateMutation::new(
                identity_claim_stream_id(&claim.key_digest)?,
                0,
                serde_json::to_vec(&claim).map_err(|_| RolloutEvaluationError::corrupt())?,
            )
            .map_err(|error| RolloutEvaluationError::storage(&error))?,
        );
    }
    Ok(commit)
}

fn commit_authorized_pair(
    storage: &mut dyn ProductStateStorage,
    command: RecordAuthorizedEvaluationPair,
    command_digest: Sha256Digest,
    identity: ReceiptIdentity,
    gate_stream: String,
    gate_revision: u64,
    assignments: Vec<ConsumedAssignmentSlot>,
) -> Result<Sha256Digest, RolloutEvaluationError> {
    let RecordAuthorizedEvaluationPair {
        scope,
        request_id: _,
        expected_gate_revision: _,
        pair,
        occurred_at_millis,
    } = command;
    let pair_digest = pair.digest().clone();
    let mut stored = StoredAuthorizedPair {
        schema: PAIR_SCHEMA.to_owned(),
        revision: 1,
        pair: pair.clone(),
        recorded_at_millis: occurred_at_millis,
        digest: Sha256Digest(String::new()),
    };
    stored.digest = stored_pair_digest(&stored)?;
    let payload = serde_json::to_vec(&stored).map_err(|_| RolloutEvaluationError::corrupt())?;
    let event = PairReceiptEvent {
        schema: PAIR_SCHEMA.to_owned(),
        pair: pair.clone(),
    };
    let audit = execution_audit_event_with_state(
        AuditEventId::from_digest(&command_digest)
            .map_err(|_| RolloutEvaluationError::corrupt())?,
        occurred_at_millis,
        identity.request_id().clone(),
        &scope,
        AuditAction::policy("rollout.evaluation.pair.recorded")
            .map_err(|_| RolloutEvaluationError::corrupt())?,
        AuditState::changed(None, digest_json(&stored)?)
            .map_err(|_| RolloutEvaluationError::corrupt())?,
        AuditSubject::new(),
        "accepted",
    )
    .map_err(|error| RolloutEvaluationError::storage(&error))?;
    let mut commit = StateCommit::new(
        identity,
        command_digest,
        pair_stream_id(&pair_digest)?,
        0,
        payload,
        vec![NewOutboxEvent::internal(
            pair_event_id(&pair_digest),
            PAIR_RECEIPT_TOPIC,
            serde_json::to_vec(&event).map_err(|_| RolloutEvaluationError::corrupt())?,
        )],
    )
    .with_state_guard(
        StateRevisionGuard::new(gate_stream, gate_revision)
            .map_err(|error| RolloutEvaluationError::storage(&error))?,
    )
    .with_pending_audit_event(audit);
    let authorizations = [pair.react_authorization(), pair.delegated_authorization()];
    for (assignment, authorization) in assignments.into_iter().zip(authorizations) {
        commit = commit.with_state_guard(
            StateRevisionGuard::new(
                assignment.assignment_stream_id,
                assignment.assignment_revision,
            )
            .map_err(|error| RolloutEvaluationError::storage(&error))?,
        );
        let mut paired_slot = assignment.slot;
        paired_slot.revision = paired_slot.revision.checked_add(1).ok_or_else(|| {
            RolloutEvaluationError::invalid("evaluation slot revision is exhausted")
        })?;
        paired_slot.state = SlotState::Paired;
        paired_slot.authorization_digest = Some(authorization.digest().clone());
        paired_slot.pair_digest = Some(pair_digest.clone());
        paired_slot.updated_at_millis = occurred_at_millis;
        paired_slot.digest = stored_slot_digest(&paired_slot)?;
        commit = commit.with_state_mutation(
            StateMutation::new(
                assignment.slot_stream_id,
                paired_slot.revision - 1,
                serde_json::to_vec(&paired_slot).map_err(|_| RolloutEvaluationError::corrupt())?,
            )
            .map_err(|error| RolloutEvaluationError::storage(&error))?,
        );
    }
    let receipt = storage
        .commit(&commit)
        .map_err(|error| RolloutEvaluationError::storage(&error))?;
    decode_pair_receipt(&receipt, &pair)?;
    Ok(pair_digest)
}

pub(crate) fn load_authorized_pairs(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    references: &[Sha256Digest],
) -> Result<Vec<PerformancePairedSampleV1>, RolloutEvaluationError> {
    if !(2..=4_096).contains(&references.len()) {
        return Err(RolloutEvaluationError::invalid(
            "rollout evidence requires at least two authorized pair references",
        ));
    }
    let mut unique = std::collections::BTreeSet::new();
    let mut pairs = Vec::with_capacity(references.len());
    for reference in references {
        if !unique.insert(reference.0.clone()) {
            return Err(RolloutEvaluationError::invalid(
                "rollout evidence repeats an authorized pair",
            ));
        }
        let stream_id = pair_stream_id(reference)?;
        let stored = storage
            .load_state(&stream_id)
            .map_err(|error| RolloutEvaluationError::storage(&error))?
            .ok_or_else(|| {
                RolloutEvaluationError::invalid("authorized evaluation pair does not exist")
            })?;
        let pair = decode_pair(&stored)?;
        if pair.pair.digest() != reference
            || &pair
                .pair
                .react_authorization()
                .assignment()
                .spec()
                .repository_scope
                != scope
        {
            return Err(RolloutEvaluationError::invalid(
                "authorized evaluation pair is outside this repository",
            ));
        }
        validate_paired_slots(storage, &pair.pair)?;
        pairs.push(pair.pair);
    }
    Ok(pairs)
}

fn validate_paired_slots(
    storage: &dyn ProductStateStorage,
    pair: &PerformancePairedSampleV1,
) -> Result<(), RolloutEvaluationError> {
    for authorization in [pair.react_authorization(), pair.delegated_authorization()] {
        let (_, slot) = load_exact_slot(storage, authorization.assignment())?;
        if slot.state != SlotState::Paired
            || slot.authorization_digest.as_ref() != Some(authorization.digest())
            || slot.pair_digest.as_ref() != Some(pair.digest())
        {
            return Err(RolloutEvaluationError::invalid(
                "authorized pair does not own its evaluation slots",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EvaluationJobSeal {
    stream_id: String,
    expected_revision: u64,
    slot_stream_id: String,
    slot_expected_revision: u64,
    assignment: EvaluationAssignmentV1,
    slot: StoredEvaluationSlot,
    existing_consumption: Option<AssignmentConsumption>,
    consumed_at_millis: u64,
}

impl EvaluationJobSeal {
    pub(crate) const fn assignment(&self) -> &EvaluationAssignmentV1 {
        &self.assignment
    }

    pub(crate) fn write_mode(
        &self,
    ) -> winwincode_execution_port::generated::ExecutionWorkspaceWriteMode {
        match self.assignment.spec().arm {
            EvaluationArmV1::React => {
                winwincode_execution_port::generated::ExecutionWorkspaceWriteMode::Candidate
            }
            EvaluationArmV1::Delegated => {
                winwincode_execution_port::generated::ExecutionWorkspaceWriteMode::ReadOnly
            }
        }
    }
}

pub(crate) fn seal_evaluation_job(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    _configured_mode: ExecutionMode,
    assignment_digest: Option<&Sha256Digest>,
    job_id: &ExecutionJobId,
    base_revision: &str,
    now_millis: u64,
) -> Result<Option<EvaluationJobSeal>, RolloutEvaluationError> {
    let Some(assignment_digest) = assignment_digest else {
        return Ok(None);
    };
    let stream_id = assignment_stream_id(assignment_digest)?;
    let stored = storage
        .load_state(&stream_id)
        .map_err(|error| RolloutEvaluationError::storage(&error))?
        .ok_or_else(|| RolloutEvaluationError::invalid("evaluation assignment does not exist"))?;
    let record = decode_assignment(&stored)?;
    let (slot_stream_id, slot) = load_exact_slot(storage, &record.assignment)?;
    let spec = record.assignment.spec();
    if record.assignment.digest() != assignment_digest
        || &spec.repository_scope != scope
        || &spec.job_id != job_id
        || spec.base_revision != base_revision
        || now_millis == 0
        || now_millis > spec.cutoff_at_millis
    {
        return Err(RolloutEvaluationError::invalid(
            "evaluation assignment does not authorize this Job",
        ));
    }
    match record.state {
        AssignmentState::Active => {
            if slot.state != SlotState::Assigned || slot.consumption.is_some() {
                return Err(RolloutEvaluationError::corrupt());
            }
            let current_policy = crate::rollout_gate::active_statistical_policy(storage, scope)
                .map_err(|_| RolloutEvaluationError::corrupt())?;
            if !current_policy.is_some_and(|(_, _, policy, statistical_policy)| {
                policy.revision() == spec.policy_revision
                    && policy.digest() == &spec.policy_digest
                    && statistical_policy
                        .authorizes_assignment(&record.assignment)
                        .is_ok()
            }) {
                return Err(RolloutEvaluationError::invalid(
                    "evaluation assignment policy is no longer active",
                ));
            }
            Ok(Some(EvaluationJobSeal {
                stream_id,
                expected_revision: record.revision,
                slot_stream_id,
                slot_expected_revision: slot.revision,
                assignment: record.assignment,
                slot,
                existing_consumption: None,
                consumed_at_millis: now_millis,
            }))
        }
        AssignmentState::Consumed => {
            let consumption = record
                .consumption
                .ok_or_else(RolloutEvaluationError::corrupt)?;
            if consumption.job_id != *job_id {
                return Err(RolloutEvaluationError::invalid(
                    "evaluation assignment was already consumed",
                ));
            }
            if slot.state != SlotState::Consumed || slot.consumption.as_ref() != Some(&consumption)
            {
                return Err(RolloutEvaluationError::corrupt());
            }
            Ok(Some(EvaluationJobSeal {
                stream_id,
                expected_revision: record.revision,
                slot_stream_id,
                slot_expected_revision: slot.revision,
                assignment: record.assignment,
                slot,
                existing_consumption: Some(consumption),
                consumed_at_millis: record.updated_at_millis,
            }))
        }
    }
}

/// Resolves the one active predeclared assignment for an exact production Job.
///
/// A missing claim means the Job is ordinary production work. Any malformed,
/// foreign, stale, or conflicting claim fails closed instead of selecting a
/// caller-provided assignment.
pub(crate) fn assignment_digest_for_job(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    job_id: &ExecutionJobId,
) -> Result<Option<Sha256Digest>, RolloutEvaluationError> {
    let Some((_, _, policy, statistical_policy)) =
        crate::rollout_gate::active_statistical_policy(storage, scope)
            .map_err(|_| RolloutEvaluationError::corrupt())?
    else {
        return Ok(None);
    };
    let key = EvaluationIdentityClaimKey {
        repository_scope: scope.clone(),
        policy_revision: policy.revision(),
        kind: EvaluationClaimKind::Job,
        identity: job_id.0.clone(),
    };
    let key_digest = digest_json(&key)?;
    let Some(stored) = storage
        .load_state(&identity_claim_stream_id(&key_digest)?)
        .map_err(|error| RolloutEvaluationError::storage(&error))?
    else {
        return Ok(None);
    };
    let claim: StoredEvaluationIdentityClaim = serde_json::from_slice(&stored.payload)
        .map_err(|_| RolloutEvaluationError::corrupt())?;
    if stored.revision != 1
        || claim.schema != CLAIM_SCHEMA
        || claim.key != key
        || claim.key_digest != key_digest
        || claim.digest
            != digest_json(&StoredIdentityClaimDigest {
                key: &claim.key_digest,
                slot: &claim.slot_digest,
                assignment: &claim.assignment_digest,
            })?
        || stored.stream_id != identity_claim_stream_id(&claim.key_digest)?
    {
        return Err(RolloutEvaluationError::corrupt());
    }
    let assignment_state = storage
        .load_state(&assignment_stream_id(&claim.assignment_digest)?)
        .map_err(|error| RolloutEvaluationError::storage(&error))?
        .ok_or_else(RolloutEvaluationError::corrupt)?;
    let assignment = decode_assignment(&assignment_state)?;
    if assignment.assignment.digest() != &claim.assignment_digest
        || assignment.slot_digest != claim.slot_digest
        || assignment.assignment.spec().job_id != *job_id
        || assignment.assignment.spec().repository_scope != *scope
        || assignment.assignment.spec().policy_revision != policy.revision()
        || assignment.assignment.spec().policy_digest != *policy.digest()
        || statistical_policy
            .authorizes_assignment(&assignment.assignment)
            .is_err()
    {
        return Err(RolloutEvaluationError::corrupt());
    }
    Ok(Some(claim.assignment_digest))
}

pub(crate) fn bind_evaluation_job(
    commit: StateCommit,
    job: &ExecutionJob,
    seal: Option<&EvaluationJobSeal>,
) -> Result<StateCommit, RolloutEvaluationError> {
    let Some(seal) = seal else {
        return Ok(commit);
    };
    if seal.assignment.spec().job_id != job.job_id
        || seal.assignment.spec().base_revision != job.workspace.checkout_revision
    {
        return Err(RolloutEvaluationError::corrupt());
    }
    if let Some(consumption) = &seal.existing_consumption {
        if consumption.job_payload_digest != job.payload_digest {
            return Err(RolloutEvaluationError::corrupt());
        }
        return Ok(commit
            .with_state_guard(
                StateRevisionGuard::new(seal.stream_id.clone(), seal.expected_revision)
                    .map_err(|error| RolloutEvaluationError::storage(&error))?,
            )
            .with_state_guard(
                StateRevisionGuard::new(seal.slot_stream_id.clone(), seal.slot_expected_revision)
                    .map_err(|error| RolloutEvaluationError::storage(&error))?,
            ));
    }
    let consumption = AssignmentConsumption {
        job_id: job.job_id.clone(),
        job_payload_digest: job.payload_digest.clone(),
    };
    let mut consumed = StoredAssignment {
        schema: STATE_SCHEMA.to_owned(),
        revision: seal
            .expected_revision
            .checked_add(1)
            .ok_or_else(|| RolloutEvaluationError::invalid("assignment revision is exhausted"))?,
        slot_digest: seal.slot.slot_digest.clone(),
        assignment: seal.assignment.clone(),
        state: AssignmentState::Consumed,
        consumption: Some(consumption.clone()),
        updated_at_millis: seal.consumed_at_millis,
        digest: Sha256Digest(String::new()),
    };
    consumed.digest = stored_assignment_digest(&consumed)?;
    let payload = serde_json::to_vec(&consumed).map_err(|_| RolloutEvaluationError::corrupt())?;
    let mut consumed_slot = seal.slot.clone();
    consumed_slot.revision = seal
        .slot_expected_revision
        .checked_add(1)
        .ok_or_else(|| RolloutEvaluationError::invalid("evaluation slot revision is exhausted"))?;
    consumed_slot.state = SlotState::Consumed;
    consumed_slot.consumption = Some(consumption);
    consumed_slot.updated_at_millis = seal.consumed_at_millis;
    consumed_slot.digest = stored_slot_digest(&consumed_slot)?;
    let slot_payload =
        serde_json::to_vec(&consumed_slot).map_err(|_| RolloutEvaluationError::corrupt())?;
    Ok(commit
        .with_state_mutation(
            StateMutation::new(seal.stream_id.clone(), seal.expected_revision, payload)
                .map_err(|error| RolloutEvaluationError::storage(&error))?,
        )
        .with_state_mutation(
            StateMutation::new(
                seal.slot_stream_id.clone(),
                seal.slot_expected_revision,
                slot_payload,
            )
            .map_err(|error| RolloutEvaluationError::storage(&error))?,
        ))
}

fn validate_create(command: &CreateEvaluationAssignment) -> Result<(), RolloutEvaluationError> {
    command
        .assignment
        .validate()
        .map_err(|_| RolloutEvaluationError::invalid("evaluation assignment is invalid"))?;
    if command.assignment.spec().repository_scope != command.scope
        || command.expected_gate_revision == 0
        || command.expected_gate_revision > MAX_SAFE_INTEGER
        || command.occurred_at_millis == 0
        || command.occurred_at_millis > MAX_SAFE_INTEGER
        || command.occurred_at_millis > command.assignment.spec().cutoff_at_millis
    {
        return Err(RolloutEvaluationError::invalid(
            "evaluation assignment command is invalid",
        ));
    }
    Ok(())
}

fn decode_assignment(stored: &StoredState) -> Result<StoredAssignment, RolloutEvaluationError> {
    let record: StoredAssignment =
        serde_json::from_slice(&stored.payload).map_err(|_| RolloutEvaluationError::corrupt())?;
    record
        .assignment
        .validate()
        .map_err(|_| RolloutEvaluationError::corrupt())?;
    if record.schema != STATE_SCHEMA
        || record.revision != stored.revision
        || stored.stream_id != assignment_stream_id(record.assignment.digest())?
        || record.slot_digest != evaluation_slot_digest(&record.assignment)?
        || record.digest != stored_assignment_digest(&record)?
        || serde_json::to_vec(&record).map_err(|_| RolloutEvaluationError::corrupt())?
            != stored.payload
        || !assignment_state_is_valid(&record)
    {
        return Err(RolloutEvaluationError::corrupt());
    }
    Ok(record)
}

fn assigned_slot(
    assignment: &EvaluationAssignmentV1,
    occurred_at_millis: u64,
) -> Result<StoredEvaluationSlot, RolloutEvaluationError> {
    let key = evaluation_slot_key(assignment);
    let slot_digest = digest_json(&key)?;
    let mut slot = StoredEvaluationSlot {
        schema: SLOT_SCHEMA.to_owned(),
        revision: 1,
        key,
        slot_digest,
        assignment_digest: assignment.digest().clone(),
        state: SlotState::Assigned,
        consumption: None,
        authorization_digest: None,
        pair_digest: None,
        updated_at_millis: occurred_at_millis,
        digest: Sha256Digest(String::new()),
    };
    slot.digest = stored_slot_digest(&slot)?;
    Ok(slot)
}

fn identity_claims(
    assignment: &EvaluationAssignmentV1,
    slot_digest: &Sha256Digest,
) -> Result<[StoredEvaluationIdentityClaim; 2], RolloutEvaluationError> {
    let spec = assignment.spec();
    let job = EvaluationIdentityClaimKey {
        repository_scope: spec.repository_scope.clone(),
        policy_revision: spec.policy_revision,
        kind: EvaluationClaimKind::Job,
        identity: spec.job_id.0.clone(),
    };
    let run = EvaluationIdentityClaimKey {
        repository_scope: spec.repository_scope.clone(),
        policy_revision: spec.policy_revision,
        kind: EvaluationClaimKind::Run,
        identity: spec.run_id.0.clone(),
    };
    Ok([
        identity_claim(job, slot_digest, assignment.digest())?,
        identity_claim(run, slot_digest, assignment.digest())?,
    ])
}

fn identity_claim(
    key: EvaluationIdentityClaimKey,
    slot_digest: &Sha256Digest,
    assignment_digest: &Sha256Digest,
) -> Result<StoredEvaluationIdentityClaim, RolloutEvaluationError> {
    let key_digest = digest_json(&key)?;
    let mut claim = StoredEvaluationIdentityClaim {
        schema: CLAIM_SCHEMA.to_owned(),
        key,
        key_digest,
        slot_digest: slot_digest.clone(),
        assignment_digest: assignment_digest.clone(),
        digest: Sha256Digest(String::new()),
    };
    claim.digest = digest_json(&StoredIdentityClaimDigest {
        key: &claim.key_digest,
        slot: &claim.slot_digest,
        assignment: &claim.assignment_digest,
    })?;
    Ok(claim)
}

fn evaluation_slot_key(assignment: &EvaluationAssignmentV1) -> EvaluationSlotKey {
    let spec = assignment.spec();
    EvaluationSlotKey {
        repository_scope: spec.repository_scope.clone(),
        policy_revision: spec.policy_revision,
        pair_id: spec.pair_id.clone(),
        arm: spec.arm,
    }
}

fn evaluation_slot_digest(
    assignment: &EvaluationAssignmentV1,
) -> Result<Sha256Digest, RolloutEvaluationError> {
    digest_json(&evaluation_slot_key(assignment))
}

fn load_slot_optional(
    storage: &dyn ProductStateStorage,
    slot_digest: &Sha256Digest,
) -> Result<Option<StoredEvaluationSlot>, RolloutEvaluationError> {
    let stream_id = slot_stream_id(slot_digest)?;
    storage
        .load_state(&stream_id)
        .map_err(|error| RolloutEvaluationError::storage(&error))?
        .as_ref()
        .map(decode_slot)
        .transpose()
}

fn load_exact_slot(
    storage: &dyn ProductStateStorage,
    assignment: &EvaluationAssignmentV1,
) -> Result<(String, StoredEvaluationSlot), RolloutEvaluationError> {
    let expected_digest = evaluation_slot_digest(assignment)?;
    let stream_id = slot_stream_id(&expected_digest)?;
    let stored = storage
        .load_state(&stream_id)
        .map_err(|error| RolloutEvaluationError::storage(&error))?
        .ok_or_else(|| RolloutEvaluationError::invalid("evaluation slot does not exist"))?;
    let slot = decode_slot(&stored)?;
    if slot.key != evaluation_slot_key(assignment)
        || slot.slot_digest != expected_digest
        || slot.assignment_digest != *assignment.digest()
    {
        return Err(RolloutEvaluationError::invalid(
            "evaluation assignment does not own its slot",
        ));
    }
    Ok((stream_id, slot))
}

fn replay_existing_slot(
    slot: &StoredEvaluationSlot,
    assignment: &EvaluationAssignmentV1,
) -> Result<EvaluationAssignmentReceipt, RolloutEvaluationError> {
    if slot.key != evaluation_slot_key(assignment) || slot.assignment_digest != *assignment.digest()
    {
        return Err(RolloutEvaluationError {
            kind: RolloutEvaluationErrorKind::RevisionConflict,
            message: "evaluation slot is already assigned",
        });
    }
    Ok(EvaluationAssignmentReceipt {
        assignment_digest: assignment.digest().clone(),
        replayed: true,
    })
}

fn decode_slot(stored: &StoredState) -> Result<StoredEvaluationSlot, RolloutEvaluationError> {
    let slot: StoredEvaluationSlot =
        serde_json::from_slice(&stored.payload).map_err(|_| RolloutEvaluationError::corrupt())?;
    if slot.schema != SLOT_SCHEMA
        || slot.revision != stored.revision
        || stored.stream_id != slot_stream_id(&slot.slot_digest)?
        || slot.slot_digest != digest_json(&slot.key)?
        || !canonical_digest(&slot.assignment_digest)
        || slot.digest != stored_slot_digest(&slot)?
        || serde_json::to_vec(&slot).map_err(|_| RolloutEvaluationError::corrupt())?
            != stored.payload
        || !slot_state_is_valid(&slot)
    {
        return Err(RolloutEvaluationError::corrupt());
    }
    Ok(slot)
}

fn slot_state_is_valid(slot: &StoredEvaluationSlot) -> bool {
    match slot.state {
        SlotState::Assigned => {
            slot.revision == 1
                && slot.consumption.is_none()
                && slot.authorization_digest.is_none()
                && slot.pair_digest.is_none()
        }
        SlotState::Consumed => {
            slot.revision == 2
                && slot
                    .consumption
                    .as_ref()
                    .is_some_and(|consumption| canonical_digest(&consumption.job_payload_digest))
                && slot.authorization_digest.is_none()
                && slot.pair_digest.is_none()
        }
        SlotState::Paired => {
            slot.revision == 3
                && slot
                    .consumption
                    .as_ref()
                    .is_some_and(|consumption| canonical_digest(&consumption.job_payload_digest))
                && slot
                    .authorization_digest
                    .as_ref()
                    .is_some_and(canonical_digest)
                && slot.pair_digest.as_ref().is_some_and(canonical_digest)
        }
    }
}

fn assignment_state_is_valid(record: &StoredAssignment) -> bool {
    match record.state {
        AssignmentState::Active => record.revision == 1 && record.consumption.is_none(),
        AssignmentState::Consumed => {
            record.revision == 2
                && record.consumption.as_ref().is_some_and(|consumption| {
                    consumption.job_id == record.assignment.spec().job_id
                        && canonical_digest(&consumption.job_payload_digest)
                })
        }
    }
}

fn validate_pair_command(
    command: &RecordAuthorizedEvaluationPair,
) -> Result<(), RolloutEvaluationError> {
    command
        .pair
        .validate()
        .map_err(|_| RolloutEvaluationError::invalid("authorized evaluation pair is invalid"))?;
    let spec = command.pair.react_authorization().assignment().spec();
    if spec.repository_scope != command.scope
        || command.expected_gate_revision == 0
        || command.expected_gate_revision > MAX_SAFE_INTEGER
        || command.occurred_at_millis == 0
        || command.occurred_at_millis > spec.cutoff_at_millis
    {
        return Err(RolloutEvaluationError::invalid(
            "authorized evaluation pair command is invalid",
        ));
    }
    Ok(())
}

fn load_consumed_assignment(
    storage: &dyn ProductStateStorage,
    assignment: &EvaluationAssignmentV1,
) -> Result<ConsumedAssignmentSlot, RolloutEvaluationError> {
    let stream_id = assignment_stream_id(assignment.digest())?;
    let stored = storage
        .load_state(&stream_id)
        .map_err(|error| RolloutEvaluationError::storage(&error))?
        .ok_or_else(|| {
            RolloutEvaluationError::invalid("authorized pair assignment does not exist")
        })?;
    let record = decode_assignment(&stored)?;
    if record.assignment != *assignment || record.state != AssignmentState::Consumed {
        return Err(RolloutEvaluationError::invalid(
            "authorized pair assignment was not consumed by its Job",
        ));
    }
    let (slot_stream_id, slot) = load_exact_slot(storage, assignment)?;
    if slot.state == SlotState::Paired {
        return Err(RolloutEvaluationError {
            kind: RolloutEvaluationErrorKind::RevisionConflict,
            message: "evaluation slot already owns an authorized pair",
        });
    }
    if slot.state != SlotState::Consumed || slot.consumption != record.consumption {
        return Err(RolloutEvaluationError::invalid(
            "authorized pair does not own its evaluation slot",
        ));
    }
    Ok(ConsumedAssignmentSlot {
        assignment_stream_id: stream_id,
        assignment_revision: record.revision,
        slot_stream_id,
        slot,
    })
}

/// Verifies that terminal projection still belongs to the exact consumed slot.
pub(crate) fn validate_consumed_evaluation_slot(
    storage: &dyn ProductStateStorage,
    assignment: &EvaluationAssignmentV1,
) -> Result<(), RolloutEvaluationError> {
    let (_, slot) = load_exact_slot(storage, assignment)?;
    let assignment_state = storage
        .load_state(&assignment_stream_id(assignment.digest())?)
        .map_err(|error| RolloutEvaluationError::storage(&error))?
        .ok_or_else(RolloutEvaluationError::corrupt)?;
    let record = decode_assignment(&assignment_state)?;
    if record.assignment != *assignment
        || record.state != AssignmentState::Consumed
        || !matches!(slot.state, SlotState::Consumed | SlotState::Paired)
        || slot.consumption != record.consumption
    {
        return Err(RolloutEvaluationError::invalid(
            "terminal projection does not own its evaluation slot",
        ));
    }
    Ok(())
}

/// Loads the only consumed React and Delegated assignments for a policy pair.
pub(crate) fn load_consumed_pair_assignments(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    policy_revision: u64,
    pair_id: &Sha256Digest,
) -> Result<ConsumedEvaluationPairAssignments, RolloutEvaluationError> {
    let react_key = EvaluationSlotKey {
        repository_scope: scope.clone(),
        policy_revision,
        pair_id: pair_id.clone(),
        arm: EvaluationArmV1::React,
    };
    let react = load_consumed_slot_assignment(storage, &react_key)?;
    let delegated_key = EvaluationSlotKey {
        repository_scope: scope.clone(),
        policy_revision,
        pair_id: pair_id.clone(),
        arm: EvaluationArmV1::Delegated,
    };
    let delegated = load_consumed_slot_assignment(storage, &delegated_key)?;
    Ok(ConsumedEvaluationPairAssignments { react, delegated })
}

fn load_consumed_slot_assignment(
    storage: &dyn ProductStateStorage,
    key: &EvaluationSlotKey,
) -> Result<EvaluationAssignmentV1, RolloutEvaluationError> {
    let slot_digest = digest_json(&key)?;
    let slot = load_slot_optional(storage, &slot_digest)?
        .ok_or_else(|| RolloutEvaluationError::invalid("evaluation slot does not exist"))?;
    if slot.key != *key || !matches!(slot.state, SlotState::Consumed | SlotState::Paired) {
        return Err(RolloutEvaluationError::invalid(
            "evaluation slot is not ready for terminal projection",
        ));
    }
    let assignment_state = storage
        .load_state(&assignment_stream_id(&slot.assignment_digest)?)
        .map_err(|error| RolloutEvaluationError::storage(&error))?
        .ok_or_else(RolloutEvaluationError::corrupt)?;
    let assignment = decode_assignment(&assignment_state)?;
    if assignment.state != AssignmentState::Consumed
        || assignment.assignment.digest() != &slot.assignment_digest
        || assignment.consumption != slot.consumption
    {
        return Err(RolloutEvaluationError::corrupt());
    }
    Ok(assignment.assignment)
}

fn decode_pair(stored: &StoredState) -> Result<StoredAuthorizedPair, RolloutEvaluationError> {
    let record: StoredAuthorizedPair =
        serde_json::from_slice(&stored.payload).map_err(|_| RolloutEvaluationError::corrupt())?;
    record
        .pair
        .validate()
        .map_err(|_| RolloutEvaluationError::corrupt())?;
    if record.schema != PAIR_SCHEMA
        || record.revision != 1
        || stored.revision != record.revision
        || stored.stream_id != pair_stream_id(record.pair.digest())?
        || !pair_authorizations_match_revision(&record.pair, record.revision)
        || record.digest != stored_pair_digest(&record)?
        || serde_json::to_vec(&record).map_err(|_| RolloutEvaluationError::corrupt())?
            != stored.payload
    {
        return Err(RolloutEvaluationError::corrupt());
    }
    Ok(record)
}

fn pair_authorizations_match_revision(pair: &PerformancePairedSampleV1, revision: u64) -> bool {
    pair.react_authorization().facts().authorization_revision == revision
        && pair
            .delegated_authorization()
            .facts()
            .authorization_revision
            == revision
}

fn stored_pair_digest(
    record: &StoredAuthorizedPair,
) -> Result<Sha256Digest, RolloutEvaluationError> {
    digest_json(&StoredPairDigest {
        pair_digest: record.pair.digest(),
        recorded_at_millis: record.recorded_at_millis,
    })
}

fn decode_pair_receipt(
    receipt: &CommitReceipt,
    pair: &PerformancePairedSampleV1,
) -> Result<(), RolloutEvaluationError> {
    let events = receipt
        .events
        .iter()
        .filter(|event| event.topic == PAIR_RECEIPT_TOPIC)
        .collect::<Vec<_>>();
    let [event] = events.as_slice() else {
        return Err(RolloutEvaluationError::corrupt());
    };
    let decoded: PairReceiptEvent =
        serde_json::from_slice(&event.payload).map_err(|_| RolloutEvaluationError::corrupt())?;
    if decoded.schema != PAIR_SCHEMA
        || decoded.pair != *pair
        || event.event_id != pair_event_id(pair.digest())
        || serde_json::to_vec(&decoded).map_err(|_| RolloutEvaluationError::corrupt())?
            != event.payload
    {
        return Err(RolloutEvaluationError::corrupt());
    }
    Ok(())
}

fn stored_assignment_digest(
    record: &StoredAssignment,
) -> Result<Sha256Digest, RolloutEvaluationError> {
    digest_json(&StoredAssignmentDigest {
        revision: record.revision,
        slot_digest: &record.slot_digest,
        assignment_digest: record.assignment.digest(),
        state: record.state,
        consumption: &record.consumption,
        updated_at_millis: record.updated_at_millis,
    })
}

fn stored_slot_digest(slot: &StoredEvaluationSlot) -> Result<Sha256Digest, RolloutEvaluationError> {
    digest_json(&StoredSlotDigest {
        revision: slot.revision,
        slot_digest: &slot.slot_digest,
        assignment_digest: &slot.assignment_digest,
        state: slot.state,
        consumption: &slot.consumption,
        authorization_digest: &slot.authorization_digest,
        pair_digest: &slot.pair_digest,
        updated_at_millis: slot.updated_at_millis,
    })
}

fn decode_creation_receipt(
    receipt: &CommitReceipt,
    assignment: &EvaluationAssignmentV1,
    replayed: bool,
) -> Result<EvaluationAssignmentReceipt, RolloutEvaluationError> {
    let events = receipt
        .events
        .iter()
        .filter(|event| event.topic == RECEIPT_TOPIC)
        .collect::<Vec<_>>();
    let [event] = events.as_slice() else {
        return Err(RolloutEvaluationError::corrupt());
    };
    let decoded: AssignmentReceiptEvent =
        serde_json::from_slice(&event.payload).map_err(|_| RolloutEvaluationError::corrupt())?;
    if decoded.schema != STATE_SCHEMA
        || decoded.assignment != *assignment
        || event.event_id != assignment_event_id(assignment.digest())
        || serde_json::to_vec(&decoded).map_err(|_| RolloutEvaluationError::corrupt())?
            != event.payload
    {
        return Err(RolloutEvaluationError::corrupt());
    }
    Ok(EvaluationAssignmentReceipt {
        assignment_digest: assignment.digest().clone(),
        replayed,
    })
}

fn receipt_identity(
    scope: &RepositoryScope,
    request_id: RequestId,
) -> Result<ReceiptIdentity, RolloutEvaluationError> {
    ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(ACTOR_KEY.to_vec())
            .map_err(|error| RolloutEvaluationError::storage(&error))?,
        repository_scope_key(scope).map_err(|error| RolloutEvaluationError::storage(&error))?,
        request_id,
    )
    .map_err(|error| RolloutEvaluationError::storage(&error))
}

fn pair_request_id(digest: &Sha256Digest) -> Result<RequestId, RolloutEvaluationError> {
    if !canonical_digest(digest) {
        return Err(RolloutEvaluationError::invalid(
            "projected pair digest is invalid",
        ));
    }
    let hashed = Sha256::digest(digest.0.as_bytes());
    let mut first = [0_u8; 16];
    first.copy_from_slice(&hashed[..16]);
    let mut value = u128::from_be_bytes(first);
    let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut suffix = [b'0'; 26];
    for byte in suffix.iter_mut().rev() {
        *byte = alphabet[usize::try_from(value & 31).expect("base32 digit fits")];
        value >>= 5;
    }
    let suffix = std::str::from_utf8(&suffix).map_err(|_| RolloutEvaluationError::corrupt())?;
    Ok(RequestId(format!("req_{suffix}")))
}

fn assignment_stream_id(digest: &Sha256Digest) -> Result<String, RolloutEvaluationError> {
    if !canonical_digest(digest) {
        return Err(RolloutEvaluationError::invalid(
            "evaluation assignment digest is invalid",
        ));
    }
    Ok(format!(
        "rollout-evaluation-assignment:{}",
        digest.0.trim_start_matches("sha256:")
    ))
}

fn slot_stream_id(digest: &Sha256Digest) -> Result<String, RolloutEvaluationError> {
    if !canonical_digest(digest) {
        return Err(RolloutEvaluationError::invalid(
            "evaluation slot digest is invalid",
        ));
    }
    Ok(format!(
        "rollout-evaluation-slot:{}",
        digest.0.trim_start_matches("sha256:")
    ))
}

fn identity_claim_stream_id(digest: &Sha256Digest) -> Result<String, RolloutEvaluationError> {
    if !canonical_digest(digest) {
        return Err(RolloutEvaluationError::invalid(
            "evaluation identity claim digest is invalid",
        ));
    }
    Ok(format!(
        "rollout-evaluation-identity-claim:{}",
        digest.0.trim_start_matches("sha256:")
    ))
}

fn assignment_event_id(digest: &Sha256Digest) -> String {
    format!(
        "rollout-evaluation-assignment-created:{}",
        digest.0.trim_start_matches("sha256:")
    )
}

fn pair_stream_id(digest: &Sha256Digest) -> Result<String, RolloutEvaluationError> {
    if !canonical_digest(digest) {
        return Err(RolloutEvaluationError::invalid(
            "authorized pair digest is invalid",
        ));
    }
    Ok(format!(
        "rollout-evaluation-pair:{}",
        digest.0.trim_start_matches("sha256:")
    ))
}

fn pair_event_id(digest: &Sha256Digest) -> String {
    format!(
        "rollout-evaluation-pair-recorded:{}",
        digest.0.trim_start_matches("sha256:")
    )
}

fn canonical_digest(digest: &Sha256Digest) -> bool {
    digest
        .0
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn digest_json(value: &impl Serialize) -> Result<Sha256Digest, RolloutEvaluationError> {
    let bytes = serde_json::to_vec(value).map_err(|_| RolloutEvaluationError::corrupt())?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}
