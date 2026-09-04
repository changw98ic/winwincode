// SPDX-License-Identifier: Apache-2.0

//! Durable model retry/fallback orchestration and request-level Usage facts.
//!
//! Retry decisions accept only closed Provider failure categories plus an
//! explicit execution-certainty fact. Usage entries are immutable source facts
//! for the separate organization billing ledger; this module does not own
//! organization quota or cost allocation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::Actor;
use winwincode_domain::{
    DeliveryId, Instant, ModelExchangeId, OrganizationId, ProductSessionId, ProjectId,
    RepositoryId, RequestId, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity,
    ReceiptScopeKey, StateCommit, StateMutation, StorageError, StorageErrorKind, StoredState,
};

use crate::{
    FrozenModelRouteAuthority, ModelReservationReceipt, ModelReservationTerminalOutcome,
    ModelReservationTerminalReceipt, ModelRetrySettlementContext, ProviderGatewayErrorKind,
    ProviderGatewaySettlement, ProviderGatewayTerminalOutcome, ProviderStreamFailureKind,
    ProviderTokenUsage,
};

const STATE_SCHEMA: &str = "winwincode.model-retry-usage.v1";
const EVENT_SCHEMA: &str = "winwincode.model-retry-usage-event.v1";
const STREAM_PREFIX: &str = "model-retry-usage:";
const USAGE_ID_PREFIX: &str = "model-usage-id:";
const USAGE_ENTRY_PREFIX: &str = "model-usage-entry:";
const USAGE_CATALOG_STREAM: &str = "model-usage-catalog:v1";
const USAGE_ENTRY_SCHEMA: &str = "winwincode.model-usage-entry.v2";
const USAGE_CATALOG_SCHEMA: &str = "winwincode.model-usage-catalog.v2";
const RETRY_CONTEXT_PREFIX: &str = "model-retry-context:";
const EVENT_TOPIC: &str = "model.retry-usage.changed.v1";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_TOTAL_ATTEMPTS: u64 = 16;
const MAX_TOTAL_ATTEMPTS_USIZE: usize = 16;
const MAX_COMMIT_RETRIES: usize = 64;
const MAX_USAGE_SOURCE_PAGE_SIZE: u64 = 200;
const MAX_USAGE_SOURCE_SCAN_ROWS: u64 = 1_000;

/// Immutable attribution dimensions attached to every request Usage fact.
/// Immutable Provider settlement source stored in catalog sequence order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsageAttribution {
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Exact owning workspace frozen with the request.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Exact repository frozen with the request.
    pub repository_id: RepositoryId,
    /// Exact Product Session.
    pub product_session_id: ProductSessionId,
    /// Optional exact Delivery when the model request belongs to one.
    pub delivery_id: Option<DeliveryId>,
    /// Original authenticated user responsible for the request.
    pub user_id: UserId,
}

impl ModelUsageAttribution {
    /// Builds attribution from the frozen request target, trusted Delivery, and
    /// original authenticated user.
    ///
    /// # Errors
    ///
    /// Rejects a malformed Delivery identity.
    pub fn from_request_authority(
        authority: &FrozenModelRouteAuthority,
        delivery_id: Option<DeliveryId>,
        actor: &Actor,
    ) -> Result<Self, ModelRetryUsageError> {
        let Actor::UserActor(actor) = actor else {
            return Err(ModelRetryUsageError::identity_mismatch());
        };
        Self::from_verified_user(authority, delivery_id, actor.id.clone())
    }

    fn from_verified_user(
        authority: &FrozenModelRouteAuthority,
        delivery_id: Option<DeliveryId>,
        user_id: UserId,
    ) -> Result<Self, ModelRetryUsageError> {
        if let Some(delivery_id) = &delivery_id {
            validate_prefixed_id(&delivery_id.0, "dlv_")?;
        }
        validate_prefixed_id(&user_id.0, "usr_")?;
        let (repository_scope, product_session_id) = match authority.target() {
            crate::ModelSettingsTarget::ProductSession {
                repository_scope,
                product_session_id,
            } => (repository_scope, product_session_id),
            crate::ModelSettingsTarget::Organization { .. }
            | crate::ModelSettingsTarget::Project { .. }
            | crate::ModelSettingsTarget::Repository { .. } => {
                return Err(ModelRetryUsageError::identity_mismatch());
            }
        };
        Ok(Self {
            organization_id: repository_scope.organization_id.clone(),
            workspace_id: repository_scope.workspace_id.clone(),
            project_id: repository_scope.project_id.clone(),
            repository_id: repository_scope.repository_id.clone(),
            product_session_id: product_session_id.clone(),
            delivery_id,
            user_id,
        })
    }
}

/// One route slot in a frozen retry/fallback plan.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRetryStep {
    authority: FrozenModelRouteAuthority,
    max_attempts: u64,
}

impl ModelRetryStep {
    /// Builds one route step with a hard attempt bound.
    ///
    /// # Errors
    ///
    /// Rejects zero or unsafe attempt counts.
    pub fn try_new(
        authority: FrozenModelRouteAuthority,
        max_attempts: u64,
    ) -> Result<Self, ModelRetryUsageError> {
        if max_attempts == 0 || max_attempts > MAX_TOTAL_ATTEMPTS {
            return Err(ModelRetryUsageError::invalid());
        }
        Ok(Self {
            authority,
            max_attempts,
        })
    }

    /// Returns the exact route authority for this step.
    #[must_use]
    pub const fn authority(&self) -> &FrozenModelRouteAuthority {
        &self.authority
    }

    /// Returns the hard attempt bound for this exact route.
    #[must_use]
    pub const fn max_attempts(&self) -> u64 {
        self.max_attempts
    }
}

/// Auditable finite retry plan. Later steps are explicit fallback routes.
#[derive(Clone, Debug, PartialEq)]
pub struct FrozenModelRetryPlan {
    policy_id: String,
    policy_revision: u64,
    steps: Vec<ModelRetryStep>,
    fingerprint: String,
}

impl FrozenModelRetryPlan {
    /// Freezes one bounded route sequence.
    ///
    /// # Errors
    ///
    /// Rejects empty, duplicate, cross-scope, oversized, or malformed plans.
    pub fn freeze(
        policy_id: String,
        policy_revision: u64,
        steps: Vec<ModelRetryStep>,
    ) -> Result<Self, ModelRetryUsageError> {
        validate_token(&policy_id, 200)?;
        if policy_revision == 0 || policy_revision > MAX_SAFE_INTEGER || steps.is_empty() {
            return Err(ModelRetryUsageError::invalid());
        }
        let first_target = steps[0].authority.target();
        let total = steps.iter().try_fold(0_u64, |total, step| {
            if step.authority.target() != first_target {
                return Err(ModelRetryUsageError::identity_mismatch());
            }
            checked_add(total, step.max_attempts)
        })?;
        if total > MAX_TOTAL_ATTEMPTS {
            return Err(ModelRetryUsageError::invalid());
        }
        let mut fingerprints = BTreeSet::new();
        if steps
            .iter()
            .any(|step| !fingerprints.insert(step.authority.fingerprint()))
        {
            return Err(ModelRetryUsageError::invalid());
        }
        let snapshots = steps.iter().map(step_snapshot).collect::<Vec<_>>();
        let payload = serde_json::to_vec(&(policy_id.as_str(), policy_revision, snapshots))
            .map_err(|_| ModelRetryUsageError::invalid())?;
        Ok(Self {
            policy_id,
            policy_revision,
            steps,
            fingerprint: format!("sha256:{:x}", Sha256::digest(payload)),
        })
    }

    /// Returns the stable policy fingerprint.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Returns the frozen retry policy identity.
    #[must_use]
    pub fn policy_id(&self) -> &str {
        &self.policy_id
    }

    /// Returns the frozen retry policy revision.
    #[must_use]
    pub const fn policy_revision(&self) -> u64 {
        self.policy_revision
    }

    /// Returns the primary route followed by explicit fallback routes.
    #[must_use]
    pub fn steps(&self) -> &[ModelRetryStep] {
        &self.steps
    }
}

/// Whether Provider acceptance or output can be ruled out.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelExecutionCertainty {
    /// The request was proven not sent to a Provider.
    NotSent,
    /// The Provider explicitly rejected it before acceptance.
    RejectedBeforeAcceptance,
    /// Acceptance is unknown, so retry may duplicate work or cost.
    AcceptanceUnknown,
    /// At least one output fragment was observed.
    OutputObserved,
}

/// Closed failure class used by retry policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAttemptFailureKind {
    Authentication,
    InvalidRequest,
    RateLimit,
    Quota,
    Timeout,
    Transport,
    Server,
    ContextWindowExceeded,
    ProviderUnavailable,
    Protocol,
    Cancelled,
    Unknown,
}

/// Secret-free failure fact for one terminal attempt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelAttemptFailureFact {
    /// Stable failure category.
    pub kind: ModelAttemptFailureKind,
    /// Explicit Provider execution certainty.
    pub certainty: ModelExecutionCertainty,
}

impl ModelAttemptFailureFact {
    /// Maps a stable Gateway category without copying Provider diagnostics.
    #[must_use]
    pub const fn from_gateway(
        kind: ProviderGatewayErrorKind,
        certainty: ModelExecutionCertainty,
    ) -> Self {
        let kind = match kind {
            ProviderGatewayErrorKind::AdapterRateLimited => ModelAttemptFailureKind::RateLimit,
            ProviderGatewayErrorKind::AdapterUnavailable
            | ProviderGatewayErrorKind::IdentityUnavailable
            | ProviderGatewayErrorKind::RouteUnavailable
            | ProviderGatewayErrorKind::PolicyUnavailable
            | ProviderGatewayErrorKind::AdmissionUnavailable
            | ProviderGatewayErrorKind::SettlementUnavailable
            | ProviderGatewayErrorKind::Storage => ModelAttemptFailureKind::ProviderUnavailable,
            ProviderGatewayErrorKind::AdapterProtocol => ModelAttemptFailureKind::Protocol,
            ProviderGatewayErrorKind::AdapterRejected
            | ProviderGatewayErrorKind::InvalidRequest
            | ProviderGatewayErrorKind::IdentityDenied
            | ProviderGatewayErrorKind::RouteMismatch
            | ProviderGatewayErrorKind::ProviderNotFound
            | ProviderGatewayErrorKind::ProviderDisabled
            | ProviderGatewayErrorKind::ModelNotFound
            | ProviderGatewayErrorKind::ModelDisabled
            | ProviderGatewayErrorKind::StructuredOutputUnsupported
            | ProviderGatewayErrorKind::AdapterNotRegistered
            | ProviderGatewayErrorKind::ExchangeConflict
            | ProviderGatewayErrorKind::ExchangeNotFound
            | ProviderGatewayErrorKind::TerminalConflict
            | ProviderGatewayErrorKind::PolicyDenied
            | ProviderGatewayErrorKind::AdmissionDenied
            | ProviderGatewayErrorKind::CredentialLeak => ModelAttemptFailureKind::InvalidRequest,
            ProviderGatewayErrorKind::CredentialUnavailable
            | ProviderGatewayErrorKind::CredentialScopeMismatch => {
                ModelAttemptFailureKind::Authentication
            }
        };
        Self { kind, certainty }
    }

    /// Maps a stable stream failure without copying Provider text or ids.
    #[must_use]
    pub const fn from_stream(
        kind: ProviderStreamFailureKind,
        certainty: ModelExecutionCertainty,
    ) -> Self {
        let kind = match kind {
            ProviderStreamFailureKind::Authentication => ModelAttemptFailureKind::Authentication,
            ProviderStreamFailureKind::InvalidRequest => ModelAttemptFailureKind::InvalidRequest,
            ProviderStreamFailureKind::RateLimit => ModelAttemptFailureKind::RateLimit,
            ProviderStreamFailureKind::Quota => ModelAttemptFailureKind::Quota,
            ProviderStreamFailureKind::Timeout => ModelAttemptFailureKind::Timeout,
            ProviderStreamFailureKind::Transport => ModelAttemptFailureKind::Transport,
            ProviderStreamFailureKind::Server => ModelAttemptFailureKind::Server,
            ProviderStreamFailureKind::ContextWindowExceeded => {
                ModelAttemptFailureKind::ContextWindowExceeded
            }
            ProviderStreamFailureKind::Unknown => ModelAttemptFailureKind::Unknown,
        };
        Self { kind, certainty }
    }

    const fn safe_to_retry(self) -> bool {
        matches!(
            self.certainty,
            ModelExecutionCertainty::NotSent | ModelExecutionCertainty::RejectedBeforeAcceptance
        ) && matches!(
            self.kind,
            ModelAttemptFailureKind::RateLimit
                | ModelAttemptFailureKind::Timeout
                | ModelAttemptFailureKind::Transport
                | ModelAttemptFailureKind::Server
                | ModelAttemptFailureKind::ProviderUnavailable
        )
    }
}

/// Exact normalized charge attached to one Provider attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAttemptCharge {
    /// Stable Provider usage identity, unique across every logical request.
    pub provider_usage_id: String,
    /// Provider-normalized token usage.
    pub usage: ProviderTokenUsage,
    /// Actual cost in micros.
    pub cost_micros: u64,
}

/// One logical model request and its frozen attribution/plan.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRetryUsageRequest {
    /// Logical request identity shared by every attempt.
    pub request_id: RequestId,
    /// Immutable Usage attribution.
    pub attribution: ModelUsageAttribution,
    /// Finite retry/fallback policy.
    pub plan: FrozenModelRetryPlan,
    /// Exact enterprise allowance frozen before Provider invocation.
    pub enterprise_quota_amounts: winwincode_storage::EnterpriseQuotaAmounts,
    /// Trusted request time frozen with the enterprise allowance.
    pub enterprise_quota_requested_at: Instant,
}

/// Starts the primary or one previously authorized retry attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAttemptStartCommand {
    /// Idempotency identity for this state mutation.
    pub command_request_id: RequestId,
    /// Admitted exchange receipt for the new attempt.
    pub admission: ModelReservationReceipt,
}

/// Terminal failed/cancelled attempt facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAttemptFailureCommand {
    /// Idempotency identity for this terminal mutation.
    pub command_request_id: RequestId,
    /// Gateway terminal identity and route facts.
    pub gateway: ProviderGatewaySettlement,
}

/// Successful terminal Usage facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelAttemptCompletionCommand {
    /// Idempotency identity for this terminal mutation.
    pub command_request_id: RequestId,
    /// Gateway successful terminal identity and route facts.
    pub gateway: ProviderGatewaySettlement,
}

/// Next action after a durable failed attempt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRetryAction {
    RetrySameRoute,
    Fallback,
    Stop,
}

/// Durable attempt-start result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelAttemptStartReceipt {
    pub request_id: RequestId,
    pub reservation_request_id: RequestId,
    pub model_exchange_id: ModelExchangeId,
    pub attempt: u64,
    pub provider_id: String,
    pub model_id: String,
    pub route_fingerprint: String,
    pub revision: u64,
    pub idempotent_replay: bool,
}

/// Durable failed-attempt result and next authorized route.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelRetryDecisionReceipt {
    pub request_id: RequestId,
    pub model_exchange_id: ModelExchangeId,
    pub attempt: u64,
    pub action: ModelRetryAction,
    pub next_attempt: Option<u64>,
    pub next_provider_id: Option<String>,
    pub next_model_id: Option<String>,
    pub next_route_fingerprint: Option<String>,
    pub revision: u64,
    pub idempotent_replay: bool,
}

/// One immutable, normalized Provider Usage entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettledModelUsage {
    pub provider_usage_id: String,
    pub attempt: u64,
    pub provider_id: String,
    pub model_id: String,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    pub cost_micros: u64,
}

/// Durable successful settlement result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsageSettlementReceipt {
    pub request_id: RequestId,
    pub model_exchange_id: ModelExchangeId,
    pub usage: SettledModelUsage,
    pub revision: u64,
    pub idempotent_replay: bool,
}

/// Optional reconciliation filters. Every populated dimension is exact.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsageFilter {
    pub organization_id: Option<OrganizationId>,
    pub workspace_id: Option<WorkspaceId>,
    pub project_id: Option<ProjectId>,
    pub repository_id: Option<RepositoryId>,
    pub product_session_id: Option<ProductSessionId>,
    pub delivery_id: Option<DeliveryId>,
    pub user_id: Option<UserId>,
    pub provider_id: Option<String>,
}

/// Stable cursor bound to one exact filter and immutable catalog snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelUsageSourceCursor {
    filter_digest: String,
    snapshot_sequence: u64,
    after_sequence: u64,
}

impl ModelUsageSourceCursor {
    #[must_use]
    pub fn filter_digest(&self) -> &str {
        &self.filter_digest
    }

    #[must_use]
    pub const fn snapshot_sequence(&self) -> u64 {
        self.snapshot_sequence
    }

    #[must_use]
    pub const fn after_sequence(&self) -> u64 {
        self.after_sequence
    }
}

/// One bounded page of immutable Provider settlement source facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelUsageSourcePage {
    pub snapshot_sequence: u64,
    pub entries: Vec<ModelUsageSourceEntry>,
    pub next: Option<ModelUsageSourceCursor>,
}

/// Summed immutable Usage facts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelUsageTotals {
    pub entries: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cost_micros: u64,
}

/// Reconciliation result plus exact Provider subtotals.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelUsageReconciliation {
    pub totals: ModelUsageTotals,
    pub by_provider: BTreeMap<String, ModelUsageTotals>,
}

/// Stable retry/Usage ledger failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRetryUsageErrorKind {
    InvalidRequest,
    IdentityMismatch,
    InvalidState,
    AttemptConflict,
    TerminalConflict,
    RequestConflict,
    UsageConflict,
    CorruptState,
    Storage,
}

/// Bounded retry/Usage ledger error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRetryUsageError {
    kind: ModelRetryUsageErrorKind,
    message: &'static str,
}

impl ModelRetryUsageError {
    const fn new(kind: ModelRetryUsageErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            ModelRetryUsageErrorKind::InvalidRequest,
            "model retry or Usage request is invalid",
        )
    }

    const fn identity_mismatch() -> Self {
        Self::new(
            ModelRetryUsageErrorKind::IdentityMismatch,
            "model retry authority or terminal identity does not match",
        )
    }

    const fn corrupt() -> Self {
        Self::new(
            ModelRetryUsageErrorKind::CorruptState,
            "model retry or Usage durable state is invalid",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> ModelRetryUsageErrorKind {
        self.kind
    }
}

impl fmt::Display for ModelRetryUsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ModelRetryUsageError {}

impl From<StorageError> for ModelRetryUsageError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::RequestConflict => Self::new(
                ModelRetryUsageErrorKind::RequestConflict,
                "model retry command identity was reused with changed input",
            ),
            StorageErrorKind::RevisionConflict => Self::new(
                ModelRetryUsageErrorKind::UsageConflict,
                "model Usage identity or retry revision already exists",
            ),
            StorageErrorKind::InvalidInput => Self::invalid(),
            StorageErrorKind::RequestReplayMissing
            | StorageErrorKind::JournalAlreadyExists
            | StorageErrorKind::JournalNotFound
            | StorageErrorKind::JournalConflict
            | StorageErrorKind::EventCursorExpired
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => Self::new(
                ModelRetryUsageErrorKind::Storage,
                "model retry or Usage storage operation failed",
            ),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RouteSnapshot {
    provider_id: String,
    model_id: String,
    credential_reference_id: String,
    fingerprint: String,
    max_attempts: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AttemptStatus {
    Active,
    Failed,
    Cancelled,
    Succeeded,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AttemptRecord {
    attempt: u64,
    step_index: usize,
    attempt_on_step: u64,
    reserve_request_id: RequestId,
    model_exchange_id: ModelExchangeId,
    status: AttemptStatus,
    failure: Option<ModelAttemptFailureFact>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingAttempt {
    attempt: u64,
    step_index: usize,
    attempt_on_step: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetryUsageState {
    schema: String,
    request_id: RequestId,
    attribution: ModelUsageAttribution,
    policy_id: String,
    policy_revision: u64,
    plan_fingerprint: String,
    steps: Vec<RouteSnapshot>,
    revision: u64,
    attempts: Vec<AttemptRecord>,
    pending: Option<PendingAttempt>,
    usage: Vec<SettledModelUsage>,
    terminal: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UsageCatalogHead {
    schema: String,
    revision: u64,
    entry_count: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsageSourceEntry {
    schema: String,
    pub sequence: u64,
    pub source_digest: String,
    pub request_id: RequestId,
    pub attribution: ModelUsageAttribution,
    pub model_exchange_id: ModelExchangeId,
    pub route_authority_fingerprint: String,
    pub settled_at: Instant,
    pub usage: SettledModelUsage,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", content = "receipt", rename_all = "snake_case")]
enum RetryUsageEvent {
    Started(ModelAttemptStartReceipt),
    Failed(ModelRetryDecisionReceipt),
    Settled(ModelUsageSettlementReceipt),
}

struct CommandReceipt {
    identity: ReceiptIdentity,
    digest: Sha256Digest,
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum RetryCommandDigest<'a> {
    Start {
        admission: &'a ModelReservationReceipt,
    },
    Fail {
        gateway: &'a ProviderGatewaySettlement,
    },
    Complete {
        gateway: &'a ProviderGatewaySettlement,
    },
}

/// Durable retry/fallback and request-level Usage service.
pub struct ModelRetryUsageService<'a> {
    storage: &'a mut dyn ProductStateStorage,
}

impl<'a> ModelRetryUsageService<'a> {
    #[must_use]
    pub fn new(storage: &'a mut dyn ProductStateStorage) -> Self {
        Self { storage }
    }

    /// Starts the primary or one previously authorized retry attempt.
    ///
    /// # Errors
    ///
    /// Rejects denied admission, route/sequence mismatch, replay conflict,
    /// corrupt state, or storage failure.
    pub fn start_attempt(
        &mut self,
        request: &ModelRetryUsageRequest,
        command: &ModelAttemptStartCommand,
    ) -> Result<ModelAttemptStartReceipt, ModelRetryUsageError> {
        validate_request(request)?;
        validate_request_id(&command.command_request_id)?;
        if !command.admission.admitted() {
            return Err(ModelRetryUsageError::invalid());
        }
        let digest = RetryCommandDigest::Start {
            admission: &command.admission,
        };
        let receipt = command_receipt(request, &command.command_request_id, &digest)?;
        if let Some(replay) = self
            .storage
            .load_receipt(&receipt.identity, &receipt.digest)?
        {
            let result = start_from_receipt(&replay, true)?;
            verify_started_context(self.storage, request, &result)?;
            return Ok(result);
        }
        for _attempt in 0..MAX_COMMIT_RETRIES {
            if let Some(replay) = self
                .storage
                .load_receipt(&receipt.identity, &receipt.digest)?
            {
                let result = start_from_receipt(&replay, true)?;
                verify_started_context(self.storage, request, &result)?;
                return Ok(result);
            }
            let (mut state, expected_revision) = load_or_new(self.storage, request)?;
            let pending = expected_pending(&state)?;
            let step = request
                .plan
                .steps
                .get(pending.step_index)
                .ok_or_else(ModelRetryUsageError::corrupt)?;
            if command.admission.route_authority_fingerprint != step.authority.fingerprint() {
                return Err(ModelRetryUsageError::identity_mismatch());
            }
            if state
                .attempts
                .iter()
                .any(|attempt| attempt.model_exchange_id == command.admission.model_exchange_id)
            {
                return Err(ModelRetryUsageError::new(
                    ModelRetryUsageErrorKind::AttemptConflict,
                    "model exchange already belongs to another attempt",
                ));
            }
            state.attempts.push(AttemptRecord {
                attempt: pending.attempt,
                step_index: pending.step_index,
                attempt_on_step: pending.attempt_on_step,
                reserve_request_id: command.admission.request_id.clone(),
                model_exchange_id: command.admission.model_exchange_id.clone(),
                status: AttemptStatus::Active,
                failure: None,
            });
            state.pending = None;
            state.revision = next_revision(expected_revision)?;
            let result = ModelAttemptStartReceipt {
                request_id: request.request_id.clone(),
                reservation_request_id: command.admission.request_id.clone(),
                model_exchange_id: command.admission.model_exchange_id.clone(),
                attempt: pending.attempt,
                provider_id: step.authority.route().provider_id.clone(),
                model_id: step.authority.route().model_id.clone(),
                route_fingerprint: step.authority.fingerprint().to_owned(),
                revision: state.revision,
                idempotent_replay: false,
            };
            let context_mutation = retry_context_mutation(request, &result)?;
            match commit_state(
                self.storage,
                request,
                &receipt,
                expected_revision,
                &state,
                &RetryUsageEvent::Started(result.clone()),
                vec![context_mutation],
            ) {
                Ok(durable) => {
                    let result = start_from_receipt(&durable, durable.idempotent_replay)?;
                    verify_started_context(self.storage, request, &result)?;
                    return Ok(result);
                }
                Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(storage_retry_exhausted())
    }

    /// Records one failed/cancelled attempt and returns the only authorized
    /// same-route retry, fallback, or stop decision.
    ///
    /// # Errors
    ///
    /// Rejects ambiguous identities, terminal conflicts, duplicate Usage ids,
    /// corrupt state, changed replay, or storage failure.
    pub fn fail_attempt(
        &mut self,
        request: &ModelRetryUsageRequest,
        command: &ModelAttemptFailureCommand,
    ) -> Result<ModelRetryDecisionReceipt, ModelRetryUsageError> {
        validate_request(request)?;
        validate_request_id(&command.command_request_id)?;
        let digest = RetryCommandDigest::Fail {
            gateway: &command.gateway,
        };
        let receipt = command_receipt(request, &command.command_request_id, &digest)?;
        if let Some(replay) = self
            .storage
            .load_receipt(&receipt.identity, &receipt.digest)?
        {
            return failure_from_receipt(&replay, true);
        }
        for _attempt in 0..MAX_COMMIT_RETRIES {
            let (mut state, expected_revision) = load_existing(self.storage, request)?;
            let active_index = active_attempt_index(&state, &command.gateway.model_exchange_id)?;
            validate_failure_terminal(request, &state, active_index, command)?;
            let charge = command
                .gateway
                .charge
                .as_ref()
                .map(|charge| settled_usage(&state, active_index, charge))
                .transpose()?;
            let failure = command
                .gateway
                .failure
                .ok_or_else(ModelRetryUsageError::identity_mismatch)?;
            let (action, pending) = next_after_failure(request, &state, active_index, failure);
            let attempt = state.attempts[active_index].attempt;
            state.attempts[active_index].status =
                if command.gateway.outcome == ProviderGatewayTerminalOutcome::Cancelled {
                    AttemptStatus::Cancelled
                } else {
                    AttemptStatus::Failed
                };
            state.attempts[active_index].failure = Some(failure);
            if let Some(usage) = &charge {
                state.usage.push(usage.clone());
            }
            state.pending.clone_from(&pending);
            state.terminal = action == ModelRetryAction::Stop;
            state.revision = next_revision(expected_revision)?;
            let next_step = pending
                .as_ref()
                .and_then(|pending| request.plan.steps.get(pending.step_index));
            let result = ModelRetryDecisionReceipt {
                request_id: request.request_id.clone(),
                model_exchange_id: command.gateway.model_exchange_id.clone(),
                attempt,
                action,
                next_attempt: pending.as_ref().map(|pending| pending.attempt),
                next_provider_id: next_step.map(|step| step.authority.route().provider_id.clone()),
                next_model_id: next_step.map(|step| step.authority.route().model_id.clone()),
                next_route_fingerprint: next_step
                    .map(|step| step.authority.fingerprint().to_owned()),
                revision: state.revision,
                idempotent_replay: false,
            };
            let usage_mutations = charge
                .as_ref()
                .map(|usage| usage_mutations(self.storage, request, &command.gateway, usage))
                .transpose()?
                .unwrap_or_default();
            match commit_state(
                self.storage,
                request,
                &receipt,
                expected_revision,
                &state,
                &RetryUsageEvent::Failed(result.clone()),
                usage_mutations,
            ) {
                Ok(durable) => return failure_from_receipt(&durable, durable.idempotent_replay),
                Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {
                    if let Some(replay) = self
                        .storage
                        .load_receipt(&receipt.identity, &receipt.digest)?
                    {
                        return failure_from_receipt(&replay, true);
                    }
                    if usage_identity_exists(self.storage, charge.as_ref())? {
                        return Err(ModelRetryUsageError::new(
                            ModelRetryUsageErrorKind::UsageConflict,
                            "Provider Usage identity already belongs to another settlement",
                        ));
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(storage_retry_exhausted())
    }

    /// Settles one successful attempt and one globally unique Provider Usage fact.
    ///
    /// # Errors
    ///
    /// Rejects identity/Usage mismatch, duplicate Usage ids, another terminal,
    /// corrupt state, changed replay, or storage failure.
    pub fn complete_attempt(
        &mut self,
        request: &ModelRetryUsageRequest,
        command: &ModelAttemptCompletionCommand,
    ) -> Result<ModelUsageSettlementReceipt, ModelRetryUsageError> {
        validate_request(request)?;
        validate_request_id(&command.command_request_id)?;
        let digest = RetryCommandDigest::Complete {
            gateway: &command.gateway,
        };
        let receipt = command_receipt(request, &command.command_request_id, &digest)?;
        if let Some(replay) = self
            .storage
            .load_receipt(&receipt.identity, &receipt.digest)?
        {
            return settlement_from_receipt(&replay, true);
        }
        for _attempt in 0..MAX_COMMIT_RETRIES {
            if let Some(replay) = self
                .storage
                .load_receipt(&receipt.identity, &receipt.digest)?
            {
                return settlement_from_receipt(&replay, true);
            }
            let prepared = (|| {
                let (mut state, expected_revision) = load_existing(self.storage, request)?;
                let active_index =
                    active_attempt_index(&state, &command.gateway.model_exchange_id)?;
                validate_completion_terminal(request, &state, active_index, command)?;
                let charge = command
                    .gateway
                    .charge
                    .as_ref()
                    .ok_or_else(ModelRetryUsageError::identity_mismatch)?;
                let usage = settled_usage(&state, active_index, charge)?;
                let usage_mutations =
                    usage_mutations(self.storage, request, &command.gateway, &usage)?;
                state.attempts[active_index].status = AttemptStatus::Succeeded;
                state.usage.push(usage.clone());
                state.terminal = true;
                state.pending = None;
                state.revision = next_revision(expected_revision)?;
                let result = ModelUsageSettlementReceipt {
                    request_id: request.request_id.clone(),
                    model_exchange_id: command.gateway.model_exchange_id.clone(),
                    usage: usage.clone(),
                    revision: state.revision,
                    idempotent_replay: false,
                };
                Ok((state, expected_revision, result, usage, usage_mutations))
            })();
            let (state, expected_revision, result, usage, usage_mutations) = match prepared {
                Ok(prepared) => prepared,
                Err(source) => match recover_exact_completion(self.storage, &receipt, source) {
                    Ok(replay) => return Ok(replay),
                    Err(source)
                        if source.kind() == ModelRetryUsageErrorKind::UsageConflict
                            && completion_usage_exists_exactly(
                                self.storage,
                                request,
                                &command.gateway,
                            )? =>
                    {
                        continue;
                    }
                    Err(source) => return Err(source),
                },
            };
            match commit_state(
                self.storage,
                request,
                &receipt,
                expected_revision,
                &state,
                &RetryUsageEvent::Settled(result),
                usage_mutations,
            ) {
                Ok(durable) => {
                    return settlement_from_receipt(&durable, durable.idempotent_replay);
                }
                Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {
                    if let Some(replay) = self
                        .storage
                        .load_receipt(&receipt.identity, &receipt.digest)?
                    {
                        return settlement_from_receipt(&replay, true);
                    }
                    if completion_usage_exists_exactly(self.storage, request, &command.gateway)? {
                        continue;
                    }
                    if usage_identity_exists(self.storage, Some(&usage))? {
                        return Err(ModelRetryUsageError::new(
                            ModelRetryUsageErrorKind::UsageConflict,
                            "Provider Usage identity already belongs to another settlement",
                        ));
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(storage_retry_exhausted())
    }

    /// Scans immutable request Usage facts and reconciles exact dimensions.
    ///
    /// # Errors
    ///
    /// Rejects malformed filters, corrupt rows, or unavailable bounded scans.
    pub fn reconcile(
        &self,
        filter: &ModelUsageFilter,
    ) -> Result<ModelUsageReconciliation, ModelRetryUsageError> {
        validate_filter(filter)?;
        let (catalog, _revision) = load_usage_catalog(self.storage)?;
        let mut result = ModelUsageReconciliation::default();
        for sequence in 1..=catalog.entry_count {
            let entry = load_usage_entry(self.storage, sequence)?;
            if attribution_matches(&entry.attribution, filter)
                && filter
                    .provider_id
                    .as_ref()
                    .is_none_or(|provider| provider == &entry.usage.provider_id)
            {
                add_usage(&mut result.totals, &entry.usage)?;
                add_usage(
                    result
                        .by_provider
                        .entry(entry.usage.provider_id.clone())
                        .or_default(),
                    &entry.usage,
                )?;
            }
        }
        Ok(result)
    }

    /// Loads the immutable catalog entry owned by one Provider usage receipt.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, corrupt source rows, or storage failures.
    pub fn usage_source(
        &self,
        provider_usage_id: &str,
    ) -> Result<Option<ModelUsageSourceEntry>, ModelRetryUsageError> {
        validate_token(provider_usage_id, 200)?;
        let Some(stored) = self
            .storage
            .load_state(&usage_identity_stream(provider_usage_id)?)?
        else {
            return Ok(None);
        };
        let entry: ModelUsageSourceEntry =
            serde_json::from_slice(&stored.payload).map_err(|_| ModelRetryUsageError::corrupt())?;
        let entry = load_usage_entry(self.storage, entry.sequence)?;
        if entry.usage.provider_usage_id != provider_usage_id {
            return Err(ModelRetryUsageError::corrupt());
        }
        Ok(Some(entry))
    }

    /// Reads one bounded page from a fixed immutable source-catalog snapshot.
    ///
    /// Each call inspects at most 1,000 source rows. A selective filter may
    /// therefore return an empty page with a continuation cursor.
    ///
    /// # Errors
    ///
    /// Rejects invalid filters, limits, changed cursors, corrupt rows, or
    /// storage failures.
    pub fn scan_usage_sources(
        &self,
        filter: &ModelUsageFilter,
        cursor: Option<&ModelUsageSourceCursor>,
        limit: u64,
    ) -> Result<ModelUsageSourcePage, ModelRetryUsageError> {
        validate_filter(filter)?;
        if limit == 0 || limit > MAX_USAGE_SOURCE_PAGE_SIZE {
            return Err(ModelRetryUsageError::invalid());
        }
        let filter_digest = canonical_digest(filter)?;
        let (catalog, _) = load_usage_catalog(self.storage)?;
        let (snapshot_sequence, after_sequence) = match cursor {
            Some(cursor) => source_cursor_position(cursor, &filter_digest, &catalog)?,
            None => (catalog.entry_count, 0),
        };
        let (entries, last_scanned) = scan_source_entries(
            self.storage,
            filter,
            snapshot_sequence,
            after_sequence,
            limit,
        )?;
        let next = (last_scanned < snapshot_sequence).then_some(ModelUsageSourceCursor {
            filter_digest,
            snapshot_sequence,
            after_sequence: last_scanned,
        });
        Ok(ModelUsageSourcePage {
            snapshot_sequence,
            entries,
            next,
        })
    }
}

pub(crate) fn validate_request(
    request: &ModelRetryUsageRequest,
) -> Result<(), ModelRetryUsageError> {
    validate_request_id(&request.request_id)?;
    validate_attribution(&request.attribution)?;
    if request.plan.steps.is_empty() || request.plan.fingerprint.is_empty() {
        return Err(ModelRetryUsageError::invalid());
    }
    let expected = ModelUsageAttribution::from_verified_user(
        request.plan.steps[0].authority(),
        request.attribution.delivery_id.clone(),
        request.attribution.user_id.clone(),
    )?;
    if expected != request.attribution {
        return Err(ModelRetryUsageError::identity_mismatch());
    }
    Ok(())
}

fn expected_pending(state: &RetryUsageState) -> Result<PendingAttempt, ModelRetryUsageError> {
    if state.terminal
        || state
            .attempts
            .iter()
            .any(|attempt| attempt.status == AttemptStatus::Active)
    {
        return Err(ModelRetryUsageError::new(
            ModelRetryUsageErrorKind::InvalidState,
            "model request cannot start another attempt",
        ));
    }
    if state.attempts.is_empty() {
        Ok(PendingAttempt {
            attempt: 1,
            step_index: 0,
            attempt_on_step: 1,
        })
    } else {
        state.pending.clone().ok_or_else(|| {
            ModelRetryUsageError::new(
                ModelRetryUsageErrorKind::InvalidState,
                "model retry has no pending authorized attempt",
            )
        })
    }
}

fn next_after_failure(
    request: &ModelRetryUsageRequest,
    state: &RetryUsageState,
    index: usize,
    failure: ModelAttemptFailureFact,
) -> (ModelRetryAction, Option<PendingAttempt>) {
    if !failure.safe_to_retry() {
        return (ModelRetryAction::Stop, None);
    }
    let current = &state.attempts[index];
    let step = &request.plan.steps[current.step_index];
    if current.attempt_on_step < step.max_attempts {
        return (
            ModelRetryAction::RetrySameRoute,
            Some(PendingAttempt {
                attempt: current.attempt + 1,
                step_index: current.step_index,
                attempt_on_step: current.attempt_on_step + 1,
            }),
        );
    }
    let next_step = current.step_index + 1;
    if next_step < request.plan.steps.len() {
        return (
            ModelRetryAction::Fallback,
            Some(PendingAttempt {
                attempt: current.attempt + 1,
                step_index: next_step,
                attempt_on_step: 1,
            }),
        );
    }
    (ModelRetryAction::Stop, None)
}

fn validate_failure_terminal(
    request: &ModelRetryUsageRequest,
    state: &RetryUsageState,
    index: usize,
    command: &ModelAttemptFailureCommand,
) -> Result<(), ModelRetryUsageError> {
    let attempt = &state.attempts[index];
    let step = &request.plan.steps[attempt.step_index];
    validate_gateway(attempt, step, &command.gateway)?;
    if command.gateway.outcome == ProviderGatewayTerminalOutcome::Succeeded
        || command.gateway.admission_terminal.model_exchange_id != command.gateway.model_exchange_id
        || command
            .gateway
            .admission_terminal
            .route_authority_fingerprint
            != step.authority.fingerprint()
    {
        return Err(ModelRetryUsageError::identity_mismatch());
    }
    let terminal_class_matches = match command.gateway.outcome {
        ProviderGatewayTerminalOutcome::Cancelled => command
            .gateway
            .failure
            .is_some_and(|failure| failure.kind == ModelAttemptFailureKind::Cancelled),
        ProviderGatewayTerminalOutcome::Failed => command
            .gateway
            .failure
            .is_some_and(|failure| failure.kind != ModelAttemptFailureKind::Cancelled),
        ProviderGatewayTerminalOutcome::Succeeded => false,
    };
    if !terminal_class_matches {
        return Err(ModelRetryUsageError::identity_mismatch());
    }
    if command.gateway.charge.is_some()
        && command
            .gateway
            .failure
            .is_some_and(ModelAttemptFailureFact::safe_to_retry)
    {
        return Err(ModelRetryUsageError::identity_mismatch());
    }
    match (
        command.gateway.outcome,
        &command.gateway.charge,
        command.gateway.admission_terminal.outcome,
    ) {
        (
            ProviderGatewayTerminalOutcome::Failed,
            None,
            ModelReservationTerminalOutcome::ProviderFailed,
        )
        | (
            ProviderGatewayTerminalOutcome::Cancelled,
            None,
            ModelReservationTerminalOutcome::Cancelled,
        ) => Ok(()),
        (
            ProviderGatewayTerminalOutcome::Failed,
            Some(charge),
            ModelReservationTerminalOutcome::Completed,
        ) => validate_charge_against_terminal(charge, &command.gateway.admission_terminal),
        _ => Err(ModelRetryUsageError::identity_mismatch()),
    }
}

fn validate_completion_terminal(
    request: &ModelRetryUsageRequest,
    state: &RetryUsageState,
    index: usize,
    command: &ModelAttemptCompletionCommand,
) -> Result<(), ModelRetryUsageError> {
    let attempt = &state.attempts[index];
    validate_gateway(
        attempt,
        &request.plan.steps[attempt.step_index],
        &command.gateway,
    )?;
    if command.gateway.outcome != ProviderGatewayTerminalOutcome::Succeeded
        || command.gateway.admission_terminal.model_exchange_id != command.gateway.model_exchange_id
        || command
            .gateway
            .admission_terminal
            .route_authority_fingerprint
            != request.plan.steps[attempt.step_index]
                .authority
                .fingerprint()
        || command.gateway.admission_terminal.outcome != ModelReservationTerminalOutcome::Completed
        || command.gateway.failure.is_some()
    {
        return Err(ModelRetryUsageError::identity_mismatch());
    }
    let charge = command
        .gateway
        .charge
        .as_ref()
        .ok_or_else(ModelRetryUsageError::identity_mismatch)?;
    validate_charge_against_terminal(charge, &command.gateway.admission_terminal)
}

fn validate_gateway(
    attempt: &AttemptRecord,
    step: &ModelRetryStep,
    gateway: &ProviderGatewaySettlement,
) -> Result<(), ModelRetryUsageError> {
    if gateway.model_exchange_id != attempt.model_exchange_id
        || gateway.request_id != attempt.reserve_request_id
        || gateway.provider_id != step.authority.route().provider_id
        || gateway.model_id != step.authority.route().model_id
    {
        return Err(ModelRetryUsageError::identity_mismatch());
    }
    Ok(())
}

fn validate_charge_against_terminal(
    charge: &ModelAttemptCharge,
    terminal: &ModelReservationTerminalReceipt,
) -> Result<(), ModelRetryUsageError> {
    let usage = normalized_usage(charge)?;
    if usage.total_tokens != terminal.actual_tokens
        || charge.cost_micros != terminal.actual_cost_micros
    {
        return Err(ModelRetryUsageError::identity_mismatch());
    }
    Ok(())
}

fn settled_usage(
    state: &RetryUsageState,
    index: usize,
    charge: &ModelAttemptCharge,
) -> Result<SettledModelUsage, ModelRetryUsageError> {
    if state
        .usage
        .iter()
        .any(|usage| usage.provider_usage_id == charge.provider_usage_id)
    {
        return Err(ModelRetryUsageError::new(
            ModelRetryUsageErrorKind::UsageConflict,
            "Provider Usage identity already exists in this request",
        ));
    }
    let attempt = &state.attempts[index];
    let mut usage = normalized_usage(charge)?;
    usage.attempt = attempt.attempt;
    let route = &state.steps[attempt.step_index];
    usage.provider_id.clone_from(&route.provider_id);
    usage.model_id.clone_from(&route.model_id);
    Ok(usage)
}

fn normalized_usage(
    charge: &ModelAttemptCharge,
) -> Result<SettledModelUsage, ModelRetryUsageError> {
    validate_token(&charge.provider_usage_id, 200)?;
    if charge.usage.cached_input_tokens > charge.usage.input_tokens
        || charge.usage.cache_write_input_tokens > charge.usage.input_tokens
        || charge.usage.reasoning_output_tokens > charge.usage.output_tokens
        || charge.cost_micros > MAX_SAFE_INTEGER
    {
        return Err(ModelRetryUsageError::invalid());
    }
    let total_tokens = checked_add(charge.usage.input_tokens, charge.usage.output_tokens)?;
    Ok(SettledModelUsage {
        provider_usage_id: charge.provider_usage_id.clone(),
        attempt: 0,
        provider_id: String::new(),
        model_id: String::new(),
        input_tokens: charge.usage.input_tokens,
        cached_input_tokens: charge.usage.cached_input_tokens,
        cache_write_input_tokens: charge.usage.cache_write_input_tokens,
        output_tokens: charge.usage.output_tokens,
        reasoning_output_tokens: charge.usage.reasoning_output_tokens,
        total_tokens,
        cost_micros: charge.cost_micros,
    })
}

fn active_attempt_index(
    state: &RetryUsageState,
    exchange: &ModelExchangeId,
) -> Result<usize, ModelRetryUsageError> {
    if state.terminal {
        return Err(ModelRetryUsageError::new(
            ModelRetryUsageErrorKind::TerminalConflict,
            "model request already reached a terminal state",
        ));
    }
    state
        .attempts
        .iter()
        .position(|attempt| {
            attempt.model_exchange_id == *exchange && attempt.status == AttemptStatus::Active
        })
        .ok_or_else(|| {
            ModelRetryUsageError::new(
                ModelRetryUsageErrorKind::AttemptConflict,
                "model exchange is not the active attempt",
            )
        })
}

fn load_or_new(
    storage: &dyn ProductStateStorage,
    request: &ModelRetryUsageRequest,
) -> Result<(RetryUsageState, u64), ModelRetryUsageError> {
    let stream = stream_id(&request.request_id)?;
    match storage.load_state(&stream)? {
        Some(stored) => decode_state(&stored, request),
        None => Ok((new_state(request), 0)),
    }
}

fn load_existing(
    storage: &dyn ProductStateStorage,
    request: &ModelRetryUsageRequest,
) -> Result<(RetryUsageState, u64), ModelRetryUsageError> {
    let stored = storage
        .load_state(&stream_id(&request.request_id)?)?
        .ok_or_else(|| {
            ModelRetryUsageError::new(
                ModelRetryUsageErrorKind::InvalidState,
                "model retry request has not started",
            )
        })?;
    decode_state(&stored, request)
}

fn new_state(request: &ModelRetryUsageRequest) -> RetryUsageState {
    RetryUsageState {
        schema: STATE_SCHEMA.to_owned(),
        request_id: request.request_id.clone(),
        attribution: request.attribution.clone(),
        policy_id: request.plan.policy_id.clone(),
        policy_revision: request.plan.policy_revision,
        plan_fingerprint: request.plan.fingerprint.clone(),
        steps: request.plan.steps.iter().map(step_snapshot).collect(),
        revision: 0,
        attempts: Vec::new(),
        pending: None,
        usage: Vec::new(),
        terminal: false,
    }
}

fn decode_state(
    stored: &StoredState,
    request: &ModelRetryUsageRequest,
) -> Result<(RetryUsageState, u64), ModelRetryUsageError> {
    let state = decode_unbound_state(stored)?;
    if state.request_id != request.request_id
        || state.attribution != request.attribution
        || state.plan_fingerprint != request.plan.fingerprint
        || state.steps
            != request
                .plan
                .steps
                .iter()
                .map(step_snapshot)
                .collect::<Vec<_>>()
    {
        return Err(ModelRetryUsageError::identity_mismatch());
    }
    Ok((state, stored.revision))
}

fn decode_unbound_state(stored: &StoredState) -> Result<RetryUsageState, ModelRetryUsageError> {
    let state: RetryUsageState =
        serde_json::from_slice(&stored.payload).map_err(|_| ModelRetryUsageError::corrupt())?;
    if serde_json::to_vec(&state).map_err(|_| ModelRetryUsageError::corrupt())? != stored.payload
        || state.schema != STATE_SCHEMA
        || state.revision != stored.revision
        || stored.stream_id != stream_id(&state.request_id)?
        || state.revision == 0
    {
        return Err(ModelRetryUsageError::corrupt());
    }
    validate_retry_state(&state)?;
    Ok(state)
}

fn validate_retry_state(state: &RetryUsageState) -> Result<(), ModelRetryUsageError> {
    validate_request_id(&state.request_id)?;
    validate_attribution(&state.attribution)?;
    validate_token(&state.policy_id, 200)?;
    validate_sha256(&state.plan_fingerprint)?;
    if state.policy_revision == 0
        || state.policy_revision > MAX_SAFE_INTEGER
        || state.steps.is_empty()
        || state.steps.len() > MAX_TOTAL_ATTEMPTS_USIZE
        || state.attempts.is_empty()
        || state.attempts.len() > MAX_TOTAL_ATTEMPTS_USIZE
    {
        return Err(ModelRetryUsageError::corrupt());
    }
    let mut route_fingerprints = BTreeSet::new();
    for step in &state.steps {
        validate_token(&step.provider_id, 128)?;
        validate_token(&step.model_id, 200)?;
        validate_prefixed_id(&step.credential_reference_id, "crd_")?;
        validate_sha256(&step.fingerprint)?;
        if step.max_attempts == 0
            || step.max_attempts > MAX_TOTAL_ATTEMPTS
            || !route_fingerprints.insert(step.fingerprint.as_str())
        {
            return Err(ModelRetryUsageError::corrupt());
        }
    }
    validate_attempt_sequence(state)?;
    validate_state_usage(state)?;
    validate_state_terminal(state)
}

fn validate_attempt_sequence(state: &RetryUsageState) -> Result<(), ModelRetryUsageError> {
    let mut previous: Option<&AttemptRecord> = None;
    for (index, attempt) in state.attempts.iter().enumerate() {
        validate_request_id(&attempt.reserve_request_id)?;
        validate_prefixed_id(&attempt.model_exchange_id.0, "mdl_")?;
        if attempt.attempt
            != u64::try_from(index + 1).map_err(|_| ModelRetryUsageError::corrupt())?
            || attempt.step_index >= state.steps.len()
            || attempt.attempt_on_step == 0
            || attempt.attempt_on_step > state.steps[attempt.step_index].max_attempts
        {
            return Err(ModelRetryUsageError::corrupt());
        }
        if let Some(previous) = previous {
            let failure = previous.failure.ok_or_else(ModelRetryUsageError::corrupt)?;
            if previous.status != AttemptStatus::Failed || !failure.safe_to_retry() {
                return Err(ModelRetryUsageError::corrupt());
            }
            let expected =
                if previous.attempt_on_step < state.steps[previous.step_index].max_attempts {
                    (previous.step_index, previous.attempt_on_step + 1)
                } else {
                    (previous.step_index + 1, 1)
                };
            if (attempt.step_index, attempt.attempt_on_step) != expected {
                return Err(ModelRetryUsageError::corrupt());
            }
        } else if attempt.step_index != 0 || attempt.attempt_on_step != 1 {
            return Err(ModelRetryUsageError::corrupt());
        }
        match (attempt.status, attempt.failure) {
            (AttemptStatus::Active | AttemptStatus::Succeeded, None)
            | (
                AttemptStatus::Failed,
                Some(ModelAttemptFailureFact {
                    kind:
                        ModelAttemptFailureKind::Authentication
                        | ModelAttemptFailureKind::InvalidRequest
                        | ModelAttemptFailureKind::RateLimit
                        | ModelAttemptFailureKind::Quota
                        | ModelAttemptFailureKind::Timeout
                        | ModelAttemptFailureKind::Transport
                        | ModelAttemptFailureKind::Server
                        | ModelAttemptFailureKind::ContextWindowExceeded
                        | ModelAttemptFailureKind::ProviderUnavailable
                        | ModelAttemptFailureKind::Protocol
                        | ModelAttemptFailureKind::Unknown,
                    ..
                }),
            )
            | (
                AttemptStatus::Cancelled,
                Some(ModelAttemptFailureFact {
                    kind: ModelAttemptFailureKind::Cancelled,
                    ..
                }),
            ) => {}
            _ => return Err(ModelRetryUsageError::corrupt()),
        }
        previous = Some(attempt);
    }
    Ok(())
}

fn validate_state_usage(state: &RetryUsageState) -> Result<(), ModelRetryUsageError> {
    let mut usage_ids = BTreeSet::new();
    let mut usage_attempts = BTreeSet::new();
    for usage in &state.usage {
        normalized_stored_usage(usage)?;
        let attempt_index =
            usize::try_from(usage.attempt - 1).map_err(|_| ModelRetryUsageError::corrupt())?;
        let attempt = state
            .attempts
            .get(attempt_index)
            .ok_or_else(ModelRetryUsageError::corrupt)?;
        let step = &state.steps[attempt.step_index];
        if !matches!(
            attempt.status,
            AttemptStatus::Failed | AttemptStatus::Succeeded
        ) || usage.provider_id != step.provider_id
            || usage.model_id != step.model_id
            || (attempt.status == AttemptStatus::Failed
                && attempt
                    .failure
                    .is_some_and(ModelAttemptFailureFact::safe_to_retry))
            || !usage_ids.insert(usage.provider_usage_id.as_str())
            || !usage_attempts.insert(usage.attempt)
        {
            return Err(ModelRetryUsageError::corrupt());
        }
    }
    Ok(())
}

fn validate_state_terminal(state: &RetryUsageState) -> Result<(), ModelRetryUsageError> {
    let last = state
        .attempts
        .last()
        .ok_or_else(ModelRetryUsageError::corrupt)?;
    match last.status {
        AttemptStatus::Active => {
            if state.pending.is_none() && !state.terminal {
                Ok(())
            } else {
                Err(ModelRetryUsageError::corrupt())
            }
        }
        AttemptStatus::Succeeded | AttemptStatus::Cancelled => {
            if state.pending.is_none() && state.terminal {
                Ok(())
            } else {
                Err(ModelRetryUsageError::corrupt())
            }
        }
        AttemptStatus::Failed => {
            let expected = pending_after_stored_failure(state, last)?;
            if state.pending == expected && state.terminal == expected.is_none() {
                Ok(())
            } else {
                Err(ModelRetryUsageError::corrupt())
            }
        }
    }
}

fn pending_after_stored_failure(
    state: &RetryUsageState,
    attempt: &AttemptRecord,
) -> Result<Option<PendingAttempt>, ModelRetryUsageError> {
    let failure = attempt.failure.ok_or_else(ModelRetryUsageError::corrupt)?;
    if !failure.safe_to_retry() {
        return Ok(None);
    }
    let step = state
        .steps
        .get(attempt.step_index)
        .ok_or_else(ModelRetryUsageError::corrupt)?;
    if attempt.attempt_on_step < step.max_attempts {
        return Ok(Some(PendingAttempt {
            attempt: attempt.attempt + 1,
            step_index: attempt.step_index,
            attempt_on_step: attempt.attempt_on_step + 1,
        }));
    }
    let next_step = attempt.step_index + 1;
    Ok(state.steps.get(next_step).map(|_| PendingAttempt {
        attempt: attempt.attempt + 1,
        step_index: next_step,
        attempt_on_step: 1,
    }))
}

fn step_snapshot(step: &ModelRetryStep) -> RouteSnapshot {
    RouteSnapshot {
        provider_id: step.authority.route().provider_id.clone(),
        model_id: step.authority.route().model_id.clone(),
        credential_reference_id: step.authority.route().credential_reference_id.0.clone(),
        fingerprint: step.authority.fingerprint().to_owned(),
        max_attempts: step.max_attempts,
    }
}

fn load_usage_catalog(
    storage: &dyn ProductStateStorage,
) -> Result<(UsageCatalogHead, u64), ModelRetryUsageError> {
    let Some(stored) = storage.load_state(USAGE_CATALOG_STREAM)? else {
        return Ok((
            UsageCatalogHead {
                schema: USAGE_CATALOG_SCHEMA.to_owned(),
                revision: 0,
                entry_count: 0,
            },
            0,
        ));
    };
    let catalog: UsageCatalogHead =
        serde_json::from_slice(&stored.payload).map_err(|_| ModelRetryUsageError::corrupt())?;
    if serde_json::to_vec(&catalog).map_err(|_| ModelRetryUsageError::corrupt())? != stored.payload
        || catalog.schema != USAGE_CATALOG_SCHEMA
        || catalog.revision != stored.revision
        || catalog.entry_count != stored.revision
        || catalog.revision == 0
        || catalog.revision > MAX_SAFE_INTEGER
    {
        return Err(ModelRetryUsageError::corrupt());
    }
    Ok((catalog, stored.revision))
}

fn load_usage_entry(
    storage: &dyn ProductStateStorage,
    sequence: u64,
) -> Result<ModelUsageSourceEntry, ModelRetryUsageError> {
    let entry_stream = usage_entry_stream(sequence)?;
    let stored = storage
        .load_state(&entry_stream)?
        .ok_or_else(ModelRetryUsageError::corrupt)?;
    let entry = decode_usage_entry(&stored, sequence)?;
    let identity = storage
        .load_state(&usage_identity_stream(&entry.usage.provider_usage_id)?)?
        .ok_or_else(ModelRetryUsageError::corrupt)?;
    if identity.revision != 1 || identity.payload != stored.payload {
        return Err(ModelRetryUsageError::corrupt());
    }
    Ok(entry)
}

fn source_cursor_position(
    cursor: &ModelUsageSourceCursor,
    filter_digest: &str,
    catalog: &UsageCatalogHead,
) -> Result<(u64, u64), ModelRetryUsageError> {
    if cursor.filter_digest != filter_digest
        || cursor.after_sequence > cursor.snapshot_sequence
        || cursor.snapshot_sequence > catalog.entry_count
        || cursor.snapshot_sequence > MAX_SAFE_INTEGER
    {
        return Err(ModelRetryUsageError::invalid());
    }
    Ok((cursor.snapshot_sequence, cursor.after_sequence))
}

fn scan_source_entries(
    storage: &dyn ProductStateStorage,
    filter: &ModelUsageFilter,
    snapshot_sequence: u64,
    after_sequence: u64,
    limit: u64,
) -> Result<(Vec<ModelUsageSourceEntry>, u64), ModelRetryUsageError> {
    let page_size = usize::try_from(limit).map_err(|_| ModelRetryUsageError::invalid())?;
    let mut entries = Vec::with_capacity(page_size);
    let mut sequence = after_sequence;
    let mut inspected = 0_u64;
    while sequence < snapshot_sequence
        && inspected < MAX_USAGE_SOURCE_SCAN_ROWS
        && entries.len() < page_size
    {
        sequence = checked_add(sequence, 1)?;
        inspected = checked_add(inspected, 1)?;
        let entry = load_usage_entry(storage, sequence)?;
        if attribution_matches(&entry.attribution, filter)
            && filter
                .provider_id
                .as_ref()
                .is_none_or(|provider| provider == &entry.usage.provider_id)
        {
            entries.push(entry);
        }
    }
    Ok((entries, sequence))
}

fn canonical_digest(value: &impl Serialize) -> Result<String, ModelRetryUsageError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ModelRetryUsageError::invalid())?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn usage_source_digest(entry: &ModelUsageSourceEntry) -> Result<String, ModelRetryUsageError> {
    canonical_digest(&(
        entry.sequence,
        &entry.request_id,
        &entry.attribution,
        &entry.model_exchange_id,
        &entry.route_authority_fingerprint,
        &entry.settled_at,
        &entry.usage,
    ))
}

fn decode_usage_entry(
    stored: &StoredState,
    sequence: u64,
) -> Result<ModelUsageSourceEntry, ModelRetryUsageError> {
    let entry: ModelUsageSourceEntry =
        serde_json::from_slice(&stored.payload).map_err(|_| ModelRetryUsageError::corrupt())?;
    if stored.revision != 1
        || stored.stream_id != usage_entry_stream(sequence)?
        || entry.schema != USAGE_ENTRY_SCHEMA
        || entry.sequence != sequence
        || serde_json::to_vec(&entry).map_err(|_| ModelRetryUsageError::corrupt())?
            != stored.payload
        || entry.usage != normalized_stored_usage(&entry.usage)?
        || entry.source_digest != usage_source_digest(&entry)?
    {
        return Err(ModelRetryUsageError::corrupt());
    }
    validate_request_id(&entry.request_id)?;
    validate_attribution(&entry.attribution)?;
    validate_prefixed_id(&entry.model_exchange_id.0, "mdl_")?;
    validate_sha256(&entry.route_authority_fingerprint)?;
    validate_instant(&entry.settled_at)?;
    Ok(entry)
}

fn normalized_stored_usage(
    usage: &SettledModelUsage,
) -> Result<SettledModelUsage, ModelRetryUsageError> {
    validate_token(&usage.provider_usage_id, 200)?;
    validate_token(&usage.provider_id, 128)?;
    validate_token(&usage.model_id, 200)?;
    if usage.attempt == 0
        || usage.attempt > MAX_TOTAL_ATTEMPTS
        || usage.cached_input_tokens > usage.input_tokens
        || usage.cache_write_input_tokens > usage.input_tokens
        || usage.reasoning_output_tokens > usage.output_tokens
        || usage.total_tokens != checked_add(usage.input_tokens, usage.output_tokens)?
        || usage.cost_micros > MAX_SAFE_INTEGER
    {
        return Err(ModelRetryUsageError::corrupt());
    }
    Ok(usage.clone())
}

fn commit_state(
    storage: &mut dyn ProductStateStorage,
    request: &ModelRetryUsageRequest,
    command: &CommandReceipt,
    expected_revision: u64,
    state: &RetryUsageState,
    event: &RetryUsageEvent,
    state_mutations: Vec<StateMutation>,
) -> Result<CommitReceipt, StorageError> {
    let state_payload = serde_json::to_vec(state)
        .map_err(|_| StorageError::invalid_input("model retry state serialization failed"))?;
    let event_payload = serde_json::to_vec(&(EVENT_SCHEMA, event))
        .map_err(|_| StorageError::invalid_input("model retry event serialization failed"))?;
    let event_id = format!(
        "model-retry-usage:{:x}",
        Sha256::digest(
            [
                command.identity.request_id().0.as_bytes(),
                command.digest.0.as_bytes(),
                event_payload.as_slice(),
            ]
            .concat()
        )
    );
    let mut commit = StateCommit::new(
        command.identity.clone(),
        command.digest.clone(),
        stream_id(&request.request_id)
            .map_err(|_| StorageError::invalid_input("model retry stream is invalid"))?,
        expected_revision,
        state_payload,
        vec![NewOutboxEvent::internal(
            event_id,
            EVENT_TOPIC,
            event_payload,
        )],
    );
    for mutation in state_mutations {
        commit = commit.with_state_mutation(mutation);
    }
    storage.commit(&commit)
}

fn usage_mutations(
    storage: &dyn ProductStateStorage,
    request: &ModelRetryUsageRequest,
    gateway: &ProviderGatewaySettlement,
    usage: &SettledModelUsage,
) -> Result<Vec<StateMutation>, ModelRetryUsageError> {
    let identity_stream = usage_identity_stream(&usage.provider_usage_id)?;
    if storage.load_state(&identity_stream)?.is_some() {
        return Err(ModelRetryUsageError::new(
            ModelRetryUsageErrorKind::UsageConflict,
            "Provider Usage identity already belongs to another settlement",
        ));
    }
    let (mut catalog, expected_catalog_revision) = load_usage_catalog(storage)?;
    let sequence = checked_add(catalog.entry_count, 1)?;
    let entry_stream = usage_entry_stream(sequence)?;
    if storage.load_state(&entry_stream)?.is_some() {
        return Err(ModelRetryUsageError::corrupt());
    }
    let mut entry = ModelUsageSourceEntry {
        schema: USAGE_ENTRY_SCHEMA.to_owned(),
        sequence,
        source_digest: String::new(),
        request_id: request.request_id.clone(),
        attribution: request.attribution.clone(),
        model_exchange_id: gateway.model_exchange_id.clone(),
        route_authority_fingerprint: gateway
            .admission_terminal
            .route_authority_fingerprint
            .clone(),
        settled_at: gateway.settled_at.clone(),
        usage: usage.clone(),
    };
    entry.source_digest = usage_source_digest(&entry)?;
    let entry_payload = serde_json::to_vec(&entry).map_err(|_| ModelRetryUsageError::invalid())?;
    catalog.revision = next_revision(expected_catalog_revision)?;
    catalog.entry_count = sequence;
    let catalog_payload =
        serde_json::to_vec(&catalog).map_err(|_| ModelRetryUsageError::invalid())?;
    Ok(vec![
        StateMutation::new(identity_stream, 0, entry_payload.clone())?,
        StateMutation::new(entry_stream, 0, entry_payload)?,
        StateMutation::new(
            USAGE_CATALOG_STREAM.to_owned(),
            expected_catalog_revision,
            catalog_payload,
        )?,
    ])
}

fn usage_identity_exists(
    storage: &dyn ProductStateStorage,
    usage: Option<&SettledModelUsage>,
) -> Result<bool, ModelRetryUsageError> {
    usage.map_or(Ok(false), |usage| {
        storage
            .load_state(&usage_identity_stream(&usage.provider_usage_id)?)
            .map(|state| state.is_some())
            .map_err(Into::into)
    })
}

fn completion_usage_exists_exactly(
    storage: &dyn ProductStateStorage,
    request: &ModelRetryUsageRequest,
    gateway: &ProviderGatewaySettlement,
) -> Result<bool, ModelRetryUsageError> {
    let Some(charge) = gateway.charge.as_ref() else {
        return Ok(false);
    };
    let Some(stored) = storage.load_state(&usage_identity_stream(&charge.provider_usage_id)?)?
    else {
        return Ok(false);
    };
    let indexed: ModelUsageSourceEntry =
        serde_json::from_slice(&stored.payload).map_err(|_| ModelRetryUsageError::corrupt())?;
    let entry = load_usage_entry(storage, indexed.sequence)?;
    let expected_usage = normalized_usage(charge)?;
    Ok(entry.request_id == request.request_id
        && entry.attribution == request.attribution
        && entry.model_exchange_id == gateway.model_exchange_id
        && entry.route_authority_fingerprint
            == gateway.admission_terminal.route_authority_fingerprint
        && entry.settled_at == gateway.settled_at
        && entry.usage.provider_usage_id == expected_usage.provider_usage_id
        && entry.usage.provider_id == gateway.provider_id
        && entry.usage.model_id == gateway.model_id
        && entry.usage.input_tokens == expected_usage.input_tokens
        && entry.usage.cached_input_tokens == expected_usage.cached_input_tokens
        && entry.usage.cache_write_input_tokens == expected_usage.cache_write_input_tokens
        && entry.usage.output_tokens == expected_usage.output_tokens
        && entry.usage.reasoning_output_tokens == expected_usage.reasoning_output_tokens
        && entry.usage.total_tokens == expected_usage.total_tokens
        && entry.usage.cost_micros == expected_usage.cost_micros)
}

fn command_receipt(
    request: &ModelRetryUsageRequest,
    command_request_id: &RequestId,
    command: &RetryCommandDigest<'_>,
) -> Result<CommandReceipt, ModelRetryUsageError> {
    let actor = ReceiptActorKey::from_encoded(b"winwincode.model-retry-usage.actor.v1".to_vec())?;
    let scope_payload =
        serde_json::to_vec(&request.attribution).map_err(|_| ModelRetryUsageError::invalid())?;
    let scope = ReceiptScopeKey::from_encoded(
        Sha256::digest(
            [
                b"winwincode.model-retry-usage.scope.v1\0".as_slice(),
                &scope_payload,
            ]
            .concat(),
        )
        .to_vec(),
    )?;
    let payload = serde_json::to_vec(&(&request.request_id, request.plan.fingerprint(), command))
        .map_err(|_| ModelRetryUsageError::invalid())?;
    Ok(CommandReceipt {
        identity: ReceiptIdentity::new(actor, scope, command_request_id.clone())?,
        digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(payload))),
    })
}

fn event_from_receipt(receipt: &CommitReceipt) -> Result<RetryUsageEvent, ModelRetryUsageError> {
    let [event] = receipt.events.as_slice() else {
        return Err(ModelRetryUsageError::corrupt());
    };
    if event.topic != EVENT_TOPIC {
        return Err(ModelRetryUsageError::corrupt());
    }
    let (schema, decoded): (String, RetryUsageEvent) =
        serde_json::from_slice(&event.payload).map_err(|_| ModelRetryUsageError::corrupt())?;
    if schema != EVENT_SCHEMA
        || serde_json::to_vec(&(schema, &decoded)).map_err(|_| ModelRetryUsageError::corrupt())?
            != event.payload
    {
        return Err(ModelRetryUsageError::corrupt());
    }
    validate_retry_event(receipt, &decoded)?;
    Ok(decoded)
}

fn validate_retry_event(
    receipt: &CommitReceipt,
    event: &RetryUsageEvent,
) -> Result<(), ModelRetryUsageError> {
    let (request_id, model_exchange_id, revision, stored_replay) = match event {
        RetryUsageEvent::Started(result) => {
            validate_token(&result.provider_id, 128)
                .map_err(|_| ModelRetryUsageError::corrupt())?;
            validate_token(&result.model_id, 200).map_err(|_| ModelRetryUsageError::corrupt())?;
            validate_sha256(&result.route_fingerprint)?;
            if result.attempt == 0 || result.attempt > MAX_TOTAL_ATTEMPTS {
                return Err(ModelRetryUsageError::corrupt());
            }
            (
                &result.request_id,
                &result.model_exchange_id,
                result.revision,
                result.idempotent_replay,
            )
        }
        RetryUsageEvent::Failed(result) => {
            validate_retry_decision(result)?;
            (
                &result.request_id,
                &result.model_exchange_id,
                result.revision,
                result.idempotent_replay,
            )
        }
        RetryUsageEvent::Settled(result) => {
            normalized_stored_usage(&result.usage)?;
            (
                &result.request_id,
                &result.model_exchange_id,
                result.revision,
                result.idempotent_replay,
            )
        }
    };
    validate_request_id(request_id).map_err(|_| ModelRetryUsageError::corrupt())?;
    validate_prefixed_id(&model_exchange_id.0, "mdl_")
        .map_err(|_| ModelRetryUsageError::corrupt())?;
    if revision != receipt.revision || stored_replay || receipt.stream_id != stream_id(request_id)?
    {
        return Err(ModelRetryUsageError::corrupt());
    }
    Ok(())
}

fn validate_retry_decision(result: &ModelRetryDecisionReceipt) -> Result<(), ModelRetryUsageError> {
    if result.attempt == 0 || result.attempt > MAX_TOTAL_ATTEMPTS {
        return Err(ModelRetryUsageError::corrupt());
    }
    let next_values_present = [
        result.next_attempt.is_some(),
        result.next_provider_id.is_some(),
        result.next_model_id.is_some(),
        result.next_route_fingerprint.is_some(),
    ];
    let expects_next = matches!(
        result.action,
        ModelRetryAction::RetrySameRoute | ModelRetryAction::Fallback
    );
    if next_values_present
        .iter()
        .any(|present| *present != expects_next)
    {
        return Err(ModelRetryUsageError::corrupt());
    }
    if result.next_attempt.is_some_and(|next_attempt| {
        next_attempt != result.attempt + 1 || next_attempt > MAX_TOTAL_ATTEMPTS
    }) {
        return Err(ModelRetryUsageError::corrupt());
    }
    if let Some(provider_id) = &result.next_provider_id {
        validate_token(provider_id, 128).map_err(|_| ModelRetryUsageError::corrupt())?;
    }
    if let Some(model_id) = &result.next_model_id {
        validate_token(model_id, 200).map_err(|_| ModelRetryUsageError::corrupt())?;
    }
    if let Some(fingerprint) = &result.next_route_fingerprint {
        validate_sha256(fingerprint)?;
    }
    Ok(())
}

fn start_from_receipt(
    receipt: &CommitReceipt,
    replay: bool,
) -> Result<ModelAttemptStartReceipt, ModelRetryUsageError> {
    match event_from_receipt(receipt)? {
        RetryUsageEvent::Started(mut result) => {
            result.idempotent_replay = replay;
            Ok(result)
        }
        RetryUsageEvent::Failed(_) | RetryUsageEvent::Settled(_) => {
            Err(ModelRetryUsageError::corrupt())
        }
    }
}

fn failure_from_receipt(
    receipt: &CommitReceipt,
    replay: bool,
) -> Result<ModelRetryDecisionReceipt, ModelRetryUsageError> {
    match event_from_receipt(receipt)? {
        RetryUsageEvent::Failed(mut result) => {
            result.idempotent_replay = replay;
            Ok(result)
        }
        RetryUsageEvent::Started(_) | RetryUsageEvent::Settled(_) => {
            Err(ModelRetryUsageError::corrupt())
        }
    }
}

fn settlement_from_receipt(
    receipt: &CommitReceipt,
    replay: bool,
) -> Result<ModelUsageSettlementReceipt, ModelRetryUsageError> {
    match event_from_receipt(receipt)? {
        RetryUsageEvent::Settled(mut result) => {
            result.idempotent_replay = replay;
            Ok(result)
        }
        RetryUsageEvent::Started(_) | RetryUsageEvent::Failed(_) => {
            Err(ModelRetryUsageError::corrupt())
        }
    }
}

fn recover_exact_completion(
    storage: &dyn ProductStateStorage,
    command: &CommandReceipt,
    source: ModelRetryUsageError,
) -> Result<ModelUsageSettlementReceipt, ModelRetryUsageError> {
    match storage.load_receipt(&command.identity, &command.digest)? {
        Some(replay) => settlement_from_receipt(&replay, true),
        None => Err(source),
    }
}

fn validate_filter(filter: &ModelUsageFilter) -> Result<(), ModelRetryUsageError> {
    if let Some(id) = &filter.organization_id {
        validate_prefixed_id(&id.0, "org_")?;
    }
    if let Some(id) = &filter.workspace_id {
        validate_prefixed_id(&id.0, "wsp_")?;
    }
    if let Some(id) = &filter.project_id {
        validate_prefixed_id(&id.0, "prj_")?;
    }
    if let Some(id) = &filter.repository_id {
        validate_prefixed_id(&id.0, "rep_")?;
    }
    if let Some(id) = &filter.product_session_id {
        validate_prefixed_id(&id.0, "psn_")?;
    }
    if let Some(id) = &filter.delivery_id {
        validate_prefixed_id(&id.0, "dlv_")?;
    }
    if let Some(id) = &filter.user_id {
        validate_prefixed_id(&id.0, "usr_")?;
    }
    if let Some(provider) = &filter.provider_id {
        validate_token(provider, 128)?;
    }
    Ok(())
}

fn attribution_matches(attribution: &ModelUsageAttribution, filter: &ModelUsageFilter) -> bool {
    filter
        .organization_id
        .as_ref()
        .is_none_or(|id| id == &attribution.organization_id)
        && filter
            .workspace_id
            .as_ref()
            .is_none_or(|id| id == &attribution.workspace_id)
        && filter
            .project_id
            .as_ref()
            .is_none_or(|id| id == &attribution.project_id)
        && filter
            .repository_id
            .as_ref()
            .is_none_or(|id| id == &attribution.repository_id)
        && filter
            .product_session_id
            .as_ref()
            .is_none_or(|id| id == &attribution.product_session_id)
        && filter
            .delivery_id
            .as_ref()
            .is_none_or(|id| Some(id) == attribution.delivery_id.as_ref())
        && filter
            .user_id
            .as_ref()
            .is_none_or(|id| id == &attribution.user_id)
}

fn add_usage(
    totals: &mut ModelUsageTotals,
    usage: &SettledModelUsage,
) -> Result<(), ModelRetryUsageError> {
    totals.entries = checked_add(totals.entries, 1)?;
    totals.input_tokens = checked_add(totals.input_tokens, usage.input_tokens)?;
    totals.output_tokens = checked_add(totals.output_tokens, usage.output_tokens)?;
    totals.total_tokens = checked_add(totals.total_tokens, usage.total_tokens)?;
    totals.cost_micros = checked_add(totals.cost_micros, usage.cost_micros)?;
    Ok(())
}

fn stream_id(request_id: &RequestId) -> Result<String, ModelRetryUsageError> {
    validate_request_id(request_id)?;
    Ok(format!(
        "{STREAM_PREFIX}{:x}",
        Sha256::digest(request_id.0.as_bytes())
    ))
}

pub(crate) fn retry_context_stream(
    model_exchange_id: &ModelExchangeId,
) -> Result<String, ModelRetryUsageError> {
    validate_prefixed_id(&model_exchange_id.0, "mdl_")?;
    Ok(format!(
        "{RETRY_CONTEXT_PREFIX}{:x}",
        Sha256::digest(model_exchange_id.0.as_bytes())
    ))
}

fn retry_context_mutation(
    request: &ModelRetryUsageRequest,
    receipt: &ModelAttemptStartReceipt,
) -> Result<StateMutation, ModelRetryUsageError> {
    let context = ModelRetrySettlementContext::try_new(request.clone(), receipt.clone())
        .map_err(|_| ModelRetryUsageError::corrupt())?;
    let payload = context
        .encode_json()
        .map_err(|_| ModelRetryUsageError::corrupt())?;
    StateMutation::new(
        retry_context_stream(&receipt.model_exchange_id)?,
        0,
        payload,
    )
    .map_err(Into::into)
}

fn verify_started_context(
    storage: &dyn ProductStateStorage,
    request: &ModelRetryUsageRequest,
    receipt: &ModelAttemptStartReceipt,
) -> Result<(), ModelRetryUsageError> {
    let stored = storage
        .load_state(&retry_context_stream(&receipt.model_exchange_id)?)?
        .ok_or_else(ModelRetryUsageError::corrupt)?;
    if stored.revision != 1 {
        return Err(ModelRetryUsageError::corrupt());
    }
    let context = ModelRetrySettlementContext::decode_json(&stored.payload)
        .map_err(|_| ModelRetryUsageError::corrupt())?;
    let mut original_receipt = receipt.clone();
    original_receipt.idempotent_replay = false;
    let expected = ModelRetrySettlementContext::try_new(request.clone(), original_receipt)
        .map_err(|_| ModelRetryUsageError::corrupt())?;
    if context.request_fingerprint() != expected.request_fingerprint()
        || context.context_fingerprint() != expected.context_fingerprint()
        || context.start_receipt() != expected.start_receipt()
    {
        return Err(ModelRetryUsageError::corrupt());
    }
    Ok(())
}

fn usage_identity_stream(provider_usage_id: &str) -> Result<String, ModelRetryUsageError> {
    validate_token(provider_usage_id, 200)?;
    Ok(format!(
        "{USAGE_ID_PREFIX}{:x}",
        Sha256::digest(provider_usage_id.as_bytes())
    ))
}

fn usage_entry_stream(sequence: u64) -> Result<String, ModelRetryUsageError> {
    if sequence == 0 || sequence > MAX_SAFE_INTEGER {
        return Err(ModelRetryUsageError::corrupt());
    }
    Ok(format!("{USAGE_ENTRY_PREFIX}{sequence:016x}"))
}

fn next_revision(revision: u64) -> Result<u64, ModelRetryUsageError> {
    revision
        .checked_add(1)
        .filter(|next| *next <= MAX_SAFE_INTEGER)
        .ok_or_else(ModelRetryUsageError::corrupt)
}

fn checked_add(left: u64, right: u64) -> Result<u64, ModelRetryUsageError> {
    left.checked_add(right)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(ModelRetryUsageError::corrupt)
}

fn validate_request_id(request_id: &RequestId) -> Result<(), ModelRetryUsageError> {
    validate_prefixed_id(&request_id.0, "req_")
}

fn validate_attribution(attribution: &ModelUsageAttribution) -> Result<(), ModelRetryUsageError> {
    validate_prefixed_id(&attribution.organization_id.0, "org_")?;
    validate_prefixed_id(&attribution.workspace_id.0, "wsp_")?;
    validate_prefixed_id(&attribution.project_id.0, "prj_")?;
    validate_prefixed_id(&attribution.repository_id.0, "rep_")?;
    validate_prefixed_id(&attribution.product_session_id.0, "psn_")?;
    if let Some(delivery_id) = &attribution.delivery_id {
        validate_prefixed_id(&delivery_id.0, "dlv_")?;
    }
    validate_prefixed_id(&attribution.user_id.0, "usr_")
}

fn validate_instant(value: &Instant) -> Result<(), ModelRetryUsageError> {
    let bytes = value.0.as_bytes();
    let valid = bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        });
    if valid {
        Ok(())
    } else {
        Err(ModelRetryUsageError::invalid())
    }
}

fn validate_prefixed_id(value: &str, prefix: &str) -> Result<(), ModelRetryUsageError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(ModelRetryUsageError::invalid());
    };
    if suffix.len() == 26
        && suffix.bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(
                    byte,
                    b'A' | b'B'
                        | b'C'
                        | b'D'
                        | b'E'
                        | b'F'
                        | b'G'
                        | b'H'
                        | b'J'
                        | b'K'
                        | b'M'
                        | b'N'
                        | b'P'
                        | b'Q'
                        | b'R'
                        | b'S'
                        | b'T'
                        | b'V'
                        | b'W'
                        | b'X'
                        | b'Y'
                        | b'Z'
                )
        })
    {
        Ok(())
    } else {
        Err(ModelRetryUsageError::invalid())
    }
}

fn validate_token(value: &str, max_len: usize) -> Result<(), ModelRetryUsageError> {
    if value.is_empty()
        || value.len() > max_len
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ModelRetryUsageError::invalid());
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), ModelRetryUsageError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(ModelRetryUsageError::corrupt());
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ModelRetryUsageError::corrupt());
    }
    Ok(())
}

fn storage_retry_exhausted() -> ModelRetryUsageError {
    ModelRetryUsageError::new(
        ModelRetryUsageErrorKind::Storage,
        "model retry storage concurrency limit was exhausted",
    )
}
