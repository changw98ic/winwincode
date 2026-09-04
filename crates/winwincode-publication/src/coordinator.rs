// SPDX-License-Identifier: Apache-2.0

use std::{collections::VecDeque, fmt};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{DeliveryId, Instant, PublicationId, RequestId, Revision, Sha256Digest};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, NewOutboxEvent,
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, StateCommit,
    StorageError, StorageErrorKind,
};

use crate::facts::{
    MAX_SAFE_INTEGER, PublicationAuthorization, PublicationFactBinding, PublicationResourceFact,
    PublicationResourceKind, PublicationResultFact, PublicationSourceIssue, PublicationTarget,
    canonical_prefixed_id, canonical_sha256, canonical_sha256_json, portable,
};
use crate::operation::{
    PublicationOperation, PublicationOperationKind, PublicationPort, PublicationPortMutation,
    PublicationPortObservation, validate_mutation, validate_observation,
};
use crate::policy::{
    PublicationPolicyAudit, PublicationPolicyContext, PublicationPolicyDecision,
    PublicationPolicyEffect, RepositoryPublicationPolicy,
};
use crate::{
    PublicationEnterpriseAttribution, PublicationMeteringError, PublicationRequester,
    metering::{attribution_mutation, source_mutations, validate_stored_attribution},
};

const PUBLICATION_AGGREGATE_TYPE: &str = "publication";
const PUBLICATION_EVENT_TOPIC: &str = "publication.state.v1";
const INTERNAL_ACTOR_KEY: &[u8] = b"winwincode.publication.system.v1";
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
pub const MAX_PUBLICATION_DETAIL_HISTORY: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationState {
    Pending,
    Publishing,
    Published,
    Cancelled,
    Failed,
}

impl PublicationState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Publishing => "publishing",
            Self::Published => "published",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationStepState {
    Pending,
    Applying,
    Unknown,
    Succeeded,
    Rejected,
}

impl PublicationStepState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applying => "applying",
            Self::Unknown => "unknown",
            Self::Succeeded => "succeeded",
            Self::Rejected => "rejected",
        }
    }

    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::Applying | Self::Unknown)
    }
}

/// Secret-safe current result of one canonical Publication operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationStepDetail {
    kind: PublicationOperationKind,
    state: PublicationStepState,
    outcome_code: Option<String>,
    resource: Option<PublicationResourceFact>,
    remote_write_performed: Option<bool>,
}

impl PublicationStepDetail {
    #[must_use]
    pub const fn kind(&self) -> PublicationOperationKind {
        self.kind
    }

    #[must_use]
    pub const fn state(&self) -> PublicationStepState {
        self.state
    }

    #[must_use]
    pub fn outcome_code(&self) -> Option<&str> {
        self.outcome_code.as_deref()
    }

    #[must_use]
    pub const fn resource(&self) -> Option<&PublicationResourceFact> {
        self.resource.as_ref()
    }

    #[must_use]
    pub const fn remote_write_performed(&self) -> Option<bool> {
        self.remote_write_performed
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        self.state.retryable()
    }
}

/// One verified Publication journal revision reduced to closed public status facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationStatusHistory {
    revision: u64,
    state: PublicationState,
    updated_at_millis: u64,
    steps: Vec<PublicationStepDetail>,
}

impl PublicationStatusHistory {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn state(&self) -> PublicationState {
        self.state
    }

    #[must_use]
    pub const fn updated_at_millis(&self) -> u64 {
        self.updated_at_millis
    }

    #[must_use]
    pub fn steps(&self) -> &[PublicationStepDetail] {
        &self.steps
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(
            self.state,
            PublicationState::Pending | PublicationState::Publishing
        )
    }

    #[must_use]
    pub const fn cancellable(&self) -> bool {
        self.retryable()
    }
}

/// Bounded result of a fully verified Publication state and journal read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationDetail {
    publication: Publication,
    history: Vec<PublicationStatusHistory>,
    history_truncated: bool,
}

impl PublicationDetail {
    #[must_use]
    pub const fn publication(&self) -> &Publication {
        &self.publication
    }

    #[must_use]
    pub fn history(&self) -> &[PublicationStatusHistory] {
        &self.history
    }

    #[must_use]
    pub const fn history_truncated(&self) -> bool {
        self.history_truncated
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublicationStep {
    operation: PublicationOperation,
    state: PublicationStepState,
    resource: Option<PublicationResourceFact>,
    code: Option<String>,
    remote_write_performed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublicationManifest {
    aggregate_type: String,
    publication_id: PublicationId,
    intent_sha256: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Publication {
    id: PublicationId,
    revision: u64,
    state: PublicationState,
    binding: PublicationFactBinding,
    source: PublicationSourceIssue,
    target: PublicationTarget,
    candidate_digest: Sha256Digest,
    candidate_commit_id: String,
    artifact_id: String,
    artifact_digest: Sha256Digest,
    approved_by: String,
    approved_at_millis: u64,
    repository_scope_sha256: Sha256Digest,
    #[serde(rename = "publicationSetSha256")]
    set_sha256: Sha256Digest,
    provider_idempotency_key: String,
    intent_sha256: Sha256Digest,
    steps: Vec<PublicationStep>,
    resource: Option<PublicationResourceFact>,
    cancellation_reason: Option<String>,
    updated_at_millis: u64,
}

impl Publication {
    fn initial(
        command: &PublicationPublishCommand,
        authorization: &PublicationAuthorization,
        occurred_at_millis: u64,
    ) -> Result<Self, PublicationError> {
        let operations = PublicationOperation::ordered(authorization);
        let intent_sha256 =
            publication_intent_sha256(&command.publication_id, authorization, &operations);
        let value = Self {
            id: command.publication_id.clone(),
            revision: 1,
            state: PublicationState::Pending,
            binding: authorization.binding.clone(),
            source: authorization.source.clone(),
            target: authorization.target.clone(),
            candidate_digest: authorization.candidate_digest.clone(),
            candidate_commit_id: authorization.candidate_commit_id.clone(),
            artifact_id: authorization.artifact_id.clone(),
            artifact_digest: authorization.artifact_digest.clone(),
            approved_by: authorization.approved_by.clone(),
            approved_at_millis: authorization.approved_at_millis,
            repository_scope_sha256: authorization.repository_scope_sha256.clone(),
            set_sha256: authorization.publication_set_sha256.clone(),
            provider_idempotency_key: authorization.provider_idempotency_key.clone(),
            intent_sha256,
            steps: operations
                .into_iter()
                .map(|operation| PublicationStep {
                    operation,
                    state: PublicationStepState::Pending,
                    resource: None,
                    code: None,
                    remote_write_performed: None,
                })
                .collect(),
            resource: None,
            cancellation_reason: None,
            updated_at_millis: occurred_at_millis,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), PublicationError> {
        let authorization = self.authorization();
        authorization
            .validate()
            .map_err(PublicationError::corrupt)?;
        let expected_operations = PublicationOperation::ordered(&authorization);
        self.validate_identity(&authorization, &expected_operations)?;
        self.validate_steps(&expected_operations)?;
        self.validate_state()?;
        Ok(())
    }

    fn validate_identity(
        &self,
        authorization: &PublicationAuthorization,
        expected_operations: &[PublicationOperation],
    ) -> Result<(), PublicationError> {
        if !canonical_prefixed_id(&self.id.0, "pub_")
            || self.revision == 0
            || self.revision > MAX_SAFE_INTEGER
            || !canonical_sha256(&self.candidate_digest)
            || !canonical_sha256(&self.artifact_digest)
            || !canonical_sha256(&self.repository_scope_sha256)
            || !canonical_sha256(&self.set_sha256)
            || !canonical_sha256(&self.intent_sha256)
            || self.approved_at_millis == 0
            || self.approved_at_millis > self.updated_at_millis
            || self.updated_at_millis > MAX_SAFE_INTEGER
            || self.steps.len() != 4
            || self.intent_sha256
                != publication_intent_sha256(&self.id, authorization, expected_operations)
        {
            return Err(PublicationError::corrupt(
                "stored publication identity is invalid",
            ));
        }
        Ok(())
    }

    fn validate_steps(
        &self,
        expected_operations: &[PublicationOperation],
    ) -> Result<(), PublicationError> {
        let mut incomplete_seen = false;
        for (index, expected) in [
            PublicationOperationKind::Branch,
            PublicationOperationKind::PullRequest,
            PublicationOperationKind::IssueComment,
            PublicationOperationKind::CommitStatus,
        ]
        .into_iter()
        .enumerate()
        {
            let step = &self.steps[index];
            step.operation
                .validate()
                .map_err(PublicationError::corrupt)?;
            if step.operation.kind() != expected
                || step.operation != expected_operations[index]
                || incomplete_seen && step.state != PublicationStepState::Pending
                || !valid_step_result(step)
            {
                return Err(PublicationError::corrupt(
                    "stored publication operation is invalid",
                ));
            }
            incomplete_seen |= step.state != PublicationStepState::Succeeded;
        }
        let pull_request = self.steps[1].resource.as_ref();
        if self.resource.as_ref() != pull_request
            || self.resource.as_ref().is_some_and(|resource| {
                resource.kind() != PublicationResourceKind::GitHubPullRequest
                    || resource.repository() != self.target.repository()
            })
        {
            return Err(PublicationError::corrupt(
                "stored publication resource is outside the target",
            ));
        }
        Ok(())
    }

    fn validate_state(&self) -> Result<(), PublicationError> {
        match self.state {
            PublicationState::Pending => {
                if self
                    .steps
                    .iter()
                    .any(|step| step.state != PublicationStepState::Pending)
                {
                    return Err(PublicationError::corrupt(
                        "pending publication contains operation progress",
                    ));
                }
            }
            PublicationState::Published => {
                if self
                    .steps
                    .iter()
                    .any(|step| step.state != PublicationStepState::Succeeded)
                    || self.resource.is_none()
                {
                    return Err(PublicationError::corrupt(
                        "published publication is incomplete",
                    ));
                }
            }
            PublicationState::Failed => {
                if self
                    .steps
                    .iter()
                    .all(|step| step.state != PublicationStepState::Rejected)
                {
                    return Err(PublicationError::corrupt(
                        "failed publication has no rejected operation",
                    ));
                }
            }
            PublicationState::Cancelled => {
                if self
                    .cancellation_reason
                    .as_deref()
                    .is_none_or(|reason| !bounded_text(reason, 2_000))
                {
                    return Err(PublicationError::corrupt(
                        "cancelled publication has no reason",
                    ));
                }
            }
            PublicationState::Publishing => {}
        }
        if self.state != PublicationState::Cancelled && self.cancellation_reason.is_some() {
            return Err(PublicationError::corrupt(
                "active publication contains a cancellation reason",
            ));
        }
        Ok(())
    }

    fn authorization(&self) -> PublicationAuthorization {
        PublicationAuthorization {
            binding: self.binding.clone(),
            source: self.source.clone(),
            target: self.target.clone(),
            candidate_digest: self.candidate_digest.clone(),
            candidate_commit_id: self.candidate_commit_id.clone(),
            artifact_id: self.artifact_id.clone(),
            artifact_digest: self.artifact_digest.clone(),
            approved_by: self.approved_by.clone(),
            approved_at_millis: self.approved_at_millis,
            repository_scope_sha256: self.repository_scope_sha256.clone(),
            publication_set_sha256: self.set_sha256.clone(),
            provider_idempotency_key: self.provider_idempotency_key.clone(),
        }
    }

    #[must_use]
    pub const fn id(&self) -> &PublicationId {
        &self.id
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn state(&self) -> PublicationState {
        self.state
    }

    #[must_use]
    pub const fn binding(&self) -> &PublicationFactBinding {
        &self.binding
    }

    #[must_use]
    pub const fn target(&self) -> &PublicationTarget {
        &self.target
    }

    #[must_use]
    pub fn approved_by(&self) -> &str {
        &self.approved_by
    }

    #[must_use]
    pub const fn approved_at_millis(&self) -> u64 {
        self.approved_at_millis
    }

    #[must_use]
    pub const fn repository_scope_sha256(&self) -> &Sha256Digest {
        &self.repository_scope_sha256
    }

    #[must_use]
    pub const fn publication_set_sha256(&self) -> &Sha256Digest {
        &self.set_sha256
    }

    #[must_use]
    pub const fn resource(&self) -> Option<&PublicationResourceFact> {
        self.resource.as_ref()
    }

    #[must_use]
    pub fn steps(&self) -> Vec<PublicationStepDetail> {
        self.steps.iter().map(publication_step_detail).collect()
    }

    #[must_use]
    pub fn cancellation_reason(&self) -> Option<&str> {
        self.cancellation_reason.as_deref()
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(
            self.state,
            PublicationState::Pending | PublicationState::Publishing
        )
    }

    #[must_use]
    pub const fn cancellable(&self) -> bool {
        self.retryable()
    }

    #[must_use]
    pub const fn updated_at_millis(&self) -> u64 {
        self.updated_at_millis
    }

    /// Builds the secret-safe projection fact owned by the publication ledger.
    ///
    /// # Errors
    ///
    /// Rejects a revision or timestamp outside the generated public contract.
    pub fn result_fact(&self) -> Result<PublicationResultFact, PublicationError> {
        let revision = i64::try_from(self.revision).map_err(|_| {
            PublicationError::corrupt("publication revision exceeds the public range")
        })?;
        let updated_at = millis_to_instant(self.updated_at_millis)?;
        PublicationResultFact::try_new(
            self.id.clone(),
            Revision(revision),
            self.state.as_str(),
            updated_at,
            self.binding.clone(),
            self.set_sha256.clone(),
            self.resource.clone(),
        )
        .map_err(PublicationError::corrupt)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationPublishCommand {
    publication_id: PublicationId,
    delivery_id: DeliveryId,
    candidate_digest: Sha256Digest,
    target: PublicationTarget,
}

impl PublicationPublishCommand {
    /// Builds one closed publish command from canonical public identifiers.
    ///
    /// # Errors
    ///
    /// Rejects malformed publication, Delivery, candidate, or target identity.
    pub fn try_new(
        publication_id: PublicationId,
        delivery_id: DeliveryId,
        candidate_digest: Sha256Digest,
        target: PublicationTarget,
    ) -> Result<Self, PublicationError> {
        target.validate().map_err(PublicationError::invalid)?;
        if !canonical_prefixed_id(&publication_id.0, "pub_")
            || !canonical_prefixed_id(&delivery_id.0, "dlv_")
            || !canonical_sha256(&candidate_digest)
        {
            return Err(PublicationError::invalid(
                "publication command identity is invalid",
            ));
        }
        Ok(Self {
            publication_id,
            delivery_id,
            candidate_digest,
            target,
        })
    }

    #[must_use]
    pub const fn publication_id(&self) -> &PublicationId {
        &self.publication_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationCancelCommand {
    publication_id: PublicationId,
    reason: String,
}

impl PublicationCancelCommand {
    /// Builds one closed cancellation command.
    ///
    /// # Errors
    ///
    /// Rejects a malformed publication identifier or cancellation reason.
    pub fn try_new(
        publication_id: PublicationId,
        reason: impl Into<String>,
    ) -> Result<Self, PublicationError> {
        let reason = reason.into();
        if !canonical_prefixed_id(&publication_id.0, "pub_") || !bounded_text(&reason, 2_000) {
            return Err(PublicationError::invalid(
                "publication cancellation is invalid",
            ));
        }
        Ok(Self {
            publication_id,
            reason,
        })
    }

    #[must_use]
    pub const fn publication_id(&self) -> &PublicationId {
        &self.publication_id
    }

    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationCommandContext {
    receipt_identity: ReceiptIdentity,
    command_digest: Sha256Digest,
    expected_revision: u64,
    occurred_at_millis: u64,
}

impl PublicationCommandContext {
    /// Builds the durable receipt and optimistic-revision context for one command.
    ///
    /// # Errors
    ///
    /// Rejects a malformed command digest, revision, or timestamp.
    pub fn try_new(
        receipt_identity: ReceiptIdentity,
        command_digest: Sha256Digest,
        expected_revision: u64,
        occurred_at_millis: u64,
    ) -> Result<Self, PublicationError> {
        if !canonical_sha256(&command_digest)
            || expected_revision > MAX_SAFE_INTEGER
            || occurred_at_millis == 0
            || occurred_at_millis > MAX_SAFE_INTEGER
        {
            return Err(PublicationError::invalid(
                "publication command context is invalid",
            ));
        }
        Ok(Self {
            receipt_identity,
            command_digest,
            expected_revision,
            occurred_at_millis,
        })
    }

    #[must_use]
    pub const fn receipt_identity(&self) -> &ReceiptIdentity {
        &self.receipt_identity
    }

    #[must_use]
    pub const fn command_digest(&self) -> &Sha256Digest {
        &self.command_digest
    }

    #[must_use]
    pub const fn expected_revision(&self) -> u64 {
        self.expected_revision
    }

    #[must_use]
    pub const fn occurred_at_millis(&self) -> u64 {
        self.occurred_at_millis
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationErrorKind {
    InvalidInput,
    StaleAuthority,
    PolicyDenied,
    AuditUnavailable,
    RequestConflict,
    RevisionConflict,
    AlreadyExists,
    NotFound,
    WrongState,
    PortContract,
    Storage,
    Corrupt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationError {
    kind: PublicationErrorKind,
    message: String,
    policy_decision: Option<Box<PublicationPolicyDecision>>,
}

impl PublicationError {
    fn new(kind: PublicationErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            policy_decision: None,
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(PublicationErrorKind::InvalidInput, message)
    }

    fn stale(message: impl Into<String>) -> Self {
        Self::new(PublicationErrorKind::StaleAuthority, message)
    }

    fn corrupt(message: impl Into<String>) -> Self {
        Self::new(PublicationErrorKind::Corrupt, message)
    }

    fn policy_denied(decision: PublicationPolicyDecision) -> Self {
        Self {
            kind: PublicationErrorKind::PolicyDenied,
            message: format!("publication denied by {}", decision.rule().as_str()),
            policy_decision: Some(Box::new(decision)),
        }
    }

    fn audit_unavailable() -> Self {
        Self::new(
            PublicationErrorKind::AuditUnavailable,
            "publication policy audit is unavailable",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> PublicationErrorKind {
        self.kind
    }

    #[must_use]
    pub fn policy_decision(&self) -> Option<&PublicationPolicyDecision> {
        self.policy_decision.as_deref()
    }
}

impl fmt::Display for PublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PublicationError {}

impl From<StorageError> for PublicationError {
    fn from(error: StorageError) -> Self {
        let kind = match error.kind() {
            StorageErrorKind::InvalidInput => PublicationErrorKind::InvalidInput,
            StorageErrorKind::RevisionConflict | StorageErrorKind::JournalConflict => {
                PublicationErrorKind::RevisionConflict
            }
            StorageErrorKind::RequestConflict => PublicationErrorKind::RequestConflict,
            StorageErrorKind::JournalAlreadyExists => PublicationErrorKind::AlreadyExists,
            StorageErrorKind::JournalNotFound => PublicationErrorKind::NotFound,
            StorageErrorKind::RequestReplayMissing
            | StorageErrorKind::EventCursorExpired
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => PublicationErrorKind::Storage,
        };
        Self::new(kind, error.to_string())
    }
}

/// Durable Publication ledger backed by the canonical product-state store.
///
/// The wrapper keeps generic state, journal, receipt, and outbox details inside this crate.
pub struct PublicationLedger<'storage> {
    storage: &'storage mut dyn ProductStateStorage,
}

/// Read-only view of the canonical Publication ledger.
///
/// This view delegates to the same state-and-journal verifier as the mutable
/// coordinator. Projection adapters therefore cannot decode a second copy of
/// the Publication format or treat an unverified state row as authority.
pub struct PublicationReadLedger<'storage> {
    storage: &'storage dyn ProductStateStorage,
}

impl<'storage> PublicationReadLedger<'storage> {
    #[must_use]
    pub const fn new(storage: &'storage dyn ProductStateStorage) -> Self {
        Self { storage }
    }

    /// Reads one Publication after validating its complete durable journal.
    ///
    /// # Errors
    ///
    /// Rejects a missing, malformed, or internally inconsistent Publication,
    /// or an unavailable canonical storage adapter.
    pub fn get(&self, publication_id: &PublicationId) -> Result<Publication, PublicationError> {
        load_publication(self.storage, publication_id)
    }

    /// Reads one Publication and returns only the newest bounded history after
    /// validating every durable journal record.
    ///
    /// # Errors
    ///
    /// Rejects the same missing, malformed, or inconsistent facts as `get`.
    pub fn detail(
        &self,
        publication_id: &PublicationId,
    ) -> Result<PublicationDetail, PublicationError> {
        let (publication, verified_history, history_truncated) =
            load_publication_history(self.storage, publication_id, MAX_PUBLICATION_DETAIL_HISTORY)?;
        let history = verified_history
            .into_iter()
            .map(|value| PublicationStatusHistory {
                revision: value.revision,
                state: value.state,
                updated_at_millis: value.updated_at_millis,
                steps: value.steps.iter().map(publication_step_detail).collect(),
            })
            .collect();
        Ok(PublicationDetail {
            publication,
            history,
            history_truncated,
        })
    }

    /// Replays the exact Publication created by one durable command receipt.
    ///
    /// # Errors
    ///
    /// Rejects a changed command digest, malformed receipt, or unavailable
    /// canonical storage. A missing receipt returns `None`.
    pub fn replay(
        &self,
        identity: &ReceiptIdentity,
        digest: &Sha256Digest,
    ) -> Result<Option<Publication>, PublicationError> {
        self.storage
            .load_receipt(identity, digest)
            .map_err(PublicationError::from)?
            .map(|receipt| publication_from_receipt(&receipt))
            .transpose()
    }
}

impl<'storage> PublicationLedger<'storage> {
    #[must_use]
    pub fn new(storage: &'storage mut dyn ProductStateStorage) -> Self {
        Self { storage }
    }

    fn replay(
        &self,
        identity: &ReceiptIdentity,
        digest: &Sha256Digest,
    ) -> Result<Option<Publication>, PublicationError> {
        self.storage
            .load_receipt(identity, digest)
            .map_err(PublicationError::from)?
            .map(|receipt| publication_from_receipt(&receipt))
            .transpose()
    }

    fn load(&self, publication_id: &PublicationId) -> Result<Publication, PublicationError> {
        load_publication(self.storage, publication_id)
    }

    fn exists(&self, publication_id: &PublicationId) -> Result<bool, PublicationError> {
        self.storage
            .load_state(&publication_stream_id(publication_id))
            .map(|state| state.is_some())
            .map_err(PublicationError::from)
    }

    fn create(
        &mut self,
        context: &PublicationCommandContext,
        publication: &Publication,
        attribution: &PublicationEnterpriseAttribution,
    ) -> Result<Publication, PublicationError> {
        let state = encode_publication(publication)?;
        let event = durable_event(publication, &state);
        let manifest = serde_json::to_vec(&PublicationManifest {
            aggregate_type: PUBLICATION_AGGREGATE_TYPE.to_owned(),
            publication_id: publication.id.clone(),
            intent_sha256: publication.intent_sha256.clone(),
        })
        .map_err(|_| PublicationError::corrupt("publication manifest cannot be encoded"))?;
        let journal = AggregateJournalPublication::Create {
            key: journal_key(&publication.id)?,
            manifest,
            first_record: AggregateJournalRecord::new(
                1,
                canonical_sha256_bytes(&state).0,
                state.clone(),
            ),
        };
        let commit = StateCommit::new(
            context.receipt_identity.clone(),
            context.command_digest.clone(),
            publication_stream_id(&publication.id),
            0,
            state,
            vec![event],
        )
        .with_journal_publication(journal)
        .with_state_mutation(
            attribution_mutation(&publication.id, attribution)
                .map_err(publication_metering_error)?,
        );
        match self.storage.commit(&commit) {
            Ok(receipt) => publication_from_receipt(&receipt),
            Err(error)
                if matches!(
                    error.kind(),
                    StorageErrorKind::JournalAlreadyExists | StorageErrorKind::RevisionConflict
                ) =>
            {
                self.replay(&context.receipt_identity, &context.command_digest)?
                    .map_or_else(|| Err(PublicationError::from(error)), Ok)
            }
            Err(error) => Err(PublicationError::from(error)),
        }
    }

    fn append(
        &mut self,
        current: &Publication,
        next: &Publication,
        command_receipt: Option<(&ReceiptIdentity, &Sha256Digest)>,
        metering_operation: Option<&PublicationOperation>,
    ) -> Result<Publication, PublicationError> {
        let state = encode_publication(next)?;
        let event = durable_event(next, &state);
        let event_digest = canonical_sha256_bytes(&state);
        let (receipt_identity, command_digest) = command_receipt.map_or_else(
            || {
                internal_receipt_identity(next, &event_digest)
                    .map(|identity| (identity, event_digest.clone()))
            },
            |(identity, digest)| Ok((identity.clone(), digest.clone())),
        )?;
        for _attempt in 0..64 {
            if let Some(replay) = self.replay(&receipt_identity, &command_digest)? {
                return Ok(replay);
            }
            let journal = self
                .storage
                .load_journal(&journal_key(&next.id)?)
                .map_err(PublicationError::from)?
                .ok_or_else(|| PublicationError::corrupt("publication journal is missing"))?;
            let tail = journal
                .records
                .last()
                .ok_or_else(|| PublicationError::corrupt("publication journal is empty"))?;
            if tail.sequence != current.revision || next.revision != current.revision + 1 {
                return Err(PublicationError::corrupt(
                    "publication state and journal revision differ",
                ));
            }
            let mut commit = StateCommit::new(
                receipt_identity.clone(),
                command_digest.clone(),
                publication_stream_id(&next.id),
                current.revision,
                state.clone(),
                vec![event.clone()],
            )
            .with_journal_publication(AggregateJournalPublication::Append {
                key: journal_key(&next.id)?,
                expected_tail_sequence: tail.sequence,
                expected_tail_digest: tail.digest.clone(),
                record: AggregateJournalRecord::new(
                    next.revision,
                    event_digest.0.clone(),
                    state.clone(),
                ),
            });
            if let Some(operation) = metering_operation {
                for mutation in source_mutations(
                    self.storage,
                    &next.id,
                    operation,
                    millis_to_instant(next.updated_at_millis)?,
                )
                .map_err(publication_metering_error)?
                {
                    commit = commit.with_state_mutation(mutation);
                }
            }
            match self.storage.commit(&commit) {
                Ok(receipt) => return publication_from_receipt(&receipt),
                Err(error)
                    if matches!(
                        error.kind(),
                        StorageErrorKind::RevisionConflict | StorageErrorKind::JournalConflict
                    ) => {}
                Err(error) => return Err(PublicationError::from(error)),
            }
        }
        Err(PublicationError::from(StorageError::adapter(
            "Publication metering transaction retry was exhausted",
        )))
    }

    fn validate_attribution(
        &self,
        publication_id: &PublicationId,
        expected: &PublicationEnterpriseAttribution,
    ) -> Result<(), PublicationError> {
        validate_stored_attribution(self.storage, publication_id, expected)
            .map_err(publication_metering_error)
    }

    fn replay_or_already_exists(
        &self,
        context: &PublicationCommandContext,
    ) -> Result<Publication, PublicationError> {
        self.replay(&context.receipt_identity, &context.command_digest)?
            .ok_or_else(|| {
                PublicationError::new(
                    PublicationErrorKind::AlreadyExists,
                    "publication identity already exists",
                )
            })
    }
}

pub struct PublicationCoordinator<'storage, 'port, 'audit> {
    ledger: PublicationLedger<'storage>,
    port: &'port mut dyn PublicationPort,
    audit: Box<dyn PublicationPolicyAudit + 'audit>,
}

enum ApplyProgress {
    Continue(Publication),
    Stop(Publication),
}

impl<'storage, 'port, 'audit> PublicationCoordinator<'storage, 'port, 'audit> {
    #[must_use]
    pub fn new(
        ledger: PublicationLedger<'storage>,
        port: &'port mut dyn PublicationPort,
        audit: Box<dyn PublicationPolicyAudit + 'audit>,
    ) -> Self {
        Self {
            ledger,
            port,
            audit,
        }
    }

    /// Persists one immutable publication intent before any provider operation.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, conflicting request identity, duplicate publication identity,
    /// invalid storage state, or an atomic storage failure.
    pub fn publish(
        &mut self,
        context: &PublicationCommandContext,
        command: &PublicationPublishCommand,
        authorization: &PublicationAuthorization,
        attribution: &PublicationEnterpriseAttribution,
        policy_context: &PublicationPolicyContext,
        policy: &RepositoryPublicationPolicy,
    ) -> Result<Publication, PublicationError> {
        if let Some(publication) = self
            .ledger
            .replay(&context.receipt_identity, &context.command_digest)?
        {
            self.ledger
                .validate_attribution(publication.id(), attribution)?;
            return Ok(publication);
        }
        validate_publish(context, command, authorization)?;
        validate_enterprise_attribution(command, authorization, attribution, policy_context)?;
        if policy_context.request_id() != context.receipt_identity().request_id()
            || policy_context.evidence().observed_at_millis() != context.occurred_at_millis()
        {
            return Err(PublicationError::stale(
                "publication policy context does not match the publish command",
            ));
        }
        self.authorize(
            command.publication_id(),
            authorization,
            policy_context,
            policy,
        )?;
        let publication = Publication::initial(command, authorization, context.occurred_at_millis)?;
        if self.ledger.exists(&publication.id)? {
            return self.replay_or_already_exists(context);
        }
        self.ledger.create(context, &publication, attribution)
    }

    /// Reads one publication only after validating its full durable journal.
    ///
    /// # Errors
    ///
    /// Rejects a missing, malformed, or internally inconsistent publication.
    pub fn get(&self, publication_id: &PublicationId) -> Result<Publication, PublicationError> {
        self.ledger.load(publication_id)
    }

    /// Reconciles the next durable operation with the provider, in canonical order.
    ///
    /// # Errors
    ///
    /// Rejects invalid durable state, provider contract violations, or storage failures.
    pub fn resume(
        &mut self,
        publication_id: &PublicationId,
        occurred_at_millis: u64,
        policy_context: &PublicationPolicyContext,
        policy: &RepositoryPublicationPolicy,
    ) -> Result<Publication, PublicationError> {
        let mut current = self.ledger.load(publication_id)?;
        self.authorize_resume(&current, occurred_at_millis, policy_context, policy)?;
        if matches!(
            current.state,
            PublicationState::Published | PublicationState::Cancelled | PublicationState::Failed
        ) {
            return Ok(current);
        }
        if current.state == PublicationState::Pending {
            current = self.transition(&current, occurred_at_millis, |next| {
                next.state = PublicationState::Publishing;
                Ok(())
            })?;
        }
        loop {
            let Some(index) = current
                .steps
                .iter()
                .position(|step| step.state != PublicationStepState::Succeeded)
            else {
                return self.transition(&current, occurred_at_millis, |next| {
                    next.state = PublicationState::Published;
                    Ok(())
                });
            };
            let operation = current.steps[index].operation.clone();
            let observation = match self.port.lookup(&operation) {
                Ok(value) => value,
                Err(error) => PublicationPortObservation::unknown(&operation, error.code()),
            };
            validate_observation(&operation, &observation).map_err(|message| {
                PublicationError::new(PublicationErrorKind::PortContract, message)
            })?;
            match observation {
                PublicationPortObservation::Found { resource, .. } => {
                    current = self.transition_found_with_metering(
                        &current,
                        occurred_at_millis,
                        &operation,
                        index,
                        resource,
                    )?;
                }
                PublicationPortObservation::Unknown { code, .. } => {
                    return self.transition(&current, occurred_at_millis, |next| {
                        mark_unknown(next, index, code)
                    });
                }
                PublicationPortObservation::Conflict { code, .. } => {
                    return self.transition(&current, occurred_at_millis, |next| {
                        reject_step(next, index, code)
                    });
                }
                PublicationPortObservation::Absent { .. } => {
                    match self.apply_absent_operation(
                        &current,
                        occurred_at_millis,
                        &operation,
                        index,
                    )? {
                        ApplyProgress::Continue(next) => current = next,
                        ApplyProgress::Stop(next) => return Ok(next),
                    }
                }
            }
        }
    }

    /// Cancels only the Publication aggregate and records an exact replayable receipt.
    ///
    /// # Errors
    ///
    /// Rejects a stale revision, terminal publication, invalid time, or storage failure.
    pub fn cancel(
        &mut self,
        context: &PublicationCommandContext,
        command: &PublicationCancelCommand,
    ) -> Result<Publication, PublicationError> {
        if let Some(publication) = self
            .ledger
            .replay(&context.receipt_identity, &context.command_digest)?
        {
            return Ok(publication);
        }
        let current = self.ledger.load(&command.publication_id)?;
        if context.expected_revision != current.revision {
            return Err(PublicationError::new(
                PublicationErrorKind::RevisionConflict,
                format!(
                    "expected publication revision {}, current {}",
                    context.expected_revision, current.revision
                ),
            ));
        }
        if context.occurred_at_millis < current.updated_at_millis {
            return Err(PublicationError::invalid(
                "publication cancellation time precedes durable progress",
            ));
        }
        if matches!(
            current.state,
            PublicationState::Published | PublicationState::Cancelled | PublicationState::Failed
        ) {
            return Err(PublicationError::new(
                PublicationErrorKind::WrongState,
                "publication is already terminal",
            ));
        }
        self.transition_with_receipt(
            &current,
            context.occurred_at_millis,
            Some((&context.receipt_identity, &context.command_digest)),
            None,
            |next| {
                next.state = PublicationState::Cancelled;
                next.cancellation_reason = Some(command.reason.clone());
                Ok(())
            },
        )
    }

    fn transition(
        &mut self,
        current: &Publication,
        occurred_at_millis: u64,
        change: impl FnOnce(&mut Publication) -> Result<(), PublicationError>,
    ) -> Result<Publication, PublicationError> {
        self.transition_with_receipt(current, occurred_at_millis, None, None, change)
    }

    fn transition_with_metering(
        &mut self,
        current: &Publication,
        occurred_at_millis: u64,
        operation: &PublicationOperation,
        change: impl FnOnce(&mut Publication) -> Result<(), PublicationError>,
    ) -> Result<Publication, PublicationError> {
        self.transition_with_receipt(current, occurred_at_millis, None, Some(operation), change)
    }

    fn transition_found_with_metering(
        &mut self,
        current: &Publication,
        occurred_at_millis: u64,
        operation: &PublicationOperation,
        index: usize,
        resource: Option<PublicationResourceFact>,
    ) -> Result<Publication, PublicationError> {
        validate_resource_target(current, resource.as_ref())?;
        self.transition_with_metering(current, occurred_at_millis, operation, |next| {
            succeed_step(next, index, resource, true)
        })
    }

    fn apply_absent_operation(
        &mut self,
        current: &Publication,
        occurred_at_millis: u64,
        operation: &PublicationOperation,
        index: usize,
    ) -> Result<ApplyProgress, PublicationError> {
        let applying = self.transition(current, occurred_at_millis, |next| {
            let step = &mut next.steps[index];
            step.state = PublicationStepState::Applying;
            step.code = None;
            step.resource = None;
            step.remote_write_performed = None;
            Ok(())
        })?;
        let mutation = self
            .port
            .apply(operation)
            .unwrap_or_else(|error| PublicationPortMutation::unknown(operation, error.code()));
        validate_mutation(operation, &mutation).map_err(|message| {
            PublicationError::new(PublicationErrorKind::PortContract, message)
        })?;
        match mutation {
            PublicationPortMutation::Applied {
                resource,
                remote_write_performed,
                ..
            } => {
                validate_resource_target(&applying, resource.as_ref())?;
                let next = if remote_write_performed {
                    self.transition_with_metering(
                        &applying,
                        occurred_at_millis,
                        operation,
                        |next| succeed_step(next, index, resource, true),
                    )?
                } else {
                    self.transition(&applying, occurred_at_millis, |next| {
                        succeed_step(next, index, resource, false)
                    })?
                };
                Ok(ApplyProgress::Continue(next))
            }
            PublicationPortMutation::Unknown { code, .. } => self
                .transition(&applying, occurred_at_millis, |next| {
                    mark_unknown(next, index, code)
                })
                .map(ApplyProgress::Stop),
            PublicationPortMutation::Rejected { code, .. } => self
                .transition(&applying, occurred_at_millis, |next| {
                    reject_step(next, index, code)
                })
                .map(ApplyProgress::Stop),
        }
    }

    fn transition_with_receipt(
        &mut self,
        current: &Publication,
        occurred_at_millis: u64,
        command_receipt: Option<(&ReceiptIdentity, &Sha256Digest)>,
        metering_operation: Option<&PublicationOperation>,
        change: impl FnOnce(&mut Publication) -> Result<(), PublicationError>,
    ) -> Result<Publication, PublicationError> {
        let mut next = current.clone();
        next.revision = next.revision.checked_add(1).ok_or_else(|| {
            PublicationError::new(
                PublicationErrorKind::RevisionConflict,
                "publication revision is exhausted",
            )
        })?;
        next.updated_at_millis = occurred_at_millis;
        change(&mut next)?;
        next.validate()?;
        self.ledger
            .append(current, &next, command_receipt, metering_operation)
    }

    fn replay_or_already_exists(
        &self,
        context: &PublicationCommandContext,
    ) -> Result<Publication, PublicationError> {
        self.ledger.replay_or_already_exists(context)
    }

    fn authorize(
        &mut self,
        publication_id: &PublicationId,
        authorization: &PublicationAuthorization,
        context: &PublicationPolicyContext,
        policy: &RepositoryPublicationPolicy,
    ) -> Result<(), PublicationError> {
        let decision = policy
            .evaluate(context, publication_id, authorization)
            .map_err(PublicationError::stale)?;
        self.audit
            .record(&decision)
            .map_err(|_| PublicationError::audit_unavailable())?;
        if decision.effect() == PublicationPolicyEffect::Deny {
            return Err(PublicationError::policy_denied(decision));
        }
        Ok(())
    }

    fn authorize_resume(
        &mut self,
        publication: &Publication,
        occurred_at_millis: u64,
        context: &PublicationPolicyContext,
        policy: &RepositoryPublicationPolicy,
    ) -> Result<(), PublicationError> {
        if occurred_at_millis < publication.updated_at_millis
            || occurred_at_millis > MAX_SAFE_INTEGER
        {
            return Err(PublicationError::invalid(
                "publication resume time precedes durable progress",
            ));
        }
        if context.evidence().observed_at_millis() != occurred_at_millis {
            return Err(PublicationError::stale(
                "publication policy context does not match the resume command",
            ));
        }
        self.authorize(
            publication.id(),
            &publication.authorization(),
            context,
            policy,
        )
    }
}

fn validate_enterprise_attribution(
    command: &PublicationPublishCommand,
    authorization: &PublicationAuthorization,
    attribution: &PublicationEnterpriseAttribution,
    policy_context: &PublicationPolicyContext,
) -> Result<(), PublicationError> {
    let PublicationRequester::User(requester) = policy_context.requester() else {
        return Err(PublicationError::stale(
            "Publication enterprise attribution requires the original User",
        ));
    };
    let scope = policy_context.scope();
    if attribution.delivery_id() != &command.delivery_id
        || attribution.delivery_id() != authorization.binding().delivery_id()
        || attribution.organization_id() != scope.organization_id()
        || attribution.workspace_id() != scope.workspace_id()
        || attribution.project_id() != scope.project_id()
        || attribution.repository_id() != scope.repository_id()
        || attribution.user_id() != requester
        || authorization.repository_scope_sha256() != &scope.sha256()
    {
        return Err(PublicationError::stale(
            "Publication enterprise attribution does not match the sealed authority",
        ));
    }
    Ok(())
}

fn publication_metering_error(error: PublicationMeteringError) -> PublicationError {
    PublicationError::new(PublicationErrorKind::Corrupt, error.to_string())
}

fn bounded_text(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= maximum
        && !value.chars().any(|character| {
            matches!(character, '\u{0000}'..='\u{0008}' | '\u{000b}' | '\u{000c}' | '\u{000e}'..='\u{001f}' | '\u{007f}')
        })
}

fn valid_step_result(step: &PublicationStep) -> bool {
    match step.state {
        PublicationStepState::Pending | PublicationStepState::Applying => {
            step.resource.is_none() && step.code.is_none() && step.remote_write_performed.is_none()
        }
        PublicationStepState::Unknown | PublicationStepState::Rejected => {
            step.resource.is_none()
                && step.code.as_deref().is_some_and(|code| portable(code, 100))
                && step.remote_write_performed.is_none()
        }
        PublicationStepState::Succeeded => {
            let resource_is_exact = match step.operation.kind() {
                PublicationOperationKind::PullRequest => step
                    .resource
                    .as_ref()
                    .is_some_and(|resource| resource.validate().is_ok()),
                _ => step.resource.is_none(),
            };
            resource_is_exact && step.code.is_none() && step.remote_write_performed.is_some()
        }
    }
}

fn validate_publish(
    context: &PublicationCommandContext,
    command: &PublicationPublishCommand,
    authorization: &PublicationAuthorization,
) -> Result<(), PublicationError> {
    if context.expected_revision != 0 {
        return Err(PublicationError::new(
            PublicationErrorKind::RevisionConflict,
            "new publication expected revision must be zero",
        ));
    }
    if command.delivery_id != *authorization.binding.delivery_id()
        || command.candidate_digest != authorization.candidate_digest
        || command.target != authorization.target
        || context.occurred_at_millis < authorization.approved_at_millis
    {
        return Err(PublicationError::stale(
            "publication command no longer matches the current approved facts",
        ));
    }
    Ok(())
}

fn succeed_step(
    publication: &mut Publication,
    index: usize,
    resource: Option<PublicationResourceFact>,
    remote_write_performed: bool,
) -> Result<(), PublicationError> {
    let step = publication
        .steps
        .get_mut(index)
        .ok_or_else(|| PublicationError::corrupt("publication operation index is unavailable"))?;
    step.state = PublicationStepState::Succeeded;
    step.resource.clone_from(&resource);
    step.code = None;
    step.remote_write_performed = Some(remote_write_performed);
    if step.operation.kind() == PublicationOperationKind::PullRequest {
        publication.resource = resource;
    }
    Ok(())
}

fn mark_unknown(
    publication: &mut Publication,
    index: usize,
    code: String,
) -> Result<(), PublicationError> {
    let step = publication
        .steps
        .get_mut(index)
        .ok_or_else(|| PublicationError::corrupt("publication operation index is unavailable"))?;
    step.state = PublicationStepState::Unknown;
    step.resource = None;
    step.code = Some(code);
    step.remote_write_performed = None;
    Ok(())
}

fn reject_step(
    publication: &mut Publication,
    index: usize,
    code: String,
) -> Result<(), PublicationError> {
    let step = publication
        .steps
        .get_mut(index)
        .ok_or_else(|| PublicationError::corrupt("publication operation index is unavailable"))?;
    step.state = PublicationStepState::Rejected;
    step.resource = None;
    step.code = Some(code);
    step.remote_write_performed = None;
    publication.state = PublicationState::Failed;
    Ok(())
}

fn validate_resource_target(
    publication: &Publication,
    resource: Option<&PublicationResourceFact>,
) -> Result<(), PublicationError> {
    if resource.is_some_and(|value| value.repository() != publication.target.repository()) {
        return Err(PublicationError::new(
            PublicationErrorKind::PortContract,
            "publication provider returned a resource from another repository",
        ));
    }
    Ok(())
}

fn publication_stream_id(publication_id: &PublicationId) -> String {
    format!("publication:{}", publication_id.0)
}

fn journal_key(publication_id: &PublicationId) -> Result<AggregateJournalKey, PublicationError> {
    AggregateJournalKey::new(PUBLICATION_AGGREGATE_TYPE, publication_id.0.clone())
        .map_err(PublicationError::from)
}

fn load_publication(
    storage: &dyn ProductStateStorage,
    publication_id: &PublicationId,
) -> Result<Publication, PublicationError> {
    load_publication_history(storage, publication_id, 0)
        .map(|(publication, _history, _history_truncated)| publication)
}

fn load_publication_history(
    storage: &dyn ProductStateStorage,
    publication_id: &PublicationId,
    history_limit: usize,
) -> Result<(Publication, Vec<Publication>, bool), PublicationError> {
    if !canonical_prefixed_id(&publication_id.0, "pub_") {
        return Err(PublicationError::invalid("publication identity is invalid"));
    }
    let stored = storage
        .load_state(&publication_stream_id(publication_id))
        .map_err(PublicationError::from)?
        .ok_or_else(|| {
            PublicationError::new(PublicationErrorKind::NotFound, "publication does not exist")
        })?;
    let publication = decode_publication(&stored.payload)?;
    if publication.id != *publication_id
        || stored.stream_id != publication_stream_id(publication_id)
        || stored.revision != publication.revision
    {
        return Err(PublicationError::corrupt(
            "stored publication state identity differs from its key",
        ));
    }
    let journal = storage
        .load_journal(&journal_key(publication_id)?)
        .map_err(PublicationError::from)?
        .ok_or_else(|| PublicationError::corrupt("publication journal is missing"))?;
    let manifest: PublicationManifest = serde_json::from_slice(&journal.manifest)
        .map_err(|_| PublicationError::corrupt("publication journal manifest is malformed"))?;
    if manifest.aggregate_type != PUBLICATION_AGGREGATE_TYPE
        || manifest.publication_id != *publication_id
        || manifest.intent_sha256 != publication.intent_sha256
        || journal.records.len() != usize::try_from(publication.revision).unwrap_or(usize::MAX)
    {
        return Err(PublicationError::corrupt(
            "publication journal manifest or length is inconsistent",
        ));
    }
    let mut previous = None;
    let mut history = VecDeque::with_capacity(history_limit);
    let mut history_truncated = false;
    for (index, record) in journal.records.iter().enumerate() {
        let expected_sequence = u64::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| PublicationError::corrupt("publication journal is too large"))?;
        let historical = decode_publication(&record.payload)?;
        if record.sequence != expected_sequence
            || record.digest != canonical_sha256_bytes(&record.payload).0
            || historical.id != *publication_id
            || historical.revision != record.sequence
            || historical.intent_sha256 != manifest.intent_sha256
        {
            return Err(PublicationError::corrupt(
                "publication journal record is inconsistent",
            ));
        }
        if let Some(previous) = previous.as_ref() {
            validate_publication_transition(previous, &historical)?;
        } else if historical.revision != 1 || historical.state != PublicationState::Pending {
            return Err(PublicationError::corrupt(
                "publication journal does not begin with its pending intent",
            ));
        }
        if history_limit > 0 {
            if history.len() == history_limit {
                history.pop_front();
                history_truncated = true;
            }
            history.push_back(historical.clone());
        }
        previous = Some(historical);
    }
    let tail = journal
        .records
        .last()
        .ok_or_else(|| PublicationError::corrupt("publication journal is empty"))?;
    if tail.payload != stored.payload {
        return Err(PublicationError::corrupt(
            "publication state differs from the journal tail",
        ));
    }
    Ok((publication, history.into(), history_truncated))
}

fn validate_publication_transition(
    previous: &Publication,
    next: &Publication,
) -> Result<(), PublicationError> {
    if previous.revision.checked_add(1) != Some(next.revision)
        || next.updated_at_millis < previous.updated_at_millis
        || previous.id != next.id
        || previous.binding != next.binding
        || previous.source != next.source
        || previous.target != next.target
        || previous.candidate_digest != next.candidate_digest
        || previous.candidate_commit_id != next.candidate_commit_id
        || previous.artifact_id != next.artifact_id
        || previous.artifact_digest != next.artifact_digest
        || previous.approved_by != next.approved_by
        || previous.approved_at_millis != next.approved_at_millis
        || previous.repository_scope_sha256 != next.repository_scope_sha256
        || previous.set_sha256 != next.set_sha256
        || previous.provider_idempotency_key != next.provider_idempotency_key
        || previous.intent_sha256 != next.intent_sha256
        || previous.steps.len() != next.steps.len()
        || previous
            .steps
            .iter()
            .zip(&next.steps)
            .any(|(left, right)| left.operation != right.operation)
        || matches!(
            previous.state,
            PublicationState::Published | PublicationState::Cancelled | PublicationState::Failed
        )
    {
        return Err(PublicationError::corrupt(
            "publication journal transition changed immutable facts",
        ));
    }
    let changed_steps = previous
        .steps
        .iter()
        .zip(&next.steps)
        .enumerate()
        .filter_map(|(index, (left, right))| (left != right).then_some(index))
        .collect::<Vec<_>>();
    let first_incomplete = previous
        .steps
        .iter()
        .position(|step| step.state != PublicationStepState::Succeeded);
    let one_current_step_changed = matches!(changed_steps.as_slice(), [index]
        if Some(*index) == first_incomplete
            && next.steps[*index].state != PublicationStepState::Pending
            && previous.steps[*index].state != PublicationStepState::Succeeded);
    let same_progress = changed_steps.is_empty() && previous.resource == next.resource;

    let valid = match next.state {
        PublicationState::Pending => false,
        PublicationState::Publishing => {
            next.cancellation_reason.is_none()
                && match previous.state {
                    PublicationState::Pending => same_progress,
                    PublicationState::Publishing => {
                        same_progress
                            || one_current_step_changed
                                && next.steps[changed_steps[0]].state
                                    != PublicationStepState::Rejected
                    }
                    PublicationState::Published
                    | PublicationState::Cancelled
                    | PublicationState::Failed => false,
                }
        }
        PublicationState::Published => {
            previous.state == PublicationState::Publishing
                && same_progress
                && next.cancellation_reason.is_none()
        }
        PublicationState::Cancelled => {
            matches!(
                previous.state,
                PublicationState::Pending | PublicationState::Publishing
            ) && same_progress
                && previous.cancellation_reason.is_none()
                && next.cancellation_reason.is_some()
        }
        PublicationState::Failed => {
            previous.state == PublicationState::Publishing
                && one_current_step_changed
                && next.steps[changed_steps[0]].state == PublicationStepState::Rejected
                && next.cancellation_reason.is_none()
        }
    };
    if !valid {
        return Err(PublicationError::corrupt(
            "publication journal state transition is invalid",
        ));
    }
    Ok(())
}

fn publication_step_detail(step: &PublicationStep) -> PublicationStepDetail {
    PublicationStepDetail {
        kind: step.operation.kind(),
        state: step.state,
        outcome_code: step.code.clone(),
        resource: step.resource.clone(),
        remote_write_performed: step.remote_write_performed,
    }
}

fn publication_intent_sha256(
    publication_id: &PublicationId,
    authorization: &PublicationAuthorization,
    operations: &[PublicationOperation],
) -> Sha256Digest {
    canonical_sha256_json(&(
        publication_id,
        &authorization.binding,
        &authorization.source,
        &authorization.target,
        &authorization.candidate_digest,
        &authorization.candidate_commit_id,
        &authorization.artifact_id,
        &authorization.artifact_digest,
        &authorization.approved_by,
        authorization.approved_at_millis,
        &authorization.repository_scope_sha256,
        &authorization.publication_set_sha256,
        &authorization.provider_idempotency_key,
        operations,
    ))
}

fn encode_publication(publication: &Publication) -> Result<Vec<u8>, PublicationError> {
    serde_json::to_vec(publication)
        .map_err(|_| PublicationError::corrupt("publication state cannot be encoded"))
}

fn decode_publication(bytes: &[u8]) -> Result<Publication, PublicationError> {
    let publication: Publication = serde_json::from_slice(bytes)
        .map_err(|_| PublicationError::corrupt("publication state is malformed"))?;
    publication.validate()?;
    Ok(publication)
}

fn durable_event(publication: &Publication, state: &[u8]) -> NewOutboxEvent {
    let digest = canonical_sha256_bytes(state);
    NewOutboxEvent::internal(
        format!(
            "publication:event:{}:{}:{}",
            publication.id.0, publication.revision, digest.0
        ),
        PUBLICATION_EVENT_TOPIC,
        state.to_vec(),
    )
}

fn publication_from_receipt(
    receipt: &winwincode_storage::CommitReceipt,
) -> Result<Publication, PublicationError> {
    let [event] = receipt.events.as_slice() else {
        return Err(PublicationError::corrupt(
            "publication receipt must contain one state event",
        ));
    };
    if event.topic != PUBLICATION_EVENT_TOPIC {
        return Err(PublicationError::corrupt(
            "publication receipt event topic is invalid",
        ));
    }
    let publication = decode_publication(&event.payload)?;
    if receipt.stream_id != publication_stream_id(&publication.id)
        || receipt.revision != publication.revision
    {
        return Err(PublicationError::corrupt(
            "publication receipt identity differs from its state",
        ));
    }
    Ok(publication)
}

fn internal_receipt_identity(
    publication: &Publication,
    event_digest: &Sha256Digest,
) -> Result<ReceiptIdentity, PublicationError> {
    let actor = ReceiptActorKey::from_encoded(INTERNAL_ACTOR_KEY.to_vec())
        .map_err(PublicationError::from)?;
    let scope = ReceiptScopeKey::from_encoded(
        format!(
            "winwincode.publication.scope.v1:{}",
            publication.repository_scope_sha256.0
        )
        .into_bytes(),
    )
    .map_err(PublicationError::from)?;
    let request_id = RequestId(derived_request_id(&(
        &publication.id,
        publication.revision,
        event_digest,
    )));
    ReceiptIdentity::new(actor, scope, request_id).map_err(PublicationError::from)
}

fn derived_request_id(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("serializable internal publication request");
    let digest = Sha256::digest(bytes);
    let mut encoded = [b'0'; 26];
    let mut first_128_bits = [0_u8; 16];
    first_128_bits.copy_from_slice(&digest[..16]);
    let mut value = u128::from_be_bytes(first_128_bits);
    for index in (0..encoded.len()).rev() {
        encoded[index] = CROCKFORD_BASE32[(value & 31) as usize];
        value >>= 5;
    }
    format!(
        "req_{}",
        std::str::from_utf8(&encoded).expect("Crockford alphabet is UTF-8")
    )
}

fn canonical_sha256_bytes(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn millis_to_instant(value: u64) -> Result<Instant, PublicationError> {
    let seconds = value / 1_000;
    let millis = value % 1_000;
    let days = i64::try_from(seconds / 86_400)
        .map_err(|_| PublicationError::corrupt("publication time exceeds RFC 3339"))?;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    if !(1970..=9999).contains(&year) {
        return Err(PublicationError::corrupt(
            "publication time exceeds RFC 3339",
        ));
    }
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let text =
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z");
    Ok(Instant(text))
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}
