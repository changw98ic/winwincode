// SPDX-License-Identifier: Apache-2.0

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::{SqliteStorage, StorageError, sql_error};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateApplicationRecord {
    pub application_id: String,
    pub approval_id: String,
    pub client_node_id: String,
    pub local_candidate_receipt_id: String,
    pub repository_binding_id: String,
    pub candidate_ref: String,
    pub candidate_digest: String,
    pub base_commit: String,
    pub target_branch: String,
    pub strategy: String,
    pub original_lease_id: String,
    pub original_epoch: u64,
    pub requester_user_id: String,
    pub created_at: String,
    pub result_json: Option<String>,
    pub outcome: Option<String>,
    pub retry_lease_id: Option<String>,
    pub retry_epoch: Option<u64>,
}

pub(crate) fn create_schema(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS candidate_applications (
             application_id TEXT PRIMARY KEY NOT NULL,
             approval_id TEXT NOT NULL UNIQUE,
             client_node_id TEXT NOT NULL,
             local_candidate_receipt_id TEXT NOT NULL,
             repository_binding_id TEXT NOT NULL,
             candidate_ref TEXT NOT NULL,
             candidate_digest TEXT NOT NULL,
             base_commit TEXT NOT NULL,
             target_branch TEXT NOT NULL,
             strategy TEXT NOT NULL,
             original_lease_id TEXT NOT NULL,
             original_epoch INTEGER NOT NULL CHECK (original_epoch >= 0),
             requester_user_id TEXT NOT NULL,
             created_at TEXT NOT NULL,
             result_json TEXT,
             outcome TEXT,
             retry_lease_id TEXT,
             retry_epoch INTEGER,
             UNIQUE (client_node_id, local_candidate_receipt_id, base_commit, target_branch, strategy)
         );
         CREATE TABLE IF NOT EXISTS candidate_application_events (
             event_id INTEGER PRIMARY KEY AUTOINCREMENT,
             application_id TEXT NOT NULL,
             kind TEXT NOT NULL,
             epoch INTEGER NOT NULL,
             payload TEXT NOT NULL,
             occurred_at TEXT NOT NULL,
             UNIQUE (application_id, kind, epoch, payload)
         );"
    ).map_err(sql_error)
}

pub(crate) fn validate_schema(connection: &rusqlite::Connection) -> Result<(), StorageError> {
    for query in [
        "SELECT application_id, approval_id, client_node_id, local_candidate_receipt_id, repository_binding_id, candidate_ref, candidate_digest, base_commit, target_branch, strategy, original_lease_id, original_epoch, requester_user_id, created_at, result_json, outcome, retry_lease_id, retry_epoch FROM candidate_applications LIMIT 0",
        "SELECT event_id, application_id, kind, epoch, payload, occurred_at FROM candidate_application_events LIMIT 0",
    ] {
        connection.prepare(query).map_err(sql_error)?;
    }
    Ok(())
}

impl SqliteStorage {
    pub fn candidate_application_blocked(
        &self,
        node: &str,
        candidate_ref: &str,
    ) -> Result<bool, StorageError> {
        self.connection()?.query_row("SELECT EXISTS(SELECT 1 FROM candidate_applications WHERE client_node_id = ?1 AND candidate_ref = ?2 AND outcome = 'outcome_unknown' AND result_json IS NULL)",
            params![node, candidate_ref], |row| row.get(0)).map_err(sql_error)
    }
    pub fn grant_candidate_application(
        &mut self,
        record: &CandidateApplicationRecord,
    ) -> Result<CandidateApplicationRecord, StorageError> {
        let transaction = self
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let record = grant_in_transaction(&transaction, record)?;
        transaction.commit().map_err(sql_error)?;
        Ok(record)
    }

    pub fn publish_candidate_application(
        &mut self,
        proposed: &CandidateApplicationRecord,
        lease_id: &str,
        epoch: u64,
        now: &winwincode_domain::Instant,
        build_downlink: impl FnOnce(
            &CandidateApplicationRecord,
            u64,
        ) -> Result<crate::ClientDownlinkAppend, StorageError>,
    ) -> Result<CandidateApplicationRecord, StorageError> {
        self.client_downlink_outbox()
            .map_err(|_| StorageError::adapter("candidate downlink ledger unavailable"))?;
        let transaction = self
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let mut record = grant_in_transaction(&transaction, proposed)?;
        if record.outcome.as_deref() == Some("not_started") {
            authorize_retry_in_transaction(&transaction, &record.application_id, lease_id, epoch)?;
            record.outcome = Some("retry_authorized".into());
            record.retry_lease_id = Some(lease_id.into());
            record.retry_epoch = Some(epoch);
        }
        let sequence = crate::client_downlink::next_sequence_in_transaction(
            &transaction,
            &record.client_node_id,
        )
        .map_err(|_| StorageError::adapter("candidate downlink sequence unavailable"))?;
        let append = build_downlink(&record, sequence)?;
        if append.client_node_id() != record.client_node_id {
            return Err(StorageError::invalid(
                "candidate publication authority differs",
            ));
        }
        crate::client_downlink::append_launch_in_transaction(&transaction, &append, now)
            .map_err(|_| StorageError::adapter("candidate downlink publication failed"))?;
        transaction.commit().map_err(sql_error)?;
        Ok(record)
    }

    pub fn candidate_application(
        &self,
        application_id: &str,
    ) -> Result<Option<CandidateApplicationRecord>, StorageError> {
        load(self.connection()?, application_id)
    }

    pub fn candidate_application_for_candidate(
        &self,
        client_node_id: &str,
        candidate_ref: &str,
    ) -> Result<Option<CandidateApplicationRecord>, StorageError> {
        let id = self.connection()?.query_row(
            "SELECT application_id FROM candidate_applications WHERE client_node_id = ?1 AND candidate_ref = ?2 ORDER BY created_at DESC, application_id DESC LIMIT 1",
            params![client_node_id, candidate_ref], |row| row.get::<_, String>(0),
        ).optional().map_err(sql_error)?;
        id.map(|id| load(self.connection()?, &id))
            .transpose()
            .map(Option::flatten)
    }

    pub fn record_candidate_application_fact(
        &mut self,
        application_id: &str,
        kind: &str,
        epoch: u64,
        payload: &str,
        occurred_at: &str,
    ) -> Result<(), StorageError> {
        if self.candidate_application(application_id)?.is_none() {
            return Err(StorageError::invalid(
                "application fact has no approved application",
            ));
        }
        self.connection_mut()?
            .execute(
                "INSERT OR IGNORE INTO candidate_application_events \
             (application_id, kind, epoch, payload, occurred_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    application_id,
                    kind,
                    sql_epoch(epoch)?,
                    payload,
                    occurred_at
                ],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    pub fn settle_candidate_application(
        &mut self,
        application_id: &str,
        outcome: &str,
        result_json: &str,
    ) -> Result<(), StorageError> {
        let updated = self.connection_mut()?.execute(
            "UPDATE candidate_applications SET outcome = ?2, result_json = ?3 \
             WHERE application_id = ?1 AND (result_json IS NULL OR (result_json = ?3 AND outcome = ?2))",
            params![application_id, outcome, result_json],
        ).map_err(sql_error)?;
        if updated != 1 {
            return Err(StorageError::invalid(
                "application result conflicts with a settled application",
            ));
        }
        Ok(())
    }

    pub fn note_candidate_application_outcome(
        &mut self,
        application_id: &str,
        outcome: &str,
    ) -> Result<(), StorageError> {
        if !matches!(outcome, "not_started" | "outcome_unknown") {
            return Err(StorageError::invalid(
                "application recovery outcome is invalid",
            ));
        }
        if self
            .candidate_application(application_id)?
            .is_some_and(|record| {
                record.outcome.as_deref() == Some("outcome_unknown") && outcome != "outcome_unknown"
            })
        {
            return Err(StorageError::invalid(
                "an unknown application outcome requires a user decision",
            ));
        }
        self.connection_mut()?.execute(
            "UPDATE candidate_applications SET outcome = ?2 WHERE application_id = ?1 AND result_json IS NULL",
            params![application_id, outcome],
        ).map_err(sql_error)?;
        Ok(())
    }

    pub fn authorize_candidate_application_retry(
        &mut self,
        application_id: &str,
        lease_id: &str,
        epoch: u64,
    ) -> Result<(), StorageError> {
        authorize_retry_in_transaction(self.connection_mut()?, application_id, lease_id, epoch)
    }
}

#[allow(clippy::too_many_lines)]
fn grant_in_transaction(
    transaction: &rusqlite::Connection,
    record: &CandidateApplicationRecord,
) -> Result<CandidateApplicationRecord, StorageError> {
    if record.application_id.is_empty()
        || record.approval_id.is_empty()
        || record.candidate_digest.is_empty()
        || record.result_json.is_some()
        || record.outcome.is_some()
    {
        return Err(StorageError::invalid(
            "candidate application approval is invalid",
        ));
    }
    let blocked: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM candidate_applications WHERE client_node_id = ?1 AND candidate_ref = ?2 AND outcome = 'outcome_unknown' AND result_json IS NULL)",
        params![record.client_node_id, record.candidate_ref], |row| row.get(0)).map_err(sql_error)?;
    if blocked {
        return Err(StorageError::invalid(
            "an unresolved application blocks another apply for this candidate",
        ));
    }
    let candidate = transaction.query_row(
        "SELECT client_node_id, repository_binding_id, candidate_ref, base_commit FROM local_candidate_receipts WHERE local_candidate_receipt_id = ?1",
        [&record.local_candidate_receipt_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?)),
    ).optional().map_err(sql_error)?;
    if candidate
        .as_ref()
        .is_none_or(|(node, binding, candidate_ref, base)| {
            node != &record.client_node_id
                || binding != &record.repository_binding_id
                || candidate_ref != &record.candidate_ref
                || base != &record.base_commit
        })
    {
        return Err(StorageError::invalid(
            "application approval differs from the frozen candidate source binding",
        ));
    }
    let approved_base = transaction.query_row(
        "SELECT base_commit FROM candidate_applications WHERE client_node_id = ?1 AND local_candidate_receipt_id = ?2 LIMIT 1",
        params![record.client_node_id, record.local_candidate_receipt_id],
        |row| row.get::<_, String>(0),
    ).optional().map_err(sql_error)?;
    if approved_base.is_some_and(|base| base != record.base_commit) {
        return Err(StorageError::invalid(
            "a stale candidate requires regeneration, verification and a new approval",
        ));
    }
    let existing = transaction.query_row(
        "SELECT application_id FROM candidate_applications WHERE client_node_id = ?1 \
         AND local_candidate_receipt_id = ?2 AND base_commit = ?3 AND target_branch = ?4 AND strategy = ?5",
        params![record.client_node_id, record.local_candidate_receipt_id,
            record.base_commit, record.target_branch, record.strategy],
        |row| row.get::<_, String>(0),
    ).optional().map_err(sql_error)?;
    if let Some(id) = existing {
        let existing = load(transaction, &id)?
            .ok_or_else(|| StorageError::invalid("application approval disappeared"))?;
        if existing.repository_binding_id != record.repository_binding_id
            || existing.candidate_ref != record.candidate_ref
            || existing.candidate_digest != record.candidate_digest
            || existing.requester_user_id != record.requester_user_id
        {
            return Err(StorageError::invalid(
                "candidate application approval binding conflicts",
            ));
        }
        return Ok(existing);
    }
    transaction.execute(
        "INSERT INTO candidate_applications \
         (application_id, approval_id, client_node_id, local_candidate_receipt_id, repository_binding_id, \
          candidate_ref, candidate_digest, base_commit, target_branch, strategy, original_lease_id, \
          original_epoch, requester_user_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![record.application_id, record.approval_id, record.client_node_id,
            record.local_candidate_receipt_id, record.repository_binding_id, record.candidate_ref,
            record.candidate_digest, record.base_commit, record.target_branch, record.strategy,
            record.original_lease_id, sql_epoch(record.original_epoch)?, record.requester_user_id, record.created_at],
    ).map_err(sql_error)?;
    transaction.execute(
        "INSERT INTO candidate_application_events (application_id, kind, epoch, payload, occurred_at) \
         VALUES (?1, 'approval.granted', ?2, ?3, ?4)",
        params![record.application_id, sql_epoch(record.original_epoch)?, record.candidate_digest, record.created_at],
    ).map_err(sql_error)?;
    Ok(record.clone())
}

fn authorize_retry_in_transaction(
    connection: &rusqlite::Connection,
    application_id: &str,
    lease_id: &str,
    epoch: u64,
) -> Result<(), StorageError> {
    let changed = connection.execute(
        "UPDATE candidate_applications SET retry_lease_id = ?2, retry_epoch = ?3, outcome = 'retry_authorized' \
         WHERE application_id = ?1 AND outcome = 'not_started' AND result_json IS NULL",
            params![application_id, lease_id, sql_epoch(epoch)?],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(StorageError::invalid(
            "application has no confirmed retryable recovery outcome",
        ));
    }
    Ok(())
}

fn load(
    connection: &rusqlite::Connection,
    id: &str,
) -> Result<Option<CandidateApplicationRecord>, StorageError> {
    connection.query_row(
        "SELECT application_id, approval_id, client_node_id, local_candidate_receipt_id, repository_binding_id, \
         candidate_ref, candidate_digest, base_commit, target_branch, strategy, original_lease_id, original_epoch, \
         requester_user_id, created_at, result_json, outcome, retry_lease_id, retry_epoch FROM candidate_applications WHERE application_id = ?1",
        [id], |row| Ok(CandidateApplicationRecord {
            application_id: row.get(0)?, approval_id: row.get(1)?, client_node_id: row.get(2)?,
            local_candidate_receipt_id: row.get(3)?, repository_binding_id: row.get(4)?,
            candidate_ref: row.get(5)?, candidate_digest: row.get(6)?, base_commit: row.get(7)?,
            target_branch: row.get(8)?, strategy: row.get(9)?, original_lease_id: row.get(10)?,
            original_epoch: read_epoch(row, 11)?, requester_user_id: row.get(12)?, created_at: row.get(13)?,
            result_json: row.get(14)?, outcome: row.get(15)?,
            retry_lease_id: row.get(16)?, retry_epoch: row.get::<_, Option<i64>>(17)?.map(|value| parse_epoch(value, 17)).transpose()?,
        }),
    ).optional().map_err(sql_error)
}

fn sql_epoch(epoch: u64) -> Result<i64, StorageError> {
    i64::try_from(epoch)
        .map_err(|_| StorageError::invalid("application epoch exceeds SQLite integer range"))
}

fn read_epoch(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    parse_epoch(row.get(index)?, index)
}

fn parse_epoch(value: i64, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}
