// SPDX-License-Identifier: Apache-2.0

//! Scope-inherited model settings and secret-free route resolution.
//!
//! Settings persist only Provider/model identifiers. Resolution joins the
//! selected identifiers to the current [`crate::ProviderCatalogService`] and
//! returns the generated [`ModelRoute`]. Credential reference metadata comes
//! from that catalog join; this module never resolves a secret, performs policy
//! admission, or invokes a Provider.

use std::fmt::{self, Write as _};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, ModelRoute, OrganizationScope, OrganizationScopeKind, PageInfo, ProjectScope,
    ProjectScopeKind, RepositoryScope, Scope, SettingsGetQuery, SettingsGetResultResponse,
    SettingsGetResultResponseQuery, SettingsProjection, SettingsUpdateCommand,
    SettingsUpdateCompletedResponse, SettingsUpdateCompletedResponseCommand,
    SettingsUpdateCompletedResponseOutcome,
};
use winwincode_domain::{ProductSessionId, RequestId, Revision, SchemaVersion, Sha256Digest};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptIdentity, ReceiptScopeKey,
    StateCommit, StorageError, StorageErrorKind, StoredState,
};

use crate::credential_leak_gate::{
    CredentialLeakError, CredentialLeakGate, CredentialOutputBoundary,
};
use crate::provider_catalog::{
    ProviderCatalogError, ProviderCatalogErrorKind, ProviderCatalogService,
};
use crate::{receipt_actor_key, receipt_scope_key};

const STATE_SCHEMA: &str = "winwincode.model-settings.v1";
const STREAM_PREFIX: &str = "model-settings:";
const EVENT_TOPIC: &str = "model.settings.changed.v1";
/// Canonical local default used before a target has durable settings.
pub const DEFAULT_WORKER_CONCURRENCY_LIMIT: u64 = 1;
const MAX_WORKER_CONCURRENCY_LIMIT: u64 = 10_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// A setting target in the exact supported inheritance hierarchy.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelSettingsTarget {
    Organization {
        scope: OrganizationScope,
    },
    Project {
        scope: ProjectScope,
    },
    Repository {
        scope: RepositoryScope,
    },
    ProductSession {
        repository_scope: RepositoryScope,
        product_session_id: ProductSessionId,
    },
}

/// Provider/model identifiers saved as one scope override.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
}

/// The complete settings value atomically replaced at one target revision.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSettingsValues {
    pub default_model_route: Option<ModelRoute>,
    pub worker_concurrency_limit: u64,
}

/// Exact durable override plus its currently resolved effective route.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSettingsProjection {
    pub target: ModelSettingsTarget,
    pub selection: Option<ModelSelection>,
    pub default_model_route: Option<ModelRoute>,
    pub worker_concurrency_limit: u64,
    pub revision: u64,
}

/// Scoped idempotency identity and optimistic settings revision.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSettingsRequest {
    pub actor: Actor,
    pub target: ModelSettingsTarget,
    pub request_id: RequestId,
    pub expected_revision: u64,
}

/// Durable model-settings mutation category.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSettingsChange {
    Updated,
    LegacyMigrated,
}

/// Durable result of one model-settings mutation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSettingsMutationReceipt {
    pub request_id: RequestId,
    pub change: ModelSettingsChange,
    pub previous_revision: u64,
    pub revision: u64,
    pub idempotent_replay: bool,
    pub projection: ModelSettingsProjection,
}

/// Stable route-setting and resolution failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelSettingsErrorKind {
    InvalidRequest,
    ScopeDenied,
    NoConfiguredRoute,
    ProviderNotFound,
    ProviderDisabled,
    ModelNotFound,
    ModelDisabled,
    AlreadyMigrated,
    RevisionConflict,
    RequestConflict,
    CredentialLeak,
    Storage,
}

/// Bounded error that never copies a model setting or Provider diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSettingsError {
    kind: ModelSettingsErrorKind,
    message: &'static str,
}

impl ModelSettingsError {
    const fn new(kind: ModelSettingsErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            ModelSettingsErrorKind::InvalidRequest,
            "Model settings request is invalid",
        )
    }

    const fn scope_denied() -> Self {
        Self::new(
            ModelSettingsErrorKind::ScopeDenied,
            "Model settings state belongs to another target",
        )
    }

    const fn no_route() -> Self {
        Self::new(
            ModelSettingsErrorKind::NoConfiguredRoute,
            "No model route is configured for this target",
        )
    }

    const fn already_migrated() -> Self {
        Self::new(
            ModelSettingsErrorKind::AlreadyMigrated,
            "Legacy model settings were already migrated",
        )
    }

    const fn revision_conflict() -> Self {
        Self::new(
            ModelSettingsErrorKind::RevisionConflict,
            "Model settings revision does not match",
        )
    }

    const fn credential_leak() -> Self {
        Self::new(
            ModelSettingsErrorKind::CredentialLeak,
            "Model settings output was rejected by the Credential leak gate",
        )
    }

    /// Returns the stable machine-readable category.
    #[must_use]
    pub const fn kind(&self) -> ModelSettingsErrorKind {
        self.kind
    }
}

impl fmt::Display for ModelSettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ModelSettingsError {}

impl From<CredentialLeakError> for ModelSettingsError {
    fn from(_error: CredentialLeakError) -> Self {
        Self::credential_leak()
    }
}

impl From<StorageError> for ModelSettingsError {
    fn from(error: StorageError) -> Self {
        match error.kind() {
            StorageErrorKind::RevisionConflict => Self::revision_conflict(),
            StorageErrorKind::RequestConflict => Self::new(
                ModelSettingsErrorKind::RequestConflict,
                "Model settings requestId was reused with different input",
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
                ModelSettingsErrorKind::Storage,
                "Model settings storage operation failed",
            ),
        }
    }
}

impl From<ProviderCatalogError> for ModelSettingsError {
    fn from(error: ProviderCatalogError) -> Self {
        let (kind, message) = match error.kind() {
            ProviderCatalogErrorKind::ScopeDenied => (
                ModelSettingsErrorKind::ScopeDenied,
                "Provider catalog belongs to another scope",
            ),
            ProviderCatalogErrorKind::ProviderNotFound => (
                ModelSettingsErrorKind::ProviderNotFound,
                "Configured Provider is not present in the effective catalog",
            ),
            ProviderCatalogErrorKind::ProviderDisabled => (
                ModelSettingsErrorKind::ProviderDisabled,
                "Configured Provider is disabled in the effective catalog",
            ),
            ProviderCatalogErrorKind::ModelNotFound => (
                ModelSettingsErrorKind::ModelNotFound,
                "Configured model is not present for the effective Provider",
            ),
            ProviderCatalogErrorKind::ModelDisabled => (
                ModelSettingsErrorKind::ModelDisabled,
                "Configured model is disabled for the effective Provider",
            ),
            ProviderCatalogErrorKind::CredentialLeak => (
                ModelSettingsErrorKind::CredentialLeak,
                "Model route was rejected by the Credential leak gate",
            ),
            ProviderCatalogErrorKind::InvalidRequest
            | ProviderCatalogErrorKind::AlreadyDisabled
            | ProviderCatalogErrorKind::VersionConflict
            | ProviderCatalogErrorKind::RequestConflict => (
                ModelSettingsErrorKind::InvalidRequest,
                "Effective Provider catalog state is invalid",
            ),
            ProviderCatalogErrorKind::Storage => (
                ModelSettingsErrorKind::Storage,
                "Provider catalog storage operation failed",
            ),
        };
        Self::new(kind, message)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelSettingsState {
    schema: String,
    target: ModelSettingsTarget,
    revision: u64,
    selection: Option<ModelSelection>,
    worker_concurrency_limit: u64,
    legacy_migration_completed: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelSettingsChangedEvent {
    target: ModelSettingsTarget,
    change: ModelSettingsChange,
    previous_revision: u64,
    revision: u64,
    has_override: bool,
    projection: ModelSettingsProjection,
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum SettingsCommandDigest<'a> {
    Update {
        request: &'a ModelSettingsRequest,
        values: &'a ModelSettingsValues,
    },
    MigrateLegacy {
        request: &'a ModelSettingsRequest,
        selection: &'a Option<ModelSelection>,
    },
}

struct CommandReceipt {
    identity: ReceiptIdentity,
    digest: Sha256Digest,
}

/// Durable scope-setting service and deterministic route resolver.
pub struct ModelSettingsService<'a> {
    storage: &'a mut dyn ProductStateStorage,
}

impl<'a> ModelSettingsService<'a> {
    #[must_use]
    pub fn new(storage: &'a mut dyn ProductStateStorage) -> Self {
        Self { storage }
    }

    /// Replaces the complete settings value at one exact target.
    ///
    /// A non-empty selection must resolve through the current inherited
    /// Provider catalog before it is committed. Clearing an override resumes
    /// parent inheritance.
    ///
    /// # Errors
    ///
    /// Rejects malformed, unavailable, stale, or conflicting input.
    pub fn update(
        &mut self,
        request: &ModelSettingsRequest,
        values: ModelSettingsValues,
    ) -> Result<ModelSettingsMutationReceipt, ModelSettingsError> {
        validate_request(request)?;
        let (values, selection) = canonical_values(values)?;
        let command = command_receipt(&SettingsCommandDigest::Update {
            request,
            values: &values,
        })?;
        if let Some(receipt) = self.replay(request, &command)? {
            return Ok(receipt);
        }
        if let (Some(route), Some(selection)) = (&values.default_model_route, &selection) {
            let (resolved, _scope) = self.resolve_selection(&request.target, selection)?;
            if resolved != *route {
                return Err(ModelSettingsError::invalid());
            }
        }
        let mut state = self.load_or_empty(&request.target)?;
        if let Err(source) = ensure_revision(request.expected_revision, state.revision) {
            return match self.replay(request, &command)? {
                Some(replay) => Ok(replay),
                None => Err(source),
            };
        }
        state.selection = selection;
        state.worker_concurrency_limit = values.worker_concurrency_limit;
        state.legacy_migration_completed = true;
        self.commit(request, command, state, ModelSettingsChange::Updated)
    }

    /// Applies the one generated `settings.update` contract as a complete
    /// atomic replacement.
    ///
    /// # Errors
    ///
    /// Rejects unsupported scopes, invalid concurrency, a stale route
    /// reference, revision conflicts, and changed-body request replays.
    pub fn update_generated(
        &mut self,
        command: &SettingsUpdateCommand,
    ) -> Result<SettingsUpdateCompletedResponse, ModelSettingsError> {
        let target = target_from_scope(&command.scope)?;
        let request = ModelSettingsRequest {
            actor: command.actor.clone(),
            target,
            request_id: command.request_id.clone(),
            expected_revision: u64::try_from(command.expected_revision.0)
                .map_err(|_| ModelSettingsError::invalid())?,
        };
        let receipt = self.update(
            &request,
            ModelSettingsValues {
                default_model_route: command.payload.patch.default_model_route.clone(),
                worker_concurrency_limit: u64::try_from(
                    command.payload.patch.worker_concurrency_limit,
                )
                .map_err(|_| ModelSettingsError::invalid())?,
            },
        )?;
        checked_http_response(SettingsUpdateCompletedResponse {
            command: SettingsUpdateCompletedResponseCommand::SettingsUpdate,
            current_revision: revision(receipt.revision)?,
            outcome: SettingsUpdateCompletedResponseOutcome::Completed,
            previous_revision: revision(receipt.previous_revision)?,
            request_id: command.request_id.clone(),
            result: generated_projection(&receipt.projection)?,
            schema_version: SchemaVersion::WinwincodeV1,
        })
    }

    /// Projects one target's durable revision and concurrency together with
    /// the currently resolved inherited route.
    ///
    /// # Errors
    ///
    /// Fails closed when a configured Provider/model is missing or disabled,
    /// or when the durable state belongs to another target.
    pub fn project(
        &mut self,
        target: &ModelSettingsTarget,
    ) -> Result<ModelSettingsProjection, ModelSettingsError> {
        validate_target(target)?;
        let state = self.load_or_empty(target)?;
        let projection = ModelSettingsProjection {
            target: target.clone(),
            selection: state.selection,
            default_model_route: self
                .resolve_effective_optional(target)?
                .map(|(route, _scope)| route),
            worker_concurrency_limit: state.worker_concurrency_limit,
            revision: state.revision,
        };
        CredentialLeakGate::default()
            .inspect_serializable(CredentialOutputBoundary::Serialization, &projection)?;
        Ok(projection)
    }

    /// Serves the generated `settings.get` response from one service-owned
    /// projection.
    ///
    /// # Errors
    ///
    /// Rejects unsupported scopes, malformed query authority/page fields, or
    /// unavailable configured Provider/model state.
    pub fn get(
        &mut self,
        query: &SettingsGetQuery,
    ) -> Result<SettingsGetResultResponse, ModelSettingsError> {
        receipt_actor_key(&query.actor)?;
        validate_prefixed_id(&query.request_id.0, "req_")?;
        if query.page.cursor.is_some() || !(1..=200).contains(&query.page.limit) {
            return Err(ModelSettingsError::invalid());
        }
        let projection = self.project(&target_from_scope(&query.scope)?)?;
        checked_http_response(SettingsGetResultResponse {
            page: PageInfo {
                has_more: false,
                next_cursor: None,
            },
            query: SettingsGetResultResponseQuery::SettingsGet,
            request_id: query.request_id.clone(),
            result: generated_projection(&projection)?,
            schema_version: SchemaVersion::WinwincodeV1,
        })
    }

    /// Imports the old generated `ModelRoute` exactly once.
    ///
    /// Only Provider/model identifiers survive migration. The legacy
    /// Credential reference is intentionally discarded and the resolved route
    /// always receives the current reference from Provider Catalog.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable route, a repeated migration, stale revision, or
    /// a conflicting scoped request replay.
    pub fn migrate_legacy_once(
        &mut self,
        request: &ModelSettingsRequest,
        legacy_route: Option<&ModelRoute>,
    ) -> Result<ModelSettingsMutationReceipt, ModelSettingsError> {
        validate_request(request)?;
        let selection = legacy_route.map(legacy_selection).transpose()?;
        let command = command_receipt(&SettingsCommandDigest::MigrateLegacy {
            request,
            selection: &selection,
        })?;
        if let Some(receipt) = self.replay(request, &command)? {
            return Ok(receipt);
        }
        if let Some(selection) = &selection {
            self.resolve_selection(&request.target, selection)?;
        }
        let mut state = self.load_or_empty(&request.target)?;
        if let Err(source) = ensure_revision(request.expected_revision, state.revision) {
            return match self.replay(request, &command)? {
                Some(replay) => Ok(replay),
                None => Err(source),
            };
        }
        if state.legacy_migration_completed {
            return Err(ModelSettingsError::already_migrated());
        }
        state.selection = selection;
        state.legacy_migration_completed = true;
        self.commit(request, command, state, ModelSettingsChange::LegacyMigrated)
    }

    /// Resolves the most-specific configured override to the generated route.
    ///
    /// Setting priority is `ProductSession`, Repository, Project, Organization.
    /// The first configured setting wins. If that explicit setting is disabled
    /// or missing from the effective catalog, resolution fails closed rather
    /// than silently falling back to a parent setting.
    ///
    /// # Errors
    ///
    /// Returns a distinct error for no setting, missing/disabled Provider, or
    /// missing/disabled model.
    pub fn resolve(
        &mut self,
        target: &ModelSettingsTarget,
    ) -> Result<ModelRoute, ModelSettingsError> {
        self.resolve_with_catalog_scope(target)
            .map(|(route, _scope)| route)
    }

    pub(crate) fn resolve_with_catalog_scope(
        &mut self,
        target: &ModelSettingsTarget,
    ) -> Result<(ModelRoute, Scope), ModelSettingsError> {
        self.resolve_effective_optional(target)?
            .ok_or_else(ModelSettingsError::no_route)
    }

    fn resolve_effective_optional(
        &mut self,
        target: &ModelSettingsTarget,
    ) -> Result<Option<(ModelRoute, Scope)>, ModelSettingsError> {
        validate_target(target)?;
        for candidate in setting_chain(target) {
            let state = self.load_or_empty(&candidate)?;
            if let Some(selection) = state.selection {
                return self.resolve_selection(target, &selection).map(Some);
            }
        }
        Ok(None)
    }

    fn resolve_selection(
        &mut self,
        target: &ModelSettingsTarget,
        selection: &ModelSelection,
    ) -> Result<(ModelRoute, Scope), ModelSettingsError> {
        for scope in catalog_chain(target) {
            let projection = ProviderCatalogService::new(self.storage).project(&scope)?;
            if projection
                .providers
                .iter()
                .any(|provider| provider.provider_id == selection.provider_id)
            {
                let resolved = ProviderCatalogService::new(self.storage).resolve_model(
                    &scope,
                    &selection.provider_id,
                    &selection.model_id,
                )?;
                let route = ModelRoute {
                    credential_reference_id: resolved.credential_reference_id,
                    model_id: resolved.model.model_id,
                    provider_id: resolved.provider_id,
                };
                CredentialLeakGate::default()
                    .inspect_serializable(CredentialOutputBoundary::Serialization, &route)?;
                return Ok((route, scope));
            }
        }
        Err(ModelSettingsError::new(
            ModelSettingsErrorKind::ProviderNotFound,
            "Configured Provider is not present in the effective catalog",
        ))
    }

    fn projection_after_replacement(
        &mut self,
        state: &ModelSettingsState,
    ) -> Result<ModelSettingsProjection, ModelSettingsError> {
        let default_model_route = if let Some(selection) = &state.selection {
            Some(self.resolve_selection(&state.target, selection)?.0)
        } else {
            let mut inherited = None;
            for candidate in setting_chain(&state.target).into_iter().skip(1) {
                let parent = self.load_or_empty(&candidate)?;
                if let Some(selection) = parent.selection {
                    inherited = Some(self.resolve_selection(&state.target, &selection)?.0);
                    break;
                }
            }
            inherited
        };
        let projection = ModelSettingsProjection {
            target: state.target.clone(),
            selection: state.selection.clone(),
            default_model_route,
            worker_concurrency_limit: state.worker_concurrency_limit,
            revision: state.revision,
        };
        CredentialLeakGate::default()
            .inspect_serializable(CredentialOutputBoundary::Serialization, &projection)?;
        Ok(projection)
    }

    fn load_or_empty(
        &self,
        target: &ModelSettingsTarget,
    ) -> Result<ModelSettingsState, ModelSettingsError> {
        let stream_id = settings_stream_id(target)?;
        match self.storage.load_state(&stream_id)? {
            Some(stored) => decode_state(&stored, target),
            None => Ok(ModelSettingsState {
                schema: STATE_SCHEMA.to_owned(),
                target: target.clone(),
                revision: 0,
                selection: None,
                worker_concurrency_limit: DEFAULT_WORKER_CONCURRENCY_LIMIT,
                legacy_migration_completed: false,
            }),
        }
    }

    fn replay(
        &self,
        request: &ModelSettingsRequest,
        command: &CommandReceipt,
    ) -> Result<Option<ModelSettingsMutationReceipt>, ModelSettingsError> {
        self.storage
            .load_receipt(&command.identity, &command.digest)?
            .map(|receipt| mutation_receipt(request, &receipt))
            .transpose()
    }

    fn commit(
        &mut self,
        request: &ModelSettingsRequest,
        command: CommandReceipt,
        mut state: ModelSettingsState,
        change: ModelSettingsChange,
    ) -> Result<ModelSettingsMutationReceipt, ModelSettingsError> {
        let previous_revision = state.revision;
        state.revision = next_revision(previous_revision)?;
        let projection = self.projection_after_replacement(&state)?;
        let event = ModelSettingsChangedEvent {
            target: state.target.clone(),
            change,
            previous_revision,
            revision: state.revision,
            has_override: state.selection.is_some(),
            projection,
        };
        let state_payload =
            serde_json::to_vec(&state).map_err(|_| ModelSettingsError::invalid())?;
        CredentialLeakGate::default()
            .inspect_json_bytes(CredentialOutputBoundary::Persistence, &state_payload)?;
        let event_payload =
            serde_json::to_vec(&event).map_err(|_| ModelSettingsError::invalid())?;
        CredentialLeakGate::default()
            .inspect_json_bytes(CredentialOutputBoundary::Event, &event_payload)?;
        let event_id = settings_event_id(request, &event)?;
        let commit = StateCommit::new(
            command.identity,
            command.digest,
            settings_stream_id(&request.target)?,
            request.expected_revision,
            state_payload,
            vec![NewOutboxEvent::internal(
                event_id,
                EVENT_TOPIC,
                event_payload,
            )],
        );
        let receipt = self.storage.commit(&commit)?;
        let durable = event_from_receipt(&receipt)?;
        if !receipt.idempotent_replay && durable != event {
            return Err(ModelSettingsError::invalid());
        }
        mutation_receipt(request, &receipt)
    }
}

fn canonical_values(
    values: ModelSettingsValues,
) -> Result<(ModelSettingsValues, Option<ModelSelection>), ModelSettingsError> {
    if !(DEFAULT_WORKER_CONCURRENCY_LIMIT..=MAX_WORKER_CONCURRENCY_LIMIT)
        .contains(&values.worker_concurrency_limit)
    {
        return Err(ModelSettingsError::invalid());
    }
    let selection = values
        .default_model_route
        .as_ref()
        .map(legacy_selection)
        .transpose()?;
    Ok((values, selection))
}

fn target_from_scope(scope: &Scope) -> Result<ModelSettingsTarget, ModelSettingsError> {
    match scope {
        Scope::OrganizationScope(scope) => Ok(ModelSettingsTarget::Organization {
            scope: scope.clone(),
        }),
        Scope::ProjectScope(scope) => Ok(ModelSettingsTarget::Project {
            scope: scope.clone(),
        }),
        Scope::RepositoryScope(scope) => Ok(ModelSettingsTarget::Repository {
            scope: scope.clone(),
        }),
        Scope::WorkspaceScope(_) => Err(ModelSettingsError::invalid()),
    }
}

fn generated_projection(
    projection: &ModelSettingsProjection,
) -> Result<SettingsProjection, ModelSettingsError> {
    Ok(SettingsProjection {
        default_model_route: projection.default_model_route.clone(),
        revision: revision(projection.revision)?,
        worker_concurrency_limit: i64::try_from(projection.worker_concurrency_limit)
            .map_err(|_| ModelSettingsError::invalid())?,
    })
}

fn revision(value: u64) -> Result<Revision, ModelSettingsError> {
    Ok(Revision(
        i64::try_from(value).map_err(|_| ModelSettingsError::invalid())?,
    ))
}

fn checked_http_response<T: Serialize>(value: T) -> Result<T, ModelSettingsError> {
    CredentialLeakGate::default().inspect_serializable(CredentialOutputBoundary::Http, &value)?;
    Ok(value)
}

fn legacy_selection(route: &ModelRoute) -> Result<ModelSelection, ModelSettingsError> {
    validate_token(&route.provider_id, 128)?;
    validate_token(&route.model_id, 200)?;
    validate_prefixed_id(&route.credential_reference_id.0, "crd_")?;
    Ok(ModelSelection {
        provider_id: route.provider_id.clone(),
        model_id: route.model_id.clone(),
    })
}

fn setting_chain(target: &ModelSettingsTarget) -> Vec<ModelSettingsTarget> {
    match target {
        ModelSettingsTarget::Organization { scope } => {
            vec![ModelSettingsTarget::Organization {
                scope: scope.clone(),
            }]
        }
        ModelSettingsTarget::Project { scope } => vec![
            ModelSettingsTarget::Project {
                scope: scope.clone(),
            },
            ModelSettingsTarget::Organization {
                scope: organization_from_project(scope),
            },
        ],
        ModelSettingsTarget::Repository { scope } => repository_setting_chain(scope),
        ModelSettingsTarget::ProductSession {
            repository_scope,
            product_session_id,
        } => {
            let mut chain = vec![ModelSettingsTarget::ProductSession {
                repository_scope: repository_scope.clone(),
                product_session_id: product_session_id.clone(),
            }];
            chain.extend(repository_setting_chain(repository_scope));
            chain
        }
    }
}

fn repository_setting_chain(scope: &RepositoryScope) -> Vec<ModelSettingsTarget> {
    let project = project_from_repository(scope);
    vec![
        ModelSettingsTarget::Repository {
            scope: scope.clone(),
        },
        ModelSettingsTarget::Project {
            scope: project.clone(),
        },
        ModelSettingsTarget::Organization {
            scope: organization_from_project(&project),
        },
    ]
}

fn catalog_chain(target: &ModelSettingsTarget) -> Vec<Scope> {
    match target {
        ModelSettingsTarget::Organization { scope } => {
            vec![Scope::OrganizationScope(scope.clone())]
        }
        ModelSettingsTarget::Project { scope } => vec![
            Scope::ProjectScope(scope.clone()),
            Scope::OrganizationScope(organization_from_project(scope)),
        ],
        ModelSettingsTarget::Repository { scope }
        | ModelSettingsTarget::ProductSession {
            repository_scope: scope,
            ..
        } => {
            let project = project_from_repository(scope);
            vec![
                Scope::RepositoryScope(scope.clone()),
                Scope::ProjectScope(project.clone()),
                Scope::OrganizationScope(organization_from_project(&project)),
            ]
        }
    }
}

fn project_from_repository(scope: &RepositoryScope) -> ProjectScope {
    ProjectScope {
        kind: ProjectScopeKind::Project,
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
    }
}

fn organization_from_project(scope: &ProjectScope) -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: scope.organization_id.clone(),
    }
}

fn command_receipt(
    command: &SettingsCommandDigest<'_>,
) -> Result<CommandReceipt, ModelSettingsError> {
    let request = match command {
        SettingsCommandDigest::Update { request, .. }
        | SettingsCommandDigest::MigrateLegacy { request, .. } => *request,
    };
    let actor_key = receipt_actor_key(&request.actor)?;
    let scope_key = target_receipt_scope_key(&request.target)?;
    validate_prefixed_id(&request.request_id.0, "req_")?;
    let identity = ReceiptIdentity::new(actor_key, scope_key, request.request_id.clone())?;
    let payload = serde_json::to_vec(command).map_err(|_| ModelSettingsError::invalid())?;
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(payload)));
    Ok(CommandReceipt { identity, digest })
}

fn mutation_receipt(
    request: &ModelSettingsRequest,
    receipt: &CommitReceipt,
) -> Result<ModelSettingsMutationReceipt, ModelSettingsError> {
    let event = event_from_receipt(receipt)?;
    if event.target != request.target {
        return Err(ModelSettingsError::scope_denied());
    }
    let result = ModelSettingsMutationReceipt {
        request_id: request.request_id.clone(),
        change: event.change,
        previous_revision: event.previous_revision,
        revision: event.revision,
        idempotent_replay: receipt.idempotent_replay,
        projection: event.projection,
    };
    CredentialLeakGate::default()
        .inspect_serializable(CredentialOutputBoundary::Serialization, &result)?;
    Ok(result)
}

fn event_from_receipt(
    receipt: &CommitReceipt,
) -> Result<ModelSettingsChangedEvent, ModelSettingsError> {
    let matching = receipt
        .events
        .iter()
        .filter(|event| event.topic == EVENT_TOPIC)
        .collect::<Vec<_>>();
    let [event] = matching.as_slice() else {
        return Err(ModelSettingsError::invalid());
    };
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Event, &event.payload)?;
    let decoded: ModelSettingsChangedEvent =
        serde_json::from_slice(&event.payload).map_err(|_| ModelSettingsError::invalid())?;
    if serde_json::to_vec(&decoded).map_err(|_| ModelSettingsError::invalid())? != event.payload
        || decoded.revision != receipt.revision
        || decoded.previous_revision.checked_add(1) != Some(decoded.revision)
        || decoded.projection.target != decoded.target
        || decoded.projection.revision != decoded.revision
        || decoded.projection.selection.is_some() != decoded.has_override
        || !projection_route_matches_selection(&decoded.projection)
        || !(DEFAULT_WORKER_CONCURRENCY_LIMIT..=MAX_WORKER_CONCURRENCY_LIMIT)
            .contains(&decoded.projection.worker_concurrency_limit)
    {
        return Err(ModelSettingsError::invalid());
    }
    Ok(decoded)
}

fn projection_route_matches_selection(projection: &ModelSettingsProjection) -> bool {
    let Some(selection) = &projection.selection else {
        return true;
    };
    projection
        .default_model_route
        .as_ref()
        .is_some_and(|route| {
            route.provider_id == selection.provider_id && route.model_id == selection.model_id
        })
}

fn decode_state(
    stored: &StoredState,
    target: &ModelSettingsTarget,
) -> Result<ModelSettingsState, ModelSettingsError> {
    CredentialLeakGate::default()
        .inspect_json_bytes(CredentialOutputBoundary::Persistence, &stored.payload)?;
    let state: ModelSettingsState =
        serde_json::from_slice(&stored.payload).map_err(|_| ModelSettingsError::invalid())?;
    if state.target != *target {
        return Err(ModelSettingsError::scope_denied());
    }
    if state.schema != STATE_SCHEMA
        || state.revision != stored.revision
        || state.revision == 0
        || state.revision > MAX_SAFE_INTEGER
        || stored.stream_id != settings_stream_id(&state.target)?
        || !(DEFAULT_WORKER_CONCURRENCY_LIMIT..=MAX_WORKER_CONCURRENCY_LIMIT)
            .contains(&state.worker_concurrency_limit)
    {
        return Err(ModelSettingsError::invalid());
    }
    if let Some(selection) = &state.selection {
        validate_token(&selection.provider_id, 128)?;
        validate_token(&selection.model_id, 200)?;
    }
    Ok(state)
}

fn settings_stream_id(target: &ModelSettingsTarget) -> Result<String, ModelSettingsError> {
    let digest = target_digest(b"winwincode.model-settings-stream.v1\0", target)?;
    let mut stream_id = String::with_capacity(STREAM_PREFIX.len() + 64);
    stream_id.push_str(STREAM_PREFIX);
    for byte in digest {
        write!(&mut stream_id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(stream_id)
}

fn target_receipt_scope_key(
    target: &ModelSettingsTarget,
) -> Result<ReceiptScopeKey, ModelSettingsError> {
    let digest = target_digest(b"winwincode.model-settings-receipt-scope.v1\0", target)?;
    ReceiptScopeKey::from_encoded(digest.to_vec()).map_err(Into::into)
}

fn target_digest(
    domain: &[u8],
    target: &ModelSettingsTarget,
) -> Result<[u8; 32], ModelSettingsError> {
    validate_target(target)?;
    let payload = serde_json::to_vec(target).map_err(|_| ModelSettingsError::invalid())?;
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(payload);
    Ok(digest.finalize().into())
}

fn settings_event_id(
    request: &ModelSettingsRequest,
    event: &ModelSettingsChangedEvent,
) -> Result<String, ModelSettingsError> {
    let payload = serde_json::to_vec(event).map_err(|_| ModelSettingsError::invalid())?;
    let mut digest = Sha256::new();
    digest.update(b"winwincode.model-settings-event.v1\0");
    digest.update(request.request_id.0.as_bytes());
    digest.update([0]);
    digest.update(payload);
    Ok(format!("model-settings:{:x}", digest.finalize()))
}

fn validate_request(request: &ModelSettingsRequest) -> Result<(), ModelSettingsError> {
    receipt_actor_key(&request.actor)?;
    validate_target(&request.target)?;
    validate_prefixed_id(&request.request_id.0, "req_")?;
    if request.expected_revision > MAX_SAFE_INTEGER {
        return Err(ModelSettingsError::invalid());
    }
    Ok(())
}

fn validate_target(target: &ModelSettingsTarget) -> Result<(), ModelSettingsError> {
    match target {
        ModelSettingsTarget::Organization { scope } => {
            receipt_scope_key(&Scope::OrganizationScope(scope.clone()))?;
        }
        ModelSettingsTarget::Project { scope } => {
            receipt_scope_key(&Scope::ProjectScope(scope.clone()))?;
        }
        ModelSettingsTarget::Repository { scope } => {
            receipt_scope_key(&Scope::RepositoryScope(scope.clone()))?;
        }
        ModelSettingsTarget::ProductSession {
            repository_scope,
            product_session_id,
        } => {
            receipt_scope_key(&Scope::RepositoryScope(repository_scope.clone()))?;
            validate_prefixed_id(&product_session_id.0, "psn_")?;
        }
    }
    Ok(())
}

fn validate_prefixed_id(value: &str, prefix: &str) -> Result<(), ModelSettingsError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(ModelSettingsError::invalid());
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
        Err(ModelSettingsError::invalid())
    }
}

fn validate_token(value: &str, max_chars: usize) -> Result<(), ModelSettingsError> {
    if value.is_empty()
        || value.len() > max_chars
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
    {
        Err(ModelSettingsError::invalid())
    } else {
        Ok(())
    }
}

fn ensure_revision(expected: u64, current: u64) -> Result<(), ModelSettingsError> {
    if expected == current {
        Ok(())
    } else {
        Err(ModelSettingsError::revision_conflict())
    }
}

fn next_revision(current: u64) -> Result<u64, ModelSettingsError> {
    current
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(ModelSettingsError::invalid)
}
