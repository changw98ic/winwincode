// SPDX-License-Identifier: Apache-2.0

//! Generated API adapter for the durable enterprise Policy version ledger.

use std::fmt;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use winwincode_api::generated::{
    Actor, EnterprisePolicyDefinition as ApiDefinition, EnterprisePolicyListQuery,
    EnterprisePolicyListResultResponse, EnterprisePolicyListResultResponseQuery,
    EnterprisePolicyPage as ApiPage, EnterprisePolicyPageKind,
    EnterprisePolicyProjection as ApiProjection, EnterprisePolicyRule as ApiRule,
    EnterprisePolicyUpdateCommand, EnterprisePolicyUpdateCompletedResponse,
    EnterprisePolicyUpdateCompletedResponseCommand, EnterprisePolicyUpdateCompletedResponseOutcome,
    EnterprisePolicyVersionReference as ApiVersionReference,
    EnterprisePolicyVersionSource as ApiVersionSource, OrganizationScope, OrganizationScopeKind,
    PageInfo, ProjectScope, ProjectScopeKind, RepositoryScope, RepositoryScopeKind, Scope,
    ServiceAccountActor, ServiceAccountActorKind, SystemActor, SystemActorKind, UserActor,
    UserActorKind, WorkspaceScope, WorkspaceScopeKind,
};
use winwincode_domain::{Instant, OpaqueCursor, Revision};
use winwincode_storage::{
    EnterprisePolicyActor, EnterprisePolicyChildOverrideMode, EnterprisePolicyCursor,
    EnterprisePolicyDefinition, EnterprisePolicyEffect, EnterprisePolicyError,
    EnterprisePolicyErrorKind, EnterprisePolicyFilter, EnterprisePolicyInheritanceMode,
    EnterprisePolicyKind, EnterprisePolicyMode, EnterprisePolicyRule, EnterprisePolicyScope,
    EnterprisePolicyState, EnterprisePolicyVersion, EnterprisePolicyVersionReference,
    EnterprisePolicyVersionSource, EnterprisePolicyWrite, SqliteStorage,
};

/// Trusted clock used only for the durable version creation time.
pub trait EnterprisePolicyClock {
    fn now(&mut self) -> Instant;
}

/// Public error categories for the generated adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterprisePolicyApiErrorKind {
    InvalidRequest,
    RevisionConflict,
    RequestConflict,
    AuthorityMismatch,
    NotFound,
    Unavailable,
}

/// Secret-free generated adapter error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterprisePolicyApiError {
    kind: EnterprisePolicyApiErrorKind,
    message: String,
}

impl EnterprisePolicyApiError {
    #[must_use]
    pub const fn kind(&self) -> EnterprisePolicyApiErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterprisePolicyApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EnterprisePolicyApiError {}

/// Unique generated command/query adapter backed by one durable Policy ledger.
pub struct EnterprisePolicyApiService<'storage, 'clock> {
    storage: &'storage mut SqliteStorage,
    clock: &'clock mut dyn EnterprisePolicyClock,
}

impl<'storage, 'clock> EnterprisePolicyApiService<'storage, 'clock> {
    #[must_use]
    pub fn new(
        storage: &'storage mut SqliteStorage,
        clock: &'clock mut dyn EnterprisePolicyClock,
    ) -> Self {
        Self { storage, clock }
    }

    /// Applies one generated Policy update with durable revision and replay semantics.
    ///
    /// # Errors
    ///
    /// Returns a generated-boundary error for invalid input, stale authority,
    /// changed request reuse, durable corruption, or storage failure.
    pub fn update(
        &mut self,
        command: &EnterprisePolicyUpdateCommand,
    ) -> Result<EnterprisePolicyUpdateCompletedResponse, EnterprisePolicyApiError> {
        let expected_revision = nonnegative(command.expected_revision.0, "expectedRevision")?;
        let scope = policy_scope(&command.scope);
        let write = EnterprisePolicyWrite {
            policy_id: command.payload.policy_id.clone(),
            policy_kind: policy_kind(&command.payload.policy_kind)?,
            scope,
            mode: policy_mode(&command.payload.mode)?,
            state: policy_state(&command.payload.state)?,
            definition: definition(&command.payload.definition)?,
            definition_sha256: command.payload.definition_sha256.clone(),
            effective_at: command.payload.effective_at.clone(),
            inheritance_mode: inheritance_mode(&command.payload.inheritance_mode)?,
            base_version: command
                .payload
                .base_version
                .as_ref()
                .map(version_reference)
                .transpose()?,
            expected_revision,
            source: EnterprisePolicyVersionSource {
                actor: policy_actor(&command.actor),
                request_id: command.request_id.clone(),
            },
            updated_at: self.clock.now(),
        };
        let receipt = self
            .storage
            .enterprise_policy_ledger()
            .map_err(|error| policy_error(&error))?
            .write(&write)
            .map_err(|error| policy_error(&error))?;
        Ok(EnterprisePolicyUpdateCompletedResponse {
            command: EnterprisePolicyUpdateCompletedResponseCommand::EnterprisePolicyUpdate,
            current_revision: revision(receipt.version.revision)?,
            outcome: EnterprisePolicyUpdateCompletedResponseOutcome::Completed,
            previous_revision: revision(receipt.previous_revision)?,
            request_id: command.request_id.clone(),
            result: projection(&receipt.version)?,
            schema_version: command.schema_version.clone(),
        })
    }

    /// Reads one stable generated page of exact-scope Policy heads.
    ///
    /// # Errors
    ///
    /// Returns a generated-boundary error for invalid filters/cursors, foreign
    /// snapshot reuse, durable corruption, or storage failure.
    pub fn list(
        &mut self,
        query: &EnterprisePolicyListQuery,
    ) -> Result<EnterprisePolicyListResultResponse, EnterprisePolicyApiError> {
        let limit = positive(query.page.limit, "page.limit")?;
        let cursor = query.page.cursor.as_ref().map(decode_cursor).transpose()?;
        let filter = EnterprisePolicyFilter {
            policy_kinds: query
                .parameters
                .policy_kinds
                .iter()
                .map(|kind| policy_kind(kind))
                .collect::<Result<Vec<_>, _>>()?,
            states: query
                .parameters
                .states
                .iter()
                .map(|state| policy_state(state))
                .collect::<Result<Vec<_>, _>>()?,
        };
        let page = self
            .storage
            .enterprise_policy_ledger()
            .map_err(|error| policy_error(&error))?
            .scan_heads(&policy_scope(&query.scope), &filter, cursor.as_ref(), limit)
            .map_err(|error| policy_error(&error))?;
        Ok(EnterprisePolicyListResultResponse {
            page: PageInfo {
                has_more: page.next.is_some(),
                next_cursor: page.next.as_ref().map(encode_cursor).transpose()?,
            },
            query: EnterprisePolicyListResultResponseQuery::EnterprisePolicyList,
            request_id: query.request_id.clone(),
            result: ApiPage {
                items: page
                    .versions
                    .iter()
                    .map(projection)
                    .collect::<Result<Vec<_>, _>>()?,
                kind: EnterprisePolicyPageKind::EnterprisePolicyPage,
                snapshot_revision: revision(page.snapshot_sequence)?,
            },
            schema_version: query.schema_version.clone(),
        })
    }
}

fn projection(
    version: &EnterprisePolicyVersion,
) -> Result<ApiProjection, EnterprisePolicyApiError> {
    Ok(ApiProjection {
        base_version: version
            .base_version
            .as_ref()
            .map(api_version_reference)
            .transpose()?,
        definition_sha256: version.definition_sha256.clone(),
        effective_at: version.effective_at.clone(),
        effective_definition_sha256: version.effective_definition_sha256.clone(),
        id: version.policy_id.clone(),
        inheritance_mode: inheritance_mode_string(version.inheritance_mode).to_owned(),
        mode: mode_string(version.mode).to_owned(),
        policy_kind: kind_string(version.policy_kind).to_owned(),
        relaxation_authority: version
            .relaxation_authority
            .as_ref()
            .map(api_version_reference)
            .transpose()?,
        revision: revision(version.revision)?,
        scope: api_scope(&version.scope),
        source: ApiVersionSource {
            actor: api_actor(&version.source.actor),
            request_id: version.source.request_id.clone(),
        },
        state: state_string(version.state).to_owned(),
        updated_at: version.updated_at.clone(),
        version: safe_integer(version.version, "version")?,
        version_digest: version.version_digest.clone(),
    })
}

fn definition(
    value: &ApiDefinition,
) -> Result<EnterprisePolicyDefinition, EnterprisePolicyApiError> {
    Ok(EnterprisePolicyDefinition {
        default_effect: effect(&value.default_effect)?,
        child_override_mode: child_override_mode(&value.child_override_mode)?,
        rules: value
            .rules
            .iter()
            .map(rule)
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn rule(value: &ApiRule) -> Result<EnterprisePolicyRule, EnterprisePolicyApiError> {
    Ok(EnterprisePolicyRule {
        kind: policy_kind(&value.kind)?,
        effect: effect(&value.effect)?,
        resource_pattern: value.resource_pattern.clone(),
        condition_sha256: value.condition_sha256.clone(),
    })
}

fn version_reference(
    value: &ApiVersionReference,
) -> Result<EnterprisePolicyVersionReference, EnterprisePolicyApiError> {
    Ok(EnterprisePolicyVersionReference {
        policy_id: value.policy_id.clone(),
        policy_kind: policy_kind(&value.policy_kind)?,
        scope: policy_scope(&value.scope),
        version: positive(value.version, "baseVersion.version")?,
        definition_sha256: value.definition_sha256.clone(),
        effective_definition_sha256: value.effective_definition_sha256.clone(),
        version_digest: value.version_digest.clone(),
    })
}

fn api_version_reference(
    value: &EnterprisePolicyVersionReference,
) -> Result<ApiVersionReference, EnterprisePolicyApiError> {
    Ok(ApiVersionReference {
        definition_sha256: value.definition_sha256.clone(),
        effective_definition_sha256: value.effective_definition_sha256.clone(),
        policy_id: value.policy_id.clone(),
        policy_kind: kind_string(value.policy_kind).to_owned(),
        scope: api_scope(&value.scope),
        version: safe_integer(value.version, "version reference")?,
        version_digest: value.version_digest.clone(),
    })
}

fn policy_scope(value: &Scope) -> EnterprisePolicyScope {
    match value {
        Scope::OrganizationScope(scope) => EnterprisePolicyScope::Organization {
            organization_id: scope.organization_id.clone(),
        },
        Scope::WorkspaceScope(scope) => EnterprisePolicyScope::Workspace {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
        },
        Scope::ProjectScope(scope) => EnterprisePolicyScope::Project {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
        },
        Scope::RepositoryScope(scope) => EnterprisePolicyScope::Repository {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
    }
}

fn api_scope(value: &EnterprisePolicyScope) -> Scope {
    match value {
        EnterprisePolicyScope::Organization { organization_id } => {
            Scope::OrganizationScope(OrganizationScope {
                kind: OrganizationScopeKind::Organization,
                organization_id: organization_id.clone(),
            })
        }
        EnterprisePolicyScope::Workspace {
            organization_id,
            workspace_id,
        } => Scope::WorkspaceScope(WorkspaceScope {
            kind: WorkspaceScopeKind::Workspace,
            organization_id: organization_id.clone(),
            workspace_id: workspace_id.clone(),
        }),
        EnterprisePolicyScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => Scope::ProjectScope(ProjectScope {
            kind: ProjectScopeKind::Project,
            organization_id: organization_id.clone(),
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
        }),
        EnterprisePolicyScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => Scope::RepositoryScope(RepositoryScope {
            kind: RepositoryScopeKind::Repository,
            organization_id: organization_id.clone(),
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            repository_id: repository_id.clone(),
        }),
    }
}

fn policy_actor(value: &Actor) -> EnterprisePolicyActor {
    match value {
        Actor::UserActor(actor) => EnterprisePolicyActor::User {
            id: actor.id.clone(),
        },
        Actor::ServiceAccountActor(actor) => EnterprisePolicyActor::ServiceAccount {
            id: actor.id.clone(),
        },
        Actor::SystemActor(actor) => EnterprisePolicyActor::System {
            id: actor.id.clone(),
        },
    }
}

fn api_actor(value: &EnterprisePolicyActor) -> Actor {
    match value {
        EnterprisePolicyActor::User { id } => Actor::UserActor(UserActor {
            id: id.clone(),
            kind: UserActorKind::User,
        }),
        EnterprisePolicyActor::ServiceAccount { id } => {
            Actor::ServiceAccountActor(ServiceAccountActor {
                id: id.clone(),
                kind: ServiceAccountActorKind::ServiceAccount,
            })
        }
        EnterprisePolicyActor::System { id } => Actor::SystemActor(SystemActor {
            id: id.clone(),
            kind: SystemActorKind::System,
        }),
    }
}

fn policy_kind(value: &str) -> Result<EnterprisePolicyKind, EnterprisePolicyApiError> {
    match value {
        "repository" => Ok(EnterprisePolicyKind::Repository),
        "model" => Ok(EnterprisePolicyKind::Model),
        "provider" => Ok(EnterprisePolicyKind::Provider),
        "tool" => Ok(EnterprisePolicyKind::Tool),
        "network" => Ok(EnterprisePolicyKind::Network),
        "approval" => Ok(EnterprisePolicyKind::Approval),
        "verifier" => Ok(EnterprisePolicyKind::Verifier),
        "worker_placement" => Ok(EnterprisePolicyKind::WorkerPlacement),
        "publication" => Ok(EnterprisePolicyKind::Publication),
        "retention" => Ok(EnterprisePolicyKind::Retention),
        "integration" => Ok(EnterprisePolicyKind::Integration),
        _ => Err(invalid("enterprise Policy kind is invalid")),
    }
}

const fn kind_string(value: EnterprisePolicyKind) -> &'static str {
    match value {
        EnterprisePolicyKind::Repository => "repository",
        EnterprisePolicyKind::Model => "model",
        EnterprisePolicyKind::Provider => "provider",
        EnterprisePolicyKind::Tool => "tool",
        EnterprisePolicyKind::Network => "network",
        EnterprisePolicyKind::Approval => "approval",
        EnterprisePolicyKind::Verifier => "verifier",
        EnterprisePolicyKind::WorkerPlacement => "worker_placement",
        EnterprisePolicyKind::Publication => "publication",
        EnterprisePolicyKind::Retention => "retention",
        EnterprisePolicyKind::Integration => "integration",
    }
}

fn policy_mode(value: &str) -> Result<EnterprisePolicyMode, EnterprisePolicyApiError> {
    match value {
        "enforce" => Ok(EnterprisePolicyMode::Enforce),
        "audit" => Ok(EnterprisePolicyMode::Audit),
        _ => Err(invalid("enterprise Policy mode is invalid")),
    }
}

const fn mode_string(value: EnterprisePolicyMode) -> &'static str {
    match value {
        EnterprisePolicyMode::Enforce => "enforce",
        EnterprisePolicyMode::Audit => "audit",
    }
}

fn policy_state(value: &str) -> Result<EnterprisePolicyState, EnterprisePolicyApiError> {
    match value {
        "draft" => Ok(EnterprisePolicyState::Draft),
        "active" => Ok(EnterprisePolicyState::Active),
        "retired" => Ok(EnterprisePolicyState::Retired),
        _ => Err(invalid("enterprise Policy state is invalid")),
    }
}

const fn state_string(value: EnterprisePolicyState) -> &'static str {
    match value {
        EnterprisePolicyState::Draft => "draft",
        EnterprisePolicyState::Active => "active",
        EnterprisePolicyState::Retired => "retired",
    }
}

fn effect(value: &str) -> Result<EnterprisePolicyEffect, EnterprisePolicyApiError> {
    match value {
        "allow" => Ok(EnterprisePolicyEffect::Allow),
        "deny" => Ok(EnterprisePolicyEffect::Deny),
        _ => Err(invalid("enterprise Policy effect is invalid")),
    }
}

fn child_override_mode(
    value: &str,
) -> Result<EnterprisePolicyChildOverrideMode, EnterprisePolicyApiError> {
    match value {
        "tighten_only" => Ok(EnterprisePolicyChildOverrideMode::TightenOnly),
        "allow_explicit_relaxation" => {
            Ok(EnterprisePolicyChildOverrideMode::AllowExplicitRelaxation)
        }
        _ => Err(invalid("enterprise Policy child override mode is invalid")),
    }
}

fn inheritance_mode(
    value: &str,
) -> Result<EnterprisePolicyInheritanceMode, EnterprisePolicyApiError> {
    match value {
        "tighten" => Ok(EnterprisePolicyInheritanceMode::Tighten),
        "override" => Ok(EnterprisePolicyInheritanceMode::Override),
        _ => Err(invalid("enterprise Policy inheritance mode is invalid")),
    }
}

const fn inheritance_mode_string(value: EnterprisePolicyInheritanceMode) -> &'static str {
    match value {
        EnterprisePolicyInheritanceMode::Tighten => "tighten",
        EnterprisePolicyInheritanceMode::Override => "override",
    }
}

fn encode_cursor(value: &EnterprisePolicyCursor) -> Result<OpaqueCursor, EnterprisePolicyApiError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| invalid("enterprise Policy cursor failed"))?;
    Ok(OpaqueCursor(URL_SAFE_NO_PAD.encode(bytes)))
}

fn decode_cursor(value: &OpaqueCursor) -> Result<EnterprisePolicyCursor, EnterprisePolicyApiError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(&value.0)
        .map_err(|_| invalid("enterprise Policy cursor is invalid"))?;
    serde_json::from_slice(&bytes).map_err(|_| invalid("enterprise Policy cursor is invalid"))
}

fn nonnegative(value: i64, field: &str) -> Result<u64, EnterprisePolicyApiError> {
    u64::try_from(value).map_err(|_| invalid(format!("{field} is negative")))
}

fn positive(value: i64, field: &str) -> Result<u64, EnterprisePolicyApiError> {
    let value = nonnegative(value, field)?;
    if value == 0 {
        return Err(invalid(format!("{field} must be positive")));
    }
    Ok(value)
}

fn safe_integer(value: u64, field: &str) -> Result<i64, EnterprisePolicyApiError> {
    i64::try_from(value).map_err(|_| invalid(format!("{field} exceeds the safe integer range")))
}

fn revision(value: u64) -> Result<Revision, EnterprisePolicyApiError> {
    safe_integer(value, "revision").map(Revision)
}

fn policy_error(error_value: &EnterprisePolicyError) -> EnterprisePolicyApiError {
    let kind = match error_value.kind() {
        EnterprisePolicyErrorKind::InvalidInput => EnterprisePolicyApiErrorKind::InvalidRequest,
        EnterprisePolicyErrorKind::RevisionConflict => {
            EnterprisePolicyApiErrorKind::RevisionConflict
        }
        EnterprisePolicyErrorKind::RequestConflict => EnterprisePolicyApiErrorKind::RequestConflict,
        EnterprisePolicyErrorKind::AuthorityMismatch => {
            EnterprisePolicyApiErrorKind::AuthorityMismatch
        }
        EnterprisePolicyErrorKind::NotFound => EnterprisePolicyApiErrorKind::NotFound,
        EnterprisePolicyErrorKind::CorruptState | EnterprisePolicyErrorKind::Storage => {
            EnterprisePolicyApiErrorKind::Unavailable
        }
    };
    EnterprisePolicyApiError {
        kind,
        message: error_value.to_string(),
    }
}

fn invalid(message: impl Into<String>) -> EnterprisePolicyApiError {
    EnterprisePolicyApiError {
        kind: EnterprisePolicyApiErrorKind::InvalidRequest,
        message: message.into(),
    }
}
