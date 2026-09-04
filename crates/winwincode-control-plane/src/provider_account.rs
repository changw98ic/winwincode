// SPDX-License-Identifier: Apache-2.0

//! Provider-account ownership, `ChatGPT` device sign-in, and organization pools.
//!
//! Public state contains only account identity, lifecycle, and Credential-reference
//! metadata. Device exchange identifiers and provider credentials cross only the
//! [`ProviderAccountSecretStore`] boundary. Organization pools reference
//! organization-owned connections and never copy credential material.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandEnvelope, PageInfo, ProviderAccountConnectionCompleteCommand,
    ProviderAccountConnectionCompleteCompletedResponse,
    ProviderAccountConnectionCompleteCompletedResponseCommand,
    ProviderAccountConnectionCompleteCompletedResponseOutcome, ProviderAccountConnectionGetQuery,
    ProviderAccountConnectionGetResultResponse, ProviderAccountConnectionGetResultResponseQuery,
    ProviderAccountConnectionListQuery, ProviderAccountConnectionListResultResponse,
    ProviderAccountConnectionListResultResponseQuery, ProviderAccountConnectionPage,
    ProviderAccountConnectionPageKind, ProviderAccountConnectionProjection,
    ProviderAccountConnectionRefreshCommand, ProviderAccountConnectionRefreshCompletedResponse,
    ProviderAccountConnectionRefreshCompletedResponseCommand,
    ProviderAccountConnectionRefreshCompletedResponseOutcome,
    ProviderAccountConnectionRevokeCommand, ProviderAccountConnectionRevokeCompletedResponse,
    ProviderAccountConnectionRevokeCompletedResponseCommand,
    ProviderAccountConnectionRevokeCompletedResponseOutcome, ProviderAccountConnectionStartCommand,
    ProviderAccountConnectionStartCompletedResponse,
    ProviderAccountConnectionStartCompletedResponseCommand,
    ProviderAccountConnectionStartCompletedResponseOutcome, ProviderAccountOwner,
    ProviderAccountPoolDisableCommand, ProviderAccountPoolDisableCompletedResponse,
    ProviderAccountPoolDisableCompletedResponseCommand,
    ProviderAccountPoolDisableCompletedResponseOutcome, ProviderAccountPoolGetQuery,
    ProviderAccountPoolGetResultResponse, ProviderAccountPoolGetResultResponseQuery,
    ProviderAccountPoolListQuery, ProviderAccountPoolListResultResponse,
    ProviderAccountPoolListResultResponseQuery, ProviderAccountPoolPage,
    ProviderAccountPoolPageKind, ProviderAccountPoolProjection, ProviderAccountPoolUpsertCommand,
    ProviderAccountPoolUpsertCompletedResponse, ProviderAccountPoolUpsertCompletedResponseCommand,
    ProviderAccountPoolUpsertCompletedResponseOutcome, ProviderAccountSource,
    ProviderLoginPromptProjection, RepositoryScope, Scope, SessionModelSelection,
};
use winwincode_domain::{
    CredentialReferenceId, Instant, ModelExchangeId, ProviderAccountConnectionId,
    ProviderAccountPoolId, RequestId, Revision, Sha256Digest, UserId,
};
use winwincode_storage::{
    NewOutboxEvent, ProductStateStorage, ReceiptIdentity, StateCommit, StorageError,
    StorageErrorKind,
};

use crate::command_receipt_identity;
use crate::credential_reference::{
    CredentialReferenceResolution, CredentialReferenceService, ResolvedSecret, SecretStoreError,
};
use crate::local_secret_store::LocalSecretStoreAdapter;
use crate::provider_catalog::{
    ModelCapability, ModelToolSupport, ProviderCatalogRequest, ProviderCatalogService,
    ProviderDescriptor,
};

const CATALOG_STREAM: &str = "provider-account-catalog:v1";
const CATALOG_SCHEMA: &str = "winwincode.provider-account-catalog.v1";
const EVENT_TOPIC: &str = "provider.account.lifecycle.v1";
const DEFAULT_DEVICE_LIFETIME_MILLIS: u64 = 15 * 60 * 1_000;
/// Reserved Provider Gateway identity for `ChatGPT` account-backed execution.
///
/// It is deliberately separate from deployment-managed Provider IDs so adding
/// account binding cannot replace or collide with the existing system route.
pub const OPENAI_CHATGPT_PROVIDER_ID: &str = "winwincode-openai-chatgpt";

/// Stable failure categories exposed by the account application.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderAccountErrorKind {
    InvalidRequest,
    PermissionDenied,
    NotFound,
    WrongState,
    RevisionConflict,
    RequestConflict,
    ProviderUnavailable,
    SecretStore,
    Storage,
}

/// Secret-safe provider-account failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderAccountError {
    kind: ProviderAccountErrorKind,
    message: &'static str,
}

impl ProviderAccountError {
    const fn new(kind: ProviderAccountErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            ProviderAccountErrorKind::InvalidRequest,
            "provider account request is invalid",
        )
    }

    const fn denied() -> Self {
        Self::new(
            ProviderAccountErrorKind::PermissionDenied,
            "provider account access is denied",
        )
    }

    const fn missing() -> Self {
        Self::new(
            ProviderAccountErrorKind::NotFound,
            "provider account resource was not found",
        )
    }

    const fn wrong_state() -> Self {
        Self::new(
            ProviderAccountErrorKind::WrongState,
            "provider account state rejects this operation",
        )
    }

    pub(crate) const fn provider_unavailable() -> Self {
        Self::new(
            ProviderAccountErrorKind::ProviderUnavailable,
            "provider sign-in service is unavailable",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderAccountErrorKind {
        self.kind
    }
}

impl fmt::Display for ProviderAccountError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProviderAccountError {}

impl From<StorageError> for ProviderAccountError {
    fn from(error: StorageError) -> Self {
        let kind = match error.kind() {
            StorageErrorKind::RevisionConflict => ProviderAccountErrorKind::RevisionConflict,
            StorageErrorKind::RequestConflict => ProviderAccountErrorKind::RequestConflict,
            StorageErrorKind::InvalidInput | StorageErrorKind::RequestReplayMissing => {
                ProviderAccountErrorKind::InvalidRequest
            }
            StorageErrorKind::EventCursorExpired
            | StorageErrorKind::JournalAlreadyExists
            | StorageErrorKind::JournalNotFound
            | StorageErrorKind::JournalConflict
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => ProviderAccountErrorKind::Storage,
        };
        Self::new(kind, "provider account storage operation failed")
    }
}

impl From<SecretStoreError> for ProviderAccountError {
    fn from(_error: SecretStoreError) -> Self {
        Self::new(
            ProviderAccountErrorKind::SecretStore,
            "provider account secret operation failed",
        )
    }
}

/// Short-lived device prompt plus the opaque provider exchange identity.
pub struct ProviderDeviceAuthorization {
    pub verification_url: String,
    pub user_code: String,
    pub device_auth_id: String,
    pub poll_after_seconds: u64,
    pub expires_at_millis: u64,
}

impl fmt::Debug for ProviderDeviceAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderDeviceAuthorization")
            .field("verification_url", &self.verification_url)
            .field("user_code", &self.user_code)
            .field("device_auth_id", &"[REDACTED]")
            .field("poll_after_seconds", &self.poll_after_seconds)
            .field("expires_at_millis", &self.expires_at_millis)
            .finish()
    }
}

/// Provider credential bundle kept exclusively in the `SecretStore`.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCredentialBundle {
    schema: String,
    access_token: String,
    refresh_token: String,
    id_token: String,
    account_id: Option<String>,
    expires_at_millis: Option<u64>,
}

impl ProviderCredentialBundle {
    pub(crate) fn from_tokens(
        access_token: String,
        refresh_token: String,
        id_token: String,
    ) -> Result<Self, ProviderAccountError> {
        if [access_token.len(), refresh_token.len(), id_token.len()]
            .into_iter()
            .any(|length| length == 0 || length > 64 * 1024)
        {
            return Err(ProviderAccountError::provider_unavailable());
        }
        let id_claims = jwt_claims(&id_token)?;
        let access_claims = jwt_claims(&access_token).ok();
        let account_id = id_claims
            .get("chatgpt_account_id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                id_claims
                    .pointer("/https://api.openai.com/auth/chatgpt_account_id")
                    .and_then(serde_json::Value::as_str)
            })
            .map(ToOwned::to_owned);
        let expires_at_millis = access_claims
            .as_ref()
            .and_then(|claims| claims.get("exp"))
            .and_then(serde_json::Value::as_u64)
            .or_else(|| id_claims.get("exp").and_then(serde_json::Value::as_u64))
            .and_then(|seconds| seconds.checked_mul(1_000));
        Ok(Self {
            schema: "winwincode.provider-credential.v1".to_owned(),
            access_token,
            refresh_token,
            id_token,
            account_id,
            expires_at_millis,
        })
    }

    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }

    pub(crate) fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    pub(crate) fn id_token(&self) -> &str {
        &self.id_token
    }
}

impl Drop for ProviderCredentialBundle {
    fn drop(&mut self) {
        wipe_string(&mut self.access_token);
        wipe_string(&mut self.refresh_token);
        wipe_string(&mut self.id_token);
    }
}

fn wipe_string(value: &mut String) {
    let mut bytes = std::mem::take(value).into_bytes();
    bytes.fill(0);
}

impl fmt::Debug for ProviderCredentialBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCredentialBundle")
            .field("schema", &self.schema)
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("id_token", &"[REDACTED]")
            .field("account_id", &self.account_id.as_ref().map(|_| "[PRESENT]"))
            .field("expires_at_millis", &self.expires_at_millis)
            .finish()
    }
}

/// One nonblocking device-poll outcome.
pub enum ProviderDevicePoll {
    Pending,
    Authorized(ProviderCredentialBundle),
    Rejected,
}

/// Network boundary for `ChatGPT` device sign-in and provider credential lifecycle.
pub trait ProviderAccountAuthorizationPort: Send + Sync {
    /// Starts a short-lived provider device authorization.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is unavailable or returns an invalid prompt.
    fn start_device_authorization(
        &self,
        now_millis: u64,
    ) -> Result<ProviderDeviceAuthorization, ProviderAccountError>;

    /// Polls one short-lived provider device authorization.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is unavailable or returns invalid credentials.
    fn poll_device_authorization(
        &self,
        device_auth_id: &str,
        user_code: &str,
        now_millis: u64,
    ) -> Result<ProviderDevicePoll, ProviderAccountError>;

    /// Refreshes a provider credential bundle.
    ///
    /// # Errors
    ///
    /// Returns an error when refresh is rejected or the provider is unavailable.
    fn refresh(
        &self,
        credential: &ProviderCredentialBundle,
        now_millis: u64,
    ) -> Result<ProviderCredentialBundle, ProviderAccountError>;

    /// Revokes a provider credential bundle upstream.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider does not accept the revocation request.
    fn revoke(&self, credential: &ProviderCredentialBundle) -> Result<(), ProviderAccountError>;
}

/// Write side of the existing canonical `SecretStore` boundary.
pub trait ProviderAccountSecretStore: Send + Sync {
    /// Stores the first secret revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the secret store rejects the write.
    fn store(
        &self,
        reference: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<(), SecretStoreError>;

    /// Stores a replacement secret revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the secret store rejects the rotation.
    fn rotate(
        &self,
        reference: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<(), SecretStoreError>;

    /// Removes obsolete secret revisions.
    ///
    /// # Errors
    ///
    /// Returns an error when the secret store cannot remove obsolete revisions.
    fn cleanup(&self, reference: &CredentialReferenceResolution) -> Result<(), SecretStoreError>;

    /// Deletes all secret material for a reference.
    ///
    /// # Errors
    ///
    /// Returns an error when the secret store cannot delete the material.
    fn delete(&self, reference: &CredentialReferenceResolution) -> Result<(), SecretStoreError>;

    /// Resolves the current secret revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the secret is missing, invalid, or unavailable.
    fn resolve(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError>;
}

impl ProviderAccountSecretStore for LocalSecretStoreAdapter {
    fn store(
        &self,
        reference: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<(), SecretStoreError> {
        LocalSecretStoreAdapter::store(self, reference, secret).map(|_| ())
    }

    fn rotate(
        &self,
        reference: &CredentialReferenceResolution,
        secret: ResolvedSecret,
    ) -> Result<(), SecretStoreError> {
        LocalSecretStoreAdapter::rotate(self, reference, secret).map(|_| ())
    }

    fn cleanup(&self, reference: &CredentialReferenceResolution) -> Result<(), SecretStoreError> {
        LocalSecretStoreAdapter::cleanup(self, reference).map(|_| ())
    }

    fn delete(&self, reference: &CredentialReferenceResolution) -> Result<(), SecretStoreError> {
        LocalSecretStoreAdapter::delete(self, reference).map(|_| ())
    }

    fn resolve(
        &self,
        reference: &CredentialReferenceResolution,
    ) -> Result<ResolvedSecret, SecretStoreError> {
        crate::SecretStorePort::resolve(self, reference)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderAccountCatalog {
    schema: String,
    revision: u64,
    connections: BTreeMap<String, StoredConnection>,
    pools: BTreeMap<String, ProviderAccountPoolProjection>,
    exchange_routes: BTreeMap<String, StoredExchangeRoute>,
    usage: BTreeMap<String, StoredAccountUsage>,
}

impl Default for ProviderAccountCatalog {
    fn default() -> Self {
        Self {
            schema: CATALOG_SCHEMA.to_owned(),
            revision: 0,
            connections: BTreeMap::new(),
            pools: BTreeMap::new(),
            exchange_routes: BTreeMap::new(),
            usage: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredExchangeRoute {
    account_connection_id: ProviderAccountConnectionId,
    account_pool_id: Option<ProviderAccountPoolId>,
    attempted_account_connection_ids: Vec<ProviderAccountConnectionId>,
    actor_user_id: UserId,
    provider_id: String,
    model_id: String,
    period_id: String,
    active: bool,
    retryable_before_acceptance: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredAccountUsage {
    tokens: u64,
}

/// Concrete account metadata frozen for one model exchange.
pub(crate) struct ProviderAccountRouteResolution {
    pub credential_reference_id: CredentialReferenceId,
    pub credential_scope: Scope,
}

/// Why an account route stopped consuming pool capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderAccountExchangeSettlement {
    /// The Provider did not accept the request and a pool retry may select a
    /// different account. Every attempted account remains durably excluded.
    RetryableBeforeAcceptance,
    /// The exchange was accepted, rejected permanently, cancelled, or reached
    /// another terminal boundary. Its frozen account must never be replaced.
    Final,
}

/// Secret-free account source selection used by the Provider Gateway.
pub struct ProviderAccountRoutingService<'a> {
    storage: &'a mut dyn ProductStateStorage,
}

impl<'a> ProviderAccountRoutingService<'a> {
    pub fn new(storage: &'a mut dyn ProductStateStorage) -> Self {
        Self { storage }
    }

    /// Confirms that a session's selected account source belongs to its user and scope.
    ///
    /// # Errors
    ///
    /// Returns an error when the account or pool is unavailable, outside the scope, or
    /// does not permit the selected model.
    pub fn validate_session_selection(
        &self,
        actor_user_id: &UserId,
        repository_scope: &RepositoryScope,
        selection: &SessionModelSelection,
    ) -> Result<(), ProviderAccountError> {
        if matches!(
            selection.account_source,
            ProviderAccountSource::SystemDefaultProviderAccountSource(_)
        ) {
            let catalog = load_catalog(self.storage)?;
            if enterprise_only_policy_applies(&catalog, repository_scope, &selection.model_id) {
                return Err(ProviderAccountError::denied());
            }
            return Ok(());
        }
        self.resolve_candidate(actor_user_id, repository_scope, selection, None, "0000-00")
            .map(|_| ())
    }

    pub(crate) fn default_selection_for_user(
        &self,
        actor_user_id: &UserId,
        repository_scope: &RepositoryScope,
        model_id: &str,
        observed_at: &Instant,
    ) -> Result<Option<SessionModelSelection>, ProviderAccountError> {
        let catalog = load_catalog(self.storage)?;
        let mut pools = catalog
            .pools
            .values()
            .filter(|pool| {
                pool.enabled
                    && pool.organization_id == repository_scope.organization_id
                    && pool
                        .allowed_model_ids
                        .iter()
                        .any(|allowed| allowed == model_id)
            })
            .collect::<Vec<_>>();
        pools.sort_by(|left, right| {
            (source_policy_priority(&left.source_policy), &left.id.0)
                .cmp(&(source_policy_priority(&right.source_policy), &right.id.0))
        });
        let Some(pool) = pools.first() else {
            return Ok(None);
        };
        if pool.source_policy == "allow_personal_default_personal" {
            let mut personal = catalog
                .connections
                .values()
                .filter(|connection| {
                    matches!(
                        &connection.projection.owner,
                        ProviderAccountOwner::PersonalProviderAccountOwner(owner)
                            if owner.user_id == *actor_user_id
                    ) && connection.projection.provider_id == OPENAI_CHATGPT_PROVIDER_ID
                        && connection_is_active_at(&connection.projection, Some(observed_at))
                })
                .map(|connection| connection.projection.id.clone())
                .collect::<Vec<_>>();
            personal.sort_by(|left, right| left.0.cmp(&right.0));
            if let Some(account_connection_id) = personal.into_iter().next() {
                return Ok(Some(SessionModelSelection {
                    account_source: ProviderAccountSource::PersonalProviderAccountSource(
                        winwincode_api::generated::PersonalProviderAccountSource {
                            account_connection_id,
                            kind: winwincode_api::generated::PersonalProviderAccountSourceKind::Personal,
                        },
                    ),
                    model_id: model_id.to_owned(),
                    provider_id: OPENAI_CHATGPT_PROVIDER_ID.to_owned(),
                }));
            }
        }
        Ok(Some(SessionModelSelection {
            account_source: ProviderAccountSource::EnterpriseProviderAccountPoolSource(
                winwincode_api::generated::EnterpriseProviderAccountPoolSource {
                    account_pool_id: pool.id.clone(),
                    kind: winwincode_api::generated::EnterpriseProviderAccountPoolSourceKind::EnterprisePool,
                },
            ),
            model_id: model_id.to_owned(),
            provider_id: OPENAI_CHATGPT_PROVIDER_ID.to_owned(),
        }))
    }

    pub(crate) fn select_for_exchange(
        &mut self,
        actor_user_id: &UserId,
        repository_scope: &RepositoryScope,
        selection: &SessionModelSelection,
        model_exchange_id: &ModelExchangeId,
        period_id: &str,
        observed_at: &Instant,
    ) -> Result<Option<ProviderAccountRouteResolution>, ProviderAccountError> {
        if matches!(
            selection.account_source,
            ProviderAccountSource::SystemDefaultProviderAccountSource(_)
        ) {
            return Ok(None);
        }
        let mut catalog = load_catalog(self.storage)?;
        if let Some(existing) = catalog.exchange_routes.get(&model_exchange_id.0).cloned() {
            if existing.actor_user_id != *actor_user_id
                || existing.provider_id != selection.provider_id
                || existing.model_id != selection.model_id
                || !stored_route_matches_selection(&existing, selection)
            {
                return Err(ProviderAccountError::new(
                    ProviderAccountErrorKind::RequestConflict,
                    "model exchange account route conflicts with durable selection",
                ));
            }
            if existing.active {
                let connection = catalog
                    .connections
                    .get(&existing.account_connection_id.0)
                    .ok_or_else(ProviderAccountError::missing)?;
                return Ok(Some(ProviderAccountRouteResolution {
                    credential_reference_id: connection.projection.credential_reference_id.clone(),
                    credential_scope: connection.scope.clone(),
                }));
            }
            if !existing.retryable_before_acceptance {
                return Err(ProviderAccountError::wrong_state());
            }
        }
        let previous = catalog.exchange_routes.get(&model_exchange_id.0).cloned();
        let excluded_account_ids = previous
            .as_ref()
            .filter(|route| route.account_pool_id.is_some())
            .map_or_else(BTreeSet::new, |route| {
                route
                    .attempted_account_connection_ids
                    .iter()
                    .map(|account_id| account_id.0.clone())
                    .collect()
            });
        let (connection_id, pool_id) = resolve_candidate_from_catalog(
            &catalog,
            actor_user_id,
            repository_scope,
            selection,
            Some((model_exchange_id, &excluded_account_ids)),
            period_id,
            Some(observed_at),
        )?;
        let connection = catalog
            .connections
            .get(&connection_id.0)
            .ok_or_else(ProviderAccountError::missing)?;
        let resolution = ProviderAccountRouteResolution {
            credential_reference_id: connection.projection.credential_reference_id.clone(),
            credential_scope: connection.scope.clone(),
        };
        let mut attempted_account_connection_ids =
            previous.map_or_else(Vec::new, |route| route.attempted_account_connection_ids);
        if !attempted_account_connection_ids.contains(&connection_id) {
            attempted_account_connection_ids.push(connection_id.clone());
        }
        let route = StoredExchangeRoute {
            account_connection_id: connection_id,
            account_pool_id: pool_id,
            attempted_account_connection_ids,
            actor_user_id: actor_user_id.clone(),
            provider_id: selection.provider_id.clone(),
            model_id: selection.model_id.clone(),
            period_id: period_id.to_owned(),
            active: true,
            retryable_before_acceptance: false,
        };
        catalog
            .exchange_routes
            .insert(model_exchange_id.0.clone(), route.clone());
        commit_internal_catalog(
            self.storage,
            repository_scope,
            model_exchange_id,
            "selected",
            catalog,
            &route,
        )?;
        Ok(Some(resolution))
    }

    pub(crate) fn settle_exchange(
        &mut self,
        repository_scope: &RepositoryScope,
        model_exchange_id: &ModelExchangeId,
        used_tokens: u64,
        settlement: ProviderAccountExchangeSettlement,
    ) -> Result<(), ProviderAccountError> {
        let mut catalog = load_catalog(self.storage)?;
        let Some(route) = catalog.exchange_routes.get_mut(&model_exchange_id.0) else {
            return Ok(());
        };
        if !route.active {
            return Ok(());
        }
        route.active = false;
        route.retryable_before_acceptance =
            settlement == ProviderAccountExchangeSettlement::RetryableBeforeAcceptance;
        if used_tokens > 0 {
            let usage_key = format!("{}:{}", route.account_connection_id.0, route.period_id);
            let usage = catalog.usage.entry(usage_key).or_default();
            usage.tokens = usage
                .tokens
                .checked_add(used_tokens)
                .ok_or_else(ProviderAccountError::invalid)?;
        }
        let result = route.clone();
        commit_internal_catalog(
            self.storage,
            repository_scope,
            model_exchange_id,
            "settled",
            catalog,
            &result,
        )
    }

    fn resolve_candidate(
        &self,
        actor_user_id: &UserId,
        repository_scope: &RepositoryScope,
        selection: &SessionModelSelection,
        model_exchange_id: Option<&ModelExchangeId>,
        period_id: &str,
    ) -> Result<(ProviderAccountConnectionId, Option<ProviderAccountPoolId>), ProviderAccountError>
    {
        let catalog = load_catalog(self.storage)?;
        let excluded_account_ids = BTreeSet::new();
        resolve_candidate_from_catalog(
            &catalog,
            actor_user_id,
            repository_scope,
            selection,
            model_exchange_id.map(|model_exchange_id| (model_exchange_id, &excluded_account_ids)),
            period_id,
            None,
        )
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredConnection {
    projection: ProviderAccountConnectionProjection,
    credential_revision: u64,
    scope: Scope,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredProviderSecret {
    PendingDevice {
        device_auth_id: String,
        user_code: String,
        expires_at_millis: u64,
    },
    Credential(ProviderCredentialBundle),
}

impl fmt::Debug for StoredProviderSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PendingDevice { .. } => "StoredProviderSecret::PendingDevice([REDACTED])",
            Self::Credential(_) => "StoredProviderSecret::Credential([REDACTED])",
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderAccountEvent<'a, T> {
    schema: &'static str,
    operation: &'static str,
    result: &'a T,
}

/// Durable account and account-pool application service.
pub struct ProviderAccountService<'a> {
    storage: &'a mut dyn ProductStateStorage,
    secrets: &'a dyn ProviderAccountSecretStore,
    authorization: &'a dyn ProviderAccountAuthorizationPort,
}

impl<'a> ProviderAccountService<'a> {
    #[must_use]
    pub fn new(
        storage: &'a mut dyn ProductStateStorage,
        secrets: &'a dyn ProviderAccountSecretStore,
        authorization: &'a dyn ProviderAccountAuthorizationPort,
    ) -> Self {
        Self {
            storage,
            secrets,
            authorization,
        }
    }

    /// Resolves the secret-free owner needed by the Server's enterprise policy gate.
    ///
    /// # Errors
    ///
    /// Returns an error when durable account state cannot be read or the account is missing.
    pub fn connection_owner(
        &self,
        id: &ProviderAccountConnectionId,
    ) -> Result<ProviderAccountOwner, ProviderAccountError> {
        self.load_catalog()?
            .connections
            .get(&id.0)
            .map(|stored| stored.projection.owner.clone())
            .ok_or_else(ProviderAccountError::missing)
    }

    /// Starts device authorization for a personal or organization-owned account.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid ownership, revision conflicts, provider failures,
    /// secret-store failures, or durable storage failures.
    #[allow(clippy::too_many_lines)] // Keeps the credential, secret, and catalog sequence visible.
    pub fn start(
        &mut self,
        command: &ProviderAccountConnectionStartCommand,
        now_millis: u64,
    ) -> Result<ProviderAccountConnectionStartCompletedResponse, ProviderAccountError> {
        let envelope = command_envelope(command)?;
        let identity = command_identity(&envelope)?;
        let digest = command_digest(command)?;
        if let Some(result) =
            self.replay::<ProviderAccountConnectionProjection>(&identity, &digest)?
        {
            return Ok(ProviderAccountConnectionStartCompletedResponse {
                command: ProviderAccountConnectionStartCompletedResponseCommand::ProviderAccountConnectionStart,
                current_revision: result.revision.clone(),
                outcome: ProviderAccountConnectionStartCompletedResponseOutcome::Completed,
                previous_revision: command.expected_revision.clone(),
                request_id: command.request_id.clone(),
                result,
                schema_version: command.schema_version.clone(),
            });
        }
        validate_connection_id(&command.payload.account_connection_id)?;
        validate_display_name(&command.payload.display_name)?;
        if command.payload.login_method != "chatgpt_device_code" {
            return Err(ProviderAccountError::invalid());
        }
        validate_owner(&command.actor, &command.scope, &command.payload.owner)?;
        let mut catalog = self.load_catalog()?;
        if catalog
            .connections
            .contains_key(&command.payload.account_connection_id.0)
            || command.expected_revision.0 != 0
        {
            return Err(ProviderAccountError::new(
                ProviderAccountErrorKind::RevisionConflict,
                "provider account connection already exists or revision does not match",
            ));
        }
        let device = self.authorization.start_device_authorization(now_millis)?;
        validate_device_authorization(&device, now_millis)?;
        let credential_reference_id =
            credential_reference_id(&command.payload.account_connection_id);
        let credential_command = credential_create_command(
            command,
            credential_reference_id.clone(),
            derived_request_id(&command.request_id, b"credential-create"),
        );
        CredentialReferenceService::new(self.storage)
            .create(&credential_command, now_millis)
            .map_err(|_| {
                ProviderAccountError::new(
                    ProviderAccountErrorKind::Storage,
                    "provider account credential metadata failed",
                )
            })?;
        let resolution = CredentialReferenceService::new(self.storage)
            .resolve(&command.scope, &credential_reference_id)
            .map_err(|_| {
                ProviderAccountError::new(
                    ProviderAccountErrorKind::Storage,
                    "provider account credential metadata failed",
                )
            })?;
        let pending = encode_secret(&StoredProviderSecret::PendingDevice {
            device_auth_id: device.device_auth_id,
            user_code: device.user_code.clone(),
            expires_at_millis: device.expires_at_millis,
        })?;
        self.secrets.store(&resolution, pending)?;
        if let Err(error) =
            ensure_openai_provider_catalog(self.storage, command, credential_reference_id.clone())
        {
            let _ = self.secrets.cleanup(&resolution);
            return Err(error);
        }
        let projection = ProviderAccountConnectionProjection {
            account_label: None,
            credential_reference_id,
            display_name: command.payload.display_name.clone(),
            expires_at: None,
            id: command.payload.account_connection_id.clone(),
            last_error_code: None,
            login_method: command.payload.login_method.clone(),
            login_prompt: Some(ProviderLoginPromptProjection {
                expires_at: instant_millis(device.expires_at_millis),
                poll_after_seconds: i64::try_from(device.poll_after_seconds)
                    .map_err(|_| ProviderAccountError::invalid())?,
                user_code: device.user_code,
                verification_url: device.verification_url,
            }),
            owner: command.payload.owner.clone(),
            plan: None,
            provider_id: OPENAI_CHATGPT_PROVIDER_ID.to_owned(),
            revision: Revision(1),
            state: "login_pending".to_owned(),
            updated_at: instant_millis(now_millis),
            workspace_id: None,
        };
        catalog.connections.insert(
            projection.id.0.clone(),
            StoredConnection {
                projection: projection.clone(),
                credential_revision: 1,
                scope: command.scope.clone(),
            },
        );
        self.commit(
            &identity,
            digest,
            catalog,
            "connection_started",
            &projection,
        )?;
        Ok(ProviderAccountConnectionStartCompletedResponse {
            command: ProviderAccountConnectionStartCompletedResponseCommand::ProviderAccountConnectionStart,
            current_revision: projection.revision.clone(),
            outcome: ProviderAccountConnectionStartCompletedResponseOutcome::Completed,
            previous_revision: command.expected_revision.clone(),
            request_id: command.request_id.clone(),
            result: projection,
            schema_version: command.schema_version.clone(),
        })
    }

    /// Polls and completes a pending device authorization.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid state, denied ownership, provider failures,
    /// secret-store failures, revision conflicts, or durable storage failures.
    pub fn complete(
        &mut self,
        command: &ProviderAccountConnectionCompleteCommand,
        now_millis: u64,
    ) -> Result<ProviderAccountConnectionCompleteCompletedResponse, ProviderAccountError> {
        let (projection, _changed) = self.mutate_connection(
            command,
            &command.payload.account_connection_id,
            now_millis,
            "connection_completed",
            |service, stored, now| {
                if stored.projection.state != "login_pending" {
                    return Err(ProviderAccountError::wrong_state());
                }
                let current = service.connection_secret(&stored.projection, &stored.scope)?;
                let StoredProviderSecret::PendingDevice {
                    device_auth_id,
                    user_code,
                    expires_at_millis,
                } = current
                else {
                    return Err(ProviderAccountError::wrong_state());
                };
                if now >= expires_at_millis {
                    "failed".clone_into(&mut stored.projection.state);
                    stored.projection.last_error_code = Some("LOGIN_EXPIRED".to_owned());
                    stored.projection.login_prompt = None;
                    return Ok(true);
                }
                match service.authorization.poll_device_authorization(
                    &device_auth_id,
                    &user_code,
                    now,
                )? {
                    ProviderDevicePoll::Pending => Ok(false),
                    ProviderDevicePoll::Rejected => {
                        "failed".clone_into(&mut stored.projection.state);
                        stored.projection.last_error_code = Some("LOGIN_REJECTED".to_owned());
                        stored.projection.login_prompt = None;
                        Ok(true)
                    }
                    ProviderDevicePoll::Authorized(credential) => {
                        service.replace_secret(
                            stored,
                            &StoredProviderSecret::Credential(credential),
                            now,
                        )?;
                        let credential =
                            service.connection_credential(&stored.projection, &stored.scope)?;
                        apply_credential_projection(&mut stored.projection, &credential, now)?;
                        Ok(true)
                    }
                }
            },
        )?;
        Ok(ProviderAccountConnectionCompleteCompletedResponse {
            command: ProviderAccountConnectionCompleteCompletedResponseCommand::ProviderAccountConnectionComplete,
            current_revision: projection.revision.clone(),
            outcome: ProviderAccountConnectionCompleteCompletedResponseOutcome::Completed,
            previous_revision: command.expected_revision.clone(),
            request_id: command.request_id.clone(),
            result: projection,
            schema_version: command.schema_version.clone(),
        })
    }

    /// Refreshes an active or refresh-required account credential.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid state, denied ownership, provider failures,
    /// secret-store failures, revision conflicts, or durable storage failures.
    pub fn refresh(
        &mut self,
        command: &ProviderAccountConnectionRefreshCommand,
        now_millis: u64,
    ) -> Result<ProviderAccountConnectionRefreshCompletedResponse, ProviderAccountError> {
        let (projection, _) = self.mutate_connection(
            command,
            &command.payload.account_connection_id,
            now_millis,
            "connection_refreshed",
            |service, stored, now| {
                if !matches!(
                    stored.projection.state.as_str(),
                    "active" | "refresh_required"
                ) {
                    return Err(ProviderAccountError::wrong_state());
                }
                let credential =
                    service.connection_credential(&stored.projection, &stored.scope)?;
                let refreshed = service.authorization.refresh(&credential, now)?;
                service.replace_secret(
                    stored,
                    &StoredProviderSecret::Credential(refreshed),
                    now,
                )?;
                let credential =
                    service.connection_credential(&stored.projection, &stored.scope)?;
                apply_credential_projection(&mut stored.projection, &credential, now)?;
                Ok(true)
            },
        )?;
        Ok(ProviderAccountConnectionRefreshCompletedResponse {
            command: ProviderAccountConnectionRefreshCompletedResponseCommand::ProviderAccountConnectionRefresh,
            current_revision: projection.revision.clone(),
            outcome: ProviderAccountConnectionRefreshCompletedResponseOutcome::Completed,
            previous_revision: command.expected_revision.clone(),
            request_id: command.request_id.clone(),
            result: projection,
            schema_version: command.schema_version.clone(),
        })
    }

    /// Revokes an account and deletes its locally stored provider credential.
    ///
    /// # Errors
    ///
    /// Returns an error for denied ownership, invalid state, revision conflicts,
    /// secret-store failures, or durable storage failures.
    pub fn revoke(
        &mut self,
        command: &ProviderAccountConnectionRevokeCommand,
        now_millis: u64,
    ) -> Result<ProviderAccountConnectionRevokeCompletedResponse, ProviderAccountError> {
        let (projection, _) = self.mutate_connection(
            command,
            &command.payload.account_connection_id,
            now_millis,
            "connection_revoked",
            |service, stored, now| {
                if stored.projection.state == "revoked" {
                    return Ok(false);
                }
                if let Ok(credential) =
                    service.connection_credential(&stored.projection, &stored.scope)
                {
                    let _ = service.authorization.revoke(&credential);
                }
                let resolution = CredentialReferenceService::new(service.storage)
                    .resolve(&stored.scope, &stored.projection.credential_reference_id)
                    .map_err(|_| ProviderAccountError::wrong_state())?;
                let revoke_command = credential_revoke_command(
                    &stored.projection,
                    &stored.scope,
                    stored.credential_revision,
                    derived_request_id_from_account(
                        &stored.projection.id,
                        stored.credential_revision,
                        b"credential-revoke",
                    ),
                )?;
                CredentialReferenceService::new(service.storage)
                    .revoke(&revoke_command, now)
                    .map_err(|_| ProviderAccountError::wrong_state())?;
                stored.credential_revision = stored
                    .credential_revision
                    .checked_add(1)
                    .ok_or_else(ProviderAccountError::invalid)?;
                let _ = service.secrets.delete(&resolution);
                "revoked".clone_into(&mut stored.projection.state);
                stored.projection.expires_at = None;
                stored.projection.login_prompt = None;
                stored.projection.last_error_code = None;
                Ok(true)
            },
        )?;
        Ok(ProviderAccountConnectionRevokeCompletedResponse {
            command: ProviderAccountConnectionRevokeCompletedResponseCommand::ProviderAccountConnectionRevoke,
            current_revision: projection.revision.clone(),
            outcome: ProviderAccountConnectionRevokeCompletedResponseOutcome::Completed,
            previous_revision: command.expected_revision.clone(),
            request_id: command.request_id.clone(),
            result: projection,
            schema_version: command.schema_version.clone(),
        })
    }

    /// Reads one account visible to the caller.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid pagination, denied ownership, missing account,
    /// or durable storage failure.
    pub fn connection_get(
        &self,
        query: &ProviderAccountConnectionGetQuery,
    ) -> Result<ProviderAccountConnectionGetResultResponse, ProviderAccountError> {
        validate_page(query.page.limit, query.page.cursor.as_ref())?;
        let catalog = self.load_catalog()?;
        let stored = catalog
            .connections
            .get(&query.parameters.account_connection_id.0)
            .ok_or_else(ProviderAccountError::missing)?;
        authorize_projection(&query.actor, &query.scope, &stored.projection.owner)?;
        Ok(ProviderAccountConnectionGetResultResponse {
            page: PageInfo {
                has_more: false,
                next_cursor: None,
            },
            query: ProviderAccountConnectionGetResultResponseQuery::ProviderAccountConnectionGet,
            request_id: query.request_id.clone(),
            result: stored.projection.clone(),
            schema_version: query.schema_version.clone(),
        })
    }

    /// Lists accounts visible to the caller.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid pagination or durable storage failure.
    pub fn connection_list(
        &self,
        query: &ProviderAccountConnectionListQuery,
    ) -> Result<ProviderAccountConnectionListResultResponse, ProviderAccountError> {
        validate_page(query.page.limit, query.page.cursor.as_ref())?;
        let limit =
            usize::try_from(query.page.limit).map_err(|_| ProviderAccountError::invalid())?;
        let mut items = self
            .load_catalog()?
            .connections
            .into_values()
            .map(|stored| stored.projection)
            .filter(|projection| {
                authorize_projection(&query.actor, &query.scope, &projection.owner).is_ok()
            })
            .filter(|projection| {
                query.parameters.states.is_empty()
                    || query.parameters.states.contains(&projection.state)
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        let has_more = items.len() > limit;
        items.truncate(limit);
        Ok(ProviderAccountConnectionListResultResponse {
            page: PageInfo {
                has_more,
                next_cursor: None,
            },
            query: ProviderAccountConnectionListResultResponseQuery::ProviderAccountConnectionList,
            request_id: query.request_id.clone(),
            result: ProviderAccountConnectionPage {
                items,
                kind: ProviderAccountConnectionPageKind::ProviderAccountConnectionPage,
            },
            schema_version: query.schema_version.clone(),
        })
    }

    /// Creates or updates an organization account pool.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid pool configuration, foreign or inactive accounts,
    /// revision conflicts, or durable storage failures.
    pub fn pool_upsert(
        &mut self,
        command: &ProviderAccountPoolUpsertCommand,
        now_millis: u64,
    ) -> Result<ProviderAccountPoolUpsertCompletedResponse, ProviderAccountError> {
        let envelope = command_envelope(command)?;
        let identity = command_identity(&envelope)?;
        let digest = command_digest(command)?;
        if let Some(result) = self.replay::<ProviderAccountPoolProjection>(&identity, &digest)? {
            return Ok(ProviderAccountPoolUpsertCompletedResponse {
                command:
                    ProviderAccountPoolUpsertCompletedResponseCommand::ProviderAccountPoolUpsert,
                current_revision: result.revision.clone(),
                outcome: ProviderAccountPoolUpsertCompletedResponseOutcome::Completed,
                previous_revision: command.expected_revision.clone(),
                request_id: command.request_id.clone(),
                result,
                schema_version: command.schema_version.clone(),
            });
        }
        validate_pool_payload(command)?;
        let organization_id = organization_for_scope(&command.scope);
        let mut catalog = self.load_catalog()?;
        let previous = catalog.pools.get(&command.payload.account_pool_id.0);
        let previous_revision = previous.map_or(0, |pool| pool.revision.0);
        if previous_revision != command.expected_revision.0 {
            return Err(ProviderAccountError::new(
                ProviderAccountErrorKind::RevisionConflict,
                "provider account pool revision does not match",
            ));
        }
        for account_id in &command.payload.account_connection_ids {
            let account = catalog
                .connections
                .get(&account_id.0)
                .ok_or_else(ProviderAccountError::missing)?;
            match &account.projection.owner {
                ProviderAccountOwner::OrganizationProviderAccountOwner(owner)
                    if owner.organization_id == organization_id
                        && account.projection.state == "active" => {}
                _ => return Err(ProviderAccountError::denied()),
            }
        }
        let revision = previous_revision
            .checked_add(1)
            .ok_or_else(ProviderAccountError::invalid)?;
        let projection = ProviderAccountPoolProjection {
            account_connection_ids: command.payload.account_connection_ids.clone(),
            allowed_model_ids: command.payload.allowed_model_ids.clone(),
            display_name: command.payload.display_name.clone(),
            enabled: true,
            id: command.payload.account_pool_id.clone(),
            max_concurrent_per_account: command.payload.max_concurrent_per_account,
            monthly_token_limit_per_account: command.payload.monthly_token_limit_per_account,
            organization_id,
            revision: Revision(revision),
            source_policy: command.payload.source_policy.clone(),
            updated_at: instant_millis(now_millis),
        };
        catalog
            .pools
            .insert(projection.id.0.clone(), projection.clone());
        self.commit(&identity, digest, catalog, "pool_upserted", &projection)?;
        Ok(ProviderAccountPoolUpsertCompletedResponse {
            command: ProviderAccountPoolUpsertCompletedResponseCommand::ProviderAccountPoolUpsert,
            current_revision: projection.revision.clone(),
            outcome: ProviderAccountPoolUpsertCompletedResponseOutcome::Completed,
            previous_revision: command.expected_revision.clone(),
            request_id: command.request_id.clone(),
            result: projection,
            schema_version: command.schema_version.clone(),
        })
    }

    /// Disables an organization account pool.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing or foreign pool, revision conflicts, or durable
    /// storage failures.
    pub fn pool_disable(
        &mut self,
        command: &ProviderAccountPoolDisableCommand,
        now_millis: u64,
    ) -> Result<ProviderAccountPoolDisableCompletedResponse, ProviderAccountError> {
        let envelope = command_envelope(command)?;
        let identity = command_identity(&envelope)?;
        let digest = command_digest(command)?;
        if let Some(result) = self.replay::<ProviderAccountPoolProjection>(&identity, &digest)? {
            return Ok(ProviderAccountPoolDisableCompletedResponse {
                command:
                    ProviderAccountPoolDisableCompletedResponseCommand::ProviderAccountPoolDisable,
                current_revision: result.revision.clone(),
                outcome: ProviderAccountPoolDisableCompletedResponseOutcome::Completed,
                previous_revision: command.expected_revision.clone(),
                request_id: command.request_id.clone(),
                result,
                schema_version: command.schema_version.clone(),
            });
        }
        let organization_id = organization_for_scope(&command.scope);
        let mut catalog = self.load_catalog()?;
        let pool = catalog
            .pools
            .get_mut(&command.payload.account_pool_id.0)
            .ok_or_else(ProviderAccountError::missing)?;
        if pool.organization_id != organization_id {
            return Err(ProviderAccountError::denied());
        }
        if pool.revision != command.expected_revision {
            return Err(ProviderAccountError::new(
                ProviderAccountErrorKind::RevisionConflict,
                "provider account pool revision does not match",
            ));
        }
        if pool.enabled {
            pool.enabled = false;
            pool.revision.0 = pool
                .revision
                .0
                .checked_add(1)
                .ok_or_else(ProviderAccountError::invalid)?;
            pool.updated_at = instant_millis(now_millis);
        }
        let projection = pool.clone();
        self.commit(&identity, digest, catalog, "pool_disabled", &projection)?;
        Ok(ProviderAccountPoolDisableCompletedResponse {
            command: ProviderAccountPoolDisableCompletedResponseCommand::ProviderAccountPoolDisable,
            current_revision: projection.revision.clone(),
            outcome: ProviderAccountPoolDisableCompletedResponseOutcome::Completed,
            previous_revision: command.expected_revision.clone(),
            request_id: command.request_id.clone(),
            result: projection,
            schema_version: command.schema_version.clone(),
        })
    }

    /// Reads one account pool within the caller's organization.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid pagination, a missing or foreign pool, or durable
    /// storage failure.
    pub fn pool_get(
        &self,
        query: &ProviderAccountPoolGetQuery,
    ) -> Result<ProviderAccountPoolGetResultResponse, ProviderAccountError> {
        validate_page(query.page.limit, query.page.cursor.as_ref())?;
        let organization_id = organization_for_scope(&query.scope);
        let pool = self
            .load_catalog()?
            .pools
            .get(&query.parameters.account_pool_id.0)
            .cloned()
            .ok_or_else(ProviderAccountError::missing)?;
        if pool.organization_id != organization_id {
            return Err(ProviderAccountError::denied());
        }
        Ok(ProviderAccountPoolGetResultResponse {
            page: PageInfo {
                has_more: false,
                next_cursor: None,
            },
            query: ProviderAccountPoolGetResultResponseQuery::ProviderAccountPoolGet,
            request_id: query.request_id.clone(),
            result: pool,
            schema_version: query.schema_version.clone(),
        })
    }

    /// Lists account pools within the caller's organization.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid pagination or durable storage failure.
    pub fn pool_list(
        &self,
        query: &ProviderAccountPoolListQuery,
    ) -> Result<ProviderAccountPoolListResultResponse, ProviderAccountError> {
        validate_page(query.page.limit, query.page.cursor.as_ref())?;
        let organization_id = organization_for_scope(&query.scope);
        let limit =
            usize::try_from(query.page.limit).map_err(|_| ProviderAccountError::invalid())?;
        let mut items = self
            .load_catalog()?
            .pools
            .into_values()
            .filter(|pool| pool.organization_id == organization_id)
            .filter(|pool| {
                query
                    .parameters
                    .enabled
                    .is_none_or(|enabled| pool.enabled == enabled)
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        let has_more = items.len() > limit;
        items.truncate(limit);
        Ok(ProviderAccountPoolListResultResponse {
            page: PageInfo {
                has_more,
                next_cursor: None,
            },
            query: ProviderAccountPoolListResultResponseQuery::ProviderAccountPoolList,
            request_id: query.request_id.clone(),
            result: ProviderAccountPoolPage {
                items,
                kind: ProviderAccountPoolPageKind::ProviderAccountPoolPage,
            },
            schema_version: query.schema_version.clone(),
        })
    }

    fn mutate_connection<C, F>(
        &mut self,
        command: &C,
        account_id: &ProviderAccountConnectionId,
        now_millis: u64,
        operation: &'static str,
        mutate: F,
    ) -> Result<(ProviderAccountConnectionProjection, bool), ProviderAccountError>
    where
        C: Serialize + CommandFields,
        F: FnOnce(&mut Self, &mut StoredConnection, u64) -> Result<bool, ProviderAccountError>,
    {
        let envelope = command_envelope(command)?;
        let identity = command_identity(&envelope)?;
        let digest = command_digest(command)?;
        if let Some(result) =
            self.replay::<ProviderAccountConnectionProjection>(&identity, &digest)?
        {
            return Ok((result, false));
        }
        let mut catalog = self.load_catalog()?;
        let mut stored = catalog
            .connections
            .remove(&account_id.0)
            .ok_or_else(ProviderAccountError::missing)?;
        authorize_projection(command.actor(), command.scope(), &stored.projection.owner)?;
        if stored.projection.revision != *command.expected_revision() {
            return Err(ProviderAccountError::new(
                ProviderAccountErrorKind::RevisionConflict,
                "provider account connection revision does not match",
            ));
        }
        let changed = mutate(self, &mut stored, now_millis)?;
        if changed {
            stored.projection.revision.0 = stored
                .projection
                .revision
                .0
                .checked_add(1)
                .ok_or_else(ProviderAccountError::invalid)?;
            stored.projection.updated_at = instant_millis(now_millis);
        }
        let projection = stored.projection.clone();
        catalog.connections.insert(account_id.0.clone(), stored);
        self.commit(&identity, digest, catalog, operation, &projection)?;
        Ok((projection, changed))
    }

    fn replace_secret(
        &mut self,
        stored: &mut StoredConnection,
        secret: &StoredProviderSecret,
        now_millis: u64,
    ) -> Result<(), ProviderAccountError> {
        let scope = stored.scope.clone();
        let current = CredentialReferenceService::new(self.storage)
            .resolve(&scope, &stored.projection.credential_reference_id)
            .map_err(|_| ProviderAccountError::wrong_state())?;
        self.secrets.rotate(&current, encode_secret(secret)?)?;
        let rotation_command = credential_rotate_command(
            &stored.projection,
            &scope,
            stored.credential_revision,
            derived_request_id_from_account(
                &stored.projection.id,
                stored.credential_revision,
                b"credential-rotate",
            ),
        )?;
        let response = CredentialReferenceService::new(self.storage)
            .rotate(&rotation_command, now_millis)
            .map_err(|_| {
                ProviderAccountError::new(
                    ProviderAccountErrorKind::Storage,
                    "provider account credential rotation failed",
                )
            })?;
        stored.credential_revision = u64::try_from(response.current_revision.0)
            .map_err(|_| ProviderAccountError::invalid())?;
        let next = CredentialReferenceService::new(self.storage)
            .resolve(&scope, &stored.projection.credential_reference_id)
            .map_err(|_| ProviderAccountError::wrong_state())?;
        self.secrets.cleanup(&next)?;
        Ok(())
    }

    fn connection_secret(
        &mut self,
        projection: &ProviderAccountConnectionProjection,
        scope: &Scope,
    ) -> Result<StoredProviderSecret, ProviderAccountError> {
        let resolution = CredentialReferenceService::new(self.storage)
            .resolve(scope, &projection.credential_reference_id)
            .map_err(|_| ProviderAccountError::wrong_state())?;
        let secret = self.secrets.resolve(&resolution)?;
        serde_json::from_slice(secret.expose()).map_err(|_| {
            ProviderAccountError::new(
                ProviderAccountErrorKind::SecretStore,
                "provider account secret record is invalid",
            )
        })
    }

    fn connection_credential(
        &mut self,
        projection: &ProviderAccountConnectionProjection,
        scope: &Scope,
    ) -> Result<ProviderCredentialBundle, ProviderAccountError> {
        match self.connection_secret(projection, scope)? {
            StoredProviderSecret::Credential(credential) => Ok(credential),
            StoredProviderSecret::PendingDevice { .. } => Err(ProviderAccountError::wrong_state()),
        }
    }

    fn load_catalog(&self) -> Result<ProviderAccountCatalog, ProviderAccountError> {
        let Some(state) = self.storage.load_state(CATALOG_STREAM)? else {
            return Ok(ProviderAccountCatalog::default());
        };
        let catalog: ProviderAccountCatalog =
            serde_json::from_slice(&state.payload).map_err(|_| {
                ProviderAccountError::new(
                    ProviderAccountErrorKind::Storage,
                    "provider account catalog is invalid",
                )
            })?;
        if catalog.schema != CATALOG_SCHEMA
            || catalog.revision != state.revision
            || serde_json::to_vec(&catalog).map_err(|_| ProviderAccountError::invalid())?
                != state.payload
        {
            return Err(ProviderAccountError::new(
                ProviderAccountErrorKind::Storage,
                "provider account catalog is invalid",
            ));
        }
        Ok(catalog)
    }

    fn replay<T: for<'de> Deserialize<'de>>(
        &self,
        identity: &ReceiptIdentity,
        digest: &Sha256Digest,
    ) -> Result<Option<T>, ProviderAccountError> {
        self.storage
            .load_receipt(identity, digest)?
            .map(|receipt| {
                let event = receipt.events.first().ok_or_else(|| {
                    ProviderAccountError::new(
                        ProviderAccountErrorKind::Storage,
                        "provider account receipt is invalid",
                    )
                })?;
                let event: serde_json::Value =
                    serde_json::from_slice(&event.payload).map_err(|_| {
                        ProviderAccountError::new(
                            ProviderAccountErrorKind::Storage,
                            "provider account receipt is invalid",
                        )
                    })?;
                serde_json::from_value(event.get("result").cloned().ok_or_else(|| {
                    ProviderAccountError::new(
                        ProviderAccountErrorKind::Storage,
                        "provider account receipt is invalid",
                    )
                })?)
                .map_err(|_| {
                    ProviderAccountError::new(
                        ProviderAccountErrorKind::Storage,
                        "provider account receipt is invalid",
                    )
                })
            })
            .transpose()
    }

    fn commit<T: Serialize>(
        &mut self,
        identity: &ReceiptIdentity,
        digest: Sha256Digest,
        mut catalog: ProviderAccountCatalog,
        operation: &'static str,
        result: &T,
    ) -> Result<(), ProviderAccountError> {
        let expected = catalog.revision;
        catalog.revision = catalog
            .revision
            .checked_add(1)
            .ok_or_else(ProviderAccountError::invalid)?;
        let state = serde_json::to_vec(&catalog).map_err(|_| ProviderAccountError::invalid())?;
        let payload = serde_json::to_vec(&ProviderAccountEvent {
            schema: "winwincode.provider-account-event.v1",
            operation,
            result,
        })
        .map_err(|_| ProviderAccountError::invalid())?;
        self.storage.commit(&StateCommit::new(
            identity.clone(),
            digest,
            CATALOG_STREAM,
            expected,
            state,
            vec![NewOutboxEvent::internal(
                format!("provider-account:{}", identity.request_id().0),
                EVENT_TOPIC,
                payload,
            )],
        ))?;
        Ok(())
    }
}

trait CommandFields {
    fn actor(&self) -> &Actor;
    fn scope(&self) -> &Scope;
    fn expected_revision(&self) -> &Revision;
}
macro_rules! command_fields {
    ($($ty:ty),+ $(,)?) => {$(
        impl CommandFields for $ty {
            fn actor(&self) -> &Actor { &self.actor }
            fn scope(&self) -> &Scope { &self.scope }
            fn expected_revision(&self) -> &Revision { &self.expected_revision }
        }
    )+};
}
command_fields!(
    ProviderAccountConnectionCompleteCommand,
    ProviderAccountConnectionRefreshCommand,
    ProviderAccountConnectionRevokeCommand
);

fn command_envelope<T: Serialize>(command: &T) -> Result<CommandEnvelope, ProviderAccountError> {
    serde_json::from_value(
        serde_json::to_value(command).map_err(|_| ProviderAccountError::invalid())?,
    )
    .map_err(|_| ProviderAccountError::invalid())
}
fn command_identity(command: &CommandEnvelope) -> Result<ReceiptIdentity, ProviderAccountError> {
    command_receipt_identity(&command.actor, &command.scope, command.request_id.clone())
        .map_err(Into::into)
}
fn command_digest<T: Serialize>(command: &T) -> Result<Sha256Digest, ProviderAccountError> {
    let bytes = serde_json::to_vec(command).map_err(|_| ProviderAccountError::invalid())?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}
fn validate_page(
    limit: i64,
    cursor: Option<&winwincode_domain::OpaqueCursor>,
) -> Result<(), ProviderAccountError> {
    if !(1..=200).contains(&limit) || cursor.is_some() {
        Err(ProviderAccountError::invalid())
    } else {
        Ok(())
    }
}
fn validate_connection_id(id: &ProviderAccountConnectionId) -> Result<(), ProviderAccountError> {
    validate_id(&id.0, "pac_")
}
fn validate_pool_id(id: &ProviderAccountPoolId) -> Result<(), ProviderAccountError> {
    validate_id(&id.0, "pap_")
}
fn validate_id(value: &str, prefix: &str) -> Result<(), ProviderAccountError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(ProviderAccountError::invalid());
    };
    if suffix.len() == 26 && suffix.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')) { Ok(()) } else { Err(ProviderAccountError::invalid()) }
}
fn validate_display_name(value: &str) -> Result<(), ProviderAccountError> {
    if value.trim() == value && !value.is_empty() && value.len() <= 500 {
        Ok(())
    } else {
        Err(ProviderAccountError::invalid())
    }
}
fn organization_for_scope(scope: &Scope) -> winwincode_domain::OrganizationId {
    match scope {
        Scope::OrganizationScope(scope) => scope.organization_id.clone(),
        Scope::WorkspaceScope(scope) => scope.organization_id.clone(),
        Scope::ProjectScope(scope) => scope.organization_id.clone(),
        Scope::RepositoryScope(scope) => scope.organization_id.clone(),
    }
}
fn validate_owner(
    actor: &Actor,
    scope: &Scope,
    owner: &ProviderAccountOwner,
) -> Result<(), ProviderAccountError> {
    authorize_projection(actor, scope, owner)
}
fn authorize_projection(
    actor: &Actor,
    scope: &Scope,
    owner: &ProviderAccountOwner,
) -> Result<(), ProviderAccountError> {
    match (actor, owner) {
        (Actor::UserActor(actor), ProviderAccountOwner::PersonalProviderAccountOwner(owner))
            if actor.id == owner.user_id =>
        {
            Ok(())
        }
        (_, ProviderAccountOwner::OrganizationProviderAccountOwner(owner))
            if organization_for_scope(scope) == owner.organization_id =>
        {
            Ok(())
        }
        _ => Err(ProviderAccountError::denied()),
    }
}
fn credential_reference_id(id: &ProviderAccountConnectionId) -> CredentialReferenceId {
    let digest = Sha256::digest(
        [
            b"winwincode.provider-account-credential.v1\0".as_slice(),
            id.0.as_bytes(),
        ]
        .concat(),
    );
    CredentialReferenceId(format!("crd_{}", crockford_26(&digest[..16])))
}
fn derived_request_id(request: &RequestId, purpose: &[u8]) -> RequestId {
    RequestId(format!(
        "req_{}",
        crockford_26(&Sha256::digest([request.0.as_bytes(), purpose].concat())[..16])
    ))
}
fn derived_request_id_from_account(
    id: &ProviderAccountConnectionId,
    revision: u64,
    purpose: &[u8],
) -> RequestId {
    RequestId(format!(
        "req_{}",
        crockford_26(
            &Sha256::digest([id.0.as_bytes(), &revision.to_be_bytes(), purpose].concat())[..16]
        )
    ))
}
fn crockford_26(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut value = u128::from_be_bytes(bytes.try_into().unwrap_or([0; 16]));
    let mut out = [b'0'; 26];
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(value & 31) as usize];
        value >>= 5;
    }
    String::from_utf8(out.to_vec()).unwrap_or_default()
}
fn instant_millis(value: u64) -> Instant {
    crate::instant_from_millis(value)
        .unwrap_or_else(|_| Instant("9999-12-31T23:59:59.999Z".to_owned()))
}
fn validate_device_authorization(
    device: &ProviderDeviceAuthorization,
    now: u64,
) -> Result<(), ProviderAccountError> {
    if device.verification_url.starts_with("https://")
        && !device.user_code.is_empty()
        && !device.device_auth_id.is_empty()
        && (1..=60).contains(&device.poll_after_seconds)
        && device.expires_at_millis > now
        && device.expires_at_millis <= now.saturating_add(DEFAULT_DEVICE_LIFETIME_MILLIS)
    {
        Ok(())
    } else {
        Err(ProviderAccountError::provider_unavailable())
    }
}
fn encode_secret(secret: &StoredProviderSecret) -> Result<ResolvedSecret, ProviderAccountError> {
    ResolvedSecret::from_bytes(
        serde_json::to_vec(secret).map_err(|_| ProviderAccountError::invalid())?,
    )
    .map_err(Into::into)
}
fn apply_credential_projection(
    projection: &mut ProviderAccountConnectionProjection,
    credential: &ProviderCredentialBundle,
    now: u64,
) -> Result<(), ProviderAccountError> {
    let claims = jwt_claims(&credential.id_token)?;
    projection.account_label = claims
        .get("email")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            claims
                .pointer("/https://api.openai.com/profile/email")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        });
    let auth = claims.get("https://api.openai.com/auth");
    projection.plan = auth
        .and_then(|value| value.get("chatgpt_plan_type"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    projection.workspace_id = credential.account_id.clone().or_else(|| {
        auth.and_then(|value| value.get("chatgpt_account_id"))
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
    });
    projection.expires_at = credential.expires_at_millis.map(instant_millis);
    projection.login_prompt = None;
    projection.last_error_code = None;
    if credential
        .expires_at_millis
        .is_some_and(|expires| expires <= now)
    {
        "refresh_required"
    } else {
        "active"
    }
    .clone_into(&mut projection.state);
    Ok(())
}
fn jwt_claims(jwt: &str) -> Result<serde_json::Value, ProviderAccountError> {
    let mut parts = jwt.split('.');
    let _ = parts.next();
    let payload = parts
        .next()
        .ok_or_else(ProviderAccountError::provider_unavailable)?;
    let _ = parts
        .next()
        .ok_or_else(ProviderAccountError::provider_unavailable)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| ProviderAccountError::provider_unavailable())?;
    serde_json::from_slice(&bytes).map_err(|_| ProviderAccountError::provider_unavailable())
}
fn validate_pool_payload(
    command: &ProviderAccountPoolUpsertCommand,
) -> Result<(), ProviderAccountError> {
    validate_pool_id(&command.payload.account_pool_id)?;
    validate_display_name(&command.payload.display_name)?;
    if command.payload.account_connection_ids.is_empty()
        || command.payload.account_connection_ids.len() > 200
        || command.payload.max_concurrent_per_account <= 0
        || command.payload.monthly_token_limit_per_account <= 0
        || !matches!(
            command.payload.source_policy.as_str(),
            "enterprise_only" | "allow_personal_default_personal" | "allow_personal_default_pool"
        )
    {
        return Err(ProviderAccountError::invalid());
    }
    let mut ids = command
        .payload
        .account_connection_ids
        .iter()
        .map(|id| id.0.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != command.payload.account_connection_ids.len() {
        return Err(ProviderAccountError::invalid());
    }
    Ok(())
}

fn load_catalog(
    storage: &dyn ProductStateStorage,
) -> Result<ProviderAccountCatalog, ProviderAccountError> {
    let Some(state) = storage.load_state(CATALOG_STREAM)? else {
        return Ok(ProviderAccountCatalog::default());
    };
    let catalog: ProviderAccountCatalog = serde_json::from_slice(&state.payload).map_err(|_| {
        ProviderAccountError::new(
            ProviderAccountErrorKind::Storage,
            "provider account catalog is invalid",
        )
    })?;
    if catalog.schema != CATALOG_SCHEMA
        || catalog.revision != state.revision
        || serde_json::to_vec(&catalog).map_err(|_| ProviderAccountError::invalid())?
            != state.payload
    {
        return Err(ProviderAccountError::new(
            ProviderAccountErrorKind::Storage,
            "provider account catalog is invalid",
        ));
    }
    Ok(catalog)
}

fn resolve_candidate_from_catalog(
    catalog: &ProviderAccountCatalog,
    actor_user_id: &UserId,
    repository_scope: &RepositoryScope,
    selection: &SessionModelSelection,
    exchange_context: Option<(&ModelExchangeId, &BTreeSet<String>)>,
    period_id: &str,
    observed_at: Option<&Instant>,
) -> Result<(ProviderAccountConnectionId, Option<ProviderAccountPoolId>), ProviderAccountError> {
    if selection.provider_id != OPENAI_CHATGPT_PROVIDER_ID
        || selection.model_id.is_empty()
        || selection.model_id.len() > 256
    {
        return Err(ProviderAccountError::invalid());
    }
    match &selection.account_source {
        ProviderAccountSource::SystemDefaultProviderAccountSource(_) => {
            Err(ProviderAccountError::invalid())
        }
        ProviderAccountSource::PersonalProviderAccountSource(source) => {
            if enterprise_only_policy_applies(catalog, repository_scope, &selection.model_id) {
                return Err(ProviderAccountError::denied());
            }
            let connection = catalog
                .connections
                .get(&source.account_connection_id.0)
                .ok_or_else(ProviderAccountError::missing)?;
            match &connection.projection.owner {
                ProviderAccountOwner::PersonalProviderAccountOwner(owner)
                    if owner.user_id == *actor_user_id
                        && connection_is_active_at(&connection.projection, observed_at)
                        && connection.projection.provider_id == selection.provider_id =>
                {
                    Ok((source.account_connection_id.clone(), None))
                }
                _ => Err(ProviderAccountError::denied()),
            }
        }
        ProviderAccountSource::EnterpriseProviderAccountPoolSource(source) => {
            let pool = catalog
                .pools
                .get(&source.account_pool_id.0)
                .ok_or_else(ProviderAccountError::missing)?;
            if !pool.enabled
                || pool.organization_id != repository_scope.organization_id
                || !pool.allowed_model_ids.contains(&selection.model_id)
            {
                return Err(ProviderAccountError::denied());
            }
            let max_concurrent = usize::try_from(pool.max_concurrent_per_account)
                .map_err(|_| ProviderAccountError::invalid())?;
            let monthly_limit = u64::try_from(pool.monthly_token_limit_per_account)
                .map_err(|_| ProviderAccountError::invalid())?;
            let mut candidates = pool
                .account_connection_ids
                .iter()
                .filter_map(|account_id| {
                    if exchange_context.is_some_and(|(_, excluded_account_ids)| {
                        excluded_account_ids.contains(&account_id.0)
                    }) {
                        return None;
                    }
                    let connection = catalog.connections.get(&account_id.0)?;
                    let organization_matches = matches!(
                        &connection.projection.owner,
                        ProviderAccountOwner::OrganizationProviderAccountOwner(owner)
                            if owner.organization_id == repository_scope.organization_id
                    );
                    if !organization_matches
                        || !connection_is_active_at(&connection.projection, observed_at)
                        || connection.projection.provider_id != selection.provider_id
                    {
                        return None;
                    }
                    let active = catalog
                        .exchange_routes
                        .values()
                        .filter(|route| route.active && route.account_connection_id == *account_id)
                        .count();
                    let used = catalog
                        .usage
                        .get(&format!("{}:{period_id}", account_id.0))
                        .map_or(0, |usage| usage.tokens);
                    if active >= max_concurrent || used >= monthly_limit {
                        return None;
                    }
                    let tie_break = exchange_context.map_or(0, |(exchange, _)| {
                        let digest = Sha256::digest(
                            [exchange.0.as_bytes(), account_id.0.as_bytes()].concat(),
                        );
                        u64::from_be_bytes(digest[..8].try_into().unwrap_or([0; 8]))
                    });
                    Some((active, used, tie_break, account_id.clone()))
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|left, right| {
                (left.0, left.1, left.2, &left.3.0).cmp(&(right.0, right.1, right.2, &right.3.0))
            });
            candidates
                .into_iter()
                .next()
                .map(|(_, _, _, account_id)| (account_id, Some(source.account_pool_id.clone())))
                .ok_or_else(ProviderAccountError::wrong_state)
        }
    }
}

fn stored_route_matches_selection(
    route: &StoredExchangeRoute,
    selection: &SessionModelSelection,
) -> bool {
    match &selection.account_source {
        ProviderAccountSource::PersonalProviderAccountSource(source) => {
            route.account_pool_id.is_none()
                && route.account_connection_id == source.account_connection_id
        }
        ProviderAccountSource::EnterpriseProviderAccountPoolSource(source) => {
            route.account_pool_id.as_ref() == Some(&source.account_pool_id)
        }
        ProviderAccountSource::SystemDefaultProviderAccountSource(_) => false,
    }
}

fn connection_is_active_at(
    projection: &ProviderAccountConnectionProjection,
    observed_at: Option<&Instant>,
) -> bool {
    projection.state == "active"
        && observed_at.is_none_or(|observed_at| {
            projection
                .expires_at
                .as_ref()
                .is_none_or(|expires_at| expires_at.0 > observed_at.0)
        })
}

fn source_policy_priority(policy: &str) -> u8 {
    match policy {
        "enterprise_only" => 0,
        "allow_personal_default_pool" => 1,
        "allow_personal_default_personal" => 2,
        _ => u8::MAX,
    }
}

fn enterprise_only_policy_applies(
    catalog: &ProviderAccountCatalog,
    repository_scope: &RepositoryScope,
    model_id: &str,
) -> bool {
    catalog.pools.values().any(|pool| {
        pool.enabled
            && pool.organization_id == repository_scope.organization_id
            && pool.source_policy == "enterprise_only"
            && pool
                .allowed_model_ids
                .iter()
                .any(|allowed| allowed == model_id)
    })
}

fn commit_internal_catalog<T: Serialize>(
    storage: &mut dyn ProductStateStorage,
    repository_scope: &RepositoryScope,
    model_exchange_id: &ModelExchangeId,
    operation: &'static str,
    mut catalog: ProviderAccountCatalog,
    result: &T,
) -> Result<(), ProviderAccountError> {
    let expected = catalog.revision;
    let request_id = RequestId(format!(
        "req_{}",
        crockford_26(
            &Sha256::digest(
                [
                    b"provider-account-route.v1\0".as_slice(),
                    model_exchange_id.0.as_bytes(),
                    operation.as_bytes(),
                    &expected.to_be_bytes(),
                ]
                .concat(),
            )[..16],
        )
    ));
    let actor = Actor::SystemActor(winwincode_api::generated::SystemActor {
        kind: winwincode_api::generated::SystemActorKind::System,
        id: winwincode_domain::SystemActorId("sys_00000000000000000000000001".to_owned()),
    });
    let scope = Scope::RepositoryScope(repository_scope.clone());
    let identity = command_receipt_identity(&actor, &scope, request_id)?;
    catalog.revision = catalog
        .revision
        .checked_add(1)
        .ok_or_else(ProviderAccountError::invalid)?;
    let state = serde_json::to_vec(&catalog).map_err(|_| ProviderAccountError::invalid())?;
    let payload = serde_json::to_vec(&ProviderAccountEvent {
        schema: "winwincode.provider-account-event.v1",
        operation,
        result,
    })
    .map_err(|_| ProviderAccountError::invalid())?;
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload)));
    storage.commit(&StateCommit::new(
        identity,
        digest,
        CATALOG_STREAM,
        expected,
        state,
        vec![NewOutboxEvent::internal(
            format!(
                "provider-account-route:{}:{operation}:{expected}",
                model_exchange_id.0
            ),
            EVENT_TOPIC,
            payload,
        )],
    ))?;
    Ok(())
}

fn credential_create_command(
    command: &ProviderAccountConnectionStartCommand,
    id: CredentialReferenceId,
    request_id: RequestId,
) -> winwincode_api::generated::CredentialReferenceCreateCommand {
    winwincode_api::generated::CredentialReferenceCreateCommand {
        actor: command.actor.clone(),
        command: winwincode_api::generated::CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
        expected_revision: Revision(0),
        payload: winwincode_api::generated::CredentialReferenceCreatePayload {
            credential_reference_id: id,
            display_name: format!("{} ChatGPT credential", command.payload.display_name),
            provider_id: OPENAI_CHATGPT_PROVIDER_ID.to_owned(),
            vault_locator: "provider-account-secret-store".to_owned(),
        },
        request_id,
        schema_version: command.schema_version.clone(),
        scope: command.scope.clone(),
    }
}
fn credential_rotate_command(
    projection: &ProviderAccountConnectionProjection,
    scope: &Scope,
    revision: u64,
    request_id: RequestId,
) -> Result<winwincode_api::generated::CredentialReferenceRotateCommand, ProviderAccountError> {
    let actor = match &projection.owner {
        ProviderAccountOwner::PersonalProviderAccountOwner(owner) => {
            Actor::UserActor(winwincode_api::generated::UserActor {
                kind: winwincode_api::generated::UserActorKind::User,
                id: owner.user_id.clone(),
            })
        }
        ProviderAccountOwner::OrganizationProviderAccountOwner(_) => {
            Actor::SystemActor(winwincode_api::generated::SystemActor {
                kind: winwincode_api::generated::SystemActorKind::System,
                id: winwincode_domain::SystemActorId("sys_00000000000000000000000001".to_owned()),
            })
        }
    };
    Ok(winwincode_api::generated::CredentialReferenceRotateCommand { actor, command: winwincode_api::generated::CredentialReferenceRotateCommandCommand::CredentialReferenceRotate, expected_revision: Revision(i64::try_from(revision).map_err(|_| ProviderAccountError::invalid())?), payload: winwincode_api::generated::CredentialReferenceRotatePayload { credential_reference_id: projection.credential_reference_id.clone(), vault_locator: "provider-account-secret-store".to_owned() }, request_id, schema_version: winwincode_domain::SchemaVersion::WinwincodeV1, scope: scope.clone() })
}

fn credential_revoke_command(
    projection: &ProviderAccountConnectionProjection,
    scope: &Scope,
    revision: u64,
    request_id: RequestId,
) -> Result<winwincode_api::generated::CredentialReferenceRevokeCommand, ProviderAccountError> {
    Ok(winwincode_api::generated::CredentialReferenceRevokeCommand {
        actor: owner_actor(&projection.owner),
        command: winwincode_api::generated::CredentialReferenceRevokeCommandCommand::CredentialReferenceRevoke,
        expected_revision: Revision(
            i64::try_from(revision).map_err(|_| ProviderAccountError::invalid())?,
        ),
        payload: winwincode_api::generated::CredentialReferenceRevokePayload {
            credential_reference_id: projection.credential_reference_id.clone(),
        },
        request_id,
        schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    })
}

fn owner_actor(owner: &ProviderAccountOwner) -> Actor {
    match owner {
        ProviderAccountOwner::PersonalProviderAccountOwner(owner) => {
            Actor::UserActor(winwincode_api::generated::UserActor {
                kind: winwincode_api::generated::UserActorKind::User,
                id: owner.user_id.clone(),
            })
        }
        ProviderAccountOwner::OrganizationProviderAccountOwner(_) => {
            Actor::SystemActor(winwincode_api::generated::SystemActor {
                kind: winwincode_api::generated::SystemActorKind::System,
                id: winwincode_domain::SystemActorId("sys_00000000000000000000000001".to_owned()),
            })
        }
    }
}

fn ensure_openai_provider_catalog(
    storage: &mut dyn ProductStateStorage,
    command: &ProviderAccountConnectionStartCommand,
    credential_reference_id: CredentialReferenceId,
) -> Result<(), ProviderAccountError> {
    let projection = ProviderCatalogService::new(storage)
        .project(&command.scope)
        .map_err(|_| {
            ProviderAccountError::new(
                ProviderAccountErrorKind::Storage,
                "provider account capability catalog failed",
            )
        })?;
    let models = [
        ("gpt-5.6-sol", "GPT-5.6-Sol"),
        ("gpt-5.6-terra", "GPT-5.6-Terra"),
        ("gpt-5.6-luna", "GPT-5.6-Luna"),
        ("gpt-5.5", "GPT-5.5"),
        ("gpt-5.4", "GPT-5.4"),
        ("gpt-5.4-mini", "GPT-5.4-Mini"),
        ("gpt-5.2", "GPT-5.2"),
        ("codex-auto-review", "Codex Auto Review"),
    ]
    .into_iter()
    .map(|(model_id, display_name)| ModelCapability {
        model_id: model_id.to_owned(),
        display_name: display_name.to_owned(),
        context_window_tokens: 272_000,
        max_output_tokens: 100_000,
        tool_support: ModelToolSupport::Parallel,
        reasoning_efforts: vec![
            "low".to_owned(),
            "medium".to_owned(),
            "high".to_owned(),
            "xhigh".to_owned(),
        ],
    })
    .collect();
    ProviderCatalogService::new(storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: command.actor.clone(),
                scope: command.scope.clone(),
                request_id: derived_request_id(&command.request_id, b"provider-catalog"),
                expected_catalog_version: projection.catalog_version,
            },
            &ProviderDescriptor {
                provider_id: OPENAI_CHATGPT_PROVIDER_ID.to_owned(),
                display_name: "OpenAI ChatGPT".to_owned(),
                adapter_kind: "openai-chatgpt-responses".to_owned(),
                credential_reference_id,
                models,
            },
        )
        .map_err(|_| {
            ProviderAccountError::new(
                ProviderAccountErrorKind::Storage,
                "provider account capability catalog failed",
            )
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use winwincode_api::generated::{
        EnterpriseProviderAccountPoolSource, EnterpriseProviderAccountPoolSourceKind,
        OrganizationProviderAccountOwner, PersonalProviderAccountOwner,
        PersonalProviderAccountSource, PersonalProviderAccountSourceKind,
        ProviderAccountConnectionCompleteCommandCommand, ProviderAccountConnectionCompletePayload,
        ProviderAccountConnectionRefreshCommandCommand, ProviderAccountConnectionRefreshPayload,
        ProviderAccountConnectionRevokeCommandCommand, ProviderAccountConnectionRevokePayload,
        ProviderAccountConnectionStartCommandCommand, ProviderAccountConnectionStartPayload,
        ProviderAccountConnectionStartPayloadProviderId, RepositoryScopeKind, UserActor,
        UserActorKind,
    };
    use winwincode_domain::{OrganizationId, ProjectId, RepositoryId, SchemaVersion, WorkspaceId};
    use winwincode_storage::SqliteStorage;

    use super::*;

    fn repository_scope() -> RepositoryScope {
        RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: OrganizationId("org_00000000000000000000000001".to_owned()),
            workspace_id: WorkspaceId("wsp_00000000000000000000000001".to_owned()),
            project_id: ProjectId("prj_00000000000000000000000001".to_owned()),
            repository_id: RepositoryId("rep_00000000000000000000000001".to_owned()),
        }
    }

    fn connection(id: &str, owner: ProviderAccountOwner) -> (String, StoredConnection) {
        let repository_scope = repository_scope();
        let id = ProviderAccountConnectionId(id.to_owned());
        (
            id.0.clone(),
            StoredConnection {
                projection: ProviderAccountConnectionProjection {
                    account_label: Some("account@example.test".to_owned()),
                    credential_reference_id: credential_reference_id(&id),
                    display_name: "Fixture account".to_owned(),
                    expires_at: None,
                    id,
                    last_error_code: None,
                    login_method: "chatgpt_device_code".to_owned(),
                    login_prompt: None,
                    owner,
                    plan: Some("team".to_owned()),
                    provider_id: OPENAI_CHATGPT_PROVIDER_ID.to_owned(),
                    revision: Revision(2),
                    state: "active".to_owned(),
                    updated_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
                    workspace_id: Some("chatgpt-workspace".to_owned()),
                },
                credential_revision: 2,
                scope: Scope::RepositoryScope(repository_scope),
            },
        )
    }

    struct AuthorizedDeviceFlow;

    impl ProviderAccountAuthorizationPort for AuthorizedDeviceFlow {
        fn start_device_authorization(
            &self,
            now_millis: u64,
        ) -> Result<ProviderDeviceAuthorization, ProviderAccountError> {
            Ok(ProviderDeviceAuthorization {
                verification_url: "https://auth.openai.com/codex/device".to_owned(),
                user_code: "ABCD-EFGH".to_owned(),
                device_auth_id: "DEVICE_AUTH_SECRET".to_owned(),
                poll_after_seconds: 5,
                expires_at_millis: now_millis + 60_000,
            })
        }

        fn poll_device_authorization(
            &self,
            _device_auth_id: &str,
            _user_code: &str,
            _now_millis: u64,
        ) -> Result<ProviderDevicePoll, ProviderAccountError> {
            let access = jwt(&serde_json::json!({ "exp": 1_900_000_000_u64 }));
            let identity = jwt(&serde_json::json!({
                "email": "account@example.test",
                "exp": 1_900_000_000_u64,
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "ACCOUNT_ID",
                    "chatgpt_plan_type": "team"
                }
            }));
            ProviderCredentialBundle::from_tokens(access, "REFRESH_TOKEN".to_owned(), identity)
                .map(ProviderDevicePoll::Authorized)
        }

        fn refresh(
            &self,
            _credential: &ProviderCredentialBundle,
            _now_millis: u64,
        ) -> Result<ProviderCredentialBundle, ProviderAccountError> {
            let access = jwt(&serde_json::json!({ "exp": 1_950_000_000_u64 }));
            let identity = jwt(&serde_json::json!({
                "email": "account@example.test",
                "exp": 1_950_000_000_u64,
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "ACCOUNT_ID",
                    "chatgpt_plan_type": "team"
                }
            }));
            ProviderCredentialBundle::from_tokens(
                access,
                "REFRESH_TOKEN_ROTATED".to_owned(),
                identity,
            )
        }

        fn revoke(
            &self,
            _credential: &ProviderCredentialBundle,
        ) -> Result<(), ProviderAccountError> {
            Ok(())
        }
    }

    fn jwt(claims: &serde_json::Value) -> String {
        format!(
            "e30.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("JWT claims"))
        )
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Covers one complete restart-safe credential lifecycle.
    fn device_login_refresh_revoke_and_restart_keep_tokens_secret() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-provider-account-device-login-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("test root");
        let mut storage = SqliteStorage::open(&root).expect("storage");
        let secret_store =
            LocalSecretStoreAdapter::open(root.join("secrets")).expect("secret store");
        let scope = Scope::RepositoryScope(repository_scope());
        let user_id = UserId("usr_00000000000000000000000001".to_owned());
        let actor = Actor::UserActor(UserActor {
            id: user_id.clone(),
            kind: UserActorKind::User,
        });
        let account_id = ProviderAccountConnectionId("pac_00000000000000000000000003".to_owned());
        let start = ProviderAccountConnectionStartCommand {
            actor: actor.clone(),
            command: ProviderAccountConnectionStartCommandCommand::ProviderAccountConnectionStart,
            expected_revision: Revision(0),
            payload: ProviderAccountConnectionStartPayload {
                account_connection_id: account_id.clone(),
                display_name: "My ChatGPT".to_owned(),
                login_method: "chatgpt_device_code".to_owned(),
                owner: ProviderAccountOwner::PersonalProviderAccountOwner(
                    PersonalProviderAccountOwner {
                        kind: winwincode_api::generated::PersonalProviderAccountOwnerKind::User,
                        user_id,
                    },
                ),
                provider_id: ProviderAccountConnectionStartPayloadProviderId::Openai,
            },
            request_id: RequestId("req_00000000000000000000000003".to_owned()),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: scope.clone(),
        };
        let started =
            ProviderAccountService::new(&mut storage, &secret_store, &AuthorizedDeviceFlow)
                .start(&start, 1_800_000_000_000)
                .expect("start device login");
        assert_eq!(started.result.state, "login_pending");
        assert_eq!(
            started
                .result
                .login_prompt
                .as_ref()
                .expect("public device prompt")
                .user_code,
            "ABCD-EFGH"
        );

        let complete = ProviderAccountConnectionCompleteCommand {
            actor: actor.clone(),
            command:
                ProviderAccountConnectionCompleteCommandCommand::ProviderAccountConnectionComplete,
            expected_revision: Revision(1),
            payload: ProviderAccountConnectionCompletePayload {
                account_connection_id: account_id,
            },
            request_id: RequestId("req_00000000000000000000000004".to_owned()),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: scope.clone(),
        };
        let completed =
            ProviderAccountService::new(&mut storage, &secret_store, &AuthorizedDeviceFlow)
                .complete(&complete, 1_800_000_001_000)
                .expect("complete device login");
        assert_eq!(completed.result.state, "active");
        assert_eq!(
            completed.result.account_label.as_deref(),
            Some("account@example.test")
        );
        assert_eq!(completed.result.workspace_id.as_deref(), Some("ACCOUNT_ID"));
        assert!(completed.result.login_prompt.is_none());
        let public = serde_json::to_string(&completed).expect("public response JSON");
        assert!(!public.contains("REFRESH_TOKEN"));
        assert!(!public.contains("DEVICE_AUTH_SECRET"));
        assert!(!public.contains("accessToken"));
        assert!(!public.contains("refreshToken"));

        drop(storage);
        let mut storage = SqliteStorage::open(&root).expect("restarted storage");
        let refresh = ProviderAccountConnectionRefreshCommand {
            actor: actor.clone(),
            command:
                ProviderAccountConnectionRefreshCommandCommand::ProviderAccountConnectionRefresh,
            expected_revision: Revision(2),
            payload: ProviderAccountConnectionRefreshPayload {
                account_connection_id: completed.result.id.clone(),
            },
            request_id: RequestId("req_00000000000000000000000005".to_owned()),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: scope.clone(),
        };
        let refreshed =
            ProviderAccountService::new(&mut storage, &secret_store, &AuthorizedDeviceFlow)
                .refresh(&refresh, 1_800_000_002_000)
                .expect("refresh after restart");
        assert_eq!(refreshed.result.revision, Revision(3));
        assert_eq!(refreshed.result.state, "active");
        let refreshed_public = serde_json::to_string(&refreshed).expect("refresh public JSON");
        assert!(!refreshed_public.contains("REFRESH_TOKEN_ROTATED"));

        let resolution = CredentialReferenceService::new(&mut storage)
            .resolve(&scope, &refreshed.result.credential_reference_id)
            .expect("credential reference before revoke");
        drop(storage);
        let mut storage = SqliteStorage::open(&root).expect("second restarted storage");
        let revoke = ProviderAccountConnectionRevokeCommand {
            actor,
            command: ProviderAccountConnectionRevokeCommandCommand::ProviderAccountConnectionRevoke,
            expected_revision: Revision(3),
            payload: ProviderAccountConnectionRevokePayload {
                account_connection_id: refreshed.result.id,
            },
            request_id: RequestId("req_00000000000000000000000006".to_owned()),
            schema_version: SchemaVersion::WinwincodeV1,
            scope,
        };
        let revoked =
            ProviderAccountService::new(&mut storage, &secret_store, &AuthorizedDeviceFlow)
                .revoke(&revoke, 1_800_000_003_000)
                .expect("revoke after restart");
        assert_eq!(revoked.result.revision, Revision(4));
        assert_eq!(revoked.result.state, "revoked");
        assert!(crate::SecretStorePort::resolve(&secret_store, &resolution).is_err());
        drop(storage);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn personal_selection_is_bound_to_its_user_and_active_connection() {
        let user = UserId("usr_00000000000000000000000001".to_owned());
        let account_id = ProviderAccountConnectionId("pac_00000000000000000000000001".to_owned());
        let mut catalog = ProviderAccountCatalog::default();
        let entry = connection(
            &account_id.0,
            ProviderAccountOwner::PersonalProviderAccountOwner(PersonalProviderAccountOwner {
                kind: winwincode_api::generated::PersonalProviderAccountOwnerKind::User,
                user_id: user.clone(),
            }),
        );
        catalog.connections.insert(entry.0, entry.1);
        let selection = SessionModelSelection {
            account_source: ProviderAccountSource::PersonalProviderAccountSource(
                PersonalProviderAccountSource {
                    account_connection_id: account_id.clone(),
                    kind: PersonalProviderAccountSourceKind::Personal,
                },
            ),
            model_id: "gpt-5.4".to_owned(),
            provider_id: OPENAI_CHATGPT_PROVIDER_ID.to_owned(),
        };

        assert_eq!(
            resolve_candidate_from_catalog(
                &catalog,
                &user,
                &repository_scope(),
                &selection,
                None,
                "2030-01",
                None,
            ),
            Ok((account_id, None))
        );
        assert_eq!(
            resolve_candidate_from_catalog(
                &catalog,
                &UserId("usr_00000000000000000000000002".to_owned()),
                &repository_scope(),
                &selection,
                None,
                "2030-01",
                None,
            )
            .expect_err("foreign personal account")
            .kind(),
            ProviderAccountErrorKind::PermissionDenied
        );

        let enterprise_account =
            ProviderAccountConnectionId("pac_00000000000000000000000002".to_owned());
        let entry = connection(
            &enterprise_account.0,
            ProviderAccountOwner::OrganizationProviderAccountOwner(
                OrganizationProviderAccountOwner {
                    kind: winwincode_api::generated::OrganizationProviderAccountOwnerKind::Organization,
                    organization_id: repository_scope().organization_id,
                },
            ),
        );
        catalog.connections.insert(entry.0, entry.1);
        catalog.pools.insert(
            "pap_00000000000000000000000001".to_owned(),
            ProviderAccountPoolProjection {
                account_connection_ids: vec![enterprise_account],
                allowed_model_ids: vec!["gpt-5.4".to_owned()],
                display_name: "Required enterprise pool".to_owned(),
                enabled: true,
                id: ProviderAccountPoolId("pap_00000000000000000000000001".to_owned()),
                max_concurrent_per_account: 1,
                monthly_token_limit_per_account: 100,
                organization_id: repository_scope().organization_id,
                revision: Revision(1),
                source_policy: "enterprise_only".to_owned(),
                updated_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
            },
        );
        assert_eq!(
            resolve_candidate_from_catalog(
                &catalog,
                &user,
                &repository_scope(),
                &selection,
                None,
                "2030-01",
                None,
            )
            .expect_err("enterprise-only policy blocks personal source")
            .kind(),
            ProviderAccountErrorKind::PermissionDenied
        );
        assert!(enterprise_only_policy_applies(
            &catalog,
            &repository_scope(),
            "gpt-5.4"
        ));
    }

    #[test]
    fn enterprise_pool_enforces_organization_concurrency_and_monthly_usage() {
        let repository_scope = repository_scope();
        let organization_id = repository_scope.organization_id.clone();
        let first = ProviderAccountConnectionId("pac_00000000000000000000000001".to_owned());
        let second = ProviderAccountConnectionId("pac_00000000000000000000000002".to_owned());
        let owner = |organization_id: OrganizationId| {
            ProviderAccountOwner::OrganizationProviderAccountOwner(
                OrganizationProviderAccountOwner {
                    kind: winwincode_api::generated::OrganizationProviderAccountOwnerKind::Organization,
                    organization_id,
                },
            )
        };
        let mut catalog = ProviderAccountCatalog::default();
        let entry = connection(&first.0, owner(organization_id.clone()));
        catalog.connections.insert(entry.0, entry.1);
        let entry = connection(&second.0, owner(organization_id.clone()));
        catalog.connections.insert(entry.0, entry.1);
        let pool_id = ProviderAccountPoolId("pap_00000000000000000000000001".to_owned());
        catalog.pools.insert(
            pool_id.0.clone(),
            ProviderAccountPoolProjection {
                account_connection_ids: vec![first.clone(), second.clone()],
                allowed_model_ids: vec!["gpt-5.4".to_owned()],
                display_name: "Engineering".to_owned(),
                enabled: true,
                id: pool_id.clone(),
                max_concurrent_per_account: 1,
                monthly_token_limit_per_account: 100,
                organization_id,
                revision: Revision(1),
                source_policy: "enterprise_only".to_owned(),
                updated_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
            },
        );
        catalog.exchange_routes.insert(
            "mdl_00000000000000000000000001".to_owned(),
            StoredExchangeRoute {
                account_connection_id: first.clone(),
                account_pool_id: Some(pool_id.clone()),
                attempted_account_connection_ids: vec![first],
                actor_user_id: UserId("usr_00000000000000000000000001".to_owned()),
                provider_id: OPENAI_CHATGPT_PROVIDER_ID.to_owned(),
                model_id: "gpt-5.4".to_owned(),
                period_id: "2030-01".to_owned(),
                active: true,
                retryable_before_acceptance: false,
            },
        );
        catalog.usage.insert(
            format!("{}:2030-01", second.0),
            StoredAccountUsage { tokens: 99 },
        );
        let selection = SessionModelSelection {
            account_source: ProviderAccountSource::EnterpriseProviderAccountPoolSource(
                EnterpriseProviderAccountPoolSource {
                    account_pool_id: pool_id.clone(),
                    kind: EnterpriseProviderAccountPoolSourceKind::EnterprisePool,
                },
            ),
            model_id: "gpt-5.4".to_owned(),
            provider_id: OPENAI_CHATGPT_PROVIDER_ID.to_owned(),
        };

        assert_eq!(
            resolve_candidate_from_catalog(
                &catalog,
                &UserId("usr_00000000000000000000000002".to_owned()),
                &repository_scope,
                &selection,
                None,
                "2030-01",
                None,
            ),
            Ok((second.clone(), Some(pool_id.clone())))
        );
        catalog
            .usage
            .get_mut(&format!("{}:2030-01", second.0))
            .expect("second account usage")
            .tokens = 100;
        assert_eq!(
            resolve_candidate_from_catalog(
                &catalog,
                &UserId("usr_00000000000000000000000002".to_owned()),
                &repository_scope,
                &selection,
                None,
                "2030-01",
                None,
            )
            .expect_err("pool capacity exhausted")
            .kind(),
            ProviderAccountErrorKind::WrongState
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Keeps one restart-safe failover and usage scenario intact.
    fn retryable_pool_open_switches_account_after_restart_without_double_usage() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-provider-account-pool-failover-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("test root");
        let repository_scope = repository_scope();
        let organization_id = repository_scope.organization_id.clone();
        let user_id = UserId("usr_00000000000000000000000009".to_owned());
        let first = ProviderAccountConnectionId("pac_00000000000000000000000011".to_owned());
        let second = ProviderAccountConnectionId("pac_00000000000000000000000012".to_owned());
        let pool_id = ProviderAccountPoolId("pap_00000000000000000000000011".to_owned());
        let exchange_id = ModelExchangeId("mdl_00000000000000000000000011".to_owned());
        let owner = |organization_id: OrganizationId| {
            ProviderAccountOwner::OrganizationProviderAccountOwner(
                OrganizationProviderAccountOwner {
                    kind: winwincode_api::generated::OrganizationProviderAccountOwnerKind::Organization,
                    organization_id,
                },
            )
        };
        let mut catalog = ProviderAccountCatalog::default();
        let entry = connection(&first.0, owner(organization_id.clone()));
        catalog.connections.insert(entry.0, entry.1);
        let entry = connection(&second.0, owner(organization_id.clone()));
        catalog.connections.insert(entry.0, entry.1);
        catalog.pools.insert(
            pool_id.0.clone(),
            ProviderAccountPoolProjection {
                account_connection_ids: vec![first, second],
                allowed_model_ids: vec!["gpt-5.4".to_owned()],
                display_name: "Failover pool".to_owned(),
                enabled: true,
                id: pool_id.clone(),
                max_concurrent_per_account: 1,
                monthly_token_limit_per_account: 100,
                organization_id,
                revision: Revision(1),
                source_policy: "enterprise_only".to_owned(),
                updated_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
            },
        );
        let selection = SessionModelSelection {
            account_source: ProviderAccountSource::EnterpriseProviderAccountPoolSource(
                EnterpriseProviderAccountPoolSource {
                    account_pool_id: pool_id,
                    kind: EnterpriseProviderAccountPoolSourceKind::EnterprisePool,
                },
            ),
            model_id: "gpt-5.4".to_owned(),
            provider_id: OPENAI_CHATGPT_PROVIDER_ID.to_owned(),
        };

        let mut storage = SqliteStorage::open(&root).expect("storage");
        commit_internal_catalog(
            &mut storage,
            &repository_scope,
            &exchange_id,
            "fixture",
            catalog,
            &"seed",
        )
        .expect("seed pool");
        ProviderAccountRoutingService::new(&mut storage)
            .select_for_exchange(
                &user_id,
                &repository_scope,
                &selection,
                &exchange_id,
                "2030-01",
                &Instant("2030-01-02T00:00:00.000Z".to_owned()),
            )
            .expect("first route");
        let first_route = load_catalog(&storage)
            .expect("first catalog")
            .exchange_routes
            .get(&exchange_id.0)
            .expect("first durable route")
            .account_connection_id
            .clone();
        ProviderAccountRoutingService::new(&mut storage)
            .settle_exchange(
                &repository_scope,
                &exchange_id,
                0,
                ProviderAccountExchangeSettlement::RetryableBeforeAcceptance,
            )
            .expect("release retryable route");

        drop(storage);
        let mut storage = SqliteStorage::open(&root).expect("restarted storage");
        ProviderAccountRoutingService::new(&mut storage)
            .select_for_exchange(
                &user_id,
                &repository_scope,
                &selection,
                &exchange_id,
                "2030-01",
                &Instant("2030-01-02T00:00:01.000Z".to_owned()),
            )
            .expect("failover route");
        let catalog = load_catalog(&storage).expect("failover catalog");
        let failover_route = catalog
            .exchange_routes
            .get(&exchange_id.0)
            .expect("failover durable route");
        assert_ne!(failover_route.account_connection_id, first_route);
        assert_eq!(failover_route.attempted_account_connection_ids.len(), 2);
        assert!(failover_route.active);
        assert!(catalog.usage.is_empty());

        ProviderAccountRoutingService::new(&mut storage)
            .settle_exchange(
                &repository_scope,
                &exchange_id,
                17,
                ProviderAccountExchangeSettlement::Final,
            )
            .expect("settle accepted route");
        let catalog = load_catalog(&storage).expect("settled catalog");
        assert_eq!(catalog.usage.len(), 1);
        assert_eq!(
            catalog.usage.values().next().map(|usage| usage.tokens),
            Some(17)
        );
        let Err(error) = ProviderAccountRoutingService::new(&mut storage).select_for_exchange(
            &user_id,
            &repository_scope,
            &selection,
            &exchange_id,
            "2030-01",
            &Instant("2030-01-02T00:00:02.000Z".to_owned()),
        ) else {
            panic!("accepted route cannot switch");
        };
        assert_eq!(error.kind(), ProviderAccountErrorKind::WrongState);
        drop(storage);
        let _ = fs::remove_dir_all(&root);
    }
}
