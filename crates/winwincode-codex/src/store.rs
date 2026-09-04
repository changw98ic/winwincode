// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use crate::model_port_client::{
    ModelCancellationFingerprint, ModelCancellationPhase, ModelChunkFingerprint,
    ModelCursorSnapshot, ModelCursorStore, ModelLeaseAuthority, ModelTerminationReason,
};
use crate::performance::{
    DelegatedPerformanceTotals, PerformanceOperationCompletion, PerformanceOperationKind,
    StoredPerformanceProjection, elapsed_millis,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{ModelExchangeId, Sha256Digest};
use winwincode_execution_port::replay::{
    ReplayAcknowledgementStore, ReplayFrame, ReplaySnapshot, ReplayStore, ReplayStreamKey,
};
use winwincode_execution_port::{
    generated::{ExecutionPortMessage, ModelChunkMessage},
    runtime_trace_outbox::{ExecutionMode, ObserverMode, PerformanceBaselineReport},
    typed_replay::frame_from_message,
};

pub(crate) const DATABASE_FILE: &str = "worker-codex.sqlite3";
const MAX_MODEL_CALL_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_SAFE_METRIC: i64 = 9_007_199_254_740_991;

/// Durable hand-off state for one Core model call. `ProviderFinal` means the
/// provider response is complete and replayable, while `CoreCommitted` means
/// the embedded Core has observed the corresponding terminal turn boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelCallPhase {
    InFlight,
    ProviderFinal,
    CoreCommitted,
}

#[derive(Clone)]
pub(crate) struct AdapterStore {
    path: PathBuf,
    connection: Arc<Mutex<Connection>>,
}

fn initialize_schema(connection: &Connection) -> Result<(), AdapterStoreError> {
    connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 PRAGMA foreign_keys=ON;
                 CREATE TABLE IF NOT EXISTS codex_run (
                   run_key TEXT PRIMARY KEY NOT NULL,
                   record_json BLOB NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS codex_store_migration (
                   migration_key TEXT PRIMARY KEY NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS model_cursor (
                   stream_key TEXT PRIMARY KEY NOT NULL,
                   snapshot_json BLOB NOT NULL
                 );
                 -- A request identity is allocated by embedded Codex and is
                 -- stable across a process restart.  Multiple calls for one
                 -- run use this ledger independently; ordinal is only a
                 -- durable ordering value, never an identity lookup.
                 CREATE TABLE IF NOT EXISTS model_call_ledger (
                   run_key TEXT NOT NULL,
                   model_call_id TEXT NOT NULL,
                   model_exchange_id TEXT,
                   ordinal INTEGER NOT NULL,
                   request_digest TEXT NOT NULL,
                   completed INTEGER NOT NULL CHECK(completed IN (0, 1)),
                   provider_final INTEGER NOT NULL DEFAULT 0
                     CHECK(provider_final IN (0, 1)),
                   core_committed INTEGER NOT NULL DEFAULT 0
                     CHECK(core_committed IN (0, 1)),
                   PRIMARY KEY (run_key, model_call_id),
                   UNIQUE (run_key, ordinal)
                 );
                 CREATE TABLE IF NOT EXISTS model_call_frame (
                   run_key TEXT NOT NULL,
                   model_call_id TEXT NOT NULL,
                   sequence INTEGER NOT NULL,
                   frame_digest TEXT NOT NULL,
                   frame_json BLOB NOT NULL,
                   PRIMARY KEY (run_key, model_call_id, sequence)
                 );
                 CREATE TABLE IF NOT EXISTS model_thread_lineage (
                   thread_id TEXT PRIMARY KEY NOT NULL,
                   run_key TEXT NOT NULL,
                   authority_json BLOB NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS model_thread_lineage_run_idx
                   ON model_thread_lineage(run_key);
                 CREATE TABLE IF NOT EXISTS runtime_replay (
                   stream_key TEXT PRIMARY KEY NOT NULL,
                   snapshot_json BLOB NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS execution_outbox (
                   position INTEGER PRIMARY KEY AUTOINCREMENT,
                   delivery_id TEXT NOT NULL UNIQUE,
                   family TEXT NOT NULL,
                   correlation_key TEXT NOT NULL,
                   acknowledgement_required INTEGER NOT NULL CHECK(acknowledgement_required IN (0, 1)),
                   state TEXT NOT NULL CHECK(state IN ('pending', 'sent_attempt')),
                   frame_digest TEXT NOT NULL,
                   frame_json BLOB NOT NULL,
                   UNIQUE(family, correlation_key)
                 );
                 CREATE TABLE IF NOT EXISTS worker_transport_state (
                   state_key TEXT PRIMARY KEY NOT NULL,
                   sequence INTEGER NOT NULL CHECK(sequence >= 0)
                 );
                 CREATE TABLE IF NOT EXISTS approval_operation (
                   approval_id TEXT PRIMARY KEY NOT NULL,
                   run_key TEXT NOT NULL,
                   kernel_session_id TEXT NOT NULL,
                   operation_kind TEXT NOT NULL CHECK(operation_kind IN ('exec', 'patch')),
                   operation_id TEXT NOT NULL,
                   turn_id TEXT,
                   request_digest TEXT NOT NULL,
                   resolution_digest TEXT,
                   state TEXT NOT NULL CHECK(state IN ('pending', 'resolved')),
                   CHECK (
                     (state = 'pending' AND resolution_digest IS NULL)
                     OR (state = 'resolved' AND resolution_digest IS NOT NULL)
                   )
                 );
                 CREATE TABLE IF NOT EXISTS input_operation (
                   input_request_id TEXT PRIMARY KEY NOT NULL,
                   run_key TEXT NOT NULL,
                   kernel_session_id TEXT NOT NULL,
                   question_id TEXT NOT NULL,
                   turn_id TEXT NOT NULL,
                   request_digest TEXT NOT NULL,
                   resolution_digest TEXT,
                   state TEXT NOT NULL CHECK(state IN ('pending', 'resolved')),
                   CHECK (
                     (state = 'pending' AND resolution_digest IS NULL)
                     OR (state = 'resolved' AND resolution_digest IS NOT NULL)
                   )
                 );",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
    initialize_performance_schema(connection)
}

fn initialize_performance_schema(connection: &Connection) -> Result<(), AdapterStoreError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS performance_run (
                   run_key TEXT PRIMARY KEY NOT NULL,
                   execution_mode TEXT NOT NULL CHECK(execution_mode IN (
                     'react', 'delegated_patch_shadow', 'delegated_patch'
                   )),
                   observer_mode TEXT NOT NULL CHECK(observer_mode IN (
                     'off', 'shadow', 'ambiguous_only', 'always'
                   ))
                 );
                 CREATE TABLE IF NOT EXISTS performance_operation (
                   run_key TEXT NOT NULL,
                   operation_kind TEXT NOT NULL CHECK(operation_kind IN (
                     'primary_model', 'tool', 'patch', 'validation',
                     'observer', 'repair', 'turn'
                   )),
                   operation_id TEXT NOT NULL,
                   started_at TEXT NOT NULL,
                   completed_at TEXT,
                   completed INTEGER NOT NULL CHECK(completed IN (0, 1)),
                   duration_millis INTEGER NOT NULL DEFAULT 0
                     CHECK(duration_millis >= 0 AND duration_millis <= 9007199254740991),
                   input_tokens INTEGER NOT NULL DEFAULT 0
                     CHECK(input_tokens >= 0 AND input_tokens <= 9007199254740991),
                   cached_tokens INTEGER NOT NULL DEFAULT 0
                     CHECK(cached_tokens >= 0 AND cached_tokens <= 9007199254740991),
                   output_tokens INTEGER NOT NULL DEFAULT 0
                     CHECK(output_tokens >= 0 AND output_tokens <= 9007199254740991),
                   actual_cost_microunits INTEGER
                     CHECK(actual_cost_microunits IS NULL OR (
                       actual_cost_microunits >= 0 AND
                       actual_cost_microunits <= 9007199254740991
                     )),
                   PRIMARY KEY (run_key, operation_kind, operation_id),
                   CHECK (
                     (completed = 0 AND completed_at IS NULL)
                     OR (completed = 1 AND completed_at IS NOT NULL)
                   )
                 );
                 CREATE TABLE IF NOT EXISTS performance_changed_file (
                   run_key TEXT NOT NULL,
                   path_digest TEXT NOT NULL,
                   PRIMARY KEY (run_key, path_digest)
                 );
                 CREATE TABLE IF NOT EXISTS performance_projection (
                   run_key TEXT PRIMARY KEY NOT NULL,
                   record_json BLOB NOT NULL
                 );",
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let has_cost_column = {
        let mut statement = connection
            .prepare("PRAGMA table_info(performance_operation)")
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let mut found = false;
        for column in columns {
            if column.map_err(|_| AdapterStoreError::Unavailable)? == "actual_cost_microunits" {
                found = true;
                break;
            }
        }
        found
    };
    if !has_cost_column {
        connection
            .execute_batch(
                "ALTER TABLE performance_operation
                 ADD COLUMN actual_cost_microunits INTEGER
                 CHECK(actual_cost_microunits IS NULL OR (
                   actual_cost_microunits >= 0 AND
                   actual_cost_microunits <= 9007199254740991
                 ));",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
    }
    Ok(())
}
impl AdapterStore {
    pub(crate) fn open(root: &Path) -> Result<Self, AdapterStoreError> {
        std::fs::create_dir_all(root).map_err(|_| AdapterStoreError::Unavailable)?;
        restrict_directory(root)?;
        let path = root.join(DATABASE_FILE);
        create_private_file(&path)?;
        let connection = Connection::open(&path).map_err(|_| AdapterStoreError::Unavailable)?;
        initialize_schema(&connection)?;
        ensure_model_call_phase_columns(&connection)?;
        restrict_file(&path)?;
        restrict_file_if_present(&root.join(format!("{DATABASE_FILE}-wal")))?;
        restrict_file_if_present(&root.join(format!("{DATABASE_FILE}-shm")))?;
        Ok(Self {
            path,
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn load_run<T: DeserializeOwned>(
        &self,
        run_key: &str,
    ) -> Result<Option<T>, AdapterStoreError> {
        let connection = self.lock()?;
        let bytes = connection
            .query_row(
                "SELECT record_json FROM codex_run WHERE run_key = ?1",
                params![run_key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        bytes
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt))
            .transpose()
    }

    pub(crate) fn save_run<T: Serialize>(
        &self,
        run_key: &str,
        record: &T,
    ) -> Result<(), AdapterStoreError> {
        let bytes = serde_json::to_vec(record).map_err(|_| AdapterStoreError::Corrupt)?;
        self.lock()?
            .execute(
                "INSERT INTO codex_run(run_key, record_json) VALUES (?1, ?2)
                 ON CONFLICT(run_key) DO UPDATE SET record_json = excluded.record_json",
                params![run_key, bytes],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(())
    }

    /// Applies one bounded startup migration to every durable run before the
    /// adapter begins serving work. The marker and all rewritten records are
    /// committed atomically, so normal run loading never needs a legacy
    /// fallback path.
    pub(crate) fn migrate_run_records_once(
        &self,
        migration_key: &str,
        mut migrate: impl FnMut(&str, &[u8]) -> Result<Option<Vec<u8>>, AdapterStoreError>,
    ) -> Result<bool, AdapterStoreError> {
        if migration_key.trim().is_empty() || migration_key.len() > 128 {
            return Err(AdapterStoreError::Conflict);
        }
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let already_applied = transaction
            .query_row(
                "SELECT 1 FROM codex_store_migration WHERE migration_key = ?1",
                params![migration_key],
                |_| Ok(()),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?
            .is_some();
        if already_applied {
            transaction
                .commit()
                .map_err(|_| AdapterStoreError::Unavailable)?;
            return Ok(false);
        }

        let records = {
            let mut statement = transaction
                .prepare("SELECT run_key, record_json FROM codex_run ORDER BY run_key")
                .map_err(|_| AdapterStoreError::Unavailable)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(|_| AdapterStoreError::Unavailable)?;
            rows.map(|row| row.map_err(|_| AdapterStoreError::Unavailable))
                .collect::<Result<Vec<_>, _>>()?
        };
        for (run_key, record) in records {
            if let Some(migrated) = migrate(&run_key, &record)? {
                transaction
                    .execute(
                        "UPDATE codex_run SET record_json = ?2 WHERE run_key = ?1",
                        params![run_key, migrated],
                    )
                    .map_err(|_| AdapterStoreError::Unavailable)?;
            }
        }
        transaction
            .execute(
                "INSERT INTO codex_store_migration(migration_key) VALUES (?1)",
                params![migration_key],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(true)
    }

    pub(crate) fn save_run_in_transaction<T: Serialize>(
        transaction: &Transaction<'_>,
        run_key: &str,
        record: &T,
    ) -> Result<(), AdapterStoreError> {
        let bytes = serde_json::to_vec(record).map_err(|_| AdapterStoreError::Corrupt)?;
        transaction
            .execute(
                "INSERT INTO codex_run(run_key, record_json) VALUES (?1, ?2)
                 ON CONFLICT(run_key) DO UPDATE SET record_json = excluded.record_json",
                params![run_key, bytes],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(())
    }

    /// Freezes the process feature gates for one exact run. A restart must
    /// present the same values; changing mode halfway through an attempt is a
    /// conflict rather than a second measurement cohort.
    pub(crate) fn register_performance_run(
        &self,
        run_key: &str,
        execution_mode: ExecutionMode,
        observer_mode: ObserverMode,
    ) -> Result<(), AdapterStoreError> {
        if run_key.trim().is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        let connection = self.lock()?;
        connection
            .execute(
                "INSERT OR IGNORE INTO performance_run(
                   run_key, execution_mode, observer_mode
                 ) VALUES (?1, ?2, ?3)",
                params![
                    run_key,
                    execution_mode.as_config(),
                    observer_mode.as_config()
                ],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let existing = connection
            .query_row(
                "SELECT execution_mode, observer_mode FROM performance_run WHERE run_key = ?1",
                params![run_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if existing
            != (
                execution_mode.as_config().to_owned(),
                observer_mode.as_config().to_owned(),
            )
        {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(())
    }

    /// Retains the first observation of one stable operation identity.
    /// Duplicate Kernel or Provider delivery is an exact no-op.
    pub(crate) fn record_performance_start(
        &self,
        run_key: &str,
        kind: PerformanceOperationKind,
        operation_id: &str,
        started_at: &winwincode_domain::Instant,
    ) -> Result<(), AdapterStoreError> {
        validate_performance_identity(run_key, operation_id, started_at)?;
        self.lock()?
            .execute(
                "INSERT OR IGNORE INTO performance_operation(
                   run_key, operation_kind, operation_id, started_at, completed
                 ) VALUES (?1, ?2, ?3, ?4, 0)",
                params![run_key, kind.as_str(), operation_id, started_at.0],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(())
    }

    /// Completes one stable operation at most once. A replay leaves the first
    /// bounded counters and timing values unchanged.
    pub(crate) fn record_performance_completion(
        &self,
        run_key: &str,
        kind: PerformanceOperationKind,
        operation_id: &str,
        completed_at: &winwincode_domain::Instant,
        completion: PerformanceOperationCompletion,
    ) -> Result<(), AdapterStoreError> {
        validate_performance_identity(run_key, operation_id, completed_at)?;
        for value in [
            completion.input_tokens,
            completion.cached_tokens,
            completion.output_tokens,
        ] {
            validate_metric(value)?;
        }
        if let Some(duration) = completion.duration_millis {
            validate_metric(duration)?;
        }
        if let Some(cost) = completion.actual_cost_microunits {
            validate_metric(cost)?;
        }
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let existing = transaction
            .query_row(
                "SELECT started_at, completed FROM performance_operation
                 WHERE run_key = ?1 AND operation_kind = ?2 AND operation_id = ?3",
                params![run_key, kind.as_str(), operation_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if existing
            .as_ref()
            .is_some_and(|(_, completed)| *completed == 1)
        {
            transaction
                .commit()
                .map_err(|_| AdapterStoreError::Unavailable)?;
            return Ok(());
        }
        let started_at = existing.map_or_else(
            || completed_at.clone(),
            |(started_at, _)| winwincode_domain::Instant(started_at),
        );
        let duration_millis = completion
            .duration_millis
            .or_else(|| elapsed_millis(&started_at, completed_at))
            .unwrap_or(0);
        validate_metric(duration_millis)?;
        transaction
            .execute(
                "INSERT INTO performance_operation(
                   run_key, operation_kind, operation_id, started_at,
                   completed_at, completed, duration_millis, input_tokens,
                   cached_tokens, output_tokens
                   , actual_cost_microunits
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(run_key, operation_kind, operation_id) DO UPDATE SET
                   completed_at = excluded.completed_at,
                   completed = 1,
                   duration_millis = excluded.duration_millis,
                   input_tokens = excluded.input_tokens,
                   cached_tokens = excluded.cached_tokens,
                   output_tokens = excluded.output_tokens,
                   actual_cost_microunits = excluded.actual_cost_microunits
                 WHERE performance_operation.completed = 0",
                params![
                    run_key,
                    kind.as_str(),
                    operation_id,
                    started_at.0,
                    completed_at.0,
                    duration_millis,
                    completion.input_tokens,
                    completion.cached_tokens,
                    completion.output_tokens,
                    completion.actual_cost_microunits,
                ],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)
    }

    /// Reads the only durable budget ledger used before a delegated follow-up
    /// turn. Primary and Observer usage share the cumulative token and cost
    /// budgets. Any pending model operation or completed operation without a
    /// settled charge is reported as incomplete rather than silently counted
    /// as zero cost.
    pub(crate) fn delegated_performance_totals(
        &self,
        run_key: &str,
    ) -> Result<DelegatedPerformanceTotals, AdapterStoreError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT
                   COALESCE(SUM(operation_kind = 'primary_model'), 0),
                   COALESCE(SUM(CASE WHEN operation_kind IN ('primary_model', 'observer')
                     THEN input_tokens + cached_tokens + output_tokens ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN operation_kind IN ('primary_model', 'observer')
                     THEN actual_cost_microunits ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN operation_kind = 'primary_model'
                     THEN duration_millis ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN operation_kind IN ('primary_model', 'observer')
                     AND actual_cost_microunits IS NULL THEN 1 ELSE 0 END), 0)
                   , COALESCE(SUM(CASE WHEN operation_kind IN ('primary_model', 'observer')
                     AND completed = 0 THEN 1 ELSE 0 END), 0)
                   , COALESCE(SUM(operation_kind = 'observer'), 0)
                 FROM performance_operation WHERE run_key = ?1",
                params![run_key],
                |row| {
                    let missing_costs = row.get::<_, i64>(4)?;
                    let pending_model_calls = row.get::<_, i64>(5)?;
                    Ok(DelegatedPerformanceTotals {
                        primary_model_calls: row.get(0)?,
                        total_tokens: row.get(1)?,
                        total_cost_microunits: row.get(2)?,
                        elapsed_millis: row.get(3)?,
                        cost_complete: missing_costs == 0 && pending_model_calls == 0,
                        pending_model_calls,
                        observer_calls: row.get(6)?,
                    })
                },
            )
            .map_err(|_| AdapterStoreError::Unavailable)
    }

    /// Counts changed files by a private path digest. Raw paths never enter
    /// the metrics database or public report.
    pub(crate) fn record_performance_changed_file(
        &self,
        run_key: &str,
        path_digest: &Sha256Digest,
    ) -> Result<(), AdapterStoreError> {
        if run_key.trim().is_empty() || !valid_sha256_digest(&path_digest.0) {
            return Err(AdapterStoreError::Conflict);
        }
        self.lock()?
            .execute(
                "INSERT OR IGNORE INTO performance_changed_file(run_key, path_digest)
                 VALUES (?1, ?2)",
                params![run_key, path_digest.0],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(())
    }

    /// Measures the complete durable operation window at terminal time.
    pub(crate) fn performance_total_runtime(
        &self,
        run_key: &str,
        completed_at: &winwincode_domain::Instant,
    ) -> Result<i64, AdapterStoreError> {
        validate_performance_identity(run_key, "terminal-runtime", completed_at)?;
        let (started_at, last_completed_at) = self
            .lock()?
            .query_row(
                "SELECT MIN(started_at), MAX(completed_at)
                 FROM performance_operation WHERE run_key = ?1",
                params![run_key],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let Some(started_at) = started_at else {
            return Ok(0);
        };
        let started_at = winwincode_domain::Instant(started_at);
        let mut total =
            elapsed_millis(&started_at, completed_at).ok_or(AdapterStoreError::Corrupt)?;
        if let Some(last_completed_at) = last_completed_at {
            total = total.max(
                elapsed_millis(&started_at, &winwincode_domain::Instant(last_completed_at))
                    .ok_or(AdapterStoreError::Corrupt)?,
            );
        }
        validate_metric(total)?;
        Ok(total)
    }

    /// Builds the bounded aggregate used by the terminal runtime projection.
    pub(crate) fn performance_report(
        &self,
        run_key: &str,
        total_runtime_ms: i64,
    ) -> Result<PerformanceBaselineReport, AdapterStoreError> {
        validate_metric(total_runtime_ms)?;
        let connection = self.lock()?;
        let (execution_mode, observer_mode) = connection
            .query_row(
                "SELECT execution_mode, observer_mode FROM performance_run WHERE run_key = ?1",
                params![run_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let execution_mode =
            ExecutionMode::from_config(&execution_mode).ok_or(AdapterStoreError::Corrupt)?;
        let observer_mode =
            ObserverMode::from_config(&observer_mode).ok_or(AdapterStoreError::Corrupt)?;
        let totals = connection
            .query_row(
                "SELECT
                   COALESCE(SUM(operation_kind = 'primary_model'), 0),
                   COALESCE(SUM(CASE WHEN operation_kind = 'primary_model' THEN input_tokens ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN operation_kind = 'primary_model' THEN cached_tokens ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN operation_kind = 'primary_model' THEN output_tokens ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN operation_kind = 'primary_model' THEN duration_millis ELSE 0 END), 0),
                   COALESCE(SUM(operation_kind = 'tool'), 0),
                   COALESCE(SUM(operation_kind = 'patch'), 0),
                   COALESCE(SUM(CASE WHEN operation_kind = 'patch' THEN duration_millis ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN operation_kind = 'validation' THEN duration_millis ELSE 0 END), 0),
                   COALESCE(SUM(operation_kind = 'observer'), 0),
                   COALESCE(SUM(CASE WHEN operation_kind = 'observer' THEN duration_millis ELSE 0 END), 0),
                   COALESCE(SUM(operation_kind = 'repair'), 0),
                   COALESCE(SUM(operation_kind = 'turn'), 0)
                 FROM performance_operation WHERE run_key = ?1",
                params![run_key],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, i64>(10)?,
                        row.get::<_, i64>(11)?,
                        row.get::<_, i64>(12)?,
                    ))
                },
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let files_changed = connection
            .query_row(
                "SELECT COUNT(*) FROM performance_changed_file WHERE run_key = ?1",
                params![run_key],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let report = PerformanceBaselineReport {
            execution_mode,
            observer_mode,
            primary_model_call_count: totals.0,
            primary_model_input_tokens: totals.1,
            primary_model_cached_tokens: totals.2,
            primary_model_output_tokens: totals.3,
            primary_model_wait_ms: totals.4,
            tool_call_count: totals.5,
            patch_call_count: totals.6,
            patch_apply_ms: totals.7,
            files_changed,
            validation_ms: totals.8,
            observer_call_count: totals.9,
            observer_wait_ms: totals.10,
            repair_rounds: totals.11,
            turn_count: totals.12,
            total_runtime_ms,
        };
        if performance_report_values(&report).any(|value| validate_metric(value).is_err()) {
            return Err(AdapterStoreError::Corrupt);
        }
        Ok(report)
    }

    /// Reserves the exact terminal aggregate event before its runtime frame is
    /// appended. Repeating the reservation requires byte-for-byte report
    /// equality.
    pub(crate) fn reserve_performance_projection(
        &self,
        run_key: &str,
        event_id: winwincode_domain::ExecutionEventId,
        sequence: winwincode_domain::ExecutionSequence,
        report: PerformanceBaselineReport,
    ) -> Result<StoredPerformanceProjection, AdapterStoreError> {
        if run_key.trim().is_empty() || sequence.0 <= 0 || sequence.0 > MAX_SAFE_METRIC {
            return Err(AdapterStoreError::Conflict);
        }
        let report_bytes = serde_json::to_vec(&report).map_err(|_| AdapterStoreError::Corrupt)?;
        let record = StoredPerformanceProjection {
            event_id,
            sequence,
            report,
            report_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&report_bytes))),
            retained: false,
        };
        if let Some(existing) = self.load_performance_projection(run_key)? {
            if existing.event_id == record.event_id
                && existing.sequence == record.sequence
                && existing.report == record.report
                && existing.report_digest == record.report_digest
            {
                return Ok(existing);
            }
            return Err(AdapterStoreError::Conflict);
        }
        let bytes = serde_json::to_vec(&record).map_err(|_| AdapterStoreError::Corrupt)?;
        self.lock()?
            .execute(
                "INSERT INTO performance_projection(run_key, record_json) VALUES (?1, ?2)",
                params![run_key, bytes],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(record)
    }

    pub(crate) fn load_performance_projection(
        &self,
        run_key: &str,
    ) -> Result<Option<StoredPerformanceProjection>, AdapterStoreError> {
        let bytes = self
            .lock()?
            .query_row(
                "SELECT record_json FROM performance_projection WHERE run_key = ?1",
                params![run_key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let record = bytes
            .map(|bytes| {
                serde_json::from_slice::<StoredPerformanceProjection>(&bytes)
                    .map_err(|_| AdapterStoreError::Corrupt)
            })
            .transpose()?;
        if let Some(record) = &record {
            let report =
                serde_json::to_vec(&record.report).map_err(|_| AdapterStoreError::Corrupt)?;
            let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(report)));
            if record.sequence.0 <= 0
                || record.sequence.0 > MAX_SAFE_METRIC
                || record.report_digest != digest
            {
                return Err(AdapterStoreError::Corrupt);
            }
        }
        Ok(record)
    }

    pub(crate) fn mark_performance_projection_retained(
        &self,
        run_key: &str,
    ) -> Result<(), AdapterStoreError> {
        let mut record = self
            .load_performance_projection(run_key)?
            .ok_or(AdapterStoreError::Conflict)?;
        if record.retained {
            return Ok(());
        }
        record.retained = true;
        let bytes = serde_json::to_vec(&record).map_err(|_| AdapterStoreError::Corrupt)?;
        self.lock()?
            .execute(
                "UPDATE performance_projection SET record_json = ?2 WHERE run_key = ?1",
                params![run_key, bytes],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(())
    }

    /// Claims one stable Core model-call identity.
    ///
    /// The row is keyed by `model_call_id`, while the exchange id is bound
    /// before any Provider open. Multiple calls for one run may be in flight
    /// at once, and a retry after a process restart resolves to the same row.
    pub(crate) fn claim_model_call(
        &self,
        run_key: &str,
        model_call_id: &str,
        model_exchange_id: &ModelExchangeId,
        request_digest: &Sha256Digest,
    ) -> Result<u64, AdapterStoreError> {
        validate_model_call_inputs(run_key, model_call_id, model_exchange_id)?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let existing = transaction
            .query_row(
                "SELECT ordinal, request_digest, completed, model_exchange_id
                 FROM model_call_ledger
                 WHERE run_key = ?1 AND model_call_id = ?2",
                params![run_key, model_call_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if let Some((ordinal, existing_digest, completed, existing_exchange)) = existing {
            if ordinal <= 0 || !matches!(completed, 0 | 1) {
                return Err(AdapterStoreError::Corrupt);
            }
            if existing_digest != request_digest.0 {
                return Err(AdapterStoreError::Conflict);
            }
            if existing_exchange
                .as_deref()
                .is_some_and(|existing| existing != model_exchange_id.0)
            {
                return Err(AdapterStoreError::Conflict);
            }
            if existing_exchange.is_none() {
                transaction
                    .execute(
                        "UPDATE model_call_ledger SET model_exchange_id = ?3
                         WHERE run_key = ?1 AND model_call_id = ?2",
                        params![run_key, model_call_id, model_exchange_id.0],
                    )
                    .map_err(|_| AdapterStoreError::Unavailable)?;
            }
            transaction
                .commit()
                .map_err(|_| AdapterStoreError::Unavailable)?;
            return u64::try_from(ordinal).map_err(|_| AdapterStoreError::Corrupt);
        }
        let exchange_owner = transaction
            .query_row(
                "SELECT model_call_id FROM model_call_ledger
                 WHERE run_key = ?1 AND model_exchange_id = ?2",
                params![run_key, model_exchange_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if exchange_owner.is_some_and(|owner| owner != model_call_id) {
            return Err(AdapterStoreError::Conflict);
        }
        let latest = transaction
            .query_row(
                "SELECT COALESCE(MAX(ordinal), 0) FROM model_call_ledger WHERE run_key = ?1",
                params![run_key],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if latest < 0 {
            return Err(AdapterStoreError::Corrupt);
        }
        let next = latest.checked_add(1).ok_or(AdapterStoreError::Conflict)?;
        if next <= 0 {
            return Err(AdapterStoreError::Conflict);
        }
        transaction
            .execute(
                "INSERT INTO model_call_ledger(
                   run_key, model_call_id, model_exchange_id, ordinal, request_digest, completed,
                   provider_final, core_committed
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 0)",
                params![
                    run_key,
                    model_call_id,
                    model_exchange_id.0,
                    next,
                    request_digest.0
                ],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        u64::try_from(next).map_err(|_| AdapterStoreError::Conflict)
    }

    /// Returns the durable phase of one stable Core model call.
    pub(crate) fn model_call_phase(
        &self,
        run_key: &str,
        model_call_id: &str,
    ) -> Result<Option<ModelCallPhase>, AdapterStoreError> {
        if run_key.trim().is_empty() || model_call_id.trim().is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        let connection = self.lock()?;
        let state = connection
            .query_row(
                "SELECT completed, provider_final, core_committed
                 FROM model_call_ledger
                 WHERE run_key = ?1 AND model_call_id = ?2",
                params![run_key, model_call_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        state
            .map(|(completed, provider_final, core_committed)| {
                decode_model_call_phase(completed, provider_final, core_committed)
            })
            .transpose()
    }

    /// Persists the exact provider frame before the call is allowed to move
    /// past the `ProviderFinal` phase. The payload is kept only in this private
    /// store so a restarted Core can consume the same response bytes without
    /// opening a second provider exchange.
    pub(crate) fn retain_model_call_frame(
        &self,
        run_key: &str,
        model_call_id: &str,
        chunk: &ModelChunkMessage,
    ) -> Result<(), AdapterStoreError> {
        if run_key.trim().is_empty() || model_call_id.trim().is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        let sequence = chunk.sequence.0;
        if sequence <= 0 {
            return Err(AdapterStoreError::Conflict);
        }
        let frame = serde_json::to_vec(chunk).map_err(|_| AdapterStoreError::Corrupt)?;
        if frame.len() > MAX_MODEL_CALL_FRAME_BYTES {
            return Err(AdapterStoreError::Conflict);
        }
        let frame_digest = format!("sha256:{:x}", Sha256::digest(&frame));
        self.transaction(|transaction| {
            let known = transaction
                .query_row(
                    "SELECT 1 FROM model_call_ledger
                     WHERE run_key = ?1 AND model_call_id = ?2",
                    params![run_key, model_call_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(|_| AdapterStoreError::Unavailable)?;
            if known != Some(1) {
                return Err(AdapterStoreError::Conflict);
            }
            let existing = transaction
                .query_row(
                    "SELECT frame_digest, frame_json FROM model_call_frame
                     WHERE run_key = ?1 AND model_call_id = ?2 AND sequence = ?3",
                    params![run_key, model_call_id, sequence],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .optional()
                .map_err(|_| AdapterStoreError::Unavailable)?;
            if let Some((existing_digest, existing_frame)) = existing {
                return if existing_digest == frame_digest && existing_frame == frame {
                    Ok(())
                } else {
                    Err(AdapterStoreError::Conflict)
                };
            }
            let next_sequence = transaction
                .query_row(
                    "SELECT COALESCE(MAX(sequence), 0) + 1 FROM model_call_frame
                     WHERE run_key = ?1 AND model_call_id = ?2",
                    params![run_key, model_call_id],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            if next_sequence != sequence {
                return Err(AdapterStoreError::Conflict);
            }
            transaction
                .execute(
                    "INSERT INTO model_call_frame(
                       run_key, model_call_id, sequence, frame_digest, frame_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![run_key, model_call_id, sequence, frame_digest, frame],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            Ok(())
        })
    }

    /// Loads the exact response frames in sequence order and validates their
    /// durable envelope before exposing any provider payload to Core.
    pub(crate) fn load_model_call_frames(
        &self,
        run_key: &str,
        model_call_id: &str,
    ) -> Result<Vec<ModelChunkMessage>, AdapterStoreError> {
        if run_key.trim().is_empty() || model_call_id.trim().is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT sequence, frame_digest, frame_json
                 FROM model_call_frame
                 WHERE run_key = ?1 AND model_call_id = ?2
                 ORDER BY sequence",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let rows = statement
            .query_map(params![run_key, model_call_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let mut frames = Vec::new();
        for row in rows {
            let (sequence, expected_digest, frame) =
                row.map_err(|_| AdapterStoreError::Unavailable)?;
            if sequence <= 0 || frame.len() > MAX_MODEL_CALL_FRAME_BYTES {
                return Err(AdapterStoreError::Corrupt);
            }
            let actual_digest = format!("sha256:{:x}", Sha256::digest(&frame));
            if actual_digest != expected_digest {
                return Err(AdapterStoreError::Corrupt);
            }
            let chunk: ModelChunkMessage =
                serde_json::from_slice(&frame).map_err(|_| AdapterStoreError::Corrupt)?;
            frame_from_message(&ExecutionPortMessage::ModelChunkMessage(chunk.clone()))
                .map_err(|_| AdapterStoreError::Corrupt)?;
            if chunk.sequence.0 != sequence
                || chunk.sequence.0
                    != i64::try_from(frames.len().saturating_add(1))
                        .map_err(|_| AdapterStoreError::Corrupt)?
                || (chunk.is_final
                    && !frames.is_empty()
                    && frames
                        .last()
                        .is_some_and(|last: &ModelChunkMessage| last.is_final))
            {
                return Err(AdapterStoreError::Corrupt);
            }
            frames.push(chunk);
        }
        if frames
            .iter()
            .enumerate()
            .any(|(index, frame)| frame.is_final && index + 1 != frames.len())
        {
            return Err(AdapterStoreError::Corrupt);
        }
        Ok(frames)
    }

    /// Finds the durable model-call identity which owns one Provider exchange.
    ///
    /// The exchange id is durably bound to the Core-owned model-call id before
    /// the Provider open is sent. Looking up that binding lets a restarted
    /// Worker acknowledge a late response even when no response frame was
    /// written before the crash. The caller must still load and validate the
    /// complete frame set before using the result; this method is only an index
    /// lookup.
    pub(crate) fn model_call_for_exchange(
        &self,
        run_key: &str,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<String>, AdapterStoreError> {
        if run_key.trim().is_empty() || model_exchange_id.0.trim().is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        let connection = self.lock()?;
        let direct = connection
            .query_row(
                "SELECT model_call_id FROM model_call_ledger
                 WHERE run_key = ?1 AND model_exchange_id = ?2",
                params![run_key, model_exchange_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if let Some(model_call_id) = direct {
            if model_call_id.trim().is_empty() || model_call_id.len() > 512 {
                return Err(AdapterStoreError::Corrupt);
            }
            return Ok(Some(model_call_id));
        }
        // Every canonical row is bound before the Provider open. A store that
        // still contains an unbound row did not complete its one-time schema
        // migration, so fail closed instead of treating frame contents as an
        // identity index or retaining a compatibility lookup.
        let has_unbound = connection
            .query_row(
                "SELECT 1 FROM model_call_ledger
                 WHERE run_key = ?1 AND model_exchange_id IS NULL
                 LIMIT 1",
                params![run_key],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if has_unbound == Some(1) {
            return Err(AdapterStoreError::Corrupt);
        }
        Ok(None)
    }

    /// Moves a call from an in-flight provider exchange to `ProviderFinal`.
    /// Repeating this write for the same call is idempotent.
    pub(crate) fn mark_model_call_provider_final(
        &self,
        run_key: &str,
        model_call_id: &str,
    ) -> Result<(), AdapterStoreError> {
        self.update_model_call_phase(run_key, model_call_id, false)
    }

    /// Records the Core-side terminal commit after the exact `ProviderFinal`
    /// frames have been consumed. This is intentionally separate from the
    /// provider terminal write so a restart can replay those frames first.
    #[cfg(test)]
    pub(crate) fn commit_model_call(
        &self,
        run_key: &str,
        model_call_id: &str,
    ) -> Result<(), AdapterStoreError> {
        self.update_model_call_phase(run_key, model_call_id, true)
    }

    /// Commits every `ProviderFinal` call for one run in one transaction at the
    /// same terminal boundary observed by the adapter.
    pub(crate) fn commit_provider_final_model_calls(
        &self,
        run_key: &str,
    ) -> Result<(), AdapterStoreError> {
        if run_key.trim().is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        self.transaction(|transaction| {
            transaction
                .execute(
                    "UPDATE model_call_ledger SET core_committed = 1
                     WHERE run_key = ?1 AND provider_final = 1",
                    params![run_key],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            Ok(())
        })
    }

    fn update_model_call_phase(
        &self,
        run_key: &str,
        model_call_id: &str,
        commit_core: bool,
    ) -> Result<(), AdapterStoreError> {
        if run_key.trim().is_empty() || model_call_id.trim().is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        let connection = self.lock()?;
        let state = connection
            .query_row(
                "SELECT provider_final, core_committed FROM model_call_ledger
                 WHERE run_key = ?1 AND model_call_id = ?2",
                params![run_key, model_call_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let Some((provider_final, core_committed)) = state else {
            return Err(AdapterStoreError::Conflict);
        };
        if !matches!(provider_final, 0 | 1) || !matches!(core_committed, 0 | 1) {
            return Err(AdapterStoreError::Corrupt);
        }
        if commit_core && provider_final == 0 {
            return Err(AdapterStoreError::Conflict);
        }
        let (new_provider_final, new_core_committed) = if commit_core {
            (1, 1)
        } else {
            (1, core_committed)
        };
        connection
            .execute(
                "UPDATE model_call_ledger
                 SET completed = 1, provider_final = ?3, core_committed = ?4
                 WHERE run_key = ?1 AND model_call_id = ?2",
                params![
                    run_key,
                    model_call_id,
                    new_provider_final,
                    new_core_committed
                ],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(())
    }

    pub(crate) fn retain_approval_operation(
        &self,
        operation: &StoredApprovalOperation,
    ) -> Result<StoredApprovalOperation, AdapterStoreError> {
        operation.validate()?;
        self.transaction(|transaction| {
            let existing = load_approval_operation(transaction, &operation.approval_id)?;
            if let Some(existing) = existing {
                return if existing == *operation {
                    Ok(existing)
                } else {
                    Err(AdapterStoreError::Conflict)
                };
            }
            transaction
                .execute(
                    "INSERT INTO approval_operation(
                       approval_id, run_key, kernel_session_id, operation_kind,
                       operation_id, turn_id, request_digest, resolution_digest, state
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        operation.approval_id,
                        operation.run_key,
                        operation.kernel_session_id,
                        operation.operation_kind.as_str(),
                        operation.operation_id,
                        operation.turn_id,
                        operation.request_digest,
                        operation.resolution_digest,
                        operation.state.as_str(),
                    ],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            Ok(operation.clone())
        })
    }

    pub(crate) fn load_approval_operation(
        &self,
        approval_id: &str,
    ) -> Result<Option<StoredApprovalOperation>, AdapterStoreError> {
        load_approval_operation(&*self.lock()?, approval_id)
    }

    pub(crate) fn list_pending_approval_operations(
        &self,
        run_key: &str,
    ) -> Result<Vec<StoredApprovalOperation>, AdapterStoreError> {
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT approval_id, run_key, kernel_session_id, operation_kind,
                        operation_id, turn_id, request_digest, resolution_digest, state
                 FROM approval_operation
                 WHERE run_key = ?1 AND state = 'pending'
                 ORDER BY approval_id",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let rows = statement
            .query_map(params![run_key], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                ))
            })
            .map_err(|_| AdapterStoreError::Unavailable)?;
        rows.map(|row| {
            let (
                approval_id,
                operation_run_key,
                kernel_session_id,
                operation_kind,
                operation_id,
                turn_id,
                request_digest,
                resolution_digest,
                state,
            ) = row.map_err(|_| AdapterStoreError::Unavailable)?;
            let operation = StoredApprovalOperation {
                approval_id,
                run_key: operation_run_key,
                kernel_session_id,
                operation_kind: StoredApprovalOperationKind::parse(&operation_kind)?,
                operation_id,
                turn_id,
                request_digest,
                resolution_digest,
                state: StoredApprovalOperationState::parse(&state)?,
            };
            operation.validate()?;
            Ok(operation)
        })
        .collect()
    }

    /// Rebinds pending approvals to the fresh Kernel session created while
    /// recovering a durable Codex rollout.  A resumed Core thread has a new
    /// in-memory session identity, but its approval operation identity and
    /// request digest remain durable and must not change.
    pub(crate) fn rebind_pending_approval_operations(
        &self,
        run_key: &str,
        previous_kernel_session_id: &str,
        current_kernel_session_id: &str,
    ) -> Result<(), AdapterStoreError> {
        if run_key.is_empty()
            || previous_kernel_session_id.is_empty()
            || current_kernel_session_id.is_empty()
        {
            return Err(AdapterStoreError::Conflict);
        }
        if previous_kernel_session_id == current_kernel_session_id {
            return Ok(());
        }
        self.transaction(|transaction| {
            {
                let mut statement = transaction
                    .prepare(
                        "SELECT kernel_session_id FROM approval_operation
                         WHERE run_key = ?1 AND state = 'pending'",
                    )
                    .map_err(|_| AdapterStoreError::Unavailable)?;
                let session_ids = statement
                    .query_map(params![run_key], |row| row.get::<_, String>(0))
                    .map_err(|_| AdapterStoreError::Unavailable)?;
                for session_id in session_ids {
                    if session_id.map_err(|_| AdapterStoreError::Unavailable)?
                        != previous_kernel_session_id
                    {
                        return Err(AdapterStoreError::Conflict);
                    }
                }
            }
            transaction
                .execute(
                    "UPDATE approval_operation SET kernel_session_id = ?3
                     WHERE run_key = ?1 AND kernel_session_id = ?2 AND state = 'pending'",
                    params![
                        run_key,
                        previous_kernel_session_id,
                        current_kernel_session_id
                    ],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            Ok(())
        })
    }

    /// Rebinds pending input requests to the fresh Kernel session created
    /// while recovering a durable Codex rollout.  Input request identity and
    /// its request digest remain durable across restart; only the in-memory
    /// Kernel session handle changes.
    pub(crate) fn rebind_pending_input_operations(
        &self,
        run_key: &str,
        previous_kernel_session_id: &str,
        current_kernel_session_id: &str,
    ) -> Result<(), AdapterStoreError> {
        if run_key.is_empty()
            || previous_kernel_session_id.is_empty()
            || current_kernel_session_id.is_empty()
        {
            return Err(AdapterStoreError::Conflict);
        }
        if previous_kernel_session_id == current_kernel_session_id {
            return Ok(());
        }
        self.transaction(|transaction| {
            {
                let mut statement = transaction
                    .prepare(
                        "SELECT kernel_session_id FROM input_operation
                         WHERE run_key = ?1 AND state = 'pending'",
                    )
                    .map_err(|_| AdapterStoreError::Unavailable)?;
                let session_ids = statement
                    .query_map(params![run_key], |row| row.get::<_, String>(0))
                    .map_err(|_| AdapterStoreError::Unavailable)?;
                for session_id in session_ids {
                    if session_id.map_err(|_| AdapterStoreError::Unavailable)?
                        != previous_kernel_session_id
                    {
                        return Err(AdapterStoreError::Conflict);
                    }
                }
            }
            transaction
                .execute(
                    "UPDATE input_operation SET kernel_session_id = ?3
                     WHERE run_key = ?1 AND kernel_session_id = ?2 AND state = 'pending'",
                    params![
                        run_key,
                        previous_kernel_session_id,
                        current_kernel_session_id
                    ],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            Ok(())
        })
    }

    pub(crate) fn resolve_approval_operation(
        &self,
        approval_id: &str,
        request_digest: &str,
        resolution_digest: &str,
    ) -> Result<StoredApprovalOperation, AdapterStoreError> {
        validate_digest(resolution_digest)?;
        self.transaction(|transaction| {
            let mut operation = load_approval_operation(transaction, approval_id)?
                .ok_or(AdapterStoreError::Conflict)?;
            if operation.request_digest != request_digest {
                return Err(AdapterStoreError::Conflict);
            }
            if operation.state == StoredApprovalOperationState::Resolved {
                return if operation.resolution_digest.as_deref() == Some(resolution_digest) {
                    Ok(operation)
                } else {
                    Err(AdapterStoreError::Conflict)
                };
            }
            let changed = transaction
                .execute(
                    "UPDATE approval_operation SET state = 'resolved', resolution_digest = ?3
                     WHERE approval_id = ?1 AND request_digest = ?2 AND state = 'pending'",
                    params![approval_id, request_digest, resolution_digest],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            if changed != 1 {
                return Err(AdapterStoreError::Conflict);
            }
            operation.state = StoredApprovalOperationState::Resolved;
            operation.resolution_digest = Some(resolution_digest.to_owned());
            Ok(operation)
        })
    }

    pub(crate) fn retain_input_operation(
        &self,
        operation: &StoredInputOperation,
    ) -> Result<StoredInputOperation, AdapterStoreError> {
        operation.validate()?;
        self.transaction(|transaction| {
            let existing = load_input_operation(transaction, &operation.input_request_id)?;
            if let Some(existing) = existing {
                return if existing == *operation {
                    Ok(existing)
                } else {
                    Err(AdapterStoreError::Conflict)
                };
            }
            transaction
                .execute(
                    "INSERT INTO input_operation(
                       input_request_id, run_key, kernel_session_id, question_id, turn_id,
                       request_digest, resolution_digest, state
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        operation.input_request_id,
                        operation.run_key,
                        operation.kernel_session_id,
                        operation.question_id,
                        operation.turn_id,
                        operation.request_digest,
                        operation.resolution_digest,
                        operation.state.as_str(),
                    ],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            Ok(operation.clone())
        })
    }

    pub(crate) fn load_input_operation(
        &self,
        input_request_id: &str,
    ) -> Result<Option<StoredInputOperation>, AdapterStoreError> {
        load_input_operation(&*self.lock()?, input_request_id)
    }

    pub(crate) fn resolve_input_operation(
        &self,
        input_request_id: &str,
        request_digest: &str,
        resolution_digest: &str,
    ) -> Result<StoredInputOperation, AdapterStoreError> {
        validate_digest(resolution_digest)?;
        self.transaction(|transaction| {
            let mut operation = load_input_operation(transaction, input_request_id)?
                .ok_or(AdapterStoreError::Conflict)?;
            if operation.request_digest != request_digest {
                return Err(AdapterStoreError::Conflict);
            }
            if operation.state == StoredInputOperationState::Resolved {
                return if operation.resolution_digest.as_deref() == Some(resolution_digest) {
                    Ok(operation)
                } else {
                    Err(AdapterStoreError::Conflict)
                };
            }
            let changed = transaction
                .execute(
                    "UPDATE input_operation SET state = 'resolved', resolution_digest = ?3
                     WHERE input_request_id = ?1 AND request_digest = ?2 AND state = 'pending'",
                    params![input_request_id, request_digest, resolution_digest],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            if changed != 1 {
                return Err(AdapterStoreError::Conflict);
            }
            operation.state = StoredInputOperationState::Resolved;
            operation.resolution_digest = Some(resolution_digest.to_owned());
            Ok(operation)
        })
    }

    /// Replaces the complete trusted thread lineage for one leased run.
    ///
    /// Root lease replacement fences every previous child in one `SQLite`
    /// transaction.  The authority bytes are already validated by the bridge;
    /// the store keeps them opaque so it cannot accidentally reinterpret a
    /// lease or session identity.
    /// Replaces a root lineage while fencing every durable attempt for the
    /// same Job.  A successor root normally has a new canonical thread and
    /// run key, so fencing only the row at the new thread id would leave old
    /// child aliases recoverable after restart.  The authority is decoded
    /// here solely to identify that Job; all other authority checks remain in
    /// the bridge.
    pub(crate) fn replace_model_thread_lineage_for_job(
        &self,
        run_key: &str,
        thread_id: &str,
        job_id: &str,
        authority_json: &[u8],
    ) -> Result<(), AdapterStoreError> {
        self.replace_model_thread_lineage_inner(run_key, thread_id, Some(job_id), authority_json)
    }

    fn replace_model_thread_lineage_inner(
        &self,
        run_key: &str,
        thread_id: &str,
        job_id: Option<&str>,
        authority_json: &[u8],
    ) -> Result<(), AdapterStoreError> {
        if run_key.trim().is_empty() || thread_id.trim().is_empty() || authority_json.is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        if job_id.is_some_and(|job_id| job_id.trim().is_empty()) {
            return Err(AdapterStoreError::Conflict);
        }
        self.transaction(|transaction| {
            // Rebinding the exact same root is the normal restart path. Check
            // it before collecting stale same-job attempts so that durable
            // child edges for this run survive a root re-install.
            let existing_root = transaction
                .query_row(
                    "SELECT run_key, authority_json FROM model_thread_lineage
                     WHERE thread_id = ?1",
                    params![thread_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .optional()
                .map_err(|_| AdapterStoreError::Unavailable)?;
            if existing_root
                .as_ref()
                .is_some_and(|(existing_run_key, existing_authority)| {
                    existing_run_key == run_key && existing_authority == authority_json
                })
            {
                return Ok(());
            }
            if let Some(job_id) = job_id {
                let stale_run_keys = {
                    let mut statement = transaction
                        .prepare("SELECT run_key, authority_json FROM model_thread_lineage")
                        .map_err(|_| AdapterStoreError::Unavailable)?;
                    let rows = statement
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                        })
                        .map_err(|_| AdapterStoreError::Unavailable)?;
                    let mut stale = Vec::new();
                    for row in rows {
                        let (candidate_run_key, authority_json) =
                            row.map_err(|_| AdapterStoreError::Unavailable)?;
                        let authority: ModelLeaseAuthority =
                            serde_json::from_slice(&authority_json)
                                .map_err(|_| AdapterStoreError::Corrupt)?;
                        if authority.lease.job_id.0 == job_id
                            && candidate_run_key != run_key
                            && !stale.contains(&candidate_run_key)
                        {
                            stale.push(candidate_run_key);
                        }
                    }
                    stale
                };
                for stale_run_key in stale_run_keys {
                    transaction
                        .execute(
                            "DELETE FROM model_thread_lineage WHERE run_key = ?1",
                            params![stale_run_key],
                        )
                        .map_err(|_| AdapterStoreError::Unavailable)?;
                }
            }
            if let Some((existing_run_key, _)) = existing_root {
                // Replacing a root also fences every child from the previous
                // run.  Delete that old run before inserting the new root so
                // the thread-id primary key cannot leave a stale lineage edge
                // or turn a run-key change into a storage error.
                transaction
                    .execute(
                        "DELETE FROM model_thread_lineage WHERE run_key = ?1",
                        params![existing_run_key],
                    )
                    .map_err(|_| AdapterStoreError::Unavailable)?;
            }
            transaction
                .execute(
                    "DELETE FROM model_thread_lineage WHERE run_key = ?1",
                    params![run_key],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            transaction
                .execute(
                    "INSERT INTO model_thread_lineage(thread_id, run_key, authority_json)
                     VALUES (?1, ?2, ?3)",
                    params![thread_id, run_key, authority_json],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            Ok(())
        })
    }

    /// Retains one child lineage edge exactly once.
    pub(crate) fn retain_model_thread_lineage(
        &self,
        run_key: &str,
        thread_id: &str,
        authority_json: &[u8],
    ) -> Result<(), AdapterStoreError> {
        if run_key.trim().is_empty() || thread_id.trim().is_empty() || authority_json.is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        self.transaction(|transaction| {
            let existing = transaction
                .query_row(
                    "SELECT run_key, authority_json FROM model_thread_lineage
                     WHERE thread_id = ?1",
                    params![thread_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .optional()
                .map_err(|_| AdapterStoreError::Unavailable)?;
            if let Some((existing_run_key, existing_authority)) = existing {
                if existing_run_key == run_key && existing_authority == authority_json {
                    return Ok(());
                }
                return Err(AdapterStoreError::Conflict);
            }
            transaction
                .execute(
                    "INSERT INTO model_thread_lineage(thread_id, run_key, authority_json)
                     VALUES (?1, ?2, ?3)",
                    params![thread_id, run_key, authority_json],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
            Ok(())
        })
    }

    pub(crate) fn load_model_thread_lineage(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, Vec<u8>)>, AdapterStoreError> {
        self.lock()?
            .query_row(
                "SELECT run_key, authority_json FROM model_thread_lineage
                 WHERE thread_id = ?1",
                params![thread_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)
    }

    /// Loads every durable lineage edge for one immutable run identity.
    ///
    /// A restarted Worker may observe a child `ModelPort` request before its
    /// process-local root alias has been rebuilt.  Returning the complete run
    /// set lets the bridge recover the root authority without trusting a job
    /// id or a lease presented by the child alone.
    pub(crate) fn load_model_thread_lineage_for_run(
        &self,
        run_key: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, AdapterStoreError> {
        if run_key.trim().is_empty() {
            return Err(AdapterStoreError::Conflict);
        }
        let connection = self.lock()?;
        let mut statement = connection
            .prepare(
                "SELECT thread_id, authority_json FROM model_thread_lineage
                 WHERE run_key = ?1 ORDER BY thread_id",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let rows = statement
            .query_map(params![run_key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|_| AdapterStoreError::Unavailable)?;
        rows.map(|row| row.map_err(|_| AdapterStoreError::Unavailable))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn remove_model_thread_lineage(
        &self,
        run_key: &str,
    ) -> Result<(), AdapterStoreError> {
        self.lock()?
            .execute(
                "DELETE FROM model_thread_lineage WHERE run_key = ?1",
                params![run_key],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(())
    }

    pub(crate) fn transaction<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> Result<T, AdapterStoreError>,
    ) -> Result<T, AdapterStoreError> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let result = operation(&transaction)?;
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(result)
    }

    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Connection>, AdapterStoreError> {
        self.connection
            .lock()
            .map_err(|_| AdapterStoreError::Unavailable)
    }
}

fn validate_model_call_inputs(
    run_key: &str,
    model_call_id: &str,
    model_exchange_id: &ModelExchangeId,
) -> Result<(), AdapterStoreError> {
    if run_key.trim().is_empty()
        || model_call_id.trim().is_empty()
        || model_call_id.len() > 512
        || model_exchange_id.0.trim().is_empty()
        || model_exchange_id.0.len() > 512
    {
        return Err(AdapterStoreError::Conflict);
    }
    Ok(())
}

fn validate_performance_identity(
    run_key: &str,
    operation_id: &str,
    observed_at: &winwincode_domain::Instant,
) -> Result<(), AdapterStoreError> {
    if run_key.trim().is_empty()
        || operation_id.trim().is_empty()
        || operation_id.len() > 512
        || observed_at.0.trim().is_empty()
        || observed_at.0.len() > 128
    {
        return Err(AdapterStoreError::Conflict);
    }
    Ok(())
}

fn validate_metric(value: i64) -> Result<(), AdapterStoreError> {
    if (0..=MAX_SAFE_METRIC).contains(&value) {
        Ok(())
    } else {
        Err(AdapterStoreError::Conflict)
    }
}

fn valid_sha256_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn performance_report_values(report: &PerformanceBaselineReport) -> impl Iterator<Item = i64> {
    [
        report.primary_model_call_count,
        report.primary_model_input_tokens,
        report.primary_model_cached_tokens,
        report.primary_model_output_tokens,
        report.primary_model_wait_ms,
        report.tool_call_count,
        report.patch_call_count,
        report.patch_apply_ms,
        report.files_changed,
        report.validation_ms,
        report.observer_call_count,
        report.observer_wait_ms,
        report.repair_rounds,
        report.turn_count,
        report.total_runtime_ms,
    ]
    .into_iter()
}

fn ensure_model_call_phase_columns(connection: &Connection) -> Result<(), AdapterStoreError> {
    let mut statement = connection
        .prepare("PRAGMA table_info(model_call_ledger)")
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let mut has_provider_final = false;
    let mut has_core_committed = false;
    let mut has_model_exchange_id = false;
    for column in columns {
        match column.map_err(|_| AdapterStoreError::Unavailable)?.as_str() {
            "provider_final" => has_provider_final = true,
            "core_committed" => has_core_committed = true,
            "model_exchange_id" => has_model_exchange_id = true,
            _ => {}
        }
    }
    drop(statement);
    if !has_provider_final {
        connection
            .execute(
                "ALTER TABLE model_call_ledger
                 ADD COLUMN provider_final INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
    }
    if !has_core_committed {
        connection
            .execute(
                "ALTER TABLE model_call_ledger
                 ADD COLUMN core_committed INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
    }
    if !has_model_exchange_id {
        connection
            .execute(
                "ALTER TABLE model_call_ledger
                 ADD COLUMN model_exchange_id TEXT",
                [],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
    }
    migrate_model_exchange_bindings(connection)?;
    connection
        .execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS model_call_ledger_exchange_idx
             ON model_call_ledger(run_key, model_exchange_id)
             WHERE model_exchange_id IS NOT NULL",
            [],
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
    // Stores created before the explicit two-phase ledger used `completed`
    // for the provider terminal write. They have no response-frame table to
    // replay, so treat those historical rows as already CoreCommitted rather
    // than reopening a provider call with an ambiguous response.
    connection
        .execute(
            "UPDATE model_call_ledger
             SET provider_final = 1, core_committed = 1
             WHERE completed = 1 AND provider_final = 0",
            [],
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
    Ok(())
}

/// Migrates retained response frames into the canonical exchange binding once
/// when opening a store created before `model_exchange_id` was persisted. The
/// old frame scan is deliberately not a runtime lookup: a frame-backed row is
/// either backfilled here or the store fails closed as corrupt.
fn migrate_model_exchange_bindings(connection: &Connection) -> Result<(), AdapterStoreError> {
    let mut statement = connection
        .prepare(
            "SELECT run_key, model_call_id
             FROM model_call_ledger
             WHERE model_exchange_id IS NULL
             ORDER BY run_key, model_call_id",
        )
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| AdapterStoreError::Unavailable)?;
    let unbound = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    drop(statement);

    for (run_key, model_call_id) in unbound {
        let mut frames = connection
            .prepare(
                "SELECT frame_json
                 FROM model_call_frame
                 WHERE run_key = ?1 AND model_call_id = ?2
                 ORDER BY sequence",
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let frame_rows = frames
            .query_map(params![run_key, model_call_id], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let mut exchange = None;
        for frame in frame_rows {
            let frame = frame.map_err(|_| AdapterStoreError::Unavailable)?;
            if frame.len() > MAX_MODEL_CALL_FRAME_BYTES {
                return Err(AdapterStoreError::Corrupt);
            }
            let chunk: ModelChunkMessage =
                serde_json::from_slice(&frame).map_err(|_| AdapterStoreError::Corrupt)?;
            frame_from_message(&ExecutionPortMessage::ModelChunkMessage(chunk.clone()))
                .map_err(|_| AdapterStoreError::Corrupt)?;
            let candidate = chunk.model_exchange_id.0;
            if candidate.trim().is_empty() || candidate.len() > 512 {
                return Err(AdapterStoreError::Corrupt);
            }
            if exchange
                .as_deref()
                .is_some_and(|existing| existing != candidate)
            {
                return Err(AdapterStoreError::Corrupt);
            }
            exchange = Some(candidate);
        }
        drop(frames);
        let Some(exchange) = exchange else {
            // An old in-flight row has no response witness. The next retry
            // binds it through claim_model_call before any Provider I/O.
            continue;
        };
        let owner = connection
            .query_row(
                "SELECT model_call_id
                 FROM model_call_ledger
                 WHERE run_key = ?1 AND model_exchange_id = ?2",
                params![run_key, exchange],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if owner.is_some_and(|owner| owner != model_call_id) {
            return Err(AdapterStoreError::Corrupt);
        }
        connection
            .execute(
                "UPDATE model_call_ledger
                 SET model_exchange_id = ?3
                 WHERE run_key = ?1 AND model_call_id = ?2
                   AND model_exchange_id IS NULL",
                params![run_key, model_call_id, exchange],
            )
            .map_err(|_| AdapterStoreError::Corrupt)?;
    }
    Ok(())
}

fn decode_model_call_phase(
    completed: i64,
    provider_final: i64,
    core_committed: i64,
) -> Result<ModelCallPhase, AdapterStoreError> {
    if !matches!(completed, 0 | 1)
        || !matches!(provider_final, 0 | 1)
        || !matches!(core_committed, 0 | 1)
    {
        return Err(AdapterStoreError::Corrupt);
    }
    match (completed, provider_final, core_committed) {
        (0, 0, 0) => Ok(ModelCallPhase::InFlight),
        (1, 1, 0) => Ok(ModelCallPhase::ProviderFinal),
        (1, 1, 1) => Ok(ModelCallPhase::CoreCommitted),
        _ => Err(AdapterStoreError::Corrupt),
    }
}

impl fmt::Debug for AdapterStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdapterStore")
            .field("database", &"private")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoredApprovalOperationKind {
    Exec,
    Patch,
}

impl StoredApprovalOperationKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Patch => "patch",
        }
    }

    fn parse(value: &str) -> Result<Self, AdapterStoreError> {
        match value {
            "exec" => Ok(Self::Exec),
            "patch" => Ok(Self::Patch),
            _ => Err(AdapterStoreError::Corrupt),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoredApprovalOperationState {
    Pending,
    Resolved,
}

impl StoredApprovalOperationState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resolved => "resolved",
        }
    }

    fn parse(value: &str) -> Result<Self, AdapterStoreError> {
        match value {
            "pending" => Ok(Self::Pending),
            "resolved" => Ok(Self::Resolved),
            _ => Err(AdapterStoreError::Corrupt),
        }
    }
}

/// Secret-free durable mapping from one public approval to the exact Kernel operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredApprovalOperation {
    pub(crate) approval_id: String,
    pub(crate) run_key: String,
    pub(crate) kernel_session_id: String,
    pub(crate) operation_kind: StoredApprovalOperationKind,
    pub(crate) operation_id: String,
    pub(crate) turn_id: Option<String>,
    pub(crate) request_digest: String,
    pub(crate) resolution_digest: Option<String>,
    pub(crate) state: StoredApprovalOperationState,
}

impl StoredApprovalOperation {
    fn validate(&self) -> Result<(), AdapterStoreError> {
        if self.approval_id.is_empty()
            || self.run_key.is_empty()
            || self.kernel_session_id.is_empty()
            || self.operation_id.is_empty()
            || self.turn_id.as_ref().is_some_and(String::is_empty)
            || validate_digest(&self.request_digest).is_err()
            || self
                .resolution_digest
                .as_deref()
                .is_some_and(|digest| validate_digest(digest).is_err())
            || match self.state {
                StoredApprovalOperationState::Pending => self.resolution_digest.is_some(),
                StoredApprovalOperationState::Resolved => self.resolution_digest.is_none(),
            }
        {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoredInputOperationState {
    Pending,
    Resolved,
}

impl StoredInputOperationState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resolved => "resolved",
        }
    }

    fn parse(value: &str) -> Result<Self, AdapterStoreError> {
        match value {
            "pending" => Ok(Self::Pending),
            "resolved" => Ok(Self::Resolved),
            _ => Err(AdapterStoreError::Corrupt),
        }
    }
}

/// Secret-free durable mapping from one public input request to the exact Kernel turn/question.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredInputOperation {
    pub(crate) input_request_id: String,
    pub(crate) run_key: String,
    pub(crate) kernel_session_id: String,
    pub(crate) question_id: String,
    pub(crate) turn_id: String,
    pub(crate) request_digest: String,
    pub(crate) resolution_digest: Option<String>,
    pub(crate) state: StoredInputOperationState,
}

impl StoredInputOperation {
    fn validate(&self) -> Result<(), AdapterStoreError> {
        if self.input_request_id.is_empty()
            || self.run_key.is_empty()
            || self.kernel_session_id.is_empty()
            || self.question_id.is_empty()
            || self.turn_id.is_empty()
            || validate_digest(&self.request_digest).is_err()
            || self
                .resolution_digest
                .as_deref()
                .is_some_and(|digest| validate_digest(digest).is_err())
            || match self.state {
                StoredInputOperationState::Pending => self.resolution_digest.is_some(),
                StoredInputOperationState::Resolved => self.resolution_digest.is_none(),
            }
        {
            return Err(AdapterStoreError::Conflict);
        }
        Ok(())
    }
}

fn load_approval_operation(
    connection: &Connection,
    approval_id: &str,
) -> Result<Option<StoredApprovalOperation>, AdapterStoreError> {
    let stored = connection
        .query_row(
            "SELECT run_key, kernel_session_id, operation_kind, operation_id,
                    turn_id, request_digest, resolution_digest, state
             FROM approval_operation WHERE approval_id = ?1",
            params![approval_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    stored
        .map(
            |(
                run_key,
                kernel_session_id,
                operation_kind,
                operation_id,
                turn_id,
                request_digest,
                resolution_digest,
                state,
            )| {
                let operation = StoredApprovalOperation {
                    approval_id: approval_id.to_owned(),
                    run_key,
                    kernel_session_id,
                    operation_kind: StoredApprovalOperationKind::parse(&operation_kind)?,
                    operation_id,
                    turn_id,
                    request_digest,
                    resolution_digest,
                    state: StoredApprovalOperationState::parse(&state)?,
                };
                operation.validate()?;
                Ok(operation)
            },
        )
        .transpose()
}

fn load_input_operation(
    connection: &Connection,
    input_request_id: &str,
) -> Result<Option<StoredInputOperation>, AdapterStoreError> {
    let stored = connection
        .query_row(
            "SELECT run_key, kernel_session_id, question_id, turn_id,
                    request_digest, resolution_digest, state
             FROM input_operation WHERE input_request_id = ?1",
            params![input_request_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    stored
        .map(
            |(
                run_key,
                kernel_session_id,
                question_id,
                turn_id,
                request_digest,
                resolution_digest,
                state,
            )| {
                let operation = StoredInputOperation {
                    input_request_id: input_request_id.to_owned(),
                    run_key,
                    kernel_session_id,
                    question_id,
                    turn_id,
                    request_digest,
                    resolution_digest,
                    state: StoredInputOperationState::parse(&state)?,
                };
                operation.validate()?;
                Ok(operation)
            },
        )
        .transpose()
}

fn validate_digest(value: &str) -> Result<(), AdapterStoreError> {
    if value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(())
    } else {
        Err(AdapterStoreError::Conflict)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AdapterStoreError {
    Unavailable,
    Corrupt,
    Conflict,
}

impl fmt::Display for AdapterStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "Codex adapter state is unavailable",
            Self::Corrupt => "Codex adapter state is invalid",
            Self::Conflict => "Codex adapter state changed concurrently",
        })
    }
}

impl std::error::Error for AdapterStoreError {}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<(), AdapterStoreError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| AdapterStoreError::Unavailable)
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> Result<(), AdapterStoreError> {
    Ok(())
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<(), AdapterStoreError> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map(|_| ())
        .map_err(|_| AdapterStoreError::Unavailable)
}

#[cfg(not(unix))]
fn create_private_file(_path: &Path) -> Result<(), AdapterStoreError> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<(), AdapterStoreError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| AdapterStoreError::Unavailable)
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<(), AdapterStoreError> {
    Ok(())
}

fn restrict_file_if_present(path: &Path) -> Result<(), AdapterStoreError> {
    if path.exists() {
        restrict_file(path)?;
    }
    Ok(())
}

impl ModelCursorStore for AdapterStore {
    type Error = AdapterStoreError;

    fn load(
        &mut self,
        stream: &ReplayStreamKey,
    ) -> Result<Option<ModelCursorSnapshot>, Self::Error> {
        let connection = self.lock()?;
        load_model_cursor(&connection, stream)
    }

    fn record_delivery(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        fingerprint: &ModelChunkFingerprint,
        termination: Option<ModelTerminationReason>,
    ) -> Result<(), Self::Error> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let mut snapshot = load_model_cursor(&transaction, stream)?.unwrap_or_default();
        snapshot
            .validate()
            .map_err(|_| AdapterStoreError::Corrupt)?;
        if snapshot.confirmed_sequence != expected_sequence
            || fingerprint.sequence != expected_sequence.saturating_add(1)
            || snapshot.termination.is_some()
        {
            return Err(AdapterStoreError::Conflict);
        }
        snapshot.confirmed_sequence = fingerprint.sequence;
        snapshot.frames.push(fingerprint.clone());
        snapshot.termination = termination;
        snapshot
            .validate()
            .map_err(|_| AdapterStoreError::Corrupt)?;
        write_model_cursor(&transaction, stream, &snapshot)?;
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)
    }

    fn terminate(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        reason: ModelTerminationReason,
    ) -> Result<(), Self::Error> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let mut snapshot = load_model_cursor(&transaction, stream)?.unwrap_or_default();
        snapshot
            .validate()
            .map_err(|_| AdapterStoreError::Corrupt)?;
        if snapshot.confirmed_sequence != expected_sequence || snapshot.termination.is_some() {
            return Err(AdapterStoreError::Conflict);
        }
        snapshot.termination = Some(reason);
        snapshot
            .validate()
            .map_err(|_| AdapterStoreError::Corrupt)?;
        write_model_cursor(&transaction, stream, &snapshot)?;
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)
    }

    fn record_cancellation_intent(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        fingerprint: &ModelCancellationFingerprint,
    ) -> Result<(), Self::Error> {
        if fingerprint.phase != ModelCancellationPhase::Intent {
            return Err(AdapterStoreError::Conflict);
        }
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let mut snapshot = load_model_cursor(&transaction, stream)?.unwrap_or_default();
        snapshot
            .validate()
            .map_err(|_| AdapterStoreError::Corrupt)?;
        if snapshot.confirmed_sequence != expected_sequence
            || snapshot.termination.is_some()
            || snapshot.cancellation.is_some()
        {
            return Err(AdapterStoreError::Conflict);
        }
        snapshot.cancellation = Some(fingerprint.clone());
        snapshot
            .validate()
            .map_err(|_| AdapterStoreError::Corrupt)?;
        write_model_cursor(&transaction, stream, &snapshot)?;
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)
    }

    fn complete_cancellation(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        fingerprint: &ModelCancellationFingerprint,
    ) -> Result<(), Self::Error> {
        if fingerprint.phase != ModelCancellationPhase::Intent {
            return Err(AdapterStoreError::Conflict);
        }
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let mut snapshot = load_model_cursor(&transaction, stream)?.unwrap_or_default();
        snapshot
            .validate()
            .map_err(|_| AdapterStoreError::Corrupt)?;
        if snapshot.confirmed_sequence != expected_sequence
            || snapshot.termination.is_some()
            || snapshot.cancellation.as_ref() != Some(fingerprint)
        {
            return Err(AdapterStoreError::Conflict);
        }
        let mut applied = fingerprint.clone();
        applied.phase = ModelCancellationPhase::Applied;
        snapshot.cancellation = Some(applied);
        snapshot.termination = Some(ModelTerminationReason::Cancelled);
        snapshot
            .validate()
            .map_err(|_| AdapterStoreError::Corrupt)?;
        write_model_cursor(&transaction, stream, &snapshot)?;
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)
    }
}

fn load_model_cursor(
    connection: &Connection,
    stream: &ReplayStreamKey,
) -> Result<Option<ModelCursorSnapshot>, AdapterStoreError> {
    load_snapshot(connection, "model_cursor", stream.as_str())
}

fn write_model_cursor(
    transaction: &Transaction<'_>,
    stream: &ReplayStreamKey,
    snapshot: &ModelCursorSnapshot,
) -> Result<(), AdapterStoreError> {
    write_snapshot(transaction, "model_cursor", stream.as_str(), snapshot)
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredReplaySnapshot {
    ack_sequence: u64,
    highest_sequence: u64,
    events: Vec<StoredReplayFrame>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredReplayFrame {
    event_id: String,
    sequence: u64,
    digest: String,
    frame: Vec<u8>,
}

impl From<StoredReplaySnapshot> for ReplaySnapshot {
    fn from(snapshot: StoredReplaySnapshot) -> Self {
        Self {
            ack_sequence: snapshot.ack_sequence,
            highest_sequence: snapshot.highest_sequence,
            events: snapshot
                .events
                .into_iter()
                .map(|frame| {
                    ReplayFrame::new(frame.event_id, frame.sequence, frame.digest, frame.frame)
                })
                .collect(),
        }
    }
}

impl From<&ReplaySnapshot> for StoredReplaySnapshot {
    fn from(snapshot: &ReplaySnapshot) -> Self {
        Self {
            ack_sequence: snapshot.ack_sequence,
            highest_sequence: snapshot.highest_sequence,
            events: snapshot
                .events
                .iter()
                .map(|frame| StoredReplayFrame {
                    event_id: frame.event_id.clone(),
                    sequence: frame.sequence,
                    digest: frame.digest.clone(),
                    frame: frame.frame.clone(),
                })
                .collect(),
        }
    }
}

impl ReplayStore for AdapterStore {
    type Error = AdapterStoreError;

    fn load(&mut self, stream: &ReplayStreamKey) -> Result<Option<ReplaySnapshot>, Self::Error> {
        let connection = self.lock()?;
        let stored: Option<StoredReplaySnapshot> =
            load_snapshot(&connection, "runtime_replay", stream.as_str())?;
        let snapshot = stored.map(ReplaySnapshot::from);
        if let Some(snapshot) = &snapshot {
            snapshot
                .validate()
                .map_err(|_| AdapterStoreError::Corrupt)?;
        }
        Ok(snapshot)
    }

    fn append(
        &mut self,
        stream: &ReplayStreamKey,
        expected_highest_sequence: u64,
        frame: &ReplayFrame,
    ) -> Result<(), Self::Error> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let stored: Option<StoredReplaySnapshot> =
            load_snapshot(&transaction, "runtime_replay", stream.as_str())?;
        let mut snapshot = stored.map_or_else(ReplaySnapshot::default, ReplaySnapshot::from);
        snapshot
            .validate()
            .map_err(|_| AdapterStoreError::Corrupt)?;
        if snapshot.highest_sequence != expected_highest_sequence
            || frame.sequence != expected_highest_sequence.saturating_add(1)
        {
            return Err(AdapterStoreError::Conflict);
        }
        snapshot.highest_sequence = frame.sequence;
        snapshot.events.push(frame.clone());
        snapshot
            .validate()
            .map_err(|_| AdapterStoreError::Corrupt)?;
        write_snapshot(
            &transaction,
            "runtime_replay",
            stream.as_str(),
            &StoredReplaySnapshot::from(&snapshot),
        )?;
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)
    }
}

impl ReplayAcknowledgementStore for AdapterStore {
    fn record_acknowledgement(
        &mut self,
        stream: &ReplayStreamKey,
        expected_ack_sequence: u64,
        ack_sequence: u64,
    ) -> Result<(), Self::Error> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let stored: Option<StoredReplaySnapshot> =
            load_snapshot(&transaction, "runtime_replay", stream.as_str())?;
        let mut snapshot = stored.map_or_else(ReplaySnapshot::default, ReplaySnapshot::from);
        snapshot
            .validate()
            .map_err(|_| AdapterStoreError::Corrupt)?;
        if snapshot.ack_sequence != expected_ack_sequence
            || ack_sequence < expected_ack_sequence
            || ack_sequence > snapshot.highest_sequence
        {
            return Err(AdapterStoreError::Conflict);
        }
        snapshot.ack_sequence = ack_sequence;
        snapshot
            .validate()
            .map_err(|_| AdapterStoreError::Corrupt)?;
        write_snapshot(
            &transaction,
            "runtime_replay",
            stream.as_str(),
            &StoredReplaySnapshot::from(&snapshot),
        )?;
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)
    }
}

fn load_snapshot<T: DeserializeOwned>(
    connection: &Connection,
    table: &str,
    key: &str,
) -> Result<Option<T>, AdapterStoreError> {
    let query = format!("SELECT snapshot_json FROM {table} WHERE stream_key = ?1");
    let bytes = connection
        .query_row(&query, params![key], |row| row.get::<_, Vec<u8>>(0))
        .optional()
        .map_err(|_| AdapterStoreError::Unavailable)?;
    bytes
        .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| AdapterStoreError::Corrupt))
        .transpose()
}

fn write_snapshot<T: Serialize>(
    transaction: &Transaction<'_>,
    table: &str,
    key: &str,
    snapshot: &T,
) -> Result<(), AdapterStoreError> {
    let bytes = serde_json::to_vec(snapshot).map_err(|_| AdapterStoreError::Corrupt)?;
    let statement = format!(
        "INSERT INTO {table}(stream_key, snapshot_json) VALUES (?1, ?2)
         ON CONFLICT(stream_key) DO UPDATE SET snapshot_json = excluded.snapshot_json"
    );
    transaction
        .execute(&statement, params![key, bytes])
        .map_err(|_| AdapterStoreError::Unavailable)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AdapterStore, AdapterStoreError, DATABASE_FILE, ModelCallPhase, StoredApprovalOperation,
        StoredApprovalOperationKind, StoredApprovalOperationState,
    };
    use crate::performance::{PerformanceOperationCompletion, PerformanceOperationKind};
    use std::path::{Path, PathBuf};
    use winwincode_domain::Sha256Digest;
    use winwincode_domain::{ExecutionEventId, ExecutionSequence, Instant, ModelExchangeId};
    use winwincode_execution_port::runtime_trace_outbox::{
        ExecutionMode, ObserverMode, PerformanceBaselineReport,
    };

    fn test_root(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-codex-store-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    fn record_primary_performance_fixture(
        store: &AdapterStore,
        started: &Instant,
        completed: &Instant,
    ) {
        store
            .register_performance_run("run", ExecutionMode::DelegatedPatch, ObserverMode::Always)
            .expect("freeze modes");
        store
            .save_run(
                "run",
                &serde_json::json!({
                    "lastActivityAt": "2030-01-01T00:00:03.000Z",
                    "finalCandidateFreeze": {
                        "counters": { "elapsedMillis": 5_000 }
                    }
                }),
            )
            .expect("retain run timing");
        store
            .claim_model_call(
                "run",
                "model-call",
                &ModelExchangeId("mdl_PERFORMANCEAAAAAAAAAAAAAAA".to_owned()),
                &Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            )
            .expect("claim primary model call");
        store
            .record_performance_start(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "model-call",
                started,
            )
            .expect("start model call");
        store
            .record_performance_completion(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "model-call",
                completed,
                PerformanceOperationCompletion {
                    duration_millis: Some(800),
                    input_tokens: 90,
                    cached_tokens: 10,
                    output_tokens: 20,
                    actual_cost_microunits: Some(30),
                },
            )
            .expect("complete model call");
        store
            .record_performance_completion(
                "run",
                PerformanceOperationKind::PrimaryModel,
                "model-call",
                completed,
                PerformanceOperationCompletion {
                    duration_millis: Some(9_999),
                    input_tokens: 9_999,
                    cached_tokens: 9_999,
                    output_tokens: 9_999,
                    actual_cost_microunits: Some(9_999),
                },
            )
            .expect("duplicate completion is ignored");
        store
            .mark_model_call_provider_final("run", "model-call")
            .expect("retain primary Provider final");
    }

    fn performance_report_fixture(root: &Path) -> PerformanceBaselineReport {
        let started = Instant("2030-01-01T00:00:00.000Z".to_owned());
        let completed = Instant("2030-01-01T00:00:01.000Z".to_owned());
        let store = AdapterStore::open(root).expect("open performance store");
        record_primary_performance_fixture(&store, &started, &completed);
        for kind in [
            PerformanceOperationKind::Tool,
            PerformanceOperationKind::Patch,
            PerformanceOperationKind::Validation,
            PerformanceOperationKind::Observer,
            PerformanceOperationKind::Repair,
            PerformanceOperationKind::Turn,
        ] {
            store
                .record_performance_start("run", kind, kind.as_str(), &started)
                .expect("start aggregate operation");
        }
        for kind in [
            PerformanceOperationKind::Tool,
            PerformanceOperationKind::Patch,
            PerformanceOperationKind::Validation,
            PerformanceOperationKind::Observer,
            PerformanceOperationKind::Repair,
        ] {
            let is_observer = matches!(kind, PerformanceOperationKind::Observer);
            store
                .record_performance_completion(
                    "run",
                    kind,
                    kind.as_str(),
                    &completed,
                    PerformanceOperationCompletion {
                        duration_millis: Some(100),
                        input_tokens: i64::from(is_observer) * 4,
                        output_tokens: i64::from(is_observer),
                        actual_cost_microunits: is_observer.then_some(7),
                        ..PerformanceOperationCompletion::default()
                    },
                )
                .expect("complete aggregate operation");
        }
        let path = Sha256Digest(format!("sha256:{}", "a".repeat(64)));
        store
            .record_performance_changed_file("run", &path)
            .expect("record changed file");
        store
            .record_performance_changed_file("run", &path)
            .expect("changed file replay");
        let delegated = store
            .delegated_performance_totals("run")
            .expect("delegated totals");
        assert_eq!(delegated.primary_model_calls, 1);
        assert_eq!(delegated.observer_calls, 1);
        assert_eq!(delegated.total_tokens, 125);
        assert_eq!(delegated.total_cost_microunits, 37);
        assert!(delegated.cost_complete);
        assert_eq!(
            store
                .performance_total_runtime("run", &Instant("2030-01-01T00:00:05.000Z".to_owned()),)
                .expect("whole-run wall time"),
            5_000
        );
        store.performance_report("run", 1_000).expect("report")
    }

    #[test]
    fn delegated_observer_charge_is_counted_or_fails_closed() {
        for (name, actual_cost_microunits, expected_cost, cost_complete) in [
            ("charged", Some(17), 17, true),
            ("missing-charge", None, 0, false),
        ] {
            let root = test_root(name);
            let store = AdapterStore::open(&root).expect("open store");
            let observed_at = Instant("2030-01-01T00:00:01.000Z".to_owned());
            store
                .record_performance_completion(
                    "run",
                    PerformanceOperationKind::Observer,
                    "batch",
                    &observed_at,
                    PerformanceOperationCompletion {
                        input_tokens: 7,
                        output_tokens: 2,
                        actual_cost_microunits,
                        ..PerformanceOperationCompletion::default()
                    },
                )
                .expect("retain Observer settlement");
            store
                .record_performance_completion(
                    "run",
                    PerformanceOperationKind::Observer,
                    "batch",
                    &observed_at,
                    PerformanceOperationCompletion {
                        input_tokens: 9_999,
                        output_tokens: 9_999,
                        actual_cost_microunits: Some(9_999),
                        ..PerformanceOperationCompletion::default()
                    },
                )
                .expect("Observer settlement replay");

            let totals = store
                .delegated_performance_totals("run")
                .expect("read delegated totals");
            assert_eq!(totals.observer_calls, 1, "{name}");
            assert_eq!(totals.pending_model_calls, 0, "{name}");
            assert_eq!(totals.total_tokens, 9, "{name}");
            assert_eq!(totals.total_cost_microunits, expected_cost, "{name}");
            assert_eq!(totals.cost_complete, cost_complete, "{name}");
        }

        for (name, kind, expected_primary, expected_observer) in [
            (
                "pending-primary-charge",
                PerformanceOperationKind::PrimaryModel,
                1,
                0,
            ),
            (
                "pending-observer-charge",
                PerformanceOperationKind::Observer,
                0,
                1,
            ),
        ] {
            let root = test_root(name);
            let store = AdapterStore::open(&root).expect("open store");
            store
                .record_performance_start(
                    "run",
                    kind,
                    "operation",
                    &Instant("2030-01-01T00:00:01.000Z".to_owned()),
                )
                .expect("reserve pending model operation");
            let pending = store
                .delegated_performance_totals("run")
                .expect("read pending delegated totals");
            assert_eq!(pending.primary_model_calls, expected_primary, "{name}");
            assert_eq!(pending.observer_calls, expected_observer, "{name}");
            assert_eq!(pending.pending_model_calls, 1, "{name}");
            assert!(!pending.cost_complete, "{name}");
        }
    }

    #[test]
    fn stable_model_call_ledger_allows_parallel_calls_and_exact_restart_replay() {
        let root = test_root("stable-model-calls");
        let first = Sha256Digest(format!("sha256:{}", "1".repeat(64)));
        let second = Sha256Digest(format!("sha256:{}", "2".repeat(64)));
        let exchange_a = ModelExchangeId("mdl_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned());
        let exchange_b = ModelExchangeId("mdl_BBBBBBBBBBBBBBBBBBBBBBBBBB".to_owned());
        {
            let store = AdapterStore::open(&root).expect("open store");
            assert_eq!(
                store
                    .claim_model_call("run", "thread-a:call-a", &exchange_a, &first,)
                    .expect("first call"),
                1
            );
            assert_eq!(
                store
                    .claim_model_call("run", "thread-a:call-b", &exchange_b, &second,)
                    .expect("parallel second call"),
                2
            );
            assert_eq!(
                store
                    .model_call_for_exchange("run", &exchange_a)
                    .expect("durable first exchange owner"),
                Some("thread-a:call-a".to_owned())
            );
            assert_eq!(
                store
                    .model_call_for_exchange("run", &exchange_b)
                    .expect("durable second exchange owner"),
                Some("thread-a:call-b".to_owned())
            );
            assert_eq!(
                store
                    .claim_model_call("run", "thread-a:call-a", &exchange_a, &first,)
                    .expect("same call replay"),
                1
            );
            assert_eq!(
                store.claim_model_call("run", "thread-a:call-a", &exchange_a, &second,),
                Err(AdapterStoreError::Conflict)
            );
            assert_eq!(
                store.claim_model_call("run", "thread-a:call-c", &exchange_a, &first,),
                Err(AdapterStoreError::Conflict)
            );
        }
        {
            let store = AdapterStore::open(&root).expect("reopen store");
            store
                .mark_model_call_provider_final("run", "thread-a:call-a")
                .expect("complete first call");
            store
                .mark_model_call_provider_final("run", "thread-a:call-a")
                .expect("repeat completion is idempotent");
            assert_eq!(
                store
                    .claim_model_call("run", "thread-a:call-b", &exchange_b, &second,)
                    .expect("replay second in-flight call"),
                2
            );
        }
        std::fs::remove_dir_all(root).expect("remove store fixture");
    }

    #[test]
    fn performance_baseline_is_exact_across_duplicate_delivery_and_restart() {
        let root = test_root("performance-baseline");
        let report = performance_report_fixture(&root);
        assert_eq!(report.execution_mode, ExecutionMode::DelegatedPatch);
        assert_eq!(report.observer_mode, ObserverMode::Always);
        assert_eq!(report.primary_model_call_count, 1);
        assert_eq!(report.primary_model_input_tokens, 90);
        assert_eq!(report.primary_model_cached_tokens, 10);
        assert_eq!(report.primary_model_output_tokens, 20);
        assert_eq!(report.primary_model_wait_ms, 800);
        assert_eq!(report.tool_call_count, 1);
        assert_eq!(report.patch_call_count, 1);
        assert_eq!(report.patch_apply_ms, 100);
        assert_eq!(report.files_changed, 1);
        assert_eq!(report.validation_ms, 100);
        assert_eq!(report.observer_call_count, 1);
        assert_eq!(report.observer_wait_ms, 100);
        assert_eq!(report.repair_rounds, 1);
        assert_eq!(report.turn_count, 1);
        assert_eq!(report.total_runtime_ms, 1_000);
        {
            let store = AdapterStore::open(&root).expect("reopen performance store");
            assert_eq!(
                store.register_performance_run("run", ExecutionMode::React, ObserverMode::Always,),
                Err(AdapterStoreError::Conflict)
            );
            let projection = store
                .reserve_performance_projection(
                    "run",
                    ExecutionEventId("xevt_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned()),
                    ExecutionSequence(1),
                    report.clone(),
                )
                .expect("reserve report projection");
            assert!(!projection.retained);
            store
                .mark_performance_projection_retained("run")
                .expect("mark report retained");
            assert!(
                store
                    .load_performance_projection("run")
                    .expect("load projection")
                    .expect("projection exists")
                    .retained
            );
        }
        let evidence = crate::performance_evidence::export_performance_v0_evidence(&root)
            .expect("export production AdapterStore evidence");
        assert_eq!(evidence.runs.len(), 1);
        assert_eq!(evidence.model_calls.len(), 2);
        let comparison = evidence
            .summarize()
            .expect("summarize production AdapterStore evidence");
        assert_eq!(comparison.structured.sample_count, 1);
        assert_eq!(comparison.structured.strong_model_call_count, 1);
        assert_eq!(comparison.structured.observer_model_call_count, 1);
        assert_eq!(comparison.structured.total_tokens, 125);
        assert_eq!(comparison.structured.settled_cost_microunits, 37);
        assert_eq!(comparison.structured.total_runtime_ms, 5_000);
        std::fs::remove_dir_all(root).expect("remove performance fixture");
    }

    #[test]
    fn model_call_provider_final_and_core_commit_are_distinct_idempotent_phases() {
        let root = test_root("model-call-phases");
        let digest = Sha256Digest(format!("sha256:{}", "3".repeat(64)));
        {
            let store = AdapterStore::open(&root).expect("open phase store");
            store
                .claim_model_call(
                    "run",
                    "thread:call",
                    &ModelExchangeId("mdl_CCCCCCCCCCCCCCCCCCCCCCCCCC".to_owned()),
                    &digest,
                )
                .expect("claim model call");
            assert_eq!(
                store
                    .model_call_phase("run", "thread:call")
                    .expect("in-flight phase"),
                Some(ModelCallPhase::InFlight)
            );
            assert_eq!(
                store.commit_model_call("run", "thread:call"),
                Err(AdapterStoreError::Conflict)
            );
            store
                .mark_model_call_provider_final("run", "thread:call")
                .expect("provider final phase");
            assert_eq!(
                store
                    .model_call_phase("run", "thread:call")
                    .expect("provider final phase"),
                Some(ModelCallPhase::ProviderFinal)
            );
            store
                .commit_model_call("run", "thread:call")
                .expect("core commit phase");
            store
                .commit_model_call("run", "thread:call")
                .expect("idempotent core commit");
            assert_eq!(
                store
                    .model_call_phase("run", "thread:call")
                    .expect("core committed phase"),
                Some(ModelCallPhase::CoreCommitted)
            );
        }
        std::fs::remove_dir_all(root).expect("remove phase store");
    }

    #[test]
    fn pending_approval_operations_are_listed_in_stable_order_and_drop_after_resolution() {
        let root = test_root("pending-approvals");
        let request_digest = format!("sha256:{}", "1".repeat(64));
        let resolution_digest = format!("sha256:{}", "2".repeat(64));
        let first = StoredApprovalOperation {
            approval_id: "apr_BBBBBBBBBBBBBBBBBBBBBBBBBB".to_owned(),
            run_key: "run".to_owned(),
            kernel_session_id: "kernel-session".to_owned(),
            operation_kind: StoredApprovalOperationKind::Patch,
            operation_id: "call-patch".to_owned(),
            turn_id: Some("turn".to_owned()),
            request_digest: request_digest.clone(),
            resolution_digest: None,
            state: StoredApprovalOperationState::Pending,
        };
        let second = StoredApprovalOperation {
            approval_id: "apr_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
            operation_kind: StoredApprovalOperationKind::Exec,
            operation_id: "call-exec".to_owned(),
            ..first.clone()
        };
        {
            let store = AdapterStore::open(&root).expect("open store");
            store
                .retain_approval_operation(&first)
                .expect("retain first approval");
            store
                .retain_approval_operation(&second)
                .expect("retain second approval");
            assert_eq!(
                store
                    .list_pending_approval_operations("run")
                    .expect("list pending approvals"),
                vec![second.clone(), first.clone()]
            );
            store
                .resolve_approval_operation(
                    &second.approval_id,
                    &second.request_digest,
                    &resolution_digest,
                )
                .expect("resolve second approval");
            assert_eq!(
                store
                    .list_pending_approval_operations("run")
                    .expect("list remaining pending approvals"),
                vec![first]
            );
        }
        std::fs::remove_dir_all(root).expect("remove store fixture");
    }

    #[test]
    fn pending_approval_operations_rebind_after_kernel_restart() {
        let root = test_root("rebind-approvals");
        let operation = StoredApprovalOperation {
            approval_id: "apr_AAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
            run_key: "run".to_owned(),
            kernel_session_id: "kernel-session-old".to_owned(),
            operation_kind: StoredApprovalOperationKind::Exec,
            operation_id: "call-exec".to_owned(),
            turn_id: Some("turn".to_owned()),
            request_digest: format!("sha256:{}", "1".repeat(64)),
            resolution_digest: None,
            state: StoredApprovalOperationState::Pending,
        };
        {
            let store = AdapterStore::open(&root).expect("open store");
            store
                .retain_approval_operation(&operation)
                .expect("retain pending approval");
            store
                .rebind_pending_approval_operations(
                    "run",
                    "kernel-session-old",
                    "kernel-session-new",
                )
                .expect("rebind pending approval");
            let mut expected = operation.clone();
            expected.kernel_session_id = "kernel-session-new".to_owned();
            assert_eq!(
                store
                    .load_approval_operation(&operation.approval_id)
                    .expect("load rebound approval"),
                Some(expected.clone())
            );
            store
                .rebind_pending_approval_operations(
                    "run",
                    "kernel-session-new",
                    "kernel-session-new",
                )
                .expect("exact rebind is idempotent");
            assert_eq!(
                store
                    .load_approval_operation(&operation.approval_id)
                    .expect("load idempotent approval"),
                Some(expected)
            );
            assert_eq!(
                store.rebind_pending_approval_operations(
                    "run",
                    "kernel-session-old",
                    "kernel-session-other",
                ),
                Err(AdapterStoreError::Conflict)
            );
        }
        std::fs::remove_dir_all(root).expect("remove store fixture");
    }

    #[cfg(unix)]
    #[test]
    fn private_state_permissions_are_tightened_without_path_disclosure() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = test_root("permissions");
        std::fs::create_dir_all(&root).expect("create fixture root");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777))
            .expect("widen fixture root");
        let database = root.join(DATABASE_FILE);
        std::fs::write(&database, []).expect("create fixture database");
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o666))
            .expect("widen fixture database");

        let store = AdapterStore::open(&root).expect("open private store");
        assert_eq!(
            std::fs::metadata(&root)
                .expect("root metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for path in [
            database,
            root.join(format!("{DATABASE_FILE}-wal")),
            root.join(format!("{DATABASE_FILE}-shm")),
        ] {
            if path.exists() {
                assert_eq!(
                    std::fs::metadata(path)
                        .expect("private file metadata")
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
        }
        let debug = format!("{store:?}");
        assert!(!debug.contains(root.to_string_lossy().as_ref()));
        drop(store);
        std::fs::remove_dir_all(root).expect("remove store fixture");
    }
}
