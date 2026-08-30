// SPDX-License-Identifier: Apache-2.0

//! Deterministic enterprise cost allocation, capacity projection, and export.
//!
//! Reports consume only immutable entries from the settled enterprise Usage
//! ledger. Every multi-page read remains on the ledger's fixed sequence
//! snapshot; no report reads producer tables or reconstructs facts from logs.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    DeliveryId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId, UserId,
    WorkspaceId,
};
use winwincode_storage::{
    EnterpriseUsageAttribution, EnterpriseUsageCursor, EnterpriseUsageEntry, EnterpriseUsageError,
    EnterpriseUsageErrorKind, EnterpriseUsageFilter, EnterpriseUsageMeasure, EnterpriseUsageSource,
    EnterpriseUsageSourceKind, SqliteStorage,
};

const MAX_LEDGER_PAGE_SIZE: u64 = 200;
const MAX_REPORT_ENTRIES: u64 = 1_000_000;
const MAX_REPORT_GROUPS: u64 = 100_000;
const MAX_EXPORT_BYTES: u64 = 64 * 1024 * 1024;

/// The only monetary rule supported by settled facts that do not carry an ISO
/// currency. Provider and Worker micro-values remain separate and no exchange
/// rate or cross-source total is inferred.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterpriseReportCurrencyRule {
    SourceNativeSeparatedNoConversion,
}

/// UTC inclusion rule used by every report and export.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterpriseReportTimeRule {
    SettledAtUtcFromInclusiveToExclusive,
}

/// One deterministic grouping dimension for allocation or trend projection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnterpriseReportDimension {
    Organization,
    Workspace,
    Project,
    Repository,
    Delivery,
    ProductSession,
    User,
    SourceKind,
    UtcDay,
}

/// Strongly typed group identity. Optional business dimensions retain an
/// explicit unbound bucket rather than disappearing from reconciliation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "value")]
pub enum EnterpriseReportGroup {
    Organization(OrganizationId),
    Workspace(WorkspaceId),
    Project(ProjectId),
    Repository(RepositoryId),
    Delivery(Option<DeliveryId>),
    ProductSession(Option<ProductSessionId>),
    User(UserId),
    SourceKind(EnterpriseUsageSourceKind),
    UtcDay(String),
}

/// Exact cost and capacity totals. Monetary values stay in their settled
/// source-native micro units and are deliberately not combined.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseReportTotals {
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

impl EnterpriseReportTotals {
    fn add_entry(&mut self, entry: &EnterpriseUsageEntry) -> Result<(), EnterpriseReportError> {
        self.entries = checked_add(self.entries, 1)?;
        match &entry.fact.measure {
            EnterpriseUsageMeasure::Provider {
                total_tokens,
                cost_micros,
                ..
            } => {
                self.provider_total_tokens =
                    checked_add(self.provider_total_tokens, *total_tokens)?;
                self.provider_cost_micros = checked_add(self.provider_cost_micros, *cost_micros)?;
            }
            EnterpriseUsageMeasure::Worker {
                runtime_millis,
                tokens,
                cost_microunits,
            } => {
                self.worker_runtime_millis =
                    checked_add(self.worker_runtime_millis, *runtime_millis)?;
                self.worker_tokens = checked_add(self.worker_tokens, *tokens)?;
                self.worker_cost_microunits =
                    checked_add(self.worker_cost_microunits, *cost_microunits)?;
            }
            EnterpriseUsageMeasure::Storage { bytes } => {
                self.storage_bytes = checked_add(self.storage_bytes, *bytes)?;
                self.storage_operations = checked_add(self.storage_operations, 1)?;
            }
            EnterpriseUsageMeasure::Publication => {
                self.publication_operations = checked_add(self.publication_operations, 1)?;
            }
        }
        Ok(())
    }

    /// Adds another exact total set while rejecting integer overflow.
    ///
    /// # Errors
    ///
    /// Returns [`EnterpriseReportErrorKind::Overflow`] when any total exceeds
    /// the supported integer range.
    pub fn checked_merge(&mut self, other: Self) -> Result<(), EnterpriseReportError> {
        self.entries = checked_add(self.entries, other.entries)?;
        self.provider_total_tokens =
            checked_add(self.provider_total_tokens, other.provider_total_tokens)?;
        self.provider_cost_micros =
            checked_add(self.provider_cost_micros, other.provider_cost_micros)?;
        self.worker_runtime_millis =
            checked_add(self.worker_runtime_millis, other.worker_runtime_millis)?;
        self.worker_tokens = checked_add(self.worker_tokens, other.worker_tokens)?;
        self.worker_cost_microunits =
            checked_add(self.worker_cost_microunits, other.worker_cost_microunits)?;
        self.storage_bytes = checked_add(self.storage_bytes, other.storage_bytes)?;
        self.storage_operations = checked_add(self.storage_operations, other.storage_operations)?;
        self.publication_operations =
            checked_add(self.publication_operations, other.publication_operations)?;
        Ok(())
    }
}

/// One allocated row in a deterministic projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseReportRow {
    pub group: EnterpriseReportGroup,
    pub totals: EnterpriseReportTotals,
}

/// Exact ledger and UTC window used by projection and detail reads.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseReportQuery {
    pub filter: EnterpriseUsageFilter,
    pub from_inclusive: Instant,
    pub to_exclusive: Instant,
    pub group_by: EnterpriseReportDimension,
}

/// Hard bounds applied before report construction or serialization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnterpriseReportingLimits {
    ledger_page_size: u64,
    max_scanned_entries: u64,
    max_groups: u64,
    max_export_bytes: u64,
}

impl EnterpriseReportingLimits {
    /// Creates one explicit set of report resource bounds.
    ///
    /// # Errors
    ///
    /// Rejects zero, unsafe, or unsupported limits.
    pub fn try_new(
        ledger_page_size: u64,
        max_scanned_entries: u64,
        max_groups: u64,
        max_export_bytes: u64,
    ) -> Result<Self, EnterpriseReportError> {
        if !(1..=MAX_LEDGER_PAGE_SIZE).contains(&ledger_page_size)
            || !(1..=MAX_REPORT_ENTRIES).contains(&max_scanned_entries)
            || !(1..=MAX_REPORT_GROUPS).contains(&max_groups)
            || !(1..=MAX_EXPORT_BYTES).contains(&max_export_bytes)
        {
            return Err(EnterpriseReportError::invalid(
                "enterprise reporting limits are outside canonical bounds",
            ));
        }
        Ok(Self {
            ledger_page_size,
            max_scanned_entries,
            max_groups,
            max_export_bytes,
        })
    }
}

/// Deterministic aggregate projection from one immutable ledger snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseReportingProjection {
    pub snapshot_sequence: u64,
    pub query: EnterpriseReportQuery,
    pub time_rule: EnterpriseReportTimeRule,
    pub currency_rule: EnterpriseReportCurrencyRule,
    pub scanned_entries: u64,
    pub matched_entries: u64,
    pub totals: EnterpriseReportTotals,
    pub rows: Vec<EnterpriseReportRow>,
}

/// Opaque typed cursor for the next ledger page of the exact same report.
#[derive(Debug, Eq, PartialEq)]
pub struct EnterpriseReportCursor {
    query_digest: String,
    ledger_cursor: EnterpriseUsageCursor,
}

impl EnterpriseReportCursor {
    /// Returns the digest binding the cursor to its exact query.
    #[must_use]
    pub fn query_digest(&self) -> &str {
        &self.query_digest
    }

    /// Returns the immutable ledger upper bound used by the cursor.
    #[must_use]
    pub const fn snapshot_sequence(&self) -> u64 {
        self.ledger_cursor.snapshot_sequence()
    }
}

/// One bounded detail page. A page can have fewer matches than scanned rows
/// because the UTC interval is applied to ledger facts after the scope scan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseReportDetail {
    pub sequence: u64,
    pub source_digest: String,
    pub source: EnterpriseUsageSource,
    pub attribution: EnterpriseUsageAttribution,
    pub measure: EnterpriseUsageMeasure,
    pub settled_at: Instant,
}

/// One deterministic detail page from a fixed ledger snapshot.
#[derive(Debug, Eq, PartialEq)]
pub struct EnterpriseReportPage {
    pub snapshot_sequence: u64,
    pub query: EnterpriseReportQuery,
    pub time_rule: EnterpriseReportTimeRule,
    pub currency_rule: EnterpriseReportCurrencyRule,
    pub scanned_entries: u64,
    pub totals: EnterpriseReportTotals,
    pub entries: Vec<EnterpriseReportDetail>,
    pub next: Option<EnterpriseReportCursor>,
}

/// Supported stable export encodings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterpriseReportFormat {
    Json,
    Csv,
}

/// Stable export bytes and their exact SHA-256 digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseReportExport {
    pub media_type: &'static str,
    pub file_extension: &'static str,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

/// Stable error categories for report construction and export.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterpriseReportErrorKind {
    InvalidInput,
    Ledger,
    LimitExceeded,
    Overflow,
    Serialization,
}

/// Secret-free reporting error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnterpriseReportError {
    kind: EnterpriseReportErrorKind,
    message: String,
}

impl EnterpriseReportError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(EnterpriseReportErrorKind::InvalidInput, message)
    }

    fn limit(message: impl Into<String>) -> Self {
        Self::new(EnterpriseReportErrorKind::LimitExceeded, message)
    }

    fn overflow() -> Self {
        Self::new(
            EnterpriseReportErrorKind::Overflow,
            "enterprise report totals exceed the supported integer range",
        )
    }

    fn serialization() -> Self {
        Self::new(
            EnterpriseReportErrorKind::Serialization,
            "enterprise report export serialization failed",
        )
    }

    fn new(kind: EnterpriseReportErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Returns the stable error category.
    #[must_use]
    pub const fn kind(&self) -> EnterpriseReportErrorKind {
        self.kind
    }
}

impl fmt::Display for EnterpriseReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EnterpriseReportError {}

impl From<EnterpriseUsageError> for EnterpriseReportError {
    fn from(error: EnterpriseUsageError) -> Self {
        match error.kind() {
            EnterpriseUsageErrorKind::InvalidInput => {
                Self::invalid("enterprise report scope filter, page limit, or cursor is invalid")
            }
            EnterpriseUsageErrorKind::SourceConflict
            | EnterpriseUsageErrorKind::CorruptState
            | EnterpriseUsageErrorKind::Adapter => Self::new(
                EnterpriseReportErrorKind::Ledger,
                "enterprise Usage ledger read failed",
            ),
        }
    }
}

/// Stateless reporting coordinator over the immutable enterprise Usage ledger.
pub struct EnterpriseReportingService<'storage> {
    storage: &'storage mut SqliteStorage,
    limits: EnterpriseReportingLimits,
}

impl<'storage> EnterpriseReportingService<'storage> {
    #[must_use]
    pub const fn new(
        storage: &'storage mut SqliteStorage,
        limits: EnterpriseReportingLimits,
    ) -> Self {
        Self { storage, limits }
    }

    /// Rebuilds a complete, bounded allocation/capacity projection from one
    /// immutable ledger snapshot.
    ///
    /// # Errors
    ///
    /// Rejects invalid intervals, corrupt ledger facts, arithmetic overflow,
    /// and scans or group cardinality beyond the configured bounds.
    pub fn project(
        &mut self,
        query: &EnterpriseReportQuery,
    ) -> Result<EnterpriseReportingProjection, EnterpriseReportError> {
        validate_query(query)?;
        let ledger = self.storage.enterprise_usage_ledger()?;
        let mut cursor = None;
        let mut snapshot_sequence = None;
        let mut scanned_entries = 0_u64;
        let mut totals = EnterpriseReportTotals::default();
        let mut groups = BTreeMap::<String, EnterpriseReportRow>::new();
        loop {
            let page = ledger.scan(&query.filter, cursor.as_ref(), self.limits.ledger_page_size)?;
            if let Some(expected) = snapshot_sequence {
                if expected != page.snapshot_sequence {
                    return Err(EnterpriseReportError::new(
                        EnterpriseReportErrorKind::Ledger,
                        "enterprise Usage ledger changed a fixed report snapshot",
                    ));
                }
            } else {
                snapshot_sequence = Some(page.snapshot_sequence);
            }
            scanned_entries = checked_add(scanned_entries, usize_u64(page.entries.len())?)?;
            if scanned_entries > self.limits.max_scanned_entries {
                return Err(EnterpriseReportError::limit(
                    "enterprise report scan exceeds its configured entry bound",
                ));
            }
            for entry in &page.entries {
                validate_ledger_instant(&entry.fact.settled_at)?;
                if !matches_interval(&entry.fact.settled_at, query) {
                    continue;
                }
                totals.add_entry(entry)?;
                let group = report_group(query.group_by, entry);
                let key = group_sort_key(&group);
                if !groups.contains_key(&key) && usize_u64(groups.len())? >= self.limits.max_groups
                {
                    return Err(EnterpriseReportError::limit(
                        "enterprise report exceeds its configured group bound",
                    ));
                }
                groups
                    .entry(key)
                    .or_insert_with(|| EnterpriseReportRow {
                        group,
                        totals: EnterpriseReportTotals::default(),
                    })
                    .totals
                    .add_entry(entry)?;
            }
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        let rows = groups.into_values().collect::<Vec<_>>();
        verify_rows(&rows, totals)?;
        Ok(EnterpriseReportingProjection {
            snapshot_sequence: snapshot_sequence.unwrap_or(0),
            query: query.clone(),
            time_rule: EnterpriseReportTimeRule::SettledAtUtcFromInclusiveToExclusive,
            currency_rule: EnterpriseReportCurrencyRule::SourceNativeSeparatedNoConversion,
            scanned_entries,
            matched_entries: totals.entries,
            totals,
            rows,
        })
    }

    /// Reads one bounded detail page from the same fixed ledger snapshot.
    ///
    /// # Errors
    ///
    /// Rejects changed queries, invalid page sizes, corrupt facts, or ledger
    /// failures. The page limit bounds scanned ledger rows, not UTC matches.
    pub fn page(
        &mut self,
        query: &EnterpriseReportQuery,
        cursor: Option<&EnterpriseReportCursor>,
        limit: u64,
    ) -> Result<EnterpriseReportPage, EnterpriseReportError> {
        validate_query(query)?;
        if limit == 0 || limit > self.limits.ledger_page_size {
            return Err(EnterpriseReportError::invalid(
                "enterprise report page limit exceeds the configured ledger page size",
            ));
        }
        let query_digest = canonical_digest(query)?;
        let ledger_cursor = match cursor {
            Some(cursor) if cursor.query_digest == query_digest => Some(&cursor.ledger_cursor),
            Some(_) => {
                return Err(EnterpriseReportError::invalid(
                    "enterprise report cursor does not match the query",
                ));
            }
            None => None,
        };
        let ledger = self.storage.enterprise_usage_ledger()?;
        let page = ledger.scan(&query.filter, ledger_cursor, limit)?;
        let scanned_entries = usize_u64(page.entries.len())?;
        let mut totals = EnterpriseReportTotals::default();
        let mut entries = Vec::new();
        for entry in page.entries {
            validate_ledger_instant(&entry.fact.settled_at)?;
            if !matches_interval(&entry.fact.settled_at, query) {
                continue;
            }
            totals.add_entry(&entry)?;
            entries.push(detail(entry));
        }
        let next = page.next.map(|ledger_cursor| EnterpriseReportCursor {
            query_digest,
            ledger_cursor,
        });
        Ok(EnterpriseReportPage {
            snapshot_sequence: page.snapshot_sequence,
            query: query.clone(),
            time_rule: EnterpriseReportTimeRule::SettledAtUtcFromInclusiveToExclusive,
            currency_rule: EnterpriseReportCurrencyRule::SourceNativeSeparatedNoConversion,
            scanned_entries,
            totals,
            entries,
            next,
        })
    }

    /// Serializes one aggregate projection to canonical JSON or RFC 4180 CSV.
    ///
    /// # Errors
    ///
    /// Rejects exports beyond the configured byte bound or serialization
    /// failures.
    pub fn export_projection(
        &self,
        projection: &EnterpriseReportingProjection,
        format: EnterpriseReportFormat,
    ) -> Result<EnterpriseReportExport, EnterpriseReportError> {
        let bytes = match format {
            EnterpriseReportFormat::Json => bounded_json(projection, self.limits.max_export_bytes)?,
            EnterpriseReportFormat::Csv => {
                projection_csv(projection, self.limits.max_export_bytes)?
            }
        };
        self.finish_export(format, bytes)
    }

    /// Serializes one fixed-snapshot detail page to canonical JSON or RFC 4180
    /// CSV without reading any producer or log source.
    ///
    /// # Errors
    ///
    /// Rejects exports beyond the configured byte bound or serialization
    /// failures.
    pub fn export_page(
        &self,
        page: &EnterpriseReportPage,
        format: EnterpriseReportFormat,
    ) -> Result<EnterpriseReportExport, EnterpriseReportError> {
        let bytes = match format {
            EnterpriseReportFormat::Json => detail_page_json(page, self.limits.max_export_bytes)?,
            EnterpriseReportFormat::Csv => detail_page_csv(page, self.limits.max_export_bytes)?,
        };
        self.finish_export(format, bytes)
    }

    fn finish_export(
        &self,
        format: EnterpriseReportFormat,
        bytes: Vec<u8>,
    ) -> Result<EnterpriseReportExport, EnterpriseReportError> {
        if usize_u64(bytes.len())? > self.limits.max_export_bytes {
            return Err(EnterpriseReportError::limit(
                "enterprise report export exceeds its configured byte bound",
            ));
        }
        let (media_type, file_extension) = match format {
            EnterpriseReportFormat::Json => ("application/json", "json"),
            EnterpriseReportFormat::Csv => ("text/csv; charset=utf-8", "csv"),
        };
        let sha256 = format!("sha256:{:x}", Sha256::digest(&bytes));
        Ok(EnterpriseReportExport {
            media_type,
            file_extension,
            bytes,
            sha256,
        })
    }
}

fn detail(entry: EnterpriseUsageEntry) -> EnterpriseReportDetail {
    EnterpriseReportDetail {
        sequence: entry.sequence,
        source_digest: entry.source_digest,
        source: entry.fact.source,
        attribution: entry.fact.attribution,
        measure: entry.fact.measure,
        settled_at: entry.fact.settled_at,
    }
}

fn report_group(
    dimension: EnterpriseReportDimension,
    entry: &EnterpriseUsageEntry,
) -> EnterpriseReportGroup {
    let attribution = &entry.fact.attribution;
    match dimension {
        EnterpriseReportDimension::Organization => {
            EnterpriseReportGroup::Organization(attribution.organization_id.clone())
        }
        EnterpriseReportDimension::Workspace => {
            EnterpriseReportGroup::Workspace(attribution.workspace_id.clone())
        }
        EnterpriseReportDimension::Project => {
            EnterpriseReportGroup::Project(attribution.project_id.clone())
        }
        EnterpriseReportDimension::Repository => {
            EnterpriseReportGroup::Repository(attribution.repository_id.clone())
        }
        EnterpriseReportDimension::Delivery => {
            EnterpriseReportGroup::Delivery(attribution.delivery_id.clone())
        }
        EnterpriseReportDimension::ProductSession => {
            EnterpriseReportGroup::ProductSession(attribution.product_session_id.clone())
        }
        EnterpriseReportDimension::User => EnterpriseReportGroup::User(attribution.user_id.clone()),
        EnterpriseReportDimension::SourceKind => {
            EnterpriseReportGroup::SourceKind(source_kind(&entry.fact.source))
        }
        EnterpriseReportDimension::UtcDay => {
            EnterpriseReportGroup::UtcDay(entry.fact.settled_at.0[..10].to_owned())
        }
    }
}

fn source_kind(source: &EnterpriseUsageSource) -> EnterpriseUsageSourceKind {
    match source {
        EnterpriseUsageSource::Provider { .. } => EnterpriseUsageSourceKind::Provider,
        EnterpriseUsageSource::Worker { .. } => EnterpriseUsageSourceKind::Worker,
        EnterpriseUsageSource::Storage { .. } => EnterpriseUsageSourceKind::Storage,
        EnterpriseUsageSource::Publication { .. } => EnterpriseUsageSourceKind::Publication,
    }
}

fn group_sort_key(group: &EnterpriseReportGroup) -> String {
    match group {
        EnterpriseReportGroup::Organization(id) => format!("organization:{}", id.0),
        EnterpriseReportGroup::Workspace(id) => format!("workspace:{}", id.0),
        EnterpriseReportGroup::Project(id) => format!("project:{}", id.0),
        EnterpriseReportGroup::Repository(id) => format!("repository:{}", id.0),
        EnterpriseReportGroup::Delivery(Some(id)) => format!("delivery:1:{}", id.0),
        EnterpriseReportGroup::Delivery(None) => "delivery:0".to_owned(),
        EnterpriseReportGroup::ProductSession(Some(id)) => {
            format!("product_session:1:{}", id.0)
        }
        EnterpriseReportGroup::ProductSession(None) => "product_session:0".to_owned(),
        EnterpriseReportGroup::User(id) => format!("user:{}", id.0),
        EnterpriseReportGroup::SourceKind(kind) => {
            format!("source_kind:{}", source_kind_name(*kind))
        }
        EnterpriseReportGroup::UtcDay(day) => format!("utc_day:{day}"),
    }
}

fn source_kind_name(kind: EnterpriseUsageSourceKind) -> &'static str {
    match kind {
        EnterpriseUsageSourceKind::Provider => "provider",
        EnterpriseUsageSourceKind::Worker => "worker",
        EnterpriseUsageSourceKind::Storage => "storage",
        EnterpriseUsageSourceKind::Publication => "publication",
    }
}

fn verify_rows(
    rows: &[EnterpriseReportRow],
    expected: EnterpriseReportTotals,
) -> Result<(), EnterpriseReportError> {
    let mut reconciled = EnterpriseReportTotals::default();
    for row in rows {
        reconciled.checked_merge(row.totals)?;
    }
    if reconciled != expected {
        return Err(EnterpriseReportError::new(
            EnterpriseReportErrorKind::Ledger,
            "enterprise report rows do not reconcile with matched details",
        ));
    }
    Ok(())
}

fn validate_query(query: &EnterpriseReportQuery) -> Result<(), EnterpriseReportError> {
    validate_canonical_instant(&query.from_inclusive, "fromInclusive")?;
    validate_canonical_instant(&query.to_exclusive, "toExclusive")?;
    if query.from_inclusive.0 >= query.to_exclusive.0 {
        return Err(EnterpriseReportError::invalid(
            "enterprise report UTC interval must satisfy fromInclusive < toExclusive",
        ));
    }
    Ok(())
}

fn validate_ledger_instant(value: &Instant) -> Result<(), EnterpriseReportError> {
    validate_canonical_instant(value, "settledAt").map_err(|_| {
        EnterpriseReportError::new(
            EnterpriseReportErrorKind::Ledger,
            "enterprise Usage ledger contains an invalid settledAt instant",
        )
    })
}

fn validate_canonical_instant(value: &Instant, field: &str) -> Result<(), EnterpriseReportError> {
    let bytes = value.0.as_bytes();
    let shape = bytes.len() == 24
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
    if !shape {
        return Err(EnterpriseReportError::invalid(format!(
            "{field} is not a canonical millisecond UTC instant"
        )));
    }
    let year = decimal(bytes, 0, 4);
    let month = decimal(bytes, 5, 7);
    let day = decimal(bytes, 8, 10);
    let hour = decimal(bytes, 11, 13);
    let minute = decimal(bytes, 14, 16);
    let second = decimal(bytes, 17, 19);
    let max_day = days_in_month(year, month);
    if year == 0
        || max_day == 0
        || day == 0
        || day > max_day
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(EnterpriseReportError::invalid(format!(
            "{field} is not a valid Gregorian UTC instant"
        )));
    }
    Ok(())
}

fn decimal(bytes: &[u8], start: usize, end: usize) -> u32 {
    bytes[start..end]
        .iter()
        .fold(0, |value, byte| value * 10 + u32::from(*byte - b'0'))
}

const fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

const fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn matches_interval(settled_at: &Instant, query: &EnterpriseReportQuery) -> bool {
    settled_at.0 >= query.from_inclusive.0 && settled_at.0 < query.to_exclusive.0
}

fn checked_add(left: u64, right: u64) -> Result<u64, EnterpriseReportError> {
    left.checked_add(right)
        .ok_or_else(EnterpriseReportError::overflow)
}

fn usize_u64(value: usize) -> Result<u64, EnterpriseReportError> {
    u64::try_from(value).map_err(|_| EnterpriseReportError::overflow())
}

fn canonical_digest(value: &impl Serialize) -> Result<String, EnterpriseReportError> {
    let bytes = serde_json::to_vec(value).map_err(|_| EnterpriseReportError::serialization())?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn projection_csv(
    projection: &EnterpriseReportingProjection,
    max_bytes: u64,
) -> Result<Vec<u8>, EnterpriseReportError> {
    let mut output = BoundedExportWriter::new(max_bytes)?;
    output.push_bytes(
        b"snapshotSequence,fromInclusive,toExclusive,timeRule,currencyRule,groupBy,group,entries,providerTotalTokens,providerCostMicros,workerRuntimeMillis,workerTokens,workerCostMicrounits,storageBytes,storageOperations,publicationOperations\r\n",
    )?;
    for row in &projection.rows {
        csv_record(
            &mut output,
            &[
                projection.snapshot_sequence.to_string(),
                projection.query.from_inclusive.0.clone(),
                projection.query.to_exclusive.0.clone(),
                "settled_at_utc_from_inclusive_to_exclusive".to_owned(),
                "source_native_separated_no_conversion".to_owned(),
                dimension_name(projection.query.group_by).to_owned(),
                serde_json::to_string(&row.group)
                    .map_err(|_| EnterpriseReportError::serialization())?,
                row.totals.entries.to_string(),
                row.totals.provider_total_tokens.to_string(),
                row.totals.provider_cost_micros.to_string(),
                row.totals.worker_runtime_millis.to_string(),
                row.totals.worker_tokens.to_string(),
                row.totals.worker_cost_microunits.to_string(),
                row.totals.storage_bytes.to_string(),
                row.totals.storage_operations.to_string(),
                row.totals.publication_operations.to_string(),
            ],
        )?;
    }
    Ok(output.finish())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DetailPageExport<'page> {
    snapshot_sequence: u64,
    query: &'page EnterpriseReportQuery,
    time_rule: EnterpriseReportTimeRule,
    currency_rule: EnterpriseReportCurrencyRule,
    scanned_entries: u64,
    totals: EnterpriseReportTotals,
    entries: &'page [EnterpriseReportDetail],
    has_more: bool,
}

fn detail_page_json(
    page: &EnterpriseReportPage,
    max_bytes: u64,
) -> Result<Vec<u8>, EnterpriseReportError> {
    bounded_json(
        &DetailPageExport {
            snapshot_sequence: page.snapshot_sequence,
            query: &page.query,
            time_rule: page.time_rule,
            currency_rule: page.currency_rule,
            scanned_entries: page.scanned_entries,
            totals: page.totals,
            entries: &page.entries,
            has_more: page.next.is_some(),
        },
        max_bytes,
    )
}

fn detail_page_csv(
    page: &EnterpriseReportPage,
    max_bytes: u64,
) -> Result<Vec<u8>, EnterpriseReportError> {
    let mut output = BoundedExportWriter::new(max_bytes)?;
    output.push_bytes(
        b"snapshotSequence,fromInclusive,toExclusive,timeRule,currencyRule,sequence,sourceDigest,settledAt,source,attribution,measure,hasMore\r\n",
    )?;
    for entry in &page.entries {
        csv_record(
            &mut output,
            &[
                page.snapshot_sequence.to_string(),
                page.query.from_inclusive.0.clone(),
                page.query.to_exclusive.0.clone(),
                "settled_at_utc_from_inclusive_to_exclusive".to_owned(),
                "source_native_separated_no_conversion".to_owned(),
                entry.sequence.to_string(),
                entry.source_digest.clone(),
                entry.settled_at.0.clone(),
                serde_json::to_string(&entry.source)
                    .map_err(|_| EnterpriseReportError::serialization())?,
                serde_json::to_string(&entry.attribution)
                    .map_err(|_| EnterpriseReportError::serialization())?,
                serde_json::to_string(&entry.measure)
                    .map_err(|_| EnterpriseReportError::serialization())?,
                page.next.is_some().to_string(),
            ],
        )?;
    }
    Ok(output.finish())
}

fn csv_record(
    output: &mut BoundedExportWriter,
    fields: &[String],
) -> Result<(), EnterpriseReportError> {
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            output.push_bytes(b",")?;
        }
        csv_field(output, field)?;
    }
    output.push_bytes(b"\r\n")
}

fn csv_field(output: &mut BoundedExportWriter, field: &str) -> Result<(), EnterpriseReportError> {
    let quoted = field
        .bytes()
        .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'));
    if !quoted {
        return output.push_bytes(field.as_bytes());
    }
    output.push_bytes(b"\"")?;
    for byte in field.bytes() {
        if byte == b'"' {
            output.push_bytes(b"\"")?;
        }
        output.push_bytes(&[byte])?;
    }
    output.push_bytes(b"\"")
}

struct BoundedExportWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
    exceeded: bool,
}

impl BoundedExportWriter {
    fn new(max_bytes: u64) -> Result<Self, EnterpriseReportError> {
        let max_bytes = usize::try_from(max_bytes).map_err(|_| {
            EnterpriseReportError::limit(
                "enterprise report export byte bound is not supported on this platform",
            )
        })?;
        Ok(Self {
            bytes: Vec::with_capacity(max_bytes.min(64 * 1024)),
            max_bytes,
            exceeded: false,
        })
    }

    fn push_bytes(&mut self, bytes: &[u8]) -> Result<(), EnterpriseReportError> {
        self.write_all(bytes).map_err(|_| {
            EnterpriseReportError::limit(
                "enterprise report export exceeds its configured byte bound",
            )
        })
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedExportWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = self.max_bytes.saturating_sub(self.bytes.len());
        if buffer.len() > remaining {
            self.exceeded = true;
            return Err(io::Error::other("enterprise report export bound exceeded"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounded_json(value: &impl Serialize, max_bytes: u64) -> Result<Vec<u8>, EnterpriseReportError> {
    let mut output = BoundedExportWriter::new(max_bytes)?;
    if serde_json::to_writer(&mut output, value).is_err() {
        return if output.exceeded {
            Err(EnterpriseReportError::limit(
                "enterprise report export exceeds its configured byte bound",
            ))
        } else {
            Err(EnterpriseReportError::serialization())
        };
    }
    Ok(output.finish())
}

fn dimension_name(dimension: EnterpriseReportDimension) -> &'static str {
    match dimension {
        EnterpriseReportDimension::Organization => "organization",
        EnterpriseReportDimension::Workspace => "workspace",
        EnterpriseReportDimension::Project => "project",
        EnterpriseReportDimension::Repository => "repository",
        EnterpriseReportDimension::Delivery => "delivery",
        EnterpriseReportDimension::ProductSession => "product_session",
        EnterpriseReportDimension::User => "user",
        EnterpriseReportDimension::SourceKind => "source_kind",
        EnterpriseReportDimension::UtcDay => "utc_day",
    }
}
