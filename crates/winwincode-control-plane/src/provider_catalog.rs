// SPDX-License-Identifier: Apache-2.0

//! Durable, scope-bound Provider and model capability catalog.
//!
//! The catalog owns only stable adapter descriptions, capability metadata,
//! availability, versions, and [`CredentialReferenceId`] values. It never
//! accepts a vault locator or resolves secret material.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource, Scope,
};
use winwincode_domain::{CredentialReferenceId, Instant, RequestId, Sha256Digest};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptIdentity, StateCommit, StorageError,
    StorageErrorKind, StoredState,
};

use crate::credential_leak_gate::{
    CredentialLeakError, CredentialLeakGate, CredentialOutputBoundary,
};
use crate::{
    model_route_availability::model_route_availability_invalidated_event, receipt_actor_key,
    receipt_scope_key,
};

const STATE_SCHEMA: &str = "winwincode.provider-catalog.v1";
const STREAM_PREFIX: &str = "provider-catalog:";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Durable topic emitted once for every accepted catalog version change.
pub const PROVIDER_CATALOG_VERSION_EVENT_TOPIC: &str = "provider.catalog.version.v1";

/// Stable failure categories for Provider catalog commands and queries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderCatalogErrorKind {
    InvalidRequest,
    ScopeDenied,
    ProviderNotFound,
    ProviderDisabled,
    ModelNotFound,
    ModelDisabled,
    AlreadyDisabled,
    VersionConflict,
    RequestConflict,
    CredentialLeak,
    Storage,
}

/// Bounded Provider catalog error that never copies descriptor input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCatalogError {
    kind: ProviderCatalogErrorKind,
    message: &'static str,
}

impl ProviderCatalogError {
    const fn new(kind: ProviderCatalogErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            ProviderCatalogErrorKind::InvalidRequest,
            "Provider catalog request is invalid",
        )
    }

    const fn scope_denied() -> Self {
        Self::new(
            ProviderCatalogErrorKind::ScopeDenied,
            "Provider catalog state belongs to another scope",
        )
    }

    const fn provider_not_found() -> Self {
        Self::new(
            ProviderCatalogErrorKind::ProviderNotFound,
            "Provider was not found in this scope",
        )
    }

    const fn provider_disabled() -> Self {
        Self::new(
            ProviderCatalogErrorKind::ProviderDisabled,
            "Provider is disabled in this scope",
        )
    }

    const fn model_not_found() -> Self {
        Self::new(
            ProviderCatalogErrorKind::ModelNotFound,
            "Model was not found for this Provider",
        )
    }

    const fn model_disabled() -> Self {
        Self::new(
            ProviderCatalogErrorKind::ModelDisabled,
            "Model is disabled for this Provider",
        )
    }

    const fn already_disabled() -> Self {
        Self::new(
            ProviderCatalogErrorKind::AlreadyDisabled,
            "Provider is already disabled",
        )
    }

    const fn version_conflict() -> Self {
        Self::new(
            ProviderCatalogErrorKind::VersionConflict,
            "Provider catalog version does not match",
        )
    }

    const fn credential_leak() -> Self {
        Self::new(
            ProviderCatalogErrorKind::CredentialLeak,
            "Provider catalog output was rejected by the Credential leak gate",
        )
    }

    /// Returns the stable machine-readable failure category.
    #[must_use]
    pub const fn kind(&self) -> ProviderCatalogErrorKind {
        self.kind
    }
}

impl fmt::Display for ProviderCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProviderCatalogError {}

impl From<CredentialLeakError> for ProviderCatalogError {
    fn from(_error: CredentialLeakError) -> Self {
        Self::credential_leak()
    }
}

impl From<StorageError> for ProviderCatalogError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::RevisionConflict => Self::version_conflict(),
            StorageErrorKind::RequestConflict => Self::new(
                ProviderCatalogErrorKind::RequestConflict,
                "Provider catalog requestId was reused with different input",
            ),
            StorageErrorKind::InvalidInput | StorageErrorKind::RequestReplayMissing => {
                Self::invalid()
            }
            StorageErrorKind::EventCursorExpired
            | StorageErrorKind::JournalAlreadyExists
            | StorageErrorKind::JournalNotFound
            | StorageErrorKind::JournalConflict
            | StorageErrorKind::Adapter
            | StorageErrorKind::Closed => Self::new(
                ProviderCatalogErrorKind::Storage,
                "Provider catalog storage operation failed",
            ),
        }
    }
}

/// Tool-call behavior supported by one model.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelToolSupport {
    Unsupported,
    Serial,
    Parallel,
}

/// Stable capabilities and limits discovered for one model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCapability {
    pub model_id: String,
    pub display_name: String,
    pub context_window_tokens: u64,
    pub max_output_tokens: u64,
    pub tool_support: ModelToolSupport,
    /// Empty means reasoning controls are unsupported.
    pub reasoning_efforts: Vec<String>,
}

/// Stable Provider adapter registration accepted by the catalog.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderDescriptor {
    pub provider_id: String,
    pub display_name: String,
    /// Stable adapter implementation identifier, never adapter configuration.
    pub adapter_kind: String,
    pub credential_reference_id: CredentialReferenceId,
    /// Complete currently-discovered enabled model snapshot.
    pub models: Vec<ModelCapability>,
}

/// Common request identity and optimistic catalog version for a mutation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCatalogRequest {
    pub actor: Actor,
    pub scope: Scope,
    pub request_id: RequestId,
    pub expected_catalog_version: u64,
}

/// Availability shown in Provider and model projections.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogAvailability {
    Enabled,
    Disabled,
}

/// Secret-free versioned model capability projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCapabilityProjection {
    pub model_id: String,
    pub display_name: String,
    pub context_window_tokens: u64,
    pub max_output_tokens: u64,
    pub tool_support: ModelToolSupport,
    pub reasoning_efforts: Vec<String>,
    pub availability: CatalogAvailability,
    pub version: u64,
}

/// Secret-free versioned Provider projection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCatalogEntryProjection {
    pub provider_id: String,
    pub display_name: String,
    pub adapter_kind: String,
    pub credential_reference_id: CredentialReferenceId,
    pub availability: CatalogAvailability,
    pub version: u64,
    pub models: Vec<ModelCapabilityProjection>,
}

/// Complete exact-scope Provider catalog projection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCatalogProjection {
    pub scope: Scope,
    pub catalog_version: u64,
    pub providers: Vec<ProviderCatalogEntryProjection>,
}

/// Enabled model resolution returned to the later route-selection layer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolvedModelCapability {
    pub scope: Scope,
    pub catalog_version: u64,
    pub provider_id: String,
    pub provider_version: u64,
    pub credential_reference_id: CredentialReferenceId,
    pub model: ModelCapabilityProjection,
}

/// Catalog change represented by one version event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCatalogChange {
    Upserted,
    Disabled,
}

/// Version/status tuple included in a catalog version event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCatalogVersion {
    pub model_id: String,
    pub version: u64,
    pub availability: CatalogAvailability,
}

/// Durable event emitted for every accepted Provider catalog mutation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCatalogVersionEvent {
    pub scope: Scope,
    pub change: ProviderCatalogChange,
    pub previous_catalog_version: u64,
    pub catalog_version: u64,
    pub provider_id: String,
    pub provider_version: u64,
    pub provider_availability: CatalogAvailability,
    pub models: Vec<ModelCatalogVersion>,
}

/// Durable result of one Provider catalog version change.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCatalogMutationReceipt {
    pub request_id: RequestId,
    pub change: ProviderCatalogChange,
    pub previous_catalog_version: u64,
    pub catalog_version: u64,
    pub provider_id: String,
    pub provider_version: u64,
    pub idempotent_replay: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderCatalogState {
    schema: String,
    scope: Scope,
    catalog_version: u64,
    providers: BTreeMap<String, ProviderRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderRecord {
    provider_id: String,
    display_name: String,
    adapter_kind: String,
    credential_reference_id: CredentialReferenceId,
    availability: CatalogAvailability,
    version: u64,
    models: BTreeMap<String, ModelRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelRecord {
    model_id: String,
    display_name: String,
    context_window_tokens: u64,
    max_output_tokens: u64,
    tool_support: ModelToolSupport,
    reasoning_efforts: Vec<String>,
    availability: CatalogAvailability,
    version: u64,
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum CatalogCommandDigest<'a> {
    Upsert {
        request: &'a ProviderCatalogRequest,
        descriptor: &'a ProviderDescriptor,
    },
    Disable {
        request: &'a ProviderCatalogRequest,
        provider_id: &'a str,
    },
}

struct CommandReceipt {
    identity: ReceiptIdentity,
    digest: Sha256Digest,
}

/// Application service backed by the shared atomic product-state storage seam.
pub struct ProviderCatalogService<'a> {
    storage: &'a mut dyn ProductStateStorage,
}

impl<'a> ProviderCatalogService<'a> {
    #[must_use]
    pub fn new(storage: &'a mut dyn ProductStateStorage) -> Self {
        Self { storage }
    }

    /// Creates or hot-updates one Provider from a complete adapter descriptor.
    /// Missing models are retained as disabled versioned entries.
    ///
    /// # Errors
    ///
    /// Rejects malformed descriptions, duplicate models, a stale catalog
    /// version, or a conflicting scoped request replay.
    pub fn upsert(
        &mut self,
        request: &ProviderCatalogRequest,
        descriptor: &ProviderDescriptor,
        occurred_at: Instant,
    ) -> Result<ProviderCatalogMutationReceipt, ProviderCatalogError> {
        validate_request(request)?;
        let descriptor = canonical_descriptor(descriptor)?;
        let command = command_receipt(&CatalogCommandDigest::Upsert {
            request,
            descriptor: &descriptor,
        })?;
        if let Some(receipt) = self.replay(request, &command)? {
            return Ok(receipt);
        }

        let mut state = self.load_or_empty(&request.scope)?;
        ensure_version(request.expected_catalog_version, state.catalog_version)?;
        let previous_catalog_version = state.catalog_version;
        let catalog_version = next_version(previous_catalog_version)?;
        let provider =
            upsert_provider(state.providers.remove(&descriptor.provider_id), descriptor)?;
        let event = version_event(
            &state.scope,
            ProviderCatalogChange::Upserted,
            previous_catalog_version,
            catalog_version,
            &provider,
        );
        state.catalog_version = catalog_version;
        state
            .providers
            .insert(provider.provider_id.clone(), provider);
        self.commit(request, command, &state, &event, occurred_at)
    }

    /// Disables one Provider at a precise catalog version.
    ///
    /// # Errors
    ///
    /// Rejects an unknown/already-disabled Provider, stale catalog version, or
    /// conflicting scoped request replay.
    pub fn disable(
        &mut self,
        request: &ProviderCatalogRequest,
        provider_id: &str,
        occurred_at: Instant,
    ) -> Result<ProviderCatalogMutationReceipt, ProviderCatalogError> {
        validate_request(request)?;
        validate_token(provider_id, 128)?;
        let command = command_receipt(&CatalogCommandDigest::Disable {
            request,
            provider_id,
        })?;
        if let Some(receipt) = self.replay(request, &command)? {
            return Ok(receipt);
        }

        let mut state = self.load_or_empty(&request.scope)?;
        ensure_version(request.expected_catalog_version, state.catalog_version)?;
        let provider = state
            .providers
            .get_mut(provider_id)
            .ok_or_else(ProviderCatalogError::provider_not_found)?;
        if provider.availability == CatalogAvailability::Disabled {
            return Err(ProviderCatalogError::already_disabled());
        }
        provider.availability = CatalogAvailability::Disabled;
        provider.version = next_version(provider.version)?;
        let previous_catalog_version = state.catalog_version;
        let catalog_version = next_version(previous_catalog_version)?;
        state.catalog_version = catalog_version;
        let event = version_event(
            &state.scope,
            ProviderCatalogChange::Disabled,
            previous_catalog_version,
            catalog_version,
            provider,
        );
        self.commit(request, command, &state, &event, occurred_at)
    }

    /// Returns the complete sorted projection for exactly one scope.
    /// An unconfigured scope returns version zero and no Providers.
    ///
    /// # Errors
    ///
    /// Rejects a malformed scope or corrupt durable state.
    pub fn project(
        &self,
        scope: &Scope,
    ) -> Result<ProviderCatalogProjection, ProviderCatalogError> {
        receipt_scope_key(scope)?;
        let state = self.load_or_empty(scope)?;
        let projection = projection(&state);
        CredentialLeakGate::default()
            .inspect_serializable(CredentialOutputBoundary::Serialization, &projection)?;
        Ok(projection)
    }

    /// Resolves one enabled Provider/model capability without reading its secret.
    ///
    /// # Errors
    ///
    /// Distinguishes unknown Provider, disabled Provider, unknown model, and
    /// disabled model results.
    pub fn resolve_model(
        &self,
        scope: &Scope,
        provider_id: &str,
        model_id: &str,
    ) -> Result<ResolvedModelCapability, ProviderCatalogError> {
        receipt_scope_key(scope)?;
        validate_token(provider_id, 128)?;
        validate_token(model_id, 200)?;
        let state = self.load_or_empty(scope)?;
        let provider = state
            .providers
            .get(provider_id)
            .ok_or_else(ProviderCatalogError::provider_not_found)?;
        if provider.availability == CatalogAvailability::Disabled {
            return Err(ProviderCatalogError::provider_disabled());
        }
        let model = provider
            .models
            .get(model_id)
            .ok_or_else(ProviderCatalogError::model_not_found)?;
        if model.availability == CatalogAvailability::Disabled {
            return Err(ProviderCatalogError::model_disabled());
        }
        let resolved = ResolvedModelCapability {
            scope: state.scope,
            catalog_version: state.catalog_version,
            provider_id: provider.provider_id.clone(),
            provider_version: provider.version,
            credential_reference_id: provider.credential_reference_id.clone(),
            model: model_projection(model),
        };
        CredentialLeakGate::default()
            .inspect_serializable(CredentialOutputBoundary::Serialization, &resolved)?;
        Ok(resolved)
    }

    fn load_or_empty(&self, scope: &Scope) -> Result<ProviderCatalogState, ProviderCatalogError> {
        let stream_id = catalog_stream_id(scope)?;
        match self.storage.load_state(&stream_id)? {
            Some(stored) => decode_state(&stored, scope),
            None => Ok(ProviderCatalogState {
                schema: STATE_SCHEMA.to_owned(),
                scope: scope.clone(),
                catalog_version: 0,
                providers: BTreeMap::new(),
            }),
        }
    }

    fn replay(
        &self,
        request: &ProviderCatalogRequest,
        command: &CommandReceipt,
    ) -> Result<Option<ProviderCatalogMutationReceipt>, ProviderCatalogError> {
        self.storage
            .load_receipt(&command.identity, &command.digest)?
            .map(|receipt| mutation_receipt(request, &receipt))
            .transpose()
    }

    fn commit(
        &mut self,
        request: &ProviderCatalogRequest,
        command: CommandReceipt,
        state: &ProviderCatalogState,
        event: &ProviderCatalogVersionEvent,
        occurred_at: Instant,
    ) -> Result<ProviderCatalogMutationReceipt, ProviderCatalogError> {
        let state_payload =
            serde_json::to_vec(state).map_err(|_| ProviderCatalogError::invalid())?;
        CredentialLeakGate::default()
            .inspect_json_bytes(CredentialOutputBoundary::Persistence, &state_payload)?;
        let event_payload =
            serde_json::to_vec(event).map_err(|_| ProviderCatalogError::invalid())?;
        CredentialLeakGate::default()
            .inspect_json_bytes(CredentialOutputBoundary::Event, &event_payload)?;
        let event_id = catalog_event_id(request, event)?;
        let invalidation = model_route_availability_invalidated_event(
            &request.actor,
            &request.scope,
            ControlPlaneWebSocketModelRouteAvailabilityInvalidationSource::ProviderCatalog,
            state.catalog_version,
            occurred_at,
            request.request_id.0.as_bytes(),
        )?;
        let commit = StateCommit::new(
            command.identity,
            command.digest,
            catalog_stream_id(&request.scope)?,
            request.expected_catalog_version,
            state_payload,
            vec![
                NewOutboxEvent::internal(
                    event_id,
                    PROVIDER_CATALOG_VERSION_EVENT_TOPIC,
                    event_payload,
                ),
                invalidation,
            ],
        );
        let receipt = self.storage.commit(&commit)?;
        let durable = version_event_from_receipt(&receipt)?;
        if !receipt.idempotent_replay && durable != *event {
            return Err(ProviderCatalogError::invalid());
        }
        mutation_receipt(request, &receipt)
    }
}

fn canonical_descriptor(
    descriptor: &ProviderDescriptor,
) -> Result<ProviderDescriptor, ProviderCatalogError> {
    validate_token(&descriptor.provider_id, 128)?;
    validate_display_name(&descriptor.display_name, 500)?;
    validate_token(&descriptor.adapter_kind, 128)?;
    validate_credential_reference_id(&descriptor.credential_reference_id)?;
    if descriptor.models.is_empty() || descriptor.models.len() > 500 {
        return Err(ProviderCatalogError::invalid());
    }
    let mut models = BTreeMap::new();
    for model in &descriptor.models {
        validate_token(&model.model_id, 200)?;
        validate_display_name(&model.display_name, 500)?;
        if model.context_window_tokens == 0
            || model.context_window_tokens > MAX_SAFE_INTEGER
            || model.max_output_tokens == 0
            || model.max_output_tokens > model.context_window_tokens
        {
            return Err(ProviderCatalogError::invalid());
        }
        if model.reasoning_efforts.len() > 16 {
            return Err(ProviderCatalogError::invalid());
        }
        let mut efforts = BTreeSet::new();
        for effort in &model.reasoning_efforts {
            validate_token(effort, 64)?;
            if !efforts.insert(effort.clone()) {
                return Err(ProviderCatalogError::invalid());
            }
        }
        let canonical = ModelCapability {
            model_id: model.model_id.clone(),
            display_name: model.display_name.clone(),
            context_window_tokens: model.context_window_tokens,
            max_output_tokens: model.max_output_tokens,
            tool_support: model.tool_support,
            reasoning_efforts: efforts.into_iter().collect(),
        };
        if models
            .insert(canonical.model_id.clone(), canonical)
            .is_some()
        {
            return Err(ProviderCatalogError::invalid());
        }
    }
    Ok(ProviderDescriptor {
        provider_id: descriptor.provider_id.clone(),
        display_name: descriptor.display_name.clone(),
        adapter_kind: descriptor.adapter_kind.clone(),
        credential_reference_id: descriptor.credential_reference_id.clone(),
        models: models.into_values().collect(),
    })
}

fn upsert_provider(
    previous: Option<ProviderRecord>,
    descriptor: ProviderDescriptor,
) -> Result<ProviderRecord, ProviderCatalogError> {
    let (provider_version, mut previous_models) = match previous {
        Some(previous) => (next_version(previous.version)?, previous.models),
        None => (1, BTreeMap::new()),
    };
    let mut models = BTreeMap::new();
    for capability in descriptor.models {
        let previous = previous_models.remove(&capability.model_id);
        let version = match &previous {
            Some(previous)
                if previous.availability == CatalogAvailability::Enabled
                    && same_capability(previous, &capability) =>
            {
                previous.version
            }
            Some(previous) => next_version(previous.version)?,
            None => 1,
        };
        models.insert(
            capability.model_id.clone(),
            ModelRecord {
                model_id: capability.model_id,
                display_name: capability.display_name,
                context_window_tokens: capability.context_window_tokens,
                max_output_tokens: capability.max_output_tokens,
                tool_support: capability.tool_support,
                reasoning_efforts: capability.reasoning_efforts,
                availability: CatalogAvailability::Enabled,
                version,
            },
        );
    }
    for (model_id, mut previous) in previous_models {
        if previous.availability == CatalogAvailability::Enabled {
            previous.availability = CatalogAvailability::Disabled;
            previous.version = next_version(previous.version)?;
        }
        models.insert(model_id, previous);
    }
    Ok(ProviderRecord {
        provider_id: descriptor.provider_id,
        display_name: descriptor.display_name,
        adapter_kind: descriptor.adapter_kind,
        credential_reference_id: descriptor.credential_reference_id,
        availability: CatalogAvailability::Enabled,
        version: provider_version,
        models,
    })
}

fn same_capability(previous: &ModelRecord, current: &ModelCapability) -> bool {
    previous.model_id == current.model_id
        && previous.display_name == current.display_name
        && previous.context_window_tokens == current.context_window_tokens
        && previous.max_output_tokens == current.max_output_tokens
        && previous.tool_support == current.tool_support
        && previous.reasoning_efforts == current.reasoning_efforts
}

fn projection(state: &ProviderCatalogState) -> ProviderCatalogProjection {
    ProviderCatalogProjection {
        scope: state.scope.clone(),
        catalog_version: state.catalog_version,
        providers: state.providers.values().map(provider_projection).collect(),
    }
}

fn provider_projection(provider: &ProviderRecord) -> ProviderCatalogEntryProjection {
    ProviderCatalogEntryProjection {
        provider_id: provider.provider_id.clone(),
        display_name: provider.display_name.clone(),
        adapter_kind: provider.adapter_kind.clone(),
        credential_reference_id: provider.credential_reference_id.clone(),
        availability: provider.availability,
        version: provider.version,
        models: provider.models.values().map(model_projection).collect(),
    }
}

fn model_projection(model: &ModelRecord) -> ModelCapabilityProjection {
    ModelCapabilityProjection {
        model_id: model.model_id.clone(),
        display_name: model.display_name.clone(),
        context_window_tokens: model.context_window_tokens,
        max_output_tokens: model.max_output_tokens,
        tool_support: model.tool_support,
        reasoning_efforts: model.reasoning_efforts.clone(),
        availability: model.availability,
        version: model.version,
    }
}

fn version_event(
    scope: &Scope,
    change: ProviderCatalogChange,
    previous_catalog_version: u64,
    catalog_version: u64,
    provider: &ProviderRecord,
) -> ProviderCatalogVersionEvent {
    ProviderCatalogVersionEvent {
        scope: scope.clone(),
        change,
        previous_catalog_version,
        catalog_version,
        provider_id: provider.provider_id.clone(),
        provider_version: provider.version,
        provider_availability: provider.availability,
        models: provider
            .models
            .values()
            .map(|model| ModelCatalogVersion {
                model_id: model.model_id.clone(),
                version: model.version,
                availability: model.availability,
            })
            .collect(),
    }
}

fn command_receipt(
    command: &CatalogCommandDigest<'_>,
) -> Result<CommandReceipt, ProviderCatalogError> {
    let request = match command {
        CatalogCommandDigest::Upsert { request, .. }
        | CatalogCommandDigest::Disable { request, .. } => *request,
    };
    let actor_key = receipt_actor_key(&request.actor)?;
    let scope_key = receipt_scope_key(&request.scope)?;
    validate_prefixed_id(&request.request_id.0, "req_")?;
    let identity = ReceiptIdentity::new(actor_key, scope_key, request.request_id.clone())?;
    let payload = serde_json::to_vec(command).map_err(|_| ProviderCatalogError::invalid())?;
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(payload)));
    Ok(CommandReceipt { identity, digest })
}

fn mutation_receipt(
    request: &ProviderCatalogRequest,
    receipt: &CommitReceipt,
) -> Result<ProviderCatalogMutationReceipt, ProviderCatalogError> {
    let event = version_event_from_receipt(receipt)?;
    if event.scope != request.scope {
        return Err(ProviderCatalogError::scope_denied());
    }
    let result = ProviderCatalogMutationReceipt {
        request_id: request.request_id.clone(),
        change: event.change,
        previous_catalog_version: event.previous_catalog_version,
        catalog_version: event.catalog_version,
        provider_id: event.provider_id,
        provider_version: event.provider_version,
        idempotent_replay: receipt.idempotent_replay,
    };
    CredentialLeakGate::default()
        .inspect_serializable(CredentialOutputBoundary::Serialization, &result)?;
    Ok(result)
}

fn version_event_from_receipt(
    receipt: &CommitReceipt,
) -> Result<ProviderCatalogVersionEvent, ProviderCatalogError> {
    let matching = receipt
        .events
        .iter()
        .filter(|event| event.topic == PROVIDER_CATALOG_VERSION_EVENT_TOPIC)
        .collect::<Vec<_>>();
    let [event] = matching.as_slice() else {
        return Err(ProviderCatalogError::invalid());
    };
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Event, &event.payload)?;
    let decoded: ProviderCatalogVersionEvent =
        serde_json::from_slice(&event.payload).map_err(|_| ProviderCatalogError::invalid())?;
    if serde_json::to_vec(&decoded).map_err(|_| ProviderCatalogError::invalid())? != event.payload
        || decoded.catalog_version != receipt.revision
        || decoded.previous_catalog_version.checked_add(1) != Some(decoded.catalog_version)
        || decoded.provider_version == 0
    {
        return Err(ProviderCatalogError::invalid());
    }
    Ok(decoded)
}

fn decode_state(
    stored: &StoredState,
    requested_scope: &Scope,
) -> Result<ProviderCatalogState, ProviderCatalogError> {
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Persistence, &stored.payload)?;
    let state: ProviderCatalogState =
        serde_json::from_slice(&stored.payload).map_err(|_| ProviderCatalogError::invalid())?;
    if state.scope != *requested_scope {
        return Err(ProviderCatalogError::scope_denied());
    }
    if state.schema != STATE_SCHEMA
        || state.catalog_version != stored.revision
        || state.catalog_version == 0
        || state.catalog_version > MAX_SAFE_INTEGER
        || stored.stream_id != catalog_stream_id(&state.scope)?
    {
        return Err(ProviderCatalogError::invalid());
    }
    for (provider_id, provider) in &state.providers {
        if provider_id != &provider.provider_id
            || provider.version == 0
            || provider.version > state.catalog_version
        {
            return Err(ProviderCatalogError::invalid());
        }
        let descriptor = ProviderDescriptor {
            provider_id: provider.provider_id.clone(),
            display_name: provider.display_name.clone(),
            adapter_kind: provider.adapter_kind.clone(),
            credential_reference_id: provider.credential_reference_id.clone(),
            models: provider
                .models
                .values()
                .map(|model| ModelCapability {
                    model_id: model.model_id.clone(),
                    display_name: model.display_name.clone(),
                    context_window_tokens: model.context_window_tokens,
                    max_output_tokens: model.max_output_tokens,
                    tool_support: model.tool_support,
                    reasoning_efforts: model.reasoning_efforts.clone(),
                })
                .collect(),
        };
        canonical_descriptor(&descriptor)?;
        for (model_id, model) in &provider.models {
            if model_id != &model.model_id || model.version == 0 || model.version > provider.version
            {
                return Err(ProviderCatalogError::invalid());
            }
        }
    }
    Ok(state)
}

fn catalog_stream_id(scope: &Scope) -> Result<String, ProviderCatalogError> {
    let scope_key = receipt_scope_key(scope)?;
    let mut digest = Sha256::new();
    digest.update(b"winwincode.provider-catalog-scope.v1\0");
    digest.update(scope_key.as_bytes());
    Ok(format!("{STREAM_PREFIX}{:x}", digest.finalize()))
}

fn catalog_event_id(
    request: &ProviderCatalogRequest,
    event: &ProviderCatalogVersionEvent,
) -> Result<String, ProviderCatalogError> {
    let payload = serde_json::to_vec(event).map_err(|_| ProviderCatalogError::invalid())?;
    let mut digest = Sha256::new();
    digest.update(b"winwincode.provider-catalog-event.v1\0");
    digest.update(request.request_id.0.as_bytes());
    digest.update([0]);
    digest.update(payload);
    Ok(format!("provider-catalog:{:x}", digest.finalize()))
}

fn validate_request(request: &ProviderCatalogRequest) -> Result<(), ProviderCatalogError> {
    receipt_actor_key(&request.actor)?;
    receipt_scope_key(&request.scope)?;
    validate_prefixed_id(&request.request_id.0, "req_")?;
    if request.expected_catalog_version > MAX_SAFE_INTEGER {
        return Err(ProviderCatalogError::invalid());
    }
    Ok(())
}

fn validate_credential_reference_id(
    value: &CredentialReferenceId,
) -> Result<(), ProviderCatalogError> {
    validate_prefixed_id(&value.0, "crd_")
}

fn validate_prefixed_id(value: &str, prefix: &str) -> Result<(), ProviderCatalogError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(ProviderCatalogError::invalid());
    };
    if suffix.len() == 26
        && suffix.bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(
                    byte,
                    b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
                )
        })
    {
        Ok(())
    } else {
        Err(ProviderCatalogError::invalid())
    }
}

fn validate_display_name(value: &str, max_chars: usize) -> Result<(), ProviderCatalogError> {
    if value.is_empty()
        || value.trim() != value
        || value.chars().count() > max_chars
        || value.chars().any(char::is_control)
    {
        Err(ProviderCatalogError::invalid())
    } else {
        Ok(())
    }
}

fn validate_token(value: &str, max_chars: usize) -> Result<(), ProviderCatalogError> {
    if value.is_empty()
        || value.len() > max_chars
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
    {
        Err(ProviderCatalogError::invalid())
    } else {
        Ok(())
    }
}

fn ensure_version(expected: u64, current: u64) -> Result<(), ProviderCatalogError> {
    if expected == current {
        Ok(())
    } else {
        Err(ProviderCatalogError::version_conflict())
    }
}

fn next_version(current: u64) -> Result<u64, ProviderCatalogError> {
    current
        .checked_add(1)
        .filter(|version| *version <= MAX_SAFE_INTEGER)
        .ok_or_else(ProviderCatalogError::invalid)
}
