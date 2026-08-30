// SPDX-License-Identifier: Apache-2.0

//! Durable enterprise Usage ledger for already-settled source facts.
//!
//! This ledger does not infer Usage from logs or reserve capacity. Producers
//! submit one closed, fully attributed settlement fact. The source receipt is
//! the idempotency authority and an immutable sequence supports reconciliation.

use std::fmt;

use rusqlite::{
    OptionalExtension, Transaction, TransactionBehavior, params_from_iter, types::Value,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionJobId, ExecutionMessageId, Instant, ModelExchangeId,
    OrganizationId, ProductSessionId, ProjectId, PublicationId, RepositoryId, RequestId,
    Sha256Digest, UserId, WorkspaceId,
};

use crate::{ArtifactStorageOperationKind, SqliteStorage, StorageError};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_PAGE_SIZE: u64 = 200;
const MAX_FACT_BYTES: usize = 64 * 1024;
const MAX_PORTABLE_TOKEN_BYTES: usize = 512;
const LEDGER_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS enterprise_usage_entries (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT CHECK (sequence > 0),
    source_key TEXT UNIQUE NOT NULL,
    source_digest TEXT NOT NULL,
    fact_json TEXT NOT NULL,
    source_kind TEXT NOT NULL CHECK (
        source_kind IN ('provider', 'worker', 'storage', 'publication')
    ),
    organization_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    delivery_id TEXT,
    product_session_id TEXT,
    user_id TEXT NOT NULL,
    settled_at TEXT NOT NULL,
    provider_total_tokens INTEGER,
    provider_cost_micros INTEGER,
    worker_runtime_millis INTEGER,
    worker_tokens INTEGER,
    worker_cost_microunits INTEGER,
    storage_bytes INTEGER,
    CHECK (provider_total_tokens IS NULL OR provider_total_tokens BETWEEN 0 AND 9007199254740991),
    CHECK (provider_cost_micros IS NULL OR provider_cost_micros BETWEEN 0 AND 9007199254740991),
    CHECK (worker_runtime_millis IS NULL OR worker_runtime_millis BETWEEN 0 AND 9007199254740991),
    CHECK (worker_tokens IS NULL OR worker_tokens BETWEEN 0 AND 9007199254740991),
    CHECK (worker_cost_microunits IS NULL OR worker_cost_microunits BETWEEN 0 AND 9007199254740991),
    CHECK (storage_bytes IS NULL OR storage_bytes BETWEEN 0 AND 9007199254740991),
    CHECK (
        (source_kind = 'provider'
            AND provider_total_tokens IS NOT NULL AND provider_cost_micros IS NOT NULL
            AND worker_runtime_millis IS NULL AND worker_tokens IS NULL
            AND worker_cost_microunits IS NULL AND storage_bytes IS NULL)
        OR (source_kind = 'worker'
            AND provider_total_tokens IS NULL AND provider_cost_micros IS NULL
            AND worker_runtime_millis IS NOT NULL AND worker_tokens IS NOT NULL
            AND worker_cost_microunits IS NOT NULL AND storage_bytes IS NULL)
        OR (source_kind = 'storage'
            AND provider_total_tokens IS NULL AND provider_cost_micros IS NULL
            AND worker_runtime_millis IS NULL AND worker_tokens IS NULL
            AND worker_cost_microunits IS NULL AND storage_bytes IS NOT NULL)
        OR (source_kind = 'publication'
            AND provider_total_tokens IS NULL AND provider_cost_micros IS NULL
            AND worker_runtime_millis IS NULL AND worker_tokens IS NULL
            AND worker_cost_microunits IS NULL AND storage_bytes IS NULL)
    )
);
CREATE INDEX IF NOT EXISTS enterprise_usage_by_scope
    ON enterprise_usage_entries (
        organization_id, workspace_id, project_id, repository_id,
        delivery_id, product_session_id, user_id, sequence
    );
CREATE INDEX IF NOT EXISTS enterprise_usage_by_kind
    ON enterprise_usage_entries (source_kind, sequence);
CREATE TRIGGER IF NOT EXISTS enterprise_usage_entries_no_update
BEFORE UPDATE ON enterprise_usage_entries
BEGIN
    SELECT RAISE(ABORT, 'enterprise Usage ledger is immutable');
END;
CREATE TRIGGER IF NOT EXISTS enterprise_usage_entries_no_delete
BEFORE DELETE ON enterprise_usage_entries
BEGIN
    SELECT RAISE(ABORT, 'enterprise Usage ledger is immutable');
END;
";

/// Every business dimension required for enterprise attribution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseUsageAttribution {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub repository_id: RepositoryId,
    pub delivery_id: Option<DeliveryId>,
    pub product_session_id: Option<ProductSessionId>,
    pub user_id: UserId,
}

/// Closed identity of the durable receipt that settled one source charge.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", deny_unknown_fields)]
pub enum EnterpriseUsageSource {
    Provider {
        provider_usage_id: String,
        source_sequence: u64,
        source_digest: String,
        model_exchange_id: ModelExchangeId,
        request_id: RequestId,
        attempt: u64,
        route_authority_fingerprint: String,
    },
    Worker {
        job_id: ExecutionJobId,
        settlement_request_id: RequestId,
        worker_pool_id: String,
    },
    Storage {
        operation_id: ExecutionMessageId,
        source_sequence: u64,
        source_digest: Sha256Digest,
        artifact_id: ArtifactId,
        operation_kind: ArtifactStorageOperationKind,
        request_id: RequestId,
    },
    Publication {
        publication_id: PublicationId,
        operation_key: String,
        request_sha256: String,
    },
}

impl EnterpriseUsageSource {
    pub(crate) const fn kind(&self) -> EnterpriseUsageSourceKind {
        match self {
            Self::Provider { .. } => EnterpriseUsageSourceKind::Provider,
            Self::Worker { .. } => EnterpriseUsageSourceKind::Worker,
            Self::Storage { .. } => EnterpriseUsageSourceKind::Storage,
            Self::Publication { .. } => EnterpriseUsageSourceKind::Publication,
        }
    }
}

/// Source family used by exact filters and reports.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterpriseUsageSourceKind {
    Provider,
    Worker,
    Storage,
    Publication,
}

impl EnterpriseUsageSourceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Worker => "worker",
            Self::Storage => "storage",
            Self::Publication => "publication",
        }
    }
}

/// Settled quantities owned by one exact source family.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", deny_unknown_fields)]
pub enum EnterpriseUsageMeasure {
    Provider {
        input_tokens: u64,
        cached_input_tokens: u64,
        cache_write_input_tokens: u64,
        output_tokens: u64,
        reasoning_output_tokens: u64,
        total_tokens: u64,
        cost_micros: u64,
    },
    Worker {
        runtime_millis: u64,
        tokens: u64,
        cost_microunits: u64,
    },
    Storage {
        bytes: u64,
    },
    Publication,
}

impl EnterpriseUsageMeasure {
    const fn kind(&self) -> EnterpriseUsageSourceKind {
        match self {
            Self::Provider { .. } => EnterpriseUsageSourceKind::Provider,
            Self::Worker { .. } => EnterpriseUsageSourceKind::Worker,
            Self::Storage { .. } => EnterpriseUsageSourceKind::Storage,
            Self::Publication => EnterpriseUsageSourceKind::Publication,
        }
    }
}

/// One fully attributed fact accepted only after its source settled.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettledEnterpriseUsage {
    pub source: EnterpriseUsageSource,
    pub attribution: EnterpriseUsageAttribution,
    pub measure: EnterpriseUsageMeasure,
    pub settled_at: Instant,
}

/// Immutable ledger entry returned for record and reconciliation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseUsageEntry {
    pub sequence: u64,
    pub source_digest: String,
    pub fact: SettledEnterpriseUsage,
}

/// Result of recording one settled source fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseUsageReceipt {
    pub entry: EnterpriseUsageEntry,
    pub idempotent_replay: bool,
}

/// Exact optional dimensions for a bounded reconciliation scan.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseUsageFilter {
    pub organization_id: Option<OrganizationId>,
    pub workspace_id: Option<WorkspaceId>,
    pub project_id: Option<ProjectId>,
    pub repository_id: Option<RepositoryId>,
    pub delivery_id: Option<DeliveryId>,
    pub product_session_id: Option<ProductSessionId>,
    pub user_id: Option<UserId>,
    pub source_kind: Option<EnterpriseUsageSourceKind>,
}

/// Stable typed cursor bound to one filter and one immutable sequence upper bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseUsageCursor {
    filter_digest: String,
    snapshot_sequence: u64,
    after_sequence: u64,
}

impl EnterpriseUsageCursor {
    #[must_use]
    pub fn filter_digest(&self) -> &str {
        &self.filter_digest
    }

    #[must_use]
    pub const fn snapshot_sequence(&self) -> u64 {
        self.snapshot_sequence
    }

    #[must_use]
    pub const fn after_sequence(&self) -> u64 {
        self.after_sequence
    }
}

/// One deterministic page from a fixed ledger snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseUsagePage {
    pub snapshot_sequence: u64,
    pub entries: Vec<EnterpriseUsageEntry>,
    pub next: Option<EnterpriseUsageCursor>,
}

/// Exact totals across immutable ledger entries.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EnterpriseUsageTotals {
    pub entries: u64,
    pub provider_total_tokens: u64,
    pub provider_cost_micros: u64,
    pub worker_runtime_millis: u64,
    pub worker_tokens: u64,
    pub worker_cost_microunits: u64,
    pub storage_bytes: u64,
    pub storage_operations: u64,
    pub publication_operations: u64,
}

/// Stable failure categories for the enterprise ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterpriseUsageErrorKind {
    InvalidInput,
    SourceConflict,
    CorruptState,
    Adapter,
}

/// Enterprise ledger failure without raw source payloads or secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseUsageError {
    kind: EnterpriseUsageErrorKind,
    message: String,
}

impl EnterpriseUsageError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(EnterpriseUsageErrorKind::InvalidInput, message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(EnterpriseUsageErrorKind::SourceConflict, message)
    }

    fn corrupt(message: impl Into<String>) -> Self {
        Self::new(EnterpriseUsageErrorKind::CorruptState, message)
    }

    fn adapter(message: impl Into<String>) -> Self {
        Self::new(EnterpriseUsageErrorKind::Adapter, message)
    }

    fn new(kind: EnterpriseUsageErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> EnterpriseUsageErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterpriseUsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EnterpriseUsageError {}

/// `SQLite`-backed immutable enterprise Usage ledger.
pub struct EnterpriseUsageLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the enterprise Usage ledger on this storage connection.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the ledger schema cannot be prepared.
    pub fn enterprise_usage_ledger(
        &mut self,
    ) -> Result<EnterpriseUsageLedger<'_>, EnterpriseUsageError> {
        EnterpriseUsageLedger::new(self)
    }
}

impl<'storage> EnterpriseUsageLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, EnterpriseUsageError> {
        storage
            .connection()
            .map_err(|error| storage_error(&error))?
            .execute_batch(LEDGER_SCHEMA)
            .map_err(|error| sql_error(&error))?;
        Ok(Self { storage })
    }

    /// Records one source settlement or returns its exact original entry.
    ///
    /// # Errors
    ///
    /// Rejects malformed facts, changed source receipt reuse, corrupt rows, and
    /// `SQLite` failures.
    pub fn record(
        &mut self,
        fact: &SettledEnterpriseUsage,
    ) -> Result<EnterpriseUsageReceipt, EnterpriseUsageError> {
        validate_fact(fact)?;
        let source_key = source_key(&fact.source)?;
        let source_digest = digest(fact)?;
        let fact_json = encode(fact)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|error| storage_error(&error))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sql_error(&error))?;
        if let Some(entry) = load_entry_by_source_key(&transaction, &source_key)? {
            return replay(transaction, entry, fact, &source_digest);
        }
        insert_fact(&transaction, &source_key, &source_digest, &fact_json, fact)?;
        let sequence = u64::try_from(transaction.last_insert_rowid())
            .map_err(|_| EnterpriseUsageError::corrupt("ledger sequence is invalid"))?;
        let entry = EnterpriseUsageEntry {
            sequence,
            source_digest,
            fact: fact.clone(),
        };
        transaction.commit().map_err(|error| sql_error(&error))?;
        Ok(EnterpriseUsageReceipt {
            entry,
            idempotent_replay: false,
        })
    }

    /// Loads the immutable entry owned by one exact source receipt.
    ///
    /// # Errors
    ///
    /// Rejects malformed source identities, corrupt rows, and `SQLite` failures.
    pub fn load_source(
        &self,
        source: &EnterpriseUsageSource,
    ) -> Result<Option<EnterpriseUsageEntry>, EnterpriseUsageError> {
        validate_source(source)?;
        load_entry_by_source_key(
            self.storage
                .connection()
                .map_err(|error| storage_error(&error))?,
            &source_key(source)?,
        )
    }

    /// Reads one bounded page from a fixed immutable sequence snapshot.
    ///
    /// # Errors
    ///
    /// Rejects invalid filters, limits or cursors, corrupt rows, and `SQLite` failures.
    pub fn scan(
        &self,
        filter: &EnterpriseUsageFilter,
        cursor: Option<&EnterpriseUsageCursor>,
        limit: u64,
    ) -> Result<EnterpriseUsagePage, EnterpriseUsageError> {
        validate_filter(filter)?;
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(EnterpriseUsageError::invalid(
                "enterprise Usage page limit is outside 1..=200",
            ));
        }
        let connection = self
            .storage
            .connection()
            .map_err(|error| storage_error(&error))?;
        let filter_digest = digest(filter)?;
        let (snapshot_sequence, after_sequence) = match cursor {
            Some(cursor) => validate_cursor(cursor, &filter_digest)?,
            None => (last_sequence(connection)?, 0),
        };
        let mut entries = scan_entries(
            connection,
            filter,
            after_sequence,
            snapshot_sequence,
            limit + 1,
        )?;
        let page_size = usize::try_from(limit)
            .map_err(|_| EnterpriseUsageError::invalid("enterprise Usage page limit is invalid"))?;
        let has_more = entries.len() > page_size;
        if has_more {
            entries.pop();
        }
        let next = if has_more {
            let after_sequence = entries.last().map(|entry| entry.sequence).ok_or_else(|| {
                EnterpriseUsageError::corrupt("bounded enterprise Usage page is empty")
            })?;
            Some(EnterpriseUsageCursor {
                filter_digest,
                snapshot_sequence,
                after_sequence,
            })
        } else {
            None
        };
        Ok(EnterpriseUsagePage {
            snapshot_sequence,
            entries,
            next,
        })
    }

    /// Reconciles exact totals from immutable rows matching one filter.
    ///
    /// # Errors
    ///
    /// Rejects invalid filters, corrupt aggregate values, and `SQLite` failures.
    pub fn reconcile(
        &self,
        filter: &EnterpriseUsageFilter,
    ) -> Result<EnterpriseUsageTotals, EnterpriseUsageError> {
        validate_filter(filter)?;
        reconcile_totals(
            self.storage
                .connection()
                .map_err(|error| storage_error(&error))?,
            filter,
        )
    }
}

fn replay(
    transaction: Transaction<'_>,
    entry: EnterpriseUsageEntry,
    fact: &SettledEnterpriseUsage,
    digest: &str,
) -> Result<EnterpriseUsageReceipt, EnterpriseUsageError> {
    if entry.source_digest != digest || &entry.fact != fact {
        return Err(EnterpriseUsageError::conflict(
            "enterprise Usage source receipt already belongs to another fact",
        ));
    }
    transaction.commit().map_err(|error| sql_error(&error))?;
    Ok(EnterpriseUsageReceipt {
        entry,
        idempotent_replay: true,
    })
}

fn insert_fact(
    transaction: &Transaction<'_>,
    source_key: &str,
    source_digest: &str,
    fact_json: &str,
    fact: &SettledEnterpriseUsage,
) -> Result<(), EnterpriseUsageError> {
    let columns = MeasureColumns::from_measure(&fact.measure)?;
    transaction
        .execute(
            "INSERT INTO enterprise_usage_entries
                (source_key, source_digest, fact_json, source_kind,
                 organization_id, workspace_id, project_id, repository_id,
                 delivery_id, product_session_id, user_id, settled_at,
                 provider_total_tokens, provider_cost_micros,
                 worker_runtime_millis, worker_tokens, worker_cost_microunits,
                 storage_bytes)
             VALUES
                (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                 ?13, ?14, ?15, ?16, ?17, ?18)",
            rusqlite::params![
                source_key,
                source_digest,
                fact_json,
                fact.source.kind().as_str(),
                fact.attribution.organization_id.0,
                fact.attribution.workspace_id.0,
                fact.attribution.project_id.0,
                fact.attribution.repository_id.0,
                fact.attribution
                    .delivery_id
                    .as_ref()
                    .map(|id| id.0.as_str()),
                fact.attribution
                    .product_session_id
                    .as_ref()
                    .map(|id| id.0.as_str()),
                fact.attribution.user_id.0,
                fact.settled_at.0,
                columns.provider_total_tokens,
                columns.provider_cost_micros,
                columns.worker_runtime_millis,
                columns.worker_tokens,
                columns.worker_cost_microunits,
                columns.storage_bytes,
            ],
        )
        .map_err(|error| sql_error(&error))?;
    Ok(())
}

#[derive(Clone, Copy)]
struct MeasureColumns {
    provider_total_tokens: Option<i64>,
    provider_cost_micros: Option<i64>,
    worker_runtime_millis: Option<i64>,
    worker_tokens: Option<i64>,
    worker_cost_microunits: Option<i64>,
    storage_bytes: Option<i64>,
}

impl MeasureColumns {
    fn from_measure(measure: &EnterpriseUsageMeasure) -> Result<Self, EnterpriseUsageError> {
        let mut columns = Self {
            provider_total_tokens: None,
            provider_cost_micros: None,
            worker_runtime_millis: None,
            worker_tokens: None,
            worker_cost_microunits: None,
            storage_bytes: None,
        };
        match measure {
            EnterpriseUsageMeasure::Provider {
                total_tokens,
                cost_micros,
                ..
            } => {
                columns.provider_total_tokens = Some(sql_integer(*total_tokens)?);
                columns.provider_cost_micros = Some(sql_integer(*cost_micros)?);
            }
            EnterpriseUsageMeasure::Worker {
                runtime_millis,
                tokens,
                cost_microunits,
            } => {
                columns.worker_runtime_millis = Some(sql_integer(*runtime_millis)?);
                columns.worker_tokens = Some(sql_integer(*tokens)?);
                columns.worker_cost_microunits = Some(sql_integer(*cost_microunits)?);
            }
            EnterpriseUsageMeasure::Storage { bytes } => {
                columns.storage_bytes = Some(sql_integer(*bytes)?);
            }
            EnterpriseUsageMeasure::Publication => {}
        }
        Ok(columns)
    }
}

fn load_entry_by_source_key(
    connection: &rusqlite::Connection,
    source_key: &str,
) -> Result<Option<EnterpriseUsageEntry>, EnterpriseUsageError> {
    connection
        .query_row(
            "SELECT sequence, source_key, source_digest, fact_json, source_kind,
                    organization_id, workspace_id, project_id, repository_id,
                    delivery_id, product_session_id, user_id, settled_at,
                    provider_total_tokens, provider_cost_micros,
                    worker_runtime_millis, worker_tokens, worker_cost_microunits,
                    storage_bytes
             FROM enterprise_usage_entries WHERE source_key = ?1",
            [source_key],
            stored_row,
        )
        .optional()
        .map_err(|error| sql_error(&error))?
        .map(complete_stored_entry)
        .transpose()
}

#[derive(Debug)]
struct StoredEntry {
    sequence: i64,
    source_key: String,
    source_digest: String,
    fact_json: String,
    source_kind: String,
    organization_id: String,
    workspace_id: String,
    project_id: String,
    repository_id: String,
    delivery_id: Option<String>,
    product_session_id: Option<String>,
    user_id: String,
    settled_at: String,
    columns: StoredMeasureColumns,
}

#[derive(Debug)]
struct StoredMeasureColumns {
    provider_total_tokens: Option<i64>,
    provider_cost_micros: Option<i64>,
    worker_runtime_millis: Option<i64>,
    worker_tokens: Option<i64>,
    worker_cost_microunits: Option<i64>,
    storage_bytes: Option<i64>,
}

fn stored_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredEntry> {
    Ok(StoredEntry {
        sequence: row.get(0)?,
        source_key: row.get(1)?,
        source_digest: row.get(2)?,
        fact_json: row.get(3)?,
        source_kind: row.get(4)?,
        organization_id: row.get(5)?,
        workspace_id: row.get(6)?,
        project_id: row.get(7)?,
        repository_id: row.get(8)?,
        delivery_id: row.get(9)?,
        product_session_id: row.get(10)?,
        user_id: row.get(11)?,
        settled_at: row.get(12)?,
        columns: StoredMeasureColumns {
            provider_total_tokens: row.get(13)?,
            provider_cost_micros: row.get(14)?,
            worker_runtime_millis: row.get(15)?,
            worker_tokens: row.get(16)?,
            worker_cost_microunits: row.get(17)?,
            storage_bytes: row.get(18)?,
        },
    })
}

fn complete_stored_entry(
    stored: StoredEntry,
) -> Result<EnterpriseUsageEntry, EnterpriseUsageError> {
    let sequence = u64::try_from(stored.sequence)
        .map_err(|_| EnterpriseUsageError::corrupt("stored ledger sequence is invalid"))?;
    let fact: SettledEnterpriseUsage = serde_json::from_str(&stored.fact_json)
        .map_err(|_| EnterpriseUsageError::corrupt("stored enterprise Usage fact is invalid"))?;
    validate_fact(&fact)
        .map_err(|_| EnterpriseUsageError::corrupt("stored enterprise Usage fact is malformed"))?;
    if source_key(&fact.source)? != stored.source_key
        || digest(&fact)? != stored.source_digest
        || !stored_dimensions_match(&stored, &fact)?
    {
        return Err(EnterpriseUsageError::corrupt(
            "stored enterprise Usage row differs from its canonical fact",
        ));
    }
    Ok(EnterpriseUsageEntry {
        sequence,
        source_digest: stored.source_digest,
        fact,
    })
}

fn stored_dimensions_match(
    stored: &StoredEntry,
    fact: &SettledEnterpriseUsage,
) -> Result<bool, EnterpriseUsageError> {
    let expected = MeasureColumns::from_measure(&fact.measure)?;
    Ok(stored.source_kind == fact.source.kind().as_str()
        && stored.organization_id == fact.attribution.organization_id.0
        && stored.workspace_id == fact.attribution.workspace_id.0
        && stored.project_id == fact.attribution.project_id.0
        && stored.repository_id == fact.attribution.repository_id.0
        && stored.delivery_id.as_deref()
            == fact
                .attribution
                .delivery_id
                .as_ref()
                .map(|id| id.0.as_str())
        && stored.product_session_id.as_deref()
            == fact
                .attribution
                .product_session_id
                .as_ref()
                .map(|id| id.0.as_str())
        && stored.user_id == fact.attribution.user_id.0
        && stored.settled_at == fact.settled_at.0
        && stored.columns.provider_total_tokens == expected.provider_total_tokens
        && stored.columns.provider_cost_micros == expected.provider_cost_micros
        && stored.columns.worker_runtime_millis == expected.worker_runtime_millis
        && stored.columns.worker_tokens == expected.worker_tokens
        && stored.columns.worker_cost_microunits == expected.worker_cost_microunits
        && stored.columns.storage_bytes == expected.storage_bytes)
}

fn scan_entries(
    connection: &rusqlite::Connection,
    filter: &EnterpriseUsageFilter,
    after_sequence: u64,
    snapshot_sequence: u64,
    limit: u64,
) -> Result<Vec<EnterpriseUsageEntry>, EnterpriseUsageError> {
    let (where_sql, mut values) = filter_sql(filter);
    let mut query = format!(
        "SELECT sequence, source_key, source_digest, fact_json, source_kind,
                organization_id, workspace_id, project_id, repository_id,
                delivery_id, product_session_id, user_id, settled_at,
                provider_total_tokens, provider_cost_micros,
                worker_runtime_millis, worker_tokens, worker_cost_microunits,
                storage_bytes
         FROM enterprise_usage_entries
         WHERE sequence > ? AND sequence <= ?{where_sql}
         ORDER BY sequence LIMIT ?"
    );
    values.insert(0, Value::from(sql_integer(snapshot_sequence)?));
    values.insert(0, Value::from(sql_integer(after_sequence)?));
    values.push(Value::from(sql_integer(limit)?));
    let mut statement = connection
        .prepare(&query)
        .map_err(|error| sql_error(&error))?;
    query.clear();
    let rows = statement
        .query_map(params_from_iter(values), stored_row)
        .map_err(|error| sql_error(&error))?;
    rows.map(|row| {
        row.map_err(|error| sql_error(&error))
            .and_then(complete_stored_entry)
    })
    .collect()
}

pub(crate) fn reconcile_totals(
    connection: &rusqlite::Connection,
    filter: &EnterpriseUsageFilter,
) -> Result<EnterpriseUsageTotals, EnterpriseUsageError> {
    let (where_sql, values) = filter_sql(filter);
    let query = format!(
        "SELECT COUNT(*),
                COALESCE(SUM(provider_total_tokens), 0),
                COALESCE(SUM(provider_cost_micros), 0),
                COALESCE(SUM(worker_runtime_millis), 0),
                COALESCE(SUM(worker_tokens), 0),
                COALESCE(SUM(worker_cost_microunits), 0),
                COALESCE(SUM(storage_bytes), 0),
                COALESCE(SUM(CASE WHEN source_kind = 'storage' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN source_kind = 'publication' THEN 1 ELSE 0 END), 0)
         FROM enterprise_usage_entries WHERE 1 = 1{where_sql}"
    );
    connection
        .query_row(&query, params_from_iter(values), |row| {
            Ok(EnterpriseUsageTotals {
                entries: unsigned_column(row, 0)?,
                provider_total_tokens: unsigned_column(row, 1)?,
                provider_cost_micros: unsigned_column(row, 2)?,
                worker_runtime_millis: unsigned_column(row, 3)?,
                worker_tokens: unsigned_column(row, 4)?,
                worker_cost_microunits: unsigned_column(row, 5)?,
                storage_bytes: unsigned_column(row, 6)?,
                storage_operations: unsigned_column(row, 7)?,
                publication_operations: unsigned_column(row, 8)?,
            })
        })
        .map_err(|error| sql_error(&error))
}

fn unsigned_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, i64>(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

pub(crate) fn validate_quota_attribution(
    attribution: &EnterpriseUsageAttribution,
) -> Result<(), EnterpriseUsageError> {
    validate_attribution(attribution)
}

pub(crate) fn load_quota_usage_source(
    connection: &rusqlite::Connection,
    source: &EnterpriseUsageSource,
) -> Result<Option<EnterpriseUsageEntry>, EnterpriseUsageError> {
    validate_source(source)?;
    load_entry_by_source_key(connection, &source_key(source)?)
}

fn filter_sql(filter: &EnterpriseUsageFilter) -> (String, Vec<Value>) {
    let mut sql = String::new();
    let mut values = Vec::new();
    for (column, value) in [
        (
            "organization_id",
            filter.organization_id.as_ref().map(|id| id.0.as_str()),
        ),
        (
            "workspace_id",
            filter.workspace_id.as_ref().map(|id| id.0.as_str()),
        ),
        (
            "project_id",
            filter.project_id.as_ref().map(|id| id.0.as_str()),
        ),
        (
            "repository_id",
            filter.repository_id.as_ref().map(|id| id.0.as_str()),
        ),
        (
            "delivery_id",
            filter.delivery_id.as_ref().map(|id| id.0.as_str()),
        ),
        (
            "product_session_id",
            filter.product_session_id.as_ref().map(|id| id.0.as_str()),
        ),
        ("user_id", filter.user_id.as_ref().map(|id| id.0.as_str())),
    ] {
        if let Some(value) = value {
            sql.push_str(" AND ");
            sql.push_str(column);
            sql.push_str(" = ?");
            values.push(Value::from(value.to_owned()));
        }
    }
    if let Some(kind) = filter.source_kind {
        sql.push_str(" AND source_kind = ?");
        values.push(Value::from(kind.as_str().to_owned()));
    }
    (sql, values)
}

fn last_sequence(connection: &rusqlite::Connection) -> Result<u64, EnterpriseUsageError> {
    let value = connection
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM enterprise_usage_entries",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| sql_error(&error))?;
    u64::try_from(value)
        .map_err(|_| EnterpriseUsageError::corrupt("stored ledger sequence is negative"))
}

fn validate_cursor(
    cursor: &EnterpriseUsageCursor,
    filter_digest: &str,
) -> Result<(u64, u64), EnterpriseUsageError> {
    if cursor.filter_digest != filter_digest
        || cursor.after_sequence > cursor.snapshot_sequence
        || cursor.snapshot_sequence > MAX_SAFE_INTEGER
    {
        return Err(EnterpriseUsageError::invalid(
            "enterprise Usage cursor does not match the requested snapshot",
        ));
    }
    Ok((cursor.snapshot_sequence, cursor.after_sequence))
}

fn validate_fact(fact: &SettledEnterpriseUsage) -> Result<(), EnterpriseUsageError> {
    validate_source(&fact.source)?;
    validate_attribution(&fact.attribution)?;
    validate_measure(&fact.measure)?;
    validate_instant(&fact.settled_at)?;
    if fact.source.kind() != fact.measure.kind() {
        return Err(EnterpriseUsageError::invalid(
            "enterprise Usage source and measure kinds differ",
        ));
    }
    let bytes = serde_json::to_vec(fact)
        .map_err(|_| EnterpriseUsageError::invalid("enterprise Usage fact is not serializable"))?;
    if bytes.len() > MAX_FACT_BYTES {
        return Err(EnterpriseUsageError::invalid(
            "enterprise Usage fact exceeds 64 KiB",
        ));
    }
    Ok(())
}

fn validate_attribution(
    attribution: &EnterpriseUsageAttribution,
) -> Result<(), EnterpriseUsageError> {
    canonical_id(&attribution.organization_id.0, "org_", "organizationId")?;
    canonical_id(&attribution.workspace_id.0, "wsp_", "workspaceId")?;
    canonical_id(&attribution.project_id.0, "prj_", "projectId")?;
    canonical_id(&attribution.repository_id.0, "rep_", "repositoryId")?;
    if let Some(delivery_id) = &attribution.delivery_id {
        canonical_id(&delivery_id.0, "dlv_", "deliveryId")?;
    }
    if let Some(product_session_id) = &attribution.product_session_id {
        canonical_id(&product_session_id.0, "psn_", "productSessionId")?;
    }
    canonical_id(&attribution.user_id.0, "usr_", "userId")
}

fn validate_source(source: &EnterpriseUsageSource) -> Result<(), EnterpriseUsageError> {
    match source {
        EnterpriseUsageSource::Provider {
            provider_usage_id,
            source_sequence,
            source_digest,
            model_exchange_id,
            request_id,
            attempt,
            route_authority_fingerprint,
        } => {
            portable_token(provider_usage_id, "providerUsageId")?;
            positive_safe(*source_sequence, "sourceSequence")?;
            sha256(source_digest, "sourceDigest")?;
            canonical_id(&model_exchange_id.0, "mdl_", "modelExchangeId")?;
            canonical_id(&request_id.0, "req_", "requestId")?;
            positive_safe(*attempt, "attempt")?;
            sha256(route_authority_fingerprint, "routeAuthorityFingerprint")
        }
        EnterpriseUsageSource::Worker {
            job_id,
            settlement_request_id,
            worker_pool_id,
        } => {
            canonical_id(&job_id.0, "job_", "jobId")?;
            canonical_id(&settlement_request_id.0, "req_", "settlementRequestId")?;
            canonical_id(worker_pool_id, "wpl_", "workerPoolId")
        }
        EnterpriseUsageSource::Storage {
            operation_id,
            source_sequence,
            source_digest,
            artifact_id,
            request_id,
            ..
        } => {
            canonical_id(&operation_id.0, "xmsg_", "operationId")?;
            positive_safe(*source_sequence, "sourceSequence")?;
            sha256(&source_digest.0, "sourceDigest")?;
            canonical_id(&artifact_id.0, "art_", "artifactId")?;
            canonical_id(&request_id.0, "req_", "requestId")
        }
        EnterpriseUsageSource::Publication {
            publication_id,
            operation_key,
            request_sha256,
        } => {
            canonical_id(&publication_id.0, "pub_", "publicationId")?;
            portable_token(operation_key, "operationKey")?;
            sha256(request_sha256, "requestSha256")
        }
    }
}

fn validate_measure(measure: &EnterpriseUsageMeasure) -> Result<(), EnterpriseUsageError> {
    let values: &[u64] = match measure {
        EnterpriseUsageMeasure::Provider {
            input_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            reasoning_output_tokens,
            total_tokens,
            cost_micros,
        } => {
            let expected = input_tokens
                .checked_add(*output_tokens)
                .ok_or_else(|| EnterpriseUsageError::invalid("Provider token total overflows"))?;
            if *total_tokens != expected {
                return Err(EnterpriseUsageError::invalid(
                    "Provider totalTokens differs from inputTokens plus outputTokens",
                ));
            }
            if cached_input_tokens > input_tokens {
                return Err(EnterpriseUsageError::invalid(
                    "Provider cachedInputTokens exceeds inputTokens",
                ));
            }
            if cache_write_input_tokens > input_tokens {
                return Err(EnterpriseUsageError::invalid(
                    "Provider cacheWriteInputTokens exceeds inputTokens",
                ));
            }
            if reasoning_output_tokens > output_tokens {
                return Err(EnterpriseUsageError::invalid(
                    "Provider reasoningOutputTokens exceeds outputTokens",
                ));
            }
            &[
                *input_tokens,
                *cached_input_tokens,
                *cache_write_input_tokens,
                *output_tokens,
                *reasoning_output_tokens,
                *total_tokens,
                *cost_micros,
            ]
        }
        EnterpriseUsageMeasure::Worker {
            runtime_millis,
            tokens,
            cost_microunits,
        } => &[*runtime_millis, *tokens, *cost_microunits],
        EnterpriseUsageMeasure::Storage { bytes } => &[*bytes],
        EnterpriseUsageMeasure::Publication => &[],
    };
    if values.iter().any(|value| *value > MAX_SAFE_INTEGER) {
        return Err(EnterpriseUsageError::invalid(
            "enterprise Usage quantity exceeds the safe integer range",
        ));
    }
    Ok(())
}

fn validate_filter(filter: &EnterpriseUsageFilter) -> Result<(), EnterpriseUsageError> {
    if let Some(id) = &filter.organization_id {
        canonical_id(&id.0, "org_", "organizationId")?;
    }
    if let Some(id) = &filter.workspace_id {
        canonical_id(&id.0, "wsp_", "workspaceId")?;
    }
    if let Some(id) = &filter.project_id {
        canonical_id(&id.0, "prj_", "projectId")?;
    }
    if let Some(id) = &filter.repository_id {
        canonical_id(&id.0, "rep_", "repositoryId")?;
    }
    if let Some(id) = &filter.delivery_id {
        canonical_id(&id.0, "dlv_", "deliveryId")?;
    }
    if let Some(id) = &filter.product_session_id {
        canonical_id(&id.0, "psn_", "productSessionId")?;
    }
    if let Some(id) = &filter.user_id {
        canonical_id(&id.0, "usr_", "userId")?;
    }
    Ok(())
}

fn source_key(source: &EnterpriseUsageSource) -> Result<String, EnterpriseUsageError> {
    let identity = match source {
        EnterpriseUsageSource::Provider {
            provider_usage_id, ..
        } => serde_json::to_vec(&("provider", provider_usage_id)),
        EnterpriseUsageSource::Worker {
            job_id,
            settlement_request_id,
            ..
        } => serde_json::to_vec(&("worker", job_id, settlement_request_id)),
        EnterpriseUsageSource::Storage { operation_id, .. } => {
            serde_json::to_vec(&("storage", operation_id))
        }
        EnterpriseUsageSource::Publication {
            publication_id,
            operation_key,
            ..
        } => serde_json::to_vec(&("publication", publication_id, operation_key)),
    }
    .map_err(|_| EnterpriseUsageError::invalid("source receipt is not serializable"))?;
    Ok(format!(
        "enterprise-usage:{}:{:x}",
        source.kind().as_str(),
        Sha256::digest(identity)
    ))
}

fn digest(value: &impl Serialize) -> Result<String, EnterpriseUsageError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| EnterpriseUsageError::invalid("enterprise Usage value is not serializable"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn encode(value: &impl Serialize) -> Result<String, EnterpriseUsageError> {
    serde_json::to_string(value)
        .map_err(|_| EnterpriseUsageError::invalid("enterprise Usage fact is not serializable"))
}

fn positive_safe(value: u64, field: &str) -> Result<(), EnterpriseUsageError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        return Err(EnterpriseUsageError::invalid(format!(
            "{field} is outside the safe positive range"
        )));
    }
    Ok(())
}

fn canonical_id(value: &str, prefix: &str, field: &str) -> Result<(), EnterpriseUsageError> {
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
        Err(EnterpriseUsageError::invalid(format!(
            "{field} is not canonical"
        )))
    }
}

fn portable_token(value: &str, field: &str) -> Result<(), EnterpriseUsageError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_PORTABLE_TOKEN_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'@' | b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(EnterpriseUsageError::invalid(format!(
            "{field} is not portable"
        )))
    }
}

fn sha256(value: &str, field: &str) -> Result<(), EnterpriseUsageError> {
    let valid = value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    });
    if valid {
        Ok(())
    } else {
        Err(EnterpriseUsageError::invalid(format!(
            "{field} is not a canonical SHA-256 digest"
        )))
    }
}

fn validate_instant(value: &Instant) -> Result<(), EnterpriseUsageError> {
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
        Err(EnterpriseUsageError::invalid(
            "settledAt is not a canonical millisecond UTC instant",
        ))
    }
}

fn sql_integer(value: u64) -> Result<i64, EnterpriseUsageError> {
    i64::try_from(value)
        .map_err(|_| EnterpriseUsageError::invalid("value exceeds the SQLite integer range"))
}

fn storage_error(error: &StorageError) -> EnterpriseUsageError {
    EnterpriseUsageError::adapter(error.to_string())
}

fn sql_error(error: &rusqlite::Error) -> EnterpriseUsageError {
    EnterpriseUsageError::adapter(format!("SQLite enterprise Usage operation failed: {error}"))
}
