// SPDX-License-Identifier: Apache-2.0

//! Secret-safe, exact-scope `ModelRoute` availability projection.
//!
//! The browser receives only a bounded server-owned join of effective Settings,
//! Provider/model Catalog, Credential lifecycle, and request-pool readiness.
//! Credential locators, secrets, Provider adapter configuration, and pool
//! occupancy never cross this boundary.

use std::collections::BTreeSet;
use std::fmt;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, ControlPlaneWebSocketModelRouteAvailabilityInvalidatedEvent,
    ControlPlaneWebSocketModelRouteAvailabilityInvalidatedEventTypeValue,
    ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource,
    ControlPlaneWebSocketModelRouteAvailabilityListReloadQuery, ModelRoute,
    ModelRouteAvailabilityListQuery, ModelRouteAvailabilityListResultResponse,
    ModelRouteAvailabilityListResultResponseQuery, ModelRouteAvailabilityPage,
    ModelRouteAvailabilityPageKind, ModelRouteAvailabilityProjection, ModelRouteAvailabilityReason,
    ModelRouteAvailabilityStatus, ModelRouteToolSupport, PageInfo, ProjectScope, ProjectScopeKind,
    Scope,
};
use winwincode_domain::{
    ControlPlaneEventId, Instant, OpaqueCursor, RepositoryScope, RequestId, Revision,
    SchemaVersion, Sha256Digest,
};
use winwincode_storage::{
    NewOutboxEvent, ProductStateStorage, ProjectionEventStream, PublicEventSource, ReceiptIdentity,
    SqliteStorage, StateCommit, StorageError,
};

use crate::credential_leak_gate::{
    CredentialLeakError, CredentialLeakGate, CredentialOutputBoundary,
};
use crate::credential_reference::{CredentialReferenceErrorKind, CredentialReferenceService};
use crate::model_request_pool::{ModelRequestPool, ModelRequestPoolConfig, ModelRequestRouteKey};
use crate::model_settings::{
    ModelSelection, ModelSettingsError, ModelSettingsService, ModelSettingsTarget,
};
use crate::provider_catalog::{
    CatalogAvailability, ModelToolSupport as CatalogModelToolSupport, ProviderCatalogError,
    ProviderCatalogService,
};
use crate::{
    public_event_actor, public_event_scope, receipt_actor_key, receipt_scope_key,
    repository_scope_key,
};

const CURSOR_VERSION: u8 = 1;
pub(crate) const MODEL_ROUTE_AVAILABILITY_INVALIDATED_TOPIC: &str =
    "model-route-availability.invalidated.v1";
const INVALIDATION_EVENT_COMPONENT: &str = "model-route-availability";
const POOL_READINESS_STREAM_PREFIX: &str = "model-request-pool-readiness:";
const RUNTIME_STATUS_SCHEMA: &str = "winwincode.model-route-runtime-status.v1";
const RUNTIME_STATUS_STREAM_PREFIX: &str = "model-route-runtime-status:";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Trusted runtime observation for one exact repository route.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelRouteRuntimeStatusCommand {
    pub actor: Actor,
    pub scope: RepositoryScope,
    pub route: ModelRoute,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub status: ModelRouteAvailabilityStatus,
    pub source: String,
    pub source_revision: u64,
}

/// Durable result of one runtime-status update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRouteRuntimeStatusReceipt {
    pub revision: u64,
    pub status: ModelRouteAvailabilityStatus,
    pub idempotent_replay: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelRouteRuntimeStatusState {
    schema: String,
    scope: RepositoryScope,
    route: ModelRoute,
    status: ModelRouteAvailabilityStatus,
    source: String,
    source_revision: u64,
    observed_at: Instant,
    revision: u64,
}

pub(crate) fn model_request_pool_readiness_stream_id(
    project_scope: &ProjectScope,
) -> Result<String, StorageError> {
    let scope = Scope::ProjectScope(project_scope.clone());
    receipt_scope_key(&scope)?;
    let scope_json = serde_json::to_vec(project_scope).map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode ModelRoute request-pool scope: {error}"
        ))
    })?;
    let mut digest = Sha256::new();
    digest.update(b"winwincode.model-request-pool-readiness-stream.v1\0");
    digest.update(scope_json);
    Ok(format!(
        "{POOL_READINESS_STREAM_PREFIX}{:x}",
        digest.finalize()
    ))
}

/// Builds the canonical secret-free scope-stream invalidation beside an
/// authority mutation. Callers append this value to the same storage commit as
/// the source revision so a crash can never expose one without the other.
pub(crate) fn model_route_availability_invalidated_event(
    actor: &Actor,
    scope: &Scope,
    source: ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource,
    source_revision: u64,
    occurred_at: Instant,
    event_discriminator: &[u8],
) -> Result<NewOutboxEvent, StorageError> {
    let occurred_at = if occurred_at.0.len() == 20 && occurred_at.0.ends_with('Z') {
        Instant(format!("{}.000Z", &occurred_at.0[..19]))
    } else {
        occurred_at
    };
    let source_revision = i64::try_from(source_revision)
        .map(Revision)
        .map_err(|_| StorageError::invalid_input("ModelRoute source revision is invalid"))?;
    let event = ControlPlaneWebSocketModelRouteAvailabilityInvalidatedEvent {
        reload_queries: (
            ControlPlaneWebSocketModelRouteAvailabilityListReloadQuery::
                ModelRouteAvailabilityList,
        ),
        source,
        source_revision,
        type_value: ControlPlaneWebSocketModelRouteAvailabilityInvalidatedEventTypeValue::
            ModelRouteAvailabilityInvalidatedV1,
    };
    let payload = serde_json::to_vec(&event).map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode ModelRoute availability invalidation: {error}"
        ))
    })?;
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Event, &payload)
        .map_err(|_| StorageError::invalid_input("ModelRoute invalidation contains a secret"))?;
    let public_scope = public_event_scope(scope);
    let scope_payload = serde_json::to_vec(&public_scope).map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode ModelRoute invalidation scope: {error}"
        ))
    })?;
    let mut digest = Sha256::new();
    digest.update(b"winwincode.model-route-availability-invalidation.v1\0");
    digest.update(&scope_payload);
    digest.update([0]);
    digest.update(&payload);
    digest.update([0]);
    digest.update(event_discriminator);
    let event_id = ControlPlaneEventId(format!("evt_{:x}", digest.finalize()));
    NewOutboxEvent::public_projection(
        event_id,
        MODEL_ROUTE_AVAILABILITY_INVALIDATED_TOPIC,
        payload,
        ProjectionEventStream::Scope,
        public_scope,
        occurred_at,
        PublicEventSource::ControlPlane {
            actor: public_event_actor(actor),
            component: INVALIDATION_EVENT_COMPONENT.to_owned(),
        },
    )
}

/// Stable public-query failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRouteAvailabilityErrorKind {
    InvalidRequest,
    ScopeDenied,
    CredentialLeak,
    Storage,
}

/// Bounded failure that never copies Catalog, Credential, or pool details.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRouteAvailabilityError {
    kind: ModelRouteAvailabilityErrorKind,
    message: &'static str,
}

impl ModelRouteAvailabilityError {
    const fn new(kind: ModelRouteAvailabilityErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            ModelRouteAvailabilityErrorKind::InvalidRequest,
            "ModelRoute availability request is invalid",
        )
    }

    const fn scope_denied() -> Self {
        Self::new(
            ModelRouteAvailabilityErrorKind::ScopeDenied,
            "ModelRoute availability scope is not authorized",
        )
    }

    const fn storage() -> Self {
        Self::new(
            ModelRouteAvailabilityErrorKind::Storage,
            "ModelRoute availability authority is unavailable",
        )
    }

    /// Returns the stable public-query error category.
    #[must_use]
    pub const fn kind(&self) -> ModelRouteAvailabilityErrorKind {
        self.kind
    }
}

impl fmt::Display for ModelRouteAvailabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ModelRouteAvailabilityError {}

impl From<CredentialLeakError> for ModelRouteAvailabilityError {
    fn from(_error: CredentialLeakError) -> Self {
        Self::new(
            ModelRouteAvailabilityErrorKind::CredentialLeak,
            "ModelRoute availability output was rejected by the Credential leak gate",
        )
    }
}

impl From<ModelSettingsError> for ModelRouteAvailabilityError {
    fn from(_error: ModelSettingsError) -> Self {
        Self::storage()
    }
}

impl From<ProviderCatalogError> for ModelRouteAvailabilityError {
    fn from(_error: ProviderCatalogError) -> Self {
        Self::storage()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CursorBinding<'query> {
    actor: &'query Actor,
    scope: &'query RepositoryScope,
    snapshot_sha256: &'query str,
    page_limit: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PageCursor {
    version: u8,
    binding_sha256: String,
    next_index: u64,
}

/// Exact-scope query service backed by the one local product-state authority.
pub struct ModelRouteAvailabilityService<'storage> {
    storage: &'storage mut SqliteStorage,
    pool_config: Option<ModelRequestPoolConfig>,
}

impl<'storage> ModelRouteAvailabilityService<'storage> {
    #[must_use]
    pub const fn new(
        storage: &'storage mut SqliteStorage,
        pool_config: Option<ModelRequestPoolConfig>,
    ) -> Self {
        Self {
            storage,
            pool_config,
        }
    }

    /// Persists one explicit Provider/runtime observation. Quota states are
    /// accepted only as reported facts; this service never estimates them.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, conflicting, or unpersistable facts.
    pub fn record_runtime_status(
        &mut self,
        command: &ModelRouteRuntimeStatusCommand,
        observed_at: Instant,
    ) -> Result<ModelRouteRuntimeStatusReceipt, ModelRouteAvailabilityError> {
        validate_runtime_status_command(command)?;
        validate_instant(&observed_at)?;
        let scope = Scope::RepositoryScope(command.scope.clone());
        let identity = ReceiptIdentity::new(
            receipt_actor_key(&command.actor)
                .map_err(|_| ModelRouteAvailabilityError::invalid())?,
            receipt_scope_key(&scope).map_err(|_| ModelRouteAvailabilityError::scope_denied())?,
            command.request_id.clone(),
        )
        .map_err(|_| ModelRouteAvailabilityError::invalid())?;
        let command_bytes =
            serde_json::to_vec(command).map_err(|_| ModelRouteAvailabilityError::invalid())?;
        let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&command_bytes)));
        if let Some(receipt) = self
            .storage
            .load_receipt(&identity, &digest)
            .map_err(|_| ModelRouteAvailabilityError::storage())?
        {
            return Ok(ModelRouteRuntimeStatusReceipt {
                revision: receipt.revision,
                status: command.status.clone(),
                idempotent_replay: true,
            });
        }
        let revision = command
            .expected_revision
            .checked_add(1)
            .filter(|revision| *revision <= MAX_SAFE_INTEGER)
            .ok_or_else(ModelRouteAvailabilityError::invalid)?;
        let state = ModelRouteRuntimeStatusState {
            schema: RUNTIME_STATUS_SCHEMA.to_owned(),
            scope: command.scope.clone(),
            route: command.route.clone(),
            status: command.status.clone(),
            source: command.source.clone(),
            source_revision: command.source_revision,
            observed_at: observed_at.clone(),
            revision,
        };
        let state_payload =
            serde_json::to_vec(&state).map_err(|_| ModelRouteAvailabilityError::invalid())?;
        CredentialLeakGate::default()
            .inspect_json_bytes(CredentialOutputBoundary::Persistence, &state_payload)?;
        let invalidation = model_route_availability_invalidated_event(
            &command.actor,
            &scope,
            ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource::RuntimeStatus,
            revision,
            observed_at,
            command.request_id.0.as_bytes(),
        )
        .map_err(|_| ModelRouteAvailabilityError::storage())?;
        let receipt = self
            .storage
            .commit(&StateCommit::new(
                identity,
                digest,
                runtime_status_stream_id(&command.scope, &command.route)?,
                command.expected_revision,
                state_payload,
                vec![invalidation],
            ))
            .map_err(|_| ModelRouteAvailabilityError::storage())?;
        Ok(ModelRouteRuntimeStatusReceipt {
            revision: receipt.revision,
            status: command.status.clone(),
            idempotent_replay: receipt.idempotent_replay,
        })
    }

    /// Returns one stable page of server-joined `ModelRoute` values.
    ///
    /// # Errors
    ///
    /// Rejects malformed/cross-scope input, changed page cursors, corrupt
    /// canonical sources, or output rejected by the Credential leak gate.
    pub fn list(
        &mut self,
        query: &ModelRouteAvailabilityListQuery,
    ) -> Result<ModelRouteAvailabilityListResultResponse, ModelRouteAvailabilityError> {
        validate_query(query)?;
        let target = ModelSettingsTarget::Repository {
            scope: query.scope.clone(),
        };
        let effective =
            ModelSettingsService::new(self.storage).effective_selection_source(&target)?;
        let catalog_scopes = ModelSettingsService::effective_catalog_scopes(&target)?;
        let request_pool_source = project_scope(&query.scope);
        let request_pool_stream_id = model_request_pool_readiness_stream_id(&request_pool_source)
            .map_err(|_| ModelRouteAvailabilityError::storage())?;
        let request_pool_revision = self
            .storage
            .load_state(&request_pool_stream_id)
            .map_err(|_| ModelRouteAvailabilityError::storage())?
            .map_or(0, |state| state.revision);
        let pool = self.load_pool()?;
        let selection = effective.as_ref().map(|(selection, _, _)| selection);
        let candidates =
            self.collect_candidates(query, selection, catalog_scopes, pool.as_ref())?;
        let response = build_response(
            query,
            &candidates,
            effective,
            request_pool_source,
            request_pool_revision,
        )?;
        CredentialLeakGate::default()
            .inspect_serializable(CredentialOutputBoundary::Http, &response)?;
        Ok(response)
    }

    fn collect_candidates(
        &mut self,
        query: &ModelRouteAvailabilityListQuery,
        selection: Option<&ModelSelection>,
        catalog_scopes: Vec<Scope>,
        pool: Option<&ModelRequestPool>,
    ) -> Result<Vec<ModelRouteAvailabilityProjection>, ModelRouteAvailabilityError> {
        let mut providers = BTreeSet::new();
        let mut candidates = Vec::new();
        for catalog_scope in catalog_scopes {
            let catalog = ProviderCatalogService::new(self.storage).project(&catalog_scope)?;
            for provider in catalog.providers {
                if !providers.insert(provider.provider_id.clone()) {
                    continue;
                }
                for model in provider.models {
                    let route = ModelRoute {
                        provider_id: provider.provider_id.clone(),
                        model_id: model.model_id.clone(),
                        credential_reference_id: provider.credential_reference_id.clone(),
                    };
                    let is_default = selection.is_some_and(|selection| {
                        selection.provider_id == route.provider_id
                            && selection.model_id == route.model_id
                    });
                    let (credential_ready, credential_rotation_version) =
                        self.credential_readiness(&catalog_scope, &route)?;
                    let pool_ready = pool.as_ref().is_some_and(|pool| {
                        ModelRequestRouteKey::from_repository_scope(&query.scope, &route)
                            .is_ok_and(|route_key| pool.is_route_ready(&route_key))
                    });
                    let runtime_status = self.runtime_status(&query.scope, &route)?;
                    let (status, reason) = candidate_status(
                        provider.availability,
                        model.availability,
                        credential_ready,
                        pool_ready,
                        runtime_status,
                    );
                    candidates.push(ModelRouteAvailabilityProjection {
                        catalog_source: catalog_scope.clone(),
                        catalog_version: revision(catalog.catalog_version)?,
                        context_window_tokens: to_i64(model.context_window_tokens)?,
                        credential_rotation_version,
                        is_default,
                        max_output_tokens: to_i64(model.max_output_tokens)?,
                        model_display_name: model.display_name,
                        model_version: revision(model.version)?,
                        provider_display_name: provider.display_name.clone(),
                        provider_version: revision(provider.version)?,
                        reason,
                        reasoning_efforts: model.reasoning_efforts,
                        route,
                        status,
                        tool_support: tool_support(model.tool_support),
                    });
                }
            }
        }
        candidates.sort_by(|left, right| {
            left.route
                .provider_id
                .cmp(&right.route.provider_id)
                .then_with(|| left.route.model_id.cmp(&right.route.model_id))
                .then_with(|| {
                    left.route
                        .credential_reference_id
                        .0
                        .cmp(&right.route.credential_reference_id.0)
                })
        });
        Ok(candidates)
    }

    fn credential_readiness(
        &mut self,
        catalog_scope: &Scope,
        route: &ModelRoute,
    ) -> Result<(bool, Option<i64>), ModelRouteAvailabilityError> {
        let credential = CredentialReferenceService::new(self.storage)
            .resolve(catalog_scope, &route.credential_reference_id);
        match credential {
            Ok(credential) if credential.provider_id() == route.provider_id => {
                Ok((true, Some(to_i64(credential.rotation_version())?)))
            }
            Ok(_) => Ok((false, None)),
            Err(error)
                if matches!(
                    error.kind(),
                    CredentialReferenceErrorKind::ScopeDenied
                        | CredentialReferenceErrorKind::NotFound
                        | CredentialReferenceErrorKind::Revoked
                        | CredentialReferenceErrorKind::WrongState
                ) =>
            {
                Ok((false, None))
            }
            Err(_) => Err(ModelRouteAvailabilityError::storage()),
        }
    }

    fn load_pool(&mut self) -> Result<Option<ModelRequestPool>, ModelRouteAvailabilityError> {
        let Some(config) = self.pool_config else {
            return Ok(None);
        };
        let mut pool =
            ModelRequestPool::new(config).map_err(|_| ModelRouteAvailabilityError::storage())?;
        let authority = self
            .storage
            .provider_exchange_store()
            .map_err(|_| ModelRouteAvailabilityError::storage())?
            .load_pool_authority()
            .map_err(|_| ModelRouteAvailabilityError::storage())?;
        if let Some(authority) = authority {
            pool.restore_authority(authority.state_json())
                .map_err(|_| ModelRouteAvailabilityError::storage())?;
        }
        Ok(Some(pool))
    }

    fn runtime_status(
        &self,
        scope: &RepositoryScope,
        route: &ModelRoute,
    ) -> Result<Option<ModelRouteAvailabilityStatus>, ModelRouteAvailabilityError> {
        let stream_id = runtime_status_stream_id(scope, route)?;
        let Some(stored) = self
            .storage
            .load_state(&stream_id)
            .map_err(|_| ModelRouteAvailabilityError::storage())?
        else {
            return Ok(None);
        };
        let state: ModelRouteRuntimeStatusState = serde_json::from_slice(&stored.payload)
            .map_err(|_| ModelRouteAvailabilityError::storage())?;
        if serde_json::to_vec(&state).map_err(|_| ModelRouteAvailabilityError::storage())?
            != stored.payload
            || state.schema != RUNTIME_STATUS_SCHEMA
            || state.scope != *scope
            || state.route != *route
            || state.revision != stored.revision
            || state.source_revision == 0
            || state.source_revision > MAX_SAFE_INTEGER
            || state.source.trim().is_empty()
            || state.source.chars().count() > 200
            || state.source.chars().any(char::is_control)
            || validate_instant(&state.observed_at).is_err()
        {
            return Err(ModelRouteAvailabilityError::storage());
        }
        Ok(Some(state.status))
    }
}

fn build_response(
    query: &ModelRouteAvailabilityListQuery,
    all_items: &[ModelRouteAvailabilityProjection],
    effective: Option<(ModelSelection, Scope, u64)>,
    request_pool_source: ProjectScope,
    request_pool_revision: u64,
) -> Result<ModelRouteAvailabilityListResultResponse, ModelRouteAvailabilityError> {
    let (settings_source, settings_revision, selection) = match effective {
        Some((selection, source, revision_value)) => (
            Some(source),
            Some(revision(revision_value)?),
            Some(selection),
        ),
        None => (None, None, None),
    };
    let (status, reason) = page_status(all_items, selection.as_ref());
    let snapshot_digest = snapshot_sha256(&(
        &query.scope,
        &settings_source,
        &settings_revision,
        &selection,
        &request_pool_source,
        request_pool_revision,
        status.clone(),
        reason.clone(),
        &all_items,
    ))?;
    let binding_sha256 = snapshot_sha256(&CursorBinding {
        actor: &query.actor,
        scope: &query.scope,
        snapshot_sha256: &snapshot_digest,
        page_limit: query.page.limit,
    })?;
    let start = decode_cursor(query.page.cursor.as_ref(), &binding_sha256)?;
    if start > all_items.len() {
        return Err(ModelRouteAvailabilityError::invalid());
    }
    let limit =
        usize::try_from(query.page.limit).map_err(|_| ModelRouteAvailabilityError::invalid())?;
    let end = start.saturating_add(limit).min(all_items.len());
    let next_cursor = (end < all_items.len())
        .then(|| encode_cursor(&binding_sha256, end))
        .transpose()?;
    let default_provider_id = selection
        .as_ref()
        .map(|selection| selection.provider_id.clone());
    let default_model_id = selection.map(|selection| selection.model_id);
    let response = ModelRouteAvailabilityListResultResponse {
        page: PageInfo {
            has_more: next_cursor.is_some(),
            next_cursor,
        },
        query: ModelRouteAvailabilityListResultResponseQuery::ModelRouteAvailabilityList,
        request_id: query.request_id.clone(),
        result: ModelRouteAvailabilityPage {
            default_model_id,
            default_provider_id,
            items: all_items[start..end].to_vec(),
            kind: ModelRouteAvailabilityPageKind::ModelRouteAvailabilityPage,
            reason,
            request_pool_revision: revision(request_pool_revision)?,
            request_pool_source,
            scope: query.scope.clone(),
            settings_revision,
            settings_source,
            status,
        },
        schema_version: SchemaVersion::WinwincodeV1,
    };
    Ok(response)
}

fn project_scope(repository: &RepositoryScope) -> ProjectScope {
    ProjectScope {
        kind: ProjectScopeKind::Project,
        organization_id: repository.organization_id.clone(),
        workspace_id: repository.workspace_id.clone(),
        project_id: repository.project_id.clone(),
    }
}

fn validate_query(
    query: &ModelRouteAvailabilityListQuery,
) -> Result<(), ModelRouteAvailabilityError> {
    receipt_actor_key(&query.actor).map_err(|_| ModelRouteAvailabilityError::invalid())?;
    repository_scope_key(&query.scope).map_err(|_| ModelRouteAvailabilityError::scope_denied())?;
    if !query.request_id.0.starts_with("req_")
        || query.request_id.0.len() > 200
        || query
            .request_id
            .0
            .bytes()
            .any(|byte| byte.is_ascii_control())
        || !(1..=200).contains(&query.page.limit)
    {
        return Err(ModelRouteAvailabilityError::invalid());
    }
    Ok(())
}

fn validate_runtime_status_command(
    command: &ModelRouteRuntimeStatusCommand,
) -> Result<(), ModelRouteAvailabilityError> {
    receipt_actor_key(&command.actor).map_err(|_| ModelRouteAvailabilityError::invalid())?;
    repository_scope_key(&command.scope)
        .map_err(|_| ModelRouteAvailabilityError::scope_denied())?;
    ModelRequestRouteKey::from_repository_scope(&command.scope, &command.route)
        .map_err(|_| ModelRouteAvailabilityError::invalid())?;
    if !command.request_id.0.starts_with("req_")
        || command.request_id.0.len() > 200
        || command
            .request_id
            .0
            .bytes()
            .any(|byte| byte.is_ascii_control())
        || command.expected_revision > MAX_SAFE_INTEGER
        || command.source_revision == 0
        || command.source_revision > MAX_SAFE_INTEGER
        || command.source.trim().is_empty()
        || command.source.chars().count() > 200
        || command.source.chars().any(char::is_control)
    {
        return Err(ModelRouteAvailabilityError::invalid());
    }
    Ok(())
}

fn validate_instant(value: &Instant) -> Result<(), ModelRouteAvailabilityError> {
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
        Err(ModelRouteAvailabilityError::invalid())
    }
}

fn runtime_status_stream_id(
    scope: &RepositoryScope,
    route: &ModelRoute,
) -> Result<String, ModelRouteAvailabilityError> {
    ModelRequestRouteKey::from_repository_scope(scope, route)
        .map_err(|_| ModelRouteAvailabilityError::invalid())?;
    let bytes =
        serde_json::to_vec(&(scope, route)).map_err(|_| ModelRouteAvailabilityError::invalid())?;
    Ok(format!(
        "{RUNTIME_STATUS_STREAM_PREFIX}{:x}",
        Sha256::digest(bytes)
    ))
}

fn candidate_status(
    provider: CatalogAvailability,
    model: CatalogAvailability,
    credential_ready: bool,
    pool_ready: bool,
    runtime_status: Option<ModelRouteAvailabilityStatus>,
) -> (ModelRouteAvailabilityStatus, ModelRouteAvailabilityReason) {
    if provider == CatalogAvailability::Disabled || model == CatalogAvailability::Disabled {
        return (
            ModelRouteAvailabilityStatus::Disabled,
            ModelRouteAvailabilityReason::ProviderOrModelDisabled,
        );
    }
    if !credential_ready {
        return (
            ModelRouteAvailabilityStatus::AuthError,
            ModelRouteAvailabilityReason::CredentialMissingOrRevoked,
        );
    }
    if let Some(status) = runtime_status
        && status != ModelRouteAvailabilityStatus::Available
    {
        let reason = runtime_reason(&status);
        return (status, reason);
    }
    if !pool_ready {
        return (
            ModelRouteAvailabilityStatus::Unknown,
            ModelRouteAvailabilityReason::RequestPoolUnavailable,
        );
    }
    (
        ModelRouteAvailabilityStatus::Available,
        ModelRouteAvailabilityReason::Ready,
    )
}

fn runtime_reason(status: &ModelRouteAvailabilityStatus) -> ModelRouteAvailabilityReason {
    match status {
        ModelRouteAvailabilityStatus::Available => ModelRouteAvailabilityReason::Ready,
        ModelRouteAvailabilityStatus::RateLimited => ModelRouteAvailabilityReason::RateLimited,
        ModelRouteAvailabilityStatus::WindowExhausted => {
            ModelRouteAvailabilityReason::WindowExhausted
        }
        ModelRouteAvailabilityStatus::WeeklyExhausted => {
            ModelRouteAvailabilityReason::WeeklyExhausted
        }
        ModelRouteAvailabilityStatus::AuthError => {
            ModelRouteAvailabilityReason::AuthenticationError
        }
        ModelRouteAvailabilityStatus::Disabled => {
            ModelRouteAvailabilityReason::ProviderOrModelDisabled
        }
        ModelRouteAvailabilityStatus::Unknown => ModelRouteAvailabilityReason::RuntimeStatusUnknown,
    }
}

fn page_status(
    items: &[ModelRouteAvailabilityProjection],
    selection: Option<&ModelSelection>,
) -> (ModelRouteAvailabilityStatus, ModelRouteAvailabilityReason) {
    if items.is_empty() {
        return (
            ModelRouteAvailabilityStatus::Disabled,
            ModelRouteAvailabilityReason::NoProvider,
        );
    }
    let Some(selection) = selection else {
        return (
            ModelRouteAvailabilityStatus::Disabled,
            ModelRouteAvailabilityReason::DefaultRouteInvalid,
        );
    };
    items
        .iter()
        .find(|item| {
            item.route.provider_id == selection.provider_id
                && item.route.model_id == selection.model_id
        })
        .map_or(
            (
                ModelRouteAvailabilityStatus::Disabled,
                ModelRouteAvailabilityReason::DefaultRouteInvalid,
            ),
            |item| (item.status.clone(), item.reason.clone()),
        )
}

const fn tool_support(source: CatalogModelToolSupport) -> ModelRouteToolSupport {
    match source {
        CatalogModelToolSupport::Unsupported => ModelRouteToolSupport::Unsupported,
        CatalogModelToolSupport::Serial => ModelRouteToolSupport::Serial,
        CatalogModelToolSupport::Parallel => ModelRouteToolSupport::Parallel,
    }
}

fn revision(value: u64) -> Result<Revision, ModelRouteAvailabilityError> {
    Ok(Revision(to_i64(value)?))
}

fn to_i64(value: u64) -> Result<i64, ModelRouteAvailabilityError> {
    i64::try_from(value).map_err(|_| ModelRouteAvailabilityError::storage())
}

fn snapshot_sha256(value: &impl Serialize) -> Result<String, ModelRouteAvailabilityError> {
    let bytes = serde_json::to_vec(value).map_err(|_| ModelRouteAvailabilityError::storage())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn encode_cursor(
    binding_sha256: &str,
    next_index: usize,
) -> Result<OpaqueCursor, ModelRouteAvailabilityError> {
    let bytes = serde_json::to_vec(&PageCursor {
        version: CURSOR_VERSION,
        binding_sha256: binding_sha256.to_owned(),
        next_index: u64::try_from(next_index)
            .map_err(|_| ModelRouteAvailabilityError::invalid())?,
    })
    .map_err(|_| ModelRouteAvailabilityError::invalid())?;
    let encoded = URL_SAFE_NO_PAD.encode(bytes);
    if encoded.len() > 2_048 {
        return Err(ModelRouteAvailabilityError::invalid());
    }
    Ok(OpaqueCursor(encoded))
}

fn decode_cursor(
    cursor: Option<&OpaqueCursor>,
    binding_sha256: &str,
) -> Result<usize, ModelRouteAvailabilityError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(&cursor.0)
        .map_err(|_| ModelRouteAvailabilityError::invalid())?;
    let decoded: PageCursor =
        serde_json::from_slice(&bytes).map_err(|_| ModelRouteAvailabilityError::invalid())?;
    if decoded.version != CURSOR_VERSION || decoded.binding_sha256 != binding_sha256 {
        return Err(ModelRouteAvailabilityError::invalid());
    }
    usize::try_from(decoded.next_index).map_err(|_| ModelRouteAvailabilityError::invalid())
}
