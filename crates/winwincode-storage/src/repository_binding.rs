// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use winwincode_domain::Instant;

use crate::{SqliteStorage, StorageError};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 96;
const MAX_DISPLAY_NAME_CHARS: usize = 200;
const MAX_FINGERPRINT_BYTES: usize = 128;
const MAX_DEFAULT_BRANCH_BYTES: usize = 256;

const REPOSITORY_BINDING_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS repository_bindings (
    repository_binding_id TEXT PRIMARY KEY NOT NULL,
    client_node_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    repository_kind TEXT NOT NULL CHECK (repository_kind IN ('git')),
    default_branch TEXT,
    head_commit TEXT,
    dirty_state TEXT NOT NULL CHECK (dirty_state IN ('clean', 'dirty')),
    availability TEXT NOT NULL CHECK (availability IN (
        'available', 'dirty', 'unavailable', 'moved',
        'invalid_git', 'permission_denied', 'scan_failed')),
    repository_fingerprint TEXT NOT NULL,
    last_scanned_at TEXT,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0 AND revision <= 9007199254740991),
    FOREIGN KEY (client_node_id) REFERENCES client_nodes(client_node_id) ON DELETE RESTRICT
);
CREATE UNIQUE INDEX IF NOT EXISTS repository_bindings_one_per_fingerprint_per_client
    ON repository_bindings (client_node_id, repository_fingerprint);
CREATE INDEX IF NOT EXISTS repository_bindings_by_client
    ON repository_bindings (client_node_id, availability);
CREATE TABLE IF NOT EXISTS repository_access_grants (
    repository_access_grant_id TEXT PRIMARY KEY NOT NULL,
    repository_binding_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    permissions TEXT NOT NULL CHECK (permissions IN ('use', 'use+manage')),
    state TEXT NOT NULL CHECK (state IN ('active', 'revoked')),
    granted_by_user_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0 AND revision <= 9007199254740991),
    FOREIGN KEY (repository_binding_id)
        REFERENCES repository_bindings(repository_binding_id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX IF NOT EXISTS repository_access_grants_one_active_per_user_binding
    ON repository_access_grants (repository_binding_id, user_id) WHERE state = 'active';
CREATE INDEX IF NOT EXISTS repository_access_grants_by_user
    ON repository_access_grants (user_id, state);
";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryAvailability {
    Available,
    Dirty,
    Unavailable,
    Moved,
    InvalidGit,
    PermissionDenied,
    ScanFailed,
}

impl RepositoryAvailability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Dirty => "dirty",
            Self::Unavailable => "unavailable",
            Self::Moved => "moved",
            Self::InvalidGit => "invalid_git",
            Self::PermissionDenied => "permission_denied",
            Self::ScanFailed => "scan_failed",
        }
    }

    fn parse(value: &str) -> Result<Self, RepositoryBindingStoreError> {
        match value {
            "available" => Ok(Self::Available),
            "dirty" => Ok(Self::Dirty),
            "unavailable" => Ok(Self::Unavailable),
            "moved" => Ok(Self::Moved),
            "invalid_git" => Ok(Self::InvalidGit),
            "permission_denied" => Ok(Self::PermissionDenied),
            "scan_failed" => Ok(Self::ScanFailed),
            _ => Err(error(
                RepositoryBindingStoreErrorKind::CorruptState,
                "stored repository availability state is invalid",
            )),
        }
    }

    #[must_use]
    pub const fn allows_launch(self) -> bool {
        matches!(self, Self::Available | Self::Dirty)
    }
}

impl fmt::Display for RepositoryAvailability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryDirtyState {
    Clean,
    Dirty,
}

impl RepositoryDirtyState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Dirty => "dirty",
        }
    }

    fn parse(value: &str) -> Result<Self, RepositoryBindingStoreError> {
        match value {
            "clean" => Ok(Self::Clean),
            "dirty" => Ok(Self::Dirty),
            _ => Err(error(
                RepositoryBindingStoreErrorKind::CorruptState,
                "stored repository dirty state is invalid",
            )),
        }
    }
}

impl fmt::Display for RepositoryDirtyState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryGrantPermissions {
    Use,
    UseManage,
}

impl RepositoryGrantPermissions {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::UseManage => "use+manage",
        }
    }

    fn parse(value: &str) -> Result<Self, RepositoryBindingStoreError> {
        match value {
            "use" => Ok(Self::Use),
            "use+manage" => Ok(Self::UseManage),
            _ => Err(error(
                RepositoryBindingStoreErrorKind::CorruptState,
                "stored repository grant permissions are invalid",
            )),
        }
    }
}

impl fmt::Display for RepositoryGrantPermissions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryGrantState {
    Active,
    Revoked,
}

impl RepositoryGrantState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Revoked => "revoked",
        }
    }

    fn parse(value: &str) -> Result<Self, RepositoryBindingStoreError> {
        match value {
            "active" => Ok(Self::Active),
            "revoked" => Ok(Self::Revoked),
            _ => Err(error(
                RepositoryBindingStoreErrorKind::CorruptState,
                "stored repository grant state is invalid",
            )),
        }
    }
}

impl fmt::Display for RepositoryGrantState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryBindingProjection {
    repository_binding_id: String,
    client_node_id: String,
    display_name: String,
    default_branch: Option<String>,
    head_commit: Option<String>,
    dirty_state: RepositoryDirtyState,
    availability: RepositoryAvailability,
    repository_fingerprint: String,
}

impl RepositoryBindingProjection {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        repository_binding_id: impl Into<String>,
        client_node_id: impl Into<String>,
        display_name: impl Into<String>,
        default_branch: Option<String>,
        head_commit: Option<String>,
        dirty_state: RepositoryDirtyState,
        availability: RepositoryAvailability,
        repository_fingerprint: impl Into<String>,
    ) -> Result<Self, RepositoryBindingStoreError> {
        let projection = Self {
            repository_binding_id: repository_binding_id.into(),
            client_node_id: client_node_id.into(),
            display_name: display_name.into(),
            default_branch,
            head_commit,
            dirty_state,
            availability,
            repository_fingerprint: repository_fingerprint.into(),
        };
        projection.validate()?;
        Ok(projection)
    }

    fn validate(&self) -> Result<(), RepositoryBindingStoreError> {
        validate_repository_binding_id(&self.repository_binding_id)?;
        validate_client_node_id(&self.client_node_id)?;
        if self.display_name.is_empty()
            || self.display_name.chars().count() > MAX_DISPLAY_NAME_CHARS
        {
            return Err(error(
                RepositoryBindingStoreErrorKind::InvalidInput,
                "repository display name must contain 1 to 200 characters",
            ));
        }
        if let Some(branch) = &self.default_branch {
            validate_default_branch(branch)?;
        }
        if let Some(commit) = &self.head_commit {
            validate_head_commit(commit)?;
        }
        ensure_dirty_state_consistent(self.availability, self.dirty_state)?;
        validate_fingerprint(&self.repository_fingerprint)?;
        Ok(())
    }

    #[must_use]
    pub fn repository_binding_id(&self) -> &str {
        &self.repository_binding_id
    }

    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.client_node_id
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    #[must_use]
    pub fn default_branch(&self) -> Option<&str> {
        self.default_branch.as_deref()
    }

    #[must_use]
    pub fn head_commit(&self) -> Option<&str> {
        self.head_commit.as_deref()
    }

    #[must_use]
    pub const fn dirty_state(&self) -> RepositoryDirtyState {
        self.dirty_state
    }

    #[must_use]
    pub const fn availability(&self) -> RepositoryAvailability {
        self.availability
    }

    #[must_use]
    pub fn repository_fingerprint(&self) -> &str {
        &self.repository_fingerprint
    }

    fn is_idempotent_replay_of(
        &self,
        record: &RepositoryBindingRecord,
        last_scanned_at: Option<&Instant>,
    ) -> bool {
        self.repository_binding_id == record.repository_binding_id
            && self.client_node_id == record.client_node_id
            && self.display_name == record.display_name
            && self.default_branch.as_deref() == record.default_branch.as_deref()
            && self.head_commit.as_deref() == record.head_commit.as_deref()
            && self.dirty_state == record.dirty_state
            && self.availability == record.availability
            && self.repository_fingerprint == record.repository_fingerprint
            && last_scanned_at == record.last_scanned_at.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryScanOutcome {
    availability: RepositoryAvailability,
    dirty_state: RepositoryDirtyState,
}

impl RepositoryScanOutcome {
    pub fn try_new(
        availability: RepositoryAvailability,
        dirty_state: RepositoryDirtyState,
    ) -> Result<Self, RepositoryBindingStoreError> {
        ensure_dirty_state_consistent(availability, dirty_state)?;
        Ok(Self {
            availability,
            dirty_state,
        })
    }

    #[must_use]
    pub const fn availability(&self) -> RepositoryAvailability {
        self.availability
    }

    #[must_use]
    pub const fn dirty_state(&self) -> RepositoryDirtyState {
        self.dirty_state
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryBindingRecord {
    pub repository_binding_id: String,
    pub client_node_id: String,
    pub display_name: String,
    pub repository_kind: String,
    pub default_branch: Option<String>,
    pub head_commit: Option<String>,
    pub dirty_state: RepositoryDirtyState,
    pub availability: RepositoryAvailability,
    pub repository_fingerprint: String,
    pub last_scanned_at: Option<Instant>,
    pub created_at: Instant,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryBindingReceipt {
    pub record: RepositoryBindingRecord,
    pub enrolled: bool,
}

#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryAccessGrantIssuance {
    repository_access_grant_id: String,
    repository_binding_id: String,
    user_id: String,
    granted_by_user_id: String,
}

impl RepositoryAccessGrantIssuance {
    pub fn try_new(
        repository_access_grant_id: impl Into<String>,
        repository_binding_id: impl Into<String>,
        user_id: impl Into<String>,
        granted_by_user_id: impl Into<String>,
    ) -> Result<Self, RepositoryBindingStoreError> {
        let issuance = Self {
            repository_access_grant_id: repository_access_grant_id.into(),
            repository_binding_id: repository_binding_id.into(),
            user_id: user_id.into(),
            granted_by_user_id: granted_by_user_id.into(),
        };
        issuance.validate()?;
        Ok(issuance)
    }

    fn validate(&self) -> Result<(), RepositoryBindingStoreError> {
        validate_repository_access_grant_id(&self.repository_access_grant_id)?;
        validate_repository_binding_id(&self.repository_binding_id)?;
        validate_user_id(&self.user_id)?;
        validate_user_id(&self.granted_by_user_id)?;
        Ok(())
    }

    #[must_use]
    pub fn repository_access_grant_id(&self) -> &str {
        &self.repository_access_grant_id
    }

    #[must_use]
    pub fn repository_binding_id(&self) -> &str {
        &self.repository_binding_id
    }

    #[must_use]
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    #[must_use]
    pub fn granted_by_user_id(&self) -> &str {
        &self.granted_by_user_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryAccessGrantRecord {
    pub repository_access_grant_id: String,
    pub repository_binding_id: String,
    pub user_id: String,
    pub permissions: RepositoryGrantPermissions,
    pub state: RepositoryGrantState,
    pub granted_by_user_id: String,
    pub created_at: Instant,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryBindingStoreErrorKind {
    InvalidInput,
    UnknownClientNode,
    UnknownRepositoryBinding,
    UnknownAccessGrant,
    FingerprintConflict,
    AccessGrantConflict,
    RevisionConflict,
    CorruptState,
    Storage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryBindingStoreError {
    kind: RepositoryBindingStoreErrorKind,
    message: String,
}

impl RepositoryBindingStoreError {
    #[must_use]
    pub const fn kind(&self) -> RepositoryBindingStoreErrorKind {
        self.kind
    }
}

impl fmt::Display for RepositoryBindingStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RepositoryBindingStoreError {}

pub struct RepositoryBindingLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    pub fn repository_binding_ledger(
        &mut self,
    ) -> Result<RepositoryBindingLedger<'_>, RepositoryBindingStoreError> {
        RepositoryBindingLedger::new(self)
    }
}

impl<'storage> RepositoryBindingLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, RepositoryBindingStoreError> {
        let connection = storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .execute_batch(REPOSITORY_BINDING_SCHEMA)
            .map_err(|sql| sql_error(&sql))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    pub fn upsert(
        &mut self,
        projection: &RepositoryBindingProjection,
        last_scanned_at: Option<&Instant>,
        expected_revision: u64,
        now: &Instant,
    ) -> Result<RepositoryBindingReceipt, RepositoryBindingStoreError> {
        projection.validate()?;
        if let Some(scanned) = last_scanned_at {
            validate_instant(scanned, "last scanned")?;
        }
        validate_revision(expected_revision)?;
        validate_instant(now, "upsert time")?;
        let transaction = self.transaction()?;
        require_client_node(&transaction, projection.client_node_id())?;
        let existing = load_binding(&transaction, projection.repository_binding_id())?;
        let receipt = match existing {
            None => {
                ensure_fingerprint_free(
                    &transaction,
                    projection,
                    projection.repository_binding_id(),
                )?;
                insert_binding(&transaction, projection, last_scanned_at, now)?;
                upsert_receipt(&transaction, projection, true, "insert")?
            }
            Some(record) => {
                if projection.client_node_id() != record.client_node_id {
                    return Err(error(
                        RepositoryBindingStoreErrorKind::FingerprintConflict,
                        "repository binding id is already used by another client node",
                    ));
                }
                if projection.is_idempotent_replay_of(&record, last_scanned_at) {
                    upsert_receipt(&transaction, projection, false, "replay")?
                } else {
                    ensure_binding_revision(&record, expected_revision)?;
                    ensure_fingerprint_free(
                        &transaction,
                        projection,
                        projection.repository_binding_id(),
                    )?;
                    refresh_binding(&transaction, projection, last_scanned_at, &record)?;
                    upsert_receipt(&transaction, projection, false, "update")?
                }
            }
        };
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(receipt)
    }

    pub fn update_availability(
        &mut self,
        repository_binding_id: &str,
        outcome: &RepositoryScanOutcome,
        last_scanned_at: &Instant,
        expected_revision: u64,
    ) -> Result<RepositoryBindingRecord, RepositoryBindingStoreError> {
        validate_repository_binding_id(repository_binding_id)?;
        validate_instant(last_scanned_at, "last scanned")?;
        validate_revision(expected_revision)?;
        let transaction = self.transaction()?;
        let record = require_binding(&transaction, repository_binding_id)?;
        let replay = record.availability == outcome.availability()
            && record.dirty_state == outcome.dirty_state()
            && record.last_scanned_at.as_ref() == Some(last_scanned_at);
        if replay {
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        ensure_binding_revision(&record, expected_revision)?;
        let updated = transaction
            .execute(
                "UPDATE repository_bindings
                 SET availability = ?2, dirty_state = ?3, last_scanned_at = ?4,
                     revision = revision + 1
                 WHERE repository_binding_id = ?1 AND revision = ?5",
                params![
                    repository_binding_id,
                    outcome.availability().as_str(),
                    outcome.dirty_state().as_str(),
                    last_scanned_at.0,
                    sql_integer(record.revision)?,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(error(
                RepositoryBindingStoreErrorKind::RevisionConflict,
                "repository binding revision changed during availability update",
            ));
        }
        let updated = require_binding(&transaction, repository_binding_id)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    pub fn remove(
        &mut self,
        repository_binding_id: &str,
    ) -> Result<bool, RepositoryBindingStoreError> {
        validate_repository_binding_id(repository_binding_id)?;
        let transaction = self.transaction()?;
        let deleted = transaction
            .execute(
                "DELETE FROM repository_bindings WHERE repository_binding_id = ?1",
                [repository_binding_id],
            )
            .map_err(|sql| sql_error(&sql))?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(deleted == 1)
    }

    pub fn snapshot(
        &self,
        repository_binding_id: &str,
    ) -> Result<Option<RepositoryBindingRecord>, RepositoryBindingStoreError> {
        validate_repository_binding_id(repository_binding_id)?;
        load_binding(self.connection()?, repository_binding_id)
    }

    pub fn bindings_for_client(
        &self,
        client_node_id: &str,
    ) -> Result<Vec<RepositoryBindingRecord>, RepositoryBindingStoreError> {
        validate_client_node_id(client_node_id)?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {BINDING_COLUMNS} FROM repository_bindings AS rb
                 WHERE client_node_id = ?1
                 ORDER BY created_at, repository_binding_id"
            ))
            .map_err(|sql| sql_error(&sql))?;
        let records = statement
            .query_map([client_node_id], read_binding_row)
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        records.into_iter().map(binding_from_row).collect()
    }

    pub fn create_grant(
        &mut self,
        issuance: &RepositoryAccessGrantIssuance,
        permissions: RepositoryGrantPermissions,
        now: &Instant,
    ) -> Result<RepositoryAccessGrantRecord, RepositoryBindingStoreError> {
        issuance.validate()?;
        validate_instant(now, "grant creation time")?;
        let transaction = self.transaction()?;
        require_binding(&transaction, issuance.repository_binding_id())?;
        let inserted = transaction
            .execute(
                "INSERT INTO repository_access_grants
                 (repository_access_grant_id, repository_binding_id, user_id,
                  permissions, state, granted_by_user_id, created_at, revision)
                 VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, 1)",
                params![
                    issuance.repository_access_grant_id(),
                    issuance.repository_binding_id(),
                    issuance.user_id(),
                    permissions.as_str(),
                    issuance.granted_by_user_id(),
                    now.0,
                ],
            )
            .map_err(|sql| map_grant_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                RepositoryBindingStoreErrorKind::Storage,
                "repository access grant insert did not store exactly one row",
            ));
        }
        let record =
            load_grant(&transaction, issuance.repository_access_grant_id())?.ok_or_else(|| {
                error(
                    RepositoryBindingStoreErrorKind::CorruptState,
                    "repository access grant row is missing after insert",
                )
            })?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(record)
    }

    pub fn revoke_grant(
        &mut self,
        repository_access_grant_id: &str,
        expected_revision: u64,
    ) -> Result<RepositoryAccessGrantRecord, RepositoryBindingStoreError> {
        validate_repository_access_grant_id(repository_access_grant_id)?;
        validate_revision(expected_revision)?;
        let transaction = self.transaction()?;
        let record = require_grant(&transaction, repository_access_grant_id)?;
        ensure_grant_revision(&record, expected_revision)?;
        if record.state == RepositoryGrantState::Revoked {
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        let updated = transaction
            .execute(
                "UPDATE repository_access_grants
                 SET state = 'revoked', revision = revision + 1
                 WHERE repository_access_grant_id = ?1
                   AND state = 'active' AND revision = ?2",
                params![repository_access_grant_id, sql_integer(record.revision)?],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(error(
                RepositoryBindingStoreErrorKind::RevisionConflict,
                "repository access grant revision changed during revoke",
            ));
        }
        let updated = require_grant(&transaction, repository_access_grant_id)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    pub fn grant_snapshot(
        &self,
        repository_access_grant_id: &str,
    ) -> Result<Option<RepositoryAccessGrantRecord>, RepositoryBindingStoreError> {
        validate_repository_access_grant_id(repository_access_grant_id)?;
        load_grant(self.connection()?, repository_access_grant_id)
    }

    pub fn active_grants_for_binding(
        &self,
        repository_binding_id: &str,
    ) -> Result<Vec<RepositoryAccessGrantRecord>, RepositoryBindingStoreError> {
        validate_repository_binding_id(repository_binding_id)?;
        self.active_grants_filtered("rag.repository_binding_id = ?1", repository_binding_id)
    }

    pub fn active_grants_for_user(
        &self,
        user_id: &str,
    ) -> Result<Vec<RepositoryAccessGrantRecord>, RepositoryBindingStoreError> {
        validate_user_id(user_id)?;
        self.active_grants_filtered("rag.user_id = ?1", user_id)
    }

    fn active_grants_filtered(
        &self,
        predicate: &str,
        subject: &str,
    ) -> Result<Vec<RepositoryAccessGrantRecord>, RepositoryBindingStoreError> {
        let connection = self.connection()?;
        let sql = format!(
            "SELECT {GRANT_COLUMNS}
             FROM repository_access_grants AS rag
             WHERE {predicate} AND rag.state = 'active'
             ORDER BY rag.created_at, rag.repository_access_grant_id"
        );
        let mut statement = connection.prepare(&sql).map_err(|sql| sql_error(&sql))?;
        let records = statement
            .query_map([subject], read_grant_row)
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        records.into_iter().map(grant_from_row).collect()
    }

    pub fn visible_bindings(
        &self,
        user_id: &str,
        client_node_id: &str,
    ) -> Result<Vec<RepositoryBindingRecord>, RepositoryBindingStoreError> {
        validate_user_id(user_id)?;
        validate_client_node_id(client_node_id)?;
        let connection = self.connection()?;
        let sql = format!(
            "SELECT {BINDING_COLUMNS}
             FROM repository_bindings AS rb
             WHERE rb.client_node_id = ?1
               AND EXISTS (
                   SELECT 1 FROM client_access_grants AS cag
                   WHERE cag.client_node_id = rb.client_node_id
                     AND cag.user_id = ?2
                     AND cag.state = 'active'
                     AND cag.permissions IN
                         ('use', 'use+manage', 'use+share', 'use+manage+share'))
               AND EXISTS (
                   SELECT 1 FROM repository_access_grants AS rag
                   WHERE rag.repository_binding_id = rb.repository_binding_id
                     AND rag.user_id = ?2
                     AND rag.state = 'active')
             ORDER BY rb.created_at, rb.repository_binding_id"
        );
        let mut statement = connection.prepare(&sql).map_err(|sql| sql_error(&sql))?;
        let records = statement
            .query_map(params![client_node_id, user_id], read_binding_row)
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        records.into_iter().map(binding_from_row).collect()
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, RepositoryBindingStoreError> {
        self.storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|sql| sql_error(&sql))
    }

    fn connection(&self) -> Result<&rusqlite::Connection, RepositoryBindingStoreError> {
        self.storage
            .connection()
            .map_err(|storage| storage_error(&storage))
    }
}

const BINDING_COLUMNS: &str = "rb.repository_binding_id, rb.client_node_id, rb.display_name,
    rb.repository_kind, rb.default_branch, rb.head_commit, rb.dirty_state, rb.availability,
    rb.repository_fingerprint, rb.last_scanned_at, rb.created_at, rb.revision";

const GRANT_COLUMNS: &str = "rag.repository_access_grant_id, rag.repository_binding_id,
    rag.user_id, rag.permissions, rag.state, rag.granted_by_user_id, rag.created_at,
    rag.revision";

fn ensure_dirty_state_consistent(
    availability: RepositoryAvailability,
    dirty_state: RepositoryDirtyState,
) -> Result<(), RepositoryBindingStoreError> {
    let consistent = match availability {
        RepositoryAvailability::Available => dirty_state == RepositoryDirtyState::Clean,
        RepositoryAvailability::Dirty => dirty_state == RepositoryDirtyState::Dirty,
        RepositoryAvailability::Unavailable
        | RepositoryAvailability::Moved
        | RepositoryAvailability::InvalidGit
        | RepositoryAvailability::PermissionDenied
        | RepositoryAvailability::ScanFailed => true,
    };
    if consistent {
        Ok(())
    } else {
        Err(error(
            RepositoryBindingStoreErrorKind::InvalidInput,
            "availability and dirty state contradict each other",
        ))
    }
}

fn require_client_node(
    transaction: &Transaction<'_>,
    client_node_id: &str,
) -> Result<(), RepositoryBindingStoreError> {
    let exists = transaction
        .query_row(
            "SELECT 1 FROM client_nodes WHERE client_node_id = ?1",
            [client_node_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    if exists.is_some() {
        Ok(())
    } else {
        Err(unknown_client_node())
    }
}

fn ensure_fingerprint_free(
    transaction: &Transaction<'_>,
    projection: &RepositoryBindingProjection,
    binding_id: &str,
) -> Result<(), RepositoryBindingStoreError> {
    let holder = transaction
        .query_row(
            "SELECT repository_binding_id FROM repository_bindings
             WHERE client_node_id = ?1 AND repository_fingerprint = ?2",
            params![
                projection.client_node_id(),
                projection.repository_fingerprint()
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    match holder {
        None => Ok(()),
        Some(holder) if holder == binding_id => Ok(()),
        Some(_) => Err(error(
            RepositoryBindingStoreErrorKind::FingerprintConflict,
            "repository fingerprint is already bound on this client node",
        )),
    }
}

fn insert_binding(
    transaction: &Transaction<'_>,
    projection: &RepositoryBindingProjection,
    last_scanned_at: Option<&Instant>,
    now: &Instant,
) -> Result<(), RepositoryBindingStoreError> {
    let inserted = transaction
        .execute(
            "INSERT INTO repository_bindings
             (repository_binding_id, client_node_id, display_name, repository_kind,
              default_branch, head_commit, dirty_state, availability,
              repository_fingerprint, last_scanned_at, created_at, revision)
             VALUES (?1, ?2, ?3, 'git', ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1)",
            params![
                projection.repository_binding_id(),
                projection.client_node_id(),
                projection.display_name(),
                projection.default_branch(),
                projection.head_commit(),
                projection.dirty_state().as_str(),
                projection.availability().as_str(),
                projection.repository_fingerprint(),
                last_scanned_at.map(|instant| instant.0.clone()),
                now.0,
            ],
        )
        .map_err(|sql| map_binding_insert_sql(&sql))?;
    if inserted != 1 {
        return Err(error(
            RepositoryBindingStoreErrorKind::Storage,
            "repository binding insert did not store exactly one row",
        ));
    }
    Ok(())
}

fn refresh_binding(
    transaction: &Transaction<'_>,
    projection: &RepositoryBindingProjection,
    last_scanned_at: Option<&Instant>,
    record: &RepositoryBindingRecord,
) -> Result<(), RepositoryBindingStoreError> {
    let updated = transaction
        .execute(
            "UPDATE repository_bindings
             SET display_name = ?2, default_branch = ?3, head_commit = ?4,
                 dirty_state = ?5, availability = ?6, repository_fingerprint = ?7,
                 last_scanned_at = ?8, revision = revision + 1
             WHERE repository_binding_id = ?1 AND revision = ?9",
            params![
                projection.repository_binding_id(),
                projection.display_name(),
                projection.default_branch(),
                projection.head_commit(),
                projection.dirty_state().as_str(),
                projection.availability().as_str(),
                projection.repository_fingerprint(),
                last_scanned_at.map(|instant| instant.0.clone()),
                sql_integer(record.revision)?,
            ],
        )
        .map_err(|sql| sql_error(&sql))?;
    if updated != 1 {
        return Err(error(
            RepositoryBindingStoreErrorKind::RevisionConflict,
            "repository binding revision changed during projection refresh",
        ));
    }
    Ok(())
}

fn upsert_receipt(
    transaction: &Transaction<'_>,
    projection: &RepositoryBindingProjection,
    enrolled: bool,
    phase: &str,
) -> Result<RepositoryBindingReceipt, RepositoryBindingStoreError> {
    let record =
        load_binding(transaction, projection.repository_binding_id())?.ok_or_else(|| {
            error(
                RepositoryBindingStoreErrorKind::CorruptState,
                format!("repository binding row is missing after {phase}"),
            )
        })?;
    Ok(RepositoryBindingReceipt { record, enrolled })
}

fn load_binding(
    connection: &rusqlite::Connection,
    repository_binding_id: &str,
) -> Result<Option<RepositoryBindingRecord>, RepositoryBindingStoreError> {
    let sql = format!(
        "SELECT {BINDING_COLUMNS} FROM repository_bindings AS rb
         WHERE rb.repository_binding_id = ?1"
    );
    connection
        .query_row(&sql, [repository_binding_id], read_binding_row)
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(binding_from_row)
        .transpose()
}

#[allow(clippy::type_complexity)]
fn read_binding_row(
    row: &rusqlite::Row<'_>,
) -> Result<
    (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        String,
        String,
        Option<String>,
        String,
        i64,
    ),
    rusqlite::Error,
> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
    ))
}

#[allow(clippy::type_complexity)]
fn binding_from_row(
    row: (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        String,
        String,
        Option<String>,
        String,
        i64,
    ),
) -> Result<RepositoryBindingRecord, RepositoryBindingStoreError> {
    let (
        repository_binding_id,
        client_node_id,
        display_name,
        repository_kind,
        default_branch,
        head_commit,
        dirty_state,
        availability,
        repository_fingerprint,
        last_scanned_at,
        created_at,
        revision,
    ) = row;
    if repository_kind != "git" {
        return Err(error(
            RepositoryBindingStoreErrorKind::CorruptState,
            "stored repository kind is invalid",
        ));
    }
    Ok(RepositoryBindingRecord {
        repository_binding_id,
        client_node_id,
        display_name,
        repository_kind,
        default_branch,
        head_commit,
        dirty_state: RepositoryDirtyState::parse(&dirty_state)?,
        availability: RepositoryAvailability::parse(&availability)?,
        repository_fingerprint,
        last_scanned_at: last_scanned_at
            .map(|value| parse_stored_instant(&value, "last scanned"))
            .transpose()?,
        created_at: parse_stored_instant(&created_at, "created at")?,
        revision: from_sql_integer(revision, "repository binding revision")?,
    })
}

fn load_grant(
    connection: &rusqlite::Connection,
    repository_access_grant_id: &str,
) -> Result<Option<RepositoryAccessGrantRecord>, RepositoryBindingStoreError> {
    let sql = format!(
        "SELECT {GRANT_COLUMNS} FROM repository_access_grants AS rag
         WHERE rag.repository_access_grant_id = ?1"
    );
    connection
        .query_row(&sql, [repository_access_grant_id], read_grant_row)
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(grant_from_row)
        .transpose()
}

#[allow(clippy::type_complexity)]
fn read_grant_row(
    row: &rusqlite::Row<'_>,
) -> Result<(String, String, String, String, String, String, String, i64), rusqlite::Error> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

#[allow(clippy::type_complexity)]
fn grant_from_row(
    row: (String, String, String, String, String, String, String, i64),
) -> Result<RepositoryAccessGrantRecord, RepositoryBindingStoreError> {
    let (
        repository_access_grant_id,
        repository_binding_id,
        user_id,
        permissions,
        state,
        granted_by_user_id,
        created_at,
        revision,
    ) = row;
    Ok(RepositoryAccessGrantRecord {
        repository_access_grant_id,
        repository_binding_id,
        user_id,
        permissions: RepositoryGrantPermissions::parse(&permissions)?,
        state: RepositoryGrantState::parse(&state)?,
        granted_by_user_id,
        created_at: parse_stored_instant(&created_at, "grant created at")?,
        revision: from_sql_integer(revision, "repository access grant revision")?,
    })
}

fn validate_schema(connection: &rusqlite::Connection) -> Result<(), RepositoryBindingStoreError> {
    validate_columns(
        connection,
        "repository_bindings",
        &[
            "repository_binding_id",
            "client_node_id",
            "display_name",
            "repository_kind",
            "default_branch",
            "head_commit",
            "dirty_state",
            "availability",
            "repository_fingerprint",
            "last_scanned_at",
            "created_at",
            "revision",
        ],
    )?;
    validate_columns(
        connection,
        "repository_access_grants",
        &[
            "repository_access_grant_id",
            "repository_binding_id",
            "user_id",
            "permissions",
            "state",
            "granted_by_user_id",
            "created_at",
            "revision",
        ],
    )
}

fn validate_columns(
    connection: &rusqlite::Connection,
    table: &str,
    expected: &[&str],
) -> Result<(), RepositoryBindingStoreError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&pragma).map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    if columns != expected {
        return Err(error(
            RepositoryBindingStoreErrorKind::CorruptState,
            "repository binding schema is incompatible",
        ));
    }
    Ok(())
}

fn validate_repository_binding_id(value: &str) -> Result<(), RepositoryBindingStoreError> {
    validate_crockford_id(value, "rbd_", "repository binding id")
}

fn validate_repository_access_grant_id(value: &str) -> Result<(), RepositoryBindingStoreError> {
    validate_crockford_id(value, "rag_", "repository access grant id")
}

fn validate_client_node_id(value: &str) -> Result<(), RepositoryBindingStoreError> {
    validate_crockford_id(value, "cnd_", "client node id")
}

fn validate_user_id(value: &str) -> Result<(), RepositoryBindingStoreError> {
    validate_crockford_id(value, "usr_", "user id")
}

fn validate_crockford_id(
    value: &str,
    prefix: &str,
    label: &str,
) -> Result<(), RepositoryBindingStoreError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(error(
            RepositoryBindingStoreErrorKind::InvalidInput,
            format!("{label} is not canonical"),
        ));
    };
    if suffix.len() != 26 || value.len() > MAX_ID_BYTES || !suffix.bytes().all(is_crockford_base32)
    {
        return Err(error(
            RepositoryBindingStoreErrorKind::InvalidInput,
            format!("{label} is not canonical"),
        ));
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

fn validate_fingerprint(value: &str) -> Result<(), RepositoryBindingStoreError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_FINGERPRINT_BYTES
        && value.bytes().all(|byte| (0x21..=0x7E).contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(error(
            RepositoryBindingStoreErrorKind::InvalidInput,
            "repository fingerprint must contain 1 to 128 printable ASCII bytes",
        ))
    }
}

fn validate_default_branch(value: &str) -> Result<(), RepositoryBindingStoreError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_DEFAULT_BRANCH_BYTES
        && !value.bytes().any(|byte| byte < 0x20 || byte == 0x7F);
    if valid {
        Ok(())
    } else {
        Err(error(
            RepositoryBindingStoreErrorKind::InvalidInput,
            "repository default branch must contain 1 to 256 printable bytes",
        ))
    }
}

fn validate_head_commit(value: &str) -> Result<(), RepositoryBindingStoreError> {
    let valid = matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if valid {
        Ok(())
    } else {
        Err(error(
            RepositoryBindingStoreErrorKind::InvalidInput,
            "repository head commit is not a lowercase 40 or 64 character hex hash",
        ))
    }
}

fn validate_instant(value: &Instant, label: &str) -> Result<(), RepositoryBindingStoreError> {
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
            .all(|(index, byte)| bytes[*index] == *byte)
        && bytes.iter().enumerate().all(|(index, byte)| {
            punctuation.iter().any(|(at, _)| at == &index) || index == 23 || byte.is_ascii_digit()
        });
    if valid {
        Ok(())
    } else {
        Err(error(
            RepositoryBindingStoreErrorKind::InvalidInput,
            format!("{label} instant is not canonical"),
        ))
    }
}

fn parse_stored_instant(value: &str, label: &str) -> Result<Instant, RepositoryBindingStoreError> {
    let instant = Instant(value.to_owned());
    validate_instant(&instant, label).map(|()| instant)
}

fn validate_revision(value: u64) -> Result<(), RepositoryBindingStoreError> {
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            RepositoryBindingStoreErrorKind::InvalidInput,
            "expected revision exceeds the safe integer range",
        ));
    }
    Ok(())
}

fn ensure_binding_revision(
    record: &RepositoryBindingRecord,
    expected_revision: u64,
) -> Result<(), RepositoryBindingStoreError> {
    if record.revision != expected_revision {
        return Err(error(
            RepositoryBindingStoreErrorKind::RevisionConflict,
            "repository binding revision does not match expectedRevision",
        ));
    }
    Ok(())
}

fn ensure_grant_revision(
    record: &RepositoryAccessGrantRecord,
    expected_revision: u64,
) -> Result<(), RepositoryBindingStoreError> {
    if record.revision != expected_revision {
        return Err(error(
            RepositoryBindingStoreErrorKind::RevisionConflict,
            "repository access grant revision does not match expectedRevision",
        ));
    }
    Ok(())
}

fn require_binding(
    connection: &rusqlite::Connection,
    repository_binding_id: &str,
) -> Result<RepositoryBindingRecord, RepositoryBindingStoreError> {
    load_binding(connection, repository_binding_id)?.ok_or_else(|| {
        error(
            RepositoryBindingStoreErrorKind::UnknownRepositoryBinding,
            "repository binding does not exist",
        )
    })
}

fn require_grant(
    connection: &rusqlite::Connection,
    repository_access_grant_id: &str,
) -> Result<RepositoryAccessGrantRecord, RepositoryBindingStoreError> {
    load_grant(connection, repository_access_grant_id)?.ok_or_else(|| {
        error(
            RepositoryBindingStoreErrorKind::UnknownAccessGrant,
            "repository access grant does not exist",
        )
    })
}

fn unknown_client_node() -> RepositoryBindingStoreError {
    error(
        RepositoryBindingStoreErrorKind::UnknownClientNode,
        "client node does not exist",
    )
}

fn sql_integer(value: u64) -> Result<i64, RepositoryBindingStoreError> {
    i64::try_from(value).map_err(|_| {
        error(
            RepositoryBindingStoreErrorKind::InvalidInput,
            "numeric value exceeds the SQLite integer range",
        )
    })
}

fn from_sql_integer(value: i64, label: &str) -> Result<u64, RepositoryBindingStoreError> {
    let value = u64::try_from(value).map_err(|_| {
        error(
            RepositoryBindingStoreErrorKind::CorruptState,
            format!("stored {label} is negative"),
        )
    })?;
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            RepositoryBindingStoreErrorKind::CorruptState,
            format!("stored {label} exceeds the safe integer range"),
        ));
    }
    Ok(value)
}

fn map_binding_insert_sql(sql: &rusqlite::Error) -> RepositoryBindingStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return match failure.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => error(
                RepositoryBindingStoreErrorKind::FingerprintConflict,
                "repository fingerprint is already bound on this client node",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => error(
                RepositoryBindingStoreErrorKind::FingerprintConflict,
                "repository binding id is already used",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => unknown_client_node(),
            _ => error(
                RepositoryBindingStoreErrorKind::InvalidInput,
                "repository binding violates a durable constraint",
            ),
        };
    }
    sql_error(sql)
}

fn map_grant_insert_sql(sql: &rusqlite::Error) -> RepositoryBindingStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return match failure.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => error(
                RepositoryBindingStoreErrorKind::AccessGrantConflict,
                "an active repository access grant already exists for this user and binding",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => error(
                RepositoryBindingStoreErrorKind::AccessGrantConflict,
                "repository access grant id is already used",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => error(
                RepositoryBindingStoreErrorKind::UnknownRepositoryBinding,
                "repository binding does not exist",
            ),
            _ => error(
                RepositoryBindingStoreErrorKind::InvalidInput,
                "repository access grant violates a durable constraint",
            ),
        };
    }
    sql_error(sql)
}

fn storage_error(storage: &StorageError) -> RepositoryBindingStoreError {
    error(
        RepositoryBindingStoreErrorKind::Storage,
        format!("repository binding storage failed: {storage}"),
    )
}

fn sql_error(_sql: &rusqlite::Error) -> RepositoryBindingStoreError {
    error(
        RepositoryBindingStoreErrorKind::Storage,
        "repository binding storage operation failed",
    )
}

fn error(
    kind: RepositoryBindingStoreErrorKind,
    message: impl Into<String>,
) -> RepositoryBindingStoreError {
    RepositoryBindingStoreError {
        kind,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory(name: &str) -> PathBuf {
        let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "winwincode-repository-binding-unit-{name}-{}-{suffix}",
            std::process::id()
        ))
    }

    #[test]
    fn contradictory_dirty_states_are_rejected() {
        assert!(
            RepositoryBindingProjection::try_new(
                "rbd_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "cnd_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "Repo",
                None,
                None,
                RepositoryDirtyState::Dirty,
                RepositoryAvailability::Available,
                "fingerprint",
            )
            .is_err()
        );
        assert!(
            RepositoryScanOutcome::try_new(
                RepositoryAvailability::Available,
                RepositoryDirtyState::Dirty,
            )
            .is_err()
        );
        assert!(
            RepositoryScanOutcome::try_new(
                RepositoryAvailability::Dirty,
                RepositoryDirtyState::Clean,
            )
            .is_err()
        );
        assert!(
            RepositoryScanOutcome::try_new(
                RepositoryAvailability::Unavailable,
                RepositoryDirtyState::Dirty,
            )
            .is_ok()
        );
        assert!(
            RepositoryScanOutcome::try_new(
                RepositoryAvailability::Unavailable,
                RepositoryDirtyState::Clean,
            )
            .is_ok()
        );
    }

    #[test]
    fn the_durable_binding_table_never_carries_a_path_column() {
        let mut storage = SqliteStorage::open(temporary_directory("no-path")).expect("storage");
        let missing_column = storage
            .connection_mut()
            .expect("connection")
            .prepare("SELECT absolute_path, local_path, root_path FROM repository_bindings")
            .expect_err("the binding table must not carry any path column");
        assert!(matches!(
            missing_column,
            rusqlite::Error::SqliteFailure(_, _)
        ));
    }
}
