// SPDX-License-Identifier: Apache-2.0

//! Canonical enterprise resource-to-scope bindings and local migration.
//!
//! A binding stores the hierarchy resource identity rather than a copied path.
//! Reads therefore reconstruct the current canonical scope through
//! [`crate::EnterpriseHierarchyService`]. One Organization aggregate owns all
//! of its bindings. Sixteen global index shards enforce cross-tenant subject
//! uniqueness without limiting an atomic local migration to sixteen records.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    CredentialReferenceId, DeliveryId, EnterpriseIntegrationId, EnterprisePolicyId,
    EnterpriseWorkerPoolId, Instant, OrganizationId, RepositoryId, RequestId, Sha256Digest,
};
use winwincode_storage::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, PublicEventActor, PublicEventScope,
    StateCommit, StateMutation, StorageError, StorageErrorKind, StoredState,
    public_receipt_identity,
};

use crate::{
    EnterpriseHierarchyError, EnterpriseHierarchyErrorKind, EnterpriseHierarchyService,
    HierarchyResourceId, HierarchyResourceState, HierarchyScope, ResolvedHierarchyResource,
};

const STATE_SCHEMA: &str = "winwincode.enterprise-scope-bindings.v1";
const INDEX_SCHEMA: &str = "winwincode.enterprise-scope-binding-index.v1";
const STREAM_PREFIX: &str = "enterprise-scope-bindings:";
const INDEX_PREFIX: &str = "enterprise-scope-binding-index:";
const EVENT_TOPIC: &str = "enterprise.scope-binding.mutated.v1";
const INDEX_SHARD_COUNT: u8 = 16;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_LOCAL_MIGRATION_BINDINGS: usize = 10_000;
const DEFAULT_ORGANIZATION_ID: &str = "org_00000000000000000000000000";
const DEFAULT_REPOSITORY_ID: &str = "rep_00000000000000000000000000";

/// Every durable resource class that must carry one explicit enterprise scope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    rename_all = "snake_case",
    tag = "kind",
    content = "id",
    deny_unknown_fields
)]
pub enum ScopeBindingSubject {
    Delivery(DeliveryId),
    ProviderSettings(Sha256Digest),
    CredentialReference(CredentialReferenceId),
    Policy(EnterprisePolicyId),
    Integration(EnterpriseIntegrationId),
    WorkerPool(EnterpriseWorkerPoolId),
    Usage(Sha256Digest),
}

impl ScopeBindingSubject {
    fn kind(&self) -> ScopeBindingSubjectKind {
        match self {
            Self::Delivery(_) => ScopeBindingSubjectKind::Delivery,
            Self::ProviderSettings(_) => ScopeBindingSubjectKind::ProviderSettings,
            Self::CredentialReference(_) => ScopeBindingSubjectKind::CredentialReference,
            Self::Policy(_) => ScopeBindingSubjectKind::Policy,
            Self::Integration(_) => ScopeBindingSubjectKind::Integration,
            Self::WorkerPool(_) => ScopeBindingSubjectKind::WorkerPool,
            Self::Usage(_) => ScopeBindingSubjectKind::Usage,
        }
    }

    fn value(&self) -> &str {
        match self {
            Self::Delivery(id) => &id.0,
            Self::ProviderSettings(id) | Self::Usage(id) => &id.0,
            Self::CredentialReference(id) => &id.0,
            Self::Policy(id) => &id.0,
            Self::Integration(id) => &id.0,
            Self::WorkerPool(id) => &id.0,
        }
    }

    fn key(&self) -> String {
        format!("{}:{}", self.kind().as_str(), self.value())
    }
}

/// Stable subject categories for inventory and query filtering.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeBindingSubjectKind {
    Delivery,
    ProviderSettings,
    CredentialReference,
    Policy,
    Integration,
    WorkerPool,
    Usage,
}

impl ScopeBindingSubjectKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Delivery => "delivery",
            Self::ProviderSettings => "provider_settings",
            Self::CredentialReference => "credential_reference",
            Self::Policy => "policy",
            Self::Integration => "integration",
            Self::WorkerPool => "worker_pool",
            Self::Usage => "usage",
        }
    }

    const fn immutable_attribution(self) -> bool {
        matches!(self, Self::Delivery | Self::Usage)
    }
}

/// Provenance of the current binding revision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeBindingSource {
    Explicit,
    LocalMigration,
}

/// One subject's only canonical hierarchy target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseScopeBinding {
    pub subject: ScopeBindingSubject,
    pub target: HierarchyResourceId,
    pub revision: u64,
    pub source: ScopeBindingSource,
    pub updated_at: Instant,
}

/// Current binding plus its reconstructed hierarchy path and inheritance chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedScopeBinding {
    pub binding: EnterpriseScopeBinding,
    pub scope: HierarchyScope,
    /// Least-specific to most-specific, including the bound target itself.
    pub inheritance_chain: Vec<HierarchyScope>,
}

/// Supported explicit binding mutations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum EnterpriseScopeBindingMutation {
    Bind {
        subject: ScopeBindingSubject,
        target: HierarchyResourceId,
    },
    Rebind {
        subject: ScopeBindingSubject,
        new_target: HierarchyResourceId,
    },
}

impl EnterpriseScopeBindingMutation {
    const fn subject(&self) -> &ScopeBindingSubject {
        match self {
            Self::Bind { subject, .. } | Self::Rebind { subject, .. } => subject,
        }
    }

    const fn target(&self) -> &HierarchyResourceId {
        match self {
            Self::Bind { target, .. } => target,
            Self::Rebind { new_target, .. } => new_target,
        }
    }
}

/// One authenticated Organization-scoped binding command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseScopeBindingCommand {
    pub actor: PublicEventActor,
    pub organization_id: OrganizationId,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub occurred_at: Instant,
    pub mutation: EnterpriseScopeBindingMutation,
}

/// Exact durable result of one explicit bind or rebind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseScopeBindingReceipt {
    pub previous_revision: u64,
    pub current_revision: u64,
    pub binding: EnterpriseScopeBinding,
    pub scope: HierarchyScope,
    pub idempotent_replay: bool,
}

/// One closed inventory snapshot imported into the generated local default scope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalScopeMigrationCommand {
    pub actor: PublicEventActor,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub occurred_at: Instant,
    pub inventory_digest: Sha256Digest,
    pub subjects: Vec<ScopeBindingSubject>,
}

/// Durable one-time local migration result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalScopeMigrationReceipt {
    pub previous_revision: u64,
    pub current_revision: u64,
    pub inventory_digest: Sha256Digest,
    pub migrated_subject_count: u64,
    pub scope: HierarchyScope,
    pub idempotent_replay: bool,
}

/// Durable status used by startup code to skip the migration after restart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalScopeMigrationStatus {
    pub inventory_digest: Sha256Digest,
    pub migrated_subject_count: u64,
    pub completed_at: Instant,
    pub registry_revision: u64,
}

/// Stable binding and migration failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterpriseScopeBindingErrorKind {
    InvalidInput,
    NotFound,
    AlreadyBound,
    ImmutableBinding,
    RevisionConflict,
    RequestConflict,
    CrossTenantReference,
    TargetUnavailable,
    AlreadyMigrated,
    MigrationConflict,
    CorruptState,
    Storage,
}

/// Bounded, secret-free scope binding failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseScopeBindingError {
    kind: EnterpriseScopeBindingErrorKind,
    message: &'static str,
}

impl EnterpriseScopeBindingError {
    #[must_use]
    pub const fn kind(&self) -> EnterpriseScopeBindingErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterpriseScopeBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for EnterpriseScopeBindingError {}

/// Durable binding registry. `storage` and `hierarchy` must use the same
/// `ProductState` database so hierarchy revision guards are atomic with writes.
pub struct EnterpriseScopeBindingService {
    storage: Mutex<Box<dyn ProductStateStorage>>,
    hierarchy: Arc<EnterpriseHierarchyService>,
}

impl EnterpriseScopeBindingService {
    #[must_use]
    pub fn new(
        storage: Box<dyn ProductStateStorage>,
        hierarchy: Arc<EnterpriseHierarchyService>,
    ) -> Self {
        Self {
            storage: Mutex::new(storage),
            hierarchy,
        }
    }

    /// Creates or moves one canonical binding.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, stale revisions, changed request replay,
    /// duplicate/cross-tenant subjects, immutable attribution moves, unavailable
    /// hierarchy targets, corruption, and storage failures.
    pub fn mutate(
        &self,
        command: &EnterpriseScopeBindingCommand,
    ) -> Result<EnterpriseScopeBindingReceipt, EnterpriseScopeBindingError> {
        validate_binding_command(command)?;
        let digest =
            digest_serializable(b"winwincode.enterprise-scope-binding.command.v1\0", command)?;
        let identity = binding_receipt_identity(
            &command.actor,
            &command.organization_id,
            command.request_id.clone(),
        )?;
        let mut storage = self.lock()?;
        if let Some(receipt) = storage
            .load_receipt(&identity, &digest)
            .map_err(|source| storage_error(&source))?
        {
            return binding_receipt(&receipt, true);
        }
        let target =
            self.resolve_active_target(&command.organization_id, command.mutation.target())?;
        let stored = storage
            .load_state(&binding_stream(&command.organization_id))
            .map_err(|source| storage_error(&source))?;
        let mut state = decode_registry(stored.as_ref(), &command.organization_id)?;
        ensure_revision(command.expected_revision, state.revision)?;
        let key = command.mutation.subject().key();
        let shard_number = subject_shard(&key);
        let shard_stored = storage
            .load_state(&index_stream(shard_number))
            .map_err(|source| storage_error(&source))?;
        let mut shard = decode_index_shard(shard_stored.as_ref(), shard_number)?;
        let binding = apply_binding_mutation(&mut state, &mut shard, command)?;
        state.revision = next_revision(state.revision)?;
        shard.revision = next_revision(shard.revision)?;
        let result = StoredOperationResult::Binding {
            previous_revision: command.expected_revision,
            current_revision: state.revision,
            binding: binding.clone(),
            scope: target.scope.clone(),
        };
        let commit = StateCommit::new(
            identity,
            digest.clone(),
            binding_stream(&command.organization_id),
            command.expected_revision,
            canonical_bytes(&state)?,
            vec![NewOutboxEvent::internal(
                event_id(&digest),
                EVENT_TOPIC,
                canonical_bytes(&result)?,
            )],
        )
        .with_state_guard(
            target
                .state_guard()
                .map_err(|source| hierarchy_error(&source))?,
        )
        .with_state_mutation(
            StateMutation::new(
                index_stream(shard_number),
                shard.revision - 1,
                canonical_bytes(&shard)?,
            )
            .map_err(|source| storage_error(&source))?,
        );
        let receipt = storage
            .commit(&commit)
            .map_err(|source| storage_error(&source))?;
        binding_receipt(&receipt, receipt.idempotent_replay)
    }

    /// Resolves one subject through the global index and reconstructs its
    /// current canonical inheritance chain.
    ///
    /// # Errors
    ///
    /// Rejects missing, cross-linked, corrupt, or unavailable durable state.
    pub fn resolve(
        &self,
        subject: &ScopeBindingSubject,
    ) -> Result<ResolvedScopeBinding, EnterpriseScopeBindingError> {
        validate_subject(subject)?;
        let key = subject.key();
        let storage = self.lock()?;
        let shard_number = subject_shard(&key);
        let shard_stored = storage
            .load_state(&index_stream(shard_number))
            .map_err(|source| storage_error(&source))?
            .ok_or_else(not_found)?;
        let shard = decode_index_shard(Some(&shard_stored), shard_number)?;
        let entry = shard.entries.get(&key).ok_or_else(corrupt)?;
        if &entry.subject != subject {
            return Err(corrupt());
        }
        let registry_stored = storage
            .load_state(&binding_stream(&entry.organization_id))
            .map_err(|source| storage_error(&source))?
            .ok_or_else(corrupt)?;
        let registry = decode_registry(Some(&registry_stored), &entry.organization_id)?;
        let binding = registry.bindings.get(&key).cloned().ok_or_else(corrupt)?;
        if binding.subject != *subject || binding.revision != entry.binding_revision {
            return Err(corrupt());
        }
        drop(storage);
        let target = self
            .hierarchy
            .resolve(&binding.target)
            .map_err(|source| hierarchy_error(&source))?;
        if target.scope.organization_id() != &entry.organization_id {
            return Err(corrupt());
        }
        Ok(ResolvedScopeBinding {
            inheritance_chain: scope_chain(&target.scope),
            scope: target.scope,
            binding,
        })
    }

    /// Imports a closed legacy inventory into the generated local default
    /// Repository in one aggregate/index transaction.
    ///
    /// The input order is ignored for digesting and persistence. Duplicate
    /// subjects are rejected. Exact request replay returns the original result;
    /// a different request after completion receives `AlreadyMigrated`.
    ///
    /// # Errors
    ///
    /// Rejects an invalid inventory digest, non-pristine local registry,
    /// duplicate/cross-tenant subjects, unavailable default hierarchy, stale
    /// revisions, corruption, or any atomic storage failure.
    pub fn migrate_local_once(
        &self,
        command: &LocalScopeMigrationCommand,
    ) -> Result<LocalScopeMigrationReceipt, EnterpriseScopeBindingError> {
        validate_local_migration_command(command)?;
        let subjects = canonical_subjects(&command.subjects)?;
        let inventory_digest = inventory_digest_from_canonical(&subjects)?;
        if inventory_digest != command.inventory_digest {
            return Err(invalid());
        }
        let organization_id = local_organization_id();
        let target_id = HierarchyResourceId::Repository(local_repository_id());
        let digest_input = LocalMigrationDigestInput {
            actor: &command.actor,
            request_id: &command.request_id,
            expected_revision: command.expected_revision,
            occurred_at: &command.occurred_at,
            inventory_digest: &inventory_digest,
            subjects: &subjects,
        };
        let digest = digest_serializable(
            b"winwincode.enterprise-scope-binding.local-migration.v1\0",
            &digest_input,
        )?;
        let identity =
            binding_receipt_identity(&command.actor, &organization_id, command.request_id.clone())?;
        let mut storage = self.lock()?;
        if let Some(receipt) = storage
            .load_receipt(&identity, &digest)
            .map_err(|source| storage_error(&source))?
        {
            return migration_receipt(&receipt, true);
        }
        let target = self.resolve_active_target(&organization_id, &target_id)?;
        let stored = storage
            .load_state(&binding_stream(&organization_id))
            .map_err(|source| storage_error(&source))?;
        let mut state = decode_registry(stored.as_ref(), &organization_id)?;
        ensure_revision(command.expected_revision, state.revision)?;
        if state.local_migration.is_some() {
            return Err(already_migrated());
        }
        if !state.bindings.is_empty() {
            return Err(migration_conflict());
        }
        let mut shards = load_migration_shards(&**storage, &subjects)?;
        install_migration_bindings(
            &mut state,
            &mut shards,
            &subjects,
            &target_id,
            &command.occurred_at,
        )?;
        state.revision = next_revision(state.revision)?;
        state.local_migration = Some(LocalMigrationMarker {
            inventory_digest: inventory_digest.clone(),
            migrated_subject_count: u64::try_from(subjects.len()).map_err(|_| invalid())?,
            completed_at: command.occurred_at.clone(),
        });
        let result = StoredOperationResult::LocalMigration {
            previous_revision: command.expected_revision,
            current_revision: state.revision,
            inventory_digest: inventory_digest.clone(),
            migrated_subject_count: u64::try_from(subjects.len()).map_err(|_| invalid())?,
            scope: target.scope.clone(),
        };
        let mut commit = StateCommit::new(
            identity,
            digest.clone(),
            binding_stream(&organization_id),
            command.expected_revision,
            canonical_bytes(&state)?,
            vec![NewOutboxEvent::internal(
                event_id(&digest),
                EVENT_TOPIC,
                canonical_bytes(&result)?,
            )],
        )
        .with_state_guard(
            target
                .state_guard()
                .map_err(|source| hierarchy_error(&source))?,
        );
        for (shard_number, shard) in shards {
            commit = commit.with_state_mutation(
                StateMutation::new(
                    index_stream(shard_number),
                    shard.revision - 1,
                    canonical_bytes(&shard)?,
                )
                .map_err(|source| storage_error(&source))?,
            );
        }
        let receipt = storage
            .commit(&commit)
            .map_err(|source| storage_error(&source))?;
        migration_receipt(&receipt, receipt.idempotent_replay)
    }

    /// Reads the one-time local migration marker after restart.
    ///
    /// # Errors
    ///
    /// Rejects corrupt or unavailable durable state.
    pub fn local_migration_status(
        &self,
    ) -> Result<Option<LocalScopeMigrationStatus>, EnterpriseScopeBindingError> {
        let organization_id = local_organization_id();
        let storage = self.lock()?;
        let stored = storage
            .load_state(&binding_stream(&organization_id))
            .map_err(|source| storage_error(&source))?;
        let state = decode_registry(stored.as_ref(), &organization_id)?;
        if state.local_migration.is_some() {
            validate_migration_indexes(&**storage, &state)?;
        }
        Ok(state
            .local_migration
            .map(|marker| LocalScopeMigrationStatus {
                inventory_digest: marker.inventory_digest,
                migrated_subject_count: marker.migrated_subject_count,
                completed_at: marker.completed_at,
                registry_revision: state.revision,
            }))
    }

    fn resolve_active_target(
        &self,
        organization_id: &OrganizationId,
        target: &HierarchyResourceId,
    ) -> Result<ResolvedHierarchyResource, EnterpriseScopeBindingError> {
        let resolved = self
            .hierarchy
            .resolve(target)
            .map_err(|source| hierarchy_error(&source))?;
        if resolved.scope.organization_id() != organization_id {
            return Err(cross_tenant());
        }
        if resolved.resource.state != HierarchyResourceState::Active {
            return Err(target_unavailable());
        }
        Ok(resolved)
    }

    fn lock(
        &self,
    ) -> Result<MutexGuard<'_, Box<dyn ProductStateStorage>>, EnterpriseScopeBindingError> {
        self.storage.lock().map_err(|_| storage_failure())
    }
}

/// Computes the canonical digest startup code must attach to a local inventory.
///
/// # Errors
///
/// Rejects malformed, duplicate, or oversized inventories.
pub fn local_scope_inventory_digest(
    subjects: &[ScopeBindingSubject],
) -> Result<Sha256Digest, EnterpriseScopeBindingError> {
    let subjects = canonical_subjects(subjects)?;
    inventory_digest_from_canonical(&subjects)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BindingRegistryState {
    schema: String,
    organization_id: OrganizationId,
    revision: u64,
    local_migration: Option<LocalMigrationMarker>,
    bindings: BTreeMap<String, EnterpriseScopeBinding>,
}

impl BindingRegistryState {
    fn empty(organization_id: &OrganizationId) -> Self {
        Self {
            schema: STATE_SCHEMA.to_owned(),
            organization_id: organization_id.clone(),
            revision: 0,
            local_migration: None,
            bindings: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalMigrationMarker {
    inventory_digest: Sha256Digest,
    migrated_subject_count: u64,
    completed_at: Instant,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BindingIndexShard {
    schema: String,
    shard: u8,
    revision: u64,
    entries: BTreeMap<String, BindingIndexEntry>,
}

impl BindingIndexShard {
    fn empty(shard: u8) -> Self {
        Self {
            schema: INDEX_SCHEMA.to_owned(),
            shard,
            revision: 0,
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BindingIndexEntry {
    organization_id: OrganizationId,
    subject: ScopeBindingSubject,
    binding_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
enum StoredOperationResult {
    Binding {
        previous_revision: u64,
        current_revision: u64,
        binding: EnterpriseScopeBinding,
        scope: HierarchyScope,
    },
    LocalMigration {
        previous_revision: u64,
        current_revision: u64,
        inventory_digest: Sha256Digest,
        migrated_subject_count: u64,
        scope: HierarchyScope,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalMigrationDigestInput<'a> {
    actor: &'a PublicEventActor,
    request_id: &'a RequestId,
    expected_revision: u64,
    occurred_at: &'a Instant,
    inventory_digest: &'a Sha256Digest,
    subjects: &'a [ScopeBindingSubject],
}

fn apply_binding_mutation(
    state: &mut BindingRegistryState,
    shard: &mut BindingIndexShard,
    command: &EnterpriseScopeBindingCommand,
) -> Result<EnterpriseScopeBinding, EnterpriseScopeBindingError> {
    let subject = command.mutation.subject();
    let key = subject.key();
    match &command.mutation {
        EnterpriseScopeBindingMutation::Bind { target, .. } => {
            if let Some(entry) = shard.entries.get(&key) {
                return if entry.organization_id == command.organization_id {
                    Err(already_bound())
                } else {
                    Err(cross_tenant())
                };
            }
            if state.bindings.contains_key(&key) {
                return Err(corrupt());
            }
            let binding = EnterpriseScopeBinding {
                subject: subject.clone(),
                target: target.clone(),
                revision: 1,
                source: ScopeBindingSource::Explicit,
                updated_at: command.occurred_at.clone(),
            };
            state.bindings.insert(key.clone(), binding.clone());
            shard.entries.insert(
                key,
                BindingIndexEntry {
                    organization_id: command.organization_id.clone(),
                    subject: subject.clone(),
                    binding_revision: binding.revision,
                },
            );
            Ok(binding)
        }
        EnterpriseScopeBindingMutation::Rebind { new_target, .. } => {
            if subject.kind().immutable_attribution() {
                return Err(immutable_binding());
            }
            let entry = shard.entries.get_mut(&key).ok_or_else(not_found)?;
            if entry.organization_id != command.organization_id || entry.subject != *subject {
                return Err(cross_tenant());
            }
            let binding = state.bindings.get_mut(&key).ok_or_else(corrupt)?;
            if binding.revision != entry.binding_revision || binding.subject != *subject {
                return Err(corrupt());
            }
            if &binding.target == new_target {
                return Err(invalid());
            }
            binding.target = new_target.clone();
            binding.revision = next_revision(binding.revision)?;
            binding.updated_at = command.occurred_at.clone();
            entry.binding_revision = binding.revision;
            Ok(binding.clone())
        }
    }
}

fn load_migration_shards(
    storage: &dyn ProductStateStorage,
    subjects: &[ScopeBindingSubject],
) -> Result<BTreeMap<u8, BindingIndexShard>, EnterpriseScopeBindingError> {
    let mut shards = BTreeMap::new();
    for subject in subjects {
        let key = subject.key();
        let shard_number = subject_shard(&key);
        if let std::collections::btree_map::Entry::Vacant(entry) = shards.entry(shard_number) {
            let stored = storage
                .load_state(&index_stream(shard_number))
                .map_err(|source| storage_error(&source))?;
            entry.insert(decode_index_shard(stored.as_ref(), shard_number)?);
        }
        let shard = shards.get(&shard_number).ok_or_else(corrupt)?;
        if let Some(existing) = shard.entries.get(&key) {
            return if existing.organization_id == local_organization_id() {
                Err(corrupt())
            } else {
                Err(cross_tenant())
            };
        }
    }
    for shard in shards.values_mut() {
        shard.revision = next_revision(shard.revision)?;
    }
    Ok(shards)
}

fn validate_migration_indexes(
    storage: &dyn ProductStateStorage,
    state: &BindingRegistryState,
) -> Result<(), EnterpriseScopeBindingError> {
    let mut shards = BTreeMap::new();
    for binding in state
        .bindings
        .values()
        .filter(|binding| binding.source == ScopeBindingSource::LocalMigration)
    {
        let key = binding.subject.key();
        let shard_number = subject_shard(&key);
        if let std::collections::btree_map::Entry::Vacant(entry) = shards.entry(shard_number) {
            let stored = storage
                .load_state(&index_stream(shard_number))
                .map_err(|source| storage_error(&source))?
                .ok_or_else(corrupt)?;
            entry.insert(decode_index_shard(Some(&stored), shard_number)?);
        }
        let entry = shards
            .get(&shard_number)
            .and_then(|shard| shard.entries.get(&key))
            .ok_or_else(corrupt)?;
        if entry.organization_id != state.organization_id
            || entry.subject != binding.subject
            || entry.binding_revision != binding.revision
        {
            return Err(corrupt());
        }
    }
    Ok(())
}

fn install_migration_bindings(
    state: &mut BindingRegistryState,
    shards: &mut BTreeMap<u8, BindingIndexShard>,
    subjects: &[ScopeBindingSubject],
    target: &HierarchyResourceId,
    occurred_at: &Instant,
) -> Result<(), EnterpriseScopeBindingError> {
    for subject in subjects {
        let key = subject.key();
        let binding = EnterpriseScopeBinding {
            subject: subject.clone(),
            target: target.clone(),
            revision: 1,
            source: ScopeBindingSource::LocalMigration,
            updated_at: occurred_at.clone(),
        };
        if state.bindings.insert(key.clone(), binding).is_some() {
            return Err(corrupt());
        }
        let shard = shards.get_mut(&subject_shard(&key)).ok_or_else(corrupt)?;
        if shard
            .entries
            .insert(
                key,
                BindingIndexEntry {
                    organization_id: state.organization_id.clone(),
                    subject: subject.clone(),
                    binding_revision: 1,
                },
            )
            .is_some()
        {
            return Err(corrupt());
        }
    }
    Ok(())
}

fn validate_binding_command(
    command: &EnterpriseScopeBindingCommand,
) -> Result<(), EnterpriseScopeBindingError> {
    validate_canonical_id(&command.organization_id.0, "org_")?;
    validate_subject(command.mutation.subject())?;
    validate_hierarchy_id(command.mutation.target())?;
    validate_instant(&command.occurred_at)?;
    if command.expected_revision > MAX_SAFE_INTEGER {
        return Err(invalid());
    }
    Ok(())
}

fn validate_local_migration_command(
    command: &LocalScopeMigrationCommand,
) -> Result<(), EnterpriseScopeBindingError> {
    validate_instant(&command.occurred_at)?;
    validate_sha256(&command.inventory_digest)?;
    if command.expected_revision > MAX_SAFE_INTEGER
        || command.subjects.len() > MAX_LOCAL_MIGRATION_BINDINGS
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_subject(subject: &ScopeBindingSubject) -> Result<(), EnterpriseScopeBindingError> {
    match subject {
        ScopeBindingSubject::Delivery(id) => validate_canonical_id(&id.0, "dlv_"),
        ScopeBindingSubject::ProviderSettings(digest) | ScopeBindingSubject::Usage(digest) => {
            validate_sha256(digest)
        }
        ScopeBindingSubject::CredentialReference(id) => validate_canonical_id(&id.0, "crd_"),
        ScopeBindingSubject::Policy(id) => validate_canonical_id(&id.0, "pol_"),
        ScopeBindingSubject::Integration(id) => validate_canonical_id(&id.0, "int_"),
        ScopeBindingSubject::WorkerPool(id) => validate_canonical_id(&id.0, "wpl_"),
    }
}

fn validate_hierarchy_id(id: &HierarchyResourceId) -> Result<(), EnterpriseScopeBindingError> {
    let (value, prefix) = match id {
        HierarchyResourceId::Organization(id) => (id.0.as_str(), "org_"),
        HierarchyResourceId::Workspace(id) => (id.0.as_str(), "wsp_"),
        HierarchyResourceId::Project(id) => (id.0.as_str(), "prj_"),
        HierarchyResourceId::Environment(id) => (id.as_str(), "env_"),
        HierarchyResourceId::Repository(id) => (id.0.as_str(), "rep_"),
    };
    validate_canonical_id(value, prefix)
}

fn validate_canonical_id(value: &str, prefix: &str) -> Result<(), EnterpriseScopeBindingError> {
    let valid = value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            })
    });
    if valid { Ok(()) } else { Err(invalid()) }
}

fn validate_sha256(digest: &Sha256Digest) -> Result<(), EnterpriseScopeBindingError> {
    let valid = digest.0.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    });
    if valid { Ok(()) } else { Err(invalid()) }
}

fn validate_instant(value: &Instant) -> Result<(), EnterpriseScopeBindingError> {
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
    if valid { Ok(()) } else { Err(invalid()) }
}

fn decode_registry(
    stored: Option<&StoredState>,
    organization_id: &OrganizationId,
) -> Result<BindingRegistryState, EnterpriseScopeBindingError> {
    let Some(stored) = stored else {
        return Ok(BindingRegistryState::empty(organization_id));
    };
    let state: BindingRegistryState =
        serde_json::from_slice(&stored.payload).map_err(|_| corrupt())?;
    if state.schema != STATE_SCHEMA
        || &state.organization_id != organization_id
        || state.revision != stored.revision
        || state.revision == 0
        || state.revision > MAX_SAFE_INTEGER
        || canonical_bytes(&state)? != stored.payload
    {
        return Err(corrupt());
    }
    validate_registry(&state)?;
    Ok(state)
}

fn validate_registry(state: &BindingRegistryState) -> Result<(), EnterpriseScopeBindingError> {
    validate_canonical_id(&state.organization_id.0, "org_").map_err(|_| corrupt())?;
    for (key, binding) in &state.bindings {
        validate_subject(&binding.subject).map_err(|_| corrupt())?;
        validate_hierarchy_id(&binding.target).map_err(|_| corrupt())?;
        validate_instant(&binding.updated_at).map_err(|_| corrupt())?;
        if key != &binding.subject.key()
            || binding.revision == 0
            || binding.revision > MAX_SAFE_INTEGER
        {
            return Err(corrupt());
        }
    }
    let migrated: Vec<_> = state
        .bindings
        .values()
        .filter(|binding| binding.source == ScopeBindingSource::LocalMigration)
        .map(|binding| binding.subject.clone())
        .collect();
    match &state.local_migration {
        None if migrated.is_empty() => Ok(()),
        Some(marker) => {
            validate_sha256(&marker.inventory_digest).map_err(|_| corrupt())?;
            validate_instant(&marker.completed_at).map_err(|_| corrupt())?;
            if marker.migrated_subject_count
                != u64::try_from(migrated.len()).map_err(|_| corrupt())?
                || marker.inventory_digest != inventory_digest_from_canonical(&migrated)?
            {
                return Err(corrupt());
            }
            Ok(())
        }
        None => Err(corrupt()),
    }
}

fn decode_index_shard(
    stored: Option<&StoredState>,
    shard_number: u8,
) -> Result<BindingIndexShard, EnterpriseScopeBindingError> {
    let Some(stored) = stored else {
        return Ok(BindingIndexShard::empty(shard_number));
    };
    let shard: BindingIndexShard =
        serde_json::from_slice(&stored.payload).map_err(|_| corrupt())?;
    if shard.schema != INDEX_SCHEMA
        || shard.shard != shard_number
        || shard.revision != stored.revision
        || shard.revision == 0
        || shard.revision > MAX_SAFE_INTEGER
        || canonical_bytes(&shard)? != stored.payload
    {
        return Err(corrupt());
    }
    for (key, entry) in &shard.entries {
        validate_canonical_id(&entry.organization_id.0, "org_").map_err(|_| corrupt())?;
        validate_subject(&entry.subject).map_err(|_| corrupt())?;
        if key != &entry.subject.key()
            || subject_shard(key) != shard_number
            || entry.binding_revision == 0
            || entry.binding_revision > MAX_SAFE_INTEGER
        {
            return Err(corrupt());
        }
    }
    Ok(shard)
}

fn canonical_subjects(
    subjects: &[ScopeBindingSubject],
) -> Result<Vec<ScopeBindingSubject>, EnterpriseScopeBindingError> {
    if subjects.len() > MAX_LOCAL_MIGRATION_BINDINGS {
        return Err(invalid());
    }
    let mut canonical = BTreeMap::new();
    for subject in subjects {
        validate_subject(subject)?;
        if canonical.insert(subject.key(), subject.clone()).is_some() {
            return Err(invalid());
        }
    }
    Ok(canonical.into_values().collect())
}

fn inventory_digest_from_canonical(
    subjects: &[ScopeBindingSubject],
) -> Result<Sha256Digest, EnterpriseScopeBindingError> {
    digest_serializable(
        b"winwincode.enterprise-scope-binding.local-inventory.v1\0",
        subjects,
    )
}

fn scope_chain(scope: &HierarchyScope) -> Vec<HierarchyScope> {
    let organization = HierarchyScope::Organization {
        organization_id: scope.organization_id().clone(),
    };
    match scope {
        HierarchyScope::Organization { .. } => vec![organization],
        HierarchyScope::Workspace {
            organization_id,
            workspace_id,
        } => vec![
            organization,
            HierarchyScope::Workspace {
                organization_id: organization_id.clone(),
                workspace_id: workspace_id.clone(),
            },
        ],
        HierarchyScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => vec![
            organization,
            HierarchyScope::Workspace {
                organization_id: organization_id.clone(),
                workspace_id: workspace_id.clone(),
            },
            HierarchyScope::Project {
                organization_id: organization_id.clone(),
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
            },
        ],
        HierarchyScope::Environment {
            organization_id,
            workspace_id,
            project_id,
            environment_id,
        } => vec![
            organization,
            HierarchyScope::Workspace {
                organization_id: organization_id.clone(),
                workspace_id: workspace_id.clone(),
            },
            HierarchyScope::Project {
                organization_id: organization_id.clone(),
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
            },
            HierarchyScope::Environment {
                organization_id: organization_id.clone(),
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
                environment_id: environment_id.clone(),
            },
        ],
        HierarchyScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => vec![
            organization,
            HierarchyScope::Workspace {
                organization_id: organization_id.clone(),
                workspace_id: workspace_id.clone(),
            },
            HierarchyScope::Project {
                organization_id: organization_id.clone(),
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
            },
            HierarchyScope::Repository {
                organization_id: organization_id.clone(),
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
                repository_id: repository_id.clone(),
            },
        ],
    }
}

fn binding_receipt(
    receipt: &CommitReceipt,
    idempotent_replay: bool,
) -> Result<EnterpriseScopeBindingReceipt, EnterpriseScopeBindingError> {
    let StoredOperationResult::Binding {
        previous_revision,
        current_revision,
        binding,
        scope,
    } = decode_receipt(receipt)?
    else {
        return Err(corrupt());
    };
    if receipt.stream_id != binding_stream(scope.organization_id())
        || current_revision != receipt.revision
        || current_revision != previous_revision.saturating_add(1)
        || !scope_matches_target(&scope, &binding.target)
    {
        return Err(corrupt());
    }
    Ok(EnterpriseScopeBindingReceipt {
        previous_revision,
        current_revision,
        binding,
        scope,
        idempotent_replay,
    })
}

fn migration_receipt(
    receipt: &CommitReceipt,
    idempotent_replay: bool,
) -> Result<LocalScopeMigrationReceipt, EnterpriseScopeBindingError> {
    let StoredOperationResult::LocalMigration {
        previous_revision,
        current_revision,
        inventory_digest,
        migrated_subject_count,
        scope,
    } = decode_receipt(receipt)?
    else {
        return Err(corrupt());
    };
    if receipt.stream_id != binding_stream(scope.organization_id())
        || scope.organization_id() != &local_organization_id()
        || current_revision != receipt.revision
        || current_revision != previous_revision.saturating_add(1)
        || !scope_matches_target(
            &scope,
            &HierarchyResourceId::Repository(local_repository_id()),
        )
        || validate_sha256(&inventory_digest).is_err()
    {
        return Err(corrupt());
    }
    Ok(LocalScopeMigrationReceipt {
        previous_revision,
        current_revision,
        inventory_digest,
        migrated_subject_count,
        scope,
        idempotent_replay,
    })
}

fn decode_receipt(
    receipt: &CommitReceipt,
) -> Result<StoredOperationResult, EnterpriseScopeBindingError> {
    let [event] = receipt.events.as_slice() else {
        return Err(corrupt());
    };
    if event.topic != EVENT_TOPIC {
        return Err(corrupt());
    }
    serde_json::from_slice(&event.payload).map_err(|_| corrupt())
}

fn scope_matches_target(scope: &HierarchyScope, target: &HierarchyResourceId) -> bool {
    matches!(
        (scope, target),
        (
            HierarchyScope::Organization { organization_id },
            HierarchyResourceId::Organization(id),
        ) if organization_id == id
    ) || matches!(
        (scope, target),
        (
            HierarchyScope::Workspace { workspace_id, .. },
            HierarchyResourceId::Workspace(id),
        ) if workspace_id == id
    ) || matches!(
        (scope, target),
        (
            HierarchyScope::Project { project_id, .. },
            HierarchyResourceId::Project(id),
        ) if project_id == id
    ) || matches!(
        (scope, target),
        (
            HierarchyScope::Environment { environment_id, .. },
            HierarchyResourceId::Environment(id),
        ) if environment_id == id
    ) || matches!(
        (scope, target),
        (
            HierarchyScope::Repository { repository_id, .. },
            HierarchyResourceId::Repository(id),
        ) if repository_id == id
    )
}

fn subject_shard(key: &str) -> u8 {
    Sha256::digest(key.as_bytes())[0] & (INDEX_SHARD_COUNT - 1)
}

fn digest_serializable<T: Serialize + ?Sized>(
    domain: &[u8],
    value: &T,
) -> Result<Sha256Digest, EnterpriseScopeBindingError> {
    let bytes = canonical_bytes(value)?;
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    Ok(Sha256Digest(format!("sha256:{:x}", digest.finalize())))
}

fn canonical_bytes<T: Serialize + ?Sized>(
    value: &T,
) -> Result<Vec<u8>, EnterpriseScopeBindingError> {
    serde_json::to_vec(value).map_err(|_| corrupt())
}

fn binding_receipt_identity(
    actor: &PublicEventActor,
    organization_id: &OrganizationId,
    request_id: RequestId,
) -> Result<winwincode_storage::ReceiptIdentity, EnterpriseScopeBindingError> {
    public_receipt_identity(
        actor,
        &PublicEventScope::Organization {
            organization_id: organization_id.clone(),
        },
        request_id,
    )
    .map_err(|source| storage_error(&source))
}

fn ensure_revision(expected: u64, current: u64) -> Result<(), EnterpriseScopeBindingError> {
    if expected == current {
        Ok(())
    } else {
        Err(revision_conflict())
    }
}

fn next_revision(current: u64) -> Result<u64, EnterpriseScopeBindingError> {
    current
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(invalid)
}

fn event_id(digest: &Sha256Digest) -> String {
    format!(
        "evt_enterprise_scope_binding_{}",
        digest.0.strip_prefix("sha256:").unwrap_or(&digest.0)
    )
}

fn binding_stream(organization_id: &OrganizationId) -> String {
    format!("{STREAM_PREFIX}{}", organization_id.0)
}

fn index_stream(shard: u8) -> String {
    format!("{INDEX_PREFIX}{shard:02x}")
}

fn local_organization_id() -> OrganizationId {
    OrganizationId(DEFAULT_ORGANIZATION_ID.to_owned())
}

fn local_repository_id() -> RepositoryId {
    RepositoryId(DEFAULT_REPOSITORY_ID.to_owned())
}

fn storage_error(source: &StorageError) -> EnterpriseScopeBindingError {
    match source.kind() {
        StorageErrorKind::InvalidInput => invalid(),
        StorageErrorKind::RevisionConflict => revision_conflict(),
        StorageErrorKind::RequestConflict => request_conflict(),
        StorageErrorKind::RequestReplayMissing => corrupt(),
        StorageErrorKind::JournalAlreadyExists
        | StorageErrorKind::JournalNotFound
        | StorageErrorKind::JournalConflict
        | StorageErrorKind::EventCursorExpired
        | StorageErrorKind::Adapter
        | StorageErrorKind::Closed => storage_failure(),
    }
}

fn hierarchy_error(source: &EnterpriseHierarchyError) -> EnterpriseScopeBindingError {
    match source.kind() {
        EnterpriseHierarchyErrorKind::InvalidInput => invalid(),
        EnterpriseHierarchyErrorKind::NotFound => not_found(),
        EnterpriseHierarchyErrorKind::CrossTenantReference => cross_tenant(),
        EnterpriseHierarchyErrorKind::Archived | EnterpriseHierarchyErrorKind::Deleted => {
            target_unavailable()
        }
        EnterpriseHierarchyErrorKind::RevisionConflict => revision_conflict(),
        EnterpriseHierarchyErrorKind::RequestConflict => request_conflict(),
        EnterpriseHierarchyErrorKind::CorruptState => corrupt(),
        EnterpriseHierarchyErrorKind::AlreadyExists
        | EnterpriseHierarchyErrorKind::InvalidParent
        | EnterpriseHierarchyErrorKind::Cycle
        | EnterpriseHierarchyErrorKind::DescendantsExist
        | EnterpriseHierarchyErrorKind::Storage => storage_failure(),
    }
}

const fn error(
    kind: EnterpriseScopeBindingErrorKind,
    message: &'static str,
) -> EnterpriseScopeBindingError {
    EnterpriseScopeBindingError { kind, message }
}

const fn invalid() -> EnterpriseScopeBindingError {
    error(
        EnterpriseScopeBindingErrorKind::InvalidInput,
        "Enterprise scope binding input is invalid",
    )
}

const fn not_found() -> EnterpriseScopeBindingError {
    error(
        EnterpriseScopeBindingErrorKind::NotFound,
        "Enterprise scope binding was not found",
    )
}

const fn already_bound() -> EnterpriseScopeBindingError {
    error(
        EnterpriseScopeBindingErrorKind::AlreadyBound,
        "Enterprise resource already has a canonical scope binding",
    )
}

const fn immutable_binding() -> EnterpriseScopeBindingError {
    error(
        EnterpriseScopeBindingErrorKind::ImmutableBinding,
        "Historical attribution scope binding is immutable",
    )
}

const fn revision_conflict() -> EnterpriseScopeBindingError {
    error(
        EnterpriseScopeBindingErrorKind::RevisionConflict,
        "Enterprise scope binding revision is stale",
    )
}

const fn request_conflict() -> EnterpriseScopeBindingError {
    error(
        EnterpriseScopeBindingErrorKind::RequestConflict,
        "Enterprise scope binding request was reused with different input",
    )
}

const fn cross_tenant() -> EnterpriseScopeBindingError {
    error(
        EnterpriseScopeBindingErrorKind::CrossTenantReference,
        "Enterprise scope binding crosses Organization ownership",
    )
}

const fn target_unavailable() -> EnterpriseScopeBindingError {
    error(
        EnterpriseScopeBindingErrorKind::TargetUnavailable,
        "Enterprise scope binding target is unavailable",
    )
}

const fn already_migrated() -> EnterpriseScopeBindingError {
    error(
        EnterpriseScopeBindingErrorKind::AlreadyMigrated,
        "Local scope inventory was already migrated",
    )
}

const fn migration_conflict() -> EnterpriseScopeBindingError {
    error(
        EnterpriseScopeBindingErrorKind::MigrationConflict,
        "Local scope migration requires a pristine binding registry",
    )
}

const fn corrupt() -> EnterpriseScopeBindingError {
    error(
        EnterpriseScopeBindingErrorKind::CorruptState,
        "Enterprise scope binding durable state is corrupt",
    )
}

const fn storage_failure() -> EnterpriseScopeBindingError {
    error(
        EnterpriseScopeBindingErrorKind::Storage,
        "Enterprise scope binding storage operation failed",
    )
}
