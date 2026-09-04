// SPDX-License-Identifier: Apache-2.0

//! Generated Delivery application boundary and its durable repository catalog.

use std::{collections::BTreeMap, fmt};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, DeliveryAdvanceCommand, DeliveryAdvanceCompletedResponse,
    DeliveryAdvanceCompletedResponseCommand, DeliveryAdvanceCompletedResponseOutcome,
    DeliveryApproveTaskBreakdownCommand, DeliveryApproveTaskBreakdownCompletedResponse,
    DeliveryApproveTaskBreakdownCompletedResponseCommand,
    DeliveryApproveTaskBreakdownCompletedResponseOutcome, DeliveryCreateCommand,
    DeliveryCreateCompletedResponse, DeliveryCreateCompletedResponseCommand,
    DeliveryCreateCompletedResponseOutcome, DeliveryListQuery, DeliveryListResultResponse,
    DeliveryListResultResponseQuery, DeliveryOwnershipProjection, DeliveryPage, DeliveryPageKind,
    DeliveryProjection, DeliveryResolveAttentionCommand, DeliveryResolveAttentionCompletedResponse,
    DeliveryResolveAttentionCompletedResponseCommand,
    DeliveryResolveAttentionCompletedResponseOutcome, DeliveryStatus as ApiDeliveryStatus,
    DeliverySubmitVerdictCommand, DeliverySubmitVerdictCompletedResponse,
    DeliverySubmitVerdictCompletedResponseCommand, DeliverySubmitVerdictCompletedResponseOutcome,
    DeliveryTaskCountsProjection, DeliveryUpdateSpecCommand, DeliveryUpdateSpecCompletedResponse,
    DeliveryUpdateSpecCompletedResponseCommand, DeliveryUpdateSpecCompletedResponseOutcome,
    ErrorCode, PageInfo, Scope,
};
use winwincode_delivery::{
    application::{
        attention::ResolvedAttentionTransition,
        stage::{StageAdvanceEffect, StageAdvanceResult},
        verdict::SubmitVerdictFacts,
    },
    domain::{
        AttentionItemStatus, Delivery, DeliverySourceRef, DeliveryStatus, DeliveryTaskStatus,
        FrozenDeliveryCandidate, RepositoryRef, StageRunStatus, evidence::ResolvedDeliveryEvidence,
        verification::IndependentVerification,
    },
    store::{DeliveryQuery, DeliveryQueryPort, DeliveryStore},
};
use winwincode_domain::RepositoryScope;
use winwincode_domain::{
    Count, DeliveryId, OpaqueCursor, RequestId, Revision, SchemaVersion, Sha256Digest,
};
use winwincode_storage::{
    CandidateGitTerminalOutcome, CommitReceipt, ProductStateStorage, ProjectionEventStream,
    ProjectionEventStreamKey, StateMutation, StateRevisionGuard, StorageError, StoredState,
};

use crate::{
    CommitError, ControlPlane, DeliveryCommandCommitError, DeliveryCommandFacts, DeliverySpecFacts,
    DeliveryVerdictCommitError,
    delivery_command_transaction::TrustedDeliverySpecFacts,
    delivery_execution::{
        DeliveryExecutionConfig, DeliveryExecutionError, ExecutionJobDispatcher,
        prepare_delivery_advance,
    },
    delivery_transaction::{StagedDeliveryJournal, delivery_journal_key, delivery_stream_id},
};

const DELIVERY_CATALOG_SCHEMA_VERSION: u8 = 1;
const DELIVERY_CATALOG_PREFIX: &str = "delivery-catalog:";
const DELIVERY_LIST_CURSOR_SCHEMA: &str = "winwincode.delivery-list.cursor.v1";
const MAX_CURSOR_BYTES: usize = 4_096;
const MAX_PAGE_SIZE: usize = 200;
const SCAN_PAGE_SIZE: usize = 256;
const MAX_SNAPSHOT_ROWS: usize = 100_000;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeliveryCatalogEntry {
    schema_version: u8,
    repository_scope: RepositoryScope,
    delivery_id: DeliveryId,
}

/// Atomic current Delivery cut used by the collaboration Inbox source adapter.
pub(crate) struct CollaborationDeliverySourceSnapshot {
    pub records: Vec<CollaborationDeliverySourceRecord>,
}

pub(crate) struct CollaborationDeliverySourceRecord {
    pub delivery: Delivery,
    pub state_guards: Vec<StateRevisionGuard>,
}

/// Exact durable Delivery and repository-membership facts used by guarded
/// collaboration writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryAuthoritySeal {
    pub delivery: Delivery,
    pub target_revision: u64,
    pub target_sha256: Sha256Digest,
    pub state_guard: StateRevisionGuard,
    pub scope_guard: StateRevisionGuard,
}

/// Loads one canonical Delivery plus both state guards needed to protect its
/// aggregate and repository membership in another atomic commit.
///
/// # Errors
///
/// Returns not-found, corruption, or storage errors without decoding private
/// catalog keys outside the Delivery owner.
pub fn load_delivery_authority_seal(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
) -> Result<DeliveryAuthoritySeal, DeliveryApplicationError> {
    let membership = load_catalog_membership(storage, scope, delivery_id)?;
    let stream_id = delivery_stream_id(delivery_id);
    let stored = storage.load_state(&stream_id)?.ok_or_else(|| {
        DeliveryApplicationError::ResourceNotFound(format!(
            "Delivery {} was not found",
            delivery_id.0
        ))
    })?;
    let delivery = Delivery::decode_json(&stored.payload).map_err(|error| {
        DeliveryApplicationError::Storage(StorageError::adapter(error.to_string()))
    })?;
    if stored.stream_id != stream_id
        || delivery.id() != delivery_id
        || delivery.revision() != stored.revision
        || delivery.encode_json().map_err(storage_error)? != stored.payload
    {
        return Err(DeliveryApplicationError::Storage(StorageError::adapter(
            "Delivery authority state is corrupt",
        )));
    }
    Ok(DeliveryAuthoritySeal {
        target_revision: delivery.revision(),
        target_sha256: Sha256Digest(digest_bytes(&stored.payload)),
        delivery,
        state_guard: StateRevisionGuard::new(stored.stream_id, stored.revision)?,
        scope_guard: StateRevisionGuard::new(membership.stream_id, membership.revision)?,
    })
}

/// Failure returned by a trusted Delivery authority adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryAuthorityError {
    message: String,
}

impl DeliveryAuthorityError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for DeliveryAuthorityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DeliveryAuthorityError {}

/// Repository and product semantics resolved by a long-lived authority port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliverySpecificationAuthority {
    pub now_millis: u64,
    pub repository: RepositoryRef,
    pub source_ref: Option<DeliverySourceRef>,
    pub scope: Vec<String>,
    pub out_of_scope: Vec<String>,
    pub constraints: Vec<String>,
    pub max_rework_attempts: u64,
    pub criterion_verification_methods: Vec<(String, String)>,
}

/// Sealed stage transition plus the execution configuration required only by
/// a newly selected Codex stage.
#[derive(Clone, Debug, PartialEq)]
pub struct DeliveryAdvanceAuthority {
    pub repository: RepositoryRef,
    pub source_ref: Option<DeliverySourceRef>,
    pub transition: StageAdvanceResult,
    pub execution: Option<DeliveryExecutionConfig>,
    pub(crate) terminal_handoff:
        Option<crate::terminal_outcome_transaction::DeliveryTerminalHandoff>,
}

/// Sealed current-Attention transition returned by the review authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryAttentionAuthority {
    pub repository: RepositoryRef,
    pub source_ref: Option<DeliverySourceRef>,
    pub transition: ResolvedAttentionTransition,
}

/// Candidate, verification and Evidence facts resolved behind the trusted
/// verdict boundary. The HTTP command carries only a stale-check digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryVerdictAuthority {
    pub candidate: FrozenDeliveryCandidate,
    pub verification: IndependentVerification,
    pub evidence: Vec<ResolvedDeliveryEvidence>,
    pub produced_at_millis: u64,
}

/// Read-only command context offered to the installed authority adapter.
pub struct DeliveryAuthorityRequest<'request> {
    command: &'request CommandEnvelope,
    delivery: Option<&'request Delivery>,
}

impl DeliveryAuthorityRequest<'_> {
    #[must_use]
    pub const fn command(&self) -> &CommandEnvelope {
        self.command
    }

    #[must_use]
    pub const fn delivery(&self) -> Option<&Delivery> {
        self.delivery
    }
}

/// Long-lived trusted read boundary used by generated Delivery commands.
/// Transport adapters never receive or construct the sealed transaction facts.
pub trait DeliveryAuthorityPort: Send {
    /// Resolves the repository and specification facts for create or update.
    ///
    /// # Errors
    ///
    /// Returns an error when the trusted source cannot resolve current facts.
    fn specification(
        &mut self,
        request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliverySpecificationAuthority, DeliveryAuthorityError>;

    /// Resolves the next stage transition and any required execution config.
    ///
    /// # Errors
    ///
    /// Returns an error when the trusted source cannot resolve current facts.
    fn advance(
        &mut self,
        request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliveryAdvanceAuthority, DeliveryAuthorityError>;

    /// Resolves the current Attention transition.
    ///
    /// # Errors
    ///
    /// Returns an error when the trusted source cannot resolve current facts.
    fn resolve_attention(
        &mut self,
        request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliveryAttentionAuthority, DeliveryAuthorityError>;

    /// Resolves the current candidate, verification, and Evidence facts.
    ///
    /// # Errors
    ///
    /// Returns an error when the trusted source cannot resolve current facts.
    fn verdict(
        &mut self,
        request: DeliveryAuthorityRequest<'_>,
    ) -> Result<DeliveryVerdictAuthority, DeliveryAuthorityError>;
}

/// Stable application error classes for generated Delivery commands/queries.
#[derive(Debug)]
pub enum DeliveryApplicationError {
    InvalidRequest(String),
    TrustedFactsUnavailable(String),
    ResourceNotFound(String),
    ReadCursorExpired,
    Command(DeliveryCommandCommitError),
    Commit(CommitError),
    Execution(DeliveryExecutionError),
    Verdict(DeliveryVerdictCommitError),
    Storage(StorageError),
}

impl DeliveryApplicationError {
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidRequest(_) => ErrorCode::InvalidRequest,
            Self::TrustedFactsUnavailable(_) => ErrorCode::TrustedFactsUnavailable,
            Self::ResourceNotFound(_) => ErrorCode::ResourceNotFound,
            Self::ReadCursorExpired => ErrorCode::ReadCursorExpired,
            Self::Command(error) => error.public_code(),
            Self::Commit(CommitError::Storage(error)) | Self::Storage(error) => {
                storage_error_code(error)
            }
            Self::Commit(CommitError::PublicationPending { .. })
            | Self::Execution(_)
            | Self::Verdict(_) => ErrorCode::ServiceUnavailable,
        }
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(
            self,
            Self::ReadCursorExpired
                | Self::Commit(CommitError::PublicationPending { .. })
                | Self::Execution(_)
                | Self::Verdict(DeliveryVerdictCommitError::PublicationPending { .. })
        ) || matches!(self, Self::Command(error) if error.retryable())
    }
}

impl fmt::Display for DeliveryApplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message)
            | Self::TrustedFactsUnavailable(message)
            | Self::ResourceNotFound(message) => formatter.write_str(message),
            Self::ReadCursorExpired => formatter.write_str("Delivery list cursor expired"),
            Self::Command(error) => write!(formatter, "{error}"),
            Self::Commit(error) => write!(formatter, "{error}"),
            Self::Execution(error) => write!(formatter, "{error}"),
            Self::Verdict(error) => write!(formatter, "{error}"),
            Self::Storage(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for DeliveryApplicationError {}

impl From<StorageError> for DeliveryApplicationError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeliveryListCursor {
    schema: String,
    scope_sha256: String,
    filter_sha256: String,
    upper_bound_stream_id: String,
    snapshot_sha256: String,
    after_delivery_id: DeliveryId,
}

struct LoadedDeliveryPage {
    items: Vec<DeliveryProjection>,
    has_more: bool,
    snapshot_sha256: String,
    after_seen: bool,
}

impl ControlPlane {
    /// Installs the one long-lived trusted Delivery authority adapter.
    /// Replacing a live adapter is rejected so request retries keep one source.
    ///
    /// # Errors
    ///
    /// Returns an error when an authority adapter is already installed.
    pub fn install_delivery_authority_port(
        &mut self,
        authority: Box<dyn DeliveryAuthorityPort>,
    ) -> Result<(), DeliveryApplicationError> {
        if self.delivery_authority.is_some() {
            return Err(DeliveryApplicationError::InvalidRequest(
                "Delivery authority port is already installed".to_owned(),
            ));
        }
        self.delivery_authority = Some(authority);
        Ok(())
    }

    /// Installs the one durable `ExecutionJob` dispatcher used by Codex stages.
    ///
    /// # Errors
    ///
    /// Returns an error when a dispatcher is already installed.
    pub fn install_delivery_execution_dispatcher(
        &mut self,
        dispatcher: Box<dyn ExecutionJobDispatcher>,
    ) -> Result<(), DeliveryApplicationError> {
        if self.delivery_dispatcher.is_some() {
            return Err(DeliveryApplicationError::InvalidRequest(
                "Delivery execution dispatcher is already installed".to_owned(),
            ));
        }
        self.delivery_dispatcher = Some(dispatcher);
        Ok(())
    }

    /// Executes one generated `delivery.create` through the installed
    /// repository authority and returns the exact committed revision.
    ///
    /// # Errors
    ///
    /// Returns a stable application error when validation, authority lookup,
    /// durable commit, or projection publication fails.
    pub fn delivery_create(
        &mut self,
        command: &DeliveryCreateCommand,
    ) -> Result<DeliveryCreateCompletedResponse, DeliveryApplicationError> {
        let mapped = map_command(
            &command.actor,
            CommandName::DeliveryCreate,
            command.expected_revision.clone(),
            &command.payload,
            &command.request_id,
            &command.schema_version,
            &command.scope,
        )?;
        if let Some((receipt, delivery)) =
            self.delivery_replay(&mapped, &command.scope, &command.payload.delivery_id)?
        {
            return create_response(command, &receipt, &delivery);
        }
        let authority = self.resolve_specification_authority(&mapped, None)?;
        let facts = DeliveryCommandFacts::specification_from_trusted_adapter(
            &mapped,
            command.scope.clone(),
            specification_facts(authority),
        )?;
        let receipt = self
            .commit_delivery_command(&mapped, &facts)
            .map_err(DeliveryApplicationError::Command)?;
        let delivery = load_delivery_revision(
            self.storage_ref()?,
            &command.payload.delivery_id,
            receipt.revision,
        )?;
        create_response(command, &receipt, &delivery)
    }

    /// Executes one generated `delivery.update_spec` using repository facts
    /// resolved after exact scope/catalog validation.
    ///
    /// # Errors
    ///
    /// Returns a stable application error when validation, authority lookup,
    /// durable commit, or projection publication fails.
    pub fn delivery_update_spec(
        &mut self,
        command: &DeliveryUpdateSpecCommand,
    ) -> Result<DeliveryUpdateSpecCompletedResponse, DeliveryApplicationError> {
        let mapped = map_command(
            &command.actor,
            CommandName::DeliveryUpdateSpec,
            command.expected_revision.clone(),
            &command.payload,
            &command.request_id,
            &command.schema_version,
            &command.scope,
        )?;
        if let Some((receipt, delivery)) =
            self.delivery_replay(&mapped, &command.scope, &command.payload.delivery_id)?
        {
            return update_response(command, &receipt, &delivery);
        }
        ensure_catalog_membership(
            self.storage_ref()?,
            &command.scope,
            &command.payload.delivery_id,
        )?;
        let current = load_current_delivery(self.storage_ref()?, &command.payload.delivery_id)?;
        let authority = self.resolve_specification_authority(&mapped, Some(&current))?;
        let facts = DeliveryCommandFacts::specification_from_trusted_adapter(
            &mapped,
            command.scope.clone(),
            specification_facts(authority),
        )?;
        let receipt = self
            .commit_delivery_command(&mapped, &facts)
            .map_err(DeliveryApplicationError::Command)?;
        let delivery = load_delivery_revision(
            self.storage_ref()?,
            &command.payload.delivery_id,
            receipt.revision,
        )?;
        update_response(command, &receipt, &delivery)
    }

    /// Promotes only the task graph already sealed by the durable approved
    /// Solution Review; no caller or authority adapter supplies task fields.
    ///
    /// # Errors
    ///
    /// Returns a stable application error when validation, durable promotion,
    /// or projection publication fails.
    pub fn delivery_approve_task_breakdown(
        &mut self,
        command: &DeliveryApproveTaskBreakdownCommand,
    ) -> Result<DeliveryApproveTaskBreakdownCompletedResponse, DeliveryApplicationError> {
        let mapped = map_command(
            &command.actor,
            CommandName::DeliveryApproveTaskBreakdown,
            command.expected_revision.clone(),
            &command.payload,
            &command.request_id,
            &command.schema_version,
            &command.scope,
        )?;
        if let Some((receipt, delivery)) =
            self.delivery_replay(&mapped, &command.scope, &command.payload.delivery_id)?
        {
            return task_breakdown_response(command, &receipt, &delivery);
        }
        ensure_catalog_membership(
            self.storage_ref()?,
            &command.scope,
            &command.payload.delivery_id,
        )?;
        let receipt = self
            .commit_delivery_task_breakdown(&mapped)
            .map_err(DeliveryApplicationError::Commit)?;
        let delivery = load_delivery_revision(
            self.storage_ref()?,
            &command.payload.delivery_id,
            receipt.revision,
        )?;
        task_breakdown_response(command, &receipt, &delivery)
    }

    /// Executes one generated `delivery.advance` from an authority-sealed
    /// stage transition. Human stages never reach the execution dispatcher.
    ///
    /// # Errors
    ///
    /// Returns a stable application error when validation, authority lookup,
    /// durable commit, execution dispatch, or projection publication fails.
    pub fn delivery_advance(
        &mut self,
        command: &DeliveryAdvanceCommand,
    ) -> Result<DeliveryAdvanceCompletedResponse, DeliveryApplicationError> {
        let mapped = map_command(
            &command.actor,
            CommandName::DeliveryAdvance,
            command.expected_revision.clone(),
            &command.payload,
            &command.request_id,
            &command.schema_version,
            &command.scope,
        )?;
        if let Some((receipt, delivery)) =
            self.delivery_replay(&mapped, &command.scope, &command.payload.delivery_id)?
        {
            return advance_response(command, receipt.revision, &delivery);
        }
        ensure_catalog_membership(
            self.storage_ref()?,
            &command.scope,
            &command.payload.delivery_id,
        )?;
        let current = load_current_delivery(self.storage_ref()?, &command.payload.delivery_id)?;
        let authority = self.resolve_advance_authority(&mapped, &current)?;
        if authority.transition.delivery.id() != &command.payload.delivery_id {
            return Err(DeliveryApplicationError::TrustedFactsUnavailable(
                "Delivery advance authority returned another Delivery".to_owned(),
            ));
        }
        let revision = self.commit_advance_authority(&mapped, authority)?;
        let delivery =
            load_delivery_revision(self.storage_ref()?, &command.payload.delivery_id, revision)?;
        advance_response(command, revision, &delivery)
    }

    fn commit_advance_authority(
        &mut self,
        command: &CommandEnvelope,
        authority: DeliveryAdvanceAuthority,
    ) -> Result<u64, DeliveryApplicationError> {
        let DeliveryAdvanceAuthority {
            repository,
            source_ref,
            transition,
            execution,
            terminal_handoff,
        } = authority;
        let Scope::RepositoryScope(scope) = &command.scope else {
            return Err(DeliveryApplicationError::InvalidRequest(
                "Delivery advance requires repository scope".to_owned(),
            ));
        };
        match &transition.effect {
            StageAdvanceEffect::Review(_) => {
                if execution.is_some() {
                    return Err(DeliveryApplicationError::TrustedFactsUnavailable(
                        "human Delivery stage unexpectedly carried execution configuration"
                            .to_owned(),
                    ));
                }
                let facts = DeliveryCommandFacts::advance_from_trusted_adapter(
                    command,
                    scope.clone(),
                    repository,
                    source_ref,
                    transition,
                )?;
                let commit = if let Some(handoff) = &terminal_handoff {
                    self.commit_delivery_command_with_handoff(command, &facts, handoff)
                } else {
                    self.commit_delivery_command(command, &facts)
                };
                Ok(commit.map_err(DeliveryApplicationError::Command)?.revision)
            }
            StageAdvanceEffect::Dispatch(_) => {
                let config = execution.ok_or_else(|| {
                    DeliveryApplicationError::TrustedFactsUnavailable(
                        "Codex Delivery stage has no trusted execution configuration".to_owned(),
                    )
                })?;
                let pending =
                    prepare_delivery_advance(command.request_id.clone(), transition, config)
                        .map_err(DeliveryApplicationError::Execution)?;
                let mut dispatcher = self.delivery_dispatcher.take().ok_or_else(|| {
                    DeliveryApplicationError::TrustedFactsUnavailable(
                        "Delivery execution dispatcher is not installed".to_owned(),
                    )
                })?;
                let result = if let Some(handoff) = &terminal_handoff {
                    self.commit_delivery_execution_with_handoff(
                        command,
                        &pending,
                        dispatcher.as_mut(),
                        handoff,
                    )
                } else {
                    self.commit_delivery_execution(command, &pending, dispatcher.as_mut())
                };
                self.delivery_dispatcher = Some(dispatcher);
                Ok(result
                    .map_err(DeliveryApplicationError::Execution)?
                    .commit
                    .committed_revision)
            }
            StageAdvanceEffect::Clarify(_) => {
                if execution.is_some() || terminal_handoff.is_some() {
                    return Err(DeliveryApplicationError::TrustedFactsUnavailable(
                        "clarification transition unexpectedly carried execution configuration"
                            .to_owned(),
                    ));
                }
                Ok(self
                    .commit_delivery_rework_clarification(command, &transition)
                    .map_err(DeliveryApplicationError::Commit)?
                    .revision)
            }
            StageAdvanceEffect::Resume(_) => {
                Err(DeliveryApplicationError::TrustedFactsUnavailable(
                    "delivery.advance cannot replace runtime recovery authority".to_owned(),
                ))
            }
        }
    }

    /// Resolves one current Attention item from a sealed review transition.
    ///
    /// # Errors
    ///
    /// Returns a stable application error when validation, authority lookup,
    /// durable commit, or projection publication fails.
    pub fn delivery_resolve_attention(
        &mut self,
        command: &DeliveryResolveAttentionCommand,
    ) -> Result<DeliveryResolveAttentionCompletedResponse, DeliveryApplicationError> {
        let mapped = map_command(
            &command.actor,
            CommandName::DeliveryResolveAttention,
            command.expected_revision.clone(),
            &command.payload,
            &command.request_id,
            &command.schema_version,
            &command.scope,
        )?;
        if let Some((receipt, delivery)) =
            self.delivery_replay(&mapped, &command.scope, &command.payload.delivery_id)?
        {
            self.finalize_terminal_candidate_if_current(
                &command.payload.delivery_id,
                &receipt,
                &delivery,
            )?;
            return attention_response(command, &receipt, &delivery);
        }
        ensure_catalog_membership(
            self.storage_ref()?,
            &command.scope,
            &command.payload.delivery_id,
        )?;
        let current = load_current_delivery(self.storage_ref()?, &command.payload.delivery_id)?;
        let authority = self.resolve_attention_authority(&mapped, &current)?;
        let facts = DeliveryCommandFacts::attention_from_trusted_adapter(
            &mapped,
            command.scope.clone(),
            authority.repository,
            authority.source_ref,
            authority.transition,
        )?;
        let receipt = self
            .commit_delivery_command(&mapped, &facts)
            .map_err(DeliveryApplicationError::Command)?;
        let delivery = load_delivery_revision(
            self.storage_ref()?,
            &command.payload.delivery_id,
            receipt.revision,
        )?;
        self.finalize_terminal_candidate_if_current(
            &command.payload.delivery_id,
            &receipt,
            &delivery,
        )?;
        attention_response(command, &receipt, &delivery)
    }

    /// Computes and commits a verdict only from the installed candidate,
    /// verification and Evidence authorities.
    ///
    /// # Errors
    ///
    /// Returns a stable application error when validation, authority lookup,
    /// verdict commit, or projection publication fails.
    pub fn delivery_submit_verdict(
        &mut self,
        command: &DeliverySubmitVerdictCommand,
    ) -> Result<DeliverySubmitVerdictCompletedResponse, DeliveryApplicationError> {
        let mapped = map_command(
            &command.actor,
            CommandName::DeliverySubmitVerdict,
            command.expected_revision.clone(),
            &command.payload,
            &command.request_id,
            &command.schema_version,
            &command.scope,
        )?;
        if let Some((receipt, delivery)) =
            self.delivery_replay(&mapped, &command.scope, &command.payload.delivery_id)?
        {
            return verdict_response(command, &receipt, &delivery);
        }
        ensure_catalog_membership(
            self.storage_ref()?,
            &command.scope,
            &command.payload.delivery_id,
        )?;
        let current = load_current_delivery(self.storage_ref()?, &command.payload.delivery_id)?;
        let authority = self.resolve_verdict_authority(&mapped, &current)?;
        if authority
            .candidate
            .candidate_ref()
            .strip_prefix("git-candidate:")
            != Some(command.payload.candidate_digest.0.as_str())
        {
            return Err(DeliveryApplicationError::TrustedFactsUnavailable(
                "current candidate differs from the command stale-check digest".to_owned(),
            ));
        }
        let expected_revision = u64::try_from(command.expected_revision.0).map_err(|_| {
            DeliveryApplicationError::InvalidRequest(
                "delivery.submit_verdict expectedRevision is invalid".to_owned(),
            )
        })?;
        let facts = SubmitVerdictFacts {
            expected_revision,
            candidate: &authority.candidate,
            verification: &authority.verification,
            evidence: &authority.evidence,
            produced_at_millis: authority.produced_at_millis,
        };
        let receipt = self
            .commit_delivery_verdict(&mapped, facts)
            .map_err(DeliveryApplicationError::Verdict)?;
        let delivery = load_delivery_revision(
            self.storage_ref()?,
            &command.payload.delivery_id,
            receipt.revision,
        )?;
        verdict_response(command, &receipt, &delivery)
    }

    /// Lists Delivery summaries only through the repository-scoped catalog
    /// written atomically with each successful create.
    ///
    /// # Errors
    ///
    /// Returns a stable application error for invalid cursors or when durable
    /// catalog and Delivery journal reads fail.
    pub fn delivery_list(
        &self,
        query: &DeliveryListQuery,
    ) -> Result<DeliveryListResultResponse, DeliveryApplicationError> {
        validate_list_query(query)?;
        let limit = usize::try_from(query.page.limit)
            .ok()
            .filter(|limit| (1..=MAX_PAGE_SIZE).contains(limit))
            .ok_or_else(|| {
                DeliveryApplicationError::InvalidRequest(
                    "delivery.list page limit is invalid".to_owned(),
                )
            })?;
        let states = normalize_states(&query.parameters.states)?;
        let scope_sha256 = repository_scope_digest(&query.scope)?;
        let filter_sha256 = digest_json(&states)?;
        let decoded = decode_cursor(query.page.cursor.as_ref(), &scope_sha256, &filter_sha256)?;
        let prefix = delivery_catalog_prefix(&query.scope)?;
        let upper_bound = match &decoded {
            Some(cursor) => Some(cursor.upper_bound_stream_id.clone()),
            None => self.storage_ref()?.last_state_stream_id(&prefix)?,
        };
        let page = upper_bound.as_deref().map_or_else(
            || {
                Ok(LoadedDeliveryPage {
                    items: Vec::new(),
                    has_more: false,
                    snapshot_sha256: digest_bytes(b""),
                    after_seen: decoded.is_none(),
                })
            },
            |upper| {
                load_delivery_page(
                    self.storage_ref()?,
                    &query.scope,
                    &states,
                    decoded.as_ref().map(|cursor| &cursor.after_delivery_id),
                    &prefix,
                    upper,
                    limit,
                )
            },
        )?;
        if let Some(cursor) = &decoded
            && (!page.after_seen || page.snapshot_sha256 != cursor.snapshot_sha256)
        {
            return Err(DeliveryApplicationError::ReadCursorExpired);
        }
        let next_cursor = if page.has_more {
            let last = page.items.last().ok_or_else(|| {
                DeliveryApplicationError::InvalidRequest(
                    "delivery.list page has no keyset anchor".to_owned(),
                )
            })?;
            Some(encode_cursor(&DeliveryListCursor {
                schema: DELIVERY_LIST_CURSOR_SCHEMA.to_owned(),
                scope_sha256,
                filter_sha256,
                upper_bound_stream_id: upper_bound.ok_or_else(|| {
                    DeliveryApplicationError::InvalidRequest(
                        "delivery.list upper bound is missing".to_owned(),
                    )
                })?,
                snapshot_sha256: page.snapshot_sha256,
                after_delivery_id: last.delivery_id.clone(),
            })?)
        } else {
            None
        };
        Ok(DeliveryListResultResponse {
            page: PageInfo {
                has_more: page.has_more,
                next_cursor,
            },
            query: DeliveryListResultResponseQuery::DeliveryList,
            request_id: query.request_id.clone(),
            result: DeliveryPage {
                items: page.items,
                kind: DeliveryPageKind::DeliveryPage,
            },
            schema_version: SchemaVersion::WinwincodeV1,
        })
    }

    fn resolve_specification_authority(
        &mut self,
        command: &CommandEnvelope,
        delivery: Option<&Delivery>,
    ) -> Result<DeliverySpecificationAuthority, DeliveryApplicationError> {
        let mut port = self.take_delivery_authority()?;
        let result = port.specification(DeliveryAuthorityRequest { command, delivery });
        self.delivery_authority = Some(port);
        result.map_err(authority_error)
    }

    fn resolve_advance_authority(
        &mut self,
        command: &CommandEnvelope,
        delivery: &Delivery,
    ) -> Result<DeliveryAdvanceAuthority, DeliveryApplicationError> {
        let mut port = self.take_delivery_authority()?;
        let result = port.advance(DeliveryAuthorityRequest {
            command,
            delivery: Some(delivery),
        });
        self.delivery_authority = Some(port);
        result.map_err(authority_error)
    }

    fn resolve_attention_authority(
        &mut self,
        command: &CommandEnvelope,
        delivery: &Delivery,
    ) -> Result<DeliveryAttentionAuthority, DeliveryApplicationError> {
        let mut port = self.take_delivery_authority()?;
        let result = port.resolve_attention(DeliveryAuthorityRequest {
            command,
            delivery: Some(delivery),
        });
        self.delivery_authority = Some(port);
        result.map_err(authority_error)
    }

    fn resolve_verdict_authority(
        &mut self,
        command: &CommandEnvelope,
        delivery: &Delivery,
    ) -> Result<DeliveryVerdictAuthority, DeliveryApplicationError> {
        let mut port = self.take_delivery_authority()?;
        let result = port.verdict(DeliveryAuthorityRequest {
            command,
            delivery: Some(delivery),
        });
        self.delivery_authority = Some(port);
        result.map_err(authority_error)
    }

    fn take_delivery_authority(
        &mut self,
    ) -> Result<Box<dyn DeliveryAuthorityPort>, DeliveryApplicationError> {
        self.delivery_authority.take().ok_or_else(|| {
            DeliveryApplicationError::TrustedFactsUnavailable(
                "Delivery authority port is not installed".to_owned(),
            )
        })
    }

    fn finalize_terminal_candidate_if_current(
        &mut self,
        delivery_id: &DeliveryId,
        terminal_receipt: &CommitReceipt,
        delivery: &Delivery,
    ) -> Result<(), DeliveryApplicationError> {
        if delivery.snapshot().status != DeliveryStatus::Delivered
            || delivery.revision() != terminal_receipt.revision
        {
            return Ok(());
        }
        self.finalize_candidate_git_after_delivery_terminal(
            delivery_id,
            terminal_receipt,
            CandidateGitTerminalOutcome::Delivered,
        )
        .map(|_| ())
        .map_err(|error| {
            DeliveryApplicationError::Storage(StorageError::adapter(error.to_string()))
        })
    }

    fn delivery_replay(
        &self,
        command: &CommandEnvelope,
        scope: &RepositoryScope,
        delivery_id: &DeliveryId,
    ) -> Result<Option<(CommitReceipt, Delivery)>, DeliveryApplicationError> {
        let (identity, digest) = crate::command_receipt(command)?;
        let Some(receipt) = self.storage_ref()?.load_receipt(&identity, &digest)? else {
            return Ok(None);
        };
        let expected_revision = u64::try_from(command.expected_revision.0)
            .map_err(|_| {
                DeliveryApplicationError::InvalidRequest(
                    "Delivery expectedRevision is invalid".to_owned(),
                )
            })?
            .saturating_add(1);
        if receipt.stream_id != delivery_stream_id(delivery_id)
            || receipt.revision != expected_revision
            || !receipt.idempotent_replay
        {
            return Err(DeliveryApplicationError::Storage(
                StorageError::invalid_input(
                    "durable Delivery receipt does not match the exact generated command",
                ),
            ));
        }
        ensure_catalog_membership(self.storage_ref()?, scope, delivery_id)?;
        let delivery = load_delivery_revision(self.storage_ref()?, delivery_id, receipt.revision)?;
        Ok(Some((receipt, delivery)))
    }
}

fn authority_error(error: DeliveryAuthorityError) -> DeliveryApplicationError {
    DeliveryApplicationError::TrustedFactsUnavailable(error.message)
}

fn specification_facts(authority: DeliverySpecificationAuthority) -> DeliverySpecFacts {
    DeliverySpecFacts::from_trusted_adapter(TrustedDeliverySpecFacts {
        now_millis: authority.now_millis,
        repository: authority.repository,
        source_ref: authority.source_ref,
        scope: authority.scope,
        out_of_scope: authority.out_of_scope,
        constraints: authority.constraints,
        max_rework_attempts: authority.max_rework_attempts,
        criterion_verification_methods: authority.criterion_verification_methods,
    })
}

fn map_command<T: Serialize>(
    actor: &Actor,
    command: CommandName,
    expected_revision: Revision,
    payload: &T,
    request_id: &RequestId,
    schema_version: &SchemaVersion,
    scope: &RepositoryScope,
) -> Result<CommandEnvelope, DeliveryApplicationError> {
    if schema_version != &SchemaVersion::WinwincodeV1 {
        return Err(DeliveryApplicationError::InvalidRequest(
            "Delivery command schemaVersion is invalid".to_owned(),
        ));
    }
    let mapped = CommandEnvelope {
        actor: actor.clone(),
        command,
        expected_revision,
        payload: serde_json::to_value(payload).map_err(|error| {
            DeliveryApplicationError::InvalidRequest(format!(
                "Delivery command payload cannot be encoded: {error}"
            ))
        })?,
        request_id: request_id.clone(),
        schema_version: schema_version.clone(),
        scope: Scope::RepositoryScope(scope.clone()),
    };
    crate::command_receipt(&mapped)?;
    Ok(mapped)
}

fn load_current_delivery(
    storage: &dyn ProductStateStorage,
    delivery_id: &DeliveryId,
) -> Result<Delivery, DeliveryApplicationError> {
    let key = delivery_journal_key(delivery_id)?;
    let loaded = storage.load_journal(&key)?.ok_or_else(|| {
        DeliveryApplicationError::ResourceNotFound(format!(
            "Delivery {} was not found",
            delivery_id.0
        ))
    })?;
    let journal = StagedDeliveryJournal::new(delivery_id.clone(), Some(loaded));
    DeliveryStore::borrowed(&journal)
        .query(DeliveryQuery::Get(delivery_id.clone()))
        .map_err(|error| {
            DeliveryApplicationError::Storage(StorageError::adapter(error.to_string()))
        })
}

fn load_delivery_revision(
    storage: &dyn ProductStateStorage,
    delivery_id: &DeliveryId,
    revision: u64,
) -> Result<Delivery, DeliveryApplicationError> {
    let key = delivery_journal_key(delivery_id)?;
    let loaded = storage.load_journal(&key)?.ok_or_else(|| {
        DeliveryApplicationError::ResourceNotFound(format!(
            "Delivery {} was not found",
            delivery_id.0
        ))
    })?;
    let journal = StagedDeliveryJournal::new(delivery_id.clone(), Some(loaded));
    DeliveryStore::borrowed(&journal)
        .query(DeliveryQuery::GetRevision {
            delivery_id: delivery_id.clone(),
            revision,
        })
        .map_err(|error| {
            DeliveryApplicationError::Storage(StorageError::adapter(error.to_string()))
        })
}

fn create_response(
    command: &DeliveryCreateCommand,
    receipt: &CommitReceipt,
    delivery: &Delivery,
) -> Result<DeliveryCreateCompletedResponse, DeliveryApplicationError> {
    Ok(DeliveryCreateCompletedResponse {
        command: DeliveryCreateCompletedResponseCommand::DeliveryCreate,
        current_revision: revision(receipt.revision)?,
        outcome: DeliveryCreateCompletedResponseOutcome::Completed,
        previous_revision: previous_revision(receipt.revision)?,
        request_id: command.request_id.clone(),
        result: delivery_projection(delivery, &command.scope)?,
        schema_version: SchemaVersion::WinwincodeV1,
    })
}

fn update_response(
    command: &DeliveryUpdateSpecCommand,
    receipt: &CommitReceipt,
    delivery: &Delivery,
) -> Result<DeliveryUpdateSpecCompletedResponse, DeliveryApplicationError> {
    Ok(DeliveryUpdateSpecCompletedResponse {
        command: DeliveryUpdateSpecCompletedResponseCommand::DeliveryUpdateSpec,
        current_revision: revision(receipt.revision)?,
        outcome: DeliveryUpdateSpecCompletedResponseOutcome::Completed,
        previous_revision: previous_revision(receipt.revision)?,
        request_id: command.request_id.clone(),
        result: delivery_projection(delivery, &command.scope)?,
        schema_version: SchemaVersion::WinwincodeV1,
    })
}

fn task_breakdown_response(
    command: &DeliveryApproveTaskBreakdownCommand,
    receipt: &CommitReceipt,
    delivery: &Delivery,
) -> Result<DeliveryApproveTaskBreakdownCompletedResponse, DeliveryApplicationError> {
    Ok(DeliveryApproveTaskBreakdownCompletedResponse {
        command: DeliveryApproveTaskBreakdownCompletedResponseCommand::DeliveryApproveTaskBreakdown,
        current_revision: revision(receipt.revision)?,
        outcome: DeliveryApproveTaskBreakdownCompletedResponseOutcome::Completed,
        previous_revision: previous_revision(receipt.revision)?,
        request_id: command.request_id.clone(),
        result: delivery_projection(delivery, &command.scope)?,
        schema_version: SchemaVersion::WinwincodeV1,
    })
}

fn advance_response(
    command: &DeliveryAdvanceCommand,
    committed_revision: u64,
    delivery: &Delivery,
) -> Result<DeliveryAdvanceCompletedResponse, DeliveryApplicationError> {
    Ok(DeliveryAdvanceCompletedResponse {
        command: DeliveryAdvanceCompletedResponseCommand::DeliveryAdvance,
        current_revision: revision(committed_revision)?,
        outcome: DeliveryAdvanceCompletedResponseOutcome::Completed,
        previous_revision: previous_revision(committed_revision)?,
        request_id: command.request_id.clone(),
        result: delivery_projection(delivery, &command.scope)?,
        schema_version: SchemaVersion::WinwincodeV1,
    })
}

fn attention_response(
    command: &DeliveryResolveAttentionCommand,
    receipt: &CommitReceipt,
    delivery: &Delivery,
) -> Result<DeliveryResolveAttentionCompletedResponse, DeliveryApplicationError> {
    Ok(DeliveryResolveAttentionCompletedResponse {
        command: DeliveryResolveAttentionCompletedResponseCommand::DeliveryResolveAttention,
        current_revision: revision(receipt.revision)?,
        outcome: DeliveryResolveAttentionCompletedResponseOutcome::Completed,
        previous_revision: previous_revision(receipt.revision)?,
        request_id: command.request_id.clone(),
        result: delivery_projection(delivery, &command.scope)?,
        schema_version: SchemaVersion::WinwincodeV1,
    })
}

fn verdict_response(
    command: &DeliverySubmitVerdictCommand,
    receipt: &CommitReceipt,
    delivery: &Delivery,
) -> Result<DeliverySubmitVerdictCompletedResponse, DeliveryApplicationError> {
    Ok(DeliverySubmitVerdictCompletedResponse {
        command: DeliverySubmitVerdictCompletedResponseCommand::DeliverySubmitVerdict,
        current_revision: revision(receipt.revision)?,
        outcome: DeliverySubmitVerdictCompletedResponseOutcome::Completed,
        previous_revision: previous_revision(receipt.revision)?,
        request_id: command.request_id.clone(),
        result: delivery_projection(delivery, &command.scope)?,
        schema_version: SchemaVersion::WinwincodeV1,
    })
}

fn revision(value: u64) -> Result<Revision, DeliveryApplicationError> {
    i64::try_from(value).map(Revision).map_err(|_| {
        DeliveryApplicationError::InvalidRequest(
            "Delivery revision exceeds the public range".to_owned(),
        )
    })
}

fn previous_revision(value: u64) -> Result<Revision, DeliveryApplicationError> {
    revision(value.saturating_sub(1))
}

fn delivery_projection(
    delivery: &Delivery,
    scope: &RepositoryScope,
) -> Result<DeliveryProjection, DeliveryApplicationError> {
    let snapshot = delivery.snapshot();
    let active_stage_run_id = snapshot
        .stage_runs
        .iter()
        .rev()
        .find(|run| {
            matches!(
                run.status,
                StageRunStatus::Waiting | StageRunStatus::Running
            )
        })
        .map(|run| run.id.clone());
    let open_attention = snapshot
        .attention_items
        .iter()
        .filter(|item| item.status == AttentionItemStatus::Open)
        .count();
    let mut pending = 0_usize;
    let mut active = 0_usize;
    let mut blocked = 0_usize;
    let mut completed = 0_usize;
    let mut failed = 0_usize;
    let mut verifying = 0_usize;
    for task in &snapshot.tasks {
        match task.status {
            DeliveryTaskStatus::Pending => pending += 1,
            DeliveryTaskStatus::Active => active += 1,
            DeliveryTaskStatus::Verifying => verifying += 1,
            DeliveryTaskStatus::Blocked => blocked += 1,
            DeliveryTaskStatus::Completed => completed += 1,
            DeliveryTaskStatus::Failed => failed += 1,
        }
    }
    Ok(DeliveryProjection {
        active_stage_run_id,
        delivery_id: delivery.id().clone(),
        open_attention_count: count(open_attention)?,
        ownership: DeliveryOwnershipProjection {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
            workspace_id: scope.workspace_id.clone(),
        },
        revision: revision(delivery.revision())?,
        schema_version: SchemaVersion::WinwincodeV1,
        status: api_delivery_status(snapshot.status),
        task_counts: DeliveryTaskCountsProjection {
            active: count(active)?,
            blocked: count(blocked)?,
            completed: count(completed)?,
            failed: count(failed)?,
            pending: count(pending)?,
            total: count(snapshot.tasks.len())?,
            verifying: count(verifying)?,
        },
        title: snapshot.spec.title.clone(),
        updated_at: crate::instant_from_millis(snapshot.updated_at_millis)?,
    })
}

fn count(value: usize) -> Result<Count, DeliveryApplicationError> {
    i64::try_from(value).map(Count).map_err(|_| {
        DeliveryApplicationError::Storage(StorageError::adapter(
            "Delivery summary count exceeds the public range",
        ))
    })
}

const fn api_delivery_status(status: DeliveryStatus) -> ApiDeliveryStatus {
    match status {
        DeliveryStatus::Draft => ApiDeliveryStatus::Draft,
        DeliveryStatus::Clarifying => ApiDeliveryStatus::Clarifying,
        DeliveryStatus::Ready => ApiDeliveryStatus::Ready,
        DeliveryStatus::Planning => ApiDeliveryStatus::Planning,
        DeliveryStatus::PlanReview => ApiDeliveryStatus::PlanReview,
        DeliveryStatus::Executing => ApiDeliveryStatus::Executing,
        DeliveryStatus::Verifying => ApiDeliveryStatus::Verifying,
        DeliveryStatus::Reworking => ApiDeliveryStatus::Reworking,
        DeliveryStatus::NeedsAttention => ApiDeliveryStatus::NeedsAttention,
        DeliveryStatus::ReadyToDeliver => ApiDeliveryStatus::ReadyToDeliver,
        DeliveryStatus::Delivered => ApiDeliveryStatus::Delivered,
    }
}

fn validate_list_query(query: &DeliveryListQuery) -> Result<(), DeliveryApplicationError> {
    if query.schema_version != SchemaVersion::WinwincodeV1 {
        return Err(DeliveryApplicationError::InvalidRequest(
            "delivery.list query is invalid".to_owned(),
        ));
    }
    crate::command_receipt_identity(
        &query.actor,
        &Scope::RepositoryScope(query.scope.clone()),
        query.request_id.clone(),
    )?;
    crate::repository_scope_key(&query.scope)?;
    Ok(())
}

fn normalize_states(states: &[String]) -> Result<Vec<String>, DeliveryApplicationError> {
    if states.iter().any(|state| {
        !matches!(
            state.as_str(),
            "draft"
                | "clarifying"
                | "ready"
                | "planning"
                | "plan-review"
                | "executing"
                | "verifying"
                | "reworking"
                | "needs-attention"
                | "ready-to-deliver"
                | "delivered"
        )
    }) {
        return Err(DeliveryApplicationError::InvalidRequest(
            "delivery.list state filter is invalid".to_owned(),
        ));
    }
    let mut normalized = states.to_vec();
    normalized.sort_unstable();
    normalized.dedup();
    Ok(normalized)
}

fn delivery_status_name(status: DeliveryStatus) -> &'static str {
    match status {
        DeliveryStatus::Draft => "draft",
        DeliveryStatus::Clarifying => "clarifying",
        DeliveryStatus::Ready => "ready",
        DeliveryStatus::Planning => "planning",
        DeliveryStatus::PlanReview => "plan-review",
        DeliveryStatus::Executing => "executing",
        DeliveryStatus::Verifying => "verifying",
        DeliveryStatus::Reworking => "reworking",
        DeliveryStatus::NeedsAttention => "needs-attention",
        DeliveryStatus::ReadyToDeliver => "ready-to-deliver",
        DeliveryStatus::Delivered => "delivered",
    }
}

#[allow(clippy::too_many_arguments)]
fn load_delivery_page(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    states: &[String],
    after: Option<&DeliveryId>,
    prefix: &str,
    upper_bound: &str,
    limit: usize,
) -> Result<LoadedDeliveryPage, DeliveryApplicationError> {
    if !upper_bound.starts_with(prefix) {
        return Err(DeliveryApplicationError::ReadCursorExpired);
    }
    let mut stream_after = String::new();
    let mut scanned = 0_usize;
    let mut items = Vec::with_capacity(limit.saturating_add(1));
    let mut snapshot = Sha256::new();
    let mut after_seen = after.is_none();
    loop {
        let rows =
            storage.scan_state_streams(prefix, &stream_after, upper_bound, SCAN_PAGE_SIZE)?;
        if rows.is_empty() {
            break;
        }
        scanned = scanned.saturating_add(rows.len());
        if scanned > MAX_SNAPSHOT_ROWS {
            return Err(DeliveryApplicationError::Storage(StorageError::adapter(
                "Delivery list snapshot exceeds its bounded catalog budget",
            )));
        }
        for row in &rows {
            let entry = decode_catalog_entry(row.payload.as_slice())?;
            if entry.schema_version != DELIVERY_CATALOG_SCHEMA_VERSION
                || &entry.repository_scope != scope
                || row.stream_id != delivery_catalog_stream_id(scope, &entry.delivery_id)?
                || row.revision != 1
            {
                return Err(DeliveryApplicationError::Storage(StorageError::adapter(
                    "Delivery catalog entry is corrupt or outside its repository scope",
                )));
            }
            let delivery = load_current_delivery(storage, &entry.delivery_id)?;
            if !states.is_empty()
                && !states
                    .iter()
                    .any(|state| state == delivery_status_name(delivery.snapshot().status))
            {
                continue;
            }
            let projection = delivery_projection(&delivery, scope)?;
            let encoded = serde_json::to_vec(&projection).map_err(storage_error)?;
            snapshot.update((encoded.len() as u64).to_be_bytes());
            snapshot.update(encoded);
            if after == Some(&entry.delivery_id) {
                after_seen = true;
            }
            if after.is_none_or(|anchor| entry.delivery_id.0 > anchor.0) && items.len() <= limit {
                items.push(projection);
            }
        }
        stream_after.clone_from(
            &rows
                .last()
                .expect("a non-empty Delivery catalog page has a final row")
                .stream_id,
        );
        if rows.len() < SCAN_PAGE_SIZE || stream_after == upper_bound {
            break;
        }
    }
    let has_more = items.len() > limit;
    if has_more {
        items.pop();
    }
    Ok(LoadedDeliveryPage {
        items,
        has_more,
        snapshot_sha256: format!("sha256:{:x}", snapshot.finalize()),
        after_seen,
    })
}

fn ensure_catalog_membership(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
) -> Result<(), DeliveryApplicationError> {
    load_catalog_membership(storage, scope, delivery_id).map(|_| ())
}

fn load_catalog_membership(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
) -> Result<StoredState, DeliveryApplicationError> {
    let stream = delivery_catalog_stream_id(scope, delivery_id)?;
    let stored = storage.load_state(&stream)?.ok_or_else(|| {
        DeliveryApplicationError::ResourceNotFound(format!(
            "Delivery {} was not found in this repository scope",
            delivery_id.0
        ))
    })?;
    let entry = decode_catalog_entry(&stored.payload)?;
    if stored.stream_id != stream
        || stored.revision != 1
        || entry.schema_version != DELIVERY_CATALOG_SCHEMA_VERSION
        || entry.repository_scope != *scope
        || entry.delivery_id != *delivery_id
    {
        return Err(DeliveryApplicationError::Storage(StorageError::adapter(
            "Delivery catalog membership is corrupt",
        )));
    }
    Ok(stored)
}

fn decode_catalog_entry(payload: &[u8]) -> Result<DeliveryCatalogEntry, DeliveryApplicationError> {
    let value: serde_json::Value = serde_json::from_slice(payload).map_err(storage_error)?;
    let entry: DeliveryCatalogEntry =
        serde_json::from_value(value.clone()).map_err(storage_error)?;
    if serde_json::to_value(&entry).map_err(storage_error)? != value {
        return Err(DeliveryApplicationError::Storage(StorageError::adapter(
            "Delivery catalog entry is not canonical",
        )));
    }
    Ok(entry)
}

fn delivery_catalog_prefix(scope: &RepositoryScope) -> Result<String, DeliveryApplicationError> {
    Ok(format!(
        "{DELIVERY_CATALOG_PREFIX}{}:",
        repository_scope_digest(scope)?
    ))
}

/// Loads every current scoped Delivery from one verified read cut.
///
/// # Errors
///
/// Rejects oversized, concurrently changed, missing, foreign or corrupt
/// Delivery catalog/state facts rather than returning a partial Inbox source.
pub(crate) fn collaboration_delivery_snapshot<S: ProductStateStorage + ?Sized>(
    storage: &S,
    scope: &RepositoryScope,
) -> Result<CollaborationDeliverySourceSnapshot, DeliveryApplicationError> {
    const MAX_DIRECTORY_BYTES: usize = 64 * 1_024 * 1_024;
    let prefix = delivery_catalog_prefix(scope)?;
    let directory =
        storage.load_bounded_state_directory(&prefix, MAX_SNAPSHOT_ROWS, MAX_DIRECTORY_BYTES)?;
    let mut delivery_ids = Vec::with_capacity(directory.len());
    let mut state_stream_ids = Vec::with_capacity(directory.len().saturating_mul(2));
    for entry in &directory {
        let raw_id = entry
            .stream_id
            .strip_prefix(&prefix)
            .filter(|value| !value.is_empty())
            .ok_or(DeliveryApplicationError::ReadCursorExpired)?;
        let delivery_id = DeliveryId(raw_id.to_owned());
        state_stream_ids.push(entry.stream_id.clone());
        state_stream_ids.push(delivery_stream_id(&delivery_id));
        delivery_ids.push(delivery_id);
    }
    let key = ProjectionEventStreamKey::new(
        crate::repository_scope_key(scope)?,
        ProjectionEventStream::Scope,
    )?;
    let cut = storage.load_projection_read_cut(&state_stream_ids, &key, None)?;
    let confirmation =
        storage.load_bounded_state_directory(&prefix, MAX_SNAPSHOT_ROWS, MAX_DIRECTORY_BYTES)?;
    if confirmation != directory {
        return Err(DeliveryApplicationError::ReadCursorExpired);
    }
    let states = cut
        .states()
        .iter()
        .map(|state| (state.stream_id.as_str(), state))
        .collect::<BTreeMap<_, _>>();
    let mut records = Vec::with_capacity(delivery_ids.len());
    for (directory_entry, delivery_id) in directory.iter().zip(delivery_ids) {
        let catalog_state = states
            .get(directory_entry.stream_id.as_str())
            .ok_or_else(|| StorageError::adapter("Delivery catalog changed during read cut"))?;
        if catalog_state.revision != directory_entry.revision
            || digest_bytes(&catalog_state.payload) != directory_entry.payload_sha256.0
        {
            return Err(DeliveryApplicationError::ReadCursorExpired);
        }
        let catalog_entry = decode_catalog_entry(&catalog_state.payload)?;
        if catalog_entry.repository_scope != *scope || catalog_entry.delivery_id != delivery_id {
            return Err(DeliveryApplicationError::Storage(StorageError::adapter(
                "Delivery catalog entry is foreign or corrupt",
            )));
        }
        let delivery_stream = delivery_stream_id(&delivery_id);
        let delivery_state = states
            .get(delivery_stream.as_str())
            .ok_or_else(|| StorageError::adapter("Delivery state is missing from its read cut"))?;
        let delivery = Delivery::decode_json(&delivery_state.payload).map_err(|error| {
            DeliveryApplicationError::Storage(StorageError::adapter(error.to_string()))
        })?;
        if delivery.id() != &delivery_id || delivery.revision() != delivery_state.revision {
            return Err(DeliveryApplicationError::Storage(StorageError::adapter(
                "Delivery state identity or revision is corrupt",
            )));
        }
        records.push(CollaborationDeliverySourceRecord {
            delivery,
            state_guards: vec![
                StateRevisionGuard::new(catalog_state.stream_id.clone(), catalog_state.revision)?,
                StateRevisionGuard::new(delivery_state.stream_id.clone(), delivery_state.revision)?,
            ],
        });
    }
    records.sort_by(|left, right| left.delivery.id().0.cmp(&right.delivery.id().0));
    Ok(CollaborationDeliverySourceSnapshot { records })
}

fn encode_cursor(cursor: &DeliveryListCursor) -> Result<OpaqueCursor, DeliveryApplicationError> {
    let encoded = serde_json::to_vec(cursor).map_err(storage_error)?;
    if encoded.len() > MAX_CURSOR_BYTES {
        return Err(DeliveryApplicationError::InvalidRequest(
            "delivery.list cursor exceeds its encoded budget".to_owned(),
        ));
    }
    Ok(OpaqueCursor(URL_SAFE_NO_PAD.encode(encoded)))
}

fn decode_cursor(
    cursor: Option<&OpaqueCursor>,
    scope_sha256: &str,
    filter_sha256: &str,
) -> Result<Option<DeliveryListCursor>, DeliveryApplicationError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    if cursor.0.len() > MAX_CURSOR_BYTES.saturating_mul(2) {
        return Err(DeliveryApplicationError::InvalidRequest(
            "delivery.list cursor is invalid".to_owned(),
        ));
    }
    let bytes = URL_SAFE_NO_PAD.decode(&cursor.0).map_err(|_| {
        DeliveryApplicationError::InvalidRequest("delivery.list cursor is invalid".to_owned())
    })?;
    if bytes.len() > MAX_CURSOR_BYTES {
        return Err(DeliveryApplicationError::InvalidRequest(
            "delivery.list cursor is invalid".to_owned(),
        ));
    }
    let decoded: DeliveryListCursor = serde_json::from_slice(&bytes).map_err(|_| {
        DeliveryApplicationError::InvalidRequest("delivery.list cursor is invalid".to_owned())
    })?;
    if decoded.schema != DELIVERY_LIST_CURSOR_SCHEMA
        || decoded.scope_sha256 != scope_sha256
        || decoded.filter_sha256 != filter_sha256
        || decoded.snapshot_sha256.len() != 71
    {
        return Err(DeliveryApplicationError::ReadCursorExpired);
    }
    Ok(Some(decoded))
}

fn digest_json<T: Serialize + ?Sized>(value: &T) -> Result<String, DeliveryApplicationError> {
    let bytes = serde_json::to_vec(value).map_err(storage_error)?;
    Ok(digest_bytes(&bytes))
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub(crate) fn delivery_catalog_mutation(
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
) -> Result<StateMutation, StorageError> {
    let entry = DeliveryCatalogEntry {
        schema_version: DELIVERY_CATALOG_SCHEMA_VERSION,
        repository_scope: scope.clone(),
        delivery_id: delivery_id.clone(),
    };
    StateMutation::new(
        delivery_catalog_stream_id(scope, delivery_id)?,
        0,
        serde_json::to_vec(&entry).map_err(storage_error)?,
    )
}

fn delivery_catalog_stream_id(
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
) -> Result<String, StorageError> {
    Ok(format!(
        "delivery-catalog:{}:{}",
        repository_scope_digest(scope)?,
        delivery_id.0
    ))
}

fn repository_scope_digest(scope: &RepositoryScope) -> Result<String, StorageError> {
    let encoded = serde_json::to_vec(scope).map_err(storage_error)?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

fn storage_error(error: impl fmt::Display) -> StorageError {
    StorageError::invalid_input(error.to_string())
}

fn storage_error_code(error: &StorageError) -> ErrorCode {
    use winwincode_storage::StorageErrorKind;
    match error.kind() {
        StorageErrorKind::InvalidInput => ErrorCode::InvalidRequest,
        StorageErrorKind::RevisionConflict => ErrorCode::RevisionConflict,
        StorageErrorKind::RequestConflict => ErrorCode::IdempotencyConflict,
        StorageErrorKind::JournalNotFound => ErrorCode::ResourceNotFound,
        StorageErrorKind::RequestReplayMissing
        | StorageErrorKind::JournalAlreadyExists
        | StorageErrorKind::JournalConflict
        | StorageErrorKind::EventCursorExpired
        | StorageErrorKind::Adapter
        | StorageErrorKind::Closed => ErrorCode::ServiceUnavailable,
    }
}
