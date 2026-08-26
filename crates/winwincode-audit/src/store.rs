// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use winwincode_domain::{OrganizationId, Sha256Digest};

use crate::event::{AuditAccess, AuditEvent, AuditRetention, validate_digest};

const DATABASE_FILE_NAME: &str = "audit.sqlite3";
const SCHEMA_VERSION: i64 = 1;
const CHAIN_DOMAIN: &[u8] = b"winwincode.audit-chain.v1";

/// Stable audit error categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditErrorKind {
    InvalidInput,
    RequestConflict,
    Corrupt,
    Adapter,
    Closed,
}

/// Adapter-neutral audit failure without remote or credential-bearing text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditError {
    kind: AuditErrorKind,
    message: String,
}

impl AuditError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: AuditErrorKind::InvalidInput,
            message: message.into(),
        }
    }

    fn request_conflict() -> Self {
        Self {
            kind: AuditErrorKind::RequestConflict,
            message: "audit event id was already used for different canonical facts".to_owned(),
        }
    }

    fn corrupt(message: impl Into<String>) -> Self {
        Self {
            kind: AuditErrorKind::Corrupt,
            message: message.into(),
        }
    }

    fn adapter(message: impl Into<String>) -> Self {
        Self {
            kind: AuditErrorKind::Adapter,
            message: message.into(),
        }
    }

    fn closed() -> Self {
        Self {
            kind: AuditErrorKind::Closed,
            message: "audit store is already closed".to_owned(),
        }
    }

    /// Builds the stable error returned when an application has no live audit store.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            kind: AuditErrorKind::Closed,
            message: "audit store is unavailable".to_owned(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> AuditErrorKind {
        self.kind
    }
}

impl fmt::Display for AuditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AuditError {}

/// One ordered audit record. `event` is absent only after its finite retained
/// payload has expired; the immutable header and chain digest remain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditRecord {
    sequence: u64,
    previous_digest: Option<Sha256Digest>,
    event_digest: Sha256Digest,
    event: Option<AuditEvent>,
}

impl AuditRecord {
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn previous_digest(&self) -> Option<&Sha256Digest> {
        self.previous_digest.as_ref()
    }

    #[must_use]
    pub const fn event_digest(&self) -> &Sha256Digest {
        &self.event_digest
    }

    #[must_use]
    pub const fn event(&self) -> Option<&AuditEvent> {
        self.event.as_ref()
    }
}

/// One bounded, scope-filtered audit page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditPage {
    records: Vec<AuditRecord>,
    next_sequence: Option<u64>,
}

impl AuditPage {
    #[must_use]
    pub fn records(&self) -> &[AuditRecord] {
        &self.records
    }

    #[must_use]
    pub const fn next_sequence(&self) -> Option<u64> {
        self.next_sequence
    }
}

/// Local `SQLite` adapter for immutable organization audit chains.
pub struct AuditStore {
    connection: Option<Connection>,
    database_path: PathBuf,
}

impl AuditStore {
    /// Opens the local audit database and applies its additive schema.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the directory, `SQLite` durability
    /// settings, or schema cannot be prepared.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, AuditError> {
        let data_directory = data_directory.as_ref();
        fs::create_dir_all(data_directory).map_err(|error| {
            AuditError::adapter(format!("failed to create audit data directory: {error}"))
        })?;
        let database_path = data_directory.join(DATABASE_FILE_NAME);
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX;
        let mut connection =
            Connection::open_with_flags(&database_path, flags).map_err(sql_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sql_error)?;
        apply_schema(&mut connection)?;
        Ok(Self {
            connection: Some(connection),
            database_path,
        })
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Appends a validated event to its organization's hash chain. Exact
    /// event-id replay returns the original record without a second sequence.
    ///
    /// # Errors
    ///
    /// Returns a request conflict for changed reuse, a corruption error for a
    /// damaged prior chain, or an adapter error when the transaction fails.
    pub fn append(&mut self, event: &AuditEvent) -> Result<AuditRecord, AuditError> {
        event.validate()?;
        let payload = serde_json::to_vec(event).map_err(json_error)?;
        let payload_digest = sha256_digest(&payload);
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;

        if let Some(stored) = load_header_by_event_id(&transaction, event.event_id().as_str())? {
            let record = record_from_header(&stored, u64::MAX)?;
            if stored.payload_digest != payload_digest
                || stored_header_facts(&stored) != event_header_facts(event)?
            {
                return Err(AuditError::request_conflict());
            }
            transaction.commit().map_err(sql_error)?;
            return Ok(record);
        }

        let organization_id = &event.scope().organization_id().0;
        let (sequence, previous_digest) = next_chain_position(&transaction, organization_id)?;
        let facts = event_header_facts(event)?;
        let event_digest = chain_digest(
            organization_id,
            sequence,
            previous_digest.as_ref(),
            &facts,
            &payload_digest,
        );
        let sequence_sql = i64::try_from(sequence)
            .map_err(|_| AuditError::adapter("audit sequence exceeds the SQLite range"))?;
        transaction
            .execute(
                "INSERT INTO audit_events \
                 (organization_id, sequence, event_id, occurred_at_millis, workspace_id, \
                  project_id, repository_id, retention_kind, retention_until_millis, \
                  previous_digest, event_digest, payload_digest) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    organization_id,
                    sequence_sql,
                    facts.event_id,
                    facts.occurred_at_millis,
                    facts.workspace_id,
                    facts.project_id,
                    facts.repository_id,
                    facts.retention_kind,
                    facts.retention_until_millis,
                    previous_digest.as_ref().map(|digest| digest.0.as_str()),
                    event_digest.0,
                    payload_digest.0,
                ],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "INSERT INTO audit_payloads (organization_id, sequence, payload) \
                 VALUES (?1, ?2, ?3)",
                params![organization_id, sequence_sql, payload],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "INSERT INTO audit_chain_heads (organization_id, last_sequence, last_digest) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT (organization_id) DO UPDATE SET \
                   last_sequence = excluded.last_sequence, last_digest = excluded.last_digest",
                params![organization_id, sequence_sql, event_digest.0],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;

        Ok(AuditRecord {
            sequence,
            previous_digest,
            event_digest,
            event: Some(event.clone()),
        })
    }

    /// Reads one bounded page filtered by an already-authorized exact scope.
    ///
    /// # Errors
    ///
    /// Rejects invalid limits/cursors, damaged headers or payloads, and payload
    /// loss before a finite retention deadline.
    pub fn read(
        &self,
        access: &AuditAccess,
        after_sequence: u64,
        limit: usize,
        as_of_millis: u64,
    ) -> Result<AuditPage, AuditError> {
        access.scope.validate()?;
        if !(1..=200).contains(&limit)
            || after_sequence > i64::MAX as u64
            || as_of_millis > i64::MAX as u64
        {
            return Err(AuditError::invalid(
                "audit page cursor, limit, or timestamp is out of range",
            ));
        }
        let scope = access.scope();
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT e.organization_id, e.sequence, e.event_id, e.occurred_at_millis, \
                    e.workspace_id, e.project_id, e.repository_id, e.retention_kind, \
                    e.retention_until_millis, e.previous_digest, e.event_digest, \
                    e.payload_digest, p.payload, t.pruned_at_millis, t.event_digest \
             FROM audit_events e LEFT JOIN audit_payloads p \
               ON p.organization_id = e.organization_id AND p.sequence = e.sequence \
             LEFT JOIN audit_payload_tombstones t \
               ON t.organization_id = e.organization_id AND t.sequence = e.sequence \
             WHERE e.organization_id = ?1 AND e.sequence > ?2 \
               AND (?3 IS NULL OR e.workspace_id = ?3) \
               AND (?4 IS NULL OR e.project_id = ?4) \
               AND (?5 IS NULL OR e.repository_id = ?5) \
             ORDER BY e.sequence LIMIT ?6",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map(
                params![
                    scope.organization_id().0,
                    i64::try_from(after_sequence).map_err(|_| {
                        AuditError::invalid("audit page cursor exceeds the SQLite range")
                    })?,
                    scope.workspace_id().map(|id| id.0.as_str()),
                    scope.project_id().map(|id| id.0.as_str()),
                    scope.repository_id().map(|id| id.0.as_str()),
                    i64::try_from(limit + 1)
                        .map_err(|_| AuditError::invalid("audit page limit is out of range"))?,
                ],
                stored_header_row,
            )
            .map_err(sql_error)?;
        let mut headers = rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)?;
        let has_more = headers.len() > limit;
        if has_more {
            headers.pop();
        }
        let records = headers
            .iter()
            .map(|header| record_from_header(header, as_of_millis))
            .collect::<Result<Vec<_>, _>>()?;
        let next_sequence = if has_more {
            records.last().map(AuditRecord::sequence)
        } else {
            None
        };
        Ok(AuditPage {
            records,
            next_sequence,
        })
    }

    /// Deletes only canonical payload bytes whose finite retention deadline
    /// has passed. Immutable event headers and their hash chain remain.
    ///
    /// # Errors
    ///
    /// Rejects an out-of-range timestamp and fails before deletion when an
    /// affected organization chain is corrupt.
    pub fn prune_expired_payloads(&mut self, now_millis: u64) -> Result<usize, AuditError> {
        let now = i64::try_from(now_millis)
            .map_err(|_| AuditError::invalid("audit retention timestamp is out of range"))?;
        let organizations = {
            let mut statement = self
                .connection()?
                .prepare(
                    "SELECT DISTINCT e.organization_id \
                     FROM audit_events e JOIN audit_payloads p \
                       ON p.organization_id = e.organization_id AND p.sequence = e.sequence \
                     WHERE e.retention_kind = 'until' AND e.retention_until_millis <= ?1 \
                     ORDER BY e.organization_id",
                )
                .map_err(sql_error)?;
            let rows = statement
                .query_map([now], |row| row.get::<_, String>(0))
                .map_err(sql_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)?
        };
        for organization_id in organizations {
            self.verify_organization(&OrganizationId(organization_id))?;
        }
        let transaction = self
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        transaction
            .execute(
                "INSERT INTO audit_payload_tombstones \
                 (organization_id, sequence, pruned_at_millis, event_digest) \
                 SELECT e.organization_id, e.sequence, ?1, e.event_digest \
                 FROM audit_events e JOIN audit_payloads p \
                   ON p.organization_id = e.organization_id AND p.sequence = e.sequence \
                 WHERE e.retention_kind = 'until' AND e.retention_until_millis <= ?1 \
                 ON CONFLICT (organization_id, sequence) DO NOTHING",
                [now],
            )
            .map_err(sql_error)?;
        let deleted = transaction
            .execute(
                "DELETE FROM audit_payloads \
                 WHERE EXISTS ( \
                   SELECT 1 FROM audit_payload_tombstones t \
                   WHERE t.organization_id = audit_payloads.organization_id \
                     AND t.sequence = audit_payloads.sequence \
                 )",
                [],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(deleted)
    }

    /// Verifies the complete immutable header chain for one organization.
    /// Retained payloads are also decoded and matched to their headers.
    ///
    /// # Errors
    ///
    /// Returns a corruption error at the first missing, changed, or reordered
    /// header/payload.
    pub fn verify_organization(&self, organization_id: &OrganizationId) -> Result<(), AuditError> {
        validate_organization_id(organization_id)?;
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT e.organization_id, e.sequence, e.event_id, e.occurred_at_millis, \
                    e.workspace_id, e.project_id, e.repository_id, e.retention_kind, \
                    e.retention_until_millis, e.previous_digest, e.event_digest, \
                    e.payload_digest, p.payload, t.pruned_at_millis, t.event_digest \
             FROM audit_events e LEFT JOIN audit_payloads p \
               ON p.organization_id = e.organization_id AND p.sequence = e.sequence \
             LEFT JOIN audit_payload_tombstones t \
               ON t.organization_id = e.organization_id AND t.sequence = e.sequence \
             WHERE e.organization_id = ?1 ORDER BY e.sequence",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([&organization_id.0], stored_header_row)
            .map_err(sql_error)?;
        let headers = rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)?;
        let mut expected_sequence = 1_u64;
        let mut previous_digest: Option<Sha256Digest> = None;
        for header in &headers {
            if header.sequence != expected_sequence
                || header.previous_digest.as_ref() != previous_digest.as_ref()
            {
                return Err(AuditError::corrupt(
                    "audit organization chain sequence or previous digest changed",
                ));
            }
            let record = record_from_header(header, u64::MAX)?;
            previous_digest = Some(record.event_digest.clone());
            expected_sequence = expected_sequence
                .checked_add(1)
                .ok_or_else(|| AuditError::corrupt("audit sequence overflow"))?;
        }
        let stored_head = self
            .connection()?
            .query_row(
                "SELECT last_sequence, last_digest FROM audit_chain_heads \
                 WHERE organization_id = ?1",
                [&organization_id.0],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(sql_error)?;
        match (headers.last(), stored_head) {
            (None, None) => Ok(()),
            (Some(last), Some((sequence, digest)))
                if i64::try_from(last.sequence).ok() == Some(sequence)
                    && last.event_digest.0 == digest =>
            {
                Ok(())
            }
            _ => Err(AuditError::corrupt(
                "audit organization chain head does not match its immutable tail",
            )),
        }
    }

    /// Closes the local adapter.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when `SQLite` close fails.
    pub fn close(mut self) -> Result<(), AuditError> {
        let Some(connection) = self.connection.take() else {
            return Err(AuditError::closed());
        };
        connection.close().map_err(|(_, error)| sql_error(error))
    }

    fn connection(&self) -> Result<&Connection, AuditError> {
        self.connection.as_ref().ok_or_else(AuditError::closed)
    }

    fn connection_mut(&mut self) -> Result<&mut Connection, AuditError> {
        self.connection.as_mut().ok_or_else(AuditError::closed)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EventHeaderFacts {
    event_id: String,
    occurred_at_millis: i64,
    workspace_id: Option<String>,
    project_id: Option<String>,
    repository_id: Option<String>,
    retention_kind: &'static str,
    retention_until_millis: Option<i64>,
}

#[derive(Clone, Debug)]
struct StoredHeader {
    organization_id: String,
    sequence: u64,
    event_id: String,
    occurred_at_millis: i64,
    workspace_id: Option<String>,
    project_id: Option<String>,
    repository_id: Option<String>,
    retention_kind: String,
    retention_until_millis: Option<i64>,
    previous_digest: Option<Sha256Digest>,
    event_digest: Sha256Digest,
    payload_digest: Sha256Digest,
    payload: Option<Vec<u8>>,
    payload_pruned_at_millis: Option<i64>,
    tombstone_event_digest: Option<Sha256Digest>,
}

fn event_header_facts(event: &AuditEvent) -> Result<EventHeaderFacts, AuditError> {
    let (retention_kind, retention_until_millis) = match event.retention() {
        AuditRetention::UntilMillis(value) => (
            "until",
            Some(i64::try_from(value).map_err(|_| {
                AuditError::invalid("audit retention deadline exceeds the SQLite range")
            })?),
        ),
        AuditRetention::Indefinite => ("indefinite", None),
    };
    Ok(EventHeaderFacts {
        event_id: event.event_id().as_str().to_owned(),
        occurred_at_millis: i64::try_from(event.occurred_at_millis())
            .map_err(|_| AuditError::invalid("audit timestamp exceeds the SQLite range"))?,
        workspace_id: event.scope().workspace_id().map(|id| id.0.clone()),
        project_id: event.scope().project_id().map(|id| id.0.clone()),
        repository_id: event.scope().repository_id().map(|id| id.0.clone()),
        retention_kind,
        retention_until_millis,
    })
}

fn next_chain_position(
    connection: &Connection,
    organization_id: &str,
) -> Result<(u64, Option<Sha256Digest>), AuditError> {
    let head = connection
        .query_row(
            "SELECT last_sequence, last_digest FROM audit_chain_heads \
             WHERE organization_id = ?1",
            [organization_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let immutable_tail = connection
        .query_row(
            "SELECT sequence, event_digest FROM audit_events \
             WHERE organization_id = ?1 ORDER BY sequence DESC LIMIT 1",
            [organization_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    match (&head, &immutable_tail) {
        (None, None) => return Ok((1, None)),
        (Some(head), Some(tail)) if head == tail => {}
        _ => {
            return Err(AuditError::corrupt(
                "audit organization chain head does not match its immutable tail",
            ));
        }
    }
    let (sequence, digest) = head.ok_or_else(|| {
        AuditError::corrupt("audit organization chain head disappeared during verification")
    })?;
    let sequence = u64::try_from(sequence)
        .map_err(|_| AuditError::corrupt("audit head sequence is negative"))?;
    let digest = Sha256Digest(digest);
    validate_digest(&digest, "stored audit head digest")?;
    Ok((
        sequence
            .checked_add(1)
            .ok_or_else(|| AuditError::adapter("audit sequence overflow"))?,
        Some(digest),
    ))
}

fn stored_header_facts(header: &StoredHeader) -> EventHeaderFacts {
    EventHeaderFacts {
        event_id: header.event_id.clone(),
        occurred_at_millis: header.occurred_at_millis,
        workspace_id: header.workspace_id.clone(),
        project_id: header.project_id.clone(),
        repository_id: header.repository_id.clone(),
        retention_kind: match header.retention_kind.as_str() {
            "until" => "until",
            "indefinite" => "indefinite",
            _ => "invalid",
        },
        retention_until_millis: header.retention_until_millis,
    }
}

fn record_from_header(header: &StoredHeader, as_of_millis: u64) -> Result<AuditRecord, AuditError> {
    validate_digest(&header.event_digest, "stored audit event digest")?;
    validate_digest(&header.payload_digest, "stored audit payload digest")?;
    if let Some(previous) = &header.previous_digest {
        validate_digest(previous, "stored audit previous digest")?;
    }
    let facts = stored_header_facts(header);
    if facts.retention_kind == "invalid"
        || (facts.retention_kind == "until") != facts.retention_until_millis.is_some()
    {
        return Err(AuditError::corrupt(
            "stored audit retention header is invalid",
        ));
    }
    let expected_digest = chain_digest(
        &header.organization_id,
        header.sequence,
        header.previous_digest.as_ref(),
        &facts,
        &header.payload_digest,
    );
    if expected_digest != header.event_digest {
        return Err(AuditError::corrupt("stored audit header digest changed"));
    }
    let event = match (
        &header.payload,
        header.payload_pruned_at_millis,
        &header.tombstone_event_digest,
    ) {
        (Some(payload), None, None) => {
            if sha256_digest(payload) != header.payload_digest {
                return Err(AuditError::corrupt("stored audit payload digest changed"));
            }
            let event: AuditEvent = serde_json::from_slice(payload)
                .map_err(|_| AuditError::corrupt("stored audit payload is not canonical JSON"))?;
            event
                .validate()
                .map_err(|_| AuditError::corrupt("stored audit payload is invalid"))?;
            if event.scope().organization_id().0 != header.organization_id
                || event_header_facts(&event)? != facts
            {
                return Err(AuditError::corrupt(
                    "stored audit payload no longer matches its immutable header",
                ));
            }
            Some(event)
        }
        (None, Some(pruned_at), Some(tombstone_digest)) => {
            let valid = header
                .retention_until_millis
                .is_some_and(|until| until <= pruned_at)
                && u64::try_from(pruned_at).is_ok_and(|pruned_at| pruned_at <= as_of_millis)
                && tombstone_digest == &header.event_digest;
            if !valid {
                return Err(AuditError::corrupt(
                    "audit retention tombstone does not authorize the missing payload",
                ));
            }
            None
        }
        _ => {
            return Err(AuditError::corrupt(
                "audit payload and retention tombstone state is incomplete",
            ));
        }
    };
    Ok(AuditRecord {
        sequence: header.sequence,
        previous_digest: header.previous_digest.clone(),
        event_digest: header.event_digest.clone(),
        event,
    })
}

fn chain_digest(
    organization_id: &str,
    sequence: u64,
    previous_digest: Option<&Sha256Digest>,
    facts: &EventHeaderFacts,
    payload_digest: &Sha256Digest,
) -> Sha256Digest {
    let mut hash = Sha256::new();
    framed(&mut hash, CHAIN_DOMAIN);
    framed(&mut hash, organization_id.as_bytes());
    hash.update(sequence.to_be_bytes());
    framed(
        &mut hash,
        previous_digest.map_or(&[][..], |digest| digest.0.as_bytes()),
    );
    framed(&mut hash, facts.event_id.as_bytes());
    hash.update(facts.occurred_at_millis.to_be_bytes());
    framed(
        &mut hash,
        facts.workspace_id.as_deref().unwrap_or_default().as_bytes(),
    );
    framed(
        &mut hash,
        facts.project_id.as_deref().unwrap_or_default().as_bytes(),
    );
    framed(
        &mut hash,
        facts
            .repository_id
            .as_deref()
            .unwrap_or_default()
            .as_bytes(),
    );
    framed(&mut hash, facts.retention_kind.as_bytes());
    hash.update(
        facts
            .retention_until_millis
            .unwrap_or_default()
            .to_be_bytes(),
    );
    framed(&mut hash, payload_digest.0.as_bytes());
    Sha256Digest(format!("sha256:{:x}", hash.finalize()))
}

fn framed(hash: &mut Sha256, value: &[u8]) {
    hash.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hash.update(value);
}

fn load_header_by_event_id(
    connection: &Connection,
    event_id: &str,
) -> Result<Option<StoredHeader>, AuditError> {
    connection
        .query_row(
            "SELECT e.organization_id, e.sequence, e.event_id, e.occurred_at_millis, \
                    e.workspace_id, e.project_id, e.repository_id, e.retention_kind, \
                    e.retention_until_millis, e.previous_digest, e.event_digest, \
                    e.payload_digest, p.payload, t.pruned_at_millis, t.event_digest \
             FROM audit_events e LEFT JOIN audit_payloads p \
               ON p.organization_id = e.organization_id AND p.sequence = e.sequence \
             LEFT JOIN audit_payload_tombstones t \
               ON t.organization_id = e.organization_id AND t.sequence = e.sequence \
             WHERE e.event_id = ?1",
            [event_id],
            stored_header_row,
        )
        .optional()
        .map_err(sql_error)
}

fn stored_header_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredHeader> {
    let sequence = row.get::<_, i64>(1)?;
    Ok(StoredHeader {
        organization_id: row.get(0)?,
        sequence: u64::try_from(sequence).unwrap_or_default(),
        event_id: row.get(2)?,
        occurred_at_millis: row.get(3)?,
        workspace_id: row.get(4)?,
        project_id: row.get(5)?,
        repository_id: row.get(6)?,
        retention_kind: row.get(7)?,
        retention_until_millis: row.get(8)?,
        previous_digest: row.get::<_, Option<String>>(9)?.map(Sha256Digest),
        event_digest: Sha256Digest(row.get(10)?),
        payload_digest: Sha256Digest(row.get(11)?),
        payload: row.get(12)?,
        payload_pruned_at_millis: row.get(13)?,
        tombstone_event_digest: row.get::<_, Option<String>>(14)?.map(Sha256Digest),
    })
}

fn apply_schema(connection: &mut Connection) -> Result<(), AuditError> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(sql_error)?;
    if !matches!(version, 0 | SCHEMA_VERSION) {
        return Err(AuditError::adapter(format!(
            "unsupported audit schema version {version}"
        )));
    }
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS audit_chain_heads (
                 organization_id TEXT PRIMARY KEY NOT NULL,
                 last_sequence INTEGER NOT NULL CHECK (last_sequence > 0),
                 last_digest TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS audit_events (
                 organization_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 event_id TEXT UNIQUE NOT NULL,
                 occurred_at_millis INTEGER NOT NULL CHECK (occurred_at_millis > 0),
                 workspace_id TEXT,
                 project_id TEXT,
                 repository_id TEXT,
                 retention_kind TEXT NOT NULL CHECK (retention_kind IN ('until', 'indefinite')),
                 retention_until_millis INTEGER,
                 previous_digest TEXT,
                 event_digest TEXT NOT NULL,
                 payload_digest TEXT NOT NULL,
                 PRIMARY KEY (organization_id, sequence),
                 CHECK ((retention_kind = 'until') = (retention_until_millis IS NOT NULL)),
                 CHECK (project_id IS NULL OR workspace_id IS NOT NULL),
                 CHECK (repository_id IS NULL OR project_id IS NOT NULL)
             );
             CREATE TABLE IF NOT EXISTS audit_payloads (
                 organization_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL,
                 payload BLOB NOT NULL,
                 PRIMARY KEY (organization_id, sequence),
                 FOREIGN KEY (organization_id, sequence)
                   REFERENCES audit_events (organization_id, sequence)
             );
             CREATE TABLE IF NOT EXISTS audit_payload_tombstones (
                 organization_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL,
                 pruned_at_millis INTEGER NOT NULL CHECK (pruned_at_millis > 0),
                 event_digest TEXT NOT NULL,
                 PRIMARY KEY (organization_id, sequence),
                 FOREIGN KEY (organization_id, sequence)
                   REFERENCES audit_events (organization_id, sequence)
             );
             CREATE INDEX IF NOT EXISTS audit_scope_sequence
               ON audit_events (organization_id, workspace_id, project_id, repository_id, sequence);
             CREATE TRIGGER IF NOT EXISTS audit_events_no_update
               BEFORE UPDATE ON audit_events BEGIN
                 SELECT RAISE(ABORT, 'audit event headers are immutable');
               END;
             CREATE TRIGGER IF NOT EXISTS audit_events_no_delete
               BEFORE DELETE ON audit_events BEGIN
                 SELECT RAISE(ABORT, 'audit event headers are immutable');
               END;
             CREATE TRIGGER IF NOT EXISTS audit_payload_tombstones_no_update
               BEFORE UPDATE ON audit_payload_tombstones BEGIN
                 SELECT RAISE(ABORT, 'audit retention tombstones are immutable');
               END;
             CREATE TRIGGER IF NOT EXISTS audit_payload_tombstones_no_delete
               BEFORE DELETE ON audit_payload_tombstones BEGIN
                 SELECT RAISE(ABORT, 'audit retention tombstones are immutable');
               END;",
        )
        .map_err(sql_error)?;
    transaction
        .pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(sql_error)?;
    transaction.commit().map_err(sql_error)
}

fn validate_organization_id(organization_id: &OrganizationId) -> Result<(), AuditError> {
    let valid = organization_id
        .0
        .strip_prefix("org_")
        .is_some_and(|suffix| {
            suffix.len() == 26
                && suffix.bytes().all(|byte| {
                    byte.is_ascii_digit()
                        || matches!(
                            byte,
                            b'A'..=b'H'
                                | b'J'..=b'K'
                                | b'M'..=b'N'
                                | b'P'..=b'T'
                                | b'V'..=b'Z'
                        )
                })
        });
    if valid {
        Ok(())
    } else {
        Err(AuditError::invalid(
            "audit organization identity is not canonical",
        ))
    }
}

fn sha256_digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn sql_error(error: rusqlite::Error) -> AuditError {
    let message = format!("SQLite audit operation failed: {error}");
    drop(error);
    AuditError::adapter(message)
}

fn json_error(error: serde_json::Error) -> AuditError {
    let message = format!("failed to encode canonical audit event: {error}");
    drop(error);
    AuditError::adapter(message)
}
