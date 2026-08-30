// SPDX-License-Identifier: Apache-2.0

//! Immutable storage-metering sources committed with Artifact completion.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionMessageId, Instant, OrganizationId, ProductSessionId,
    ProjectId, RepositoryId, RequestId, Sha256Digest, UserId, WorkspaceId,
};

use super::{
    ArtifactChunk, ArtifactError, ArtifactRecord, MAX_ARTIFACT_BYTES, canonical_id, load_record,
    sha256_hex,
};

const MAX_PAGE_SIZE: u64 = 200;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub(super) const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS artifact_metering_sources (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT CHECK (sequence > 0),
    source_key TEXT UNIQUE NOT NULL,
    source_digest TEXT NOT NULL,
    fact_json TEXT NOT NULL,
    artifact_id TEXT UNIQUE NOT NULL,
    operation_id TEXT UNIQUE NOT NULL,
    FOREIGN KEY (artifact_id) REFERENCES artifacts (artifact_id) ON DELETE RESTRICT
);
";

/// Complete business attribution frozen from authenticated Control Plane facts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactMeteringAttribution {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub repository_id: RepositoryId,
    pub delivery_id: Option<DeliveryId>,
    pub product_session_id: Option<ProductSessionId>,
    pub user_id: UserId,
}

impl ArtifactMeteringAttribution {
    pub(super) fn validate(&self) -> Result<(), ArtifactError> {
        canonical_id(&self.organization_id.0, "org_", "organizationId")?;
        canonical_id(&self.workspace_id.0, "wsp_", "workspaceId")?;
        canonical_id(&self.project_id.0, "prj_", "projectId")?;
        canonical_id(&self.repository_id.0, "rep_", "repositoryId")?;
        if let Some(delivery_id) = &self.delivery_id {
            canonical_id(&delivery_id.0, "dlv_", "deliveryId")?;
        }
        if let Some(product_session_id) = &self.product_session_id {
            canonical_id(&product_session_id.0, "psn_", "productSessionId")?;
        }
        canonical_id(&self.user_id.0, "usr_", "userId")
    }
}

/// Closed storage operation represented by one immutable source receipt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStorageOperationKind {
    ArtifactFinalize,
}

/// Canonical source fact committed in the same catalog transaction as completion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactStorageSourceFact {
    pub operation_id: ExecutionMessageId,
    pub request_id: RequestId,
    pub artifact_id: ArtifactId,
    pub operation_kind: ArtifactStorageOperationKind,
    pub artifact_kind: String,
    pub bytes: u64,
    pub occurred_at: Instant,
    pub attribution: ArtifactMeteringAttribution,
}

/// One immutable source row consumed by enterprise Usage reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactStorageSourceEntry {
    pub sequence: u64,
    pub source_digest: Sha256Digest,
    pub fact: ArtifactStorageSourceFact,
}

/// Stable cursor over one immutable source-sequence upper bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactStorageSourceCursor {
    snapshot_sequence: u64,
    after_sequence: u64,
}

impl ArtifactStorageSourceCursor {
    #[must_use]
    pub const fn snapshot_sequence(&self) -> u64 {
        self.snapshot_sequence
    }

    #[must_use]
    pub const fn after_sequence(&self) -> u64 {
        self.after_sequence
    }
}

/// One bounded page from a fixed source snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactStorageSourcePage {
    pub snapshot_sequence: u64,
    pub entries: Vec<ArtifactStorageSourceEntry>,
    pub next: Option<ArtifactStorageSourceCursor>,
}

pub(super) fn validate_schema(transaction: &Transaction<'_>) -> Result<(), ArtifactError> {
    let mut statement = transaction
        .prepare("PRAGMA table_info(artifact_metering_sources)")
        .map_err(super::sql_error)?;
    let columns = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, u32>(5)?,
            ))
        })
        .map_err(super::sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(super::sql_error)?;
    if columns
        != [
            ("sequence".to_owned(), "INTEGER".to_owned(), false, 1),
            ("source_key".to_owned(), "TEXT".to_owned(), true, 0),
            ("source_digest".to_owned(), "TEXT".to_owned(), true, 0),
            ("fact_json".to_owned(), "TEXT".to_owned(), true, 0),
            ("artifact_id".to_owned(), "TEXT".to_owned(), true, 0),
            ("operation_id".to_owned(), "TEXT".to_owned(), true, 0),
        ]
    {
        return Err(ArtifactError::adapter(
            "Artifact metering source schema is not canonical",
        ));
    }
    validate_unique_columns(transaction)?;
    validate_artifact_foreign_key(transaction)?;
    Ok(())
}

fn validate_unique_columns(transaction: &Transaction<'_>) -> Result<(), ArtifactError> {
    let mut statement = transaction
        .prepare("PRAGMA index_list(artifact_metering_sources)")
        .map_err(super::sql_error)?;
    let indexes = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, bool>(2)?))
        })
        .map_err(super::sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(super::sql_error)?;
    let mut unique_columns = Vec::new();
    for (name, unique) in indexes {
        if !unique {
            continue;
        }
        let escaped = name.replace('"', "\"\"");
        let pragma = format!("PRAGMA index_info(\"{escaped}\")");
        let mut index_statement = transaction.prepare(&pragma).map_err(super::sql_error)?;
        let columns = index_statement
            .query_map([], |row| row.get::<_, String>(2))
            .map_err(super::sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(super::sql_error)?;
        unique_columns.push(columns);
    }
    unique_columns.sort();
    if unique_columns
        != [
            vec!["artifact_id".to_owned()],
            vec!["operation_id".to_owned()],
            vec!["source_key".to_owned()],
        ]
    {
        return Err(ArtifactError::adapter(
            "Artifact metering source uniqueness is not canonical",
        ));
    }
    Ok(())
}

fn validate_artifact_foreign_key(transaction: &Transaction<'_>) -> Result<(), ArtifactError> {
    let mut statement = transaction
        .prepare("PRAGMA foreign_key_list(artifact_metering_sources)")
        .map_err(super::sql_error)?;
    let foreign_keys = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(6)?,
            ))
        })
        .map_err(super::sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(super::sql_error)?;
    if foreign_keys
        != [(
            "artifacts".to_owned(),
            "artifact_id".to_owned(),
            "artifact_id".to_owned(),
            "RESTRICT".to_owned(),
        )]
    {
        return Err(ArtifactError::adapter(
            "Artifact metering source foreign key is not canonical",
        ));
    }
    Ok(())
}

pub(super) fn insert_final_source(
    transaction: &Transaction<'_>,
    record: &ArtifactRecord,
    chunk: &ArtifactChunk,
) -> Result<ArtifactStorageSourceEntry, ArtifactError> {
    let expected = expected_entry(0, record, chunk)?;
    let source_key = source_key(&expected.fact)?;
    if let Some(existing) = load_by_identity(
        transaction,
        &source_key,
        &expected.fact.artifact_id,
        &expected.fact.operation_id,
    )? {
        if existing.source_digest == expected.source_digest && existing.fact == expected.fact {
            return Ok(existing);
        }
        return Err(ArtifactError::conflict(
            "Artifact metering source identity was reused with different facts",
        ));
    }
    transaction
        .execute(
            "INSERT INTO artifact_metering_sources
             (source_key, source_digest, fact_json, artifact_id, operation_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                source_key,
                expected.source_digest.0,
                serde_json::to_string(&expected.fact).map_err(|_| {
                    ArtifactError::invalid("Artifact metering fact is not serializable")
                })?,
                expected.fact.artifact_id.0,
                expected.fact.operation_id.0,
            ],
        )
        .map_err(super::sql_error)?;
    let sequence = u64::try_from(transaction.last_insert_rowid())
        .map_err(|_| ArtifactError::corrupt("Artifact metering sequence is invalid"))?;
    Ok(ArtifactStorageSourceEntry {
        sequence,
        ..expected
    })
}

pub(super) fn require_complete_source(
    connection: &Connection,
    record: &ArtifactRecord,
) -> Result<(), ArtifactError> {
    let chunk = load_final_chunk(connection, record)?;
    let expected = expected_entry(0, record, &chunk)?;
    let stored = load_by_identity(
        connection,
        &source_key(&expected.fact)?,
        &expected.fact.artifact_id,
        &expected.fact.operation_id,
    )?
    .ok_or_else(|| ArtifactError::corrupt("completed Artifact has no immutable metering source"))?;
    if stored.source_digest != expected.source_digest || stored.fact != expected.fact {
        return Err(ArtifactError::corrupt(
            "completed Artifact differs from its immutable metering source",
        ));
    }
    Ok(())
}

pub(super) fn scan(
    connection: &Connection,
    cursor: Option<&ArtifactStorageSourceCursor>,
    limit: u64,
) -> Result<ArtifactStorageSourcePage, ArtifactError> {
    if limit == 0 || limit > MAX_PAGE_SIZE {
        return Err(ArtifactError::invalid(
            "Artifact metering page limit is outside 1..=200",
        ));
    }
    let (snapshot_sequence, after_sequence) = match cursor {
        Some(cursor)
            if cursor.snapshot_sequence <= MAX_SAFE_INTEGER
                && cursor.after_sequence <= cursor.snapshot_sequence =>
        {
            (cursor.snapshot_sequence, cursor.after_sequence)
        }
        Some(_) => {
            return Err(ArtifactError::invalid(
                "Artifact metering cursor does not identify a valid snapshot",
            ));
        }
        None => (last_sequence(connection)?, 0),
    };
    let query_limit = limit
        .checked_add(1)
        .ok_or_else(|| ArtifactError::invalid("Artifact metering page limit overflowed"))?;
    let mut statement = connection
        .prepare(
            "SELECT sequence, source_key, source_digest, fact_json, artifact_id, operation_id
             FROM artifact_metering_sources
             WHERE sequence > ?1 AND sequence <= ?2
             ORDER BY sequence LIMIT ?3",
        )
        .map_err(super::sql_error)?;
    let rows = statement
        .query_map(
            params![
                super::i64_value(after_sequence, "Artifact metering cursor")?,
                super::i64_value(snapshot_sequence, "Artifact metering snapshot")?,
                super::i64_value(query_limit, "Artifact metering page limit")?,
            ],
            stored_row,
        )
        .map_err(super::sql_error)?;
    let mut entries = rows
        .map(|row| {
            row.map_err(super::sql_error)
                .and_then(complete_stored_entry)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for entry in &entries {
        let record = load_record(connection, &entry.fact.artifact_id)?.ok_or_else(|| {
            ArtifactError::corrupt("Artifact metering source points to missing metadata")
        })?;
        if !record.is_complete() {
            return Err(ArtifactError::corrupt(
                "Artifact metering source points to incomplete metadata",
            ));
        }
    }
    let page_size = usize::try_from(limit)
        .map_err(|_| ArtifactError::invalid("Artifact metering page limit is invalid"))?;
    let has_more = entries.len() > page_size;
    if has_more {
        entries.pop();
    }
    let next = if has_more {
        Some(ArtifactStorageSourceCursor {
            snapshot_sequence,
            after_sequence: entries
                .last()
                .ok_or_else(|| ArtifactError::corrupt("Artifact metering page is empty"))?
                .sequence,
        })
    } else {
        None
    };
    Ok(ArtifactStorageSourcePage {
        snapshot_sequence,
        entries,
        next,
    })
}

pub(super) fn load_for_artifact(
    connection: &Connection,
    artifact_id: &ArtifactId,
) -> Result<Option<ArtifactStorageSourceEntry>, ArtifactError> {
    canonical_id(&artifact_id.0, "art_", "artifactId")?;
    let stored = connection
        .query_row(
            "SELECT sequence, source_key, source_digest, fact_json, artifact_id, operation_id
             FROM artifact_metering_sources WHERE artifact_id = ?1",
            [&artifact_id.0],
            stored_row,
        )
        .optional()
        .map_err(super::sql_error)?;
    let Some(entry) = stored.map(complete_stored_entry).transpose()? else {
        return Ok(None);
    };
    if entry.fact.artifact_id != *artifact_id {
        return Err(ArtifactError::corrupt(
            "Artifact metering source identity differs from its Artifact",
        ));
    }
    let record = load_record(connection, artifact_id)?.ok_or_else(|| {
        ArtifactError::corrupt("Artifact metering source points to missing metadata")
    })?;
    if !record.is_complete() {
        return Err(ArtifactError::corrupt(
            "Artifact metering source points to incomplete metadata",
        ));
    }
    Ok(Some(entry))
}

fn expected_entry(
    sequence: u64,
    record: &ArtifactRecord,
    chunk: &ArtifactChunk,
) -> Result<ArtifactStorageSourceEntry, ArtifactError> {
    let expected_sequence = if record.complete {
        record.acknowledged_sequence
    } else {
        record.acknowledged_sequence.saturating_add(1)
    };
    if !chunk.is_final
        || chunk.artifact_id != record.open.artifact_id
        || chunk.sequence != expected_sequence
    {
        return Err(ArtifactError::corrupt(
            "Artifact completion does not identify one exact final chunk",
        ));
    }
    let fact = ArtifactStorageSourceFact {
        operation_id: chunk.message_id.clone(),
        request_id: record.open.request_id.clone(),
        artifact_id: record.open.artifact_id.clone(),
        operation_kind: ArtifactStorageOperationKind::ArtifactFinalize,
        artifact_kind: record.open.kind.clone(),
        bytes: record.open.size_bytes,
        occurred_at: millis_to_instant(chunk.sent_at_millis)?,
        attribution: record.open.metering_attribution.clone(),
    };
    validate_fact(&fact)?;
    Ok(ArtifactStorageSourceEntry {
        sequence,
        source_digest: digest(&fact)?,
        fact,
    })
}

fn validate_fact(fact: &ArtifactStorageSourceFact) -> Result<(), ArtifactError> {
    canonical_id(&fact.operation_id.0, "xmsg_", "operationId")?;
    canonical_id(&fact.request_id.0, "req_", "requestId")?;
    canonical_id(&fact.artifact_id.0, "art_", "artifactId")?;
    super::bounded_text(&fact.artifact_kind, "Artifact kind", 120)?;
    if fact.bytes > MAX_ARTIFACT_BYTES {
        return Err(ArtifactError::invalid(
            "Artifact metering bytes exceed the supported maximum",
        ));
    }
    fact.attribution.validate()?;
    let _ = instant_millis(&fact.occurred_at)?;
    Ok(())
}

fn source_key(fact: &ArtifactStorageSourceFact) -> Result<String, ArtifactError> {
    let identity = serde_json::to_vec(&(
        fact.operation_kind,
        &fact.artifact_id,
        &fact.operation_id,
        &fact.request_id,
    ))
    .map_err(|_| ArtifactError::invalid("Artifact metering identity is not serializable"))?;
    Ok(format!("artifact-storage:{:x}", Sha256::digest(identity)))
}

fn digest(fact: &ArtifactStorageSourceFact) -> Result<Sha256Digest, ArtifactError> {
    let bytes = serde_json::to_vec(fact)
        .map_err(|_| ArtifactError::invalid("Artifact metering fact is not serializable"))?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn load_by_identity(
    connection: &Connection,
    source_key: &str,
    artifact_id: &ArtifactId,
    operation_id: &ExecutionMessageId,
) -> Result<Option<ArtifactStorageSourceEntry>, ArtifactError> {
    let mut statement = connection
        .prepare(
            "SELECT sequence, source_key, source_digest, fact_json, artifact_id, operation_id
             FROM artifact_metering_sources
             WHERE source_key = ?1 OR artifact_id = ?2 OR operation_id = ?3
             ORDER BY sequence",
        )
        .map_err(super::sql_error)?;
    let stored = statement
        .query_map(
            params![source_key, artifact_id.0, operation_id.0],
            stored_row,
        )
        .map_err(super::sql_error)?
        .map(|row| {
            row.map_err(super::sql_error)
                .and_then(complete_stored_entry)
        })
        .collect::<Result<Vec<_>, _>>()?;
    match stored.as_slice() {
        [] => Ok(None),
        [entry] => Ok(Some(entry.clone())),
        _ => Err(ArtifactError::conflict(
            "Artifact metering identities resolve to different sources",
        )),
    }
}

#[derive(Debug)]
struct StoredEntry {
    sequence: i64,
    source_key: String,
    source_digest: String,
    fact_json: String,
    artifact_id: String,
    operation_id: String,
}

fn stored_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredEntry> {
    Ok(StoredEntry {
        sequence: row.get(0)?,
        source_key: row.get(1)?,
        source_digest: row.get(2)?,
        fact_json: row.get(3)?,
        artifact_id: row.get(4)?,
        operation_id: row.get(5)?,
    })
}

fn complete_stored_entry(stored: StoredEntry) -> Result<ArtifactStorageSourceEntry, ArtifactError> {
    let sequence = u64::try_from(stored.sequence)
        .map_err(|_| ArtifactError::corrupt("Artifact metering sequence is invalid"))?;
    let fact: ArtifactStorageSourceFact = serde_json::from_str(&stored.fact_json)
        .map_err(|_| ArtifactError::corrupt("Artifact metering fact is invalid"))?;
    validate_fact(&fact)
        .map_err(|_| ArtifactError::corrupt("Artifact metering fact is malformed"))?;
    let source_digest = Sha256Digest(stored.source_digest);
    if stored.source_key != source_key(&fact)?
        || stored.artifact_id != fact.artifact_id.0
        || stored.operation_id != fact.operation_id.0
        || serde_json::to_string(&fact).ok().as_deref() != Some(&stored.fact_json)
        || source_digest != digest(&fact)?
        || sha256_hex(&source_digest).is_err()
    {
        return Err(ArtifactError::corrupt(
            "Artifact metering row differs from its canonical fact",
        ));
    }
    Ok(ArtifactStorageSourceEntry {
        sequence,
        source_digest,
        fact,
    })
}

fn load_final_chunk(
    connection: &Connection,
    record: &ArtifactRecord,
) -> Result<ArtifactChunk, ArtifactError> {
    let stored = connection
        .query_row(
            "SELECT message_id, sent_at_millis, sequence, content_type, digest, is_final
             FROM artifact_chunks
             WHERE artifact_id = ?1 AND sequence = ?2",
            params![
                record.open.artifact_id.0,
                super::i64_value(record.acknowledged_sequence, "Artifact final sequence")?,
            ],
            |row| {
                let sent_at_millis = row.get::<_, i64>(1)?;
                let sequence = row.get::<_, i64>(2)?;
                Ok(ArtifactChunk {
                    scope_key: record.open.scope_key.clone(),
                    message_id: ExecutionMessageId(row.get(0)?),
                    artifact_id: record.open.artifact_id.clone(),
                    provenance: record.open.provenance.clone(),
                    sent_at_millis: u64::try_from(sent_at_millis)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, sent_at_millis))?,
                    sequence: u64::try_from(sequence)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(2, sequence))?,
                    content_type: row.get(3)?,
                    digest: Sha256Digest(row.get(4)?),
                    bytes: Vec::new(),
                    is_final: row.get::<_, i64>(5)? == 1,
                })
            },
        )
        .optional()
        .map_err(super::sql_error)?
        .ok_or_else(|| ArtifactError::corrupt("completed Artifact final chunk is missing"))?;
    if !stored.is_final {
        return Err(ArtifactError::corrupt(
            "completed Artifact final chunk is not marked final",
        ));
    }
    Ok(stored)
}

fn last_sequence(connection: &Connection) -> Result<u64, ArtifactError> {
    let value = connection
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM artifact_metering_sources",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(super::sql_error)?;
    u64::try_from(value)
        .map_err(|_| ArtifactError::corrupt("Artifact metering sequence is negative"))
}

fn millis_to_instant(value: u64) -> Result<Instant, ArtifactError> {
    let seconds = value / 1_000;
    let milliseconds = value % 1_000;
    let days = i64::try_from(seconds / 86_400)
        .map_err(|_| ArtifactError::invalid("Artifact metering time is out of range"))?;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days)?;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let instant = Instant(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milliseconds:03}Z"
    ));
    let parsed = instant_millis(&instant)?;
    if parsed != value {
        return Err(ArtifactError::invalid(
            "Artifact metering time cannot be represented exactly",
        ));
    }
    Ok(instant)
}

fn instant_millis(instant: &Instant) -> Result<u64, ArtifactError> {
    let bytes = instant.0.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return Err(ArtifactError::invalid(
            "Artifact metering Instant is not canonical UTC milliseconds",
        ));
    }
    let year = decimal(bytes, 0, 4)?;
    let month = decimal(bytes, 5, 2)?;
    let day = decimal(bytes, 8, 2)?;
    let hour = decimal(bytes, 11, 2)?;
    let minute = decimal(bytes, 14, 2)?;
    let second = decimal(bytes, 17, 2)?;
    let millis = decimal(bytes, 20, 3)?;
    if year < 1970
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(ArtifactError::invalid(
            "Artifact metering Instant contains an invalid date-time",
        ));
    }
    let days = days_from_civil(year, month, day)?;
    days.checked_mul(86_400_000)
        .and_then(|value| value.checked_add(hour * 3_600_000))
        .and_then(|value| value.checked_add(minute * 60_000))
        .and_then(|value| value.checked_add(second * 1_000))
        .and_then(|value| value.checked_add(millis))
        .ok_or_else(|| ArtifactError::invalid("Artifact metering Instant is out of range"))
}

fn decimal(bytes: &[u8], start: usize, length: usize) -> Result<u64, ArtifactError> {
    bytes[start..start + length]
        .iter()
        .try_fold(0_u64, |value, byte| {
            if byte.is_ascii_digit() {
                Ok(value * 10 + u64::from(byte - b'0'))
            } else {
                Err(ArtifactError::invalid(
                    "Artifact metering Instant contains a non-decimal component",
                ))
            }
        })
}

const fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

fn days_from_civil(year: u64, month: u64, day: u64) -> Result<u64, ArtifactError> {
    let year = i64::try_from(year)
        .map_err(|_| ArtifactError::invalid("Artifact metering year is out of range"))?;
    let month = i64::try_from(month)
        .map_err(|_| ArtifactError::invalid("Artifact metering month is out of range"))?;
    let day = i64::try_from(day)
        .map_err(|_| ArtifactError::invalid("Artifact metering day is out of range"))?;
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let since_epoch = era * 146_097 + day_of_era - 719_468;
    u64::try_from(since_epoch)
        .map_err(|_| ArtifactError::invalid("Artifact metering Instant predates Unix epoch"))
}

fn civil_from_days(days_since_epoch: i64) -> Result<(i64, u64, u64), ArtifactError> {
    let z = days_since_epoch
        .checked_add(719_468)
        .ok_or_else(|| ArtifactError::invalid("Artifact metering time is out of range"))?;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let month = u64::try_from(month)
        .map_err(|_| ArtifactError::invalid("Artifact metering month is out of range"))?;
    let day = u64::try_from(day)
        .map_err(|_| ArtifactError::invalid("Artifact metering day is out of range"))?;
    Ok((year, month, day))
}
