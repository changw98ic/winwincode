// SPDX-License-Identifier: Apache-2.0

//! Community `SQLite` storage for the storage-neutral observability core.

use std::{fs, path::Path, time::Duration};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use winwincode_observability_core::{
    AlertPage, AlertRule, AlertState, AlertStatus, AlertTransition, MetricCursor, MetricDelta,
    MetricPage, MetricRow, MetricSeriesKey, ObservabilityConfig, ObservabilityError,
    ObservabilityStore, Observation, ObservationReceipt, TraceId, TracePage, TraceRow,
    evaluate_alert_rule, safe_integer, validate_alert_transition, validate_bucket_window,
    validate_limit,
};

const SCHEMA_VERSION: &str = "winwincode.observability.sqlite.v1";

/// SQLite-backed telemetry service. Each operation is bounded and owns its
/// transaction; no query returns a live cursor or holds a lock after return.
pub struct SqliteObservability {
    connection: Connection,
    config: ObservabilityConfig,
}

impl SqliteObservability {
    /// Opens or creates a durable observability database in WAL mode.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe error for invalid configuration, inaccessible
    /// storage, schema mismatch, or an alert rule set that changed after the
    /// database was initialized.
    pub fn open(
        path: impl AsRef<Path>,
        mut config: ObservabilityConfig,
    ) -> Result<Self, ObservabilityError> {
        config.validate_and_normalize()?;
        let path = path.as_ref();
        prepare_parent(path)?;
        let mut connection = Connection::open(path).map_err(|_| ObservabilityError::storage())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|_| ObservabilityError::storage())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 PRAGMA foreign_keys=ON;
                 PRAGMA trusted_schema=OFF;",
            )
            .map_err(|_| ObservabilityError::storage())?;
        protect_database(path)?;
        initialize_schema(&connection)?;
        validate_metadata(&mut connection, &config)?;
        validate_metric_counter(&connection)?;
        Ok(Self { connection, config })
    }

    /// Atomically records a structured observation, updates its fixed metric
    /// bucket, and evaluates durable alert state.
    ///
    /// Exact replay returns the original transition set without updating
    /// metrics or alerts. Reusing an observation or source identity with
    /// changed facts fails before any write.
    ///
    /// # Errors
    ///
    /// Returns a stable error for invalid input, replay conflict, configured
    /// receipt exhaustion, corrupt durable rows, or storage failure.
    pub fn record(
        &mut self,
        observation: &Observation,
    ) -> Result<ObservationReceipt, ObservabilityError> {
        observation.validate()?;
        observation.validate_for_config(&self.config)?;
        let observation_json =
            serde_json::to_vec(observation).map_err(|_| ObservabilityError::invalid())?;
        let body_digest = sha256(&observation_json);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ObservabilityError::storage())?;
        if let Some(receipt) = replay_receipt(&transaction, observation, &body_digest)? {
            transaction
                .commit()
                .map_err(|_| ObservabilityError::storage())?;
            return Ok(receipt);
        }
        reject_source_reuse(&transaction, observation)?;
        enforce_receipt_bound(&transaction, self.config.max_receipts)?;

        let sequence = insert_receipt(&transaction, observation, &body_digest)?;
        insert_trace_row(&transaction, sequence, observation, &observation_json)?;
        update_metric(&transaction, &self.config, observation)?;
        let transitions = evaluate_alerts(&transaction, &self.config.alert_rules, observation)?;
        store_receipt_transitions(&transaction, sequence, &transitions)?;
        trim_trace_rows(&transaction, self.config.max_trace_rows, sequence)?;
        trim_metric_rows(&transaction, self.config.max_metric_rows)?;
        transaction
            .commit()
            .map_err(|_| ObservabilityError::storage())?;
        Ok(ObservationReceipt {
            accepted_sequence: sequence,
            duplicate: false,
            alert_transitions: transitions,
        })
    }

    /// Loads one bounded materialized trace page.
    ///
    /// # Errors
    ///
    /// Rejects malformed trace identities, zero or oversized limits, and
    /// corrupt retained observations.
    pub fn trace_page(
        &self,
        trace_id: &TraceId,
        after_sequence: u64,
        limit: u32,
    ) -> Result<TracePage, ObservabilityError> {
        TraceId::try_new(trace_id.as_str().to_owned())?;
        validate_limit(limit, self.config.max_query_rows)?;
        safe_integer(after_sequence)?;
        let fetch_limit = i64::from(limit) + 1;
        let mut statement = self
            .connection
            .prepare(
                "SELECT sequence, observation_json
                 FROM observation_log
                 WHERE trace_id = ?1 AND sequence > ?2
                 ORDER BY sequence ASC
                 LIMIT ?3",
            )
            .map_err(|_| ObservabilityError::storage())?;
        let mut rows = statement
            .query(params![
                trace_id.as_str(),
                to_i64(after_sequence)?,
                fetch_limit
            ])
            .map_err(|_| ObservabilityError::storage())?;
        let mut result = Vec::with_capacity(
            usize::try_from(limit).map_err(|_| ObservabilityError::limit())? + 1,
        );
        while let Some(row) = rows.next().map_err(|_| ObservabilityError::storage())? {
            let sequence = from_i64(row.get(0).map_err(|_| ObservabilityError::corrupt())?)?;
            let json: Vec<u8> = row.get(1).map_err(|_| ObservabilityError::corrupt())?;
            let observation: Observation =
                serde_json::from_slice(&json).map_err(|_| ObservabilityError::corrupt())?;
            observation
                .validate()
                .map_err(|_| ObservabilityError::corrupt())?;
            result.push(TraceRow {
                sequence,
                observation,
            });
        }
        let has_more =
            result.len() > usize::try_from(limit).map_err(|_| ObservabilityError::limit())?;
        if has_more {
            result.pop();
        }
        let next_after_sequence = has_more
            .then(|| result.last().map(|row| row.sequence))
            .flatten();
        Ok(TracePage {
            rows: result,
            next_after_sequence,
        })
    }

    /// Loads a bounded metric page over an explicitly bounded bucket window.
    ///
    /// # Errors
    ///
    /// Rejects non-aligned or oversized windows, malformed cursors, oversized
    /// pages, and corrupt metric rows.
    pub fn metric_page(
        &self,
        from_bucket_inclusive: u64,
        to_bucket_exclusive: u64,
        after: Option<&MetricCursor>,
        limit: u32,
    ) -> Result<MetricPage, ObservabilityError> {
        validate_limit(limit, self.config.max_query_rows)?;
        validate_bucket_window(&self.config, from_bucket_inclusive, to_bucket_exclusive)?;
        let (after_bucket, after_key) = match after {
            Some(cursor) => {
                if cursor.bucket_start_unix_millis < from_bucket_inclusive
                    || cursor.bucket_start_unix_millis >= to_bucket_exclusive
                {
                    return Err(ObservabilityError::invalid());
                }
                cursor.key.validate()?;
                (
                    cursor.bucket_start_unix_millis,
                    serde_json::to_string(&cursor.key)
                        .map_err(|_| ObservabilityError::invalid())?,
                )
            }
            None => (from_bucket_inclusive, String::new()),
        };
        let mut statement = self
            .connection
            .prepare(
                "SELECT bucket_start_millis, series_key, observations,
                        latency_total_millis, latency_max_millis, recovered_items,
                        latest_used, latest_limit, maximum_used
                 FROM metric_series
                 WHERE bucket_start_millis >= ?1 AND bucket_start_millis < ?2
                   AND (bucket_start_millis > ?3 OR
                        (bucket_start_millis = ?3 AND series_key > ?4))
                 ORDER BY bucket_start_millis ASC, series_key ASC
                 LIMIT ?5",
            )
            .map_err(|_| ObservabilityError::storage())?;
        let fetch_limit = i64::from(limit) + 1;
        let mut rows = statement
            .query(params![
                to_i64(from_bucket_inclusive)?,
                to_i64(to_bucket_exclusive)?,
                to_i64(after_bucket)?,
                after_key,
                fetch_limit
            ])
            .map_err(|_| ObservabilityError::storage())?;
        let mut result = Vec::with_capacity(
            usize::try_from(limit).map_err(|_| ObservabilityError::limit())? + 1,
        );
        while let Some(row) = rows.next().map_err(|_| ObservabilityError::storage())? {
            result.push(decode_metric_row(row)?);
        }
        let has_more =
            result.len() > usize::try_from(limit).map_err(|_| ObservabilityError::limit())?;
        if has_more {
            result.pop();
        }
        let next = has_more
            .then(|| {
                result.last().map(|row| MetricCursor {
                    bucket_start_unix_millis: row.bucket_start_unix_millis,
                    key: row.key.clone(),
                })
            })
            .flatten();
        Ok(MetricPage { rows: result, next })
    }

    /// Loads a bounded alert-transition page.
    ///
    /// # Errors
    ///
    /// Rejects oversized limits and corrupt durable transitions.
    pub fn alert_page(
        &self,
        after_sequence: u64,
        limit: u32,
    ) -> Result<AlertPage, ObservabilityError> {
        validate_limit(limit, self.config.max_query_rows)?;
        safe_integer(after_sequence)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT transition_json
                 FROM alert_transitions
                 WHERE sequence > ?1
                 ORDER BY sequence ASC
                 LIMIT ?2",
            )
            .map_err(|_| ObservabilityError::storage())?;
        let fetch_limit = i64::from(limit) + 1;
        let mut rows = statement
            .query(params![to_i64(after_sequence)?, fetch_limit])
            .map_err(|_| ObservabilityError::storage())?;
        let mut result = Vec::with_capacity(
            usize::try_from(limit).map_err(|_| ObservabilityError::limit())? + 1,
        );
        while let Some(row) = rows.next().map_err(|_| ObservabilityError::storage())? {
            let json: Vec<u8> = row.get(0).map_err(|_| ObservabilityError::corrupt())?;
            let transition: AlertTransition =
                serde_json::from_slice(&json).map_err(|_| ObservabilityError::corrupt())?;
            validate_alert_transition(&transition)?;
            result.push(transition);
        }
        let has_more =
            result.len() > usize::try_from(limit).map_err(|_| ObservabilityError::limit())?;
        if has_more {
            result.pop();
        }
        let next_after_sequence = has_more
            .then(|| result.last().map(|row| row.sequence))
            .flatten();
        Ok(AlertPage {
            transitions: result,
            next_after_sequence,
        })
    }
}

fn initialize_schema(connection: &Connection) -> Result<(), ObservabilityError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS observability_metadata (
                 key TEXT PRIMARY KEY NOT NULL,
                 value BLOB NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS observation_receipts (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 observation_id TEXT UNIQUE NOT NULL,
                 body_digest TEXT NOT NULL,
                 source_kind TEXT NOT NULL,
                 source_id TEXT NOT NULL,
                 source_digest TEXT NOT NULL,
                 alert_transitions_json BLOB NOT NULL,
                 UNIQUE(source_kind, source_id)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS observation_log (
                 sequence INTEGER PRIMARY KEY NOT NULL,
                 trace_id TEXT NOT NULL,
                 occurred_at_millis INTEGER NOT NULL,
                 observation_json BLOB NOT NULL,
                 FOREIGN KEY(sequence) REFERENCES observation_receipts(sequence)
             ) STRICT;
             CREATE INDEX IF NOT EXISTS observation_log_trace_sequence
                 ON observation_log(trace_id, sequence);
             CREATE TABLE IF NOT EXISTS metric_series (
                 bucket_start_millis INTEGER NOT NULL,
                 series_key TEXT NOT NULL,
                 observations INTEGER NOT NULL,
                 latency_total_millis INTEGER NOT NULL,
                 latency_max_millis INTEGER NOT NULL,
                 recovered_items INTEGER NOT NULL,
                 latest_used INTEGER NOT NULL,
                 latest_limit INTEGER NOT NULL,
                 maximum_used INTEGER NOT NULL,
                 PRIMARY KEY(bucket_start_millis, series_key)
             ) STRICT, WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS alert_states (
                 rule_id TEXT PRIMARY KEY NOT NULL,
                 status TEXT NOT NULL,
                 generation INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS alert_transitions (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 rule_id TEXT NOT NULL,
                 generation INTEGER NOT NULL,
                 status TEXT NOT NULL,
                 transition_json BLOB NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS observability_counters (
                 key TEXT PRIMARY KEY NOT NULL,
                 value INTEGER NOT NULL CHECK(value >= 0)
             ) STRICT;
             INSERT OR IGNORE INTO observability_counters(key, value)
                 VALUES ('metric_rows', 0);",
        )
        .map_err(|_| ObservabilityError::storage())
}

fn validate_metadata(
    connection: &mut Connection,
    config: &ObservabilityConfig,
) -> Result<(), ObservabilityError> {
    let rule_json =
        serde_json::to_vec(&config.alert_rules).map_err(|_| ObservabilityError::invalid())?;
    let rules_digest = sha256(&rule_json);
    let bucket_width = config.bucket_width_millis.to_string();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| ObservabilityError::storage())?;
    let existing_schema: Option<Vec<u8>> = transaction
        .query_row(
            "SELECT value FROM observability_metadata WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| ObservabilityError::storage())?;
    let existing_rules: Option<Vec<u8>> = transaction
        .query_row(
            "SELECT value FROM observability_metadata WHERE key = 'alert_rules_digest'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| ObservabilityError::storage())?;
    let existing_bucket_width: Option<Vec<u8>> = transaction
        .query_row(
            "SELECT value FROM observability_metadata WHERE key = 'bucket_width_millis'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| ObservabilityError::storage())?;
    let validation = match (existing_schema, existing_rules, existing_bucket_width) {
        (None, None, None) => {
            transaction
                .execute(
                    "INSERT INTO observability_metadata(key, value) VALUES ('schema_version', ?1)",
                    [SCHEMA_VERSION.as_bytes()],
                )
                .map_err(|_| ObservabilityError::storage())?;
            transaction
                .execute(
                    "INSERT INTO observability_metadata(key, value) VALUES ('alert_rules_digest', ?1)",
                    [rules_digest.as_bytes()],
                )
                .map_err(|_| ObservabilityError::storage())?;
            transaction
                .execute(
                    "INSERT INTO observability_metadata(key, value) VALUES ('bucket_width_millis', ?1)",
                    [bucket_width.as_bytes()],
                )
                .map_err(|_| ObservabilityError::storage())?;
            Ok(())
        }
        (Some(schema), Some(rules), Some(stored_bucket_width))
            if schema == SCHEMA_VERSION.as_bytes() =>
        {
            if rules != rules_digest.as_bytes() {
                Err(ObservabilityError::rule_set_changed())
            } else if stored_bucket_width != bucket_width.as_bytes() {
                Err(ObservabilityError::configuration_changed())
            } else {
                Ok(())
            }
        }
        _ => Err(ObservabilityError::corrupt()),
    };
    validation?;
    transaction
        .commit()
        .map_err(|_| ObservabilityError::storage())
}

fn validate_metric_counter(connection: &Connection) -> Result<(), ObservabilityError> {
    let stored: i64 = connection
        .query_row(
            "SELECT value FROM observability_counters WHERE key = 'metric_rows'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| ObservabilityError::corrupt())?;
    let actual: i64 = connection
        .query_row("SELECT COUNT(*) FROM metric_series", [], |row| row.get(0))
        .map_err(|_| ObservabilityError::corrupt())?;
    if stored == actual {
        Ok(())
    } else {
        Err(ObservabilityError::corrupt())
    }
}

fn replay_receipt(
    transaction: &Transaction<'_>,
    observation: &Observation,
    body_digest: &str,
) -> Result<Option<ObservationReceipt>, ObservabilityError> {
    let stored: Option<(String, i64, Vec<u8>)> = transaction
        .query_row(
            "SELECT body_digest, sequence, alert_transitions_json
             FROM observation_receipts WHERE observation_id = ?1",
            [observation.observation_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|_| ObservabilityError::storage())?;
    let Some((stored_digest, sequence, transitions_json)) = stored else {
        return Ok(None);
    };
    if stored_digest != body_digest {
        return Err(ObservabilityError::conflict());
    }
    let transitions: Vec<AlertTransition> =
        serde_json::from_slice(&transitions_json).map_err(|_| ObservabilityError::corrupt())?;
    for transition in &transitions {
        validate_alert_transition(transition)?;
    }
    Ok(Some(ObservationReceipt {
        accepted_sequence: from_i64(sequence)?,
        duplicate: true,
        alert_transitions: transitions,
    }))
}

fn reject_source_reuse(
    transaction: &Transaction<'_>,
    observation: &Observation,
) -> Result<(), ObservabilityError> {
    let source_kind = serde_json::to_string(&observation.source.kind)
        .map_err(|_| ObservabilityError::invalid())?;
    let existing: Option<(String, String)> = transaction
        .query_row(
            "SELECT observation_id, source_digest FROM observation_receipts
             WHERE source_kind = ?1 AND source_id = ?2",
            params![source_kind, observation.source.fact_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| ObservabilityError::storage())?;
    if existing.is_some() {
        return Err(ObservabilityError::conflict());
    }
    Ok(())
}

fn enforce_receipt_bound(
    transaction: &Transaction<'_>,
    max_receipts: u64,
) -> Result<(), ObservabilityError> {
    let highest_sequence: i64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM observation_receipts",
            [],
            |row| row.get(0),
        )
        .map_err(|_| ObservabilityError::storage())?;
    if from_i64(highest_sequence)? >= max_receipts {
        return Err(ObservabilityError::limit());
    }
    Ok(())
}

fn insert_receipt(
    transaction: &Transaction<'_>,
    observation: &Observation,
    body_digest: &str,
) -> Result<u64, ObservabilityError> {
    let source_kind = serde_json::to_string(&observation.source.kind)
        .map_err(|_| ObservabilityError::invalid())?;
    transaction
        .execute(
            "INSERT INTO observation_receipts(
                 observation_id, body_digest, source_kind, source_id, source_digest,
                 alert_transitions_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, X'5B5D')",
            params![
                observation.observation_id.as_str(),
                body_digest,
                source_kind,
                observation.source.fact_id.as_str(),
                observation.source.fact_digest.as_str()
            ],
        )
        .map_err(|_| ObservabilityError::storage())?;
    from_i64(transaction.last_insert_rowid())
}

fn insert_trace_row(
    transaction: &Transaction<'_>,
    sequence: u64,
    observation: &Observation,
    observation_json: &[u8],
) -> Result<(), ObservabilityError> {
    transaction
        .execute(
            "INSERT INTO observation_log(
                 sequence, trace_id, occurred_at_millis, observation_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                to_i64(sequence)?,
                observation.trace.trace_id.as_str(),
                to_i64(observation.occurred_at_unix_millis)?,
                observation_json
            ],
        )
        .map_err(|_| ObservabilityError::storage())?;
    Ok(())
}

fn update_metric(
    transaction: &Transaction<'_>,
    config: &ObservabilityConfig,
    observation: &Observation,
) -> Result<(), ObservabilityError> {
    let bucket_start = observation.occurred_at_unix_millis
        - observation.occurred_at_unix_millis % config.bucket_width_millis;
    let update = MetricDelta::try_from_observation(observation)?;
    let key_json = serde_json::to_string(&update.key).map_err(|_| ObservabilityError::invalid())?;
    let existed: bool = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM metric_series
                 WHERE bucket_start_millis = ?1 AND series_key = ?2
             )",
            params![to_i64(bucket_start)?, key_json],
            |row| row.get(0),
        )
        .map_err(|_| ObservabilityError::storage())?;
    transaction
        .execute(
            "INSERT INTO metric_series(
                 bucket_start_millis, series_key, observations,
                 latency_total_millis, latency_max_millis, recovered_items,
                 latest_used, latest_limit, maximum_used
             ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(bucket_start_millis, series_key) DO UPDATE SET
                 observations = observations + 1,
                 latency_total_millis = latency_total_millis + excluded.latency_total_millis,
                 latency_max_millis = MAX(latency_max_millis, excluded.latency_max_millis),
                 recovered_items = recovered_items + excluded.recovered_items,
                 latest_used = excluded.latest_used,
                 latest_limit = excluded.latest_limit,
                 maximum_used = MAX(maximum_used, excluded.maximum_used)",
            params![
                to_i64(bucket_start)?,
                key_json,
                to_i64(update.latency_millis)?,
                to_i64(update.latency_millis)?,
                to_i64(update.recovered_items)?,
                to_i64(update.used)?,
                to_i64(update.limit)?,
                to_i64(update.used)?
            ],
        )
        .map_err(|_| ObservabilityError::storage())?;
    if !existed {
        let changed = transaction
            .execute(
                "UPDATE observability_counters SET value = value + 1
                 WHERE key = 'metric_rows'",
                [],
            )
            .map_err(|_| ObservabilityError::storage())?;
        if changed != 1 {
            return Err(ObservabilityError::corrupt());
        }
    }
    Ok(())
}

fn evaluate_alerts(
    transaction: &Transaction<'_>,
    rules: &[AlertRule],
    observation: &Observation,
) -> Result<Vec<AlertTransition>, ObservabilityError> {
    let mut transitions = Vec::new();
    for rule in rules {
        let state: Option<(String, i64)> = transaction
            .query_row(
                "SELECT status, generation FROM alert_states WHERE rule_id = ?1",
                [rule.rule_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|_| ObservabilityError::storage())?;
        let current = state
            .map(|(status, generation)| {
                AlertState::try_new(parse_alert_status(&status)?, from_i64(generation)?)
            })
            .transpose()?;
        let Some(next) = evaluate_alert_rule(rule, observation, current)? else {
            continue;
        };
        transaction
            .execute(
                "INSERT INTO alert_states(rule_id, status, generation)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(rule_id) DO UPDATE SET
                     status = excluded.status,
                     generation = excluded.generation",
                params![
                    rule.rule_id.as_str(),
                    alert_status_key(next.status),
                    to_i64(next.generation)?
                ],
            )
            .map_err(|_| ObservabilityError::storage())?;
        transaction
            .execute(
                "INSERT INTO alert_transitions(rule_id, generation, status, transition_json)
                 VALUES (?1, ?2, ?3, X'7B7D')",
                params![
                    rule.rule_id.as_str(),
                    to_i64(next.generation)?,
                    alert_status_key(next.status)
                ],
            )
            .map_err(|_| ObservabilityError::storage())?;
        let sequence = from_i64(transaction.last_insert_rowid())?;
        let transition = AlertTransition::try_from_state(sequence, rule, next, observation)?;
        let transition_json =
            serde_json::to_vec(&transition).map_err(|_| ObservabilityError::invalid())?;
        transaction
            .execute(
                "UPDATE alert_transitions SET transition_json = ?1 WHERE sequence = ?2",
                params![transition_json, to_i64(sequence)?],
            )
            .map_err(|_| ObservabilityError::storage())?;
        transitions.push(transition);
    }
    Ok(transitions)
}

fn store_receipt_transitions(
    transaction: &Transaction<'_>,
    sequence: u64,
    transitions: &[AlertTransition],
) -> Result<(), ObservabilityError> {
    let json = serde_json::to_vec(transitions).map_err(|_| ObservabilityError::invalid())?;
    transaction
        .execute(
            "UPDATE observation_receipts SET alert_transitions_json = ?1 WHERE sequence = ?2",
            params![json, to_i64(sequence)?],
        )
        .map_err(|_| ObservabilityError::storage())?;
    Ok(())
}

fn trim_trace_rows(
    transaction: &Transaction<'_>,
    max_trace_rows: u64,
    current_sequence: u64,
) -> Result<(), ObservabilityError> {
    let cutoff = current_sequence.saturating_sub(max_trace_rows);
    if cutoff > 0 {
        transaction
            .execute(
                "DELETE FROM observation_log WHERE sequence <= ?1",
                [to_i64(cutoff)?],
            )
            .map_err(|_| ObservabilityError::storage())?;
    }
    Ok(())
}

fn trim_metric_rows(
    transaction: &Transaction<'_>,
    max_metric_rows: u64,
) -> Result<(), ObservabilityError> {
    let count: i64 = transaction
        .query_row(
            "SELECT value FROM observability_counters WHERE key = 'metric_rows'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| ObservabilityError::storage())?;
    let excess = from_i64(count)?.saturating_sub(max_metric_rows);
    if excess > 0 {
        let deleted = transaction
            .execute(
                "DELETE FROM metric_series WHERE (bucket_start_millis, series_key) IN (
                     SELECT bucket_start_millis, series_key FROM metric_series
                     ORDER BY bucket_start_millis ASC, series_key ASC LIMIT ?1
                 )",
                [to_i64(excess)?],
            )
            .map_err(|_| ObservabilityError::storage())?;
        if u64::try_from(deleted).map_err(|_| ObservabilityError::corrupt())? != excess {
            return Err(ObservabilityError::corrupt());
        }
        let changed = transaction
            .execute(
                "UPDATE observability_counters SET value = value - ?1
                 WHERE key = 'metric_rows' AND value >= ?1",
                [to_i64(excess)?],
            )
            .map_err(|_| ObservabilityError::storage())?;
        if changed != 1 {
            return Err(ObservabilityError::corrupt());
        }
    }
    Ok(())
}

fn decode_metric_row(row: &rusqlite::Row<'_>) -> Result<MetricRow, ObservabilityError> {
    let key_json: String = row.get(1).map_err(|_| ObservabilityError::corrupt())?;
    let key: MetricSeriesKey =
        serde_json::from_str(&key_json).map_err(|_| ObservabilityError::corrupt())?;
    key.validate().map_err(|_| ObservabilityError::corrupt())?;
    Ok(MetricRow {
        bucket_start_unix_millis: from_i64(row.get(0).map_err(|_| ObservabilityError::corrupt())?)?,
        key,
        observations: from_i64(row.get(2).map_err(|_| ObservabilityError::corrupt())?)?,
        latency_total_millis: from_i64(row.get(3).map_err(|_| ObservabilityError::corrupt())?)?,
        latency_max_millis: from_i64(row.get(4).map_err(|_| ObservabilityError::corrupt())?)?,
        recovered_items: from_i64(row.get(5).map_err(|_| ObservabilityError::corrupt())?)?,
        latest_used: from_i64(row.get(6).map_err(|_| ObservabilityError::corrupt())?)?,
        latest_limit: from_i64(row.get(7).map_err(|_| ObservabilityError::corrupt())?)?,
        maximum_used: from_i64(row.get(8).map_err(|_| ObservabilityError::corrupt())?)?,
    })
}

fn parse_alert_status(value: &str) -> Result<AlertStatus, ObservabilityError> {
    match value {
        "firing" => Ok(AlertStatus::Firing),
        "resolved" => Ok(AlertStatus::Resolved),
        _ => Err(ObservabilityError::corrupt()),
    }
}

const fn alert_status_key(status: AlertStatus) -> &'static str {
    match status {
        AlertStatus::Firing => "firing",
        AlertStatus::Resolved => "resolved",
    }
}

fn to_i64(value: u64) -> Result<i64, ObservabilityError> {
    i64::try_from(value).map_err(|_| ObservabilityError::invalid())
}

fn from_i64(value: i64) -> Result<u64, ObservabilityError> {
    u64::try_from(value).map_err(|_| ObservabilityError::corrupt())
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn prepare_parent(path: &Path) -> Result<(), ObservabilityError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let existed = parent.exists();
    fs::create_dir_all(parent).map_err(|_| ObservabilityError::storage())?;
    if existed {
        Ok(())
    } else {
        protect_path(parent, 0o700)
    }
}

fn protect_database(path: &Path) -> Result<(), ObservabilityError> {
    protect_path(path, 0o600)
}

#[cfg(unix)]
fn protect_path(path: &Path, mode: u32) -> Result<(), ObservabilityError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|_| ObservabilityError::storage())
}

#[cfg(not(unix))]
fn protect_path(_path: &Path, _mode: u32) -> Result<(), ObservabilityError> {
    Ok(())
}

impl ObservabilityStore for SqliteObservability {
    fn record(
        &mut self,
        observation: &Observation,
    ) -> Result<ObservationReceipt, ObservabilityError> {
        Self::record(self, observation)
    }

    fn trace_page(
        &self,
        trace_id: &TraceId,
        after_sequence: u64,
        limit: u32,
    ) -> Result<TracePage, ObservabilityError> {
        Self::trace_page(self, trace_id, after_sequence, limit)
    }

    fn metric_page(
        &self,
        from_bucket_inclusive: u64,
        to_bucket_exclusive: u64,
        after: Option<&MetricCursor>,
        limit: u32,
    ) -> Result<MetricPage, ObservabilityError> {
        Self::metric_page(
            self,
            from_bucket_inclusive,
            to_bucket_exclusive,
            after,
            limit,
        )
    }

    fn alert_page(&self, after_sequence: u64, limit: u32) -> Result<AlertPage, ObservabilityError> {
        Self::alert_page(self, after_sequence, limit)
    }
}
