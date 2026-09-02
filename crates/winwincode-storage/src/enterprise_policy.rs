// SPDX-License-Identifier: Apache-2.0

//! Immutable enterprise Policy versions and inheritance seals.
//!
//! The ledger owns one version chain for each exact scope and Policy kind. A
//! child version freezes the nearest effective ancestor and, when it relaxes
//! that ancestor, the exact organization version that authorized the override.

use std::collections::HashSet;
use std::fmt;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    EnterprisePolicyId, Instant, OrganizationId, ProjectId, RepositoryId, RequestId,
    ServiceAccountId, Sha256Digest, SystemActorId, UserId, WorkspaceId,
};

use crate::{SqliteStorage, StorageError};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_PAGE_SIZE: u64 = 200;
const MAX_POLICY_BYTES: usize = 256 * 1024;
const POLICY_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS enterprise_policy_versions (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT CHECK (sequence > 0),
    policy_id TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    scope_key TEXT NOT NULL,
    policy_kind TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('draft', 'active', 'retired')),
    effective_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    definition_digest TEXT NOT NULL,
    effective_definition_digest TEXT NOT NULL,
    version_digest TEXT UNIQUE NOT NULL,
    record_json TEXT NOT NULL,
    UNIQUE (policy_id, version),
    UNIQUE (policy_id, revision)
);
CREATE INDEX IF NOT EXISTS enterprise_policy_versions_by_scope_kind
    ON enterprise_policy_versions (scope_key, policy_kind, effective_at, version);
CREATE TABLE IF NOT EXISTS enterprise_policy_heads (
    policy_id TEXT PRIMARY KEY NOT NULL,
    scope_key TEXT NOT NULL,
    policy_kind TEXT NOT NULL,
    head_version INTEGER NOT NULL CHECK (head_version > 0),
    head_revision INTEGER NOT NULL CHECK (head_revision > 0),
    head_digest TEXT UNIQUE NOT NULL,
    UNIQUE (scope_key, policy_kind),
    FOREIGN KEY (policy_id, head_version)
        REFERENCES enterprise_policy_versions(policy_id, version) ON DELETE RESTRICT
);
CREATE TABLE IF NOT EXISTS enterprise_policy_receipts (
    actor_key TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    command_digest TEXT NOT NULL,
    policy_id TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    PRIMARY KEY (actor_key, scope_key, request_id),
    FOREIGN KEY (policy_id, version)
        REFERENCES enterprise_policy_versions(policy_id, version) ON DELETE RESTRICT
);
CREATE TRIGGER IF NOT EXISTS enterprise_policy_versions_no_update
BEFORE UPDATE ON enterprise_policy_versions
BEGIN
    SELECT RAISE(ABORT, 'enterprise Policy versions are immutable');
END;
CREATE TRIGGER IF NOT EXISTS enterprise_policy_versions_no_delete
BEFORE DELETE ON enterprise_policy_versions
BEGIN
    SELECT RAISE(ABORT, 'enterprise Policy versions are immutable');
END;
CREATE TRIGGER IF NOT EXISTS enterprise_policy_receipts_no_update
BEFORE UPDATE ON enterprise_policy_receipts
BEGIN
    SELECT RAISE(ABORT, 'enterprise Policy receipts are immutable');
END;
CREATE TRIGGER IF NOT EXISTS enterprise_policy_receipts_no_delete
BEFORE DELETE ON enterprise_policy_receipts
BEGIN
    SELECT RAISE(ABORT, 'enterprise Policy receipts are immutable');
END;
";

/// Closed enterprise Policy families.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterprisePolicyKind {
    Repository,
    Model,
    Provider,
    Tool,
    Network,
    Approval,
    Verifier,
    WorkerPlacement,
    Publication,
    Retention,
    Integration,
}

impl EnterprisePolicyKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Tool => "tool",
            Self::Network => "network",
            Self::Approval => "approval",
            Self::Verifier => "verifier",
            Self::WorkerPlacement => "worker_placement",
            Self::Publication => "publication",
            Self::Retention => "retention",
            Self::Integration => "integration",
        }
    }
}

/// One exact tenant scope in the organization hierarchy.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum EnterprisePolicyScope {
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
    Repository {
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        repository_id: RepositoryId,
    },
}

impl EnterprisePolicyScope {
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
            | Self::Repository {
                organization_id, ..
            } => organization_id,
        }
    }

    const fn is_organization(&self) -> bool {
        matches!(self, Self::Organization { .. })
    }
}

/// Authenticated, secret-free author of one Policy version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum EnterprisePolicyActor {
    User { id: UserId },
    ServiceAccount { id: ServiceAccountId },
    System { id: SystemActorId },
}

/// Policy enforcement behavior.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterprisePolicyMode {
    Enforce,
    Audit,
}

/// Lifecycle state of one immutable version.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterprisePolicyState {
    Draft,
    Active,
    Retired,
}

impl EnterprisePolicyState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Active => "active",
            Self::Retired => "retired",
        }
    }
}

/// Rule result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterprisePolicyEffect {
    Allow,
    Deny,
}

/// Organization control over child relaxation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterprisePolicyChildOverrideMode {
    TightenOnly,
    AllowExplicitRelaxation,
}

/// Relationship of this version to its frozen base.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterprisePolicyInheritanceMode {
    Tighten,
    Override,
}

/// One closed rule reference. Condition bodies are owned by a separate policy compiler.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyRule {
    pub kind: EnterprisePolicyKind,
    pub effect: EnterprisePolicyEffect,
    pub resource_pattern: String,
    pub condition_sha256: Sha256Digest,
}

/// Secret-free Policy definition stored as canonical JSON.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyDefinition {
    pub default_effect: EnterprisePolicyEffect,
    pub child_override_mode: EnterprisePolicyChildOverrideMode,
    pub rules: Vec<EnterprisePolicyRule>,
}

/// Exact immutable source of one version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyVersionSource {
    pub actor: EnterprisePolicyActor,
    pub request_id: RequestId,
}

/// Exact immutable seal used as an inheritance edge.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyVersionReference {
    pub policy_id: EnterprisePolicyId,
    pub policy_kind: EnterprisePolicyKind,
    pub scope: EnterprisePolicyScope,
    pub version: u64,
    pub definition_sha256: Sha256Digest,
    pub effective_definition_sha256: Sha256Digest,
    pub version_digest: Sha256Digest,
}

/// One immutable Policy version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyVersion {
    pub policy_id: EnterprisePolicyId,
    pub policy_kind: EnterprisePolicyKind,
    pub scope: EnterprisePolicyScope,
    pub mode: EnterprisePolicyMode,
    pub state: EnterprisePolicyState,
    pub version: u64,
    pub source: EnterprisePolicyVersionSource,
    pub effective_at: Instant,
    pub inheritance_mode: EnterprisePolicyInheritanceMode,
    pub base_version: Option<EnterprisePolicyVersionReference>,
    pub relaxation_authority: Option<EnterprisePolicyVersionReference>,
    pub definition: EnterprisePolicyDefinition,
    pub definition_sha256: Sha256Digest,
    pub effective_definition_sha256: Sha256Digest,
    pub version_digest: Sha256Digest,
    pub revision: u64,
    pub updated_at: Instant,
}

impl EnterprisePolicyVersion {
    #[must_use]
    pub fn reference(&self) -> EnterprisePolicyVersionReference {
        EnterprisePolicyVersionReference {
            policy_id: self.policy_id.clone(),
            policy_kind: self.policy_kind,
            scope: self.scope.clone(),
            version: self.version,
            definition_sha256: self.definition_sha256.clone(),
            effective_definition_sha256: self.effective_definition_sha256.clone(),
            version_digest: self.version_digest.clone(),
        }
    }
}

/// Canonical command accepted by the durable ledger.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyWrite {
    pub policy_id: EnterprisePolicyId,
    pub policy_kind: EnterprisePolicyKind,
    pub scope: EnterprisePolicyScope,
    pub mode: EnterprisePolicyMode,
    pub state: EnterprisePolicyState,
    pub definition: EnterprisePolicyDefinition,
    pub definition_sha256: Sha256Digest,
    pub effective_at: Instant,
    pub inheritance_mode: EnterprisePolicyInheritanceMode,
    pub base_version: Option<EnterprisePolicyVersionReference>,
    pub expected_revision: u64,
    pub source: EnterprisePolicyVersionSource,
    pub updated_at: Instant,
}

/// Durable write receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterprisePolicyWriteReceipt {
    pub version: EnterprisePolicyVersion,
    pub previous_revision: u64,
    pub idempotent_replay: bool,
}

/// Filters for current Policy heads at one exact scope.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyFilter {
    pub policy_kinds: Vec<EnterprisePolicyKind>,
    pub states: Vec<EnterprisePolicyState>,
}

/// Stable list cursor bound to scope and filters.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterprisePolicyCursor {
    pub scope_digest: Sha256Digest,
    pub filter_digest: Sha256Digest,
    pub snapshot_sequence: u64,
    pub after_policy_id: EnterprisePolicyId,
}

/// One bounded current-head page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterprisePolicyPage {
    pub snapshot_sequence: u64,
    pub versions: Vec<EnterprisePolicyVersion>,
    pub next: Option<EnterprisePolicyCursor>,
}

/// Error categories stable across storage adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterprisePolicyErrorKind {
    InvalidInput,
    RevisionConflict,
    RequestConflict,
    AuthorityMismatch,
    CorruptState,
    NotFound,
    Storage,
}

/// Secret-free enterprise Policy error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterprisePolicyError {
    kind: EnterprisePolicyErrorKind,
    message: String,
}

impl EnterprisePolicyError {
    fn new(kind: EnterprisePolicyErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> EnterprisePolicyErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterprisePolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EnterprisePolicyError {}

/// `SQLite`-backed enterprise Policy ledger.
pub struct EnterprisePolicyLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the versioned enterprise Policy ledger on this connection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical existing schema or an unavailable connection.
    pub fn enterprise_policy_ledger(
        &mut self,
    ) -> Result<EnterprisePolicyLedger<'_>, EnterprisePolicyError> {
        EnterprisePolicyLedger::new(self)
    }
}

impl<'storage> EnterprisePolicyLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, EnterprisePolicyError> {
        let connection = storage
            .connection()
            .map_err(|error| storage_error(&error))?;
        connection
            .execute_batch(POLICY_SCHEMA)
            .map_err(|error| sql_error(&error))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    /// Appends one immutable version or replays its exact original receipt.
    ///
    /// # Errors
    ///
    /// Rejects invalid definitions, stale revisions or bases, changed request
    /// reuse, unauthorized relaxation, cross-tenant references, corrupt rows,
    /// and storage failures.
    pub fn write(
        &mut self,
        write: &EnterprisePolicyWrite,
    ) -> Result<EnterprisePolicyWriteReceipt, EnterprisePolicyError> {
        validate_write(write)?;
        let command_digest = write_command_digest(write)?;
        let actor_key = actor_key(&write.source.actor)?;
        let scope_key = scope_key(&write.scope);
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|error| storage_error(&error))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error(&error))?;
        if let Some(receipt) = load_receipt(
            &transaction,
            &actor_key,
            &scope_key,
            &write.source.request_id,
        )? {
            return replay_write(transaction, &receipt, write, &command_digest);
        }
        let chain = prepare_chain(&transaction, write, &scope_key)?;
        let record = prepare_version(&transaction, write, &chain)?;
        insert_version(&transaction, &record, &scope_key)?;
        upsert_head(&transaction, &record, &scope_key, chain.head_exists)?;
        insert_receipt(
            &transaction,
            &actor_key,
            &scope_key,
            &command_digest,
            &record,
        )?;
        transaction.commit().map_err(|error| sql_error(&error))?;
        Ok(EnterprisePolicyWriteReceipt {
            version: record,
            previous_revision: chain.previous_revision,
            idempotent_replay: false,
        })
    }

    /// Loads the current head of one Policy chain.
    ///
    /// # Errors
    ///
    /// Rejects an invalid identity, corrupt durable bytes, or storage failure.
    pub fn load_head(
        &self,
        policy_id: &EnterprisePolicyId,
    ) -> Result<Option<EnterprisePolicyVersion>, EnterprisePolicyError> {
        validate_id(&policy_id.0, "pol_", "enterprise Policy id")?;
        let connection = self
            .storage
            .connection()
            .map_err(|error| storage_error(&error))?;
        let head = load_head_for_policy(connection, policy_id)?;
        head.map(|head| load_version(connection, &head.policy_id, head.version))
            .transpose()
    }

    /// Loads one immutable historical version.
    ///
    /// # Errors
    ///
    /// Rejects invalid coordinates, missing versions, corruption, or storage failure.
    pub fn load_version(
        &self,
        policy_id: &EnterprisePolicyId,
        version: u64,
    ) -> Result<EnterprisePolicyVersion, EnterprisePolicyError> {
        validate_id(&policy_id.0, "pol_", "enterprise Policy id")?;
        positive_safe(version, "enterprise Policy version")?;
        load_version(
            self.storage
                .connection()
                .map_err(|error| storage_error(&error))?,
            policy_id,
            version,
        )
    }

    /// Enumerates one bounded, ascending slice of an immutable version chain.
    ///
    /// # Errors
    ///
    /// Rejects invalid coordinates, corrupt durable bytes, or storage failure.
    pub fn scan_versions(
        &self,
        policy_id: &EnterprisePolicyId,
        after_version: u64,
        limit: u64,
    ) -> Result<Vec<EnterprisePolicyVersion>, EnterprisePolicyError> {
        validate_id(&policy_id.0, "pol_", "enterprise Policy id")?;
        if after_version > MAX_SAFE_INTEGER || limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(error(
                EnterprisePolicyErrorKind::InvalidInput,
                "enterprise Policy history coordinates are outside their bounded range",
            ));
        }
        let connection = self
            .storage
            .connection()
            .map_err(|error| storage_error(&error))?;
        let mut statement = connection
            .prepare(
                "SELECT version FROM enterprise_policy_versions
                 WHERE policy_id = ?1 AND version > ?2
                 ORDER BY version ASC LIMIT ?3",
            )
            .map_err(|error| sql_error(&error))?;
        let versions = statement
            .query_map(
                params![
                    policy_id.0,
                    sql_integer(after_version)?,
                    sql_integer(limit)?,
                ],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| sql_error(&error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sql_error(&error))?;
        versions
            .into_iter()
            .map(|version| {
                load_version(
                    connection,
                    policy_id,
                    from_sql_integer(version, "enterprise Policy history version")?,
                )
            })
            .collect()
    }

    /// Lists exact-scope current heads from one stable version snapshot.
    ///
    /// # Errors
    ///
    /// Rejects invalid filters/cursors, cross-snapshot cursors, corruption, and
    /// storage failures.
    pub fn scan_heads(
        &self,
        scope: &EnterprisePolicyScope,
        filter: &EnterprisePolicyFilter,
        cursor: Option<&EnterprisePolicyCursor>,
        limit: u64,
    ) -> Result<EnterprisePolicyPage, EnterprisePolicyError> {
        validate_scope(scope)?;
        validate_filter(filter)?;
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(error(
                EnterprisePolicyErrorKind::InvalidInput,
                "enterprise Policy page limit is outside 1..=200",
            ));
        }
        let connection = self
            .storage
            .connection()
            .map_err(|error| storage_error(&error))?;
        let scope_digest = digest_sha(scope)?;
        let filter_digest = digest_sha(filter)?;
        let (snapshot_sequence, after_policy_id) = match cursor {
            Some(cursor) => {
                if cursor.scope_digest != scope_digest || cursor.filter_digest != filter_digest {
                    return Err(error(
                        EnterprisePolicyErrorKind::AuthorityMismatch,
                        "enterprise Policy cursor belongs to another query",
                    ));
                }
                validate_id(
                    &cursor.after_policy_id.0,
                    "pol_",
                    "enterprise Policy cursor id",
                )?;
                (
                    cursor.snapshot_sequence,
                    Some(cursor.after_policy_id.0.as_str()),
                )
            }
            None => (last_sequence(connection)?, None),
        };
        let mut versions = scan_heads(
            connection,
            &scope_key(scope),
            filter,
            snapshot_sequence,
            after_policy_id,
            limit + 1,
        )?;
        let page_size = usize::try_from(limit).map_err(|_| {
            error(
                EnterprisePolicyErrorKind::InvalidInput,
                "enterprise Policy page limit is invalid",
            )
        })?;
        let has_more = versions.len() > page_size;
        if has_more {
            versions.pop();
        }
        let next = if has_more {
            Some(EnterprisePolicyCursor {
                scope_digest,
                filter_digest,
                snapshot_sequence,
                after_policy_id: versions
                    .last()
                    .ok_or_else(|| corrupt("enterprise Policy page is unexpectedly empty"))?
                    .policy_id
                    .clone(),
            })
        } else {
            None
        };
        Ok(EnterprisePolicyPage {
            snapshot_sequence,
            versions,
            next,
        })
    }
}

#[derive(Clone)]
struct StoredHead {
    policy_id: EnterprisePolicyId,
    version: u64,
    revision: u64,
}

struct StoredReceipt {
    command_digest: String,
    policy_id: EnterprisePolicyId,
    version: u64,
}

struct PreparedPolicyChain {
    previous_revision: u64,
    version: u64,
    base: Option<EnterprisePolicyVersion>,
    previous_effective: Option<EnterprisePolicyVersion>,
    head_exists: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionDigestFacts<'a> {
    policy_id: &'a EnterprisePolicyId,
    policy_kind: EnterprisePolicyKind,
    scope: &'a EnterprisePolicyScope,
    mode: EnterprisePolicyMode,
    state: EnterprisePolicyState,
    version: u64,
    source: &'a EnterprisePolicyVersionSource,
    effective_at: &'a Instant,
    inheritance_mode: EnterprisePolicyInheritanceMode,
    base_version: Option<EnterprisePolicyVersionReference>,
    relaxation_authority: Option<EnterprisePolicyVersionReference>,
    definition_sha256: &'a Sha256Digest,
    effective_definition_sha256: &'a Sha256Digest,
    revision: u64,
    updated_at: &'a Instant,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WriteCommandDigestFacts<'a> {
    policy_id: &'a EnterprisePolicyId,
    policy_kind: EnterprisePolicyKind,
    scope: &'a EnterprisePolicyScope,
    mode: EnterprisePolicyMode,
    state: EnterprisePolicyState,
    definition: &'a EnterprisePolicyDefinition,
    definition_sha256: &'a Sha256Digest,
    effective_at: &'a Instant,
    inheritance_mode: EnterprisePolicyInheritanceMode,
    base_version: &'a Option<EnterprisePolicyVersionReference>,
    expected_revision: u64,
    source: &'a EnterprisePolicyVersionSource,
}

fn prepare_chain(
    connection: &Connection,
    write: &EnterprisePolicyWrite,
    scope_key: &str,
) -> Result<PreparedPolicyChain, EnterprisePolicyError> {
    let head = load_head_for_scope_kind(connection, scope_key, write.policy_kind)?;
    let identity_head = load_head_for_policy(connection, &write.policy_id)?;
    if identity_head.is_some() != head.is_some()
        || identity_head
            .as_ref()
            .zip(head.as_ref())
            .is_some_and(|(identity, scoped)| {
                identity.policy_id != scoped.policy_id
                    || identity.version != scoped.version
                    || identity.revision != scoped.revision
            })
    {
        return Err(error(
            EnterprisePolicyErrorKind::AuthorityMismatch,
            "enterprise Policy id belongs to another scope or kind",
        ));
    }
    validate_head_identity(head.as_ref(), write)?;
    let previous_revision = head.as_ref().map_or(0, |head| head.revision);
    if write.expected_revision != previous_revision {
        return Err(error(
            EnterprisePolicyErrorKind::RevisionConflict,
            "enterprise Policy expected revision is stale",
        ));
    }
    if let Some(current) = head
        .as_ref()
        .map(|head| load_version(connection, &head.policy_id, head.version))
        .transpose()?
        && (write.updated_at.0 < current.updated_at.0
            || write.effective_at.0 < current.effective_at.0)
    {
        return Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy version time precedes its canonical chain head",
        ));
    }
    let base = resolve_nearest_parent(
        connection,
        &write.scope,
        write.policy_kind,
        &write.effective_at,
    )?;
    if write.base_version != base.as_ref().map(EnterprisePolicyVersion::reference) {
        return Err(error(
            EnterprisePolicyErrorKind::AuthorityMismatch,
            "enterprise Policy base version is stale or belongs to another scope",
        ));
    }
    let previous_effective = resolve_effective_at_scope(
        connection,
        &write.scope,
        write.policy_kind,
        &write.effective_at,
    )?;
    Ok(PreparedPolicyChain {
        previous_revision,
        version: next_version(head.as_ref())?,
        base,
        previous_effective,
        head_exists: head.is_some(),
    })
}

fn prepare_version(
    connection: &Connection,
    write: &EnterprisePolicyWrite,
    chain: &PreparedPolicyChain,
) -> Result<EnterprisePolicyVersion, EnterprisePolicyError> {
    let relaxes_definition = chain
        .base
        .iter()
        .chain(chain.previous_effective.iter())
        .any(|baseline| !definition_tightens(&write.definition, &baseline.definition));
    let retires_active_child = write.state == EnterprisePolicyState::Retired
        && chain
            .previous_effective
            .as_ref()
            .is_some_and(|previous| previous.state == EnterprisePolicyState::Active);
    let relaxation_authority = validate_inheritance(
        connection,
        write,
        chain.base.as_ref(),
        relaxes_definition || retires_active_child,
    )?;
    let effective_definition_sha256 = effective_definition_digest(
        chain
            .base
            .as_ref()
            .map(|version| &version.effective_definition_sha256),
        &write.definition,
    )?;
    let revision = chain.previous_revision + 1;
    let version_digest = version_digest(
        write,
        chain.version,
        revision,
        chain.base.as_ref(),
        relaxation_authority.as_ref(),
        &effective_definition_sha256,
    )?;
    Ok(EnterprisePolicyVersion {
        policy_id: write.policy_id.clone(),
        policy_kind: write.policy_kind,
        scope: write.scope.clone(),
        mode: write.mode,
        state: write.state,
        version: chain.version,
        source: write.source.clone(),
        effective_at: write.effective_at.clone(),
        inheritance_mode: write.inheritance_mode,
        base_version: chain.base.as_ref().map(EnterprisePolicyVersion::reference),
        relaxation_authority: relaxation_authority
            .as_ref()
            .map(EnterprisePolicyVersion::reference),
        definition: write.definition.clone(),
        definition_sha256: write.definition_sha256.clone(),
        effective_definition_sha256,
        version_digest,
        revision,
        updated_at: write.updated_at.clone(),
    })
}

fn validate_schema(connection: &Connection) -> Result<(), EnterprisePolicyError> {
    for (table, expected) in [
        (
            "enterprise_policy_versions",
            &[
                "sequence",
                "policy_id",
                "version",
                "revision",
                "scope_key",
                "policy_kind",
                "state",
                "effective_at",
                "updated_at",
                "definition_digest",
                "effective_definition_digest",
                "version_digest",
                "record_json",
            ][..],
        ),
        (
            "enterprise_policy_heads",
            &[
                "policy_id",
                "scope_key",
                "policy_kind",
                "head_version",
                "head_revision",
                "head_digest",
            ][..],
        ),
        (
            "enterprise_policy_receipts",
            &[
                "actor_key",
                "scope_key",
                "request_id",
                "command_digest",
                "policy_id",
                "version",
            ][..],
        ),
    ] {
        let mut statement = connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|error| sql_error(&error))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| sql_error(&error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sql_error(&error))?;
        if columns != expected {
            return Err(corrupt("enterprise Policy schema is not canonical"));
        }
    }
    require_unique_indexes(
        connection,
        "enterprise_policy_versions",
        &[
            &["policy_id", "version"],
            &["policy_id", "revision"],
            &["version_digest"],
        ],
    )?;
    require_unique_indexes(
        connection,
        "enterprise_policy_heads",
        &[
            &["policy_id"],
            &["scope_key", "policy_kind"],
            &["head_digest"],
        ],
    )?;
    require_unique_indexes(
        connection,
        "enterprise_policy_receipts",
        &[&["actor_key", "scope_key", "request_id"]],
    )?;
    Ok(())
}

fn require_unique_indexes(
    connection: &Connection,
    table: &str,
    required: &[&[&str]],
) -> Result<(), EnterprisePolicyError> {
    let mut statement = connection
        .prepare(&format!("PRAGMA index_list({table})"))
        .map_err(|error| sql_error(&error))?;
    let indexes = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        })
        .map_err(|error| sql_error(&error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sql_error(&error))?;
    let mut unique_columns = Vec::new();
    for (name, unique) in indexes {
        if unique == 0 {
            continue;
        }
        let mut index_statement = connection
            .prepare(&format!("PRAGMA index_info({name})"))
            .map_err(|error| sql_error(&error))?;
        unique_columns.push(
            index_statement
                .query_map([], |row| row.get::<_, String>(2))
                .map_err(|error| sql_error(&error))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| sql_error(&error))?,
        );
    }
    if required.iter().all(|expected| {
        unique_columns.iter().any(|actual| {
            actual
                .iter()
                .map(String::as_str)
                .eq(expected.iter().copied())
        })
    }) {
        Ok(())
    } else {
        Err(corrupt(
            "enterprise Policy schema uniqueness constraints are not canonical",
        ))
    }
}

fn validate_write(write: &EnterprisePolicyWrite) -> Result<(), EnterprisePolicyError> {
    validate_id(&write.policy_id.0, "pol_", "enterprise Policy id")?;
    validate_scope(&write.scope)?;
    validate_actor(&write.source.actor)?;
    validate_id(
        &write.source.request_id.0,
        "req_",
        "enterprise Policy request id",
    )?;
    validate_instant(&write.effective_at, "enterprise Policy effectiveAt")?;
    validate_instant(&write.updated_at, "enterprise Policy updatedAt")?;
    validate_definition(write.policy_kind, &write.definition)?;
    if digest_sha(&write.definition)? != write.definition_sha256 {
        return Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy definition digest does not match canonical bytes",
        ));
    }
    if !write.scope.is_organization()
        && write.definition.child_override_mode != EnterprisePolicyChildOverrideMode::TightenOnly
    {
        return Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            "only an organization Policy may authorize child relaxation",
        ));
    }
    if write.expected_revision > MAX_SAFE_INTEGER {
        return Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy expected revision exceeds the safe range",
        ));
    }
    Ok(())
}

fn validate_definition(
    policy_kind: EnterprisePolicyKind,
    definition: &EnterprisePolicyDefinition,
) -> Result<(), EnterprisePolicyError> {
    if definition.rules.len() > 256 {
        return Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy has more than 256 rules",
        ));
    }
    let mut seen = HashSet::new();
    for rule in &definition.rules {
        if rule.kind != policy_kind {
            return Err(error(
                EnterprisePolicyErrorKind::InvalidInput,
                "enterprise Policy rule kind differs from its Policy kind",
            ));
        }
        if rule.resource_pattern.is_empty() || rule.resource_pattern.len() > 2048 {
            return Err(error(
                EnterprisePolicyErrorKind::InvalidInput,
                "enterprise Policy resource pattern is invalid",
            ));
        }
        validate_digest(&rule.condition_sha256, "enterprise Policy condition digest")?;
        if !seen.insert(rule) {
            return Err(error(
                EnterprisePolicyErrorKind::InvalidInput,
                "enterprise Policy contains a duplicate rule",
            ));
        }
    }
    let bytes = serde_json::to_vec(definition).map_err(|_| {
        error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy definition is not serializable",
        )
    })?;
    if bytes.len() > MAX_POLICY_BYTES {
        return Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy definition exceeds the durable size limit",
        ));
    }
    Ok(())
}

fn validate_inheritance(
    transaction: &Connection,
    write: &EnterprisePolicyWrite,
    base: Option<&EnterprisePolicyVersion>,
    relaxes: bool,
) -> Result<Option<EnterprisePolicyVersion>, EnterprisePolicyError> {
    if write.scope.is_organization() {
        if write.base_version.is_some()
            || write.inheritance_mode != EnterprisePolicyInheritanceMode::Tighten
        {
            return Err(error(
                EnterprisePolicyErrorKind::InvalidInput,
                "organization Policy versions have no inherited base or override",
            ));
        }
        return Ok(None);
    }
    match write.inheritance_mode {
        EnterprisePolicyInheritanceMode::Tighten if !relaxes => Ok(None),
        EnterprisePolicyInheritanceMode::Tighten => Err(error(
            EnterprisePolicyErrorKind::AuthorityMismatch,
            "child Policy relaxation requires an explicit organization override",
        )),
        EnterprisePolicyInheritanceMode::Override => {
            if base.is_none() {
                return Err(error(
                    EnterprisePolicyErrorKind::InvalidInput,
                    "child Policy override requires an inherited base",
                ));
            }
            let organization_scope = EnterprisePolicyScope::Organization {
                organization_id: write.scope.organization_id().clone(),
            };
            let authority = resolve_effective_at_scope(
                transaction,
                &organization_scope,
                write.policy_kind,
                &write.effective_at,
            )?
            .filter(|version| {
                version.state == EnterprisePolicyState::Active
                    && version.definition.child_override_mode
                        == EnterprisePolicyChildOverrideMode::AllowExplicitRelaxation
            })
            .ok_or_else(|| {
                error(
                    EnterprisePolicyErrorKind::AuthorityMismatch,
                    "organization Policy does not authorize explicit child relaxation",
                )
            })?;
            Ok(Some(authority))
        }
    }
}

fn definition_tightens(
    child: &EnterprisePolicyDefinition,
    parent: &EnterprisePolicyDefinition,
) -> bool {
    if parent.default_effect == EnterprisePolicyEffect::Deny
        && child.default_effect == EnterprisePolicyEffect::Allow
    {
        return false;
    }
    let child_rules = child.rules.iter().collect::<HashSet<_>>();
    let parent_rules = parent.rules.iter().collect::<HashSet<_>>();
    let preserves_denials = parent
        .rules
        .iter()
        .filter(|rule| rule.effect == EnterprisePolicyEffect::Deny)
        .all(|rule| child_rules.contains(rule));
    let adds_no_allowance = parent.default_effect == EnterprisePolicyEffect::Allow
        || child
            .rules
            .iter()
            .filter(|rule| rule.effect == EnterprisePolicyEffect::Allow)
            .all(|rule| parent_rules.contains(rule));
    preserves_denials && adds_no_allowance
}

fn resolve_nearest_parent(
    connection: &Connection,
    scope: &EnterprisePolicyScope,
    kind: EnterprisePolicyKind,
    effective_at: &Instant,
) -> Result<Option<EnterprisePolicyVersion>, EnterprisePolicyError> {
    for ancestor in ancestor_scopes(scope) {
        if let Some(version) =
            resolve_effective_at_scope(connection, &ancestor, kind, effective_at)?
            && version.state == EnterprisePolicyState::Active
        {
            return Ok(Some(version));
        }
    }
    Ok(None)
}

/// Resolves the one active Policy version that governs an exact scope at a
/// frozen instant. The lookup and every returned byte come from the canonical
/// version ledger; callers must keep the surrounding `SQLite` transaction open
/// when they need to commit a decision against the same read cut.
pub(crate) fn resolve_effective_policy(
    connection: &Connection,
    scope: &EnterprisePolicyScope,
    kind: EnterprisePolicyKind,
    effective_at: &Instant,
) -> Result<Option<EnterprisePolicyVersion>, EnterprisePolicyError> {
    validate_scope(scope)?;
    validate_instant(effective_at, "enterprise Policy evaluation instant")?;
    for candidate_scope in std::iter::once(scope.clone()).chain(ancestor_scopes(scope)) {
        if let Some(version) =
            resolve_effective_at_scope(connection, &candidate_scope, kind, effective_at)?
            && version.state == EnterprisePolicyState::Active
        {
            validate_effective_version_seals(connection, &version)?;
            return Ok(Some(version));
        }
    }
    Ok(None)
}

fn validate_effective_version_seals(
    connection: &Connection,
    version: &EnterprisePolicyVersion,
) -> Result<(), EnterprisePolicyError> {
    for reference in version
        .base_version
        .iter()
        .chain(version.relaxation_authority.iter())
    {
        let referenced = load_version(connection, &reference.policy_id, reference.version)?;
        if referenced.reference() != *reference {
            return Err(corrupt(
                "enterprise Policy inheritance seal differs from its immutable version",
            ));
        }
    }
    Ok(())
}

fn resolve_effective_at_scope(
    connection: &Connection,
    scope: &EnterprisePolicyScope,
    kind: EnterprisePolicyKind,
    effective_at: &Instant,
) -> Result<Option<EnterprisePolicyVersion>, EnterprisePolicyError> {
    let coordinates = connection
        .query_row(
            "SELECT policy_id, version FROM enterprise_policy_versions
             WHERE scope_key = ?1 AND policy_kind = ?2 AND state != 'draft'
               AND effective_at <= ?3
             ORDER BY effective_at DESC, version DESC LIMIT 1",
            params![scope_key(scope), kind.as_str(), effective_at.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|error| sql_error(&error))?;
    coordinates
        .map(|(policy_id, version)| {
            load_version(
                connection,
                &EnterprisePolicyId(policy_id),
                from_sql_integer(version, "enterprise Policy version")?,
            )
        })
        .transpose()
}

fn ancestor_scopes(scope: &EnterprisePolicyScope) -> Vec<EnterprisePolicyScope> {
    match scope {
        EnterprisePolicyScope::Organization { .. } => Vec::new(),
        EnterprisePolicyScope::Workspace {
            organization_id, ..
        } => vec![EnterprisePolicyScope::Organization {
            organization_id: organization_id.clone(),
        }],
        EnterprisePolicyScope::Project {
            organization_id,
            workspace_id,
            ..
        } => vec![
            EnterprisePolicyScope::Workspace {
                organization_id: organization_id.clone(),
                workspace_id: workspace_id.clone(),
            },
            EnterprisePolicyScope::Organization {
                organization_id: organization_id.clone(),
            },
        ],
        EnterprisePolicyScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            ..
        } => vec![
            EnterprisePolicyScope::Project {
                organization_id: organization_id.clone(),
                workspace_id: workspace_id.clone(),
                project_id: project_id.clone(),
            },
            EnterprisePolicyScope::Workspace {
                organization_id: organization_id.clone(),
                workspace_id: workspace_id.clone(),
            },
            EnterprisePolicyScope::Organization {
                organization_id: organization_id.clone(),
            },
        ],
    }
}

fn load_head_for_scope_kind(
    connection: &Connection,
    scope_key: &str,
    kind: EnterprisePolicyKind,
) -> Result<Option<StoredHead>, EnterprisePolicyError> {
    connection
        .query_row(
            "SELECT policy_id, head_version, head_revision
             FROM enterprise_policy_heads WHERE scope_key = ?1 AND policy_kind = ?2",
            params![scope_key, kind.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .map(|(policy_id, version, revision)| {
            Ok(StoredHead {
                policy_id: EnterprisePolicyId(policy_id),
                version: from_sql_integer(version, "enterprise Policy head version")?,
                revision: from_sql_integer(revision, "enterprise Policy head revision")?,
            })
        })
        .transpose()
}

fn load_head_for_policy(
    connection: &Connection,
    policy_id: &EnterprisePolicyId,
) -> Result<Option<StoredHead>, EnterprisePolicyError> {
    connection
        .query_row(
            "SELECT policy_id, head_version, head_revision
             FROM enterprise_policy_heads WHERE policy_id = ?1",
            [&policy_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .map(|(policy_id, version, revision)| {
            Ok(StoredHead {
                policy_id: EnterprisePolicyId(policy_id),
                version: from_sql_integer(version, "enterprise Policy head version")?,
                revision: from_sql_integer(revision, "enterprise Policy head revision")?,
            })
        })
        .transpose()
}

fn validate_head_identity(
    head: Option<&StoredHead>,
    write: &EnterprisePolicyWrite,
) -> Result<(), EnterprisePolicyError> {
    if let Some(head) = head {
        if head.policy_id != write.policy_id {
            return Err(error(
                EnterprisePolicyErrorKind::AuthorityMismatch,
                "enterprise Policy scope and kind already belong to another version chain",
            ));
        }
    } else if write.expected_revision != 0 {
        return Err(error(
            EnterprisePolicyErrorKind::RevisionConflict,
            "first enterprise Policy version requires revision zero",
        ));
    }
    Ok(())
}

fn next_version(head: Option<&StoredHead>) -> Result<u64, EnterprisePolicyError> {
    head.map_or(Ok(1), |head| {
        head.version
            .checked_add(1)
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .ok_or_else(|| corrupt("enterprise Policy version overflows"))
    })
}

fn insert_version(
    transaction: &Transaction<'_>,
    record: &EnterprisePolicyVersion,
    scope_key: &str,
) -> Result<(), EnterprisePolicyError> {
    let record_json = encode(record)?;
    transaction
        .execute(
            "INSERT INTO enterprise_policy_versions
             (policy_id, version, revision, scope_key, policy_kind, state,
              effective_at, updated_at, definition_digest,
              effective_definition_digest, version_digest, record_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                record.policy_id.0,
                sql_integer(record.version)?,
                sql_integer(record.revision)?,
                scope_key,
                record.policy_kind.as_str(),
                record.state.as_str(),
                record.effective_at.0,
                record.updated_at.0,
                record.definition_sha256.0,
                record.effective_definition_sha256.0,
                record.version_digest.0,
                record_json,
            ],
        )
        .map_err(|error| sql_error(&error))?;
    Ok(())
}

fn upsert_head(
    transaction: &Transaction<'_>,
    record: &EnterprisePolicyVersion,
    scope_key: &str,
    exists: bool,
) -> Result<(), EnterprisePolicyError> {
    if exists {
        transaction
            .execute(
                "UPDATE enterprise_policy_heads
                 SET head_version = ?2, head_revision = ?3, head_digest = ?4
                 WHERE policy_id = ?1",
                params![
                    record.policy_id.0,
                    sql_integer(record.version)?,
                    sql_integer(record.revision)?,
                    record.version_digest.0,
                ],
            )
            .map_err(|error| sql_error(&error))?;
    } else {
        transaction
            .execute(
                "INSERT INTO enterprise_policy_heads
                 (policy_id, scope_key, policy_kind, head_version, head_revision, head_digest)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    record.policy_id.0,
                    scope_key,
                    record.policy_kind.as_str(),
                    sql_integer(record.version)?,
                    sql_integer(record.revision)?,
                    record.version_digest.0,
                ],
            )
            .map_err(|error| sql_error(&error))?;
    }
    Ok(())
}

fn insert_receipt(
    transaction: &Transaction<'_>,
    actor_key: &str,
    scope_key: &str,
    command_digest: &str,
    record: &EnterprisePolicyVersion,
) -> Result<(), EnterprisePolicyError> {
    transaction
        .execute(
            "INSERT INTO enterprise_policy_receipts
             (actor_key, scope_key, request_id, command_digest, policy_id, version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                actor_key,
                scope_key,
                record.source.request_id.0,
                command_digest,
                record.policy_id.0,
                sql_integer(record.version)?,
            ],
        )
        .map_err(|error| sql_error(&error))?;
    Ok(())
}

fn load_receipt(
    connection: &Connection,
    actor_key: &str,
    scope_key: &str,
    request_id: &RequestId,
) -> Result<Option<StoredReceipt>, EnterprisePolicyError> {
    connection
        .query_row(
            "SELECT command_digest, policy_id, version
             FROM enterprise_policy_receipts
             WHERE actor_key = ?1 AND scope_key = ?2 AND request_id = ?3",
            params![actor_key, scope_key, request_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .map(|(command_digest, policy_id, version)| {
            Ok(StoredReceipt {
                command_digest,
                policy_id: EnterprisePolicyId(policy_id),
                version: from_sql_integer(version, "enterprise Policy receipt version")?,
            })
        })
        .transpose()
}

fn replay_write(
    transaction: Transaction<'_>,
    receipt: &StoredReceipt,
    write: &EnterprisePolicyWrite,
    command_digest: &str,
) -> Result<EnterprisePolicyWriteReceipt, EnterprisePolicyError> {
    if receipt.command_digest != command_digest || receipt.policy_id != write.policy_id {
        return Err(error(
            EnterprisePolicyErrorKind::RequestConflict,
            "enterprise Policy request id already belongs to another command",
        ));
    }
    let record = load_version(&transaction, &receipt.policy_id, receipt.version)?;
    let previous_revision = record
        .revision
        .checked_sub(1)
        .ok_or_else(|| corrupt("enterprise Policy replay revision has no predecessor"))?;
    transaction.commit().map_err(|error| sql_error(&error))?;
    Ok(EnterprisePolicyWriteReceipt {
        version: record,
        previous_revision,
        idempotent_replay: true,
    })
}

fn load_version(
    connection: &Connection,
    policy_id: &EnterprisePolicyId,
    version: u64,
) -> Result<EnterprisePolicyVersion, EnterprisePolicyError> {
    let row = connection
        .query_row(
            "SELECT revision, scope_key, policy_kind, state, effective_at,
                    updated_at, definition_digest, effective_definition_digest,
                    version_digest, record_json
             FROM enterprise_policy_versions WHERE policy_id = ?1 AND version = ?2",
            params![policy_id.0, sql_integer(version)?],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                ))
            },
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .ok_or_else(|| {
            error(
                EnterprisePolicyErrorKind::NotFound,
                "enterprise Policy version was not found",
            )
        })?;
    let record: EnterprisePolicyVersion = serde_json::from_str(&row.9)
        .map_err(|_| corrupt("enterprise Policy version JSON is corrupt"))?;
    if encode(&record)? != row.9
        || record.policy_id != *policy_id
        || record.version != version
        || record.revision != from_sql_integer(row.0, "enterprise Policy revision")?
        || scope_key(&record.scope) != row.1
        || record.policy_kind.as_str() != row.2
        || record.state.as_str() != row.3
        || record.effective_at.0 != row.4
        || record.updated_at.0 != row.5
        || record.definition_sha256.0 != row.6
        || record.effective_definition_sha256.0 != row.7
        || record.version_digest.0 != row.8
    {
        return Err(corrupt(
            "enterprise Policy version columns differ from canonical bytes",
        ));
    }
    validate_stored_version(&record)?;
    Ok(record)
}

fn validate_stored_version(record: &EnterprisePolicyVersion) -> Result<(), EnterprisePolicyError> {
    validate_id(&record.policy_id.0, "pol_", "enterprise Policy id").map_err(as_corrupt)?;
    validate_scope(&record.scope).map_err(as_corrupt)?;
    validate_actor(&record.source.actor).map_err(as_corrupt)?;
    validate_definition(record.policy_kind, &record.definition).map_err(as_corrupt)?;
    if digest_sha(&record.definition)? != record.definition_sha256 {
        return Err(corrupt("enterprise Policy definition digest is corrupt"));
    }
    let effective = effective_definition_digest(
        record
            .base_version
            .as_ref()
            .map(|base| &base.effective_definition_sha256),
        &record.definition,
    )?;
    if effective != record.effective_definition_sha256 {
        return Err(corrupt(
            "enterprise Policy effective definition digest is corrupt",
        ));
    }
    let facts = VersionDigestFacts {
        policy_id: &record.policy_id,
        policy_kind: record.policy_kind,
        scope: &record.scope,
        mode: record.mode,
        state: record.state,
        version: record.version,
        source: &record.source,
        effective_at: &record.effective_at,
        inheritance_mode: record.inheritance_mode,
        base_version: record.base_version.clone(),
        relaxation_authority: record.relaxation_authority.clone(),
        definition_sha256: &record.definition_sha256,
        effective_definition_sha256: &record.effective_definition_sha256,
        revision: record.revision,
        updated_at: &record.updated_at,
    };
    if digest_sha(&facts)? != record.version_digest {
        return Err(corrupt("enterprise Policy version digest is corrupt"));
    }
    Ok(())
}

fn scan_heads(
    connection: &Connection,
    scope_key: &str,
    filter: &EnterprisePolicyFilter,
    snapshot_sequence: u64,
    after_policy_id: Option<&str>,
    limit: u64,
) -> Result<Vec<EnterprisePolicyVersion>, EnterprisePolicyError> {
    let mut statement = connection
        .prepare(
            "SELECT v.policy_id, v.version
             FROM enterprise_policy_versions v
             JOIN (
                 SELECT policy_id, MAX(version) AS version
                 FROM enterprise_policy_versions
                 WHERE scope_key = ?1 AND sequence <= ?2
                 GROUP BY policy_id
             ) heads ON heads.policy_id = v.policy_id AND heads.version = v.version
             WHERE v.policy_id > ?3
             ORDER BY v.policy_id ASC",
        )
        .map_err(|error| sql_error(&error))?;
    let coordinates = statement
        .query_map(
            params![
                scope_key,
                sql_integer(snapshot_sequence)?,
                after_policy_id.unwrap_or(""),
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|error| sql_error(&error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| sql_error(&error))?;
    let mut versions = Vec::new();
    let page_size = usize::try_from(limit).map_err(|_| {
        error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy page limit is invalid",
        )
    })?;
    for (policy_id, version) in coordinates {
        let version = load_version(
            connection,
            &EnterprisePolicyId(policy_id),
            from_sql_integer(version, "enterprise Policy version")?,
        )?;
        if (filter.policy_kinds.is_empty() || filter.policy_kinds.contains(&version.policy_kind))
            && (filter.states.is_empty() || filter.states.contains(&version.state))
        {
            versions.push(version);
            if versions.len() == page_size {
                break;
            }
        }
    }
    Ok(versions)
}

fn validate_filter(filter: &EnterprisePolicyFilter) -> Result<(), EnterprisePolicyError> {
    if filter.policy_kinds.len() > 11 || filter.states.len() > 3 {
        return Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy filter exceeds its bounded set",
        ));
    }
    if filter.policy_kinds.iter().collect::<HashSet<_>>().len() != filter.policy_kinds.len()
        || filter.states.iter().collect::<HashSet<_>>().len() != filter.states.len()
    {
        return Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy filter contains duplicates",
        ));
    }
    Ok(())
}

fn last_sequence(connection: &Connection) -> Result<u64, EnterprisePolicyError> {
    let value = connection
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM enterprise_policy_versions",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| sql_error(&error))?;
    from_sql_nonnegative(value, "enterprise Policy snapshot sequence")
}

fn effective_definition_digest(
    base: Option<&Sha256Digest>,
    definition: &EnterprisePolicyDefinition,
) -> Result<Sha256Digest, EnterprisePolicyError> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct EffectiveFacts<'a> {
        base_effective_definition_sha256: Option<&'a Sha256Digest>,
        definition: &'a EnterprisePolicyDefinition,
    }
    digest_sha(&EffectiveFacts {
        base_effective_definition_sha256: base,
        definition,
    })
}

fn write_command_digest(write: &EnterprisePolicyWrite) -> Result<String, EnterprisePolicyError> {
    digest(&WriteCommandDigestFacts {
        policy_id: &write.policy_id,
        policy_kind: write.policy_kind,
        scope: &write.scope,
        mode: write.mode,
        state: write.state,
        definition: &write.definition,
        definition_sha256: &write.definition_sha256,
        effective_at: &write.effective_at,
        inheritance_mode: write.inheritance_mode,
        base_version: &write.base_version,
        expected_revision: write.expected_revision,
        source: &write.source,
    })
}

fn version_digest(
    write: &EnterprisePolicyWrite,
    version: u64,
    revision: u64,
    base: Option<&EnterprisePolicyVersion>,
    relaxation: Option<&EnterprisePolicyVersion>,
    effective_definition_sha256: &Sha256Digest,
) -> Result<Sha256Digest, EnterprisePolicyError> {
    digest_sha(&VersionDigestFacts {
        policy_id: &write.policy_id,
        policy_kind: write.policy_kind,
        scope: &write.scope,
        mode: write.mode,
        state: write.state,
        version,
        source: &write.source,
        effective_at: &write.effective_at,
        inheritance_mode: write.inheritance_mode,
        base_version: base.map(EnterprisePolicyVersion::reference),
        relaxation_authority: relaxation.map(EnterprisePolicyVersion::reference),
        definition_sha256: &write.definition_sha256,
        effective_definition_sha256,
        revision,
        updated_at: &write.updated_at,
    })
}

fn scope_key(scope: &EnterprisePolicyScope) -> String {
    match scope {
        EnterprisePolicyScope::Organization { organization_id } => {
            format!("organization:{}", organization_id.0)
        }
        EnterprisePolicyScope::Workspace {
            organization_id,
            workspace_id,
        } => format!(
            "organization:{}/workspace:{}",
            organization_id.0, workspace_id.0
        ),
        EnterprisePolicyScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => format!(
            "organization:{}/workspace:{}/project:{}",
            organization_id.0, workspace_id.0, project_id.0
        ),
        EnterprisePolicyScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => format!(
            "organization:{}/workspace:{}/project:{}/repository:{}",
            organization_id.0, workspace_id.0, project_id.0, repository_id.0
        ),
    }
}

fn actor_key(actor: &EnterprisePolicyActor) -> Result<String, EnterprisePolicyError> {
    validate_actor(actor)?;
    Ok(match actor {
        EnterprisePolicyActor::User { id } => format!("user:{}", id.0),
        EnterprisePolicyActor::ServiceAccount { id } => format!("service_account:{}", id.0),
        EnterprisePolicyActor::System { id } => format!("system:{}", id.0),
    })
}

fn validate_scope(scope: &EnterprisePolicyScope) -> Result<(), EnterprisePolicyError> {
    validate_id(
        &scope.organization_id().0,
        "org_",
        "enterprise Policy organization id",
    )?;
    match scope {
        EnterprisePolicyScope::Organization { .. } => {}
        EnterprisePolicyScope::Workspace { workspace_id, .. } => {
            validate_id(&workspace_id.0, "wsp_", "enterprise Policy workspace id")?;
        }
        EnterprisePolicyScope::Project {
            workspace_id,
            project_id,
            ..
        } => {
            validate_id(&workspace_id.0, "wsp_", "enterprise Policy workspace id")?;
            validate_id(&project_id.0, "prj_", "enterprise Policy project id")?;
        }
        EnterprisePolicyScope::Repository {
            workspace_id,
            project_id,
            repository_id,
            ..
        } => {
            validate_id(&workspace_id.0, "wsp_", "enterprise Policy workspace id")?;
            validate_id(&project_id.0, "prj_", "enterprise Policy project id")?;
            validate_id(&repository_id.0, "rep_", "enterprise Policy repository id")?;
        }
    }
    Ok(())
}

fn validate_actor(actor: &EnterprisePolicyActor) -> Result<(), EnterprisePolicyError> {
    match actor {
        EnterprisePolicyActor::User { id } => {
            validate_id(&id.0, "usr_", "enterprise Policy user actor")
        }
        EnterprisePolicyActor::ServiceAccount { id } => {
            validate_id(&id.0, "svc_", "enterprise Policy service account actor")
        }
        EnterprisePolicyActor::System { id } => {
            validate_id(&id.0, "sys_", "enterprise Policy system actor")
        }
    }
}

fn validate_id(value: &str, prefix: &str, field: &str) -> Result<(), EnterprisePolicyError> {
    let valid = value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            })
    });
    if valid {
        Ok(())
    } else {
        Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            format!("{field} is not canonical"),
        ))
    }
}

fn validate_digest(value: &Sha256Digest, field: &str) -> Result<(), EnterprisePolicyError> {
    let valid = value.0.strip_prefix("sha256:").is_some_and(|suffix| {
        suffix.len() == 64
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    });
    if valid {
        Ok(())
    } else {
        Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            format!("{field} is not canonical"),
        ))
    }
}

fn validate_instant(value: &Instant, field: &str) -> Result<(), EnterprisePolicyError> {
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
        Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            format!("{field} is not canonical"),
        ))
    }
}

fn positive_safe(value: u64, field: &str) -> Result<(), EnterprisePolicyError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        return Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            format!("{field} is outside the safe positive range"),
        ));
    }
    Ok(())
}

fn sql_integer(value: u64) -> Result<i64, EnterprisePolicyError> {
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy integer exceeds the safe range",
        ));
    }
    i64::try_from(value).map_err(|_| corrupt("enterprise Policy integer exceeds SQLite range"))
}

fn from_sql_integer(value: i64, field: &str) -> Result<u64, EnterprisePolicyError> {
    let value = u64::try_from(value).map_err(|_| corrupt(format!("{field} is negative")))?;
    positive_safe(value, field).map_err(as_corrupt)?;
    Ok(value)
}

fn from_sql_nonnegative(value: i64, field: &str) -> Result<u64, EnterprisePolicyError> {
    let value = u64::try_from(value).map_err(|_| corrupt(format!("{field} is negative")))?;
    if value > MAX_SAFE_INTEGER {
        return Err(corrupt(format!("{field} exceeds the safe range")));
    }
    Ok(value)
}

fn digest<T: Serialize>(value: &T) -> Result<String, EnterprisePolicyError> {
    Ok(digest_sha(value)?.0)
}

fn digest_sha<T: Serialize>(value: &T) -> Result<Sha256Digest, EnterprisePolicyError> {
    let canonical = serde_json::to_value(value).map_err(|_| {
        error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy value is not serializable",
        )
    })?;
    let bytes = serde_json::to_vec(&canonical).map_err(|_| {
        error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy value is not serializable",
        )
    })?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn encode<T: Serialize>(value: &T) -> Result<String, EnterprisePolicyError> {
    serde_json::to_string(value).map_err(|_| {
        error(
            EnterprisePolicyErrorKind::InvalidInput,
            "enterprise Policy value is not serializable",
        )
    })
}

fn storage_error(error_value: &StorageError) -> EnterprisePolicyError {
    error(
        EnterprisePolicyErrorKind::Storage,
        format!("enterprise Policy storage failed: {error_value}"),
    )
}

fn sql_error(error_value: &rusqlite::Error) -> EnterprisePolicyError {
    error(
        EnterprisePolicyErrorKind::Storage,
        format!("enterprise Policy SQLite operation failed: {error_value}"),
    )
}

fn as_corrupt(error_value: EnterprisePolicyError) -> EnterprisePolicyError {
    error(EnterprisePolicyErrorKind::CorruptState, error_value.message)
}

fn corrupt(message: impl Into<String>) -> EnterprisePolicyError {
    error(EnterprisePolicyErrorKind::CorruptState, message)
}

fn error(kind: EnterprisePolicyErrorKind, message: impl Into<String>) -> EnterprisePolicyError {
    EnterprisePolicyError::new(kind, message)
}
