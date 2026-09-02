// SPDX-License-Identifier: Apache-2.0

//! Durable bridge from verified Provider terminal facts into the canonical
//! retry and Usage ledger.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{ModelExchangeId, RequestId};
use winwincode_storage::{
    EnterpriseQuotaReservationRequest, EnterpriseQuotaSourceSeal, EnterpriseUsageAttribution,
    ProductStateStorage, SqliteStorage,
};

use crate::{
    FrozenModelRetryPlan, FrozenModelRouteAuthority, ModelAttemptCompletionCommand,
    ModelAttemptFailureCommand, ModelAttemptStartReceipt, ModelRetryDecisionReceipt,
    ModelRetryStep, ModelRetryUsageError, ModelRetryUsageRequest, ModelRetryUsageService,
    ModelUsageAttribution, ModelUsageSettlementReceipt, ProviderGatewaySettlement,
    ProviderGatewaySettlementError, ProviderGatewaySettlementPort, ProviderGatewayTerminalOutcome,
};

const CONTEXT_SCHEMA: &str = "winwincode.model-retry-settlement-context.v2";

/// Stable context dependency failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRetrySettlementContextErrorKind {
    Corrupt,
    Unavailable,
}

/// Bounded error returned by the durable frozen-context authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRetrySettlementContextError {
    kind: ModelRetrySettlementContextErrorKind,
}

impl ModelRetrySettlementContextError {
    #[must_use]
    pub const fn corrupt() -> Self {
        Self {
            kind: ModelRetrySettlementContextErrorKind::Corrupt,
        }
    }

    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            kind: ModelRetrySettlementContextErrorKind::Unavailable,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ModelRetrySettlementContextErrorKind {
        self.kind
    }
}

impl fmt::Display for ModelRetrySettlementContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("model retry settlement context is unavailable")
    }
}

impl std::error::Error for ModelRetrySettlementContextError {}

/// Reads the exact frozen retry context committed with a Provider exchange.
pub trait ModelRetrySettlementContextPort: Send + Sync {
    /// Loads the context for one exact exchange. Absence is a fail-closed fact.
    ///
    /// # Errors
    ///
    /// Returns a stable error for corrupt state or dependency unavailability.
    fn load_context(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ModelRetrySettlementContext>, ModelRetrySettlementContextError>;
}

/// Production reader for contexts atomically committed by
/// [`crate::ModelRetryUsageService::start_attempt`].
pub struct DurableModelRetryContextSource {
    storage: Mutex<SqliteStorage>,
    database_path: PathBuf,
}

impl DurableModelRetryContextSource {
    /// Opens the retry-context index in the product database.
    ///
    /// # Errors
    ///
    /// Returns `Unavailable` when the database cannot be opened.
    pub fn open(
        data_directory: impl AsRef<Path>,
    ) -> Result<Self, ModelRetrySettlementContextError> {
        let storage = SqliteStorage::open(data_directory)
            .map_err(|_| ModelRetrySettlementContextError::unavailable())?;
        let database_path = storage.database_path().to_path_buf();
        Ok(Self {
            storage: Mutex::new(storage),
            database_path,
        })
    }

    /// Returns the exact canonical database path for composition checks.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Deterministically closes the owned database connection.
    ///
    /// # Errors
    ///
    /// Returns `Unavailable` for a poisoned mutex or close failure.
    pub fn close(self) -> Result<(), ModelRetrySettlementContextError> {
        let storage = self
            .storage
            .into_inner()
            .map_err(|_| ModelRetrySettlementContextError::unavailable())?;
        Box::new(storage)
            .close()
            .map_err(|_| ModelRetrySettlementContextError::unavailable())
    }
}

impl fmt::Debug for DurableModelRetryContextSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableModelRetryContextSource")
            .field("database_path", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl ModelRetrySettlementContextPort for DurableModelRetryContextSource {
    fn load_context(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ModelRetrySettlementContext>, ModelRetrySettlementContextError> {
        let stream_id = crate::model_retry_usage::retry_context_stream(model_exchange_id)
            .map_err(|_| ModelRetrySettlementContextError::corrupt())?;
        let storage = self
            .storage
            .lock()
            .map_err(|_| ModelRetrySettlementContextError::unavailable())?;
        let stored = storage
            .load_state(&stream_id)
            .map_err(|_| ModelRetrySettlementContextError::unavailable())?;
        let Some(stored) = stored else {
            return Ok(None);
        };
        if stored.revision != 1 {
            return Err(ModelRetrySettlementContextError::corrupt());
        }
        ModelRetrySettlementContext::decode_json(&stored.payload)
            .map(Some)
            .map_err(|_| ModelRetrySettlementContextError::corrupt())
    }
}

/// Canonical secret-free retry context frozen before Provider invocation.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRetrySettlementContext {
    request: ModelRetryUsageRequest,
    start_receipt: ModelAttemptStartReceipt,
    enterprise_quota_request: EnterpriseQuotaReservationRequest,
    request_fingerprint: String,
    context_fingerprint: String,
}

impl ModelRetrySettlementContext {
    /// Validates and freezes the request and exact started attempt.
    ///
    /// # Errors
    ///
    /// Rejects changed plan, attribution, attempt, route, or request identity.
    pub fn try_new(
        request: ModelRetryUsageRequest,
        start_receipt: ModelAttemptStartReceipt,
    ) -> Result<Self, ModelRetrySettlementError> {
        crate::model_retry_usage::validate_request(&request)
            .map_err(ModelRetrySettlementError::ledger)?;
        validate_start(&request, &start_receipt)?;
        let enterprise_quota_request =
            Self::build_enterprise_quota_request(&request, &start_receipt)?;
        let request_fingerprint = request_fingerprint(&request, &enterprise_quota_request)?;
        let context_fingerprint = context_fingerprint(&request_fingerprint, &start_receipt)?;
        Ok(Self {
            request,
            start_receipt,
            enterprise_quota_request,
            request_fingerprint,
            context_fingerprint,
        })
    }

    /// Serializes the only canonical durable context representation.
    ///
    /// # Errors
    ///
    /// Returns `CorruptContext` if canonical serialization fails.
    pub fn encode_json(&self) -> Result<Vec<u8>, ModelRetrySettlementError> {
        serde_json::to_vec(&StoredSettlementContext::from_context(self)?)
            .map_err(|_| ModelRetrySettlementError::corrupt_context())
    }

    /// Rehydrates and fully revalidates canonical durable context bytes.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical JSON or any changed authority, plan, attribution,
    /// attempt, route, or request fingerprint.
    pub fn decode_json(bytes: &[u8]) -> Result<Self, ModelRetrySettlementError> {
        let stored: StoredSettlementContext = serde_json::from_slice(bytes)
            .map_err(|_| ModelRetrySettlementError::corrupt_context())?;
        if serde_json::to_vec(&stored).map_err(|_| ModelRetrySettlementError::corrupt_context())?
            != bytes
            || stored.schema != CONTEXT_SCHEMA
        {
            return Err(ModelRetrySettlementError::corrupt_context());
        }
        let mut steps = Vec::with_capacity(stored.steps.len());
        for step in stored.steps {
            let authority =
                FrozenModelRouteAuthority::from_durable_json(step.authority_json.as_bytes())
                    .map_err(|_| ModelRetrySettlementError::corrupt_context())?;
            authority
                .validate_fingerprint()
                .map_err(|_| ModelRetrySettlementError::corrupt_context())?;
            steps.push(
                ModelRetryStep::try_new(authority, step.max_attempts)
                    .map_err(ModelRetrySettlementError::ledger)?,
            );
        }
        let plan = FrozenModelRetryPlan::freeze(stored.policy_id, stored.policy_revision, steps)
            .map_err(ModelRetrySettlementError::ledger)?;
        if plan.fingerprint() != stored.plan_fingerprint {
            return Err(ModelRetrySettlementError::corrupt_context());
        }
        let context = Self::try_new(
            ModelRetryUsageRequest {
                request_id: stored.request_id,
                attribution: stored.attribution,
                plan,
                enterprise_quota_amounts: stored.enterprise_quota_request.reserved,
                enterprise_quota_requested_at: stored.enterprise_quota_request.requested_at.clone(),
            },
            stored.start_receipt,
        )?;
        if context.enterprise_quota_request != stored.enterprise_quota_request
            || context.request_fingerprint != stored.request_fingerprint
            || context.context_fingerprint != stored.context_fingerprint
            || context.encode_json()?.as_slice() != bytes
        {
            return Err(ModelRetrySettlementError::corrupt_context());
        }
        Ok(context)
    }

    #[must_use]
    pub const fn request(&self) -> &ModelRetryUsageRequest {
        &self.request
    }

    #[must_use]
    pub const fn start_receipt(&self) -> &ModelAttemptStartReceipt {
        &self.start_receipt
    }

    /// Returns the full immutable enterprise quota request sealed into this context.
    #[must_use]
    pub const fn enterprise_quota_request(&self) -> &EnterpriseQuotaReservationRequest {
        &self.enterprise_quota_request
    }

    fn build_enterprise_quota_request(
        request: &ModelRetryUsageRequest,
        start: &ModelAttemptStartReceipt,
    ) -> Result<EnterpriseQuotaReservationRequest, ModelRetrySettlementError> {
        if request.enterprise_quota_amounts.operations == 0
            || request.enterprise_quota_amounts.tokens == 0
            || request.enterprise_quota_requested_at.0.is_empty()
        {
            return Err(ModelRetrySettlementError::corrupt_context());
        }
        Ok(EnterpriseQuotaReservationRequest {
            reservation_id: start.reservation_request_id.clone(),
            attribution: EnterpriseUsageAttribution {
                organization_id: request.attribution.organization_id.clone(),
                workspace_id: request.attribution.workspace_id.clone(),
                project_id: request.attribution.project_id.clone(),
                repository_id: request.attribution.repository_id.clone(),
                delivery_id: request.attribution.delivery_id.clone(),
                product_session_id: Some(request.attribution.product_session_id.clone()),
                user_id: request.attribution.user_id.clone(),
            },
            source_seal: EnterpriseQuotaSourceSeal::Provider {
                model_exchange_id: start.model_exchange_id.clone(),
                request_id: request.request_id.clone(),
                attempt: start.attempt,
                route_authority_fingerprint: start.route_fingerprint.clone(),
            },
            reserved: request.enterprise_quota_amounts,
            requested_at: request.enterprise_quota_requested_at.clone(),
        })
    }

    #[must_use]
    pub fn request_fingerprint(&self) -> &str {
        &self.request_fingerprint
    }

    /// Returns the digest binding request, plan, attribution, exchange, and attempt.
    #[must_use]
    pub fn context_fingerprint(&self) -> &str {
        &self.context_fingerprint
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredSettlementContext {
    schema: String,
    request_id: RequestId,
    attribution: ModelUsageAttribution,
    policy_id: String,
    policy_revision: u64,
    plan_fingerprint: String,
    request_fingerprint: String,
    context_fingerprint: String,
    steps: Vec<StoredRetryStep>,
    start_receipt: ModelAttemptStartReceipt,
    enterprise_quota_request: EnterpriseQuotaReservationRequest,
}

impl StoredSettlementContext {
    fn from_context(
        context: &ModelRetrySettlementContext,
    ) -> Result<Self, ModelRetrySettlementError> {
        Ok(Self {
            schema: CONTEXT_SCHEMA.to_owned(),
            request_id: context.request.request_id.clone(),
            attribution: context.request.attribution.clone(),
            policy_id: context.request.plan.policy_id().to_owned(),
            policy_revision: context.request.plan.policy_revision(),
            plan_fingerprint: context.request.plan.fingerprint().to_owned(),
            request_fingerprint: context.request_fingerprint.clone(),
            context_fingerprint: context.context_fingerprint.clone(),
            steps: context
                .request
                .plan
                .steps()
                .iter()
                .map(|step| {
                    let authority_json = step
                        .authority()
                        .to_durable_json()
                        .map_err(|_| ModelRetrySettlementError::corrupt_context())?;
                    Ok(StoredRetryStep {
                        authority_json: String::from_utf8(authority_json)
                            .map_err(|_| ModelRetrySettlementError::corrupt_context())?,
                        max_attempts: step.max_attempts(),
                    })
                })
                .collect::<Result<Vec<_>, ModelRetrySettlementError>>()?,
            start_receipt: context.start_receipt.clone(),
            enterprise_quota_request: context.enterprise_quota_request.clone(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRetryStep {
    authority_json: String,
    max_attempts: u64,
}

/// Stable production settlement failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRetrySettlementErrorKind {
    MissingContext,
    CorruptContext,
    ContextUnavailable,
    IdentityMismatch,
    Ledger,
    StorageUnavailable,
}

/// Bounded production settlement failure.
#[derive(Debug)]
pub struct ModelRetrySettlementError {
    kind: ModelRetrySettlementErrorKind,
    ledger: Option<ModelRetryUsageError>,
}

impl ModelRetrySettlementError {
    const fn new(kind: ModelRetrySettlementErrorKind) -> Self {
        Self { kind, ledger: None }
    }

    const fn corrupt_context() -> Self {
        Self::new(ModelRetrySettlementErrorKind::CorruptContext)
    }

    fn ledger(error: ModelRetryUsageError) -> Self {
        Self {
            kind: ModelRetrySettlementErrorKind::Ledger,
            ledger: Some(error),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ModelRetrySettlementErrorKind {
        self.kind
    }
}

impl fmt::Display for ModelRetrySettlementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("durable Provider retry settlement failed")
    }
}

impl std::error::Error for ModelRetrySettlementError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.ledger
            .as_ref()
            .map(|error| error as &dyn std::error::Error)
    }
}

/// Typed result of one durable retry/Usage settlement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelRetrySettlementReceipt {
    Failed(ModelRetryDecisionReceipt),
    Completed(ModelUsageSettlementReceipt),
}

impl ModelRetrySettlementReceipt {
    #[must_use]
    pub const fn idempotent_replay(&self) -> bool {
        match self {
            Self::Failed(receipt) => receipt.idempotent_replay,
            Self::Completed(receipt) => receipt.idempotent_replay,
        }
    }
}

/// Unique production adapter from Gateway settlement to durable retry Usage.
pub struct DurableProviderRetrySettlement<'a> {
    storage: Mutex<SqliteStorage>,
    contexts: &'a dyn ModelRetrySettlementContextPort,
}

impl<'a> DurableProviderRetrySettlement<'a> {
    /// Opens the canonical retry ledger in the product data directory.
    ///
    /// # Errors
    ///
    /// Returns `StorageUnavailable` if the `SQLite` ledger cannot be opened.
    pub fn open(
        data_directory: impl AsRef<Path>,
        contexts: &'a dyn ModelRetrySettlementContextPort,
    ) -> Result<Self, ModelRetrySettlementError> {
        let storage = SqliteStorage::open(data_directory).map_err(|_| {
            ModelRetrySettlementError::new(ModelRetrySettlementErrorKind::StorageUnavailable)
        })?;
        Ok(Self {
            storage: Mutex::new(storage),
            contexts,
        })
    }

    /// Deterministically checkpoints and closes the settlement ledger.
    ///
    /// # Errors
    ///
    /// Returns `StorageUnavailable` for a poisoned mutex or close failure.
    pub fn close(self) -> Result<(), ModelRetrySettlementError> {
        let storage = self.storage.into_inner().map_err(|_| {
            ModelRetrySettlementError::new(ModelRetrySettlementErrorKind::StorageUnavailable)
        })?;
        Box::new(storage).close().map_err(|_| {
            ModelRetrySettlementError::new(ModelRetrySettlementErrorKind::StorageUnavailable)
        })
    }

    /// Applies one verified terminal to the canonical retry/Usage ledger.
    ///
    /// # Errors
    ///
    /// Fails closed for missing/corrupt context, changed identities, poisoned
    /// storage access, or any durable ledger conflict.
    pub fn apply(
        &self,
        settlement: &ProviderGatewaySettlement,
    ) -> Result<ModelRetrySettlementReceipt, ModelRetrySettlementError> {
        let context = self
            .contexts
            .load_context(&settlement.model_exchange_id)
            .map_err(|error| match error.kind() {
                ModelRetrySettlementContextErrorKind::Corrupt => {
                    ModelRetrySettlementError::corrupt_context()
                }
                ModelRetrySettlementContextErrorKind::Unavailable => {
                    ModelRetrySettlementError::new(
                        ModelRetrySettlementErrorKind::ContextUnavailable,
                    )
                }
            })?
            .ok_or_else(|| {
                ModelRetrySettlementError::new(ModelRetrySettlementErrorKind::MissingContext)
            })?;
        validate_settlement(&context, settlement)?;
        let command_request_id = terminal_request_id(&settlement.model_exchange_id);
        let mut storage = self.storage.lock().map_err(|_| {
            ModelRetrySettlementError::new(ModelRetrySettlementErrorKind::StorageUnavailable)
        })?;
        let result = {
            let mut service = ModelRetryUsageService::new(&mut *storage);
            match settlement.outcome {
                ProviderGatewayTerminalOutcome::Succeeded => service
                    .complete_attempt(
                        context.request(),
                        &ModelAttemptCompletionCommand {
                            command_request_id,
                            gateway: settlement.clone(),
                        },
                    )
                    .map(ModelRetrySettlementReceipt::Completed)
                    .map_err(ModelRetrySettlementError::ledger),
                ProviderGatewayTerminalOutcome::Failed
                | ProviderGatewayTerminalOutcome::Cancelled => service
                    .fail_attempt(
                        context.request(),
                        &ModelAttemptFailureCommand {
                            command_request_id,
                            gateway: settlement.clone(),
                        },
                    )
                    .map(ModelRetrySettlementReceipt::Failed)
                    .map_err(ModelRetrySettlementError::ledger),
            }?
        };
        if settlement.outcome == ProviderGatewayTerminalOutcome::Succeeded {
            crate::ProviderEnterpriseUsageReconciler::new(&mut storage)
                .reconcile_provider_page(None, 200)
                .map_err(|_| {
                    ModelRetrySettlementError::new(
                        ModelRetrySettlementErrorKind::StorageUnavailable,
                    )
                })?;
        }
        Ok(result)
    }
}

impl ProviderGatewaySettlementPort for DurableProviderRetrySettlement<'_> {
    fn settle(
        &self,
        settlement: &ProviderGatewaySettlement,
    ) -> Result<(), ProviderGatewaySettlementError> {
        self.apply(settlement)
            .map(|_| ())
            .map_err(|_| ProviderGatewaySettlementError)
    }
}

fn validate_start(
    request: &ModelRetryUsageRequest,
    receipt: &ModelAttemptStartReceipt,
) -> Result<(), ModelRetrySettlementError> {
    if receipt.request_id != request.request_id || receipt.attempt == 0 {
        return Err(ModelRetrySettlementError::new(
            ModelRetrySettlementErrorKind::IdentityMismatch,
        ));
    }
    let mut first_attempt = 1_u64;
    let expected = request.plan.steps().iter().find(|step| {
        let last_attempt = first_attempt
            .saturating_add(step.max_attempts())
            .saturating_sub(1);
        let matches = (first_attempt..=last_attempt).contains(&receipt.attempt);
        first_attempt = last_attempt.saturating_add(1);
        matches
    });
    let Some(step) = expected else {
        return Err(ModelRetrySettlementError::new(
            ModelRetrySettlementErrorKind::IdentityMismatch,
        ));
    };
    if receipt.provider_id != step.authority().route().provider_id
        || receipt.model_id != step.authority().route().model_id
        || receipt.route_fingerprint != step.authority().fingerprint()
    {
        return Err(ModelRetrySettlementError::new(
            ModelRetrySettlementErrorKind::IdentityMismatch,
        ));
    }
    Ok(())
}

fn validate_settlement(
    context: &ModelRetrySettlementContext,
    settlement: &ProviderGatewaySettlement,
) -> Result<(), ModelRetrySettlementError> {
    let start = context.start_receipt();
    if settlement.model_exchange_id != start.model_exchange_id
        || settlement.request_id != start.reservation_request_id
        || settlement.provider_id != start.provider_id
        || settlement.model_id != start.model_id
        || settlement.admission_terminal.route_authority_fingerprint != start.route_fingerprint
    {
        return Err(ModelRetrySettlementError::new(
            ModelRetrySettlementErrorKind::IdentityMismatch,
        ));
    }
    Ok(())
}

fn request_fingerprint(
    request: &ModelRetryUsageRequest,
    enterprise_quota_request: &EnterpriseQuotaReservationRequest,
) -> Result<String, ModelRetrySettlementError> {
    let bytes = serde_json::to_vec(&(
        &request.request_id,
        &request.attribution,
        request.plan.policy_id(),
        request.plan.policy_revision(),
        request.plan.fingerprint(),
        enterprise_quota_request,
    ))
    .map_err(|_| ModelRetrySettlementError::corrupt_context())?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn context_fingerprint(
    request_fingerprint: &str,
    start_receipt: &ModelAttemptStartReceipt,
) -> Result<String, ModelRetrySettlementError> {
    let bytes = serde_json::to_vec(&(request_fingerprint, start_receipt))
        .map_err(|_| ModelRetrySettlementError::corrupt_context())?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn terminal_request_id(model_exchange_id: &ModelExchangeId) -> RequestId {
    let digest = Sha256::digest(
        [
            b"winwincode.model-retry-terminal.v1\0".as_slice(),
            model_exchange_id.0.as_bytes(),
        ]
        .concat(),
    );
    let mut first = [0_u8; 16];
    first.copy_from_slice(&digest[..16]);
    let mut value = u128::from_be_bytes(first);
    let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut suffix = [b'0'; 26];
    for byte in suffix.iter_mut().rev() {
        *byte = alphabet[usize::try_from(value & 31).expect("base32 digit fits usize")];
        value >>= 5;
    }
    RequestId(format!(
        "req_{}",
        std::str::from_utf8(&suffix).expect("Crockford alphabet is UTF-8")
    ))
}
