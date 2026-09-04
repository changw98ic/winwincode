// SPDX-License-Identifier: Apache-2.0

//! Pre-open retry planning from durable job and admission authority.

use std::{fmt, path::Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind,
};
use winwincode_domain::{
    DeliveryId, ExecutionJobId, ModelExchangeId, ProductSessionId, RequestId, Sha256Digest,
};
use winwincode_execution_port::generated::{ExecutionScope, ModelOpenMessage};
use winwincode_storage::{
    NewOutboxEvent, ProductStateStorage, PublicEventActor, PublicEventScope, ReceiptActorKey,
    ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit, public_actor_from_receipt_key,
    repository_scope_from_receipt_key,
};

use crate::{
    FrozenModelRetryPlan, FrozenModelRouteAuthority, ModelAttemptStartCommand,
    ModelRetrySettlementContext, ModelRetryStep, ModelRetryUsageError, ModelRetryUsageRequest,
    ModelRetryUsageService, ModelSettingsTarget, ModelUsageAttribution,
    ProviderAdmissionOpenReceipt, ProviderGatewayIdentity,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_TOTAL_ATTEMPTS: u64 = 16;
const FAILURE_SCHEMA: &str = "winwincode.model-retry-pre-open-failure.v1";
const FAILURE_STREAM_PREFIX: &str = "model-retry-pre-open-failure:";

/// A trusted policy source which freezes the complete retry/fallback plan.
pub trait ModelRetryPlanAuthorityPort: Send + Sync {
    /// Freezes the plan whose first step is the already-admitted primary route.
    ///
    /// # Errors
    ///
    /// Rejects unavailable policy or a plan that does not begin with `primary`.
    fn freeze_plan(
        &self,
        primary: FrozenModelRouteAuthority,
    ) -> Result<FrozenModelRetryPlan, ModelRetryPlannerError>;
}

/// Explicit deployment policy for one admitted route and a hard attempt cap.
///
/// This is a real policy value, not an implicit fallback. Deployments that
/// support alternate routes provide another [`ModelRetryPlanAuthorityPort`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfiguredModelRetryPlanAuthority {
    policy_id: String,
    policy_revision: u64,
    max_attempts: u64,
}

impl ConfiguredModelRetryPlanAuthority {
    /// Creates an explicit immutable single-route retry policy.
    ///
    /// # Errors
    ///
    /// Rejects malformed policy identity, revision, or attempt limits.
    pub fn try_new(
        policy_id: String,
        policy_revision: u64,
        max_attempts: u64,
    ) -> Result<Self, ModelRetryPlannerError> {
        if policy_id.is_empty()
            || policy_id.len() > 200
            || policy_id.trim() != policy_id
            || policy_id.chars().any(char::is_control)
            || policy_revision == 0
            || policy_revision > MAX_SAFE_INTEGER
            || max_attempts == 0
            || max_attempts > MAX_TOTAL_ATTEMPTS
        {
            return Err(ModelRetryPlannerError::invalid());
        }
        Ok(Self {
            policy_id,
            policy_revision,
            max_attempts,
        })
    }
}

impl ModelRetryPlanAuthorityPort for ConfiguredModelRetryPlanAuthority {
    fn freeze_plan(
        &self,
        primary: FrozenModelRouteAuthority,
    ) -> Result<FrozenModelRetryPlan, ModelRetryPlannerError> {
        let step = ModelRetryStep::try_new(primary, self.max_attempts)
            .map_err(|_| ModelRetryPlannerError::new(ModelRetryPlannerErrorKind::Policy))?;
        FrozenModelRetryPlan::freeze(self.policy_id.clone(), self.policy_revision, vec![step])
            .map_err(|_| ModelRetryPlannerError::new(ModelRetryPlannerErrorKind::Policy))
    }
}

/// Stable pre-open planning failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRetryPlannerErrorKind {
    InvalidRequest,
    IdentityMismatch,
    Policy,
    Ledger,
    Storage,
}

/// Exact durable admission identity that composition is permitted to release.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRetryAdmissionReleaseAuthority {
    model_exchange_id: ModelExchangeId,
    request_id: RequestId,
    route_authority_fingerprint: String,
    reservation_digest: String,
}

impl ModelRetryAdmissionReleaseAuthority {
    fn from_admission(
        admission: &ProviderAdmissionOpenReceipt,
    ) -> Result<Self, ModelRetryPlannerError> {
        let reservation_digest = normalized_reservation_digest(admission)?;
        Ok(Self {
            model_exchange_id: admission.reservation.model_exchange_id.clone(),
            request_id: admission.reservation.request_id.clone(),
            route_authority_fingerprint: admission.route_authority.fingerprint().to_owned(),
            reservation_digest,
        })
    }

    /// Checks that a release still names the exact admitted receipt and route.
    #[must_use]
    pub fn authorizes(&self, admission: &ProviderAdmissionOpenReceipt) -> bool {
        admission.reservation.model_exchange_id == self.model_exchange_id
            && admission.reservation.request_id == self.request_id
            && admission.route_authority.fingerprint() == self.route_authority_fingerprint
            && normalized_reservation_digest(admission)
                .is_ok_and(|digest| digest == self.reservation_digest)
    }

    #[must_use]
    pub const fn model_exchange_id(&self) -> &ModelExchangeId {
        &self.model_exchange_id
    }

    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        &self.request_id
    }
}

fn normalized_reservation_digest(
    admission: &ProviderAdmissionOpenReceipt,
) -> Result<String, ModelRetryPlannerError> {
    let mut reservation = admission.reservation.clone();
    reservation.idempotent_replay = false;
    let payload =
        serde_json::to_vec(&reservation).map_err(|_| ModelRetryPlannerError::invalid())?;
    Ok(format!("sha256:{:x}", Sha256::digest(payload)))
}

/// Bounded failure that retains no model input, credential, or Provider text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRetryPlannerError {
    kind: ModelRetryPlannerErrorKind,
    release_authority: Option<ModelRetryAdmissionReleaseAuthority>,
}

impl ModelRetryPlannerError {
    const fn new(kind: ModelRetryPlannerErrorKind) -> Self {
        Self {
            kind,
            release_authority: None,
        }
    }

    fn with_release_authority(
        mut self,
        release_authority: &ModelRetryAdmissionReleaseAuthority,
    ) -> Self {
        self.release_authority = Some(release_authority.clone());
        self
    }

    const fn invalid() -> Self {
        Self::new(ModelRetryPlannerErrorKind::InvalidRequest)
    }

    const fn identity() -> Self {
        Self::new(ModelRetryPlannerErrorKind::IdentityMismatch)
    }

    fn ledger(_error: ModelRetryUsageError) -> Self {
        Self::new(ModelRetryPlannerErrorKind::Ledger)
    }

    #[must_use]
    pub const fn kind(&self) -> ModelRetryPlannerErrorKind {
        self.kind
    }

    /// Exact authority for an idempotent admission release, when one is safe.
    #[must_use]
    pub const fn release_authority(&self) -> Option<&ModelRetryAdmissionReleaseAuthority> {
        self.release_authority.as_ref()
    }
}

impl fmt::Display for ModelRetryPlannerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("model retry pre-open planning failed")
    }
}

impl std::error::Error for ModelRetryPlannerError {}

impl From<winwincode_storage::StorageError> for ModelRetryPlannerError {
    fn from(_error: winwincode_storage::StorageError) -> Self {
        Self::new(ModelRetryPlannerErrorKind::Storage)
    }
}

/// Prepares the canonical context after admission and before Provider access.
pub trait ModelRetryPreOpenPlannerPort: Send {
    /// Commits or replays one exact retry context for an admitted open.
    ///
    /// # Errors
    ///
    /// Rejects foreign job/actor/scope/route authority, changed replay, corrupt
    /// durable state, invalid policy, or unavailable storage.
    fn prepare(
        &mut self,
        message: &ModelOpenMessage,
        admission: &ProviderAdmissionOpenReceipt,
    ) -> Result<ModelRetrySettlementContext, ModelRetryPlannerError>;
}

/// Production planner over the same canonical Control Plane database.
pub struct DurableModelRetryPreOpenPlanner<'policy> {
    storage: SqliteStorage,
    policy: &'policy dyn ModelRetryPlanAuthorityPort,
}

impl<'policy> DurableModelRetryPreOpenPlanner<'policy> {
    /// Opens the canonical product database used by job and retry authority.
    ///
    /// # Errors
    ///
    /// Returns `Storage` when the database cannot be opened.
    pub fn open(
        data_directory: impl AsRef<Path>,
        policy: &'policy dyn ModelRetryPlanAuthorityPort,
    ) -> Result<Self, ModelRetryPlannerError> {
        Ok(Self {
            storage: SqliteStorage::open(data_directory)?,
            policy,
        })
    }

    /// Returns the exact database path for composition-root equality checks.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        self.storage.database_path()
    }

    /// Deterministically checkpoints and closes the owned connection.
    ///
    /// # Errors
    ///
    /// Returns `Storage` when close or checkpoint fails.
    pub fn close(self) -> Result<(), ModelRetryPlannerError> {
        Box::new(self.storage).close().map_err(Into::into)
    }
}

impl ModelRetryPreOpenPlannerPort for DurableModelRetryPreOpenPlanner<'_> {
    fn prepare(
        &mut self,
        message: &ModelOpenMessage,
        admission: &ProviderAdmissionOpenReceipt,
    ) -> Result<ModelRetrySettlementContext, ModelRetryPlannerError> {
        validate_admission(message, admission)?;
        let release_authority = ModelRetryAdmissionReleaseAuthority::from_admission(admission)?;
        let context_stream =
            crate::model_retry_usage::retry_context_stream(&message.model_exchange_id)
                .map_err(ModelRetryPlannerError::ledger)?;
        let context_existed = self.storage.load_state(&context_stream)?.is_some();
        let authority = execution_job_authority(&self.storage, message)
            .map_err(|error| deterministic_release(error, context_existed, &release_authority))?;
        validate_job_target(&authority, admission.route_authority.target())
            .map_err(|error| deterministic_release(error, context_existed, &release_authority))?;
        let plan = self
            .policy
            .freeze_plan(admission.route_authority.clone())
            .map_err(|error| deterministic_release(error, context_existed, &release_authority))?;
        let Some(primary) = plan.steps().first() else {
            return Err(deterministic_release(
                ModelRetryPlannerError::new(ModelRetryPlannerErrorKind::Policy),
                context_existed,
                &release_authority,
            ));
        };
        if primary.authority() != &admission.route_authority {
            return Err(deterministic_release(
                ModelRetryPlannerError::new(ModelRetryPlannerErrorKind::Policy),
                context_existed,
                &release_authority,
            ));
        }
        let request = ModelRetryUsageRequest {
            request_id: message.request_id.clone(),
            attribution: ModelUsageAttribution::from_request_authority(
                &admission.route_authority,
                authority.delivery_id,
                &authority.actor,
            )
            .map_err(ModelRetryPlannerError::ledger)?,
            plan,
            enterprise_quota_amounts: admission.enterprise_quota_amounts,
            enterprise_quota_requested_at: message.sent_at.clone(),
        };
        let failure = PreOpenFailure::new(message, admission, &request)?;
        if load_failure(&self.storage, &failure)?.is_some() {
            return Err(
                ModelRetryPlannerError::new(ModelRetryPlannerErrorKind::Ledger)
                    .with_release_authority(&release_authority),
            );
        }
        let mut reservation = admission.reservation.clone();
        reservation.idempotent_replay = false;
        let start = match ModelRetryUsageService::new(&mut self.storage).start_attempt(
            &request,
            &ModelAttemptStartCommand {
                command_request_id: message.request_id.clone(),
                admission: reservation,
            },
        ) {
            Ok(start) => start,
            Err(error) if !context_existed => {
                if self.storage.load_state(&context_stream)?.is_some() {
                    return Err(ModelRetryPlannerError::ledger(error));
                }
                persist_failure(&mut self.storage, &failure)?;
                return Err(
                    ModelRetryPlannerError::new(ModelRetryPlannerErrorKind::Ledger)
                        .with_release_authority(&release_authority),
                );
            }
            Err(error) => return Err(ModelRetryPlannerError::ledger(error)),
        };
        let mut original_start = start;
        original_start.idempotent_replay = false;
        ModelRetrySettlementContext::try_new(request, original_start)
            .map_err(|_| ModelRetryPlannerError::new(ModelRetryPlannerErrorKind::Ledger))
    }
}

fn deterministic_release(
    error: ModelRetryPlannerError,
    context_existed: bool,
    release_authority: &ModelRetryAdmissionReleaseAuthority,
) -> ModelRetryPlannerError {
    if !context_existed
        && matches!(
            error.kind(),
            ModelRetryPlannerErrorKind::IdentityMismatch | ModelRetryPlannerErrorKind::Policy
        )
    {
        error.with_release_authority(release_authority)
    } else {
        error
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PreOpenFailure {
    schema: String,
    request_id: RequestId,
    model_exchange_id: ModelExchangeId,
    execution_job_id: ExecutionJobId,
    route_authority_fingerprint: String,
    plan_fingerprint: String,
    attribution_digest: String,
    admission_digest: String,
}

impl PreOpenFailure {
    fn new(
        message: &ModelOpenMessage,
        admission: &ProviderAdmissionOpenReceipt,
        request: &ModelRetryUsageRequest,
    ) -> Result<Self, ModelRetryPlannerError> {
        let attribution = serde_json::to_vec(&request.attribution)
            .map_err(|_| ModelRetryPlannerError::invalid())?;
        let mut reservation = admission.reservation.clone();
        reservation.idempotent_replay = false;
        let admission_bytes =
            serde_json::to_vec(&reservation).map_err(|_| ModelRetryPlannerError::invalid())?;
        Ok(Self {
            schema: FAILURE_SCHEMA.to_owned(),
            request_id: message.request_id.clone(),
            model_exchange_id: message.model_exchange_id.clone(),
            execution_job_id: message.lease.job_id.clone(),
            route_authority_fingerprint: admission.route_authority.fingerprint().to_owned(),
            plan_fingerprint: request.plan.fingerprint().to_owned(),
            attribution_digest: format!("sha256:{:x}", Sha256::digest(attribution)),
            admission_digest: format!("sha256:{:x}", Sha256::digest(admission_bytes)),
        })
    }
}

fn failure_stream(model_exchange_id: &ModelExchangeId) -> String {
    format!(
        "{FAILURE_STREAM_PREFIX}{:x}",
        Sha256::digest(model_exchange_id.0.as_bytes())
    )
}

fn load_failure(
    storage: &dyn ProductStateStorage,
    expected: &PreOpenFailure,
) -> Result<Option<PreOpenFailure>, ModelRetryPlannerError> {
    let stream_id = failure_stream(&expected.model_exchange_id);
    let Some(stored) = storage.load_state(&stream_id)? else {
        return Ok(None);
    };
    let failure: PreOpenFailure = serde_json::from_slice(&stored.payload)
        .map_err(|_| ModelRetryPlannerError::new(ModelRetryPlannerErrorKind::Storage))?;
    if stored.revision != 1
        || failure != *expected
        || failure.schema != FAILURE_SCHEMA
        || serde_json::to_vec(&failure)
            .map_err(|_| ModelRetryPlannerError::new(ModelRetryPlannerErrorKind::Storage))?
            != stored.payload
    {
        return Err(ModelRetryPlannerError::identity());
    }
    let (identity, digest) = failure_receipt(&failure, &stored.payload)?;
    let receipt = storage
        .load_receipt(&identity, &digest)?
        .ok_or_else(|| ModelRetryPlannerError::new(ModelRetryPlannerErrorKind::Storage))?;
    let event = receipt
        .events
        .first()
        .ok_or_else(|| ModelRetryPlannerError::new(ModelRetryPlannerErrorKind::Storage))?;
    if receipt.stream_id != stream_id
        || receipt.revision != 1
        || receipt.events.len() != 1
        || event.event_id != stream_id
        || event.topic != "model.retry-pre-open.failed"
        || event.payload != stored.payload
    {
        return Err(ModelRetryPlannerError::new(
            ModelRetryPlannerErrorKind::Storage,
        ));
    }
    Ok(Some(failure))
}

fn persist_failure(
    storage: &mut dyn ProductStateStorage,
    failure: &PreOpenFailure,
) -> Result<(), ModelRetryPlannerError> {
    if load_failure(storage, failure)?.is_some() {
        return Ok(());
    }
    let payload = serde_json::to_vec(failure).map_err(|_| ModelRetryPlannerError::invalid())?;
    let (identity, digest) = failure_receipt(failure, &payload)?;
    let stream_id = failure_stream(&failure.model_exchange_id);
    storage.commit(&StateCommit::new(
        identity,
        digest,
        stream_id.clone(),
        0,
        payload.clone(),
        vec![NewOutboxEvent::internal(
            stream_id,
            "model.retry-pre-open.failed",
            payload,
        )],
    ))?;
    Ok(())
}

fn failure_receipt(
    failure: &PreOpenFailure,
    payload: &[u8],
) -> Result<(ReceiptIdentity, Sha256Digest), ModelRetryPlannerError> {
    let actor = ReceiptActorKey::from_encoded(
        b"winwincode.model-retry-pre-open-failure.actor.v1".to_vec(),
    )?;
    let scope = ReceiptScopeKey::from_encoded(
        Sha256::digest(
            [
                b"winwincode.model-retry-pre-open-failure.scope.v1\0".as_slice(),
                payload,
            ]
            .concat(),
        )
        .to_vec(),
    )?;
    let identity = ReceiptIdentity::new(actor, scope, failure.request_id.clone())?;
    Ok((
        identity,
        Sha256Digest(format!("sha256:{:x}", Sha256::digest(payload))),
    ))
}

struct ExecutionJobAuthority {
    actor: Actor,
    repository_scope: RepositoryScope,
    product_session_id: ProductSessionId,
    delivery_id: Option<DeliveryId>,
}

fn execution_job_authority(
    storage: &dyn ProductStateStorage,
    message: &ModelOpenMessage,
) -> Result<ExecutionJobAuthority, ModelRetryPlannerError> {
    let (durable, job) =
        crate::delivery_transaction::load_durable_execution_job(storage, &message.lease.job_id)?;
    let actor = match public_actor_from_receipt_key(durable.receipt_identity().actor_key())? {
        PublicEventActor::User { id } => Actor::UserActor(UserActor {
            id,
            kind: UserActorKind::User,
        }),
        PublicEventActor::ServiceAccount { .. } | PublicEventActor::System { .. } => {
            return Err(ModelRetryPlannerError::identity());
        }
    };
    let repository_scope =
        match repository_scope_from_receipt_key(durable.receipt_identity().scope_key())? {
            PublicEventScope::Repository {
                organization_id,
                workspace_id,
                project_id,
                repository_id,
            } => RepositoryScope {
                kind: RepositoryScopeKind::Repository,
                organization_id,
                workspace_id,
                project_id,
                repository_id,
            },
            PublicEventScope::Organization { .. }
            | PublicEventScope::Workspace { .. }
            | PublicEventScope::Project { .. } => return Err(ModelRetryPlannerError::identity()),
        };
    if job.job_id != message.lease.job_id
        || job.attempt != message.lease.attempt
        || job.workspace.repository_id != repository_scope.repository_id
        || message.worker_session_id != message.session_identity.worker_session_id
    {
        return Err(ModelRetryPlannerError::identity());
    }
    let (product_session_id, delivery_id) = match &job.scope {
        ExecutionScope::ProductSessionExecutionScope(scope) => {
            if message.session_identity.stage_run_id.is_some() {
                return Err(ModelRetryPlannerError::identity());
            }
            (scope.product_session_id.clone(), None)
        }
        ExecutionScope::DeliveryStageExecutionScope(scope) => {
            if message.session_identity.stage_run_id.as_ref() != Some(&scope.stage_run_id) {
                return Err(ModelRetryPlannerError::identity());
            }
            (
                scope.product_session_id.clone(),
                Some(scope.delivery_id.clone()),
            )
        }
    };
    if message.session_identity.product_session_id != product_session_id {
        return Err(ModelRetryPlannerError::identity());
    }
    Ok(ExecutionJobAuthority {
        actor,
        repository_scope,
        product_session_id,
        delivery_id,
    })
}

pub(crate) fn provider_gateway_identity(
    storage: &dyn ProductStateStorage,
    message: &ModelOpenMessage,
) -> Result<ProviderGatewayIdentity, ModelRetryPlannerError> {
    let authority = match execution_job_authority(storage, message) {
        Ok(authority) => authority,
        Err(error) => {
            if std::env::var_os("WWC_DEBUG_RUNTIME").is_some() {
                eprintln!("provider identity planner error={error:?}");
            }
            return Err(error);
        }
    };
    Ok(match authority.actor {
        Actor::UserActor(actor) => ProviderGatewayIdentity::product_session_for_user(
            authority.repository_scope,
            authority.product_session_id,
            actor.id,
        ),
        Actor::SystemActor(_) | Actor::ServiceAccountActor(_) => {
            ProviderGatewayIdentity::product_session(
                authority.repository_scope,
                authority.product_session_id,
            )
        }
    })
}

fn validate_job_target(
    job: &ExecutionJobAuthority,
    target: &ModelSettingsTarget,
) -> Result<(), ModelRetryPlannerError> {
    let ModelSettingsTarget::ProductSession {
        repository_scope,
        product_session_id,
    } = target
    else {
        return Err(ModelRetryPlannerError::identity());
    };
    if repository_scope != &job.repository_scope || product_session_id != &job.product_session_id {
        return Err(ModelRetryPlannerError::identity());
    }
    Ok(())
}

fn validate_admission(
    message: &ModelOpenMessage,
    admission: &ProviderAdmissionOpenReceipt,
) -> Result<(), ModelRetryPlannerError> {
    admission
        .route_authority
        .validate_fingerprint()
        .map_err(|_| ModelRetryPlannerError::identity())?;
    if !admission.reservation.admitted()
        || admission.reservation.request_id != message.request_id
        || admission.reservation.model_exchange_id != message.model_exchange_id
        || admission.reservation.route_authority_fingerprint
            != admission.route_authority.fingerprint()
        || message.sent_at.0 < message.lease.issued_at.0
        || message.sent_at.0 >= message.lease.expires_at.0
    {
        return Err(ModelRetryPlannerError::identity());
    }
    Ok(())
}
