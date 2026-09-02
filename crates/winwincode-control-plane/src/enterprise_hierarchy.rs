// SPDX-License-Identifier: Apache-2.0

//! Canonical enterprise resource hierarchy.
//!
//! The hierarchy is deliberately independent from RBAC and Policy. Every
//! mutation is one organization-scoped product-state commit, while a secondary
//! resource index is updated in the same transaction. The index makes a
//! resource id globally unique and locates its tenant; the tenant aggregate
//! remains the only source used to reconstruct canonical scope after restart.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::ScopedResourceLocator;
use winwincode_domain::{
    Instant, OrganizationId, ProjectId, RepositoryId, RequestId, Sha256Digest, WorkspaceId,
};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, PublicEventActor, PublicEventScope,
    StateCommit, StateMutation, StateRevisionGuard, StorageError, StorageErrorKind, StoredState,
    public_receipt_identity,
};

const STATE_SCHEMA: &str = "winwincode.enterprise-hierarchy.v1";
const INDEX_SCHEMA: &str = "winwincode.enterprise-hierarchy-index.v1";
const STREAM_PREFIX: &str = "enterprise-hierarchy:";
const INDEX_PREFIX: &str = "enterprise-hierarchy-index:";
const EVENT_TOPIC: &str = "enterprise.hierarchy.mutated.v1";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_DISPLAY_NAME_BYTES: usize = 160;

/// Internal canonical identity for one enterprise Environment.
///
/// Environment is not part of the public v1 schema yet, so this type remains
/// owned by the hierarchy module until the one schema-generation window adds it.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct EnvironmentId(String);

impl EnvironmentId {
    /// Builds one canonical Environment id.
    ///
    /// # Errors
    ///
    /// Rejects values outside `env_` plus 26 Crockford Base32 characters.
    pub fn try_new(value: impl Into<String>) -> Result<Self, EnterpriseHierarchyError> {
        let value = value.into();
        validate_canonical_id(&value, "env_", "environmentId")?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Closed identity of every resource in the hierarchy.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(
    rename_all = "snake_case",
    tag = "kind",
    content = "id",
    deny_unknown_fields
)]
pub enum HierarchyResourceId {
    Organization(OrganizationId),
    Workspace(WorkspaceId),
    Project(ProjectId),
    Environment(EnvironmentId),
    Repository(RepositoryId),
}

impl HierarchyResourceId {
    fn kind(&self) -> HierarchyResourceKind {
        match self {
            Self::Organization(_) => HierarchyResourceKind::Organization,
            Self::Workspace(_) => HierarchyResourceKind::Workspace,
            Self::Project(_) => HierarchyResourceKind::Project,
            Self::Environment(_) => HierarchyResourceKind::Environment,
            Self::Repository(_) => HierarchyResourceKind::Repository,
        }
    }

    fn value(&self) -> &str {
        match self {
            Self::Organization(id) => &id.0,
            Self::Workspace(id) => &id.0,
            Self::Project(id) => &id.0,
            Self::Environment(id) => id.as_str(),
            Self::Repository(id) => &id.0,
        }
    }
}

/// Closed hierarchy levels.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HierarchyResourceKind {
    Organization,
    Workspace,
    Project,
    Environment,
    Repository,
}

impl HierarchyResourceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Organization => "organization",
            Self::Workspace => "workspace",
            Self::Project => "project",
            Self::Environment => "environment",
            Self::Repository => "repository",
        }
    }

    const fn parent_kind(self) -> Option<Self> {
        match self {
            Self::Organization => None,
            Self::Workspace => Some(Self::Organization),
            Self::Project => Some(Self::Workspace),
            Self::Environment | Self::Repository => Some(Self::Project),
        }
    }
}

/// Durable lifecycle state. Deleted records remain as immutable tombstones so
/// old receipts and attribution remain explainable.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HierarchyResourceState {
    Active,
    Archived,
    Deleted,
}

/// One canonical scope at the current hierarchy revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum HierarchyScope {
    Organization {
        organization_id: OrganizationId,
    },
    Workspace {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    },
    Project {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    },
    Environment {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        environment_id: EnvironmentId,
    },
    Repository {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        repository_id: RepositoryId,
    },
}

impl HierarchyScope {
    #[must_use]
    pub const fn organization_id(&self) -> &OrganizationId {
        match self {
            Self::Organization { organization_id }
            | Self::Workspace {
                organization_id, ..
            }
            | Self::Project {
                organization_id, ..
            }
            | Self::Environment {
                organization_id, ..
            }
            | Self::Repository {
                organization_id, ..
            } => organization_id,
        }
    }

    /// Converts a Repository scope to the existing generated locator.
    #[must_use]
    pub fn repository_locator(&self) -> Option<ScopedResourceLocator> {
        let Self::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } = self
        else {
            return None;
        };
        Some(ScopedResourceLocator {
            organization_id: organization_id.clone(),
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            repository_id: repository_id.clone(),
        })
    }
}

/// Current canonical resource record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HierarchyResource {
    pub id: HierarchyResourceId,
    pub parent: Option<HierarchyResourceId>,
    pub display_name: String,
    pub state: HierarchyResourceState,
    pub revision: u64,
    pub updated_at: Instant,
}

/// A resource together with its only canonical scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedHierarchyResource {
    pub resource: HierarchyResource,
    pub scope: HierarchyScope,
    /// Organization aggregate revision that authenticated `resource` and `scope`.
    pub hierarchy_revision: u64,
}

impl ResolvedHierarchyResource {
    /// Builds the exact durable guard required by a dependent atomic commit.
    ///
    /// # Errors
    ///
    /// Returns a bounded hierarchy error if the durable guard is invalid.
    pub fn state_guard(&self) -> Result<StateRevisionGuard, EnterpriseHierarchyError> {
        StateRevisionGuard::new(
            hierarchy_stream(self.scope.organization_id()),
            self.hierarchy_revision,
        )
        .map_err(|source| storage_error(&source))
    }
}

/// Closed hierarchy mutations. Parent types are checked at runtime so corrupt
/// or future callers cannot manufacture a cycle through an invalid edge.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum HierarchyMutation {
    Create {
        id: HierarchyResourceId,
        parent: Option<HierarchyResourceId>,
        display_name: String,
    },
    Move {
        id: HierarchyResourceId,
        new_parent: HierarchyResourceId,
    },
    Archive {
        id: HierarchyResourceId,
    },
    Delete {
        id: HierarchyResourceId,
    },
}

impl HierarchyMutation {
    fn resource_id(&self) -> &HierarchyResourceId {
        match self {
            Self::Create { id, .. }
            | Self::Move { id, .. }
            | Self::Archive { id }
            | Self::Delete { id } => id,
        }
    }
}

/// One authenticated, organization-scoped hierarchy command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseHierarchyCommand {
    pub actor: PublicEventActor,
    pub organization_id: OrganizationId,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub occurred_at: Instant,
    pub mutation: HierarchyMutation,
}

/// Durable mutation result. Replays return the original bytes and revision,
/// not a projection recomputed from newer hierarchy state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseHierarchyReceipt {
    pub previous_revision: u64,
    pub current_revision: u64,
    pub resource: HierarchyResource,
    pub scope: HierarchyScope,
    pub idempotent_replay: bool,
}

/// Stable hierarchy failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterpriseHierarchyErrorKind {
    InvalidInput,
    NotFound,
    AlreadyExists,
    RevisionConflict,
    RequestConflict,
    CrossTenantReference,
    InvalidParent,
    Cycle,
    Archived,
    Deleted,
    DescendantsExist,
    CorruptState,
    Storage,
}

/// Secret-free hierarchy error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseHierarchyError {
    kind: EnterpriseHierarchyErrorKind,
    message: String,
}

impl EnterpriseHierarchyError {
    #[must_use]
    pub const fn kind(&self) -> EnterpriseHierarchyErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterpriseHierarchyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EnterpriseHierarchyError {}

/// Storage-independent hierarchy application service.
pub struct EnterpriseHierarchyService {
    storage: Mutex<Box<dyn ProductStateStorage>>,
}

impl EnterpriseHierarchyService {
    #[must_use]
    pub fn new(storage: Box<dyn ProductStateStorage>) -> Self {
        Self {
            storage: Mutex::new(storage),
        }
    }

    /// Applies one atomic hierarchy mutation or returns its exact durable replay.
    ///
    /// # Errors
    ///
    /// Rejects malformed ids, stale revisions, changed request reuse, invalid
    /// parents, cross-tenant edges, lifecycle violations, corruption, and
    /// storage failures.
    pub fn mutate(
        &self,
        command: &EnterpriseHierarchyCommand,
    ) -> Result<EnterpriseHierarchyReceipt, EnterpriseHierarchyError> {
        validate_command(command)?;
        let digest = command_digest(command)?;
        let identity = public_receipt_identity(
            &command.actor,
            &PublicEventScope::Organization {
                organization_id: command.organization_id.clone(),
            },
            command.request_id.clone(),
        )
        .map_err(|source| storage_error(&source))?;
        let mut storage = self.lock()?;
        if let Some(receipt) = storage
            .load_receipt(&identity, &digest)
            .map_err(|source| storage_error(&source))?
        {
            return receipt_result(&receipt, true);
        }
        let stored = storage
            .load_state(&hierarchy_stream(&command.organization_id))
            .map_err(|source| storage_error(&source))?;
        let mut state = decode_or_empty(stored.as_ref(), &command.organization_id)?;
        if state.revision != command.expected_revision {
            return Err(error(
                EnterpriseHierarchyErrorKind::RevisionConflict,
                "enterprise hierarchy expected revision is stale",
            ));
        }
        let previous_resource_revision = apply_mutation(&**storage, &mut state, command)?;
        state.revision = state
            .revision
            .checked_add(1)
            .filter(|revision| *revision <= MAX_SAFE_INTEGER)
            .ok_or_else(|| invalid("enterprise hierarchy revision is exhausted"))?;
        let resource = state
            .resource(command.mutation.resource_id())
            .cloned()
            .ok_or_else(corrupt)?;
        let scope = resolve_scope_in_state(&state, &resource)?;
        let result = StoredMutationResult {
            previous_revision: command.expected_revision,
            current_revision: state.revision,
            resource: resource.clone(),
            scope: scope.clone(),
        };
        let event_payload = serde_json::to_vec(&result).map_err(|_| corrupt())?;
        let index_entry = HierarchyIndexEntry {
            schema: INDEX_SCHEMA.to_owned(),
            organization_id: command.organization_id.clone(),
            resource_id: resource.id.clone(),
            resource_revision: resource.revision,
        };
        let commit = StateCommit::new(
            identity,
            digest.clone(),
            hierarchy_stream(&command.organization_id),
            command.expected_revision,
            serde_json::to_vec(&state).map_err(|_| corrupt())?,
            vec![NewOutboxEvent::internal(
                event_id(&digest),
                EVENT_TOPIC,
                event_payload,
            )],
        )
        .with_state_mutation(
            StateMutation::new(
                index_stream(command.mutation.resource_id()),
                previous_resource_revision,
                serde_json::to_vec(&index_entry).map_err(|_| corrupt())?,
            )
            .map_err(|source| storage_error(&source))?,
        );
        let receipt = storage
            .commit(&commit)
            .map_err(|source| storage_error(&source))?;
        receipt_result(&receipt, receipt.idempotent_replay)
    }

    /// Resolves one resource id to its canonical scope without a tenant scan.
    ///
    /// # Errors
    ///
    /// Rejects invalid ids, missing records, corrupt index rows, and storage failures.
    pub fn resolve(
        &self,
        id: &HierarchyResourceId,
    ) -> Result<ResolvedHierarchyResource, EnterpriseHierarchyError> {
        validate_resource_id(id)?;
        let storage = self.lock()?;
        let stored = storage
            .load_state(&index_stream(id))
            .map_err(|source| storage_error(&source))?
            .ok_or_else(|| not_found("enterprise hierarchy resource does not exist"))?;
        let entry = decode_index(&stored, id)?;
        let hierarchy = storage
            .load_state(&hierarchy_stream(&entry.organization_id))
            .map_err(|source| storage_error(&source))?
            .ok_or_else(corrupt)?;
        let state = decode_or_empty(Some(&hierarchy), &entry.organization_id)?;
        let resource = state.resource(id).cloned().ok_or_else(corrupt)?;
        let scope = resolve_scope_in_state(&state, &resource)?;
        if resource.revision != entry.resource_revision {
            return Err(corrupt());
        }
        Ok(ResolvedHierarchyResource {
            resource,
            scope,
            hierarchy_revision: state.revision,
        })
    }

    /// Resolves the existing generated Repository locator from the one durable index.
    ///
    /// # Errors
    ///
    /// Rejects a missing, deleted, non-Repository, or corrupt resource.
    pub fn repository_locator(
        &self,
        repository_id: &RepositoryId,
    ) -> Result<ScopedResourceLocator, EnterpriseHierarchyError> {
        let resolved = self.resolve(&HierarchyResourceId::Repository(repository_id.clone()))?;
        if resolved.resource.state == HierarchyResourceState::Deleted {
            return Err(error(
                EnterpriseHierarchyErrorKind::Deleted,
                "enterprise Repository was deleted",
            ));
        }
        resolved.scope.repository_locator().ok_or_else(corrupt)
    }

    fn lock(
        &self,
    ) -> Result<MutexGuard<'_, Box<dyn ProductStateStorage>>, EnterpriseHierarchyError> {
        self.storage
            .lock()
            .map_err(|_| storage_failure("enterprise hierarchy storage lock is poisoned"))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HierarchyState {
    schema: String,
    organization_id: OrganizationId,
    revision: u64,
    organization: Option<HierarchyResource>,
    workspaces: BTreeMap<String, HierarchyResource>,
    projects: BTreeMap<String, HierarchyResource>,
    environments: BTreeMap<String, HierarchyResource>,
    repositories: BTreeMap<String, HierarchyResource>,
}

impl HierarchyState {
    fn empty(organization_id: &OrganizationId) -> Self {
        Self {
            schema: STATE_SCHEMA.to_owned(),
            organization_id: organization_id.clone(),
            revision: 0,
            organization: None,
            workspaces: BTreeMap::new(),
            projects: BTreeMap::new(),
            environments: BTreeMap::new(),
            repositories: BTreeMap::new(),
        }
    }

    fn resource(&self, id: &HierarchyResourceId) -> Option<&HierarchyResource> {
        match id {
            HierarchyResourceId::Organization(organization_id) => self
                .organization
                .as_ref()
                .filter(|resource| resource.id.value() == organization_id.0),
            HierarchyResourceId::Workspace(workspace_id) => self.workspaces.get(&workspace_id.0),
            HierarchyResourceId::Project(project_id) => self.projects.get(&project_id.0),
            HierarchyResourceId::Environment(environment_id) => {
                self.environments.get(environment_id.as_str())
            }
            HierarchyResourceId::Repository(repository_id) => {
                self.repositories.get(&repository_id.0)
            }
        }
    }

    fn resource_mut(&mut self, id: &HierarchyResourceId) -> Option<&mut HierarchyResource> {
        match id {
            HierarchyResourceId::Organization(organization_id) => self
                .organization
                .as_mut()
                .filter(|resource| resource.id.value() == organization_id.0),
            HierarchyResourceId::Workspace(workspace_id) => {
                self.workspaces.get_mut(&workspace_id.0)
            }
            HierarchyResourceId::Project(project_id) => self.projects.get_mut(&project_id.0),
            HierarchyResourceId::Environment(environment_id) => {
                self.environments.get_mut(environment_id.as_str())
            }
            HierarchyResourceId::Repository(repository_id) => {
                self.repositories.get_mut(&repository_id.0)
            }
        }
    }

    fn insert(&mut self, resource: HierarchyResource) {
        match &resource.id {
            HierarchyResourceId::Organization(_) => self.organization = Some(resource),
            HierarchyResourceId::Workspace(id) => {
                self.workspaces.insert(id.0.clone(), resource);
            }
            HierarchyResourceId::Project(id) => {
                self.projects.insert(id.0.clone(), resource);
            }
            HierarchyResourceId::Environment(id) => {
                self.environments.insert(id.as_str().to_owned(), resource);
            }
            HierarchyResourceId::Repository(id) => {
                self.repositories.insert(id.0.clone(), resource);
            }
        }
    }

    fn resources(&self) -> impl Iterator<Item = &HierarchyResource> {
        self.organization
            .iter()
            .chain(self.workspaces.values())
            .chain(self.projects.values())
            .chain(self.environments.values())
            .chain(self.repositories.values())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HierarchyIndexEntry {
    schema: String,
    organization_id: OrganizationId,
    resource_id: HierarchyResourceId,
    resource_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredMutationResult {
    previous_revision: u64,
    current_revision: u64,
    resource: HierarchyResource,
    scope: HierarchyScope,
}

fn validate_command(command: &EnterpriseHierarchyCommand) -> Result<(), EnterpriseHierarchyError> {
    validate_canonical_id(&command.organization_id.0, "org_", "organizationId")?;
    validate_resource_id(command.mutation.resource_id())?;
    if command.expected_revision > MAX_SAFE_INTEGER {
        return Err(invalid("enterprise hierarchy expected revision is invalid"));
    }
    validate_instant(&command.occurred_at)?;
    if let HierarchyMutation::Create {
        parent,
        display_name,
        ..
    } = &command.mutation
    {
        if let Some(parent) = parent {
            validate_resource_id(parent)?;
        }
        validate_display_name(display_name)?;
    }
    if let HierarchyMutation::Move { new_parent, .. } = &command.mutation {
        validate_resource_id(new_parent)?;
    }
    Ok(())
}

fn validate_resource_id(id: &HierarchyResourceId) -> Result<(), EnterpriseHierarchyError> {
    let (value, prefix, label): (&str, &str, &str) = match id {
        HierarchyResourceId::Organization(id) => (id.0.as_str(), "org_", "organizationId"),
        HierarchyResourceId::Workspace(id) => (id.0.as_str(), "wsp_", "workspaceId"),
        HierarchyResourceId::Project(id) => (id.0.as_str(), "prj_", "projectId"),
        HierarchyResourceId::Environment(id) => (id.as_str(), "env_", "environmentId"),
        HierarchyResourceId::Repository(id) => (id.0.as_str(), "rep_", "repositoryId"),
    };
    validate_canonical_id(value, prefix, label)
}

fn validate_canonical_id(
    value: &str,
    prefix: &str,
    label: &str,
) -> Result<(), EnterpriseHierarchyError> {
    let valid = value
        .strip_prefix(prefix)
        .is_some_and(|suffix| suffix.len() == 26 && suffix.bytes().all(is_crockford_base32));
    if !valid {
        return Err(invalid(format!(
            "enterprise hierarchy {label} is not canonical"
        )));
    }
    Ok(())
}

const fn is_crockford_base32(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9'
            | b'A'..=b'H'
            | b'J'
            | b'K'
            | b'M'
            | b'N'
            | b'P'..=b'T'
            | b'V'..=b'Z'
    )
}

fn validate_display_name(value: &str) -> Result<(), EnterpriseHierarchyError> {
    if value.trim().is_empty()
        || value.len() > MAX_DISPLAY_NAME_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(invalid(
            "enterprise hierarchy display name is outside its bounded format",
        ));
    }
    Ok(())
}

fn validate_instant(value: &Instant) -> Result<(), EnterpriseHierarchyError> {
    let bytes = value.0.as_bytes();
    let punctuation = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'.'),
    ];
    let valid = bytes.len() == 24
        && bytes[23] == b'Z'
        && punctuation
            .iter()
            .all(|(index, expected)| bytes[*index] == *expected)
        && bytes.iter().enumerate().all(|(index, byte)| {
            index == 23 || punctuation.iter().any(|(at, _)| *at == index) || byte.is_ascii_digit()
        });
    if !valid {
        return Err(invalid("enterprise hierarchy occurredAt is invalid"));
    }
    Ok(())
}

fn apply_mutation(
    storage: &dyn ProductStateStorage,
    state: &mut HierarchyState,
    command: &EnterpriseHierarchyCommand,
) -> Result<u64, EnterpriseHierarchyError> {
    match &command.mutation {
        HierarchyMutation::Create {
            id,
            parent,
            display_name,
        } => apply_create(storage, state, command, id, parent.as_ref(), display_name),
        HierarchyMutation::Move { id, new_parent } => {
            apply_move(storage, state, command, id, new_parent)
        }
        HierarchyMutation::Archive { id } => apply_archive(state, command, id),
        HierarchyMutation::Delete { id } => apply_delete(state, command, id),
    }
}

fn apply_create(
    storage: &dyn ProductStateStorage,
    state: &mut HierarchyState,
    command: &EnterpriseHierarchyCommand,
    id: &HierarchyResourceId,
    parent: Option<&HierarchyResourceId>,
    display_name: &str,
) -> Result<u64, EnterpriseHierarchyError> {
    if let Some(stored) = storage
        .load_state(&index_stream(id))
        .map_err(|source| storage_error(&source))?
    {
        let entry = decode_index(&stored, id)?;
        let kind = if entry.organization_id == command.organization_id {
            EnterpriseHierarchyErrorKind::AlreadyExists
        } else {
            EnterpriseHierarchyErrorKind::CrossTenantReference
        };
        return Err(error(
            kind,
            "enterprise hierarchy resource id is already owned",
        ));
    }
    validate_parent(storage, state, command, id, parent)?;
    let resource = HierarchyResource {
        id: id.clone(),
        parent: parent.cloned(),
        display_name: display_name.to_owned(),
        state: HierarchyResourceState::Active,
        revision: 1,
        updated_at: command.occurred_at.clone(),
    };
    state.insert(resource);
    Ok(0)
}

fn validate_parent(
    storage: &dyn ProductStateStorage,
    state: &HierarchyState,
    command: &EnterpriseHierarchyCommand,
    id: &HierarchyResourceId,
    parent: Option<&HierarchyResourceId>,
) -> Result<(), EnterpriseHierarchyError> {
    match (id.kind().parent_kind(), parent) {
        (None, None) => {
            let HierarchyResourceId::Organization(organization_id) = id else {
                return Err(corrupt());
            };
            if organization_id != &command.organization_id {
                return Err(cross_tenant());
            }
            Ok(())
        }
        (None, Some(_)) | (Some(_), None) => Err(error(
            EnterpriseHierarchyErrorKind::InvalidParent,
            "enterprise hierarchy parent is missing or forbidden",
        )),
        (Some(expected), Some(parent)) if parent.kind() != expected => Err(error(
            EnterpriseHierarchyErrorKind::InvalidParent,
            "enterprise hierarchy parent level is invalid",
        )),
        (Some(_), Some(parent)) => require_active_parent(storage, state, command, parent),
    }
}

fn require_active_parent(
    storage: &dyn ProductStateStorage,
    state: &HierarchyState,
    command: &EnterpriseHierarchyCommand,
    parent: &HierarchyResourceId,
) -> Result<(), EnterpriseHierarchyError> {
    if let Some(resource) = state.resource(parent) {
        return match resource.state {
            HierarchyResourceState::Active => Ok(()),
            HierarchyResourceState::Archived => Err(error(
                EnterpriseHierarchyErrorKind::Archived,
                "enterprise hierarchy parent is archived",
            )),
            HierarchyResourceState::Deleted => Err(error(
                EnterpriseHierarchyErrorKind::Deleted,
                "enterprise hierarchy parent is deleted",
            )),
        };
    }
    let Some(stored) = storage
        .load_state(&index_stream(parent))
        .map_err(|source| storage_error(&source))?
    else {
        return Err(not_found("enterprise hierarchy parent does not exist"));
    };
    let entry = decode_index(&stored, parent)?;
    if entry.organization_id != command.organization_id {
        return Err(cross_tenant());
    }
    Err(corrupt())
}

fn apply_move(
    storage: &dyn ProductStateStorage,
    state: &mut HierarchyState,
    command: &EnterpriseHierarchyCommand,
    id: &HierarchyResourceId,
    new_parent: &HierarchyResourceId,
) -> Result<u64, EnterpriseHierarchyError> {
    if id == new_parent {
        return Err(error(
            EnterpriseHierarchyErrorKind::Cycle,
            "enterprise hierarchy move would create a cycle",
        ));
    }
    if id.kind() == HierarchyResourceKind::Organization {
        return Err(error(
            EnterpriseHierarchyErrorKind::InvalidParent,
            "enterprise Organization cannot be moved",
        ));
    }
    validate_parent(storage, state, command, id, Some(new_parent))?;
    let resource = state
        .resource_mut(id)
        .ok_or_else(|| not_found("enterprise hierarchy resource does not exist"))?;
    require_active(resource)?;
    if resource.parent.as_ref() == Some(new_parent) {
        return Err(invalid(
            "enterprise hierarchy move does not change the parent",
        ));
    }
    let previous_revision = resource.revision;
    resource.parent = Some(new_parent.clone());
    advance_resource(resource, &command.occurred_at)?;
    Ok(previous_revision)
}

fn apply_archive(
    state: &mut HierarchyState,
    command: &EnterpriseHierarchyCommand,
    id: &HierarchyResourceId,
) -> Result<u64, EnterpriseHierarchyError> {
    let current = state
        .resource(id)
        .ok_or_else(|| not_found("enterprise hierarchy resource does not exist"))?;
    require_active(current)?;
    if state.resources().any(|resource| {
        resource.parent.as_ref() == Some(id) && resource.state == HierarchyResourceState::Active
    }) {
        return Err(error(
            EnterpriseHierarchyErrorKind::DescendantsExist,
            "enterprise hierarchy resource has active children",
        ));
    }
    let resource = state.resource_mut(id).ok_or_else(corrupt)?;
    let previous_revision = resource.revision;
    resource.state = HierarchyResourceState::Archived;
    advance_resource(resource, &command.occurred_at)?;
    Ok(previous_revision)
}

fn apply_delete(
    state: &mut HierarchyState,
    command: &EnterpriseHierarchyCommand,
    id: &HierarchyResourceId,
) -> Result<u64, EnterpriseHierarchyError> {
    let current = state
        .resource(id)
        .ok_or_else(|| not_found("enterprise hierarchy resource does not exist"))?;
    match current.state {
        HierarchyResourceState::Active => {
            return Err(error(
                EnterpriseHierarchyErrorKind::Archived,
                "enterprise hierarchy resource must be archived before deletion",
            ));
        }
        HierarchyResourceState::Deleted => {
            return Err(error(
                EnterpriseHierarchyErrorKind::Deleted,
                "enterprise hierarchy resource is already deleted",
            ));
        }
        HierarchyResourceState::Archived => {}
    }
    if state.resources().any(|resource| {
        resource.parent.as_ref() == Some(id) && resource.state != HierarchyResourceState::Deleted
    }) {
        return Err(error(
            EnterpriseHierarchyErrorKind::DescendantsExist,
            "enterprise hierarchy resource still has retained children",
        ));
    }
    let resource = state.resource_mut(id).ok_or_else(corrupt)?;
    let previous_revision = resource.revision;
    resource.state = HierarchyResourceState::Deleted;
    advance_resource(resource, &command.occurred_at)?;
    Ok(previous_revision)
}

fn require_active(resource: &HierarchyResource) -> Result<(), EnterpriseHierarchyError> {
    match resource.state {
        HierarchyResourceState::Active => Ok(()),
        HierarchyResourceState::Archived => Err(error(
            EnterpriseHierarchyErrorKind::Archived,
            "enterprise hierarchy resource is archived",
        )),
        HierarchyResourceState::Deleted => Err(error(
            EnterpriseHierarchyErrorKind::Deleted,
            "enterprise hierarchy resource is deleted",
        )),
    }
}

fn advance_resource(
    resource: &mut HierarchyResource,
    occurred_at: &Instant,
) -> Result<(), EnterpriseHierarchyError> {
    resource.revision = resource
        .revision
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(|| invalid("enterprise hierarchy resource revision is exhausted"))?;
    resource.updated_at = occurred_at.clone();
    Ok(())
}

fn resolve_scope_in_state(
    state: &HierarchyState,
    resource: &HierarchyResource,
) -> Result<HierarchyScope, EnterpriseHierarchyError> {
    match &resource.id {
        HierarchyResourceId::Organization(organization_id) => Ok(HierarchyScope::Organization {
            organization_id: organization_id.clone(),
        }),
        HierarchyResourceId::Workspace(workspace_id) => Ok(HierarchyScope::Workspace {
            organization_id: state.organization_id.clone(),
            workspace_id: workspace_id.clone(),
        }),
        HierarchyResourceId::Project(project_id) => {
            let workspace_id = parent_workspace(resource)?;
            Ok(HierarchyScope::Project {
                organization_id: state.organization_id.clone(),
                workspace_id,
                project_id: project_id.clone(),
            })
        }
        HierarchyResourceId::Environment(environment_id) => {
            let project = parent_project(resource)?;
            let workspace_id = workspace_for_project(state, &project)?;
            Ok(HierarchyScope::Environment {
                organization_id: state.organization_id.clone(),
                workspace_id,
                project_id: project,
                environment_id: environment_id.clone(),
            })
        }
        HierarchyResourceId::Repository(repository_id) => {
            let project = parent_project(resource)?;
            let workspace_id = workspace_for_project(state, &project)?;
            Ok(HierarchyScope::Repository {
                organization_id: state.organization_id.clone(),
                workspace_id,
                project_id: project,
                repository_id: repository_id.clone(),
            })
        }
    }
}

fn parent_workspace(resource: &HierarchyResource) -> Result<WorkspaceId, EnterpriseHierarchyError> {
    match resource.parent.as_ref() {
        Some(HierarchyResourceId::Workspace(workspace_id)) => Ok(workspace_id.clone()),
        _ => Err(corrupt()),
    }
}

fn parent_project(resource: &HierarchyResource) -> Result<ProjectId, EnterpriseHierarchyError> {
    match resource.parent.as_ref() {
        Some(HierarchyResourceId::Project(project_id)) => Ok(project_id.clone()),
        _ => Err(corrupt()),
    }
}

fn workspace_for_project(
    state: &HierarchyState,
    project_id: &ProjectId,
) -> Result<WorkspaceId, EnterpriseHierarchyError> {
    let project = state.projects.get(&project_id.0).ok_or_else(corrupt)?;
    parent_workspace(project)
}

fn decode_or_empty(
    stored: Option<&StoredState>,
    organization_id: &OrganizationId,
) -> Result<HierarchyState, EnterpriseHierarchyError> {
    let Some(stored) = stored else {
        return Ok(HierarchyState::empty(organization_id));
    };
    let state: HierarchyState = serde_json::from_slice(&stored.payload).map_err(|_| corrupt())?;
    if state.schema != STATE_SCHEMA
        || &state.organization_id != organization_id
        || state.revision != stored.revision
        || state.revision == 0
        || state.revision > MAX_SAFE_INTEGER
        || serde_json::to_vec(&state).map_err(|_| corrupt())? != stored.payload
    {
        return Err(corrupt());
    }
    validate_state(&state)?;
    Ok(state)
}

fn validate_state(state: &HierarchyState) -> Result<(), EnterpriseHierarchyError> {
    validate_canonical_id(&state.organization_id.0, "org_", "organizationId")
        .map_err(|_| corrupt())?;
    let Some(organization) = state.organization.as_ref() else {
        return Err(corrupt());
    };
    if organization.id != HierarchyResourceId::Organization(state.organization_id.clone())
        || organization.parent.is_some()
    {
        return Err(corrupt());
    }
    if !state
        .workspaces
        .iter()
        .all(|(key, resource)| matches!(&resource.id, HierarchyResourceId::Workspace(id) if &id.0 == key))
        || !state
            .projects
            .iter()
            .all(|(key, resource)| matches!(&resource.id, HierarchyResourceId::Project(id) if &id.0 == key))
        || !state
            .environments
            .iter()
            .all(|(key, resource)| matches!(&resource.id, HierarchyResourceId::Environment(id) if id.as_str() == key))
        || !state
            .repositories
            .iter()
            .all(|(key, resource)| matches!(&resource.id, HierarchyResourceId::Repository(id) if &id.0 == key))
    {
        return Err(corrupt());
    }
    for resource in state.resources() {
        validate_resource_id(&resource.id).map_err(|_| corrupt())?;
        validate_display_name(&resource.display_name).map_err(|_| corrupt())?;
        validate_instant(&resource.updated_at).map_err(|_| corrupt())?;
        if resource.revision == 0 || resource.revision > MAX_SAFE_INTEGER {
            return Err(corrupt());
        }
        let expected_parent = resource.id.kind().parent_kind();
        if resource.parent.as_ref().map(HierarchyResourceId::kind) != expected_parent {
            return Err(corrupt());
        }
        if let Some(parent) = &resource.parent {
            let parent = state.resource(parent).ok_or_else(corrupt)?;
            if (parent.state == HierarchyResourceState::Archived
                && resource.state == HierarchyResourceState::Active)
                || (parent.state == HierarchyResourceState::Deleted
                    && resource.state != HierarchyResourceState::Deleted)
            {
                return Err(corrupt());
            }
        }
        resolve_scope_in_state(state, resource)?;
    }
    Ok(())
}

fn decode_index(
    stored: &StoredState,
    expected_id: &HierarchyResourceId,
) -> Result<HierarchyIndexEntry, EnterpriseHierarchyError> {
    let entry: HierarchyIndexEntry =
        serde_json::from_slice(&stored.payload).map_err(|_| corrupt())?;
    if entry.schema != INDEX_SCHEMA
        || &entry.resource_id != expected_id
        || entry.resource_revision != stored.revision
        || entry.resource_revision == 0
        || entry.resource_revision > MAX_SAFE_INTEGER
        || serde_json::to_vec(&entry).map_err(|_| corrupt())? != stored.payload
    {
        return Err(corrupt());
    }
    validate_resource_id(&entry.resource_id).map_err(|_| corrupt())?;
    Ok(entry)
}

fn receipt_result(
    receipt: &CommitReceipt,
    idempotent_replay: bool,
) -> Result<EnterpriseHierarchyReceipt, EnterpriseHierarchyError> {
    let [event] = receipt.events.as_slice() else {
        return Err(corrupt());
    };
    if event.topic != EVENT_TOPIC {
        return Err(corrupt());
    }
    let stored: StoredMutationResult =
        serde_json::from_slice(&event.payload).map_err(|_| corrupt())?;
    if stored.current_revision != receipt.revision
        || stored.current_revision != stored.previous_revision.saturating_add(1)
        || receipt.stream_id != hierarchy_stream(stored.scope.organization_id())
        || stored.resource.revision == 0
        || stored.resource.revision > MAX_SAFE_INTEGER
        || !scope_matches_resource(&stored.scope, &stored.resource.id)
    {
        return Err(corrupt());
    }
    Ok(EnterpriseHierarchyReceipt {
        previous_revision: stored.previous_revision,
        current_revision: stored.current_revision,
        resource: stored.resource,
        scope: stored.scope,
        idempotent_replay,
    })
}

fn scope_matches_resource(scope: &HierarchyScope, id: &HierarchyResourceId) -> bool {
    matches!(
        (scope, id),
        (
            HierarchyScope::Organization { organization_id },
            HierarchyResourceId::Organization(id),
        ) if organization_id == id
    ) || matches!(
        (scope, id),
        (
            HierarchyScope::Workspace { workspace_id, .. },
            HierarchyResourceId::Workspace(id),
        ) if workspace_id == id
    ) || matches!(
        (scope, id),
        (
            HierarchyScope::Project { project_id, .. },
            HierarchyResourceId::Project(id),
        ) if project_id == id
    ) || matches!(
        (scope, id),
        (
            HierarchyScope::Environment { environment_id, .. },
            HierarchyResourceId::Environment(id),
        ) if environment_id == id
    ) || matches!(
        (scope, id),
        (
            HierarchyScope::Repository { repository_id, .. },
            HierarchyResourceId::Repository(id),
        ) if repository_id == id
    )
}

fn command_digest(
    command: &EnterpriseHierarchyCommand,
) -> Result<Sha256Digest, EnterpriseHierarchyError> {
    let encoded = serde_json::to_vec(command).map_err(|_| corrupt())?;
    let mut digest = Sha256::new();
    digest.update(b"winwincode.enterprise-hierarchy.command.v1\0");
    digest.update(encoded);
    Ok(Sha256Digest(format!("sha256:{:x}", digest.finalize())))
}

fn event_id(digest: &Sha256Digest) -> String {
    format!(
        "evt_enterprise_hierarchy_{}",
        digest.0.strip_prefix("sha256:").unwrap_or(&digest.0)
    )
}

fn hierarchy_stream(organization_id: &OrganizationId) -> String {
    format!("{STREAM_PREFIX}{}", organization_id.0)
}

fn index_stream(id: &HierarchyResourceId) -> String {
    format!("{INDEX_PREFIX}{}:{}", id.kind().as_str(), id.value())
}

fn storage_error(source: &StorageError) -> EnterpriseHierarchyError {
    let kind = match source.kind() {
        StorageErrorKind::InvalidInput => EnterpriseHierarchyErrorKind::InvalidInput,
        StorageErrorKind::RevisionConflict => EnterpriseHierarchyErrorKind::RevisionConflict,
        StorageErrorKind::RequestConflict => EnterpriseHierarchyErrorKind::RequestConflict,
        StorageErrorKind::RequestReplayMissing
        | StorageErrorKind::JournalAlreadyExists
        | StorageErrorKind::JournalNotFound
        | StorageErrorKind::JournalConflict
        | StorageErrorKind::EventCursorExpired
        | StorageErrorKind::Adapter
        | StorageErrorKind::Closed => EnterpriseHierarchyErrorKind::Storage,
    };
    error(kind, "enterprise hierarchy storage operation failed")
}

fn invalid(message: impl Into<String>) -> EnterpriseHierarchyError {
    error(EnterpriseHierarchyErrorKind::InvalidInput, message)
}

fn not_found(message: impl Into<String>) -> EnterpriseHierarchyError {
    error(EnterpriseHierarchyErrorKind::NotFound, message)
}

fn cross_tenant() -> EnterpriseHierarchyError {
    error(
        EnterpriseHierarchyErrorKind::CrossTenantReference,
        "enterprise hierarchy reference belongs to another Organization",
    )
}

fn corrupt() -> EnterpriseHierarchyError {
    error(
        EnterpriseHierarchyErrorKind::CorruptState,
        "enterprise hierarchy durable state is corrupt",
    )
}

fn storage_failure(message: impl Into<String>) -> EnterpriseHierarchyError {
    error(EnterpriseHierarchyErrorKind::Storage, message)
}

fn error(
    kind: EnterpriseHierarchyErrorKind,
    message: impl Into<String>,
) -> EnterpriseHierarchyError {
    EnterpriseHierarchyError {
        kind,
        message: message.into(),
    }
}
